//! Dynamic tool registry for app-server provider sessions.
//!
//! Tools are advertised to app-server providers via `dynamicTools` in `thread/start`.
//! When the provider calls a tool, the monitor loop dispatches to the registry.

use crate::error::Result;
use serde_json::Value;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// Specification for a tool that can be injected into provider sessions.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema for the tool's input parameters.
    pub parameters: Value,
}

/// Handler function type for dynamic tools.
/// Takes the tool call arguments and returns a result value.
pub type ToolHandler =
    Arc<dyn Fn(Value) -> Pin<Box<dyn Future<Output = Result<Value>> + Send>> + Send + Sync>;

/// Registry mapping tool names to their specs and async handler functions.
pub struct ToolRegistry {
    tools: HashMap<String, (ToolSpec, ToolHandler)>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    /// Register a tool spec with its async handler.
    pub fn register(&mut self, spec: ToolSpec, handler: ToolHandler) {
        self.tools.insert(spec.name.clone(), (spec, handler));
    }

    /// Return all registered tool specs (for injection into `thread/start`).
    pub fn tool_specs(&self) -> Vec<ToolSpec> {
        self.tools.values().map(|(spec, _)| spec.clone()).collect()
    }

    /// Execute a tool by name with the given arguments.
    pub async fn execute(&self, name: &str, args: Value) -> Result<Value> {
        let (_, handler) = self
            .tools
            .get(name)
            .ok_or_else(|| crate::error::DaemonError::Process(format!("Unknown tool: {}", name)))?;
        (handler)(args).await
    }

    /// Whether any tools are registered.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Number of registered tools.
    pub fn len(&self) -> usize {
        self.tools.len()
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

fn agent_issue_invalid_request(
    validation: rsi_common::rpc::AgentIssueValidationV1,
) -> crate::error::DaemonError {
    crate::error::agent_issue_invalid_request(validation)
}

pub(crate) fn serialize_agent_issue_tool_result<T: serde::Serialize>(
    result: crate::error::Result<T>,
) -> Result<Value> {
    let value = result.map_err(crate::error::normalize_agent_issue_error)?;
    serde_json::to_value(value).map_err(|_| {
        crate::error::agent_issue_error(
            rsi_common::rpc::AgentIssueErrorCodeV1::StorageFailure,
            None,
            None,
        )
    })
}

/// Register the native `rsi_control` coordination dynamic tools for
/// CodexAppServer sessions. No-op unless both a control handle and a bound
/// caller session id are present. The arg-parsing / outcome-formatting logic is
/// shared with the Harness tools via `session::harness::tools::rsi_control`, so
/// only the transport wrapper (a `ToolHandler` closure vs. a `HarnessTool`
/// impl) differs — the authority path is identical.
fn register_rsi_control_tools(
    registry: &mut ToolRegistry,
    agent_control: Option<crate::session::agent_verbs::AgentControlHandle>,
    caller_session_id: Option<uuid::Uuid>,
) {
    use crate::session::harness::tools::rsi_control;
    use rsi_common::agent_control_schema::AgentControlVerbV1;

    let (Some(control), Some(caller)) = (agent_control, caller_session_id) else {
        return;
    };

    // Operator-appointed manager scope is checked by the shared guarded service.
    for kind in rsi_control::ManagerControlToolKind::ALL {
        let descriptor = kind.verb().descriptor();
        let spec = ToolSpec {
            name: kind.name().to_string(),
            description: descriptor.description.to_string(),
            parameters: descriptor.parameters(),
        };
        let control = control.clone();
        let handler: ToolHandler = Arc::new(move |args: Value| {
            let control = control.clone();
            Box::pin(async move {
                rsi_control::execute_manager_tool(&control, caller, kind, args).await
            })
        });
        registry.register(spec, handler);
    }

    // strong master baton reservation
    {
        let control = control.clone();
        let spec = ToolSpec {
            name: "rsi_control_reserve_successor".to_string(),
            description: "Reserve one daemon-authored same-Epic successor and transfer the master baton only after provider establishment. Exact retries return the original successor; authority identities are server-bound.".to_string(),
            parameters: AgentControlVerbV1::ReserveSuccessor
                .descriptor()
                .parameters(),
        };
        let handler: ToolHandler = Arc::new(move |args: Value| {
            let control = control.clone();
            Box::pin(async move {
                let request = rsi_control::reserve_successor_request_from_args(&args)
                    .map_err(crate::error::DaemonError::InvalidParam)?;
                let result = control.agent_reserve_successor(caller, request).await?;
                serde_json::to_value(result).map_err(crate::error::DaemonError::Json)
            })
        });
        registry.register(spec, handler);
    }

    // spawn
    {
        let control = control.clone();
        let spec = ToolSpec {
            name: "rsi_control_spawn".to_string(),
            description: "Spawn a child agent session under the Epic you lead. Validated \
                 through the same lead-identity, recursion-depth, and rate-limit checks as \
                 the spawn coordinator; the spawning session is bound server-side."
                .to_string(),
            parameters: AgentControlVerbV1::SpawnChild.descriptor().parameters(),
        };
        let handler: ToolHandler = Arc::new(move |args: Value| {
            let control = control.clone();
            Box::pin(async move {
                let request = rsi_control::spawn_request_from_args(&args)
                    .map_err(crate::error::DaemonError::InvalidParam)?;
                let outcome = control.agent_spawn_child(caller, request).await;
                match outcome {
                    crate::session::agent_verbs::AgentSpawnChildOutcome::Accepted(result) => {
                        serde_json::to_value(result).map_err(crate::error::DaemonError::Json)
                    }
                    crate::session::agent_verbs::AgentSpawnChildOutcome::Rejected(reason) => {
                        Err(crate::error::DaemonError::InvalidParam(format!(
                            "agent_spawn_rejected:{reason:?}"
                        )))
                    }
                }
            })
        });
        registry.register(spec, handler);
    }

    // aggregate progress
    {
        let control = control.clone();
        let spec = ToolSpec {
            name: "rsi_control_progress".to_string(),
            description: "Read one bounded durable snapshot for the child cohort you are authorized to observe, including status counts, event cursors, freshness, watch state, and message counts."
                .to_string(),
            parameters: AgentControlVerbV1::GetProgress.descriptor().parameters(),
        };
        let handler: ToolHandler = Arc::new(move |args: Value| {
            let control = control.clone();
            Box::pin(async move {
                let params = rsi_control::progress_params_from_args(&args)
                    .map_err(crate::error::DaemonError::InvalidParam)?;
                let result = control
                    .agent_get_progress(caller, &params.session_ids)
                    .await?;
                serde_json::to_value(result).map_err(crate::error::DaemonError::Json)
            })
        });
        registry.register(spec, handler);
    }

    // durable owner->child mail (P2-03)
    {
        let control = control.clone();
        let spec = ToolSpec {
            name: "rsi_control_send_message".to_string(),
            description: "Queue durable mail for a child agent you own (your own \
                 reserved or direct child, or a child of an Epic you lead). A queued \
                 receipt proves acceptance, not provider delivery. Delivery requires a \
                 supported idle boundary before expiry; expiry can win first, and mail \
                 never interrupts a running turn. Omitted expires_at defaults to 30 \
                 minutes after first acceptance. Requires a stable idempotency_key; an \
                 exact retry returns the same message ID and original deadline. You cannot \
                 message yourself, and the sending session is bound server-side."
                .to_string(),
            parameters: AgentControlVerbV1::SendMessage.descriptor().parameters(),
        };
        let handler: ToolHandler = Arc::new(move |args: Value| {
            let control = control.clone();
            Box::pin(async move {
                let request = rsi_control::send_message_request_from_args(&args)
                    .map_err(crate::error::DaemonError::InvalidParam)?;
                let receipt = control.agent_send_message(caller, request).await?;
                serde_json::to_value(receipt).map_err(crate::error::DaemonError::Json)
            })
        });
        registry.register(spec, handler);
    }

    // argument-free daemon-authoritative program identity
    {
        let control = control.clone();
        let spec = ToolSpec {
            name: "rsi_control_program_guard".to_string(),
            description: "Idempotently register daemon-authoritative master-orchestrate \
                 program identity. This tool accepts no arguments; caller identity, \
                 same-session Resume custody, timing, and row UUID are bound and derived \
                 server-side."
                .to_string(),
            parameters: serde_json::from_str(rsi_control::PROGRAM_GUARD_INPUT_SCHEMA)
                .expect("PROGRAM_GUARD_INPUT_SCHEMA is valid JSON"),
        };
        let handler: ToolHandler = Arc::new(move |args: Value| {
            let control = control.clone();
            Box::pin(async move {
                rsi_control::validate_program_guard_args(&args)
                    .map_err(crate::error::DaemonError::InvalidParam)?;
                let outcome = control.register_bound_program_guard(caller).await?;
                Ok(rsi_control::program_guard_registration_value(&outcome))
            })
        });
        registry.register(spec, handler);
    }

    // status
    {
        let control = control.clone();
        let spec = ToolSpec {
            name: "rsi_control_status".to_string(),
            description: "Read the status of a session you are authorized to observe \
                 (yourself, a direct child, or a child of an Epic you lead). Omit \
                 session_id to target your own session."
                .to_string(),
            parameters: AgentControlVerbV1::GetStatus.descriptor().parameters(),
        };
        let handler: ToolHandler = Arc::new(move |args: Value| {
            let control = control.clone();
            Box::pin(async move {
                let target = rsi_control::resolve_target_id(&args, caller)
                    .map_err(crate::error::DaemonError::InvalidParam)?;
                let session = control.agent_get_status(caller, target).await?;
                Ok(serde_json::to_value(session).map_err(crate::error::DaemonError::Json)?)
            })
        });
        registry.register(spec, handler);
    }

    // halt
    {
        let control = control.clone();
        let spec = ToolSpec {
            name: "rsi_control_halt".to_string(),
            description: "Interrupt a session you are authorized to control (yourself, a \
                 direct child, or a child of an Epic you lead). Omit session_id to halt \
                 your own session."
                .to_string(),
            parameters: AgentControlVerbV1::Halt.descriptor().parameters(),
        };
        let handler: ToolHandler = Arc::new(move |args: Value| {
            let control = control.clone();
            Box::pin(async move {
                let target = rsi_control::resolve_target_id(&args, caller)
                    .map_err(crate::error::DaemonError::InvalidParam)?;
                control.agent_halt(caller, target).await?;
                Ok(serde_json::json!({ "ok": true, "session_id": target }))
            })
        });
        registry.register(spec, handler);
    }

    // create issue
    {
        let control = control.clone();
        let spec = ToolSpec {
            name: "rsi_control_create_issue".to_string(),
            description: "Create one durable local issue follow-up. The creator is bound to this session; use a stable idempotency_key for safe retries.".to_string(),
            parameters: AgentControlVerbV1::CreateIssue.descriptor().parameters(),
        };
        let handler: ToolHandler = Arc::new(move |args: Value| {
            let control = control.clone();
            Box::pin(async move {
                let params = rsi_control::agent_create_issue_from_args(&args)
                    .map_err(crate::error::DaemonError::InvalidParam)?;
                let result = control.agent_create_issue(caller, params).await?;
                serde_json::to_value(result).map_err(crate::error::DaemonError::Json)
            })
        });
        registry.register(spec, handler);
    }

    // V95 lead-scoped Issue controls. The native schemas/parsers are shared
    // byte-for-byte with Harness and the caller remains construction-bound.
    for kind in [
        rsi_control::IssueControlToolKind::List,
        rsi_control::IssueControlToolKind::Get,
        rsi_control::IssueControlToolKind::Update,
        rsi_control::IssueControlToolKind::UpdateStatus,
        rsi_control::IssueControlToolKind::Archive,
        rsi_control::IssueControlToolKind::Restore,
        rsi_control::IssueControlToolKind::ListEvents,
    ] {
        let description = match kind {
            rsi_control::IssueControlToolKind::List => {
                "List bounded local Issues in the project owned by the Epic you currently lead."
            }
            rsi_control::IssueControlToolKind::Get => {
                "Read one local Issue in the project owned by the Epic you currently lead."
            }
            rsi_control::IssueControlToolKind::Update => {
                "CAS-update active Issue content as the current owning-Epic lead or manager with issue-coordinate authority."
            }
            rsi_control::IssueControlToolKind::UpdateStatus => {
                "CAS-update one Issue status as the current owning-Epic lead or manager with issue-coordinate authority."
            }
            rsi_control::IssueControlToolKind::Archive => {
                "Archive a terminal Issue with row-version CAS as the current owning-Epic lead or manager with issue-coordinate authority."
            }
            rsi_control::IssueControlToolKind::Restore => {
                "Restore an archived Issue with row-version CAS as the current owning-Epic lead or manager with issue-coordinate authority."
            }
            rsi_control::IssueControlToolKind::ListEvents => {
                "Read bounded immutable Issue audit history as the current owning-Epic lead."
            }
        };
        let descriptor = kind.verb().descriptor();
        let name = descriptor
            .native_tool
            .expect("every guarded Issue verb has a native mapping")
            .name();
        let spec = ToolSpec {
            name: name.to_string(),
            description: description.to_string(),
            parameters: descriptor.parameters(),
        };
        let control = control.clone();
        let handler: ToolHandler = Arc::new(move |args: Value| {
            let control = control.clone();
            Box::pin(async move {
                match kind {
                    rsi_control::IssueControlToolKind::List => {
                        let params = rsi_control::agent_list_issues_from_args(&args)
                            .map_err(agent_issue_invalid_request)?;
                        serialize_agent_issue_tool_result(
                            control.agent_list_issues(caller, params).await,
                        )
                    }
                    rsi_control::IssueControlToolKind::Get => {
                        let params = rsi_control::agent_get_issue_from_args(&args)
                            .map_err(agent_issue_invalid_request)?;
                        serialize_agent_issue_tool_result(
                            control.agent_get_issue(caller, params).await,
                        )
                    }
                    rsi_control::IssueControlToolKind::Update => {
                        let params = rsi_control::agent_update_issue_from_args(&args)
                            .map_err(agent_issue_invalid_request)?;
                        serialize_agent_issue_tool_result(
                            control.agent_update_issue(caller, params).await,
                        )
                    }
                    rsi_control::IssueControlToolKind::UpdateStatus => {
                        let params = rsi_control::agent_update_issue_status_from_args(&args)
                            .map_err(agent_issue_invalid_request)?;
                        serialize_agent_issue_tool_result(
                            control.agent_update_issue_status(caller, params).await,
                        )
                    }
                    rsi_control::IssueControlToolKind::Archive => {
                        let params = rsi_control::agent_archive_issue_from_args(&args)
                            .map_err(agent_issue_invalid_request)?;
                        serialize_agent_issue_tool_result(
                            control.agent_archive_issue(caller, params).await,
                        )
                    }
                    rsi_control::IssueControlToolKind::Restore => {
                        let params = rsi_control::agent_restore_issue_from_args(&args)
                            .map_err(agent_issue_invalid_request)?;
                        serialize_agent_issue_tool_result(
                            control.agent_restore_issue(caller, params).await,
                        )
                    }
                    rsi_control::IssueControlToolKind::ListEvents => {
                        let params = rsi_control::agent_list_issue_events_from_args(&args)
                            .map_err(agent_issue_invalid_request)?;
                        serialize_agent_issue_tool_result(
                            control.agent_list_issue_events(caller, params).await,
                        )
                    }
                }
            })
        });
        registry.register(spec, handler);
    }
}

/// Register built-in tools into the registry.
///
/// Registers `rsi_memory_search` if a memory handle is available, and the
/// native `rsi_control` coordination tools if an [`AgentControlHandle`]
/// and a bound caller session id are supplied.
///
/// `project_id` is the host session's project scope. When `Some`, the
/// tool restricts results to that project's indexed memory; when `None`,
/// the search is unscoped. The agent cannot widen the scope through tool
/// args — the schema deliberately omits `project_id` (plan §D2).
///
/// `agent_control` + `caller_session_id` back the native `rsi_control` tools
/// for CodexAppServer sessions. Like the Harness native tools, the caller
/// session id is bound here at registration and never appears in any tool's
/// input schema, and every call routes through the same guarded
/// [`AgentControlHandle`] the `Agent*` RPC verbs use (gate-pack invariant:
/// native tools bind at construction + two enforcement layers). Native tools
/// are needed here because CodexAppServer runs in-process and does not carry
/// the token through a shell→`rsi-rpc` bridge.
pub fn register_builtin_tools(
    registry: &mut ToolRegistry,
    memory_handle: Option<crate::memory::worker::MemoryHandle>,
    project_id: Option<uuid::Uuid>,
    agent_control: Option<crate::session::agent_verbs::AgentControlHandle>,
    caller_session_id: Option<uuid::Uuid>,
) {
    register_rsi_control_tools(registry, agent_control, caller_session_id);
    if let Some(handle) = memory_handle {
        let spec = ToolSpec {
            name: "rsi_memory_search".to_string(),
            description:
                "Search Rsi's memory index for relevant context from past sessions and documents"
                    .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Search query to find relevant context"
                    },
                    "max_results": {
                        "type": "integer",
                        "description": "Maximum number of results to return (default 5)"
                    }
                },
                "required": ["query"]
            }),
        };
        let handle = Arc::new(handle);
        let handler: ToolHandler = Arc::new(move |args: Value| {
            let handle = Arc::clone(&handle);
            Box::pin(async move {
                let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
                let max = args
                    .get("max_results")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(5) as usize;
                let results = handle.search(query, Some(max), None, project_id).await?;
                Ok(serde_json::to_value(results).map_err(crate::error::DaemonError::Json)?)
            })
        });
        registry.register(spec, handler);
    }
}

/// Add the same read-only Codegraph tools as Harness to a session-bound
/// CodexAppServer registry. Authority is rechecked from durable session state
/// on each call; provider arguments cannot name a project or root.
pub fn register_codegraph_tools(
    registry: &mut ToolRegistry,
    handle: crate::codegraph::IndexHandle,
    store: Arc<tokio::sync::Mutex<crate::store::Store>>,
    session_id: uuid::Uuid,
    binding: crate::codegraph::NativeCodegraphBinding,
) {
    for kind in crate::codegraph::NativeCodegraphToolKind::ALL {
        if !binding.permits(kind) {
            continue;
        }
        let spec = ToolSpec {
            name: kind.name().into(),
            description: kind.description().into(),
            parameters: kind.schema(),
        };
        let handle = handle.clone();
        let store = Arc::clone(&store);
        let binding = binding.clone();
        let handler: ToolHandler = Arc::new(move |args: Value| {
            let handle = handle.clone();
            let store = Arc::clone(&store);
            let binding = binding.clone();
            Box::pin(async move {
                crate::codegraph::execute_native_read(
                    handle, store, session_id, binding, kind, args,
                )
                .await
            })
        });
        registry.register(spec, handler);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_control_handle() -> crate::session::agent_verbs::AgentControlHandle {
        use crate::session::spawn_coordinator::SpawnCoordinator;
        let active = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
        let completed = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
        let store = Arc::new(tokio::sync::Mutex::new(
            crate::store::Store::open_in_memory().unwrap(),
        ));
        let (tx, _rx) = tokio::sync::mpsc::channel(4);
        crate::session::agent_verbs::AgentControlHandle::new(
            active,
            completed,
            store,
            Arc::new(crate::bus::EventBus::new(16)),
            Arc::new(SpawnCoordinator::new(tx)),
        )
    }

    #[test]
    fn d05_program_run_rpcs_are_absent_from_native_tool_registry() {
        let registry = ToolRegistry::new();
        let specs = registry.tool_specs();
        for name in [
            "CreateProgramRun",
            "GetProgramRun",
            "ListProgramRuns",
            "ListProgramRunTransitions",
            "GetProgramRunOperationalStatus",
            "CancelProgramRun",
            "ResumeBlockedProgramRun",
            "ReconcileProgramRuns",
        ] {
            assert!(specs.iter().all(|spec| spec.name != name));
        }
        let source = include_str!("tool_registry.rs");
        assert!(!source.contains(&["rsi_control", "program_run"].join("_")));
    }
    use serde_json::json;

    #[tokio::test]
    async fn test_registry_register_and_execute() {
        let mut registry = ToolRegistry::new();
        let spec = ToolSpec {
            name: "echo_tool".to_string(),
            description: "Echoes input".to_string(),
            parameters: json!({"type": "object", "properties": {"input": {"type": "string"}}}),
        };
        let handler: ToolHandler = Arc::new(|args: Value| Box::pin(async move { Ok(args) }));
        registry.register(spec, handler);

        let result = registry
            .execute("echo_tool", json!({"input": "hello"}))
            .await
            .unwrap();
        assert_eq!(result["input"], "hello");
    }

    #[test]
    fn test_registry_tool_specs() {
        let mut registry = ToolRegistry::new();
        assert_eq!(registry.len(), 0);

        registry.register(
            ToolSpec {
                name: "tool_a".to_string(),
                description: "A".to_string(),
                parameters: json!({}),
            },
            Arc::new(|args| Box::pin(async move { Ok(args) })),
        );
        registry.register(
            ToolSpec {
                name: "tool_b".to_string(),
                description: "B".to_string(),
                parameters: json!({}),
            },
            Arc::new(|args| Box::pin(async move { Ok(args) })),
        );

        assert_eq!(registry.len(), 2);
        let specs = registry.tool_specs();
        assert_eq!(specs.len(), 2);
    }

    #[tokio::test]
    async fn test_registry_unknown_tool_returns_error() {
        let registry = ToolRegistry::new();
        let result = registry.execute("nonexistent", json!({})).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Unknown tool"));
    }

    #[tokio::test]
    async fn codex_app_server_registers_argument_free_program_guard() {
        let caller = uuid::Uuid::new_v4();
        let mut registry = ToolRegistry::new();
        register_builtin_tools(
            &mut registry,
            None,
            None,
            Some(test_control_handle()),
            Some(caller),
        );
        let spec = registry
            .tool_specs()
            .into_iter()
            .find(|spec| spec.name == "rsi_control_program_guard")
            .expect("CodexAppServer program guard tool");
        assert_eq!(spec.parameters["additionalProperties"], json!(false));
        assert_eq!(spec.parameters["properties"], json!({}));

        let injected = registry
            .execute(
                "rsi_control_program_guard",
                json!({ "caller_session_id": caller }),
            )
            .await
            .expect_err("caller injection must fail");
        assert!(injected.to_string().contains("accepts no arguments"));

        let routed = registry
            .execute("rsi_control_program_guard", json!({}))
            .await
            .expect_err("missing construction-bound caller row must fail");
        assert!(routed.to_string().to_lowercase().contains("not found"));
    }

    #[test]
    fn codex_app_server_agent_tool_schemas_match_the_common_catalog() {
        use rsi_common::agent_control_schema::NativeAgentControlToolV1;

        let mut registry = ToolRegistry::new();
        register_builtin_tools(
            &mut registry,
            None,
            None,
            Some(test_control_handle()),
            Some(uuid::Uuid::new_v4()),
        );
        let specs = registry.tool_specs();
        let catalog = rsi_common::agent_control_schema::agent_control_catalog_v1();
        let rpc_only = catalog
            .iter()
            .filter(|descriptor| descriptor.native_tool.is_none())
            .map(|descriptor| descriptor.method)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            rpc_only,
            std::collections::BTreeSet::from(["AgentContinueChild", "AgentArchiveChild"]),
            "the native CodexAppServer RPC-only verb set changed without review"
        );
        for descriptor in catalog {
            let Some(native) = descriptor.native_tool else {
                continue;
            };
            if native == NativeAgentControlToolV1::ScheduleWake {
                assert!(specs.iter().all(|spec| spec.name != native.name()));
                continue;
            }
            let spec = specs
                .iter()
                .find(|spec| spec.name == native.name())
                .unwrap_or_else(|| panic!("missing CodexAppServer tool {}", native.name()));
            assert_eq!(
                spec.parameters,
                descriptor.parameters(),
                "tool: {}",
                native.name()
            );
        }
    }

    #[tokio::test]
    async fn manager_codex_tools_bind_caller_and_redact_malformed_args() {
        use crate::session::harness::tools::rsi_control::ManagerControlToolKind;

        for (control, caller) in [
            (None, Some(uuid::Uuid::new_v4())),
            (Some(test_control_handle()), None),
        ] {
            let mut registry = ToolRegistry::new();
            register_builtin_tools(&mut registry, None, None, control, caller);
            assert!(
                registry.is_empty(),
                "unbound registries must expose no agent tools"
            );
        }
        let mut registry = ToolRegistry::new();
        register_builtin_tools(
            &mut registry,
            None,
            None,
            Some(test_control_handle()),
            Some(uuid::Uuid::new_v4()),
        );
        assert_eq!(registry.len(), 28);
        for kind in ManagerControlToolKind::ALL {
            let reference = uuid::Uuid::new_v4();
            let mut args = match kind {
                ManagerControlToolKind::Progress
                | ManagerControlToolKind::Inbox
                | ManagerControlToolKind::Inspect
                | ManagerControlToolKind::WorkView => json!({}),
                ManagerControlToolKind::Update => {
                    json!({"fence":{"scope_version":1,"policy_version":1},"idempotency_key":"one","change":{"update":"handoff","summary":"Ready","next_actions":[]}})
                }
                ManagerControlToolKind::SubmitReviewReceipt => {
                    json!({"assignment_id":reference,"verdict":"accepted","findings":[],"idempotency_key":"receipt"})
                }
                ManagerControlToolKind::Control => {
                    json!({"fence":{"scope_version":1,"policy_version":1},"idempotency_key":"one","operation":{"action":"create_container","kind":"Group","name":"Group","tags":[]}})
                }
                ManagerControlToolKind::PrepareControl => {
                    json!({"operation":{"action":"resume_lead","epic_id":reference,"message":"continue"}})
                }
                ManagerControlToolKind::CommitPreparedControl => {
                    json!({"prepared_id":reference,"target_digest":format!("sha256:{}", "a".repeat(64)),"idempotency_key":"commit"})
                }
                ManagerControlToolKind::GetAction => json!({"operation_id":reference}),
                ManagerControlToolKind::Send => json!({
                    "epic_id": reference, "message": "evidence?", "idempotency_key": "request"
                }),
                ManagerControlToolKind::Reply => json!({
                    "request_id": reference, "message": "checks passed", "idempotency_key": "reply"
                }),
                ManagerControlToolKind::Notify => json!({
                    "message": "checks passed", "idempotency_key": "notify"
                }),
            };
            args["sender_session_id"] = json!("/private/value token=secret");
            let error = registry.execute(kind.name(), args).await.unwrap_err();
            assert!(
                matches!(error, crate::error::DaemonError::InvalidParam(code) if code == "manager_invalid_request")
            );
        }
    }

    #[tokio::test]
    async fn agent_issue_codex_parser_and_authority_failures_use_safe_envelopes() {
        use rsi_common::rpc::{
            AgentIssueErrorCodeV1, AgentIssueErrorV1, AgentIssueValidationClassV1 as Class,
            AgentIssueValidationFieldV1 as Field,
        };

        let mut registry = ToolRegistry::new();
        register_builtin_tools(
            &mut registry,
            None,
            None,
            Some(test_control_handle()),
            Some(uuid::Uuid::new_v4()),
        );
        let issue_id = uuid::Uuid::new_v4();
        for (name, args, expected_class, expected_field) in [
            (
                "rsi_control_list_issues",
                json!({"limit": "many"}),
                Class::InvalidField,
                Some(Field::Limit),
            ),
            (
                "rsi_control_get_issue",
                json!({"issue_id": issue_id, "project_id/secret": "never"}),
                Class::UnknownField,
                None,
            ),
            (
                "rsi_control_update_issue",
                json!({"issue_id": issue_id}),
                Class::MissingField,
                Some(Field::ExpectedRowVersion),
            ),
            (
                "rsi_control_update_issue_status",
                json!({
                    "issue_id": issue_id,
                    "status": 7,
                    "expected_row_version": 1,
                    "idempotency_key": "status-codex"
                }),
                Class::InvalidField,
                Some(Field::Status),
            ),
            (
                "rsi_control_archive_issue",
                json!({
                    "issue_id": issue_id,
                    "expected_row_version": 1,
                    "idempotency_key": 7
                }),
                Class::InvalidField,
                Some(Field::IdempotencyKey),
            ),
            (
                "rsi_control_restore_issue",
                json!([]),
                Class::InvalidShape,
                None,
            ),
            (
                "rsi_control_list_issue_events",
                json!({"issue_id": issue_id, "after_sequence": "zero"}),
                Class::InvalidField,
                Some(Field::AfterSequence),
            ),
        ] {
            let error = registry.execute(name, args).await.unwrap_err();
            let crate::error::DaemonError::StructuredRpc { data, .. } = error else {
                panic!("Issue tool returned an unstructured error");
            };
            let encoded = data.to_string();
            let envelope: AgentIssueErrorV1 = serde_json::from_value(data).unwrap();
            assert_eq!(envelope.code, AgentIssueErrorCodeV1::InvalidRequest);
            let validation = envelope.validation.expect("bounded validation hint");
            assert_eq!(validation.class, expected_class, "tool: {name}");
            assert_eq!(validation.field, expected_field, "tool: {name}");
            assert!(!encoded.contains("project_id/secret"));
            assert!(!encoded.contains("never"));
        }

        let error = registry
            .execute(
                "rsi_control_get_issue",
                json!({"issue_id": uuid::Uuid::new_v4()}),
            )
            .await
            .unwrap_err();
        let crate::error::DaemonError::StructuredRpc { data, .. } = error else {
            panic!("Issue tool returned an unstructured error");
        };
        let envelope: AgentIssueErrorV1 = serde_json::from_value(data).unwrap();
        assert_eq!(envelope.code, AgentIssueErrorCodeV1::AuthorityDenied);
        assert_eq!(envelope.validation, None);
    }

    #[test]
    fn test_rsi_memory_search_tool_spec_json_schema() {
        // Verify the spec would have correct JSON schema if memory was available
        let spec = ToolSpec {
            name: "rsi_memory_search".to_string(),
            description:
                "Search Rsi's memory index for relevant context from past sessions and documents"
                    .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Search query to find relevant context"
                    },
                    "max_results": {
                        "type": "integer",
                        "description": "Maximum number of results to return (default 5)"
                    }
                },
                "required": ["query"]
            }),
        };
        assert_eq!(spec.name, "rsi_memory_search");
        assert_eq!(spec.parameters["type"], "object");
        assert!(
            spec.parameters["required"]
                .as_array()
                .unwrap()
                .contains(&json!("query"))
        );
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn d04_codex_app_server_create_issue_schema_has_no_linkage_or_authority_fields() {
        let schema = rsi_common::agent_control_schema::AgentControlVerbV1::CreateIssue
            .descriptor()
            .parameters();
        for field in [
            "project_id",
            "idea_id",
            "source_event_id",
            "source_finding_ref",
            "created_by_session_id",
            "caller_session_id",
            "actor_id",
        ] {
            assert!(
                schema["properties"].get(field).is_none(),
                "schema exposed {field}"
            );
        }
        assert!(
            !schema.to_string().contains("LinkIssueToIdea"),
            "CodexAppServer must not advertise a native Issue-link tool"
        );
    }
}
