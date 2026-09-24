use anyhow::Result;
use rsi_graph::compiler;
use rsi_graph::data::NodeData;
use rsi_graph::error::GraphError;
use rsi_graph::format::WorkflowDefinition;
use rsi_graph::hook::{Hook, HookAction, HookContext, HookStage, Selector};
use rsi_graph::topology::DagExecutor;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Per-node execution state emitted while a workflow runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeExecutionState {
    Running,
    Succeeded,
    Failed,
}

/// Node execution update forwarded to the daemon execution manager.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeExecutionUpdate {
    pub node_id: String,
    pub state: NodeExecutionState,
    pub output_preview: Option<String>,
}

/// Optional live hooks used while executing a workflow.
#[derive(Clone, Default)]
pub struct ExecutionHooks {
    pub cancel_flag: Option<Arc<AtomicBool>>,
    pub on_node_update: Option<Arc<dyn Fn(NodeExecutionUpdate) + Send + Sync>>,
}

/// Result of executing a workflow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionResult {
    pub success: bool,
    pub output: Option<serde_json::Value>,
    pub error: Option<String>,
}

/// Execute a workflow definition.
/// Compiles it, runs through the DAG executor, and emits node lifecycle callbacks.
pub fn execute_workflow(
    workflow: &WorkflowDefinition,
    input: NodeData,
    hooks: ExecutionHooks,
) -> Result<ExecutionResult> {
    let mut graph = match compiler::compile(workflow) {
        Ok((graph, diag)) => {
            for warning in &diag.warnings {
                tracing::warn!("Compilation warning: {}", warning.message);
            }
            graph
        }
        Err(diag) => {
            let errors: Vec<String> = diag.errors.iter().map(|e| e.message.clone()).collect();
            return Ok(ExecutionResult {
                success: false,
                output: None,
                error: Some(format!("Compilation failed: {}", errors.join(", "))),
            });
        }
    };

    if let Some(cancel_flag) = hooks.cancel_flag {
        graph = graph.with_hook(Box::new(CancellationHook { cancel_flag }));
    }

    if let Some(on_node_update) = hooks.on_node_update {
        graph = graph
            .with_hook(Box::new(NodeLifecycleHook::new(
                HookStage::BeforeExecute,
                on_node_update.clone(),
            )))
            .with_hook(Box::new(NodeLifecycleHook::new(
                HookStage::AfterExecute,
                on_node_update.clone(),
            )))
            .with_hook(Box::new(NodeLifecycleHook::new(
                HookStage::OnError,
                on_node_update,
            )));
    }

    match DagExecutor::execute(&mut graph, input) {
        Ok(output) => Ok(ExecutionResult {
            success: true,
            output: serde_json::to_value(&output).ok(),
            error: None,
        }),
        Err(error) => Ok(ExecutionResult {
            success: false,
            output: None,
            error: Some(error.to_string()),
        }),
    }
}

struct CancellationHook {
    cancel_flag: Arc<AtomicBool>,
}

impl Hook for CancellationHook {
    fn stage(&self) -> HookStage {
        HookStage::BeforeExecute
    }

    fn selector(&self) -> Selector {
        Selector::All
    }

    fn execute(&self, _ctx: &HookContext) -> HookAction {
        if self.cancel_flag.load(Ordering::Relaxed) {
            HookAction::Abort(GraphError::ExecutionFailed(
                "workflow execution interrupted".to_string(),
            ))
        } else {
            HookAction::Continue
        }
    }
}

struct NodeLifecycleHook {
    stage: HookStage,
    on_node_update: Arc<dyn Fn(NodeExecutionUpdate) + Send + Sync>,
}

impl NodeLifecycleHook {
    fn new(
        stage: HookStage,
        on_node_update: Arc<dyn Fn(NodeExecutionUpdate) + Send + Sync>,
    ) -> Self {
        Self {
            stage,
            on_node_update,
        }
    }
}

impl Hook for NodeLifecycleHook {
    fn stage(&self) -> HookStage {
        self.stage
    }

    fn selector(&self) -> Selector {
        Selector::All
    }

    fn execute(&self, ctx: &HookContext) -> HookAction {
        let update = match self.stage {
            HookStage::BeforeExecute => Some(NodeExecutionUpdate {
                node_id: ctx.node_id.to_string(),
                state: NodeExecutionState::Running,
                output_preview: None,
            }),
            HookStage::AfterExecute => Some(NodeExecutionUpdate {
                node_id: ctx.node_id.to_string(),
                state: NodeExecutionState::Succeeded,
                output_preview: preview_node_data(ctx.data),
            }),
            HookStage::OnError => Some(NodeExecutionUpdate {
                node_id: ctx.node_id.to_string(),
                state: NodeExecutionState::Failed,
                output_preview: None,
            }),
            _ => None,
        };

        if let Some(update) = update {
            (self.on_node_update)(update);
        }

        HookAction::Continue
    }
}

fn preview_node_data(data: &NodeData) -> Option<String> {
    let json = serde_json::to_string(data).ok()?;
    const MAX_PREVIEW_CHARS: usize = 120;
    if json.chars().count() > MAX_PREVIEW_CHARS {
        let truncated: String = json.chars().take(MAX_PREVIEW_CHARS).collect();
        Some(format!("{}...", truncated))
    } else {
        Some(json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_graph::format::{EdgeDef, NodeDef, WorkflowDefinition};
    use std::sync::Mutex;

    fn three_node_pipeline() -> WorkflowDefinition {
        WorkflowDefinition::new("test-pipeline")
            .with_node(NodeDef::action("a", "Node A"))
            .with_node(NodeDef::action("b", "Node B"))
            .with_node(NodeDef::action("c", "Node C"))
            .with_edge(EdgeDef::new("a", "b"))
            .with_edge(EdgeDef::new("b", "c"))
    }

    #[test]
    fn execute_simple_three_node_pipeline_succeeds() {
        let workflow = three_node_pipeline();
        let updates = Arc::new(Mutex::new(Vec::<NodeExecutionUpdate>::new()));
        let sink = Arc::clone(&updates);

        let result = execute_workflow(
            &workflow,
            NodeData::new(),
            ExecutionHooks {
                cancel_flag: None,
                on_node_update: Some(Arc::new(move |event| {
                    sink.lock().unwrap().push(event);
                })),
            },
        )
        .unwrap();

        assert!(result.success);
        assert!(result.error.is_none());
        assert!(result.output.is_some());
        assert_eq!(updates.lock().unwrap().len(), 6);
    }

    #[test]
    fn execute_workflow_with_invalid_compilation_returns_error() {
        let workflow = WorkflowDefinition::new("bad")
            .with_node(NodeDef::action("a", "A"))
            .with_edge(EdgeDef::new("a", "nonexistent"));

        let result =
            execute_workflow(&workflow, NodeData::new(), ExecutionHooks::default()).unwrap();

        assert!(!result.success);
        assert!(result.error.is_some());
        assert!(result.error.unwrap().contains("Compilation failed"));
    }

    #[test]
    fn node_lifecycle_updates_cover_all_nodes() {
        let workflow = three_node_pipeline();
        let updates = Arc::new(Mutex::new(Vec::<NodeExecutionUpdate>::new()));
        let sink = Arc::clone(&updates);

        let result = execute_workflow(
            &workflow,
            NodeData::new(),
            ExecutionHooks {
                cancel_flag: None,
                on_node_update: Some(Arc::new(move |event| {
                    sink.lock().unwrap().push(event);
                })),
            },
        )
        .unwrap();

        assert!(result.success);

        let updates = updates.lock().unwrap();
        let running = updates
            .iter()
            .filter(|update| update.state == NodeExecutionState::Running)
            .count();
        let succeeded = updates
            .iter()
            .filter(|update| update.state == NodeExecutionState::Succeeded)
            .count();

        assert_eq!(running, 3);
        assert_eq!(succeeded, 3);
    }

    #[test]
    fn cancellation_aborts_before_node_execution() {
        let workflow = three_node_pipeline();
        let cancel_flag = Arc::new(AtomicBool::new(true));
        let updates = Arc::new(Mutex::new(Vec::<NodeExecutionUpdate>::new()));
        let sink = Arc::clone(&updates);

        let result = execute_workflow(
            &workflow,
            NodeData::new(),
            ExecutionHooks {
                cancel_flag: Some(cancel_flag),
                on_node_update: Some(Arc::new(move |event| {
                    sink.lock().unwrap().push(event);
                })),
            },
        )
        .unwrap();

        assert!(!result.success);
        assert!(
            result
                .error
                .unwrap_or_default()
                .contains("workflow execution interrupted")
        );
        assert!(updates.lock().unwrap().is_empty());
    }
}
