use super::*;

pub(super) fn codegraph_rpc_error(error: crate::codegraph::CodegraphServiceError) -> DaemonError {
    use crate::codegraph::CodegraphServiceError as Error;
    match error {
        Error::ScopeDenied | Error::UnsafePath => {
            DaemonError::PolicyDenied("codegraph scope is unavailable".into())
        }
        Error::HistoryDenied => DaemonError::PolicyDenied("codegraph history is denied".into()),
        Error::CursorExpired => DaemonError::InvalidParam("codegraph_cursor_expired".into()),
        Error::AmbiguousWorkspace => {
            DaemonError::InvalidParam("codegraph workspace is ambiguous".into())
        }
        Error::Invalid(_) => DaemonError::InvalidParam("invalid codegraph query".into()),
        Error::ResourceLimit => DaemonError::PolicyDenied("codegraph result limit reached".into()),
        Error::Index(_) | Error::Store(_) => {
            DaemonError::Rpc("codegraph read is unavailable".into())
        }
    }
}

impl RpcServer {
    /// Return the current runtime-mutable daemon config as a JSON object.
    ///
    /// All keys in the response are valid `field` values for
    /// `UpdateDaemonConfig`. Includes:
    /// - boolean toggles (e.g. `retry_enabled`, `dream_enabled`)
    /// - numeric counters (e.g. `retry_max_default`, `dream_observation_threshold`)
    /// - model name strings (e.g. `title_model_local`, `dream_model`)
    /// - `codex_sandbox_mode` — one of `"read-only"`, `"workspace-write"`,
    ///   `"danger-full-access"`. Drives the `--sandbox` arg on fresh
    ///   `codex exec` launches; resume launches ignore it (Codex CLI
    ///   inherits the original session's sandbox policy).
    pub(super) async fn handle_get_codegraph_capabilities(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        if !request.params.is_null() && request.params != serde_json::json!({}) {
            return Err(DaemonError::InvalidParam(
                "Codegraph capabilities accept no parameters".into(),
            ));
        }
        let available = self.codegraph_handle.is_some();
        let capabilities = rsi_common::codegraph::CodegraphCapabilitiesV1 {
            wire_version: rsi_common::codegraph::CODEGRAPH_WIRE_VERSION,
            metadata_schema_version: 1,
            available,
            indexing_enabled: self
                .runtime_config
                .codegraph_indexing_enabled
                .load(std::sync::atomic::Ordering::Acquire),
            supported_read_methods: if available {
                [
                    "ListCodegraphWorkspaces",
                    "GetCodegraphStatus",
                    "GetCodegraphSnapshot",
                    "ListCodegraphSnapshots",
                    "SearchCodegraph",
                    "ExplainCodegraph",
                    "GetCodegraphNeighbors",
                    "FindCodegraphPath",
                    "GetCodegraphSubgraph",
                    "GetCodegraphImpact",
                    "DiffCodegraph",
                ]
                .into_iter()
                .map(str::to_string)
                .collect()
            } else {
                Vec::new()
            },
            node_kinds: rsi_codegraph::NodeKind::ALL
                .iter()
                .map(|kind| kind.as_str().to_string())
                .collect(),
            relation_kinds: rsi_codegraph::RelationKind::ALL
                .iter()
                .map(|kind| kind.as_str().to_string())
                .collect(),
            historical_reads: available,
            fts_search: available,
            federation: false,
            native_tools: if available {
                crate::codegraph::NativeCodegraphToolKind::ALL
                    .into_iter()
                    .map(|kind| kind.name().to_string())
                    .collect()
            } else {
                Vec::new()
            },
            max_query_results: rsi_codegraph::MAX_QUERY_LIMIT,
            max_operator_output_bytes: rsi_codegraph::MAX_QUERY_OUTPUT_BYTES,
            max_native_output_bytes: crate::codegraph::NATIVE_MAX_OUTPUT_BYTES,
            max_native_output_tokens: crate::codegraph::NATIVE_MAX_OUTPUT_TOKENS,
            max_snapshot_page_size: 32,
            max_workspace_page_size: 32,
        };
        Ok(serde_json::to_value(capabilities)?)
    }

    pub(super) async fn handle_list_codegraph_workspaces(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: rsi_common::codegraph::CodegraphWorkspacePageRequestV1 =
            serde_json::from_value(request.params.clone())
                .map_err(|_| DaemonError::InvalidParam("invalid Codegraph project ID".into()))?;
        let handle = self
            .codegraph_handle
            .clone()
            .ok_or_else(|| DaemonError::Rpc("codegraph index service is unavailable".into()))?;
        let store = Arc::clone(self.session_manager.store());
        let workspaces = tokio::task::spawn_blocking(move || {
            crate::codegraph::CodegraphReadService::with_store(&handle, store)
                .list_workspaces(params)
        })
        .await
        .map_err(|_| DaemonError::Rpc("codegraph read task failed".into()))?
        .map_err(codegraph_rpc_error)?;
        Ok(serde_json::to_value(workspaces)?)
    }

    pub(super) async fn handle_get_codegraph_status(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let scope: rsi_common::codegraph::CodegraphScopeV1 =
            serde_json::from_value(request.params.clone())
                .map_err(|_| DaemonError::InvalidParam("invalid Codegraph scope".into()))?;
        let handle = self
            .codegraph_handle
            .clone()
            .ok_or_else(|| DaemonError::Rpc("codegraph index service is unavailable".into()))?;
        let store = Arc::clone(self.session_manager.store());
        let status = tokio::task::spawn_blocking(move || {
            let service = crate::codegraph::CodegraphReadService::with_store(&handle, store);
            let bound = service.resolve_operator_scope(scope, false)?;
            service.status(&bound)
        })
        .await
        .map_err(|_| DaemonError::Rpc("codegraph read task failed".into()))?
        .map_err(codegraph_rpc_error)?;
        Ok(serde_json::to_value(status)?)
    }

    pub(super) async fn handle_get_codegraph_snapshot(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: rsi_common::codegraph::CodegraphSnapshotRequestV1 =
            serde_json::from_value(request.params.clone()).map_err(|_| {
                DaemonError::InvalidParam("invalid Codegraph snapshot request".into())
            })?;
        let handle = self
            .codegraph_handle
            .clone()
            .ok_or_else(|| DaemonError::Rpc("codegraph index service is unavailable".into()))?;
        let store = Arc::clone(self.session_manager.store());
        let snapshot = tokio::task::spawn_blocking(move || {
            crate::codegraph::CodegraphReadService::with_store(&handle, store)
                .snapshot_at(params.scope, params.generation)
        })
        .await
        .map_err(|_| DaemonError::Rpc("codegraph read task failed".into()))?
        .map_err(codegraph_rpc_error)?;
        Ok(serde_json::to_value(snapshot)?)
    }

    pub(super) async fn handle_list_codegraph_snapshots(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: rsi_common::codegraph::CodegraphSnapshotPageRequestV1 =
            serde_json::from_value(request.params.clone()).map_err(|_| {
                DaemonError::InvalidParam("invalid Codegraph snapshot page request".into())
            })?;
        let handle = self
            .codegraph_handle
            .clone()
            .ok_or_else(|| DaemonError::Rpc("codegraph index service is unavailable".into()))?;
        let store = Arc::clone(self.session_manager.store());
        let page = tokio::task::spawn_blocking(move || {
            crate::codegraph::CodegraphReadService::with_store(&handle, store)
                .list_snapshots(params)
        })
        .await
        .map_err(|_| DaemonError::Rpc("codegraph read task failed".into()))?
        .map_err(codegraph_rpc_error)?;
        Ok(serde_json::to_value(page)?)
    }

    pub(super) async fn handle_codegraph_read(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        use rsi_common::codegraph::CodegraphReadV1;
        let params: rsi_common::codegraph::CodegraphOperatorReadV1 =
            serde_json::from_value(request.params.clone())
                .map_err(|_| DaemonError::InvalidParam("invalid Codegraph read request".into()))?;
        let matching_method = matches!(
            (request.method.as_str(), &params.read),
            ("SearchCodegraph", CodegraphReadV1::Search { .. })
                | ("ExplainCodegraph", CodegraphReadV1::Explain { .. })
                | ("GetCodegraphNeighbors", CodegraphReadV1::Neighbors { .. })
                | ("FindCodegraphPath", CodegraphReadV1::Path { .. })
                | ("GetCodegraphSubgraph", CodegraphReadV1::Subgraph { .. })
                | ("GetCodegraphImpact", CodegraphReadV1::Impact { .. })
                | ("DiffCodegraph", CodegraphReadV1::Diff { .. })
        );
        if !matching_method {
            return Err(DaemonError::InvalidParam(
                "Codegraph operation does not match RPC method".into(),
            ));
        }
        let handle = self
            .codegraph_handle
            .clone()
            .ok_or_else(|| DaemonError::Rpc("codegraph index service is unavailable".into()))?;
        let store = Arc::clone(self.session_manager.store());
        let result = tokio::task::spawn_blocking(move || {
            crate::codegraph::CodegraphReadService::with_store(&handle, store)
                .read_operator(params, true)
        })
        .await
        .map_err(|_| DaemonError::Rpc("codegraph read task failed".into()))?
        .map_err(codegraph_rpc_error)?;
        Ok(serde_json::to_value(result)?)
    }
}
