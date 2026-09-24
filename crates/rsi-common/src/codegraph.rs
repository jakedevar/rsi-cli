//! Versioned, provider-neutral Codegraph read contracts. Scope is supplied by
//! the daemon for native calls; only local operator RPC accepts an explicit ID.
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const CODEGRAPH_WIRE_VERSION: u8 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodegraphCapabilitiesV1 {
    pub wire_version: u8,
    pub metadata_schema_version: u8,
    pub available: bool,
    pub indexing_enabled: bool,
    pub supported_read_methods: Vec<String>,
    pub node_kinds: Vec<String>,
    pub relation_kinds: Vec<String>,
    pub historical_reads: bool,
    pub fts_search: bool,
    pub federation: bool,
    pub native_tools: Vec<String>,
    pub max_query_results: usize,
    pub max_operator_output_bytes: usize,
    pub max_native_output_bytes: usize,
    pub max_native_output_tokens: usize,
    pub max_snapshot_page_size: usize,
    pub max_workspace_page_size: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodegraphScopeV1 {
    pub project_id: Uuid,
    #[serde(default)]
    pub workspace_id: Option<Uuid>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodegraphProvenanceV1 {
    Strict,
    Exploratory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodegraphDirectionV1 {
    Outgoing,
    Incoming,
    Both,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodegraphSearchModeV1 {
    ExactName,
    NameContains,
    ExactPath,
    Fts,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodegraphRelationKindV1 {
    Contains,
    Declares,
    Calls,
    UsesType,
    Defines,
    Implements,
    HasMethod,
    Imports,
    DependsOn,
    DevDependsOn,
    BuildDependsOn,
    Tests,
    Documents,
    ReferencesDoc,
    ReferencesFinding,
    SatisfiesFinding,
    Supersedes,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodegraphFilterV1 {
    pub provenance: CodegraphProvenanceV1,
    #[serde(default)]
    pub relation_kinds: Vec<CodegraphRelationKindV1>,
}
impl Default for CodegraphFilterV1 {
    fn default() -> Self {
        Self {
            provenance: CodegraphProvenanceV1::Strict,
            relation_kinds: vec![],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodegraphQueryLimitsV1 {
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
impl Default for CodegraphQueryLimitsV1 {
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum CodegraphReadV1 {
    Search {
        mode: CodegraphSearchModeV1,
        query: String,
        #[serde(default)]
        node_kinds: Vec<String>,
        #[serde(default)]
        path_prefix: Option<String>,
    },
    Explain {
        #[serde(default)]
        node_id: Option<Uuid>,
        #[serde(default)]
        relation_id: Option<Uuid>,
    },
    Neighbors {
        node_id: Uuid,
        direction: CodegraphDirectionV1,
    },
    Subgraph {
        seeds: Vec<Uuid>,
        direction: CodegraphDirectionV1,
    },
    Path {
        from_node_id: Uuid,
        to_node_id: Uuid,
        direction: CodegraphDirectionV1,
    },
    Impact {
        changed_node_id: Uuid,
    },
    Diff {
        #[serde(default)]
        baseline_generation: Option<i64>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodegraphOperatorReadV1 {
    pub scope: CodegraphScopeV1,
    pub read: CodegraphReadV1,
    #[serde(default)]
    pub filter: CodegraphFilterV1,
    #[serde(default)]
    pub limits: CodegraphQueryLimitsV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodegraphNativeReadV1 {
    pub read: CodegraphReadV1,
    #[serde(default)]
    pub filter: CodegraphFilterV1,
    #[serde(default)]
    pub limits: CodegraphQueryLimitsV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodegraphSnapshotSummaryV1 {
    pub project_id: Uuid,
    pub repository_id: Uuid,
    pub workspace_id: Uuid,
    pub generation: i64,
    pub snapshot_digest: String,
    pub graph_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodegraphSnapshotDetailV1 {
    pub snapshot: CodegraphSnapshotSummaryV1,
    pub counts: CodegraphReadyCountsV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodegraphSnapshotRequestV1 {
    pub scope: CodegraphScopeV1,
    pub generation: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodegraphSnapshotPageRequestV1 {
    pub scope: CodegraphScopeV1,
    #[serde(default)]
    pub cursor: Option<String>,
    pub limit: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodegraphSnapshotPageV1 {
    pub snapshots: Vec<CodegraphSnapshotSummaryV1>,
    pub next_cursor: Option<String>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodegraphIndexPhaseV1 {
    Missing,
    Queued,
    Building,
    Ready,
    Stale,
    Degraded,
    Failed,
    Recovering,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodegraphReadyCountsV1 {
    pub files: usize,
    pub parsed_files: usize,
    pub degraded_files: usize,
    pub nodes: usize,
    pub relations: usize,
    pub unresolved_references: usize,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodegraphRunCountsV1 {
    pub files_discovered: usize,
    pub bytes_discovered: usize,
    pub files_hashed: usize,
    pub files_reused: usize,
    pub files_extracted: usize,
    pub changed_paths: usize,
    pub staged_files: usize,
    pub overflow_count: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodegraphStatusV1 {
    pub wire_version: u8,
    pub project_id: Uuid,
    pub workspace_id: Uuid,
    pub phase: CodegraphIndexPhaseV1,
    pub ready: Option<CodegraphSnapshotSummaryV1>,
    pub ready_counts: Option<CodegraphReadyCountsV1>,
    pub run_counts: Option<CodegraphRunCountsV1>,
    pub run_id: Option<Uuid>,
    pub last_attempt_at: Option<String>,
    pub last_success_at: Option<String>,
    pub last_error: Option<String>,
    pub pending_rescan: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodegraphWorkspaceV1 {
    pub project_id: Uuid,
    pub workspace_id: Uuid,
    pub status: CodegraphStatusV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodegraphWorkspacePageRequestV1 {
    pub project_id: Uuid,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default = "default_codegraph_page_limit")]
    pub limit: usize,
}

fn default_codegraph_page_limit() -> usize {
    32
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodegraphWorkspacePageV1 {
    pub workspaces: Vec<CodegraphWorkspaceV1>,
    pub next_cursor: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodegraphQueryMetaV1 {
    pub wire_version: u8,
    pub snapshot: CodegraphSnapshotSummaryV1,
    pub requested_limits: CodegraphQueryLimitsV1,
    pub effective_limits: CodegraphQueryLimitsV1,
    pub complete: bool,
    pub truncation: Vec<String>,
    pub returned_nodes: usize,
    pub returned_relations: usize,
    pub returned_evidence: usize,
    /// Whole records omitted by the native presentation ceiling, after the
    /// query engine has applied its own limits.
    #[serde(default)]
    pub presentation_omitted_records: usize,
    pub estimated_output_bytes: usize,
    pub estimated_output_tokens: usize,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodegraphSpanV1 {
    pub path: String,
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_line: usize,
    pub start_column: usize,
    pub end_line: usize,
    pub end_column: usize,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodegraphEvidenceV1 {
    pub label: String,
    pub span: CodegraphSpanV1,
    pub source_digest: String,
    pub evidence_digest: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodegraphNodeV1 {
    pub id: Uuid,
    pub kind: String,
    pub name: String,
    pub span: CodegraphSpanV1,
    pub provenance: String,
    pub evidence: Vec<CodegraphEvidenceV1>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodegraphRelationV1 {
    pub id: Uuid,
    pub kind: String,
    pub source: Uuid,
    pub target: Uuid,
    pub provenance: String,
    pub evidence: Vec<CodegraphEvidenceV1>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodegraphGraphV1 {
    pub nodes: Vec<CodegraphNodeV1>,
    pub relations: Vec<CodegraphRelationV1>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodegraphUnresolvedV1 {
    pub kind: String,
    pub owner: Uuid,
    pub raw_target: String,
    pub span: CodegraphSpanV1,
    pub source_digest: String,
    pub reference_digest: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodegraphChangeV1<T> {
    pub kind: String,
    pub before: Option<T>,
    pub after: Option<T>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum CodegraphReadValueV1 {
    Search(Vec<CodegraphNodeV1>),
    Explain {
        node: Option<CodegraphNodeV1>,
        relations: Vec<CodegraphRelationV1>,
        unresolved: Vec<CodegraphUnresolvedV1>,
    },
    Graph(CodegraphGraphV1),
    Path {
        graph: CodegraphGraphV1,
        alternatives: Vec<CodegraphGraphV1>,
        found: bool,
    },
    Diff {
        nodes: Vec<CodegraphChangeV1<CodegraphNodeV1>>,
        relations: Vec<CodegraphChangeV1<CodegraphRelationV1>>,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodegraphReadResultV1 {
    pub meta: CodegraphQueryMetaV1,
    pub value: CodegraphReadValueV1,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_read_rejects_scope_and_unknown_operation_fields() {
        let valid = serde_json::json!({"read":{"operation":"explain","node_id":Uuid::nil()}});
        let parsed: CodegraphNativeReadV1 = serde_json::from_value(valid.clone()).unwrap();
        assert!(matches!(parsed.read, CodegraphReadV1::Explain { .. }));
        for bad in [
            serde_json::json!({"scope":{"project_id":Uuid::nil()},"read":{"operation":"explain","node_id":Uuid::nil()}}),
            serde_json::json!({"read":{"operation":"explain","node_id":Uuid::nil(),"workspace_id":Uuid::nil()}}),
            serde_json::json!({"read":{"operation":"cancel","node_id":Uuid::nil()}}),
        ] {
            assert!(serde_json::from_value::<CodegraphNativeReadV1>(bad).is_err());
        }
    }
}
