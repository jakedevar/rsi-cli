//! hub #3012: the project-file RPCs resolve their root from a project or a
//! session, refuse escapes and unknown roots, and stay operator-only.
use super::*;
use rsi_common::project_files::PROJECT_FILE_METHODS;

async fn seed_project(fixture: &RecursiveDagRpcFixture, path: Option<std::path::PathBuf>) -> Uuid {
    let id = Uuid::new_v4();
    fixture
        .manager
        .store()
        .lock()
        .await
        .insert_project(&rsi_common::types::Project {
            id,
            name: format!("files-{id}"),
            path,
            description: None,
            color: rsi_common::types::Project::DEFAULT_COLOR.to_string(),
            context_files: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        })
        .unwrap();
    id
}

fn refusal(response: &RpcResponse) -> (i32, String) {
    let error = response.error.as_ref().expect("refusal");
    (error.code, error.message.clone())
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn project_file_rpcs_serve_a_project_root() {
    let fixture = recursive_dag_rpc_fixture();
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(dir.path().join("notes.md"), "a\nb\n").unwrap();
    let project = seed_project(&fixture, Some(dir.path().to_path_buf())).await;
    let root = dir.path().canonicalize().unwrap();

    let listing = call_rpc(
        &fixture.server,
        "ListProjectDirectory",
        serde_json::json!({"project_id": project, "path": ""}),
    )
    .await;
    let listing = listing.result.expect("listing");
    assert_eq!(listing["root"], root.to_string_lossy().as_ref());
    assert_eq!(listing["entries"][0]["name"], "src");
    assert_eq!(listing["entries"][0]["kind"], "dir");
    assert_eq!(listing["entries"][1]["name"], "notes.md");
    assert_eq!(listing["truncated"], false);

    let read = call_rpc(
        &fixture.server,
        "ReadProjectFile",
        serde_json::json!({"project_id": project, "path": "notes.md", "start_line": 2}),
    )
    .await;
    let read = read.result.expect("read");
    assert_eq!(read["content"], "b\n");
    assert_eq!(read["binary"], false);
    assert_eq!(read["start_line"], 2);

    let feed = call_rpc(
        &fixture.server,
        "ListProjectFiles",
        serde_json::json!({"project_id": project}),
    )
    .await;
    let feed = feed.result.expect("feed");
    assert_eq!(feed["source"], "walk");
    assert_eq!(
        feed["paths"],
        serde_json::json!(["notes.md", "src/main.rs"])
    );

    let gutter = call_rpc(
        &fixture.server,
        "GetProjectFileGitGutter",
        serde_json::json!({"project_id": project, "path": "src/main.rs"}),
    )
    .await;
    let gutter = gutter.result.expect("gutter");
    assert_eq!(gutter["status"], "not_a_repo");
    assert_eq!(gutter["markers"], serde_json::json!([]));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn project_file_rpcs_refuse_unknown_roots_escapes_and_bad_scope() {
    let fixture = recursive_dag_rpc_fixture();
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(dir.path().join("a.txt"), "a").unwrap();
    let project = seed_project(&fixture, Some(dir.path().to_path_buf())).await;
    let pathless = seed_project(&fixture, None).await;
    let absolute = dir.path().join("a.txt").to_string_lossy().into_owned();

    let cases = [
        (
            serde_json::json!({"project_id": Uuid::new_v4(), "path": "a.txt"}),
            "project_file_unknown_root",
        ),
        (
            serde_json::json!({"session_id": Uuid::new_v4(), "path": "a.txt"}),
            "project_file_unknown_root",
        ),
        (
            serde_json::json!({"project_id": pathless, "path": "a.txt"}),
            "project_file_unknown_root",
        ),
        (
            serde_json::json!({"project_id": project, "session_id": Uuid::new_v4(), "path": "a.txt"}),
            "project_file_invalid_request",
        ),
        (
            serde_json::json!({"path": "a.txt"}),
            "project_file_invalid_request",
        ),
        (
            serde_json::json!({"project_id": project, "path": "../a.txt"}),
            "project_file_outside_root",
        ),
        (
            serde_json::json!({"project_id": project, "path": absolute}),
            "project_file_outside_root",
        ),
        (
            serde_json::json!({"project_id": project, "path": "a.txt", "bogus": 1}),
            "project_file_invalid_request",
        ),
    ];
    for (params, code) in cases {
        let response = call_rpc(&fixture.server, "ReadProjectFile", params.clone()).await;
        let (rpc_code, message) = refusal(&response);
        assert_eq!(rpc_code, INVALID_PARAMS, "{params}");
        assert!(message.contains(code), "{params}: {message}");
    }
    let response = call_rpc(
        &fixture.server,
        "ListProjectDirectory",
        serde_json::json!({"project_id": Uuid::new_v4()}),
    )
    .await;
    assert!(refusal(&response).1.contains("project_file_unknown_root"));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn project_file_rpcs_resolve_a_session_working_dir() {
    let fixture = recursive_dag_rpc_fixture();
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(dir.path().join("s.txt"), "session\n").unwrap();
    let mut session = make_recursive_dag_test_session(Uuid::new_v4());
    session.working_dir = dir.path().to_path_buf();
    fixture
        .manager
        .store()
        .lock()
        .await
        .insert_session(&session)
        .unwrap();

    let read = call_rpc(
        &fixture.server,
        "ReadProjectFile",
        serde_json::json!({"session_id": session.id, "path": "s.txt"}),
    )
    .await;
    assert_eq!(read.result.expect("read")["content"], "session\n");
}

/// Operator-only: in no attributed audience, refused to a tokened caller,
/// and outside the `Remote*` family the rsi-remote gateway speaks.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn project_file_rpcs_are_operator_only_and_off_the_remote_gateway() {
    let fixture = recursive_dag_rpc_fixture();
    for method in PROJECT_FILE_METHODS {
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
        assert!(!method.starts_with("Remote"), "{method}");
        let mut request = RpcRequest::new(*method, serde_json::json!({}));
        request.session_token = Some("test-token".into());
        let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
        else {
            panic!("expected response");
        };
        let (code, message) = refusal(&response);
        assert_eq!(code, INVALID_PARAMS, "{method}");
        assert!(
            message.contains("not available to session-attributed callers"),
            "{method}"
        );
    }
}

/// One surface an RPC method can be exposed on (hub #3151).
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[derive(Clone)]
struct Surface {
    name: &'static str,
    methods: Vec<&'static str>,
}

/// Every surface an operator-only method must stay off, by underlying RPC
/// method: the attributed gate lists, the native tool mappings (provider,
/// Harness and MCP tools all derive from the catalog descriptors), the
/// `rsi-rpc agent` CLI catalog, manager operator delegation, and the daemon
/// calls the rsi-remote gateway can make.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
fn exposure_surfaces() -> Vec<Surface> {
    use rsi_common::rpc_verb_registry as registry;
    let native = rsi_common::agent_control_schema::agent_control_catalog_v1()
        .iter()
        .filter(|descriptor| descriptor.native_tool.is_some())
        .map(|descriptor| descriptor.method)
        .chain(
            registry::rpc_verb_declarations()
                .iter()
                .filter(|declaration| declaration.native_tool.is_some())
                .map(|declaration| declaration.method),
        )
        .collect();
    vec![
        Surface {
            name: "AGENT_VERBS",
            methods: agent_gate::AGENT_VERBS.to_vec(),
        },
        Surface {
            name: "READ_VERBS",
            methods: agent_gate::READ_VERBS.to_vec(),
        },
        Surface {
            name: "HOOK_VERBS",
            methods: agent_gate::HOOK_VERBS.to_vec(),
        },
        Surface {
            name: "attributed audience",
            methods: registry::rpc_verb_declarations()
                .iter()
                .map(|declaration| declaration.method)
                .collect(),
        },
        Surface {
            name: "native tool mappings",
            methods: native,
        },
        Surface {
            name: "agent CLI catalog",
            methods: registry::cli_verb_methods(),
        },
        Surface {
            name: "manager delegation",
            methods: rsi_common::manager_operator_delegation::DELEGABLE_OPERATOR_METHODS.to_vec(),
        },
        Surface {
            name: "remote gateway calls",
            methods: rsi_remote::reads::GATEWAY_DAEMON_METHODS.to_vec(),
        },
    ]
}

/// The surfaces `method` is exposed on.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
fn exposures(method: &str, surfaces: &[Surface]) -> Vec<&'static str> {
    surfaces
        .iter()
        .filter(|surface| surface.methods.contains(&method))
        .map(|surface| surface.name)
        .collect()
}

/// hub #3103 / #3151: the write methods are operator-only on every surface
/// (enumerated by `exposure_surfaces`), and a tokened caller is refused
/// before dispatch.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn project_file_write_rpcs_are_operator_only_on_every_surface() {
    use rsi_common::project_files::PROJECT_FILE_WRITE_METHODS;
    let fixture = recursive_dag_rpc_fixture();
    assert_eq!(
        PROJECT_FILE_WRITE_METHODS,
        [
            "WriteProjectFile",
            "CreateProjectEntry",
            "MoveProjectEntry",
            "CopyProjectEntry",
            "TrashProjectEntry",
            "UndoProjectFileOperation",
        ]
    );
    let surfaces = exposure_surfaces();
    for method in PROJECT_FILE_WRITE_METHODS {
        assert!(PROJECT_FILE_METHODS.contains(method), "{method}");
        assert_eq!(exposures(method, &surfaces), Vec::<&str>::new(), "{method}");
        assert_eq!(
            rsi_common::rpc_verb_registry::audience_of(method),
            None,
            "{method}"
        );
        assert!(
            !agent_gate::is_allowed_for_attributed_caller(method),
            "{method}"
        );
        let mut request = RpcRequest::new(*method, serde_json::json!({}));
        request.session_token = Some("test-token".into());
        let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
        else {
            panic!("expected response");
        };
        let (code, message) = refusal(&response);
        assert_eq!(code, INVALID_PARAMS, "{method}");
        assert!(
            message.contains("not available to session-attributed callers"),
            "{method}"
        );
    }
}

/// hub #3151: each fence fails when a write method is exposed on its
/// surface: a fixture copy of the real surfaces with one write method added
/// to one surface is caught on exactly that surface.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn project_file_write_fence_catches_a_write_method_exposed_on_any_surface() {
    use rsi_common::project_files::PROJECT_FILE_WRITE_METHODS;
    let real = exposure_surfaces();
    assert_eq!(real.len(), 8);
    for (index, surface) in real.iter().enumerate() {
        assert!(!surface.methods.is_empty(), "{}", surface.name);
        for method in PROJECT_FILE_WRITE_METHODS {
            let mut exposed = real.clone();
            exposed[index].methods.push(method);
            assert_eq!(exposures(method, &exposed), vec![surface.name], "{method}");
        }
    }
}

/// hub #3103 end to end on the operator socket: save against the read's
/// version, a stale save refused, force, then trash and undo.
#[cfg(unix)]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn project_file_write_rpcs_save_trash_and_undo_under_a_project_root() {
    let fixture = recursive_dag_rpc_fixture();
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(dir.path().join("notes.md"), "one\n").unwrap();
    let project = seed_project(&fixture, Some(dir.path().to_path_buf())).await;
    let call = |method: &'static str, params: serde_json::Value| {
        let server = &fixture.server;
        async move { call_rpc(server, method, params).await }
    };

    let read = call(
        "ReadProjectFile",
        serde_json::json!({"project_id": project, "path": "notes.md"}),
    )
    .await
    .result
    .expect("read");
    let version = read["version"].as_str().expect("version").to_string();

    let saved = call(
        "WriteProjectFile",
        serde_json::json!({"project_id": project, "path": "notes.md",
            "content": "two\n", "expected_version": version}),
    )
    .await
    .result
    .expect("save");
    assert_eq!(saved["forced"], false);
    assert_eq!(saved["size"], 4);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("notes.md")).unwrap(),
        "two\n"
    );

    let stale = call(
        "WriteProjectFile",
        serde_json::json!({"project_id": project, "path": "notes.md",
            "content": "three\n", "expected_version": version}),
    )
    .await;
    let (code, message) = refusal(&stale);
    assert_eq!(code, INVALID_PARAMS);
    assert!(message.contains("project_file_stale"), "{message}");
    let forced = call(
        "WriteProjectFile",
        serde_json::json!({"project_id": project, "path": "notes.md",
            "content": "three\n", "expected_version": version, "force": true}),
    )
    .await
    .result
    .expect("force");
    assert_eq!(forced["forced"], true);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("notes.md")).unwrap(),
        "three\n"
    );

    let trashed = call(
        "TrashProjectEntry",
        serde_json::json!({"project_id": project, "path": "notes.md"}),
    )
    .await
    .result
    .expect("trash");
    assert_eq!(trashed["op"], "trashed");
    assert_eq!(trashed["undo_depth"], 1);
    assert!(!dir.path().join("notes.md").exists());
    let undone = call(
        "UndoProjectFileOperation",
        serde_json::json!({"project_id": project}),
    )
    .await
    .result
    .expect("undo");
    assert_eq!(undone["undone"], "trashed");
    assert_eq!(undone["effect"], "restored");
    assert_eq!(undone["path"], "notes.md");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("notes.md")).unwrap(),
        "three\n"
    );

    let escape = call(
        "MoveProjectEntry",
        serde_json::json!({"project_id": project, "from": "notes.md", "to": "../x.md"}),
    )
    .await;
    assert!(refusal(&escape).1.contains("project_file_outside_root"));
    assert!(dir.path().join("notes.md").exists());
}
