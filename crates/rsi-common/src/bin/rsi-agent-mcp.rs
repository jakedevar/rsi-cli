//! Ephemeral stdio MCP gateway for the closed, token-bound agent RPC catalog.
//!
//! This binary is launched only as a child of a Codex or Claude CLI session
//! (the daemon passes it as a session-ephemeral MCP server). It has no
//! configuration file and never accepts an authority token from tool input.

use rsi_common::agent_control_schema::{
    MANAGER_PRELOADED_NATIVE_TOOLS, MCP_ALWAYS_LOAD_META_KEY, NativeAgentControlToolV1,
    agent_control_catalog_v1, roles_preload_manager_tools,
};
use rsi_common::rpc::RpcResponse;
use serde_json::{Value, json};
use std::io::{BufRead as _, Write as _};
use std::time::Duration;

/// #3565: how long `tools/list` waits for the daemon to name the caller's
/// roles. A slow or failed answer leaves every tool deferred, as before.
const ROLE_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// `preload_manager_tools` marks the manager seat's hot tools
/// `_meta["anthropic/alwaysLoad"] = true`, so Claude Code puts their schemas in
/// the prompt instead of deferring them behind `ToolSearch`. Every other tool
/// carries no marker and stays deferred.
fn tool_entries(preload_manager_tools: bool) -> Vec<Value> {
    agent_control_catalog_v1()
        .iter()
        .filter_map(|descriptor| {
            descriptor.native_tool.map(|native| {
                let mut entry = json!({
                    "name": native.name(),
                    "description": descriptor.description,
                    "inputSchema": descriptor.parameters(),
                });
                if preload_manager_tools && MANAGER_PRELOADED_NATIVE_TOOLS.contains(&native.name())
                {
                    entry["_meta"] = json!({ MCP_ALWAYS_LOAD_META_KEY: true });
                }
                entry
            })
        })
        .collect()
}

/// Whether the daemon names the caller a manager seat. Any failure is `false`.
fn caller_is_manager_seat<F>(dispatch: F) -> bool
where
    F: FnOnce(&str, Value) -> std::io::Result<RpcResponse>,
{
    let Ok(rpc) = dispatch(
        "AgentGetAuthorityCatalog",
        json!({"verb": "rsi_control_authority_catalog"}),
    ) else {
        return false;
    };
    if rpc.error.is_some() {
        return false;
    }
    rpc.result
        .as_ref()
        .and_then(|result| result.get("roles"))
        .and_then(Value::as_array)
        .is_some_and(|roles| {
            let roles = roles.iter().filter_map(Value::as_str).collect::<Vec<_>>();
            roles_preload_manager_tools(&roles)
        })
}

fn tool_by_name(name: &str) -> Option<NativeAgentControlToolV1> {
    agent_control_catalog_v1()
        .iter()
        .filter_map(|descriptor| descriptor.native_tool)
        .find(|native| native.name() == name)
}

fn response(id: Value, result: Value) -> Value {
    json!({"jsonrpc":"2.0", "id":id, "result":result})
}

fn tool_error(code: &str, detail: Value) -> Value {
    json!({
        "content": [{"type":"text", "text": serde_json::to_string(&json!({"code":code, "detail":detail})).expect("static JSON serializes")}],
        "isError": true,
    })
}

/// Test entry point: a worker's gateway (no hot tools preloaded).
#[cfg(test)]
fn handle_request<F>(request: Value, dispatch: F) -> Option<Value>
where
    F: FnOnce(&str, Value) -> std::io::Result<RpcResponse>,
{
    handle_request_for(request, dispatch, false)
}

fn handle_request_for<F>(request: Value, dispatch: F, preload_manager_tools: bool) -> Option<Value>
where
    F: FnOnce(&str, Value) -> std::io::Result<RpcResponse>,
{
    let id = request.get("id")?.clone();
    let method = request.get("method")?.as_str()?;
    match method {
        "initialize" => Some(response(
            id,
            json!({
                "protocolVersion":"2024-11-05",
                "capabilities":{"tools":{"listChanged":false}},
                "serverInfo":{"name":"rsi-agent-mcp","version":env!("CARGO_PKG_VERSION")},
            }),
        )),
        "notifications/initialized" => None,
        "tools/list" => Some(response(
            id,
            json!({"tools":tool_entries(preload_manager_tools)}),
        )),
        "tools/call" => {
            let Some(params) = request.get("params").and_then(Value::as_object) else {
                return Some(response(
                    id,
                    tool_error("invalid_input", json!({"field":"params"})),
                ));
            };
            let Some(name) = params.get("name").and_then(Value::as_str) else {
                return Some(response(
                    id,
                    tool_error("invalid_input", json!({"field":"name"})),
                ));
            };
            let Some(tool) = tool_by_name(name) else {
                return Some(response(
                    id,
                    tool_error("invalid_input", json!({"field":"name"})),
                ));
            };
            let arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let verb = tool.verb();
            if let Err(error) = verb.validate_params(&arguments) {
                let mut detail = json!({"field": error.field});
                error.enrich(&mut detail);
                return Some(response(id, tool_error(error.class, detail)));
            }
            let normalized = match dispatch(verb.descriptor().method, arguments) {
                Ok(rpc) if rpc.error.is_none() => json!({"code":"accepted", "result":rpc.result}),
                Ok(rpc) => json!({"code":"rpc_error", "error":rpc.error}),
                // A transport failure keeps `rpc_error` and now says why; a
                // response timeout keeps its own typed code and hint (#2622).
                Err(error) => {
                    let timed_out =
                        rsi_common::agent_rpc_client::response_timeout(&error).is_some();
                    json!({
                        "code": if timed_out { "rpc_response_timeout" } else { "rpc_error" },
                        "message": error.to_string(),
                    })
                }
            };
            Some(response(
                id,
                json!({
                    "content":[{"type":"text", "text":serde_json::to_string(&normalized).expect("normalized response serializes")}],
                    "isError": normalized["code"] != "accepted",
                }),
            ))
        }
        _ => Some(
            json!({"jsonrpc":"2.0", "id":id, "error":{"code":-32601,"message":"method not found"}}),
        ),
    }
}

fn main() {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    let mut preload_manager_tools: Option<bool> = None;
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(request) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        // The roles are asked once, on the first `tools/list`; the answer is
        // kept for the life of this gateway (one provider session).
        let preload = if request.get("method").and_then(Value::as_str) == Some("tools/list") {
            *preload_manager_tools.get_or_insert_with(|| {
                caller_is_manager_seat(|method, params| {
                    rsi_common::agent_rpc_client::dispatch_with_timeout(
                        &rsi_common::agent_rpc_client::resolve_socket_path(),
                        method,
                        params,
                        ROLE_PROBE_TIMEOUT,
                    )
                })
            })
        } else {
            false
        };
        let reply = handle_request_for(
            request,
            rsi_common::agent_rpc_client::dispatch_from_env,
            preload,
        );
        if let Some(reply) = reply
            && writeln!(
                stdout,
                "{}",
                serde_json::to_string(&reply).expect("MCP reply serializes")
            )
            .is_err()
        {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::rpc::RpcResponse;
    use std::cell::Cell;

    #[test]
    fn tool_list_is_exactly_the_native_catalog_without_rpc_only_verbs() {
        let reply =
            handle_request(json!({"id":1,"method":"tools/list"}), |_, _| unreachable!()).unwrap();
        let names = reply["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            names.len(),
            agent_control_catalog_v1()
                .iter()
                .filter(|entry| entry.native_tool.is_some())
                .count()
        );
        assert_eq!(names, rsi_common::rpc_verb_registry::native_tool_names());
        assert!(!names.contains(&"AgentArchiveChild"));
        assert!(!names.contains(&"rsi_control_archive_child"));
        for prepared_tool in [
            "rsi_control_manager_prepare_control",
            "rsi_control_manager_commit_prepared_control",
            "rsi_control_manager_get_action",
            "rsi_control_topology_upsert",
            "rsi_control_topology_list",
            "rsi_control_topology_get",
            "rsi_control_topology_execute",
            "rsi_control_topology_get_execution",
            "rsi_control_topology_interrupt",
            "rsi_control_topology_resolve_attempt",
        ] {
            assert!(names.contains(&prepared_tool));
        }
        // #3774: the verbs that were rsi-rpc-only are native tools whose input
        // schema is exactly the verb schema.
        let tools = reply["result"]["tools"].as_array().unwrap();
        for descriptor in agent_control_catalog_v1() {
            let Some(native) = descriptor.native_tool else {
                continue;
            };
            let tool = tools
                .iter()
                .find(|tool| tool["name"] == native.name())
                .unwrap_or_else(|| panic!("{} not listed", native.name()));
            assert_eq!(
                tool["inputSchema"],
                descriptor.parameters(),
                "{}",
                native.name()
            );
            assert_eq!(tool_by_name(native.name()), Some(native));
        }
        for rpc_layer_tool in [
            "rsi_control_enqueue_landing_source",
            "rsi_control_send_satellite_message",
            "rsi_control_cancel_wake",
            "rsi_control_submit_job",
            "rsi_control_get_job",
            "rsi_control_list_jobs",
            "rsi_control_cancel_job",
            "rsi_control_request_deploy",
            "rsi_control_get_daemon_info",
            "rsi_control_get_provider_status",
            "rsi_control_continue_child",
            "rsi_control_manager_delegate_node",
            "rsi_control_manager_escalate",
            "rsi_control_manager_list_escalations",
            "rsi_control_manager_resolve_escalation",
        ] {
            assert!(names.contains(&rpc_layer_tool), "{rpc_layer_tool}");
        }
    }

    fn listed(preload: bool) -> Vec<Value> {
        handle_request_for(
            json!({"id":1,"method":"tools/list"}),
            |_, _| unreachable!(),
            preload,
        )
        .unwrap()["result"]["tools"]
            .as_array()
            .unwrap()
            .clone()
    }

    /// #3565: a manager seat gets exactly the hot tools preloaded.
    #[test]
    fn manager_seat_preloads_exactly_the_hot_tools() {
        let tools = listed(true);
        let loaded = tools
            .iter()
            .filter(|tool| tool["_meta"][MCP_ALWAYS_LOAD_META_KEY] == true)
            .map(|tool| tool["name"].as_str().unwrap())
            .collect::<Vec<_>>();
        let mut expected = MANAGER_PRELOADED_NATIVE_TOOLS.to_vec();
        let mut got = loaded.clone();
        expected.sort_unstable();
        got.sort_unstable();
        assert_eq!(got, expected);
        // The rest of the catalog carries no marker (stays deferred).
        assert!(tools.len() > loaded.len());
        assert!(
            tools
                .iter()
                .filter(|tool| !loaded.contains(&tool["name"].as_str().unwrap()))
                .all(|tool| tool.get("_meta").is_none())
        );
    }

    /// #3565: a worker (or an unknown caller) preloads nothing.
    #[test]
    fn worker_preloads_no_tool() {
        assert!(listed(false).iter().all(|tool| tool.get("_meta").is_none()));
    }

    fn catalog_reply(roles: Value) -> std::io::Result<RpcResponse> {
        Ok(RpcResponse::success(
            Some(json!(1)),
            json!({"roles": roles}),
        ))
    }

    #[test]
    fn role_probe_names_manager_seats_and_fails_closed() {
        let asked = Cell::new(false);
        assert!(caller_is_manager_seat(|method, params| {
            asked.set(true);
            assert_eq!(method, "AgentGetAuthorityCatalog");
            assert_eq!(params["verb"], "rsi_control_authority_catalog");
            catalog_reply(json!(["worker", "manager"]))
        }));
        assert!(asked.get());
        assert!(caller_is_manager_seat(|_, _| catalog_reply(json!([
            "worker",
            "manager",
            "global_manager"
        ]))));
        assert!(!caller_is_manager_seat(|_, _| catalog_reply(json!([
            "worker",
            "epic_lead"
        ]))));
        assert!(!caller_is_manager_seat(|_, _| Err(std::io::Error::other(
            "daemon down"
        ))));
        assert!(!caller_is_manager_seat(|_, _| Ok(RpcResponse::error(
            Some(json!(1)),
            rsi_common::rpc::RpcError {
                code: -32000,
                message: "refused".into(),
                data: None,
            }
        ))));
    }

    #[test]
    fn malformed_call_is_rejected_before_dispatch() {
        let called = Cell::new(false);
        let reply = handle_request(json!({
            "id":1, "method":"tools/call", "params":{"name":"rsi_control_spawn", "arguments":{"kind":"Task"}}
        }), |_, _| { called.set(true); unreachable!() }).unwrap();
        assert!(!called.get());
        assert_eq!(reply["result"]["isError"], true);
        let error: Value =
            serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(error["code"], "invalid_input");
        assert_eq!(error["detail"]["field"], "params");
        assert_eq!(error["detail"]["path"], "params.query");
        assert_eq!(error["detail"]["constraint"], "required \"query\"");
    }

    #[test]
    fn valid_call_normalizes_daemon_result_without_claiming_completion() {
        let reply = handle_request(json!({
            "id":1, "method":"tools/call", "params":{"name":"rsi_control_status", "arguments":{}}
        }), |method, _| {
            assert_eq!(method, "AgentGetStatus");
            Ok(RpcResponse::success(Some(json!(1)), json!({"state":"queued"})))
        }).unwrap();
        let text = reply["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("accepted"));
        assert!(text.contains("queued"));
    }

    #[test]
    fn prepared_manager_call_routes_to_the_closed_agent_verb() {
        let epic = "5d73c05d-1040-49f7-92ab-0123456789ab";
        let reply = handle_request(json!({
            "id":1,
            "method":"tools/call",
            "params":{
                "name":"rsi_control_manager_prepare_control",
                "arguments":{"operation":{"action":"resume_lead","epic_id":epic,"message":"continue"}}
            }
        }), |method, arguments| {
            assert_eq!(method, "AgentManagerPrepareControl");
            assert_eq!(arguments["operation"]["epic_id"], epic);
            Ok(RpcResponse::success(Some(json!(1)), json!({"readiness":"ready"})))
        }).unwrap();
        let text = reply["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("accepted"));
        assert!(text.contains("ready"));
    }

    #[test]
    fn tool_call_dispatches_without_caller_supplied_identity() {
        let reply = handle_request(
            json!({
                "id":1,
                "method":"tools/call",
                "params":{
                    "name":"rsi_control_status",
                    "arguments":{
                        "session_token":"must-not-be-accepted",
                        "caller_session_id":"must-not-be-accepted"
                    }
                }
            }),
            |method, arguments| {
                assert_eq!(method, "AgentGetStatus");
                assert!(arguments.get("session_token").is_none());
                assert!(arguments.get("caller_session_id").is_none());
                Ok(RpcResponse::success(
                    Some(json!(1)),
                    json!({"state":"queued"}),
                ))
            },
        )
        .unwrap();
        let text = reply["result"]["content"][0]["text"].as_str().unwrap();
        let normalized: Value = serde_json::from_str(text).unwrap();
        assert_eq!(normalized["code"], "invalid_input");
        assert_eq!(normalized["detail"]["field"], "params");
    }
    #[test]
    fn audit_response_timeout_is_typed_and_does_not_advise_a_replay() {
        let reply = handle_request(
            json!({
                "id":1,
                "method":"tools/call",
                "params":{
                    "name":"rsi_control_audit_source_worktree_cohort",
                    "arguments":{"repository_identity":"repo:example"}
                }
            }),
            |method, _| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    rsi_common::agent_rpc_client::RpcResponseTimeout {
                        method: method.to_string(),
                        timeout: std::time::Duration::from_secs(30),
                    },
                ))
            },
        )
        .unwrap();
        let text = reply["result"]["content"][0]["text"].as_str().unwrap();
        let normalized: Value = serde_json::from_str(text).unwrap();
        assert_eq!(normalized["code"], "rpc_response_timeout");
        let message = normalized["message"].as_str().unwrap();
        assert!(
            message.contains("AgentAuditSourceWorktreeCohort"),
            "{message}"
        );
        assert!(!message.contains("Replay it"), "{message}");
        assert_eq!(reply["result"]["isError"], true);
    }
}
