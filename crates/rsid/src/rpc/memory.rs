use super::*;

/// RPC params for saving an ESP game.
#[derive(Debug, Deserialize)]
pub struct SaveEspGameParams {
    pub score: u8,
    pub rounds_played: u8,
    pub total_rounds: u8,
    pub p_value: f64,
    pub round_details: String,
}

/// RPC params for listing ESP games.
#[derive(Debug, Default, Deserialize)]
pub struct ListEspGamesParams {
    #[serde(default = "default_esp_limit")]
    pub limit: usize,
}

pub(super) fn default_esp_limit() -> usize {
    50
}

impl RpcServer {
    pub(super) async fn handle_get_dream_status(
        &self,
        _request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        match &self.dreamer_handle {
            Some(handle) => Ok(serde_json::to_value(handle.status().await)?),
            None => Err(DaemonError::Rpc("Dreamer is not available".to_string())),
        }
    }

    pub(super) async fn handle_trigger_dream(
        &self,
        _request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        match &self.dreamer_handle {
            Some(handle) => {
                let snapshot = handle.trigger_now().await?;
                Ok(serde_json::json!({ "triggered": true, "status": snapshot }))
            }
            None => Err(DaemonError::Rpc("Dreamer is not available".to_string())),
        }
    }

    pub(super) async fn handle_query_memory(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: rsi_common::rpc::QueryMemoryParams =
            serde_json::from_value(request.params.clone())
                .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;

        let engine = self.dialectic_engine.as_ref().ok_or_else(|| {
            DaemonError::Rpc(
                "Dialectic query engine is not enabled. Configure RSI_DIALECTIC_URL or set RSI_DIALECTIC_ENABLED=true."
                    .to_string(),
            )
        })?;

        let response = engine
            .query(
                &params.query,
                params.project_id,
                &params.conversation_history,
            )
            .await
            .map_err(|e| DaemonError::Rpc(format!("Dialectic query failed: {}", e)))?;

        Ok(serde_json::to_value(&response)?)
    }

    pub(super) fn require_memory(&self) -> Result<&Arc<crate::memory::manager::MemoryManager>> {
        self.memory_manager
            .as_ref()
            .ok_or_else(|| DaemonError::Rpc("Memory system is not enabled".to_string()))
    }

    pub(super) async fn handle_memory_search(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let memory = self.require_memory()?;
        let params: MemorySearchParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;

        if params.query.trim().is_empty() {
            return Ok(serde_json::json!([]));
        }

        let results = memory
            .search(
                &params.query,
                params.max_results,
                params.min_score,
                params.project_id,
            )
            .await?;

        // Convert internal MemorySearchResult → rsi_common::rpc::MemorySearchResult
        let rpc_results: Vec<rsi_common::rpc::MemorySearchResult> = results
            .into_iter()
            .map(|r| rsi_common::rpc::MemorySearchResult {
                path: r.path,
                start_line: r.start_line,
                end_line: r.end_line,
                score: r.score,
                snippet: r.snippet,
                source: r.source.as_str().to_string(),
            })
            .collect();

        Ok(serde_json::to_value(&rpc_results)?)
    }

    pub(super) async fn handle_memory_status(
        &self,
        _request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let memory = self.require_memory()?;
        let status = memory.status().await?;

        // Convert internal → rsi_common::rpc::MemoryProviderStatus
        let rpc_status = rsi_common::rpc::MemoryProviderStatus {
            enabled: true,
            backend: status.backend,
            provider: status.provider,
            model: status.model,
            search_mode: if status.vector_available {
                "hybrid".to_string()
            } else {
                "fts".to_string()
            },
            file_count: status.file_count as usize,
            chunk_count: status.chunk_count as usize,
            dirty: status.dirty,
            memory_dir: String::new(), // filled by caller if needed
            db_path: status.db_path,
            vector_available: status.vector_available,
            fts_available: status.fts_available,
            cache_entries: status.cache_entries as usize,
        };

        Ok(serde_json::to_value(&rpc_status)?)
    }

    pub(super) async fn handle_memory_index(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let memory = self.require_memory()?;
        let params: MemoryIndexParams =
            serde_json::from_value(request.params.clone()).unwrap_or_default();

        memory.sync(params.force).await?;
        Ok(serde_json::json!({ "ok": true }))
    }

    pub(super) async fn handle_memory_read(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let memory = self.require_memory()?;
        let params: MemoryReadParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;

        let (text, path) = memory
            .read_file(&params.path, params.from, params.lines)
            .await?;

        Ok(serde_json::json!({ "text": text, "path": path }))
    }

    pub(super) async fn handle_list_observations(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let memory = self.require_memory()?;
        let params: ListObservationsParams =
            serde_json::from_value(request.params.clone()).unwrap_or_default();

        let observations = memory
            .list_observations(params.session_id, params.project_id, params.limit)
            .await?;

        Ok(serde_json::to_value(&observations)?)
    }

    pub(super) async fn handle_search_observations(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let memory = self.require_memory()?;
        let params: SearchObservationsParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;

        if params.query.trim().is_empty() {
            return Ok(serde_json::json!([]));
        }

        let results = memory
            .search_observations(&params.query, params.max_results, params.project_id)
            .await?;

        Ok(serde_json::to_value(&results)?)
    }

    pub(super) async fn handle_get_entity_card(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: rsi_common::rpc::GetEntityCardParams =
            serde_json::from_value(request.params.clone())
                .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;
        let card = self
            .session_manager
            .get_entity_card(&params.entity_type, &params.entity_id)
            .await?;
        Ok(serde_json::to_value(&card)?)
    }

    pub(super) async fn handle_set_entity_card(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: rsi_common::rpc::SetEntityCardParams =
            serde_json::from_value(request.params.clone())
                .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;
        let card = self
            .session_manager
            .set_entity_card(params.entity_type, params.entity_id, params.facts)
            .await?;
        Ok(serde_json::to_value(&card)?)
    }

    pub(super) async fn handle_save_esp_game(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: SaveEspGameParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;
        let game = rsi_common::types::EspGame {
            id: uuid::Uuid::new_v4(),
            played_at: chrono::Utc::now(),
            score: params.score,
            rounds_played: params.rounds_played,
            total_rounds: params.total_rounds,
            p_value: params.p_value,
            round_details: params.round_details,
        };
        self.session_manager.save_esp_game(game.clone()).await?;
        Ok(serde_json::to_value(&game)?)
    }

    pub(super) async fn handle_list_esp_games(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: ListEspGamesParams =
            serde_json::from_value(request.params.clone()).unwrap_or_default();
        let games = self.session_manager.list_esp_games(params.limit).await?;
        Ok(serde_json::to_value(&games)?)
    }

    pub(super) async fn handle_clear_graph_cache(
        &self,
        _request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        // Clear in-memory cache.
        let mut cache = self.session_manager.graph_cache().lock().await;
        let count = cache.len();
        cache.clear();
        drop(cache);

        // Clear SQLite-backed cache.
        let store = self.session_manager.store().lock().await;
        if let Err(e) = crate::store::graph_cache::clear_cache(&store.conn) {
            tracing::warn!("Failed to clear SQLite graph cache: {}", e);
        }

        Ok(serde_json::json!({ "cleared": true, "entries_removed": count }))
    }

    pub(super) async fn handle_update_index_status(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: UpdateIndexStatusParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;
        let mgr = self.session_manager.clone();
        tokio::task::spawn_blocking(move || mgr.update_index_status(params))
            .await
            .map_err(|e| DaemonError::Rpc(format!("spawn_blocking join error: {}", e)))??;
        Ok(serde_json::json!({"ok": true}))
    }

    pub(super) async fn handle_get_index_status(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: GetIndexStatusParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;
        let mgr = self.session_manager.clone();
        let sidecar = tokio::task::spawn_blocking(move || mgr.get_index_status(params))
            .await
            .map_err(|e| DaemonError::Rpc(format!("spawn_blocking join error: {}", e)))??;
        Ok(serde_json::to_value(&sidecar)?)
    }
}
