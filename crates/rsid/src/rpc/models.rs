use super::*;

#[derive(Debug, Default, Deserialize)]
pub struct DiscoverModelsParams {
    #[serde(default)]
    pub provider: Option<rsi_common::types::SessionProvider>,
    #[serde(default)]
    pub include_capabilities: bool,
}

impl RpcServer {
    #[cfg(test)]
    pub(crate) fn model_control_runtime_for_tests(
        &self,
    ) -> crate::model_control::ModelControlRuntime {
        self.model_control_runtime.clone()
    }

    pub(super) async fn handle_get_model_segments(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        #[derive(Deserialize)]
        struct Params {
            session_id: Uuid,
        }

        let params: Params = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;

        let store = self.session_manager.store().clone();
        let sid = params.session_id;
        let segments = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.load_model_segments(sid)
        })
        .await
        .map_err(|e| DaemonError::Process(format!("Task join error: {}", e)))??;

        Ok(serde_json::to_value(segments)?)
    }

    /// T8 — read-only lifetime usage aggregate for the Settings -> Stats
    /// category. No writes, no schema change; `project_id` is the tab-scoped
    /// filter (D1).
    pub(super) async fn handle_get_usage_stats(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        // `RpcRequest.params` defaults to `Value::Null` (repo rule) when a
        // caller sends no params at all (e.g. `rsi-rpc GetUsageStats` with no
        // `--params`). A derived struct `Deserialize` impl rejects a bare
        // top-level `null` even with `#[serde(default)]` on every field —
        // that attribute only fills in fields absent from a *present*
        // object — so the null case must be checked explicitly here rather
        // than via `.unwrap_or()`-style params handling.
        let params: rsi_common::rpc::GetUsageStatsParams = if request.params.is_null() {
            rsi_common::rpc::GetUsageStatsParams::default()
        } else {
            serde_json::from_value(request.params.clone())
                .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?
        };

        let store = self.session_manager.store().clone();
        let project_id = params.project_id.map(|id| id.to_string());
        let stats = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.usage_stats(project_id.as_deref())
        })
        .await
        .map_err(|e| DaemonError::Process(format!("Task join error: {}", e)))??;

        Ok(serde_json::to_value(stats)?)
    }

    pub(super) async fn handle_get_efficiency_metrics(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: rsi_common::rpc::GetEfficiencyMetricsParams =
            serde_json::from_value(request.params.clone())
                .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;
        let store = self.session_manager.store().clone();
        let project_repo = self.session_manager.workspace_roots().first().cloned();
        let response = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.efficiency_metrics(params, project_repo.as_deref())
        })
        .await
        .map_err(|e| DaemonError::Process(format!("Task join error: {}", e)))??;
        Ok(serde_json::to_value(response)?)
    }

    pub(super) async fn handle_get_model_control_status(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: GetModelControlStatusParams = if request.params.is_null() {
            GetModelControlStatusParams::default()
        } else {
            serde_json::from_value(request.params.clone())
                .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?
        };
        let runtime_snapshot = self.model_control_runtime.snapshot();
        let store = self.session_manager.store().clone();
        let report_result = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.build_model_control_status(params.recent_limit)
        })
        .await
        .map_err(|e| DaemonError::Process(format!("Task join error: {}", e)))?;
        let report = match report_result {
            Ok(report) => report,
            Err(error) if runtime_snapshot.fault.is_some() => {
                let fault = runtime_snapshot.fault.unwrap_or_default();
                rsi_common::model_control::ModelControlStatusReport {
                    mode: ModelControlMode::StopAll,
                    mode_updated_at: runtime_snapshot.updated_at,
                    restart_required_fields: Vec::new(),
                    circuit_state: "faulted".to_string(),
                    circuit_reason: format!(
                        "{fault}; detailed model-control status unavailable: {error}"
                    ),
                    circuits: runtime_snapshot.circuits,
                    policies: Vec::new(),
                    active_invocations: Vec::new(),
                    recent_invocations: Vec::new(),
                    recent_denials: Vec::new(),
                    recent_budget_alerts: Vec::new(),
                }
            }
            Err(error) => return Err(error),
        };
        Ok(serde_json::to_value(report)?)
    }

    pub(super) async fn handle_update_model_control_policy(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: UpdateModelControlPolicyParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::InvalidParam(format!("Invalid params: {}", e)))?;
        let transition = crate::model_control::apply_policy_transition(
            self.session_manager.store(),
            &self.model_control_runtime,
            self.session_manager.event_bus(),
            params.mode,
            params.replace_policies,
            &params.policies,
            &params.circuit_updates,
        )
        .await?;

        let mut interrupted_sessions = Vec::new();
        let mut requested_invocations = Vec::new();
        let mut skipped_invocations = Vec::new();
        let mut skipped_details = Vec::new();

        if params.mode == ModelControlMode::StopAll && params.interrupt_active {
            let mut cursor: Option<String> = None;
            loop {
                let active = tokio::task::spawn_blocking({
                    let store = self.session_manager.store().clone();
                    let cursor = cursor.clone();
                    move || {
                        let store = store.blocking_lock();
                        store.list_cancelable_model_invocation_records_batch(256, cursor.as_deref())
                    }
                })
                .await
                .map_err(|e| DaemonError::Process(format!("Task join error: {}", e)))??;
                if active.is_empty() {
                    break;
                }
                for record in active {
                    cursor = Some(record.id.to_string());
                    let request_outcome = crate::model_control::request_invocation_cancellation(
                        self.session_manager.store(),
                        &self.model_control_runtime,
                        self.session_manager.event_bus(),
                        record.id,
                        "stop_all",
                        "stop_all",
                    )
                    .await?;
                    let current_record = match request_outcome {
                        crate::store::StoreCancellationOutcome::Missing => continue,
                        crate::store::StoreCancellationOutcome::Requested(record) => {
                            requested_invocations.push(record.id);
                            record
                        }
                        crate::store::StoreCancellationOutcome::NoChange(record) => record,
                    };

                    if let Some(session_id) = current_record.owner.session_id {
                        match self
                            .session_manager
                            .interrupt_session_operator(session_id)
                            .await
                        {
                            Ok(()) => interrupted_sessions.push(session_id),
                            Err(error) => {
                                skipped_invocations.push(current_record.id);
                                let reason = format!("interrupt_failed:{error}");
                                skipped_details.push(
                                    rsi_common::model_control::ModelCancellationSkipped {
                                        invocation_id: current_record.id,
                                        reason: reason.clone(),
                                    },
                                );
                                self.session_manager.event_bus().publish(
                                    DaemonEvent::ModelInvocationCancellationSkipped {
                                        invocation_id: current_record.id,
                                        session_id: Some(session_id),
                                        reason,
                                    },
                                );
                            }
                        }
                        continue;
                    }

                    let runtime_cancel = self
                        .model_control_runtime
                        .cancel_invocation(current_record.id);
                    if !(runtime_cancel.request_started || runtime_cancel.already_requested) {
                        skipped_invocations.push(current_record.id);
                        let reason = "no_registered_cancellation_handle".to_string();
                        skipped_details.push(rsi_common::model_control::ModelCancellationSkipped {
                            invocation_id: current_record.id,
                            reason: reason.clone(),
                        });
                        self.session_manager.event_bus().publish(
                            DaemonEvent::ModelInvocationCancellationSkipped {
                                invocation_id: current_record.id,
                                session_id: None,
                                reason,
                            },
                        );
                    }
                }
            }
        }

        let report = rsi_common::model_control::ModelControlPolicyUpdateReport {
            previous_mode: transition.previous_mode,
            current_mode: transition.current_mode,
            updated_at: transition.updated_at,
            live_applied: true,
            restart_required_fields: Vec::new(),
            interrupted_sessions,
            requested_invocations,
            cancelled_invocations: Vec::new(),
            skipped_details,
            skipped_invocations,
        };
        Ok(serde_json::to_value(report)?)
    }

    pub(super) async fn handle_list_model_invocations(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: ListModelInvocationsParams = if request.params.is_null() {
            ListModelInvocationsParams::default()
        } else {
            serde_json::from_value(request.params.clone())
                .map_err(|e| DaemonError::InvalidParam(format!("Invalid params: {}", e)))?
        };
        let store = self.session_manager.store().clone();
        let list = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.build_model_invocation_list(
                params.limit,
                params.active_only,
                params.purpose,
                params.session_id,
            )
        })
        .await
        .map_err(|e| DaemonError::Process(format!("Task join error: {}", e)))??;
        Ok(serde_json::to_value(list)?)
    }

    pub(super) async fn handle_cancel_model_invocation(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: CancelModelInvocationParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::InvalidParam(format!("Invalid params: {}", e)))?;
        let store = self.session_manager.store().clone();
        let record = tokio::task::spawn_blocking({
            let store = store.clone();
            move || {
                let store = store.blocking_lock();
                store.load_model_invocation_record(params.invocation_id)
            }
        })
        .await
        .map_err(|e| DaemonError::Process(format!("Task join error: {}", e)))??;
        let Some(record) = record else {
            return Err(DaemonError::InvalidParam(format!(
                "model invocation not found: {}",
                params.invocation_id
            )));
        };
        let request_outcome = crate::model_control::request_invocation_cancellation(
            self.session_manager.store(),
            &self.model_control_runtime,
            self.session_manager.event_bus(),
            record.id,
            "operator_cancelled",
            "operator_request",
        )
        .await?;
        let (request_recorded, already_requested, current_record) = match request_outcome {
            crate::store::StoreCancellationOutcome::Missing => {
                return Err(DaemonError::InvalidParam(format!(
                    "model invocation not found: {}",
                    params.invocation_id
                )));
            }
            crate::store::StoreCancellationOutcome::Requested(record) => (true, false, record),
            crate::store::StoreCancellationOutcome::NoChange(record) => (
                false,
                record.status
                    == rsi_common::model_control::ModelInvocationStatus::CancellationRequested,
                record,
            ),
        };

        let (mechanism, message) = if let Some(session_id) = current_record.owner.session_id {
            let mechanism = "interrupt_session".to_string();
            if let Err(error) = self
                .session_manager
                .interrupt_session_from(
                    session_id,
                    crate::terminal_cause::InterruptSource::ModelCancel,
                )
                .await
            {
                let reason = format!("interrupt_failed:{error}");
                self.session_manager.event_bus().publish(
                    DaemonEvent::ModelInvocationCancellationSkipped {
                        invocation_id: current_record.id,
                        session_id: Some(session_id),
                        reason: reason.clone(),
                    },
                );
                (mechanism, reason)
            } else {
                (mechanism, "cancellation request recorded".to_string())
            }
        } else {
            let runtime_cancel = self
                .model_control_runtime
                .cancel_invocation(current_record.id);
            if runtime_cancel.request_started {
                (
                    runtime_cancel.mechanism,
                    "runtime cancellation callback triggered".to_string(),
                )
            } else if runtime_cancel.already_requested {
                (
                    runtime_cancel.mechanism,
                    "cancellation already pending".to_string(),
                )
            } else {
                let reason = "no_safe_live_cancellation_path".to_string();
                self.session_manager.event_bus().publish(
                    DaemonEvent::ModelInvocationCancellationSkipped {
                        invocation_id: current_record.id,
                        session_id: None,
                        reason: reason.clone(),
                    },
                );
                (runtime_cancel.mechanism, reason)
            }
        };

        let report = rsi_common::model_control::CancelModelInvocationReport {
            invocation_id: current_record.id,
            request_recorded,
            already_requested,
            cancelled: current_record.status
                == rsi_common::model_control::ModelInvocationStatus::Cancelled,
            mechanism,
            session_id: current_record.owner.session_id,
            final_status: current_record.status,
            message,
        };
        Ok(serde_json::to_value(report)?)
    }

    pub(super) async fn handle_save_compiled_prompt(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: rsi_common::rpc::SaveCompiledPromptParams =
            serde_json::from_value(request.params.clone())?;
        let now = chrono::Utc::now();

        let prompt = rsi_common::types::CompiledPrompt {
            id: Uuid::new_v4(),
            session_id: params.session_id,
            original_input: params.original_input,
            compiled_output: params.compiled_output,
            contract_status: params.contract_status,
            layer_semantic: params.layer_semantic,
            layer_syntactic: params.layer_syntactic,
            layer_deictic: params.layer_deictic,
            layer_discourse: params.layer_discourse,
            layer_pragmatic: params.layer_pragmatic,
            accepted: params.accepted,
            created_at: now,
        };

        let store = self.session_manager.store();
        let guard = store.lock().await;
        guard.insert_compiled_prompt(&prompt)?;

        Ok(serde_json::json!({ "ok": true, "id": prompt.id.to_string() }))
    }

    pub(super) async fn handle_list_compiled_prompts(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: rsi_common::rpc::ListCompiledPromptsParams =
            serde_json::from_value(request.params.clone())?;
        let store = self.session_manager.store();
        let guard = store.lock().await;
        let prompts = guard.list_compiled_prompts(params.session_id.as_ref(), params.limit)?;
        Ok(serde_json::to_value(&prompts)?)
    }

    pub(super) async fn handle_compile_prompt(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: rsi_common::rpc::CompilePromptParams =
            serde_json::from_value(request.params.clone())
                .map_err(|e| DaemonError::InvalidParam(format!("Invalid params: {}", e)))?;

        let resp = Arc::clone(&self.compile_engine)
            .compile(
                params.caller_id,
                params.input,
                params.model,
                params.provider,
                params.base_url,
                params.api_key,
            )
            .await;
        serde_json::to_value(resp).map_err(|e| DaemonError::Rpc(e.to_string()))
    }

    pub(super) async fn handle_generate_text(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: rsi_common::rpc::GenerateTextParams =
            serde_json::from_value(request.params.clone())
                .map_err(|e| DaemonError::InvalidParam(format!("Invalid params: {}", e)))?;

        let model = params.model.unwrap_or_else(|| {
            self.runtime_config
                .prompt_compile_model_local
                .read()
                .clone()
        });
        let provider = params
            .provider
            .unwrap_or_else(|| *self.runtime_config.prompt_compile_model_provider.read());
        let base_url = params.base_url.or_else(|| {
            self.runtime_config
                .prompt_compile_model_base_url
                .read()
                .clone()
        });
        let api_key = params.api_key.or_else(|| {
            self.runtime_config
                .prompt_compile_model_api_key
                .read()
                .clone()
        });

        let system_owned = if model.to_ascii_lowercase().contains("qwen") {
            format!("{}\n/no_think", params.system)
        } else {
            params.system.clone()
        };

        let target = crate::memory::llm::MemoryLlmTarget {
            provider,
            model,
            base_url,
            api_key,
        };
        let provider_label = crate::memory::llm::provider_label(&target)?;
        let backend_label = crate::memory::llm::backend_label(&target)?.to_string();
        let admission_request = crate::model_control::ModelAdmissionRequest {
            purpose: rsi_common::model_control::ModelInvocationPurpose::TextGenerateRpc,
            provider: Some(provider_label.clone()),
            model: Some(target.model.clone()),
            backend: Some(backend_label.clone()),
            effort: None,
            trigger: "GenerateText".to_string(),
            owner: rsi_common::model_control::InvocationOwner {
                operator: Some("generate_text".to_string()),
                ..rsi_common::model_control::InvocationOwner::default()
            },
            dedup_key: Some(crate::model_control::stable_dedup_key(
                "generate-text",
                &[
                    &format!("{provider:?}"),
                    &target.model,
                    target.base_url.as_deref().unwrap_or(""),
                    &params.system,
                    &params.prompt,
                ],
            )),
            request_fingerprint: Some(crate::model_control::hash_request_fingerprint(&[
                &target.model,
                target.base_url.as_deref().unwrap_or(""),
                &params.prompt,
                &params.system,
            ])),
            parent_invocation_id: None,
            retry_of_invocation_id: None,
            expected_usage: Some(crate::model_control::explicit_expected_usage(
                rsi_common::model_control::ModelInvocationPurpose::TextGenerateRpc,
                Some(provider_label.as_str()),
                Some(backend_label.as_str()),
                Some(target.model.as_str()),
            )),
            baseline_input_tokens: 0,
            baseline_output_tokens: 0,
            baseline_cache_creation_tokens: 0,
            baseline_cache_read_tokens: 0,
            baseline_reasoning_tokens: 0,
            baseline_embedding_input_count: 0,
            baseline_wall_time_ms: 0,
        };
        let permit = match crate::model_control::admit_invocation(
            self.session_manager.store(),
            admission_request,
            self.session_manager.event_bus(),
        )
        .await?
        {
            crate::model_control::AdmissionDecision::Admitted(permit) => permit,
            crate::model_control::AdmissionDecision::Duplicate { invocation_id } => {
                return Err(DaemonError::PolicyDenied(format!(
                    "duplicate GenerateText invocation suppressed ({invocation_id})"
                )));
            }
        };
        let started_at = std::time::Instant::now();

        let raw_result = if crate::memory::llm::uses_native_ollama(&target)? {
            let opts = crate::ollama_client::GenerateOptions {
                num_predict: None,
                temperature: 0.3,
                think: false,
                keep_alive: Some("60m".to_string()),
            };
            crate::ollama_client::generate(
                &self.ollama_http,
                &target.model,
                Some(&system_owned),
                &params.prompt,
                opts,
                std::time::Duration::from_secs(120),
            )
            .await
            .map_err(|e| DaemonError::Rpc(e.to_string()))
        } else {
            let prompt = format!("{}\n\n{}", system_owned, params.prompt);
            crate::memory::llm::generate_text(&permit, &target, &prompt, 4096, "GenerateText")
                .await
                .map_err(|e| DaemonError::Rpc(e.to_string()))
        };
        let mut completion = match &raw_result {
            Ok(_) => crate::model_control::completion_with_wall_time(
                started_at,
                None,
                rsi_common::model_control::ModelUsageConfidence::Partial,
            ),
            Err(error) => crate::model_control::completion_with_wall_time(
                started_at,
                Some(crate::model_control::classify_error_class(error)),
                rsi_common::model_control::ModelUsageConfidence::Partial,
            ),
        };
        if let Ok(reply) = &raw_result {
            crate::memory::llm::apply_estimated_usage(
                &mut completion,
                &format!("{}\n\n{}", system_owned, params.prompt),
                reply,
            );
        }
        let raw = crate::model_control::settle_result(
            self.session_manager.store(),
            &permit,
            completion,
            raw_result,
            "GenerateText",
            self.session_manager.event_bus(),
        )
        .await?;

        let cleaned = crate::prompt_compile::post_process::strip_think_tags(&raw);
        let text = cleaned.trim().to_string();
        let resp = rsi_common::rpc::GenerateTextResponse { text };
        serde_json::to_value(resp).map_err(|e| DaemonError::Rpc(e.to_string()))
    }

    pub(super) async fn handle_discover_models(
        &self,
        _request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: DiscoverModelsParams =
            serde_json::from_value(_request.params.clone()).unwrap_or_default();
        let provider = params
            .provider
            .unwrap_or(rsi_common::types::SessionProvider::Claude);
        let models = match provider {
            rsi_common::types::SessionProvider::Claude => {
                let claude_client = match crate::claude::ClaudeClient::for_discovery() {
                    Ok(client) => client,
                    Err(e) => {
                        return Err(DaemonError::Process(format!(
                            "Failed to create Claude client for model discovery: {}",
                            e
                        )));
                    }
                };
                claude_client.discover_models().await?
            }
            rsi_common::types::SessionProvider::Codex => {
                self.session_manager
                    .discover_codex_models(CatalogRefreshReason::ExplicitDiscovery)
                    .await?
            }
            rsi_common::types::SessionProvider::Pioneer => {
                crate::pioneer::discover_pioneer_models()
                    .await
                    .map_err(|error| {
                        DaemonError::Process(format!(
                            "Pioneer model discovery failed: {error}; verify ~/.rsi/.env and restart rsid after credential changes"
                        ))
                    })?
            }
            rsi_common::types::SessionProvider::OpenRouter => {
                crate::openrouter::discover_openrouter_models()
                    .await
                    .map_err(|error| {
                        DaemonError::Process(format!(
                            "OpenRouter model discovery failed: {error}; verify {} and network access",
                            crate::openrouter::OPENROUTER_ENV,
                        ))
                    })?
            }
            rsi_common::types::SessionProvider::Bedrock => {
                crate::bedrock::discover_models().await.map_err(DaemonError::Process)?
            }
            rsi_common::types::SessionProvider::Local => {
                let local_client = crate::openai::OpenAiClient::new_local()?;
                local_client.discover_models().await
            }
            rsi_common::types::SessionProvider::Antigravity => {
                let agy_client = crate::agy::AgyClient::new()?;
                agy_client.discover_models().await
            }
            rsi_common::types::SessionProvider::CodexAppServer => {
                // Reuse Codex model list for app-server mode
                self.session_manager
                    .discover_codex_models(CatalogRefreshReason::ExplicitDiscovery)
                    .await?
            }
            rsi_common::types::SessionProvider::Harness => crate::session::harness::models::harness_models(),
            _ => Vec::new(),
        };
        if params.include_capabilities {
            let effort_capabilities = if matches!(
                provider,
                rsi_common::types::SessionProvider::Codex
                    | rsi_common::types::SessionProvider::CodexAppServer
            ) {
                crate::provider_capabilities::provider_capabilities()
                    .cached_codex_catalog()
                    .map(|snapshot| snapshot.effort_capabilities())
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            return Ok(serde_json::to_value(
                rsi_common::model_utils::DiscoveredModels {
                    models,
                    effort_capabilities,
                },
            )?);
        }
        Ok(serde_json::to_value(&models)?)
    }
}
