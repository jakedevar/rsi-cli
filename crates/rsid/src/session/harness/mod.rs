pub mod agent_loop;
pub(crate) mod agent_mail;
pub mod api_key;
mod bedrock_stream;
pub mod compaction;
pub mod compatible_table;
pub mod egress;
pub mod errors;
pub mod models;
mod normalize;
pub mod provider;
pub mod providers;
pub mod retry;
// wired in slice 3
#[allow(dead_code)]
pub mod mcp;
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
use crate::session::harness::agent_loop::run_harness_loop_with_compact_budget;
use crate::session::harness::agent_mail::{HarnessAgentMailBoundary, HarnessMailBoundary};
use crate::session::harness::provider::resolve_provider;
use crate::session::harness::tools::HarnessToolRegistry;
use crate::session::harness::tools::policy::ToolPolicyRuntime;
use crate::session::harness::types::ChatMessage;
use crate::session::types::HarnessProcess;
use crate::store::Store;
use rsi_common::harness_tool_policy::HarnessToolPolicy;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub struct HarnessClient {
    codegraph_handle: Option<crate::codegraph::IndexHandle>,
    /// #966: read at each launch for the OpenRouter context budget.
    runtime_config: Option<Arc<crate::config::RuntimeConfig>>,
    agent_message_arbiter: Option<Arc<crate::session::agent_message_arbiter::AgentMessageArbiter>>,
    process_registry_manager: Option<Arc<tools::process_registry::HarnessProcessRegistryManager>>,
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
            runtime_config: None,
            agent_message_arbiter: None,
            process_registry_manager: None,
        }
    }

    /// #1050: the operator's per-turn iteration cap; the built-in default
    /// applies when no runtime config is attached.
    fn turn_iteration_cap(&self) -> u32 {
        self.runtime_config.as_ref().map_or(
            crate::config::HARNESS_MAX_ITERATIONS_PER_TURN_DEFAULT,
            |runtime| runtime.harness_max_iterations(),
        )
    }

    fn mcp_deferred_tool_threshold(&self) -> usize {
        self.runtime_config.as_ref().map_or(
            crate::config::MCP_DEFERRED_TOOL_THRESHOLD_DEFAULT,
            |config| config.mcp_deferred_tool_threshold(),
        )
    }

    pub fn set_runtime_config(&mut self, runtime_config: Arc<crate::config::RuntimeConfig>) {
        self.runtime_config = Some(runtime_config);
    }

    pub fn set_agent_message_arbiter(
        &mut self,
        arbiter: Arc<crate::session::agent_message_arbiter::AgentMessageArbiter>,
    ) {
        self.agent_message_arbiter = Some(arbiter);
    }

    pub fn set_process_registry_manager(
        &mut self,
        manager: Arc<tools::process_registry::HarnessProcessRegistryManager>,
    ) {
        self.process_registry_manager = Some(manager);
    }

    /// The session policy layered over the daemon-wide defaults.
    fn effective_tool_policy(&self, session: Option<&HarnessToolPolicy>) -> HarnessToolPolicy {
        let defaults = self
            .runtime_config
            .as_ref()
            .map(|config| config.harness_tool_policy_defaults())
            .unwrap_or_default();
        session.map_or(defaults.clone(), |policy| policy.or_defaults(&defaults))
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
        monitor_generation: u64,
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
        session_tool_policy: Option<HarnessToolPolicy>,
    ) -> Result<(HarnessProcess, mpsc::Receiver<StreamEvent>)> {
        self.launch_with_binding(
            monitor_generation,
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
            session_tool_policy,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn launch_with_binding(
        &self,
        monitor_generation: u64,
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
        session_tool_policy: Option<HarnessToolPolicy>,
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
        let bedrock_route = config.provider == Some(rsi_common::types::SessionProvider::Bedrock);
        let provider_backend: Box<dyn provider::ApiProvider> = if openrouter_route {
            model = crate::openrouter::harness_model_id(&model)
                .ok_or(crate::error::DaemonError::OpenRouterRoutePreflight {
                    cause: "unsupported_model",
                })?
                .to_string();
            Box::new(providers::openai_api::OpenAiApiProvider::openrouter()?)
        } else if bedrock_route {
            Box::new(providers::openai_responses::OpenAiResponsesProvider::bedrock()?)
        } else {
            resolve_provider(&model, base_url, api_key)?
        };
        let provider_name = provider_backend.name().to_string();
        let image_input_supported = provider_backend.supports_image_input(&model);
        let agent_mail_boundary = if config.provider
            == Some(rsi_common::types::SessionProvider::Harness)
            && let (Some(arbiter), Some(session_id)) =
                (&self.agent_message_arbiter, config.rsi_session_id)
        {
            Some(Arc::new(HarnessAgentMailBoundary::new(
                Arc::clone(&store),
                Arc::clone(arbiter),
                session_id,
                monitor_generation,
            )) as Arc<dyn HarnessMailBoundary>)
        } else {
            None
        };
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
        let process_registry = match (&self.process_registry_manager, config.rsi_session_id) {
            (Some(manager), Some(session_id)) => manager.resolve(session_id),
            _ => tools::exec::default_process_registry(),
        };
        let mut tools = HarnessToolRegistry::default_tools_with_process_registry(
            memory_handle.as_ref(),
            project_id,
            store_for_tools,
            origin_session_id,
            Some(launch_invocation_id),
            config.working_dir.clone(),
            config.provider,
            config.model.clone(),
            agent_control,
            image_input_supported,
            config.execution_scratch.clone(),
            process_registry,
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
        let (event_tx, event_rx) = mpsc::channel(256);
        // Provider dispatch is synchronous, but the daemon runtime is
        // multi-threaded. Keep the process launch off its worker without
        // widening this launch path into a provider-trait refactor.
        let mut mcp_bridge = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                crate::session::harness::mcp::McpBridge::build(&store, &crate::vault::global())
                    .await
            })
        });
        // #792: the session's stored or inherited policy, layered over the
        // daemon defaults. An unrestricted result installs no runtime at all.
        let effective_policy = self.effective_tool_policy(session_tool_policy.as_ref());
        if !effective_policy.is_default() {
            tools.set_policy(Arc::new(ToolPolicyRuntime::new(effective_policy)));
        }
        mcp_bridge.register_with_threshold(&mut tools, self.mcp_deferred_tool_threshold());
        let mcp_events = mcp_bridge.take_events();
        let tools = Arc::new(tools);

        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

        let system_prompt = system_prompt.unwrap_or_default();
        let query = config.query.clone();
        let reasoning_effort = config.effort.clone();
        let working_dir = working_dir.to_path_buf();
        // Harness compaction needs the full model window. The context pipeline's
        // separately bounded 2% value is only an injection allowance.
        let token_limit = resolved_context_budget.active_tokens;
        // #966: OpenRouter sessions compact at the operator's absolute budget.
        let compact_budget = if openrouter_route {
            self.runtime_config
                .as_ref()
                .and_then(|runtime| runtime.openrouter_context_budget())
        } else {
            None
        };
        let completion_gates = config.completion_gates.clone();
        let completion_gates_enabled = self
            .runtime_config
            .as_ref()
            .map_or(true, |runtime| runtime.completion_gates_enabled());
        // #1050: the operator's per-turn iteration cap, read at turn start.
        let max_iterations = self.turn_iteration_cap();
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
            for event in mcp_events {
                if event_tx.send(event).await.is_err() {
                    break;
                }
            }
            let result = run_harness_loop_with_compact_budget(
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
                max_iterations,
                token_limit,
                conversation_history,
                compact_budget,
                agent_mail_boundary,
                completion_gates,
                completion_gates_enabled,
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

    /// #1111: the daemon default decides context editing for a launch that sets
    /// nothing, an explicit session policy overrides it, and a policy resolved
    /// at an earlier launch keeps its value when the default changes.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn context_editing_resolves_session_policy_then_daemon_default() {
        let mut client = HarnessClient::new();
        // No runtime config: the built-in default is on.
        assert!(client.effective_tool_policy(None).context_editing_enabled());
        let runtime = Arc::new(crate::config::RuntimeConfig::from_config(
            &crate::config::Config::default(),
        ));
        client.set_runtime_config(Arc::clone(&runtime));
        let launched_before = client.effective_tool_policy(None);
        assert!(launched_before.context_editing_enabled());

        runtime
            .update_field("harness_context_editing", &serde_json::json!(false))
            .unwrap();
        assert!(!client.effective_tool_policy(None).context_editing_enabled());
        let unset = HarnessToolPolicy::default();
        assert!(
            !client
                .effective_tool_policy(Some(&unset))
                .context_editing_enabled()
        );
        let explicit_on = HarnessToolPolicy {
            context_editing: Some(true),
            ..HarnessToolPolicy::default()
        };
        assert!(
            client
                .effective_tool_policy(Some(&explicit_on))
                .context_editing_enabled()
        );
        // The session launched before the change keeps what it launched with.
        assert!(launched_before.context_editing_enabled());

        runtime
            .update_field("harness_context_editing", &serde_json::json!(true))
            .unwrap();
        let explicit_off = HarnessToolPolicy {
            context_editing: Some(false),
            ..HarnessToolPolicy::default()
        };
        assert!(
            !client
                .effective_tool_policy(Some(&explicit_off))
                .context_editing_enabled()
        );
        assert!(client.effective_tool_policy(None).context_editing_enabled());
    }

    /// #1050: a turn reads the operator's iteration cap when it starts.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn turn_iteration_cap_follows_the_operator_setting() {
        let mut client = HarnessClient::new();
        assert_eq!(client.turn_iteration_cap(), 150);
        let runtime = Arc::new(crate::config::RuntimeConfig::from_config(
            &crate::config::Config::default(),
        ));
        client.set_runtime_config(Arc::clone(&runtime));
        assert_eq!(client.turn_iteration_cap(), 150);
        runtime
            .update_field("harness_max_iterations_per_turn", &serde_json::json!(40))
            .unwrap();
        assert_eq!(client.turn_iteration_cap(), 40);
    }

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
