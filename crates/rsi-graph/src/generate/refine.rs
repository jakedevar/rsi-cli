//! Iterative refinement loop for workflow definitions.
//!
//! Takes an existing `WorkflowDefinition` + natural language edit instruction
//! and produces a modified definition. Supports multi-turn refinement via
//! `RefinementContext` which tracks the history of instructions and results.

use crate::compiler;
use crate::error::GraphError;
use crate::format::*;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Context for a refinement conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefinementContext {
    /// History of refinement instructions and results.
    pub history: Vec<RefinementTurn>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefinementTurn {
    pub instruction: String,
    pub result: WorkflowDefinition,
}

impl RefinementContext {
    pub fn new() -> Self {
        Self {
            history: Vec::new(),
        }
    }

    pub fn add_turn(&mut self, instruction: String, result: WorkflowDefinition) {
        self.history.push(RefinementTurn {
            instruction,
            result,
        });
    }

    pub fn last_result(&self) -> Option<&WorkflowDefinition> {
        self.history.last().map(|t| &t.result)
    }
}

impl Default for RefinementContext {
    fn default() -> Self {
        Self::new()
    }
}

/// Result of a refinement operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefineResult {
    pub workflow: WorkflowDefinition,
    pub changes_made: Vec<String>,
    pub valid: bool,
}

/// Apply a refinement instruction to a workflow.
/// In production, this would be an LLM call. For now, keyword-based transformations.
pub fn refine_workflow(
    workflow: &WorkflowDefinition,
    instruction: &str,
) -> Result<RefineResult, GraphError> {
    let mut refined = workflow.clone();
    let mut changes = Vec::new();
    let inst_lower = instruction.to_lowercase();

    // Add node
    if inst_lower.contains("add") && inst_lower.contains("node") {
        let name = extract_quoted_name(instruction).unwrap_or_else(|| "new_node".to_string());
        let id = name.to_lowercase().replace(' ', "_");
        let node = NodeDef::action(&id, &name);

        // Connect to last node if there are existing nodes
        if let Some(last) = refined.nodes.last() {
            let last_id = last.id.clone();
            refined.edges.push(EdgeDef::new(&last_id, &id));
            changes.push(format!("Added edge from '{}' to '{}'", last_id, id));
        }

        refined.nodes.push(node);
        changes.push(format!("Added node '{}'", name));
    }

    // Remove node
    if (inst_lower.contains("remove") || inst_lower.contains("delete"))
        && let Some(name) = extract_quoted_name(instruction)
    {
        let name_lower = name.to_lowercase();
        let before_count = refined.nodes.len();
        refined.nodes.retain(|n| {
            !n.id.to_lowercase().contains(&name_lower)
                && !n.name.to_lowercase().contains(&name_lower)
        });
        let removed = before_count - refined.nodes.len();
        if removed > 0 {
            // Clean up edges referencing removed nodes
            let remaining_ids: HashSet<&str> =
                refined.nodes.iter().map(|n| n.id.as_str()).collect();
            refined.edges.retain(|e| {
                remaining_ids.contains(e.source.as_str())
                    && remaining_ids.contains(e.target.as_str())
            });
            changes.push(format!("Removed {} node(s) matching '{}'", removed, name));
        }
    }

    // Change model
    if inst_lower.contains("model")
        && (inst_lower.contains("change")
            || inst_lower.contains("set")
            || inst_lower.contains("use"))
        && let Some(model_name) = extract_after_keyword(instruction, &["to", "use", "model"])
    {
        for node in &mut refined.nodes {
            let settings = node.model_settings.get_or_insert(ModelSettings {
                model: None,
                max_tokens: None,
                temperature: None,
                top_p: None,
                provider: None,
            });
            settings.model = Some(model_name.clone());
        }
        changes.push(format!("Set model to '{}' for all nodes", model_name));
    }

    // Rename
    if inst_lower.contains("rename")
        && let Some(new_name) = extract_quoted_name(instruction)
    {
        refined.name = new_name.clone();
        changes.push(format!("Renamed workflow to '{}'", new_name));
    }

    // Change instructions for a specific node
    if (inst_lower.contains("instructions") || inst_lower.contains("instruct"))
        && let Some(text) = extract_quoted_name(instruction)
    {
        // Apply to the first node that might be referenced
        if let Some(node) = refined.nodes.first_mut() {
            node.instructions = text.clone();
            changes.push(format!("Updated instructions for node '{}'", node.id));
        }
    }

    // If no changes were made, return the original with a note
    if changes.is_empty() {
        changes.push("No matching transformation found for instruction".to_string());
    }

    // Validate the refined workflow
    let valid = compiler::compile(&refined).is_ok();

    Ok(RefineResult {
        workflow: refined,
        changes_made: changes,
        valid,
    })
}

/// Extract a quoted name from instruction text.
fn extract_quoted_name(text: &str) -> Option<String> {
    // Try single quotes
    if let Some(start) = text.find('\'')
        && let Some(end) = text[start + 1..].find('\'')
    {
        return Some(text[start + 1..start + 1 + end].to_string());
    }
    // Try double quotes
    if let Some(start) = text.find('"')
        && let Some(end) = text[start + 1..].find('"')
    {
        return Some(text[start + 1..start + 1 + end].to_string());
    }
    None
}

/// Extract a value after a keyword sequence.
/// Scans for the last keyword match and returns the word after it.
/// This handles "change model to opus" by finding "to" (last keyword) and returning "opus".
fn extract_after_keyword(text: &str, keywords: &[&str]) -> Option<String> {
    let words: Vec<&str> = text.split_whitespace().collect();
    let mut last_match_idx = None;
    for (i, word) in words.iter().enumerate() {
        if keywords.contains(&word.to_lowercase().as_str()) {
            last_match_idx = Some(i);
        }
    }
    if let Some(idx) = last_match_idx
        && let Some(next) = words.get(idx + 1)
    {
        return Some(
            next.trim_matches(|c: char| !c.is_alphanumeric() && c != '-' && c != '_')
                .to_string(),
        );
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: build a simple 2-node workflow for testing.
    fn two_node_workflow() -> WorkflowDefinition {
        WorkflowDefinition::new("test-workflow")
            .with_node(NodeDef::action("step1", "Step 1").with_instructions("Do step 1"))
            .with_node(NodeDef::action("step2", "Step 2").with_instructions("Do step 2"))
            .with_edge(EdgeDef::new("step1", "step2"))
    }

    #[test]
    fn add_node_to_workflow() {
        let wf = two_node_workflow();
        let result = refine_workflow(&wf, "add node 'Quality Check'").unwrap();

        assert_eq!(result.workflow.nodes.len(), 3);
        let added = result.workflow.nodes.last().unwrap();
        assert_eq!(added.name, "Quality Check");
        assert_eq!(added.id, "quality_check");
        assert!(
            result
                .changes_made
                .iter()
                .any(|c| c.contains("Quality Check"))
        );
        assert!(result.valid);
    }

    #[test]
    fn remove_node_and_edges() {
        let wf = two_node_workflow();
        let result = refine_workflow(&wf, "remove 'step2'").unwrap();

        assert_eq!(result.workflow.nodes.len(), 1);
        assert_eq!(result.workflow.nodes[0].id, "step1");
        // Edge from step1 -> step2 should be cleaned up
        assert!(result.workflow.edges.is_empty());
        assert!(result.changes_made.iter().any(|c| c.contains("Removed")));
    }

    #[test]
    fn change_model_for_all_nodes() {
        let wf = two_node_workflow();
        let result = refine_workflow(&wf, "change model to opus").unwrap();

        for node in &result.workflow.nodes {
            let settings = node.model_settings.as_ref().unwrap();
            assert_eq!(settings.model, Some("opus".to_string()));
        }
        assert!(result.changes_made.iter().any(|c| c.contains("opus")));
    }

    #[test]
    fn rename_workflow() {
        let wf = two_node_workflow();
        let result = refine_workflow(&wf, "rename to 'My Pipeline'").unwrap();

        assert_eq!(result.workflow.name, "My Pipeline");
        assert!(
            result
                .changes_made
                .iter()
                .any(|c| c.contains("My Pipeline"))
        );
    }

    #[test]
    fn multi_turn_refinement_accumulates() {
        let wf = two_node_workflow();

        // Turn 1: add a node
        let r1 = refine_workflow(&wf, "add node 'Review'").unwrap();
        assert_eq!(r1.workflow.nodes.len(), 3);

        // Turn 2: rename the workflow (using result of turn 1)
        let r2 = refine_workflow(&r1.workflow, "rename to 'Review Pipeline'").unwrap();
        assert_eq!(r2.workflow.nodes.len(), 3); // node from turn 1 preserved
        assert_eq!(r2.workflow.name, "Review Pipeline"); // turn 2 change applied
    }

    #[test]
    fn unmentioned_nodes_preserved() {
        let wf = WorkflowDefinition::new("preserve-test")
            .with_node(NodeDef::action("a", "Node A").with_instructions("Do A"))
            .with_node(NodeDef::action("b", "Node B").with_instructions("Do B"))
            .with_node(NodeDef::action("c", "Node C").with_instructions("Do C"))
            .with_edge(EdgeDef::new("a", "b"))
            .with_edge(EdgeDef::new("b", "c"));

        // Remove only node B
        let result = refine_workflow(&wf, "remove 'Node B'").unwrap();
        assert_eq!(result.workflow.nodes.len(), 2);
        assert!(result.workflow.nodes.iter().any(|n| n.id == "a"));
        assert!(result.workflow.nodes.iter().any(|n| n.id == "c"));
        // Node A and C instructions preserved
        let node_a = result.workflow.nodes.iter().find(|n| n.id == "a").unwrap();
        assert_eq!(node_a.instructions, "Do A");
        let node_c = result.workflow.nodes.iter().find(|n| n.id == "c").unwrap();
        assert_eq!(node_c.instructions, "Do C");
    }

    #[test]
    fn invalid_refinement_returns_noop() {
        let wf = two_node_workflow();
        let result = refine_workflow(&wf, "do something completely unrelated").unwrap();

        // Should not crash, returns original workflow with no-op message
        assert_eq!(result.workflow.nodes.len(), 2);
        assert!(
            result
                .changes_made
                .iter()
                .any(|c| c.contains("No matching transformation"))
        );
    }

    #[test]
    fn refinement_context_tracks_history() {
        let mut ctx = RefinementContext::new();
        assert!(ctx.last_result().is_none());

        let wf1 = two_node_workflow();
        ctx.add_turn("initial generation".to_string(), wf1.clone());
        assert_eq!(ctx.history.len(), 1);
        assert_eq!(ctx.last_result().unwrap().name, "test-workflow");

        let r1 = refine_workflow(&wf1, "rename to 'Updated'").unwrap();
        ctx.add_turn("rename to 'Updated'".to_string(), r1.workflow.clone());
        assert_eq!(ctx.history.len(), 2);
        assert_eq!(ctx.last_result().unwrap().name, "Updated");
    }
}
