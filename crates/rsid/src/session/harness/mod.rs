pub mod agent_loop;
pub(crate) mod agent_mail;
pub mod api_key;
pub(crate) mod base_prompt;
mod bedrock_stream;
pub(crate) mod build_digest;
pub mod compaction;
pub mod compatible_table;

/// The model a Harness launch runs when its config names none.
pub(crate) const DEFAULT_HARNESS_MODEL: &str = "claude-sonnet-5";

#[cfg(test)]
mod bench_loop_tests;
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
        store: &Arc<rsid_core::store_lock::Mutex<Store>>,
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
        store: &Arc<rsid_core::store_lock::Mutex<Store>>,
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
        store: Arc<rsid_core::store_lock::Mutex<Store>>,
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
        store: Arc<rsid_core::store_lock::Mutex<Store>>,
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
            .unwrap_or_else(|| DEFAULT_HARNESS_MODEL.into());
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
        let call_default_max_output_tokens = provider_backend.default_max_output_tokens();
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
        // #3176: a session bound to a benchmark container runs only the
        // container-side shell tools. Every host-touching tool (file tools,
        // apply_patch, git, view_image, web, codegraph, memory, schedule_wake,
        // rsi_control, MCP) stays unregistered, and a target that is malformed
        // or paired with a host-run completion gate never starts.
        let container_target = config.exec_target.container().cloned();
        if let Some(target) = &container_target {
            target.validate_shape().map_err(|reason| {
                crate::error::DaemonError::PolicyDenied(format!(
                    "{}:{reason}",
                    rsi_common::exec_target::EXEC_TARGET_INVALID
                ))
            })?;
            if config.completion_gates.is_some() {
                return Err(crate::error::DaemonError::PolicyDenied(format!(
                    "{}:completion_gates_unsupported",
                    rsi_common::exec_target::EXEC_TARGET_INVALID
                )));
            }
        }
        // #3177: a bench launch runs in a task container only, and per-launch
        // loop knobs must be in range. Both are checked before any tool or
        // provider state is built.
        validate_bench_launch(config)?;
        let bench = config.bench();
        let mut tools = match &container_target {
            Some(target) => HarnessToolRegistry::container_target_tools(
                target,
                store_for_tools,
                origin_session_id,
                Some(launch_invocation_id),
                process_registry,
            ),
            None => HarnessToolRegistry::default_tools_with_process_registry(
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
            ),
        };
        if container_target.is_some() {
            // No codegraph tool: it reads the host index.
        } else if let (Some(binding), Some(handle), Some(session_id)) = (
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
        // A container-bound session starts no MCP server: they run on the host.
        let mut mcp_bridge = (container_target.is_none()).then(|| {
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async {
                    crate::session::harness::mcp::McpBridge::build(&store, &crate::vault::global())
                        .await
                })
            })
        });
        // #792: the session's stored or inherited policy, layered over the
        // daemon defaults. An unrestricted result installs no runtime at all.
        let mut effective_policy = self.effective_tool_policy(session_tool_policy.as_ref());
        if let Some(profile) = bench.as_ref() {
            // #3177: the bench allowlist always wins; the stored policy and the
            // daemon defaults can only narrow it further (their deny sets union).
            // #3191: a run overlay narrows the allowlist itself, never widens it.
            let allowlist = profile
                .overlay
                .as_ref()
                .map_or_else(HarnessToolPolicy::bench, |overlay| overlay.tool_policy());
            effective_policy = allowlist.or_defaults(&effective_policy);
        }
        if !effective_policy.is_default() {
            tools.set_policy(Arc::new(ToolPolicyRuntime::new(effective_policy.clone())));
        }
        // #2251: the native search tool, only when a backend key resolves. The
        // policy is set first so a refused tool raises no notice.
        let web_search_notice = if container_target.is_some() {
            None
        } else {
            self.register_web_search(
                &mut tools,
                &crate::vault::global(),
                provider_backend
                    .web_capabilities(&model)
                    .tools
                    .iter()
                    .any(|tool| tool.name == rsi_common::harness_tool_policy::WEB_SEARCH_TOOL),
            )
        };
        let mut mcp_events = match mcp_bridge.as_mut() {
            Some(bridge) => {
                bridge.register_with_threshold(&mut tools, self.mcp_deferred_tool_threshold());
                bridge.take_events()
            }
            None => Vec::new(),
        };
        mcp_events.extend(web_search_notice);
        // #3178: a bench session writes its live, untruncated trace; a trace
        // that cannot be opened refuses the launch (no trial without one).
        let bench_trace = match bench
            .as_ref()
            .and_then(|profile| profile.trace_path.as_deref())
        {
            Some(path) => Some(Arc::new(
                crate::bench::trace::TraceSink::open(Path::new(path)).map_err(|error| {
                    crate::error::DaemonError::PolicyDenied(format!(
                        "{}: {error}",
                        rsi_common::bench::BENCH_TRACE_UNAVAILABLE
                    ))
                })?,
            )),
            None => None,
        };
        if let Some(trace) = &bench_trace {
            tools.set_trace(Arc::clone(trace));
        }
        // #4028: the length-stop recovery ceiling rides on the registry like the trace.
        tools.set_recovery_max_output_tokens(config.loop_knobs.recovery_max_output_tokens);
        let tools = Arc::new(tools);

        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

        // #2258: the static base instructions lead every Harness-loop prompt,
        // then the launch's own text. A resumed history carries no system
        // message, so the whole composed prompt is put back at its front
        // (#2347): the continue path's custody instruction, and on a fresh
        // relaunch the router and preamble parts, reach the model again.
        // A bench session (#3177) swaps the worktree base for the bench base.
        // #3191: a run overlay replaces the bench base's editable body only.
        let overlay_body = bench
            .as_ref()
            .map(|profile| profile.overlay.as_ref().and_then(|o| o.instructions_text()));
        let system_prompt =
            base_prompt::compose_system_prompt_with(config.provider, system_prompt, overlay_body)
                .unwrap_or_default();
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
        // #3177: a per-launch compaction budget overrides it, on any route.
        let global_compact_budget = if openrouter_route {
            self.runtime_config
                .as_ref()
                .and_then(|runtime| runtime.openrouter_context_budget())
        } else {
            None
        };
        let (max_iterations, compact_budget) = resolve_loop_knobs(
            &config.loop_knobs,
            self.turn_iteration_cap(),
            global_compact_budget,
        );
        // #3199: a run overlay's compaction policy reaches this session's loop;
        // no overlay (or no `[compaction]`) is the built-in behaviour.
        let compaction_policy = compaction_policy_for(bench.as_ref());
        let completion_gates = config.completion_gates.clone();
        let completion_gates_enabled = self
            .runtime_config
            .as_ref()
            .map_or(true, |runtime| runtime.completion_gates_enabled());
        // #1050: the operator's per-turn iteration cap, read at turn start (a
        // per-launch cap, #3177, replaces it above for this session only).
        if bench.is_some() {
            // The digest names the exact configuration this session ran under.
            mcp_events.push(build_digest_event(&build_digest::BuildDigestInputs {
                harness_source_digest: crate::daemon_info::HARNESS_SOURCE_DIGEST.to_string(),
                base_prompt_version: base_prompt::BASE_PROMPT_VERSION,
                base_prompt_digest: base_prompt::base_digest(),
                bench_prompt_version: base_prompt::BENCH_PROMPT_VERSION,
                bench_prompt_digest: base_prompt::bench_digest(),
                tool_spec_digest: build_digest::tool_spec_digest(&tools.specs()),
                tool_policy: effective_policy.clone(),
                max_iterations,
                compaction_budget_tokens: compact_budget,
                rotation_off: config.loop_knobs.rotation_off,
                recovery_max_output_tokens: config.loop_knobs.recovery_max_output_tokens,
                provider: provider_name.clone(),
                model: model.clone(),
                effort: config.effort.clone(),
                route: format!("{execution_route:?}"),
                overlay: bench
                    .as_ref()
                    .and_then(|profile| profile.overlay.as_ref())
                    .map(|overlay| {
                        overlay
                            .identity(&base_prompt::bench_base_digest(overlay.instructions_text()))
                    }),
            }));
        }
        // #3154: an OpenRouter-billed session checks the spend ceiling before
        // every model call.
        let spend_enforcer = self.runtime_config.as_ref().and_then(|runtime| {
            crate::openrouter_spend::launch_bills_openrouter(
                config
                    .provider
                    .unwrap_or(rsi_common::types::SessionProvider::Harness),
                config.openai_base_url.as_deref(),
                Some(model.as_str()),
            )
            .then(|| {
                crate::openrouter_spend::SpendEnforcer::production(
                    Arc::clone(runtime),
                    Arc::clone(&store),
                )
            })
        });
        let bench_store = bench.is_some().then(|| Arc::clone(&store));
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
        let model_call_control: Arc<dyn ModelCallControl> = match spend_enforcer {
            Some(enforcer) => Arc::new(crate::openrouter_spend::SpendGuardedControl::new(
                model_call_control,
                enforcer,
            )),
            None => model_call_control,
        };
        // #3178: the per-task USD cap of a bench run, checked first.
        let model_call_control: Arc<dyn ModelCallControl> = match (bench_store, bench.as_ref()) {
            (Some(bench_store), Some(profile)) => {
                Arc::new(crate::bench::cap::BenchTaskGuardedControl::new(
                    model_call_control,
                    bench_store,
                    config.rsi_session_id,
                    profile.task_cap_micro_usd,
                    // #3350: the most one call can cost (None: no known price).
                    crate::model_control::price_for_model(&model).map(|price| {
                        crate::bench::cap::call_upper_bound_usd(
                            price,
                            token_limit,
                            rsid_harness::agent_loop::max_call_output_tokens(
                                call_default_max_output_tokens,
                                config.loop_knobs.recovery_max_output_tokens,
                            ),
                        )
                    }),
                    bench_trace,
                ))
            }
            _ => model_call_control,
        };

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
                compaction_policy,
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

/// #3177: a bench launch runs in a task container only, and per-launch loop
/// knobs must be in range. Checked before any tool or provider state is built.
fn validate_bench_launch(config: &LaunchConfig) -> Result<()> {
    let invalid = |reason: &str| {
        crate::error::DaemonError::PolicyDenied(format!(
            "{}:{reason}",
            rsi_common::bench_profile::BENCH_PROFILE_INVALID
        ))
    };
    if config.bench().is_some() && !config.exec_target.is_container() {
        return Err(invalid("container_target_required"));
    }
    // #3191: a run overlay is re-checked here, so a tampered or mismatched one
    // never launches (the typed code is the overlay's own).
    if let Some(overlay) = config.bench().and_then(|profile| profile.overlay) {
        overlay.verify().map_err(|error| {
            crate::error::DaemonError::PolicyDenied(format!(
                "{}:{}",
                rsi_common::bench_profile::BENCH_PROFILE_INVALID,
                error.code
            ))
        })?;
    }
    config.loop_knobs.validate().map_err(invalid)
}

/// #3177: the loop knobs a session runs with: each per-launch knob that is set
/// replaces the daemon global for this session only (the caller passes the
/// globals by value and nothing here writes one). Returns the per-turn
/// iteration cap and the compaction budget. A per-launch iteration cap is
/// clamped to the operator's range (#3171), so a launch can neither starve the
/// loop nor lift the ceiling the operator set.
fn resolve_loop_knobs(
    knobs: &rsi_common::bench_profile::LoopKnobs,
    global_iteration_cap: u32,
    global_compaction_budget: Option<u64>,
) -> (u32, Option<u64>) {
    (
        knobs.max_iterations.map_or(global_iteration_cap, |cap| {
            cap.clamp(
                crate::config::HARNESS_MAX_ITERATIONS_PER_TURN_MIN,
                crate::config::HARNESS_MAX_ITERATIONS_PER_TURN_MAX,
            )
        }),
        knobs.compaction_budget_tokens.or(global_compaction_budget),
    )
}

/// #3199: the compaction policy a launch runs with: the run overlay's, else the
/// built-in default.
fn compaction_policy_for(
    bench: Option<&rsi_common::bench_profile::BenchLaunchProfile>,
) -> rsi_common::compaction_policy::CompactionPolicy {
    bench
        .and_then(|profile| profile.overlay.as_ref())
        .map_or_else(Default::default, |overlay| overlay.compaction_policy())
}

/// #3177: the `bench_build_digest` stream event of a bench launch: the digest
/// and the canonical inputs it was taken over, so a reader can see which input
/// differs between two runs.
fn build_digest_event(inputs: &build_digest::BuildDigestInputs) -> StreamEvent {
    let digest = build_digest::harness_build_digest(inputs);
    tracing::info!(harness_build_digest = %digest, "bench harness build digest");
    StreamEvent {
        event_type: build_digest::BUILD_DIGEST_STREAM_EVENT.into(),
        data: serde_json::json!({
            "harness_build_digest": digest,
            "harness_source_digest": inputs.harness_source_digest,
            // Provenance only (#3192): not an input of the digest, so a redeploy that leaves the
            // harness alone keeps the digest and the benchmark baseline comparable.
            "rsid_build_sha": crate::daemon_info::BUILD_SHA,
            "effective_inputs": build_digest::effective_input_digests(inputs),
            "inputs": serde_json::to_value(inputs).unwrap_or_default(),
        }),
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
            &Arc::new(rsid_core::store_lock::Mutex::new(
                Store::open_in_memory().unwrap(),
            )),
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
            exec_target: rsi_common::exec_target::ExecTarget::Host,
            bench_profile: None,
            loop_knobs: rsi_common::bench_profile::LoopKnobs::default(),
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
        let (body, _events) =
            recorded_launch(model, system_prompt, Some(history), query, |_| {}).await;
        body["messages"].as_array().expect("messages").clone()
    }

    /// The one model request body and every stream event of a Harness launch
    /// (`tweak` edits the launch config first), against a recording model.
    async fn recorded_launch(
        model: &str,
        system_prompt: String,
        history: Option<Vec<ChatMessage>>,
        query: &str,
        tweak: impl FnOnce(&mut LaunchConfig),
    ) -> (serde_json::Value, Vec<StreamEvent>) {
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

        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().expect("store"),
        ));
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
        let mut config =
            resume_launch_config(session_id, model, query, system_prompt.clone(), dir.path());
        tweak(&mut config);
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
                history,
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
        let mut seen = Vec::new();
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            while let Some(event) = events.recv().await {
                seen.push(event);
            }
        })
        .await
        .expect("the turn ends");

        let requests = server.received_requests().await.expect("request recording");
        assert_eq!(requests.len(), 1, "one model request");
        let body: serde_json::Value = requests[0].body_json().expect("JSON request body");
        (body, seen)
    }

    /// An obviously inert Anthropic-shaped key for the #595 loopback tests.
    const INERT_ANTHROPIC_KEY: &str = "sk-ant-api03-inert-595-test-not-a-credential";

    /// Persist through Provider Setup, then resolve the saved endpoint and its
    /// isolated credential exactly as LaunchSession does. All state is local.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    fn saved_custom_endpoint(configured: &str) -> (String, Option<String>) {
        let root = crate::test_support::disk_backed_tempdir("messages-provider");
        let settings_store = Store::open_in_memory().expect("settings store");
        let vault =
            crate::vault::VaultHandleBuilder::new(Arc::new(crate::vault::VaultSettings::default()))
                .dir(root.path().join("vault"))
                .env(|_| None)
                .open()
                .expect("vault");
        let runtime_config =
            crate::config::RuntimeConfig::from_config(&crate::config::Config::default());
        let paths = crate::operator_config::OperatorConfigPaths::under(root.path());
        let context = crate::operator_config::OperatorConfigContext {
            store: &settings_store,
            vault: &vault,
            runtime_config: &runtime_config,
            paths: &paths,
        };
        let saved = crate::operator_config::handle(
            &context,
            rsi_common::operator_config::METHOD_PUT_CUSTOM_PROVIDER,
            false,
            &serde_json::json!({
                "name": "Messages regression",
                "base_url": configured,
                "default_model": "claude-opus-5-5",
                "api_key": {"action": "set", "secret": INERT_ANTHROPIC_KEY},
            }),
        )
        .expect("PutCustomProvider saves the provider");
        assert_eq!(saved["base_url"], configured);
        assert!(!saved.to_string().contains(INERT_ANTHROPIC_KEY));
        let id = serde_json::from_value(saved["id"].clone()).expect("id");
        let (base_url, secret) = crate::operator_config::custom_providers::resolve_for_request(
            &settings_store,
            &vault,
            id,
        )
        .expect("resolve the saved provider");
        (base_url, secret.map(|secret| secret.expose().to_owned()))
    }

    /// Run one fresh Harness launch of `config` to its end and return every
    /// stream event (#595).
    async fn run_launch_to_end(config: LaunchConfig, working_dir: &Path) -> Vec<StreamEvent> {
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().expect("store"),
        ));
        let session_id = uuid::Uuid::new_v4();
        let permit = agent_loop::tests::root_permit_keyed(&store, session_id, "595").await;
        let event_bus = Arc::new(EventBus::new(8));
        let worker = crate::model_control::call_control::ModelCallSettlementWorker::new(
            Arc::clone(&store),
            Arc::clone(&event_bus),
        )
        .expect("settlement worker");
        let settlements = worker.handle().expect("settlement producer");
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
                Some("You are a test.".to_string()),
                working_dir,
                None,
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
            .expect("the launch starts");
        let mut seen = Vec::new();
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            while let Some(event) = events.recv().await {
                seen.push(event);
            }
        })
        .await
        .expect("the turn ends");
        seen
    }

    /// A native Anthropic Messages SSE reply that streams `text`.
    fn anthropic_text_stream(text: &str) -> String {
        [
            (
                "message_start",
                serde_json::json!({"type": "message_start",
                "message": {"usage": {"input_tokens": 9}}}),
            ),
            (
                "content_block_start",
                serde_json::json!({"type": "content_block_start",
                "index": 0, "content_block": {"type": "text", "text": ""}}),
            ),
            (
                "content_block_delta",
                serde_json::json!({"type": "content_block_delta",
                "index": 0, "delta": {"type": "text_delta", "text": text}}),
            ),
            (
                "content_block_stop",
                serde_json::json!({"type": "content_block_stop", "index": 0}),
            ),
            (
                "message_delta",
                serde_json::json!({"type": "message_delta",
                "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 3}}),
            ),
            ("message_stop", serde_json::json!({"type": "message_stop"})),
        ]
        .iter()
        .map(|(event, data)| format!("event: {event}\ndata: {data}\n\n"))
        .collect()
    }

    /// #595: the operator's failing route, end to end. A custom provider saved
    /// in Settings > API Providers with Anthropic's native Messages URL
    /// (`<root>/v1/messages`, with or without a trailing `/`) and a key is
    /// persisted by `PutCustomProvider`, resolved by id the way `LaunchSession`
    /// resolves it, routed off the Local Chat Completions client onto Harness,
    /// and its first request is a native Messages call to `<root>/v1/messages`
    /// with the saved key as `x-api-key`; the streamed reply reaches the
    /// session. Before the fix this launch posted to
    /// `<root>/v1/messages/chat/completions` and Anthropic answered 404.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_saved_anthropic_messages_custom_provider_launches_the_native_messages_route() {
        use rsi_common::types::SessionProvider;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(anthropic_text_stream("hello from messages")),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/anthropic/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(anthropic_text_stream("hello from messages")),
            )
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().expect("working dir");
        let configured_urls = [
            format!("{}/v1/messages", server.uri()),
            format!("{}/v1/messages/", server.uri()),
            format!("{}/anthropic/v1/messages", server.uri()),
            format!("{}/anthropic/v1/messages/", server.uri()),
        ];
        for configured in &configured_urls {
            let (base_url, secret) = saved_custom_endpoint(configured);
            let routed = provider::provider_for_explicit_endpoint(
                Some(SessionProvider::Local),
                Some(&base_url),
            )
            .expect("supported native URL");
            assert_eq!(routed, Some(SessionProvider::Harness));

            let mut config = resume_launch_config(
                uuid::Uuid::new_v4(),
                "claude-opus-5-5",
                "say hello",
                String::new(),
                dir.path(),
            );
            config.resume_session_id = None;
            config.model_invocation_purpose =
                rsi_common::model_control::ModelInvocationPurpose::SessionLaunchFresh;
            config.provider = routed;
            config.openai_base_url = Some(base_url);
            config.openai_api_key = secret;
            let events = run_launch_to_end(config, dir.path()).await;
            let transcript =
                serde_json::to_string(&events.iter().map(|event| &event.data).collect::<Vec<_>>())
                    .expect("events serialize");
            assert!(
                transcript.contains("hello from messages"),
                "the streamed reply reaches the session: {transcript}"
            );
        }

        let requests = server.received_requests().await.expect("request recording");
        assert_eq!(requests.len(), 4, "one native Messages request per launch");
        for (index, request) in requests.iter().enumerate() {
            assert_eq!(
                request.url.path(),
                if index < 2 {
                    "/v1/messages"
                } else {
                    "/anthropic/v1/messages"
                }
            );
            let header = |name: &str| {
                request
                    .headers
                    .get(name)
                    .map(|value| value.to_str().expect("ascii header").to_owned())
            };
            assert_eq!(header("x-api-key").as_deref(), Some(INERT_ANTHROPIC_KEY));
            assert_eq!(header("anthropic-version").as_deref(), Some("2023-06-01"));
            assert_eq!(
                header("authorization"),
                None,
                "a standard key uses x-api-key"
            );
            let body: serde_json::Value = request.body_json().expect("JSON body");
            assert_eq!(body["model"], "claude-opus-5-5");
            assert_eq!(body["stream"], true);
            assert!(body["max_tokens"].as_u64().is_some_and(|tokens| tokens > 0));
            assert!(body["system"].is_array() || body["system"].is_string());
            let messages = body["messages"].as_array().expect("messages");
            assert_eq!(messages.last().expect("user turn")["role"], "user");
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    fn save_binding_provider(
        store: &Store,
        vault: &crate::vault::VaultHandle,
        root: &Path,
        url: &str,
        key: &str,
    ) -> uuid::Uuid {
        let runtime = crate::config::RuntimeConfig::from_config(&crate::config::Config::default());
        let paths = crate::operator_config::OperatorConfigPaths::under(root);
        let context = crate::operator_config::OperatorConfigContext {
            store,
            vault,
            runtime_config: &runtime,
            paths: &paths,
        };
        let saved = crate::operator_config::handle(
            &context,
            rsi_common::operator_config::METHOD_PUT_CUSTOM_PROVIDER,
            false,
            &serde_json::json!({"name": "Continuation regression", "base_url": url,
                "default_model": "claude-opus-5-5", "api_key": {"action": "set", "secret": key}}),
        )
        .unwrap();
        serde_json::from_value(saved["id"].clone()).unwrap()
    }

    /// #609: saved references survive a same-ID continuation and multiple
    /// successor links; the credential is read again after vault rotation.
    /// Both protocols use a deliberately misleading Claude model name, and
    /// another saved provider proves isolation between vault slots/endpoints.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn saved_provider_continuations_keep_route_protocol_and_vault_isolation() {
        use crate::session::custom_provider_launch::{persist, resolve};
        use rsi_common::types::SessionProvider;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        const FIRST: &str = "sk-ant-api03-inert-609-first-not-a-credential";
        const ROTATED: &str = "sk-ant-api03-inert-609-rotated-not-a-credential";
        const OTHER: &str = "sk-ant-api03-inert-609-other-not-a-credential";
        for native in [true, false] {
            let server = MockServer::start().await;
            let other_server = MockServer::start().await;
            let request_path = if native {
                "/anthropic/v1/messages"
            } else {
                "/v1/chat/completions"
            };
            let stream = if native {
                anthropic_text_stream("binding reply")
            } else {
                "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"binding reply\"}}]}\n\n\
                data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
                data: [DONE]\n\n"
                    .into()
            };
            for endpoint in [&server, &other_server] {
                Mock::given(method("POST"))
                    .and(path(request_path))
                    .respond_with(
                        ResponseTemplate::new(200)
                            .set_body_raw(stream.clone(), "text/event-stream"),
                    )
                    .mount(endpoint)
                    .await;
            }
            let dir = crate::test_support::disk_backed_tempdir("continuation-provider");
            let vault = crate::vault::VaultHandleBuilder::new(Arc::new(
                crate::vault::VaultSettings::default(),
            ))
            .dir(dir.path().join("vault"))
            .env(|_| None)
            .open()
            .unwrap();
            let store = Store::open_in_memory().unwrap();
            let suffix = if native {
                "/anthropic/v1/messages/"
            } else {
                "/v1"
            };
            let url = format!("{}{suffix}", server.uri());
            let custom_id = save_binding_provider(&store, &vault, dir.path(), &url, FIRST);
            let other_id = save_binding_provider(
                &store,
                &vault,
                dir.path(),
                &format!("{}{suffix}", other_server.uri()),
                OTHER,
            );
            let source_id = uuid::Uuid::new_v4();
            let mut row =
                crate::session::agent_verbs::tests::test_session(source_id, dir.path().into());
            row.provider = SessionProvider::Harness;
            store.insert_session(&row).unwrap();
            let make_config = |id| {
                resume_launch_config(
                    id,
                    "claude-opus-5-5",
                    "say hello",
                    "Test.".into(),
                    dir.path(),
                )
            };
            let mut fresh = make_config(source_id);
            fresh.resume_session_id = None;
            fresh.provider = Some(if native {
                SessionProvider::Local
            } else {
                SessionProvider::Harness
            });
            let binding = resolve(&store, &vault, &mut fresh, None, Some(custom_id))
                .unwrap()
                .unwrap();
            assert_eq!(fresh.provider, Some(SessionProvider::Harness));
            persist(&store, source_id, binding, SessionProvider::Harness).unwrap();
            let durable = store
                .get_daemon_setting(&format!("session_custom_provider.{source_id}"))
                .unwrap()
                .unwrap();
            let durable: serde_json::Value = serde_json::from_str(&durable).unwrap();
            assert_eq!(
                durable,
                serde_json::json!({"custom_provider_id": custom_id,
                "base_url": url, "provider": "Harness", "uses_saved_credential": true,
                    "requires_credential": true, "uses_saved_endpoint": true})
            );
            let assert_reply = |events: Vec<StreamEvent>| {
                assert!(
                    events
                        .iter()
                        .any(|event| event.data.to_string().contains("binding reply"))
                );
            };
            assert_reply(run_launch_to_end(fresh, dir.path()).await);

            vault.set_custom_provider_key(custom_id, ROTATED).unwrap();
            let mut continued = make_config(source_id);
            resolve(&store, &vault, &mut continued, Some(source_id), None).unwrap();
            assert_reply(run_launch_to_end(continued, dir.path()).await);

            // Agent successor/retry launch configs start with a parent hint;
            // rotation rows have already persisted their continued_from link.
            let middle_id = uuid::Uuid::new_v4();
            row.id = middle_id;
            row.continued_from = Some(source_id);
            store.insert_session(&row).unwrap();
            let leaf_id = uuid::Uuid::new_v4();
            let mut successor = make_config(leaf_id);
            successor.continued_from = Some(middle_id);
            resolve(&store, &vault, &mut successor, Some(leaf_id), None).unwrap();
            assert_reply(run_launch_to_end(successor, dir.path()).await);
            let mut unrelated = make_config(uuid::Uuid::new_v4());
            resolve(&store, &vault, &mut unrelated, None, Some(other_id)).unwrap();
            assert_reply(run_launch_to_end(unrelated, dir.path()).await);

            let requests = server.received_requests().await.unwrap();
            assert_eq!(requests.len(), 3);
            for (index, request) in requests.iter().enumerate() {
                assert_eq!(request.url.path(), request_path);
                let expected = if index == 0 { FIRST } else { ROTATED };
                if native {
                    assert_eq!(request.headers["x-api-key"].to_str().unwrap(), expected);
                    assert_eq!(request.headers["anthropic-version"], "2023-06-01");
                    assert!(!request.headers.contains_key("authorization"));
                } else {
                    assert_eq!(
                        request.headers["authorization"].to_str().unwrap(),
                        format!("Bearer {expected}")
                    );
                    assert!(!request.headers.contains_key("x-api-key"));
                }
                let body: serde_json::Value = request.body_json().unwrap();
                assert_eq!(body["model"], "claude-opus-5-5");
                assert_eq!(body["stream"], true);
                assert!(body["messages"].is_array());
                if native {
                    assert!(body["max_tokens"].is_number());
                }
            }
            let unrelated = other_server.received_requests().await.unwrap();
            assert_eq!(unrelated.len(), 1);
            assert_eq!(
                unrelated[0].headers[if native { "x-api-key" } else { "authorization" }]
                    .to_str()
                    .unwrap(),
                if native {
                    OTHER.to_owned()
                } else {
                    format!("Bearer {OTHER}")
                }
            );
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn saved_provider_continuation_refuses_unavailable_or_launch_only_credentials() {
        use crate::session::custom_provider_launch::{persist, resolve};
        use rsi_common::types::SessionProvider;
        let dir = crate::test_support::disk_backed_tempdir("continuation-credential-refusal");
        let vault =
            crate::vault::VaultHandleBuilder::new(Arc::new(crate::vault::VaultSettings::default()))
                .dir(dir.path().join("vault"))
                .env(|_| None)
                .open()
                .unwrap();
        let store = Store::open_in_memory().unwrap();
        let server = wiremock::MockServer::start().await;
        let id = save_binding_provider(
            &store,
            &vault,
            dir.path(),
            &format!("{}/v1", server.uri()),
            INERT_ANTHROPIC_KEY,
        );
        for launch_only in [true, false] {
            let session = uuid::Uuid::new_v4();
            let mut config = resume_launch_config(
                session,
                "claude-opus-5-5",
                "hello",
                "Test.".into(),
                dir.path(),
            );
            if launch_only {
                config.openai_api_key = Some("inert-launch-only-609".into());
            }
            let binding = resolve(&store, &vault, &mut config, None, Some(id))
                .unwrap()
                .unwrap();
            persist(&store, session, binding, SessionProvider::Harness).unwrap();
            if !launch_only {
                vault.clear_custom_provider_key(id).unwrap();
            }
            config.openai_base_url = None;
            config.openai_api_key = None;
            let error = resolve(&store, &vault, &mut config, Some(session), None)
                .err()
                .expect("refusal");
            assert!(
                error
                    .to_string()
                    .contains("custom_provider_credential_unavailable")
            );
            assert_eq!(config.openai_api_key, None);
        }
        assert_eq!(server.received_requests().await.unwrap().len(), 0);
    }

    /// An edited/deleted provider, a changed runtime, or damaged binding
    /// must not redirect a continuation or substitute another credential.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn saved_provider_continuation_refuses_changed_or_unreadable_bindings() {
        use crate::session::custom_provider_launch::{persist, resolve};
        use rsi_common::types::SessionProvider;
        let dir = crate::test_support::disk_backed_tempdir("continuation-binding-refusal");
        let vault =
            crate::vault::VaultHandleBuilder::new(Arc::new(crate::vault::VaultSettings::default()))
                .dir(dir.path().join("vault"))
                .env(|_| None)
                .open()
                .unwrap();
        let store = Store::open_in_memory().unwrap();
        let server = wiremock::MockServer::start().await;
        for reason in [
            "endpoint_override",
            "endpoint_edit",
            "deleted",
            "runtime",
            "malformed",
            "cycle",
        ] {
            let id = save_binding_provider(
                &store,
                &vault,
                dir.path(),
                &format!("{}/v1", server.uri()),
                INERT_ANTHROPIC_KEY,
            );
            let session = uuid::Uuid::new_v4();
            let make_config = || {
                resume_launch_config(
                    session,
                    "claude-opus-5-5",
                    "hello",
                    "Test.".into(),
                    dir.path(),
                )
            };
            let mut fresh = make_config();
            if reason == "endpoint_override" {
                fresh.openai_base_url = Some(format!("{}/override", server.uri()));
            }
            let binding = resolve(&store, &vault, &mut fresh, None, Some(id))
                .unwrap()
                .unwrap();
            persist(&store, session, binding, SessionProvider::Harness).unwrap();
            let binding_key = format!("session_custom_provider.{session}");
            if reason == "endpoint_edit" || reason == "deleted" {
                let mut row = store.get_custom_provider_record(id).unwrap().unwrap();
                if reason == "endpoint_edit" {
                    row.base_url = format!("{}/edited", server.uri());
                } else {
                    row.deleted_at = Some(chrono::Utc::now());
                }
                store.set_custom_provider_record(&row).unwrap();
            }
            if reason == "malformed" {
                store.set_daemon_setting(&binding_key, "{}").unwrap();
            }
            let mut continued = make_config();
            if reason == "runtime" {
                continued.provider = Some(SessionProvider::Local);
            }
            if reason == "cycle" {
                let mut row = crate::session::agent_verbs::tests::test_session(
                    uuid::Uuid::new_v4(),
                    dir.path().into(),
                );
                row.continued_from = Some(row.id);
                store.insert_session(&row).unwrap();
                continued.rsi_session_id = Some(row.id);
            }
            let expected = match reason {
                "endpoint_override" => "custom_provider_endpoint_unavailable",
                "endpoint_edit" => "custom_provider_endpoint_changed",
                "deleted" => "custom provider is not configured",
                "runtime" => "custom_provider_runtime_changed",
                "malformed" => "custom_provider_binding_unreadable",
                "cycle" => "custom_provider_lineage_cycle",
                _ => unreachable!(),
            };
            let session_id = continued.rsi_session_id;
            let error = resolve(&store, &vault, &mut continued, session_id, None)
                .err()
                .expect("refusal");
            assert!(error.to_string().contains(expected), "{reason}: {error}");
            assert_eq!(continued.openai_api_key, None);
            assert_eq!(continued.openai_base_url, None);
        }
        assert_eq!(server.received_requests().await.unwrap().len(), 0);
    }

    /// #608: accepted saved URLs with unsupported native components fail at
    /// launch routing and Harness resolution, before any HTTP or fallback.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn saved_native_messages_components_fail_before_launch_without_exposing_values() {
        use rsi_common::types::SessionProvider;
        let server = wiremock::MockServer::start().await;
        let expected = "Invalid parameter: anthropic_endpoint_components_unsupported: native Anthropic Messages URLs \
            cannot contain a query, fragment or inline credentials; configure a URL \
            ending in /v1/messages without these components in Settings > API Providers, \
            and set the key in the provider's API key field";
        for suffix in [
            "/v1/messages?tenant=credential-shaped-query-value",
            "/v1/messages/?tenant=credential-shaped-query-value",
            "/anthropic/v1/messages?tenant=credential-shaped-query-value#fragment-secret",
            "/anthropic/v1/messages/#fragment-secret",
            "/v1/messages#fragment-secret",
            "/v1/messages?",
            "/v1/messages#",
        ] {
            let (base_url, secret) = saved_custom_endpoint(&format!("{}{suffix}", server.uri()));
            for requested in [SessionProvider::Local, SessionProvider::Harness] {
                let error =
                    provider::provider_for_explicit_endpoint(Some(requested), Some(&base_url))
                        .expect_err("native components must fail before launch");
                // Exact equality proves neither query/fragment values, the
                // endpoint address nor the saved credential enters the error.
                assert_eq!(error.to_string(), expected);
            }
            let error =
                match resolve_provider("claude-opus-5-5", Some(&base_url), secret.as_deref()) {
                    Ok(_) => panic!("Harness must also reject unsupported native components"),
                    Err(error) => error,
                };
            assert_eq!(error.to_string(), expected);
        }
        assert_eq!(server.received_requests().await.unwrap().len(), 0);
    }

    /// #608: query/fragment text is not a protocol selector. Exercise the
    /// persisted route and a real mock request/stream, retaining the existing
    /// OpenAI client's URL construction and explicit credential semantics.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn saved_compatible_routes_with_deceptive_suffixes_keep_the_openai_protocol() {
        use rsi_common::types::SessionProvider;
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let stream = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"compatible reply\"}}]}\n\n\
            data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
            data: [DONE]\n\n";
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(stream, "text/event-stream"))
            .mount(&server)
            .await;
        let dir = crate::test_support::disk_backed_tempdir("compatible-components");
        for (index, (suffix, expected_path, expected_query)) in [
            ("/v1", "/v1/chat/completions", None),
            ("/v1/", "/v1/chat/completions", None),
            (
                "/openai/v1?route=/v1/messages",
                "/openai/v1",
                Some("route=/v1/messages/chat/completions"),
            ),
            ("/openai/v1#route/v1/messages", "/openai/v1", None),
            (
                "/v1/messagesx?route=/v1/messages/",
                "/v1/messagesx",
                Some("route=/v1/messages/chat/completions"),
            ),
            ("/v1/messages//", "/v1/messages/chat/completions", None),
        ]
        .into_iter()
        .enumerate()
        {
            let (base_url, secret) = saved_custom_endpoint(&format!("{}{suffix}", server.uri()));
            for requested in [SessionProvider::Local, SessionProvider::Harness] {
                assert_eq!(
                    provider::provider_for_explicit_endpoint(Some(requested), Some(&base_url))
                        .unwrap(),
                    Some(requested)
                );
            }
            let mut config = resume_launch_config(
                uuid::Uuid::new_v4(),
                "claude-opus-5-5",
                "say hello",
                String::new(),
                dir.path(),
            );
            config.resume_session_id = None;
            config.model_invocation_purpose =
                rsi_common::model_control::ModelInvocationPurpose::SessionLaunchFresh;
            config.openai_base_url = Some(base_url);
            config.openai_api_key = secret;
            let events = run_launch_to_end(config, dir.path()).await;
            let transcript =
                serde_json::to_string(&events.iter().map(|event| &event.data).collect::<Vec<_>>())
                    .unwrap();
            assert!(transcript.contains("compatible reply"), "{transcript}");
            assert!(!transcript.contains(INERT_ANTHROPIC_KEY));
            let requests = server.received_requests().await.unwrap();
            assert_eq!(requests.len(), index + 1, "one request per launch");
            let request = &requests[index];
            assert_eq!(request.url.path(), expected_path);
            assert_eq!(request.url.query(), expected_query);
            assert_eq!(
                request.headers["authorization"].to_str().unwrap(),
                format!("Bearer {INERT_ANTHROPIC_KEY}")
            );
            assert!(request.headers.get("x-api-key").is_none());
            let body: serde_json::Value = request.body_json().unwrap();
            assert_eq!(body["stream"], true);
            assert_eq!(body["model"], "claude-opus-5-5");
            assert_eq!(body["messages"][0]["role"], "system");
            assert!(body.get("system").is_none());
        }
    }

    /// #595: a native Messages endpoint that refuses the key ends the launch
    /// with a typed, sanitized authentication error, never the key.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_refused_anthropic_messages_endpoint_key_fails_with_a_sanitized_auth_error() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
                "type": "error",
                "error": {"type": "authentication_error", "message": "invalid x-api-key"},
            })))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().expect("working dir");
        let mut config = resume_launch_config(
            uuid::Uuid::new_v4(),
            "claude-opus-5-5",
            "say hello",
            String::new(),
            dir.path(),
        );
        config.resume_session_id = None;
        config.openai_base_url = Some(format!("{}/v1/messages", server.uri()));
        config.openai_api_key = Some(INERT_ANTHROPIC_KEY.to_string());
        let events = run_launch_to_end(config, dir.path()).await;
        let transcript =
            serde_json::to_string(&events.iter().map(|event| &event.data).collect::<Vec<_>>())
                .expect("events serialize");
        assert!(
            transcript.contains("401") || transcript.to_lowercase().contains("auth"),
            "the failure names the refused credential: {transcript}"
        );
        assert!(
            !transcript.contains(INERT_ANTHROPIC_KEY),
            "the key never reaches the transcript"
        );
        let requests = server.received_requests().await.expect("request recording");
        assert!(!requests.is_empty());
        assert!(
            requests
                .iter()
                .all(|request| request.url.path() == "/v1/messages")
        );
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

    fn bench_container_target() -> rsi_common::exec_target::ExecTarget {
        rsi_common::exec_target::ExecTarget::Container(
            rsi_common::exec_target::ContainerExecTarget {
                docker_host: "ssh://arch-laptop".into(),
                container: "tb2-main-1".into(),
                workdir: "/app".into(),
                user: None,
                compose_project: "tb2-trial".into(),
                pinned_digests: vec![format!("sha256:{}", "a".repeat(64))],
                per_call_ceiling_secs: 900,
            },
        )
    }

    /// #3177: a bench launch replaces the worktree base with the bench base,
    /// advertises exactly the bench tool set to the model and records one
    /// `harness_build_digest` event; a normal launch gets the worktree base, the
    /// full tool set and no digest.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_bench_launch_sends_the_bench_prompt_the_closed_tool_set_and_its_digest() {
        let supplemental = "Solve the task in the container.".to_string();
        let (body, events) = recorded_launch(
            "bench-launch-model",
            supplemental.clone(),
            None,
            "the task",
            |config| {
                config.exec_target = bench_container_target();
                config.bench_profile =
                    Some(rsi_common::bench_profile::BenchLaunchProfile::default());
                config.loop_knobs = rsi_common::bench_profile::LoopKnobs {
                    max_iterations: Some(70),
                    compaction_budget_tokens: Some(123_456),
                    rotation_off: true,
                    ..rsi_common::bench_profile::LoopKnobs::default()
                };
                config.effort = Some("high".into());
            },
        )
        .await;
        let system = body["messages"][0]["content"].as_str().expect("system");
        assert!(system.starts_with(base_prompt::bench_instructions()));
        assert!(!system.contains(base_prompt::base_instructions()));
        assert!(system.contains(&supplemental));
        let advertised: std::collections::BTreeSet<&str> = body["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .filter_map(|tool| tool["function"]["name"].as_str())
            .collect();
        let allowed: std::collections::BTreeSet<&str> =
            rsi_common::harness_tool_policy::BENCH_ENABLED_TOOLS
                .into_iter()
                .collect();
        assert_eq!(advertised, allowed);

        // One digest event, carrying the effective knobs, policy and prompts.
        let digests: Vec<_> = events
            .iter()
            .filter(|event| event.event_type == build_digest::BUILD_DIGEST_STREAM_EVENT)
            .collect();
        assert_eq!(digests.len(), 1, "{events:?}");
        let data = &digests[0].data;
        let digest = data["harness_build_digest"].as_str().expect("digest");
        assert_eq!(digest.len(), 64);
        let inputs = &data["inputs"];
        assert_eq!(inputs["max_iterations"], 70);
        assert_eq!(inputs["compaction_budget_tokens"], 123_456);
        assert_eq!(inputs["rotation_off"], true);
        assert_eq!(inputs["effort"], "high");
        assert_eq!(inputs["model"], "bench-launch-model");
        assert_eq!(inputs["bench_prompt_digest"], base_prompt::bench_digest());
        assert_eq!(inputs["base_prompt_digest"], base_prompt::base_digest());
        // The digest holds the harness source, not the build SHA (#3192); the build SHA rides
        // beside it as provenance.
        assert_eq!(
            inputs["harness_source_digest"],
            crate::daemon_info::HARNESS_SOURCE_DIGEST
        );
        assert!(inputs.get("rsid_build_sha").is_none());
        assert_eq!(data["rsid_build_sha"], crate::daemon_info::BUILD_SHA);
        assert_eq!(
            data["harness_source_digest"],
            crate::daemon_info::HARNESS_SOURCE_DIGEST
        );
        assert_eq!(
            data["effective_inputs"]
                .as_object()
                .map(serde_json::Map::len),
            Some(5)
        );
        assert_eq!(
            inputs["tool_policy"]["enabled_tools"]
                .as_array()
                .map(Vec::len),
            Some(7)
        );
        assert_eq!(inputs["tool_policy"]["web_access"], "disabled");
        // The row the monitor stores carries the same digest for S4 to read.
        let mut sequence = 0;
        let rows = crate::session::SessionManager::convert_recognized_stream_event(
            digests[0],
            uuid::Uuid::new_v4(),
            &mut sequence,
        )
        .expect("the digest event is persisted");
        let health = &rows[0].metadata.as_ref().expect("metadata")["provider_health"];
        assert_eq!(health["kind"], "bench_build_digest");
        assert_eq!(health["harness_build_digest"], digest);

        // A normal launch: worktree base, host tool set, no digest, no knobs.
        let (body, events) =
            recorded_launch("normal-launch-model", supplemental, None, "task", |_| {}).await;
        let system = body["messages"][0]["content"].as_str().expect("system");
        assert!(system.starts_with(base_prompt::base_instructions()));
        assert!(!system.contains(base_prompt::bench_instructions()));
        let names: Vec<&str> = body["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .filter_map(|tool| tool["function"]["name"].as_str())
            .collect();
        assert!(names.contains(&"read_file") && names.contains(&"apply_patch"));
        assert!(
            events
                .iter()
                .all(|event| event.event_type != build_digest::BUILD_DIGEST_STREAM_EVENT)
        );
    }

    /// #3191: bench launches that run at the same time with different run overlays
    /// each send their own prompt body, advertise their own narrowed tools and run
    /// their own iteration cap and compaction budget; the fixed header is in every
    /// prompt and a launch with no overlay is the unmodified bench launch.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_bench_launches_with_different_overlays_each_run_their_own_configuration() {
        use rsi_common::bench_overlay::{BenchOverlay, RawOverlay};
        use rsi_common::bench_profile::{BenchLaunchProfile, LoopKnobs};
        let overlay = |value: serde_json::Value, body: &str| -> BenchOverlay {
            serde_json::from_value::<RawOverlay>(value)
                .unwrap()
                .resolve(Some(body.as_bytes()))
                .unwrap()
                .expect("an overlay that sets something")
        };
        let alpha = overlay(
            serde_json::json!({
                "instructions": "i.md",
                "tool_policy": {"denied_tools": ["update_plan"]},
                "context": {"compact_budget_tokens": 96_000},
                "loop": {"max_iterations": 120},
            }),
            "<approach>\nALPHA-BODY\n</approach>",
        );
        let beta = overlay(
            serde_json::json!({
                "instructions": "i.md",
                "tool_policy": {"enabled_tools": ["shell", "exec_command", "read_output"]},
                "loop": {"max_iterations": 300},
            }),
            "<approach>\nBETA-BODY\n</approach>",
        );
        let launch = |model: &'static str, overlay: Option<BenchOverlay>| async move {
            recorded_launch(model, "Solve it.".into(), None, "the task", move |config| {
                config.exec_target = bench_container_target();
                // What the bench task start builds: the overlay's knobs over the config's.
                config.loop_knobs = overlay
                    .as_ref()
                    .map_or_else(LoopKnobs::default, |o| o.apply_knobs(LoopKnobs::default()));
                config.bench_profile = Some(BenchLaunchProfile {
                    overlay,
                    ..BenchLaunchProfile::default()
                });
            })
            .await
        };
        let (a, b, none) = tokio::join!(
            launch("overlay-alpha-model", Some(alpha.clone())),
            launch("overlay-beta-model", Some(beta.clone())),
            launch("overlay-none-model", None),
        );
        let system = |body: &serde_json::Value| -> String {
            body["messages"][0]["content"]
                .as_str()
                .expect("system")
                .to_string()
        };
        let tools = |body: &serde_json::Value| -> std::collections::BTreeSet<String> {
            body["tools"]
                .as_array()
                .expect("tools")
                .iter()
                .filter_map(|tool| tool["function"]["name"].as_str().map(str::to_string))
                .collect()
        };
        let digest_data = |events: &[StreamEvent]| -> serde_json::Value {
            let found: Vec<_> = events
                .iter()
                .filter(|event| event.event_type == build_digest::BUILD_DIGEST_STREAM_EVENT)
                .collect();
            assert_eq!(found.len(), 1);
            found[0].data.clone()
        };

        // The prompts: each its own body, never the other's, the header in all of them.
        let (prompt_a, prompt_b, prompt_none) = (system(&a.0), system(&b.0), system(&none.0));
        assert!(prompt_a.contains("ALPHA-BODY") && !prompt_a.contains("BETA-BODY"));
        assert!(prompt_b.contains("BETA-BODY") && !prompt_b.contains("ALPHA-BODY"));
        assert!(!prompt_none.contains("ALPHA-BODY") && !prompt_none.contains("BETA-BODY"));
        assert!(prompt_none.starts_with(base_prompt::bench_instructions()));
        for prompt in [&prompt_a, &prompt_b, &prompt_none] {
            assert!(prompt.contains("<container>") && prompt.contains("<integrity>"));
            assert!(prompt.contains("Do not fetch Terminal-Bench, Harbor or the task's solutions"));
            assert!(
                prompt.contains("Solve it."),
                "the supplemental part follows"
            );
        }
        for prompt in [&prompt_a, &prompt_b] {
            assert!(!prompt.contains("<working>") && !prompt.contains("<tools>"));
        }

        // The tools each session advertises.
        let all: std::collections::BTreeSet<String> =
            rsi_common::harness_tool_policy::BENCH_ENABLED_TOOLS
                .iter()
                .map(|n| (*n).to_string())
                .collect();
        let mut without_plan = all.clone();
        without_plan.remove("update_plan");
        assert_eq!(tools(&a.0), without_plan);
        assert_eq!(
            tools(&b.0),
            ["shell", "exec_command", "read_output"]
                .iter()
                .map(|n| (*n).to_string())
                .collect()
        );
        assert_eq!(tools(&none.0), all);

        // The knobs, the policy and the identity each digest event records.
        let (data_a, data_b, data_none) =
            (digest_data(&a.1), digest_data(&b.1), digest_data(&none.1));
        assert_eq!(data_a["inputs"]["max_iterations"], 120);
        assert_eq!(data_a["inputs"]["compaction_budget_tokens"], 96_000);
        assert_eq!(data_b["inputs"]["max_iterations"], 300);
        assert!(data_b["inputs"]["compaction_budget_tokens"].is_null());
        assert_eq!(data_none["inputs"]["max_iterations"], 150);
        assert_eq!(
            data_a["inputs"]["tool_policy"]["denied_tools"][0],
            "update_plan"
        );
        assert_eq!(data_a["inputs"]["tool_policy"]["web_access"], "disabled");
        assert_eq!(
            data_b["inputs"]["tool_policy"]["enabled_tools"]
                .as_array()
                .map(Vec::len),
            Some(3)
        );
        assert_eq!(data_a["inputs"]["overlay"]["digest"], alpha.digest);
        assert_eq!(
            data_a["inputs"]["overlay"]["rendered_prompt_digest"],
            base_prompt::bench_base_digest(alpha.instructions_text())
        );
        assert_eq!(
            data_b["inputs"]["overlay"]["files"]["i.md"],
            beta.instructions.as_ref().unwrap().sha256
        );
        assert!(
            data_none["inputs"].get("overlay").is_none(),
            "no overlay adds no input"
        );
        let digests: std::collections::BTreeSet<&str> = [&data_a, &data_b, &data_none]
            .iter()
            .map(|d| d["harness_build_digest"].as_str().unwrap())
            .collect();
        assert_eq!(digests.len(), 3, "three configurations, three digests");
        let overlay_components: std::collections::BTreeSet<&str> = [&data_a, &data_b, &data_none]
            .iter()
            .map(|d| d["effective_inputs"]["overlay"].as_str().unwrap())
            .collect();
        assert_eq!(overlay_components.len(), 3);
        // The overlay-free launch keeps the component the unmodified bench profile had.
        assert_eq!(
            data_none["effective_inputs"]["overlay"],
            build_digest::effective_input_digests(&build_digest::BuildDigestInputs {
                harness_source_digest: String::new(),
                base_prompt_version: base_prompt::BASE_PROMPT_VERSION,
                base_prompt_digest: base_prompt::base_digest(),
                bench_prompt_version: base_prompt::BENCH_PROMPT_VERSION,
                bench_prompt_digest: base_prompt::bench_digest(),
                tool_spec_digest: String::new(),
                tool_policy: rsi_common::harness_tool_policy::HarnessToolPolicy::bench(),
                max_iterations: 150,
                compaction_budget_tokens: None,
                rotation_off: false,
                recovery_max_output_tokens: None,
                provider: String::new(),
                model: String::new(),
                effort: None,
                route: String::new(),
                overlay: None,
            })["overlay"]
        );
    }

    /// #3191: a carried overlay is re-verified at launch: a tampered one never starts.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn a_tampered_overlay_is_refused_at_launch() {
        use rsi_common::bench_overlay::RawOverlay;
        let overlay = serde_json::from_value::<RawOverlay>(serde_json::json!({
            "loop": {"max_iterations": 120}
        }))
        .unwrap()
        .resolve(None)
        .unwrap()
        .unwrap();
        let dir = Path::new("/tmp");
        let mut config = resume_launch_config(uuid::Uuid::new_v4(), "m", "q", String::new(), dir);
        config.exec_target = bench_container_target();
        config.bench_profile = Some(rsi_common::bench_profile::BenchLaunchProfile {
            overlay: Some(overlay.clone()),
            ..Default::default()
        });
        assert!(validate_bench_launch(&config).is_ok());
        let mut tampered = overlay;
        tampered.max_iterations = Some(900);
        config.bench_profile.as_mut().unwrap().overlay = Some(tampered);
        let refused = validate_bench_launch(&config).unwrap_err().to_string();
        assert!(
            refused.contains("bench_profile_invalid:overlay_digest_mismatch"),
            "{refused}"
        );
    }

    /// #3177: per-launch knobs replace the daemon globals for one session and
    /// write no global setting.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn loop_knobs_apply_per_session_and_leave_the_globals_unchanged() {
        use rsi_common::bench_profile::LoopKnobs;
        let mut client = HarnessClient::new();
        let runtime = Arc::new(crate::config::RuntimeConfig::from_config(
            &crate::config::Config::default(),
        ));
        client.set_runtime_config(Arc::clone(&runtime));
        runtime
            .update_field("harness_max_iterations_per_turn", &serde_json::json!(40))
            .unwrap();
        runtime
            .update_field(
                "openrouter_context_budget_tokens",
                &serde_json::json!(90_000),
            )
            .unwrap();
        let rotation_before = runtime
            .context_rotation_enabled
            .load(std::sync::atomic::Ordering::Relaxed);
        let global_budget = runtime.openrouter_context_budget();
        assert_eq!(global_budget, Some(90_000));

        // No knobs: the globals, exactly as before.
        let none = LoopKnobs::default();
        assert_eq!(
            resolve_loop_knobs(&none, client.turn_iteration_cap(), global_budget),
            (40, Some(90_000))
        );
        // Knobs set: this session's values, on any route (a budget with no
        // global one included).
        let knobs = LoopKnobs {
            max_iterations: Some(12),
            compaction_budget_tokens: Some(30_000),
            rotation_off: true,
            ..LoopKnobs::default()
        };
        assert_eq!(
            resolve_loop_knobs(&knobs, client.turn_iteration_cap(), global_budget),
            (12, Some(30_000))
        );
        assert_eq!(
            resolve_loop_knobs(&knobs, client.turn_iteration_cap(), None),
            (12, Some(30_000))
        );
        let cap_only = LoopKnobs {
            max_iterations: Some(25),
            ..LoopKnobs::default()
        };
        assert_eq!(
            resolve_loop_knobs(&cap_only, client.turn_iteration_cap(), global_budget),
            (25, Some(90_000))
        );

        // The next session, and the settings themselves, still see the globals.
        assert_eq!(client.turn_iteration_cap(), 40);
        assert_eq!(runtime.harness_max_iterations(), 40);
        assert_eq!(runtime.openrouter_context_budget(), global_budget);
        assert_eq!(
            runtime
                .context_rotation_enabled
                .load(std::sync::atomic::Ordering::Relaxed),
            rotation_before
        );
        assert_eq!(
            resolve_loop_knobs(&none, client.turn_iteration_cap(), global_budget),
            (40, Some(90_000))
        );
    }

    /// #3199: the overlay's compaction table reaches the launch's loop policy,
    /// per launch; no overlay or no table is the built-in policy.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn a_launch_compacts_under_its_own_overlay_policy() {
        use rsi_common::bench_overlay::RawOverlay;
        use rsi_common::bench_profile::BenchLaunchProfile;
        use rsi_common::compaction_policy::CompactionPolicy;
        let profile = |value: serde_json::Value| BenchLaunchProfile {
            overlay: serde_json::from_value::<RawOverlay>(value)
                .expect("a well-formed overlay")
                .resolve(None)
                .expect("a valid overlay"),
            ..BenchLaunchProfile::default()
        };
        assert_eq!(compaction_policy_for(None), CompactionPolicy::default());
        assert_eq!(
            compaction_policy_for(Some(&BenchLaunchProfile::default())),
            CompactionPolicy::default()
        );
        let tuned = profile(serde_json::json!({"compaction": {
            "message_trigger": 90,
            "keep_recent": 12,
            "window_percent": 60,
            "summary_tokens": 4000,
            "summary_instruction": "Summarize briefly.",
        }}));
        let other = profile(serde_json::json!({"compaction": {"keep_recent": 6}}));
        let no_table = profile(serde_json::json!({"loop": {"max_iterations": 40}}));
        assert_eq!(
            compaction_policy_for(Some(&tuned)),
            CompactionPolicy {
                message_trigger: 90,
                keep_recent: 12,
                window_percent: 60,
                summary_tokens: Some(4000),
                summary_instruction: Some("Summarize briefly.".into()),
            }
        );
        assert_eq!(compaction_policy_for(Some(&other)).keep_recent, 6);
        assert_eq!(
            compaction_policy_for(Some(&other)).message_trigger,
            CompactionPolicy::default().message_trigger
        );
        assert_eq!(
            compaction_policy_for(Some(&no_table)),
            CompactionPolicy::default()
        );
    }

    /// #3171: a per-launch iteration cap is clamped to the operator's range
    /// (10-1000); an in-range value is kept and no cap falls back to the global.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn per_launch_iteration_cap_clamps_to_the_operator_range() {
        use crate::config::{
            HARNESS_MAX_ITERATIONS_PER_TURN_MAX as MAX, HARNESS_MAX_ITERATIONS_PER_TURN_MIN as MIN,
        };
        use rsi_common::bench_profile::LoopKnobs;
        let cap = |requested: Option<u32>| {
            resolve_loop_knobs(
                &LoopKnobs {
                    max_iterations: requested,
                    ..LoopKnobs::default()
                },
                40,
                None,
            )
            .0
        };
        assert_eq!(cap(None), 40);
        assert_eq!(cap(Some(1)), MIN);
        assert_eq!(cap(Some(MIN)), MIN);
        assert_eq!(cap(Some(80)), 80);
        assert_eq!(cap(Some(MAX)), MAX);
        assert_eq!(cap(Some(LoopKnobs::MAX_ITERATIONS_LIMIT)), MAX);
    }

    /// #3177: a bench profile needs a container target, and knobs stay in range;
    /// the typed refusals are `bench_profile_invalid:<reason>`.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn a_bench_launch_needs_a_container_target_and_in_range_knobs() {
        let dir = Path::new("/tmp");
        let mut config = resume_launch_config(uuid::Uuid::new_v4(), "m", "q", String::new(), dir);
        assert!(validate_bench_launch(&config).is_ok());
        assert!(config.bench().is_none());

        config.bench_profile = Some(rsi_common::bench_profile::BenchLaunchProfile::default());
        let refused = validate_bench_launch(&config).unwrap_err().to_string();
        assert!(
            refused.contains("bench_profile_invalid:container_target_required"),
            "{refused}"
        );
        config.exec_target = bench_container_target();
        assert!(validate_bench_launch(&config).is_ok());

        // A container-bound session is a bench session even with no explicit profile.
        config.bench_profile = None;
        assert!(config.bench().is_some());

        config.loop_knobs.max_iterations = Some(0);
        let refused = validate_bench_launch(&config).unwrap_err().to_string();
        assert!(
            refused.contains("bench_profile_invalid:max_iterations_out_of_range"),
            "{refused}"
        );
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
