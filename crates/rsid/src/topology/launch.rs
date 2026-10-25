//! One launch configuration for both first attempts and retries.

use crate::claude::LaunchConfig;
use crate::error::{DaemonError, Result};
use crate::model_control::hash_request_fingerprint;
use crate::topology::custody::{NodeForkPlan, TopologyCustody, TopologyForkSource};
use rsi_common::model_control::ModelInvocationPurpose;
use rsi_common::types::{SandboxKind, SandboxSpec, SessionKind};
use rsi_graph::format::NodeDef;
use serde_json::Value;
use uuid::Uuid;

pub(crate) struct NodeLaunchBuilder<'a> {
    pub(crate) custody: &'a TopologyCustody,
    pub(crate) is_topology: bool,
    pub(crate) workflow_id: Uuid,
    pub(crate) project_id: Option<Uuid>,
    pub(crate) parent_id: Option<Uuid>,
}

/// Byte ceiling for a topology node session title (matches the generated-title
/// ceiling so the session list reads the same).
const NODE_TITLE_MAX_BYTES: usize = 72;

/// #3222: the deterministic title of a topology node session,
/// `<node_id> #<issue>: <issue title prefix>`. The node's query opens with the
/// untrusted-data envelope, so a model-generated title named an implement
/// node "Reviewer: Issue Data Validation". The node id and the execution's
/// Issue snapshot already say what the session is; no model call is needed.
pub(crate) fn node_session_title(node_id: &str, input: Option<&Value>) -> String {
    let issue = input.and_then(|input| input.get(crate::topology::starters::ISSUE_INPUT_KEY));
    let number = issue
        .and_then(|issue| issue.get("display_number"))
        .and_then(Value::as_i64);
    let issue_title = issue
        .and_then(|issue| issue.get("title"))
        .and_then(Value::as_str)
        .map(|title| title.split_whitespace().collect::<Vec<_>>().join(" "))
        .unwrap_or_default();
    let mut title = node_id.trim().to_owned();
    if let Some(number) = number {
        title.push_str(&format!(" #{number}"));
    }
    if !issue_title.is_empty() {
        title.push_str(": ");
        title.push_str(&issue_title);
    }
    if title.len() > NODE_TITLE_MAX_BYTES {
        let mut end = NODE_TITLE_MAX_BYTES;
        while !title.is_char_boundary(end) {
            end -= 1;
        }
        title.truncate(end);
        title.truncate(title.trim_end().len());
    }
    title
}

struct NodeLaunchOptions {
    iteration: u32,
    kind: SessionKind,
    effort: Option<String>,
    model: Option<String>,
    provider: Option<rsi_common::types::SessionProvider>,
    key: String,
    fingerprint: String,
}

impl NodeLaunchBuilder<'_> {
    pub(crate) fn build(
        &self,
        node: &NodeDef,
        query: String,
        iteration: u32,
        retry: u32,
        fork_plan: &NodeForkPlan,
    ) -> Result<(LaunchConfig, TopologyForkSource)> {
        let fork = fork_plan.resolve(self.custody, iteration)?;
        let key = format!(
            "workflow.graph.node:{}:{}:{}:{}",
            self.custody.execution_id, node.id, iteration, retry
        );
        let options = self.options(node, &query, iteration, retry, key)?;
        Ok((self.launch_config(node, query, options), fork))
    }

    /// Durable executor launch: the fork is already resolved from persisted
    /// pins, and the dedup key is the attempt's reserved key, so a recovery
    /// relaunch re-presents exactly the same admission identity.
    pub(crate) fn build_attempt(
        &self,
        node: &NodeDef,
        query: String,
        iteration: u32,
        attempt_no: u32,
        dedup_key: String,
    ) -> Result<LaunchConfig> {
        let options = self.options(node, &query, iteration, attempt_no, dedup_key)?;
        Ok(self.launch_config(node, query, options))
    }

    fn options(
        &self,
        node: &NodeDef,
        query: &str,
        iteration: u32,
        ordinal: u32,
        key: String,
    ) -> Result<NodeLaunchOptions> {
        let kind = self.resolve_kind(node)?;
        let effort = node
            .tags
            .iter()
            .find_map(|tag| tag.strip_prefix("effort="))
            .map(str::to_owned);
        let model = node.model_settings.as_ref().and_then(|ms| ms.model.clone());
        let provider = crate::session::graph_runner::resolve_provider(node);
        let fingerprint = hash_request_fingerprint(&[
            ModelInvocationPurpose::WorkflowGraphNode.as_str(),
            &self.custody.execution_id.to_string(),
            &node.id,
            &iteration.to_string(),
            &ordinal.to_string(),
            model.as_deref().unwrap_or(""),
            query,
        ]);
        Ok(NodeLaunchOptions {
            iteration,
            kind,
            effort,
            model,
            provider,
            key,
            fingerprint,
        })
    }

    fn resolve_kind(&self, node: &NodeDef) -> Result<SessionKind> {
        let kind = if self.is_topology {
            node.tags
                .iter()
                .find_map(|tag| tag.strip_prefix("kind:"))
                .map(|name| serde_json::from_str::<SessionKind>(&format!("\"{name}\"")))
                .transpose()
                .map_err(|_| DaemonError::InvalidParam("invalid topology node kind".into()))?
                .unwrap_or(SessionKind::Standard)
        } else {
            SessionKind::Standard
        };
        if !rsi_common::is_leaf_kind(kind) {
            return Err(DaemonError::InvalidParam(
                "topology node kind must be spawnable".into(),
            ));
        }
        Ok(kind)
    }

    fn launch_config(
        &self,
        node: &NodeDef,
        query: String,
        options: NodeLaunchOptions,
    ) -> LaunchConfig {
        LaunchConfig {
            completion_gates: None,
            query,
            title: None,
            agent_role: None,
            epic_spawn_ordinal: None,
            working_dir: Some(self.custody.repo_root.clone()),
            provider: options.provider,
            model: options.model,
            configured_context_window: None,
            project_id: self.project_id,
            workflow_id: Some(self.workflow_id),
            workflow_id_override: None,
            session_kind: Some(options.kind),
            system_prompt: None,
            max_turns: None,
            resume_session_id: None,
            rsi_session_id: None,
            rsi_socket: None,
            rsi_session_token: None,
            continued_from: None,
            openai_base_url: None,
            openai_api_key: None,
            conversation_history: None,
            max_retries: None,
            group_id: None,
            parent_id: self.parent_id,
            effort: options.effort,
            issue_identifier: None,
            issue_url: None,
            issue_tracker_id: None,
            scheduled_job_id: None,
            model_invocation_owner: None,
            model_invocation_dedup_key: Some(options.key),
            model_invocation_request_fingerprint: Some(options.fingerprint),
            skip_project_model_default: false,
            tool_policy: None,
            model_invocation_purpose: ModelInvocationPurpose::WorkflowGraphNode,
            sandbox: Some(SandboxSpec {
                kind: Some(SandboxKind::GitWorktree),
                branch: None,
            }),
            cargo_target_dir: None,
            execution_scratch: None,
            is_eval: false,
            skip_context_pipeline: false,
            capability_class: None,
            tags: vec![],
            topology_node_id: Some(node.id.clone()),
            topology_iteration: options.iteration,
            closure_selector: None,
            structured_handoff: false,
            exec_target: rsi_common::exec_target::ExecTarget::Host,
            bench_profile: None,
            loop_knobs: rsi_common::bench_profile::LoopKnobs::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    use super::*;
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    use serde_json::json;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn title_names_node_issue_number_and_title() {
        let input =
            json!({"issue": {"display_number": 3222, "title": "kaizen:  topology\nnode titles"}});
        assert_eq!(
            node_session_title("implement", Some(&input)),
            "implement #3222: kaizen: topology node titles"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn title_without_an_issue_is_the_node_id() {
        assert_eq!(node_session_title("review", None), "review");
        assert_eq!(
            node_session_title("review", Some(&json!({"x": 1}))),
            "review"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn title_is_clamped_at_a_char_boundary() {
        let input = json!({"issue": {"display_number": 7, "title": "é".repeat(100)}});
        let title = node_session_title("implement", Some(&input));
        assert!(title.len() <= NODE_TITLE_MAX_BYTES);
        assert!(title.starts_with("implement #7: é"));
    }
}
