//! Dialectic query engine -- agentic LLM loop for natural-language Q&A
//! against accumulated knowledge (memory files, sessions, projects).

pub mod prompts;
pub mod tools;

use crate::error::{DaemonError, Result};
use crate::memory::manager::MemoryManager;
use crate::model_control::registry::RuntimeExecutionRoute;
use crate::model_control::{
    AdmissionDecision, ModelAdmissionRequest, ModelExecutionCapability, classify_error_class,
    completion_with_wall_time, hash_request_fingerprint,
};
use crate::session::SessionManager;
use rsi_common::model_control::{InvocationOwner, ModelInvocationPurpose, ModelUsageConfidence};
use rsi_common::rpc::{DialecticSource, QueryMemoryResponse};
use rsi_common::types::SessionProvider;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Maximum number of agent loop iterations (tool calls) before forcing a final answer.
const MAX_TOOL_ITERATIONS: u32 = 8;

/// Agent loop timeout in seconds.
const AGENT_TIMEOUT_SECS: u64 = 30;

pub struct DialecticEngine {
    memory: Option<MemoryManager>,
    sessions: Arc<SessionManager>,
    /// OpenAI-compatible API URL for the agent LLM.
    api_url: String,
    api_key: Option<String>,
    model: String,
    client: reqwest::Client,
    max_iterations: u32,
}

#[derive(Debug, Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<String>,
    max_tokens: u32,
    temperature: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChatMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ToolCall {
    id: String,
    #[serde(rename = "type")]
    call_type: String,
    function: FunctionCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FunctionCall {
    name: String,
    arguments: String,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: ResponseMessage,
}

#[derive(Debug, Deserialize)]
struct ResponseMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ToolCall>>,
}

impl DialecticEngine {
    pub fn new(
        memory: Option<MemoryManager>,
        sessions: Arc<SessionManager>,
        api_url: String,
        api_key: Option<String>,
        model: String,
        max_iterations: u32,
    ) -> Self {
        Self {
            memory,
            sessions,
            api_url,
            api_key,
            model,
            client: reqwest::Client::new(),
            max_iterations: max_iterations.min(MAX_TOOL_ITERATIONS),
        }
    }

    /// Run the dialectic query: prefetch context, run agent loop, return answer.
    ///
    /// `project_id` enforces project scope for both the prefetch and the
    /// in-loop `search_memory` tool. When set, the agent cannot pivot into
    /// another project's sessions through `list_sessions`,
    /// `get_session_detail`, or `get_conversation_excerpt` either (plan
    /// §Phase 6).
    pub async fn query(
        &self,
        user_query: &str,
        project_id: Option<uuid::Uuid>,
        conversation_history: &[(String, String)],
    ) -> Result<QueryMemoryResponse> {
        // Apply timeout to the entire operation
        match tokio::time::timeout(
            std::time::Duration::from_secs(AGENT_TIMEOUT_SECS),
            self.query_inner(user_query, project_id, conversation_history),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(DaemonError::Process(
                "Dialectic query timed out after 30 seconds".to_string(),
            )),
        }
    }

    async fn query_inner(
        &self,
        user_query: &str,
        project_id: Option<uuid::Uuid>,
        conversation_history: &[(String, String)],
    ) -> Result<QueryMemoryResponse> {
        let mut sources: Vec<DialecticSource> = Vec::new();
        let mut tool_call_count: u32 = 0;

        // Step 1: Prefetch memory context (project-scoped when set)
        let prefetch_context = self.prefetch_memory(user_query, project_id).await;

        // Step 2: Build initial messages
        let mut messages = Vec::new();

        // System prompt with prefetched context
        let system_content = if prefetch_context.is_empty() {
            prompts::DIALECTIC_SYSTEM_PROMPT.to_string()
        } else {
            format!(
                "{}\n\n{}",
                prompts::DIALECTIC_SYSTEM_PROMPT,
                prefetch_context
            )
        };
        messages.push(ChatMessage {
            role: "system".to_string(),
            content: Some(system_content),
            tool_calls: None,
            tool_call_id: None,
        });

        // Include conversation history from prior turns
        for (role, content) in conversation_history {
            // Skip the current query (last user message) -- we'll add it below
            if role == "user" && content == user_query {
                continue;
            }
            messages.push(ChatMessage {
                role: role.clone(),
                content: Some(content.clone()),
                tool_calls: None,
                tool_call_id: None,
            });
        }

        // Current user query
        messages.push(ChatMessage {
            role: "user".to_string(),
            content: Some(user_query.to_string()),
            tool_calls: None,
            tool_call_id: None,
        });

        // Step 3: Agent loop
        let tool_defs = tools::tool_definitions();

        for _ in 0..self.max_iterations {
            let request = ChatRequest {
                model: self.model.clone(),
                messages: messages.clone(),
                tools: Some(tool_defs.clone()),
                tool_choice: None,
                max_tokens: 4096,
                temperature: 0.3,
            };

            let response = self.call_llm(&request).await?;

            let choice = response
                .choices
                .into_iter()
                .next()
                .ok_or_else(|| DaemonError::Process("LLM returned no choices".to_string()))?;

            // Check if the LLM wants to call tools
            if let Some(ref tool_calls) = choice.message.tool_calls {
                if !tool_calls.is_empty() {
                    // Add the assistant message with tool calls
                    messages.push(ChatMessage {
                        role: "assistant".to_string(),
                        content: choice.message.content.clone(),
                        tool_calls: Some(tool_calls.clone()),
                        tool_call_id: None,
                    });

                    // Execute each tool call
                    for tc in tool_calls {
                        let args: serde_json::Value =
                            serde_json::from_str(&tc.function.arguments).unwrap_or_default();

                        let (result, source) = tools::execute_tool(
                            &tc.function.name,
                            &args,
                            &self.memory,
                            &self.sessions,
                            project_id,
                        )
                        .await?;

                        if let Some(s) = source {
                            sources.push(s);
                        }

                        // Add tool result message
                        messages.push(ChatMessage {
                            role: "tool".to_string(),
                            content: Some(result),
                            tool_calls: None,
                            tool_call_id: Some(tc.id.clone()),
                        });

                        tool_call_count += 1;
                    }

                    // Continue the loop to let the LLM process tool results
                    continue;
                }
            }

            // No tool calls -- this is the final answer
            let answer = choice
                .message
                .content
                .unwrap_or_else(|| "(No response from model)".to_string());

            return Ok(QueryMemoryResponse {
                answer,
                sources,
                tool_calls: tool_call_count,
            });
        }

        // Reached max iterations -- force a response from the last context
        let final_request = ChatRequest {
            model: self.model.clone(),
            messages,
            tools: None, // No tools = force a text response
            tool_choice: None,
            max_tokens: 2048,
            temperature: 0.3,
        };

        let response = self.call_llm(&final_request).await?;
        let answer = response
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.content)
            .unwrap_or_else(|| {
                "(Max tool iterations reached, could not produce answer)".to_string()
            });

        Ok(QueryMemoryResponse {
            answer,
            sources,
            tool_calls: tool_call_count,
        })
    }

    /// Prefetch relevant memory chunks as seed context.
    ///
    /// When `project_id` is `Some`, the prefetch is restricted to that
    /// project. When `None`, the prefetch is unscoped (manual TUI use).
    async fn prefetch_memory(&self, query: &str, project_id: Option<uuid::Uuid>) -> String {
        let mm = match &self.memory {
            Some(m) => m,
            None => return String::new(),
        };

        match mm.search(query, Some(5), None, project_id).await {
            Ok(results) if !results.is_empty() => {
                let mut context = Vec::new();
                for r in &results {
                    context.push(format!(
                        "[{} lines {}-{} score={:.2}]\n{}",
                        r.path, r.start_line, r.end_line, r.score, r.snippet
                    ));
                }
                context.join("\n---\n")
            }
            _ => String::new(),
        }
    }

    /// Make a single chat completion call to the LLM.
    async fn call_llm(&self, request: &ChatRequest) -> Result<ChatResponse> {
        let target = crate::memory::llm::MemoryLlmTarget {
            provider: SessionProvider::Harness,
            model: self.model.clone(),
            base_url: Some(self.api_url.clone()),
            api_key: self.api_key.clone(),
        };
        let provider_label = crate::memory::llm::provider_label(&target)?;
        let backend_label = crate::memory::llm::backend_label(&target)?.to_string();
        let admission_request = ModelAdmissionRequest {
            purpose: ModelInvocationPurpose::DialecticQuery,
            provider: Some(provider_label.clone()),
            model: Some(target.model.clone()),
            backend: Some(backend_label.clone()),
            effort: None,
            trigger: "dialectic_query".to_string(),
            owner: InvocationOwner {
                operator: Some("query_memory".to_string()),
                ..InvocationOwner::default()
            },
            dedup_key: Some(crate::model_control::stable_dedup_key(
                "dialectic-query",
                &[
                    &self.model,
                    &self.api_url,
                    &format!("{:?}", request.messages),
                ],
            )),
            request_fingerprint: Some(hash_request_fingerprint(&[
                &self.model,
                &self.api_url,
                &format!("{:?}", request.messages),
            ])),
            parent_invocation_id: None,
            retry_of_invocation_id: None,
            expected_usage: Some(crate::model_control::explicit_expected_usage(
                ModelInvocationPurpose::DialecticQuery,
                Some(provider_label.as_str()),
                Some(backend_label.as_str()),
                Some(target.model.as_str()),
            )),
            baseline_input_tokens: 0,
            baseline_output_tokens: 0,
            baseline_cache_creation_tokens: 0,
            baseline_cache_read_tokens: 0,
            baseline_reasoning_tokens: 0,
            baseline_embedding_input_count: 0,
            baseline_wall_time_ms: 0,
        };
        let permit = match crate::model_control::admit_invocation(
            self.sessions.store(),
            admission_request,
            self.sessions.event_bus(),
        )
        .await?
        {
            AdmissionDecision::Admitted(permit) => permit,
            AdmissionDecision::Duplicate { invocation_id } => {
                return Err(DaemonError::PolicyDenied(format!(
                    "duplicate dialectic invocation suppressed ({invocation_id})"
                )));
            }
        };
        let admitted = self
            .sessions
            .guard_admitted_model_call(permit, RuntimeExecutionRoute::DialecticHttp)?;
        let (settlement, execution) = admitted.into_parts();
        let started_at = std::time::Instant::now();
        let response = self.send_admitted_request(request, execution).await;
        let completion = match &response {
            Ok(_) => completion_with_wall_time(started_at, None, ModelUsageConfidence::Partial),
            Err(error) => completion_with_wall_time(
                started_at,
                Some(classify_error_class(error)),
                ModelUsageConfidence::Partial,
            ),
        };
        settlement
            .settle_result(
                self.sessions.store(),
                self.sessions.event_bus(),
                completion,
                response,
                "Dialectic query",
            )
            .await
    }

    async fn send_admitted_request(
        &self,
        request: &ChatRequest,
        execution: ModelExecutionCapability,
    ) -> Result<ChatResponse> {
        let url = format!("{}/chat/completions", self.api_url.trim_end_matches('/'));

        let mut request_builder = self
            .client
            .post(&url)
            .timeout(std::time::Duration::from_secs(60))
            .json(request);

        if let Some(ref key) = self.api_key {
            request_builder = request_builder.header("Authorization", format!("Bearer {}", key));
        }

        let response = execution
            .bind_http(RuntimeExecutionRoute::DialecticHttp, request_builder)
            .send("Dialectic")
            .await
            .map_err(|e| DaemonError::Process(format!("Dialectic LLM call failed: {e}")))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(DaemonError::Process(format!(
                "Dialectic LLM returned {status}: {body}"
            )));
        }

        response.json::<ChatResponse>().await.map_err(|e| {
            DaemonError::Process(format!("Failed to parse dialectic LLM response: {e}"))
        })
    }
}

#[cfg(test)]
mod production_path_tests {
    use super::*;
    use crate::bus::EventBus;
    use crate::config::{Config, RuntimeConfig};
    use crate::store::Store;
    use rsi_common::model_control::ModelControlMode;
    use rusqlite::params;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;

    fn manager() -> (Arc<SessionManager>, TempDir) {
        let directory = TempDir::new().expect("temporary directory");
        let store = Store::open(&directory.path().join("rsi.db")).expect("store");
        let runtime_config = RuntimeConfig::from_config(&Config::from_env());
        let manager = SessionManager::new(
            Arc::new(EventBus::new(32)),
            store,
            false,
            directory.path().join("daemon.sock"),
            None,
            Vec::new(),
            runtime_config,
            directory.path().join("sandboxes"),
        )
        .expect("session manager");
        (Arc::new(manager), directory)
    }

    fn request() -> ChatRequest {
        ChatRequest {
            model: "test-model".to_string(),
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: Some("hello".to_string()),
                tool_calls: None,
                tool_call_id: None,
            }],
            tools: None,
            tool_choice: None,
            max_tokens: 32,
            temperature: 0.0,
        }
    }

    async fn response_server(
        status: &str,
        body: &'static str,
    ) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback listener");
        let address = listener.local_addr().expect("listener address");
        let requests = Arc::new(AtomicUsize::new(0));
        let server_requests = Arc::clone(&requests);
        let status = status.to_string();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("request connection");
            let mut request = [0_u8; 8192];
            let bytes = socket.read(&mut request).await.expect("request bytes");
            assert!(
                std::str::from_utf8(&request[..bytes])
                    .expect("UTF-8 request")
                    .starts_with("POST /v1/chat/completions HTTP/1.1")
            );
            server_requests.fetch_add(1, Ordering::SeqCst);
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("HTTP response");
        });
        (format!("http://{address}/v1"), requests, server)
    }

    async fn invocation_state(manager: &SessionManager) -> (String, Option<String>) {
        let store = manager.store().lock().await;
        store
            .conn
            .query_row(
                "SELECT status, error_class FROM model_invocations ORDER BY created_at DESC LIMIT 1",
                params![],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("invocation row")
    }

    #[tokio::test]
    async fn admitted_dialectic_request_uses_loopback_and_settles_completed() {
        let (api_url, requests, server) = response_server(
            "200 OK",
            r#"{"choices":[{"message":{"content":"ok","tool_calls":null}}]}"#,
        )
        .await;
        let (manager, _directory) = manager();
        let engine = DialecticEngine::new(
            None,
            Arc::clone(&manager),
            api_url,
            None,
            "test-model".into(),
            1,
        );

        let response = engine.call_llm(&request()).await.expect("dialectic call");
        assert_eq!(response.choices[0].message.content.as_deref(), Some("ok"));
        server.await.expect("loopback server");
        assert_eq!(requests.load(Ordering::SeqCst), 1);
        assert_eq!(
            invocation_state(&manager).await,
            ("completed".to_string(), None)
        );
    }

    #[tokio::test]
    async fn denied_dialectic_request_sends_nothing() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback listener");
        let address = listener.local_addr().expect("listener address");
        let (manager, _directory) = manager();
        manager
            .store()
            .lock()
            .await
            .set_model_control_mode(ModelControlMode::StopAll)
            .expect("stop all");
        let engine = DialecticEngine::new(
            None,
            Arc::clone(&manager),
            format!("http://{address}/v1"),
            None,
            "test-model".into(),
            1,
        );

        let error = engine
            .call_llm(&request())
            .await
            .expect_err("policy denial");
        assert!(matches!(error, DaemonError::PolicyDenied(_)));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), listener.accept())
                .await
                .is_err(),
            "denial must precede transport"
        );
    }

    #[tokio::test]
    async fn failed_dialectic_request_settles_failed_once() {
        let (api_url, requests, server) =
            response_server("500 Internal Server Error", "failure").await;
        let (manager, _directory) = manager();
        let engine = DialecticEngine::new(
            None,
            Arc::clone(&manager),
            api_url,
            None,
            "test-model".into(),
            1,
        );

        let error = engine.call_llm(&request()).await.expect_err("HTTP failure");
        assert!(matches!(error, DaemonError::Process(_)));
        server.await.expect("loopback server");
        assert_eq!(requests.load(Ordering::SeqCst), 1);
        let (status, error_class) = invocation_state(&manager).await;
        assert_eq!(status, "failed");
        assert!(error_class.is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn outer_timeout_settles_admitted_dialectic_invocation_exactly_once() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback listener");
        let address = listener.local_addr().expect("listener address");
        let (request_started_tx, request_started_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("request connection");
            let mut request = [0_u8; 8192];
            // Only the arrival of the request matters here, not its length;
            // bind the count so the partial-read lint stays satisfied.
            let _read = socket.read(&mut request).await.expect("request bytes");
            let _ = request_started_tx.send(());
            std::future::pending::<()>().await;
        });
        let (manager, _directory) = manager();
        let mut events = manager.event_bus().subscribe();
        let engine = DialecticEngine::new(
            None,
            Arc::clone(&manager),
            format!("http://{address}/v1"),
            None,
            "test-model".into(),
            1,
        );

        let query = tokio::spawn(async move { engine.query("hello", None, &[]).await });
        request_started_rx.await.expect("admitted request started");
        tokio::time::advance(std::time::Duration::from_secs(31)).await;
        let error = query.await.expect("query task").expect_err("outer timeout");
        assert!(error.to_string().contains("timed out after 30 seconds"));
        manager
            .drain_model_call_settlements()
            .await
            .expect("abandoned settlement drains");
        server.abort();
        let _ = server.await;

        let store = manager.store().lock().await;
        let (invocation_id, status, error_class, row_count): (String, String, Option<String>, i64) =
            store
                .conn
                .query_row(
                    "SELECT id, status, error_class, (SELECT COUNT(*) FROM model_invocations) \
                     FROM model_invocations ORDER BY created_at DESC LIMIT 1",
                    params![],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .expect("dialectic invocation row");
        drop(store);
        assert_eq!(status, "failed");
        assert_eq!(error_class.as_deref(), Some("model_call_task_exited"));
        assert_eq!(row_count, 1);

        let invocation_id = uuid::Uuid::parse_str(&invocation_id).expect("invocation UUID");
        let mut terminal_events = 0;
        while let Ok(event) = events.try_recv() {
            if matches!(
                event.as_ref(),
                crate::bus::DaemonEvent::ModelInvocationCompleted {
                    invocation_id: completed_id,
                    ..
                } if *completed_id == invocation_id
            ) {
                terminal_events += 1;
            }
        }
        manager.event_bus().unsubscribe();
        assert_eq!(terminal_events, 1);
    }
}
