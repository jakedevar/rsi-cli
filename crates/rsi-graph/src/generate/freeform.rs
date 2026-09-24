use crate::compiler;
use crate::error::GraphError;
use crate::format::*;

use super::GenerateResult;
use serde::{Deserialize, Serialize};

/// Stage 1 output: Intent analysis
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntentAnalysis {
    pub agents: Vec<AgentSpec>,
    pub data_flow: String,
    pub complexity: String,
    pub suggested_topology: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSpec {
    pub role: String,
    pub description: String,
    pub capabilities: Vec<String>,
}

/// Stage 2 output: Graph structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphStructure {
    pub topology: Option<String>,
    pub nodes: Vec<NodeSpec>,
    pub edges: Vec<EdgeSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeSpec {
    pub id: String,
    pub name: String,
    pub role: String,
    pub node_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EdgeSpec {
    pub source: String,
    pub target: String,
    pub label: Option<String>,
}

/// Multi-stage generation context.
#[derive(Debug)]
pub struct GenerationPipeline {
    max_retries: usize,
}

impl GenerationPipeline {
    pub fn new() -> Self {
        Self { max_retries: 2 }
    }

    pub fn with_max_retries(mut self, retries: usize) -> Self {
        self.max_retries = retries;
        self
    }

    /// Stage 1: Analyze intent into agents and data flow.
    pub fn analyze_intent(&self, intent: &str) -> Result<IntentAnalysis, GraphError> {
        // In production, this would be an LLM call.
        // For now, use keyword analysis to produce a reasonable IntentAnalysis.
        let intent_lower = intent.to_lowercase();

        let mut agents = Vec::new();
        let mut suggested_topology = None;

        // Detect patterns in the intent
        if intent_lower.contains("review") || intent_lower.contains("analyze") {
            agents.push(AgentSpec {
                role: "analyzer".to_string(),
                description: "Performs initial analysis".to_string(),
                capabilities: vec!["analysis".to_string()],
            });
        }

        if intent_lower.contains("lint") || intent_lower.contains("code") {
            agents.push(AgentSpec {
                role: "linter".to_string(),
                description: "Checks code quality".to_string(),
                capabilities: vec!["linting".to_string()],
            });
        }

        if intent_lower.contains("parallel") || intent_lower.contains("reviewer") {
            suggested_topology = Some("vertical".to_string());
            for i in 1..=3 {
                agents.push(AgentSpec {
                    role: format!("reviewer_{}", i),
                    description: format!("Reviewer {}", i),
                    capabilities: vec!["review".to_string()],
                });
            }
        }

        if intent_lower.contains("summarize") || intent_lower.contains("synthesize") {
            agents.push(AgentSpec {
                role: "summarizer".to_string(),
                description: "Synthesizes results".to_string(),
                capabilities: vec!["summarization".to_string()],
            });
        }

        if intent_lower.contains("search") || intent_lower.contains("research") {
            agents.push(AgentSpec {
                role: "researcher".to_string(),
                description: "Searches and gathers information".to_string(),
                capabilities: vec!["search".to_string()],
            });
        }

        if intent_lower.contains("evaluate") || intent_lower.contains("assess") {
            agents.push(AgentSpec {
                role: "evaluator".to_string(),
                description: "Evaluates and scores results".to_string(),
                capabilities: vec!["evaluation".to_string()],
            });
        }

        // Default: at least 2 agents in a pipeline
        if agents.is_empty() {
            agents.push(AgentSpec {
                role: "processor".to_string(),
                description: "Processes input".to_string(),
                capabilities: vec!["processing".to_string()],
            });
            agents.push(AgentSpec {
                role: "output".to_string(),
                description: "Produces output".to_string(),
                capabilities: vec!["output".to_string()],
            });
        }

        let complexity = if agents.len() > 3 {
            "complex"
        } else {
            "simple"
        }
        .to_string();

        Ok(IntentAnalysis {
            agents,
            data_flow: intent.to_string(),
            complexity,
            suggested_topology,
        })
    }

    /// Stage 2: Build graph structure from intent analysis.
    pub fn build_structure(&self, analysis: &IntentAnalysis) -> Result<GraphStructure, GraphError> {
        let topology = analysis.suggested_topology.clone();

        let nodes: Vec<NodeSpec> = analysis
            .agents
            .iter()
            .map(|a| NodeSpec {
                id: a.role.clone(),
                name: a.description.clone(),
                role: a.role.clone(),
                node_type: "action".to_string(),
            })
            .collect();

        // Build edges: sequential unless topology suggests otherwise
        let edges = if topology.as_deref() == Some("vertical") {
            // Fan-out: all feed into an aggregator
            // Don't create edges between parallel workers
            Vec::new()
        } else {
            // Sequential edges
            nodes
                .windows(2)
                .map(|pair| EdgeSpec {
                    source: pair[0].id.clone(),
                    target: pair[1].id.clone(),
                    label: None,
                })
                .collect()
        };

        Ok(GraphStructure {
            topology,
            nodes,
            edges,
        })
    }

    /// Stage 3: Parameterize into a full WorkflowDefinition.
    pub fn parameterize(
        &self,
        intent: &str,
        structure: &GraphStructure,
    ) -> Result<WorkflowDefinition, GraphError> {
        let mut workflow = WorkflowDefinition::new(format!(
            "generated_{}",
            intent
                .split_whitespace()
                .take(3)
                .collect::<Vec<_>>()
                .join("_")
        ));
        workflow.description = intent.to_string();

        for node_spec in &structure.nodes {
            let mut node = NodeDef::action(&node_spec.id, &node_spec.name);
            node.instructions = format!("You are a {}. {}", node_spec.role, node_spec.name);
            node.tags = vec![node_spec.role.clone()];
            workflow = workflow.with_node(node);
        }

        for edge_spec in &structure.edges {
            let mut edge = EdgeDef::new(&edge_spec.source, &edge_spec.target);
            edge.label = edge_spec.label.clone();
            workflow = workflow.with_edge(edge);
        }

        Ok(workflow)
    }

    /// Full generation pipeline: analyze -> structure -> parameterize -> validate.
    pub fn generate(&self, intent: &str) -> Result<GenerateResult, GraphError> {
        let mut last_error = None;

        for _attempt in 0..=self.max_retries {
            // Stage 1
            let analysis = self.analyze_intent(intent)?;

            // Stage 2
            let structure = self.build_structure(&analysis)?;

            // Stage 3
            let workflow = self.parameterize(intent, &structure)?;

            // Validate
            match compiler::compile(&workflow) {
                Ok(_) => {
                    return Ok(GenerateResult {
                        workflow,
                        reasoning: format!(
                            "Generated from intent analysis: {} agents, complexity: {}",
                            analysis.agents.len(),
                            analysis.complexity
                        ),
                        cache_hit: false,
                    });
                }
                Err(diag) => {
                    let errors: Vec<String> =
                        diag.errors.iter().map(|e| e.message.clone()).collect();
                    last_error = Some(format!(
                        "Generated workflow failed validation: {}",
                        errors.join(", ")
                    ));
                }
            }
        }

        Err(GraphError::ExecutionFailed(last_error.unwrap_or_else(
            || "Generation failed after retries".to_string(),
        )))
    }
}

impl Default for GenerationPipeline {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn analyze_intent_review_code_produces_analyzer() {
        let pipeline = GenerationPipeline::new();
        let analysis = pipeline.analyze_intent("review code").unwrap();

        let roles: Vec<&str> = analysis.agents.iter().map(|a| a.role.as_str()).collect();
        assert!(
            roles.contains(&"analyzer"),
            "Expected analyzer agent for 'review code', got: {:?}",
            roles
        );
    }

    #[test]
    fn analyze_intent_parallel_reviewers_suggests_vertical() {
        let pipeline = GenerationPipeline::new();
        let analysis = pipeline
            .analyze_intent("parallel reviewers for evaluation")
            .unwrap();

        assert_eq!(
            analysis.suggested_topology,
            Some("vertical".to_string()),
            "Expected vertical topology for parallel reviewers"
        );
        // Should have reviewer agents
        let reviewer_count = analysis
            .agents
            .iter()
            .filter(|a| a.role.starts_with("reviewer_"))
            .count();
        assert_eq!(reviewer_count, 3);
    }

    #[test]
    fn build_structure_produces_correct_nodes_and_edges() {
        let pipeline = GenerationPipeline::new();
        let analysis = IntentAnalysis {
            agents: vec![
                AgentSpec {
                    role: "step_a".to_string(),
                    description: "First".to_string(),
                    capabilities: vec![],
                },
                AgentSpec {
                    role: "step_b".to_string(),
                    description: "Second".to_string(),
                    capabilities: vec![],
                },
                AgentSpec {
                    role: "step_c".to_string(),
                    description: "Third".to_string(),
                    capabilities: vec![],
                },
            ],
            data_flow: "test".to_string(),
            complexity: "simple".to_string(),
            suggested_topology: None,
        };

        let structure = pipeline.build_structure(&analysis).unwrap();
        assert_eq!(structure.nodes.len(), 3);
        assert_eq!(structure.edges.len(), 2);
        assert_eq!(structure.edges[0].source, "step_a");
        assert_eq!(structure.edges[0].target, "step_b");
        assert_eq!(structure.edges[1].source, "step_b");
        assert_eq!(structure.edges[1].target, "step_c");
    }

    #[test]
    fn build_structure_vertical_topology_no_sequential_edges() {
        let pipeline = GenerationPipeline::new();
        let analysis = IntentAnalysis {
            agents: vec![
                AgentSpec {
                    role: "w1".to_string(),
                    description: "Worker 1".to_string(),
                    capabilities: vec![],
                },
                AgentSpec {
                    role: "w2".to_string(),
                    description: "Worker 2".to_string(),
                    capabilities: vec![],
                },
            ],
            data_flow: "test".to_string(),
            complexity: "simple".to_string(),
            suggested_topology: Some("vertical".to_string()),
        };

        let structure = pipeline.build_structure(&analysis).unwrap();
        assert_eq!(structure.nodes.len(), 2);
        assert!(
            structure.edges.is_empty(),
            "Vertical topology should not have sequential edges"
        );
    }

    #[test]
    fn parameterize_produces_valid_workflow_definition() {
        let pipeline = GenerationPipeline::new();
        let structure = GraphStructure {
            topology: None,
            nodes: vec![
                NodeSpec {
                    id: "a".to_string(),
                    name: "Node A".to_string(),
                    role: "processor".to_string(),
                    node_type: "action".to_string(),
                },
                NodeSpec {
                    id: "b".to_string(),
                    name: "Node B".to_string(),
                    role: "output".to_string(),
                    node_type: "action".to_string(),
                },
            ],
            edges: vec![EdgeSpec {
                source: "a".to_string(),
                target: "b".to_string(),
                label: None,
            }],
        };

        let wf = pipeline.parameterize("test workflow", &structure).unwrap();
        assert_eq!(wf.nodes.len(), 2);
        assert_eq!(wf.edges.len(), 1);
        assert_eq!(wf.nodes[0].id, "a");
        assert_eq!(wf.nodes[1].id, "b");
        assert!(!wf.nodes[0].instructions.is_empty());
        assert_eq!(wf.nodes[0].tags, vec!["processor".to_string()]);
    }

    #[test]
    fn full_pipeline_code_review_with_reviewers_and_summarizer() {
        let pipeline = GenerationPipeline::new();
        let result = pipeline
            .generate("code review pipeline: lint then 3 parallel reviewers then summarize")
            .unwrap();

        assert!(!result.workflow.nodes.is_empty());
        assert!(!result.reasoning.is_empty());
        assert!(!result.cache_hit);

        // Should compile successfully
        compiler::compile(&result.workflow).expect("Generated workflow should compile");
    }

    #[test]
    fn full_pipeline_research_topic() {
        let pipeline = GenerationPipeline::new();
        let result = pipeline
            .generate("research topic: search then evaluate then summarize")
            .unwrap();

        assert!(!result.workflow.nodes.is_empty());
        assert!(!result.reasoning.is_empty());
        assert!(!result.cache_hit);

        // Should compile successfully
        compiler::compile(&result.workflow).expect("Generated workflow should compile");
    }

    #[test]
    fn generated_workflow_passes_compilation() {
        let pipeline = GenerationPipeline::new();
        let result = pipeline.generate("process and output data").unwrap();

        let (graph, diag) = compiler::compile(&result.workflow).expect("Should compile");
        assert!(!diag.has_errors());
        assert!(!graph.nodes.is_empty());
    }

    #[test]
    fn with_max_retries_configurable() {
        let pipeline = GenerationPipeline::new().with_max_retries(5);
        assert_eq!(pipeline.max_retries, 5);
    }

    #[test]
    fn default_pipeline() {
        let pipeline = GenerationPipeline::default();
        assert_eq!(pipeline.max_retries, 2);
    }
}
