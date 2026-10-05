//! Per-invocation bounded retry delay; each attempt is admitted separately.

use super::errors::ProviderError;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub const MAX_RETRIES: u32 = 4;
pub const MAX_WAIT: Duration = Duration::from_secs(60);

pub fn delay(error: &ProviderError, retries_done: u32) -> Option<Duration> {
    if !error.retryable() || retries_done >= MAX_RETRIES {
        return None;
    }
    let floor = Duration::from_millis(125u64.saturating_mul(1u64 << retries_done.min(9)));
    let jitter_ms = u64::from(rand::random::<u16>())
        % u64::try_from(floor.as_millis())
            .unwrap_or(u64::MAX)
            .saturating_add(1);
    let backoff = floor + Duration::from_millis(jitter_ms);
    Some(
        Duration::from_millis(error.retry_after_ms.unwrap_or(0))
            .max(backoff)
            .min(MAX_WAIT),
    )
}

pub async fn wait(error: &ProviderError, retries_done: u32, cancel: &CancellationToken) -> bool {
    let Some(delay) = delay(error, retries_done) else {
        return false;
    };
    tokio::select! { biased; () = cancel.cancelled() => false, () = tokio::time::sleep(delay) => true }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::session::harness::errors::{ProviderError, ProviderErrorClass};
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test(start_paused = true)]
    async fn retry_after_and_cancellation() {
        let error = ProviderError {
            class: ProviderErrorClass::RateLimited,
            http_status: Some(429),
            retry_after_ms: Some(2000),
            detail_code: "rate_limited".into(),
        };
        let cancel = CancellationToken::new();
        let task = tokio::spawn({
            let cancel = cancel.clone();
            let error = error.clone();
            async move { wait(&error, 0, &cancel).await }
        });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(1999)).await;
        assert!(!task.is_finished());
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(task.await.unwrap());
        let task = tokio::spawn({
            let cancel = cancel.clone();
            async move { wait(&error, 0, &cancel).await }
        });
        tokio::task::yield_now().await;
        cancel.cancel();
        assert!(!task.await.unwrap());
    }

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
        };
        let execution =
            || ModelExecutionCapability::for_test(RuntimeExecutionRoute::SessionHarnessOpenAiHttp);
        let first = provider.chat(&request, execution()).await.unwrap_err();
        let error = ProviderError::from_daemon_error(&first).unwrap();
        assert_eq!(error.class, ProviderErrorClass::RateLimited);
        tokio::time::pause();
        let cancel = CancellationToken::new();
        let waiting = tokio::spawn(async move { wait(&error, 0, &cancel).await });
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
