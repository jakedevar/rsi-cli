pub mod agent_loop;
pub mod api_key;
pub mod compaction;
pub mod compatible_table;
pub mod errors;
mod normalize;
pub mod provider;
pub mod providers;
pub mod retry;
pub mod sse;
pub mod tools;
pub mod types;

use crate::bus::EventBus;
use crate::claude::LaunchConfig;
use crate::claude::StreamEvent;
use crate::error::Result;
use crate::memory::worker::MemoryHandle;
use crate::model_control::AdmissionPermit;
use crate::model_control::call_control::{
    ModelCallControl, ModelCallSettlementHandle, StoreBackedModelCallControl,
};
use crate::session::harness::agent_loop::run_harness_loop;
use crate::session::harness::provider::resolve_provider;
use crate::session::harness::tools::HarnessToolRegistry;
use crate::session::harness::types::ChatMessage;
use crate::session::types::HarnessProcess;
use crate::store::Store;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub struct HarnessClient {
    codegraph_handle: Option<crate::codegraph::IndexHandle>,
}

impl Default for HarnessClient {
    fn default() -> Self {
        Self::new()
    }
}

impl HarnessClient {
    pub fn new() -> Self {
        Self {
            codegraph_handle: None,
        }
    }

    pub fn set_codegraph_handle(&mut self, handle: crate::codegraph::IndexHandle) {
        self.codegraph_handle = Some(handle);
    }

    pub(super) fn register_codegraph_tools(
        &self,
        tools: &mut HarnessToolRegistry,
        project_id: Option<uuid::Uuid>,
        origin_session_id: Option<uuid::Uuid>,
        working_dir: &Path,
        store: &Arc<Mutex<Store>>,
    ) {
        if let (Some(handle), Some(project_id), Some(session_id)) =
            (&self.codegraph_handle, project_id, origin_session_id)
        {
            if let Some(binding) = crate::codegraph::NativeCodegraphBinding::for_launch(
                handle,
                project_id,
                working_dir,
            ) {
                Self::register_resolved_codegraph_tools(tools, handle, store, session_id, binding);
            }
        }
    }

    fn register_resolved_codegraph_tools(
        tools: &mut HarnessToolRegistry,
        handle: &crate::codegraph::IndexHandle,
        store: &Arc<Mutex<Store>>,
        session_id: uuid::Uuid,
        binding: crate::codegraph::NativeCodegraphBinding,
    ) {
        for kind in crate::codegraph::NativeCodegraphToolKind::ALL {
            if binding.permits(kind) {
                tools.register(Arc::new(tools::codegraph::CodegraphTool::new(
                    kind,
                    handle.clone(),
                    Arc::clone(store),
                    session_id,
                    binding.clone(),
                )));
            }
        }
    }

    /// Launch a harness session, returning the process handle and event receiver.
    ///
    /// `project_id` is the launching session's project scope. It is passed
    /// to the tool registry so `memory_search` is constructed with an
    /// enforced project filter (plan §Phase 5). The agent cannot widen
    /// this scope through tool args.
    ///
    /// `store` and `origin_session_id` enable the `schedule_wake` tool so
    /// agents can schedule future session launches from within a harness run.
    #[allow(clippy::too_many_arguments)]
    pub fn launch(
        &self,
        config: &LaunchConfig,
        resolved_context_budget: &rsi_common::ResolvedContextBudget,
        system_prompt: Option<String>,
        working_dir: &Path,
        conversation_history: Option<Vec<ChatMessage>>,
        initial_admission_permit: AdmissionPermit,
        memory_handle: Option<MemoryHandle>,
        project_id: Option<uuid::Uuid>,
        store: Arc<Mutex<Store>>,
        event_bus: Arc<EventBus>,
        model_call_settlements: ModelCallSettlementHandle,
        origin_session_id: Option<uuid::Uuid>,
        agent_control: Option<crate::session::agent_verbs::AgentControlHandle>,
    ) -> Result<(HarnessProcess, mpsc::Receiver<StreamEvent>)> {
        self.launch_with_binding(
            config,
            resolved_context_budget,
            system_prompt,
            working_dir,
            conversation_history,
            initial_admission_permit,
            memory_handle,
            project_id,
            store,
            event_bus,
            model_call_settlements,
            origin_session_id,
            agent_control,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn launch_with_binding(
        &self,
        config: &LaunchConfig,
        resolved_context_budget: &rsi_common::ResolvedContextBudget,
        system_prompt: Option<String>,
        working_dir: &Path,
        conversation_history: Option<Vec<ChatMessage>>,
        initial_admission_permit: AdmissionPermit,
        memory_handle: Option<MemoryHandle>,
        project_id: Option<uuid::Uuid>,
        store: Arc<Mutex<Store>>,
        event_bus: Arc<EventBus>,
        model_call_settlements: ModelCallSettlementHandle,
        origin_session_id: Option<uuid::Uuid>,
        agent_control: Option<crate::session::agent_verbs::AgentControlHandle>,
        prebound_codegraph: Option<crate::codegraph::NativeCodegraphBinding>,
    ) -> Result<(HarnessProcess, mpsc::Receiver<StreamEvent>)> {
        let mut model = config
            .model
            .clone()
            .unwrap_or_else(|| "claude-sonnet-5".into());
        let api_key = config.openai_api_key.as_deref();
        let base_url = config.openai_base_url.as_deref();

        // Resolve provider based on model name
        let openrouter_route =
            config.provider == Some(rsi_common::types::SessionProvider::OpenRouter);
        let provider_backend: Box<dyn provider::ApiProvider> = if openrouter_route {
            model = crate::openrouter::harness_model_id(&model)
                .ok_or(crate::error::DaemonError::OpenRouterRoutePreflight {
                    cause: "unsupported_model",
                })?
                .to_string();
            Box::new(providers::openai_api::OpenAiApiProvider::openrouter()?)
        } else {
            resolve_provider(&model, base_url, api_key)?
        };
        let provider_name = provider_backend.name().to_string();
        let execution_route = if openrouter_route {
            crate::model_control::registry::RuntimeExecutionRoute::SessionHarnessOpenRouterHttp
        } else if provider_name == "anthropic" {
            crate::model_control::registry::RuntimeExecutionRoute::SessionHarnessAnthropicHttp
        } else {
            crate::model_control::registry::RuntimeExecutionRoute::SessionHarnessOpenAiHttp
        };
        let launch_invocation_id = initial_admission_permit.invocation_id();

        // Build tool registry with memory handle. Project scope is captured
        // here, before the tool registry is frozen into the agent loop.
        let store_for_tools = Some(Arc::clone(&store));
        let mut tools = HarnessToolRegistry::default_tools_with_execution_scratch(
            memory_handle.as_ref(),
            project_id,
            store_for_tools,
            origin_session_id,
            Some(launch_invocation_id),
            config.working_dir.clone(),
            config.provider,
            config.model.clone(),
            agent_control,
            config.execution_scratch.clone(),
        );
        if let (Some(binding), Some(handle), Some(session_id)) = (
            prebound_codegraph,
            self.codegraph_handle.as_ref(),
            origin_session_id,
        ) {
            Self::register_resolved_codegraph_tools(
                &mut tools, handle, &store, session_id, binding,
            );
        } else {
            self.register_codegraph_tools(
                &mut tools,
                project_id,
                origin_session_id,
                working_dir,
                &store,
            );
        }
        let tools = Arc::new(tools);

        let (event_tx, event_rx) = mpsc::channel(256);
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

        let system_prompt = system_prompt.unwrap_or_default();
        let query = config.query.clone();
        let reasoning_effort = config.effort.clone();
        let working_dir = working_dir.to_path_buf();
        // Harness compaction needs the full model window. The context pipeline's
        // separately bounded 2% value is only an injection allowance.
        let token_limit = resolved_context_budget.active_tokens;
        let model_call_control: Arc<dyn ModelCallControl> =
            Arc::new(StoreBackedModelCallControl::new(
                store,
                Arc::clone(&event_bus),
                model_call_settlements,
                rsi_common::model_control::InvocationOwner {
                    session_id: config.rsi_session_id,
                    project_id,
                    ..Default::default()
                },
                "Harness",
                Some(model.clone()),
                provider_name.clone(),
                config.effort.clone(),
                "session_harness",
                initial_admission_permit,
                rsi_common::model_control::ModelInvocationPurpose::SessionHarnessTurn,
                Some(rsi_common::model_control::ModelInvocationPurpose::SessionHarnessCompaction),
                execution_route,
            ));

        let task_handle = tokio::spawn(async move {
            let result = run_harness_loop(
                provider_backend,
                tools,
                system_prompt,
                query,
                working_dir,
                model,
                reasoning_effort,
                event_tx.clone(),
                cancel_clone,
                model_call_control,
                25, // max_iterations
                token_limit,
                conversation_history,
            )
            .await;

            if let Err(e) = result {
                let typed = errors::ProviderError::from_daemon_error(&e);
                let _ = event_tx
                    .send(StreamEvent {
                        event_type: "process_error".into(),
                        data: typed.map_or_else(
                            || serde_json::json!({ "error": e.to_string() }),
                            |error| {
                                serde_json::json!({
                                    "error": format!("provider_error:{}", error.detail_code),
                                    "source": "harness",
                                    "terminal": true,
                                    "error_class": error.class,
                                    "http_status": error.http_status,
                                    "retry_after_ms": error.retry_after_ms,
                                    "detail_code": error.detail_code,
                                })
                            },
                        ),
                    })
                    .await;
            }
        });

        Ok((
            HarnessProcess {
                cancel,
                task_handle,
            },
            event_rx,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn prebound_ready_codegraph_tools_survive_slot_removal_before_launch() {
        use crate::codegraph::{
            IndexPhase, IndexRuntime, NativeCodegraphBinding, NativeCodegraphToolKind,
        };
        use rsi_common::types::Project;
        let root = tempfile::tempdir().unwrap();
        let indexes = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("lib.rs"), "pub fn visible() {}\n").unwrap();
        let project_id = uuid::Uuid::new_v4();
        let project = Project {
            id: project_id,
            name: "native tools".into(),
            path: Some(root.path().to_path_buf()),
            description: None,
            color: Project::DEFAULT_COLOR.into(),
            context_files: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let runtime =
            IndexRuntime::start(indexes.path().to_path_buf(), vec![project], &[]).unwrap();
        runtime.handle().set_enabled(true);
        let workspace_id =
            rsi_codegraph::CodegraphStore::workspace_id(project_id, "primary").unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if runtime
                    .handle()
                    .status(workspace_id)
                    .is_some_and(|item| item.phase == IndexPhase::Ready)
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        let binding =
            NativeCodegraphBinding::for_launch(runtime.handle(), project_id, root.path()).unwrap();
        runtime.handle().reconcile(Vec::new()).unwrap();
        let mut tools = HarnessToolRegistry::new();
        HarnessClient::register_resolved_codegraph_tools(
            &mut tools,
            runtime.handle(),
            &Arc::new(Mutex::new(Store::open_in_memory().unwrap())),
            uuid::Uuid::new_v4(),
            binding,
        );
        let names = tools
            .specs()
            .into_iter()
            .map(|spec| spec.name)
            .collect::<std::collections::HashSet<_>>();
        for kind in NativeCodegraphToolKind::ALL {
            assert!(
                names.contains(kind.name()),
                "ready native tool missing: {}",
                kind.name()
            );
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn harness_compaction_window_stays_distinct_from_injection_allowance() {
        let model = "claude-sonnet-5";
        let budget = crate::provider_capabilities::resolve_fresh_context_budget(
            rsi_common::types::SessionProvider::Harness,
            model,
            None,
        );
        assert_eq!(budget.active_tokens, 1_000_000);
        assert_eq!(
            crate::session::context_pipeline::context_injection_allowance(1_000_000),
            20_000
        );
    }
}
