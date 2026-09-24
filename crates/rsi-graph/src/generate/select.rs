use crate::data::Value;
use crate::error::GraphError;
use crate::format::*;

use super::GenerateResult;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Available topology patterns with descriptions for LLM selection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopologyOption {
    pub name: String,
    pub description: String,
    pub use_cases: Vec<String>,
    pub example_config: serde_json::Value,
}

/// Get the list of available topology options.
pub fn available_topologies() -> Vec<TopologyOption> {
    vec![
        TopologyOption {
            name: "horizontal".to_string(),
            description: "Sequential chain: nodes execute in order, output flows to next input"
                .to_string(),
            use_cases: vec![
                "Processing pipeline".to_string(),
                "Step-by-step workflows".to_string(),
                "Data transformation chain".to_string(),
            ],
            example_config: serde_json::json!({
                "early_exit": null
            }),
        },
        TopologyOption {
            name: "pingpong".to_string(),
            description: "Two nodes alternate for N rounds or until termination".to_string(),
            use_cases: vec![
                "Debate between two perspectives".to_string(),
                "Iterative refinement with evaluator".to_string(),
                "Back-and-forth negotiation".to_string(),
            ],
            example_config: serde_json::json!({
                "max_rounds": 5,
                "history_policy": "KeepAll"
            }),
        },
        TopologyOption {
            name: "hub".to_string(),
            description: "Hub node routes work to specialist spoke nodes".to_string(),
            use_cases: vec![
                "PR review with specialists".to_string(),
                "Task delegation to experts".to_string(),
                "Manager-worker pattern".to_string(),
            ],
            example_config: serde_json::json!({
                "max_rounds": 10
            }),
        },
        TopologyOption {
            name: "vertical".to_string(),
            description: "Fan-out to parallel workers, then aggregate results".to_string(),
            use_cases: vec![
                "Parallel evaluation".to_string(),
                "Multi-perspective analysis".to_string(),
                "Batch processing".to_string(),
            ],
            example_config: serde_json::json!({
                "aggregate": "concatenate",
                "concurrency_limit": null
            }),
        },
        TopologyOption {
            name: "vertical_decision".to_string(),
            description: "Critics evaluate, solver improves, loop until consensus".to_string(),
            use_cases: vec![
                "Code review with multiple reviewers".to_string(),
                "Quality assurance pipeline".to_string(),
                "Iterative improvement with judges".to_string(),
            ],
            example_config: serde_json::json!({
                "max_iterations": 3,
                "consensus_threshold": 0.8
            }),
        },
        TopologyOption {
            name: "brainstorm".to_string(),
            description: "Sequential critics each seeing prior feedback, then solver synthesizes"
                .to_string(),
            use_cases: vec![
                "Brainstorming sessions".to_string(),
                "Collaborative writing".to_string(),
                "Multi-angle analysis".to_string(),
            ],
            example_config: serde_json::json!({}),
        },
        TopologyOption {
            name: "instructor_assistant".to_string(),
            description: "Instructor provides directives, assistant executes, loop until satisfied"
                .to_string(),
            use_cases: vec![
                "Guided task completion".to_string(),
                "Teach-evaluate cycles".to_string(),
                "Supervised learning".to_string(),
            ],
            example_config: serde_json::json!({
                "max_iterations": 5,
                "history_policy": "SlidingWindow(10)"
            }),
        },
    ]
}

/// Build a system prompt for topology selection.
pub fn build_selection_prompt(topologies: &[TopologyOption]) -> String {
    let mut prompt = String::from(
        "You are a workflow topology selector. Given a user's intent, select the most appropriate \
         topology pattern and configure it.\n\n\
         Available topologies:\n\n",
    );

    for topo in topologies {
        prompt.push_str(&format!(
            "## {}\n{}\n\nUse cases:\n",
            topo.name, topo.description
        ));
        for uc in &topo.use_cases {
            prompt.push_str(&format!("- {}\n", uc));
        }
        prompt.push_str(&format!(
            "\nExample config: {}\n\n",
            serde_json::to_string_pretty(&topo.example_config).unwrap_or_default()
        ));
    }

    prompt.push_str(
        "\nRespond with a JSON object containing:\n\
         - \"topology\": the topology name\n\
         - \"nodes\": array of node definitions (with id, name, instructions)\n\
         - \"reasoning\": why this topology was chosen\n\
         - \"params\": topology-specific parameters\n\n\
         Output ONLY valid JSON, no markdown fences.",
    );

    prompt
}

/// Parse an LLM response into a WorkflowDefinition.
/// This is the simple path: single topology selection.
pub fn parse_selection_response(response: &str) -> Result<GenerateResult, GraphError> {
    // Try to parse the response as JSON
    let parsed: serde_json::Value = serde_json::from_str(response.trim())
        .or_else(|_| {
            // Try extracting JSON from markdown code fences
            let trimmed = response.trim();
            let json_str = if let Some(start) = trimmed.find('{') {
                if let Some(end) = trimmed.rfind('}') {
                    &trimmed[start..=end]
                } else {
                    trimmed
                }
            } else {
                trimmed
            };
            serde_json::from_str(json_str)
        })
        .map_err(|e| {
            GraphError::ExecutionFailed(format!("Failed to parse LLM response as JSON: {}", e))
        })?;

    let topology = parsed
        .get("topology")
        .and_then(|v| v.as_str())
        .unwrap_or("horizontal")
        .to_string();

    let reasoning = parsed
        .get("reasoning")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // Build WorkflowDefinition from the parsed response
    let mut workflow = WorkflowDefinition::new(format!("{}_workflow", topology));
    workflow.description = reasoning.clone();

    // Parse nodes from the response
    if let Some(nodes) = parsed.get("nodes").and_then(|v| v.as_array()) {
        for node_json in nodes {
            let id = node_json
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("unnamed")
                .to_string();
            let name = node_json
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or(&id)
                .to_string();
            let instructions = node_json
                .get("instructions")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let mut node = NodeDef::action(&id, &name);
            node.instructions = instructions;
            workflow = workflow.with_node(node);
        }
    }

    // If the topology is not just a simple chain, wrap in a topology node
    if topology != "horizontal" || workflow.nodes.is_empty() {
        // Create a topology wrapper
        let mut topo_node = NodeDef::topology("main_topology", &topology, &topology);
        if let Some(params) = parsed.get("params")
            && let Ok(map) = serde_json::from_value::<BTreeMap<String, Value>>(params.clone())
        {
            topo_node.pattern_params = map;
        }

        // For non-horizontal, keep the topology node at the front
        workflow.nodes.insert(0, topo_node);
    }

    // Add sequential edges for simple topologies
    let node_ids: Vec<String> = workflow.nodes.iter().map(|n| n.id.clone()).collect();
    for pair in node_ids.windows(2) {
        workflow = workflow.with_edge(EdgeDef::new(&pair[0], &pair[1]));
    }

    Ok(GenerateResult {
        workflow,
        reasoning,
        cache_hit: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn available_topologies_returns_all_seven_patterns() {
        let topos = available_topologies();
        assert_eq!(topos.len(), 7);

        let names: Vec<&str> = topos.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"horizontal"));
        assert!(names.contains(&"pingpong"));
        assert!(names.contains(&"hub"));
        assert!(names.contains(&"vertical"));
        assert!(names.contains(&"vertical_decision"));
        assert!(names.contains(&"brainstorm"));
        assert!(names.contains(&"instructor_assistant"));
    }

    #[test]
    fn build_selection_prompt_includes_all_topology_names() {
        let topos = available_topologies();
        let prompt = build_selection_prompt(&topos);

        for topo in &topos {
            assert!(
                prompt.contains(&topo.name),
                "Prompt missing topology: {}",
                topo.name
            );
        }
        assert!(prompt.contains("Respond with a JSON object"));
    }

    #[test]
    fn parse_selection_response_valid_json() {
        let response = serde_json::json!({
            "topology": "horizontal",
            "nodes": [
                {"id": "step_1", "name": "Step 1", "instructions": "Do step 1"},
                {"id": "step_2", "name": "Step 2", "instructions": "Do step 2"}
            ],
            "reasoning": "Sequential pipeline for ordered steps",
            "params": {}
        });

        let result = parse_selection_response(&response.to_string()).unwrap();
        assert_eq!(result.workflow.name, "horizontal_workflow");
        assert_eq!(result.reasoning, "Sequential pipeline for ordered steps");
        assert!(!result.cache_hit);
        // horizontal with nodes: 2 action nodes, edges connecting them
        assert_eq!(result.workflow.nodes.len(), 2);
        assert_eq!(result.workflow.edges.len(), 1);
    }

    #[test]
    fn parse_selection_response_non_horizontal_adds_topology_node() {
        let response = serde_json::json!({
            "topology": "pingpong",
            "nodes": [
                {"id": "side_a", "name": "Side A", "instructions": "Argue A"},
                {"id": "side_b", "name": "Side B", "instructions": "Argue B"}
            ],
            "reasoning": "Debate pattern",
            "params": {"max_rounds": 5}
        });

        let result = parse_selection_response(&response.to_string()).unwrap();
        assert_eq!(result.workflow.nodes.len(), 3); // topology + 2 action nodes
        assert_eq!(result.workflow.nodes[0].node_type, NodeType::Topology);
        assert_eq!(
            result.workflow.nodes[0].pattern,
            Some("pingpong".to_string())
        );
    }

    #[test]
    fn parse_selection_response_json_in_code_fences() {
        let response = r#"```json
        {
            "topology": "hub",
            "nodes": [
                {"id": "router", "name": "Router", "instructions": "Route tasks"}
            ],
            "reasoning": "Hub routing pattern",
            "params": {"max_rounds": 10}
        }
        ```"#;

        let result = parse_selection_response(response).unwrap();
        assert_eq!(result.workflow.name, "hub_workflow");
        assert_eq!(result.reasoning, "Hub routing pattern");
    }

    #[test]
    fn parse_selection_response_invalid_json_errors() {
        let response = "this is not json at all";
        let result = parse_selection_response(response);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("Failed to parse LLM response"));
    }

    #[test]
    fn parse_selection_response_preserves_topology_params() {
        let response = serde_json::json!({
            "topology": "vertical",
            "nodes": [
                {"id": "w1", "name": "Worker 1", "instructions": "Process"}
            ],
            "reasoning": "Fan-out",
            "params": {"aggregate": "concatenate"}
        });

        let result = parse_selection_response(&response.to_string()).unwrap();
        let topo_node = &result.workflow.nodes[0];
        assert_eq!(topo_node.node_type, NodeType::Topology);
        assert!(topo_node.pattern_params.contains_key("aggregate"));
    }
}
