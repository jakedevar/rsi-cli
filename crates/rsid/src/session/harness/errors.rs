//! Safe provider failure classification shared by the HTTP adapters and loop.

use crate::error::DaemonError;
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderErrorClass {
    Auth,
    CreditExhausted,
    RateLimited,
    Overloaded,
    Transient,
    ContextTooLong,
    BadRequest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderError {
    pub class: ProviderErrorClass,
    pub http_status: Option<u16>,
    pub retry_after_ms: Option<u64>,
    pub detail_code: String,
}

impl ProviderError {
    pub const fn retryable(&self) -> bool {
        matches!(
            self.class,
            ProviderErrorClass::RateLimited
                | ProviderErrorClass::Overloaded
                | ProviderErrorClass::Transient
        )
    }

    pub fn into_daemon_error(self) -> DaemonError {
        let data = serde_json::json!({"kind": "provider_error", "provider_error": self});
        DaemonError::StructuredRpc {
            rpc_code: -32000,
            message: format!("provider_error:{}", self.detail_code),
            data,
        }
    }

    pub fn from_daemon_error(error: &DaemonError) -> Option<Self> {
        match error {
            DaemonError::StructuredRpc { data, .. }
                if data.get("kind")?.as_str()? == "provider_error" =>
            {
                serde_json::from_value(data.get("provider_error")?.clone()).ok()
            }
            _ => None,
        }
    }
}

pub fn from_http(status: u16, body: &str, retry_after: Option<&str>) -> ProviderError {
    let body = body.to_ascii_lowercase();
    let credit = [
        "insufficient_quota",
        "insufficient credits",
        "credit balance",
        "credits exhausted",
        "out of credits",
        "credit limit",
        "key limit",
        "limit_remaining",
        "payment required",
    ]
    .iter()
    .any(|term| body.contains(term));
    let auth = [
        "invalid_api_key",
        "invalid x-api-key",
        "incorrect api key",
        "authentication_error",
        "unknown key",
    ]
    .iter()
    .any(|term| body.contains(term));
    let context = [
        "context_length_exceeded",
        "context window",
        "context length",
        "too many tokens",
        "prompt is too long",
    ]
    .iter()
    .any(|term| body.contains(term));
    let (class, detail_code) = if status == 401 || auth {
        (ProviderErrorClass::Auth, "invalid_key")
    } else if status == 402 || credit || (status == 403 && body.contains("limit")) {
        (ProviderErrorClass::CreditExhausted, "credit_exhausted")
    } else if context {
        (ProviderErrorClass::ContextTooLong, "context_too_long")
    } else if status == 429 {
        (ProviderErrorClass::RateLimited, "rate_limited")
    } else if matches!(status, 503 | 529) {
        (ProviderErrorClass::Overloaded, "provider_overloaded")
    } else if matches!(status, 500 | 502 | 504) {
        (ProviderErrorClass::Transient, "server_error")
    } else {
        (ProviderErrorClass::BadRequest, "request_rejected")
    };
    ProviderError {
        class,
        http_status: Some(status),
        retry_after_ms: retry_after
            .and_then(parse_retry_after)
            .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)),
        detail_code: detail_code.into(),
    }
}

pub fn transport(error: DaemonError) -> DaemonError {
    match &error {
        DaemonError::Process(message) if message.contains(" HTTP error:") => ProviderError {
            class: ProviderErrorClass::Transient,
            http_status: None,
            retry_after_ms: None,
            detail_code: "transport_error".into(),
        }
        .into_daemon_error(),
        _ => error,
    }
}

pub fn parse_retry_after(value: &str) -> Option<Duration> {
    if let Ok(seconds) = value.trim().parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let at = chrono::DateTime::parse_from_rfc2822(value.trim()).ok()?;
    let now = chrono::Utc::now();
    let remaining = at
        .signed_duration_since(now)
        .num_milliseconds()
        .max(0)
        .cast_unsigned();
    Some(Duration::from_millis(remaining))
}

pub async fn classify_response(response: reqwest::Response) -> DaemonError {
    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let body = response.text().await.unwrap_or_default();
    let bounded: String = body.chars().take(64 * 1024).collect();
    from_http(status, &bounded, retry_after.as_deref()).into_daemon_error()
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[test]
    fn provider_status_and_body_table() {
        use ProviderErrorClass::*;
        for (status, body, expected) in [
            (401, "{}", Auth),
            (400, r#"{"type":"authentication_error"}"#, Auth),
            (402, "{}", CreditExhausted),
            (403, "credit balance is too low", CreditExhausted),
            (403, "OpenRouter key limit reached", CreditExhausted),
            (403, "OpenRouter limit exceeded", CreditExhausted),
            (403, "moderation rejected", BadRequest),
            (429, "rate limit", RateLimited),
            (529, "overloaded", Overloaded),
            (503, "unavailable", Overloaded),
            (500, "internal", Transient),
            (502, "gateway", Transient),
            (504, "timeout", Transient),
            (400, "context_length_exceeded", ContextTooLong),
            (400, "bad model", BadRequest),
            (429, "insufficient_quota", CreditExhausted),
        ] {
            assert_eq!(
                from_http(status, body, None).class,
                expected,
                "{status} {body}"
            );
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[test]
    fn retry_after_accepts_seconds_and_http_date() {
        assert_eq!(parse_retry_after("3"), Some(Duration::from_secs(3)));
        let date = (chrono::Utc::now() + chrono::Duration::seconds(5)).to_rfc2822();
        let parsed = parse_retry_after(&date).expect("HTTP date");
        assert!(parsed >= Duration::from_secs(3));
        assert!(parsed <= Duration::from_secs(5));
    }
}
