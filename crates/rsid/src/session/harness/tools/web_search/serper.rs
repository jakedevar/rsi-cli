//! Serper (Google Search API) adapter for `web_search` (#2251).
//!
//! The result normalization (answer box and knowledge graph lines, organic
//! results, "people also ask") is adapted from prime-agent's websearch skill,
//! `packages/coding-agent/skills/websearch/src/websearch/websearch.py`
//! (`_format_serper_results`) @ cd1f215c (MIT); see THIRD_PARTY_NOTICES.md. The
//! Python code formats text; this file normalizes into [`SearchResults`] and
//! leaves the formatting to the tool.
//!
//! Safety properties:
//!
//! - The target is the constant [`SERPER_SEARCH_URL`], posted by the one
//!   production sink [`SerperBackend::post_search`], which the model-control
//!   validator registers as a non-model POST (`registry::NON_INVOCATIONS`). A
//!   `cfg(test)` twin posts to a loopback server instead. The client follows
//!   no redirects, has connect and total timeouts and reads at most
//!   [`MAX_RESPONSE_BYTES`].
//! - The key is read from the vault slot on every call
//!   ([`ApiCredential::for_slot_only`]: no generic fallback), is sent only in
//!   the `X-API-KEY` header (marked sensitive), and appears in no argument,
//!   output, event or error text. Vendor error bodies are truncated and have
//!   the key scrubbed. Transport errors are reported by category, never by
//!   the library's message.

use super::{
    Recency, SearchBackend, SearchError, SearchHit, SearchRequest, SearchResults, clean_line,
};
use crate::session::harness::api_key::ApiCredential;
use reqwest::header::{HeaderValue, RETRY_AFTER};
use serde_json::{Value, json};
use std::time::Duration;

/// The Serper search endpoint. A direct string literal: the model-control
/// validator reads this constant to prove the sink never reaches a model API.
const SERPER_SEARCH_URL: &str = "https://google.serper.dev/search";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
/// Most response bytes read; a search response is a few tens of KB.
pub(super) const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
/// Most characters of a vendor error message passed to the model.
const ERROR_MESSAGE_CHARS: usize = 200;
/// Knowledge-graph attributes shown (prime's formatter shows all; bounded here).
const MAX_ATTRIBUTES: usize = 8;
/// "People also ask" entries kept (as prime's formatter).
const MAX_RELATED_QUESTIONS: usize = 3;
const USER_AGENT: &str = "rsi-harness-web-search";

pub struct SerperBackend {
    client: reqwest::Client,
    credential: ApiCredential,
    /// Test seam: post here instead of [`SERPER_SEARCH_URL`].
    #[cfg(test)]
    test_url: Option<String>,
}

impl SerperBackend {
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

    /// A backend that posts to `{base_url}/search` instead of Serper.
    #[cfg(test)]
    pub(crate) fn with_base_url(credential: ApiCredential, base_url: String) -> Self {
        let mut backend = Self::new(credential).expect("test HTTP client");
        backend.test_url = Some(format!("{base_url}/search"));
        backend
    }

    /// The one production sink: a POST to the constant Serper URL. Keep it a
    /// single direct `.post(SERPER_SEARCH_URL)...send()` chain; the
    /// model-control validator proves the target from the constant.
    async fn post_search(
        &self,
        key: HeaderValue,
        body: &Value,
    ) -> Result<reqwest::Response, reqwest::Error> {
        self.client
            .post(SERPER_SEARCH_URL)
            .header("X-API-KEY", key)
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
            .header("X-API-KEY", key)
            .json(body)
            .send()
            .await
    }
}

#[async_trait::async_trait]
impl SearchBackend for SerperBackend {
    fn id(&self) -> &'static str {
        "serper"
    }

    async fn search(&self, request: &SearchRequest) -> Result<SearchResults, SearchError> {
        let Some(secret) = self.credential.current() else {
            return Err(SearchError::Unavailable(
                "no Serper key is configured; the operator can add one under Provider Keys"
                    .to_string(),
            ));
        };
        let mut key = HeaderValue::from_str(secret.expose()).map_err(|_| {
            SearchError::Failed(
                "the configured Serper key is not a valid HTTP header value".to_string(),
            )
        })?;
        key.set_sensitive(true);

        let request_json = request_body(request);
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

/// A transport failure by category only: the library's message can carry the
/// request URL and is not worth the risk.
pub(super) fn transport_error(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "the search request timed out".to_string()
    } else if error.is_connect() {
        "could not connect to the search service".to_string()
    } else {
        "the search request failed".to_string()
    }
}

/// The vendor's own error text, bounded, on one line, with the key scrubbed.
pub(super) fn vendor_message(body: &[u8], key: &str) -> String {
    let text = String::from_utf8_lossy(body);
    let message = serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|json| {
            json.get("message")
                .or_else(|| json.get("error"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| text.into_owned());
    let scrubbed = if key.is_empty() {
        message
    } else {
        message.replace(key, "[redacted]")
    };
    let cleaned = clean_line(&scrubbed, ERROR_MESSAGE_CHARS);
    if cleaned.is_empty() {
        "(no message)".to_string()
    } else {
        cleaned
    }
}

/// Serper has no domain parameters, so a domain filter is a Google `site:`
/// operator in the query; the tool filters the hits again as a backstop. The SearXNG
/// adapter composes its query the same way.
pub(super) fn compose_query(request: &SearchRequest) -> String {
    let mut query = request.query.clone();
    match request.allowed_domains.as_slice() {
        [] => {}
        [only] => query.push_str(&format!(" site:{only}")),
        several => {
            let sites: Vec<String> = several
                .iter()
                .map(|domain| format!("site:{domain}"))
                .collect();
            query.push_str(&format!(" ({})", sites.join(" OR ")));
        }
    }
    for domain in &request.blocked_domains {
        query.push_str(&format!(" -site:{domain}"));
    }
    query
}

fn request_body(request: &SearchRequest) -> Value {
    let mut body = json!({
        "q": compose_query(request),
        "num": request.max_results,
    });
    if let Some(recency) = request.recency {
        body["tbs"] = json!(match recency {
            Recency::Day => "qdr:d",
            Recency::Week => "qdr:w",
            Recency::Month => "qdr:m",
            Recency::Year => "qdr:y",
        });
    }
    body
}

/// Fold a Serper response into [`SearchResults`].
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

    if let Some(answer) = data
        .get("answerBox")
        .and_then(|answer_box| text(answer_box, "answer").or_else(|| text(answer_box, "snippet")))
    {
        results.answer_lines.push(format!("Answer: {answer}"));
    }
    if let Some(graph) = data.get("knowledgeGraph") {
        if let Some(title) = text(graph, "title") {
            results
                .answer_lines
                .push(format!("Knowledge Graph: {title}"));
        }
        if let Some(description) = text(graph, "description") {
            results.answer_lines.push(description);
        }
        if let Some(attributes) = graph.get("attributes").and_then(Value::as_object) {
            for (name, value) in attributes.iter().take(MAX_ATTRIBUTES) {
                let value = value
                    .as_str()
                    .map_or_else(|| value.to_string(), str::to_string);
                if !value.trim().is_empty() {
                    results
                        .answer_lines
                        .push(format!("{name}: {}", value.trim()));
                }
            }
        }
    }

    for item in data
        .get("organic")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(url) = text(item, "link") else {
            continue;
        };
        results.hits.push(SearchHit {
            title: text(item, "title").unwrap_or_else(|| "Untitled".to_string()),
            url,
            snippet: text(item, "snippet").unwrap_or_default(),
            date: text(item, "date"),
        });
    }

    for item in data
        .get("peopleAlsoAsk")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(MAX_RELATED_QUESTIONS)
    {
        if let Some(question) = text(item, "question") {
            results
                .related_questions
                .push((question, text(item, "snippet").unwrap_or_default()));
        }
    }
    results
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// The fake search key every test sends; it must appear only in the
    /// `X-API-KEY` header the mock vendor receives.
    pub(crate) const SENTINEL: &str = "serper-fake-key-0123456789abcdef";

    pub(crate) fn credential() -> ApiCredential {
        ApiCredential::explicit(Some(SENTINEL))
    }

    pub(crate) fn mock_backend(server: &MockServer) -> SerperBackend {
        SerperBackend::with_base_url(credential(), server.uri())
    }

    /// A Serper-shaped response with one of everything.
    pub(crate) fn ok_body() -> Value {
        json!({
            "searchParameters": {"q": "mercury diffusion llm", "type": "search"},
            "knowledgeGraph": {
                "title": "Inception",
                "type": "Company",
                "description": "Inception builds diffusion language models.",
                "attributes": {"Founded": "2021", "Product": "Mercury"}
            },
            "organic": [
                {
                    "title": "Mercury 2 by Inception",
                    "link": "http://public.test/notes",
                    "snippet": "A diffusion LLM.",
                    "date": "Oct 3, 2026",
                    "position": 1
                },
                {"title": "Docs", "link": "https://docs.inceptionlabs.ai/", "snippet": "API docs."}
            ],
            "peopleAlsoAsk": [
                {"question": "What is Mercury?", "snippet": "A diffusion model."}
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
    use crate::session::harness::tools::ToolContext;
    use crate::session::harness::tools::web_search::WebSearchTool;
    use crate::session::harness::tools::{HarnessTool, ToolPolicy};
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;
    use wiremock::matchers::{body_json, header, method, path};
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn a_search_posts_the_documented_request_and_normalizes_the_response() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/search"))
            .and(header("x-api-key", SENTINEL))
            .and(header("content-type", "application/json"))
            .and(body_json(json!({
                "q": "mercury diffusion llm site:inceptionlabs.ai",
                "num": 3,
                "tbs": "qdr:w"
            })))
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

        assert_eq!(
            results.answer_lines,
            [
                "Knowledge Graph: Inception",
                "Inception builds diffusion language models.",
                "Founded: 2021",
                "Product: Mercury"
            ]
        );
        assert_eq!(results.hits.len(), 2);
        assert_eq!(
            results.hits[0],
            SearchHit {
                title: "Mercury 2 by Inception".into(),
                url: "http://public.test/notes".into(),
                snippet: "A diffusion LLM.".into(),
                date: Some("Oct 3, 2026".into()),
            }
        );
        assert_eq!(results.hits[1].date, None);
        assert_eq!(
            results.related_questions,
            [(
                "What is Mercury?".to_string(),
                "A diffusion model.".to_string()
            )]
        );
        // `expect(1)` is verified when the server drops.
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn domain_filters_and_recency_become_query_operators() {
        let allowed = SearchRequest {
            allowed_domains: vec!["a.test".into(), "b.test".into()],
            ..request("q")
        };
        assert_eq!(compose_query(&allowed), "q (site:a.test OR site:b.test)");
        let blocked = SearchRequest {
            blocked_domains: vec!["spam.test".into(), "ads.test".into()],
            ..request("q")
        };
        assert_eq!(compose_query(&blocked), "q -site:spam.test -site:ads.test");
        assert_eq!(compose_query(&request("plain")), "plain");
        for (recency, tbs) in [
            (Recency::Day, "qdr:d"),
            (Recency::Week, "qdr:w"),
            (Recency::Month, "qdr:m"),
            (Recency::Year, "qdr:y"),
        ] {
            let body = request_body(&SearchRequest {
                recency: Some(recency),
                ..request("q")
            });
            assert_eq!(body["tbs"], tbs);
            assert_eq!(body["num"], 5);
        }
        assert!(request_body(&request("q")).get("tbs").is_none());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn partial_responses_normalize_without_panicking() {
        assert_eq!(normalize(&json!({})), SearchResults::default());
        assert_eq!(normalize(&json!("nonsense")), SearchResults::default());
        let results = normalize(&json!({
            "organic": [
                {"snippet": "no link, skipped"},
                {"link": "https://only-link.test/"},
                {"title": "  ", "link": "https://blank-title.test/"}
            ],
            "answerBox": {"snippet": "direct answer"},
            "knowledgeGraph": {"attributes": {"n": 4, "empty": " "}},
            "peopleAlsoAsk": [{"snippet": "no question"}, {"question": "Q?"}]
        }));
        let titles: Vec<&str> = results.hits.iter().map(|h| h.title.as_str()).collect();
        assert_eq!(titles, ["Untitled", "Untitled"]);
        assert_eq!(results.answer_lines, ["Answer: direct answer", "n: 4"]);
        assert_eq!(
            results.related_questions,
            [("Q?".to_string(), String::new())]
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn vendor_failures_map_to_typed_errors_that_never_carry_the_key() {
        let server = MockServer::start().await;
        let backend = mock_backend(&server);
        let cases: Vec<(ResponseTemplate, &str)> = vec![
            (
                ResponseTemplate::new(401).set_body_string("bad"),
                "web_search_auth_failed",
            ),
            (
                ResponseTemplate::new(403).set_body_string("Unauthorized."),
                "web_search_auth_failed",
            ),
            (
                ResponseTemplate::new(429).insert_header("Retry-After", "7"),
                "web_search_rate_limited: the serper backend is rate limiting this key; retry after 7s",
            ),
            (ResponseTemplate::new(429), "retry in a few seconds"),
            (
                ResponseTemplate::new(400)
                    .set_body_json(json!({"message": "Not enough credits", "statusCode": 400})),
                "web_search_failed: HTTP 400: Not enough credits",
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
            let message = error.clone().into_message("serper");
            assert!(message.contains(expected), "{expected}: {message}");
            assert!(!message.contains(SENTINEL), "{message}");
            // A redirect is reported, never followed.
            assert_eq!(server.received_requests().await.unwrap().len(), 1);
        }
        assert_eq!(
            backend
                .search(&request("q"))
                .await
                .map(|results| results.hits.len()),
            Err(SearchError::Failed(
                "the response was not valid JSON".into()
            ))
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn a_vendor_that_echoes_the_key_is_scrubbed_and_long_bodies_are_cut() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "message": format!("invalid key {SENTINEL} supplied {}", "x".repeat(2_000))
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
        let keyless = SerperBackend::with_base_url(ApiCredential::explicit(None), server.uri());
        let error = keyless.search(&request("q")).await.unwrap_err();
        assert!(matches!(error, SearchError::Unavailable(_)), "{error:?}");
        assert!(
            error
                .into_message("serper")
                .starts_with("web_search_unavailable:")
        );
        assert!(server.received_requests().await.unwrap().is_empty());

        // The key follows the credential: a header-invalid key is refused locally.
        let broken =
            SerperBackend::with_base_url(ApiCredential::explicit(Some("bad\nkey")), server.uri());
        let error = broken.search(&request("q")).await.unwrap_err();
        assert!(matches!(error, SearchError::Failed(_)), "{error:?}");
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn cancellation_abandons_a_slow_search() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(30)))
            .mount(&server)
            .await;
        let tool = WebSearchTool::new(Arc::new(mock_backend(&server)));
        let cancel = CancellationToken::new();
        let context = ToolContext {
            session_id: None,
            working_dir: std::env::temp_dir(),
            cancel: cancel.clone(),
            event_sink: None,
            policy: ToolPolicy::default(),
        };
        let canceller = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            cancel.cancel();
        });
        let result = tool
            .execute_with_context(json!({"query": "slow"}), &context)
            .await;
        canceller.await.unwrap();
        assert!(!result.success);
        assert_eq!(
            result.error_msg.unwrap(),
            "web_search_failed: search cancelled"
        );
    }

    /// The production sink posts to exactly this constant; the validator
    /// proves the same thing from the source.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn the_production_target_is_the_serper_search_endpoint_over_https() {
        let url = reqwest::Url::parse(SERPER_SEARCH_URL).unwrap();
        assert_eq!(url.scheme(), "https");
        assert_eq!(url.host_str(), Some("google.serper.dev"));
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
