//! hub #3040 / local #74: the prompt-file RPCs round-trip a plain name, refuse
//! escaping names, delete into a recoverable trash, and stay operator-only.
//! Every test points the server at its own temp data directory.
use super::*;
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
use rsi_common::prompt_files::PROMPT_FILE_METHODS;

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
fn refusal_message(response: &RpcResponse) -> String {
    let error = response.error.as_ref().expect("refusal");
    assert_eq!(error.code, INVALID_PARAMS, "{}", error.message);
    error.message.clone()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn prompt_file_rpcs_round_trip_and_delete_into_the_trash() {
    let mut fixture = recursive_dag_rpc_fixture();
    let data = tempfile::TempDir::new().unwrap();
    fixture.server.prompt_data_dir = data.path().to_path_buf();

    let put = call_rpc(
        &fixture.server,
        "PutPromptFile",
        serde_json::json!({"name": "plan.md", "content": "# Plan\n\nsteps\n", "create_only": true}),
    )
    .await;
    let put = put.result.expect("put");
    assert_eq!(put["name"], "plan.md");
    assert_eq!(put["created"], true);
    assert_eq!(put["size"], 14);

    let listing = call_rpc(&fixture.server, "ListPromptFiles", serde_json::Value::Null).await;
    let listing = listing.result.expect("listing");
    assert_eq!(listing["files"][0]["name"], "plan.md");
    assert_eq!(listing["files"][0]["first_line"], "Plan");
    assert_eq!(listing["truncated"], false);

    let read = call_rpc(
        &fixture.server,
        "GetPromptFile",
        serde_json::json!({"name": "plan.md"}),
    )
    .await;
    assert_eq!(read.result.expect("get")["content"], "# Plan\n\nsteps\n");

    let deleted = call_rpc(
        &fixture.server,
        "DeletePromptFile",
        serde_json::json!({"name": "plan.md"}),
    )
    .await;
    let deleted = deleted.result.expect("delete");
    let trash_path = std::path::PathBuf::from(deleted["trash_path"].as_str().unwrap());
    assert_eq!(
        trash_path.parent().unwrap(),
        std::fs::canonicalize(data.path().join("trash/prompts")).unwrap()
    );
    assert_eq!(
        std::fs::read_to_string(&trash_path).unwrap(),
        "# Plan\n\nsteps\n"
    );
    let listing = call_rpc(&fixture.server, "ListPromptFiles", serde_json::json!({})).await;
    assert_eq!(
        listing.result.expect("listing")["files"],
        serde_json::json!([])
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn prompt_file_rpcs_refuse_escaping_names_and_unknown_fields() {
    let mut fixture = recursive_dag_rpc_fixture();
    let data = tempfile::TempDir::new().unwrap();
    fixture.server.prompt_data_dir = data.path().to_path_buf();
    std::fs::write(data.path().join("outside.md"), "keep").unwrap();
    let absolute = data.path().join("outside.md").display().to_string();

    let cases = [
        (
            "GetPromptFile",
            serde_json::json!({"name": "../outside.md"}),
            "prompt_file_invalid_name",
        ),
        (
            "GetPromptFile",
            serde_json::json!({"name": "a/b.md"}),
            "prompt_file_invalid_name",
        ),
        (
            "GetPromptFile",
            serde_json::json!({"name": "a\\b.md"}),
            "prompt_file_invalid_name",
        ),
        (
            "GetPromptFile",
            serde_json::json!({"name": absolute}),
            "prompt_file_invalid_name",
        ),
        (
            "GetPromptFile",
            serde_json::json!({"name": "a.txt"}),
            "prompt_file_invalid_name",
        ),
        (
            "PutPromptFile",
            serde_json::json!({"name": "../outside.md", "content": "x"}),
            "prompt_file_invalid_name",
        ),
        (
            "DeletePromptFile",
            serde_json::json!({"name": ".."}),
            "prompt_file_invalid_name",
        ),
        (
            "GetPromptFile",
            serde_json::json!({"name": "a.md", "path": "/etc"}),
            "prompt_file_invalid_request",
        ),
        (
            "ListPromptFiles",
            serde_json::json!({"dir": "/"}),
            "prompt_file_invalid_request",
        ),
        (
            "PutPromptFile",
            serde_json::json!({"name": "a.md"}),
            "prompt_file_invalid_request",
        ),
        (
            "GetPromptFile",
            serde_json::json!({"name": "missing.md"}),
            "prompt_file_not_found",
        ),
    ];
    for (method, params, code) in cases {
        let response = call_rpc(&fixture.server, method, params.clone()).await;
        let message = refusal_message(&response);
        assert!(message.contains(code), "{method} {params}: {message}");
    }
    assert_eq!(
        std::fs::read_to_string(data.path().join("outside.md")).unwrap(),
        "keep"
    );
}

/// The operator-only fence: in no attributed audience, not a native tool or
/// agent CLI verb, refused to a tokened caller, and outside the `Remote*`
/// family the rsi-remote gateway speaks.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn prompt_file_rpcs_are_operator_only_and_off_the_remote_gateway() {
    let fixture = recursive_dag_rpc_fixture();
    assert_eq!(
        PROMPT_FILE_METHODS,
        &[
            "ListPromptFiles",
            "GetPromptFile",
            "PutPromptFile",
            "DeletePromptFile"
        ]
    );
    let cli = rsi_common::rpc_verb_registry::cli_verb_methods();
    for method in PROMPT_FILE_METHODS {
        assert!(
            !agent_gate::is_allowed_for_attributed_caller(method),
            "{method}"
        );
        assert!(!agent_gate::READ_VERBS.contains(method), "{method}");
        assert!(!agent_gate::AGENT_VERBS.contains(method), "{method}");
        assert_eq!(
            rsi_common::rpc_verb_registry::audience_of(method),
            None,
            "{method}"
        );
        assert!(!cli.contains(method), "{method}");
        assert!(!method.starts_with("Remote"), "{method}");
        let mut request =
            RpcRequest::new(*method, serde_json::json!({"name": "a.md", "content": "x"}));
        request.session_token = Some("test-token".into());
        let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
        else {
            panic!("expected response");
        };
        let message = refusal_message(&response);
        assert!(
            message.contains("not available to session-attributed callers"),
            "{method}: {message}"
        );
    }
    // The tokened calls touched nothing.
    assert!(!fixture.server.prompt_data_dir.exists());
}
