//! End-to-end coverage for the CompilePrompt engine.
//!
//! Slices covered:
//! - Cache hit: engine returns without HTTP (wiremock not involved).
//! - Happy path: streaming `/api/generate` response parsed → `CompilePromptCompleted`.
//! - Supersede: a second `compile` call cancels the first in-flight request.
//! - 500 error propagation: Ollama error bubbles up as `CompilePromptFailed`.
//!
//! The three HTTP-dependent slices are driven sequentially inside a single
//! `#[tokio::test]` behind `TEST_LOCK` so that `RSI_OLLAMA_URL` is written
//! exactly once under the one wiremock server bound for the test. Integration-test
//! files compile to isolated binaries, so `TEST_LOCK` is sufficient to serialise
//! any future HTTP-dependent tests added to this file.

use rsi_common::prompt_compile::{CompileResult, LayerValidation, OutputContract};
use rsid::bus::{DaemonEvent, EventBus};
use rsid::config::{Config, RuntimeConfig};
use rsid::prompt_compile::CompileEngine;
use rsid::store::Store;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;
use wiremock::matchers::{body_string_contains, method, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Serialises HTTP-dependent tests in this binary so concurrent
/// `std::env::set_var("RSI_OLLAMA_URL", …)` calls cannot race.
/// Integration-test files compile to isolated binaries — this lock is
/// sufficient; no cross-file collision is possible.
static TEST_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

fn runtime_config() -> Arc<RuntimeConfig> {
    RuntimeConfig::from_config(&Config::from_env())
}

fn store() -> Arc<tokio::sync::Mutex<Store>> {
    let path = std::env::temp_dir().join(format!("compile-prompt-{}.db", Uuid::new_v4()));
    Arc::new(tokio::sync::Mutex::new(Store::open(&path).expect("store")))
}

fn canned_result() -> CompileResult {
    CompileResult {
        compiled: "You are executing. First, do X.".to_string(),
        contract: OutputContract::Complete,
        layer_validation: LayerValidation {
            semantic: true,
            syntactic: true,
            deictic: true,
            discourse: true,
            pragmatic: true,
        },
    }
}

#[tokio::test]
async fn cache_hit_short_circuits_and_emits_stream() {
    let bus = Arc::new(EventBus::new(256));
    let engine = CompileEngine::new(
        reqwest::Client::new(),
        store(),
        runtime_config(),
        Arc::clone(&bus),
    );
    let caller = Uuid::new_v4();
    let input = "cache-hit-integration-input".to_string();
    let model = engine
        .runtime_config_clone_for_tests()
        .prompt_compile_model_local
        .read()
        .clone();

    engine.seed_cache_for_tests(&input, &model, canned_result());

    let mut rx = bus.subscribe();
    let resp = Arc::clone(&engine)
        .compile(caller, input, None, None, None, None)
        .await;
    assert!(
        resp.cached.is_some(),
        "expected cache hit to populate cached"
    );

    let first = rx.try_recv().expect("chunk event");
    let second = rx.try_recv().expect("completed event");
    assert!(
        matches!(*first, DaemonEvent::CompilePromptChunk { .. }),
        "first event should be a chunk"
    );
    assert!(
        matches!(*second, DaemonEvent::CompilePromptCompleted { .. }),
        "second event should be completed"
    );
}

/// Supersede, happy path, and HTTP 500 error propagation — driven
/// sequentially against one `MockServer` so the `RSI_OLLAMA_URL`
/// override is deterministic regardless of parallel test runners.
#[tokio::test]
async fn http_paths_supersede_happy_and_error() {
    // Hold TEST_LOCK for the entire test so any future HTTP tests added to
    // this file cannot race on RSI_OLLAMA_URL.
    let _lock = TEST_LOCK.lock();

    let server = MockServer::start().await;

    // SAFETY: TEST_LOCK is held above; no other test in this binary can write
    // RSI_OLLAMA_URL concurrently. The var is read fresh per generate call
    // (see `ollama_client::ollama_url`), so our server binding stays pinned.
    unsafe {
        std::env::set_var("RSI_OLLAMA_URL", format!("{}/api/generate", server.uri()));
    }

    let bus = Arc::new(EventBus::new(256));
    let engine = CompileEngine::new(
        reqwest::Client::new(),
        store(),
        runtime_config(),
        Arc::clone(&bus),
    );
    let model_override = "llama3:8b".to_string(); // non-qwen: skip /no_think suffix

    // ── 1. Supersede ──────────────────────────────────────────────────────
    // Slow mock (30s delay) guarantees the first call stays in-flight until
    // the second supersedes it.
    Mock::given(method("POST"))
        .and(path_regex(r"^/api/generate$"))
        .and(body_string_contains("slow-first-call-input"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(30))
                .set_body_string("{\"response\":\"never-seen\",\"done\":true}\n"),
        )
        .mount(&server)
        .await;

    let caller_super = Uuid::new_v4();
    let mut rx_super = bus.subscribe();
    let first_resp = Arc::clone(&engine)
        .compile(
            caller_super,
            "slow-first-call-input".to_string(),
            Some(model_override.clone()),
            None,
            None,
            None,
        )
        .await;
    let first_id = first_resp.request_id;
    assert!(first_resp.cached.is_none());

    // Let the engine register the in_flight entry before the supersede fires.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Cache-hits short-circuit BEFORE the supersede check (see
    // `CompileEngine::compile`), so we force an uncached second call. Mount
    // a fast mock with a valid NDJSON response so the second call completes
    // after triggering supersede.
    let second_input = "second-winner-input".to_string();
    let fast_ndjson = format!(
        "{}\n{}\n",
        serde_json::json!({
            "response": "Given INPUT, return OUTPUT.\n\
                semantic: yes\nsyntactic: yes\ndeictic: yes\ndiscourse: yes\npragmatic: yes\n\
                COMPLETE",
            "done": false
        }),
        serde_json::json!({ "response": "", "done": true }),
    );
    Mock::given(method("POST"))
        .and(path_regex(r"^/api/generate$"))
        .and(body_string_contains("second-winner-input"))
        .respond_with(ResponseTemplate::new(200).set_body_string(fast_ndjson))
        .mount(&server)
        .await;
    let _ = Arc::clone(&engine)
        .compile(
            caller_super,
            second_input,
            Some(model_override.clone()),
            None,
            None,
            None,
        )
        .await;

    let saw_supersede = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match rx_super.recv().await {
                Ok(ev) => {
                    if let DaemonEvent::CompilePromptFailed { request_id, error } = &*ev
                        && *request_id == first_id
                        && error == "superseded"
                    {
                        return true;
                    }
                }
                Err(_) => return false,
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(
        saw_supersede,
        "expected CompilePromptFailed {{ error: \"superseded\" }} for first request_id {first_id}"
    );

    // ── 2. Happy path ─────────────────────────────────────────────────────
    // Text crafted to pass all five `validate_layers` heuristics
    // (see post_process::tests::validate_layers_full for provenance).
    let compiled_text = "You are investigating a production defect. Treat the following as a specification.\n\
                        First, extract all error-level entries from the application error logs.\n\
                        Then, for each extracted error entry, classify the entry by root cause category.\n\
                        Next, compare the timestamps and stack traces against the commit history.\n\
                        Finally, return a verdict: whether the memory leak correlates with the cache changes.\n\
                        COMPLETE";
    let ndjson = format!(
        "{}\n{}\n",
        serde_json::json!({ "response": compiled_text, "done": false }),
        serde_json::json!({ "response": "", "done": true }),
    );
    Mock::given(method("POST"))
        .and(path_regex(r"^/api/generate$"))
        .and(body_string_contains("happy-path-input"))
        .respond_with(ResponseTemplate::new(200).set_body_string(ndjson))
        .mount(&server)
        .await;

    let caller_happy = Uuid::new_v4();
    let mut rx_happy = bus.subscribe();
    let happy_resp = Arc::clone(&engine)
        .compile(
            caller_happy,
            "happy-path-input".to_string(),
            Some(model_override.clone()),
            None,
            None,
            None,
        )
        .await;
    assert!(happy_resp.cached.is_none());
    let happy_id = happy_resp.request_id;

    let completed = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match rx_happy.recv().await {
                Ok(ev) => match &*ev {
                    DaemonEvent::CompilePromptCompleted { request_id, result }
                        if *request_id == happy_id =>
                    {
                        return Some(result.clone());
                    }
                    DaemonEvent::CompilePromptFailed { request_id, error }
                        if *request_id == happy_id =>
                    {
                        panic!("happy path failed unexpectedly: {error}");
                    }
                    _ => continue,
                },
                Err(_) => return None,
            }
        }
    })
    .await
    .expect("timed out waiting for happy-path completion")
    .expect("stream closed before completion");
    assert!(matches!(completed.contract, OutputContract::Complete));
    // Layer validation is a best-effort heuristic over the compiled body; we
    // only assert that at least the semantic layer fires (our body contains
    // an operational verb).
    assert!(
        completed.layer_validation.semantic,
        "expected semantic layer to fire: {:?}",
        completed.layer_validation
    );

    // ── 3. Error propagation ──────────────────────────────────────────────
    Mock::given(method("POST"))
        .and(path_regex(r"^/api/generate$"))
        .and(body_string_contains("error-propagation-input"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let caller_err = Uuid::new_v4();
    let mut rx_err = bus.subscribe();
    let err_resp = Arc::clone(&engine)
        .compile(
            caller_err,
            "error-propagation-input".to_string(),
            Some(model_override),
            None,
            None,
            None,
        )
        .await;
    let err_id = err_resp.request_id;

    let failure = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match rx_err.recv().await {
                Ok(ev) => {
                    if let DaemonEvent::CompilePromptFailed { request_id, error } = &*ev
                        && *request_id == err_id
                    {
                        return Some(error.clone());
                    }
                }
                Err(_) => return None,
            }
        }
    })
    .await
    .expect("timed out waiting for error propagation")
    .expect("stream closed before failure event");
    assert!(
        !failure.is_empty() && failure != "superseded",
        "expected non-empty non-superseded error, got: {failure:?}"
    );
}
