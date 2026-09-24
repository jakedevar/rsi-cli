pub mod freeform;
pub mod refine;
pub mod select;
pub mod templates;

use crate::error::GraphError;
use crate::format::WorkflowDefinition;
use serde::{Deserialize, Serialize};

/// Result of workflow generation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenerateResult {
    pub workflow: WorkflowDefinition,
    pub reasoning: String,
    #[serde(default)]
    pub cache_hit: bool,
}

/// Complexity level to route between simple (topology selection) and complex (freeform) generation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Complexity {
    Simple,
    Complex,
}

/// Determine complexity from intent text. Simple heuristic.
pub fn classify_complexity(intent: &str) -> Complexity {
    let complex_signals = [
        "custom",
        "multi-stage",
        "parallel then",
        "fan-out",
        "nested",
        "subgraph",
        "complex",
        "critics",
        "brainstorm",
        "mixed",
    ];
    let word_count = intent.split_whitespace().count();

    if word_count > 30
        || complex_signals
            .iter()
            .any(|s| intent.to_lowercase().contains(s))
    {
        Complexity::Complex
    } else {
        Complexity::Simple
    }
}

/// Generate a workflow from natural language intent.
/// Routes between simple (topology selection) and complex (freeform) based on complexity.
pub fn generate_workflow(intent: &str) -> Result<GenerateResult, GraphError> {
    match classify_complexity(intent) {
        Complexity::Simple => {
            // Use topology selection via local keyword generation
            generate_workflow_local(intent)
        }
        Complexity::Complex => {
            // Use multi-stage pipeline
            let pipeline = freeform::GenerationPipeline::new();
            pipeline.generate(intent)
        }
    }
}

/// Simple keyword-based generation using topology selection.
fn generate_workflow_local(intent: &str) -> Result<GenerateResult, GraphError> {
    let intent_lower = intent.to_lowercase();

    let topology = if intent_lower.contains("review") && intent_lower.contains("specialist") {
        "hub"
    } else if intent_lower.contains("debate") {
        "pingpong"
    } else if intent_lower.contains("parallel") {
        "vertical"
    } else {
        "horizontal"
    };

    let mock = serde_json::json!({
        "topology": topology,
        "nodes": [
            {"id": "step_1", "name": "Step 1", "instructions": "Process"},
            {"id": "step_2", "name": "Step 2", "instructions": "Continue"},
        ],
        "reasoning": format!("Selected {} topology for intent", topology),
        "params": {}
    });

    select::parse_selection_response(&serde_json::to_string(&mock).unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_complexity_simple_for_short_intent() {
        assert_eq!(classify_complexity("review my code"), Complexity::Simple);
    }

    #[test]
    fn classify_complexity_complex_for_multi_stage() {
        assert_eq!(
            classify_complexity("Build a multi-stage pipeline"),
            Complexity::Complex
        );
    }

    #[test]
    fn classify_complexity_complex_for_long_intent() {
        let long = "word ".repeat(31);
        assert_eq!(classify_complexity(&long), Complexity::Complex);
    }

    #[test]
    fn classify_complexity_complex_for_brainstorm() {
        assert_eq!(
            classify_complexity("brainstorm ideas for the project"),
            Complexity::Complex
        );
    }

    #[test]
    fn classify_complexity_simple_for_normal_sentence() {
        assert_eq!(
            classify_complexity("Run tests and report results"),
            Complexity::Simple
        );
    }

    #[test]
    fn generate_workflow_simple_intent_uses_simple_path() {
        let result = generate_workflow("review my code").unwrap();
        // Simple path produces a workflow via topology selection
        assert!(!result.workflow.nodes.is_empty());
        assert!(!result.reasoning.is_empty());
    }

    #[test]
    fn generate_workflow_complex_intent_uses_complex_path() {
        let result = generate_workflow("multi-stage complex pipeline with critics").unwrap();
        // Complex path produces a workflow via freeform pipeline
        assert!(!result.workflow.nodes.is_empty());
        assert!(!result.reasoning.is_empty());
        // Freeform reasoning mentions "intent analysis"
        assert!(
            result.reasoning.contains("intent analysis"),
            "Complex path should mention intent analysis in reasoning, got: {}",
            result.reasoning
        );
    }

    #[test]
    fn generate_workflow_both_paths_produce_valid_definitions() {
        // Simple
        let simple = generate_workflow("run tests").unwrap();
        crate::compiler::compile(&simple.workflow).expect("Simple workflow should compile");

        // Complex
        let complex = generate_workflow("multi-stage processing pipeline").unwrap();
        crate::compiler::compile(&complex.workflow).expect("Complex workflow should compile");
    }
}
