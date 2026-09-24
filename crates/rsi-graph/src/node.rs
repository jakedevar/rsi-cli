use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::context::ContextRegistry;
use crate::data::{DataSchema, NodeData, Value};
use crate::error::GraphError;
use crate::hook::HookRegistry;
use crate::state::{ScopeId, StateStore};

/// Unique identifier for a node within a graph.
#[derive(Clone, Debug, Hash, Eq, PartialEq, Serialize, Deserialize)]
pub struct NodeId(String);

impl NodeId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Execution context passed to a node during [`Node::execute`].
///
/// Kept extensible — future epics will add state tracking (T1-E2) and hooks
/// (T1-E4).
pub struct NodeContext {
    pub(crate) node_id: NodeId,
    pub(crate) tags: Vec<String>,
    pub(crate) state_scope: Option<ScopeId>,
    pub(crate) hooks: Option<Arc<HookRegistry>>,
    pub(crate) context_registry: Option<Arc<ContextRegistry>>,
}

impl NodeContext {
    /// Create a new context for a given node.
    ///
    /// Only used in tests — production code constructs `NodeContext` via struct
    /// literal in the executor.
    #[cfg(test)]
    pub(crate) fn new(node_id: NodeId, tags: Vec<String>) -> Self {
        Self {
            node_id,
            tags,
            state_scope: None,
            hooks: None,
            context_registry: None,
        }
    }

    /// The ID of the node currently executing.
    pub fn node_id(&self) -> &NodeId {
        &self.node_id
    }

    /// Tags associated with the current node.
    pub fn tags(&self) -> &[String] {
        &self.tags
    }

    /// Access the hook registry, if one has been attached by the executor.
    pub fn hooks(&self) -> Option<&HookRegistry> {
        self.hooks.as_deref()
    }

    /// Access the context registry, if one has been attached by the executor.
    pub fn context_registry(&self) -> Option<&ContextRegistry> {
        self.context_registry.as_deref()
    }

    /// Read a value from this node's state scope, walking up through imports.
    pub fn get_state<'a>(&self, store: &'a StateStore, key: &str) -> Option<&'a Value> {
        self.state_scope
            .as_ref()
            .and_then(|scope| store.get(scope, key))
    }

    /// Write a value to this node's state scope.
    pub fn set_state(
        &self,
        store: &mut StateStore,
        key: &str,
        value: Value,
    ) -> Result<(), GraphError> {
        match &self.state_scope {
            Some(scope) => store.set(scope, key, value),
            None => Err(GraphError::StateError("no state scope".to_string())),
        }
    }

    /// Write a value to a specific target scope (escape hatch).
    pub fn set_state_at(
        &self,
        store: &mut StateStore,
        target: &ScopeId,
        key: &str,
        value: Value,
    ) -> Result<(), GraphError> {
        match &self.state_scope {
            Some(scope) => store.set_at(scope, target, key, value),
            None => Err(GraphError::StateError("no state scope".to_string())),
        }
    }

    /// Read a value and convert it via `TryFrom<Value>`.
    pub fn get_state_as<T: TryFrom<Value, Error = GraphError>>(
        &self,
        store: &StateStore,
        key: &str,
    ) -> Result<T, GraphError> {
        match &self.state_scope {
            Some(scope) => store.get_as(scope, key),
            None => Err(GraphError::StateError("no state scope".to_string())),
        }
    }
}

/// A synchronous, composable unit of work within a graph.
pub trait Node: Send + Sync {
    /// The unique identifier for this node.
    fn id(&self) -> &NodeId;

    /// Human-readable name for display and logging.
    fn name(&self) -> &str;

    /// Tags for categorization and filtering.
    fn tags(&self) -> &[String] {
        &[]
    }

    /// Optional schema declaring expected inputs and outputs.
    fn schema(&self) -> Option<&DataSchema> {
        None
    }

    /// Execute this node with the given input data and context.
    fn execute(&self, input: NodeData, ctx: &mut NodeContext) -> Result<NodeData, GraphError>;

    /// Optional metadata for provenance tracking and decompilation.
    /// Returns a serialized representation of the node's definition, if available.
    fn metadata(&self) -> Option<serde_json::Value> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trivial node implementation that passes input through unchanged.
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

    #[test]
    fn passthrough_node_returns_input() {
        let node = PassthroughNode::new("test-1");
        assert_eq!(node.id().as_str(), "test-1");
        assert_eq!(node.name(), "passthrough");
        assert!(node.tags().is_empty());
        assert!(node.schema().is_none());

        let mut input = NodeData::new();
        input.insert("key", crate::data::Value::String("value".into()));

        let mut ctx = NodeContext::new(node.id().clone(), vec![]);
        let output = node.execute(input.clone(), &mut ctx).unwrap();
        assert_eq!(output, input);
    }

    #[test]
    fn node_id_display() {
        let id = NodeId::new("my-node");
        assert_eq!(id.to_string(), "my-node");
        assert_eq!(id.as_str(), "my-node");
    }

    #[test]
    fn node_context_accessors() {
        let id = NodeId::new("ctx-node");
        let ctx = NodeContext::new(id.clone(), vec!["tag1".into(), "tag2".into()]);
        assert_eq!(ctx.node_id(), &id);
        assert_eq!(ctx.tags(), &["tag1", "tag2"]);
    }
}
