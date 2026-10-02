//! Trait-based tool system for the agent harness.
//!
//! Each tool is a struct implementing `HarnessTool`. Built-in tools provide
//! file I/O, shell execution, and git operations. The registry collects tools
//! and dispatches execution by name.

pub mod apply_patch;
pub mod arguments;
pub mod codegraph;
mod completion_gate;
pub mod exec;
pub mod file;
pub mod git;
pub mod list_files;
pub mod memory;
pub mod policy;
pub(crate) mod process_registry;
pub mod rsi_control;
pub mod schedule_wake;
pub mod shell;
pub(crate) mod truncation;
pub mod utility;
pub mod view_image;

use crate::claude::StreamEvent;
use crate::session::agent_verbs::AgentControlHandle;
use crate::session::harness::mcp::StdioMcpClient;
use crate::session::harness::types::{HarnessToolSpec, HarnessToolSpecKind, ToolResult};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Whether a tool may execute alongside other calls from the same model turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolExecutionMode {
    ParallelSafe,
    Sequential,
}

/// Shared limits passed to tools. Later session policy can narrow these limits.
#[derive(Debug, Clone, Copy)]
pub struct ToolPolicy {
    pub max_output_bytes: usize,
    /// Network egress policy (#774). A network-capable tool reads it from its
    /// `ToolContext` and never from an ambient global; the default is the
    /// fail-closed `deny_private` mode.
    pub egress: rsi_common::egress_policy::EgressPolicy,
}

impl Default for ToolPolicy {
    fn default() -> Self {
        Self {
            max_output_bytes: 1024 * 1024,
            egress: rsi_common::egress_policy::EgressPolicy::default(),
        }
    }
}

/// Construction-bound context for one tool invocation.
#[derive(Clone)]
pub struct ToolContext {
    pub session_id: Option<uuid::Uuid>,
    pub working_dir: PathBuf,
    pub cancel: CancellationToken,
    pub event_sink: Option<mpsc::Sender<StreamEvent>>,
    pub policy: ToolPolicy,
}

impl ToolContext {
    pub async fn report_progress(&self, tool_name: &str, message: &str) {
        if let Some(sink) = &self.event_sink {
            let _ = sink
                .send(StreamEvent {
                    event_type: "tool_progress".into(),
                    data: json!({
                        "session_id": self.session_id,
                        "name": tool_name,
                        "message": message,
                    }),
                })
                .await;
        }
    }
}

/// Trait for tools executable by the agent harness.
#[async_trait::async_trait]
pub trait HarnessTool: Send + Sync {
    /// Tool name (must be unique within a registry).
    fn name(&self) -> &str;

    /// Human-readable description for the LLM.
    fn description(&self) -> &str;

    /// JSON Schema string for input parameters.
    fn parameters_json(&self) -> &str;

    /// Calls are sequential unless the tool explicitly declares itself safe.
    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Sequential
    }

    /// Execute the tool with the given arguments.
    async fn execute(&self, args: serde_json::Value, working_dir: &Path) -> ToolResult;

    /// Execute while honoring harness cancellation. Process-backed tools
    /// override this so cancellation waits for child cleanup and reap; other
    /// tools retain their existing future-drop behavior.
    async fn execute_cancellable(
        &self,
        args: serde_json::Value,
        working_dir: &Path,
        cancel: &CancellationToken,
    ) -> ToolResult {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => ToolResult {
                success: false,
                output: String::new(),
                error_msg: Some("Tool execution cancelled".to_string()),
            },
            result = self.execute(args, working_dir) => result,
        }
    }

    /// New tools receive progress, cancellation, policy and session context.
    /// Existing tools keep their cancellable implementation during migration.
    async fn execute_with_context(
        &self,
        args: serde_json::Value,
        context: &ToolContext,
    ) -> ToolResult {
        self.execute_cancellable(args, &context.working_dir, &context.cancel)
            .await
    }

    /// Convert to a HarnessToolSpec for provider API calls.
    fn to_spec(&self) -> HarnessToolSpec {
        HarnessToolSpec {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters_json: self.parameters_json().to_string(),
            freeform: None,
            kind: HarnessToolSpecKind::Function,
        }
    }
}

/// Registry of available tools for the agent loop.
pub struct HarnessToolRegistry {
    tools: HashMap<String, Arc<dyn HarnessTool>>,
    order: Vec<String>,
    session_id: Option<uuid::Uuid>,
    process_registry: Option<Arc<exec::ProcessRegistry>>,
    mcp_clients: Vec<Arc<tokio::sync::Mutex<StdioMcpClient>>>,
    utility_state: Arc<utility::UtilityToolState>,
    /// #792: per-session tool policy. `None` is the unrestricted default.
    policy: Option<Arc<policy::ToolPolicyRuntime>>,
    launch_invocation_id: Option<uuid::Uuid>,
    execution_scratch: Option<crate::sandbox::execution_scratch::SandboxExecutionScratch>,
    deferred_mcp: Arc<RwLock<DeferredMcpCatalog>>,
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
            process_registry: None,
            mcp_clients: Vec::new(),
            utility_state: Arc::new(utility::UtilityToolState::default()),
            policy: None,
            launch_invocation_id: None,
            execution_scratch: None,
            deferred_mcp: Arc::new(RwLock::new(DeferredMcpCatalog::default())),
        }
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

    #[must_use]
    pub fn policy(&self) -> Option<&Arc<policy::ToolPolicyRuntime>> {
        self.policy.as_ref()
    }

    /// The policy gate shared by every execution entry point: a denial or an
    /// exhausted budget settles as a tool-error result without running the tool.
    fn policy_refusal(&self, name: &str) -> Option<ToolResult> {
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
                ..ToolPolicy::default()
            },
        };
        if let Some(refusal) = self.policy_refusal(name) {
            return refusal;
        }
        if let Some(refusal) = self.deferred_mcp_error(name) {
            return refusal;
        }
        let result = match self.tools.get(name) {
            Some(tool) => cap_tool_result(
                tool.execute_with_context(args, &context).await,
                context.policy.max_output_bytes,
            ),
            None => ToolResult {
                success: false,
                output: String::new(),
                error_msg: Some(format!("Unknown tool: {name}")),
            },
        };
        self.record_policy_bytes(&result);
        result
    }

    pub(crate) fn set_context_usage(&self, used_tokens: u64, token_limit: u64) {
        self.utility_state
            .set_context_usage(used_tokens, token_limit);
    }

    pub(crate) async fn take_compaction_focus(&self) -> Option<String> {
        self.utility_state.take_compaction_focus().await
    }

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
        let result = match self.tools.get(name) {
            Some(tool) => cap_tool_result(
                tool.execute(args, working_dir).await,
                ToolPolicy::default().max_output_bytes,
            ),
            None => ToolResult {
                success: false,
                output: String::new(),
                error_msg: Some(format!("Unknown tool: {name}")),
            },
        };
        self.record_policy_bytes(&result);
        result
    }

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
        let result = match self.tools.get(name) {
            Some(tool) => cap_tool_result(
                tool.execute_cancellable(args, working_dir, cancel).await,
                ToolPolicy::default().max_output_bytes,
            ),
            None => ToolResult {
                success: false,
                output: String::new(),
                error_msg: Some(format!("Unknown tool: {name}")),
            },
        };
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
    #[allow(clippy::too_many_arguments)]
    pub fn default_tools(
        memory_handle: Option<&crate::memory::worker::MemoryHandle>,
        project_id: Option<uuid::Uuid>,
        store: Option<Arc<tokio::sync::Mutex<crate::store::Store>>>,
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

    #[allow(clippy::too_many_arguments)]
    pub fn default_tools_with_execution_scratch(
        memory_handle: Option<&crate::memory::worker::MemoryHandle>,
        project_id: Option<uuid::Uuid>,
        store: Option<Arc<tokio::sync::Mutex<crate::store::Store>>>,
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
        store: Option<Arc<tokio::sync::Mutex<crate::store::Store>>>,
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
        registry.register(Arc::new(utility::CompactTool::new(utility_state)));
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
        registry.register(Arc::new(view_image::ViewImageTool::new(
            image_input_supported,
        )));
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

impl Default for HarnessToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

fn cap_tool_result(mut result: ToolResult, max_output_bytes: usize) -> ToolResult {
    if !result.has_typed_blocks() && result.output.len() > max_output_bytes {
        result.output = truncation::truncate_text(&result.output, max_output_bytes, false).content;
    }
    result
}

/// System blocklist -- these paths are never writable even with wildcard allowed_paths.
const SYSTEM_BLOCKLIST: &[&str] = &[
    "/bin",
    "/sbin",
    "/usr/bin",
    "/usr/sbin",
    "/usr/lib",
    "/etc",
    "/dev",
    "/proc",
    "/sys",
    "/boot",
];

/// Check if a resolved path falls within the system blocklist.
pub fn is_system_blocked(path: &Path) -> bool {
    let path_str = path.to_string_lossy();
    SYSTEM_BLOCKLIST
        .iter()
        .any(|blocked| path_str.starts_with(blocked))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[test]
    fn test_is_system_blocked() {
        assert!(is_system_blocked(Path::new("/etc/passwd")));
        assert!(is_system_blocked(Path::new("/bin/bash")));
        assert!(is_system_blocked(Path::new("/usr/bin/ls")));
        assert!(is_system_blocked(Path::new("/proc/self/maps")));
        assert!(!is_system_blocked(Path::new("/home/user/project/file.rs")));
        assert!(!is_system_blocked(Path::new("/tmp/workdir/file.txt")));
    }

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
                "view_image"
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
        assert_eq!(
            registry.execution_mode("view_image"),
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
        let store = Arc::new(tokio::sync::Mutex::new(
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
        let store = Arc::new(tokio::sync::Mutex::new(
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

        let expected: std::collections::BTreeSet<String> = [
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
            "view_image",
            "schedule_wake",
            "rsi_control_spawn",
            "rsi_control_reserve_successor",
            "rsi_control_progress",
            "rsi_control_send_message",
            "rsi_control_status",
            "rsi_control_halt",
            "rsi_control_program_guard",
            "rsi_control_create_issue",
            "rsi_control_list_issues",
            "rsi_control_get_issue",
            "rsi_control_update_issue",
            "rsi_control_update_issue_status",
            "rsi_control_archive_issue",
            "rsi_control_restore_issue",
            "rsi_control_list_issue_events",
            "rsi_control_authority_catalog",
            "rsi_control_manager_progress",
            "rsi_control_manager_inbox",
            "rsi_control_manager_send",
            "rsi_control_manager_reply",
            "rsi_control_manager_notify",
            "rsi_control_manager_inspect",
            "rsi_control_manager_update",
            "rsi_control_submit_review_receipt",
            "rsi_control_manager_control",
            "rsi_control_manager_prepare_control",
            "rsi_control_manager_commit_prepared_control",
            "rsi_control_manager_get_action",
            "rsi_control_manager_work_view",
            "rsi_control_topology_upsert",
            "rsi_control_topology_list",
            "rsi_control_topology_execute",
            "rsi_control_topology_get_execution",
            "rsi_control_topology_interrupt",
            "rsi_control_topology_resolve_attempt",
            "rsi_control_read_session_events",
            "rsi_control_query_failure_signatures",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(fresh, expected, "unified Harness tool roster drifted");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[test]
    fn agent_bound_native_tool_schemas_match_the_common_catalog() {
        let store = Arc::new(tokio::sync::Mutex::new(
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
                "AgentContinueChild",
                "AgentArchiveChild",
                "AgentManagerDelegateNode",
                "AgentManagerEscalate",
                "AgentManagerListEscalations",
                "AgentManagerResolveEscalation",
                "AgentEnqueueLandingSource",
                "AgentGetProviderStatus",
                "AgentSubmitJob",
                "AgentGetJob",
                "AgentListJobs",
                "AgentSendSatelliteMessage",
                "AgentGetDaemonInfo",
                "AgentRequestDeploy",
                // #1028 and #1042 landed both wake verbs as RPC-only by design.
                "AgentCancelWake",
                "AgentListWakes",
            ]),
            "the native Harness RPC-only verb set changed without review"
        );
        for descriptor in catalog {
            let Some(native_tool) = descriptor.native_tool else {
                continue;
            };
            let native_name = native_tool.name();
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
}
