//! Unified Workflow Definition Format for flywheel-graph.
//!
//! This is the canonical serializable workflow definition format. JSON is the
//! only canonical serialization. All authoring paths (NL generation, manual
//! creation, visual editing) produce and consume this format.
//!
//! No executable closures — everything is data, fully round-trippable through
//! serde.

use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeMap;
use thiserror::Error;

use crate::data::Value;
use crate::hook::ConditionExpression;

/// The top-level workflow definition. JSON is the canonical serialization.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkflowDefinition {
    pub version: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub nodes: Vec<NodeDef>,
    pub edges: Vec<EdgeDef>,
    #[serde(default)]
    pub metadata: BTreeMap<String, Value>,
}

pub const GRAPH_VIEW_METADATA_KEY: &str = "graph_view";
pub const GRAPH_VIEW_SCHEMA_VERSION: u64 = 1;

impl WorkflowDefinition {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            version: "1.0".to_string(),
            name: name.into(),
            description: String::new(),
            nodes: Vec::new(),
            edges: Vec::new(),
            metadata: BTreeMap::new(),
        }
    }

    pub fn with_node(mut self, node: NodeDef) -> Self {
        self.nodes.push(node);
        self
    }

    pub fn with_edge(mut self, edge: EdgeDef) -> Self {
        self.edges.push(edge);
        self
    }

    pub fn graph_view_metadata(&self) -> Result<Option<GraphViewMetadata>, GraphViewMetadataError> {
        parse_graph_view_metadata(&self.metadata)
    }
}

/// Durable graph-renderer metadata reserved under `metadata.graph_view`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GraphViewMetadata {
    #[serde(deserialize_with = "deserialize_integer_like_u64")]
    pub schema_version: u64,
    #[serde(default)]
    pub visual_edges: Vec<GraphViewVisualEdge>,
    #[serde(default, flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// Durable visual edge annotation stored in `graph_view.visual_edges`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GraphViewVisualEdge {
    pub id: String,
    pub kind: GraphViewVisualEdgeKind,
    pub source: String,
    pub target: String,
    #[serde(default)]
    pub source_port: Option<String>,
    #[serde(default)]
    pub target_port: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GraphViewVisualEdgeKind {
    LoopArrow,
}

#[derive(Debug, Clone, Error, PartialEq, Eq)]
#[error("{path}: {message}")]
pub struct GraphViewMetadataError {
    pub path: String,
    pub message: String,
}

pub fn parse_graph_view_metadata(
    metadata: &BTreeMap<String, Value>,
) -> Result<Option<GraphViewMetadata>, GraphViewMetadataError> {
    let Some(raw_graph_view) = metadata.get(GRAPH_VIEW_METADATA_KEY) else {
        return Ok(None);
    };

    let json = serde_json::to_value(raw_graph_view).map_err(|error| GraphViewMetadataError {
        path: format!("/metadata/{GRAPH_VIEW_METADATA_KEY}"),
        message: error.to_string(),
    })?;

    serde_json::from_value(json)
        .map(Some)
        .map_err(|error| GraphViewMetadataError {
            path: format!("/metadata/{GRAPH_VIEW_METADATA_KEY}"),
            message: error.to_string(),
        })
}

fn deserialize_integer_like_u64<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::Number(number) => {
            if let Some(integer) = number.as_u64() {
                return Ok(integer);
            }

            if let Some(float) = number.as_f64()
                && float.is_finite()
                && float >= 0.0
                && float.fract() == 0.0
            {
                return Ok(float as u64);
            }

            Err(serde::de::Error::custom(
                "expected a non-negative integer-compatible number",
            ))
        }
        _ => Err(serde::de::Error::custom(
            "expected a non-negative integer-compatible number",
        )),
    }
}

/// A node in the workflow definition.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NodeDef {
    pub id: String,
    pub name: String,
    /// Node type: "action" for leaf nodes, "topology" for pattern nodes, "subgraph" for nested.
    #[serde(rename = "type")]
    pub node_type: NodeType,
    #[serde(default)]
    pub description: String,
    /// Instructions/system prompt for this node (for LLM-backed nodes).
    #[serde(default)]
    pub instructions: String,
    /// Input port declarations.
    #[serde(default)]
    pub inputs: Vec<PortDef>,
    /// Output port declarations.
    #[serde(default)]
    pub outputs: Vec<PortDef>,
    /// Tags for hook selector matching.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Model settings for LLM-backed nodes.
    #[serde(default)]
    pub model_settings: Option<ModelSettings>,
    /// Hook attachment points.
    #[serde(default)]
    pub hooks: Vec<HookAttachment>,
    /// For topology nodes: the pattern name (horizontal, pingpong, hub, vertical, etc.).
    #[serde(default)]
    pub pattern: Option<String>,
    /// For topology nodes: pattern-specific parameters.
    #[serde(default)]
    pub pattern_params: BTreeMap<String, Value>,
    /// For subgraph nodes: nested workflow definition.
    #[serde(default)]
    pub subgraph: Option<Box<WorkflowDefinition>>,
    /// Conditional routing: only execute if condition is met.
    #[serde(default)]
    pub route_when: Option<ConditionExpression>,
    /// Bounded repetition policy.
    #[serde(default)]
    pub repeat_policy: Option<RepeatPolicy>,
    /// Working directory override for this node's session.
    /// If None, inherits from workflow execution context (project working dir).
    #[serde(default)]
    pub working_dir: Option<std::path::PathBuf>,
    /// Provider override for this node (e.g. "claude", "codex", "gemini").
    /// If None, inherits from model_settings or workflow default.
    #[serde(default)]
    pub provider: Option<String>,
    /// Whether this node's session runs in a restricted sandbox environment
    /// (per-session git-worktree isolation). When `true`, the daemon allocates
    /// a sandbox before spawning the provider subprocess; see `docs/agent-sandbox.md`.
    /// Defaults to `false` — pre-existing JSON deserializes with sandbox off.
    #[serde(default)]
    pub sandbox: bool,
}

impl NodeDef {
    pub fn action(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            node_type: NodeType::Action,
            description: String::new(),
            instructions: String::new(),
            inputs: Vec::new(),
            outputs: Vec::new(),
            tags: Vec::new(),
            model_settings: None,
            hooks: Vec::new(),
            pattern: None,
            pattern_params: BTreeMap::new(),
            subgraph: None,
            route_when: None,
            repeat_policy: None,
            working_dir: None,
            provider: None,
            sandbox: false,
        }
    }

    pub fn topology(
        id: impl Into<String>,
        name: impl Into<String>,
        pattern: impl Into<String>,
    ) -> Self {
        let mut node = Self::action(id, name);
        node.node_type = NodeType::Topology;
        node.pattern = Some(pattern.into());
        node
    }

    pub fn subgraph(
        id: impl Into<String>,
        name: impl Into<String>,
        inner: WorkflowDefinition,
    ) -> Self {
        let mut node = Self::action(id, name);
        node.node_type = NodeType::Subgraph;
        node.subgraph = Some(Box::new(inner));
        node
    }

    pub fn with_instructions(mut self, inst: impl Into<String>) -> Self {
        self.instructions = inst.into();
        self
    }

    pub fn with_model(mut self, settings: ModelSettings) -> Self {
        self.model_settings = Some(settings);
        self
    }

    pub fn with_tag(mut self, tag: impl Into<String>) -> Self {
        self.tags.push(tag.into());
        self
    }
}

/// The type of a node in the workflow definition.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum NodeType {
    Action,
    Topology,
    Subgraph,
}

/// An edge connecting two nodes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EdgeDef {
    pub source: String,
    #[serde(default)]
    pub source_port: Option<String>,
    pub target: String,
    #[serde(default)]
    pub target_port: Option<String>,
    #[serde(default)]
    pub filter: Option<FilterDef>,
    #[serde(default)]
    pub label: Option<String>,
}

impl EdgeDef {
    pub fn new(source: impl Into<String>, target: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            source_port: None,
            target: target.into(),
            target_port: None,
            filter: None,
            label: None,
        }
    }
}

/// A named I/O port on a node.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PortDef {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub field_type: Option<String>,
    #[serde(default)]
    pub required: bool,
}

/// Model settings for LLM-backed nodes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelSettings {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub max_tokens: Option<usize>,
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(default)]
    pub top_p: Option<f64>,
    /// Session provider override (claude, codex, gemini, local, etc.).
    #[serde(default)]
    pub provider: Option<String>,
}

/// Hook attachment in workflow definition.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HookAttachment {
    pub stage: String,
    pub action: String,
    #[serde(default)]
    pub condition: Option<ConditionExpression>,
}

/// Filter definition in edge.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FilterDef {
    #[serde(default)]
    pub include: Option<Vec<String>>,
    #[serde(default)]
    pub exclude: Option<Vec<String>>,
}

/// Bounded repetition policy for nodes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RepeatPolicy {
    pub max_iterations: usize,
    #[serde(default)]
    pub termination: Option<ConditionExpression>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_workflow_definition() {
        let wf = WorkflowDefinition::new("test-workflow")
            .with_node(NodeDef::action("n1", "Node One").with_tag("llm"))
            .with_node(NodeDef::action("n2", "Node Two"))
            .with_edge(EdgeDef::new("n1", "n2"));

        let json = serde_json::to_string_pretty(&wf).unwrap();
        let parsed: WorkflowDefinition = serde_json::from_str(&json).unwrap();
        assert_eq!(wf, parsed);
    }

    #[test]
    fn simple_pipeline_three_action_nodes() {
        let wf = WorkflowDefinition::new("pipeline")
            .with_node(NodeDef::action("research", "Research"))
            .with_node(NodeDef::action("draft", "Draft"))
            .with_node(NodeDef::action("review", "Review"))
            .with_edge(EdgeDef::new("research", "draft"))
            .with_edge(EdgeDef::new("draft", "review"));

        let json = serde_json::to_string(&wf).unwrap();
        let parsed: WorkflowDefinition = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.nodes.len(), 3);
        assert_eq!(parsed.edges.len(), 2);
        assert_eq!(parsed.edges[0].source, "research");
        assert_eq!(parsed.edges[0].target, "draft");
        assert_eq!(parsed.edges[1].source, "draft");
        assert_eq!(parsed.edges[1].target, "review");
    }

    #[test]
    fn hub_spoke_topology_node() {
        let mut hub_params = BTreeMap::new();
        hub_params.insert("routing".to_string(), Value::String("round_robin".into()));

        let mut hub_node = NodeDef::topology("hub1", "Central Hub", "hub");
        hub_node.pattern_params = hub_params;

        let wf = WorkflowDefinition::new("hub-spoke")
            .with_node(hub_node)
            .with_node(NodeDef::action("spoke1", "Spoke A"))
            .with_node(NodeDef::action("spoke2", "Spoke B"))
            .with_node(NodeDef::action("spoke3", "Spoke C"))
            .with_edge(EdgeDef::new("hub1", "spoke1"))
            .with_edge(EdgeDef::new("hub1", "spoke2"))
            .with_edge(EdgeDef::new("hub1", "spoke3"));

        let json = serde_json::to_string(&wf).unwrap();
        let parsed: WorkflowDefinition = serde_json::from_str(&json).unwrap();

        let hub = &parsed.nodes[0];
        assert_eq!(hub.node_type, NodeType::Topology);
        assert_eq!(hub.pattern, Some("hub".to_string()));
        assert_eq!(
            hub.pattern_params.get("routing"),
            Some(&Value::String("round_robin".into()))
        );
    }

    #[test]
    fn fan_out_aggregate_vertical_pattern() {
        let mut params = BTreeMap::new();
        params.insert("aggregate".to_string(), Value::String("concatenate".into()));

        let mut vert = NodeDef::topology("fanout", "Fan-Out Aggregate", "vertical");
        vert.pattern_params = params;

        let wf = WorkflowDefinition::new("fan-out")
            .with_node(NodeDef::action("source", "Source"))
            .with_node(vert)
            .with_node(NodeDef::action("sink", "Sink"))
            .with_edge(EdgeDef::new("source", "fanout"))
            .with_edge(EdgeDef::new("fanout", "sink"));

        let json = serde_json::to_string(&wf).unwrap();
        let parsed: WorkflowDefinition = serde_json::from_str(&json).unwrap();

        let fanout = parsed.nodes.iter().find(|n| n.id == "fanout").unwrap();
        assert_eq!(fanout.node_type, NodeType::Topology);
        assert_eq!(fanout.pattern, Some("vertical".to_string()));
    }

    #[test]
    fn nested_subgraph() {
        let inner = WorkflowDefinition::new("inner-pipeline")
            .with_node(NodeDef::action("step1", "Inner Step 1"))
            .with_node(NodeDef::action("step2", "Inner Step 2"))
            .with_edge(EdgeDef::new("step1", "step2"));

        let outer = WorkflowDefinition::new("outer")
            .with_node(NodeDef::action("pre", "Pre-process"))
            .with_node(NodeDef::subgraph(
                "nested",
                "Nested Pipeline",
                inner.clone(),
            ))
            .with_node(NodeDef::action("post", "Post-process"))
            .with_edge(EdgeDef::new("pre", "nested"))
            .with_edge(EdgeDef::new("nested", "post"));

        let json = serde_json::to_string_pretty(&outer).unwrap();
        let parsed: WorkflowDefinition = serde_json::from_str(&json).unwrap();

        let nested_node = parsed.nodes.iter().find(|n| n.id == "nested").unwrap();
        assert_eq!(nested_node.node_type, NodeType::Subgraph);
        let sub = nested_node.subgraph.as_ref().unwrap();
        assert_eq!(sub.name, "inner-pipeline");
        assert_eq!(sub.nodes.len(), 2);
        assert_eq!(sub.edges.len(), 1);
    }

    #[test]
    fn repeated_review_loop() {
        let wf = WorkflowDefinition::new("review-loop").with_node({
            let mut node = NodeDef::action("reviewer", "Code Reviewer")
                .with_instructions("Review the code for correctness");
            node.repeat_policy = Some(RepeatPolicy {
                max_iterations: 3,
                termination: Some(ConditionExpression::FieldEquals {
                    field: "approved".into(),
                    value: Value::Bool(true),
                }),
            });
            node
        });

        let json = serde_json::to_string(&wf).unwrap();
        let parsed: WorkflowDefinition = serde_json::from_str(&json).unwrap();

        let reviewer = &parsed.nodes[0];
        let policy = reviewer.repeat_policy.as_ref().unwrap();
        assert_eq!(policy.max_iterations, 3);
        assert!(policy.termination.is_some());
    }

    #[test]
    fn all_tier2_topology_patterns_expressible() {
        // Verify all known topology patterns can be represented in the format.
        let patterns = [
            "horizontal",
            "pingpong",
            "hub",
            "vertical",
            "brainstorm",
            "decision",
            "instructor",
        ];

        for pattern_name in &patterns {
            let node = NodeDef::topology(
                format!("{}_node", pattern_name),
                format!("{} Pattern", pattern_name),
                *pattern_name,
            );
            assert_eq!(node.node_type, NodeType::Topology);
            assert_eq!(node.pattern.as_deref(), Some(*pattern_name));

            // Verify it round-trips through JSON.
            let wf = WorkflowDefinition::new("pattern-test").with_node(node);
            let json = serde_json::to_string(&wf).unwrap();
            let parsed: WorkflowDefinition = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed.nodes[0].pattern.as_deref(), Some(*pattern_name));
        }
    }

    #[test]
    fn version_field_defaults_to_1_0() {
        let wf = WorkflowDefinition::new("test");
        assert_eq!(wf.version, "1.0");
    }

    #[test]
    fn unknown_fields_ignored_forward_compatibility() {
        // Simulate a future version of the format with extra fields.
        let json = r#"{
            "version": "1.1",
            "name": "future-workflow",
            "nodes": [{
                "id": "n1",
                "name": "Node",
                "type": "action",
                "future_field": "should be ignored",
                "another_new_field": 42
            }],
            "edges": [],
            "some_new_top_level": true
        }"#;

        // This must not fail — unknown fields are silently ignored.
        let parsed: WorkflowDefinition = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.name, "future-workflow");
        assert_eq!(parsed.nodes.len(), 1);
        assert_eq!(parsed.nodes[0].id, "n1");
    }

    #[test]
    fn edge_with_filter_serializes_correctly() {
        let mut edge = EdgeDef::new("src", "dst");
        edge.filter = Some(FilterDef {
            include: Some(vec!["field_a".into(), "field_b".into()]),
            exclude: None,
        });
        edge.label = Some("data flow".into());

        let json = serde_json::to_string_pretty(&edge).unwrap();
        let parsed: EdgeDef = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.source, "src");
        assert_eq!(parsed.target, "dst");
        assert_eq!(parsed.label, Some("data flow".into()));
        let filter = parsed.filter.unwrap();
        assert_eq!(
            filter.include,
            Some(vec!["field_a".into(), "field_b".into()])
        );
        assert!(filter.exclude.is_none());
    }

    #[test]
    fn model_settings_roundtrip() {
        let node = NodeDef::action("llm", "LLM Node").with_model(ModelSettings {
            model: Some("claude-opus-4-20250514".into()),
            max_tokens: Some(4096),
            temperature: Some(0.7),
            top_p: None,
            provider: None,
        });

        let json = serde_json::to_string(&node).unwrap();
        let parsed: NodeDef = serde_json::from_str(&json).unwrap();

        let settings = parsed.model_settings.unwrap();
        assert_eq!(settings.model, Some("claude-opus-4-20250514".into()));
        assert_eq!(settings.max_tokens, Some(4096));
        assert_eq!(settings.temperature, Some(0.7));
        assert_eq!(settings.top_p, None);
    }

    #[test]
    fn hook_attachment_roundtrip() {
        let mut node = NodeDef::action("guarded", "Guarded Node");
        node.hooks.push(HookAttachment {
            stage: "before_execute".into(),
            action: "validate_input".into(),
            condition: Some(ConditionExpression::FieldExists("prompt".into())),
        });

        let json = serde_json::to_string(&node).unwrap();
        let parsed: NodeDef = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.hooks.len(), 1);
        assert_eq!(parsed.hooks[0].stage, "before_execute");
        assert_eq!(parsed.hooks[0].action, "validate_input");
        assert!(parsed.hooks[0].condition.is_some());
    }

    #[test]
    fn port_def_roundtrip() {
        let mut node = NodeDef::action("typed", "Typed Node");
        node.inputs.push(PortDef {
            name: "prompt".into(),
            description: "The user prompt".into(),
            field_type: Some("string".into()),
            required: true,
        });
        node.outputs.push(PortDef {
            name: "response".into(),
            description: String::new(),
            field_type: Some("string".into()),
            required: false,
        });

        let json = serde_json::to_string(&node).unwrap();
        let parsed: NodeDef = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.inputs.len(), 1);
        assert_eq!(parsed.inputs[0].name, "prompt");
        assert!(parsed.inputs[0].required);
        assert_eq!(parsed.outputs.len(), 1);
        assert_eq!(parsed.outputs[0].name, "response");
        assert!(!parsed.outputs[0].required);
    }

    #[test]
    fn conditional_routing_roundtrip() {
        let mut node = NodeDef::action("conditional", "Conditional Node");
        node.route_when = Some(ConditionExpression::And(vec![
            ConditionExpression::FieldExists("input".into()),
            ConditionExpression::FieldGreaterThan {
                field: "confidence".into(),
                threshold: 0.8,
            },
        ]));

        let json = serde_json::to_string(&node).unwrap();
        let parsed: NodeDef = serde_json::from_str(&json).unwrap();

        assert!(parsed.route_when.is_some());
    }

    #[test]
    fn metadata_on_workflow() {
        let mut wf = WorkflowDefinition::new("annotated");
        wf.metadata
            .insert("author".into(), Value::String("test".into()));
        wf.metadata.insert("priority".into(), Value::Number(1.0));

        let json = serde_json::to_string(&wf).unwrap();
        let parsed: WorkflowDefinition = serde_json::from_str(&json).unwrap();

        assert_eq!(
            parsed.metadata.get("author"),
            Some(&Value::String("test".into()))
        );
        assert_eq!(parsed.metadata.get("priority"), Some(&Value::Number(1.0)));
    }

    #[test]
    fn graph_view_metadata_roundtrip() {
        let mut wf = WorkflowDefinition::new("annotated");
        let graph_view = GraphViewMetadata {
            schema_version: GRAPH_VIEW_SCHEMA_VERSION,
            visual_edges: vec![GraphViewVisualEdge {
                id: "loop-1".to_string(),
                kind: GraphViewVisualEdgeKind::LoopArrow,
                source: "review".to_string(),
                target: "draft".to_string(),
                source_port: Some("out".to_string()),
                target_port: Some("in".to_string()),
                label: Some("retry".to_string()),
            }],
            extra: BTreeMap::from([("lane".to_string(), Value::Number(2.0))]),
        };
        wf.metadata.insert(
            GRAPH_VIEW_METADATA_KEY.to_string(),
            serde_json::from_value(serde_json::to_value(&graph_view).unwrap()).unwrap(),
        );

        let parsed = wf.graph_view_metadata().unwrap().unwrap();
        assert_eq!(parsed.schema_version, GRAPH_VIEW_SCHEMA_VERSION);
        assert_eq!(parsed.visual_edges.len(), 1);
        assert_eq!(parsed.extra.get("lane"), Some(&Value::Number(2.0)));
    }

    #[test]
    fn minimal_workflow_deserialization() {
        // Minimal valid JSON — only required fields, all defaults kick in.
        let json = r#"{
            "version": "1.0",
            "name": "minimal",
            "nodes": [],
            "edges": []
        }"#;

        let parsed: WorkflowDefinition = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.version, "1.0");
        assert_eq!(parsed.name, "minimal");
        assert!(parsed.description.is_empty());
        assert!(parsed.nodes.is_empty());
        assert!(parsed.edges.is_empty());
        assert!(parsed.metadata.is_empty());
    }

    /// Round-trips `NodeDef.sandbox = true` through JSON serde — catches any
    /// silent drop by a misconfigured `#[serde(...)]` attribute on the field.
    #[test]
    fn node_def_sandbox_roundtrip() {
        let mut node = NodeDef::action("worker", "Sandboxed Worker");
        node.sandbox = true;
        let wf = WorkflowDefinition::new("sandbox-rt").with_node(node);

        let json = serde_json::to_string(&wf).unwrap();
        let parsed: WorkflowDefinition = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.nodes.len(), 1);
        assert!(
            parsed.nodes[0].sandbox,
            "sandbox=true must survive a JSON round-trip"
        );
    }

    /// Default value: `NodeDef::action` returns `sandbox = false`, and the
    /// same node round-trips without flipping the flag.
    #[test]
    fn node_def_sandbox_default_false() {
        let node = NodeDef::action("n1", "Default Node");
        assert!(!node.sandbox, "default sandbox must be false");

        let json = serde_json::to_string(&node).unwrap();
        let parsed: NodeDef = serde_json::from_str(&json).unwrap();
        assert!(!parsed.sandbox);
    }

    /// Back-compat: old JSON (no `sandbox` field at all) deserializes into a
    /// NodeDef with `sandbox = false`. `#[serde(default)]` is the contract.
    #[test]
    fn node_def_sandbox_serde_backcompat() {
        let json = r#"{
            "id": "legacy",
            "name": "Legacy Node",
            "type": "action"
        }"#;
        let parsed: NodeDef = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.id, "legacy");
        assert!(
            !parsed.sandbox,
            "pre-sandbox JSON must deserialize with sandbox=false"
        );
    }

    /// Topology nodes also get the sandbox field via the `action` delegation.
    #[test]
    fn topology_node_sandbox_roundtrip() {
        let mut node = NodeDef::topology("hub", "Hub", "hub");
        assert!(!node.sandbox);
        node.sandbox = true;
        let json = serde_json::to_string(&node).unwrap();
        let parsed: NodeDef = serde_json::from_str(&json).unwrap();
        assert!(parsed.sandbox);
    }
}
