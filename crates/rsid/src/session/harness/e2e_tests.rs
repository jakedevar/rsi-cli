//! End-to-end Harness scenarios against a Mercury-shaped mock model server
//! (#2234 S9; the skeleton lands with S1 and each slice extends it).
//!
//! One `wiremock` server plays the model: it streams OpenAI Chat Completions
//! SSE the way Inception's API does (a tool call whose `arguments` arrive in
//! several deltas, a finish chunk, then a final `include_usage` chunk with an
//! empty `choices` array and nested token details). The real
//! [`OpenAiApiProvider`], agent loop and tool registry run against it. Pages
//! are served by a loopback server behind the egress test seams, so no test
//! touches the network or a real credential.
//!
//! The fake model key below must appear only in the `Authorization` header the
//! mock model server receives; every scenario scans the tool outputs, stream
//! events and later requests for it.

use super::agent_loop::run_harness_loop;
use super::agent_loop::tests::{controller, root_permit_keyed};
use super::api_key::ApiCredential;
use super::compatible_table;
use super::providers::openai_api::OpenAiApiProvider;
use super::tools::web_fetch::test_support::{html, serve, tool_for};
use super::tools::web_search::WebSearchTool;
use super::tools::web_search::serper::test_support::{
    SENTINEL as SEARCH_KEY_SENTINEL, mock_backend, mount_ok, ok_body,
};
use super::tools::{HarnessTool, HarnessToolRegistry};
use super::types::{ChatMessage, ProviderQuirks, ToolResult};
use crate::claude::StreamEvent;
use crate::store::Store;
use rsi_common::types::{ConversationEvent, EventType, Role};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

mod acceptance;
mod navigation;
mod recovery;
mod stream_retry;

/// The fake Inception key the scenarios send; see the module comment.
const MODEL_KEY_SENTINEL: &str = "sk-fake-inception-key-0123456789";

/// Streams the scripted replies in order, one per model request.
struct ScriptedModel {
    /// HTTP statuses served (with an empty error body) before any reply.
    failures: StdMutex<VecDeque<u16>>,
    replies: StdMutex<VecDeque<String>>,
}

impl Respond for ScriptedModel {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        if let Some(status) = self.failures.lock().unwrap().pop_front() {
            return ResponseTemplate::new(status).set_body_string("{\"error\":\"upstream\"}");
        }
        let reply = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("the scripted model ran out of replies");
        ResponseTemplate::new(200).set_body_raw(reply, "text/event-stream")
    }
}

/// Frame JSON chunks as an SSE body ending in `[DONE]`.
fn sse(chunks: &[Value]) -> String {
    let mut body = String::new();
    for chunk in chunks {
        body.push_str(&format!("data: {chunk}\n\n"));
    }
    body.push_str("data: [DONE]\n\n");
    body
}

/// Mercury-shaped usage: the last chunk has no choices, and the token
/// details are nested objects.
fn usage_chunk() -> Value {
    json!({
        "choices": [],
        "usage": {
            "prompt_tokens": 120,
            "completion_tokens": 30,
            "total_tokens": 150,
            "prompt_tokens_details": {"cached_tokens": 64},
            "completion_tokens_details": {"reasoning_tokens": 12}
        }
    })
}

/// A reply that calls one tool; `id` and `name` come in the first delta and
/// `arguments` is split across two deltas.
fn tool_call_reply(id: &str, name: &str, arguments: &str) -> String {
    let split = arguments.len() / 2;
    sse(&[
        json!({"choices": [{"index": 0, "delta": {"role": "assistant", "tool_calls": [{
            "index": 0, "id": id, "type": "function",
            "function": {"name": name, "arguments": &arguments[..split]}
        }]}}]}),
        json!({"choices": [{"index": 0, "delta": {"tool_calls": [{
            "index": 0, "function": {"arguments": &arguments[split..]}
        }]}}]}),
        json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
        usage_chunk(),
    ])
}

/// A reply that calls one tool and streams its `name` the way a provider that
/// splits it does: `name_a` in the first delta, `name_b` in the second.
fn split_name_reply(id: &str, name_a: &str, name_b: &str, arguments: &str) -> String {
    sse(&[
        json!({"choices": [{"index": 0, "delta": {"role": "assistant", "tool_calls": [{
            "index": 0, "id": id, "type": "function",
            "function": {"name": name_a, "arguments": ""}
        }]}}]}),
        json!({"choices": [{"index": 0, "delta": {"tool_calls": [{
            "index": 0, "function": {"name": name_b, "arguments": arguments}
        }]}}]}),
        json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
        usage_chunk(),
    ])
}

/// A reply that calls one tool and repeats the full `name` in every delta, the
/// other shape a provider may use.
fn repeated_name_reply(id: &str, name: &str, arguments: &str) -> String {
    let split = arguments.len() / 2;
    sse(&[
        json!({"choices": [{"index": 0, "delta": {"role": "assistant", "tool_calls": [{
            "index": 0, "id": id, "type": "function",
            "function": {"name": name, "arguments": &arguments[..split]}
        }]}}]}),
        json!({"choices": [{"index": 0, "delta": {"tool_calls": [{
            "index": 0, "function": {"name": name, "arguments": &arguments[split..]}
        }]}}]}),
        json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
        usage_chunk(),
    ])
}

/// Frame JSON chunks as SSE `data:` lines with no space after the colon,
/// which the SSE format allows.
fn sse_unspaced(chunks: &[Value]) -> String {
    let mut body = String::new();
    for chunk in chunks {
        body.push_str(&format!("data:{chunk}\n\n"));
    }
    body.push_str("data:[DONE]\n\n");
    body
}

fn text_reply(text: &str) -> String {
    sse(&[
        json!({"choices": [{"index": 0, "delta": {"role": "assistant", "content": text}}]}),
        json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
        usage_chunk(),
    ])
}

struct Outcome {
    /// The JSON body of every model request, in order.
    requests: Vec<Value>,
    /// The `Authorization` header of every model request.
    authorizations: Vec<String>,
    events: Vec<StreamEvent>,
}

/// How one scenario launches the loop.
struct ScenarioOptions {
    /// The route quirks. The default is the real `mercury` table entry, so the
    /// request body carries Mercury's wire profile (#2253).
    quirks: ProviderQuirks,
    /// The session's reasoning effort, as RSI spells it.
    effort: Option<&'static str>,
    max_iterations: u32,
    /// The session's working directory; the system temp dir when `None`.
    working_dir: Option<std::path::PathBuf>,
    /// The rebuilt transcript of earlier turns of the session, if any.
    history: Option<Vec<ChatMessage>>,
    /// The system prompt; a one-line stand-in when `None`. The acceptance
    /// scenario passes the composed base instructions (#2258).
    system_prompt: Option<String>,
    /// How the route's key is found; the fixed fake model key when `None`.
    /// The acceptance scenario resolves it per request from a vault slot.
    credential: Option<ApiCredential>,
    /// HTTP error statuses the model server answers with before its replies.
    failures: Vec<u16>,
    /// The operator's retry budget for the turn's root call. The daemon
    /// default allows no retry (A9), so a scenario that retries sets one.
    retry_budget: Option<u32>,
    /// Send earlier reasoning back as the OpenRouter route does (#3170).
    replay_reasoning: bool,
}

impl Default for ScenarioOptions {
    fn default() -> Self {
        Self {
            quirks: compatible_table::lookup("mercury-2")
                .expect("the mercury table entry")
                .quirks
                .clone(),
            effort: None,
            max_iterations: 10,
            working_dir: None,
            history: None,
            system_prompt: None,
            credential: None,
            failures: Vec::new(),
            retry_budget: None,
            replay_reasoning: false,
        }
    }
}

/// Drive one Harness turn against the scripted model.
async fn run_scenario(registry: HarnessToolRegistry, replies: Vec<String>, query: &str) -> Outcome {
    run_scenario_with(registry, replies, query, ScenarioOptions::default()).await
}

/// [`run_scenario`] with `working_dir` as the session's working directory.
async fn run_scenario_in(
    registry: HarnessToolRegistry,
    replies: Vec<String>,
    query: &str,
    working_dir: std::path::PathBuf,
) -> Outcome {
    let options = ScenarioOptions {
        working_dir: Some(working_dir),
        ..ScenarioOptions::default()
    };
    run_scenario_with(registry, replies, query, options).await
}

async fn run_scenario_with(
    registry: HarnessToolRegistry,
    replies: Vec<String>,
    query: &str,
    options: ScenarioOptions,
) -> Outcome {
    let store = Arc::new(rsid_core::store_lock::Mutex::new(
        Store::open_in_memory().expect("store"),
    ));
    run_turn_with(
        &store,
        uuid::Uuid::new_v4(),
        "launch",
        registry,
        replies,
        query,
        options,
    )
    .await
}

/// One physical turn of a session: `root_key` names the turn's root admission
/// (a launch or a `session.continue.resume`), `history` is the rebuilt
/// transcript of the earlier turns.
async fn run_turn(
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    session_id: uuid::Uuid,
    root_key: &str,
    registry: HarnessToolRegistry,
    replies: Vec<String>,
    query: &str,
    history: Option<Vec<ChatMessage>>,
) -> Outcome {
    let options = ScenarioOptions {
        history,
        ..ScenarioOptions::default()
    };
    run_turn_with(
        store, session_id, root_key, registry, replies, query, options,
    )
    .await
}

/// [`run_turn`] with explicit route quirks, effort, iteration cap, working
/// directory and history.
async fn run_turn_with(
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    session_id: uuid::Uuid,
    root_key: &str,
    registry: HarnessToolRegistry,
    replies: Vec<String>,
    query: &str,
    options: ScenarioOptions,
) -> Outcome {
    let model = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ScriptedModel {
            failures: StdMutex::new(options.failures.into()),
            replies: StdMutex::new(replies.into()),
        })
        .mount(&model)
        .await;
    let provider = OpenAiApiProvider::with_config(
        model.uri(),
        options
            .credential
            .unwrap_or_else(|| ApiCredential::explicit(Some(MODEL_KEY_SENTINEL))),
        options.quirks,
    )
    .expect("provider")
    .with_reasoning_replay(options.replay_reasoning);

    let permit = root_permit_keyed(store, session_id, root_key).await;
    if let Some(max_retries) = options.retry_budget {
        let root_invocation_id = permit.invocation_id();
        store
            .lock()
            .await
            .conn
            .execute(
                "INSERT INTO model_budget_policies (
                    policy_key, scope_kind, scope_id, max_retries, updated_at
                 ) VALUES (?1, 'retry', ?2, ?3, datetime('now'))",
                rusqlite::params![
                    format!("retry-budget:{root_invocation_id}"),
                    root_invocation_id.to_string(),
                    max_retries
                ],
            )
            .expect("retry policy");
    }
    let (event_tx, mut event_rx) = mpsc::channel(256);
    run_harness_loop(
        Box::new(provider),
        Arc::new(registry),
        options
            .system_prompt
            .unwrap_or_else(|| "You are a coding agent.".to_string()),
        query.to_string(),
        options.working_dir.unwrap_or_else(std::env::temp_dir),
        "mercury-2".to_string(),
        options.effort.map(str::to_string),
        event_tx,
        CancellationToken::new(),
        controller(store, session_id, permit),
        options.max_iterations,
        100_000,
        options.history,
    )
    .await
    .expect("the turn completes");

    let received = model.received_requests().await.expect("request recording");
    Outcome {
        requests: received
            .iter()
            .map(|request| request.body_json().expect("JSON request body"))
            .collect(),
        authorizations: received
            .iter()
            .map(|request| {
                request
                    .headers
                    .get("authorization")
                    .map(|value| value.to_str().unwrap_or_default().to_string())
                    .unwrap_or_default()
            })
            .collect(),
        events: std::iter::from_fn(|| event_rx.try_recv().ok()).collect(),
    }
}

impl Outcome {
    fn tool_names(&self, request: usize) -> Vec<String> {
        self.requests[request]["tools"]
            .as_array()
            .map(|tools| {
                tools
                    .iter()
                    .filter_map(|tool| tool["function"]["name"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// `(tool_use_id, is_error, content)` for every settled call, in order.
    fn settled_calls(&self) -> Vec<(String, bool, String)> {
        self.events
            .iter()
            .filter(|event| event.event_type == "tool_result")
            .map(|event| {
                (
                    event.data["tool_use_id"].as_str().unwrap().to_string(),
                    event.data["is_error"].as_bool().unwrap(),
                    event.data["content"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    }

    /// The `tool_use` ids in event order. The loop announces each call once
    /// (#2279), so an id never repeats; a repeat is a defect.
    fn tool_use_ids(&self) -> Vec<String> {
        self.events
            .iter()
            .filter(|event| event.event_type == "tool_use")
            .map(|event| event.data["id"].as_str().unwrap().to_string())
            .collect()
    }

    /// The `tool` message answering `call_id` in a later request.
    fn tool_message(&self, request: usize, call_id: &str) -> String {
        self.requests[request]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["role"] == "tool" && message["tool_call_id"] == call_id)
            .unwrap_or_else(|| panic!("request {request} has no tool message for {call_id}"))["content"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn assert_key_only_in_authorization(&self) {
        assert!(
            self.authorizations
                .iter()
                .all(|header| header == &format!("Bearer {MODEL_KEY_SENTINEL}")),
            "{:?}",
            self.authorizations
        );
        for request in &self.requests {
            assert!(!request.to_string().contains(MODEL_KEY_SENTINEL));
        }
        for event in &self.events {
            assert!(!event.data.to_string().contains(MODEL_KEY_SENTINEL));
        }
    }
}

impl Outcome {
    /// A secret that must never reach the model, the stream or the transcript:
    /// it appears in no model request and no stream event.
    fn assert_secret_absent(&self, secret: &str) {
        for request in &self.requests {
            assert!(!request.to_string().contains(secret), "model request");
        }
        for event in &self.events {
            assert!(!event.data.to_string().contains(secret), "{event:?}");
        }
    }
}

/// Every assistant tool call in `request` is answered by exactly one later
/// `tool` message, and every `tool` message answers an earlier call.
fn assert_well_formed(request: &Value) {
    let mut open: Vec<String> = Vec::new();
    for message in request["messages"].as_array().expect("messages") {
        match message["role"].as_str().expect("role") {
            "assistant" => {
                for call in message["tool_calls"].as_array().into_iter().flatten() {
                    open.push(call["id"].as_str().expect("call id").to_string());
                }
            }
            "tool" => {
                let id = message["tool_call_id"]
                    .as_str()
                    .expect("a tool message names its call");
                let position = open
                    .iter()
                    .position(|call| call == id)
                    .unwrap_or_else(|| panic!("orphan tool message for {id}"));
                open.remove(position);
            }
            _ => {}
        }
    }
    assert!(open.is_empty(), "unanswered tool calls: {open:?}");
}

/// What the daemon persists for a finished turn: the user's message, then
/// every recognized stream event, converted as the session monitor does.
fn persist_turn(
    session_id: uuid::Uuid,
    query: &str,
    outcome: &Outcome,
    sequence: &mut i32,
) -> Vec<ConversationEvent> {
    *sequence += 1;
    let mut rows = vec![ConversationEvent {
        id: 0,
        session_id,
        sequence: *sequence,
        event_type: EventType::Message,
        role: Some(Role::User),
        content: query.to_string(),
        tool_name: None,
        tool_input: None,
        created_at: chrono::Utc::now(),
        offload_id: None,
        tool_use_id: None,
        metadata: None,
    }];
    for event in &outcome.events {
        if let Some(converted) = crate::session::SessionManager::convert_recognized_stream_event(
            event, session_id, sequence,
        ) {
            rows.extend(converted);
        }
    }
    rows
}

/// A registry with the native fetch (behind the loopback page server) and
/// the native search (behind the mock Serper server).
fn web_registry(vendor: &MockServer, page_server: std::net::SocketAddr) -> HarnessToolRegistry {
    let mut registry = HarnessToolRegistry::new();
    registry.register(Arc::new(tool_for(page_server)));
    registry.register(Arc::new(WebSearchTool::new(Arc::new(mock_backend(vendor)))));
    registry
}

fn vendor_calls(requests: &[wiremock::Request]) -> Vec<(String, Value)> {
    requests
        .iter()
        .map(|request| {
            (
                request
                    .headers
                    .get("x-api-key")
                    .map(|value| value.to_str().unwrap_or_default().to_string())
                    .unwrap_or_default(),
                request.body_json().expect("JSON vendor body"),
            )
        })
        .collect()
}

const PAGE: &str = "<html><head><title>Mercury notes</title></head>\
    <body><h1>Diffusion language models</h1><p>They denoise text in parallel.</p></body></html>";

/// S1: the model fetches a denied address (a tool error, the session
/// continues), then a public page, then answers from the page.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
#[tokio::test]
async fn mercury_session_fetches_a_page_through_web_fetch() {
    let (page_server, pages) = serve(|_| html(PAGE)).await;
    let mut registry = HarnessToolRegistry::new();
    registry.register(Arc::new(tool_for(page_server)));
    let private_url = format!("http://127.0.0.1:{}/private", page_server.port());

    let outcome = run_scenario(
        registry,
        vec![
            tool_call_reply(
                "call_private",
                "web_fetch",
                &json!({ "url": private_url }).to_string(),
            ),
            tool_call_reply(
                "call_page",
                "web_fetch",
                &json!({"url": "http://public.test/notes"}).to_string(),
            ),
            text_reply("Diffusion models denoise text in parallel."),
        ],
        "Summarize the Mercury notes page.",
    )
    .await;

    // The native fetch is the only web tool a Chat Completions route sees.
    assert_eq!(outcome.requests.len(), 3);
    assert_eq!(outcome.tool_names(0), ["web_fetch"]);
    assert_eq!(outcome.tool_names(2), ["web_fetch"]);

    // Every call is paired; the denied one is a tool error, the next works.
    let settled = outcome.settled_calls();
    assert_eq!(outcome.tool_use_ids(), ["call_private", "call_page"]);
    assert_eq!(settled.len(), 2, "one result per call");
    assert_eq!(settled[0].0, "call_private");
    assert!(settled[0].1, "a denied address is a tool error");
    assert!(settled[0].2.contains("egress_denied"), "{}", settled[0].2);
    assert_eq!(settled[1].0, "call_page");
    assert!(!settled[1].1, "{}", settled[1].2);

    // The next request carries each result as a `tool` message.
    assert!(
        outcome
            .tool_message(1, "call_private")
            .contains("egress_denied")
    );
    let page_message = outcome.tool_message(2, "call_page");
    assert!(page_message.contains("<untrusted_web_content>"));
    assert!(page_message.contains("title: Mercury notes"));
    assert!(page_message.contains("# Diffusion language models"));

    // Only the public page was requested, and the session ended with an answer.
    assert_eq!(*pages.lock().unwrap(), ["/notes"]);
    assert!(outcome.events.iter().any(|event| {
        event.event_type == "assistant"
            && event.data.to_string().contains("denoise text in parallel")
    }));
    assert!(
        outcome
            .events
            .iter()
            .any(|event| event.event_type == "result" && event.data["subtype"] == "turn_completed")
    );
    outcome.assert_key_only_in_authorization();
}

/// A model-callable tool whose only job is to prove a streamed call reached
/// the right name.
struct ProbeTool;

#[async_trait::async_trait]
impl HarnessTool for ProbeTool {
    fn name(&self) -> &str {
        "probe_tool"
    }
    fn description(&self) -> &str {
        "Probe."
    }
    fn parameters_json(&self) -> &str {
        r#"{"type":"object","properties":{"q":{"type":"string"}}}"#
    }
    async fn execute(&self, _args: Value, _working_dir: &Path) -> ToolResult {
        ToolResult {
            success: true,
            output: "probe ok".to_string(),
            error_msg: None,
        }
    }
}

fn probe_registry() -> HarnessToolRegistry {
    let mut registry = HarnessToolRegistry::new();
    registry.register(Arc::new(ProbeTool));
    registry
}

impl Outcome {
    /// The `function.name` of every `tool_calls` entry the assistant message
    /// in `request` carries.
    fn assistant_call_names(&self, request: usize) -> Vec<String> {
        self.requests[request]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|message| message["tool_calls"].as_array())
            .flatten()
            .filter_map(|call| call["function"]["name"].as_str().map(str::to_string))
            .collect()
    }

    fn result_usage(&self) -> Value {
        self.events
            .iter()
            .find(|event| event.event_type == "result")
            .expect("a result event")
            .data["usage"]
            .clone()
    }
}

/// S3 (#2253): the request body carries Mercury's wire profile, a tool name
/// streamed in two shapes dispatches the right tool, an unspaced `data:` line
/// is read, usage details and a provider warning reach the stream.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
#[tokio::test]
async fn mercury_wire_profile_reaches_the_model_server_and_the_stream() {
    let outcome = run_scenario_with(
        probe_registry(),
        vec![
            // The name arrives in two fragments.
            split_name_reply("call_split", "probe_", "tool", r#"{"q":"one"}"#),
            // The name is repeated in every chunk and must not double.
            repeated_name_reply("call_repeat", "probe_tool", r#"{"q":"two"}"#),
            // A warning chunk, then the answer on unspaced `data:` lines.
            sse_unspaced(&[
                json!({"warning": "temperature out of range; reset to 0.75", "choices": []}),
                json!({"choices": [{"index": 0, "delta": {"role": "assistant", "content": "All probes ran."}}]}),
                json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
                usage_chunk(),
            ]),
        ],
        "Run the probe twice.",
        ScenarioOptions {
            effort: Some("low"),
            ..ScenarioOptions::default()
        },
    )
    .await;

    // The wire profile: Mercury's effort and output-cap fields, none of the
    // parameters Mercury must never get.
    assert_eq!(outcome.requests.len(), 3);
    for body in &outcome.requests {
        assert_eq!(body["model"], "mercury-2");
        assert_eq!(body["reasoning_effort"], "low");
        assert_eq!(
            body["max_completion_tokens"], 16_384,
            "the route's output budget replaces the generic 8192 cap"
        );
        for absent in [
            "max_tokens",
            "diffusing",
            "parallel_tool_calls",
            "reasoning",
            "tool_choice",
            "temperature",
        ] {
            assert!(body.get(absent).is_none(), "{absent} in {body}");
        }
        assert!(
            body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .all(|message| message["role"] != "developer")
        );
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"], json!({"include_usage": true}));
    }
    assert_eq!(outcome.tool_names(0), ["probe_tool"]);

    // Both name shapes dispatched `probe_tool`, once each, and the next
    // request echoes the accumulated name.
    assert_eq!(outcome.tool_use_ids(), ["call_split", "call_repeat"]);
    let settled = outcome.settled_calls();
    assert_eq!(settled.len(), 2);
    assert!(
        settled
            .iter()
            .all(|(_, is_error, content)| !is_error && content == "probe ok"),
        "{settled:?}"
    );
    assert!(
        outcome
            .events
            .iter()
            .filter(|event| event.event_type == "tool_use")
            .all(|event| event.data["name"] == "probe_tool")
    );
    assert_eq!(outcome.assistant_call_names(1), ["probe_tool"]);
    assert_eq!(
        outcome.assistant_call_names(2),
        ["probe_tool", "probe_tool"]
    );

    // The unspaced `data:` reply was parsed into the answer.
    assert!(outcome.events.iter().any(|event| {
        event.event_type == "assistant" && event.data.to_string().contains("All probes ran.")
    }));

    // Usage details reach the turn's usage: cached tokens are the last
    // call's, reasoning tokens add up over the three calls.
    let usage = outcome.result_usage();
    assert_eq!(usage["cache_read_input_tokens"], 64);
    assert_eq!(usage["reasoning_output_tokens"], 36);
    assert_eq!(usage["input_tokens"], 120);

    // The provider's warning became an event.
    let warning = outcome
        .events
        .iter()
        .find(|event| event.data["subtype"] == "provider_warning")
        .expect("a provider_warning event");
    assert_eq!(warning.event_type, "system");
    assert!(
        warning.data["warning"]
            .as_str()
            .unwrap()
            .contains("reset to 0.75")
    );
    outcome.assert_key_only_in_authorization();
}

/// S3 (#2253): the tool-less wrap-up turn keeps `tools` and sends
/// `tool_choice: "none"` on Mercury; a route without the quirk drops `tools`
/// as before.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
#[tokio::test]
async fn the_wrap_up_turn_sends_tool_choice_none_only_on_a_route_that_allows_it() {
    for (route, quirks) in [
        ("mercury", ScenarioOptions::default().quirks),
        (
            "plain",
            ProviderQuirks {
                native_tools: true,
                ..Default::default()
            },
        ),
    ] {
        let outcome = run_scenario_with(
            probe_registry(),
            vec![
                tool_call_reply("call_1", "probe_tool", r#"{"q":"one"}"#),
                text_reply("Summary of what is done."),
            ],
            "Run the probe.",
            ScenarioOptions {
                quirks,
                max_iterations: 2,
                ..ScenarioOptions::default()
            },
        )
        .await;

        assert_eq!(outcome.requests.len(), 2, "{route}");
        // The first step is an ordinary one.
        assert_eq!(outcome.tool_names(0), ["probe_tool"], "{route}");
        assert!(outcome.requests[0].get("tool_choice").is_none(), "{route}");
        // The last permitted step is the wrap-up.
        let wrap_up_prompt = outcome.requests[1]["messages"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()["content"]
            .as_str()
            .unwrap();
        assert!(wrap_up_prompt.contains("last permitted step"), "{route}");
        if route == "mercury" {
            assert_eq!(outcome.requests[1]["tool_choice"], "none");
            assert_eq!(outcome.tool_names(1), ["probe_tool"]);
            assert_eq!(outcome.assistant_call_names(1), ["probe_tool"]);
        } else {
            assert!(outcome.requests[1].get("tool_choice").is_none());
            assert!(outcome.requests[1].get("tools").is_none());
        }
        outcome.assert_key_only_in_authorization();
    }
}

/// S3 (#2253): the #1061 final-answer retry, after a reply cut off at the
/// output limit, also keeps `tools` and sends `tool_choice: "none"` on
/// Mercury, with its larger output budget and `low` effort.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
#[tokio::test]
async fn the_final_answer_retry_turn_sends_tool_choice_none_on_mercury() {
    let cut_off = sse(&[
        json!({"choices": [{"index": 0, "delta": {"role": "assistant"}, "finish_reason": "length"}]}),
        usage_chunk(),
    ]);
    let outcome = run_scenario_with(
        probe_registry(),
        vec![
            tool_call_reply("call_1", "probe_tool", r#"{"q":"one"}"#),
            cut_off.clone(),
            cut_off.clone(),
            cut_off,
            text_reply("Done."),
        ],
        "Run the probe.",
        ScenarioOptions {
            effort: Some("max"),
            ..ScenarioOptions::default()
        },
    )
    .await;

    assert_eq!(outcome.requests.len(), 5);
    // `max` is Mercury's `high`; the first two calls use it.
    assert_eq!(outcome.requests[0]["reasoning_effort"], "high");
    assert_eq!(outcome.requests[1]["reasoning_effort"], "high");
    assert!(outcome.requests[1].get("tool_choice").is_none());
    // #4028: the two length-stop recoveries keep tools callable, at `low` effort
    // and the 32,768 ceiling.
    for recovery in [2, 3] {
        assert_eq!(outcome.requests[recovery]["reasoning_effort"], "low");
        assert_eq!(outcome.requests[recovery]["max_completion_tokens"], 32_768);
        assert!(outcome.requests[recovery].get("tool_choice").is_none());
    }
    // The retry turn: `low` effort, the 32,768 ceiling, tools kept but not
    // callable.
    assert_eq!(outcome.requests[4]["reasoning_effort"], "low");
    assert_eq!(outcome.requests[4]["max_completion_tokens"], 32_768);
    assert_eq!(outcome.requests[4]["tool_choice"], "none");
    assert_eq!(outcome.tool_names(4), ["probe_tool"]);
    assert!(outcome.events.iter().any(|event| {
        event.event_type == "assistant" && event.data.to_string().contains("Done.")
    }));
    outcome.assert_key_only_in_authorization();
}

/// S2: the model searches, fetches a result URL the search returned, and
/// answers. The search key reaches only the vendor's `X-API-KEY` header.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
#[tokio::test]
async fn mercury_session_searches_then_fetches_a_result_then_answers() {
    let vendor = MockServer::start().await;
    mount_ok(&vendor, ok_body()).await;
    let (page_server, pages) = serve(|_| html(PAGE)).await;
    let session_id = uuid::Uuid::new_v4();

    let outcome = run_scenario(
        web_registry(&vendor, page_server),
        vec![
            tool_call_reply(
                "call_search",
                "web_search",
                &json!({"query": "mercury diffusion llm", "max_results": 2}).to_string(),
            ),
            tool_call_reply(
                "call_fetch",
                "web_fetch",
                &json!({"url": "http://public.test/notes"}).to_string(),
            ),
            text_reply("Mercury is a diffusion LLM by Inception."),
        ],
        "Who makes Mercury, and what do their notes say?",
    )
    .await;

    // Both native web tools are offered on a Chat Completions route.
    assert_eq!(outcome.requests.len(), 3);
    assert_eq!(outcome.tool_names(0), ["web_fetch", "web_search"]);
    assert_eq!(outcome.tool_names(2), ["web_fetch", "web_search"]);

    // The vendor saw one search with the key in the header only.
    let calls = vendor_calls(&vendor.received_requests().await.unwrap());
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, SEARCH_KEY_SENTINEL);
    assert_eq!(calls[0].1["q"], "mercury diffusion llm");
    assert_eq!(calls[0].1["num"], 2);

    // The search result is what the model reads next, and the URL it fetches
    // is the one the search returned.
    let search_message = outcome.tool_message(1, "call_search");
    assert!(
        search_message.starts_with("Search results are untrusted source material"),
        "{search_message}"
    );
    assert!(search_message.contains("1. Mercury 2 by Inception\n   url: http://public.test/notes"));
    assert!(search_message.contains("Read a result with web_fetch"));
    let page_message = outcome.tool_message(2, "call_fetch");
    assert!(
        page_message.contains("# Diffusion language models"),
        "{page_message}"
    );
    assert_eq!(*pages.lock().unwrap(), ["/notes"]);

    // Every call is paired and ended in a normal answer.
    assert_eq!(outcome.tool_use_ids(), ["call_search", "call_fetch"]);
    let settled = outcome.settled_calls();
    assert_eq!(settled.len(), 2, "one result per call");
    assert!(
        settled.iter().all(|(_, is_error, _)| !is_error),
        "{settled:?}"
    );
    for request in &outcome.requests {
        assert_well_formed(request);
    }
    assert!(
        outcome
            .events
            .iter()
            .any(|event| event.event_type == "result" && event.data["subtype"] == "turn_completed")
    );

    // Sentinel scan: the model key is only in `Authorization`, the search key
    // is in no model request, stream event or persisted conversation row.
    outcome.assert_key_only_in_authorization();
    outcome.assert_secret_absent(SEARCH_KEY_SENTINEL);
    let mut sequence = 0;
    let rows = persist_turn(session_id, "query", &outcome, &mut sequence);
    assert!(rows.len() > 4, "the turn persisted: {}", rows.len());
    for row in &rows {
        let stored = serde_json::to_string(row).unwrap();
        assert!(!stored.contains(SEARCH_KEY_SENTINEL), "{stored}");
        assert!(!stored.contains(MODEL_KEY_SENTINEL), "{stored}");
    }
}

/// S2 / #748: the reported failure was a follow-up turn of the same Harness
/// session. Its first tool-loop call was admitted under the key the first
/// turn had already used (`session.harness.turn:<session>:2:root:turn:1`) and
/// the session ended with `Policy denied: model invocation dedup conflict`
/// before any web request. Here turn 1 uses `web_search`, the daemon persists
/// it, and turn 2 (a new root admission of the SAME session, with at least two
/// model calls like turn 1) continues from that persisted history: it must be
/// admitted, search again, and send well-formed requests.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
#[tokio::test]
async fn a_follow_up_turn_searches_again_from_persisted_history() {
    let vendor = MockServer::start().await;
    mount_ok(&vendor, ok_body()).await;
    let (page_server, _pages) = serve(|_| html(PAGE)).await;
    let store = Arc::new(rsid_core::store_lock::Mutex::new(
        Store::open_in_memory().expect("store"),
    ));
    let session_id = uuid::Uuid::new_v4();

    // Turn 1: three model calls (search, fetch, answer).
    let first_query = "Search the web for the company behind Mercury.";
    let first = run_turn(
        &store,
        session_id,
        "launch",
        web_registry(&vendor, page_server),
        vec![
            tool_call_reply(
                "call_search_1",
                "web_search",
                &json!({"query": "mercury diffusion llm company"}).to_string(),
            ),
            tool_call_reply(
                "call_fetch_1",
                "web_fetch",
                &json!({"url": "http://public.test/notes"}).to_string(),
            ),
            text_reply("Inception makes Mercury."),
        ],
        first_query,
        None,
    )
    .await;
    assert_eq!(first.requests.len(), 3);

    // The daemon persists turn 1; the continuation rebuilds its history from
    // those rows exactly as `session.continue.resume` does.
    let mut sequence = 0;
    let persisted = persist_turn(session_id, first_query, &first, &mut sequence);
    assert!(
        persisted
            .iter()
            .any(|row| row.event_type == EventType::ToolResult
                && row
                    .content
                    .contains("Search results are untrusted source material")),
        "the search result was persisted"
    );
    let history = crate::session::SessionManager::events_to_harness_messages(&persisted);
    assert!(history.iter().any(|m| m.content == first_query));
    assert!(
        history
            .iter()
            .any(|m| m.content == "Inception makes Mercury.")
    );

    // Turn 2: a new root admission of the same session, two model calls.
    let second_query = "Now search for their latest options.";
    let second = run_turn(
        &store,
        session_id,
        "continue-1",
        web_registry(&vendor, page_server),
        vec![
            tool_call_reply(
                "call_search_2",
                "web_search",
                &json!({"query": "Inception Labs latest models", "recency": "month"}).to_string(),
            ),
            text_reply("Their latest option is Mercury 2."),
        ],
        second_query,
        Some(history),
    )
    .await;

    assert_eq!(second.requests.len(), 2, "turn 2 made both model calls");
    assert_eq!(second.tool_names(0), ["web_fetch", "web_search"]);
    for request in &second.requests {
        assert_well_formed(request);
    }
    // The follow-up request carries the earlier turn and the new question.
    let carried: Vec<String> = second.requests[0]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|message| message["content"].as_str().map(str::to_string))
        .collect();
    assert!(
        carried.iter().any(|text| text == first_query),
        "{carried:?}"
    );
    assert!(
        carried
            .iter()
            .any(|text| text == "Inception makes Mercury.")
    );
    assert_eq!(carried.last().map(String::as_str), Some(second_query));
    // The search ran and its result reached the model.
    assert!(
        second
            .tool_message(1, "call_search_2")
            .starts_with("Search results are untrusted source material")
    );
    assert_eq!(second.tool_use_ids(), ["call_search_2"]);
    assert!(
        second
            .events
            .iter()
            .any(|event| event.event_type == "result" && event.data["subtype"] == "turn_completed")
    );

    // One search per turn reached the vendor, both with the key header.
    let calls = vendor_calls(&vendor.received_requests().await.unwrap());
    assert_eq!(calls.len(), 2);
    assert!(calls.iter().all(|(key, _)| key == SEARCH_KEY_SENTINEL));
    assert_eq!(calls[1].1["tbs"], "qdr:m");

    for outcome in [&first, &second] {
        outcome.assert_key_only_in_authorization();
        outcome.assert_secret_absent(SEARCH_KEY_SENTINEL);
    }
}

/// #3170: the model's thinking from a tool step is streamed, kept on the
/// assistant message and sent back on the next request of the chain; a route
/// that does not replay never sees it. The final answer's thinking is stored
/// beside it and a resumed session rebuilds it.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
#[tokio::test]
async fn streamed_reasoning_is_replayed_on_the_tool_chain_and_rebuilt_on_resume() {
    let details = json!({"type": "reasoning.text", "index": 0, "text": "check the probe first"});
    let replies = || {
        vec![
            sse(&[
                json!({"choices": [{"index": 0, "delta": {
                    "role": "assistant",
                    "reasoning": "check the probe first",
                    "reasoning_details": [details.clone()],
                    "tool_calls": [{"index": 0, "id": "call_think", "type": "function",
                        "function": {"name": "probe_tool", "arguments": "{\"q\":\"one\"}"}}]
                }}]}),
                json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
                usage_chunk(),
            ]),
            sse(&[
                json!({"choices": [{"index": 0, "delta": {
                    "role": "assistant", "reasoning": "it worked", "content": "Probe done.",
                    "reasoning_details": [{"type": "reasoning.text", "index": 0, "text": "it worked"}]
                }}]}),
                json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
                usage_chunk(),
            ]),
        ]
    };

    let replaying = run_scenario_with(
        probe_registry(),
        replies(),
        "Run the probe.",
        ScenarioOptions {
            replay_reasoning: true,
            ..ScenarioOptions::default()
        },
    )
    .await;
    assert_eq!(replaying.requests.len(), 2);
    assert_well_formed(&replaying.requests[1]);
    let messages = replaying.requests[1]["messages"].as_array().unwrap();
    let step = messages
        .iter()
        .find(|m| m["role"] == "assistant")
        .expect("the tool step is carried");
    assert_eq!(step["reasoning_details"], json!([details]));
    assert!(step.get("reasoning").is_none(), "one form, not both");
    assert_eq!(step["tool_calls"][0]["id"], "call_think");
    assert!(
        replaying.requests[0]["messages"]
            .to_string()
            .find("check the probe first")
            .is_none(),
        "the first request has nothing to replay"
    );

    // Another route is byte-for-byte unchanged: no reasoning on the wire.
    let plain = run_scenario_with(
        probe_registry(),
        replies(),
        "Run the probe.",
        ScenarioOptions::default(),
    )
    .await;
    let second = plain.requests[1].to_string();
    assert!(!second.contains("check the probe first"), "{second}");
    assert!(!second.contains("reasoning_details"), "{second}");

    // The final answer's thinking is persisted and a resumed session rebuilds it.
    let session_id = uuid::Uuid::new_v4();
    let mut sequence = 0;
    let persisted = persist_turn(session_id, "Run the probe.", &replaying, &mut sequence);
    assert!(
        persisted
            .iter()
            .any(|row| row.event_type == EventType::Thinking && row.content == "it worked")
    );
    let rebuilt = crate::session::SessionManager::events_to_harness_messages(&persisted);
    let answer = rebuilt
        .iter()
        .find(|m| m.content == "Probe done.")
        .expect("the answer is rebuilt");
    assert_eq!(answer.reasoning.as_deref(), Some("it worked"));
    assert_eq!(
        answer.reasoning_details,
        Some(vec![
            json!({"type": "reasoning.text", "index": 0, "text": "it worked"})
        ])
    );
}
