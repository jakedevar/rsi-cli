//! Exa adapter for `web_search` (#2252; plan
//! `thoughts/shared/plans/2026-10-13-rsi-native-harness.md` §6 S2b).
//!
//! Inception's search cookbook pairs Mercury with Exa, so this is the second
//! [`SearchBackend`] after [`super::serper`]. The request shape follows Exa's
//! public `POST /search` documentation: `type:"auto"` and
//! `contents.highlights`, so each hit carries query-relevant excerpts and the
//! model needs fewer follow-up fetches [inferred from the vendor docs; no
//! request is made by any test].
//!
//! Safety properties match the Serper adapter:
//!
//! - The target is the constant [`EXA_SEARCH_URL`], posted by the one
//!   production sink [`ExaBackend::post_search`], which the model-control
//!   validator registers as a non-model POST (`registry::NON_INVOCATIONS`). A
//!   `cfg(test)` twin posts to a loopback server instead. The client follows
//!   no redirects, has connect and total timeouts and reads at most
//!   [`MAX_RESPONSE_BYTES`].
//! - The key is read from the vault slot on every call
//!   ([`ApiCredential::for_slot_only`]: no generic fallback), is sent only in
//!   the `x-api-key` header (marked sensitive), and appears in no argument,
//!   output, event or error text. Vendor error bodies are truncated and have
//!   the key scrubbed. Transport errors are reported by category only.

use super::serper::{MAX_RESPONSE_BYTES, transport_error, vendor_message};
use super::{Recency, SearchBackend, SearchError, SearchHit, SearchRequest, SearchResults};
use crate::session::harness::api_key::ApiCredential;
use chrono::{DateTime, Duration as ChronoDuration, SecondsFormat, Utc};
use reqwest::header::{HeaderValue, RETRY_AFTER};
use serde_json::{Value, json};
use std::time::Duration;

/// The Exa search endpoint. A direct string literal: the model-control
/// validator reads this constant to prove the sink never reaches a model API.
const EXA_SEARCH_URL: &str = "https://api.exa.ai/search";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
/// Most characters of highlight text Exa returns per result.
const HIGHLIGHT_MAX_CHARACTERS: u32 = 600;
/// Highlight excerpts joined into one snippet.
const MAX_HIGHLIGHTS: usize = 3;
const USER_AGENT: &str = "rsi-harness-web-search";

pub struct ExaBackend {
    client: reqwest::Client,
    credential: ApiCredential,
    /// Test seam: post here instead of [`EXA_SEARCH_URL`].
    #[cfg(test)]
    test_url: Option<String>,
}

impl ExaBackend {
    /// # Errors
    /// The HTTP client could not be built (TLS initialization failed).
    pub fn new(credential: ApiCredential) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(USER_AGENT)
            .build()
            .map_err(|error| error.without_url().to_string())?;
        Ok(Self {
            client,
            credential,
            #[cfg(test)]
            test_url: None,
        })
    }

    /// A backend that posts to `{base_url}/search` instead of Exa.
    #[cfg(test)]
    pub(crate) fn with_base_url(credential: ApiCredential, base_url: String) -> Self {
        let mut backend = Self::new(credential).expect("test HTTP client");
        backend.test_url = Some(format!("{base_url}/search"));
        backend
    }

    /// The one production sink: a POST to the constant Exa URL. Keep it a
    /// single direct `.post(EXA_SEARCH_URL)...send()` chain; the model-control
    /// validator proves the target from the constant.
    async fn post_search(
        &self,
        key: HeaderValue,
        body: &Value,
    ) -> Result<reqwest::Response, reqwest::Error> {
        self.client
            .post(EXA_SEARCH_URL)
            .header("x-api-key", key)
            .json(body)
            .send()
            .await
    }

    /// Test twin of [`Self::post_search`] aimed at a loopback server.
    #[cfg(test)]
    async fn post_search_loopback(
        &self,
        url: &str,
        key: HeaderValue,
        body: &Value,
    ) -> Result<reqwest::Response, reqwest::Error> {
        self.client
            .post(url)
            .header("x-api-key", key)
            .json(body)
            .send()
            .await
    }
}

#[async_trait::async_trait]
impl SearchBackend for ExaBackend {
    fn id(&self) -> &'static str {
        "exa"
    }

    async fn search(&self, request: &SearchRequest) -> Result<SearchResults, SearchError> {
        let Some(secret) = self.credential.current() else {
            return Err(SearchError::Unavailable(
                "no Exa key is configured; the operator can add one under Provider Keys"
                    .to_string(),
            ));
        };
        let mut key = HeaderValue::from_str(secret.expose()).map_err(|_| {
            SearchError::Failed(
                "the configured Exa key is not a valid HTTP header value".to_string(),
            )
        })?;
        key.set_sensitive(true);

        let request_json = request_body(request, Utc::now());
        #[cfg(test)]
        let sent = match &self.test_url {
            Some(url) => self.post_search_loopback(url, key, &request_json).await,
            None => self.post_search(key, &request_json).await,
        };
        #[cfg(not(test))]
        let sent = self.post_search(key, &request_json).await;
        let mut response = sent.map_err(|error| SearchError::Failed(transport_error(&error)))?;

        let status = response.status().as_u16();
        match status {
            401 | 403 => return Err(SearchError::AuthFailed { status }),
            429 => {
                let retry_after_secs = response
                    .headers()
                    .get(RETRY_AFTER)
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.trim().parse::<u64>().ok());
                return Err(SearchError::RateLimited { retry_after_secs });
            }
            _ => {}
        }

        let mut body: Vec<u8> = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| SearchError::Failed(transport_error(&error)))?
        {
            if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
                return Err(SearchError::Failed(format!(
                    "the response is larger than {MAX_RESPONSE_BYTES} bytes"
                )));
            }
            body.extend_from_slice(&chunk);
        }

        if !(200..300).contains(&status) {
            return Err(SearchError::Failed(format!(
                "HTTP {status}: {}",
                vendor_message(&body, secret.expose())
            )));
        }
        let data: Value = serde_json::from_slice(&body)
            .map_err(|_| SearchError::Failed("the response was not valid JSON".to_string()))?;
        Ok(normalize(&data))
    }
}

/// The earliest publication time `recency` asks for, as Exa's ISO 8601 date.
fn start_published(recency: Recency, now: DateTime<Utc>) -> String {
    let days = match recency {
        Recency::Day => 1,
        Recency::Week => 7,
        Recency::Month => 30,
        Recency::Year => 365,
    };
    (now - ChronoDuration::days(days)).to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn request_body(request: &SearchRequest, now: DateTime<Utc>) -> Value {
    let mut body = json!({
        "query": request.query,
        "type": "auto",
        "numResults": request.max_results,
        "contents": {"highlights": {"maxCharacters": HIGHLIGHT_MAX_CHARACTERS}},
    });
    if !request.allowed_domains.is_empty() {
        body["includeDomains"] = json!(request.allowed_domains);
    }
    if !request.blocked_domains.is_empty() {
        body["excludeDomains"] = json!(request.blocked_domains);
    }
    if let Some(recency) = request.recency {
        body["startPublishedDate"] = json!(start_published(recency, now));
    }
    body
}

/// Fold an Exa response into [`SearchResults`]. Exa has no answer box or
/// related questions, so only `hits` is filled.
fn normalize(data: &Value) -> SearchResults {
    let text = |value: &Value, key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    };
    let mut results = SearchResults::default();
    for item in data
        .get("results")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(url) = text(item, "url") else {
            continue;
        };
        let highlights: Vec<&str> = item
            .get("highlights")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|highlight| !highlight.is_empty())
            .take(MAX_HIGHLIGHTS)
            .collect();
        results.hits.push(SearchHit {
            title: text(item, "title").unwrap_or_else(|| "Untitled".to_string()),
            url,
            snippet: highlights.join(" ... "),
            // `2026-10-03T08:00:00.000Z` -> `2026-10-03`.
            date: text(item, "publishedDate").map(|date| {
                date.split_once('T')
                    .map_or(date.clone(), |(day, _)| day.to_string())
            }),
        });
    }
    results
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// The fake search key every test sends; it must appear only in the
    /// `x-api-key` header the mock vendor receives.
    pub(crate) const SENTINEL: &str = "exa-fake-key-0123456789abcdef";

    pub(crate) fn credential() -> ApiCredential {
        ApiCredential::explicit(Some(SENTINEL))
    }

    pub(crate) fn mock_backend(server: &MockServer) -> ExaBackend {
        ExaBackend::with_base_url(credential(), server.uri())
    }

    /// An Exa-shaped response with a highlighted hit and a bare one.
    pub(crate) fn ok_body() -> Value {
        json!({
            "requestId": "req-fake",
            "resolvedSearchType": "neural",
            "results": [
                {
                    "title": "Mercury 2 by Inception",
                    "url": "http://public.test/notes",
                    "publishedDate": "2026-10-03T08:00:00.000Z",
                    "author": "Inception",
                    "highlights": ["A diffusion LLM.", "  ", "Fast parallel decoding."],
                    "highlightScores": [0.9, 0.5, 0.4]
                },
                {"title": "Docs", "url": "https://docs.inceptionlabs.ai/"}
            ]
        })
    }

    pub(crate) async fn mount_ok(server: &MockServer, body: Value) {
        Mock::given(method("POST"))
            .and(path("/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::test_support::{SENTINEL, credential, mock_backend, mount_ok, ok_body};
    use super::*;
    use crate::session::harness::tools::web_search::WebSearchTool;
    use crate::session::harness::tools::{HarnessTool, ToolContext, ToolPolicy};
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn request(query: &str) -> SearchRequest {
        SearchRequest {
            query: query.to_string(),
            max_results: 5,
            allowed_domains: Vec::new(),
            blocked_domains: Vec::new(),
            recency: None,
        }
    }

    fn at(rfc3339: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn a_search_posts_the_documented_request_and_normalizes_the_response() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/search"))
            .and(header("x-api-key", SENTINEL))
            .and(header("content-type", "application/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
            .expect(1)
            .mount(&server)
            .await;
        let results = mock_backend(&server)
            .search(&SearchRequest {
                max_results: 3,
                allowed_domains: vec!["inceptionlabs.ai".into()],
                recency: Some(Recency::Week),
                ..request("mercury diffusion llm")
            })
            .await
            .unwrap();

        assert!(results.answer_lines.is_empty() && results.related_questions.is_empty());
        assert_eq!(results.hits.len(), 2);
        assert_eq!(
            results.hits[0],
            SearchHit {
                title: "Mercury 2 by Inception".into(),
                url: "http://public.test/notes".into(),
                snippet: "A diffusion LLM. ... Fast parallel decoding.".into(),
                date: Some("2026-10-03".into()),
            }
        );
        assert_eq!(results.hits[1].snippet, "");
        assert_eq!(results.hits[1].date, None);

        let sent = &server.received_requests().await.unwrap()[0];
        let body: Value = serde_json::from_slice(&sent.body).unwrap();
        assert_eq!(body["query"], "mercury diffusion llm");
        assert_eq!(body["type"], "auto");
        assert_eq!(body["numResults"], 3);
        assert_eq!(body["includeDomains"], json!(["inceptionlabs.ai"]));
        assert_eq!(
            body["contents"]["highlights"]["maxCharacters"],
            HIGHLIGHT_MAX_CHARACTERS
        );
        assert!(body["startPublishedDate"].is_string());
        // `expect(1)` is verified when the server drops.
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn filters_and_recency_become_vendor_parameters() {
        let now = at("2026-10-13T12:30:45.123Z");
        let plain = request_body(&request("q"), now);
        assert_eq!(
            plain,
            json!({
                "query": "q",
                "type": "auto",
                "numResults": 5,
                "contents": {"highlights": {"maxCharacters": HIGHLIGHT_MAX_CHARACTERS}}
            })
        );
        let allowed = request_body(
            &SearchRequest {
                allowed_domains: vec!["a.test".into(), "b.test".into()],
                ..request("q")
            },
            now,
        );
        assert_eq!(allowed["includeDomains"], json!(["a.test", "b.test"]));
        assert!(allowed.get("excludeDomains").is_none());
        let blocked = request_body(
            &SearchRequest {
                blocked_domains: vec!["spam.test".into()],
                ..request("q")
            },
            now,
        );
        assert_eq!(blocked["excludeDomains"], json!(["spam.test"]));
        assert!(blocked.get("includeDomains").is_none());
        for (recency, expected) in [
            (Recency::Day, "2026-10-12T12:30:45.123Z"),
            (Recency::Week, "2026-10-06T12:30:45.123Z"),
            (Recency::Month, "2026-09-13T12:30:45.123Z"),
            (Recency::Year, "2025-10-13T12:30:45.123Z"),
        ] {
            let body = request_body(
                &SearchRequest {
                    recency: Some(recency),
                    ..request("q")
                },
                now,
            );
            assert_eq!(body["startPublishedDate"], expected);
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn partial_responses_normalize_without_panicking() {
        assert_eq!(normalize(&json!({})), SearchResults::default());
        assert_eq!(normalize(&json!("nonsense")), SearchResults::default());
        let results = normalize(&json!({
            "results": [
                {"title": "no url, skipped"},
                {"url": "https://only-url.test/", "highlights": "not a list"},
                {"title": "  ", "url": "https://blank-title.test/", "publishedDate": "2026-01-02"}
            ]
        }));
        let titles: Vec<&str> = results.hits.iter().map(|h| h.title.as_str()).collect();
        assert_eq!(titles, ["Untitled", "Untitled"]);
        assert_eq!(results.hits[0].snippet, "");
        assert_eq!(results.hits[1].date.as_deref(), Some("2026-01-02"));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn vendor_failures_map_to_typed_errors_that_never_carry_the_key() {
        let server = MockServer::start().await;
        let backend = mock_backend(&server);
        let cases: Vec<(ResponseTemplate, &str)> = vec![
            (
                ResponseTemplate::new(401).set_body_string("bad"),
                "web_search_auth_failed: the exa backend rejected the configured key (HTTP 401)",
            ),
            (
                ResponseTemplate::new(403).set_body_string("Unauthorized."),
                "web_search_auth_failed",
            ),
            (
                ResponseTemplate::new(429).insert_header("Retry-After", "7"),
                "web_search_rate_limited: the exa backend is rate limiting this key; retry after 7s",
            ),
            (ResponseTemplate::new(429), "retry in a few seconds"),
            (
                ResponseTemplate::new(402).set_body_json(
                    json!({"error": "Insufficient credits", "tag": "NO_MORE_CREDITS"}),
                ),
                "web_search_failed: HTTP 402: Insufficient credits",
            ),
            (
                ResponseTemplate::new(500).set_body_string("upstream\nexploded"),
                "web_search_failed: HTTP 500: upstream exploded",
            ),
            (
                ResponseTemplate::new(302)
                    .insert_header("Location", "http://127.0.0.1:9/elsewhere"),
                "web_search_failed: HTTP 302",
            ),
            (
                ResponseTemplate::new(200).set_body_string("<html>"),
                "not valid JSON",
            ),
        ];
        for (template, expected) in cases {
            server.reset().await;
            Mock::given(method("POST"))
                .and(path("/search"))
                .respond_with(template)
                .mount(&server)
                .await;
            let error = backend.search(&request("q")).await.unwrap_err();
            let message = error.clone().into_message("exa");
            assert!(message.contains(expected), "{expected}: {message}");
            assert!(!message.contains(SENTINEL), "{message}");
            // A redirect is reported, never followed.
            assert_eq!(server.received_requests().await.unwrap().len(), 1);
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn a_vendor_that_echoes_the_key_is_scrubbed_and_long_bodies_are_cut() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "error": format!("invalid key {SENTINEL} supplied {}", "x".repeat(2_000))
            })))
            .mount(&server)
            .await;
        let error = mock_backend(&server)
            .search(&request("q"))
            .await
            .unwrap_err();
        let SearchError::Failed(message) = &error else {
            panic!("{error:?}");
        };
        assert!(
            message.contains("invalid key [redacted] supplied"),
            "{message}"
        );
        assert!(!message.contains(SENTINEL));
        assert!(message.chars().count() < 260, "{}", message.len());
        assert!(!format!("{error:?}").contains(SENTINEL));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn an_oversized_response_is_refused_not_buffered() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string("a".repeat(MAX_RESPONSE_BYTES + 1)),
            )
            .mount(&server)
            .await;
        let error = mock_backend(&server)
            .search(&request("q"))
            .await
            .unwrap_err();
        assert!(
            matches!(&error, SearchError::Failed(text) if text.contains("larger than")),
            "{error:?}"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn a_missing_key_is_unavailable_and_sends_nothing() {
        let server = MockServer::start().await;
        mount_ok(&server, ok_body()).await;
        let keyless = ExaBackend::with_base_url(ApiCredential::explicit(None), server.uri());
        let error = keyless.search(&request("q")).await.unwrap_err();
        assert!(matches!(error, SearchError::Unavailable(_)), "{error:?}");
        assert!(
            error
                .into_message("exa")
                .starts_with("web_search_unavailable:")
        );
        assert!(server.received_requests().await.unwrap().is_empty());

        // A header-invalid key is refused locally.
        let broken =
            ExaBackend::with_base_url(ApiCredential::explicit(Some("bad\nkey")), server.uri());
        let error = broken.search(&request("q")).await.unwrap_err();
        assert!(matches!(error, SearchError::Failed(_)), "{error:?}");
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    /// Acceptance 2: the fake key reaches the vendor header and nothing the
    /// model, the stream or the log can see.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn the_tool_output_never_contains_the_key() {
        let server = MockServer::start().await;
        mount_ok(&server, ok_body()).await;
        let tool = WebSearchTool::new(Arc::new(mock_backend(&server)));
        let context = ToolContext {
            session_id: None,
            working_dir: std::env::temp_dir(),
            cancel: CancellationToken::new(),
            event_sink: None,
            policy: ToolPolicy::default(),
        };
        let result = tool
            .execute_with_context(json!({"query": "mercury", "recency": "week"}), &context)
            .await;
        assert!(result.success, "{:?}", result.error_msg);
        assert!(result.output.contains("backend: exa"), "{}", result.output);
        assert!(
            result
                .output
                .contains("A diffusion LLM. ... Fast parallel decoding."),
            "{}",
            result.output
        );
        assert!(result.output.contains("date: 2026-10-03"));
        assert!(!result.output.contains(SENTINEL));
        let sent = &server.received_requests().await.unwrap()[0];
        assert!(!String::from_utf8_lossy(&sent.body).contains(SENTINEL));
        assert_eq!(sent.headers.get("x-api-key").unwrap(), SENTINEL);
    }

    /// The production sink posts to exactly this constant; the validator
    /// proves the same thing from the source.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn the_production_target_is_the_exa_search_endpoint_over_https() {
        let url = reqwest::Url::parse(EXA_SEARCH_URL).unwrap();
        assert_eq!(url.scheme(), "https");
        assert_eq!(url.host_str(), Some("api.exa.ai"));
        assert_eq!(url.path(), "/search");
        assert!(url.query().is_none() && url.username().is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn the_credential_never_prints() {
        let shown = format!("{:?}", credential());
        assert!(!shown.contains(SENTINEL), "{shown}");
    }
}
