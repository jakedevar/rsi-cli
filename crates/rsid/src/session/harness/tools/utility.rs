use super::{HarnessTool, ToolContext, ToolExecutionMode};
use crate::claude::StreamEvent;
use crate::session::harness::types::ToolResult;
use serde::Deserialize;
use serde_json::json;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

pub(crate) const PLAN_STREAM_EVENT: &str = "plan_update";
pub(crate) const ROTATION_REQUEST_STREAM_EVENT: &str = "rotation_request";

#[derive(Debug, Default)]
pub(crate) struct UtilityToolState {
    used_tokens: AtomicU64,
    token_limit: AtomicU64,
    compaction_focus: tokio::sync::Mutex<Option<String>>,
}

impl UtilityToolState {
    pub(crate) fn set_context_usage(&self, used_tokens: u64, token_limit: u64) {
        self.used_tokens.store(used_tokens, Ordering::Release);
        self.token_limit.store(token_limit, Ordering::Release);
    }

    pub(crate) async fn set_compaction_focus(&self, focus: String) {
        *self.compaction_focus.lock().await = Some(focus);
    }

    pub(crate) async fn take_compaction_focus(&self) -> Option<String> {
        self.compaction_focus.lock().await.take()
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanStep {
    step: String,
    status: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdatePlanArgs {
    plan: Vec<PlanStep>,
    #[serde(default)]
    explanation: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CompactArgs {
    focus: String,
    #[serde(default)]
    fresh_window: bool,
}

fn error(message: impl Into<String>) -> ToolResult {
    ToolResult {
        success: false,
        output: String::new(),
        error_msg: Some(message.into()),
    }
}

pub(crate) struct UpdatePlanTool;

#[async_trait::async_trait]
impl HarnessTool for UpdatePlanTool {
    fn name(&self) -> &str {
        "update_plan"
    }

    fn description(&self) -> &str {
        "Publish the current task plan. The plan is persisted as a session event, rendered in session detail, and replayed after rotation."
    }

    fn parameters_json(&self) -> &str {
        r#"{"type":"object","additionalProperties":false,"required":["plan"],"properties":{"plan":{"type":"array","minItems":1,"items":{"type":"object","additionalProperties":false,"required":["step","status"],"properties":{"step":{"type":"string","minLength":1},"status":{"type":"string","minLength":1}}}},"explanation":{"type":"string"}}}"#
    }

    async fn execute(&self, _args: serde_json::Value, _working_dir: &Path) -> ToolResult {
        unreachable!("context execution must be used")
    }

    async fn execute_with_context(
        &self,
        args: serde_json::Value,
        context: &ToolContext,
    ) -> ToolResult {
        let parsed: UpdatePlanArgs = match serde_json::from_value(args) {
            Ok(parsed) => parsed,
            Err(message) => return error(format!("invalid update_plan arguments: {message}")),
        };
        if parsed
            .plan
            .iter()
            .any(|step| step.step.trim().is_empty() || step.status.trim().is_empty())
        {
            return error("update_plan steps and statuses must be nonempty");
        }

        let mut content = if parsed.explanation.is_empty() {
            "Plan updated.".to_string()
        } else {
            format!("Plan updated: {}", parsed.explanation)
        };
        for step in &parsed.plan {
            content.push_str(&format!("\n- [{}] {}", step.status, step.step));
        }

        let plan = parsed
            .plan
            .iter()
            .map(|step| json!({"step": step.step, "status": step.status}))
            .collect::<Vec<_>>();
        let event = StreamEvent {
            event_type: PLAN_STREAM_EVENT.into(),
            data: json!({
                "explanation": parsed.explanation,
                "plan": plan,
            }),
        };
        if let Some(sink) = &context.event_sink
            && sink.send(event).await.is_err()
        {
            return error("plan event sink closed");
        }

        ToolResult {
            success: true,
            output: "Plan updated.".into(),
            error_msg: None,
        }
    }
}

pub(crate) struct ContextStatusTool {
    state: Arc<UtilityToolState>,
}

impl ContextStatusTool {
    pub(crate) fn new(state: Arc<UtilityToolState>) -> Self {
        Self { state }
    }
}

#[async_trait::async_trait]
impl HarnessTool for ContextStatusTool {
    fn name(&self) -> &str {
        "context_status"
    }

    fn description(&self) -> &str {
        "Return the live context token count, resolved context limit, and fill percentage used by RSI."
    }

    fn parameters_json(&self) -> &str {
        r#"{"type":"object","additionalProperties":false,"properties":{}}"#
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::ParallelSafe
    }

    async fn execute(&self, _args: serde_json::Value, _working_dir: &Path) -> ToolResult {
        let used_tokens = self.state.used_tokens.load(Ordering::Acquire);
        let limit_tokens = self.state.token_limit.load(Ordering::Acquire);
        let used_percent = if limit_tokens == 0 {
            0.0
        } else {
            (used_tokens as f64 * 100.0 / limit_tokens as f64).min(100.0)
        };
        ToolResult {
            success: true,
            output: json!({
                "used_tokens": used_tokens,
                "limit_tokens": limit_tokens,
                "used_percent": used_percent,
            })
            .to_string(),
            error_msg: None,
        }
    }
}

pub(crate) struct CompactTool {
    state: Arc<UtilityToolState>,
}

impl CompactTool {
    pub(crate) fn new(state: Arc<UtilityToolState>) -> Self {
        Self { state }
    }
}

#[async_trait::async_trait]
impl HarnessTool for CompactTool {
    fn name(&self) -> &str {
        "compact"
    }

    fn description(&self) -> &str {
        "Request focused conversation compaction. Set fresh_window=true to route through RSI context rotation with a handoff instead of resetting this session."
    }

    fn parameters_json(&self) -> &str {
        r#"{"type":"object","additionalProperties":false,"required":["focus"],"properties":{"focus":{"type":"string","minLength":1},"fresh_window":{"type":"boolean","default":false}}}"#
    }

    async fn execute(&self, _args: serde_json::Value, _working_dir: &Path) -> ToolResult {
        unreachable!("context execution must be used")
    }

    async fn execute_with_context(
        &self,
        args: serde_json::Value,
        context: &ToolContext,
    ) -> ToolResult {
        let parsed: CompactArgs = match serde_json::from_value(args) {
            Ok(parsed) => parsed,
            Err(message) => return error(format!("invalid compact arguments: {message}")),
        };
        if parsed.focus.trim().is_empty() {
            return error("compact focus must be nonempty");
        }

        self.state.set_compaction_focus(parsed.focus.clone()).await;
        if parsed.fresh_window
            && let Some(sink) = &context.event_sink
            && sink
                .send(StreamEvent {
                    event_type: ROTATION_REQUEST_STREAM_EVENT.into(),
                    data: json!({"focus": parsed.focus}),
                })
                .await
                .is_err()
        {
            return error("rotation request event sink closed");
        }

        ToolResult {
            success: true,
            output: if parsed.fresh_window {
                "Fresh context window requested through RSI rotation.".into()
            } else {
                "Focused compaction requested for the next model request.".into()
            },
            error_msg: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::harness::tools::HarnessToolRegistry;
    use std::path::Path;
    use tokio_util::sync::CancellationToken;

    fn registry() -> HarnessToolRegistry {
        let state = Arc::new(UtilityToolState::default());
        let mut registry = HarnessToolRegistry::new();
        registry.utility_state = Arc::clone(&state);
        registry.register(Arc::new(UpdatePlanTool));
        registry.register(Arc::new(ContextStatusTool::new(Arc::clone(&state))));
        registry.register(Arc::new(CompactTool::new(state)));
        registry
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[tokio::test]
    async fn update_plan_emits_a_typed_plan_stream_event() {
        let registry = registry();
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(1);
        let result = registry
            .execute_with_context(
                "update_plan",
                json!({
                    "plan": [
                        {"step": "Inspect mailbox", "status": "in_progress"},
                        {"step": "Inject mail", "status": "pending"}
                    ],
                    "explanation": "Continue the harness boundary work"
                }),
                Path::new("/tmp"),
                &CancellationToken::new(),
                Some(event_tx),
            )
            .await;

        assert!(!result.is_error());
        let event = event_rx.recv().await.expect("plan stream event");
        assert_eq!(event.event_type, PLAN_STREAM_EVENT);
        assert_eq!(
            event.data["explanation"],
            "Continue the harness boundary work"
        );
        assert_eq!(event.data["plan"][0]["step"], "Inspect mailbox");
        assert_eq!(event.data["plan"][1]["status"], "pending");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[tokio::test]
    async fn update_plan_rejects_blank_steps() {
        let registry = registry();
        let result = registry
            .execute_with_context(
                "update_plan",
                json!({"plan": [{"step": " ", "status": "pending"}]}),
                Path::new("/tmp"),
                &CancellationToken::new(),
                None,
            )
            .await;

        assert!(result.is_error());
        assert_eq!(
            result.error_msg.as_deref(),
            Some("update_plan steps and statuses must be nonempty")
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[tokio::test]
    async fn context_status_reports_the_shared_harness_context_figures() {
        let registry = registry();
        registry.set_context_usage(25_000, 100_000);
        let result = registry
            .execute_with_context(
                "context_status",
                json!({}),
                Path::new("/tmp"),
                &CancellationToken::new(),
                None,
            )
            .await;

        assert!(!result.is_error());
        let output: serde_json::Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(output["used_tokens"], 25_000);
        assert_eq!(output["limit_tokens"], 100_000);
        assert_eq!(output["used_percent"], 25.0);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[tokio::test]
    async fn compact_queues_focus_and_fresh_rotation_requests_rotation() {
        let registry = registry();
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(1);
        let result = registry
            .execute_with_context(
                "compact",
                json!({"focus": "Preserve failing tests and migration steps", "fresh_window": true}),
                Path::new("/tmp"),
                &CancellationToken::new(),
                Some(event_tx),
            )
            .await;

        assert!(!result.is_error());
        let event = event_rx.recv().await.expect("rotation request event");
        assert_eq!(event.event_type, ROTATION_REQUEST_STREAM_EVENT);
        assert_eq!(
            event.data["focus"],
            "Preserve failing tests and migration steps"
        );
        assert_eq!(
            registry.take_compaction_focus().await.as_deref(),
            Some("Preserve failing tests and migration steps")
        );
        assert_eq!(registry.take_compaction_focus().await, None);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[tokio::test]
    async fn compact_rejects_a_blank_focus() {
        let registry = registry();
        let result = registry
            .execute_with_context(
                "compact",
                json!({"focus": " "}),
                Path::new("/tmp"),
                &CancellationToken::new(),
                None,
            )
            .await;

        assert!(result.is_error());
        assert_eq!(
            result.error_msg.as_deref(),
            Some("compact focus must be nonempty")
        );
    }
}
