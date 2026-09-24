//! Ephemeral stdio MCP gateway for the closed, token-bound agent RPC catalog.
//!
//! This binary is launched only as a child of a Codex session. It has no
//! configuration file and never accepts an authority token from tool input.

use rsi_common::agent_control_schema::{NativeAgentControlToolV1, agent_control_catalog_v1};
use rsi_common::rpc::RpcResponse;
use serde_json::{Value, json};
use std::io::{BufRead as _, Write as _};

fn tool_entries() -> Vec<Value> {
    agent_control_catalog_v1()
        .iter()
        .filter_map(|descriptor| {
            descriptor.native_tool.map(|native| {
                json!({
                    "name": native.name(),
                    "description": descriptor.description,
                    "inputSchema": descriptor.parameters(),
                })
            })
        })
        .collect()
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

fn handle_request<F>(request: Value, dispatch: F) -> Option<Value>
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
        "tools/list" => Some(response(id, json!({"tools":tool_entries()}))),
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
                return Some(response(
                    id,
                    tool_error(error.class, json!({"field": error.field})),
                ));
            }
            let normalized = match dispatch(verb.descriptor().method, arguments) {
                Ok(rpc) if rpc.error.is_none() => json!({"code":"accepted", "result":rpc.result}),
                Ok(rpc) => json!({"code":"rpc_error", "error":rpc.error}),
                Err(_) => json!({"code":"rpc_error"}),
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
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(request) = serde_json::from_str(&line) else {
            continue;
        };
        let reply = handle_request(request, rsi_common::agent_rpc_client::dispatch_from_env);
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
        assert!(!names.contains(&"AgentContinueChild"));
        assert!(!names.contains(&"AgentArchiveChild"));
        assert!(!names.contains(&"rsi_control_continue_child"));
        assert!(!names.contains(&"rsi_control_archive_child"));
        for prepared_tool in [
            "rsi_control_manager_prepare_control",
            "rsi_control_manager_commit_prepared_control",
            "rsi_control_manager_get_action",
        ] {
            assert!(names.contains(&prepared_tool));
        }
    }

    #[test]
    fn malformed_call_is_rejected_before_dispatch() {
        let called = Cell::new(false);
        let reply = handle_request(json!({
            "id":1, "method":"tools/call", "params":{"name":"rsi_control_spawn", "arguments":{"kind":"Task"}}
        }), |_, _| { called.set(true); unreachable!() }).unwrap();
        assert!(!called.get());
        assert_eq!(reply["result"]["isError"], true);
        assert!(
            reply["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("invalid_input")
        );
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
}
