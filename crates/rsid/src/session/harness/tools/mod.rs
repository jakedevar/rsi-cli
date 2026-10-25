//! Trait-based tool system for the agent harness.
//!
//! Each tool is a struct implementing `HarnessTool`. Built-in tools provide
//! file I/O, shell execution, and git operations. The registry collects tools
//! and dispatches execution by name.

pub mod arguments;
#[cfg(test)]
mod bench_policy_tests; // #3177
pub mod codegraph;
mod completion_gate;
mod custody_guard;
pub mod exec;
mod exec_pty;
pub(crate) mod exec_target;
#[cfg(all(test, unix))]
mod exec_target_tests;
pub mod git;
pub mod memory;
pub(crate) mod process_registry;
pub mod rsi_control;
pub mod schedule_wake;
pub mod shell;
pub mod utility;
pub mod web_fetch;
pub mod web_search;

// #2616: the tool trait, context, limits and the pure file/navigation tools live in
// `rsid-harness`. These re-exports keep every old `harness::tools::<name>` path.
#[allow(unused_imports)] // Preserves the old module paths for callers and tests.
pub use rsid_harness::tools::{
    HarnessTool, ToolContext, ToolExecutionMode, ToolPolicy, apply_patch, file, glob, grep,
    is_system_blocked, list_files, policy, read_output, suggest_tool_names, view_image,
};
pub(crate) use rsid_harness::tools::{MAX_LISTED_TOOL_NAMES, truncation};

use crate::claude::StreamEvent;
use crate::session::agent_verbs::AgentControlHandle;
use crate::session::harness::mcp::StdioMcpClient;
use crate::session::harness::types::{HarnessToolSpec, ToolResult};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, RwLock};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Registry of available tools for the agent loop.
pub struct HarnessToolRegistry {
    tools: HashMap<String, Arc<dyn HarnessTool>>,
    order: Vec<String>,
    session_id: Option<uuid::Uuid>,
    /// #1097: where large tool outputs spill (shared rsi-common store).
    spill: Arc<rsi_common::spill::SpillConfig>,
    process_registry: Option<Arc<exec::ProcessRegistry>>,
    mcp_clients: Vec<Arc<tokio::sync::Mutex<StdioMcpClient>>>,
    utility_state: Arc<utility::UtilityToolState>,
    /// #792: per-session tool policy. `None` is the unrestricted default.
    policy: Option<Arc<policy::ToolPolicyRuntime>>,
    launch_invocation_id: Option<uuid::Uuid>,
    execution_scratch: Option<crate::sandbox::execution_scratch::SandboxExecutionScratch>,
    deferred_mcp: Arc<RwLock<DeferredMcpCatalog>>,
    /// #3176: where this session's shell tools run. A container target refuses
    /// every host-touching tool at every execution entry point.
    exec_target: rsi_common::exec_target::ExecTarget,
    /// #3178: a bench session's live trace, written by the loop ports.
    trace: Option<Arc<crate::bench::trace::TraceSink>>,
    /// #4028: the launch's output-token ceiling for a length-stop recovery call.
    recovery_max_output_tokens: Option<u32>,
}

pub(crate) struct ProcessTurnGuard(Option<Arc<exec::ProcessRegistry>>);

impl Drop for ProcessTurnGuard {
    fn drop(&mut self) {
        if let Some(registry) = self.0.take() {
            registry.end_turn();
        }
    }
}

#[derive(Default)]
pub(crate) struct DeferredMcpCatalog {
    hidden: HashSet<String>,
    revealed: HashSet<String>,
}

impl DeferredMcpCatalog {
    fn is_hidden(&self, name: &str) -> bool {
        self.hidden.contains(name) && !self.revealed.contains(name)
    }

    pub(crate) fn reveal(&mut self, names: impl IntoIterator<Item = String>) {
        self.revealed.extend(names);
    }
}

impl HarnessToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
            order: Vec::new(),
            session_id: None,
            spill: Arc::new(default_spill_config()),
            process_registry: None,
            mcp_clients: Vec::new(),
            utility_state: Arc::new(utility::UtilityToolState::default()),
            policy: None,
            launch_invocation_id: None,
            execution_scratch: None,
            deferred_mcp: Arc::new(RwLock::new(DeferredMcpCatalog::default())),
            exec_target: rsi_common::exec_target::ExecTarget::Host,
            trace: None,
            recovery_max_output_tokens: None,
        }
    }

    /// #4028: set the length-stop recovery ceiling. Set once, before the registry
    /// is frozen into the agent loop.
    pub fn set_recovery_max_output_tokens(&mut self, cap: Option<u32>) {
        self.recovery_max_output_tokens = cap;
    }

    /// The launch's length-stop recovery ceiling, if it set one.
    #[must_use]
    pub fn recovery_max_output_tokens(&self) -> Option<u32> {
        self.recovery_max_output_tokens
    }

    /// #3178: attach a bench session's live trace. Set once, before the registry
    /// is frozen into the agent loop; the loop ports record into it.
    pub fn set_trace(&mut self, trace: Arc<crate::bench::trace::TraceSink>) {
        self.trace = Some(trace);
    }

    /// The attached bench trace, if any.
    #[must_use]
    pub(crate) fn trace_sink(&self) -> Option<Arc<crate::bench::trace::TraceSink>> {
        self.trace.clone()
    }

    /// Bind the session's tool policy. Set once, before the registry is frozen
    /// into the agent loop.
    pub fn set_policy(&mut self, policy: Arc<policy::ToolPolicyRuntime>) {
        self.policy = Some(policy);
    }

    /// The session's network egress policy (#774); `deny_private` when no
    /// tool policy runtime is attached.
    #[must_use]
    pub fn egress_policy(&self) -> rsi_common::egress_policy::EgressPolicy {
        self.policy
            .as_ref()
            .map(|policy| policy.egress_policy())
            .unwrap_or_default()
    }

    /// Where the file tools may reach: the working directory unless the
    /// session policy opts into the task environment (#3172).
    pub fn file_path_scope(&self) -> rsi_common::harness_tool_policy::FilePathScope {
        self.policy
            .as_ref()
            .map(|policy| policy.file_path_scope())
            .unwrap_or_default()
    }

    /// Extra environment names a host `shell` / `exec_command` launch passes
    /// through (#3169); empty unless the session policy lists some.
    pub fn env_passthrough(&self) -> Arc<[String]> {
        self.policy
            .as_ref()
            .map(|policy| policy.env_passthrough())
            .unwrap_or_else(|| Arc::from([]))
    }

    /// Server-side context editing is requested for this session (default on;
    /// only the direct Anthropic provider acts on it).
    #[must_use]
    pub fn context_editing_enabled(&self) -> bool {
        self.policy
            .as_ref()
            .is_none_or(|policy| policy.policy().context_editing_enabled())
    }

    #[must_use]
    pub fn policy(&self) -> Option<&Arc<policy::ToolPolicyRuntime>> {
        self.policy.as_ref()
    }

    /// The policy gate shared by every execution entry point: a denial or an
    /// exhausted budget settles as a tool-error result without running the tool.
    fn policy_refusal(&self, name: &str) -> Option<ToolResult> {
        // #3176: a container-bound session never runs a tool on the daemon host.
        // Besides leaving them unregistered, a call that still names one (a
        // stale transcript, a forced call) is refused here, before the tool runs.
        if self.exec_target.is_container()
            && !exec_target::CONTAINER_TARGET_TOOLS.contains(&name)
            && (exec_target::is_host_tool(name) || self.tools.contains_key(name))
        {
            return Some(ToolResult {
                success: false,
                output: String::new(),
                error_msg: Some(exec_target::host_tool_refusal_message(name)),
            });
        }
        self.policy
            .as_ref()?
            .admit_call(name, false)
            .err()
            .map(policy::PolicyError::into_tool_result)
    }

    fn deferred_mcp_error(&self, name: &str) -> Option<ToolResult> {
        if self
            .deferred_mcp
            .read()
            .expect("deferred MCP state lock")
            .is_hidden(name)
        {
            return Some(ToolResult {
                success: false,
                output: String::new(),
                error_msg: Some("mcp_tool_not_revealed: use tool_search first".to_string()),
            });
        }
        None
    }

    /// Drop provider-hosted web specs the session policy refuses or whose
    /// budget is used up, so they are never sent to the provider.
    #[must_use]
    pub fn filter_hosted_specs(&self, specs: Vec<HarnessToolSpec>) -> Vec<HarnessToolSpec> {
        let Some(policy) = &self.policy else {
            return specs;
        };
        specs
            .into_iter()
            .filter(|spec| policy.hosted_spec_allowed(&spec.name))
            .collect()
    }

    pub(crate) async fn execute_completion_gate(
        &self,
        gate: &rsi_common::completion_gates::CompletionGate,
        working_dir: &Path,
        cancel: &CancellationToken,
    ) -> completion_gate::CompletionGateOutcome {
        // A gate runs a host shell; a container-bound session has none, so the
        // gate fails closed instead of running on the host (#3176).
        if self.exec_target.is_container() {
            return completion_gate::CompletionGateOutcome {
                success: false,
                output: exec_target::host_tool_refusal_message("completion_gate"),
                exit_code: None,
                timed_out: false,
                cancelled: false,
            };
        }
        completion_gate::execute_completion_gate(
            &gate.command,
            working_dir,
            std::time::Duration::from_secs(gate.timeout_secs),
            gate.max_output_bytes,
            self.session_id,
            self.launch_invocation_id,
            self.execution_scratch.as_ref(),
            self.egress_policy().mode,
            cancel,
        )
        .await
    }

    fn record_policy_bytes(&self, result: &ToolResult) {
        if let Some(policy) = &self.policy {
            policy.record_result_bytes(result.output.len());
        }
    }

    pub fn register(&mut self, tool: Arc<dyn HarnessTool>) {
        self.try_register(tool)
            .expect("built-in Harness tool names must be unique");
    }

    /// Swap a registered tool for a test double of the same name, keeping the
    /// roster order. The production web tools refuse loopback servers, so an
    /// end-to-end scenario built on [`Self::default_tools`] aims them at a
    /// local page server this way (#2259).
    #[cfg(test)]
    pub(crate) fn replace_for_test(&mut self, tool: Arc<dyn HarnessTool>) {
        let name = tool.name().to_string();
        assert!(
            self.tools.insert(name.clone(), tool).is_some(),
            "replace_for_test: `{name}` is not registered"
        );
    }

    /// Register a tool without changing the existing roster on a duplicate.
    pub fn try_register(&mut self, tool: Arc<dyn HarnessTool>) -> Result<(), String> {
        let name = tool.name().to_string();
        if self.tools.contains_key(&name) {
            return Err(format!("Duplicate Harness tool: {name}"));
        }
        self.tools.insert(name.clone(), tool);
        self.order.push(name);
        Ok(())
    }

    /// JSON Schema string for a registered tool's parameters, if any.
    pub fn parameters_json(&self, name: &str) -> Option<&str> {
        self.tools.get(name).map(|tool| tool.parameters_json())
    }

    /// The advertised catalog. Tools the session policy refuses are omitted.
    pub fn specs(&self) -> Vec<HarnessToolSpec> {
        let deferred_mcp = self.deferred_mcp.read().expect("deferred MCP state lock");
        self.order
            .iter()
            .filter(|name| {
                self.policy
                    .as_ref()
                    .is_none_or(|policy| policy.tool_permitted(name, false).is_ok())
                    && !deferred_mcp.is_hidden(name)
            })
            .map(|name| self.tools[name].to_spec())
            .collect()
    }

    pub fn execution_mode(&self, name: &str) -> Option<ToolExecutionMode> {
        self.tools.get(name).map(|tool| tool.execution_mode())
    }

    pub(crate) async fn take_process_notices(&self) -> Vec<String> {
        match &self.process_registry {
            Some(registry) => registry.take_notices().await,
            None => Vec::new(),
        }
    }

    pub(crate) async fn process_turn_guard(self: &Arc<Self>) -> ProcessTurnGuard {
        let registry = self
            .process_registry
            .as_ref()
            .map(|registry| Arc::clone(registry));
        if let Some(registry) = &registry {
            registry.begin_turn().await;
        }
        ProcessTurnGuard(registry)
    }

    #[cfg(test)]
    pub(crate) async fn shutdown_processes(&self) {
        if let Some(registry) = &self.process_registry {
            registry.shutdown_processes().await;
        }
        for client in &self.mcp_clients {
            let mut client = client.lock().await;
            let _ = client.terminate().await;
        }
    }

    pub(crate) fn register_mcp_client(&mut self, client: Arc<tokio::sync::Mutex<StdioMcpClient>>) {
        self.mcp_clients.push(client);
    }

    pub(crate) fn set_deferred_mcp_tools(
        &mut self,
        names: impl IntoIterator<Item = String>,
    ) -> Arc<RwLock<DeferredMcpCatalog>> {
        let names: HashSet<String> = names.into_iter().collect();
        let mut state = self.deferred_mcp.write().expect("deferred MCP state lock");
        state.hidden = names;
        state.revealed.clear();
        drop(state);
        Arc::clone(&self.deferred_mcp)
    }

    pub async fn execute_with_context(
        &self,
        name: &str,
        args: serde_json::Value,
        working_dir: &Path,
        cancel: &CancellationToken,
        event_sink: Option<mpsc::Sender<StreamEvent>>,
    ) -> ToolResult {
        let context = ToolContext {
            session_id: self.session_id,
            working_dir: working_dir.to_path_buf(),
            cancel: cancel.clone(),
            event_sink,
            policy: ToolPolicy {
                egress: self.egress_policy(),
                file_paths: self.file_path_scope(),
                env_passthrough: self.env_passthrough(),
                ..ToolPolicy::default()
            },
        };
        if let Some(refusal) = self.policy_refusal(name) {
            return refusal;
        }
        if let Some(refusal) = self.deferred_mcp_error(name) {
            return refusal;
        }
        if let Some(refusal) = self.argument_refusal(name, &args) {
            return refusal;
        }
        let schema_error = rsi_control::schema_error(name, &args);
        let mut result = match self.tools.get(name) {
            Some(tool) => self.finish_result(
                name,
                tool.execute_with_context(args, &context).await,
                context.policy.max_output_bytes,
            ),
            None => self.unknown_tool_result(name),
        };
        rsi_control::enrich_schema_refusal(schema_error, &mut result);
        self.record_policy_bytes(&result);
        result
    }

    /// The refusal for arguments that do not fit the tool's JSON Schema: every
    /// missing or invalid field and the expected type or enum, with the tool
    /// not run (#2257). Native control tools keep their own, richer diagnostic
    /// (`rsi_control::schema_error`), so they are not checked here.
    fn argument_refusal(&self, name: &str, args: &serde_json::Value) -> Option<ToolResult> {
        let tool = self.tools.get(name)?;
        if rsi_control::has_catalog_schema(name) {
            return None;
        }
        let schema: serde_json::Value = serde_json::from_str(tool.parameters_json()).ok()?;
        let problems = arguments::validate_with_schema(args, &schema);
        if problems.is_empty() {
            return None;
        }
        Some(ToolResult {
            success: false,
            output: String::new(),
            error_msg: Some(arguments::invalid_arguments_message(
                name, args, &schema, &problems,
            )),
        })
    }

    /// Names the model can call now: registration order, minus tools the
    /// session policy refuses and MCP tools still deferred.
    fn callable_names(&self) -> Vec<String> {
        let deferred_mcp = self.deferred_mcp.read().expect("deferred MCP state lock");
        self.order
            .iter()
            .filter(|name| {
                self.policy
                    .as_ref()
                    .is_none_or(|policy| policy.tool_permitted(name, false).is_ok())
                    && !deferred_mcp.is_hidden(name)
            })
            .cloned()
            .collect()
    }

    /// The error for a call to a tool that is not registered (or an empty
    /// name): close names first, then the callable names (#2257).
    pub(crate) fn unknown_tool_message(&self, name: &str) -> String {
        let callable = self.callable_names();
        let mut message = if name.trim().is_empty() {
            "The tool call has an empty name.".to_string()
        } else {
            format!("Unknown tool: {name}.")
        };
        let suggestions = suggest_tool_names(name, &callable);
        if !suggestions.is_empty() {
            message.push_str(&format!(" Did you mean: {}?", suggestions.join(", ")));
        }
        if !callable.is_empty() {
            let shown = callable.len().min(MAX_LISTED_TOOL_NAMES);
            message.push_str(&format!(
                " Available tools: {}",
                callable[..shown].join(", ")
            ));
            if callable.len() > shown {
                message.push_str(&format!(" (and {} more)", callable.len() - shown));
            }
            message.push('.');
        }
        message
    }

    fn unknown_tool_result(&self, name: &str) -> ToolResult {
        ToolResult {
            success: false,
            output: String::new(),
            error_msg: Some(self.unknown_tool_message(name)),
        }
    }

    pub(crate) fn set_context_usage(&self, used_tokens: u64, token_limit: u64) {
        self.utility_state
            .set_context_usage(used_tokens, token_limit);
    }

    pub(crate) async fn take_compaction_focus(&self) -> Option<String> {
        self.utility_state.take_compaction_focus().await
    }

    #[cfg(test)]
    pub async fn execute(
        &self,
        name: &str,
        args: serde_json::Value,
        working_dir: &Path,
    ) -> ToolResult {
        if let Some(refusal) = self.policy_refusal(name) {
            return refusal;
        }
        if let Some(refusal) = self.deferred_mcp_error(name) {
            return refusal;
        }
        if let Some(refusal) = self.argument_refusal(name, &args) {
            return refusal;
        }
        let schema_error = rsi_control::schema_error(name, &args);
        let mut result = match self.tools.get(name) {
            Some(tool) => self.finish_result(
                name,
                tool.execute(args, working_dir).await,
                ToolPolicy::default().max_output_bytes,
            ),
            None => self.unknown_tool_result(name),
        };
        rsi_control::enrich_schema_refusal(schema_error, &mut result);
        self.record_policy_bytes(&result);
        result
    }

    #[cfg(test)]
    pub async fn execute_cancellable(
        &self,
        name: &str,
        args: serde_json::Value,
        working_dir: &Path,
        cancel: &CancellationToken,
    ) -> ToolResult {
        if let Some(refusal) = self.policy_refusal(name) {
            return refusal;
        }
        if let Some(refusal) = self.deferred_mcp_error(name) {
            return refusal;
        }
        if let Some(refusal) = self.argument_refusal(name, &args) {
            return refusal;
        }
        let schema_error = rsi_control::schema_error(name, &args);
        let mut result = match self.tools.get(name) {
            Some(tool) => self.finish_result(
                name,
                tool.execute_cancellable(args, working_dir, cancel).await,
                ToolPolicy::default().max_output_bytes,
            ),
            None => self.unknown_tool_result(name),
        };
        rsi_control::enrich_schema_refusal(schema_error, &mut result);
        self.record_policy_bytes(&result);
        result
    }

    /// Build the default tool set for harness sessions.
    ///
    /// `project_id` is the session's project scope at launch time. When
    /// `Some`, `memory_search` is restricted to that project's indexed
    /// memory; the tool struct captures it and never exposes it via JSON
    /// schema (plan §D2).
    ///
    /// `store`, `origin_session_id`, `default_working_dir`, `provider`, and
    /// `model` are used to construct the `schedule_wake` tool when `store`
    /// is provided.
    ///
    /// `agent_control` (with `origin_session_id` as the bound caller) enables
    /// the native `rsi_control` spawn/status/halt tools, and (A8.1 Q3) hands
    /// `schedule_wake` its watch-arming authority — with both present the
    /// tool advertises `mode:"on_terminal"`. The caller session id is
    /// captured here at construction and never exposed via any tool's JSON
    /// schema, so the agent cannot spawn/observe/halt on behalf of another
    /// session (P2 binding invariant). This is the single registration path
    /// used by BOTH fresh launch and rotation, so both carry an identical tool
    /// set; store-gated tools (memory, schedule_wake) still construct a valid
    /// registry when `store`/`memory_handle` are absent.
    ///
    /// `image_input_supported` is resolved from the actual provider route and
    /// transformed model; the tool remains discoverable on non-vision routes
    /// so attempted calls return an explicit unsupported error.
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub fn default_tools(
        memory_handle: Option<&crate::memory::worker::MemoryHandle>,
        project_id: Option<uuid::Uuid>,
        store: Option<Arc<rsid_core::store_lock::Mutex<crate::store::Store>>>,
        origin_session_id: Option<uuid::Uuid>,
        launch_invocation_id: Option<uuid::Uuid>,
        default_working_dir: Option<std::path::PathBuf>,
        provider: Option<rsi_common::types::SessionProvider>,
        model: Option<String>,
        agent_control: Option<AgentControlHandle>,
        image_input_supported: bool,
    ) -> Self {
        Self::default_tools_with_execution_scratch(
            memory_handle,
            project_id,
            store,
            origin_session_id,
            launch_invocation_id,
            default_working_dir,
            provider,
            model,
            agent_control,
            image_input_supported,
            None,
        )
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub fn default_tools_with_execution_scratch(
        memory_handle: Option<&crate::memory::worker::MemoryHandle>,
        project_id: Option<uuid::Uuid>,
        store: Option<Arc<rsid_core::store_lock::Mutex<crate::store::Store>>>,
        origin_session_id: Option<uuid::Uuid>,
        launch_invocation_id: Option<uuid::Uuid>,
        default_working_dir: Option<std::path::PathBuf>,
        provider: Option<rsi_common::types::SessionProvider>,
        model: Option<String>,
        agent_control: Option<AgentControlHandle>,
        image_input_supported: bool,
        execution_scratch: Option<crate::sandbox::execution_scratch::SandboxExecutionScratch>,
    ) -> Self {
        Self::default_tools_with_process_registry(
            memory_handle,
            project_id,
            store,
            origin_session_id,
            launch_invocation_id,
            default_working_dir,
            provider,
            model,
            agent_control,
            image_input_supported,
            execution_scratch,
            exec::default_process_registry(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn default_tools_with_process_registry(
        memory_handle: Option<&crate::memory::worker::MemoryHandle>,
        project_id: Option<uuid::Uuid>,
        store: Option<Arc<rsid_core::store_lock::Mutex<crate::store::Store>>>,
        origin_session_id: Option<uuid::Uuid>,
        launch_invocation_id: Option<uuid::Uuid>,
        default_working_dir: Option<std::path::PathBuf>,
        provider: Option<rsi_common::types::SessionProvider>,
        model: Option<String>,
        agent_control: Option<AgentControlHandle>,
        image_input_supported: bool,
        execution_scratch: Option<crate::sandbox::execution_scratch::SandboxExecutionScratch>,
        process_registry: Arc<exec::ProcessRegistry>,
    ) -> Self {
        let mut registry = Self::new();
        registry.session_id = origin_session_id;
        registry.launch_invocation_id = launch_invocation_id;
        registry.execution_scratch = execution_scratch.clone();
        let utility_state = Arc::clone(&registry.utility_state);
        registry.register(Arc::new(utility::UpdatePlanTool));
        registry.register(Arc::new(utility::ContextStatusTool::new(Arc::clone(
            &utility_state,
        ))));
        registry.register(Arc::new(utility::CompactTool::new(
            utility_state,
            store.clone(),
        )));
        registry.register(Arc::new(file::ReadFileTool));
        registry.register(Arc::new(file::WriteFileTool));
        registry.register(Arc::new(file::EditFileTool));
        registry.register(Arc::new(apply_patch::ApplyPatchTool));
        // The shell keeps the construction-bound session stamp even though it
        // scrubs every ambient variable. Startup and resume orphan exclusion
        // can therefore identify a command that outlives a daemon crash.
        registry.register(Arc::new(shell::ShellTool::new(
            origin_session_id,
            launch_invocation_id,
            execution_scratch.clone(),
        )));
        registry.register(Arc::new(exec::ExecCommandTool::new(
            Arc::clone(&process_registry),
            origin_session_id,
            launch_invocation_id,
            execution_scratch,
        )));
        registry.register(Arc::new(exec::WriteStdinTool::new(Arc::clone(
            &process_registry,
        ))));
        registry.register(Arc::new(git::GitTool::new(
            origin_session_id,
            launch_invocation_id,
        )));
        registry.register(Arc::new(list_files::ListFilesTool));
        // #2254: native content and path search, so a model does not shell out
        // to `rg` or `find` (no external binary, same sandbox and cancel rules).
        registry.register(Arc::new(grep::GrepTool));
        registry.register(Arc::new(glob::GlobTool));
        registry.register(Arc::new(read_output::ReadOutputTool::new(
            registry.spill_config(),
        )));
        registry.register(Arc::new(view_image::ViewImageTool::new(
            image_input_supported,
        )));
        // #2250: the native fetch for routes without a hosted one. The agent
        // loop drops this spec when the route advertises its own `web_fetch`.
        registry.register(Arc::new(web_fetch::WebFetchTool::new()));
        if let Some(handle) = memory_handle {
            registry.register(Arc::new(memory::MemorySearchTool::new(
                handle.clone(),
                project_id,
            )));
        }
        if let Some(store) = store {
            registry.register(Arc::new(schedule_wake::ScheduleWakeTool::new(
                store,
                origin_session_id,
                default_working_dir.unwrap_or_else(|| std::path::PathBuf::from("/")),
                provider,
                model,
                project_id,
                agent_control.clone(),
            )));
        }
        // Native rsi_control coordination: gated on a control handle AND a
        // known caller session id (bound here, never agent-supplied). Requires
        // no store of its own — the handle carries the collaborators it needs.
        if let (Some(control), Some(caller)) = (agent_control, origin_session_id) {
            registry.register(Arc::new(rsi_control::RsiControlReserveSuccessorTool::new(
                control.clone(),
                caller,
            )));
            registry.register(Arc::new(rsi_control::RsiControlSpawnTool::new(
                control.clone(),
                caller,
            )));
            registry.register(Arc::new(rsi_control::RsiControlStatusTool::new(
                control.clone(),
                caller,
            )));
            registry.register(Arc::new(rsi_control::RsiControlProgressTool::new(
                control.clone(),
                caller,
            )));
            registry.register(Arc::new(rsi_control::RsiControlReadSessionEventsTool::new(
                control.clone(),
                caller,
            )));
            registry.register(Arc::new(
                rsi_control::RsiControlQueryFailureSignaturesTool::new(control.clone(), caller),
            ));
            registry.register(Arc::new(rsi_control::RsiControlWorkerRosterTool::new(
                control.clone(),
                caller,
            )));
            registry.register(Arc::new(rsi_control::RsiControlListWakesTool::new(
                control.clone(),
                caller,
            )));
            for verb in rsi_control::GLOBAL_NATIVE_VERBS {
                registry.register(Arc::new(rsi_control::RsiControlGlobalTool::new(
                    control.clone(),
                    caller,
                    verb,
                )));
            }
            registry.register(Arc::new(rsi_control::RsiControlSendMessageTool::new(
                control.clone(),
                caller,
            )));
            registry.register(Arc::new(rsi_control::RsiControlHaltTool::new(
                control.clone(),
                caller,
            )));
            registry.register(Arc::new(rsi_control::RsiControlProgramGuardTool::new(
                control.clone(),
                caller,
            )));
            registry.register(Arc::new(rsi_control::RsiControlCreateIssueTool::new(
                control.clone(),
                caller,
            )));
            for kind in [
                rsi_control::IssueControlToolKind::List,
                rsi_control::IssueControlToolKind::Get,
                rsi_control::IssueControlToolKind::Update,
                rsi_control::IssueControlToolKind::UpdateStatus,
                rsi_control::IssueControlToolKind::Archive,
                rsi_control::IssueControlToolKind::Restore,
                rsi_control::IssueControlToolKind::ListEvents,
            ] {
                registry.register(Arc::new(rsi_control::RsiControlIssueTool::new(
                    control.clone(),
                    caller,
                    kind,
                )));
            }
            for kind in rsi_control::ManagerControlToolKind::ALL {
                registry.register(Arc::new(rsi_control::RsiControlManagerTool::new(
                    control.clone(),
                    caller,
                    kind,
                )));
            }
            for verb in crate::session::topology_agent_verbs::TOPOLOGY_AGENT_VERBS {
                registry.register(Arc::new(rsi_control::RsiControlTopologyTool::new(
                    control.clone(),
                    caller,
                    verb,
                )));
            }
        }
        registry.process_registry = Some(process_registry);
        registry
    }
}

impl HarnessToolRegistry {
    /// #3176: the tool set of a container-bound session: the plan, context and
    /// compaction tools, `read_output`, and `shell`, `exec_command` and
    /// `write_stdin` running through `docker exec` in the target container. No
    /// file tool, `apply_patch`, `git`, `view_image`, web, codegraph, memory,
    /// `schedule_wake` or `rsi_control` tool is registered, so nothing can touch
    /// the daemon host.
    pub(crate) fn container_target_tools(
        target: &rsi_common::exec_target::ContainerExecTarget,
        store: Option<Arc<rsid_core::store_lock::Mutex<crate::store::Store>>>,
        origin_session_id: Option<uuid::Uuid>,
        launch_invocation_id: Option<uuid::Uuid>,
        process_registry: Arc<exec::ProcessRegistry>,
    ) -> Self {
        let exec_target = rsi_common::exec_target::ExecTarget::Container(target.clone());
        let mut registry = Self::new();
        registry.session_id = origin_session_id;
        registry.launch_invocation_id = launch_invocation_id;
        registry.exec_target = exec_target.clone();
        let utility_state = Arc::clone(&registry.utility_state);
        registry.register(Arc::new(utility::UpdatePlanTool));
        registry.register(Arc::new(utility::ContextStatusTool::new(Arc::clone(
            &utility_state,
        ))));
        registry.register(Arc::new(utility::CompactTool::new(utility_state, store)));
        registry.register(Arc::new(
            shell::ShellTool::new(origin_session_id, launch_invocation_id, None)
                .with_exec_target(exec_target.clone()),
        ));
        registry.register(Arc::new(
            exec::ExecCommandTool::new(
                Arc::clone(&process_registry),
                origin_session_id,
                launch_invocation_id,
                None,
            )
            .with_exec_target(exec_target),
        ));
        registry.register(Arc::new(exec::WriteStdinTool::new(Arc::clone(
            &process_registry,
        ))));
        registry.register(Arc::new(read_output::ReadOutputTool::new(
            registry.spill_config(),
        )));
        registry.process_registry = Some(process_registry);
        registry
    }

    /// The exec target this registry's shell tools run against.
    #[cfg(test)]
    pub(crate) fn exec_target(&self) -> &rsi_common::exec_target::ExecTarget {
        &self.exec_target
    }
}

impl Default for HarnessToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Production reads the environment; unit tests never write to the real store.
fn default_spill_config() -> rsi_common::spill::SpillConfig {
    #[cfg(test)]
    {
        let mut config = rsi_common::spill::SpillConfig::from_lookup(&|_| None);
        config.root = std::env::temp_dir().join("rsid-harness-test-spill");
        config.disabled = false;
        config
    }
    #[cfg(not(test))]
    rsi_common::spill::SpillConfig::from_env()
}

/// Tools whose output the model asked for verbatim: they spill only when the
/// text would otherwise be cut at the hard cap, never at the 8 KB threshold.
/// A `web_fetch` page slice is already bounded by its own `max_chars` and
/// pages by `start_index`, and a `web_search` result list by its own cap, so a
/// stub would only hide the text the model asked for.
fn is_deliberate_read(name: &str) -> bool {
    matches!(
        name,
        "read_file" | "view_image" | "web_fetch" | "web_search"
    )
}

/// `Exit code: N` from a failed command result, else `0` for a successful
/// command-shaped tool, else unknown.
fn result_exit_code(name: &str, result: &ToolResult) -> Option<i32> {
    if let Some(code) = result
        .error_msg
        .as_deref()
        .and_then(|message| message.strip_prefix("Exit code: "))
        .and_then(|code| code.trim().parse().ok())
    {
        return Some(code);
    }
    let command_shaped = matches!(name, "shell" | "git" | "exec_command" | "write_stdin");
    (command_shaped && result.success).then_some(0)
}

impl HarnessToolRegistry {
    /// The spill store for this registry (the `read_output` tool reads it).
    pub fn spill_config(&self) -> Arc<rsi_common::spill::SpillConfig> {
        Arc::clone(&self.spill)
    }

    /// Replace the spill store, before tools are registered (tests).
    #[cfg(test)]
    pub fn set_spill_config(&mut self, config: rsi_common::spill::SpillConfig) {
        self.spill = Arc::new(config);
    }

    /// The spill store settings for one tool's result: the registry's, with the
    /// session policy's `output_spill` thresholds for a command tool on top
    /// (#3173). No entry keeps the default.
    pub(crate) fn spill_config_for(&self, name: &str) -> rsi_common::spill::SpillConfig {
        let mut config = (*self.spill).clone();
        if let Some(set) = self
            .policy
            .as_ref()
            .and_then(|policy| policy.policy().output_spill_for(name))
        {
            if let Some(bytes) = set.max_bytes {
                config.max_bytes = usize::try_from(bytes).unwrap_or(usize::MAX);
            }
            if let Some(lines) = set.max_lines {
                config.max_lines = usize::try_from(lines).unwrap_or(usize::MAX);
            }
        }
        config
    }

    fn spill_key(&self) -> String {
        match self.session_id {
            Some(id) => rsi_common::spill::session_key(&id.to_string()),
            None => rsi_common::spill::current_session_key(None),
        }
    }

    /// The shared output contract (#778, #1097): a large result is spilled to
    /// the store and replaced by one deterministic stub with a handle; it is
    /// never silently discarded. Applies to this new result only, so the
    /// transcript stays append-only.
    fn finish_result(
        &self,
        name: &str,
        mut result: ToolResult,
        max_output_bytes: usize,
    ) -> ToolResult {
        if result.has_typed_blocks() {
            return result;
        }
        if name == "read_output" {
            // The retrieval tool bounds itself; spilling its output again
            // would hand back a stub for the thing just asked for.
            if result.output.len() > max_output_bytes {
                result.output =
                    truncation::truncate_text(&result.output, max_output_bytes, false).content;
            }
            return result;
        }
        let exit = result_exit_code(name, &result);
        let spill = self.spill_config_for(name);
        let bounded = truncation::spill_or_truncate(
            &spill,
            &self.spill_key(),
            name,
            exit,
            &result.output,
            max_output_bytes,
            is_deliberate_read(name),
        );
        if bounded.truncated {
            result.output = bounded.content;
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct CountingTool {
        name: &'static str,
        runs: Arc<std::sync::atomic::AtomicUsize>,
        output: &'static str,
    }

    #[async_trait::async_trait]
    impl HarnessTool for CountingTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "counts its runs"
        }
        fn parameters_json(&self) -> &str {
            "{}"
        }
        async fn execute(&self, _args: serde_json::Value, _working_dir: &Path) -> ToolResult {
            self.runs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            ToolResult {
                success: true,
                output: self.output.to_string(),
                error_msg: None,
            }
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[tokio::test]
    async fn native_agent_schema_refusals_share_diagnostics_across_execution_modes() {
        struct RefusingAgentTool;
        #[async_trait::async_trait]
        impl HarnessTool for RefusingAgentTool {
            fn name(&self) -> &str {
                "rsi_control_read_session_events"
            }
            fn description(&self) -> &str {
                "fixture refusal"
            }
            fn parameters_json(&self) -> &str {
                "{}"
            }
            async fn execute(&self, _: serde_json::Value, _: &Path) -> ToolResult {
                ToolResult {
                    success: false,
                    output: String::new(),
                    error_msg: Some(
                        r#"{"code":"existing_refusal","class":"legacy","field":"limit"}"#.into(),
                    ),
                }
            }
        }
        let mut registry = HarnessToolRegistry::new();
        registry.register(Arc::new(RefusingAgentTool));
        let name = "rsi_control_read_session_events";
        let args = serde_json::json!({"session_id":uuid::Uuid::new_v4(), "limit":101});
        let cancel = CancellationToken::new();
        let dir = Path::new(".");
        for result in [
            registry.execute(name, args.clone(), dir).await,
            registry
                .execute_cancellable(name, args.clone(), dir, &cancel)
                .await,
            registry
                .execute_with_context(name, args, dir, &cancel, None)
                .await,
        ] {
            assert!(!result.success);
            let error: serde_json::Value =
                serde_json::from_str(result.error_msg.as_deref().unwrap()).unwrap();
            assert_eq!(error["code"], "existing_refusal");
            assert_eq!(error["class"], "legacy");
            assert_eq!(error["field"], "limit");
            assert_eq!(error["path"], "params.limit");
            assert_eq!(error["constraint"], "maximum 100");
            assert_eq!(error["value"], 101);
        }
    }

    fn policy_registry(
        policy: rsi_common::harness_tool_policy::HarnessToolPolicy,
    ) -> (HarnessToolRegistry, Arc<std::sync::atomic::AtomicUsize>) {
        let runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut registry = HarnessToolRegistry::new();
        for name in ["alpha", "beta"] {
            registry.register(Arc::new(CountingTool {
                name,
                runs: Arc::clone(&runs),
                output: "0123456789",
            }));
        }
        registry.set_policy(Arc::new(policy::ToolPolicyRuntime::new(policy)));
        (registry, runs)
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[tokio::test]
    async fn policy_omits_denied_tools_from_the_catalog_and_refuses_a_forced_call() {
        let (registry, runs) =
            policy_registry(rsi_common::harness_tool_policy::HarnessToolPolicy {
                denied_tools: vec!["beta".into()],
                ..Default::default()
            });
        let names: Vec<_> = registry.specs().into_iter().map(|spec| spec.name).collect();
        assert_eq!(names, ["alpha"]);
        let forced = registry
            .execute("beta", serde_json::json!({}), Path::new("/tmp"))
            .await;
        assert!(!forced.success);
        assert!(
            forced
                .error_msg
                .as_deref()
                .is_some_and(|msg| msg.starts_with("tool_policy_denied:"))
        );
        assert_eq!(runs.load(std::sync::atomic::Ordering::SeqCst), 0);
        let allowed = registry
            .execute("alpha", serde_json::json!({}), Path::new("/tmp"))
            .await;
        assert!(allowed.success);
        assert_eq!(runs.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[tokio::test]
    async fn an_enabled_set_limits_the_catalog_and_execution() {
        let (registry, runs) =
            policy_registry(rsi_common::harness_tool_policy::HarnessToolPolicy {
                enabled_tools: Some(vec!["beta".into()]),
                ..Default::default()
            });
        let names: Vec<_> = registry.specs().into_iter().map(|spec| spec.name).collect();
        assert_eq!(names, ["beta"]);
        let refused = registry
            .execute_with_context(
                "alpha",
                serde_json::json!({}),
                Path::new("/tmp"),
                &CancellationToken::new(),
                None,
            )
            .await;
        assert!(!refused.success);
        assert_eq!(runs.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[tokio::test]
    async fn the_output_byte_budget_returns_a_typed_error_and_later_calls_keep_failing() {
        let (registry, runs) =
            policy_registry(rsi_common::harness_tool_policy::HarnessToolPolicy {
                budgets: rsi_common::harness_tool_policy::ToolBudgets {
                    max_result_bytes: Some(15),
                    ..Default::default()
                },
                ..Default::default()
            });
        for _ in 0..2 {
            assert!(
                registry
                    .execute("alpha", serde_json::json!({}), Path::new("/tmp"))
                    .await
                    .success
            );
        }
        let exhausted = registry
            .execute("alpha", serde_json::json!({}), Path::new("/tmp"))
            .await;
        assert!(
            exhausted
                .error_msg
                .as_deref()
                .is_some_and(|msg| msg.starts_with("tool_budget_exhausted:"))
        );
        assert_eq!(runs.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[test]
    fn test_registry_unknown_tool() {
        let registry = HarnessToolRegistry::new();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(registry.execute(
            "nonexistent",
            serde_json::Value::Null,
            Path::new("/tmp"),
        ));
        assert!(!result.success);
        assert!(result.error_msg.unwrap().contains("Unknown tool"));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[test]
    fn test_default_tools_has_all_builtins() {
        let registry = HarnessToolRegistry::default_tools(
            None, None, None, None, None, None, None, None, None, false,
        );
        let names: Vec<String> = registry.tools.keys().cloned().collect();
        assert!(names.contains(&"read_file".to_string()));
        assert!(names.contains(&"write_file".to_string()));
        assert!(names.contains(&"file_edit".to_string()));
        assert!(names.contains(&"shell".to_string()));
        assert!(names.contains(&"exec_command".to_string()));
        assert!(names.contains(&"write_stdin".to_string()));
        assert!(names.contains(&"git".to_string()));
        assert!(names.contains(&"list_files".to_string()));
        assert!(names.contains(&"grep".to_string()));
        assert!(names.contains(&"glob".to_string()));
        assert!(names.contains(&"view_image".to_string()));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[test]
    fn test_specs_returns_all() {
        let registry = HarnessToolRegistry::default_tools(
            None, None, None, None, None, None, None, None, None, false,
        );
        let names: Vec<_> = registry.specs().into_iter().map(|spec| spec.name).collect();
        assert_eq!(
            names,
            [
                "update_plan",
                "context_status",
                "compact",
                "read_file",
                "write_file",
                "file_edit",
                "apply_patch",
                "shell",
                "exec_command",
                "write_stdin",
                "git",
                "list_files",
                "grep",
                "glob",
                "read_output",
                "view_image",
                "web_fetch"
            ]
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[test]
    fn registry_specs_are_stable_and_duplicate_registration_fails() {
        let mut first = HarnessToolRegistry::default_tools(
            None, None, None, None, None, None, None, None, None, false,
        );
        let second = HarnessToolRegistry::default_tools(
            None, None, None, None, None, None, None, None, None, false,
        );
        let before = serde_json::to_vec(&first.specs()).unwrap();
        assert_eq!(before, serde_json::to_vec(&second.specs()).unwrap());
        assert!(first.try_register(Arc::new(file::ReadFileTool)).is_err());
        assert_eq!(before, serde_json::to_vec(&first.specs()).unwrap());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    struct OutputTool(Arc<std::sync::Mutex<String>>);

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[async_trait::async_trait]
    impl HarnessTool for OutputTool {
        fn name(&self) -> &str {
            "output_probe"
        }

        fn description(&self) -> &str {
            "Return test output"
        }

        fn parameters_json(&self) -> &str {
            r#"{"type":"object"}"#
        }

        async fn execute(&self, _args: serde_json::Value, _working_dir: &Path) -> ToolResult {
            let output = self.0.lock().unwrap().clone();
            ToolResult {
                success: true,
                output,
                error_msg: None,
            }
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[tokio::test]
    async fn registry_caps_oversized_text_at_a_line_boundary() {
        let line = format!("{}\n", "x".repeat(512));
        let output = line.repeat(3000);
        let mut registry = HarnessToolRegistry::new();
        // With the spill store off the contract falls back to truncation.
        let mut off = (*registry.spill_config()).clone();
        off.disabled = true;
        registry.set_spill_config(off);
        registry.register(Arc::new(OutputTool(Arc::new(std::sync::Mutex::new(
            output,
        )))));

        let result = registry
            .execute_with_context(
                "output_probe",
                json!({}),
                Path::new("/tmp"),
                &CancellationToken::new(),
                None,
            )
            .await;

        assert!(result.success);
        assert!(result.output.len() <= ToolPolicy::default().max_output_bytes);
        let marker_at = result
            .output
            .find("[truncated: limit 1048576 bytes; 1539000 bytes total]")
            .expect("truncation marker");
        assert!(result.output[..marker_at].ends_with('\n'));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[tokio::test]
    async fn registry_preserves_within_limit_and_already_truncated_text() {
        let output = Arc::new(std::sync::Mutex::new("within\nlimit\n".to_string()));
        let mut registry = HarnessToolRegistry::new();
        registry.register(Arc::new(OutputTool(Arc::clone(&output))));
        let cancel = CancellationToken::new();

        let within_limit = registry
            .execute_with_context("output_probe", json!({}), Path::new("/tmp"), &cancel, None)
            .await;
        assert_eq!(within_limit.output, "within\nlimit\n");

        let already_truncated = truncation::truncate_text(&"aaaa\n".repeat(30), 100, false).content;
        *output.lock().unwrap() = already_truncated.clone();
        let bounded = registry
            .execute_with_context("output_probe", json!({}), Path::new("/tmp"), &cancel, None)
            .await;
        assert_eq!(bounded.output, already_truncated);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[tokio::test]
    async fn registry_preserves_typed_content_blocks_without_text_truncation() {
        use crate::session::harness::types::ToolContentBlock;

        let original = ToolResult::from_blocks(
            vec![ToolContentBlock::Image {
                media_type: "image/png".into(),
                data: "x".repeat(1_100_000),
                detail: None,
                reference: None,
                width: None,
                height: None,
            }],
            false,
        )
        .output;
        let mut registry = HarnessToolRegistry::new();
        registry.register(Arc::new(OutputTool(Arc::new(std::sync::Mutex::new(
            original.clone(),
        )))));

        let result = registry
            .execute_with_context(
                "output_probe",
                json!({}),
                Path::new("/tmp"),
                &CancellationToken::new(),
                None,
            )
            .await;

        assert_eq!(result.output, original);
        assert!(result.has_typed_blocks());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    struct ProgressTool;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[async_trait::async_trait]
    impl HarnessTool for ProgressTool {
        fn name(&self) -> &str {
            "progress_probe"
        }

        fn description(&self) -> &str {
            "Report progress until cancelled"
        }

        fn parameters_json(&self) -> &str {
            r#"{"type":"object"}"#
        }

        async fn execute(&self, _args: serde_json::Value, _working_dir: &Path) -> ToolResult {
            unreachable!("context execution must be used")
        }

        async fn execute_with_context(
            &self,
            _args: serde_json::Value,
            context: &ToolContext,
        ) -> ToolResult {
            context.report_progress(self.name(), "started").await;
            context.cancel.cancelled().await;
            ToolResult {
                success: false,
                output: String::new(),
                error_msg: Some("cancelled".into()),
            }
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[tokio::test]
    async fn tool_context_reports_progress_and_observes_midrun_cancel() {
        let mut registry = HarnessToolRegistry::new();
        registry.session_id = Some(uuid::Uuid::nil());
        registry.register(Arc::new(ProgressTool));
        let (tx, mut rx) = mpsc::channel(2);
        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let task = tokio::spawn(async move {
            registry
                .execute_with_context(
                    "progress_probe",
                    json!({}),
                    Path::new("/tmp"),
                    &task_cancel,
                    Some(tx),
                )
                .await
        });
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.event_type, "tool_progress");
        assert_eq!(event.data["session_id"], uuid::Uuid::nil().to_string());
        assert_eq!(event.data["message"], "started");
        cancel.cancel();
        assert!(task.await.unwrap().is_error());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[test]
    fn execution_modes_default_to_sequential_except_read_only_tools() {
        let registry = HarnessToolRegistry::default_tools(
            None, None, None, None, None, None, None, None, None, false,
        );
        assert_eq!(
            registry.execution_mode("read_file"),
            Some(ToolExecutionMode::ParallelSafe)
        );
        assert_eq!(
            registry.execution_mode("list_files"),
            Some(ToolExecutionMode::ParallelSafe)
        );
        for navigation_tool in ["grep", "glob"] {
            assert_eq!(
                registry.execution_mode(navigation_tool),
                Some(ToolExecutionMode::ParallelSafe),
                "{navigation_tool}"
            );
        }
        assert_eq!(
            registry.execution_mode("view_image"),
            Some(ToolExecutionMode::ParallelSafe)
        );
        assert_eq!(
            registry.execution_mode("web_fetch"),
            Some(ToolExecutionMode::ParallelSafe)
        );
        assert_eq!(
            registry.execution_mode("context_status"),
            Some(ToolExecutionMode::ParallelSafe)
        );
        assert_eq!(
            registry.execution_mode("update_plan"),
            Some(ToolExecutionMode::Sequential)
        );
        assert_eq!(
            registry.execution_mode("compact"),
            Some(ToolExecutionMode::Sequential)
        );
        assert_eq!(
            registry.execution_mode("shell"),
            Some(ToolExecutionMode::Sequential)
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[test]
    fn test_register_and_specs() {
        let mut registry = HarnessToolRegistry::new();
        assert_eq!(registry.specs().len(), 0);
        registry.register(Arc::new(file::ReadFileTool));
        assert_eq!(registry.specs().len(), 1);
        let spec = &registry.specs()[0];
        assert_eq!(spec.name, "read_file");
    }

    fn test_control_handle() -> AgentControlHandle {
        use crate::session::spawn_coordinator::SpawnCoordinator;
        use std::collections::HashMap;
        let active = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
        let completed = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            crate::store::Store::open_in_memory().unwrap(),
        ));
        let (tx, _rx) = tokio::sync::mpsc::channel(4);
        let coordinator = Arc::new(SpawnCoordinator::new(tx));
        AgentControlHandle::new(
            active,
            completed,
            store,
            Arc::new(crate::bus::EventBus::new(16)),
            coordinator,
        )
    }

    /// Rotation-parity ratchet: BOTH the fresh-launch and rotation Harness
    /// paths now build their registry through this single `default_tools`
    /// function (see `session/launch.rs`, `session/lifecycle.rs`, and the two
    /// `session::harness::HarnessClient::launch` call sites in
    /// `session/rotation.rs`). A registry built with a store + control handle +
    /// bound caller therefore carries the full identical tool set — including
    /// `schedule_wake` and the native `rsi_control` tools the legacy rotation
    /// registry used to omit. Pin the exact non-memory roster so any future
    /// tool added to one path only (re-introducing split-brain) trips here.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[test]
    fn rotation_and_fresh_share_identical_tool_set() {
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            crate::store::Store::open_in_memory().unwrap(),
        ));
        let caller = uuid::Uuid::new_v4();
        let control = test_control_handle();

        // Simulate both the fresh and rotation call sites: identical args in →
        // identical registry out, because both go through this one function.
        let build = || {
            HarnessToolRegistry::default_tools(
                None,
                None,
                Some(Arc::clone(&store)),
                Some(caller),
                Some(uuid::Uuid::new_v4()),
                Some(std::path::PathBuf::from("/tmp")),
                None,
                None,
                Some(control.clone()),
                false,
            )
        };
        let fresh: std::collections::BTreeSet<String> = build().tools.keys().cloned().collect();
        let rotation: std::collections::BTreeSet<String> = build().tools.keys().cloned().collect();
        assert_eq!(fresh, rotation, "fresh and rotation tool sets must match");

        // Non-agent-control tools are pinned by hand; every agent-control
        // tool is derived from the descriptor table (#1116) so adding a verb
        // with a native tool can never leave this roster stale. The argument-
        // free program-guard convenience is the one native-only extra.
        let mut expected: std::collections::BTreeSet<String> = [
            "update_plan",
            "context_status",
            "compact",
            "read_file",
            "write_file",
            "file_edit",
            "apply_patch",
            "shell",
            "exec_command",
            "write_stdin",
            "git",
            "list_files",
            "grep",
            "glob",
            "read_output",
            "view_image",
            "web_fetch",
            "rsi_control_program_guard",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        expected.extend(
            rsi_common::agent_control_schema::agent_control_catalog_v1()
                .iter()
                .filter_map(|descriptor| descriptor.native_tool)
                // #3774: gateway-only tools are not in this in-process registry.
                .filter(|native| !native.mcp_gateway_only())
                .map(|native| native.name().to_string()),
        );
        assert_eq!(fresh, expected, "unified Harness tool roster drifted");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[test]
    fn agent_bound_native_tool_schemas_match_the_common_catalog() {
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            crate::store::Store::open_in_memory().unwrap(),
        ));
        let caller = uuid::Uuid::new_v4();
        let registry = HarnessToolRegistry::default_tools(
            None,
            None,
            Some(Arc::clone(&store)),
            Some(caller),
            Some(uuid::Uuid::new_v4()),
            Some(std::path::PathBuf::from("/tmp")),
            None,
            None,
            Some(test_control_handle()),
            false,
        );
        let specs = registry.specs();
        let catalog = rsi_common::agent_control_schema::agent_control_catalog_v1();
        let rpc_only = catalog
            .iter()
            .filter(|descriptor| descriptor.native_tool.is_none())
            .map(|descriptor| descriptor.method)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            rpc_only,
            std::collections::BTreeSet::from([
                "AgentArchiveChild",
                "AgentReportToHub",
                "AgentPushSatelliteManagerPolicy",
                "AgentReseatSatelliteManager",
                "AgentSwitchHostPreset",
                "AgentGlobalAppointManager",
                "AgentManagerAppointChild",
                "AgentManagerRevokeChild",
            ]),
            "the native Harness RPC-only verb set changed without review"
        );
        for descriptor in catalog {
            let Some(native_tool) = descriptor.native_tool else {
                continue;
            };
            let native_name = native_tool.name();
            // #3774: served by the MCP gateway only, not by this registry.
            if native_tool.mcp_gateway_only() {
                assert!(specs.iter().all(|spec| spec.name != native_name));
                continue;
            }
            let spec = specs
                .iter()
                .find(|spec| spec.name == native_name)
                .unwrap_or_else(|| panic!("missing native tool {native_name}"));
            let parameters: serde_json::Value =
                serde_json::from_str(&spec.parameters_json).unwrap();
            assert_eq!(parameters, descriptor.parameters(), "tool: {native_name}");
        }

        assert!(catalog.iter().all(|descriptor| {
            descriptor
                .native_tool
                .is_none_or(|native| native.name() != "rsi_control_program_guard")
        }));
        let program_guard = specs
            .iter()
            .find(|spec| spec.name == "rsi_control_program_guard")
            .expect("native program-guard convenience");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&program_guard.parameters_json).unwrap(),
            serde_json::json!({"type": "object", "properties": {}, "additionalProperties": false})
        );

        let generic = HarnessToolRegistry::default_tools(
            None,
            None,
            Some(store),
            Some(caller),
            Some(uuid::Uuid::new_v4()),
            Some(std::path::PathBuf::from("/tmp")),
            None,
            None,
            None,
            false,
        );
        let generic_wake = generic
            .specs()
            .into_iter()
            .find(|spec| spec.name == "schedule_wake")
            .expect("generic schedule_wake");
        let generic_parameters: serde_json::Value =
            serde_json::from_str(&generic_wake.parameters_json).unwrap();
        assert_ne!(
            generic_parameters,
            rsi_common::agent_control_schema::AgentControlVerbV1::ScheduleWake
                .descriptor()
                .parameters()
        );
        assert_eq!(
            generic_parameters["properties"]["mode"]["enum"],
            serde_json::json!(["fresh", "resume"])
        );
    }

    /// The native `rsi_control` tools are gated on BOTH a control handle and a
    /// bound caller session id — without a caller there is nothing to bind, so
    /// they must not register (defense against an unbound/spoofable caller).
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[test]
    fn rsi_control_tools_require_a_bound_caller() {
        let control = test_control_handle();
        // Control handle present but no caller id → not registered.
        let registry = HarnessToolRegistry::default_tools(
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(control),
            false,
        );
        let names: Vec<&String> = registry.tools.keys().collect();
        assert!(!names.iter().any(|n| n.starts_with("rsi_control")));
    }

    fn spill_registry(root: &Path) -> HarnessToolRegistry {
        let mut registry = HarnessToolRegistry::new();
        registry.set_spill_config(rsi_common::spill::SpillConfig {
            root: root.to_path_buf(),
            max_bytes: 8 * 1024,
            max_lines: 200,
            disabled: false,
        });
        registry.session_id =
            Some(uuid::Uuid::parse_str("1a2b3c4d-0000-4000-8000-000000000000").unwrap());
        registry
    }

    async fn run(
        registry: &HarnessToolRegistry,
        name: &str,
        args: serde_json::Value,
    ) -> ToolResult {
        registry
            .execute_with_context(
                name,
                args,
                Path::new("/tmp"),
                &CancellationToken::new(),
                None,
            )
            .await
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    struct NamedOutput(&'static str, String);

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[async_trait::async_trait]
    impl HarnessTool for NamedOutput {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "Return fixed output"
        }
        fn parameters_json(&self) -> &str {
            r#"{"type":"object"}"#
        }
        async fn execute(&self, _args: serde_json::Value, _working_dir: &Path) -> ToolResult {
            ToolResult {
                success: true,
                output: self.1.clone(),
                error_msg: None,
            }
        }
    }

    /// #3173: a preset's `output_spill` threshold is respected for the command
    /// tool it names, and only for that tool; the default stays 8 KiB / 200 lines.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[tokio::test]
    async fn a_policy_spill_threshold_is_respected_for_its_command_tool_only() {
        use rsi_common::harness_tool_policy::{HarnessToolPolicy, OutputSpill};
        // 150 lines, about 5 KB: under the 8 KiB / 200 line default, and big
        // enough that a stub is a real saving (a smaller one stays inline).
        let mid: String = (0..150)
            .map(|n| format!("a reasonably long line number {n}\n"))
            .collect();
        let build = |dir: &Path, spill: Option<OutputSpill>| {
            let mut registry = spill_registry(dir);
            registry.register(Arc::new(NamedOutput("shell", mid.clone())));
            registry.register(Arc::new(NamedOutput("exec_command", mid.clone())));
            if let Some(spill) = spill {
                let mut policy = HarnessToolPolicy::default();
                policy.output_spill.insert("shell".into(), spill);
                assert_eq!(policy.validate(), Ok(()));
                registry.set_policy(Arc::new(policy::ToolPolicyRuntime::new(policy)));
            }
            registry
        };

        // Default: unchanged, the output is returned whole.
        let dir = tempfile::tempdir().unwrap();
        let registry = build(dir.path(), None);
        assert_eq!(run(&registry, "shell", json!({})).await.output, mid);

        // A line threshold below the output spills `shell`, not `exec_command`.
        let dir = tempfile::tempdir().unwrap();
        let registry = build(
            dir.path(),
            Some(OutputSpill {
                max_bytes: None,
                max_lines: Some(100),
            }),
        );
        let shell = run(&registry, "shell", json!({})).await;
        assert!(
            shell.output.starts_with(rsi_common::spill::STUB_PREFIX),
            "{}",
            shell.output
        );
        assert_eq!(run(&registry, "exec_command", json!({})).await.output, mid);

        // A byte threshold below the output spills it too.
        let dir = tempfile::tempdir().unwrap();
        let registry = build(
            dir.path(),
            Some(OutputSpill {
                max_bytes: Some(4096),
                max_lines: None,
            }),
        );
        assert!(
            run(&registry, "shell", json!({}))
                .await
                .output
                .starts_with(rsi_common::spill::STUB_PREFIX)
        );
        assert_eq!(run(&registry, "exec_command", json!({})).await.output, mid);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[tokio::test]
    async fn large_tool_output_is_spilled_and_read_output_returns_the_exact_lines() {
        let dir = tempfile::tempdir().unwrap();
        let mut registry = spill_registry(dir.path());
        let mut output = String::new();
        for n in 0..5_000 {
            output.push_str(&format!("test mod::t{n} ... ok\n"));
        }
        output.push_str("test mod::broken ... FAILED\n");
        registry.register(Arc::new(OutputTool(Arc::new(std::sync::Mutex::new(
            output.clone(),
        )))));
        let handle_tool = read_output::ReadOutputTool::new(registry.spill_config());
        registry.register(Arc::new(handle_tool));

        let stub = run(&registry, "output_probe", json!({})).await;
        assert!(stub.success);
        assert!(stub.output.len() < 2048);
        assert!(
            stub.output
                .starts_with("[rsi-spill 1a2b3c4d/1] output_probe")
        );
        assert!(stub.output.contains("mod::broken ... FAILED"));

        let grep = run(
            &registry,
            "read_output",
            json!({"handle": "1a2b3c4d/1", "grep": "broken"}),
        )
        .await;
        assert!(grep.success, "{:?}", grep.error_msg);
        assert_eq!(grep.output, "5001:test mod::broken ... FAILED\n");

        let range = run(
            &registry,
            "read_output",
            json!({"handle": "1a2b3c4d/1", "range": "1:2"}),
        )
        .await;
        assert_eq!(range.output, "test mod::t0 ... ok\ntest mod::t1 ... ok\n");

        // The retrieval result is never re-spilled into another stub.
        let all = run(&registry, "read_output", json!({"handle": "1a2b3c4d/1"})).await;
        assert!(all.output.starts_with("test mod::t0 ... ok"));
        assert!(all.output.contains("read_output: showing"));
        assert!(!all.output.contains("[rsi-spill"));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[tokio::test]
    async fn read_output_rejects_bad_handles_and_ranges_and_reports_no_match() {
        let dir = tempfile::tempdir().unwrap();
        let mut registry = spill_registry(dir.path());
        registry.register(Arc::new(read_output::ReadOutputTool::new(
            registry.spill_config(),
        )));
        let missing = run(&registry, "read_output", json!({})).await;
        assert!(!missing.success);
        let traversal = run(
            &registry,
            "read_output",
            json!({"handle": "../../etc/passwd"}),
        )
        .await;
        assert!(!traversal.success);
        let unknown = run(&registry, "read_output", json!({"handle": "nope/9"})).await;
        assert!(!unknown.success);

        rsi_common::spill::store(
            dir.path(),
            "zz",
            &rsi_common::spill::SpillMeta::default(),
            b"alpha\nbeta\n",
        )
        .unwrap();
        let bad_range = run(
            &registry,
            "read_output",
            json!({"handle": "zz/1", "range": "x:y"}),
        )
        .await;
        assert!(!bad_range.success);
        let none = run(
            &registry,
            "read_output",
            json!({"handle": "zz/1", "grep": "gamma"}),
        )
        .await;
        assert!(none.success);
        assert_eq!(none.output, "(no lines matched)");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[tokio::test]
    async fn a_failing_command_keeps_its_exit_code_in_the_stub() {
        struct Failing;
        #[async_trait::async_trait]
        impl HarnessTool for Failing {
            fn name(&self) -> &str {
                "shell"
            }
            fn description(&self) -> &str {
                "fails loudly"
            }
            fn parameters_json(&self) -> &str {
                "{}"
            }
            async fn execute(&self, _args: serde_json::Value, _dir: &Path) -> ToolResult {
                ToolResult {
                    success: false,
                    output: "error[E0308]: mismatched types\n".to_string()
                        + &"noise\n".repeat(3000),
                    error_msg: Some("Exit code: 101".into()),
                }
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let mut registry = spill_registry(dir.path());
        registry.register(Arc::new(Failing));
        let result = run(&registry, "shell", json!({})).await;
        assert!(!result.success);
        assert_eq!(result.error_msg.as_deref(), Some("Exit code: 101"));
        assert!(result.output.contains("exit=101"));
        assert!(result.output.contains("error[E0308]"));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[test]
    fn context_editing_defaults_on_and_follows_the_session_policy() {
        assert!(HarnessToolRegistry::new().context_editing_enabled());
        let (on, _) =
            policy_registry(rsi_common::harness_tool_policy::HarnessToolPolicy::default());
        assert!(on.context_editing_enabled());
        let (off, _) = policy_registry(rsi_common::harness_tool_policy::HarnessToolPolicy {
            context_editing: Some(false),
            ..Default::default()
        });
        assert!(!off.context_editing_enabled());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[test]
    fn the_default_catalog_offers_read_output() {
        let registry = HarnessToolRegistry::default_tools(
            None, None, None, None, None, None, None, None, None, false,
        );
        assert!(
            registry
                .specs()
                .iter()
                .any(|spec| spec.name == "read_output")
        );
    }
}

/// Tool-call recovery (#2257): argument validation and unknown-tool guidance
/// are the registry's, so every execution mode answers alike.
#[cfg(test)]
mod recovery_tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct SchemaTool {
        name: &'static str,
        schema: &'static str,
        runs: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl HarnessTool for SchemaTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "fixture"
        }
        fn parameters_json(&self) -> &str {
            self.schema
        }
        async fn execute(&self, _args: serde_json::Value, _working_dir: &Path) -> ToolResult {
            self.runs.fetch_add(1, Ordering::SeqCst);
            ToolResult {
                success: true,
                output: "ran".to_string(),
                error_msg: None,
            }
        }
    }

    const READ_SCHEMA: &str = r#"{"type":"object","required":["path"],"properties":{"path":{"type":"string"},"offset":{"type":"integer"}}}"#;

    fn registry_with(names: &[&'static str]) -> (HarnessToolRegistry, Arc<AtomicUsize>) {
        let runs = Arc::new(AtomicUsize::new(0));
        let mut registry = HarnessToolRegistry::new();
        for name in names {
            registry.register(Arc::new(SchemaTool {
                name,
                schema: READ_SCHEMA,
                runs: Arc::clone(&runs),
            }));
        }
        (registry, runs)
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[tokio::test]
    async fn invalid_arguments_refuse_the_call_in_every_execution_mode() {
        let (registry, runs) = registry_with(&["read_file"]);
        let dir = Path::new("/tmp");
        let cancel = CancellationToken::new();
        let args = json!({"offset": "later"});

        let results = [
            registry.execute("read_file", args.clone(), dir).await,
            registry
                .execute_cancellable("read_file", args.clone(), dir, &cancel)
                .await,
            registry
                .execute_with_context("read_file", args.clone(), dir, &cancel, None)
                .await,
        ];
        assert_eq!(runs.load(Ordering::SeqCst), 0, "the tool must not run");
        let first = results[0].error_msg.clone().expect("a validation error");
        for result in &results {
            assert!(!result.success);
            assert_eq!(result.error_msg.as_deref(), Some(first.as_str()));
        }
        assert!(
            first.contains("missing required field `path` (expected string)"),
            "{first}"
        );
        assert!(
            first.contains("`offset`: expected integer, got string \"later\""),
            "{first}"
        );

        // The corrected call runs.
        let fixed = registry
            .execute("read_file", json!({"path": "a.rs", "offset": 3}), dir)
            .await;
        assert!(fixed.success, "{fixed:?}");
        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[tokio::test]
    async fn unknown_tools_get_suggestions_and_the_callable_names() {
        let (registry, _) = registry_with(&["read_file", "file_edit", "glob", "grep", "shell"]);
        let message = registry
            .execute("read", json!({}), Path::new("/tmp"))
            .await
            .error_msg
            .expect("an unknown-tool error");
        assert!(
            message.starts_with("Unknown tool: read. Did you mean: read_file?"),
            "{message}"
        );
        assert!(
            message.contains("Available tools: read_file, file_edit, glob, grep, shell."),
            "{message}"
        );

        // Within three edits, ignoring case and `-` versus `_`.
        let typo = registry.unknown_tool_message("Read-Fil");
        assert!(typo.contains("Did you mean: read_file?"), "{typo}");
        let namespaced = registry.unknown_tool_message("functions.grep");
        assert!(namespaced.contains("Did you mean: grep?"), "{namespaced}");

        // Nothing close: no suggestion, still the names.
        let far = registry.unknown_tool_message("deploy_to_production");
        assert!(!far.contains("Did you mean"), "{far}");
        assert!(far.contains("Available tools:"), "{far}");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[test]
    fn unknown_tool_names_are_capped_and_an_empty_name_is_named() {
        let names: Vec<&'static str> = (0..25)
            .map(|index| &*Box::leak(format!("tool_{index:02}").into_boxed_str()))
            .collect();
        let (registry, _) = registry_with(&names);
        let message = registry.unknown_tool_message("");
        assert!(
            message.starts_with("The tool call has an empty name."),
            "{message}"
        );
        assert!(message.contains("tool_19"), "{message}");
        assert!(!message.contains("tool_20"), "{message}");
        assert!(message.contains("(and 5 more)"), "{message}");
        assert!(!message.contains("Did you mean"), "{message}");
    }
}
