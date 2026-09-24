//! flywheel-graph: Graph-based multi-agent orchestration engine.
//!
//! This crate provides the primitives for building, executing, and generating
//! directed acyclic graph workflows for multi-agent coordination.

pub mod cache;
pub mod compiler;
pub mod compose;
pub mod context;
pub mod data;
pub mod edge;
pub mod error;
pub mod executor;
pub mod filter;
pub mod format;
pub mod generate;
pub mod hook;
pub mod node;
pub mod retrieval;
pub mod state;
#[cfg(test)]
pub(crate) mod test_nodes;
pub mod topology;
pub mod validate;

pub use compiler::{CompileDiagnostics, CompileError, Severity, compile, decompile};
pub use compose::{ComposedNode, MAX_NESTING_DEPTH};
pub use context::{ContextRegistry, ContextSource, ContextSourceType};
pub use data::{DataSchema, FieldDeclaration, FieldType, NodeData, Value};
pub use edge::{Edge, EdgeId};
pub use error::GraphError;
pub use executor::LinearExecutor;
pub use filter::{FieldFilter, FilterValidation, MergeStrategy};
pub use format::{
    EdgeDef, FilterDef, GRAPH_VIEW_METADATA_KEY, GRAPH_VIEW_SCHEMA_VERSION, GraphViewMetadata,
    GraphViewMetadataError, GraphViewVisualEdge, GraphViewVisualEdgeKind, HookAttachment,
    ModelSettings, NodeDef, NodeType, PortDef, RepeatPolicy, WorkflowDefinition,
};
pub use hook::{
    ConditionExpression, Hook, HookAction, HookContext, HookRegistry, HookStage, Selector,
};
pub use node::{Node, NodeContext, NodeId};
pub use retrieval::{
    RetrievalBackend, RetrievalItem, RetrievalRequest, RetrievalResult, ToolDefinition,
    ToolParameters, ToolProperty, execute_retrieval, generate_retrieval_tools,
};
pub use state::{ScopeId, StateStore};
pub use topology::brainstorm::BrainstormingGraph;
pub use topology::decision::VerticalDecisionGraph;
pub use topology::hub::{HubGraph, RoutingDecision, SpokeConfig};
pub use topology::instructor::{InstructorAssistantGraph, InstructorConfig};
pub use topology::vertical::{AggregateStrategy, VerticalGraph};
pub use topology::{
    DagExecutor, ExecutableGraph, HistoryPolicy, Topology, TopologyFactory, TopologyRegistry,
};
pub use validate::validate_executable_workflow;
