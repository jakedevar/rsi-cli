//! Lightweight LLM client for dreamer background agent calls.
//!
//! Dream execution needs stable per-phase identity, admission immediately
//! before backend execution, and exact-once settlement even when a live cycle
//! is cancelled. This client owns that boundary.

use crate::bus::EventBus;
use crate::dreamer::state::DreamPhase;
use crate::error::{DaemonError, Result};
use crate::model_control::{
    AdmissionDecision, AdmissionPermit, InvocationCompletion, ModelAdmissionRequest,
    complete_invocation, completion_with_wall_time, hash_request_fingerprint,
};
use crate::store::Store;
use rsi_common::model_control::{InvocationOwner, ModelInvocationPurpose, ModelUsageConfidence};
use rsi_common::types::SessionProvider;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct DreamCallSuccess {
    pub raw_response: String,
    pub completion: InvocationCompletion,
    pub result_hash: String,
}

#[derive(Clone)]
pub struct DreamerLlmClient {
    pub(crate) store: Arc<Mutex<Store>>,
    pub(crate) event_bus: Arc<EventBus>,
    pub(crate) provider: SessionProvider,
    pub(crate) api_url: String,
    pub(crate) api_key: Option<String>,
    pub(crate) model: String,
    backend: Arc<dyn DreamCompletionBackend>,
}

#[derive(Debug, Clone)]
pub struct DreamCallContext {
    pub run_id: Uuid,
    pub phase: DreamPhase,
    pub item_key: String,
    pub owner: String,
    pub dedup_key: String,
    pub request_fingerprint: String,
    pub cancel: CancellationToken,
}

#[async_trait::async_trait]
pub(crate) trait DreamCompletionBackend: Send + Sync {
    async fn execute(
        &self,
        permit: &AdmissionPermit,
        target: &crate::memory::llm::MemoryLlmTarget,
        prompt: &str,
        max_tokens: u32,
        purpose: &str,
        cancel: &CancellationToken,
    ) -> Result<String>;
}

struct LiveDreamCompletionBackend;

#[async_trait::async_trait]
impl DreamCompletionBackend for LiveDreamCompletionBackend {
    async fn execute(
        &self,
        permit: &AdmissionPermit,
        target: &crate::memory::llm::MemoryLlmTarget,
        prompt: &str,
        max_tokens: u32,
        purpose: &str,
        cancel: &CancellationToken,
    ) -> Result<String> {
        crate::memory::llm::generate_text_cancellable(
            permit, target, prompt, max_tokens, purpose, cancel,
        )
        .await
    }
}

impl DreamerLlmClient {
    pub fn new(
        store: Arc<Mutex<Store>>,
        event_bus: Arc<EventBus>,
        provider: SessionProvider,
        api_url: String,
        api_key: Option<String>,
        model: String,
    ) -> Self {
        Self {
            store,
            event_bus,
            provider,
            api_url,
            api_key,
            model,
            backend: Arc::new(LiveDreamCompletionBackend),
        }
    }

    #[cfg(test)]
    pub(crate) fn new_with_backend(
        store: Arc<Mutex<Store>>,
        event_bus: Arc<EventBus>,
        provider: SessionProvider,
        api_url: String,
        api_key: Option<String>,
        model: String,
        backend: Arc<dyn DreamCompletionBackend>,
    ) -> Self {
        Self {
            store,
            event_bus,
            provider,
            api_url,
            api_key,
            model,
            backend,
        }
    }

    pub fn target(&self) -> crate::memory::llm::MemoryLlmTarget {
        crate::memory::llm::MemoryLlmTarget {
            provider: self.provider,
            model: self.model.clone(),
            base_url: if self.api_url.is_empty() {
                None
            } else {
                Some(self.api_url.clone())
            },
            api_key: self.api_key.clone(),
        }
    }

    pub fn request_fingerprint_for(&self, phase: DreamPhase, item_key: &str) -> String {
        self.request_fingerprint_for_prompts(phase, item_key, "", "", Uuid::nil())
    }

    pub fn request_fingerprint_for_prompts(
        &self,
        phase: DreamPhase,
        item_key: &str,
        system_prompt: &str,
        user_prompt: &str,
        run_id: Uuid,
    ) -> String {
        hash_request_fingerprint(&[
            provider_fingerprint_label(self.provider),
            self.model.as_str(),
            self.api_url.as_str(),
            phase.as_str(),
            item_key,
            run_id.to_string().as_str(),
            system_prompt,
            user_prompt,
        ])
    }

    pub async fn admit_with_context(
        &self,
        context: &DreamCallContext,
    ) -> Result<AdmissionDecision> {
        let target = self.target();
        let provider_label = crate::memory::llm::provider_label(&target)?;
        let backend_label = crate::memory::llm::backend_label(&target)?.to_string();
        let request = ModelAdmissionRequest {
            purpose: ModelInvocationPurpose::DreamConsolidation,
            provider: Some(provider_label.clone()),
            model: Some(target.model.clone()),
            backend: Some(backend_label.clone()),
            effort: None,
            trigger: "dream_consolidation".to_string(),
            owner: InvocationOwner {
                operator: Some(context.owner.clone()),
                ..InvocationOwner::default()
            },
            dedup_key: Some(context.dedup_key.clone()),
            request_fingerprint: Some(context.request_fingerprint.clone()),
            parent_invocation_id: None,
            retry_of_invocation_id: None,
            expected_usage: Some(crate::model_control::explicit_expected_usage(
                ModelInvocationPurpose::DreamConsolidation,
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
        crate::model_control::admit_invocation(&self.store, request, &self.event_bus).await
    }

    pub async fn execute_with_permit(
        &self,
        permit: &AdmissionPermit,
        context: &DreamCallContext,
        system_prompt: &str,
        user_prompt: &str,
        max_tokens: u32,
        input_estimate: u64,
    ) -> Result<DreamCallSuccess> {
        let prompt = format!("{system_prompt}\n\n{user_prompt}");
        let target = self.target();
        let started_at = std::time::Instant::now();
        let backend = Arc::clone(&self.backend);
        match backend
            .execute(
                permit,
                &target,
                &prompt,
                max_tokens,
                "Dream consolidation",
                &context.cancel,
            )
            .await
        {
            Ok(raw_response) => {
                let output_tokens = ((raw_response.len() + 3) / 4) as u64;
                let completion = InvocationCompletion {
                    input_tokens: Some(input_estimate),
                    output_tokens: Some(output_tokens),
                    ..completion_with_wall_time(started_at, None, ModelUsageConfidence::Estimated)
                };
                Ok(DreamCallSuccess {
                    result_hash: hash_request_fingerprint(&[raw_response.as_str()]),
                    raw_response,
                    completion,
                })
            }
            Err(error) => {
                let completion = InvocationCompletion {
                    input_tokens: Some(input_estimate),
                    ..completion_with_wall_time(
                        started_at,
                        Some(crate::model_control::classify_error_class(&error)),
                        ModelUsageConfidence::Partial,
                    )
                };
                complete_invocation(&self.store, permit, completion, &self.event_bus).await?;
                Err(error)
            }
        }
    }

    pub async fn complete_with_context(
        &self,
        context: &DreamCallContext,
        system_prompt: &str,
        user_prompt: &str,
        max_tokens: u32,
    ) -> Result<String> {
        let permit = match self.admit_with_context(context).await? {
            AdmissionDecision::Admitted(permit) => permit,
            AdmissionDecision::Duplicate { invocation_id } => {
                let duplicate_state = {
                    let guard = self.store.lock().await;
                    crate::model_control::lookup_existing_invocation_by_dedup(
                        &guard,
                        &context.dedup_key,
                    )?
                };
                let detail = duplicate_state
                    .map(|existing| {
                        format!(
                            "dream phase {} item {} already {} ({})",
                            context.phase.as_str(),
                            context.item_key,
                            existing.status,
                            existing.invocation_id
                        )
                    })
                    .unwrap_or_else(|| {
                        format!(
                            "duplicate dream phase {} item {} suppressed ({invocation_id})",
                            context.phase.as_str(),
                            context.item_key
                        )
                    });
                return Err(DaemonError::PolicyDenied(detail));
            }
        };
        let outcome = self
            .execute_with_permit(
                &permit,
                context,
                system_prompt,
                user_prompt,
                max_tokens,
                ((user_prompt.len() + 3) / 4) as u64,
            )
            .await?;
        complete_invocation(&self.store, &permit, outcome.completion, &self.event_bus).await?;
        Ok(outcome.raw_response)
    }

    /// Legacy helper retained for phase-unit tests that do not need explicit
    /// Dream run identity.
    pub async fn complete(&self, system_prompt: &str, user_prompt: &str) -> Result<String> {
        let prompt = format!("{system_prompt}\n\n{user_prompt}");
        let context = DreamCallContext {
            run_id: Uuid::nil(),
            phase: DreamPhase::Extraction,
            item_key: "legacy".to_string(),
            owner: "dream_legacy".to_string(),
            dedup_key: crate::model_control::stable_dedup_key(
                "dream-legacy",
                &[self.model.as_str(), self.api_url.as_str(), prompt.as_str()],
            ),
            request_fingerprint: self.request_fingerprint_for_prompts(
                DreamPhase::Extraction,
                "legacy",
                system_prompt,
                user_prompt,
                Uuid::nil(),
            ),
            cancel: CancellationToken::new(),
        };
        self.complete_with_context(&context, system_prompt, user_prompt, 4096)
            .await
    }
}

fn provider_fingerprint_label(provider: SessionProvider) -> &'static str {
    match provider {
        SessionProvider::Claude => "Claude",
        SessionProvider::Codex => "Codex",
        SessionProvider::Pioneer => "Pioneer",
        SessionProvider::OpenRouter => "OpenRouter",
        SessionProvider::Bedrock => "Bedrock",
        SessionProvider::Local => "Local",
        SessionProvider::Antigravity => "Antigravity",
        SessionProvider::CodexAppServer => "CodexAppServer",
        SessionProvider::Harness => "Harness",
        _ => "Other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::types::SessionProvider;
    use std::collections::VecDeque;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[derive(Default)]
    struct TestDreamCompletionBackend {
        calls: AtomicU32,
        responses: StdMutex<VecDeque<Result<String>>>,
    }

    impl TestDreamCompletionBackend {
        fn with_response(response: Result<String>) -> Arc<Self> {
            let backend = Arc::new(Self::default());
            backend.responses.lock().unwrap().push_back(response);
            backend
        }
    }

    #[async_trait::async_trait]
    impl DreamCompletionBackend for TestDreamCompletionBackend {
        async fn execute(
            &self,
            _permit: &AdmissionPermit,
            _target: &crate::memory::llm::MemoryLlmTarget,
            _prompt: &str,
            _max_tokens: u32,
            _purpose: &str,
            _cancel: &CancellationToken,
        ) -> Result<String> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok("ok".to_string()))
        }
    }

    #[test]
    fn test_client_creation() {
        let client = DreamerLlmClient::new(
            Arc::new(Mutex::new(Store::open_in_memory().expect("store"))),
            Arc::new(EventBus::new(8)),
            SessionProvider::Local,
            "http://localhost:8080/v1/chat/completions".to_string(),
            Some("test-key".to_string()),
            "test-model".to_string(),
        );
        assert_eq!(client.api_url, "http://localhost:8080/v1/chat/completions");
        assert_eq!(client.model, "test-model");
        assert_eq!(client.api_key.as_deref(), Some("test-key"));
    }

    #[tokio::test]
    async fn test_complete_with_context_uses_backend() {
        let backend = TestDreamCompletionBackend::with_response(Ok("response".to_string()));
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let client = DreamerLlmClient::new_with_backend(
            store,
            Arc::new(EventBus::new(8)),
            SessionProvider::Local,
            String::new(),
            None,
            "qwen3:14b".to_string(),
            backend.clone(),
        );
        let context = DreamCallContext {
            run_id: Uuid::new_v4(),
            phase: DreamPhase::Deduction,
            item_key: "project:none".to_string(),
            owner: "dream:test".to_string(),
            dedup_key: "dream:test:deduction".to_string(),
            request_fingerprint: "sha256:test".to_string(),
            cancel: CancellationToken::new(),
        };

        let result = client
            .complete_with_context(&context, "system", "user", 256)
            .await
            .expect("response");
        assert_eq!(result, "response");
        assert_eq!(backend.calls.load(Ordering::Relaxed), 1);
    }
}
