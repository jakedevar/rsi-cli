//! S7 (#2257): a Mercury-shaped session recovers from its own bad tool calls.
//! The model, the provider and the loop are the real ones; only the model
//! server is scripted, so no test touches the network or a key.
//!
//! A call with a missing field, a misspelt tool name or no id is answered with
//! what to fix, and a call that keeps failing the same way is nudged once.

use super::{Outcome, run_scenario_in, text_reply, tool_call_reply};
use crate::session::harness::tools::HarnessToolRegistry;
use crate::session::harness::tools::file::{EditFileTool, ReadFileTool};
use crate::session::harness::tools::glob::GlobTool;
use crate::session::harness::tools::grep::GrepTool;
use serde_json::{Value, json};
use std::sync::Arc;

const NUDGE_PREFIX: &str = "[harness] You called `read_file` with the same arguments 3 times";

fn fixture() -> tempfile::TempDir {
    let dir = crate::test_support::disk_backed_tempdir("session-harness-e2e-recovery-");
    std::fs::write(dir.path().join("notes.txt"), "alpha\nbeta\n").unwrap();
    dir
}

fn registry() -> HarnessToolRegistry {
    let mut registry = HarnessToolRegistry::new();
    registry.register(Arc::new(ReadFileTool));
    registry.register(Arc::new(EditFileTool));
    registry.register(Arc::new(GrepTool));
    registry.register(Arc::new(GlobTool));
    registry
}

/// The text of the error the model was shown for `call_id`. The Chat
/// Completions wire carries a tool error as a JSON envelope.
fn error_message(outcome: &Outcome, request: usize, call_id: &str) -> String {
    let wire = outcome.tool_message(request, call_id);
    let envelope: Value = serde_json::from_str(&wire)
        .unwrap_or_else(|_| panic!("an error tool message is a JSON envelope: {wire}"));
    assert_eq!(envelope["error"]["type"], "tool_execution", "{wire}");
    envelope["error"]["message"].as_str().unwrap().to_string()
}

fn read_args(path: &str) -> String {
    json!({"path": path}).to_string()
}

/// The `user` messages of one model request that carry the repeated-failure
/// nudge.
fn nudges_in(outcome: &Outcome, request: usize) -> Vec<String> {
    outcome.requests[request]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "user")
        .filter_map(|message| message["content"].as_str())
        .filter(|content| content.contains("with the same arguments"))
        .map(str::to_string)
        .collect()
}

fn repeat_failure_events(outcome: &Outcome) -> Vec<Value> {
    outcome
        .events
        .iter()
        .filter(|event| event.event_type == "repeat_failure")
        .map(|event| event.data.clone())
        .collect()
}

/// A call with no `path` is refused with an error that names it, and the
/// corrected call then reads the file.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
#[tokio::test]
async fn a_missing_required_field_is_named_and_the_corrected_call_succeeds() {
    let repo = fixture();
    let outcome = run_scenario_in(
        registry(),
        vec![
            tool_call_reply("call_bad", "read_file", &json!({"offset": "2"}).to_string()),
            tool_call_reply("call_good", "read_file", &read_args("notes.txt")),
            text_reply("It says alpha then beta."),
        ],
        "What do the notes say?",
        repo.path().to_path_buf(),
    )
    .await;

    let refusal = error_message(&outcome, 1, "call_bad");
    assert!(
        refusal.starts_with("Error: invalid arguments for tool `read_file`; it was not run:"),
        "{refusal}"
    );
    assert!(
        refusal.contains("- missing required field `path` (expected string)"),
        "{refusal}"
    );
    assert!(
        refusal.contains("Expected: read_file(path: string"),
        "{refusal}"
    );

    let settled = outcome.settled_calls();
    assert_eq!(settled.len(), 2);
    assert!(settled[0].1, "the refused call is an error: {settled:?}");
    assert!(!settled[1].1, "the corrected call succeeds: {settled:?}");
    let read = outcome.tool_message(2, "call_good");
    assert!(read.starts_with("1\talpha\n2\tbeta"), "{read}");
    assert!(repeat_failure_events(&outcome).is_empty());
    outcome.assert_key_only_in_authorization();
}

/// A wrongly typed field is listed with the type it needs; a numeric string
/// is still coerced rather than refused.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
#[tokio::test]
async fn invalid_types_are_listed_together_and_numeric_strings_still_coerce() {
    let repo = fixture();
    let outcome = run_scenario_in(
        registry(),
        vec![
            tool_call_reply(
                "call_bad",
                "read_file",
                &json!({"path": 7, "limit": "many", "file": "notes.txt"}).to_string(),
            ),
            tool_call_reply(
                "call_coerced",
                "read_file",
                &json!({"path": "notes.txt", "offset": "2"}).to_string(),
            ),
            text_reply("Done."),
        ],
        "Read the notes.",
        repo.path().to_path_buf(),
    )
    .await;

    let refusal = error_message(&outcome, 1, "call_bad");
    assert!(
        refusal.contains("- `path`: expected string, got number 7"),
        "{refusal}"
    );
    assert!(
        refusal.contains("- `limit`: expected integer, got string \"many\""),
        "{refusal}"
    );
    assert!(
        refusal.contains("This tool takes no field `file`"),
        "{refusal}"
    );
    let coerced = outcome.tool_message(2, "call_coerced");
    assert!(coerced.starts_with("2\tbeta"), "{coerced}");
}

/// `read` is not a tool; the error suggests `read_file` and lists what can be
/// called, and the retry under the right name works.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
#[tokio::test]
async fn an_unknown_tool_name_is_answered_with_a_suggestion() {
    let repo = fixture();
    let outcome = run_scenario_in(
        registry(),
        vec![
            tool_call_reply("call_read", "read", &read_args("notes.txt")),
            tool_call_reply("call_read_file", "read_file", &read_args("notes.txt")),
            text_reply("Read it."),
        ],
        "Read the notes.",
        repo.path().to_path_buf(),
    )
    .await;

    let refusal = error_message(&outcome, 1, "call_read");
    assert!(
        refusal.starts_with("Error: Unknown tool: read. Did you mean: read_file?"),
        "{refusal}"
    );
    assert!(
        refusal.contains("Available tools: read_file, file_edit, grep, glob."),
        "{refusal}"
    );
    let settled = outcome.settled_calls();
    assert!(settled[0].1 && !settled[1].1, "{settled:?}");
    assert!(
        outcome
            .tool_message(2, "call_read_file")
            .starts_with("1\talpha")
    );
}

/// The same call failing the same way three times is nudged once, after the
/// third result, and never again however many more times it fails.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
#[tokio::test]
async fn three_identical_failures_produce_exactly_one_nudge() {
    let repo = fixture();
    let missing = read_args("missing.txt");
    let outcome = run_scenario_in(
        registry(),
        vec![
            tool_call_reply("c1", "read_file", &missing),
            tool_call_reply("c2", "read_file", &missing),
            tool_call_reply("c3", "read_file", &missing),
            tool_call_reply("c4", "read_file", &missing),
            text_reply("The file does not exist."),
        ],
        "Read missing.txt.",
        repo.path().to_path_buf(),
    )
    .await;

    assert_eq!(outcome.requests.len(), 5);
    // Nothing before the third failure; one nudge from the request after it on.
    for request in 0..3 {
        assert!(nudges_in(&outcome, request).is_empty(), "request {request}");
    }
    for request in 3..5 {
        let nudges = nudges_in(&outcome, request);
        assert_eq!(nudges.len(), 1, "request {request}: {nudges:?}");
        assert!(nudges[0].starts_with(NUDGE_PREFIX), "{}", nudges[0]);
        assert!(nudges[0].contains("Do not repeat the call unchanged"));
    }

    let events = repeat_failure_events(&outcome);
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["name"], "read_file");
    assert_eq!(events[0]["count"], 3);
    assert!(
        events[0]["error"]
            .as_str()
            .unwrap()
            .starts_with("Error reading file"),
        "{events:?}"
    );

    // The nudge follows every result of its step: each call is still paired.
    let messages = outcome.requests[3]["messages"].as_array().unwrap();
    let nudge_at = messages
        .iter()
        .position(|message| {
            message["content"]
                .as_str()
                .is_some_and(|content| content.starts_with(NUDGE_PREFIX))
        })
        .unwrap();
    assert_eq!(messages[nudge_at - 1]["tool_call_id"], "c3");
    outcome.assert_key_only_in_authorization();
}

/// Failures that differ in their arguments, or that a success interrupts, are
/// not "the same": no nudge.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
#[tokio::test]
async fn different_or_interrupted_failures_are_not_nudged() {
    let repo = fixture();
    let outcome = run_scenario_in(
        registry(),
        vec![
            tool_call_reply("c1", "read_file", &read_args("a.txt")),
            tool_call_reply("c2", "read_file", &read_args("b.txt")),
            tool_call_reply("c3", "read_file", &read_args("c.txt")),
            tool_call_reply("c4", "read_file", &read_args("a.txt")),
            tool_call_reply("c5", "read_file", &read_args("notes.txt")),
            text_reply("None of those exist."),
        ],
        "Find the file.",
        repo.path().to_path_buf(),
    )
    .await;

    assert_eq!(outcome.requests.len(), 6);
    for request in 0..6 {
        assert!(nudges_in(&outcome, request).is_empty(), "request {request}");
    }
    assert!(repeat_failure_events(&outcome).is_empty());
}

/// A call the provider sent with no id still gets a result in the history:
/// the loop gives it an id, so normalization has nothing to repair.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
#[tokio::test]
async fn a_tool_call_with_no_id_is_paired_and_nothing_is_dropped() {
    let repo = fixture();
    let outcome = run_scenario_in(
        registry(),
        vec![
            tool_call_reply("", "read_file", &read_args("notes.txt")),
            text_reply("It says alpha."),
        ],
        "Read the notes.",
        repo.path().to_path_buf(),
    )
    .await;

    let messages = outcome.requests[1]["messages"].as_array().unwrap();
    let assistant = messages
        .iter()
        .find(|message| message["role"] == "assistant" && message["tool_calls"].is_array())
        .expect("the assistant tool call is in the history");
    let call_id = assistant["tool_calls"][0]["id"].as_str().unwrap();
    assert!(call_id.starts_with("call_"), "{call_id}");
    let result = messages
        .iter()
        .find(|message| message["role"] == "tool")
        .expect("the call's result is in the history");
    assert_eq!(result["tool_call_id"], call_id);
    assert!(result["content"].as_str().unwrap().starts_with("1\talpha"));

    // Normalization neither synthesized a result nor dropped one.
    assert!(
        outcome
            .events
            .iter()
            .all(|event| event.event_type != "history_repaired"),
        "{:?}",
        outcome.events
    );
    let settled = outcome.settled_calls();
    assert_eq!(settled.len(), 1);
    assert_eq!(settled[0].0, call_id);
    assert!(!settled[0].1);
}

/// Two id-less calls in one step get distinct ids, and ids made in a later
/// step do not repeat them.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
#[tokio::test]
async fn synthesized_ids_are_distinct_across_the_run() {
    let repo = fixture();
    let outcome = run_scenario_in(
        registry(),
        vec![
            tool_call_reply("", "read_file", &read_args("notes.txt")),
            tool_call_reply("", "grep", &json!({"pattern": "beta"}).to_string()),
            text_reply("Found."),
        ],
        "Look around.",
        repo.path().to_path_buf(),
    )
    .await;

    let ids: Vec<String> = outcome.requests[2]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|message| message["tool_calls"].as_array())
        .flatten()
        .map(|call| call["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids.len(), 2, "{ids:?}");
    assert_ne!(ids[0], ids[1]);
    let results: Vec<&Value> = outcome.requests[2]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "tool")
        .collect();
    assert_eq!(results.len(), 2);
    for (id, result) in ids.iter().zip(results) {
        assert_eq!(result["tool_call_id"], id.as_str());
    }
    assert!(
        outcome
            .events
            .iter()
            .all(|event| event.event_type != "history_repaired")
    );
}

/// A call with no name cannot run: it gets a tool error that lists the tools,
/// and the next call under a real name works.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
#[tokio::test]
async fn a_tool_call_with_no_name_returns_a_tool_error() {
    let repo = fixture();
    let outcome = run_scenario_in(
        registry(),
        vec![
            tool_call_reply("call_nameless", "", &read_args("notes.txt")),
            tool_call_reply("call_named", "read_file", &read_args("notes.txt")),
            text_reply("Read it."),
        ],
        "Read the notes.",
        repo.path().to_path_buf(),
    )
    .await;

    let refusal = error_message(&outcome, 1, "call_nameless");
    assert!(
        refusal.starts_with("Error: The tool call has an empty name."),
        "{refusal}"
    );
    assert!(refusal.contains("Available tools: read_file"), "{refusal}");
    let settled = outcome.settled_calls();
    assert!(settled[0].1 && !settled[1].1, "{settled:?}");
}
