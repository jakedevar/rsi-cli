//! Construction-bound native Codegraph tools for Harness and CodexAppServer.
//! Provider arguments contain query data only; durable session state supplies
//! the project and workspace on every invocation.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rsi_common::codegraph::{CodegraphNativeReadV1, CodegraphReadV1};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use uuid::Uuid;

use super::{
    BoundCodegraphScope, CodegraphReadService, IndexHandle, NATIVE_MAX_OUTPUT_BYTES,
    NATIVE_MAX_OUTPUT_TOKENS,
};
use crate::error::{DaemonError, Result};
use crate::store::Store;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeCodegraphToolKind {
    Search,
    Explain,
    Traverse,
    Diff,
    Status,
}

#[derive(Clone)]
pub struct NativeCodegraphBinding {
    project_id: Uuid,
    workspace_id: Uuid,
    root: PathBuf,
    ready_at_launch: bool,
}

impl NativeCodegraphBinding {
    /// Resolve only a daemon-registered root while constructing the provider
    /// registry. The binding is rechecked against durable session state later.
    pub fn for_launch(handle: &IndexHandle, project_id: Uuid, root: &Path) -> Option<Self> {
        let root = root.canonicalize().ok()?;
        let binding = handle
            .registered_project_workspaces(project_id)
            .ok()?
            .into_iter()
            .find(|binding| binding.workspace.root() == root)?;
        let workspace_id = binding.workspace.workspace_id();
        let scope = BoundCodegraphScope::from_daemon_identity(project_id, workspace_id, false);
        let ready_at_launch = CodegraphReadService::new(handle)
            .status(&scope)
            .ok()?
            .ready
            .is_some();
        Some(Self {
            project_id,
            workspace_id,
            root,
            ready_at_launch,
        })
    }

    pub fn permits(&self, kind: NativeCodegraphToolKind) -> bool {
        kind == NativeCodegraphToolKind::Status || self.ready_at_launch
    }
}

impl NativeCodegraphToolKind {
    pub const ALL: [Self; 5] = [
        Self::Search,
        Self::Explain,
        Self::Traverse,
        Self::Diff,
        Self::Status,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Self::Search => "rsi_codegraph_search",
            Self::Explain => "rsi_codegraph_explain",
            Self::Traverse => "rsi_codegraph_traverse",
            Self::Diff => "rsi_codegraph_diff",
            Self::Status => "rsi_codegraph_status",
        }
    }

    pub const fn description(self) -> &'static str {
        match self {
            Self::Search => {
                "Search the current ready Codegraph snapshot in this session's registered workspace"
            }
            Self::Explain => {
                "Explain a Codegraph node and its bounded evidence in this session's registered workspace"
            }
            Self::Traverse => {
                "Read bounded Codegraph neighbors, path, subgraph or impact in this session's registered workspace"
            }
            Self::Diff => {
                "Diff the current ready Codegraph snapshot against a retained generation in this session's registered workspace"
            }
            Self::Status => {
                "Read durable Codegraph indexing health for this session's registered workspace"
            }
        }
    }

    fn allows(self, read: &CodegraphReadV1) -> bool {
        matches!(
            (self, read),
            (Self::Search, CodegraphReadV1::Search { .. })
                | (Self::Explain, CodegraphReadV1::Explain { .. })
                | (Self::Traverse, CodegraphReadV1::Neighbors { .. })
                | (Self::Traverse, CodegraphReadV1::Path { .. })
                | (Self::Traverse, CodegraphReadV1::Subgraph { .. })
                | (Self::Traverse, CodegraphReadV1::Impact { .. })
                | (Self::Diff, CodegraphReadV1::Diff { .. })
        )
    }

    /// Strict provider schema. Serde applies the same variant-specific rules
    /// again at execution, independent of provider-side schema validation.
    pub fn schema(self) -> Value {
        if self == Self::Status {
            return json!({"type":"object","properties":{},"additionalProperties":false});
        }
        let id = json!({"type":"string","format":"uuid"});
        let direction = json!({"type":"string","enum":["outgoing","incoming","both"]});
        let node_kinds = rsi_codegraph::NodeKind::ALL
            .iter()
            .map(|kind| kind.as_str())
            .collect::<Vec<_>>();
        let variants = match self {
            Self::Search => vec![variant(
                "search",
                json!({"mode":{"type":"string","enum":["exact_name","name_contains","exact_path","fts"]},"query":{"type":"string","minLength":1},"node_kinds":{"type":"array","items":{"type":"string","enum":node_kinds},"maxItems":rsi_codegraph::NodeKind::ALL.len()},"path_prefix":{"type":"string","minLength":1,"maxLength":rsi_codegraph::MAX_PATH_BYTES}}),
                &["mode", "query"],
            )],
            Self::Explain => vec![
                variant("explain", json!({"node_id":id}), &["node_id"]),
                variant("explain", json!({"relation_id":id}), &["relation_id"]),
            ],
            Self::Traverse => vec![
                variant(
                    "neighbors",
                    json!({"node_id":id,"direction":direction}),
                    &["node_id", "direction"],
                ),
                variant(
                    "path",
                    json!({"from_node_id":id,"to_node_id":id,"direction":direction}),
                    &["from_node_id", "to_node_id", "direction"],
                ),
                variant(
                    "subgraph",
                    json!({"seeds":{"type":"array","items":id,"minItems":1},"direction":direction}),
                    &["seeds", "direction"],
                ),
                variant(
                    "impact",
                    json!({"changed_node_id":id}),
                    &["changed_node_id"],
                ),
            ],
            Self::Diff => vec![variant(
                "diff",
                json!({"baseline_generation":{"type":"integer","minimum":1}}),
                &[],
            )],
            Self::Status => unreachable!(),
        };
        json!({
            "type":"object",
            "properties":{
                "read":{"oneOf":variants},
                "filter":{
                    "type":"object","properties":{
                        "provenance":{"type":"string","enum":["strict","exploratory"]},
                        "relation_kinds":{"type":"array","items":{"type":"string","enum":[
                            "contains","declares","calls","uses_type","defines","implements",
                            "has_method","imports","depends_on","dev_depends_on","build_depends_on",
                            "tests","documents","references_doc","references_finding",
                            "satisfies_finding","supersedes"
                        ]}}
                    },"required":["provenance"],"additionalProperties":false
                },
                "limits":{
                    "type":"object","properties":{
                        "max_results":{"type":"integer","minimum":1},
                        "max_depth":{"type":"integer","minimum":1},
                        "max_nodes":{"type":"integer","minimum":1},
                        "max_relations":{"type":"integer","minimum":1},
                        "max_frontier":{"type":"integer","minimum":1},
                        "max_paths":{"type":"integer","minimum":1},
                        "max_evidence_per_fact":{"type":"integer","minimum":1},
                        "timeout_ms":{"type":"integer","minimum":1},
                        "max_output_bytes":{"type":"integer","minimum":1},
                        "max_output_tokens":{"type":"integer","minimum":1}
                    },"required":["max_results","max_depth","max_nodes","max_relations","max_frontier","max_paths","max_evidence_per_fact","timeout_ms","max_output_bytes","max_output_tokens"],"additionalProperties":false
                }
            },
            "required":["read"],"additionalProperties":false
        })
    }
}

fn variant(operation: &str, fields: Value, required: &[&str]) -> Value {
    let mut properties = fields.as_object().cloned().unwrap_or_default();
    properties.insert("operation".into(), json!({"const":operation}));
    let mut required = required.to_vec();
    required.push("operation");
    json!({"type":"object","properties":properties,"required":required,"additionalProperties":false})
}

/// Called only by a construction-bound provider tool. The session ID and
/// project ID are captured at launch and never accepted in the JSON arguments.
pub async fn execute_native_read(
    handle: IndexHandle,
    store: Arc<Mutex<Store>>,
    session_id: Uuid,
    binding: NativeCodegraphBinding,
    kind: NativeCodegraphToolKind,
    args: Value,
) -> Result<Value> {
    let read = if kind == NativeCodegraphToolKind::Status {
        if args.as_object().is_none_or(|object| !object.is_empty()) {
            return Err(DaemonError::InvalidParam(
                "status accepts no arguments".into(),
            ));
        }
        None
    } else {
        let request: CodegraphNativeReadV1 = serde_json::from_value(args)
            .map_err(|_| DaemonError::InvalidParam("invalid Codegraph tool arguments".into()))?;
        if !kind.allows(&request.read) {
            return Err(DaemonError::InvalidParam(
                "Codegraph operation does not match native tool".into(),
            ));
        }
        Some(request)
    };
    tokio::task::spawn_blocking(move || {
        let session = store
            .blocking_lock()
            .get_session(session_id)?
            .ok_or_else(|| DaemonError::PolicyDenied("Codegraph session is unavailable".into()))?;
        if session.project_id != Some(binding.project_id) {
            return Err(DaemonError::PolicyDenied(
                "Codegraph project scope changed".into(),
            ));
        }
        let session_root = session
            .sandbox_root
            .as_ref()
            .unwrap_or(&session.working_dir)
            .canonicalize()
            .map_err(|_| DaemonError::PolicyDenied("Codegraph workspace is unavailable".into()))?;
        if session_root != binding.root {
            return Err(DaemonError::PolicyDenied(
                "Codegraph workspace scope changed".into(),
            ));
        }
        let current = handle
            .registered_workspace(binding.workspace_id)
            .map_err(|_| DaemonError::Rpc("Codegraph registration is unavailable".into()))?;
        let current = current.ok_or_else(|| {
            DaemonError::PolicyDenied("Codegraph workspace is unregistered".into())
        })?;
        if current.workspace.root() != binding.root
            || current.workspace.project_id() != binding.project_id
        {
            return Err(DaemonError::PolicyDenied(
                "Codegraph workspace scope changed".into(),
            ));
        }
        let scope = BoundCodegraphScope::from_daemon_identity(
            binding.project_id,
            binding.workspace_id,
            kind == NativeCodegraphToolKind::Diff,
        );
        let service = CodegraphReadService::new(&handle);
        let output = match read {
            Some(request) => serde_json::to_value(
                service
                    .read_native(&scope, request)
                    .map_err(native_service_error)?,
            )?,
            None => {
                let mut status = service.status(&scope).map_err(native_service_error)?;
                if status.last_error.is_some() {
                    status.last_error = Some("Indexing failed; inspect daemon logs".into());
                }
                serde_json::to_value(status)?
            }
        };
        // The query engine's token estimate is one token per output byte.
        // Apply the same conservative ceiling to the status projection.
        let output_bytes = output.to_string().len();
        if output_bytes > NATIVE_MAX_OUTPUT_BYTES || output_bytes > NATIVE_MAX_OUTPUT_TOKENS {
            return Err(DaemonError::PolicyDenied(
                "Codegraph native output limit reached".into(),
            ));
        }
        Ok(output)
    })
    .await
    .map_err(|_| DaemonError::Rpc("Codegraph read task failed".into()))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_registration_binds_registered_root_and_status_before_ready() {
        let root = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let index = tempfile::tempdir().unwrap();
        let project_id = Uuid::new_v4();
        let workspace =
            super::super::RegisteredWorkspace::primary(project_id, root.path()).unwrap();
        let workspace_id = workspace.workspace_id();
        let (_manager, handle) =
            super::super::IndexManager::new(index.path().to_path_buf(), vec![workspace]).unwrap();
        assert!(NativeCodegraphBinding::for_launch(&handle, project_id, other.path()).is_none());
        assert!(NativeCodegraphBinding::for_launch(&handle, Uuid::new_v4(), root.path()).is_none());
        let binding = NativeCodegraphBinding::for_launch(&handle, project_id, root.path()).unwrap();
        assert_eq!(binding.workspace_id, workspace_id);
        assert!(binding.permits(NativeCodegraphToolKind::Status));
        assert!(!binding.permits(NativeCodegraphToolKind::Search));
    }

    #[test]
    fn native_schemas_bind_queries_without_caller_scope_fields() {
        let names: Vec<_> = NativeCodegraphToolKind::ALL
            .into_iter()
            .map(NativeCodegraphToolKind::name)
            .collect();
        assert_eq!(names.len(), 5);
        let mut unique = names.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), 5);
        for kind in NativeCodegraphToolKind::ALL {
            let schema = kind.schema();
            assert_eq!(schema["additionalProperties"], false);
            let serialized = schema.to_string();
            for forbidden in [
                "project_id",
                "workspace_id",
                "filesystem_root",
                "socket_path",
                "session_token",
                "caller_session_id",
            ] {
                assert!(
                    !serialized.contains(forbidden),
                    "{forbidden} in {}",
                    kind.name()
                );
            }
            if kind == NativeCodegraphToolKind::Status {
                assert!(schema["properties"].as_object().unwrap().is_empty());
            } else {
                assert_eq!(schema["required"], json!(["read"]));
                assert!(schema["properties"]["read"]["oneOf"].is_array());
            }
        }
    }

    #[test]
    fn native_query_variants_reject_wrong_tool_and_unknown_fields() {
        let search: CodegraphNativeReadV1 = serde_json::from_value(json!({
            "read":{"operation":"search","mode":"exact_name","query":"Node"}
        }))
        .unwrap();
        assert!(NativeCodegraphToolKind::Search.allows(&search.read));
        assert!(!NativeCodegraphToolKind::Explain.allows(&search.read));
        assert!(serde_json::from_value::<CodegraphNativeReadV1>(json!({
            "read":{"operation":"search","mode":"exact_name","query":"Node","project_id":Uuid::new_v4()}
        }))
        .is_err());
        let diff: CodegraphNativeReadV1 = serde_json::from_value(json!({
            "read":{"operation":"diff"}
        }))
        .unwrap();
        assert!(matches!(
            diff.read,
            CodegraphReadV1::Diff {
                baseline_generation: None
            }
        ));
        assert_eq!(
            NativeCodegraphToolKind::Diff.schema()["properties"]["read"]["oneOf"][0]["required"],
            json!(["operation"])
        );
        let explain_schema = NativeCodegraphToolKind::Explain.schema();
        let explain = &explain_schema["properties"]["read"]["oneOf"];
        assert_eq!(explain.as_array().unwrap().len(), 2);
        assert_eq!(explain[0]["required"], json!(["node_id", "operation"]));
        assert_eq!(explain[1]["required"], json!(["relation_id", "operation"]));
        assert_eq!(explain[0]["additionalProperties"], false);
        assert_eq!(explain[1]["additionalProperties"], false);
        let search = NativeCodegraphToolKind::Search.schema();
        let search_schema = &search["properties"]["read"]["oneOf"][0];
        assert!(search_schema["properties"]["node_kinds"]["items"]["enum"].is_array());
        assert_eq!(
            search_schema["properties"]["path_prefix"]["maxLength"],
            rsi_codegraph::MAX_PATH_BYTES
        );
        let relation: CodegraphNativeReadV1 = serde_json::from_value(json!({
            "read":{"operation":"explain","relation_id":Uuid::new_v4()}
        }))
        .unwrap();
        assert!(NativeCodegraphToolKind::Explain.allows(&relation.read));
    }
}

fn native_service_error(error: super::CodegraphServiceError) -> DaemonError {
    use super::CodegraphServiceError as Error;
    match error {
        Error::ScopeDenied | Error::UnsafePath | Error::HistoryDenied => {
            DaemonError::PolicyDenied("Codegraph scope is unavailable".into())
        }
        Error::AmbiguousWorkspace | Error::Invalid(_) | Error::CursorExpired => {
            DaemonError::InvalidParam("invalid Codegraph query".into())
        }
        Error::ResourceLimit => DaemonError::PolicyDenied("Codegraph result limit reached".into()),
        Error::Index(_) | Error::Store(_) => {
            DaemonError::Rpc("Codegraph read is unavailable".into())
        }
    }
}
