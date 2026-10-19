//! Per-invocation bounded retry delay; each attempt is admitted separately.
//!
//! The delay and wait algorithm lives in `rsid-harness`; the HTTP retry test stays here
//! because it drives a real provider.

#[allow(unused_imports)] // Preserves the old module path.
pub use rsid_harness::retry::{RetryBudget, wait};

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::session::harness::errors::{ProviderError, ProviderErrorClass, ProviderErrorExt};
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    async fn mock_http_429_retry_after_then_200() {
        use crate::model_control::ModelExecutionCapability;
        use crate::model_control::registry::RuntimeExecutionRoute;
        use crate::session::harness::api_key::ApiCredential;
        use crate::session::harness::provider::ApiProvider;
        use crate::session::harness::providers::openai_api::OpenAiApiProvider;
        use crate::session::harness::types::{ChatMessage, ChatRequest, ProviderQuirks};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let server = wiremock::MockServer::start().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let responder = Arc::clone(&calls);
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(move |_request: &wiremock::Request| {
                if responder.fetch_add(1, Ordering::SeqCst) == 0 {
                    wiremock::ResponseTemplate::new(429).insert_header("Retry-After", "2")
                } else {
                    wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "choices": [{"message": {"content": "ok"}, "finish_reason": "stop"}]
                    }))
                }
            })
            .mount(&server)
            .await;
        let provider = OpenAiApiProvider::with_config(
            server.uri(),
            ApiCredential::None,
            ProviderQuirks::default(),
        )
        .unwrap();
        let request = ChatRequest {
            messages: vec![ChatMessage::user("ping")],
            model: "test-model".into(),
            temperature: None,
            max_tokens: Some(8),
            tools: vec![],
            stream: false,
            reasoning_effort: None,
            context_editing: false,
            tool_choice_none: false,
        };
        let execution =
            || ModelExecutionCapability::for_test(RuntimeExecutionRoute::SessionHarnessOpenAiHttp);
        let first = provider.chat(&request, execution()).await.unwrap_err();
        let error = ProviderError::from_daemon_error(&first).unwrap();
        assert_eq!(error.class, ProviderErrorClass::RateLimited);
        tokio::time::pause();
        let cancel = CancellationToken::new();
        let delay = RetryBudget::default().next_delay(&error, false).unwrap();
        assert_eq!(delay, Duration::from_secs(2));
        let waiting = tokio::spawn(async move { wait(delay, &cancel).await });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(1999)).await;
        assert!(!waiting.is_finished());
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(waiting.await.unwrap());
        tokio::time::resume();
        assert_eq!(
            provider.chat(&request, execution()).await.unwrap().content,
            "ok"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}
