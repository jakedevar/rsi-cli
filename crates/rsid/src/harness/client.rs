//! HarnessClient — bridges the harness into Flywheel's session system.

use crate::claude::{LaunchConfig, StreamEvent};
use crate::error::Result;
use crate::harness::agent_loop::{AgentLoopConfig, run_agent_loop};
use crate::harness::provider::resolve_provider;
use crate::harness::tools::HarnessToolRegistry;
use crate::memory::worker::MemoryHandle;
use crate::model_control::call_control::ModelCallControl;
use crate::provider_capabilities::resolve_fresh_context_budget;
use crate::session::types::HarnessProcess;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub struct HarnessClient;

impl HarnessClient {
    /// Launch a harness session. Returns a process handle and an event receiver.
    ///
    /// If `memory_handle` is provided, the `memory_search` tool is registered
    /// alongside the default built-in tools.
    pub fn launch(
        config: &LaunchConfig,
        memory_handle: Option<MemoryHandle>,
        model_call_control: Arc<dyn ModelCallControl>,
    ) -> Result<(HarnessProcess, mpsc::Receiver<StreamEvent>)> {
        let model = config
            .model
            .clone()
            .unwrap_or_else(|| "claude-sonnet-5".into());

        // Resolve which API backend to use
        let provider = resolve_provider(
            &model,
            config.openai_base_url.as_deref(),
            config.openai_api_key.as_deref(),
        )?;

        let (event_tx, event_rx) = mpsc::channel::<StreamEvent>(256);
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

        let working_dir = config
            .working_dir
            .clone()
            .unwrap_or_else(|| std::path::PathBuf::from("/tmp"));
        let query = config.query.clone();
        let system_prompt = config.system_prompt.clone();
        let token_limit =
            resolve_fresh_context_budget(rsi_common::types::SessionProvider::Harness, &model, None)
                .active_tokens;

        let loop_config = AgentLoopConfig {
            model,
            token_limit,
            reasoning_effort: config.effort.clone(),
            ..Default::default()
        };

        // Build tools
        let mut tools = HarnessToolRegistry::default_tools();
        if let Some(handle) = memory_handle {
            // Legacy continuation/rotation harness — project scope is not
            // available here. All current callers pass `None` for
            // memory_handle so the tool is never actually registered in
            // practice. If a future caller does wire a handle through this
            // path it MUST plumb the launching session's project_id; not
            // doing so re-introduces the cross-project leak this work fixes.
            tools.register(Arc::new(
                crate::harness::tools::memory::MemorySearchTool::new(handle, None),
            ));
        }

        let task = tokio::spawn(async move {
            if let Err(e) = run_agent_loop(
                provider.as_ref(),
                &tools,
                &loop_config,
                system_prompt.as_deref(),
                &query,
                None, // prior_history — set for continuation
                &working_dir,
                &event_tx,
                &cancel_clone,
                model_call_control,
            )
            .await
            {
                tracing::error!(error = %e, "Harness agent loop failed");
                let _ = event_tx
                    .send(StreamEvent {
                        event_type: "system".into(),
                        data: serde_json::json!({
                            "subtype": "error",
                            "error": e.to_string(),
                        }),
                    })
                    .await;
            }
        });

        Ok((
            HarnessProcess {
                task_handle: task,
                cancel,
            },
            event_rx,
        ))
    }
}

/// Return a static model list for the Harness provider.
/// Checks environment variables to determine which backends are available.
pub fn harness_models() -> Vec<(String, String)> {
    let mut models = Vec::new();

    if std::env::var("INCEPTION_API_KEY").is_ok() || std::env::var("MERCURY_API_KEY").is_ok() {
        models.push(("mercury-2".into(), "Mercury 2 (Inception Direct)".into()));
    }

    if std::env::var("ANTHROPIC_API_KEY").is_ok() {
        models.extend([
            ("claude-sonnet-5".into(), "Claude Sonnet 5 (Direct)".into()),
            (
                "claude-opus-4-20250514".into(),
                "Claude Opus 4 (Direct)".into(),
            ),
            (
                "claude-haiku-4-20250514".into(),
                "Claude Haiku 4 (Direct)".into(),
            ),
        ]);
    }

    if std::env::var("OPENAI_API_KEY").is_ok() {
        models.extend([
            ("gpt-4o".into(), "GPT-4o (Direct)".into()),
            ("o4-mini".into(), "o4-mini (Direct)".into()),
        ]);
    }

    // Always available: local Ollama fallback
    models.push(("local-model".into(), "Local Model (Ollama)".into()));

    models
}
