//! Shared read core for operator RPC and session-bound native adapters.
//! Adapters supply authority; this service rechecks live S3 registration on
//! every call and pins S2 query sessions before reading any facts.
use std::path::{Component, Path};

use rsi_codegraph::{
    CodegraphStore, EvidenceView, NodeKind, NodeView, ReadySnapshot, RelationKind, RelationView,
    SourceSpan, UnresolvedReferenceView,
    query::{self, QueryResponse, SnapshotSelector},
};
use rsi_common::codegraph::*;
use thiserror::Error;
use uuid::Uuid;

use super::{
    IndexHandle, IndexWorkspaceBinding, NATIVE_MAX_OUTPUT_BYTES, NATIVE_MAX_OUTPUT_TOKENS,
};

#[derive(Debug, Error)]
pub enum CodegraphServiceError {
    #[error("codegraph workspace is unregistered or outside the authorized project")]
    ScopeDenied,
    #[error("codegraph workspace is ambiguous; supply a registered workspace ID")]
    AmbiguousWorkspace,
    #[error("codegraph history is not authorized for this caller")]
    HistoryDenied,
    #[error("codegraph pagination cursor expired")]
    CursorExpired,
    #[error("codegraph request is invalid: {0}")]
    Invalid(String),
    #[error("codegraph output exceeds the service presentation limit")]
    ResourceLimit,
    #[error("codegraph returned an unsafe source path")]
    UnsafePath,
    #[error(transparent)]
    Index(#[from] super::IndexError),
    #[error(transparent)]
    Store(#[from] rsi_codegraph::CodegraphError),
}

/// Created by a daemon adapter from durable caller identity, never provider
/// arguments. The adapter remains responsible for validating that a live native
/// session still belongs to this project before each call.
#[derive(Debug, Clone)]
pub struct BoundCodegraphScope {
    project_id: Uuid,
    workspace_id: Uuid,
    allow_history: bool,
}
impl BoundCodegraphScope {
    /// The caller must establish authority over `project_id` and `workspace_id`
    /// through daemon-owned session state before binding this capability.
    pub(crate) fn from_daemon_identity(
        project_id: Uuid,
        workspace_id: Uuid,
        allow_history: bool,
    ) -> Self {
        Self {
            project_id,
            workspace_id,
            allow_history,
        }
    }
}

pub struct CodegraphReadService<'a> {
    handle: &'a IndexHandle,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotCursorV1 {
    project_id: Uuid,
    workspace_id: Uuid,
    head_generation: i64,
    head_digest: String,
    before_generation: i64,
    limit: usize,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceCursorV1 {
    project_id: Uuid,
    registration_digest: String,
    after_workspace_id: Uuid,
    limit: usize,
}

impl<'a> CodegraphReadService<'a> {
    pub fn new(handle: &'a IndexHandle) -> Self {
        Self { handle }
    }

    /// Operator scope is resolved only against the live project registration.
    /// A missing workspace ID is accepted only when exactly one primary
    /// registration exists; a detached or sandbox registration never becomes
    /// an implicit default.
    pub fn resolve_operator_scope(
        &self,
        scope: CodegraphScopeV1,
        allow_history: bool,
    ) -> Result<BoundCodegraphScope, CodegraphServiceError> {
        let workspace_id = if let Some(id) = scope.workspace_id {
            id
        } else {
            let registered = self
                .handle
                .registered_project_workspaces(scope.project_id)?;
            let primary = CodegraphStore::workspace_id(scope.project_id, "primary")?;
            if registered
                .iter()
                .any(|item| item.workspace.workspace_id() == primary)
            {
                primary
            } else {
                return Err(CodegraphServiceError::AmbiguousWorkspace);
            }
        };
        self.binding(scope.project_id, workspace_id)?;
        Ok(BoundCodegraphScope::from_daemon_identity(
            scope.project_id,
            workspace_id,
            allow_history,
        ))
    }

    fn binding(
        &self,
        project_id: Uuid,
        workspace_id: Uuid,
    ) -> Result<IndexWorkspaceBinding, CodegraphServiceError> {
        let binding = self
            .handle
            .registered_workspace(workspace_id)?
            .ok_or(CodegraphServiceError::ScopeDenied)?;
        if binding.workspace.project_id() != project_id {
            return Err(CodegraphServiceError::ScopeDenied);
        }
        if binding.workspace.root().canonicalize().ok().as_deref() != Some(binding.workspace.root())
        {
            return Err(CodegraphServiceError::ScopeDenied);
        }
        Ok(binding)
    }

    pub fn status(
        &self,
        scope: &BoundCodegraphScope,
    ) -> Result<CodegraphStatusV1, CodegraphServiceError> {
        self.binding(scope.project_id, scope.workspace_id)?;
        let durable = self.handle.durable_status(scope.workspace_id)?;
        let transient = self.handle.status(scope.workspace_id);
        if let Some(ref item) = durable {
            if item.project_id != scope.project_id || item.workspace_id != scope.workspace_id {
                return Err(CodegraphServiceError::ScopeDenied);
            }
        }
        if let Some(ref item) = transient {
            if item.project_id != scope.project_id || item.workspace_id != scope.workspace_id {
                return Err(CodegraphServiceError::ScopeDenied);
            }
        }
        Ok(CodegraphStatusV1 {
            wire_version: CODEGRAPH_WIRE_VERSION,
            project_id: scope.project_id,
            workspace_id: scope.workspace_id,
            phase: transient
                .as_ref()
                .map(|s| phase(s.phase))
                .or_else(|| durable.as_ref().map(|s| durable_phase(s.phase)))
                .unwrap_or(CodegraphIndexPhaseV1::Missing),
            ready: durable
                .as_ref()
                .and_then(|s| s.ready.as_ref())
                .or_else(|| transient.as_ref().and_then(|s| s.ready.as_ref()))
                .map(snapshot),
            ready_counts: durable
                .as_ref()
                .and_then(|s| s.ready_counts.as_ref())
                .map(|counts| CodegraphReadyCountsV1 {
                    files: counts.files,
                    parsed_files: counts.parsed_files,
                    degraded_files: counts.degraded_files,
                    nodes: counts.nodes,
                    relations: counts.relations,
                    unresolved_references: counts.unresolved_references,
                }),
            run_counts: durable.as_ref().map(|s| CodegraphRunCountsV1 {
                files_discovered: s.metrics.files_discovered,
                bytes_discovered: s.metrics.bytes_discovered,
                files_hashed: s.metrics.files_hashed,
                files_reused: s.metrics.files_reused,
                files_extracted: s.metrics.files_extracted,
                changed_paths: s.metrics.changed_paths,
                staged_files: s.metrics.staged_files,
                overflow_count: s.metrics.overflow_count,
            }),
            run_id: durable.as_ref().and_then(|s| s.run_id),
            last_attempt_at: durable.as_ref().and_then(|s| s.last_attempt_at.clone()),
            last_success_at: durable.as_ref().and_then(|s| s.last_success_at.clone()),
            last_error: transient
                .as_ref()
                .and_then(|s| s.last_error.clone())
                .or_else(|| durable.as_ref().and_then(|s| s.last_error.clone()))
                .map(|error| bounded_status_error(&error)),
            pending_rescan: transient.as_ref().is_some_and(|s| s.pending_rescan)
                || durable.as_ref().is_some_and(|s| s.metrics.pending_rescan),
        })
    }

    pub fn list_workspaces(
        &self,
        request: CodegraphWorkspacePageRequestV1,
    ) -> Result<CodegraphWorkspacePageV1, CodegraphServiceError> {
        if request.limit == 0 || request.limit > 32 {
            return Err(CodegraphServiceError::Invalid(
                "workspace page limit must be 1..32".into(),
            ));
        }
        let project_id = request.project_id;
        let mut registered = self.handle.registered_project_workspaces(project_id)?;
        registered.sort_by_key(|binding| binding.workspace.workspace_id());
        let mut hasher = blake3::Hasher::new();
        for binding in &registered {
            hasher.update(binding.workspace.workspace_id().as_bytes());
            let root = binding.workspace.root().as_os_str().as_encoded_bytes();
            hasher.update(&(root.len() as u64).to_be_bytes());
            hasher.update(root);
        }
        let digest = hasher.finalize().to_hex().to_string();
        let start = if let Some(cursor) = request.cursor {
            if cursor.len() > 512 {
                return Err(CodegraphServiceError::CursorExpired);
            }
            let bytes = hex::decode(cursor).map_err(|_| CodegraphServiceError::CursorExpired)?;
            let cursor: WorkspaceCursorV1 =
                serde_json::from_slice(&bytes).map_err(|_| CodegraphServiceError::CursorExpired)?;
            if cursor.project_id != project_id
                || cursor.registration_digest != digest
                || cursor.limit != request.limit
            {
                return Err(CodegraphServiceError::CursorExpired);
            }
            registered
                .iter()
                .position(|binding| binding.workspace.workspace_id() == cursor.after_workspace_id)
                .map(|index| index + 1)
                .ok_or(CodegraphServiceError::CursorExpired)?
        } else {
            0
        };
        let has_more = registered.len().saturating_sub(start) > request.limit;
        let workspaces = registered
            .into_iter()
            .skip(start)
            .take(request.limit)
            .map(|binding| {
                let workspace_id = binding.workspace.workspace_id();
                let status = self.status(&BoundCodegraphScope::from_daemon_identity(
                    project_id,
                    workspace_id,
                    false,
                ))?;
                Ok::<CodegraphWorkspaceV1, CodegraphServiceError>(CodegraphWorkspaceV1 {
                    project_id,
                    workspace_id,
                    status,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let next_cursor = if has_more {
            let last = workspaces.last().expect("nonempty workspace page");
            Some(hex::encode(
                serde_json::to_vec(&WorkspaceCursorV1 {
                    project_id,
                    registration_digest: digest,
                    after_workspace_id: last.workspace_id,
                    limit: request.limit,
                })
                .map_err(|error| CodegraphServiceError::Invalid(error.to_string()))?,
            ))
        } else {
            None
        };
        Ok(CodegraphWorkspacePageV1 {
            workspaces,
            next_cursor,
        })
    }

    pub fn snapshot_at(
        &self,
        scope: CodegraphScopeV1,
        generation: i64,
    ) -> Result<CodegraphSnapshotDetailV1, CodegraphServiceError> {
        if generation <= 0 {
            return Err(CodegraphServiceError::Invalid(
                "generation must be positive".into(),
            ));
        }
        let bound = self.resolve_operator_scope(scope, true)?;
        let binding = self.binding(bound.project_id, bound.workspace_id)?;
        if !binding.db_path.is_file() {
            return Err(rsi_codegraph::CodegraphError::NoReadySnapshot.into());
        }
        let store = CodegraphStore::open(&binding.db_path, bound.project_id)?;
        let current = store.current_ready(bound.workspace_id)?;
        validate_snapshot(&current, &bound)?;
        if generation > current.generation {
            return Err(CodegraphServiceError::Invalid(
                "generation is newer than current ready".into(),
            ));
        }
        let selected = store.query(bound.workspace_id, SnapshotSelector::Generation(generation))?;
        validate_snapshot(selected.snapshot(), &bound)?;
        let counts = store.ready_counts_at(bound.workspace_id, generation)?;
        Ok(CodegraphSnapshotDetailV1 {
            snapshot: snapshot(selected.snapshot()),
            counts: CodegraphReadyCountsV1 {
                files: counts.files,
                parsed_files: counts.parsed_files,
                degraded_files: counts.degraded_files,
                nodes: counts.nodes,
                relations: counts.relations,
                unresolved_references: counts.unresolved_references,
            },
        })
    }

    pub fn list_snapshots(
        &self,
        request: CodegraphSnapshotPageRequestV1,
    ) -> Result<CodegraphSnapshotPageV1, CodegraphServiceError> {
        if request.limit == 0 || request.limit > 32 {
            return Err(CodegraphServiceError::Invalid(
                "snapshot page limit must be 1..32".into(),
            ));
        }
        let bound = self.resolve_operator_scope(request.scope, true)?;
        let binding = self.binding(bound.project_id, bound.workspace_id)?;
        if !binding.db_path.is_file() {
            return Ok(CodegraphSnapshotPageV1 {
                snapshots: Vec::new(),
                next_cursor: None,
            });
        }
        let store = CodegraphStore::open(&binding.db_path, bound.project_id)?;
        let current = match store.current_ready(bound.workspace_id) {
            Ok(value) => value,
            Err(rsi_codegraph::CodegraphError::NoReadySnapshot) => {
                return Ok(CodegraphSnapshotPageV1 {
                    snapshots: Vec::new(),
                    next_cursor: None,
                });
            }
            Err(error) => return Err(error.into()),
        };
        validate_snapshot(&current, &bound)?;
        let before = if let Some(cursor) = request.cursor {
            if cursor.len() > 1024 {
                return Err(CodegraphServiceError::CursorExpired);
            }
            let bytes = hex::decode(cursor).map_err(|_| CodegraphServiceError::CursorExpired)?;
            let cursor: SnapshotCursorV1 =
                serde_json::from_slice(&bytes).map_err(|_| CodegraphServiceError::CursorExpired)?;
            if cursor.project_id != bound.project_id
                || cursor.workspace_id != bound.workspace_id
                || cursor.head_generation != current.generation
                || cursor.head_digest != current.snapshot_digest
                || cursor.limit != request.limit
                || cursor.before_generation <= 0
                || cursor.before_generation > current.generation
            {
                return Err(CodegraphServiceError::CursorExpired);
            }
            let anchor = match store.query(
                bound.workspace_id,
                SnapshotSelector::Generation(cursor.before_generation),
            ) {
                Ok(anchor) => anchor,
                Err(rsi_codegraph::CodegraphError::Sqlite(
                    rusqlite::Error::QueryReturnedNoRows,
                )) => return Err(CodegraphServiceError::CursorExpired),
                Err(error) => return Err(error.into()),
            };
            validate_snapshot(anchor.snapshot(), &bound)?;
            Some(cursor.before_generation)
        } else {
            None
        };
        let mut retained =
            store.ready_snapshots_before(bound.workspace_id, before, request.limit + 1)?;
        let has_more = retained.len() > request.limit;
        retained.truncate(request.limit);
        for selected in &retained {
            validate_snapshot(selected, &bound)?;
        }
        let next_cursor = if has_more {
            let last = retained.last().expect("nonempty retained page");
            Some(hex::encode(
                serde_json::to_vec(&SnapshotCursorV1 {
                    project_id: bound.project_id,
                    workspace_id: bound.workspace_id,
                    head_generation: current.generation,
                    head_digest: current.snapshot_digest,
                    before_generation: last.generation,
                    limit: request.limit,
                })
                .map_err(|error| CodegraphServiceError::Invalid(error.to_string()))?,
            ))
        } else {
            None
        };
        Ok(CodegraphSnapshotPageV1 {
            snapshots: retained.iter().map(snapshot).collect(),
            next_cursor,
        })
    }

    pub fn read_native(
        &self,
        scope: &BoundCodegraphScope,
        request: CodegraphNativeReadV1,
    ) -> Result<CodegraphReadResultV1, CodegraphServiceError> {
        self.read_inner(scope, request.read, request.filter, request.limits, true)
    }
    pub fn read_operator(
        &self,
        request: CodegraphOperatorReadV1,
        allow_history: bool,
    ) -> Result<CodegraphReadResultV1, CodegraphServiceError> {
        let scope = self.resolve_operator_scope(request.scope, allow_history)?;
        self.read_inner(&scope, request.read, request.filter, request.limits, false)
    }
    fn read_inner(
        &self,
        scope: &BoundCodegraphScope,
        read: CodegraphReadV1,
        filter: CodegraphFilterV1,
        requested: CodegraphQueryLimitsV1,
        native: bool,
    ) -> Result<CodegraphReadResultV1, CodegraphServiceError> {
        let binding = self.binding(scope.project_id, scope.workspace_id)?;
        if !binding.db_path.is_file() {
            return Err(rsi_codegraph::CodegraphError::NoReadySnapshot.into());
        }
        let limits = to_limits(requested, native)?;
        let filter = query::QueryFilter {
            provenance: match filter.provenance {
                CodegraphProvenanceV1::Strict => query::ProvenanceMode::Strict,
                CodegraphProvenanceV1::Exploratory => query::ProvenanceMode::Exploratory,
            },
            relation_kinds: filter
                .relation_kinds
                .into_iter()
                .map(relation_kind)
                .collect(),
        };
        let store = CodegraphStore::open(&binding.db_path, scope.project_id)?;
        let current = store.query(scope.workspace_id, SnapshotSelector::CurrentReady)?;
        validate_snapshot(current.snapshot(), scope)?;
        let (meta, value) = match read {
            CodegraphReadV1::Search {
                mode,
                query,
                node_kinds,
                path_prefix,
            } => {
                let mode = match mode {
                    CodegraphSearchModeV1::ExactName => query::SearchMode::ExactName,
                    CodegraphSearchModeV1::NameContains => query::SearchMode::NameContains,
                    CodegraphSearchModeV1::ExactPath => query::SearchMode::ExactPath,
                    CodegraphSearchModeV1::Fts => query::SearchMode::Fts,
                };
                if query.trim().is_empty() {
                    return Err(CodegraphServiceError::Invalid(
                        "query must be nonempty".into(),
                    ));
                }
                if node_kinds.len() > NodeKind::ALL.len() {
                    return Err(CodegraphServiceError::Invalid("too many node kinds".into()));
                }
                let kinds = node_kinds
                    .iter()
                    .map(|name| {
                        NodeKind::ALL
                            .iter()
                            .copied()
                            .find(|kind| kind.as_str() == name)
                            .ok_or_else(|| {
                                CodegraphServiceError::Invalid("unknown node kind".into())
                            })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                project(
                    current.search_filtered(
                        mode,
                        &query,
                        &kinds,
                        path_prefix.as_deref(),
                        &filter,
                        limits,
                    )?,
                    |v| CodegraphReadValueV1::Search(v.into_iter().map(node).collect()),
                )?
            }
            CodegraphReadV1::Explain {
                node_id: Some(node_id),
                relation_id: None,
            } => project(current.explain(non_nil(node_id)?, &filter, limits)?, |v| {
                CodegraphReadValueV1::Explain {
                    node: v.node.map(node),
                    relations: v.relations.into_iter().map(relation).collect(),
                    unresolved: v.unresolved.into_iter().map(unresolved).collect(),
                }
            })?,
            CodegraphReadV1::Explain {
                node_id: None,
                relation_id: Some(relation_id),
            } => project(
                current.explain_relation(non_nil(relation_id)?, &filter, limits)?,
                |v| CodegraphReadValueV1::Graph(graph(v)),
            )?,
            CodegraphReadV1::Explain { .. } => {
                return Err(CodegraphServiceError::Invalid(
                    "explain requires exactly one of node_id or relation_id".into(),
                ));
            }
            CodegraphReadV1::Neighbors { node_id, direction } => project(
                current.neighbors(non_nil(node_id)?, to_direction(direction), &filter, limits)?,
                |v| CodegraphReadValueV1::Graph(graph(v)),
            )?,
            CodegraphReadV1::Subgraph { seeds, direction } => {
                if seeds.is_empty()
                    || seeds.len() > limits.max_nodes
                    || seeds.iter().any(Uuid::is_nil)
                {
                    return Err(CodegraphServiceError::Invalid(
                        "seeds must be nonempty, unique valid IDs within max_nodes".into(),
                    ));
                }
                let mut unique = seeds.clone();
                unique.sort();
                unique.dedup();
                if unique.len() != seeds.len() {
                    return Err(CodegraphServiceError::Invalid("duplicate seed".into()));
                }
                project(
                    current.subgraph(&seeds, to_direction(direction), &filter, limits)?,
                    |v| CodegraphReadValueV1::Graph(graph(v)),
                )?
            }
            CodegraphReadV1::Path {
                from_node_id,
                to_node_id,
                direction,
            } => project(
                current.path(
                    non_nil(from_node_id)?,
                    non_nil(to_node_id)?,
                    to_direction(direction),
                    &filter,
                    limits,
                )?,
                |v| CodegraphReadValueV1::Path {
                    graph: graph(v.graph),
                    alternatives: v.alternatives.into_iter().map(graph).collect(),
                    found: v.found,
                },
            )?,
            CodegraphReadV1::Impact { changed_node_id } => project(
                current.impact(non_nil(changed_node_id)?, &filter, limits)?,
                |v| CodegraphReadValueV1::Graph(graph(v)),
            )?,
            CodegraphReadV1::Diff {
                baseline_generation,
            } => {
                if !scope.allow_history {
                    return Err(CodegraphServiceError::HistoryDenied);
                }
                let previous = store
                    .ready_snapshots_before(
                        scope.workspace_id,
                        Some(current.snapshot().generation),
                        1,
                    )?
                    .into_iter()
                    .next()
                    .ok_or_else(|| {
                        CodegraphServiceError::Invalid("no previous ready snapshot".into())
                    })?;
                let baseline_generation = baseline_generation.unwrap_or(previous.generation);
                if native && baseline_generation != previous.generation {
                    return Err(CodegraphServiceError::HistoryDenied);
                }
                if baseline_generation <= 0 || baseline_generation >= current.snapshot().generation
                {
                    return Err(CodegraphServiceError::Invalid(
                        "baseline generation must precede current ready".into(),
                    ));
                }
                let baseline = store.query(
                    scope.workspace_id,
                    SnapshotSelector::Generation(baseline_generation),
                )?;
                validate_snapshot(baseline.snapshot(), scope)?;
                project(current.diff(&baseline, &filter, limits)?, |v| {
                    CodegraphReadValueV1::Diff {
                        nodes: v
                            .nodes
                            .into_iter()
                            .map(|v| CodegraphChangeV1 {
                                kind: format!("{:?}", v.kind),
                                before: v.before.map(node),
                                after: v.after.map(node),
                            })
                            .collect(),
                        relations: v
                            .relations
                            .into_iter()
                            .map(|v| CodegraphChangeV1 {
                                kind: format!("{:?}", v.kind),
                                before: v.before.map(relation),
                                after: v.after.map(relation),
                            })
                            .collect(),
                    }
                })?
            }
        };
        let mut result = CodegraphReadResultV1 {
            meta: CodegraphQueryMetaV1 {
                wire_version: CODEGRAPH_WIRE_VERSION,
                snapshot: snapshot(&meta.snapshot),
                requested_limits: requested,
                effective_limits: from_limits(meta.limits),
                complete: meta.complete,
                truncation: meta
                    .truncation
                    .into_iter()
                    .map(|v| format!("{v:?}"))
                    .collect(),
                returned_nodes: meta.returned_nodes,
                returned_relations: meta.returned_relations,
                returned_evidence: meta.returned_evidence,
                presentation_omitted_records: 0,
                estimated_output_bytes: meta.estimated_output_bytes,
                estimated_output_tokens: meta.estimated_output_tokens,
            },
            value,
        };
        validate_paths(&result.value)?;
        if native {
            bound_native_presentation(&mut result, limits)?;
        } else {
            let bytes = serde_json::to_vec(&result)
                .map_err(|e| CodegraphServiceError::Invalid(e.to_string()))?
                .len();
            if bytes > limits.max_output_bytes || bytes.div_ceil(4) > limits.max_output_tokens {
                return Err(CodegraphServiceError::ResourceLimit);
            }
        }
        Ok(result)
    }
}

fn bounded_status_error(error: &str) -> String {
    const MAX_STATUS_ERROR_BYTES: usize = 1_024;
    let mut end = error.len().min(MAX_STATUS_ERROR_BYTES);
    while !error.is_char_boundary(end) {
        end -= 1;
    }
    error[..end].to_owned()
}

fn validate_snapshot(
    snapshot: &ReadySnapshot,
    scope: &BoundCodegraphScope,
) -> Result<(), CodegraphServiceError> {
    if snapshot.project_id != scope.project_id
        || snapshot.workspace_id != scope.workspace_id
        || snapshot.repository_id != CodegraphStore::repository_id(scope.project_id)
    {
        return Err(CodegraphServiceError::ScopeDenied);
    }
    Ok(())
}
fn to_limits(
    v: CodegraphQueryLimitsV1,
    native: bool,
) -> Result<query::QueryLimits, CodegraphServiceError> {
    let limits = query::QueryLimits {
        max_results: v.max_results,
        max_depth: v.max_depth,
        max_nodes: v.max_nodes,
        max_relations: v.max_relations,
        max_frontier: v.max_frontier,
        max_paths: v.max_paths,
        max_evidence_per_fact: v.max_evidence_per_fact,
        timeout_ms: v.timeout_ms,
        max_output_bytes: v.max_output_bytes.min(if native {
            NATIVE_MAX_OUTPUT_BYTES
        } else {
            usize::MAX
        }),
        max_output_tokens: v.max_output_tokens.min(if native {
            NATIVE_MAX_OUTPUT_TOKENS
        } else {
            usize::MAX
        }),
    };
    limits.validate()?;
    Ok(limits)
}
fn from_limits(v: query::QueryLimits) -> CodegraphQueryLimitsV1 {
    CodegraphQueryLimitsV1 {
        max_results: v.max_results,
        max_depth: v.max_depth,
        max_nodes: v.max_nodes,
        max_relations: v.max_relations,
        max_frontier: v.max_frontier,
        max_paths: v.max_paths,
        max_evidence_per_fact: v.max_evidence_per_fact,
        timeout_ms: v.timeout_ms,
        max_output_bytes: v.max_output_bytes,
        max_output_tokens: v.max_output_tokens,
    }
}

/// Shrink only complete projected facts. The query engine has already applied
/// its own limits; this pass accounts for JSON framing and native tool budgets.
fn bound_native_presentation(
    result: &mut CodegraphReadResultV1,
    limits: query::QueryLimits,
) -> Result<(), CodegraphServiceError> {
    loop {
        // The metadata contains the encoded length, so settle that field
        // before checking the final payload against either ceiling.
        let mut bytes = 0;
        for _ in 0..8 {
            bytes = serde_json::to_vec(result)
                .map_err(|e| CodegraphServiceError::Invalid(e.to_string()))?
                .len();
            if result.meta.estimated_output_bytes == bytes
                && result.meta.estimated_output_tokens == bytes
            {
                break;
            }
            result.meta.estimated_output_bytes = bytes;
            // S2 counts one UTF-8 byte per estimated token; retain its
            // conservative estimate for the provider-facing ceiling.
            result.meta.estimated_output_tokens = bytes;
        }
        if bytes <= limits.max_output_bytes && bytes <= limits.max_output_tokens {
            return Ok(());
        }
        result.meta.complete = false;
        if bytes > limits.max_output_bytes
            && !result
                .meta
                .truncation
                .iter()
                .any(|v| v == "PresentationBytes")
        {
            result.meta.truncation.push("PresentationBytes".into());
        }
        if bytes > limits.max_output_tokens
            && !result
                .meta
                .truncation
                .iter()
                .any(|v| v == "PresentationTokens")
        {
            result.meta.truncation.push("PresentationTokens".into());
        }
        if !pop_presentation_record(&mut result.value) {
            return Err(CodegraphServiceError::ResourceLimit);
        }
        result.meta.presentation_omitted_records += 1;
        let (nodes, relations, evidence) = presentation_counts(&result.value);
        result.meta.returned_nodes = nodes;
        result.meta.returned_relations = relations;
        result.meta.returned_evidence = evidence;
    }
}

fn pop_graph_record(graph: &mut CodegraphGraphV1) -> bool {
    graph.relations.pop().is_some() || graph.nodes.pop().is_some()
}

fn pop_presentation_record(value: &mut CodegraphReadValueV1) -> bool {
    match value {
        CodegraphReadValueV1::Search(nodes) => nodes.pop().is_some(),
        CodegraphReadValueV1::Explain {
            relations,
            unresolved,
            ..
        } => unresolved.pop().is_some() || relations.pop().is_some(),
        CodegraphReadValueV1::Graph(graph) => pop_graph_record(graph),
        CodegraphReadValueV1::Path {
            graph,
            alternatives,
            ..
        } => {
            while let Some(last) = alternatives.last_mut() {
                if pop_graph_record(last) {
                    if last.nodes.is_empty() && last.relations.is_empty() {
                        alternatives.pop();
                    }
                    return true;
                }
                alternatives.pop();
            }
            pop_graph_record(graph)
        }
        CodegraphReadValueV1::Diff { nodes, relations } => {
            relations.pop().is_some() || nodes.pop().is_some()
        }
    }
}

fn presentation_counts(value: &CodegraphReadValueV1) -> (usize, usize, usize) {
    fn graph_counts(graph: &CodegraphGraphV1) -> (usize, usize, usize) {
        (
            graph.nodes.len(),
            graph.relations.len(),
            graph.nodes.iter().map(|v| v.evidence.len()).sum::<usize>()
                + graph
                    .relations
                    .iter()
                    .map(|v| v.evidence.len())
                    .sum::<usize>(),
        )
    }
    match value {
        CodegraphReadValueV1::Search(nodes) => {
            (nodes.len(), 0, nodes.iter().map(|v| v.evidence.len()).sum())
        }
        CodegraphReadValueV1::Explain {
            node, relations, ..
        } => (
            usize::from(node.is_some()),
            relations.len(),
            node.iter().map(|v| v.evidence.len()).sum::<usize>()
                + relations.iter().map(|v| v.evidence.len()).sum::<usize>(),
        ),
        CodegraphReadValueV1::Graph(graph) => graph_counts(graph),
        CodegraphReadValueV1::Path {
            graph,
            alternatives,
            ..
        } => {
            let mut counts = graph_counts(graph);
            for alternative in alternatives {
                let other = graph_counts(alternative);
                counts.0 += other.0;
                counts.1 += other.1;
                counts.2 += other.2;
            }
            counts
        }
        CodegraphReadValueV1::Diff { nodes, relations } => (
            nodes.len(),
            relations.len(),
            nodes
                .iter()
                .flat_map(|v| v.before.iter().chain(v.after.iter()))
                .map(|v| v.evidence.len())
                .sum::<usize>()
                + relations
                    .iter()
                    .flat_map(|v| v.before.iter().chain(v.after.iter()))
                    .map(|v| v.evidence.len())
                    .sum::<usize>(),
        ),
    }
}
fn non_nil(id: Uuid) -> Result<Uuid, CodegraphServiceError> {
    if id.is_nil() {
        Err(CodegraphServiceError::Invalid(
            "node ID must be nonnil".into(),
        ))
    } else {
        Ok(id)
    }
}
fn to_direction(v: CodegraphDirectionV1) -> query::Direction {
    match v {
        CodegraphDirectionV1::Outgoing => query::Direction::Outgoing,
        CodegraphDirectionV1::Incoming => query::Direction::Incoming,
        CodegraphDirectionV1::Both => query::Direction::Both,
    }
}
fn relation_kind(v: CodegraphRelationKindV1) -> RelationKind {
    use CodegraphRelationKindV1 as V;
    match v {
        V::Contains => RelationKind::Contains,
        V::Declares => RelationKind::Declares,
        V::Calls => RelationKind::Calls,
        V::UsesType => RelationKind::UsesType,
        V::Defines => RelationKind::Defines,
        V::Implements => RelationKind::Implements,
        V::HasMethod => RelationKind::HasMethod,
        V::Imports => RelationKind::Imports,
        V::DependsOn => RelationKind::DependsOn,
        V::DevDependsOn => RelationKind::DevDependsOn,
        V::BuildDependsOn => RelationKind::BuildDependsOn,
        V::Tests => RelationKind::Tests,
        V::Documents => RelationKind::Documents,
        V::ReferencesDoc => RelationKind::ReferencesDoc,
        V::ReferencesFinding => RelationKind::ReferencesFinding,
        V::SatisfiesFinding => RelationKind::SatisfiesFinding,
        V::Supersedes => RelationKind::Supersedes,
    }
}
fn project<T>(
    response: QueryResponse<T>,
    map: impl FnOnce(T) -> CodegraphReadValueV1,
) -> Result<(query::QueryMeta, CodegraphReadValueV1), CodegraphServiceError> {
    Ok((response.meta, map(response.value)))
}
fn phase(v: super::IndexPhase) -> CodegraphIndexPhaseV1 {
    use super::IndexPhase as S;
    match v {
        S::Queued => CodegraphIndexPhaseV1::Queued,
        S::Building => CodegraphIndexPhaseV1::Building,
        S::Ready => CodegraphIndexPhaseV1::Ready,
        S::Stale => CodegraphIndexPhaseV1::Stale,
        S::Degraded => CodegraphIndexPhaseV1::Degraded,
        S::Failed => CodegraphIndexPhaseV1::Failed,
        S::Recovering => CodegraphIndexPhaseV1::Recovering,
    }
}
fn durable_phase(v: rsi_codegraph::lifecycle::IndexLifecyclePhase) -> CodegraphIndexPhaseV1 {
    use rsi_codegraph::lifecycle::IndexLifecyclePhase as S;
    match v {
        S::Queued => CodegraphIndexPhaseV1::Queued,
        S::Building => CodegraphIndexPhaseV1::Building,
        S::Ready => CodegraphIndexPhaseV1::Ready,
        S::Stale => CodegraphIndexPhaseV1::Stale,
        S::Degraded => CodegraphIndexPhaseV1::Degraded,
        S::Failed => CodegraphIndexPhaseV1::Failed,
        S::Recovering => CodegraphIndexPhaseV1::Recovering,
    }
}
fn snapshot(v: &ReadySnapshot) -> CodegraphSnapshotSummaryV1 {
    CodegraphSnapshotSummaryV1 {
        project_id: v.project_id,
        repository_id: v.repository_id,
        workspace_id: v.workspace_id,
        generation: v.generation,
        snapshot_digest: v.snapshot_digest.clone(),
        graph_digest: v.graph_digest.clone(),
    }
}
fn span(v: SourceSpan) -> CodegraphSpanV1 {
    CodegraphSpanV1 {
        path: v.path,
        start_byte: v.start_byte,
        end_byte: v.end_byte,
        start_line: v.start_line,
        start_column: v.start_column,
        end_line: v.end_line,
        end_column: v.end_column,
    }
}
fn evidence(v: EvidenceView) -> CodegraphEvidenceV1 {
    CodegraphEvidenceV1 {
        label: v.label,
        span: span(v.span),
        source_digest: v.source_digest,
        evidence_digest: v.evidence_digest,
    }
}
fn node(v: NodeView) -> CodegraphNodeV1 {
    CodegraphNodeV1 {
        id: v.id,
        kind: format!("{:?}", v.kind),
        name: v.name,
        span: span(v.span),
        provenance: format!("{:?}", v.provenance),
        evidence: v.evidence.into_iter().map(evidence).collect(),
    }
}
fn relation(v: RelationView) -> CodegraphRelationV1 {
    CodegraphRelationV1 {
        id: v.id,
        kind: format!("{:?}", v.kind),
        source: v.source,
        target: v.target,
        provenance: format!("{:?}", v.provenance),
        evidence: v.evidence.into_iter().map(evidence).collect(),
    }
}
fn unresolved(v: UnresolvedReferenceView) -> CodegraphUnresolvedV1 {
    CodegraphUnresolvedV1 {
        kind: format!("{:?}", v.kind),
        owner: v.owner,
        raw_target: v.raw_target,
        span: span(v.span),
        source_digest: v.source_digest,
        reference_digest: v.reference_digest,
    }
}
fn graph(v: query::GraphView) -> CodegraphGraphV1 {
    CodegraphGraphV1 {
        nodes: v.nodes.into_iter().map(node).collect(),
        relations: v.relations.into_iter().map(relation).collect(),
    }
}
fn safe_path(path: &str) -> bool {
    !path.is_empty()
        && !Path::new(path).is_absolute()
        && Path::new(path)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
        && !path.contains('\\')
        && !path.contains('\0')
        && !path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
}
fn validate_span(v: &CodegraphSpanV1) -> Result<(), CodegraphServiceError> {
    if safe_path(&v.path) {
        Ok(())
    } else {
        Err(CodegraphServiceError::UnsafePath)
    }
}
fn validate_node(v: &CodegraphNodeV1) -> Result<(), CodegraphServiceError> {
    validate_span(&v.span)?;
    for e in &v.evidence {
        validate_span(&e.span)?;
    }
    Ok(())
}
fn validate_relation(v: &CodegraphRelationV1) -> Result<(), CodegraphServiceError> {
    for e in &v.evidence {
        validate_span(&e.span)?;
    }
    Ok(())
}
fn validate_graph(v: &CodegraphGraphV1) -> Result<(), CodegraphServiceError> {
    for n in &v.nodes {
        validate_node(n)?;
    }
    for r in &v.relations {
        validate_relation(r)?;
    }
    Ok(())
}
fn validate_paths(v: &CodegraphReadValueV1) -> Result<(), CodegraphServiceError> {
    match v {
        CodegraphReadValueV1::Search(nodes) => {
            for n in nodes {
                validate_node(n)?;
            }
        }
        CodegraphReadValueV1::Explain {
            node,
            relations,
            unresolved,
        } => {
            if let Some(n) = node {
                validate_node(n)?;
            }
            for r in relations {
                validate_relation(r)?;
            }
            for u in unresolved {
                validate_span(&u.span)?;
            }
        }
        CodegraphReadValueV1::Graph(g) => validate_graph(g)?,
        CodegraphReadValueV1::Path {
            graph,
            alternatives,
            ..
        } => {
            validate_graph(graph)?;
            for g in alternatives {
                validate_graph(g)?;
            }
        }
        CodegraphReadValueV1::Diff { nodes, relations } => {
            for change in nodes {
                if let Some(v) = &change.before {
                    validate_node(v)?;
                }
                if let Some(v) = &change.after {
                    validate_node(v)?;
                }
            }
            for change in relations {
                if let Some(v) = &change.before {
                    validate_relation(v)?;
                }
                if let Some(v) = &change.after {
                    validate_relation(v)?;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn status_error_projection_is_utf8_and_bounded() {
        let source = "é".repeat(1_000);
        let error = bounded_status_error(&source);
        assert_eq!(error.len(), 1_024);
        assert!(source.starts_with(&error));
    }

    #[test]
    fn native_presentation_keeps_whole_records_and_reports_omissions() {
        let id = Uuid::new_v4();
        let span = CodegraphSpanV1 {
            path: "src/lib.rs".into(),
            start_byte: 0,
            end_byte: 1,
            start_line: 1,
            start_column: 1,
            end_line: 1,
            end_column: 2,
        };
        let nodes: Vec<_> = (0..12)
            .map(|_| CodegraphNodeV1 {
                id: Uuid::new_v4(),
                kind: "Function".into(),
                name: "a".repeat(120),
                span: span.clone(),
                provenance: "Strict".into(),
                evidence: vec![],
            })
            .collect();
        let original = nodes.len();
        let requested = CodegraphQueryLimitsV1::default();
        let mut result = CodegraphReadResultV1 {
            meta: CodegraphQueryMetaV1 {
                wire_version: CODEGRAPH_WIRE_VERSION,
                snapshot: CodegraphSnapshotSummaryV1 {
                    project_id: id,
                    repository_id: id,
                    workspace_id: id,
                    generation: 1,
                    snapshot_digest: "snapshot".into(),
                    graph_digest: "graph".into(),
                },
                requested_limits: requested,
                effective_limits: requested,
                complete: true,
                truncation: vec![],
                returned_nodes: original,
                returned_relations: 0,
                returned_evidence: 0,
                presentation_omitted_records: 0,
                estimated_output_bytes: 0,
                estimated_output_tokens: 0,
            },
            value: CodegraphReadValueV1::Search(nodes),
        };
        let mut limits = query::QueryLimits::default();
        limits.max_output_bytes = 2_000;
        limits.max_output_tokens = 1_500;
        bound_native_presentation(&mut result, limits).unwrap();
        let CodegraphReadValueV1::Search(kept) = &result.value else {
            panic!("search result changed kind");
        };
        assert!(!kept.is_empty());
        assert_eq!(result.meta.returned_nodes, kept.len());
        assert_eq!(
            result.meta.presentation_omitted_records,
            original - kept.len()
        );
        assert!(!result.meta.complete);
        assert!(
            result
                .meta
                .truncation
                .contains(&"PresentationTokens".into())
        );
        let encoded = serde_json::to_vec(&result).unwrap();
        assert!(encoded.len() <= 1_500);
        assert_eq!(result.meta.estimated_output_bytes, encoded.len());
        assert_eq!(result.meta.estimated_output_tokens, encoded.len());
        for kept_node in kept {
            assert_eq!(kept_node.name.len(), 120);
        }
    }

    #[test]
    fn rejects_unsafe_paths_and_invalid_limits() {
        assert!(safe_path("src/lib.rs"));
        for path in [
            "../secret",
            "/tmp/secret",
            "src/../secret",
            "src\\secret",
            "",
        ] {
            assert!(!safe_path(path));
        }
        let mut limits = CodegraphQueryLimitsV1::default();
        limits.max_depth = 9;
        assert!(to_limits(limits, false).is_err());
        let native = to_limits(CodegraphQueryLimitsV1::default(), true).unwrap();
        assert_eq!(native.max_output_bytes, NATIVE_MAX_OUTPUT_BYTES);
    }
    #[test]
    fn live_registration_enforces_project_and_workspace_isolation() {
        let root = tempfile::tempdir().unwrap();
        let index = tempfile::tempdir().unwrap();
        let project_id = Uuid::new_v4();
        let other_project_id = Uuid::new_v4();
        let workspace =
            super::super::RegisteredWorkspace::primary(project_id, root.path()).unwrap();
        let workspace_id = workspace.workspace_id();
        let (_manager, handle) =
            super::super::IndexManager::new(index.path().to_path_buf(), vec![workspace]).unwrap();
        let service = CodegraphReadService::new(&handle);
        let status = service
            .status(&BoundCodegraphScope::from_daemon_identity(
                project_id,
                workspace_id,
                false,
            ))
            .unwrap();
        assert_eq!(status.project_id, project_id);
        assert_eq!(status.workspace_id, workspace_id);
        assert_eq!(status.phase, CodegraphIndexPhaseV1::Queued);
        let workspace_page = service
            .list_workspaces(CodegraphWorkspacePageRequestV1 {
                project_id,
                cursor: None,
                limit: 32,
            })
            .unwrap();
        assert_eq!(workspace_page.workspaces.len(), 1);
        assert_eq!(workspace_page.workspaces[0].workspace_id, workspace_id);
        assert!(workspace_page.next_cursor.is_none());
        assert!(matches!(
            service.list_workspaces(CodegraphWorkspacePageRequestV1 {
                project_id,
                cursor: Some("forged".into()),
                limit: 32,
            }),
            Err(CodegraphServiceError::CursorExpired)
        ));
        assert!(matches!(
            service.list_workspaces(CodegraphWorkspacePageRequestV1 {
                project_id,
                cursor: None,
                limit: 0,
            }),
            Err(CodegraphServiceError::Invalid(_))
        ));
        assert!(
            service
                .list_workspaces(CodegraphWorkspacePageRequestV1 {
                    project_id: other_project_id,
                    cursor: None,
                    limit: 32,
                })
                .unwrap()
                .workspaces
                .is_empty()
        );
        assert!(matches!(
            service.status(&BoundCodegraphScope::from_daemon_identity(
                other_project_id,
                workspace_id,
                false
            )),
            Err(CodegraphServiceError::ScopeDenied)
        ));
        assert!(matches!(
            service.status(&BoundCodegraphScope::from_daemon_identity(
                project_id,
                Uuid::new_v4(),
                false
            )),
            Err(CodegraphServiceError::ScopeDenied)
        ));
        let primary = service
            .resolve_operator_scope(
                CodegraphScopeV1 {
                    project_id,
                    workspace_id: None,
                },
                false,
            )
            .unwrap();
        assert_eq!(primary.workspace_id, workspace_id);
        assert!(matches!(
            service.resolve_operator_scope(
                CodegraphScopeV1 {
                    project_id: other_project_id,
                    workspace_id: None
                },
                false
            ),
            Err(CodegraphServiceError::AmbiguousWorkspace)
        ));
    }
    #[test]
    fn snapshot_scope_rejects_cross_project_and_workspace() {
        let project_id = Uuid::new_v4();
        let workspace_id = Uuid::new_v4();
        let scope = BoundCodegraphScope::from_daemon_identity(project_id, workspace_id, false);
        let mut snapshot = ReadySnapshot {
            project_id,
            repository_id: CodegraphStore::repository_id(project_id),
            workspace_id,
            generation: 1,
            snapshot_digest: String::new(),
            graph_digest: String::new(),
            extraction: rsi_codegraph::ExtractionContract {
                mode: rsi_codegraph::ExtractionMode::ExtractedV1_0,
                extractor: rsi_codegraph::ExtractorIdentity {
                    name: "test".into(),
                    version: "1".into(),
                },
            },
        };
        assert!(validate_snapshot(&snapshot, &scope).is_ok());
        snapshot.workspace_id = Uuid::new_v4();
        assert!(matches!(
            validate_snapshot(&snapshot, &scope),
            Err(CodegraphServiceError::ScopeDenied)
        ));
    }
}
