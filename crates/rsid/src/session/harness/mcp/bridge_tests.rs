use crate::session::harness::tools::HarnessTool;
use crate::session::harness::tools::policy::ToolPolicyRuntime;
use crate::store::Store;
use crate::vault::{VaultHandleBuilder, VaultSettings};
use rsi_common::harness_tool_policy::HarnessToolPolicy;
use rsi_common::mcp::McpServerDefinition;
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

use super::McpBridge;

const TEST_SERVER: &str = r#"
import argparse
import json
import os
import sys
import time

parser = argparse.ArgumentParser()
parser.add_argument("--mode", required=True)
parser.add_argument("--pid-file", required=True)
parser.add_argument("--count", type=int, default=1)
args = parser.parse_args()

with open(args.pid_file, "w") as file:
    file.write(str(os.getpid()))

if args.mode == "hang_initialize":
    while True:
        time.sleep(1)

def send(identifier, result):
    sys.stdout.write(json.dumps({
        "jsonrpc": "2.0",
        "id": identifier,
        "result": result,
    }, separators=(",", ":")) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    identifier = message.get("id")
    if method == "initialize":
        send(identifier, {
            "protocolVersion": message["params"]["protocolVersion"],
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "rsi-bridge-test", "version": "1.0"},
        })
    elif method == "notifications/initialized":
        continue
    elif method == "tools/list":
        if args.mode == "edge":
            tools = [
                {"name": "shell", "inputSchema": {"type": "object"}},
                {"name": "shell", "inputSchema": {"type": "object"}},
                {"name": "invalid name", "inputSchema": {"type": "object"}},
                {"name": "large_schema", "inputSchema": {
                    "type": "object", "padding": "x" * 70000,
                }},
                {"name": "deep_schema", "inputSchema": json.loads(
                    '{"value":' * 20 + '"x"' + '}' * 20
                )},
            ]
        elif args.mode == "many":
            tools = [{
                "name": f"tool-{index}",
                "inputSchema": {"type": "object"},
            } for index in range(args.count)]
        else:
            tools = [{"name": "echo", "inputSchema": {"type": "object"}}]
        send(identifier, {"tools": tools})
    elif method == "tools/call":
        if args.mode == "echo_env":
            text = json.dumps(dict(os.environ), sort_keys=True)
        elif args.mode == "secret_keys":
            secret = os.environ.get("MCP_DOCS_TOKEN", "")
            send(identifier, {
                "content": [],
                "structuredContent": {
                    secret: "outer",
                    "nested": [{secret: "inner"}],
                },
                "isError": False,
            })
            continue
        else:
            text = "called"
        send(identifier, {
            "content": [{"type": "text", "text": text}],
            "structuredContent": {"result": text},
            "isError": False,
        })
"#;

const SECRET_CANARY: &str = "mcp-bridge-secret-canary-788";

fn definition(
    id: &str,
    mode: &str,
    count: usize,
    temp: &TempDir,
    secret: bool,
) -> McpServerDefinition {
    let pid_file = temp
        .path()
        .join(format!("{id}.pid"))
        .to_string_lossy()
        .into_owned();
    McpServerDefinition {
        id: id.to_owned(),
        command: "/usr/bin/python3".into(),
        args: vec![
            "-c".into(),
            TEST_SERVER.into(),
            "--mode".into(),
            mode.into(),
            "--pid-file".into(),
            pid_file,
            "--count".into(),
            count.to_string(),
        ],
        secret_env_names: if secret {
            vec!["MCP_DOCS_TOKEN".into()]
        } else {
            Vec::new()
        },
        working_dir: None,
        enabled: true,
    }
}

async fn world() -> (
    TempDir,
    Arc<tokio::sync::Mutex<Store>>,
    crate::vault::VaultHandle,
) {
    let temp = TempDir::new().expect("temp dir");
    let store = Arc::new(tokio::sync::Mutex::new(
        Store::open_in_memory().expect("store"),
    ));
    let vault = VaultHandleBuilder::new(Arc::new(VaultSettings::default()))
        .dir(temp.path().join("vault"))
        .open()
        .expect("vault");
    (temp, store, vault)
}

async fn configure(store: &Arc<tokio::sync::Mutex<Store>>, definition: &McpServerDefinition) {
    let store = store.lock().await;
    let raw = serde_json::to_string(definition).expect("definition json");
    store
        .set_daemon_setting(&format!("mcp.server.{}", definition.id), &raw)
        .expect("definition setting");
}

async fn built_bridge(
    store: &Arc<tokio::sync::Mutex<Store>>,
    vault: &crate::vault::VaultHandle,
) -> McpBridge {
    McpBridge::build(store, vault).await
}

fn has_event(bridge: &McpBridge, subtype: &str) -> bool {
    bridge
        .events_json()
        .iter()
        .any(|event| event.get("subtype").and_then(Value::as_str) == Some(subtype))
}

fn pid(temp: &TempDir, id: &str) -> i32 {
    std::fs::read_to_string(temp.path().join(format!("{id}.pid")))
        .expect("pid file")
        .trim()
        .parse()
        .expect("pid")
}

fn process_alive(pid: i32) -> bool {
    Path::new("/proc").join(pid.to_string()).exists()
}

struct StubTool(String);

#[async_trait::async_trait]
impl HarnessTool for StubTool {
    fn name(&self) -> &str {
        &self.0
    }
    fn description(&self) -> &str {
        "stub"
    }
    fn parameters_json(&self) -> &str {
        "{}"
    }
    async fn execute(
        &self,
        _: serde_json::Value,
        _: &Path,
    ) -> crate::session::harness::types::ToolResult {
        unreachable!()
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
#[tokio::test]
async fn bridge_exposes_calls_and_redacts_the_vault_secret() {
    temp_env::async_with_vars(
        [("RSI_TEST_SECRET_SENTINEL", Some("parent-secret-sentinel"))],
        async {
            let (temp, store, vault) = world().await;
            vault.set_mcp("docs", SECRET_CANARY).expect("secret");
            configure(&store, &definition("docs", "echo_env", 1, &temp, true)).await;
            let mut bridge = built_bridge(&store, &vault).await;
            let mut registry = crate::session::harness::tools::HarnessToolRegistry::new();
            bridge.register(&mut registry);
            let events = bridge.events_json();

            let specs = registry.specs();
            let spec = specs
                .iter()
                .find(|spec| spec.name == "mcp__docs__echo")
                .expect("mcp spec");
            assert!(spec.description.starts_with("External MCP tool."));
            assert!(
                serde_json::to_string(&specs)
                    .expect("specs json")
                    .contains("mcp__docs__echo")
            );
            for text in [
                serde_json::to_string(&specs).expect("specs json"),
                serde_json::to_string(&events).expect("event json"),
            ] {
                assert!(!text.contains(SECRET_CANARY));
                assert!(!text.contains("parent-secret-sentinel"));
            }

            let cancel = CancellationToken::new();
            let result = registry
                .execute_cancellable("mcp__docs__echo", json!({}), Path::new("/tmp"), &cancel)
                .await;
            assert!(result.success, "{result:?}");
            assert!(result.output.contains("MCP_DOCS_TOKEN"));
            assert!(result.output.contains("<redacted>"));
            assert!(!result.output.contains(SECRET_CANARY));
            assert!(!result.output.contains("parent-secret-sentinel"));

            let child_pid = pid(&temp, "docs");
            registry.shutdown_processes().await;
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            assert!(!process_alive(child_pid));
        },
    )
    .await;
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
#[tokio::test]
async fn bridge_redacts_secret_object_keys_in_tool_output() {
    let (temp, store, vault) = world().await;
    vault.set_mcp("docs", SECRET_CANARY).expect("secret");
    configure(&store, &definition("docs", "secret_keys", 1, &temp, true)).await;
    let mut bridge = built_bridge(&store, &vault).await;
    let mut registry = crate::session::harness::tools::HarnessToolRegistry::new();
    bridge.register(&mut registry);

    let cancel = CancellationToken::new();
    let result = registry
        .execute_cancellable("mcp__docs__echo", json!({}), Path::new("/tmp"), &cancel)
        .await;
    assert!(result.success, "{result:?}");
    assert!(result.output.contains("<redacted>"));
    assert!(result.output.contains("nested"));
    assert!(!result.output.contains(SECRET_CANARY));

    registry.shutdown_processes().await;
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
#[tokio::test]
async fn mcp_tools_at_or_below_the_threshold_are_advertised_directly() {
    let (temp, store, vault) = world().await;
    configure(&store, &definition("docs", "basic", 1, &temp, false)).await;
    let mut bridge = built_bridge(&store, &vault).await;
    let mut registry = crate::session::harness::tools::HarnessToolRegistry::new();
    bridge.register_with_threshold(&mut registry, 1);
    let names: Vec<_> = registry.specs().into_iter().map(|spec| spec.name).collect();
    assert_eq!(names, vec!["mcp__docs__echo"]);
    registry.shutdown_processes().await;
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
#[tokio::test]
async fn zero_threshold_defers_one_tool_and_reveals_it_on_search() {
    let (temp, store, vault) = world().await;
    configure(&store, &definition("docs", "basic", 1, &temp, false)).await;
    let mut bridge = built_bridge(&store, &vault).await;
    let mut registry = crate::session::harness::tools::HarnessToolRegistry::new();
    bridge.register_with_threshold(&mut registry, 0);
    let names: Vec<_> = registry.specs().into_iter().map(|spec| spec.name).collect();
    assert_eq!(names, vec!["tool_search"]);

    let cancel = CancellationToken::new();
    let refused = registry
        .execute_cancellable("mcp__docs__echo", json!({}), Path::new("/tmp"), &cancel)
        .await;
    assert!(!refused.success);
    assert_eq!(
        refused.error_msg.as_deref(),
        Some("mcp_tool_not_revealed: use tool_search first")
    );

    let search = registry
        .execute_cancellable(
            "tool_search",
            json!({"query": "echo"}),
            Path::new("/tmp"),
            &cancel,
        )
        .await;
    assert!(search.success, "{search:?}");
    let output: Value = serde_json::from_str(&search.output).expect("search json");
    assert_eq!(output["omitted"], 0);
    assert_eq!(
        output["results"][0]["name"].as_str(),
        Some("mcp__docs__echo")
    );

    let revealed = registry
        .execute_cancellable("mcp__docs__echo", json!({}), Path::new("/tmp"), &cancel)
        .await;
    assert!(revealed.success, "{revealed:?}");
    assert_eq!(revealed.output, "called");
    registry.shutdown_processes().await;
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
#[tokio::test]
async fn deferred_search_is_ranked_bounded_and_stable() {
    let (temp, store, vault) = world().await;
    configure(&store, &definition("docs", "many", 40, &temp, false)).await;
    let mut bridge = built_bridge(&store, &vault).await;
    let mut registry = crate::session::harness::tools::HarnessToolRegistry::new();
    bridge.register_with_threshold(&mut registry, 32);
    let names: Vec<_> = registry.specs().into_iter().map(|spec| spec.name).collect();
    assert_eq!(names, vec!["tool_search"]);

    let cancel = CancellationToken::new();
    let result = registry
        .execute_cancellable(
            "tool_search",
            json!({"query": "tool"}),
            Path::new("/tmp"),
            &cancel,
        )
        .await;
    assert!(result.success, "{result:?}");
    let output: Value = serde_json::from_str(&result.output).expect("search json");
    assert_eq!(output["results"].as_array().map(Vec::len), Some(8));
    assert_eq!(output["omitted"], 32);
    assert!(
        output["results"]
            .as_array()
            .expect("results")
            .iter()
            .all(|entry| entry["server_id"].as_str() == Some("docs"))
    );
    let first_names: Vec<_> = output["results"]
        .as_array()
        .expect("results")
        .iter()
        .map(|entry| entry["name"].as_str().expect("name"))
        .take(2)
        .collect();
    assert_eq!(first_names, vec!["mcp__docs__tool-0", "mcp__docs__tool-1"]);
    registry.shutdown_processes().await;
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
#[tokio::test]
async fn a_revealed_tool_still_cannot_bypass_tool_policy() {
    let (temp, store, vault) = world().await;
    configure(&store, &definition("docs", "basic", 1, &temp, false)).await;
    let mut bridge = built_bridge(&store, &vault).await;
    let mut registry = crate::session::harness::tools::HarnessToolRegistry::new();
    bridge.register_with_threshold(&mut registry, 0);
    let cancel = CancellationToken::new();
    let search = registry
        .execute_cancellable(
            "tool_search",
            json!({"query": "echo"}),
            Path::new("/tmp"),
            &cancel,
        )
        .await;
    assert!(search.success, "{search:?}");
    registry.set_policy(Arc::new(ToolPolicyRuntime::new(HarnessToolPolicy {
        denied_tools: vec!["mcp__docs__echo".into()],
        ..HarnessToolPolicy::default()
    })));
    let result = registry
        .execute_cancellable("mcp__docs__echo", json!({}), Path::new("/tmp"), &cancel)
        .await;
    assert!(!result.success);
    assert_eq!(
        result.error_msg.as_deref(),
        Some("tool_policy_denied: mcp__docs__echo is denied by this session's tool policy")
    );
    registry.shutdown_processes().await;
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
#[tokio::test]
async fn a_policy_denied_tool_is_absent_from_catalog_and_search() {
    let (temp, store, vault) = world().await;
    configure(&store, &definition("docs", "many", 2, &temp, false)).await;
    let mut bridge = built_bridge(&store, &vault).await;
    let mut registry = crate::session::harness::tools::HarnessToolRegistry::new();
    registry.set_policy(Arc::new(ToolPolicyRuntime::new(HarnessToolPolicy {
        denied_tools: vec!["mcp__docs__tool-0".into()],
        ..HarnessToolPolicy::default()
    })));
    bridge.register_with_threshold(&mut registry, 0);
    assert_eq!(
        registry
            .specs()
            .iter()
            .filter(|spec| spec.name.starts_with("mcp__"))
            .count(),
        0
    );
    let cancel = CancellationToken::new();
    let search = registry
        .execute_cancellable(
            "tool_search",
            json!({"query": "tool"}),
            Path::new("/tmp"),
            &cancel,
        )
        .await;
    assert!(search.success, "{search:?}");
    let output: Value = serde_json::from_str(&search.output).expect("search json");
    assert_eq!(
        output["results"]
            .as_array()
            .expect("results")
            .iter()
            .map(|entry| entry["name"].as_str().expect("name"))
            .collect::<Vec<_>>(),
        vec!["mcp__docs__tool-1"]
    );
    registry.shutdown_processes().await;
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
#[tokio::test]
async fn policy_denies_the_mcp_tool_in_catalog_and_call() {
    let (temp, store, vault) = world().await;
    configure(&store, &definition("docs", "basic", 1, &temp, false)).await;
    let mut bridge = built_bridge(&store, &vault).await;
    let mut registry = crate::session::harness::tools::HarnessToolRegistry::new();
    bridge.register(&mut registry);
    registry.set_policy(Arc::new(ToolPolicyRuntime::new(HarnessToolPolicy {
        denied_tools: vec!["mcp__docs__echo".into()],
        ..HarnessToolPolicy::default()
    })));

    assert!(
        registry
            .specs()
            .iter()
            .all(|spec| spec.name != "mcp__docs__echo")
    );
    let cancel = CancellationToken::new();
    let result = registry
        .execute_cancellable("mcp__docs__echo", json!({}), Path::new("/tmp"), &cancel)
        .await;
    assert!(!result.success);
    assert_eq!(
        result.error_msg.as_deref(),
        Some("tool_policy_denied: mcp__docs__echo is denied by this session's tool policy")
    );
    registry.shutdown_processes().await;
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
#[tokio::test]
async fn an_unavailable_server_is_nonfatal_and_visible() {
    let (temp, store, vault) = world().await;
    configure(
        &store,
        &definition("hangs", "hang_initialize", 1, &temp, false),
    )
    .await;
    let mut bridge = built_bridge(&store, &vault).await;
    let mut registry = crate::session::harness::tools::HarnessToolRegistry::new();
    bridge.register(&mut registry);
    assert!(registry.specs().is_empty());
    assert!(has_event(&bridge, "mcp_server_unavailable"));
    assert_eq!(
        bridge.events_json()[0]["data"]["server_id"].as_str(),
        Some("hangs")
    );
    let child_pid = pid(&temp, "hangs");
    assert!(!process_alive(child_pid));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
#[tokio::test]
async fn invalid_duplicate_and_builtin_like_names_are_skipped_or_namespaced() {
    let (temp, store, vault) = world().await;
    configure(&store, &definition("docs", "edge", 1, &temp, false)).await;
    let mut bridge = built_bridge(&store, &vault).await;
    let mut registry = crate::session::harness::tools::HarnessToolRegistry::new();
    registry
        .try_register(Arc::new(StubTool("mcp__docs__shell".into())))
        .expect("stub");
    bridge.register(&mut registry);

    let names: Vec<_> = registry.specs().into_iter().map(|spec| spec.name).collect();
    assert_eq!(names, vec!["mcp__docs__shell"]);
    let events = bridge.events_json();
    let reasons: Vec<_> = events
        .iter()
        .filter_map(|event| event["data"]["reason"].as_str())
        .collect();
    assert_eq!(
        reasons,
        vec![
            "duplicate_tool_name",
            "invalid_tool_name",
            "invalid_input_schema",
            "invalid_input_schema",
            "duplicate_registry_name",
        ]
    );
    let duplicate_event = events
        .iter()
        .find(|event| event["data"]["reason"].as_str() == Some("duplicate_registry_name"))
        .expect("duplicate registry event");
    assert_eq!(
        duplicate_event["data"]["tool_name"].as_str(),
        Some("mcp__docs__shell")
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
#[tokio::test]
async fn the_session_tool_limit_skips_later_servers_once() {
    let (temp, store, vault) = world().await;
    configure(&store, &definition("first", "many", 200, &temp, false)).await;
    configure(&store, &definition("second", "many", 200, &temp, false)).await;
    let mut bridge = built_bridge(&store, &vault).await;
    let mut registry = crate::session::harness::tools::HarnessToolRegistry::new();
    bridge.register_with_threshold(&mut registry, 256);
    assert_eq!(registry.specs().len(), 256);
    assert!(
        registry
            .specs()
            .iter()
            .all(|spec| spec.name.starts_with("mcp__"))
    );
    let events = bridge.events_json();
    let limit_events: Vec<_> = events
        .iter()
        .filter(|event| event.get("subtype").and_then(Value::as_str) == Some("mcp_tool_limit"))
        .collect();
    assert_eq!(limit_events.len(), 1);
    assert_eq!(
        limit_events[0]["data"]["server_id"].as_str(),
        Some("second")
    );
    registry.shutdown_processes().await;
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
#[tokio::test]
async fn rebuilding_the_bridge_repeats_the_namespaced_tool() {
    let (temp, store, vault) = world().await;
    configure(&store, &definition("docs", "basic", 1, &temp, false)).await;
    for _ in 0..2 {
        let mut bridge = built_bridge(&store, &vault).await;
        let mut registry = crate::session::harness::tools::HarnessToolRegistry::new();
        bridge.register(&mut registry);
        let names: Vec<_> = registry.specs().into_iter().map(|spec| spec.name).collect();
        assert_eq!(names, vec!["mcp__docs__echo"]);
        registry.shutdown_processes().await;
    }
}
