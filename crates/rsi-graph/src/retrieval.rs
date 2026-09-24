//! Active-mode retrieval system for graph nodes.
//!
//! Nodes can call tools to retrieve context on demand from registered
//! [`ContextSource`]s. This module generates tool definitions for LLM-based
//! nodes and executes retrieval requests against pluggable backends.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::context::{ContextRegistry, ContextSourceType};
use crate::data::NodeData;
use crate::error::GraphError;

/// A tool definition that can be exposed to LLM-based nodes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: ToolParameters,
}

/// Parameters for a tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolParameters {
    pub required: Vec<String>,
    pub properties: BTreeMap<String, ToolProperty>,
}

/// A single property within tool parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolProperty {
    pub property_type: String,
    pub description: String,
}

/// A retrieval request from a node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetrievalRequest {
    pub source_name: String,
    pub query: String,
    pub top_k: Option<usize>,
    pub max_tokens: Option<usize>,
}

/// A retrieval result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetrievalResult {
    pub source_name: String,
    pub items: Vec<RetrievalItem>,
    pub total_tokens: Option<usize>,
}

/// A single item returned from a retrieval query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetrievalItem {
    pub content: String,
    pub metadata: NodeData,
    pub relevance_score: Option<f64>,
}

/// Backend trait for retrieval implementations.
/// Different source types will have different backends.
pub trait RetrievalBackend: Send + Sync {
    /// Execute a retrieval request and return matching items.
    fn retrieve(&self, request: &RetrievalRequest) -> Result<RetrievalResult, GraphError>;

    /// The context source type this backend handles.
    fn source_type(&self) -> ContextSourceType;
}

/// Generates tool definitions from a [`ContextRegistry`] for use by LLM nodes.
///
/// Always produces a `list_context_sources` tool. If the registry contains any
/// retrievable sources, also produces a `retrieve_context` tool whose description
/// enumerates the available sources.
pub fn generate_retrieval_tools(registry: &ContextRegistry) -> Vec<ToolDefinition> {
    let mut tools = Vec::new();

    // list_context_sources tool — always present
    tools.push(ToolDefinition {
        name: "list_context_sources".to_string(),
        description: "List available context sources for retrieval".to_string(),
        parameters: ToolParameters {
            required: vec![],
            properties: BTreeMap::new(),
        },
    });

    // retrieve_context tool — only if retrievable sources exist
    let retrievable = registry.list_retrievable();
    if !retrievable.is_empty() {
        let mut props = BTreeMap::new();
        props.insert(
            "source".to_string(),
            ToolProperty {
                property_type: "string".to_string(),
                description: "Name of the context source to query".to_string(),
            },
        );
        props.insert(
            "query".to_string(),
            ToolProperty {
                property_type: "string".to_string(),
                description: "Search query".to_string(),
            },
        );
        props.insert(
            "top_k".to_string(),
            ToolProperty {
                property_type: "integer".to_string(),
                description: "Maximum number of results".to_string(),
            },
        );

        tools.push(ToolDefinition {
            name: "retrieve_context".to_string(),
            description: format!(
                "Retrieve context from a source. Available sources: {}",
                retrievable
                    .iter()
                    .map(|s| format!("{} ({})", s.name, s.description))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            parameters: ToolParameters {
                required: vec!["source".to_string(), "query".to_string()],
                properties: props,
            },
        });
    }

    tools
}

/// Execute a retrieval request against a registry of backends.
///
/// Looks up the named source in the registry, verifies it is retrievable,
/// then dispatches to the first backend whose [`RetrievalBackend::source_type`]
/// matches the source's type (by enum discriminant).
pub fn execute_retrieval(
    request: &RetrievalRequest,
    registry: &ContextRegistry,
    backends: &[Box<dyn RetrievalBackend>],
) -> Result<RetrievalResult, GraphError> {
    let source = registry.get(&request.source_name).ok_or_else(|| {
        GraphError::ExecutionFailed(format!("context source not found: {}", request.source_name))
    })?;

    if !source.retrievable {
        return Err(GraphError::ExecutionFailed(format!(
            "source '{}' is not retrievable",
            request.source_name
        )));
    }

    let backend = backends
        .iter()
        .find(|b| {
            std::mem::discriminant(&b.source_type()) == std::mem::discriminant(&source.source_type)
        })
        .ok_or_else(|| {
            GraphError::ExecutionFailed(format!(
                "no backend for source type {:?}",
                source.source_type
            ))
        })?;

    backend.retrieve(request)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{ContextSource, ContextSourceType};

    fn make_source(name: &str, source_type: ContextSourceType, retrievable: bool) -> ContextSource {
        ContextSource {
            name: name.to_string(),
            description: format!("{name} source"),
            source_type,
            output_schema: None,
            tags: vec![],
            retrievable,
        }
    }

    struct MockMemoryBackend;

    impl RetrievalBackend for MockMemoryBackend {
        fn retrieve(&self, request: &RetrievalRequest) -> Result<RetrievalResult, GraphError> {
            Ok(RetrievalResult {
                source_name: request.source_name.clone(),
                items: vec![RetrievalItem {
                    content: format!("result for: {}", request.query),
                    metadata: NodeData::new(),
                    relevance_score: Some(0.95),
                }],
                total_tokens: Some(100),
            })
        }

        fn source_type(&self) -> ContextSourceType {
            ContextSourceType::Memory
        }
    }

    #[test]
    fn generate_tools_includes_list_and_retrieve() {
        let mut registry = ContextRegistry::new();
        registry.register(make_source("mem", ContextSourceType::Memory, true));

        let tools = generate_retrieval_tools(&registry);
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "list_context_sources");
        assert_eq!(tools[1].name, "retrieve_context");
        assert!(tools[1].description.contains("mem"));
        assert_eq!(tools[1].parameters.required.len(), 2);
        assert!(tools[1].parameters.properties.contains_key("source"));
        assert!(tools[1].parameters.properties.contains_key("query"));
        assert!(tools[1].parameters.properties.contains_key("top_k"));
    }

    #[test]
    fn generate_tools_no_retrievable_only_list() {
        let mut registry = ContextRegistry::new();
        registry.register(make_source(
            "blackboard",
            ContextSourceType::Blackboard,
            false,
        ));

        let tools = generate_retrieval_tools(&registry);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "list_context_sources");
    }

    #[test]
    fn execute_retrieval_with_mock_backend() {
        let mut registry = ContextRegistry::new();
        registry.register(make_source("mem", ContextSourceType::Memory, true));

        let backends: Vec<Box<dyn RetrievalBackend>> = vec![Box::new(MockMemoryBackend)];

        let request = RetrievalRequest {
            source_name: "mem".to_string(),
            query: "test query".to_string(),
            top_k: Some(5),
            max_tokens: None,
        };

        let result = execute_retrieval(&request, &registry, &backends).unwrap();
        assert_eq!(result.source_name, "mem");
        assert_eq!(result.items.len(), 1);
        assert_eq!(result.items[0].content, "result for: test query");
        assert_eq!(result.items[0].relevance_score, Some(0.95));
        assert_eq!(result.total_tokens, Some(100));
    }

    #[test]
    fn execute_retrieval_non_retrievable_source_errors() {
        let mut registry = ContextRegistry::new();
        registry.register(make_source(
            "blackboard",
            ContextSourceType::Blackboard,
            false,
        ));

        let backends: Vec<Box<dyn RetrievalBackend>> = vec![];

        let request = RetrievalRequest {
            source_name: "blackboard".to_string(),
            query: "test".to_string(),
            top_k: None,
            max_tokens: None,
        };

        let err = execute_retrieval(&request, &registry, &backends).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("not retrievable"), "got: {msg}");
    }

    #[test]
    fn execute_retrieval_unknown_source_errors() {
        let registry = ContextRegistry::new();
        let backends: Vec<Box<dyn RetrievalBackend>> = vec![];

        let request = RetrievalRequest {
            source_name: "nonexistent".to_string(),
            query: "test".to_string(),
            top_k: None,
            max_tokens: None,
        };

        let err = execute_retrieval(&request, &registry, &backends).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("not found"), "got: {msg}");
    }

    #[test]
    fn tool_definition_serde_roundtrip() {
        let mut props = BTreeMap::new();
        props.insert(
            "query".to_string(),
            ToolProperty {
                property_type: "string".to_string(),
                description: "The search query".to_string(),
            },
        );

        let tool = ToolDefinition {
            name: "search".to_string(),
            description: "Search for things".to_string(),
            parameters: ToolParameters {
                required: vec!["query".to_string()],
                properties: props,
            },
        };

        let json = serde_json::to_string(&tool).expect("serialize");
        let deserialized: ToolDefinition = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(deserialized.name, "search");
        assert_eq!(deserialized.description, "Search for things");
        assert_eq!(deserialized.parameters.required, vec!["query"]);
        assert!(deserialized.parameters.properties.contains_key("query"));
        let prop = &deserialized.parameters.properties["query"];
        assert_eq!(prop.property_type, "string");
        assert_eq!(prop.description, "The search query");
    }

    #[test]
    fn scoped_registry_isolates_retrieval_tools() {
        let mut registry = ContextRegistry::new();
        registry.register(ContextSource {
            name: "mem".to_string(),
            description: "memory source".to_string(),
            source_type: ContextSourceType::Memory,
            output_schema: None,
            retrievable: true,
            tags: vec![],
        });
        registry.register(ContextSource {
            name: "board".to_string(),
            description: "blackboard source".to_string(),
            source_type: ContextSourceType::Blackboard,
            output_schema: None,
            retrievable: false,
            tags: vec![],
        });

        // Node A's view: imports "mem" (retrievable) — gets list + retrieve tools
        let node_a_reg = registry.scoped(&["mem".to_string()]);
        let node_a_tools = generate_retrieval_tools(&node_a_reg);
        assert_eq!(
            node_a_tools.len(),
            2,
            "Node A should see list + retrieve tools"
        );

        // Node B's view: imports only "board" (not retrievable) — gets list tool only
        let node_b_reg = registry.scoped(&["board".to_string()]);
        let node_b_tools = generate_retrieval_tools(&node_b_reg);
        assert_eq!(node_b_tools.len(), 1, "Node B should see only list tool");
        assert_eq!(node_b_tools[0].name, "list_context_sources");

        // Node C's view: empty imports — gets list tool only
        let node_c_reg = registry.scoped(&[]);
        let node_c_tools = generate_retrieval_tools(&node_c_reg);
        assert_eq!(
            node_c_tools.len(),
            1,
            "Node C with empty scope should see only list tool"
        );
    }
}
