//! Credential validity and credit checks.
//!
//! Only `invalid` (401 or an unknown-key body) and `exhausted` (402,
//! `OpenRouter`'s `limit_remaining <= 0`, or a credit-exhausted body) are authoritative.
//! Everything transient (timeout, connect error, 429, 5xx, malformed body,
//! ambiguous 4xx) is `unknown` and never refuses a launch.

use super::secret::SecretString;
use super::slots::Slot;
use chrono::{DateTime, Utc};
use rsi_common::provider_credentials::{CredentialCheckClass, CredentialCheckMetadata};
use std::time::Duration;

/// Timeout for one probe, including the lazy launch-time check.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// A transient (`unknown`) result is rechecked at most this often.
pub const TRANSIENT_RECHECK_SECS: i64 = 60;
const MAX_PROBE_BODY_BYTES: usize = 64 * 1024;

/// Raw transport outcome of one probe; classification is separate so it can
/// be tested without a network.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProbeOutcome {
    Http {
        status: u16,
        body: Vec<u8>,
    },
    Timeout,
    Connect,
    Failed,
    /// No check endpoint is defined for this slot.
    Unsupported,
}

/// HTTP behind a trait so tests use a mock (no live network in tests).
#[async_trait::async_trait]
pub trait CredentialProbe: Send + Sync {
    async fn probe(&self, slot: Slot, secret: &SecretString) -> ProbeOutcome;
}

/// Production endpoints. Base URLs are fields so tests can point the real
/// HTTP implementation at a local mock server.
#[derive(Clone, Debug)]
pub struct HttpCredentialProbe {
    pub openrouter_base: String,
    pub anthropic_base: String,
    pub openai_base: String,
    /// `None` = derive `https://bedrock-runtime.<region>.amazonaws.com`.
    pub bedrock_base: Option<String>,
}

impl Default for HttpCredentialProbe {
    fn default() -> Self {
        Self {
            openrouter_base: "https://openrouter.ai".into(),
            anthropic_base: "https://api.anthropic.com".into(),
            openai_base: "https://api.openai.com".into(),
            bedrock_base: None,
        }
    }
}

#[async_trait::async_trait]
impl CredentialProbe for HttpCredentialProbe {
    async fn probe(&self, slot: Slot, secret: &SecretString) -> ProbeOutcome {
        let Ok(http) = reqwest::Client::builder()
            .connect_timeout(PROBE_TIMEOUT)
            .timeout(PROBE_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
        else {
            return ProbeOutcome::Failed;
        };
        let request = match slot {
            Slot::Openrouter => http
                .get(format!("{}/api/v1/key", self.openrouter_base))
                .bearer_auth(secret.expose()),
            Slot::Anthropic => http
                .get(format!("{}/v1/models", self.anthropic_base))
                .header("x-api-key", secret.expose())
                .header("anthropic-version", "2023-06-01"),
            Slot::Openai => http
                .get(format!("{}/v1/models", self.openai_base))
                .bearer_auth(secret.expose()),
            Slot::Bedrock => {
                let base = match &self.bedrock_base {
                    Some(base) => base.clone(),
                    None => match crate::bedrock::region() {
                        Ok(region) => format!("https://bedrock-runtime.{region}.amazonaws.com"),
                        Err(_) => return ProbeOutcome::Failed,
                    },
                };
                // 1-token probe on the default model.
                http.post(format!("{base}/openai/v1/chat/completions"))
                    .bearer_auth(secret.expose())
                    .json(&serde_json::json!({
                        "model": crate::bedrock::BEDROCK_DEFAULT_MODEL,
                        "max_tokens": 1,
                        "messages": [{"role": "user", "content": "ping"}],
                    }))
            }
            _ => return ProbeOutcome::Unsupported,
        };
        let response = match request.send().await {
            Ok(response) => response,
            Err(error) if error.is_timeout() => return ProbeOutcome::Timeout,
            Err(error) if error.is_connect() => return ProbeOutcome::Connect,
            Err(_) => return ProbeOutcome::Failed,
        };
        let status = response.status().as_u16();
        let mut body = Vec::new();
        let mut stream = futures::StreamExt::fuse(response.bytes_stream());
        while let Some(chunk) = futures::StreamExt::next(&mut stream).await {
            let Ok(chunk) = chunk else {
                return ProbeOutcome::Failed;
            };
            let room = MAX_PROBE_BODY_BYTES.saturating_sub(body.len());
            body.extend_from_slice(&chunk[..chunk.len().min(room)]);
            if body.len() >= MAX_PROBE_BODY_BYTES {
                break;
            }
        }
        ProbeOutcome::Http { status, body }
    }
}

fn body_says_exhausted(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    lower.contains("insufficient_quota")
        || lower.contains("insufficient credits")
        || lower.contains("credit balance is too low")
        || lower.contains("credits exhausted")
        || lower.contains("out of credits")
}

fn body_says_unknown_key(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    lower.contains("invalid_api_key")
        || lower.contains("invalid x-api-key")
        || lower.contains("incorrect api key")
        || lower.contains("authentication_error")
        || lower.contains("user not found")
}

/// The key authenticated but is not authorized for the probe's model (e.g.
/// Bedrock's 401 when an AWS service control policy denies `InvokeModel` on
/// the default model). That says nothing about other models, so it is not an
/// authoritative verdict on the key.
fn body_says_model_access_denied(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    lower.contains("\"code\":\"access_denied\"")
        || lower.contains("permission_denied_error")
        || lower.contains("is not authorized to perform")
}

/// Classify a probe outcome. Pure: no I/O, no clock.
#[must_use]
pub fn classify(
    slot: Slot,
    outcome: &ProbeOutcome,
    at: DateTime<Utc>,
    fingerprint: Option<&str>,
) -> CredentialCheckMetadata {
    use CredentialCheckClass::{Exhausted, Invalid, Unknown, Valid};
    let record =
        |class, http_status, credit_remaining, detail_code: &str| CredentialCheckMetadata {
            // The caller stamps the generation the check started under.
            generation: 0,
            at,
            class,
            http_status,
            credit_remaining,
            detail_code: detail_code.to_string(),
            fingerprint: fingerprint.map(str::to_owned),
        };
    match outcome {
        ProbeOutcome::Unsupported => record(Unknown, None, None, "no_check_endpoint"),
        ProbeOutcome::Timeout => record(Unknown, None, None, "timeout"),
        ProbeOutcome::Connect => record(Unknown, None, None, "connect_error"),
        ProbeOutcome::Failed => record(Unknown, None, None, "request_failed"),
        ProbeOutcome::Http { status, body } => {
            let status = *status;
            let text = String::from_utf8_lossy(body);
            match status {
                401 | 403 if body_says_model_access_denied(&text) => {
                    record(Unknown, Some(status), None, "model_access_denied")
                }
                401 => record(Invalid, Some(status), None, "http_401"),
                402 => record(Exhausted, Some(status), None, "http_402"),
                429 => record(Unknown, Some(status), None, "rate_limited"),
                500..=599 => record(Unknown, Some(status), None, "server_error"),
                200..=299 if slot == Slot::Openrouter => {
                    match serde_json::from_slice::<serde_json::Value>(body) {
                        Ok(value)
                            if value.get("data").is_some_and(serde_json::Value::is_object) =>
                        {
                            let remaining = value
                                .pointer("/data/limit_remaining")
                                .and_then(serde_json::Value::as_f64);
                            match remaining {
                                Some(left) if left <= 0.0 => {
                                    record(Exhausted, Some(status), Some(left), "limit_exhausted")
                                }
                                _ => record(Valid, Some(status), remaining, "ok"),
                            }
                        }
                        _ => record(Unknown, Some(status), None, "malformed_body"),
                    }
                }
                200..=299 => record(Valid, Some(status), None, "ok"),
                400..=499 if body_says_exhausted(&text) => {
                    record(Exhausted, Some(status), None, "credit_exhausted_body")
                }
                400..=499 if body_says_unknown_key(&text) => {
                    record(Invalid, Some(status), None, "unknown_key_body")
                }
                _ => record(Unknown, Some(status), None, "unclassified_status"),
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
pub(crate) mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// Scripted mock probe: pops one outcome per call and counts calls.
    #[derive(Default)]
    pub struct ScriptedProbe {
        pub outcomes: Mutex<VecDeque<ProbeOutcome>>,
        pub calls: std::sync::atomic::AtomicUsize,
        pub delay: Option<Duration>,
    }

    impl ScriptedProbe {
        pub fn new(outcomes: impl IntoIterator<Item = ProbeOutcome>) -> Self {
            Self {
                outcomes: Mutex::new(outcomes.into_iter().collect()),
                ..Self::default()
            }
        }
        pub fn calls(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl CredentialProbe for ScriptedProbe {
        async fn probe(&self, _slot: Slot, _secret: &SecretString) -> ProbeOutcome {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if let Some(delay) = self.delay {
                tokio::time::sleep(delay).await;
            }
            self.outcomes
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(ProbeOutcome::Failed)
        }
    }

    pub fn http(status: u16, body: &str) -> ProbeOutcome {
        ProbeOutcome::Http {
            status,
            body: body.as_bytes().to_vec(),
        }
    }

    fn class(slot: Slot, outcome: &ProbeOutcome) -> CredentialCheckClass {
        classify(slot, outcome, Utc::now(), None).class
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn only_401_402_and_exhausted_bodies_are_authoritative() {
        use CredentialCheckClass::*;
        assert_eq!(class(Slot::Anthropic, &http(401, "")), Invalid);
        assert_eq!(class(Slot::Openai, &http(402, "")), Exhausted);
        assert_eq!(
            class(
                Slot::Openai,
                &http(403, r#"{"error":{"code":"insufficient_quota"}}"#)
            ),
            Exhausted
        );
        assert_eq!(
            class(
                Slot::Openai,
                &http(400, r#"{"error":{"code":"invalid_api_key"}}"#)
            ),
            Invalid
        );
        for transient in [
            http(429, ""),
            http(500, ""),
            http(503, ""),
            http(403, "forbidden"),
            ProbeOutcome::Timeout,
            ProbeOutcome::Connect,
            ProbeOutcome::Failed,
        ] {
            assert_eq!(class(Slot::Openai, &transient), Unknown, "{transient:?}");
        }
        assert_eq!(class(Slot::Openai, &http(200, "{}")), Valid);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn bedrock_model_access_denial_does_not_invalidate_the_key() {
        // Verbatim shape of Bedrock's answer when an org SCP denies the probe
        // model: the bearer token authenticated, so other models still work.
        let body = r#"{"error":{"message":"User: arn:aws:sts::000000000000:assumed-role/Example/user is not authorized to perform: bedrock:InvokeModel on resource: arn:aws:bedrock:::foundation-model/openai.gpt-5.6-sol with an explicit deny in a service control policy","type":"permission_denied_error","param":null,"code":"access_denied"}}"#;
        let check = classify(Slot::Bedrock, &http(401, body), Utc::now(), None);
        assert_eq!(check.class, CredentialCheckClass::Unknown);
        assert_eq!(check.detail_code, "model_access_denied");
        assert_eq!(check.http_status, Some(401));
        assert_eq!(
            class(Slot::Bedrock, &http(401, "")),
            CredentialCheckClass::Invalid
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn openrouter_key_body_classifies_credit() {
        use CredentialCheckClass::*;
        let exhausted = classify(
            Slot::Openrouter,
            &http(
                200,
                r#"{"data":{"limit":10,"usage":10,"limit_remaining":0}}"#,
            ),
            Utc::now(),
            None,
        );
        assert_eq!(exhausted.class, Exhausted);
        assert_eq!(exhausted.credit_remaining, Some(0.0));
        let ok = classify(
            Slot::Openrouter,
            &http(
                200,
                r#"{"data":{"limit":10,"usage":2,"limit_remaining":8}}"#,
            ),
            Utc::now(),
            None,
        );
        assert_eq!(ok.class, Valid);
        assert_eq!(ok.credit_remaining, Some(8.0));
        let unlimited = classify(
            Slot::Openrouter,
            &http(
                200,
                r#"{"data":{"limit":null,"usage":2,"limit_remaining":null}}"#,
            ),
            Utc::now(),
            None,
        );
        assert_eq!(unlimited.class, Valid);
        assert_eq!(class(Slot::Openrouter, &http(200, "not json")), Unknown);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[tokio::test]
    async fn http_probe_sends_bearer_key_to_openrouter_key_endpoint() {
        use wiremock::matchers::{header, method, path};
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(method("GET"))
            .and(path("/api/v1/key"))
            .and(header("authorization", "Bearer sk-test-probe"))
            .respond_with(wiremock::ResponseTemplate::new(402))
            .expect(1)
            .mount(&server)
            .await;
        let probe = HttpCredentialProbe {
            openrouter_base: server.uri(),
            ..HttpCredentialProbe::default()
        };
        let outcome = probe
            .probe(Slot::Openrouter, &SecretString::new("sk-test-probe".into()))
            .await;
        assert_eq!(
            classify(Slot::Openrouter, &outcome, Utc::now(), None).class,
            CredentialCheckClass::Exhausted
        );
    }
}
