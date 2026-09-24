use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use rsi_graph::*;

// ---------------------------------------------------------------------------
// Test node implementations (integration tests can't use pub(crate) test_nodes)
// ---------------------------------------------------------------------------

struct PassthroughNode {
    id: NodeId,
}

impl PassthroughNode {
    fn new(id: &str) -> Self {
        Self {
            id: NodeId::new(id),
        }
    }
}

impl Node for PassthroughNode {
    fn id(&self) -> &NodeId {
        &self.id
    }
    fn name(&self) -> &str {
        "passthrough"
    }
    fn execute(&self, input: NodeData, _ctx: &mut NodeContext) -> Result<NodeData, GraphError> {
        Ok(input)
    }
}

struct AppendFieldNode {
    id: NodeId,
    field_name: String,
    field_value: Value,
}

impl AppendFieldNode {
    fn new(id: &str, field_name: &str, value: Value) -> Self {
        Self {
            id: NodeId::new(id),
            field_name: field_name.to_string(),
            field_value: value,
        }
    }
}

impl Node for AppendFieldNode {
    fn id(&self) -> &NodeId {
        &self.id
    }
    fn name(&self) -> &str {
        "append"
    }
    fn execute(&self, mut input: NodeData, _ctx: &mut NodeContext) -> Result<NodeData, GraphError> {
        input.insert(self.field_name.clone(), self.field_value.clone());
        Ok(input)
    }
}

struct FailingNode {
    id: NodeId,
}

impl FailingNode {
    fn new(id: &str) -> Self {
        Self {
            id: NodeId::new(id),
        }
    }
}

impl Node for FailingNode {
    fn id(&self) -> &NodeId {
        &self.id
    }
    fn name(&self) -> &str {
        "failing"
    }
    fn execute(&self, _input: NodeData, _ctx: &mut NodeContext) -> Result<NodeData, GraphError> {
        Err(GraphError::ExecutionFailed(
            "intentional failure".to_string(),
        ))
    }
}

// ---------------------------------------------------------------------------
// Test hooks
// ---------------------------------------------------------------------------

/// Hook that injects a field into the data at BeforeExecute.
struct InjectFieldHook {
    field: String,
    value: Value,
}

impl Hook for InjectFieldHook {
    fn stage(&self) -> HookStage {
        HookStage::BeforeExecute
    }
    fn selector(&self) -> Selector {
        Selector::All
    }
    fn execute(&self, ctx: &HookContext) -> HookAction {
        let mut data = ctx.data.clone();
        data.insert(self.field.clone(), self.value.clone());
        HookAction::ModifyData(data)
    }
}

/// Hook that counts how many times AfterExecute fires.
struct CountingHook {
    counter: Arc<AtomicUsize>,
}

impl Hook for CountingHook {
    fn stage(&self) -> HookStage {
        HookStage::AfterExecute
    }
    fn selector(&self) -> Selector {
        Selector::All
    }
    fn execute(&self, _ctx: &HookContext) -> HookAction {
        self.counter.fetch_add(1, Ordering::SeqCst);
        HookAction::Continue
    }
}

/// Hook that records node IDs on error.
struct ErrorRecorderHook {
    log: Arc<Mutex<Vec<String>>>,
}

impl Hook for ErrorRecorderHook {
    fn stage(&self) -> HookStage {
        HookStage::OnError
    }
    fn selector(&self) -> Selector {
        Selector::All
    }
    fn execute(&self, ctx: &HookContext) -> HookAction {
        let mut log = self.log.lock().unwrap();
        log.push(ctx.node_id.as_str().to_string());
        // Skip = "error handled, swallow and continue pipeline".
        HookAction::Skip
    }
}

// ---------------------------------------------------------------------------
// Test 1: 3-node pipeline with edge filter
// ---------------------------------------------------------------------------
#[test]
fn three_node_pipeline_with_edge_filter() {
    // Node A: writes {x, y, z}
    // Edge A→B: Include(["x", "z"]) — filters out y
    // Node B: receives {x, z}, appends "enriched" field
    // Node C: receives {x, z, enriched}

    let node_a = AppendFieldNode::new("a", "unused", Value::Null);
    let node_b = AppendFieldNode::new("b", "enriched", Value::Bool(true));
    let node_c = PassthroughNode::new("c");

    let mut edge_ab = Edge::new("e_ab", "a", "b");
    edge_ab.filter = Some(FieldFilter::Include(vec!["x".into(), "z".into()]));

    let edge_bc = Edge::new("e_bc", "b", "c");

    let mut exec = LinearExecutor::new()
        .with_node(Box::new(node_a))
        .with_node(Box::new(node_b))
        .with_node(Box::new(node_c))
        .with_edge(edge_ab)
        .with_edge(edge_bc);

    let mut input = NodeData::new();
    input.insert("x", Value::Number(1.0));
    input.insert("y", Value::Number(2.0));
    input.insert("z", Value::Number(3.0));

    let output = exec.execute(input).unwrap();

    // y was filtered out by edge A→B
    assert!(output.get("y").is_none(), "y should have been filtered out");
    // x and z survived the filter
    assert_eq!(output.get("x"), Some(&Value::Number(1.0)));
    assert_eq!(output.get("z"), Some(&Value::Number(3.0)));
    // Node B appended "enriched"
    assert_eq!(output.get("enriched"), Some(&Value::Bool(true)));
    // Node A's "unused" was filtered out along with y
    assert!(output.get("unused").is_none());
}

// ---------------------------------------------------------------------------
// Test 2: BeforeExecute hook injects a field
// ---------------------------------------------------------------------------
#[test]
fn before_execute_hook_injects_field() {
    let node = PassthroughNode::new("n1");

    let mut exec = LinearExecutor::new()
        .with_node(Box::new(node))
        .with_hook(Box::new(InjectFieldHook {
            field: "debug".into(),
            value: Value::Bool(true),
        }));

    let input = NodeData::new();
    let output = exec.execute(input).unwrap();

    assert_eq!(
        output.get("debug"),
        Some(&Value::Bool(true)),
        "hook should have injected debug: true"
    );
}

// ---------------------------------------------------------------------------
// Test 3: AfterExecute hook fires for each node
// ---------------------------------------------------------------------------
#[test]
fn after_execute_hook_fires_for_each_node() {
    let counter = Arc::new(AtomicUsize::new(0));

    let mut exec = LinearExecutor::new()
        .with_node(Box::new(PassthroughNode::new("n1")))
        .with_node(Box::new(PassthroughNode::new("n2")))
        .with_node(Box::new(PassthroughNode::new("n3")))
        .with_hook(Box::new(CountingHook {
            counter: counter.clone(),
        }));

    exec.execute(NodeData::new()).unwrap();

    assert_eq!(
        counter.load(Ordering::SeqCst),
        3,
        "AfterExecute should fire once per node"
    );
}

// ---------------------------------------------------------------------------
// Test 4: OnError hook fires on node failure
// ---------------------------------------------------------------------------
#[test]
fn on_error_hook_fires_on_failure() {
    let error_log = Arc::new(Mutex::new(Vec::<String>::new()));

    let mut exec = LinearExecutor::new()
        .with_node(Box::new(PassthroughNode::new("ok1")))
        .with_node(Box::new(FailingNode::new("fail1")))
        .with_node(Box::new(PassthroughNode::new("ok2")))
        .with_hook(Box::new(ErrorRecorderHook {
            log: error_log.clone(),
        }));

    // The OnError hook returns Continue, so execution should not abort.
    let result = exec.execute(NodeData::new());
    assert!(result.is_ok(), "error should be swallowed by OnError hook");

    let log = error_log.lock().unwrap();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0], "fail1");
}

// ---------------------------------------------------------------------------
// Test 5: Without OnError hook, failure propagates
// ---------------------------------------------------------------------------
#[test]
fn failure_propagates_without_on_error_hook() {
    let mut exec = LinearExecutor::new()
        .with_node(Box::new(PassthroughNode::new("ok1")))
        .with_node(Box::new(FailingNode::new("fail1")))
        .with_node(Box::new(PassthroughNode::new("ok2")));

    let result = exec.execute(NodeData::new());
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("intentional failure")
    );
}
