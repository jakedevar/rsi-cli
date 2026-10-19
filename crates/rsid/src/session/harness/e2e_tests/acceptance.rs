//! S9 (#2259): the umbrella acceptance scenario of the native Harness (#2234).
//!
//! One scripted Mercury-shaped model server drives one Harness session through
//! the whole native tool set: search and fetch, glob, grep and ranged read, a
//! malformed call that is recovered, a multi-edit and an `apply_patch`, a shell
//! command, the plan, a control read and an MCP tool on the stdio fixture
//! server, then the final answer. The provider, loop, tool registry and tools
//! are the real ones, assembled by [`HarnessToolRegistry::default_tools`] as a
//! launch does; only the model, the search vendor and the web page are
//! loopback servers, so no test touches the network or a real credential.
//!
//! The model key sits in a vault slot and is resolved per request; the search
//! key sits in the mock vendor's backend. Each must reach only the one header
//! that carries it.

// The scenario needs a POSIX shell, `git` and `python3`; other targets skip it.
#![cfg_attr(not(unix), allow(dead_code, unused_imports))]

use super::{
    MODEL_KEY_SENTINEL, Outcome, PAGE, SEARCH_KEY_SENTINEL, ScenarioOptions, assert_well_formed,
    persist_turn, run_turn_with, sse, text_reply, tool_call_reply, usage_chunk, vendor_calls,
};
use crate::session::agent_verbs::tests::{control_handle_with_store, test_session};
use crate::session::harness::api_key::ApiCredential;
use crate::session::harness::base_prompt;
use crate::session::harness::mcp::McpBridge;
use crate::session::harness::mcp::bridge_tests::{configure, definition};
use crate::session::harness::tools::HarnessToolRegistry;
use crate::session::harness::tools::web_fetch::test_support::{html, serve, tool_for};
use crate::session::harness::tools::web_search::WebSearchTool;
use crate::session::harness::tools::web_search::serper::test_support::{
    mock_backend, mount_ok, ok_body,
};
use crate::vault::{Slot, VaultHandleBuilder, VaultSettings};
use rsi_common::types::{EventType, SessionProvider, SessionStatus};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use wiremock::MockServer;

const CONFIG_LINES: usize = 60;
const RETRY_LINE: usize = 42;

/// Names of the ambient variables the daemon process holds during the run.
/// They carry the two fake keys and are scoped to this test; neither may reach
/// the model's shell.
const AMBIENT_MODEL_KEY_VAR: &str = "RSI_E2E_ACCEPTANCE_MODEL_KEY";
const AMBIENT_SEARCH_KEY_VAR: &str = "RSI_E2E_ACCEPTANCE_SEARCH_KEY";

const PATCH: &str = "*** Begin Patch\n\
    *** Add File: docs/NOTES.md\n\
    +# Retry policy\n\
    +\n\
    +`RETRY_LIMIT` is 5.\n\
    *** Update File: src/lib.rs\n\
    @@\n\
    -pub fn retries() -> u32 {\n\
    +/// The configured retry limit.\n\
    +pub fn retries() -> u32 {\n\
    *** End Patch\n";

/// Run `git` in `dir` with a fixed identity and none of the caller's repository
/// environment.
fn git(dir: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args([
            "-c",
            "user.name=rsi-test",
            "-c",
            "user.email=rsi-test@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("UTF-8 git output")
}

/// A committed repository: `src/config.rs` mentions `RETRY_LIMIT` on line
/// [`RETRY_LINE`], `src/lib.rs` reads it, and an ignored `target/` copy
/// mentions it too.
fn repo() -> tempfile::TempDir {
    let dir = crate::test_support::disk_backed_tempdir("session-harness-e2e-acceptance-");
    let root = dir.path();
    git(root, &["init", "-q"]);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("target/debug")).unwrap();
    std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
    let config: String = (1..=CONFIG_LINES)
        .map(|line| {
            if line == RETRY_LINE {
                "const RETRY_LIMIT: u32 = 3;\n".to_string()
            } else {
                format!("// config line {line}\n")
            }
        })
        .collect();
    std::fs::write(root.join("src/config.rs"), config).unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "pub mod config;\n\npub fn retries() -> u32 {\n    config::RETRY_LIMIT\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("target/debug/stale.rs"),
        "const RETRY_LIMIT: u32 = 99;\n",
    )
    .unwrap();
    git(root, &["add", ".gitignore", "src"]);
    git(root, &["commit", "-q", "--no-verify", "-m", "baseline"]);
    dir
}

/// One model response that calls several tools at once, each call's arguments
/// split across two deltas the way Mercury streams them.
fn parallel_calls_reply(calls: &[(&str, &str, &str)]) -> String {
    let mut chunks = Vec::new();
    for (index, (id, name, arguments)) in calls.iter().enumerate() {
        let split = arguments.len() / 2;
        chunks.push(
            json!({"choices": [{"index": 0, "delta": {"role": "assistant", "tool_calls": [{
                "index": index, "id": id, "type": "function",
                "function": {"name": name, "arguments": &arguments[..split]}
            }]}}]}),
        );
        chunks.push(json!({"choices": [{"index": 0, "delta": {"tool_calls": [{
            "index": index, "function": {"arguments": &arguments[split..]}
        }]}}]}));
    }
    chunks.push(json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}));
    chunks.push(usage_chunk());
    sse(&chunks)
}

/// The `+`/`-` lines of the worktree's unstaged diff, headers excluded.
fn changed_lines(root: &Path) -> Vec<String> {
    git(root, &["diff", "--unified=0"])
        .lines()
        .filter(|line| {
            (line.starts_with('+') || line.starts_with('-'))
                && !line.starts_with("+++")
                && !line.starts_with("---")
        })
        .map(str::to_string)
        .collect()
}

fn status_lines(root: &Path) -> Vec<String> {
    let mut lines: Vec<String> = git(root, &["status", "--porcelain", "--untracked-files=all"])
        .lines()
        .map(str::to_string)
        .collect();
    lines.sort();
    lines
}

/// What every request of a Mercury session carries and must not carry.
fn assert_mercury_wire_profile(outcome: &Outcome, max_output: u32) {
    for body in &outcome.requests {
        assert_eq!(body["model"], "mercury-2");
        assert_eq!(body["reasoning_effort"], "medium");
        assert_eq!(body["max_completion_tokens"], max_output);
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
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"], json!({"include_usage": true}));
        assert!(
            body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .all(|message| message["role"] != "developer")
        );
        // Mercury caps a function name at 64 characters of `[A-Za-z0-9_-]`.
        for tool in body["tools"].as_array().expect("tools") {
            let name = tool["function"]["name"].as_str().expect("tool name");
            assert!(
                !name.is_empty()
                    && name.len() <= 64
                    && name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
                "{name}"
            );
        }
    }
}

/// The Mercury session end to end over the whole native tool set.
#[cfg(unix)]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
#[tokio::test]
async fn mercury_session_runs_the_whole_native_tool_set_end_to_end() {
    let repo = repo();
    let root = repo.path().to_path_buf();
    let session_id = uuid::Uuid::new_v4();
    let scratch = tempfile::TempDir::new().expect("scratch dir");

    // The model key lives in a vault slot and is read on every request.
    let vault = VaultHandleBuilder::new(Arc::new(VaultSettings::default()))
        .dir(scratch.path().join("vault"))
        .env(|_| None)
        .open()
        .expect("vault");
    vault
        .set(Slot::Inception, MODEL_KEY_SENTINEL)
        .expect("model key");

    // The control handle, the session row it reads and the stdio MCP server
    // all share the one store the loop's admissions use.
    let (control, store) = control_handle_with_store();
    {
        let mut session = test_session(session_id, root.clone());
        session.status = SessionStatus::Running;
        session.provider = SessionProvider::Harness;
        session.model = Some("mercury-2".to_string());
        store
            .lock()
            .await
            .insert_session(&session)
            .expect("session row");
    }
    configure(&store, &definition("docs", "basic", 1, &scratch, false)).await;
    let mut bridge = McpBridge::build(&store, &vault).await;

    // The launch's own registry assembly, with the two web tools aimed at
    // loopback servers (the production ones refuse loopback by design).
    let vendor = MockServer::start().await;
    mount_ok(&vendor, ok_body()).await;
    let (page_server, pages) = serve(|_| html(PAGE)).await;
    let mut registry = HarnessToolRegistry::default_tools_with_execution_scratch(
        None,
        None,
        Some(Arc::clone(&store)),
        Some(session_id),
        Some(uuid::Uuid::new_v4()),
        Some(root.clone()),
        Some(SessionProvider::Harness),
        Some("mercury-2".to_string()),
        Some(control),
        false,
        None,
    );
    registry.replace_for_test(Arc::new(tool_for(page_server)));
    registry.register(Arc::new(WebSearchTool::new(Arc::new(mock_backend(
        &vendor,
    )))));
    bridge.register(&mut registry);

    let edits = json!({
        "path": "src/config.rs",
        "edits": [
            {"old_string": "// config line 7\n", "new_string": "// config line seven\n"},
            {
                "old_string": "const RETRY_LIMIT: u32 = 3;",
                "new_string": "const RETRY_LIMIT: u32 = 5;"
            }
        ]
    });
    let plan = json!({
        "explanation": "Checkpoint after the edits",
        "plan": [
            {"step": "Find the retry constant", "status": "completed"},
            {"step": "Raise it to 5", "status": "completed"},
            {"step": "Document the policy", "status": "completed"}
        ]
    });
    let replies = vec![
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
        parallel_calls_reply(&[
            ("call_glob", "glob", r#"{"pattern":"**/*.rs"}"#),
            (
                "call_grep",
                "grep",
                r#"{"pattern":"RETRY_LIMIT","output_mode":"content"}"#,
            ),
        ]),
        // A malformed call: no `path`.
        tool_call_reply("call_bad", "read_file", r#"{"offset":"2"}"#),
        tool_call_reply(
            "call_read",
            "read_file",
            &json!({"path": "src/config.rs", "offset": RETRY_LINE - 2, "limit": 5}).to_string(),
        ),
        tool_call_reply("call_edit", "file_edit", &edits.to_string()),
        tool_call_reply(
            "call_patch",
            "apply_patch",
            &json!({ "patch": PATCH }).to_string(),
        ),
        tool_call_reply(
            "call_exec",
            "exec_command",
            &json!({"cmd": "wc -l < src/config.rs; env", "yield_time_ms": 10000}).to_string(),
        ),
        tool_call_reply("call_plan", "update_plan", &plan.to_string()),
        tool_call_reply("call_status", "rsi_control_status", "{}"),
        tool_call_reply("call_mcp", "mcp__docs__echo", "{}"),
        text_reply("Raised RETRY_LIMIT to 5 and documented it in docs/NOTES.md."),
    ];

    let options = ScenarioOptions {
        effort: Some("medium"),
        max_iterations: 40,
        working_dir: Some(root.clone()),
        system_prompt: base_prompt::compose_system_prompt(
            None,
            Some("Work only inside the sandbox you were given.".to_string()),
        ),
        credential: Some(ApiCredential::for_slot_only_in(&vault, Slot::Inception)),
        ..ScenarioOptions::default()
    };
    let max_output = options
        .quirks
        .default_max_output_tokens
        .expect("the Mercury route's output budget");

    // The daemon process holds both keys while the model's shell runs.
    // SAFETY: the variable names are unique to this test and removed below.
    unsafe {
        std::env::set_var(AMBIENT_MODEL_KEY_VAR, MODEL_KEY_SENTINEL);
        std::env::set_var(AMBIENT_SEARCH_KEY_VAR, SEARCH_KEY_SENTINEL);
    }
    let query = "Raise the retry limit to 5 and write the policy down.";
    let outcome = run_turn_with(
        &store, session_id, "launch", registry, replies, query, options,
    )
    .await;
    // SAFETY: test cleanup; these names are unique to this test.
    unsafe {
        std::env::remove_var(AMBIENT_MODEL_KEY_VAR);
        std::env::remove_var(AMBIENT_SEARCH_KEY_VAR);
    }

    // One model request per scripted reply, each on Mercury's wire profile,
    // and the first leads with the base instructions (#2258).
    assert_eq!(outcome.requests.len(), 12);
    assert_mercury_wire_profile(&outcome, max_output);
    let first_messages = outcome.requests[0]["messages"].as_array().unwrap();
    assert_eq!(first_messages[0]["role"], "system");
    assert!(
        first_messages[0]["content"]
            .as_str()
            .unwrap()
            .starts_with(base_prompt::base_instructions())
    );
    let advertised = outcome.tool_names(0);
    for name in [
        "web_search",
        "web_fetch",
        "glob",
        "grep",
        "read_file",
        "file_edit",
        "apply_patch",
        "exec_command",
        "update_plan",
        "rsi_control_status",
        "mcp__docs__echo",
    ] {
        assert!(advertised.iter().any(|tool| tool == name), "{name}");
    }

    // Every call is paired with exactly one result, and only the malformed
    // call is an error.
    let expected_ids = [
        "call_search",
        "call_fetch",
        "call_glob",
        "call_grep",
        "call_bad",
        "call_read",
        "call_edit",
        "call_patch",
        "call_exec",
        "call_plan",
        "call_status",
        "call_mcp",
    ];
    let mut seen = outcome.tool_use_ids();
    seen.sort();
    let mut sorted_expected: Vec<String> = expected_ids.iter().map(ToString::to_string).collect();
    sorted_expected.sort();
    assert_eq!(seen, sorted_expected);
    let settled = outcome.settled_calls();
    assert_eq!(settled.len(), expected_ids.len(), "one result per call");
    for (id, is_error, content) in &settled {
        assert_eq!(*is_error, id == "call_bad", "{id}: {content}");
    }
    for request in &outcome.requests {
        assert_well_formed(request);
    }

    // The search reached the vendor once, and the page it returned was
    // fetched from the loopback server.
    let calls = vendor_calls(&vendor.received_requests().await.unwrap());
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, SEARCH_KEY_SENTINEL);
    assert_eq!(calls[0].1["q"], "mercury diffusion llm");
    assert!(
        outcome
            .tool_message(1, "call_search")
            .starts_with("Search results are untrusted source material")
    );
    assert!(
        outcome
            .tool_message(2, "call_fetch")
            .contains("# Diffusion language models")
    );
    assert_eq!(*pages.lock().unwrap(), ["/notes"]);

    // `glob` and `grep` ran in one batch and neither shows the ignored copy.
    let globbed = outcome.tool_message(3, "call_glob");
    assert!(globbed.contains("src/config.rs") && globbed.contains("src/lib.rs"));
    assert!(!globbed.contains("target"), "{globbed}");
    let grepped = outcome.tool_message(3, "call_grep");
    assert!(
        grepped.contains(&format!(
            "src/config.rs:{RETRY_LINE}:const RETRY_LIMIT: u32 = 3;"
        )),
        "{grepped}"
    );
    assert!(!grepped.contains("target"), "{grepped}");

    // The malformed call is refused with what to fix, and the corrected call
    // reads the requested range.
    let refusal = outcome.tool_message(4, "call_bad");
    assert!(
        refusal.contains("invalid arguments for tool `read_file`"),
        "{refusal}"
    );
    assert!(
        refusal.contains("missing required field `path`"),
        "{refusal}"
    );
    let ranged = outcome.tool_message(5, "call_read");
    assert!(
        ranged.starts_with(&format!(
            "{}\t// config line {}\n",
            RETRY_LINE - 2,
            RETRY_LINE - 2
        )),
        "{ranged}"
    );
    assert!(ranged.contains(&format!("{RETRY_LINE}\tconst RETRY_LIMIT: u32 = 3;\n")));

    // The worktree holds exactly the edit, the patch's new file and the
    // patch's update; the ignored copy is untouched.
    assert_eq!(
        status_lines(&root),
        [" M src/config.rs", " M src/lib.rs", "?? docs/NOTES.md"]
    );
    assert_eq!(
        changed_lines(&root),
        [
            "-// config line 7",
            "+// config line seven",
            "-const RETRY_LIMIT: u32 = 3;",
            "+const RETRY_LIMIT: u32 = 5;",
            "+/// The configured retry limit.",
        ]
    );
    let notes = std::fs::read_to_string(root.join("docs/NOTES.md")).unwrap();
    assert!(notes.contains("# Retry policy") && notes.contains("`RETRY_LIMIT` is 5."));
    assert_eq!(
        std::fs::read_to_string(root.join("target/debug/stale.rs")).unwrap(),
        "const RETRY_LIMIT: u32 = 99;\n"
    );

    // The shell ran, saw a scrubbed environment, and the plan, control read
    // and MCP tool each answered.
    let exec: Value = serde_json::from_str(&outcome.tool_message(8, "call_exec"))
        .expect("exec_command answers with typed JSON");
    assert_eq!(exec["exit_code"], 0, "{exec}");
    let shell_output = exec["output"].as_str().expect("shell output");
    assert_eq!(
        shell_output.lines().next().map(str::trim),
        Some(CONFIG_LINES.to_string().as_str())
    );
    assert!(
        shell_output.lines().skip(1).any(|line| line.contains('=')),
        "`env` printed the shell's variables: {shell_output}"
    );
    for secret in [
        MODEL_KEY_SENTINEL,
        SEARCH_KEY_SENTINEL,
        AMBIENT_MODEL_KEY_VAR,
        AMBIENT_SEARCH_KEY_VAR,
    ] {
        assert!(!shell_output.contains(secret), "{secret} reached the shell");
    }
    // The plan itself travels as a typed stream event; the model is told only
    // that it was recorded.
    assert_eq!(outcome.tool_message(9, "call_plan"), "Plan updated.");
    let plan_event = outcome
        .events
        .iter()
        .find(|event| event.event_type == "plan_update")
        .expect("a plan event");
    assert_eq!(plan_event.data["explanation"], "Checkpoint after the edits");
    assert_eq!(plan_event.data["plan"].as_array().unwrap().len(), 3);
    let status: Value = serde_json::from_str(&outcome.tool_message(10, "call_status"))
        .expect("rsi_control_status answers with the session");
    assert_eq!(status["id"], session_id.to_string());
    assert!(outcome.tool_message(11, "call_mcp").contains("called"));

    // The session ends with the model's answer.
    assert!(outcome.events.iter().any(|event| {
        event.event_type == "assistant" && event.data.to_string().contains("documented it")
    }));
    assert!(
        outcome
            .events
            .iter()
            .any(|event| event.event_type == "result" && event.data["subtype"] == "turn_completed")
    );

    // Usage: Mercury's nested reasoning tokens add up over the twelve calls.
    let usage = outcome.result_usage();
    assert_eq!(usage["reasoning_output_tokens"], 12 * 12);
    assert_eq!(usage["cache_read_input_tokens"], 64);

    // The model key reached only the `Authorization` header of the model
    // server, and the search key only the vendor's `X-API-KEY` header: neither
    // is in any request body, tool output, stream event or persisted row.
    assert_eq!(outcome.authorizations.len(), 12);
    outcome.assert_key_only_in_authorization();
    outcome.assert_secret_absent(SEARCH_KEY_SENTINEL);
    let mut sequence = 0;
    let rows = persist_turn(session_id, query, &outcome, &mut sequence);
    assert!(
        rows.iter()
            .any(|row| row.event_type == EventType::ToolResult
                && row.content.contains("Plan updated")),
        "the turn persisted its tool results"
    );
    for row in &rows {
        let stored = serde_json::to_string(row).unwrap();
        assert!(!stored.contains(SEARCH_KEY_SENTINEL), "{stored}");
        assert!(!stored.contains(MODEL_KEY_SENTINEL), "{stored}");
    }
}
