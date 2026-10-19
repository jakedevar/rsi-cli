//! #3163: a Harness session survives upstream 503 bursts and streams that die
//! after output began. The model, provider and loop are the real ones; only
//! the model server is scripted. (The idle timeout and the long flowing stream
//! run on a paused clock in `rsid-harness`: `idle` and `agent_loop` tests.)

use super::{Outcome, ScenarioOptions, run_scenario_with, sse, text_reply};
use crate::session::harness::tools::HarnessToolRegistry;
use serde_json::json;

/// A stream that carries one text chunk and then just ends: no finish reason,
/// no `[DONE]`.
fn truncated_after_one_chunk(text: &str) -> String {
    let chunk = json!({"choices": [{"index": 0, "delta": {"role": "assistant", "content": text}}]});
    format!("data: {chunk}\n\n")
}

/// A stream whose upstream provider reports a failure after one text chunk
/// (the shape OpenRouter uses).
fn upstream_error_after_one_chunk(text: &str) -> String {
    sse(&[
        json!({"choices": [{"index": 0, "delta": {"role": "assistant", "content": text}}]}),
        json!({"error": {"code": 502, "message": "upstream died"},
               "choices": [{"index": 0, "delta": {}, "finish_reason": "error"}]}),
    ])
}

fn superseded(outcome: &Outcome) -> Vec<&crate::claude::StreamEvent> {
    outcome
        .events
        .iter()
        .filter(|event| event.event_type == "stream_superseded")
        .collect()
}

fn final_text(outcome: &Outcome) -> String {
    outcome
        .events
        .iter()
        .rev()
        .find(|event| event.event_type == "assistant")
        .and_then(|event| event.data["message"]["content"][0]["text"].as_str())
        .unwrap_or_default()
        .to_string()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
#[tokio::test]
async fn upstream_503s_and_streams_that_die_mid_response_do_not_end_the_turn() {
    let outcome = run_scenario_with(
        HarnessToolRegistry::new(),
        vec![
            truncated_after_one_chunk("par"),
            upstream_error_after_one_chunk("half"),
            text_reply("the whole answer"),
        ],
        "answer me",
        ScenarioOptions {
            failures: vec![503, 503],
            retry_budget: Some(8),
            ..ScenarioOptions::default()
        },
    )
    .await;

    // Two refused requests, two broken streams, one good stream.
    assert_eq!(outcome.requests.len(), 5);
    // Every retry sent the same pre-call history; no partial text leaked in.
    for request in &outcome.requests[1..] {
        assert_eq!(request["messages"], outcome.requests[0]["messages"]);
    }
    assert_eq!(final_text(&outcome), "the whole answer");
    // Only the two streams that had output are superseded.
    let marks = superseded(&outcome);
    assert_eq!(marks.len(), 2);
    assert_eq!(marks[0].data["detail_code"], "stream_disconnected");
    assert_eq!(marks[1].data["detail_code"], "stream_server_error");
}
