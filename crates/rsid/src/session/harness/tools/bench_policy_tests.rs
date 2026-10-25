//! #3177: the bench tool allowlist (`HarnessToolPolicy::bench`) hides and
//! refuses every tool outside the container set, on the full default registry
//! (where an operator MCP tool, the web tools, memory, the control tools,
//! `schedule_wake` and the codegraph tools would otherwise all be advertised)
//! and on the container registry a bench session really runs on.

use super::{HarnessTool, HarnessToolRegistry, policy::ToolPolicyRuntime};
use crate::session::harness::types::ToolResult;
use rsi_common::harness_tool_policy::{
    BENCH_ENABLED_TOOLS, HarnessToolPolicy, TOOL_POLICY_DENIED, WebAccessMode,
};
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Tools a bench session must never advertise or run, by the surfaces the
/// Issue names: the MCP bridge (`mcp__<server>__<tool>`), the web tools, memory,
/// the control tools, `schedule_wake` and the codegraph tools; plus the file
/// tools and git, which are host-only.
const FORBIDDEN: &[&str] = &[
    "mcp__docs__echo",
    "mcp__fs__read_file",
    "web_search",
    "web_fetch",
    "memory_search",
    "rsi_control_status",
    "rsi_control_spawn",
    "schedule_wake",
    "codegraph_search",
    "codegraph_symbols",
    "read_file",
    "write_file",
    "apply_patch",
    "git",
];

struct Fake {
    name: &'static str,
    runs: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl HarnessTool for Fake {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "a tool the bench allowlist must hide"
    }
    fn parameters_json(&self) -> &str {
        "{}"
    }
    async fn execute(&self, _: serde_json::Value, _: &Path) -> ToolResult {
        self.runs.fetch_add(1, Ordering::SeqCst);
        ToolResult {
            success: true,
            output: "ran".into(),
            error_msg: None,
        }
    }
}

fn names(registry: &HarnessToolRegistry) -> BTreeSet<String> {
    registry.specs().into_iter().map(|spec| spec.name).collect()
}

fn allowed() -> BTreeSet<String> {
    BENCH_ENABLED_TOOLS
        .iter()
        .map(|name| (*name).to_string())
        .collect()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test]
async fn the_bench_allowlist_neither_advertises_nor_runs_a_non_container_tool() {
    let mut registry = HarnessToolRegistry::default_tools_with_execution_scratch(
        None, None, None, None, None, None, None, None, None, true, None,
    );
    let runs = Arc::new(AtomicUsize::new(0));
    for name in FORBIDDEN {
        // A tool the default registry already holds is the real one; the rest
        // are stand-ins for the MCP bridge, codegraph and control tools.
        let _ = registry.try_register(Arc::new(Fake {
            name,
            runs: Arc::clone(&runs),
        }));
    }
    // Before the policy every forbidden tool is advertised, so the test would
    // fail if the allowlist did nothing.
    let before = names(&registry);
    for name in FORBIDDEN {
        assert!(before.contains(*name), "`{name}` is not registered");
    }

    registry.set_policy(Arc::new(ToolPolicyRuntime::new(HarnessToolPolicy::bench())));
    let advertised = names(&registry);
    assert!(
        advertised.is_subset(&allowed()),
        "advertised outside the allowlist: {:?}",
        advertised.difference(&allowed()).collect::<Vec<_>>()
    );
    for name in BENCH_ENABLED_TOOLS {
        assert!(advertised.contains(name), "`{name}` is not advertised");
    }

    let dir = Path::new(".");
    for name in FORBIDDEN {
        let result = registry.execute(name, serde_json::json!({}), dir).await;
        assert!(!result.success, "`{name}` ran");
        assert!(
            result
                .error_msg
                .as_deref()
                .is_some_and(|msg| msg.contains(TOOL_POLICY_DENIED)),
            "`{name}`: {result:?}"
        );
    }
    assert_eq!(runs.load(Ordering::SeqCst), 0, "a forbidden tool executed");
    assert_eq!(
        HarnessToolPolicy::bench().web_mode(),
        WebAccessMode::Disabled
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test]
async fn the_container_registry_under_the_bench_policy_is_exactly_the_allowlist() {
    let target = rsi_common::exec_target::ContainerExecTarget {
        docker_host: "ssh://arch-laptop".into(),
        container: "tb2-main-1".into(),
        workdir: "/app".into(),
        user: None,
        compose_project: "tb2-trial".into(),
        pinned_digests: Vec::new(),
        per_call_ceiling_secs: 900,
    };
    let mut registry = HarnessToolRegistry::container_target_tools(
        &target,
        None,
        None,
        None,
        super::exec::default_process_registry(),
    );
    registry.set_policy(Arc::new(ToolPolicyRuntime::new(HarnessToolPolicy::bench())));
    assert_eq!(names(&registry), allowed());
    for name in FORBIDDEN {
        let result = registry
            .execute(name, serde_json::json!({}), Path::new("."))
            .await;
        assert!(!result.success, "`{name}` ran on the container registry");
    }
}
