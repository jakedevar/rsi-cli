//! Trait-based tool system for the agent harness.
//!
//! Each tool is a struct implementing `HarnessTool`. Built-in tools provide
//! file I/O, shell execution, and git operations. The registry collects tools
//! and dispatches execution by name.

pub mod codegraph;
pub mod file;
pub mod git;
pub mod list_files;
pub mod memory;
pub mod rsi_control;
pub mod schedule_wake;
pub mod shell;

use crate::session::agent_verbs::AgentControlHandle;
use crate::session::harness::types::{HarnessToolSpec, ToolResult};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Trait for tools executable by the agent harness.
#[async_trait::async_trait]
pub trait HarnessTool: Send + Sync {
    /// Tool name (must be unique within a registry).
    fn name(&self) -> &str;

    /// Human-readable description for the LLM.
    fn description(&self) -> &str;

    /// JSON Schema string for input parameters.
    fn parameters_json(&self) -> &str;

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

    /// Convert to a HarnessToolSpec for provider API calls.
    fn to_spec(&self) -> HarnessToolSpec {
        HarnessToolSpec {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters_json: self.parameters_json().to_string(),
        }
    }
}

/// Registry of available tools for the agent loop.
pub struct HarnessToolRegistry {
    tools: HashMap<String, Arc<dyn HarnessTool>>,
}

impl HarnessToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    pub fn register(&mut self, tool: Arc<dyn HarnessTool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    pub fn specs(&self) -> Vec<HarnessToolSpec> {
        self.tools.values().map(|t| t.to_spec()).collect()
    }

    pub async fn execute(
        &self,
        name: &str,
        args: serde_json::Value,
        working_dir: &Path,
    ) -> ToolResult {
        match self.tools.get(name) {
            Some(tool) => tool.execute(args, working_dir).await,
            None => ToolResult {
                success: false,
                output: String::new(),
                error_msg: Some(format!("Unknown tool: {name}")),
            },
        }
    }

    pub async fn execute_cancellable(
        &self,
        name: &str,
        args: serde_json::Value,
        working_dir: &Path,
        cancel: &CancellationToken,
    ) -> ToolResult {
        match self.tools.get(name) {
            Some(tool) => tool.execute_cancellable(args, working_dir, cancel).await,
            None => ToolResult {
                success: false,
                output: String::new(),
                error_msg: Some(format!("Unknown tool: {name}")),
            },
        }
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
        execution_scratch: Option<crate::sandbox::execution_scratch::SandboxExecutionScratch>,
    ) -> Self {
        let mut registry = Self::new();
        registry.register(Arc::new(file::ReadFileTool));
        registry.register(Arc::new(file::WriteFileTool));
        registry.register(Arc::new(file::EditFileTool));
        // The shell keeps the construction-bound session stamp even though it
        // scrubs every ambient variable. Startup and resume orphan exclusion
        // can therefore identify a command that outlives a daemon crash.
        registry.register(Arc::new(shell::ShellTool::new(
            origin_session_id,
            launch_invocation_id,
            execution_scratch,
        )));
        registry.register(Arc::new(git::GitTool::new(
            origin_session_id,
            launch_invocation_id,
        )));
        registry.register(Arc::new(list_files::ListFilesTool));
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
        }
        registry
    }
}

impl Default for HarnessToolRegistry {
    fn default() -> Self {
        Self::new()
    }
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

    #[test]
    fn test_is_system_blocked() {
        assert!(is_system_blocked(Path::new("/etc/passwd")));
        assert!(is_system_blocked(Path::new("/bin/bash")));
        assert!(is_system_blocked(Path::new("/usr/bin/ls")));
        assert!(is_system_blocked(Path::new("/proc/self/maps")));
        assert!(!is_system_blocked(Path::new("/home/user/project/file.rs")));
        assert!(!is_system_blocked(Path::new("/tmp/workdir/file.txt")));
    }

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

    #[test]
    fn test_default_tools_has_all_builtins() {
        let registry = HarnessToolRegistry::default_tools(
            None, None, None, None, None, None, None, None, None,
        );
        let names: Vec<String> = registry.tools.keys().cloned().collect();
        assert!(names.contains(&"read_file".to_string()));
        assert!(names.contains(&"write_file".to_string()));
        assert!(names.contains(&"file_edit".to_string()));
        assert!(names.contains(&"shell".to_string()));
        assert!(names.contains(&"git".to_string()));
        assert!(names.contains(&"list_files".to_string()));
    }

    #[test]
    fn test_specs_returns_all() {
        // Without a store, schedule_wake is not registered, so count stays 6.
        let registry = HarnessToolRegistry::default_tools(
            None, None, None, None, None, None, None, None, None,
        );
        let specs = registry.specs();
        assert_eq!(specs.len(), 6);
    }

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
            )
        };
        let fresh: std::collections::BTreeSet<String> = build().tools.keys().cloned().collect();
        let rotation: std::collections::BTreeSet<String> = build().tools.keys().cloned().collect();
        assert_eq!(fresh, rotation, "fresh and rotation tool sets must match");

        let expected: std::collections::BTreeSet<String> = [
            "read_file",
            "write_file",
            "file_edit",
            "shell",
            "git",
            "list_files",
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
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(fresh, expected, "unified Harness tool roster drifted");
    }

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
            std::collections::BTreeSet::from(["AgentContinueChild", "AgentArchiveChild"]),
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
        );
        let names: Vec<&String> = registry.tools.keys().collect();
        assert!(!names.iter().any(|n| n.starts_with("rsi_control")));
    }
}
