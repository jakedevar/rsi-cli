pub mod agent_loop;
pub(crate) mod agent_mail;
pub mod api_key;
pub(crate) mod base_prompt;
mod bedrock_stream;
pub mod compaction;
pub mod compatible_table;
#[cfg(test)]
mod e2e_tests;
pub mod egress;
pub mod errors;
mod loop_ports;
pub mod models;
pub(crate) mod net;
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
use crate::session::harness::errors::ProviderErrorExt as _;
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

    /// Register the native `web_search` tool when the operator's backend choice
    /// names a backend that is configured: its vault key resolves, or for
    /// SearXNG the operator set `harness_searxng_url` (#2251, #2315). Otherwise nothing is
    /// registered, so an unconfigured tool never wastes an autonomous call; the
    /// returned `system` notice tells the operator why (none when the operator
    /// disabled the tool, when the session policy would not advertise it, or
    /// when the route offers its own hosted search).
    pub(super) fn register_web_search(
        &self,
        tools: &mut HarnessToolRegistry,
        vault: &crate::vault::VaultHandle,
        route_has_hosted_search: bool,
    ) -> Option<StreamEvent> {
        let choice = self
            .runtime_config
            .as_ref()
            .map(|config| config.harness_web_search_backend())
            .unwrap_or_default();
        let searxng_url = self
            .runtime_config
            .as_ref()
            .and_then(|config| config.harness_searxng_url());
        match tools::web_search::select_backend(choice, vault, searxng_url.as_deref()) {
            tools::web_search::BackendSelection::Ready(backend) => {
                tools.register(Arc::new(tools::web_search::WebSearchTool::new(backend)));
                None
            }
            tools::web_search::BackendSelection::Unavailable(unavailable) => {
                let advertised_otherwise = route_has_hosted_search
                    || tools.policy().is_some_and(|policy| {
                        policy
                            .tool_permitted(rsi_common::harness_tool_policy::WEB_SEARCH_TOOL, false)
                            .is_err()
                    });
                if advertised_otherwise {
                    None
                } else {
                    unavailable.notice()
                }
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
        // #1183: every session on this tool loop takes mail between its
        // model calls: Harness, and the Harness routes of OpenRouter and
        // Bedrock (their Codex routes use the Codex PostToolUse hook).
        let agent_mail_boundary = if let (Some(arbiter), Some(session_id)) =
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
        // #2251: the native search tool, only when a backend key resolves. The
        // policy is set first so a refused tool raises no notice.
        let web_search_notice = self.register_web_search(
            &mut tools,
            &crate::vault::global(),
            provider_backend
                .web_capabilities(&model)
                .tools
                .iter()
                .any(|tool| tool.name == rsi_common::harness_tool_policy::WEB_SEARCH_TOOL),
        );
        mcp_bridge.register_with_threshold(&mut tools, self.mcp_deferred_tool_threshold());
        let mut mcp_events = mcp_bridge.take_events();
        mcp_events.extend(web_search_notice);
        let tools = Arc::new(tools);

        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

        // #2258: the static base instructions lead every Harness-loop prompt,
        // then the launch's own text. A resumed history carries no system
        // message, so the whole composed prompt is put back at its front
        // (#2347): the continue path's custody instruction, and on a fresh
        // relaunch the router and preamble parts, reach the model again.
        let system_prompt =
            base_prompt::compose_system_prompt(config.provider, system_prompt).unwrap_or_default();
        let conversation_history = base_prompt::seed_resumed_history(
            config.provider,
            &system_prompt,
            conversation_history,
        );
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
                let _ = event_tx.send(process_error_event(&e)).await;
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

/// The terminal `process_error` a failed Harness turn ends with (#304).
///
/// The monitor settles a turn `Failed` only on daemon-owned terminal evidence
/// (`monitor.rs::is_terminal_provider_error`); an event without it is a
/// nonterminal diagnostic and the drained stream completes the session. A
/// classified provider failure carries its typed fields; every other loop
/// error (a policy denial, a process or store fault) is just as final for the
/// turn, so it carries the same `source: "harness"` / `terminal: true` marker
/// and keeps its text visible.
pub(in crate::session) fn process_error_event(error: &crate::error::DaemonError) -> StreamEvent {
    let data = match errors::ProviderError::from_daemon_error(error) {
        Some(typed) => serde_json::json!({
            "error": format!("provider_error:{}", typed.detail_code),
            "source": "harness",
            "terminal": true,
            "error_class": typed.class,
            "http_status": typed.http_status,
            "retry_after_ms": typed.retry_after_ms,
            "detail_code": typed.detail_code,
        }),
        None => serde_json::json!({
            "error": error.to_string(),
            "source": "harness",
            "terminal": true,
        }),
    };
    StreamEvent {
        event_type: "process_error".into(),
        data,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vault(env: &'static [(&'static str, &'static str)]) -> crate::vault::VaultHandle {
        crate::vault::VaultHandleBuilder::new(Arc::new(crate::vault::VaultSettings::default()))
            .env(move |name| {
                env.iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| (*value).to_string())
            })
            .open()
            .expect("test vault")
    }

    fn advertises_web_search(tools: &HarnessToolRegistry) -> bool {
        tools.specs().iter().any(|spec| spec.name == "web_search")
    }

    /// #2251: the native `web_search` tool is advertised only when the chosen
    /// backend's vault key resolves. Without one the tool is absent and the
    /// operator gets one visible `system` notice; the notice is withheld when
    /// the operator disabled the tool, the session policy refuses it, or the
    /// route has a hosted search of its own.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn web_search_is_registered_only_when_a_backend_key_resolves() {
        let keyless = vault(&[]);
        let keyed = vault(&[("SERPER_API_KEY", "serper-fake-key-0123456789abcdef")]);
        let client = HarnessClient::new();

        // No key: no tool, one notice that persists as a visible System row.
        let mut tools = HarnessToolRegistry::new();
        let notice = client
            .register_web_search(&mut tools, &keyless, false)
            .expect("a missing key is announced");
        assert!(!advertises_web_search(&tools));
        assert_eq!(notice.event_type, "system");
        assert_eq!(notice.data["subtype"], "web_search_unavailable");
        assert_eq!(notice.data["reason"], "no_backend_credential");
        let mut sequence = 0;
        let rows = crate::session::SessionManager::convert_recognized_stream_event(
            &notice,
            uuid::Uuid::new_v4(),
            &mut sequence,
        )
        .expect("a system event is recognized");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].event_type, rsi_common::types::EventType::System);
        assert!(
            rows[0].content.contains("Provider Keys"),
            "{}",
            rows[0].content
        );

        // The route's own hosted search needs no notice.
        let mut tools = HarnessToolRegistry::new();
        assert!(
            client
                .register_web_search(&mut tools, &keyless, true)
                .is_none()
        );
        assert!(!advertises_web_search(&tools));

        // A refused tool raises no notice either.
        let mut tools = HarnessToolRegistry::new();
        tools.set_policy(Arc::new(ToolPolicyRuntime::new(HarnessToolPolicy {
            web_access: Some(rsi_common::harness_tool_policy::WebAccessMode::Disabled),
            ..HarnessToolPolicy::default()
        })));
        assert!(
            client
                .register_web_search(&mut tools, &keyless, false)
                .is_none()
        );

        // A resolvable key registers the tool and says nothing.
        let mut tools = HarnessToolRegistry::new();
        assert!(
            client
                .register_web_search(&mut tools, &keyed, false)
                .is_none()
        );
        assert!(advertises_web_search(&tools));

        // The operator's `disabled` wins over a key, silently.
        let mut disabled = HarnessClient::new();
        let runtime = Arc::new(crate::config::RuntimeConfig::from_config(
            &crate::config::Config::default(),
        ));
        runtime
            .update_field("harness_web_search_backend", &serde_json::json!("disabled"))
            .unwrap();
        disabled.set_runtime_config(Arc::clone(&runtime));
        let mut tools = HarnessToolRegistry::new();
        assert!(
            disabled
                .register_web_search(&mut tools, &keyed, false)
                .is_none()
        );
        assert!(!advertises_web_search(&tools));

        // `serper` named explicitly behaves like `auto` with a key.
        runtime
            .update_field("harness_web_search_backend", &serde_json::json!("serper"))
            .unwrap();
        let mut tools = HarnessToolRegistry::new();
        assert!(
            disabled
                .register_web_search(&mut tools, &keyed, false)
                .is_none()
        );
        assert!(advertises_web_search(&tools));
        // ... and says so when the key is missing.
        let mut tools = HarnessToolRegistry::new();
        let named = disabled
            .register_web_search(&mut tools, &keyless, false)
            .expect("a named backend without a key is announced");
        assert_eq!(named.data["backend"], "serper");
    }

    /// #2315: with no key at all, the operator's SearXNG URL registers the
    /// tool (under `auto` and when named); `searxng` named without a URL is
    /// announced with its own reason; clearing the URL takes the tool away.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn web_search_is_registered_from_the_searxng_url_without_any_key() {
        let keyless = vault(&[]);
        let mut client = HarnessClient::new();
        let runtime = Arc::new(crate::config::RuntimeConfig::from_config(
            &crate::config::Config::default(),
        ));
        client.set_runtime_config(Arc::clone(&runtime));

        // No URL under `auto`: the generic notice, now naming SearXNG too.
        let mut tools = HarnessToolRegistry::new();
        let none = client
            .register_web_search(&mut tools, &keyless, false)
            .expect("nothing configured is announced");
        assert_eq!(none.data["reason"], "no_backend_credential");
        assert!(
            none.data["message"]
                .as_str()
                .is_some_and(|message| message.contains("SearXNG URL"))
        );
        assert!(!advertises_web_search(&tools));

        runtime
            .update_field(
                "harness_searxng_url",
                &serde_json::json!("http://127.0.0.1:8888/"),
            )
            .unwrap();
        for choice in ["auto", "searxng"] {
            runtime
                .update_field("harness_web_search_backend", &serde_json::json!(choice))
                .unwrap();
            let mut tools = HarnessToolRegistry::new();
            assert!(
                client
                    .register_web_search(&mut tools, &keyless, false)
                    .is_none(),
                "{choice}"
            );
            assert!(advertises_web_search(&tools), "{choice}");
        }

        // Cleared again: `searxng` named is announced with its own reason.
        runtime
            .update_field("harness_searxng_url", &serde_json::Value::Null)
            .unwrap();
        let mut tools = HarnessToolRegistry::new();
        let named = client
            .register_web_search(&mut tools, &keyless, false)
            .expect("a named backend without a URL is announced");
        assert_eq!(named.data["reason"], "no_backend_url");
        assert_eq!(named.data["backend"], "searxng");
        assert!(!advertises_web_search(&tools));
    }

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

    fn resume_launch_config(
        session_id: uuid::Uuid,
        model: &str,
        query: &str,
        system_prompt: String,
        working_dir: &Path,
    ) -> LaunchConfig {
        LaunchConfig {
            completion_gates: None,
            query: query.to_string(),
            title: None,
            agent_role: None,
            epic_spawn_ordinal: None,
            working_dir: Some(working_dir.to_path_buf()),
            provider: Some(rsi_common::types::SessionProvider::Harness),
            model: Some(model.to_string()),
            configured_context_window: None,
            max_turns: None,
            system_prompt: Some(system_prompt),
            resume_session_id: Some(session_id.to_string()),
            session_kind: None,
            project_id: None,
            rsi_session_id: Some(session_id),
            rsi_socket: None,
            rsi_session_token: None,
            continued_from: None,
            openai_base_url: None,
            openai_api_key: None,
            conversation_history: None,
            workflow_id: None,
            workflow_id_override: None,
            max_retries: None,
            group_id: None,
            parent_id: None,
            effort: None,
            issue_identifier: None,
            issue_url: None,
            issue_tracker_id: None,
            scheduled_job_id: None,
            model_invocation_owner: None,
            model_invocation_dedup_key: None,
            model_invocation_request_fingerprint: None,
            skip_project_model_default: false,
            tool_policy: None,
            model_invocation_purpose:
                rsi_common::model_control::ModelInvocationPurpose::SessionContinueResume,
            sandbox: None,
            cargo_target_dir: None,
            execution_scratch: None,
            is_eval: false,
            skip_context_pipeline: false,
            capability_class: None,
            tags: vec![],
            topology_node_id: None,
            topology_iteration: 0,
            closure_selector: None,
            structured_handoff: false,
        }
    }

    /// Launch a resumed Harness session through `launch_with_binding` (the call
    /// `ResumedLauncher` makes) against a recording model, and return the
    /// `messages` of the one request it sent.
    async fn first_resumed_request(
        model: &str,
        system_prompt: String,
        history: Vec<ChatMessage>,
        query: &str,
    ) -> Vec<serde_json::Value> {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let body = [
            serde_json::json!({"choices": [{"index": 0, "delta": {
                "role": "assistant", "content": "ok"}}]}),
            serde_json::json!({"choices": [{"index": 0, "delta": {},
                "finish_reason": "stop"}]}),
            serde_json::json!({"choices": [], "usage": {
                "prompt_tokens": 10, "completion_tokens": 1, "total_tokens": 11}}),
        ]
        .iter()
        .map(|chunk| format!("data: {chunk}\n\n"))
        .collect::<String>()
            + "data: [DONE]\n\n";
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
            .mount(&server)
            .await;
        provider::install_test_openai_compatible_route(model, &server.uri());

        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let session_id = uuid::Uuid::new_v4();
        let permit = agent_loop::tests::root_permit_keyed(&store, session_id, "resume").await;
        let event_bus = Arc::new(EventBus::new(8));
        let worker = crate::model_control::call_control::ModelCallSettlementWorker::new(
            Arc::clone(&store),
            Arc::clone(&event_bus),
        )
        .expect("settlement worker");
        let settlements = worker.handle().expect("settlement producer");
        let dir = tempfile::tempdir().expect("working dir");
        let config =
            resume_launch_config(session_id, model, query, system_prompt.clone(), dir.path());
        let budget = crate::provider_capabilities::resolve_fresh_context_budget(
            rsi_common::types::SessionProvider::Harness,
            "claude-sonnet-5",
            None,
        );

        let (_process, mut events) = HarnessClient::new()
            .launch_with_binding(
                1,
                &config,
                &budget,
                Some(system_prompt),
                dir.path(),
                Some(history),
                permit,
                None,
                None,
                Arc::clone(&store),
                event_bus,
                settlements,
                Some(session_id),
                None,
                None,
                None,
            )
            .expect("the resumed launch");
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            while events.recv().await.is_some() {}
        })
        .await
        .expect("the turn ends");

        let requests = server.received_requests().await.expect("request recording");
        assert_eq!(requests.len(), 1, "one model request");
        let body: serde_json::Value = requests[0].body_json().expect("JSON request body");
        body["messages"].as_array().expect("messages").clone()
    }

    /// #2347: a continued Harness session's first request starts with one system
    /// message holding the base and the launch's supplemental prompt (the
    /// sandbox custody instruction), ahead of the replayed history. A fresh
    /// relaunch replays its router and preamble parts the same way.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_resumed_launch_sends_the_launch_prompt_once_ahead_of_the_history() {
        let custody = crate::session::preamble::sandbox_custody_instruction(
            Path::new("/tmp/rsi-sandboxes/2347"),
            "rsi/2347",
        );
        let history = vec![
            ChatMessage::user("first task"),
            ChatMessage::assistant("first answer"),
        ];

        // A plain continue: the custody instruction is the supplemental text.
        let supplemental = crate::session::preamble::prepend_sandbox_custody_instruction(
            None,
            Some(custody.clone()),
        )
        .expect("custody prompt");
        let messages = first_resumed_request(
            "resume-prompt-continue",
            supplemental,
            history.clone(),
            "keep going",
        )
        .await;
        let roles: Vec<&str> = messages
            .iter()
            .map(|message| message["role"].as_str().unwrap_or_default())
            .collect();
        assert_eq!(
            roles,
            ["system", "user", "assistant", "user"],
            "{messages:?}"
        );
        let system = messages[0]["content"].as_str().expect("system text");
        assert!(system.starts_with(base_prompt::base_instructions()));
        assert_eq!(system.matches(&custody).count(), 1, "custody appears once");
        assert!(system.contains("<session_instructions>"));
        assert_eq!(messages[1]["content"], "first task");
        assert_eq!(messages[2]["content"], "first answer");
        assert_eq!(messages[3]["content"], "keep going");

        // A fresh relaunch carries the preamble parts ahead of the custody text.
        let preamble = "ROUTER-AND-PREAMBLE-MARKER";
        let relaunch = format!("{preamble}\n\n{custody}");
        let messages =
            first_resumed_request("resume-prompt-relaunch", relaunch, Vec::new(), "start").await;
        let roles: Vec<&str> = messages
            .iter()
            .map(|message| message["role"].as_str().unwrap_or_default())
            .collect();
        assert_eq!(roles, ["system", "user"], "{messages:?}");
        let system = messages[0]["content"].as_str().expect("system text");
        assert!(system.starts_with(base_prompt::base_instructions()));
        assert_eq!(system.matches(preamble).count(), 1);
        assert_eq!(system.matches(&custody).count(), 1);
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
