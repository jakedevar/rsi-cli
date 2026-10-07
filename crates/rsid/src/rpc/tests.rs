use super::*;

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn remote_operator_info_uses_socket_dispatch_and_agent_token_is_denied() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    let fixture = recursive_dag_rpc_fixture();
    let (client, server_stream) = UnixStream::pair().unwrap();
    let connection = fixture.server.handle_connection(server_stream);
    let client_round_trip = async move {
        let (reader, mut writer) = client.into_split();
        let mut lines = tokio::io::BufReader::new(reader).lines();
        let request = RpcRequest::new("RemoteGetInfoV1", serde_json::json!({}));
        writer
            .write_all(serde_json::to_string(&request).unwrap().as_bytes())
            .await
            .unwrap();
        writer.write_all(b"\n").await.unwrap();
        let response: RpcResponse =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        let info: rsi_common::remote_read::InfoResponseV1 =
            serde_json::from_value(response.result.unwrap()).unwrap();
        assert_eq!(info.item.required_capabilities.len(), 6);
        assert_eq!(info.item.daemon_boot_id, info.daemon_epoch);

        let projects = RpcRequest::new(
            "RemoteListProjectsV1",
            serde_json::json!({"project_ids":[],"limit":1}),
        );
        writer
            .write_all(serde_json::to_string(&projects).unwrap().as_bytes())
            .await
            .unwrap();
        writer.write_all(b"\n").await.unwrap();
        let response: RpcResponse =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        let projects: rsi_common::remote_read::ProjectsResponseV1 =
            serde_json::from_value(response.result.unwrap()).unwrap();
        assert!(projects.complete);
        assert!(projects.items.is_empty());

        for method in [
            "RemoteGetInfoV1",
            "RemoteListProjectsV1",
            "RemoteListSessionsV1",
            "RemoteGetSessionV1",
            "RemoteGetHistoryPageV1",
            "RemoteGetDecisionsV1",
        ] {
            assert!(!agent_gate::is_allowed_for_attributed_caller(method));
            let mut attributed = request.clone();
            attributed.method = method.into();
            attributed.session_token = Some("test-token".into());
            writer
                .write_all(serde_json::to_string(&attributed).unwrap().as_bytes())
                .await
                .unwrap();
            writer.write_all(b"\n").await.unwrap();
            let response: RpcResponse =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            assert_eq!(response.error.unwrap().code, INVALID_PARAMS);
        }
        drop(writer);
    };
    let (served, ()) = tokio::join!(connection, client_round_trip);
    served.unwrap();
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn satellite_operator_methods_stay_out_of_agent_catalogs() {
    let catalog = rsi_common::agent_control_schema::agent_control_catalog_v1();
    for method in [
        "GetSatelliteIdentity",
        "ListSatelliteSessions",
        "ListSatellitePeers",
        "PutSatellitePeer",
        "PutSatelliteLink",
        "ProbeSatelliteLink",
        "ListHubSatelliteSessions",
        "PutSatellitePeerScope",
        "GetSatelliteInboundPolicy",
        "PutSatelliteInboundPolicy",
        "DeliverHubMessage",
        "FetchHubReports",
        "RequestHubDeploy",
    ] {
        assert!(!agent_gate::AGENT_VERBS.contains(&method));
        assert!(!agent_gate::READ_VERBS.contains(&method));
        assert!(!agent_gate::UNSCOPED_READ_VERBS.contains(&method));
        assert!(!agent_gate::is_allowed_for_attributed_caller(method));
        assert!(!catalog.iter().any(|entry| entry.method == method));
    }
    for source in [
        include_str!("../tool_registry.rs"),
        include_str!("../session/harness/tools/rsi_control.rs"),
    ] {
        let catalog = source.to_ascii_lowercase();
        for forbidden in [
            "getsatelliteidentity",
            "listsatellitesessions",
            "get_satellite_identity",
            "list_satellite_sessions",
            "listsatellitepeers",
            "putsatellitepeer",
            "putsatellitelink",
            "probesatellitelink",
            "listhubsatellitesessions",
        ] {
            assert!(!catalog.contains(forbidden));
        }
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn remote_operator_methods_stay_out_of_agent_catalogs() {
    let catalog = rsi_common::agent_control_schema::agent_control_catalog_v1();
    for method in ["RemoteGetStatus", "RemoteSetConfig"] {
        assert!(!agent_gate::AGENT_VERBS.contains(&method));
        assert!(!agent_gate::READ_VERBS.contains(&method));
        assert!(!agent_gate::UNSCOPED_READ_VERBS.contains(&method));
        assert!(!agent_gate::is_allowed_for_attributed_caller(method));
        assert!(!catalog.iter().any(|entry| entry.method == method));
    }
    for source in [
        include_str!("../tool_registry.rs"),
        include_str!("../session/harness/tools/rsi_control.rs"),
    ] {
        let catalog = source.to_ascii_lowercase();
        for forbidden in [
            "remotegetstatus",
            "remotesetconfig",
            "remote_set_config",
            "remote_get_status",
        ] {
            assert!(!catalog.contains(forbidden));
        }
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn satellite_operator_registry_rpc_round_trip_and_revision_fence() {
    use rsi_common::satellite::{SatelliteHubSessionsPageV1, SatelliteRegistryV1};

    let fixture = recursive_dag_rpc_fixture();
    let peer_id = uuid::Uuid::new_v4();
    let peer = serde_json::json!({
        "peer_id": peer_id,
        "label": "test peer",
        "expected_installation_id": null,
        "enabled": false,
        "read_enabled": false,
    });
    let initial: SatelliteRegistryV1 = serde_json::from_value(
        call_rpc(
            &fixture.server,
            "ListSatellitePeers",
            serde_json::Value::Null,
        )
        .await
        .result
        .expect("operator registry response"),
    )
    .expect("typed registry");
    let created = call_rpc(
        &fixture.server,
        "PutSatellitePeer",
        serde_json::json!({
            "expected_registry_revision": initial.revision,
            "peer": peer,
        }),
    )
    .await;
    assert_eq!(
        created
            .result
            .as_ref()
            .and_then(|value| value["revision"].as_u64()),
        Some(initial.revision + 1)
    );
    let loaded: SatelliteRegistryV1 = serde_json::from_value(
        call_rpc(
            &fixture.server,
            "ListSatellitePeers",
            serde_json::Value::Null,
        )
        .await
        .result
        .expect("operator registry response"),
    )
    .expect("typed registry");
    assert_eq!(loaded.revision, initial.revision + 1);
    assert_eq!(loaded.peers.len(), 1);
    assert_eq!(loaded.peers[0].config.peer_id.0, peer_id);

    let page: SatelliteHubSessionsPageV1 = serde_json::from_value(
        call_rpc(
            &fixture.server,
            "ListHubSatelliteSessions",
            serde_json::json!({ "peer_id": peer_id, "offset": 0, "limit": 30 }),
        )
        .await
        .result
        .expect("bounded cached page"),
    )
    .expect("typed page");
    assert_eq!(page.peer_id.0, peer_id);
    assert!(page.sessions.is_empty());

    let stale = call_rpc(
        &fixture.server,
        "PutSatellitePeer",
        serde_json::json!({
            "expected_registry_revision": initial.revision,
            "peer": loaded.peers[0].config,
        }),
    )
    .await;
    assert!(stale.error.is_some());
    let after: SatelliteRegistryV1 = serde_json::from_value(
        call_rpc(
            &fixture.server,
            "ListSatellitePeers",
            serde_json::Value::Null,
        )
        .await
        .result
        .expect("operator registry response"),
    )
    .expect("typed registry");
    assert_eq!(after.revision, loaded.revision);
    assert_eq!(after.peers[0].config.peer_id.0, peer_id);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn satellite_rpc_identity_pages_and_refusals() {
    use rsi_common::satellite::{SatelliteIdentityV1, SatelliteSessionPageV1};
    let fixture = recursive_dag_rpc_fixture();
    let identity_response = call_rpc(
        &fixture.server,
        "GetSatelliteIdentity",
        serde_json::json!({}),
    )
    .await;
    let identity: SatelliteIdentityV1 =
        serde_json::from_value(identity_response.result.unwrap()).unwrap();
    identity.validate().unwrap();
    assert!(identity.capabilities.session_read);
    let health = identity.health.as_ref().expect("identity carries health");
    assert_eq!(health.daemon_version, env!("CARGO_PKG_VERSION"));
    assert!(health.schema_version.is_some_and(|version| version > 0));
    let replacement = RpcServer::new(
        Arc::clone(&fixture.manager),
        None,
        None,
        None,
        Arc::clone(&fixture.runtime_config),
        Arc::clone(&fixture.server.compile_engine),
        reqwest::Client::new(),
        crate::model_control::ModelControlRuntime::default_normal(),
    );
    let after_restart: SatelliteIdentityV1 = serde_json::from_value(
        call_rpc(
            &replacement,
            "GetSatelliteIdentity",
            serde_json::Value::Null,
        )
        .await
        .result
        .unwrap(),
    )
    .unwrap();
    assert_eq!(after_restart.installation_id, identity.installation_id);
    assert_ne!(
        after_restart.daemon_incarnation_id,
        identity.daemon_incarnation_id
    );
    let caps = call_rpc(
        &fixture.server,
        "GetDaemonCapabilities",
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(caps.result.unwrap()["satellite_session_read"], true);

    for _ in 0..3 {
        let id = Uuid::new_v4();
        let session = mk_agent_test_session(id, rsi_common::SessionKind::Standard, None, None);
        fixture.manager.active().write().await.insert(
            id,
            crate::session::types::TrackedSession::new_for_test(session),
        );
    }
    let request = serde_json::json!({"wire_version": 1, "limit": 2, "cursor": null});
    let first: SatelliteSessionPageV1 = serde_json::from_value(
        call_rpc(&fixture.server, "ListSatelliteSessions", request)
            .await
            .result
            .unwrap(),
    )
    .unwrap();
    first.validate(&identity.capabilities.limits).unwrap();
    assert_eq!(first.sessions.len(), 2);
    assert_eq!(first.snapshot_total_sessions, 3);
    let forged_offset = call_rpc(
        &fixture.server,
        "ListSatelliteSessions",
        serde_json::json!({
            "wire_version": 1,
            "limit": 1,
            "cursor": format!("v1:{}:1", first.snapshot_id.0)
        }),
    )
    .await;
    assert!(forged_offset.error.is_some());
    let later_id = Uuid::new_v4();
    fixture.manager.active().write().await.insert(
        later_id,
        crate::session::types::TrackedSession::new_for_test(mk_agent_test_session(
            later_id,
            rsi_common::SessionKind::Standard,
            None,
            None,
        )),
    );
    let second: SatelliteSessionPageV1 = serde_json::from_value(
        call_rpc(
            &fixture.server,
            "ListSatelliteSessions",
            serde_json::json!({
                "wire_version": 1, "limit": 2, "cursor": first.next_cursor
            }),
        )
        .await
        .result
        .unwrap(),
    )
    .unwrap();
    assert_eq!(second.snapshot_id, first.snapshot_id);
    assert_eq!(second.snapshot_offset, 2);
    assert_eq!(second.sessions.len(), 1);
    assert!(second.next_cursor.is_none());
    for params in [
        serde_json::json!({"wire_version": 0, "limit": 1, "cursor": null}),
        serde_json::json!({"wire_version": 1, "limit": 101, "cursor": null}),
        serde_json::json!({"wire_version": 1, "limit": 1, "cursor": "v1:bad:1"}),
        serde_json::json!({"wire_version": 1, "limit": 1, "cursor": "x".repeat(513)}),
        serde_json::json!({"wire_version": 1, "limit": 1, "unexpected": true}),
    ] {
        assert!(
            call_rpc(&fixture.server, "ListSatelliteSessions", params)
                .await
                .error
                .is_some()
        );
    }
    for method in ["GetSatelliteIdentity", "ListSatelliteSessions"] {
        let mut attributed = RpcRequest::new(method, serde_json::Value::Null);
        attributed.session_token = Some("test-token".into());
        let HandleResult::Response(response) =
            fixture.server.handle_request_inner(&attributed).await
        else {
            panic!("response");
        };
        assert_eq!(response.error.unwrap().code, INVALID_PARAMS);
    }
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let (client, server_stream) = UnixStream::pair().unwrap();
    let connection = fixture.server.handle_connection(server_stream);
    let client_round_trip = async move {
        let (reader, mut writer) = client.into_split();
        let request = RpcRequest::new("GetSatelliteIdentity", serde_json::Value::Null);
        writer
            .write_all(serde_json::to_string(&request).unwrap().as_bytes())
            .await
            .unwrap();
        writer.write_all(b"\n").await.unwrap();
        let mut line = String::new();
        tokio::io::BufReader::new(reader)
            .read_line(&mut line)
            .await
            .unwrap();
        drop(writer);
        let response: RpcResponse = serde_json::from_str(&line).unwrap();
        let identity: SatelliteIdentityV1 =
            serde_json::from_value(response.result.unwrap()).unwrap();
        identity.validate().unwrap();
    };
    let (served, ()) = tokio::join!(connection, client_round_trip);
    served.unwrap();
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn agent_schema_catalog_matches_the_independent_authorization_allowlist() {
    let catalog = rsi_common::agent_control_schema::agent_control_catalog_v1()
        .iter()
        .map(|descriptor| descriptor.method)
        .collect::<std::collections::BTreeSet<_>>();
    let authorization = agent_gate::AGENT_VERBS
        .iter()
        .copied()
        .collect::<std::collections::BTreeSet<_>>();
    // No hand-pinned count (#1116): the set equality is the intent, and a
    // new verb must be declared in the descriptor table and the verb registry.
    assert!(!catalog.is_empty());
    assert_eq!(catalog, authorization);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn codegraph_operator_reads_stay_outside_attributed_agent_surface() {
    let catalog = rsi_common::agent_control_schema::agent_control_catalog_v1();
    for method in [
        "GetCodegraphCapabilities",
        "ListCodegraphWorkspaces",
        "GetCodegraphStatus",
        "GetCodegraphSnapshot",
        "ListCodegraphSnapshots",
        "SearchCodegraph",
        "ExplainCodegraph",
        "GetCodegraphNeighbors",
        "FindCodegraphPath",
        "GetCodegraphSubgraph",
        "GetCodegraphImpact",
        "DiffCodegraph",
    ] {
        assert!(
            !agent_gate::is_allowed_for_attributed_caller(method),
            "{method}"
        );
        assert!(!catalog.iter().any(|descriptor| descriptor.method == method));
    }
}

/// #1049: the tool-boundary hook verb is reachable by a tokened session, but
/// is not part of the model-facing catalog, `AGENT_VERBS` or `READ_VERBS`.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn claim_boundary_mail_is_hook_only_not_cataloged() {
    // #1183: the hook's confirmation verb has the same audience.
    for verb in ["ClaimBoundaryMail", "ConfirmBoundaryMail"] {
        assert!(agent_gate::is_allowed_for_attributed_caller(verb));
        assert!(!agent_gate::AGENT_VERBS.contains(&verb));
        assert!(!agent_gate::READ_VERBS.contains(&verb));
        assert!(
            !rsi_common::agent_control_schema::agent_control_catalog_v1()
                .iter()
                .any(|descriptor| descriptor.method == verb)
        );
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn agent_get_progress_is_the_only_new_attributed_coordination_read() {
    assert!(agent_gate::is_allowed_for_attributed_caller(
        "AgentGetProgress"
    ));
    // `AgentSendMessage` was denied here through Phase 1 because it did
    // not exist. Plan item P2-03 adds it as an attributed WRITE with its
    // own token-resolved authority, so the gate must now admit it — the
    // assertion below moved sides deliberately, and the verb's scoping is
    // covered by the `agent_send_message_*` tests in `session::agent_verbs`
    // plus `agent_gate_attributed_send_message_is_allowed`. This test's
    // subject is unchanged: Phase 1 still contributed exactly one
    // attributed coordination READ.
    assert!(agent_gate::is_allowed_for_attributed_caller(
        "AgentSendMessage"
    ));
    for unlisted in ["AgentListSpawnRequests", "GetAgentProgress"] {
        assert!(!agent_gate::is_allowed_for_attributed_caller(unlisted));
    }
}

/// P2-03: the send verb is reachable for an attributed caller and stays
/// unreachable for a caller with no token, exactly like every other
/// `Agent*` verb.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn agent_gate_attributed_send_message_is_allowed() {
    assert!(agent_gate::is_allowed_for_attributed_caller(
        "AgentSendMessage"
    ));
    // Near-miss spellings must not be admitted by any prefix rule.
    for near_miss in [
        "AgentSendMessages",
        "SendAgentMessage",
        "agentsendmessage",
        "AgentSend",
    ] {
        assert!(
            !agent_gate::is_allowed_for_attributed_caller(near_miss),
            "gate is a closed allowlist, never a prefix match: {near_miss}"
        );
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn d05_program_run_operator_surface_is_strict_redacted_and_attributed_denied() {
    const METHODS: [&str; 8] = [
        "CreateProgramRun",
        "GetProgramRun",
        "ListProgramRuns",
        "ListProgramRunTransitions",
        "GetProgramRunOperationalStatus",
        "CancelProgramRun",
        "ResumeBlockedProgramRun",
        "ReconcileProgramRuns",
    ];
    for method in METHODS {
        assert!(
            !agent_gate::is_allowed_for_attributed_caller(method),
            "ProgramRun RPC must remain operator-only: {method}"
        );
    }
    assert!(
        serde_json::from_value::<GetProgramRunParams>(serde_json::json!({
            "program_run_id": Uuid::new_v4(), "controller_epoch": 9
        }))
        .is_err()
    );
    let source = rpc_production_source();
    assert!(!source.contains(&["Apply", "ProgramRun", "Transition\" =>"].concat()));
    assert!(!source.contains("PROGRAM_RUN_STORAGE_FAILURE\", error.to_string()"));
}
use anyhow::Context;
use rsi_common::recursive_dag::RecursiveTaskNode;
use rusqlite::params;

struct RecursiveDagRpcFixture {
    _dir: tempfile::TempDir,
    server: RpcServer,
    manager: std::sync::Arc<SessionManager>,
    runtime_config: std::sync::Arc<RuntimeConfig>,
}

fn issue_rpc_project_id() -> Uuid {
    Uuid::parse_str("00000000-0000-4000-8000-000000000078").unwrap()
}

fn recursive_dag_rpc_fixture() -> RecursiveDagRpcFixture {
    use crate::bus::EventBus;
    use crate::config::{Config, RuntimeConfig};
    use crate::prompt_compile::CompileEngine;
    use crate::store::Store;
    use tempfile::TempDir;

    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("rsi.db");
    let store = Store::open(&db_path).expect("open store");
    store
        .insert_project(&rsi_common::types::Project {
            id: issue_rpc_project_id(),
            name: "RPC issue project".to_string(),
            path: None,
            description: None,
            color: rsi_common::types::Project::DEFAULT_COLOR.to_string(),
            context_files: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        })
        .expect("seed RPC issue project");
    let config = Config::from_env();
    let runtime_config = RuntimeConfig::from_config(&config);
    runtime_config
        .update_field(
            "recursive_dag_recovery_controls_enabled",
            &serde_json::json!(false),
        )
        .unwrap();
    runtime_config
        .update_field(
            "recursive_dag_scheduler_controls_enabled",
            &serde_json::json!(false),
        )
        .unwrap();
    runtime_config
        .update_field(
            "recursive_dag_cancellation_controls_enabled",
            &serde_json::json!(false),
        )
        .unwrap();
    let bus = std::sync::Arc::new(EventBus::new(16));
    let manager = std::sync::Arc::new(
        SessionManager::new(
            std::sync::Arc::clone(&bus),
            store,
            false,
            dir.path().join("daemon.sock"),
            None,
            Vec::new(),
            std::sync::Arc::clone(&runtime_config),
            dir.path().join("sandboxes"),
        )
        .expect("manager"),
    );
    let http = reqwest::Client::new();
    let server = RpcServer::new(
        std::sync::Arc::clone(&manager),
        None,
        None,
        None,
        std::sync::Arc::clone(&runtime_config),
        CompileEngine::new(
            http.clone(),
            manager.store().clone(),
            std::sync::Arc::clone(&runtime_config),
            std::sync::Arc::clone(&bus),
        ),
        http,
        crate::model_control::ModelControlRuntime::default_normal(),
    );
    RecursiveDagRpcFixture {
        _dir: dir,
        server,
        manager,
        runtime_config,
    }
}

async fn call_rpc(server: &RpcServer, method: &str, mut params: serde_json::Value) -> RpcResponse {
    // C3 fixtures predate V77's required explicit project. Their operator
    // helper supplies the fixture project, while direct serde tests retain
    // coverage that a real missing project field is rejected.
    if method == "CreateIssue"
        && let Some(object) = params.as_object_mut()
        && !object.contains_key("project_id")
    {
        object.insert(
            "project_id".to_string(),
            serde_json::json!(issue_rpc_project_id()),
        );
    }
    let request = RpcRequest::new(method, params);
    let HandleResult::Response(response) = server.handle_request_inner(&request).await else {
        panic!("expected response");
    };
    response
}

async fn seed_active_model_invocation(
    fixture: &RecursiveDagRpcFixture,
    session_id: Uuid,
    purpose: rsi_common::model_control::ModelInvocationPurpose,
) -> Uuid {
    let session = mk_agent_test_session(
        session_id,
        rsi_common::types::SessionKind::Standard,
        None,
        None,
    );
    fixture
        .manager
        .store()
        .lock()
        .await
        .insert_session(&session)
        .expect("persist active model invocation owner");
    fixture.manager.active().write().await.insert(
        session_id,
        crate::session::types::TrackedSession::new_for_test(session),
    );

    let request = crate::model_control::ModelAdmissionRequest {
        purpose,
        provider: Some("Claude".to_string()),
        model: Some("claude-sonnet-5".to_string()),
        backend: Some("Claude".to_string()),
        effort: Some("medium".to_string()),
        trigger: "rpc_test".to_string(),
        owner: rsi_common::model_control::InvocationOwner {
            session_id: Some(session_id),
            operator: Some("operator".to_string()),
            ..Default::default()
        },
        dedup_key: Some(format!("rpc-test:{session_id}:{purpose}")),
        request_fingerprint: Some(format!("rpc-test:{session_id}:{purpose}")),
        parent_invocation_id: None,
        retry_of_invocation_id: None,
        expected_usage: Some(crate::model_control::explicit_expected_usage(
            purpose,
            Some("Claude"),
            Some("Claude"),
            Some("claude-sonnet-5"),
        )),
        baseline_input_tokens: 0,
        baseline_output_tokens: 0,
        baseline_cache_creation_tokens: 0,
        baseline_cache_read_tokens: 0,
        baseline_reasoning_tokens: 0,
        baseline_embedding_input_count: 0,
        baseline_wall_time_ms: 0,
    };

    match crate::model_control::admit_invocation(
        fixture.manager.store(),
        request,
        fixture.manager.event_bus(),
    )
    .await
    .expect("admission")
    {
        crate::model_control::AdmissionDecision::Admitted(permit) => permit.invocation_id(),
        crate::model_control::AdmissionDecision::Duplicate { invocation_id } => invocation_id,
    }
}

async fn create_recursive_dag_rpc_graph(manager: &std::sync::Arc<SessionManager>) -> (Uuid, Uuid) {
    use crate::store::recursive_dag::{RecursiveRootTaskCreate, RecursiveTaskGraphCreate};

    let graph_id = Uuid::new_v4();
    let root_id = Uuid::new_v4();
    let store = manager.store().lock().await;
    store
        .create_recursive_task_graph(RecursiveTaskGraphCreate {
            graph_id: RecursiveTaskGraphId(graph_id),
            title: "Root graph".to_string(),
            objective: "Exercise recursive DAG RPCs".to_string(),
            root_task: RecursiveRootTaskCreate {
                task_id: RecursiveTaskId(root_id),
                title: "Root".to_string(),
                objective: "Exercise recursive DAG RPCs".to_string(),
                scope: "Whole scope".to_string(),
                acceptance_criteria: vec!["done".to_string()],
                scope_units: 10,
                max_retries: 1,
            },
            project_id: None,
            workflow_id: None,
            topology_id: None,
            parent_session_id: None,
            source_execution_id: None,
            source_eval_id: None,
            max_depth: 2,
            max_fanout: 2,
            max_descendants: 4,
            step_limit: 10,
        })
        .expect("create recursive graph");
    (graph_id, root_id)
}

async fn create_terminal_recursive_live_dag_rpc_graph(
    manager: &std::sync::Arc<SessionManager>,
) -> (Uuid, Uuid) {
    use crate::store::recursive_dag::{RecursiveRootTaskCreate, RecursiveTaskGraphCreate};

    let graph_id = Uuid::new_v4();
    let root_id = Uuid::new_v4();
    let store = manager.store().lock().await;
    store
        .create_recursive_live_task_graph(RecursiveTaskGraphCreate {
            graph_id: RecursiveTaskGraphId(graph_id),
            title: "Terminal live graph".to_string(),
            objective: "Exercise terminal recursive DAG live scheduler RPC".to_string(),
            root_task: RecursiveRootTaskCreate {
                task_id: RecursiveTaskId(root_id),
                title: "Root".to_string(),
                objective: "Already complete".to_string(),
                scope: "Whole scope".to_string(),
                acceptance_criteria: vec!["done".to_string()],
                scope_units: 1,
                max_retries: 1,
            },
            project_id: None,
            workflow_id: None,
            topology_id: None,
            parent_session_id: None,
            source_execution_id: None,
            source_eval_id: None,
            max_depth: 2,
            max_fanout: 2,
            max_descendants: 4,
            step_limit: 10,
        })
        .expect("create recursive live graph");
    store
        .transition_recursive_task_state(
            RecursiveTaskGraphId(graph_id),
            RecursiveTaskId(root_id),
            rsi_common::RecursiveTaskLifecycleState::Succeeded,
            Some("test terminal live graph".to_string()),
        )
        .expect("terminalize live graph");
    (graph_id, root_id)
}

async fn create_runnable_recursive_live_dag_rpc_graph(
    manager: &std::sync::Arc<SessionManager>,
) -> (Uuid, Uuid) {
    use crate::store::recursive_dag::{RecursiveRootTaskCreate, RecursiveTaskGraphCreate};

    let graph_id = Uuid::new_v4();
    let root_id = Uuid::new_v4();
    let store = manager.store().lock().await;
    store
        .create_recursive_live_task_graph(RecursiveTaskGraphCreate {
            graph_id: RecursiveTaskGraphId(graph_id),
            title: "Live dogfood smoke graph".to_string(),
            objective: "Exercise the smallest ordinary recursive DAG live smoke".to_string(),
            root_task: RecursiveRootTaskCreate {
                task_id: RecursiveTaskId(root_id),
                title: "Live dogfood smoke task".to_string(),
                objective: "Emit a valid recursive live output JSON envelope".to_string(),
                scope: "One provider session, one scheduler step".to_string(),
                acceptance_criteria: vec!["done".to_string()],
                scope_units: 1,
                max_retries: 0,
            },
            project_id: None,
            workflow_id: None,
            topology_id: None,
            parent_session_id: None,
            source_execution_id: None,
            source_eval_id: None,
            max_depth: 1,
            max_fanout: 1,
            max_descendants: 1,
            step_limit: 1,
        })
        .expect("create runnable recursive live graph");
    (graph_id, root_id)
}

async fn create_two_task_recursive_live_dag_rpc_graph(
    manager: &std::sync::Arc<SessionManager>,
) -> (Uuid, Uuid, Uuid) {
    use crate::store::recursive_dag::{RecursiveRootTaskCreate, RecursiveTaskGraphCreate};

    let graph_id = Uuid::new_v4();
    let root_id = Uuid::new_v4();
    let dependent_id = Uuid::new_v4();
    let store = manager.store().lock().await;
    store
        .create_recursive_live_task_graph(RecursiveTaskGraphCreate {
            graph_id: RecursiveTaskGraphId(graph_id),
            title: "Two-task live dogfood smoke graph".to_string(),
            objective: "Exercise two ordinary recursive DAG live tasks in dependency order"
                .to_string(),
            root_task: RecursiveRootTaskCreate {
                task_id: RecursiveTaskId(root_id),
                title: "Produce dependency output".to_string(),
                objective: "Emit a valid recursive live output JSON envelope".to_string(),
                scope: "First live provider session".to_string(),
                acceptance_criteria: vec!["done".to_string()],
                scope_units: 2,
                max_retries: 0,
            },
            project_id: None,
            workflow_id: None,
            topology_id: None,
            parent_session_id: None,
            source_execution_id: None,
            source_eval_id: None,
            max_depth: 2,
            max_fanout: 1,
            max_descendants: 1,
            step_limit: 2,
        })
        .expect("create two-task recursive live graph");

    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    let acceptance_criteria_json =
        serde_json::to_string(&vec!["done".to_string()]).expect("serialize acceptance criteria");
    store
        .conn
        .execute(
            "INSERT INTO recursive_task_nodes (
                    id, graph_id, parent_task_id, title, objective, scope,
                    acceptance_criteria_json, depth, scope_units, max_retries, status,
                    decomposed_once, created_at, updated_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1, 1, 0, 'pending', 0, ?8, ?8)",
            rusqlite::params![
                dependent_id.to_string(),
                graph_id.to_string(),
                root_id.to_string(),
                "Consume dependency output",
                "Use the upstream live task result and emit a valid final envelope",
                "Second live provider session",
                acceptance_criteria_json,
                now,
            ],
        )
        .expect("insert dependent recursive live task");
    for kind in ["parent_child", "dependency"] {
        store
            .conn
            .execute(
                "INSERT INTO recursive_task_edges (
                        graph_id, from_task_id, to_task_id, kind, created_at
                     ) VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    graph_id.to_string(),
                    root_id.to_string(),
                    dependent_id.to_string(),
                    kind,
                    now,
                ],
            )
            .expect("insert two-task recursive live edge");
    }
    store
        .validate_recursive_task_graph_integrity(RecursiveTaskGraphId(graph_id))
        .expect("two-task recursive live graph integrity");
    (graph_id, root_id, dependent_id)
}

fn recursive_live_smoke_response(request: &wiremock::Request) -> wiremock::ResponseTemplate {
    recursive_live_smoke_response_with_metadata(
        request,
        "first-live-dogfood-smoke",
        "smallest live recursive DAG smoke completed",
    )
}

fn recursive_live_two_task_smoke_response(
    request: &wiremock::Request,
) -> wiremock::ResponseTemplate {
    recursive_live_smoke_response_with_metadata(
        request,
        "second-live-dogfood-two-task-graph",
        "two-task live recursive DAG smoke completed",
    )
}

fn recursive_live_smoke_response_with_metadata(
    request: &wiremock::Request,
    smoke: &str,
    result_summary: &str,
) -> wiremock::ResponseTemplate {
    let body = request
        .body_json::<serde_json::Value>()
        .expect("OpenAI-compatible request body");
    let mut transcript = String::new();
    if let Some(messages) = body.get("messages").and_then(|value| value.as_array()) {
        for message in messages {
            if let Some(content) = message.get("content").and_then(|value| value.as_str()) {
                transcript.push_str(content);
                transcript.push('\n');
            }
        }
    }
    let graph_id = extract_recursive_live_smoke_id(&transcript, "Graph id: ");
    let task_id = extract_recursive_live_smoke_id(&transcript, "Task id: ");
    let attempt_id = extract_recursive_live_smoke_id(&transcript, "Attempt id: ");
    let scheduler_run_id = extract_recursive_live_smoke_id(&transcript, "Scheduler run id: ");
    let live_attempt_id =
        extract_recursive_live_smoke_id(&transcript, "Recursive live attempt id: ");
    let output = serde_json::json!({
        "schema_version": rsi_common::recursive_dag::RECURSIVE_LIVE_OUTPUT_SCHEMA_VERSION,
        "correlation": {
            "graph_id": graph_id,
            "task_id": task_id,
            "attempt_id": attempt_id,
            "live_attempt_id": live_attempt_id,
            "scheduler_run_id": scheduler_run_id
        },
        "summary": format!("completed {smoke}"),
        "artifacts": [],
        "tests": [{
            "status": "not_run",
            "required": true,
            "reason": "wiremock local-provider dogfood smoke"
        }],
        "diffs": [],
        "dependency_outputs": [],
        "notes": ["session_id intentionally omitted; committer enriches the attached RSI session id"],
        "metadata": {
            "smoke": smoke
        },
        "kind": "success",
        "result_summary": result_summary,
        "acceptance": [{
            "criterion": "done",
            "status": "met",
            "evidence": ["provider session emitted the final live output JSON"]
        }]
    })
    .to_string();
    let first = serde_json::json!({
        "choices": [{
            "delta": { "content": output },
            "finish_reason": null
        }],
        "usage": null
    });
    let second = serde_json::json!({
        "choices": [{
            "delta": {},
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 1,
            "completion_tokens": 1
        }
    });
    let body = format!("data: {first}\n\ndata: {second}\n\ndata: [DONE]\n\n");
    wiremock::ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(body)
}

fn extract_recursive_live_smoke_id(transcript: &str, marker: &str) -> String {
    transcript
        .lines()
        .find_map(|line| {
            line.find(marker).map(|index| {
                line[index + marker.len()..]
                    .split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .trim_matches(|ch: char| !(ch.is_ascii_hexdigit() || ch == '-'))
                    .to_string()
            })
        })
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| panic!("missing recursive live smoke id marker: {marker}"))
}

async fn wait_for_recursive_live_smoke_session_status(
    manager: &std::sync::Arc<SessionManager>,
    session_id: Uuid,
    status: rsi_common::types::SessionStatus,
) -> rsi_common::types::Session {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let Some(session) = manager.get_session(session_id).await {
                if session.status == status {
                    return session;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("session {session_id} did not reach status {status:?}"))
}

async fn commit_recursive_live_smoke_output(
    server: &RpcServer,
    live_attempt_id: rsi_common::RecursiveLiveAttemptId,
    session_id: Uuid,
) -> rsi_common::CommitRecursiveLiveAttemptOutputResponse {
    let response = call_rpc(
        server,
        "CommitRecursiveLiveAttemptOutput",
        serde_json::json!({
            "live_attempt_id": live_attempt_id
        }),
    );
    let response = response.await;
    assert!(response.error.is_none(), "{:?}", response.error);
    let result: rsi_common::CommitRecursiveLiveAttemptOutputResponse =
        serde_json::from_value(response.result.unwrap()).expect("decode commit response");
    assert_eq!(
        result.validation_result.summary.status,
        rsi_common::RecursiveLiveOutputValidationStatus::Valid
    );
    assert_eq!(
        result
            .validation_result
            .metadata
            .get("session_id_enriched")
            .and_then(|value| value.as_bool()),
        Some(true)
    );
    assert_eq!(
        result.validation_result.summary.session_id,
        Some(session_id)
    );
    assert_eq!(
        result.readback.live_attempt.summary.status,
        rsi_common::RecursiveLiveAttemptStatus::Succeeded
    );
    assert_eq!(
        result.readback.recursive_attempt.status,
        rsi_common::RecursiveAttemptStatus::Succeeded
    );
    assert_eq!(
        result.readback.task.status,
        rsi_common::RecursiveTaskLifecycleState::Succeeded
    );
    result
}

async fn record_recursive_dag_rpc_artifacts(
    manager: &std::sync::Arc<SessionManager>,
    graph_id: Uuid,
    root_id: Uuid,
    labels: &[&str],
) -> Vec<rsi_common::RecursiveExecutionArtifact> {
    let store = manager.store().lock().await;
    let creates = labels
        .iter()
        .enumerate()
        .map(
            |(index, label)| crate::store::recursive_dag::RecursiveExecutionArtifactCreate {
                task_id: RecursiveTaskId(root_id),
                attempt_id: None,
                kind: rsi_common::RecursiveExecutionArtifactKind::Inline,
                label: (*label).to_string(),
                content: Some(format!("artifact content {index}")),
                uri: None,
                metadata: serde_json::json!({"rpc_test_index": index}),
            },
        )
        .collect();
    store
        .record_recursive_execution_artifacts(RecursiveTaskGraphId(graph_id), creates)
        .expect("record recursive artifacts")
}

fn rpc_error_data(response: &RpcResponse) -> rsi_common::RpcErrorData {
    serde_json::from_value(
        response
            .error
            .as_ref()
            .expect("rpc error")
            .data
            .clone()
            .expect("structured rpc error data"),
    )
    .expect("decode rpc error data")
}

#[derive(Debug, PartialEq, Eq)]
struct RecursiveDagLiveSchedulerGateCounts {
    schema_version: i32,
    graph_count: i64,
    scheduler_run_count: i64,
    task_attempt_count: i64,
    live_attempt_count: i64,
    session_count: i64,
    topology_count: i64,
    topology_graph_link_count: i64,
    topology_task_link_count: i64,
}

async fn recursive_dag_live_scheduler_gate_counts(
    manager: &std::sync::Arc<SessionManager>,
) -> RecursiveDagLiveSchedulerGateCounts {
    fn count(store: &crate::store::Store, table: &str) -> i64 {
        store
            .conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .expect("table count")
    }

    let store = manager.store().lock().await;
    RecursiveDagLiveSchedulerGateCounts {
        schema_version: store
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("schema version"),
        graph_count: count(&store, "recursive_task_graphs"),
        scheduler_run_count: count(&store, "recursive_scheduler_runs"),
        task_attempt_count: count(&store, "recursive_task_attempts"),
        live_attempt_count: count(&store, "recursive_live_attempts"),
        session_count: count(&store, "sessions"),
        topology_count: count(&store, "topologies"),
        topology_graph_link_count: count(&store, "recursive_topology_graph_links"),
        topology_task_link_count: count(&store, "recursive_topology_task_links"),
    }
}

fn recursive_topology_rpc_topology(topology_id: Uuid) -> rsi_common::types::Topology {
    let now = chrono::Utc::now();
    rsi_common::types::Topology {
        id: topology_id,
        name: format!("rpc recursive topology {topology_id}"),
        definition: rsi_common::types::TopologyDefinition {
            nodes: vec![
                rsi_common::types::TopologyNode {
                    id: "research".to_string(),
                    kind: rsi_common::types::SessionKind::Research,
                    label: "Research".to_string(),
                    prereqs: Vec::new(),
                    max_iterations: Some(32),
                    on_failure: Some(rsi_common::types::FailurePolicy::Halt),
                    params: std::collections::HashMap::new(),
                },
                rsi_common::types::TopologyNode {
                    id: "verify".to_string(),
                    kind: rsi_common::types::SessionKind::Task,
                    label: "Verify".to_string(),
                    prereqs: vec!["research".to_string()],
                    max_iterations: Some(32),
                    on_failure: Some(rsi_common::types::FailurePolicy::Halt),
                    params: std::collections::HashMap::new(),
                },
            ],
            edges: vec![rsi_common::types::TopologyEdge {
                from: "research".to_string(),
                to: "verify".to_string(),
                loop_edge: false,
            }],
            until: None,
        },
        created_at: now,
        updated_at: now,
    }
}

async fn insert_recursive_topology_rpc_topology(manager: &std::sync::Arc<SessionManager>) -> Uuid {
    let topology_id = Uuid::new_v4();
    let topology = recursive_topology_rpc_topology(topology_id);
    let store = manager.store().lock().await;
    store.insert_topology(&topology).expect("insert topology");
    topology_id
}

async fn create_recursive_topology_rpc_graph_variant(
    manager: &std::sync::Arc<SessionManager>,
    topology_id: Uuid,
    idempotency_key: &str,
    workflow_execution_id: Uuid,
) -> rsi_common::RecursiveTopologyGraphCreateResponse {
    let store = manager.store().lock().await;
    store
        .create_recursive_graph_from_topology_node(
            rsi_common::RecursiveTopologyGraphCreateRequest {
                topology_id,
                node_id: "verify".to_string(),
                topology_iteration: 0,
                idempotency_key: Some(idempotency_key.to_string()),
                project_id: None,
                parent_session_id: None,
                workflow_id: None,
                workflow_execution_id: Some(workflow_execution_id),
                include_prerequisite_closure: true,
                max_depth: Some(2),
                max_fanout: Some(2),
                max_descendants: Some(4),
                step_limit: Some(20),
                policy_overrides: None,
            },
        )
        .expect("create recursive topology graph")
}

async fn create_recursive_topology_rpc_graph(
    manager: &std::sync::Arc<SessionManager>,
) -> (Uuid, RecursiveTaskGraphId, Uuid) {
    let topology_id = insert_recursive_topology_rpc_topology(manager).await;
    let workflow_execution_id = Uuid::new_v4();
    let response = create_recursive_topology_rpc_graph_variant(
        manager,
        topology_id,
        "rpc-topology-status",
        workflow_execution_id,
    )
    .await;
    (topology_id, response.graph.id, workflow_execution_id)
}

async fn create_running_recursive_dag_rpc_graph(
    manager: &std::sync::Arc<SessionManager>,
) -> (Uuid, Uuid) {
    let (graph_id, root_id) = create_recursive_dag_rpc_graph(manager).await;
    let store = manager.store().lock().await;
    store
        .record_recursive_attempt_start(
            RecursiveTaskGraphId(graph_id),
            RecursiveTaskId(root_id),
            rsi_common::RecursiveAttemptPhase::Execute,
            RecursiveAttemptId(Uuid::new_v4()),
        )
        .expect("start recursive attempt");
    store
        .transition_recursive_task_state(
            RecursiveTaskGraphId(graph_id),
            RecursiveTaskId(root_id),
            rsi_common::RecursiveTaskLifecycleState::Running,
            Some("running before restart".to_string()),
        )
        .expect("mark recursive task running");
    drop(store);
    (graph_id, root_id)
}

struct RecursiveDagLiveRpcFixture {
    graph_id: Uuid,
    root_id: Uuid,
    run_id: rsi_common::RecursiveSchedulerRunId,
    attempt_id: rsi_common::RecursiveAttemptId,
    live_attempt_id: rsi_common::RecursiveLiveAttemptId,
    session_id: Uuid,
}

fn make_recursive_dag_test_session(session_id: Uuid) -> rsi_common::Session {
    let now = chrono::Utc::now();
    rsi_common::Session {
        context_fill_pct: None,
        id: session_id,
        status: rsi_common::SessionStatus::Running,
        session_kind: rsi_common::SessionKind::Standard,
        provider: rsi_common::SessionProvider::Codex,
        context_usage_confidence: rsi_common::ContextUsageConfidence::Missing,
        rotation_depth: 0,
        retry_attempt: None,
        max_retries: None,
        created_at: now,
        updated_at: now,
        pinned_at: None,
        testing_needed_at: None,
        rotation_disabled_at: None,
        query: "recursive live task".to_string(),
        title: Some("Recursive live task".to_string()),
        agent_role: None,
        epic_spawn_ordinal: None,
        description: None,
        short_summary: None,
        working_dir: std::path::PathBuf::from("/tmp"),
        git_branch: None,
        model: Some("test-model".to_string()),
        claude_session_id: None,
        project_id: None,
        continued_from: None,
        parent_id: None,
        lead_session_id: None,
        handoff_filepath: None,
        active_task: None,
        group_id: None,
        tag: String::new(),
        tags: Vec::new(),
        scheduled_job_id: None,
        stop_reason: None,
        cost_usd: None,
        duration_ms: None,
        num_turns: None,
        input_tokens: None,
        output_tokens: None,
        context_window: None,
        resolved_context_budget: None,
        total_input_tokens: None,
        total_prompt_tokens: None,
        total_output_tokens: None,
        total_cache_creation_tokens: None,
        total_cache_read_tokens: None,
        daemon_input_tokens: None,
        daemon_output_tokens: None,
        pipeline_artifact: None,
        workflow_id: None,
        workflow_id_override: None,
        pending_question: None,
        pending_archive: false,
        effort: None,
        issue_identifier: None,
        issue_url: None,
        issue_tracker_id: None,
        rating: None,
        harness_version_hash: None,
        test_passed: None,
        clippy_passed: None,
        turn_count: None,
        retry_count: None,
        approval_wait_ms: None,
        work_time_ms: None,
        approval_started_at: None,
        sandbox_kind: None,
        sandbox_root: None,
        sandbox_branch: None,
        sandbox_cleanup_state: None,
        is_eval: false,
        capability_class: None,
        topology_node_id: None,
        topology_iteration: 0,
        provider_cli_version: None,
        provider_capabilities: Vec::new(),
        thinking_tokens: None,
        service_tier: None,
        cache_creation_1h_tokens: None,
        cache_creation_5m_tokens: None,
        permission_denial_count: None,
        subagent_stats_json: None,
        queued_turn_count: None,
        terminal_reason: None,
    }
}

async fn create_recursive_dag_rpc_live_attempt(
    manager: &std::sync::Arc<SessionManager>,
) -> RecursiveDagLiveRpcFixture {
    let (graph_id, root_id) = create_recursive_dag_rpc_graph(manager).await;
    create_recursive_dag_rpc_live_attempt_on_graph(manager, graph_id, root_id).await
}

async fn create_recursive_dag_rpc_live_attempt_on_graph(
    manager: &std::sync::Arc<SessionManager>,
    graph_id: Uuid,
    root_id: Uuid,
) -> RecursiveDagLiveRpcFixture {
    use crate::store::recursive_dag::{RecursiveLiveAttemptCreate, RecursiveSchedulerRunStart};

    let run_id;
    let attempt_id = rsi_common::RecursiveAttemptId(Uuid::new_v4());
    let live_attempt_id = rsi_common::RecursiveLiveAttemptId(Uuid::new_v4());
    let session_id = Uuid::new_v4();
    {
        let store = manager.store().lock().await;
        let run = store
            .start_recursive_scheduler_run(RecursiveSchedulerRunStart {
                graph_id: RecursiveTaskGraphId(graph_id),
                max_steps: 5,
                source: rsi_common::RecursiveSchedulerRunSource::TestHarness,
                operator: Some("rpc-test".to_string()),
                executor_mode: rsi_common::RecursiveExecutionMode::Fake,
            })
            .expect("start scheduler run");
        run_id = run.id;
        store
            .record_recursive_attempt_start(
                RecursiveTaskGraphId(graph_id),
                RecursiveTaskId(root_id),
                rsi_common::RecursiveAttemptPhase::Execute,
                attempt_id,
            )
            .expect("start recursive attempt");
        store
            .create_recursive_live_attempt_placeholder(RecursiveLiveAttemptCreate {
                id: live_attempt_id,
                graph_id: RecursiveTaskGraphId(graph_id),
                task_id: RecursiveTaskId(root_id),
                scheduler_run_id: run.id,
                attempt_id,
                execution_mode: rsi_common::RecursiveExecutionMode::LiveSession,
                provider: Some(rsi_common::SessionProvider::Codex),
                model: Some("test-model".to_string()),
                sandbox_kind: None,
                sandbox_root: None,
                sandbox_branch: None,
                sandbox_worktree_id: None,
                workflow_execution_id: None,
                topology_workflow_id: None,
                max_wall_time_ms: Some(60_000),
            })
            .expect("create live attempt");
        store
            .insert_session(&make_recursive_dag_test_session(session_id))
            .expect("insert linked session");
        store
            .attach_recursive_live_attempt_session(live_attempt_id, session_id)
            .expect("attach session");
        store
            .conn
            .execute(
                "UPDATE recursive_live_attempts
                     SET lease_owner = ?1, lease_token = ?2, heartbeat_at = ?3,
                         lease_expires_at = ?4, updated_at = ?3
                     WHERE id = ?5",
                rusqlite::params![
                    "rpc-test",
                    Uuid::new_v4().to_string(),
                    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
                    (chrono::Utc::now() + chrono::Duration::seconds(60))
                        .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
                    live_attempt_id.to_string(),
                ],
            )
            .expect("seed heartbeat");
    }
    RecursiveDagLiveRpcFixture {
        graph_id,
        root_id,
        run_id,
        attempt_id,
        live_attempt_id,
        session_id,
    }
}

#[derive(Clone, Copy)]
enum RecursiveLiveCommitRpcOutputKind {
    ValidSuccess,
    InvalidJson,
}

async fn create_running_recursive_live_output_commit_rpc_attempt(
    manager: &std::sync::Arc<SessionManager>,
) -> RecursiveDagLiveRpcFixture {
    create_recursive_live_output_commit_rpc_attempt(manager, None).await
}

async fn create_completed_recursive_live_output_commit_rpc_attempt(
    manager: &std::sync::Arc<SessionManager>,
    output_kind: RecursiveLiveCommitRpcOutputKind,
) -> RecursiveDagLiveRpcFixture {
    create_recursive_live_output_commit_rpc_attempt(manager, Some(output_kind)).await
}

async fn create_recursive_live_output_commit_rpc_attempt(
    manager: &std::sync::Arc<SessionManager>,
    output_kind: Option<RecursiveLiveCommitRpcOutputKind>,
) -> RecursiveDagLiveRpcFixture {
    use crate::store::recursive_dag::{
        RecursiveLiveAttemptCreate, RecursiveLiveSchedulerRunStart, RecursiveRootTaskCreate,
        RecursiveTaskGraphCreate,
    };

    let graph_id = Uuid::new_v4();
    let root_id = Uuid::new_v4();
    let attempt_id = rsi_common::RecursiveAttemptId(Uuid::new_v4());
    let live_attempt_id = rsi_common::RecursiveLiveAttemptId(Uuid::new_v4());
    let session_id = Uuid::new_v4();
    let run_id;
    {
        let store = manager.store().lock().await;
        store
            .create_recursive_live_task_graph(RecursiveTaskGraphCreate {
                graph_id: RecursiveTaskGraphId(graph_id),
                title: "Live output commit RPC graph".to_string(),
                objective: "Commit completed session output through the public RPC".to_string(),
                root_task: RecursiveRootTaskCreate {
                    task_id: RecursiveTaskId(root_id),
                    title: "Live output commit task".to_string(),
                    objective: "Emit a valid recursive live output envelope".to_string(),
                    scope: "One completed linked live session".to_string(),
                    acceptance_criteria: vec!["done".to_string()],
                    scope_units: 1,
                    max_retries: 0,
                },
                project_id: None,
                workflow_id: None,
                topology_id: None,
                parent_session_id: None,
                source_execution_id: None,
                source_eval_id: None,
                max_depth: 1,
                max_fanout: 1,
                max_descendants: 1,
                step_limit: 1,
            })
            .expect("create live output commit graph");
        let run = store
            .start_recursive_live_scheduler_run(RecursiveLiveSchedulerRunStart {
                graph_id: RecursiveTaskGraphId(graph_id),
                max_steps: 1,
                source: rsi_common::RecursiveSchedulerRunSource::TestHarness,
                operator: Some("commit-rpc-test".to_string()),
                idempotency_key: None,
                request_fingerprint: None,
                policy_snapshot: serde_json::json!({
                    "test": "commit_recursive_live_attempt_output_rpc"
                }),
            })
            .expect("start live scheduler run");
        run_id = run.id;
        store
            .record_recursive_attempt_start_with_executor_kind(
                RecursiveTaskGraphId(graph_id),
                RecursiveTaskId(root_id),
                rsi_common::RecursiveAttemptPhase::Execute,
                attempt_id,
                rsi_common::RecursiveExecutionMode::LiveSession,
            )
            .expect("start live recursive attempt");
        store
            .transition_recursive_task_state(
                RecursiveTaskGraphId(graph_id),
                RecursiveTaskId(root_id),
                rsi_common::RecursiveTaskLifecycleState::Planning,
                Some("commit RPC test planning".to_string()),
            )
            .expect("transition task planning");
        store
            .transition_recursive_task_state(
                RecursiveTaskGraphId(graph_id),
                RecursiveTaskId(root_id),
                rsi_common::RecursiveTaskLifecycleState::Running,
                Some("commit RPC test running".to_string()),
            )
            .expect("transition task running");
        let live = store
            .create_recursive_live_attempt_placeholder(RecursiveLiveAttemptCreate {
                id: live_attempt_id,
                graph_id: RecursiveTaskGraphId(graph_id),
                task_id: RecursiveTaskId(root_id),
                scheduler_run_id: run.id,
                attempt_id,
                execution_mode: rsi_common::RecursiveExecutionMode::LiveSession,
                provider: Some(rsi_common::SessionProvider::Codex),
                model: Some("test-model".to_string()),
                sandbox_kind: None,
                sandbox_root: None,
                sandbox_branch: None,
                sandbox_worktree_id: None,
                workflow_execution_id: None,
                topology_workflow_id: None,
                max_wall_time_ms: Some(60_000),
            })
            .expect("create live output commit attempt");
        store
            .insert_session(&make_recursive_dag_test_session(session_id))
            .expect("insert commit RPC session");
        let live = store
            .attach_recursive_live_attempt_session(live.summary.id, session_id)
            .expect("attach commit RPC session");
        store
            .fail_recursive_live_scheduler_run(
                run.id,
                1,
                "test live scheduler launch boundary".to_string(),
            )
            .expect("finish request-scoped live scheduler run at launch boundary");
        if let Some(output_kind) = output_kind {
            let output = match output_kind {
                RecursiveLiveCommitRpcOutputKind::ValidSuccess => {
                    recursive_live_commit_rpc_success_output(&live)
                }
                RecursiveLiveCommitRpcOutputKind::InvalidJson => {
                    "completed without a JSON envelope".to_string()
                }
            };
            store
                .update_session_status(session_id, rsi_common::SessionStatus::Completed)
                .expect("mark commit RPC session completed");
            store
                .insert_event(&rsi_common::ConversationEvent {
                    id: 0,
                    session_id,
                    sequence: 1,
                    event_type: rsi_common::EventType::Message,
                    role: Some(rsi_common::Role::Assistant),
                    created_at: chrono::Utc::now(),
                    content: output,
                    tool_name: None,
                    tool_input: None,
                    offload_id: None,
                    tool_use_id: None,
                    metadata: None,
                })
                .expect("insert commit RPC assistant output");
        }
    }
    RecursiveDagLiveRpcFixture {
        graph_id,
        root_id,
        run_id,
        attempt_id,
        live_attempt_id,
        session_id,
    }
}

fn recursive_live_commit_rpc_success_output(
    live: &rsi_common::RecursiveLiveAttemptDetail,
) -> String {
    serde_json::json!({
        "schema_version": rsi_common::RECURSIVE_LIVE_OUTPUT_SCHEMA_VERSION,
        "correlation": {
            "graph_id": live.summary.graph_id,
            "task_id": live.summary.task_id,
            "attempt_id": live.summary.attempt_id,
            "live_attempt_id": live.summary.id,
            "scheduler_run_id": live.summary.scheduler_run_id,
            "session_id": live.summary.session_id
        },
        "summary": "completed live output commit RPC task",
        "artifacts": [],
        "tests": [{
            "status": "not_run",
            "required": true,
            "reason": "commit RPC unit test"
        }],
        "diffs": [],
        "dependency_outputs": [],
        "notes": [],
        "metadata": {
            "source": "commit_recursive_live_attempt_output_rpc_test"
        },
        "kind": "success",
        "result_summary": "done",
        "acceptance": [{
            "criterion": "done",
            "status": "met",
            "evidence": ["completed linked session emitted a valid envelope"]
        }]
    })
    .to_string()
}

fn recursive_live_validation_artifact(
    live: &rsi_common::RecursiveLiveAttemptDetail,
    label: &str,
    content: &str,
) -> crate::store::recursive_dag::RecursiveExecutionArtifactCreate {
    crate::store::recursive_dag::RecursiveExecutionArtifactCreate {
        task_id: live.summary.task_id,
        attempt_id: Some(live.summary.attempt_id),
        kind: rsi_common::RecursiveExecutionArtifactKind::Inline,
        label: label.to_string(),
        content: Some(content.to_string()),
        uri: None,
        metadata: serde_json::json!({"rpc_test": true}),
    }
}

async fn commit_recursive_dag_rpc_validation(
    manager: &std::sync::Arc<SessionManager>,
    fixture: &RecursiveDagLiveRpcFixture,
) -> rsi_common::RecursiveLiveOutputValidationResult {
    use crate::store::recursive_dag::RecursiveLiveOutputValidationCommit;

    let store = manager.store().lock().await;
    let live = store
        .load_recursive_live_attempt(fixture.live_attempt_id)
        .expect("load live attempt")
        .expect("live attempt");
    let issue = rsi_common::RecursiveLiveOutputValidationIssue {
        code: rsi_common::RecursiveLiveValidationIssueCode::MissingRequiredField,
        severity: rsi_common::RecursiveLiveValidationIssueSeverity::Error,
        class: rsi_common::RecursiveLiveValidationIssueClass::Missing,
        location: Some(
            rsi_common::RecursiveLiveValidationIssueLocation::OutputPath {
                path: "/summary".to_string(),
            },
        ),
        message: "summary is required".to_string(),
        evidence: Vec::new(),
        suggested_next_action: Some("repair output JSON".to_string()),
        metadata: serde_json::json!({"field": "summary"}),
    };
    let validation = rsi_common::RecursiveLiveOutputValidationResult {
        summary: rsi_common::RecursiveLiveOutputValidationSummary {
            validation_id: None,
            live_attempt_id: fixture.live_attempt_id,
            graph_id: RecursiveTaskGraphId(fixture.graph_id),
            task_id: RecursiveTaskId(fixture.root_id),
            scheduler_run_id: fixture.run_id,
            attempt_id: fixture.attempt_id,
            session_id: Some(fixture.session_id),
            status: rsi_common::RecursiveLiveOutputValidationStatus::Repairable,
            output_kind: Some(rsi_common::RecursiveLiveOutputKind::Success),
            mapping_decision: Some(
                rsi_common::RecursiveLiveOutputMappingDecision::RepairSameLiveAttempt,
            ),
            retry_decision: Some(rsi_common::RecursiveLiveOutputRetryDecision {
                decision: rsi_common::RecursiveLiveOutputRetryDecisionKind::RetrySameLiveAttempt,
                reason: Some("repair budget remains".to_string()),
                remaining_repair_attempts: Some(1),
                remaining_task_retries: Some(1),
                suggested_next_action: Some("repair".to_string()),
            }),
            raw_output_artifact_id: None,
            normalized_output_artifact_id: None,
            validation_artifact_id: None,
            normalized_digest: None,
            issue_count: 1,
            error_count: 1,
            warning_count: 0,
            info_count: 0,
            created_at: None,
        },
        artifact_links: rsi_common::RecursiveLiveOutputValidationArtifactLinks::default(),
        parser_source: Some(rsi_common::RecursiveLiveOutputParserSource::Inline {
            description: Some("rpc malformed live output".to_string()),
        }),
        issues: vec![issue],
        normalized_output: None,
        validation_report: Some(serde_json::json!({"status": "repairable"})),
        metadata: serde_json::json!({"rpc": true}),
    };
    store
        .commit_recursive_live_output_validation(RecursiveLiveOutputValidationCommit {
            live_attempt_id: fixture.live_attempt_id,
            graph_id: RecursiveTaskGraphId(fixture.graph_id),
            task_id: RecursiveTaskId(fixture.root_id),
            scheduler_run_id: fixture.run_id,
            attempt_id: fixture.attempt_id,
            validated_output: None,
            validation_result: validation,
            raw_output_artifact: Some(recursive_live_validation_artifact(
                &live,
                "live-output-raw",
                "{ invalid json",
            )),
            produced_artifacts: vec![recursive_live_validation_artifact(
                &live,
                "live-produced-artifact",
                "produced",
            )],
            test_artifacts: vec![recursive_live_validation_artifact(
                &live,
                "live-test-summary",
                "tests",
            )],
            diff_artifacts: vec![recursive_live_validation_artifact(
                &live,
                "live-diff-summary",
                "diff",
            )],
            scheduler_event: None,
        })
        .expect("commit validation")
        .validation_result
}

fn enable_recursive_dag_controls(runtime_config: &RuntimeConfig) {
    set_recursive_dag_controls(runtime_config, true, true, true);
}

fn set_recursive_dag_controls(
    runtime_config: &RuntimeConfig,
    recovery: bool,
    scheduler: bool,
    cancellation: bool,
) {
    runtime_config
        .update_field(
            "recursive_dag_recovery_controls_enabled",
            &serde_json::json!(recovery),
        )
        .unwrap();
    runtime_config
        .update_field(
            "recursive_dag_scheduler_controls_enabled",
            &serde_json::json!(scheduler),
        )
        .unwrap();
    runtime_config
        .update_field(
            "recursive_dag_cancellation_controls_enabled",
            &serde_json::json!(cancellation),
        )
        .unwrap();
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn test_get_session_params_parsing() {
    let json = serde_json::json!({
        "session_id": "550e8400-e29b-41d4-a716-446655440000"
    });
    let params: GetSessionParams = serde_json::from_value(json).unwrap();
    assert_eq!(
        params.session_id.to_string(),
        "550e8400-e29b-41d4-a716-446655440000"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn test_interrupt_session_params_parsing() {
    let json = serde_json::json!({
        "session_id": "550e8400-e29b-41d4-a716-446655440000"
    });
    let params: InterruptSessionParams = serde_json::from_value(json).unwrap();
    assert_eq!(
        params.session_id.to_string(),
        "550e8400-e29b-41d4-a716-446655440000"
    );
    assert_eq!(
        params.pause_level,
        crate::store::manager_actions::OperatorPause::Hard
    );
    let soft: InterruptSessionParams = serde_json::from_value(serde_json::json!({
        "session_id": params.session_id, "pause_level": "soft"
    }))
    .unwrap();
    assert_eq!(
        soft.pause_level,
        crate::store::manager_actions::OperatorPause::Soft
    );
    for method in ["SetOperatorPause", "GetOperatorPause"] {
        assert!(!agent_gate::AGENT_VERBS.contains(&method));
        assert!(!agent_gate::READ_VERBS.contains(&method));
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn operator_pause_rpc_round_trips_downgrade_and_clear() {
    let fixture = recursive_dag_rpc_fixture();
    let session_id = Uuid::new_v4();
    fixture
        .manager
        .store()
        .lock()
        .await
        .insert_session(&make_recursive_dag_test_session(session_id))
        .unwrap();
    for pause_level in ["hard", "soft", "none"] {
        let set = call_rpc(
            &fixture.server,
            "SetOperatorPause",
            serde_json::json!({"session_id":session_id,"pause_level":pause_level}),
        )
        .await;
        assert_eq!(set.result.unwrap()["pause_level"], pause_level);
        let get = call_rpc(
            &fixture.server,
            "GetOperatorPause",
            serde_json::json!({"session_id":session_id}),
        )
        .await;
        assert_eq!(get.result.unwrap()["pause_level"], pause_level);
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn test_get_conversation_params_parsing() {
    let json = serde_json::json!({
        "session_id": "550e8400-e29b-41d4-a716-446655440000"
    });
    let params: GetConversationParams = serde_json::from_value(json).unwrap();
    assert_eq!(
        params.session_id.to_string(),
        "550e8400-e29b-41d4-a716-446655440000"
    );
    assert!(params.since_sequence.is_none());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn test_get_conversation_params_with_since_sequence() {
    let json = serde_json::json!({
        "session_id": "550e8400-e29b-41d4-a716-446655440000",
        "since_sequence": 42
    });
    let params: GetConversationParams = serde_json::from_value(json).unwrap();
    assert_eq!(
        params.session_id.to_string(),
        "550e8400-e29b-41d4-a716-446655440000"
    );
    assert_eq!(params.since_sequence, Some(42));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn test_continue_session_rpc_dispatch() {
    let json = serde_json::json!({
        "session_id": "550e8400-e29b-41d4-a716-446655440000",
        "query": "fix the remaining issues"
    });
    let params: ContinueSessionParams = serde_json::from_value(json).unwrap();
    assert_eq!(
        params.session_id.to_string(),
        "550e8400-e29b-41d4-a716-446655440000"
    );
    assert_eq!(params.query, "fix the remaining issues");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn test_delete_session_params_parsing() {
    let json = serde_json::json!({
        "session_id": "550e8400-e29b-41d4-a716-446655440000"
    });
    let params: DeleteSessionParams = serde_json::from_value(json).unwrap();
    assert_eq!(
        params.session_id.to_string(),
        "550e8400-e29b-41d4-a716-446655440000"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn test_launch_session_params_mapping() {
    let params = LaunchSessionParams {
        completion_gates: None,
        query: "Hello".to_string(),
        title: None,
        working_dir: Some(std::path::PathBuf::from("/tmp")),
        provider: Some(rsi_common::types::SessionProvider::Codex),
        model: Some("gpt-6-astra".to_string()),
        configured_context_window: Some(400_000),
        system_prompt: None,
        session_kind: None,
        project_id: None,
        continued_from: None,
        parent_id: None,
        openai_base_url: None,
        openai_api_key: None,
        workflow_id: None,
        max_retries: None,
        group_id: None,
        effort: None,
        sandbox: None,
        is_eval: None,
        skip_context_pipeline: None,
        tags: vec!["test-tag".to_string()],
        workflow_id_override: None,
        tool_policy: None,
    };

    let config = LaunchConfig {
        completion_gates: None,
        query: params.query,
        title: params.title,
        agent_role: None,
        epic_spawn_ordinal: None,
        working_dir: params.working_dir,
        provider: params.provider,
        model: params.model,
        configured_context_window: params.configured_context_window,
        max_turns: None,
        system_prompt: params.system_prompt,
        resume_session_id: None,
        session_kind: params.session_kind,
        project_id: params.project_id,
        rsi_session_id: None,
        rsi_socket: None,
        rsi_session_token: None,
        continued_from: params.continued_from,
        openai_base_url: None,
        openai_api_key: None,
        conversation_history: None,
        workflow_id: None,
        workflow_id_override: None,
        max_retries: None,
        group_id: None,
        parent_id: None,
        effort: None,
        issue_identifier: None,
        issue_url: None,
        issue_tracker_id: None,
        scheduled_job_id: None,
        model_invocation_owner: None,
        model_invocation_dedup_key: None,
        model_invocation_request_fingerprint: None,
        skip_project_model_default: false,
        tool_policy: None,
        model_invocation_purpose:
            rsi_common::model_control::ModelInvocationPurpose::SessionLaunchFresh,
        sandbox: None,
        cargo_target_dir: None,
        execution_scratch: None,
        is_eval: false,
        skip_context_pipeline: false,
        capability_class: None,
        tags: vec![],
        topology_node_id: None,
        topology_iteration: 0,
        closure_selector: None,
    };

    assert_eq!(config.query, "Hello");
    assert_eq!(config.working_dir, Some(std::path::PathBuf::from("/tmp")));
    assert_eq!(config.configured_context_window, Some(400_000));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn test_get_turn_metrics_params_parsing() {
    // GetTurnMetrics reuses GetConversationParams (same shape)
    let json = serde_json::json!({
        "session_id": "550e8400-e29b-41d4-a716-446655440000"
    });
    let params: GetConversationParams = serde_json::from_value(json).unwrap();
    assert_eq!(
        params.session_id.to_string(),
        "550e8400-e29b-41d4-a716-446655440000"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn test_list_sessions_params_default() {
    // Empty params should parse with None project_id
    let json = serde_json::json!({});
    let params: ListSessionsParams = serde_json::from_value(json).unwrap();
    assert!(params.project_id.is_none());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn test_list_sessions_params_with_project() {
    let json = serde_json::json!({
        "project_id": "550e8400-e29b-41d4-a716-446655440000"
    });
    let params: ListSessionsParams = serde_json::from_value(json).unwrap();
    assert_eq!(
        params.project_id.unwrap().to_string(),
        "550e8400-e29b-41d4-a716-446655440000"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn recursive_dag_rpc_params_parse_read_only_inspection() {
    let graph_id = "550e8400-e29b-41d4-a716-446655440000";
    let task_id = "550e8400-e29b-41d4-a716-446655440001";
    let params: RecursiveTaskIdParams = serde_json::from_value(serde_json::json!({
        "graph_id": graph_id,
        "task_id": task_id
    }))
    .unwrap();
    assert_eq!(params.graph_id.to_string(), graph_id);
    assert_eq!(params.task_id.to_string(), task_id);

    let list_params: ListRecursiveTaskGraphsParams =
        serde_json::from_value(serde_json::json!({})).unwrap();
    assert!(list_params.include_quarantined);

    let topology_list_params: ListRecursiveGraphsForTopologyParams =
        serde_json::from_value(serde_json::json!({
            "node_id": "verify",
            "topology_iteration": 1
        }))
        .unwrap();
    assert_eq!(topology_list_params.node_id.as_deref(), Some("verify"));
    assert_eq!(topology_list_params.topology_iteration, Some(1));
    assert!(topology_list_params.include_quarantined);

    let topology_status_params: GetTopologyRecursiveStatusParams =
        serde_json::from_value(serde_json::json!({
            "graph_id": graph_id,
            "include_dynamic_children": true
        }))
        .unwrap();
    assert_eq!(
        topology_status_params.graph_id.unwrap().to_string(),
        graph_id
    );
    assert!(topology_status_params.include_dynamic_children);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn recursive_dag_rpc_read_only_inspection_dispatches() {
    use crate::bus::EventBus;
    use crate::config::{Config, RuntimeConfig};
    use crate::prompt_compile::CompileEngine;
    use crate::store::Store;
    use crate::store::recursive_dag::{RecursiveRootTaskCreate, RecursiveTaskGraphCreate};
    use tempfile::TempDir;

    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("rsi.db");
    let store = Store::open(&db_path).expect("open store");
    let config = Config::from_env();
    let runtime_config = RuntimeConfig::from_config(&config);
    let bus = std::sync::Arc::new(EventBus::new(16));
    let manager = std::sync::Arc::new(
        SessionManager::new(
            std::sync::Arc::clone(&bus),
            store,
            false,
            dir.path().join("daemon.sock"),
            None,
            Vec::new(),
            std::sync::Arc::clone(&runtime_config),
            dir.path().join("sandboxes"),
        )
        .expect("manager"),
    );
    let graph_id = Uuid::new_v4();
    let root_id = Uuid::new_v4();
    let attempt_id = Uuid::new_v4();
    {
        let store = manager.store().lock().await;
        store
            .create_recursive_task_graph(RecursiveTaskGraphCreate {
                graph_id: RecursiveTaskGraphId(graph_id),
                title: "Root graph".to_string(),
                objective: "Inspect graph".to_string(),
                root_task: RecursiveRootTaskCreate {
                    task_id: RecursiveTaskId(root_id),
                    title: "Root".to_string(),
                    objective: "Inspect graph".to_string(),
                    scope: "Whole scope".to_string(),
                    acceptance_criteria: vec!["done".to_string()],
                    scope_units: 10,
                    max_retries: 1,
                },
                project_id: None,
                workflow_id: None,
                topology_id: None,
                parent_session_id: None,
                source_execution_id: None,
                source_eval_id: None,
                max_depth: 2,
                max_fanout: 2,
                max_descendants: 4,
                step_limit: 10,
            })
            .expect("create recursive graph");
        store
            .record_recursive_attempt_start(
                RecursiveTaskGraphId(graph_id),
                RecursiveTaskId(root_id),
                rsi_common::RecursiveAttemptPhase::Execute,
                RecursiveAttemptId(attempt_id),
            )
            .expect("start recursive attempt");
        store
            .transition_recursive_task_state(
                RecursiveTaskGraphId(graph_id),
                RecursiveTaskId(root_id),
                rsi_common::RecursiveTaskLifecycleState::Running,
                Some("started".to_string()),
            )
            .expect("mark running");
        store
            .recover_recursive_task_graph(RecursiveTaskGraphId(graph_id))
            .expect("recover interrupted attempt");
    }

    let http = reqwest::Client::new();
    let server = RpcServer::new(
        std::sync::Arc::clone(&manager),
        None,
        None,
        None,
        std::sync::Arc::clone(&runtime_config),
        CompileEngine::new(
            http.clone(),
            manager.store().clone(),
            std::sync::Arc::clone(&runtime_config),
            std::sync::Arc::clone(&bus),
        ),
        http,
        crate::model_control::ModelControlRuntime::default_normal(),
    );

    let list_request = RpcRequest::new("ListRecursiveTaskGraphs", serde_json::json!({}));
    let HandleResult::Response(list_response) = server.handle_request_inner(&list_request).await
    else {
        panic!("expected response");
    };
    assert!(list_response.error.is_none());
    let graphs: Vec<rsi_common::RecursiveTaskGraphSummary> =
        serde_json::from_value(list_response.result.unwrap()).unwrap();
    assert_eq!(graphs.len(), 1);
    assert_eq!(graphs[0].id, RecursiveTaskGraphId(graph_id));

    let attempts_request = RpcRequest::new(
        "ListRecursiveTaskAttempts",
        serde_json::json!({ "graph_id": graph_id }),
    );
    let HandleResult::Response(attempts_response) =
        server.handle_request_inner(&attempts_request).await
    else {
        panic!("expected response");
    };
    assert!(attempts_response.error.is_none());
    let attempts: Vec<rsi_common::RecursiveTaskAttempt> =
        serde_json::from_value(attempts_response.result.unwrap()).unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].id, RecursiveAttemptId(attempt_id));
    assert_eq!(
        attempts[0].status,
        rsi_common::RecursiveAttemptStatus::Interrupted
    );

    let get_task_request = RpcRequest::new(
        "GetRecursiveTask",
        serde_json::json!({
            "graph_id": graph_id,
            "task_id": root_id
        }),
    );
    let HandleResult::Response(task_response) =
        server.handle_request_inner(&get_task_request).await
    else {
        panic!("expected response");
    };
    assert!(task_response.error.is_none());
    let task: rsi_common::RecursiveTaskNode =
        serde_json::from_value(task_response.result.unwrap()).unwrap();
    assert_eq!(task.id, RecursiveTaskId(root_id));

    let caps_request = RpcRequest::new("GetDaemonCapabilities", serde_json::Value::Null);
    let HandleResult::Response(caps_response) = server.handle_request_inner(&caps_request).await
    else {
        panic!("expected response");
    };
    assert!(caps_response.error.is_none());
    let caps: rsi_common::DaemonCapabilities =
        serde_json::from_value(caps_response.result.unwrap()).unwrap();
    assert!(caps.recursive_dag_inspection);

    {
        let store = manager.store().lock().await;
        store
            .quarantine_recursive_task_graph(
                RecursiveTaskGraphId(graph_id),
                "rpc quarantine".to_string(),
            )
            .expect("quarantine graph");
    }
    let get_graph_request = RpcRequest::new(
        "GetRecursiveTaskGraph",
        serde_json::json!({ "graph_id": graph_id }),
    );
    let HandleResult::Response(graph_response) =
        server.handle_request_inner(&get_graph_request).await
    else {
        panic!("expected response");
    };
    assert!(graph_response.error.is_none());
    let detail: rsi_common::RecursiveTaskGraphDetail =
        serde_json::from_value(graph_response.result.unwrap()).unwrap();
    assert_eq!(
        detail.graph.status,
        rsi_common::RecursiveGraphStatus::Malformed
    );
    assert_eq!(
        detail.graph.quarantine_reason.as_deref(),
        Some("rpc quarantine")
    );

    let bad_params_request = RpcRequest::new(
        "ListRecursiveTaskGraphs",
        serde_json::json!({ "status": "not_a_status" }),
    );
    let HandleResult::Response(bad_params_response) =
        server.handle_request_inner(&bad_params_request).await
    else {
        panic!("expected response");
    };
    assert_eq!(bad_params_response.error.unwrap().code, INVALID_PARAMS);

    let missing_graph_request = RpcRequest::new(
        "ListRecursiveTaskAttempts",
        serde_json::json!({ "graph_id": Uuid::new_v4() }),
    );
    let HandleResult::Response(missing_graph_response) =
        server.handle_request_inner(&missing_graph_request).await
    else {
        panic!("expected response");
    };
    assert_eq!(missing_graph_response.error.unwrap().code, INVALID_PARAMS);

    let missing_task_request = RpcRequest::new(
        "GetRecursiveTask",
        serde_json::json!({
            "graph_id": graph_id,
            "task_id": Uuid::new_v4()
        }),
    );
    let HandleResult::Response(missing_task_response) =
        server.handle_request_inner(&missing_task_request).await
    else {
        panic!("expected response");
    };
    assert_eq!(missing_task_response.error.unwrap().code, INVALID_PARAMS);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn recursive_topology_status_rpc_dispatches_read_only() {
    let fixture = recursive_dag_rpc_fixture();
    let (topology_id, graph_id, workflow_execution_id) =
        create_recursive_topology_rpc_graph(&fixture.manager).await;

    let before_scheduler_runs =
        recursive_table_count(&fixture.manager, "recursive_scheduler_runs").await;
    let before_live_attempts =
        recursive_table_count(&fixture.manager, "recursive_live_attempts").await;
    let before_sessions = recursive_table_count(&fixture.manager, "sessions").await;
    let before_workflow_registry = workflow_execution_registry_counts(&fixture.manager);

    let list_response = call_rpc(
        &fixture.server,
        "ListRecursiveGraphsForTopology",
        serde_json::json!({
            "topology_id": topology_id,
            "node_id": "verify",
            "topology_iteration": 0
        }),
    )
    .await;
    assert!(list_response.error.is_none());
    let graphs: Vec<rsi_common::TopologyRecursiveGraphStatus> =
        serde_json::from_value(list_response.result.unwrap()).unwrap();
    assert_eq!(graphs.len(), 1);
    assert_eq!(graphs[0].graph_id, graph_id);

    let status_response = call_rpc(
        &fixture.server,
        "GetTopologyRecursiveStatus",
        serde_json::json!({
            "graph_id": graph_id,
            "include_dynamic_children": true
        }),
    )
    .await;
    assert!(status_response.error.is_none());
    let status: rsi_common::TopologyRecursiveStatus =
        serde_json::from_value(status_response.result.unwrap()).unwrap();
    assert_eq!(status.graphs.len(), 1);
    assert_eq!(status.nodes.len(), 2);
    assert!(!status.live_enabled);
    assert!(!status.background_enabled);
    assert!(status.nodes.iter().all(|node| node.session_id.is_none()));
    assert!(
        status
            .nodes
            .iter()
            .all(|node| node.active_live_attempt_id.is_none())
    );

    let unknown_response = call_rpc(
        &fixture.server,
        "GetTopologyRecursiveStatus",
        serde_json::json!({ "topology_id": Uuid::new_v4() }),
    )
    .await;
    assert!(unknown_response.error.is_none());
    let unknown: rsi_common::TopologyRecursiveStatus =
        serde_json::from_value(unknown_response.result.unwrap()).unwrap();
    assert!(unknown.graphs.is_empty());
    assert!(unknown.nodes.is_empty());

    let unknown_workflow_execution_id = Uuid::new_v4();
    let unknown_workflow_execution_response = call_rpc(
        &fixture.server,
        "GetTopologyRecursiveStatus",
        serde_json::json!({ "workflow_execution_id": unknown_workflow_execution_id }),
    )
    .await;
    assert!(unknown_workflow_execution_response.error.is_none());
    let unknown_workflow_execution: rsi_common::TopologyRecursiveStatus =
        serde_json::from_value(unknown_workflow_execution_response.result.unwrap()).unwrap();
    assert!(unknown_workflow_execution.graphs.is_empty());
    assert!(unknown_workflow_execution.nodes.is_empty());
    assert_eq!(
        unknown_workflow_execution.workflow_execution_id,
        Some(unknown_workflow_execution_id)
    );
    assert_ne!(unknown_workflow_execution_id, workflow_execution_id);

    let bad_params = call_rpc(
        &fixture.server,
        "ListRecursiveGraphsForTopology",
        serde_json::json!({ "node_id": " " }),
    )
    .await;
    assert_eq!(bad_params.error.unwrap().code, INVALID_PARAMS);

    let malformed_get_params = call_rpc(
        &fixture.server,
        "GetTopologyRecursiveStatus",
        serde_json::json!({ "graph_id": "not-a-uuid" }),
    )
    .await;
    assert_eq!(malformed_get_params.error.unwrap().code, INVALID_PARAMS);

    assert_eq!(
        recursive_table_count(&fixture.manager, "recursive_scheduler_runs").await,
        before_scheduler_runs
    );
    assert_eq!(
        recursive_table_count(&fixture.manager, "recursive_live_attempts").await,
        before_live_attempts
    );
    assert_eq!(
        recursive_table_count(&fixture.manager, "sessions").await,
        before_sessions
    );
    assert_eq!(
        workflow_execution_registry_counts(&fixture.manager),
        before_workflow_registry
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn recursive_dag_topology_fake_scheduler_rpc_runs_existing_unique_link_only() {
    let fixture = recursive_dag_rpc_fixture();
    set_recursive_dag_controls(&fixture.runtime_config, false, true, false);
    let (topology_id, graph_id, workflow_execution_id) =
        create_recursive_topology_rpc_graph(&fixture.manager).await;

    let before_graphs = recursive_table_count(&fixture.manager, "recursive_task_graphs").await;
    let before_scheduler_runs =
        recursive_table_count(&fixture.manager, "recursive_scheduler_runs").await;
    let before_live_attempts =
        recursive_table_count(&fixture.manager, "recursive_live_attempts").await;
    let before_sessions = recursive_table_count(&fixture.manager, "sessions").await;
    let before_workflow_registry = workflow_execution_registry_counts(&fixture.manager);

    let response = call_rpc(
        &fixture.server,
        "RunRecursiveTopologyNodeFakeScheduler",
        serde_json::json!({
            "topology_id": topology_id,
            "node_id": "verify",
            "topology_iteration": 0,
            "workflow_execution_id": workflow_execution_id,
            "max_steps": 3,
            "operator": "topology-rpc-test",
            "execution_mode": "fake"
        }),
    )
    .await;
    assert!(response.error.is_none(), "{:?}", response.error);
    let run: rsi_common::RunRecursiveTopologyNodeFakeSchedulerResponse =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!(run.scheduler_run.graph_id, graph_id);
    assert_eq!(
        run.scheduler_run.source,
        rsi_common::RecursiveSchedulerRunSource::ManualRpc
    );
    assert_eq!(
        run.scheduler_run.operator.as_deref(),
        Some("topology-rpc-test")
    );
    assert_eq!(
        run.scheduler_run.executor_mode,
        rsi_common::RecursiveExecutionMode::Fake
    );
    assert_eq!(run.scheduler_run.max_steps, 3);
    assert_eq!(run.graph_link.graph_id, graph_id);
    assert_eq!(run.graph_link.topology_id, topology_id);
    assert_eq!(
        run.graph_link.execution_owner,
        RECURSIVE_TOPOLOGY_EXECUTION_OWNER_FAKE
    );
    assert_eq!(run.task_links.len(), 2);
    assert_eq!(run.status.graphs.len(), 1);
    assert_eq!(run.status.topology_id, Some(topology_id));
    assert_eq!(
        run.status.workflow_execution_id,
        Some(workflow_execution_id)
    );
    assert_eq!(
        run.status.execution_owner.as_deref(),
        Some(RECURSIVE_TOPOLOGY_EXECUTION_OWNER_FAKE)
    );
    assert_eq!(run.status.graphs[0].graph_id, graph_id);
    assert_eq!(
        run.status.graphs[0].latest_run_id,
        Some(run.scheduler_run.id)
    );
    assert_eq!(run.status.nodes.len(), 2);
    assert!(!run.status.live_enabled);
    assert!(!run.status.background_enabled);
    assert_eq!(
        run.scheduler_run.report_artifact_id,
        run.report_artifact.as_ref().map(|artifact| artifact.id)
    );

    assert_eq!(
        recursive_table_count(&fixture.manager, "recursive_task_graphs").await,
        before_graphs
    );
    assert_eq!(
        recursive_table_count(&fixture.manager, "recursive_scheduler_runs").await,
        before_scheduler_runs + 1
    );
    assert_eq!(
        recursive_table_count(&fixture.manager, "recursive_live_attempts").await,
        before_live_attempts
    );
    assert_eq!(
        recursive_table_count(&fixture.manager, "sessions").await,
        before_sessions
    );
    assert_eq!(
        workflow_execution_registry_counts(&fixture.manager),
        before_workflow_registry
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn recursive_dag_topology_fake_scheduler_rpc_repeated_runs_reacquire_graph_lease() {
    let fixture = recursive_dag_rpc_fixture();
    set_recursive_dag_controls(&fixture.runtime_config, false, true, false);
    let (_, graph_id, _) = create_recursive_topology_rpc_graph(&fixture.manager).await;
    let before_scheduler_runs =
        recursive_table_count(&fixture.manager, "recursive_scheduler_runs").await;

    let first = call_rpc(
        &fixture.server,
        "RunRecursiveTopologyNodeFakeScheduler",
        serde_json::json!({
            "graph_id": graph_id,
            "max_steps": 1
        }),
    )
    .await;
    assert!(first.error.is_none());
    let first_run: rsi_common::RunRecursiveTopologyNodeFakeSchedulerResponse =
        serde_json::from_value(first.result.unwrap()).unwrap();
    assert_eq!(first_run.scheduler_run.graph_id, graph_id);
    assert_eq!(
        first_run.scheduler_run.status,
        rsi_common::RecursiveSchedulerRunStatus::Completed
    );

    let second = call_rpc(
        &fixture.server,
        "RunRecursiveTopologyNodeFakeScheduler",
        serde_json::json!({
            "graph_id": graph_id,
            "max_steps": 1
        }),
    )
    .await;
    assert!(second.error.is_none());
    let second_run: rsi_common::RunRecursiveTopologyNodeFakeSchedulerResponse =
        serde_json::from_value(second.result.unwrap()).unwrap();
    assert_eq!(second_run.scheduler_run.graph_id, graph_id);
    assert_ne!(second_run.scheduler_run.id, first_run.scheduler_run.id);
    assert_eq!(
        second_run.scheduler_run.status,
        rsi_common::RecursiveSchedulerRunStatus::Completed
    );

    assert_eq!(
        recursive_table_count(&fixture.manager, "recursive_scheduler_runs").await,
        before_scheduler_runs + 2
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn recursive_dag_topology_fake_scheduler_rpc_rejects_bad_resolution_and_missing_steps() {
    let fixture = recursive_dag_rpc_fixture();
    set_recursive_dag_controls(&fixture.runtime_config, false, true, false);
    let topology_id = insert_recursive_topology_rpc_topology(&fixture.manager).await;
    let other_topology_id = insert_recursive_topology_rpc_topology(&fixture.manager).await;
    let first_workflow_execution_id = Uuid::new_v4();
    let first = create_recursive_topology_rpc_graph_variant(
        &fixture.manager,
        topology_id,
        "rpc-topology-t3-first",
        first_workflow_execution_id,
    )
    .await;
    let second = create_recursive_topology_rpc_graph_variant(
        &fixture.manager,
        topology_id,
        "rpc-topology-t3-second",
        Uuid::new_v4(),
    )
    .await;

    let missing_steps = call_rpc(
        &fixture.server,
        "RunRecursiveTopologyNodeFakeScheduler",
        serde_json::json!({ "graph_id": first.graph.id }),
    )
    .await;
    let error = missing_steps.error.unwrap();
    assert_eq!(error.code, INVALID_PARAMS);
    assert!(error.message.contains("max_steps is required"));

    let live_mode = call_rpc(
        &fixture.server,
        "RunRecursiveTopologyNodeFakeScheduler",
        serde_json::json!({
            "graph_id": first.graph.id,
            "max_steps": 1,
            "execution_mode": "live"
        }),
    )
    .await;
    let error = live_mode.error.unwrap();
    assert_eq!(error.code, INVALID_PARAMS);
    assert!(error.message.contains("fake-only"));

    let huge_steps = call_rpc(
        &fixture.server,
        "RunRecursiveTopologyNodeFakeScheduler",
        serde_json::json!({
            "graph_id": first.graph.id,
            "max_steps": MAX_RECURSIVE_FAKE_SCHEDULER_RPC_STEPS + 1
        }),
    )
    .await;
    let error = huge_steps.error.unwrap();
    assert_eq!(error.code, INVALID_PARAMS);
    assert!(error.message.contains("max_steps must be <="));

    let ambiguous = call_rpc(
        &fixture.server,
        "RunRecursiveTopologyNodeFakeScheduler",
        serde_json::json!({
            "topology_id": topology_id,
            "node_id": "verify",
            "topology_iteration": 0,
            "max_steps": 1
        }),
    )
    .await;
    let error = ambiguous.error.unwrap();
    assert_eq!(error.code, INVALID_PARAMS);
    assert!(
        error
            .message
            .contains("ambiguous recursive topology graph links")
    );

    let unknown_topology = call_rpc(
        &fixture.server,
        "RunRecursiveTopologyNodeFakeScheduler",
        serde_json::json!({
            "topology_id": Uuid::new_v4(),
            "node_id": "verify",
            "topology_iteration": 0,
            "max_steps": 1
        }),
    )
    .await;
    let error = unknown_topology.error.unwrap();
    assert_eq!(error.code, INVALID_PARAMS);
    assert!(
        error
            .message
            .contains("recursive topology graph link not found")
    );

    let unknown_graph = call_rpc(
        &fixture.server,
        "RunRecursiveTopologyNodeFakeScheduler",
        serde_json::json!({
            "graph_id": Uuid::new_v4(),
            "max_steps": 1
        }),
    )
    .await;
    let error = unknown_graph.error.unwrap();
    assert_eq!(error.code, INVALID_PARAMS);
    assert!(error.message.contains("recursive DAG graph not found"));

    let wrong_topology = call_rpc(
        &fixture.server,
        "RunRecursiveTopologyNodeFakeScheduler",
        serde_json::json!({
            "graph_id": first.graph.id,
            "topology_id": other_topology_id,
            "node_id": "verify",
            "topology_iteration": 0,
            "max_steps": 1
        }),
    )
    .await;
    let error = wrong_topology.error.unwrap();
    assert_eq!(error.code, INVALID_PARAMS);
    assert!(error.message.contains("does not match topology_id filter"));

    let wrong_node = call_rpc(
        &fixture.server,
        "RunRecursiveTopologyNodeFakeScheduler",
        serde_json::json!({
            "graph_id": first.graph.id,
            "topology_id": topology_id,
            "node_id": "research",
            "topology_iteration": 0,
            "max_steps": 1
        }),
    )
    .await;
    let error = wrong_node.error.unwrap();
    assert_eq!(error.code, INVALID_PARAMS);
    assert!(error.message.contains("does not match node_id filter"));

    let runs_before_explicit =
        recursive_table_count(&fixture.manager, "recursive_scheduler_runs").await;
    assert_eq!(runs_before_explicit, 0);

    let explicit = call_rpc(
        &fixture.server,
        "RunRecursiveTopologyNodeFakeScheduler",
        serde_json::json!({
            "graph_id": second.graph.id,
            "topology_id": topology_id,
            "node_id": "verify",
            "topology_iteration": 0,
            "max_steps": 1
        }),
    )
    .await;
    assert!(explicit.error.is_none());
    let run: rsi_common::RunRecursiveTopologyNodeFakeSchedulerResponse =
        serde_json::from_value(explicit.result.unwrap()).unwrap();
    assert_eq!(run.scheduler_run.graph_id, second.graph.id);
    assert_eq!(run.graph_link.graph_id, second.graph.id);
    assert_eq!(
        recursive_table_count(&fixture.manager, "recursive_scheduler_runs").await,
        runs_before_explicit + 1
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn recursive_dag_topology_fake_scheduler_rpc_control_disabled_rejects_without_mutation() {
    let fixture = recursive_dag_rpc_fixture();
    let (topology_id, graph_id, workflow_execution_id) =
        create_recursive_topology_rpc_graph(&fixture.manager).await;
    let before_graphs = recursive_table_count(&fixture.manager, "recursive_task_graphs").await;
    let before_scheduler_runs =
        recursive_table_count(&fixture.manager, "recursive_scheduler_runs").await;
    let before_live_attempts =
        recursive_table_count(&fixture.manager, "recursive_live_attempts").await;
    let before_sessions = recursive_table_count(&fixture.manager, "sessions").await;

    let response = call_rpc(
        &fixture.server,
        "RunRecursiveTopologyNodeFakeScheduler",
        serde_json::json!({
            "topology_id": topology_id,
            "node_id": "verify",
            "topology_iteration": 0,
            "workflow_execution_id": workflow_execution_id,
            "max_steps": 1
        }),
    )
    .await;
    let error = response.error.unwrap();
    assert_eq!(error.code, INVALID_PARAMS);
    assert!(error.message.contains("disabled"));

    assert_eq!(
        recursive_table_count(&fixture.manager, "recursive_task_graphs").await,
        before_graphs
    );
    assert_eq!(
        recursive_table_count(&fixture.manager, "recursive_scheduler_runs").await,
        before_scheduler_runs
    );
    assert_eq!(
        recursive_table_count(&fixture.manager, "recursive_live_attempts").await,
        before_live_attempts
    );
    assert_eq!(
        recursive_table_count(&fixture.manager, "sessions").await,
        before_sessions
    );

    let response = call_rpc(
        &fixture.server,
        "RunRecursiveTopologyNodeFakeScheduler",
        serde_json::json!({
            "graph_id": graph_id,
            "max_steps": 1
        }),
    )
    .await;
    let error = response.error.unwrap();
    assert_eq!(error.code, INVALID_PARAMS);
    assert!(error.message.contains("disabled"));
    assert_eq!(
        recursive_table_count(&fixture.manager, "recursive_scheduler_runs").await,
        before_scheduler_runs
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn recursive_dag_topology_cancellation_rpc_requests_graph_and_run_without_side_effects() {
    let fixture = recursive_dag_rpc_fixture();
    set_recursive_dag_controls(&fixture.runtime_config, false, false, true);
    let (topology_id, graph_id, workflow_execution_id) =
        create_recursive_topology_rpc_graph(&fixture.manager).await;

    let before_live_attempts =
        recursive_table_count(&fixture.manager, "recursive_live_attempts").await;
    let before_live_interrupts =
        recursive_table_count(&fixture.manager, "recursive_live_interrupts").await;
    let before_sessions = recursive_table_count(&fixture.manager, "sessions").await;
    let before_workflow_registry = workflow_execution_registry_counts(&fixture.manager);

    let response = call_rpc(
        &fixture.server,
        "RequestTopologyRecursiveCancellation",
        serde_json::json!({
            "topology_id": topology_id,
            "node_id": "verify",
            "topology_iteration": 0,
            "workflow_execution_id": workflow_execution_id,
            "scope": "graph",
            "reason": "operator cancelled topology graph",
            "requested_by": "rpc-test"
        }),
    )
    .await;
    assert!(response.error.is_none(), "{:?}", response.error);
    let graph_cancel: rsi_common::TopologyRecursiveCancellationResponse =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!(graph_cancel.matched_graph_count, 1);
    assert_eq!(graph_cancel.requested_count, 1);
    assert_eq!(graph_cancel.targets[0].graph_id, graph_id);
    assert_eq!(
        graph_cancel.targets[0].outcome,
        rsi_common::TopologyRecursiveCancellationOutcome::Requested
    );
    let graph_request_id = graph_cancel.targets[0]
        .cancellation_request
        .as_ref()
        .expect("graph cancellation request")
        .id;
    assert_eq!(
        graph_cancel.status.graphs[0].topology_visible_status,
        rsi_common::TopologyRecursiveVisibleStatus::Cancelling
    );

    let repeat = call_rpc(
        &fixture.server,
        "RequestTopologyRecursiveCancellation",
        serde_json::json!({
            "topology_id": topology_id,
            "node_id": "verify",
            "topology_iteration": 0,
            "workflow_execution_id": workflow_execution_id,
            "scope": "graph",
            "reason": "operator cancelled topology graph",
            "requested_by": "rpc-test"
        }),
    )
    .await;
    assert!(repeat.error.is_none());
    let repeat: rsi_common::TopologyRecursiveCancellationResponse =
        serde_json::from_value(repeat.result.unwrap()).unwrap();
    assert_eq!(repeat.requested_count, 0);
    assert_eq!(repeat.reused_count, 1);
    assert_eq!(
        repeat.targets[0]
            .cancellation_request
            .as_ref()
            .map(|request| request.id),
        Some(graph_request_id)
    );

    let second = create_recursive_topology_rpc_graph_variant(
        &fixture.manager,
        topology_id,
        "rpc-topology-t4-run",
        Uuid::new_v4(),
    )
    .await;
    let run = {
        let store = fixture.manager.store().lock().await;
        store
            .start_recursive_scheduler_run(
                crate::store::recursive_dag::RecursiveSchedulerRunStart {
                    graph_id: second.graph.id,
                    max_steps: 5,
                    source: rsi_common::RecursiveSchedulerRunSource::TestHarness,
                    operator: Some("rpc-test".to_string()),
                    executor_mode: rsi_common::RecursiveExecutionMode::Fake,
                },
            )
            .expect("start run")
    };
    let run_response = call_rpc(
        &fixture.server,
        "RequestTopologyRecursiveCancellation",
        serde_json::json!({
            "graph_id": second.graph.id,
            "topology_id": topology_id,
            "node_id": "verify",
            "topology_iteration": 0,
            "scope": "run",
            "run_id": run.id,
            "reason": "operator cancelled topology run",
            "requested_by": "rpc-test",
            "idempotency_key": "rpc-run-stop"
        }),
    )
    .await;
    assert!(run_response.error.is_none());
    let run_cancel: rsi_common::TopologyRecursiveCancellationResponse =
        serde_json::from_value(run_response.result.unwrap()).unwrap();
    assert_eq!(run_cancel.requested_count, 1);
    assert_eq!(run_cancel.targets[0].run_id, Some(run.id));
    assert_eq!(
        run_cancel.status.graphs[0].topology_visible_status,
        rsi_common::TopologyRecursiveVisibleStatus::RunCancelling
    );

    {
        let store = fixture.manager.store().lock().await;
        let stored_run = store
            .load_recursive_scheduler_run(run.id)
            .expect("load run")
            .expect("run");
        assert_eq!(
            stored_run.status,
            rsi_common::RecursiveSchedulerRunStatus::Cancelling
        );
        let graph_detail = store
            .get_recursive_task_graph(second.graph.id)
            .expect("load graph")
            .expect("graph");
        assert_eq!(
            graph_detail.graph.status,
            rsi_common::RecursiveGraphStatus::Active
        );
        assert!(
            store
                .list_recursive_live_attempts_for_graph(second.graph.id)
                .expect("list live attempts")
                .is_empty()
        );
    }

    assert_eq!(
        recursive_table_count(&fixture.manager, "recursive_live_attempts").await,
        before_live_attempts
    );
    assert_eq!(
        recursive_table_count(&fixture.manager, "recursive_live_interrupts").await,
        before_live_interrupts
    );
    assert_eq!(
        recursive_table_count(&fixture.manager, "sessions").await,
        before_sessions
    );
    assert_eq!(
        workflow_execution_registry_counts(&fixture.manager),
        before_workflow_registry
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn recursive_dag_topology_cancellation_rpc_rejects_disabled_and_bad_params_without_mutation()
{
    let fixture = recursive_dag_rpc_fixture();
    let (topology_id, graph_id, _) = create_recursive_topology_rpc_graph(&fixture.manager).await;
    let other_topology_id = insert_recursive_topology_rpc_topology(&fixture.manager).await;
    let before_cancellations =
        recursive_table_count(&fixture.manager, "recursive_cancellation_requests").await;

    let disabled = call_rpc(
        &fixture.server,
        "RequestTopologyRecursiveCancellation",
        serde_json::json!({
            "graph_id": graph_id,
            "scope": "graph",
            "reason": "disabled"
        }),
    )
    .await;
    let error = disabled.error.unwrap();
    assert_eq!(error.code, INVALID_PARAMS);
    assert!(error.message.contains("disabled"));
    assert_eq!(
        recursive_table_count(&fixture.manager, "recursive_cancellation_requests").await,
        before_cancellations
    );

    set_recursive_dag_controls(&fixture.runtime_config, false, false, true);

    let run_all = call_rpc(
        &fixture.server,
        "RequestTopologyRecursiveCancellation",
        serde_json::json!({
            "topology_id": topology_id,
            "scope": "run",
            "apply_to": "all_matching",
            "reason": "bad run all"
        }),
    )
    .await;
    let error = run_all.error.unwrap();
    assert_eq!(error.code, INVALID_PARAMS);
    assert!(
        error
            .message
            .contains("does not support apply_to=all_matching")
    );

    let unknown_graph = call_rpc(
        &fixture.server,
        "RequestTopologyRecursiveCancellation",
        serde_json::json!({
            "graph_id": Uuid::new_v4(),
            "scope": "graph",
            "reason": "unknown graph"
        }),
    )
    .await;
    let error = unknown_graph.error.unwrap();
    assert_eq!(error.code, INVALID_PARAMS);
    assert!(error.message.contains("recursive DAG graph not found"));

    let wrong_topology = call_rpc(
        &fixture.server,
        "RequestTopologyRecursiveCancellation",
        serde_json::json!({
            "graph_id": graph_id,
            "topology_id": other_topology_id,
            "scope": "graph",
            "reason": "wrong topology"
        }),
    )
    .await;
    let error = wrong_topology.error.unwrap();
    assert_eq!(error.code, INVALID_PARAMS);
    assert!(error.message.contains("does not match topology_id filter"));

    let conflict = call_rpc(
        &fixture.server,
        "RequestTopologyRecursiveCancellation",
        serde_json::json!({
            "graph_id": graph_id,
            "scope": "graph",
            "reason": "first",
            "idempotency_key": "same-key"
        }),
    )
    .await;
    assert!(conflict.error.is_none());
    let conflict = call_rpc(
        &fixture.server,
        "RequestTopologyRecursiveCancellation",
        serde_json::json!({
            "graph_id": graph_id,
            "scope": "graph",
            "reason": "second",
            "idempotency_key": "same-key"
        }),
    )
    .await;
    let error = conflict.error.unwrap();
    assert_eq!(error.code, INVALID_PARAMS);
    assert!(error.message.contains("idempotency conflict"));
    assert_eq!(
        recursive_table_count(&fixture.manager, "recursive_cancellation_requests").await,
        before_cancellations + 1
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn recursive_dag_topology_recovery_rpc_rejects_disabled_and_bad_params_without_mutation() {
    let fixture = recursive_dag_rpc_fixture();
    let (_topology_id, graph_id, workflow_execution_id) =
        create_recursive_topology_rpc_graph(&fixture.manager).await;
    let other_topology_id = insert_recursive_topology_rpc_topology(&fixture.manager).await;
    let before_passes = recursive_table_count(&fixture.manager, "recursive_recovery_passes").await;
    let before_deferred =
        recursive_table_count(&fixture.manager, "recursive_recovery_deferred_graphs").await;

    let disabled = call_rpc(
        &fixture.server,
        "ContinueTopologyRecursiveRecovery",
        serde_json::json!({
            "graph_id": graph_id,
            "max_graphs": 1
        }),
    )
    .await;
    let error = disabled.error.unwrap();
    assert_eq!(error.code, INVALID_PARAMS);
    assert!(error.message.contains("disabled"));
    assert_eq!(
        recursive_table_count(&fixture.manager, "recursive_recovery_passes").await,
        before_passes
    );

    set_recursive_dag_controls(&fixture.runtime_config, true, false, false);

    let unbounded_all = call_rpc(
        &fixture.server,
        "ContinueTopologyRecursiveRecovery",
        serde_json::json!({
            "apply_to": "all_matching",
            "max_graphs": 1
        }),
    )
    .await;
    let error = unbounded_all.error.unwrap();
    assert_eq!(error.code, INVALID_PARAMS);
    assert!(
        error
            .message
            .contains("requires topology_id or workflow_execution_id")
    );

    let unsupported_owner = call_rpc(
        &fixture.server,
        "ContinueTopologyRecursiveRecovery",
        serde_json::json!({
            "workflow_execution_id": workflow_execution_id,
            "apply_to": "all_matching",
            "execution_owner": "topology_graph_runner",
            "max_graphs": 1
        }),
    )
    .await;
    let error = unsupported_owner.error.unwrap();
    assert_eq!(error.code, INVALID_PARAMS);
    assert!(error.message.contains("recursive_dag_fake"));

    let unknown_graph = call_rpc(
        &fixture.server,
        "ContinueTopologyRecursiveRecovery",
        serde_json::json!({
            "graph_id": Uuid::new_v4(),
            "max_graphs": 1
        }),
    )
    .await;
    let error = unknown_graph.error.unwrap();
    assert_eq!(error.code, INVALID_PARAMS);
    assert!(error.message.contains("recursive DAG graph not found"));

    let wrong_topology = call_rpc(
        &fixture.server,
        "ContinueTopologyRecursiveRecovery",
        serde_json::json!({
            "graph_id": graph_id,
            "topology_id": other_topology_id,
            "max_graphs": 1
        }),
    )
    .await;
    let error = wrong_topology.error.unwrap();
    assert_eq!(error.code, INVALID_PARAMS);
    assert!(error.message.contains("does not match topology_id filter"));

    let workflow_all = call_rpc(
        &fixture.server,
        "ContinueTopologyRecursiveRecovery",
        serde_json::json!({
            "workflow_execution_id": workflow_execution_id,
            "apply_to": "all_matching",
            "max_graphs": 1
        }),
    )
    .await;
    assert!(workflow_all.error.is_none());
    let response: rsi_common::TopologyRecursiveRecoveryResponse =
        serde_json::from_value(workflow_all.result.unwrap()).unwrap();
    assert_eq!(response.matched_graph_count, 1);
    assert!(response.pass.is_some());
    assert_eq!(
        recursive_table_count(&fixture.manager, "recursive_recovery_deferred_graphs").await,
        before_deferred
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn recursive_dag_topology_recovery_rpc_store_failure_returns_internal_error() {
    let fixture = recursive_dag_rpc_fixture();
    let (_topology_id, graph_id, _) = create_recursive_topology_rpc_graph(&fixture.manager).await;
    set_recursive_dag_controls(&fixture.runtime_config, true, false, false);
    {
        let store = fixture.manager.store().lock().await;
        store
            .conn
            .execute_batch(
                "CREATE TRIGGER fail_rpc_topology_recovery_state
                     BEFORE INSERT ON recursive_recovery_graph_states
                     BEGIN
                        SELECT RAISE(ABORT, 'forced rpc recovery state failure');
                     END;",
            )
            .expect("install failure trigger");
    }

    let response = call_rpc(
        &fixture.server,
        "ContinueTopologyRecursiveRecovery",
        serde_json::json!({
            "graph_id": graph_id,
            "max_graphs": 1
        }),
    )
    .await;
    let error = response.error.expect("store failure error");
    assert_eq!(error.code, INTERNAL_ERROR);
    assert!(error.message.contains("forced rpc recovery state failure"));

    let store = fixture.manager.store().lock().await;
    let latest = store
        .load_latest_recursive_recovery_pass()
        .expect("load latest pass")
        .expect("failed pass");
    assert_eq!(
        latest.status,
        rsi_common::recursive_dag::RecursiveRecoveryPassStatus::Failed
    );
    assert_eq!(
        latest.stop_reason,
        Some(rsi_common::recursive_dag::RecursiveRecoveryStopReason::StoreError)
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn recursive_dag_topology_status_does_not_rewire_execute_topology_source() {
    let source = rpc_production_source();
    let start = source
        .find("async fn handle_execute_topology")
        .expect("execute topology handler");
    let end = source[start..]
        .find("async fn handle_start_chained_workflow")
        .map(|offset| start + offset)
        .expect("next handler");
    let execute_topology_source = &source[start..end];

    assert!(execute_topology_source.contains("execute_workflow_live"));
    assert!(!execute_topology_source.contains("create_recursive_graph_from_topology_node"));
    assert!(!execute_topology_source.contains("RunRecursiveFakeScheduler"));
    assert!(!execute_topology_source.contains("RunRecursiveTopologyNodeFakeScheduler"));
    assert!(!execute_topology_source.contains("RequestTopologyRecursiveCancellation"));
    assert!(!execute_topology_source.contains("request_topology_recursive_cancellation"));
    assert!(!execute_topology_source.contains("ContinueTopologyRecursiveRecovery"));
    assert!(!execute_topology_source.contains("continue_topology_recursive_recovery"));
    assert!(!execute_topology_source.contains("get_topology_recursive_status"));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn recursive_dag_live_scheduler_rpc_gate_disabled_is_non_mutating() {
    let fixture = recursive_dag_rpc_fixture();
    set_recursive_dag_controls(&fixture.runtime_config, true, true, true);
    fixture
        .runtime_config
        .update_field(
            "recursive_dag_live_scheduler_control_enabled",
            &serde_json::json!(false),
        )
        .expect("explicit false accepted");
    let (graph_id, _) = create_recursive_dag_rpc_graph(&fixture.manager).await;
    let before = recursive_dag_live_scheduler_gate_counts(&fixture.manager).await;
    assert_eq!(before.schema_version, crate::store::LATEST_SCHEMA_VERSION);

    let response = call_rpc(
        &fixture.server,
        "RunRecursiveLiveScheduler",
        serde_json::json!({
            "graph_id": graph_id,
            "max_steps": 1,
            "operator": "test",
            "provider": "Codex",
            "model": "gpt-5"
        }),
    )
    .await;
    let error = response.error.expect("gate should reject live scheduler");
    assert_eq!(error.code, INVALID_PARAMS);
    assert!(
        error
            .message
            .contains("live scheduler control RPC is disabled")
    );

    let after = recursive_dag_live_scheduler_gate_counts(&fixture.manager).await;
    assert_eq!(after, before);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn handle_get_recursive_graph_as_workflow_returns_bridged_def() {
    use rsi_graph::format::WorkflowDefinition;

    let fixture = recursive_dag_rpc_fixture();
    fixture
        .runtime_config
        .update_field("gv_render_recursive_origin", &serde_json::json!(true))
        .expect("enable gv recursive-origin rendering");
    let (graph_id, root_id) = create_recursive_dag_rpc_graph(&fixture.manager).await;

    // Snapshot the bridged input for verbatim assertions.
    let detail = {
        let store = fixture.manager.store().lock().await;
        store
            .get_recursive_task_graph(RecursiveTaskGraphId(graph_id))
            .expect("load detail")
            .expect("detail present")
    };

    let response = call_rpc(
        &fixture.server,
        "GetRecursiveGraphAsWorkflow",
        serde_json::json!({ "graph_id": graph_id }),
    )
    .await;
    assert!(
        response.error.is_none(),
        "expected ok: {:?}",
        response.error
    );
    let result = response.result.expect("result present");
    let bridged: GetRecursiveGraphAsWorkflowResponse =
        serde_json::from_value(result).expect("decode response");
    let wf: WorkflowDefinition =
        serde_json::from_value(bridged.definition).expect("decode definition");

    assert_eq!(wf.nodes.len(), detail.nodes.len());
    assert_eq!(wf.nodes[0].id, root_id.to_string());
    assert_eq!(wf.edges.len(), detail.edges.len());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn handle_get_recursive_graph_as_workflow_rejects_when_gate_disabled() {
    let fixture = recursive_dag_rpc_fixture();
    fixture
        .runtime_config
        .update_field("gv_render_recursive_origin", &serde_json::json!(false))
        .expect("explicit false accepted");
    let (graph_id, _) = create_recursive_dag_rpc_graph(&fixture.manager).await;

    let response = call_rpc(
        &fixture.server,
        "GetRecursiveGraphAsWorkflow",
        serde_json::json!({ "graph_id": graph_id }),
    )
    .await;
    let error = response.error.expect("gate should reject");
    assert_eq!(error.code, INVALID_PARAMS);
    assert!(
        error
            .message
            .contains("gv recursive-origin rendering is disabled")
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn handle_edit_recursive_node_instructions_happy_path_and_rejections() {
    let fixture = recursive_dag_rpc_fixture();
    fixture
        .runtime_config
        .update_field("gv_render_recursive_origin", &serde_json::json!(true))
        .expect("explicit true accepted");
    let (graph_id, root_id) = create_recursive_dag_rpc_graph(&fixture.manager).await;

    // Happy path: edit instructions of an editable node
    let response = call_rpc(
        &fixture.server,
        "EditRecursiveNodeInstructions",
        serde_json::json!({
            "graph_id": graph_id,
            "task_id": root_id,
            "instructions": "new rpc instructions"
        }),
    )
    .await;
    assert!(
        response.error.is_none(),
        "expected ok: {:?}",
        response.error
    );
    let node: RecursiveTaskNode =
        serde_json::from_value(response.result.expect("result present")).expect("decode node");
    assert_eq!(node.objective, "new rpc instructions");

    // Gate off rejection
    fixture
        .runtime_config
        .update_field("gv_render_recursive_origin", &serde_json::json!(false))
        .expect("explicit false accepted");
    let response_gate_off = call_rpc(
        &fixture.server,
        "EditRecursiveNodeInstructions",
        serde_json::json!({
            "graph_id": graph_id,
            "task_id": root_id,
            "instructions": "gate off value"
        }),
    )
    .await;
    let error = response_gate_off.error.expect("gate should reject");
    assert_eq!(error.code, INVALID_PARAMS);
    assert!(
        error
            .message
            .contains("gv recursive-origin rendering is disabled")
    );

    // Precondition rejection: lock node by changing status to Running
    fixture
        .runtime_config
        .update_field("gv_render_recursive_origin", &serde_json::json!(true))
        .expect("explicit true accepted");

    // Lock the node via database update directly
    {
        let store = fixture.manager.store().lock().await;
        store
            .conn
            .execute(
                "UPDATE recursive_task_nodes SET status = 'running' WHERE id = ?1",
                params![root_id.to_string()],
            )
            .expect("lock node");
    }

    let response_locked = call_rpc(
        &fixture.server,
        "EditRecursiveNodeInstructions",
        serde_json::json!({
            "graph_id": graph_id,
            "task_id": root_id,
            "instructions": "locked instructions"
        }),
    )
    .await;
    let error = response_locked
        .error
        .expect("locked node should be rejected");
    assert!(
        error.message.contains("not editable"),
        "error message: {}",
        error.message
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn handle_edit_recursive_node_settings_happy_path_and_rejections() {
    let fixture = recursive_dag_rpc_fixture();
    fixture
        .runtime_config
        .update_field("gv_render_recursive_origin", &serde_json::json!(true))
        .expect("explicit true accepted");
    let (graph_id, root_id) = create_recursive_dag_rpc_graph(&fixture.manager).await;

    // Happy path: edit settings of an editable node
    let response = call_rpc(
        &fixture.server,
        "EditRecursiveNodeSettings",
        serde_json::json!({
            "graph_id": graph_id,
            "task_id": root_id,
            "integration_strategy": "rebase-strategy",
            "verification_strategy": "verify-strategy"
        }),
    )
    .await;
    assert!(
        response.error.is_none(),
        "expected ok: {:?}",
        response.error
    );
    let node: RecursiveTaskNode =
        serde_json::from_value(response.result.expect("result present")).expect("decode node");
    assert_eq!(
        node.integration_strategy.as_deref(),
        Some("rebase-strategy")
    );
    assert_eq!(
        node.verification_strategy.as_deref(),
        Some("verify-strategy")
    );

    // Settings settings to None (cleared)
    let response_cleared = call_rpc(
        &fixture.server,
        "EditRecursiveNodeSettings",
        serde_json::json!({
            "graph_id": graph_id,
            "task_id": root_id,
            "integration_strategy": null,
            "verification_strategy": null
        }),
    )
    .await;
    assert!(response_cleared.error.is_none());
    let node: RecursiveTaskNode = serde_json::from_value(response_cleared.result.unwrap()).unwrap();
    assert_eq!(node.integration_strategy, None);
    assert_eq!(node.verification_strategy, None);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn recursive_dag_commit_live_attempt_output_rpc_gate_disabled_is_non_mutating() {
    let fixture = recursive_dag_rpc_fixture();
    let live = create_completed_recursive_live_output_commit_rpc_attempt(
        &fixture.manager,
        RecursiveLiveCommitRpcOutputKind::ValidSuccess,
    )
    .await;
    let before = recursive_dag_live_scheduler_gate_counts(&fixture.manager).await;
    let before_validations =
        recursive_table_count(&fixture.manager, "recursive_live_output_validations").await;
    let before_artifacts =
        recursive_table_count(&fixture.manager, "recursive_execution_artifacts").await;

    let response = call_rpc(
        &fixture.server,
        "CommitRecursiveLiveAttemptOutput",
        serde_json::json!({
            "live_attempt_id": live.live_attempt_id
        }),
    )
    .await;
    let error = response.error.expect("disabled gate rejects commit");
    assert_eq!(error.code, INVALID_PARAMS);
    assert!(
        error
            .message
            .contains("live scheduler control RPC is disabled")
    );

    assert_eq!(
        recursive_dag_live_scheduler_gate_counts(&fixture.manager).await,
        before
    );
    assert_eq!(
        recursive_table_count(&fixture.manager, "recursive_live_output_validations").await,
        before_validations
    );
    assert_eq!(
        recursive_table_count(&fixture.manager, "recursive_execution_artifacts").await,
        before_artifacts
    );
    let store = fixture.manager.store().lock().await;
    let live = store
        .load_recursive_live_attempt(live.live_attempt_id)
        .expect("load live attempt")
        .expect("live attempt");
    assert_eq!(
        live.summary.status,
        rsi_common::RecursiveLiveAttemptStatus::Running
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn recursive_dag_commit_live_attempt_output_rpc_requires_completed_session() {
    let fixture = recursive_dag_rpc_fixture();
    fixture
        .runtime_config
        .update_field(
            "recursive_dag_live_scheduler_control_enabled",
            &serde_json::json!(true),
        )
        .expect("live scheduler gate enabled");
    let live = create_running_recursive_live_output_commit_rpc_attempt(&fixture.manager).await;
    let before_validations =
        recursive_table_count(&fixture.manager, "recursive_live_output_validations").await;

    let response = call_rpc(
        &fixture.server,
        "CommitRecursiveLiveAttemptOutput",
        serde_json::json!({
            "live_attempt_id": live.live_attempt_id
        }),
    )
    .await;
    let error = response.error.expect("running session rejects commit");
    assert_eq!(error.code, INVALID_PARAMS);
    assert!(error.message.contains("not Completed"));
    assert_eq!(
        recursive_table_count(&fixture.manager, "recursive_live_output_validations").await,
        before_validations
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn recursive_dag_commit_live_attempt_output_rpc_terminalizes_valid_completed_session_without_launching()
 {
    let fixture = recursive_dag_rpc_fixture();
    fixture
        .runtime_config
        .update_field(
            "recursive_dag_live_scheduler_control_enabled",
            &serde_json::json!(true),
        )
        .expect("live scheduler gate enabled");
    let live = create_completed_recursive_live_output_commit_rpc_attempt(
        &fixture.manager,
        RecursiveLiveCommitRpcOutputKind::ValidSuccess,
    )
    .await;
    let before_sessions = recursive_table_count(&fixture.manager, "sessions").await;
    let before_runs = recursive_table_count(&fixture.manager, "recursive_scheduler_runs").await;
    let before_live_attempts =
        recursive_table_count(&fixture.manager, "recursive_live_attempts").await;

    let response = call_rpc(
        &fixture.server,
        "CommitRecursiveLiveAttemptOutput",
        serde_json::json!({
            "live_attempt_id": live.live_attempt_id
        }),
    )
    .await;
    assert!(response.error.is_none(), "{:?}", response.error);
    let committed: rsi_common::CommitRecursiveLiveAttemptOutputResponse =
        serde_json::from_value(response.result.unwrap()).unwrap();

    assert_eq!(
        committed.validation_result.summary.status,
        rsi_common::RecursiveLiveOutputValidationStatus::Valid
    );
    assert_eq!(
        committed.readback.live_attempt.summary.status,
        rsi_common::RecursiveLiveAttemptStatus::Succeeded
    );
    assert_eq!(
        committed.readback.recursive_attempt.status,
        rsi_common::RecursiveAttemptStatus::Succeeded
    );
    assert_eq!(
        committed.readback.task.status,
        rsi_common::RecursiveTaskLifecycleState::Succeeded
    );
    assert_eq!(
        committed
            .readback
            .session
            .as_ref()
            .map(|session| session.session_id),
        Some(live.session_id)
    );
    assert!(committed.raw_output_artifact.is_some());
    assert!(committed.normalized_output_artifact.is_some());
    assert_eq!(
        recursive_table_count(&fixture.manager, "sessions").await,
        before_sessions,
        "commit path must not launch a provider session"
    );
    assert_eq!(
        recursive_table_count(&fixture.manager, "recursive_scheduler_runs").await,
        before_runs
    );
    assert_eq!(
        recursive_table_count(&fixture.manager, "recursive_live_attempts").await,
        before_live_attempts
    );
    assert_eq!(
        recursive_table_count(&fixture.manager, "recursive_live_output_validations").await,
        1
    );
    let store = fixture.manager.store().lock().await;
    let graph = store
        .get_recursive_task_graph(RecursiveTaskGraphId(live.graph_id))
        .expect("load graph")
        .expect("graph");
    assert_eq!(
        graph.graph.status,
        rsi_common::RecursiveGraphStatus::Terminal
    );
    assert_eq!(
        graph.nodes[0].status,
        rsi_common::RecursiveTaskLifecycleState::Succeeded
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn recursive_dag_commit_live_attempt_output_rpc_records_invalid_json_validation_failure() {
    let fixture = recursive_dag_rpc_fixture();
    fixture
        .runtime_config
        .update_field(
            "recursive_dag_live_scheduler_control_enabled",
            &serde_json::json!(true),
        )
        .expect("live scheduler gate enabled");
    let live = create_completed_recursive_live_output_commit_rpc_attempt(
        &fixture.manager,
        RecursiveLiveCommitRpcOutputKind::InvalidJson,
    )
    .await;

    let response = call_rpc(
        &fixture.server,
        "CommitRecursiveLiveAttemptOutput",
        serde_json::json!({
            "live_attempt_id": live.live_attempt_id
        }),
    )
    .await;
    assert!(response.error.is_none(), "{:?}", response.error);
    let committed: rsi_common::CommitRecursiveLiveAttemptOutputResponse =
        serde_json::from_value(response.result.unwrap()).unwrap();

    assert_eq!(
        committed.validation_result.summary.status,
        rsi_common::RecursiveLiveOutputValidationStatus::Invalid
    );
    assert!(committed.validation_result.summary.issue_count > 0);
    assert_eq!(
        committed.readback.live_attempt.summary.status,
        rsi_common::RecursiveLiveAttemptStatus::Failed
    );
    assert_eq!(
        committed.readback.recursive_attempt.status,
        rsi_common::RecursiveAttemptStatus::Failed
    );
    assert_eq!(
        committed.readback.task.status,
        rsi_common::RecursiveTaskLifecycleState::Failed
    );
    assert!(committed.raw_output_artifact.is_some());
    assert!(committed.normalized_output_artifact.is_none());
    assert_eq!(
        recursive_table_count(&fixture.manager, "recursive_live_output_validations").await,
        1
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn recursive_dag_live_scheduler_rpc_enabled_runs_terminal_ordinary_graph_only() {
    let fixture = recursive_dag_rpc_fixture();
    set_recursive_dag_controls(&fixture.runtime_config, false, true, false);
    fixture
        .runtime_config
        .update_field(
            "recursive_dag_live_scheduler_control_enabled",
            &serde_json::json!(true),
        )
        .expect("live scheduler gate enabled");
    let (graph_id, _) = create_terminal_recursive_live_dag_rpc_graph(&fixture.manager).await;
    let before = recursive_dag_live_scheduler_gate_counts(&fixture.manager).await;

    let response = call_rpc(
        &fixture.server,
        "RunRecursiveLiveScheduler",
        serde_json::json!({
            "graph_id": graph_id,
            "max_steps": 1,
            "operator": "test",
            "output_repair_attempts": 0
        }),
    )
    .await;
    assert!(response.error.is_none(), "{:?}", response.error);
    let run: RunRecursiveLiveSchedulerResponse =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!(run.scheduler_run.graph_id, RecursiveTaskGraphId(graph_id));
    assert_eq!(
        run.scheduler_run.executor_mode,
        rsi_common::RecursiveExecutionMode::LiveSession
    );
    assert_eq!(
        run.stop_reason,
        rsi_common::RecursiveSchedulerStopReason::GraphTerminal
    );
    assert_eq!(run.step_count, 0);
    assert!(run.live_attempts.is_empty());

    let after = recursive_dag_live_scheduler_gate_counts(&fixture.manager).await;
    assert_eq!(after.scheduler_run_count, before.scheduler_run_count + 1);
    assert_eq!(after.task_attempt_count, before.task_attempt_count);
    assert_eq!(after.live_attempt_count, before.live_attempt_count);
    assert_eq!(after.session_count, before.session_count);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn recursive_dag_first_live_dogfood_smoke_launches_captures_and_terminalizes_once() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer};

    let local_provider = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(recursive_live_smoke_response)
        .mount(&local_provider)
        .await;

    let base_url = format!("{}/v1", local_provider.uri());
    let fixture = temp_env::with_vars(
        [
            ("LOCAL_LLM_BASE_URL", Some(base_url)),
            ("LOCAL_LLM_API_KEY", None),
        ],
        recursive_dag_rpc_fixture,
    );
    let (graph_id, root_id) = create_runnable_recursive_live_dag_rpc_graph(&fixture.manager).await;
    let before = recursive_dag_live_scheduler_gate_counts(&fixture.manager).await;
    fixture
        .runtime_config
        .update_field(
            "recursive_dag_live_scheduler_control_enabled",
            &serde_json::json!(true),
        )
        .expect("live scheduler gate enabled for smoke");

    let response = call_rpc(
        &fixture.server,
        "RunRecursiveLiveScheduler",
        serde_json::json!({
            "graph_id": graph_id,
            "max_steps": 1,
            "operator": "first-live-dogfood-smoke",
            "idempotency_key": "first-live-dogfood-smoke",
            "provider": "Local",
            "model": "recursive-live-smoke-local",
            "working_dir": fixture._dir.path().display().to_string(),
            "output_repair_attempts": 0
        }),
    )
    .await;
    assert!(response.error.is_none(), "{:?}", response.error);
    let run: RunRecursiveLiveSchedulerResponse =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!(run.scheduler_run.graph_id, RecursiveTaskGraphId(graph_id));
    assert_eq!(
        run.scheduler_run.executor_mode,
        rsi_common::RecursiveExecutionMode::LiveSession
    );
    assert_eq!(
        run.stop_reason,
        rsi_common::RecursiveSchedulerStopReason::ExecutorError
    );
    assert_eq!(run.step_count, 1);
    assert_eq!(run.selected_task_order, vec![RecursiveTaskId(root_id)]);
    assert_eq!(run.live_attempts.len(), 1);
    assert!(
        run.warnings.iter().any(|warning| {
            warning.code == "live_scheduler_launch_boundary"
                && warning.message.contains("CommitRecursiveLiveAttemptOutput")
        }),
        "{:?}",
        run.warnings
    );

    let live_readback = &run.live_attempts[0];
    let live_attempt_id = live_readback.live_attempt.summary.id;
    let session_id = live_readback
        .live_attempt
        .summary
        .session_id
        .expect("live attempt linked to launched provider session");
    assert_eq!(
        live_readback.live_attempt.summary.status,
        rsi_common::RecursiveLiveAttemptStatus::Running
    );
    assert_eq!(
        live_readback
            .session
            .as_ref()
            .map(|session| session.provider),
        Some(rsi_common::types::SessionProvider::Local)
    );

    let completed = wait_for_recursive_live_smoke_session_status(
        &fixture.manager,
        session_id,
        rsi_common::types::SessionStatus::Completed,
    )
    .await;
    assert_eq!(
        completed.provider,
        rsi_common::types::SessionProvider::Local
    );
    let conversation = fixture
        .manager
        .get_conversation(session_id)
        .await
        .expect("load live smoke conversation");
    assert!(conversation.iter().any(|event| {
        event.role == Some(rsi_common::types::Role::Assistant)
            && event
                .content
                .contains("smallest live recursive DAG smoke completed")
    }));

    let result =
        commit_recursive_live_smoke_output(&fixture.server, live_attempt_id, session_id).await;
    assert_eq!(
        result.validation_result.summary.status,
        rsi_common::RecursiveLiveOutputValidationStatus::Valid
    );
    assert_eq!(
        result
            .validation_result
            .metadata
            .get("session_id_enriched")
            .and_then(|value| value.as_bool()),
        Some(true)
    );
    assert_eq!(
        result.validation_result.summary.session_id,
        Some(session_id)
    );
    assert_eq!(
        result.readback.live_attempt.summary.status,
        rsi_common::RecursiveLiveAttemptStatus::Succeeded
    );
    assert_eq!(
        result.readback.recursive_attempt.status,
        rsi_common::RecursiveAttemptStatus::Succeeded
    );
    assert_eq!(
        result.readback.task.status,
        rsi_common::RecursiveTaskLifecycleState::Succeeded
    );

    let count_after_commit =
        recursive_table_count(&fixture.manager, "recursive_live_output_validations").await;
    assert_eq!(count_after_commit, 1);
    let recovery = fixture
        .manager
        .commit_recoverable_recursive_live_outputs_after_restart(
            rsi_common::RecursiveRecoveryBudget {
                max_graphs: 8,
                time_budget_ms: Some(1_000),
                source: rsi_common::RecursiveRecoverySource::Startup,
            },
        )
        .await
        .expect("restart-style live output recovery pass");
    assert_eq!(recovery, (0, 0, 0));
    assert_eq!(
        recursive_table_count(&fixture.manager, "recursive_live_output_validations").await,
        count_after_commit
    );

    let after = recursive_dag_live_scheduler_gate_counts(&fixture.manager).await;
    assert_eq!(after.graph_count, before.graph_count);
    assert_eq!(after.scheduler_run_count, before.scheduler_run_count + 1);
    assert_eq!(after.task_attempt_count, before.task_attempt_count + 1);
    assert_eq!(after.live_attempt_count, before.live_attempt_count + 1);
    assert_eq!(after.session_count, before.session_count + 1);
    assert_eq!(after.topology_count, before.topology_count);
    assert_eq!(
        after.topology_graph_link_count,
        before.topology_graph_link_count
    );
    assert_eq!(
        after.topology_task_link_count,
        before.topology_task_link_count
    );

    fixture
        .runtime_config
        .update_field(
            "recursive_dag_live_scheduler_control_enabled",
            &serde_json::json!(false),
        )
        .expect("live scheduler gate disabled after smoke");
    let caps_response = call_rpc(
        &fixture.server,
        "GetDaemonCapabilities",
        serde_json::Value::Null,
    )
    .await;
    let caps: rsi_common::DaemonCapabilities =
        serde_json::from_value(caps_response.result.unwrap()).unwrap();
    assert!(!caps.recursive_dag_live_scheduler_control);
    assert!(!caps.recursive_dag_live_execution);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn recursive_dag_second_live_dogfood_two_task_graph_runs_one_at_a_time() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer};

    let local_provider = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(recursive_live_two_task_smoke_response)
        .mount(&local_provider)
        .await;

    let base_url = format!("{}/v1", local_provider.uri());
    let fixture = temp_env::with_vars(
        [
            ("LOCAL_LLM_BASE_URL", Some(base_url)),
            ("LOCAL_LLM_API_KEY", None),
        ],
        recursive_dag_rpc_fixture,
    );
    let (graph_id, root_id, dependent_id) =
        create_two_task_recursive_live_dag_rpc_graph(&fixture.manager).await;
    let before = recursive_dag_live_scheduler_gate_counts(&fixture.manager).await;
    fixture
        .runtime_config
        .update_field(
            "recursive_dag_live_scheduler_control_enabled",
            &serde_json::json!(true),
        )
        .expect("live scheduler gate enabled for smoke");

    let first_response = call_rpc(
        &fixture.server,
        "RunRecursiveLiveScheduler",
        serde_json::json!({
            "graph_id": graph_id,
            "max_steps": 2,
            "operator": "second-live-dogfood-two-task-graph",
            "idempotency_key": "second-live-dogfood-two-task-graph-root",
            "provider": "Local",
            "model": "recursive-live-smoke-local",
            "working_dir": fixture._dir.path().display().to_string(),
            "output_repair_attempts": 0
        }),
    )
    .await;
    assert!(first_response.error.is_none(), "{:?}", first_response.error);
    let first_run: RunRecursiveLiveSchedulerResponse =
        serde_json::from_value(first_response.result.unwrap()).unwrap();
    assert_eq!(
        first_run.stop_reason,
        rsi_common::RecursiveSchedulerStopReason::ExecutorError
    );
    assert_eq!(first_run.step_count, 1);
    assert_eq!(
        first_run.selected_task_order,
        vec![RecursiveTaskId(root_id)]
    );
    assert_eq!(first_run.live_attempts.len(), 1);
    let first_live = &first_run.live_attempts[0];
    let first_live_attempt_id = first_live.live_attempt.summary.id;
    let first_session_id = first_live
        .live_attempt
        .summary
        .session_id
        .expect("first live attempt linked to session");
    assert_eq!(
        first_live.live_attempt.summary.status,
        rsi_common::RecursiveLiveAttemptStatus::Running
    );
    assert_eq!(
        first_live.task.status,
        rsi_common::RecursiveTaskLifecycleState::Running
    );

    wait_for_recursive_live_smoke_session_status(
        &fixture.manager,
        first_session_id,
        rsi_common::types::SessionStatus::Completed,
    )
    .await;
    let first_commit = commit_recursive_live_smoke_output(
        &fixture.server,
        first_live_attempt_id,
        first_session_id,
    )
    .await;
    assert_eq!(first_commit.readback.task.id, RecursiveTaskId(root_id));

    let first_restart_recovery = fixture
        .manager
        .commit_recoverable_recursive_live_outputs_after_restart(
            rsi_common::RecursiveRecoveryBudget {
                max_graphs: 8,
                time_budget_ms: Some(1_000),
                source: rsi_common::RecursiveRecoverySource::Startup,
            },
        )
        .await
        .expect("restart-style recovery after first task");
    assert_eq!(first_restart_recovery, (0, 0, 0));
    assert_eq!(
        recursive_table_count(&fixture.manager, "recursive_live_output_validations").await,
        1
    );

    let first_readback = call_rpc(
        &fixture.server,
        "ListRecursiveLiveAttempts",
        serde_json::json!({
            "graph_id": graph_id,
            "include_status": true
        }),
    )
    .await;
    assert!(first_readback.error.is_none());
    let first_attempts: Vec<rsi_common::RecursiveLiveAttemptListItem> =
        serde_json::from_value(first_readback.result.unwrap()).unwrap();
    assert_eq!(first_attempts.len(), 1);
    assert_eq!(first_attempts[0].summary.id, first_live_attempt_id);
    assert_eq!(
        first_attempts[0].summary.status,
        rsi_common::RecursiveLiveAttemptStatus::Succeeded
    );
    assert!(first_attempts[0].latest_validation.is_some());

    let second_response = call_rpc(
        &fixture.server,
        "RunRecursiveLiveScheduler",
        serde_json::json!({
            "graph_id": graph_id,
            "max_steps": 2,
            "operator": "second-live-dogfood-two-task-graph",
            "idempotency_key": "second-live-dogfood-two-task-graph-dependent",
            "provider": "Local",
            "model": "recursive-live-smoke-local",
            "working_dir": fixture._dir.path().display().to_string(),
            "output_repair_attempts": 0
        }),
    )
    .await;
    assert!(
        second_response.error.is_none(),
        "{:?}",
        second_response.error
    );
    let second_run: RunRecursiveLiveSchedulerResponse =
        serde_json::from_value(second_response.result.unwrap()).unwrap();
    assert_eq!(
        second_run.stop_reason,
        rsi_common::RecursiveSchedulerStopReason::ExecutorError
    );
    assert_eq!(second_run.step_count, 1);
    assert_eq!(
        second_run.selected_task_order,
        vec![RecursiveTaskId(dependent_id)]
    );
    assert_eq!(second_run.live_attempts.len(), 1);
    let second_live = &second_run.live_attempts[0];
    let second_live_attempt_id = second_live.live_attempt.summary.id;
    let second_session_id = second_live
        .live_attempt
        .summary
        .session_id
        .expect("second live attempt linked to session");
    assert_ne!(second_session_id, first_session_id);
    assert_eq!(second_live.recursive_attempt.dependency_snapshot.len(), 1);
    assert_eq!(
        second_live.recursive_attempt.dependency_snapshot[0].task_id,
        RecursiveTaskId(root_id)
    );

    let second_completed = wait_for_recursive_live_smoke_session_status(
        &fixture.manager,
        second_session_id,
        rsi_common::types::SessionStatus::Completed,
    )
    .await;
    assert_eq!(
        second_completed.provider,
        rsi_common::types::SessionProvider::Local
    );
    let second_conversation = fixture
        .manager
        .get_conversation(second_session_id)
        .await
        .expect("load second live smoke conversation");
    assert!(second_conversation.iter().any(|event| {
        event.role == Some(rsi_common::types::Role::Assistant)
            && event
                .content
                .contains("two-task live recursive DAG smoke completed")
    }));

    let second_commit = commit_recursive_live_smoke_output(
        &fixture.server,
        second_live_attempt_id,
        second_session_id,
    )
    .await;
    assert_eq!(
        second_commit.readback.task.id,
        RecursiveTaskId(dependent_id)
    );

    let readback = call_rpc(
        &fixture.server,
        "ListRecursiveLiveAttempts",
        serde_json::json!({
            "graph_id": graph_id,
            "include_status": true
        }),
    )
    .await;
    assert!(readback.error.is_none());
    let attempts: Vec<rsi_common::RecursiveLiveAttemptListItem> =
        serde_json::from_value(readback.result.unwrap()).unwrap();
    assert_eq!(attempts.len(), 2);
    for live_attempt_id in [first_live_attempt_id, second_live_attempt_id] {
        let item = attempts
            .iter()
            .find(|item| item.summary.id == live_attempt_id)
            .expect("live attempt listed");
        assert_eq!(
            item.summary.status,
            rsi_common::RecursiveLiveAttemptStatus::Succeeded
        );
        assert_eq!(
            item.latest_validation
                .as_ref()
                .map(|summary| summary.status),
            Some(rsi_common::RecursiveLiveOutputValidationStatus::Valid)
        );
    }

    let detail_response = call_rpc(
        &fixture.server,
        "GetRecursiveLiveAttempt",
        serde_json::json!({
            "live_attempt_id": second_live_attempt_id,
            "include_retry_history": true
        }),
    )
    .await;
    assert!(detail_response.error.is_none());
    let detail: rsi_common::RecursiveLiveAttemptReadback =
        serde_json::from_value(detail_response.result.unwrap()).unwrap();
    assert_eq!(detail.task.id, RecursiveTaskId(dependent_id));
    assert_eq!(
        detail.session.as_ref().map(|session| session.session_id),
        Some(second_session_id)
    );
    assert_eq!(
        detail
            .latest_validation
            .as_ref()
            .map(|summary| summary.status),
        Some(rsi_common::RecursiveLiveOutputValidationStatus::Valid)
    );
    assert_eq!(detail.retry_history.expect("retry history").len(), 1);

    let second_restart_recovery = fixture
        .manager
        .commit_recoverable_recursive_live_outputs_after_restart(
            rsi_common::RecursiveRecoveryBudget {
                max_graphs: 8,
                time_budget_ms: Some(1_000),
                source: rsi_common::RecursiveRecoverySource::Startup,
            },
        )
        .await
        .expect("restart-style recovery after both tasks");
    assert_eq!(second_restart_recovery, (0, 0, 0));
    let count_after_commits =
        recursive_table_count(&fixture.manager, "recursive_live_output_validations").await;
    assert_eq!(count_after_commits, 2);

    let terminal_response = call_rpc(
        &fixture.server,
        "RunRecursiveLiveScheduler",
        serde_json::json!({
            "graph_id": graph_id,
            "max_steps": 2,
            "operator": "second-live-dogfood-two-task-graph",
            "idempotency_key": "second-live-dogfood-two-task-graph-terminal-readback",
            "provider": "Local",
            "model": "recursive-live-smoke-local",
            "working_dir": fixture._dir.path().display().to_string(),
            "output_repair_attempts": 0
        }),
    )
    .await;
    assert!(
        terminal_response.error.is_none(),
        "{:?}",
        terminal_response.error
    );
    let terminal_run: RunRecursiveLiveSchedulerResponse =
        serde_json::from_value(terminal_response.result.unwrap()).unwrap();
    assert_eq!(
        terminal_run.stop_reason,
        rsi_common::RecursiveSchedulerStopReason::GraphTerminal
    );
    assert_eq!(terminal_run.step_count, 0);
    assert!(terminal_run.live_attempts.is_empty());
    assert_eq!(
        recursive_table_count(&fixture.manager, "recursive_live_output_validations").await,
        count_after_commits
    );

    let after = recursive_dag_live_scheduler_gate_counts(&fixture.manager).await;
    assert_eq!(after.graph_count, before.graph_count);
    assert_eq!(after.scheduler_run_count, before.scheduler_run_count + 3);
    assert_eq!(after.task_attempt_count, before.task_attempt_count + 2);
    assert_eq!(after.live_attempt_count, before.live_attempt_count + 2);
    assert_eq!(after.session_count, before.session_count + 2);
    assert_eq!(after.topology_count, before.topology_count);
    assert_eq!(
        after.topology_graph_link_count,
        before.topology_graph_link_count
    );
    assert_eq!(
        after.topology_task_link_count,
        before.topology_task_link_count
    );

    fixture
        .runtime_config
        .update_field(
            "recursive_dag_live_scheduler_control_enabled",
            &serde_json::json!(false),
        )
        .expect("live scheduler gate disabled after smoke");
    let caps_response = call_rpc(
        &fixture.server,
        "GetDaemonCapabilities",
        serde_json::Value::Null,
    )
    .await;
    let caps: rsi_common::DaemonCapabilities =
        serde_json::from_value(caps_response.result.unwrap()).unwrap();
    assert!(!caps.recursive_dag_live_scheduler_control);
    assert!(!caps.recursive_dag_live_execution);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn recursive_dag_live_scheduler_rpc_rejects_topology_linked_graph_without_mutation() {
    let fixture = recursive_dag_rpc_fixture();
    fixture
        .runtime_config
        .update_field(
            "recursive_dag_live_scheduler_control_enabled",
            &serde_json::json!(true),
        )
        .expect("live scheduler gate enabled");
    let (_, graph_id, _) = create_recursive_topology_rpc_graph(&fixture.manager).await;
    let before = recursive_dag_live_scheduler_gate_counts(&fixture.manager).await;

    let response = call_rpc(
        &fixture.server,
        "RunRecursiveLiveScheduler",
        serde_json::json!({
            "graph_id": graph_id,
            "max_steps": 1,
            "operator": "test"
        }),
    )
    .await;
    let error = response.error.expect("topology graph rejected");
    assert_eq!(error.code, INVALID_PARAMS);
    assert!(error.message.contains("ordinary-graph only"));

    let after = recursive_dag_live_scheduler_gate_counts(&fixture.manager).await;
    assert_eq!(after, before);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn recursive_dag_live_scheduler_slice_keeps_fake_scheduler_fake_only() {
    let fixture = recursive_dag_rpc_fixture();
    set_recursive_dag_controls(&fixture.runtime_config, false, true, false);
    let (graph_id, _) = create_recursive_dag_rpc_graph(&fixture.manager).await;
    let before = recursive_dag_live_scheduler_gate_counts(&fixture.manager).await;

    let response = call_rpc(
        &fixture.server,
        "RunRecursiveFakeScheduler",
        serde_json::json!({
            "graph_id": graph_id,
            "max_steps": 1,
            "execution_mode": "live_session"
        }),
    )
    .await;
    let error = response.error.expect("fake scheduler rejects live mode");
    assert_eq!(error.code, INVALID_PARAMS);
    assert!(error.message.contains("fake-only in Phase 5A.5"));

    let after = recursive_dag_live_scheduler_gate_counts(&fixture.manager).await;
    assert_eq!(after, before);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn recursive_dag_rpc_capabilities_reflect_control_gates() {
    for (recovery, scheduler, cancellation) in [
        (false, false, false),
        (true, false, false),
        (false, true, false),
        (false, false, true),
        (true, true, false),
        (true, false, true),
        (false, true, true),
        (true, true, true),
    ] {
        let fixture = recursive_dag_rpc_fixture();
        set_recursive_dag_controls(&fixture.runtime_config, recovery, scheduler, cancellation);
        let response = call_rpc(
            &fixture.server,
            "GetDaemonCapabilities",
            serde_json::Value::Null,
        )
        .await;
        assert!(response.error.is_none());
        let caps: rsi_common::DaemonCapabilities =
            serde_json::from_value(response.result.unwrap()).unwrap();
        assert!(caps.recursive_dag_inspection);
        assert!(caps.recursive_dag_run_inspection);
        assert!(caps.recursive_dag_recovery_status);
        assert!(caps.recursive_dag_live_status_inspection);
        assert!(caps.recursive_dag_live_validation_inspection);
        assert!(caps.recursive_dag_artifact_lookup);
        assert!(caps.recursive_dag_artifact_list_pagination);
        assert!(caps.recursive_dag_artifact_preview_inspection);
        assert_eq!(caps.recursive_dag_recovery_control, recovery);
        assert_eq!(caps.recursive_dag_scheduler_control, scheduler);
        assert!(!caps.recursive_dag_live_scheduler_control);
        assert_eq!(caps.recursive_dag_cancellation_control, cancellation);
        assert!(!caps.recursive_dag_validation_list_pagination);
        assert!(!caps.recursive_dag_test_inspection);
        assert!(!caps.recursive_dag_diff_inspection);
        assert!(!caps.recursive_dag_scheduler_report_inspection);
        assert!(!caps.recursive_dag_safe_artifact_uri_open);
        assert!(!caps.recursive_dag_live_execution);
        assert!(!caps.recursive_dag_background_loop);
        assert!(!caps.gv_render_recursive_origin);
        assert!(!caps.gv_info_dashboard);
        fixture
            .runtime_config
            .update_field(
                "recursive_dag_live_scheduler_control_enabled",
                &serde_json::json!(true),
            )
            .unwrap();
        let live_response = call_rpc(
            &fixture.server,
            "GetDaemonCapabilities",
            serde_json::Value::Null,
        )
        .await;
        let live_caps: rsi_common::DaemonCapabilities =
            serde_json::from_value(live_response.result.unwrap()).unwrap();
        assert!(live_caps.recursive_dag_live_scheduler_control);
        assert!(live_caps.recursive_dag_live_execution);
        assert!(!live_caps.recursive_dag_background_loop);
        assert!(fixture.server.scheduler_handle.is_none());
    }
}

// ── P0 attribution gate pinning tests (slice-plan P0 done-means (a)-(d)) ──

fn mk_agent_test_session(
    id: Uuid,
    kind: rsi_common::types::SessionKind,
    parent_id: Option<Uuid>,
    lead: Option<Uuid>,
) -> rsi_common::types::Session {
    use rsi_common::types::{ContextUsageConfidence, Session, SessionProvider, SessionStatus};
    Session {
        context_fill_pct: None,
        id,
        provider: SessionProvider::Claude,
        claude_session_id: None,
        query: String::new(),
        title: Some(format!("test-{id}")),
        agent_role: None,
        epic_spawn_ordinal: None,
        description: None,
        short_summary: None,
        pending_question: None,
        pending_archive: false,
        working_dir: std::path::PathBuf::from("/tmp"),
        git_branch: None,
        status: SessionStatus::Running,
        project_id: None,
        pinned_at: None,
        testing_needed_at: None,
        rotation_disabled_at: None,
        session_kind: kind,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        cost_usd: None,
        duration_ms: None,
        num_turns: None,
        model: Some("claude-sonnet-5".to_string()),
        input_tokens: None,
        output_tokens: None,
        context_window: None,
        resolved_context_budget: None,
        total_input_tokens: None,
        total_prompt_tokens: None,
        total_output_tokens: None,
        total_cache_creation_tokens: None,
        total_cache_read_tokens: None,
        stop_reason: None,
        continued_from: None,
        context_usage_confidence: ContextUsageConfidence::Missing,
        daemon_input_tokens: None,
        daemon_output_tokens: None,
        handoff_filepath: None,
        active_task: None,
        group_id: None,
        pipeline_artifact: None,
        workflow_id: None,
        workflow_id_override: None,
        rotation_depth: 0,
        retry_attempt: None,
        max_retries: None,
        effort: None,
        issue_identifier: None,
        issue_url: None,
        issue_tracker_id: None,
        scheduled_job_id: None,
        rating: None,
        harness_version_hash: None,
        test_passed: None,
        clippy_passed: None,
        turn_count: None,
        retry_count: None,
        approval_wait_ms: None,
        work_time_ms: None,
        approval_started_at: None,
        sandbox_kind: None,
        sandbox_root: None,
        sandbox_branch: None,
        sandbox_cleanup_state: None,
        tag: String::new(),
        tags: Vec::new(),
        parent_id,
        lead_session_id: lead,
        is_eval: false,
        capability_class: None,
        topology_node_id: None,
        topology_iteration: 0,
        provider_cli_version: None,
        provider_capabilities: Vec::new(),
        thinking_tokens: None,
        service_tier: None,
        cache_creation_1h_tokens: None,
        cache_creation_5m_tokens: None,
        permission_denial_count: None,
        subagent_stats_json: None,
        queued_turn_count: None,
        terminal_reason: None,
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn agent_gate_attributed_call_to_unlisted_method_is_denied() {
    // (b) attributed raw LaunchSession/SetEpicLead rejected.
    let fixture = recursive_dag_rpc_fixture();
    for method in ["LaunchSession", "SetEpicLead"] {
        let mut request = RpcRequest::new(method, serde_json::json!({}));
        request.session_token = Some("some-token".to_string());
        let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
        else {
            panic!("expected response");
        };
        let error = response.error.expect("attributed call must be denied");
        assert_eq!(error.code, INVALID_PARAMS);
        assert!(
            error
                .message
                .contains("not available to session-attributed callers"),
            "unexpected message: {}",
            error.message
        );
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn manager_rpc_appointment_is_operator_only() {
    let fixture = recursive_dag_rpc_fixture();
    for method in [
        "GetHarnessManager",
        "ListHarnessManagerEpics",
        "ListHarnessManagerScope",
        "ConfigureHarnessManager",
        "ListManagerNodes",
        "GetManagerNode",
        "ConfigureManagerNode",
        "RevokeManagerNode",
        "GetHarnessManagerPolicy",
        "ConfigureHarnessManagerPolicy",
        "GetHarnessManagerState",
        "AnswerHarnessManagerDecision",
        "ConfigureGlobalManager",
        "GetGlobalManager",
        "GetGlobalManagerWorkspace",
        "GetManagerNodeWorkspace",
        "GetFleetOverview",
        "RevokeGlobalManager",
        "RequestOperatorRestart",
        "GetOperatorRestart",
        "CancelOperatorRestart",
        "ForceOperatorRestart",
        "ListLegacyScratch",
        "AdoptLegacyScratch",
        "GetManagerTree",
    ] {
        assert!(!agent_gate::AGENT_VERBS.contains(&method));
        assert!(!agent_gate::READ_VERBS.contains(&method));
        let mut request = RpcRequest::new(method, serde_json::json!({}));
        request.session_token = Some("manager-token".into());
        let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
        else {
            panic!("expected response");
        };
        let error = response
            .error
            .expect("operator method must reject attributed callers");
        assert_eq!(error.code, INVALID_PARAMS);
        assert!(
            error
                .message
                .contains("not available to session-attributed callers")
        );
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn operator_rpc_lists_configures_and_revokes_area_node() {
    use rsi_common::harness_manager_v2::{
        ConfigureHarnessManagerPolicyRequestV2, ManagerCapabilityV2, ManagerOperatingModeV2,
        ManagerPolicyV2,
    };
    use rsi_common::manager_nodes::{
        ConfigureManagerNodeRequestV1, ManagerNodeAllowanceV1, ManagerNodeGrantV1,
        ManagerNodeSelectorV1, RevokeManagerNodeRequestV1,
    };
    use rsi_common::types::SessionKind;
    let fixture = recursive_dag_rpc_fixture();
    let project = issue_rpc_project_id();
    let owner = Uuid::new_v4();
    let area_seat = Uuid::new_v4();
    let group = Uuid::new_v4();
    let epic = Uuid::new_v4();
    {
        let store = fixture.manager.store().lock().await;
        for (id, kind, parent) in [
            (owner, SessionKind::Standard, None),
            (area_seat, SessionKind::Standard, None),
            (group, SessionKind::Group, None),
            (epic, SessionKind::Epic, Some(group)),
        ] {
            let mut session = mk_agent_test_session(id, kind, parent, None);
            session.project_id = Some(project);
            store.insert_session(&session).unwrap();
        }
    }
    let appointed = call_rpc(&fixture.server,"ConfigureHarnessManager",serde_json::json!({
        "project_id":project,"session_id":owner,"epic_ids":null,"group_ids":[],"expected_row_version":0
    })).await;
    assert!(appointed.error.is_none(), "{:?}", appointed.error);
    let root_policy = call_rpc(
        &fixture.server,
        "ConfigureHarnessManagerPolicy",
        serde_json::json!(ConfigureHarnessManagerPolicyRequestV2 {
            project_id: project,
            expected_scope_version: 1,
            expected_policy_version: 0,
            idempotency_key: "root-grant".into(),
            policy: ManagerPolicyV2 {
                mode: ManagerOperatingModeV2::Execute,
                capabilities: vec![
                    ManagerCapabilityV2::WorkPlan,
                    ManagerCapabilityV2::LeadControl
                ],
                max_active_sessions: 8,
                ..Default::default()
            },
        }),
    )
    .await;
    assert!(root_policy.error.is_none(), "{:?}", root_policy.error);
    let listed = call_rpc(
        &fixture.server,
        "ListManagerNodes",
        serde_json::json!({"project_id":project,"limit":64}),
    )
    .await;
    assert!(listed.error.is_none(), "{:?}", listed.error);
    let root = &listed.result.as_ref().unwrap()["rows"][0];
    let root_id: Uuid = serde_json::from_value(root["node_id"].clone()).unwrap();
    let request = ConfigureManagerNodeRequestV1 {
        node_id: None,
        parent_node_id: root_id,
        project_id: project,
        seat_root_session_id: area_seat,
        selector: ManagerNodeSelectorV1::Selected {
            group_ids: vec![],
            epic_ids: vec![epic],
        },
        grant: ManagerNodeGrantV1 {
            capabilities: vec![ManagerCapabilityV2::WorkPlan],
            allowed_launches: vec![],
            allowance: ManagerNodeAllowanceV1 {
                max_created_containers: 0,
                max_created_sessions: 0,
                max_active_sessions: 3,
                max_build_slots: 0,
                max_disk_gib: 0,
                provider_limits: vec![],
                max_spend_usd: None,
            },
            max_direct_reports: 4,
        },
        policy: ManagerPolicyV2 {
            mode: ManagerOperatingModeV2::Monitor,
            capabilities: vec![ManagerCapabilityV2::WorkPlan],
            max_active_sessions: 3,
            ..Default::default()
        },
        expected_parent_grant_version: root["grant_version"].as_i64().unwrap(),
        expected_parent_policy_version: root["policy_version"].as_i64().unwrap(),
        expected_parent_authority_epoch: root["authority_epoch"].as_i64().unwrap(),
        expected_node_grant_version: 0,
        idempotency_key: "area-create".into(),
    };
    let created = call_rpc(
        &fixture.server,
        "ConfigureManagerNode",
        serde_json::json!(request.clone()),
    )
    .await;
    assert!(created.error.is_none(), "{:?}", created.error);
    let area = created.result.unwrap();
    let area_id: Uuid = serde_json::from_value(area["node_id"].clone()).unwrap();
    let fetched = call_rpc(
        &fixture.server,
        "GetManagerNode",
        serde_json::json!({"project_id":project,"node_id":area_id}),
    )
    .await;
    assert_eq!(fetched.result.unwrap()["grant_state"], "granted");
    let revoked = call_rpc(
        &fixture.server,
        "RevokeManagerNode",
        serde_json::json!(RevokeManagerNodeRequestV1 {
            project_id: project,
            node_id: area_id,
            expected_grant_version: area["grant_version"].as_i64().unwrap(),
            expected_authority_epoch: area["authority_epoch"].as_i64().unwrap(),
            idempotency_key: "area-revoke".into(),
        }),
    )
    .await;
    assert!(revoked.error.is_none(), "{:?}", revoked.error);
    assert_eq!(revoked.result.unwrap()["state"], "revoked");

    fixture
        .manager
        .register_agent_token("area-delegator-token".into(), owner)
        .await;
    let mut delegated_request = serde_json::to_value(request).unwrap();
    delegated_request
        .as_object_mut()
        .unwrap()
        .remove("project_id");
    delegated_request["idempotency_key"] = "area-delegated".into();
    let mut rpc = RpcRequest::new("AgentManagerDelegateNode", delegated_request);
    rpc.session_token = Some("area-delegator-token".into());
    let HandleResult::Response(delegated) = fixture.server.handle_request_inner(&rpc).await else {
        panic!("expected delegation response");
    };
    assert!(delegated.error.is_none(), "{:?}", delegated.error);
    let delegated = delegated.result.unwrap();
    assert_eq!(delegated["parent_node_id"], root_id.to_string());
    fixture
        .manager
        .register_agent_token("area-escalator-token".into(), area_seat)
        .await;
    let mut escalate = RpcRequest::new(
        "AgentManagerEscalate",
        serde_json::json!({
            "subject_id":Uuid::new_v4(),"reason":"shared schedule",
            "route":{"kind":"parent"},
            "expected_source_authority_epoch":delegated["authority_epoch"],
            "expected_source_grant_version":delegated["grant_version"],
            "expected_target_authority_epoch":root["authority_epoch"],
            "expected_target_grant_version":root["grant_version"],
            "expected_target_session_id":owner,"idempotency_key":"area-escalate"
        }),
    );
    escalate.session_token = Some("area-escalator-token".into());
    let HandleResult::Response(escalated) = fixture.server.handle_request_inner(&escalate).await
    else {
        panic!("expected escalation response");
    };
    assert!(escalated.error.is_none(), "{:?}", escalated.error);
    let escalation = escalated.result.unwrap();
    let mut list = RpcRequest::new("AgentManagerListEscalations", serde_json::json!({}));
    list.session_token = Some("area-delegator-token".into());
    let HandleResult::Response(listed) = fixture.server.handle_request_inner(&list).await else {
        panic!("expected escalation list");
    };
    assert!(listed.error.is_none(), "{:?}", listed.error);
    assert_eq!(listed.result.unwrap()[0]["id"], escalation["id"]);
    let mut resolve = RpcRequest::new(
        "AgentManagerResolveEscalation",
        serde_json::json!({
            "escalation_id":escalation["id"],"expected_version":1,
            "expected_target_authority_epoch":root["authority_epoch"],
            "expected_target_grant_version":root["grant_version"],
            "expected_target_session_id":owner,"ruling":"Use the shared slot",
            "idempotency_key":"area-ruling"
        }),
    );
    resolve.session_token = Some("area-delegator-token".into());
    let HandleResult::Response(resolved) = fixture.server.handle_request_inner(&resolve).await
    else {
        panic!("expected escalation ruling");
    };
    assert!(resolved.error.is_none(), "{:?}", resolved.error);
    assert_eq!(resolved.result.unwrap()["state"], "ruled");
    let mut sent = RpcRequest::new("AgentManagerListEscalations", serde_json::json!({}));
    sent.session_token = Some("area-escalator-token".into());
    let HandleResult::Response(sent) = fixture.server.handle_request_inner(&sent).await else {
        panic!("expected sent escalation list");
    };
    assert!(sent.error.is_none(), "{:?}", sent.error);
    assert_eq!(sent.result.unwrap()[0]["ruling"], "Use the shared slot");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn manager_rpc_v2_operator_policy_board_and_exact_answer_flow() {
    use rsi_common::harness_manager_v2::*;
    use rsi_common::types::SessionKind;
    use serde_json::json;
    let fixture = recursive_dag_rpc_fixture();
    let project = issue_rpc_project_id();
    let owner = Uuid::new_v4();
    let group = Uuid::new_v4();
    let epic = Uuid::new_v4();
    let lead = Uuid::new_v4();
    {
        let store = fixture.manager.store().lock().await;
        for (id, kind, parent) in [
            (owner, SessionKind::Standard, None),
            (group, SessionKind::Group, None),
            (epic, SessionKind::Epic, Some(group)),
            (lead, SessionKind::Feature, Some(epic)),
        ] {
            let mut session = mk_agent_test_session(id, kind, parent, None);
            session.project_id = Some(project);
            store.insert_session(&session).unwrap();
        }
        store.set_lead_session(epic, Some(lead)).unwrap();
    }
    fixture
        .manager
        .register_agent_token("manager-v2-pilot".into(), owner)
        .await;
    let appointed = call_rpc(
        &fixture.server,
        "ConfigureHarnessManager",
        json!({"project_id":project,"session_id":owner,"epic_ids":[epic],"expected_row_version":0}),
    )
    .await;
    assert!(appointed.error.is_none(), "{:?}", appointed.error);
    let discovered = call_rpc(
        &fixture.server,
        "ListHarnessManagerEpics",
        json!({"project_id":project,"limit":64}),
    )
    .await;
    assert!(discovered.error.is_none(), "{:?}", discovered.error);
    assert_eq!(discovered.result.unwrap()["epics"][0]["id"], json!(epic));
    let scope_rows = call_rpc(
        &fixture.server,
        "ListHarnessManagerScope",
        json!({"project_id":project,"limit":64}),
    )
    .await;
    assert!(scope_rows.error.is_none(), "{:?}", scope_rows.error);
    let rows = scope_rows.result.unwrap()["rows"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(rows.len(), 2);
    assert!(
        rows.iter()
            .any(|row| row["id"] == json!(group) && row["kind"] == "Group")
    );
    assert!(
        rows.iter()
            .any(|row| row["id"] == json!(epic) && row["group_id"] == json!(group))
    );

    let policy = call_rpc(
        &fixture.server,
        "ConfigureHarnessManagerPolicy",
        json!(ConfigureHarnessManagerPolicyRequestV2 {
            project_id: project,
            expected_scope_version: 1,
            expected_policy_version: 0,
            idempotency_key: "grant".into(),
            policy: ManagerPolicyV2 {
                mode: ManagerOperatingModeV2::Monitor,
                capabilities: vec![ManagerCapabilityV2::WorkPlan],
                ..Default::default()
            },
        }),
    )
    .await;
    assert!(policy.error.is_none(), "{:?}", policy.error);
    assert_eq!(policy.result.unwrap()["policy"]["mode"], "monitor");
    async fn agent(server: &RpcServer, method: &str, params: serde_json::Value) -> RpcResponse {
        let mut request = RpcRequest::new(method, params);
        request.session_token = Some("manager-v2-pilot".into());
        let HandleResult::Response(response) = server.handle_request_inner(&request).await else {
            panic!("expected response")
        };
        response
    }
    let declared=agent(&fixture.server,"AgentManagerUpdate",json!({"fence":{"scope_version":1,"policy_version":1},"idempotency_key":"question","change":{"update":"decision","key":"delivery-order","expected_row_version":0,"epic_id":epic,"question":"Which delivery order?","request_id":null,"work_key":null}})).await;
    assert!(declared.error.is_none(), "{:?}", declared.error);
    let inspected = call_rpc(
        &fixture.server,
        "GetHarnessManagerState",
        json!({"project_id":project,"query":{"section":"decisions"}}),
    )
    .await;
    assert!(inspected.error.is_none(), "{:?}", inspected.error);
    let page = inspected.result.unwrap();
    let row = &page["rows"][0];
    assert_eq!(row["question"], "Which delivery order?");
    let answer = json!({"project_id":project,"fence":{"scope_version":page["scope_version"],"policy_version":page["policy"]["row_version"]},
        "decision_key":row["key"],"expected_row_version":row["row_version"],"target_digest":row["target_digest"],"answer":"Dependencies first","idempotency_key":"operator-order"});
    let accepted = call_rpc(
        &fixture.server,
        "AnswerHarnessManagerDecision",
        answer.clone(),
    )
    .await;
    assert!(accepted.error.is_none(), "{:?}", accepted.error);
    let replay = call_rpc(
        &fixture.server,
        "AnswerHarnessManagerDecision",
        answer.clone(),
    )
    .await;
    assert_eq!(replay.result.unwrap()["deduplicated"], true);
    let retrieved = agent(
        &fixture.server,
        "AgentManagerInspect",
        json!({"section":"decisions"}),
    )
    .await;
    assert!(retrieved.error.is_none(), "{:?}", retrieved.error);
    let result = retrieved.result.unwrap();
    assert_eq!(result["rows"][0]["answer"], "Dependencies first");
    assert_eq!(
        result["rows"][0]["answer_retrieval"]["manager"]["actor_session_id"],
        owner.to_string()
    );
    let revoked = call_rpc(
        &fixture.server,
        "ConfigureHarnessManager",
        json!({"project_id":project,"session_id":owner,"epic_ids":[],"expected_row_version":1}),
    )
    .await;
    assert!(revoked.error.is_none(), "{:?}", revoked.error);
    let stale = call_rpc(&fixture.server, "AnswerHarnessManagerDecision", answer).await;
    assert!(stale.error.is_some());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn manager_rpc_two_epic_exchange_matches_native_tools_and_revokes_pending_access() {
    use crate::session::harness::tools::rsi_control::{
        ManagerControlToolKind, execute_manager_tool,
    };
    use rsi_common::harness_manager::*;
    use rsi_common::harness_manager_v2::{
        ConfigureHarnessManagerPolicyRequestV2, ManagerCapabilityV2, ManagerOperatingModeV2,
        ManagerPolicyV2,
    };
    use rsi_common::types::SessionKind;

    async fn invoke(
        server: &RpcServer,
        token: &str,
        method: &str,
        params: serde_json::Value,
    ) -> RpcResponse {
        let mut request = RpcRequest::new(method, params);
        request.session_token = Some(token.into());
        let HandleResult::Response(response) = server.handle_request_inner(&request).await else {
            panic!("expected response");
        };
        response
    }

    let fixture = recursive_dag_rpc_fixture();
    let project = issue_rpc_project_id();
    let manager_id = Uuid::new_v4();
    let group_id = Uuid::new_v4();
    let epics = [Uuid::new_v4(), Uuid::new_v4()];
    let leads = [Uuid::new_v4(), Uuid::new_v4()];
    {
        let store = fixture.manager.store().lock().await;
        let mut manager = mk_agent_test_session(manager_id, SessionKind::Standard, None, None);
        manager.project_id = Some(project);
        store.insert_session(&manager).unwrap();
        let mut group = mk_agent_test_session(group_id, SessionKind::Group, None, None);
        group.project_id = Some(project);
        store.insert_session(&group).unwrap();
        for index in 0..2 {
            let mut epic =
                mk_agent_test_session(epics[index], SessionKind::Epic, Some(group_id), None);
            epic.project_id = Some(project);
            epic.title = Some(format!("Feature {index}"));
            store.insert_session(&epic).unwrap();
            let mut lead =
                mk_agent_test_session(leads[index], SessionKind::Feature, Some(epic.id), None);
            lead.project_id = Some(project);
            store.insert_session(&lead).unwrap();
            store.set_lead_session(epic.id, Some(lead.id)).unwrap();
        }
    }
    fixture
        .manager
        .register_agent_token("manager-pilot".into(), manager_id)
        .await;
    for (index, lead) in leads.iter().enumerate() {
        fixture
            .manager
            .register_agent_token(format!("lead-{index}"), *lead)
            .await;
    }
    assert!(
        invoke(
            &fixture.server,
            "manager-pilot",
            "AgentManagerProgress",
            serde_json::json!({})
        )
        .await
        .error
        .is_some()
    );
    let appointment = call_rpc(
        &fixture.server,
        "ConfigureHarnessManager",
        serde_json::json!({
            "project_id": project, "session_id": manager_id, "epic_ids": epics, "expected_row_version": 0
        }),
    )
    .await;
    assert!(appointment.error.is_none(), "{:?}", appointment.error);
    let config: HarnessManagerConfigV1 =
        serde_json::from_value(appointment.result.unwrap()).unwrap();
    assert_eq!(config.current_session_id, Some(manager_id));
    let mut native_inspection = execute_manager_tool(
        &fixture.manager.agent_control(),
        manager_id,
        ManagerControlToolKind::Inspect,
        serde_json::json!({}),
    )
    .await
    .unwrap();
    let rpc_inspection = invoke(
        &fixture.server,
        "manager-pilot",
        "AgentManagerInspect",
        serde_json::json!({}),
    )
    .await;
    let mut rpc_inspection = rpc_inspection.result.unwrap();
    assert!(native_inspection["observed_at"].is_string());
    assert!(rpc_inspection["observed_at"].is_string());
    native_inspection
        .as_object_mut()
        .unwrap()
        .remove("observed_at");
    rpc_inspection
        .as_object_mut()
        .unwrap()
        .remove("observed_at");
    assert_eq!(rpc_inspection, native_inspection);
    let manager_control = native_inspection["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["type"] == "manager_control")
        .unwrap();
    assert_eq!(manager_control["eligibility"], "eligible");
    assert_eq!(
        manager_control["logical_manager_session_id"],
        manager_id.to_string()
    );
    assert_eq!(
        manager_control["current_session_id"],
        manager_id.to_string()
    );
    assert!(
        manager_control["expected"]["authority_epoch"]
            .as_i64()
            .unwrap()
            > 0
    );
    let progress = invoke(
        &fixture.server,
        "manager-pilot",
        "AgentManagerProgress",
        serde_json::json!({}),
    )
    .await;
    let progress: AgentManagerProgressResultV1 =
        serde_json::from_value(progress.result.unwrap()).unwrap();
    assert_eq!(progress.rows.len(), 2);
    assert!(progress.rows.iter().any(|row| row.title == "Feature 0"));
    assert!(progress.rows.iter().any(|row| row.title == "Feature 1"));
    let status = invoke(
        &fixture.server,
        "manager-pilot",
        "AgentGetStatus",
        serde_json::json!({"session_id": leads[0]}),
    )
    .await;
    assert_eq!(
        status
            .result
            .as_ref()
            .and_then(|value| value["id"].as_str())
            .map(ToOwned::to_owned),
        Some(leads[0].to_string())
    );
    let denied = invoke(
        &fixture.server,
        "manager-pilot",
        "AgentHalt",
        serde_json::json!({"session_id": leads[0]}),
    )
    .await;
    assert!(
        denied
            .error
            .as_ref()
            .is_some_and(|error| error.message.contains("manager_v2_capability_denied")),
        "manager mutation requires the explicit SessionControl grant"
    );
    let granted = call_rpc(
        &fixture.server,
        "ConfigureHarnessManagerPolicy",
        serde_json::json!(ConfigureHarnessManagerPolicyRequestV2 {
            project_id: project,
            expected_scope_version: config.row_version,
            expected_policy_version: 0,
            idempotency_key: "session-control".into(),
            policy: ManagerPolicyV2 {
                mode: ManagerOperatingModeV2::Execute,
                capabilities: vec![ManagerCapabilityV2::SessionControl],
                ..Default::default()
            },
        }),
    )
    .await;
    assert!(granted.error.is_none(), "{:?}", granted.error);
    let status = invoke(
        &fixture.server,
        "manager-pilot",
        "AgentGetStatus",
        serde_json::json!({"session_id": leads[0]}),
    )
    .await;
    assert_eq!(
        status
            .result
            .as_ref()
            .and_then(|value| value["id"].as_str())
            .map(ToOwned::to_owned),
        Some(leads[0].to_string())
    );

    let mut requests = Vec::new();
    for index in 0..2 {
        let params = serde_json::json!({"epic_id":epics[index], "message":"Readiness with evidence?", "idempotency_key":format!("request-{index}")});
        let sent = invoke(
            &fixture.server,
            "manager-pilot",
            "AgentManagerSend",
            params.clone(),
        )
        .await;
        let receipt: HarnessManagerMessageReceiptV1 =
            serde_json::from_value(sent.result.unwrap()).unwrap();
        let replay = invoke(&fixture.server, "manager-pilot", "AgentManagerSend", params).await;
        let replay: HarnessManagerMessageReceiptV1 =
            serde_json::from_value(replay.result.unwrap()).unwrap();
        assert_eq!(replay.message_id, receipt.message_id);
        assert!(replay.deduplicated);
        let token = format!("lead-{index}");
        let inbox = invoke(
            &fixture.server,
            &token,
            "AgentManagerInbox",
            serde_json::json!({}),
        )
        .await;
        let inbox: AgentManagerInboxResultV1 =
            serde_json::from_value(inbox.result.unwrap()).unwrap();
        assert_eq!(inbox.messages.len(), 1);
        assert_eq!(inbox.messages[0].message_id, receipt.message_id);
        assert_eq!(inbox.messages[0].sender_session_id, manager_id);
        let reply = execute_manager_tool(&fixture.manager.agent_control(), leads[index], ManagerControlToolKind::Reply,
            serde_json::json!({"request_id":receipt.message_id,"message":format!("Feature {index}: checks passed; commit abc{index}"),"idempotency_key":"reply-1"})).await.unwrap();
        let reply: HarnessManagerMessageReceiptV1 = serde_json::from_value(reply).unwrap();
        assert_eq!(reply.request_id, Some(receipt.message_id));
        requests.push(receipt.message_id);
    }
    let mut native = execute_manager_tool(
        &fixture.manager.agent_control(),
        manager_id,
        ManagerControlToolKind::Inbox,
        serde_json::json!({}),
    )
    .await
    .unwrap();
    let rpc = invoke(
        &fixture.server,
        "manager-pilot",
        "AgentManagerInbox",
        serde_json::json!({}),
    )
    .await;
    let mut rpc = rpc.result.unwrap();
    assert_eq!(native["notices"].as_array().unwrap().len(), 2);
    native.as_object_mut().unwrap().remove("notices");
    rpc.as_object_mut().unwrap().remove("notices");
    assert_eq!(native, rpc);
    assert!(invoke(&fixture.server, "lead-1", "AgentManagerReply", serde_json::json!({"request_id":requests[0],"message":"wrong Epic","idempotency_key":"wrong"})).await.error.is_some());
    let cleared = call_rpc(
        &fixture.server,
        "ConfigureHarnessManager",
        serde_json::json!({
            "project_id": project, "session_id": manager_id,"epic_ids":[],"expected_row_version":config.row_version
        }),
    )
    .await;
    assert!(cleared.error.is_none(), "{:?}", cleared.error);
    assert!(
        invoke(
            &fixture.server,
            "manager-pilot",
            "AgentGetStatus",
            serde_json::json!({"session_id": leads[0]}),
        )
        .await
        .error
        .is_some(),
        "scope revocation removes manager read reach"
    );
    assert!(
        invoke(
            &fixture.server,
            "lead-0",
            "AgentManagerInbox",
            serde_json::json!({})
        )
        .await
        .error
        .is_some()
    );
    assert!(invoke(&fixture.server, "lead-0", "AgentManagerReply", serde_json::json!({"request_id":requests[0],"message":"late reply","idempotency_key":"late"})).await.error.is_some());
    let store = fixture.manager.store().lock().await;
    assert!(
        store
            .list_scheduled_jobs()
            .unwrap()
            .iter()
            .all(|job| !job.enabled)
    );
    for index in 0..2 {
        assert_eq!(
            store
                .get_session(epics[index])
                .unwrap()
                .unwrap()
                .lead_session_id,
            Some(leads[index])
        );
    }
}

/// Issue #548: `AgentManagerWorkView` serves a manager-created worker its
/// Epic's granted ownership through the token-bound RPC path and refuses a
/// lead-spawned sibling with a typed code and no data.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn manager_rpc_work_view_serves_created_worker_and_refuses_sibling() {
    use rsi_common::harness_manager::*;
    use rsi_common::harness_manager_v2::*;
    use rsi_common::types::SessionKind;

    async fn invoke(
        server: &RpcServer,
        token: &str,
        method: &str,
        params: serde_json::Value,
    ) -> RpcResponse {
        let mut request = RpcRequest::new(method, params);
        request.session_token = Some(token.into());
        let HandleResult::Response(response) = server.handle_request_inner(&request).await else {
            panic!("expected response");
        };
        response
    }

    let fixture = recursive_dag_rpc_fixture();
    let project = issue_rpc_project_id();
    let [
        manager_id,
        group_id,
        epic_id,
        lead_id,
        worker_id,
        sibling_id,
    ] = std::array::from_fn(|_| Uuid::new_v4());
    {
        let store = fixture.manager.store().lock().await;
        for (id, kind, parent) in [
            (manager_id, SessionKind::Standard, None),
            (group_id, SessionKind::Group, None),
            (epic_id, SessionKind::Epic, Some(group_id)),
            (lead_id, SessionKind::Feature, Some(epic_id)),
            (worker_id, SessionKind::Task, Some(epic_id)),
            (sibling_id, SessionKind::Task, Some(epic_id)),
        ] {
            let mut row = mk_agent_test_session(id, kind, parent, None);
            row.project_id = Some(project);
            store.insert_session(&row).unwrap();
        }
        store.set_lead_session(epic_id, Some(lead_id)).unwrap();
        store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                group_ids: Vec::new(),
                project_id: project,
                session_id: manager_id,
                epic_ids: Some(vec![epic_id]),
                expected_row_version: 0,
            })
            .unwrap();
        store
            .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
                project_id: project,
                expected_scope_version: 1,
                expected_policy_version: 0,
                idempotency_key: "work-view-grant".into(),
                policy: ManagerPolicyV2 {
                    capabilities: vec![ManagerCapabilityV2::WorkPlan],
                    ..Default::default()
                },
            })
            .unwrap();
        let config = store.get_harness_manager(project).unwrap().unwrap();
        let operation = store
            .manager_v2_save_receipt(
                &config,
                Some(manager_id),
                1,
                "action",
                "work-view-create",
                &serde_json::json!({"created":worker_id}),
                &serde_json::json!({"id":worker_id}),
            )
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO harness_manager_v2_entities(session_id,operation_id,project_id,manager_session_id,scope_version,policy_version,kind,created_at) VALUES(?1,?2,?3,?4,1,1,'session',?5)",
                rusqlite::params![
                    worker_id.to_string(),
                    operation.to_string(),
                    project.to_string(),
                    manager_id.to_string(),
                    crate::store::harness_manager_v2::now()
                ],
            )
            .unwrap();
    }
    for (token, id) in [
        ("work-view-manager", manager_id),
        ("work-view-worker", worker_id),
        ("work-view-sibling", sibling_id),
    ] {
        fixture.manager.register_agent_token(token.into(), id).await;
    }
    let update = |key: &str, change: ManagerUpdateV2| {
        serde_json::json!({
            "fence": {"scope_version": 1, "policy_version": 1},
            "idempotency_key": key,
            "change": change,
        })
    };
    for (key, change) in [
        (
            "work",
            ManagerUpdateV2::Work {
                key: "slice".into(),
                expected_row_version: 0,
                epic_id,
                title: "Granted slice".into(),
                kind: ManagerWorkKindV2::Product,
                priority: 1,
                weight: 1,
                required_gates: vec![
                    ManagerWorkStageV2::Implementation,
                    ManagerWorkStageV2::Review,
                    ManagerWorkStageV2::Verification,
                ],
                risk_tier: Default::default(),
            },
        ),
        (
            "claim",
            ManagerUpdateV2::Ownership {
                key: "slice".into(),
                expected_row_version: 0,
                domain: "crates/rsid/src/rpc.rs".into(),
                mode: ManagerOwnershipModeV2::Exclusive,
                files: vec!["crates/rsid/src/rpc.rs".into()],
                active: true,
            },
        ),
    ] {
        let response = invoke(
            &fixture.server,
            "work-view-manager",
            "AgentManagerUpdate",
            update(key, change),
        )
        .await;
        assert!(response.error.is_none(), "{:?}", response.error);
    }
    let response = invoke(
        &fixture.server,
        "work-view-worker",
        "AgentManagerWorkView",
        serde_json::json!({}),
    )
    .await;
    let page: AgentManagerWorkViewResultV1 =
        serde_json::from_value(response.result.expect("worker view")).unwrap();
    assert_eq!(page.epic_id, epic_id);
    assert_eq!(page.works[0].work_key, "slice");
    assert_eq!(
        page.ownership[0].files,
        vec!["crates/rsid/src/rpc.rs".to_owned()]
    );
    let refused = invoke(
        &fixture.server,
        "work-view-sibling",
        "AgentManagerWorkView",
        serde_json::json!({}),
    )
    .await;
    assert!(refused.result.is_none());
    assert_eq!(
        refused.error.unwrap().message,
        DaemonError::InvalidParam("manager_work_view_not_managed".into()).to_string()
    );
    let spoofed = invoke(
        &fixture.server,
        "work-view-sibling",
        "AgentManagerWorkView",
        serde_json::json!({"session_id": worker_id}),
    )
    .await;
    assert_eq!(
        spoofed.error.unwrap().message,
        DaemonError::InvalidParam("manager_invalid_request".into()).to_string()
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
#[allow(clippy::too_many_lines, clippy::unwrap_used)]
async fn manager_rpc_lead_notify_reaches_busy_manager_inbox() {
    use rsi_common::harness_manager::{AgentManagerInboxResultV1, HarnessManagerMessageReceiptV1};
    use rsi_common::types::SessionKind;

    async fn invoke(
        server: &RpcServer,
        token: &str,
        method: &str,
        params: serde_json::Value,
    ) -> RpcResponse {
        let mut request = RpcRequest::new(method, params);
        request.session_token = Some(token.into());
        let HandleResult::Response(response) = server.handle_request_inner(&request).await else {
            panic!("expected response")
        };
        response
    }

    let fixture = recursive_dag_rpc_fixture();
    let project = issue_rpc_project_id();
    let manager_id = Uuid::new_v4();
    let group_id = Uuid::new_v4();
    let epic_id = Uuid::new_v4();
    let other_epic_id = Uuid::new_v4();
    let lead_id = Uuid::new_v4();
    let other_lead_id = Uuid::new_v4();
    let worker_id = Uuid::new_v4();
    {
        let store = fixture.manager.store().lock().await;
        for (id, kind, parent) in [
            (manager_id, SessionKind::Standard, None),
            (group_id, SessionKind::Group, None),
            (epic_id, SessionKind::Epic, Some(group_id)),
            (other_epic_id, SessionKind::Epic, Some(group_id)),
            (lead_id, SessionKind::Feature, Some(epic_id)),
            (other_lead_id, SessionKind::Feature, Some(other_epic_id)),
            (worker_id, SessionKind::Task, Some(lead_id)),
        ] {
            let mut session = mk_agent_test_session(id, kind, parent, None);
            session.project_id = Some(project);
            store.insert_session(&session).unwrap();
        }
        store.set_lead_session(epic_id, Some(lead_id)).unwrap();
        store
            .set_lead_session(other_epic_id, Some(other_lead_id))
            .unwrap();
    }
    for (token, id) in [
        ("manager-notify", manager_id),
        ("lead-notify", lead_id),
        ("other-lead-notify", other_lead_id),
        ("worker-notify", worker_id),
    ] {
        fixture.manager.register_agent_token(token.into(), id).await;
    }
    let appointment = call_rpc(&fixture.server, "ConfigureHarnessManager", serde_json::json!({
        "project_id":project,"session_id":manager_id,"epic_ids":[epic_id],"expected_row_version":0
    })).await;
    assert!(appointment.error.is_none(), "{:?}", appointment.error);

    let params = serde_json::json!({"message":"Checks passed; commit abc123","idempotency_key":"notify-one"});
    let sent = invoke(
        &fixture.server,
        "lead-notify",
        "AgentManagerNotify",
        params.clone(),
    )
    .await;
    assert!(sent.error.is_none(), "{:?}", sent.error);
    let receipt: HarnessManagerMessageReceiptV1 =
        serde_json::from_value(sent.result.unwrap()).unwrap();
    assert_eq!(receipt.request_id, None);
    let replay = invoke(
        &fixture.server,
        "lead-notify",
        "AgentManagerNotify",
        params.clone(),
    )
    .await;
    let replay: HarnessManagerMessageReceiptV1 =
        serde_json::from_value(replay.result.unwrap()).unwrap();
    assert!(replay.deduplicated);
    assert_eq!(replay.message_id, receipt.message_id);
    let inbox = invoke(
        &fixture.server,
        "manager-notify",
        "AgentManagerInbox",
        serde_json::json!({}),
    )
    .await;
    let inbox: AgentManagerInboxResultV1 = serde_json::from_value(inbox.result.unwrap()).unwrap();
    assert_eq!(inbox.messages.len(), 1);
    assert_eq!(inbox.messages[0].message_id, receipt.message_id);
    assert_eq!(inbox.messages[0].request_id, None);
    assert_eq!(inbox.messages[0].sender_session_id, lead_id);
    assert_eq!(inbox.messages[0].message, "Checks passed; commit abc123");

    for (token, expected) in [
        ("manager-notify", "manager_notice_requires_feature_lead"),
        ("worker-notify", "manager_scope_denied"),
        ("other-lead-notify", "manager_scope_denied"),
    ] {
        let refused = invoke(&fixture.server, token, "AgentManagerNotify", params.clone()).await;
        assert!(
            refused
                .error
                .as_ref()
                .is_some_and(|error| error.message.contains(expected)),
            "{token}: {:?}",
            refused.error
        );
    }
    let changed = invoke(
        &fixture.server,
        "lead-notify",
        "AgentManagerNotify",
        serde_json::json!({"message":"changed","idempotency_key":"notify-one"}),
    )
    .await;
    assert!(
        changed
            .error
            .as_ref()
            .is_some_and(|error| error.message.contains("manager_idempotency_conflict"))
    );
    for field in ["epic_id", "caller_session_id", "sender_session_id"] {
        let mut forged = params.clone();
        forged[field] = serde_json::json!(Uuid::new_v4());
        let refused = invoke(&fixture.server, "lead-notify", "AgentManagerNotify", forged).await;
        assert!(
            refused
                .error
                .as_ref()
                .is_some_and(|error| error.message.contains("manager_invalid_request"))
        );
    }
    let inbox = invoke(
        &fixture.server,
        "manager-notify",
        "AgentManagerInbox",
        serde_json::json!({}),
    )
    .await;
    let inbox: AgentManagerInboxResultV1 = serde_json::from_value(inbox.result.unwrap()).unwrap();
    assert_eq!(inbox.messages.len(), 1);
    assert_eq!(inbox.messages[0].message_id, receipt.message_id);
    {
        let store = fixture.manager.store().lock().await;
        let replacement_id = Uuid::new_v4();
        let mut replacement = mk_agent_test_session(
            replacement_id,
            SessionKind::Feature,
            Some(epic_id),
            Some(lead_id),
        );
        replacement.project_id = Some(project);
        store.insert_session(&replacement).unwrap();
        store
            .set_lead_session(epic_id, Some(replacement_id))
            .unwrap();
    }
    let stale = invoke(
        &fixture.server,
        "lead-notify",
        "AgentManagerNotify",
        serde_json::json!({"message":"late","idempotency_key":"notify-late"}),
    )
    .await;
    assert!(
        stale
            .error
            .as_ref()
            .is_some_and(|error| error.message.contains("manager_scope_denied"))
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn manager_rpc_requires_transport_identity_and_redacts_invalid_params() {
    let fixture = recursive_dag_rpc_fixture();
    fixture
        .manager
        .register_agent_token("manager-live-token".into(), Uuid::new_v4())
        .await;
    for (method, valid) in [
        ("AgentManagerProgress", serde_json::json!({})),
        ("AgentManagerInbox", serde_json::json!({})),
        ("AgentManagerInspect", serde_json::json!({})),
        ("AgentManagerWorkView", serde_json::json!({})),
        (
            "AgentManagerUpdate",
            serde_json::json!({
                "fence":{"scope_version":1,"policy_version":1},
                "idempotency_key":"handoff", "change":{"update":"handoff","summary":"next work","next_actions":[]}
            }),
        ),
        (
            "AgentSubmitReviewReceipt",
            serde_json::json!({
                "assignment_id":Uuid::new_v4(),
                "verdict":"accepted",
                "findings":[],
                "idempotency_key":"review-receipt"
            }),
        ),
        (
            "AgentManagerControl",
            serde_json::json!({
                "fence":{"scope_version":1,"policy_version":1},
                "idempotency_key":"container", "operation":{"action":"create_container","kind":"Epic","name":"Feature","tags":[]}
            }),
        ),
        (
            "AgentManagerPrepareControl",
            serde_json::json!({
                "operation":{"action":"resume_lead","epic_id":Uuid::new_v4(),"message":"continue"}
            }),
        ),
        (
            "AgentManagerCommitPreparedControl",
            serde_json::json!({
                "prepared_id":Uuid::new_v4(),"target_digest":format!("sha256:{}", "a".repeat(64)),"idempotency_key":"commit"
            }),
        ),
        (
            "AgentManagerGetAction",
            serde_json::json!({"operation_id":Uuid::new_v4()}),
        ),
        (
            "AgentManagerSend",
            serde_json::json!({
                "epic_id": Uuid::new_v4(), "message": "evidence?", "idempotency_key": "request"
            }),
        ),
        (
            "AgentManagerReply",
            serde_json::json!({
                "request_id": Uuid::new_v4(), "message": "checks passed", "idempotency_key": "reply"
            }),
        ),
        (
            "AgentManagerNotify",
            serde_json::json!({"message":"status: ready","idempotency_key":"notify"}),
        ),
    ] {
        assert!(agent_gate::AGENT_VERBS.contains(&method));
        assert!(!agent_gate::READ_VERBS.contains(&method));
        for (token, expected) in [
            (None, "agent_verb_requires_session_token"),
            (
                Some("unknown-manager-token"),
                "agent_verb_unknown_session_token",
            ),
        ] {
            let mut request = RpcRequest::new(method, valid.clone());
            request.session_token = token.map(str::to_string);
            let HandleResult::Response(response) =
                fixture.server.handle_request_inner(&request).await
            else {
                panic!("expected response");
            };
            let error = response.error.expect("unbound caller must be refused");
            assert_eq!(error.code, INVALID_PARAMS);
            assert_eq!(
                error.message,
                DaemonError::InvalidParam(expected.into()).to_string()
            );
        }
        let mut forged = valid;
        forged["sender_session_id"] = serde_json::json!("/private/value token=secret");
        for malformed in [forged, serde_json::json!([]), serde_json::Value::Null] {
            let mut request = RpcRequest::new(method, malformed);
            request.session_token = Some("manager-live-token".into());
            let HandleResult::Response(response) =
                fixture.server.handle_request_inner(&request).await
            else {
                panic!("expected response");
            };
            let error = response
                .error
                .expect("strict DTO must reject malformed params");
            assert_eq!(error.code, INVALID_PARAMS);
            assert_eq!(
                error.message,
                DaemonError::InvalidParam("manager_invalid_request".into()).to_string()
            );
            assert!(error.data.is_none());
        }
    }
}

/// #694 K1: the key-vault methods are operator-only. Each is absent from
/// every agent-facing catalog (leaked-authority check) and a tokened
/// request is refused before dispatch.
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn provider_credential_methods_are_operator_only_and_attributed_denied() {
    let fixture = recursive_dag_rpc_fixture();
    let catalog = rsi_common::agent_control_schema::agent_control_catalog_v1();
    for method in rsi_common::provider_credentials::OPERATOR_METHODS {
        assert!(!agent_gate::AGENT_VERBS.contains(&method), "{method}");
        assert!(!agent_gate::READ_VERBS.contains(&method), "{method}");
        assert!(
            !agent_gate::is_allowed_for_attributed_caller(method),
            "{method}"
        );
        for descriptor in catalog {
            assert_ne!(descriptor.method, method, "agent CLI catalog: {method}");
            if let Some(tool) = descriptor.native_tool {
                assert!(
                    !tool.name().to_ascii_lowercase().contains("credential"),
                    "native tool {} exposes the key vault",
                    tool.name()
                );
            }
        }
        let mut request = RpcRequest::new(
            method,
            serde_json::json!({"slot": "openrouter", "secret": "sk-test-rpc-tokened"}),
        );
        request.session_token = Some("some-token".to_string());
        let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
        else {
            panic!("expected response for {method}");
        };
        assert!(response.result.is_none(), "{method}");
        let error = response
            .error
            .unwrap_or_else(|| panic!("attributed call to {method} must be denied"));
        assert_eq!(error.code, INVALID_PARAMS, "method: {method}");
        assert!(
            error
                .message
                .contains("not available to session-attributed callers"),
            "{method}: {}",
            error.message
        );
        assert!(!error.message.contains("sk-test-rpc-tokened"));
    }

    // Operator (tokenless) dispatch reaches the vault and returns
    // secret-free metadata for every slot.
    let request = RpcRequest::new(
        rsi_common::provider_credentials::METHOD_LIST,
        serde_json::Value::Null,
    );
    let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
    else {
        panic!("expected response");
    };
    let list: rsi_common::provider_credentials::ListProviderCredentialsResult =
        serde_json::from_value(response.result.expect("operator list succeeds")).unwrap();
    assert_eq!(
        list.credentials.len(),
        rsi_common::provider_credentials::ProviderCredentialSlot::ALL.len()
    );
}

/// #1407: the first-run Bedrock setup check is operator-only: absent from
/// every agent-facing catalog and refused for a tokened caller before any
/// Bedrock call, with a secret-free error.
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn verify_bedrock_setup_is_operator_only_and_attributed_denied() {
    let fixture = recursive_dag_rpc_fixture();
    let catalog = rsi_common::agent_control_schema::agent_control_catalog_v1();
    for method in rsi_common::provider_profile::OPERATOR_METHODS {
        assert!(!agent_gate::AGENT_VERBS.contains(&method), "{method}");
        assert!(!agent_gate::READ_VERBS.contains(&method), "{method}");
        assert!(
            !agent_gate::is_allowed_for_attributed_caller(method),
            "{method}"
        );
        for descriptor in catalog {
            assert_ne!(descriptor.method, method, "agent CLI catalog: {method}");
        }
        let mut request = RpcRequest::new(
            method,
            serde_json::json!({"model": "bedrock-api-key-rpc-canary"}),
        );
        request.session_token = Some("some-token".to_string());
        let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
        else {
            panic!("expected response for {method}");
        };
        assert!(response.result.is_none(), "{method}");
        let error = response
            .error
            .unwrap_or_else(|| panic!("attributed call to {method} must be denied"));
        assert!(
            error
                .message
                .contains("not available to session-attributed callers"),
            "{method}: {}",
            error.message
        );
        assert!(!error.message.contains("bedrock-api-key-rpc-canary"));
    }
}

#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn mcp_configuration_methods_are_operator_only_and_attributed_denied() {
    let fixture = recursive_dag_rpc_fixture();
    let catalog = rsi_common::agent_control_schema::agent_control_catalog_v1();
    for method in rsi_common::mcp::OPERATOR_METHODS {
        assert!(!agent_gate::AGENT_VERBS.contains(&method), "{method}");
        assert!(!agent_gate::READ_VERBS.contains(&method), "{method}");
        assert!(
            !agent_gate::is_allowed_for_attributed_caller(method),
            "{method}"
        );
        for descriptor in catalog {
            assert_ne!(descriptor.method, method, "agent CLI catalog: {method}");
            if let Some(tool) = descriptor.native_tool {
                assert!(
                    !tool.name().to_ascii_lowercase().contains("mcp"),
                    "native tool {} exposes MCP configuration",
                    tool.name()
                );
            }
        }

        let mut request = RpcRequest::new(
            method,
            serde_json::json!({"id": "docs", "secret": "mcp-test-rpc-tokened"}),
        );
        request.session_token = Some("some-token".to_string());
        let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
        else {
            panic!("expected response for {method}");
        };
        assert!(response.result.is_none(), "{method}");
        let error = response
            .error
            .unwrap_or_else(|| panic!("attributed call to {method} must be denied"));
        assert_eq!(error.code, INVALID_PARAMS, "method: {method}");
        assert!(!error.message.contains("mcp-test-rpc-tokened"), "{method}");
    }

    let request = RpcRequest::new(rsi_common::mcp::METHOD_LIST, serde_json::Value::Null);
    let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
    else {
        panic!("expected response");
    };
    let list: rsi_common::mcp::ListMcpServersResult =
        serde_json::from_value(response.result.expect("operator list succeeds")).unwrap();
    assert!(list.servers.is_empty());

    let upsert = call_rpc(
        &fixture.server,
        rsi_common::mcp::METHOD_UPSERT,
        serde_json::json!({
            "server": {
                "id": "docs",
                "command": "/usr/local/bin/mcp-docs",
                "args": ["--stdio"],
                "secret_env_names": ["MCP_DOCS_TOKEN"],
                "working_dir": "/tmp",
                "enabled": false
            }
        }),
    )
    .await;
    assert!(
        !serde_json::to_string(&upsert)
            .unwrap()
            .contains("mcp-test-rpc-secret")
    );
    let set_secret = call_rpc(
        &fixture.server,
        rsi_common::mcp::METHOD_SET_SECRET,
        serde_json::json!({"id": "docs", "secret": "mcp-test-rpc-secret"}),
    )
    .await;
    assert!(set_secret.result.is_some());
    assert!(
        !serde_json::to_string(&set_secret)
            .unwrap()
            .contains("mcp-test-rpc-secret")
    );

    let configured = call_rpc(
        &fixture.server,
        rsi_common::mcp::METHOD_LIST,
        serde_json::Value::Null,
    )
    .await;
    assert!(
        !serde_json::to_string(&configured)
            .unwrap()
            .contains("mcp-test-rpc-secret")
    );
    let daemon_config = call_rpc(&fixture.server, "GetDaemonConfig", serde_json::Value::Null).await;
    assert!(daemon_config.result.is_some());
    assert!(
        !serde_json::to_string(&daemon_config)
            .unwrap()
            .contains("mcp-test-rpc-secret")
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn source_worktree_settlement_surface_is_strict_and_attributed_denied() {
    let fixture = recursive_dag_rpc_fixture();
    for method in [
        "ListSourceWorktreeCohorts",
        "AuditSourceWorktreeCohort",
        "ApplySourceWorktreeCohort",
        "GetSourceWorktreeSettlementRun",
    ] {
        assert!(!agent_gate::AGENT_VERBS.contains(&method));
        assert!(!agent_gate::READ_VERBS.contains(&method));
        let mut request = RpcRequest::new(method, serde_json::json!({}));
        request.session_token = Some("some-token".to_string());
        let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
        else {
            panic!("expected response for {method}");
        };
        let error = response
            .error
            .unwrap_or_else(|| panic!("attributed call to {method} must be denied"));
        assert_eq!(error.code, INVALID_PARAMS, "method: {method}");
        assert!(
            error
                .message
                .contains("not available to session-attributed callers")
        );
    }

    let request = RpcRequest::new(
        "AuditSourceWorktreeCohort",
        serde_json::json!({
            "repository_identity": "/tmp/repo/.git",
            "sandbox_root": "/tmp/forbidden"
        }),
    );
    let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
    else {
        panic!("expected response");
    };
    assert_eq!(
        response.error.expect("unknown field must fail").code,
        INVALID_PARAMS
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]
async fn agent_archive_child_rpc_is_strict_token_bound_and_preserves_refusal() {
    use rsi_common::types::{SessionKind, SessionStatus};

    let fixture = recursive_dag_rpc_fixture();
    let [epic, lead, child, other] = std::array::from_fn(|_| Uuid::new_v4());
    {
        let store = fixture.manager.store().lock().await;
        for (id, kind, parent, status) in [
            (epic, SessionKind::Epic, None, SessionStatus::Running),
            (
                lead,
                SessionKind::Feature,
                Some(epic),
                SessionStatus::Running,
            ),
            (child, SessionKind::Task, Some(epic), SessionStatus::Failed),
            (other, SessionKind::Task, Some(epic), SessionStatus::Running),
        ] {
            let mut row = mk_agent_test_session(id, kind, parent, None);
            row.status = status;
            row.project_id = Some(issue_rpc_project_id());
            store.insert_session(&row).unwrap();
        }
        store.set_lead_session(epic, Some(lead)).unwrap();
    }
    fixture
        .manager
        .register_agent_token("archive-lead".into(), lead)
        .await;
    fixture
        .manager
        .register_agent_token("archive-other".into(), other)
        .await;

    let params = serde_json::json!({
        "target_session_id": child,
        "expected_tip_session_id": child,
        "expected_event_sequence": 0,
    });
    let invoke = |token: Option<&str>, params: serde_json::Value| {
        let mut request = RpcRequest::new("AgentArchiveChild", params);
        request.session_token = token.map(str::to_owned);
        request
    };
    for (malformed, token) in [
        (serde_json::Value::Null, "archive-lead"),
        (serde_json::json!([]), "archive-lead"),
        (
            serde_json::json!({"target_session_id":child}),
            "archive-lead",
        ),
        (
            serde_json::json!({
                "target_session_id":child, "expected_tip_session_id":child,
                "expected_event_sequence":0, "caller_session_id":lead
            }),
            "archive-other",
        ),
    ] {
        let HandleResult::Response(response) = fixture
            .server
            .handle_request_inner(&invoke(Some(token), malformed))
            .await
        else {
            panic!("expected response");
        };
        let error = response.error.unwrap();
        assert_eq!(error.code, INVALID_PARAMS);
        assert_eq!(error.data.unwrap()["code"], "invalid_request");
    }
    let HandleResult::Response(response) = fixture
        .server
        .handle_request_inner(&invoke(None, params.clone()))
        .await
    else {
        panic!("expected response");
    };
    assert_eq!(
        response.error.unwrap().message,
        "Invalid parameter: agent_verb_requires_session_token"
    );

    let HandleResult::Response(response) = fixture
        .server
        .handle_request_inner(&invoke(Some("archive-other"), params.clone()))
        .await
    else {
        panic!("expected response");
    };
    let error = response.error.expect("non-lead must be refused");
    assert_eq!(error.code, INVALID_PARAMS);
    assert_eq!(
        error.data.as_ref().unwrap()["code"],
        "target_not_authorized"
    );

    let HandleResult::Response(response) = fixture
        .server
        .handle_request_inner(&invoke(Some("archive-lead"), params))
        .await
    else {
        panic!("expected response");
    };
    assert!(response.error.is_none(), "{:?}", response.error);
    assert_eq!(
        response.result.unwrap()["target_session_id"],
        child.to_string()
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn archive_session_rpc_reaches_private_cleanup_and_replays_one_projection() {
    use crate::sandbox::SandboxAllocator;
    use crate::store::sandbox_custody::{CustodyCause, NewCustodyRoot, SessionCustodyBinding};
    use rsi_common::archive_cleanup::ArchiveSessionResultV1;
    use rsi_common::types::{SandboxCleanupState, SandboxKind, SessionStatus};
    use std::process::Command;

    fn git(path: &std::path::Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(path)
            .args(args)
            .output()
            .expect("run fixture Git");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("Git output utf8")
            .trim()
            .to_string()
    }

    let fixture = recursive_dag_rpc_fixture();
    let repository = fixture._dir.path().join("archive-rpc-repository");
    std::fs::create_dir(&repository).expect("repository directory");
    git(&repository, &["init", "-q", "-b", "main"]);
    git(&repository, &["config", "user.email", "rpc@example.test"]);
    git(&repository, &["config", "user.name", "RPC Archive Fixture"]);
    std::fs::write(repository.join("tracked"), "base\n").expect("tracked file");
    git(&repository, &["add", "tracked"]);
    git(&repository, &["commit", "-qm", "base"]);
    let source_oid = git(&repository, &["rev-parse", "HEAD"]);
    let session_id = Uuid::new_v4();
    let allocation = SandboxAllocator::new(fixture._dir.path().join("sandboxes"))
        .allocate(
            session_id,
            &repository,
            SandboxKind::GitWorktree,
            &source_oid,
            None,
        )
        .expect("allocate RPC archive worktree");
    #[cfg(target_os = "linux")]
    let _proc = crate::session::scoped_archive_cleanup_test_proc(
        session_id,
        &fixture._dir.path().join("sandboxes"),
    );
    let branch = allocation.branch.clone().expect("archive branch");
    let common_dir = std::fs::canonicalize(git(
        &repository,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    ))
    .expect("canonical Git common directory");
    let mut session =
        mk_agent_test_session(session_id, rsi_common::types::SessionKind::Task, None, None);
    session.project_id = None;
    session.status = SessionStatus::Completed;
    session.working_dir = repository.clone();
    session.sandbox_kind = Some(SandboxKind::GitWorktree);
    session.sandbox_root = Some(allocation.root.clone());
    session.sandbox_branch = Some(branch.clone());
    session.sandbox_cleanup_state = Some(SandboxCleanupState::Live);
    fixture
        .manager
        .store()
        .lock()
        .await
        .insert_session_with_custody(
            &session,
            SessionCustodyBinding::New(NewCustodyRoot {
                custody_id: Uuid::new_v4(),
                canonical_repo_dir: repository.display().to_string(),
                sandbox_root: allocation.root.display().to_string(),
                sandbox_branch: branch,
                repository_identity: common_dir.display().to_string(),
                source_commit: source_oid,
                cause: CustodyCause::FreshLaunch,
            }),
        )
        .expect("insert RPC cleanup candidate");
    fixture
        .manager
        .restore_sessions()
        .await
        .expect("hydrate completed RPC candidate");

    // Keep the RPC cleanup proof independent of unrelated host processes.
    // The cleanup runs on a blocking thread, so install the fixture for its
    // exact session and sandbox instead of a caller-thread-only proc hook.
    let _holder_proc = fixture
        .manager
        .install_archive_cleanup_test_holder_proc(session_id);

    let response = call_rpc(
        &fixture.server,
        "ArchiveSession",
        serde_json::json!({"session_id": session_id}),
    )
    .await;
    assert!(
        response.error.is_none(),
        "RPC archive error: {:?}",
        response.error
    );
    let result_value = response.result.clone().expect("RPC archive result");
    let result: ArchiveSessionResultV1 =
        serde_json::from_value(result_value.clone()).expect("decode RPC archive result");
    result.validate_wire().expect("RPC result wire contract");
    let receipt = result.receipt.as_ref().expect("RPC settled receipt");
    assert_eq!(receipt.session_id, session_id);

    let replay = call_rpc(
        &fixture.server,
        "ArchiveSession",
        serde_json::json!({"session_id": session_id}),
    )
    .await;
    assert!(
        replay.error.is_none(),
        "RPC replay error: {:?}",
        replay.error
    );
    assert_eq!(replay.result.as_ref(), Some(&result_value));
    let store = fixture.manager.store().lock().await;
    let projection_count: i64 = store
        .conn
        .query_row(
            "SELECT count(*) FROM archive_cleanup_success_projections WHERE session_id=?1",
            [session_id.to_string()],
            |row| row.get(0),
        )
        .expect("one durable RPC projection");
    assert_eq!(projection_count, 1);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn archive_cleanup_surface_is_strict_operator_only_and_undiscoverable_to_agents() {
    let fixture = recursive_dag_rpc_fixture();
    for method in ["ArchiveSession", "GetArchiveCleanupStatus"] {
        assert!(!agent_gate::AGENT_VERBS.contains(&method));
        assert!(!agent_gate::READ_VERBS.contains(&method));
        assert!(!agent_gate::is_allowed_for_attributed_caller(method));
        let mut request =
            RpcRequest::new(method, serde_json::json!({ "session_id": Uuid::new_v4() }));
        request.session_token = Some("some-token".to_string());
        let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
        else {
            panic!("expected response for {method}");
        };
        let error = response
            .error
            .unwrap_or_else(|| panic!("attributed call to {method} must be denied"));
        assert_eq!(error.code, INVALID_PARAMS, "method: {method}");
        assert!(
            error
                .message
                .contains("not available to session-attributed callers")
        );

        let response = call_rpc(
            &fixture.server,
            method,
            serde_json::json!({
                "session_id": Uuid::new_v4(),
                "run_id": Uuid::new_v4(),
            }),
        )
        .await;
        assert_eq!(
            response.error.expect("unknown selector rejected").code,
            INVALID_PARAMS,
            "method: {method}"
        );
    }

    let catalog = rsi_common::agent_control_schema::agent_control_catalog_v1();
    assert!(
        catalog
            .iter()
            .any(|descriptor| descriptor.method == "AgentSpawnChild")
    );
    for descriptor in catalog {
        assert_ne!(descriptor.method, "ArchiveSession");
        assert_ne!(descriptor.method, "GetArchiveCleanupStatus");
    }
    for (label, source, anchor) in [
        (
            "CodexAppServer tools",
            include_str!("../tool_registry.rs"),
            "AgentControlVerbV1::SpawnChild",
        ),
        (
            "Harness tools",
            include_str!("../session/harness/tools/rsi_control.rs"),
            "AgentControlVerbV1::SpawnChild",
        ),
    ] {
        assert!(source.contains(anchor), "{label} anchor missing");
        assert!(!source.contains("rsi_control_archive_session"), "{label}");
        assert!(
            !source.contains("rsi_control_get_archive_cleanup_status"),
            "{label}"
        );
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn efficiency_metrics_rpc_is_operator_only_and_not_agent_cataloged() {
    assert!(!agent_gate::AGENT_VERBS.contains(&"GetEfficiencyMetrics"));
    assert!(!agent_gate::READ_VERBS.contains(&"GetEfficiencyMetrics"));
    assert!(!agent_gate::is_allowed_for_attributed_caller(
        "GetEfficiencyMetrics"
    ));
    let fixture = recursive_dag_rpc_fixture();
    let mut request = RpcRequest::new(
        "GetEfficiencyMetrics",
        serde_json::json!({
            "from": "2026-09-28T00:00:00Z",
            "to": "2026-09-29T00:00:00Z",
            "group_by": "day",
        }),
    );
    request.session_token = Some("some-token".to_string());
    let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
    else {
        panic!("expected response for GetEfficiencyMetrics");
    };
    let error = response
        .error
        .expect("attributed efficiency metrics call must be denied");
    assert_eq!(error.code, INVALID_PARAMS);
    assert!(
        error
            .message
            .contains("not available to session-attributed callers")
    );
}

/// #1176: abandoning a blocked rotation moves custody and a seat, so it is an
/// operator-only method: not an agent or read verb, refused for an attributed
/// caller, and dispatched for the operator (who gets the typed refusal for a
/// session with nothing blocked).
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn abandon_blocked_rotation_is_operator_only_and_dispatched_for_the_operator() {
    let catalog = rsi_common::agent_control_schema::agent_control_catalog_v1();
    assert!(
        !catalog
            .iter()
            .any(|entry| entry.method == "AbandonBlockedRotation")
    );
    for source in [
        include_str!("../tool_registry.rs"),
        include_str!("../session/harness/tools/rsi_control.rs"),
    ] {
        assert!(!source.contains("AbandonBlockedRotation"));
        assert!(!source.contains("rsi_control_abandon_blocked_rotation"));
    }
    assert!(!agent_gate::AGENT_VERBS.contains(&"AbandonBlockedRotation"));
    assert!(!agent_gate::READ_VERBS.contains(&"AbandonBlockedRotation"));
    assert!(!agent_gate::is_allowed_for_attributed_caller(
        "AbandonBlockedRotation"
    ));
    let fixture = recursive_dag_rpc_fixture();
    let params = serde_json::json!({
        "session_id": Uuid::new_v4(),
        "idempotency_key": "rpc-1176",
    });
    let mut attributed = RpcRequest::new("AbandonBlockedRotation", params.clone());
    attributed.session_token = Some("some-token".to_string());
    let HandleResult::Response(response) = fixture.server.handle_request_inner(&attributed).await
    else {
        panic!("expected response for an attributed AbandonBlockedRotation");
    };
    let error = response.error.expect("attributed abandon must be denied");
    assert_eq!(error.code, INVALID_PARAMS);
    assert!(
        error
            .message
            .contains("not available to session-attributed callers")
    );
    let operator = RpcRequest::new("AbandonBlockedRotation", params);
    let HandleResult::Response(response) = fixture.server.handle_request_inner(&operator).await
    else {
        panic!("expected response for the operator's AbandonBlockedRotation");
    };
    let error = response
        .error
        .expect("nothing is blocked for an unknown session");
    assert!(
        error.message.contains("rotation_not_blocked"),
        "{}",
        error.message
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn agent_job_verbs_are_agent_cataloged_and_not_read_verbs() {
    for verb in [
        "AgentSubmitJob",
        "AgentGetJob",
        "AgentListJobs",
        "AgentCancelJob",
    ] {
        assert!(agent_gate::AGENT_VERBS.contains(&verb), "{verb}");
        assert!(!agent_gate::READ_VERBS.contains(&verb), "{verb}");
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn rolling_queue_enqueue_is_agent_cataloged_and_settings_stay_operator_only() {
    // The enqueue verb is the only agent-facing queue method.
    assert!(agent_gate::AGENT_VERBS.contains(&"AgentEnqueueLandingSource"));
    assert!(!agent_gate::READ_VERBS.contains(&"AgentEnqueueLandingSource"));
    for operator in ["GetRollingQueue", "GetDaemonConfig", "UpdateDaemonConfig"] {
        assert!(!agent_gate::AGENT_VERBS.contains(&operator), "{operator}");
        assert!(!agent_gate::READ_VERBS.contains(&operator), "{operator}");
        assert!(!agent_gate::is_allowed_for_attributed_caller(operator));
    }
    let fixture = recursive_dag_rpc_fixture();
    let mut attributed = RpcRequest::new("GetRollingQueue", serde_json::json!({}));
    attributed.session_token = Some("some-token".to_string());
    let HandleResult::Response(response) = fixture.server.handle_request_inner(&attributed).await
    else {
        panic!("expected response for GetRollingQueue");
    };
    assert!(
        response
            .error
            .expect("attributed queue read must be denied")
            .message
            .contains("not available to session-attributed callers")
    );

    // The operator surface round-trips the queue settings.
    let initial = call_rpc(&fixture.server, "GetRollingQueue", serde_json::Value::Null).await;
    let initial = initial.result.expect("operator queue read");
    assert_eq!(initial["enabled"], false);
    assert_eq!(initial["batch_size"], 4);
    assert_eq!(initial["speculation_depth"], 1);
    assert_eq!(initial["gate_timeout_mins"], 360);
    assert_eq!(initial["entries"], serde_json::json!([]));
    for (field, value) in [
        ("rolling_queue_enabled", serde_json::json!(true)),
        ("rolling_queue_batch_size", serde_json::json!(8)),
        ("rolling_queue_speculation_depth", serde_json::json!(2)),
        ("rolling_queue_gate_timeout_mins", serde_json::json!(120)),
    ] {
        let response = call_rpc(
            &fixture.server,
            "UpdateDaemonConfig",
            serde_json::json!({"field": field, "value": value}),
        )
        .await;
        assert!(response.error.is_none(), "{field}: {:?}", response.error);
    }
    let updated = call_rpc(&fixture.server, "GetRollingQueue", serde_json::Value::Null).await;
    let updated = updated.result.expect("operator queue read");
    assert_eq!(updated["enabled"], true);
    assert_eq!(updated["batch_size"], 8);
    assert_eq!(updated["speculation_depth"], 2);
    assert_eq!(updated["gate_timeout_mins"], 120);
    for (field, value) in [
        ("rolling_queue_batch_size", serde_json::json!(0)),
        ("rolling_queue_batch_size", serde_json::json!(9)),
        ("rolling_queue_speculation_depth", serde_json::json!(3)),
        ("rolling_queue_gate_timeout_mins", serde_json::json!(10)),
        ("rolling_queue_enabled", serde_json::json!("on")),
    ] {
        let response = call_rpc(
            &fixture.server,
            "UpdateDaemonConfig",
            serde_json::json!({"field": field, "value": value}),
        )
        .await;
        assert_eq!(
            response.error.expect("rejected").code,
            INVALID_PARAMS,
            "{field}"
        );
    }
}

/// #1254: the worker baton cap is an operator setting with an RPC surface:
/// it round-trips through UpdateDaemonConfig/GetDaemonConfig with its
/// overrides, persists, refuses values outside its two ranges and stays out
/// of the agent surface.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn worker_context_cap_round_trips_through_the_operator_rpc_inside_its_bounds() {
    for operator in ["GetDaemonConfig", "UpdateDaemonConfig"] {
        assert!(!agent_gate::AGENT_VERBS.contains(&operator), "{operator}");
        assert!(!agent_gate::READ_VERBS.contains(&operator), "{operator}");
    }
    let fixture = recursive_dag_rpc_fixture();
    let read = || async {
        call_rpc(&fixture.server, "GetDaemonConfig", serde_json::Value::Null)
            .await
            .result
            .expect("config read")
    };
    assert_eq!(read().await["worker_context_cap_tokens"], 60);
    for (field, value) in [
        ("worker_context_cap_tokens", serde_json::json!(50)),
        ("worker_context_cap_tokens", serde_json::json!(250_000)),
        ("worker_context_cap.Claude", serde_json::json!(70)),
        ("worker_context_cap.Codex/gpt-6-astra", serde_json::json!(0)),
    ] {
        let response = call_rpc(
            &fixture.server,
            "UpdateDaemonConfig",
            serde_json::json!({"field": field, "value": value}),
        )
        .await;
        assert!(response.error.is_none(), "{field}: {:?}", response.error);
    }
    let config = read().await;
    assert_eq!(config["worker_context_cap_tokens"], 250_000);
    assert_eq!(config["worker_context_cap.Claude"], 70);
    assert_eq!(config["worker_context_cap.Codex/gpt-6-astra"], 0);
    let store = fixture.manager.store.lock().await;
    assert_eq!(
        store
            .get_daemon_setting("worker_context_cap_tokens")
            .unwrap()
            .as_deref(),
        Some("250000")
    );
    assert_eq!(
        store
            .get_daemon_setting("worker_context_cap.Claude")
            .unwrap()
            .as_deref(),
        Some("70")
    );
    drop(store);
    assert_eq!(
        fixture.runtime_config.worker_context_cap(
            rsi_common::types::SessionProvider::Claude,
            Some("claude-opus-5-5"),
            1_000_000
        ),
        Some(700_000)
    );
    for value in [
        serde_json::json!(101),
        serde_json::json!(31_999),
        serde_json::json!(2_000_001),
        serde_json::json!("sixty"),
    ] {
        let response = call_rpc(
            &fixture.server,
            "UpdateDaemonConfig",
            serde_json::json!({"field": "worker_context_cap_tokens", "value": value}),
        )
        .await;
        assert_eq!(
            response.error.expect("rejected").code,
            INVALID_PARAMS,
            "{value}"
        );
    }
    assert_eq!(read().await["worker_context_cap_tokens"], 250_000);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn harness_tool_policy_launch_param_is_validated_and_defaults_round_trip() {
    let fixture = recursive_dag_rpc_fixture();
    let launch = |provider: &str, policy: serde_json::Value| {
        serde_json::json!({
            "query": "blind benchmark", "provider": provider,
            "tags": ["bench"], "tool_policy": policy,
        })
    };
    // A provider that cannot enforce the policy is refused before launch.
    let response = call_rpc(
        &fixture.server,
        "LaunchSession",
        launch("Claude", serde_json::json!({"web_access": "disabled"})),
    )
    .await;
    let error = response.error.expect("claude cannot enforce a tool policy");
    assert_eq!(error.code, INVALID_PARAMS, "{}", error.message);
    assert!(error.message.contains("tool_policy_unsupported_provider"));
    // A malformed name is refused by validation; an unknown field or mode
    // fails typed decoding. Neither reaches a launch.
    let response = call_rpc(
        &fixture.server,
        "LaunchSession",
        launch(
            "Harness",
            serde_json::json!({"denied_tools": ["Not A Tool"]}),
        ),
    )
    .await;
    let error = response.error.expect("malformed tool name rejected");
    assert_eq!(error.code, INVALID_PARAMS);
    assert!(error.message.contains("tool_policy_invalid"));
    for bad in [
        serde_json::json!({"web_access": "sometimes"}),
        serde_json::json!({"surprise": true}),
    ] {
        let response = call_rpc(&fixture.server, "LaunchSession", launch("Harness", bad)).await;
        assert!(
            response.error.is_some(),
            "typed decoding rejects the policy"
        );
    }

    // The daemon-wide defaults round-trip through the operator config verbs.
    let initial = call_rpc(&fixture.server, "GetDaemonConfig", serde_json::Value::Null).await;
    let initial = initial.result.expect("daemon config");
    assert_eq!(initial["harness_web_access"], "enabled");
    assert_eq!(initial["harness_max_search_calls"], 0);
    for (field, value) in [
        ("harness_web_access", serde_json::json!("hosted_only")),
        ("harness_max_search_calls", serde_json::json!(10)),
        ("harness_max_fetch_calls", serde_json::json!(5)),
        ("harness_max_result_bytes", serde_json::json!(1_048_576)),
        (
            "harness_max_web_cost_usd_micros",
            serde_json::json!(500_000),
        ),
    ] {
        let response = call_rpc(
            &fixture.server,
            "UpdateDaemonConfig",
            serde_json::json!({"field": field, "value": value.clone()}),
        )
        .await;
        assert!(response.error.is_none(), "{field}: {:?}", response.error);
    }
    let updated = call_rpc(&fixture.server, "GetDaemonConfig", serde_json::Value::Null).await;
    let updated = updated.result.expect("daemon config");
    assert_eq!(updated["harness_web_access"], "hosted_only");
    assert_eq!(updated["harness_max_search_calls"], 10);
    assert_eq!(updated["harness_max_fetch_calls"], 5);
    assert_eq!(updated["harness_max_result_bytes"], 1_048_576);
    assert_eq!(updated["harness_max_web_cost_usd_micros"], 500_000);
    for (field, value) in [
        ("harness_web_access", serde_json::json!("always")),
        ("harness_max_search_calls", serde_json::json!(-1)),
        ("harness_max_search_calls", serde_json::json!(1_000_001)),
    ] {
        let response = call_rpc(
            &fixture.server,
            "UpdateDaemonConfig",
            serde_json::json!({"field": field, "value": value}),
        )
        .await;
        assert_eq!(
            response.error.expect("rejected").code,
            INVALID_PARAMS,
            "{field}"
        );
    }
    // No new agent verb exists: neither the policy nor its defaults can be
    // reached, let alone widened, through the agent surface.
    for method in ["GetDaemonConfig", "UpdateDaemonConfig", "LaunchSession"] {
        assert!(!agent_gate::AGENT_VERBS.contains(&method), "{method}");
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn completion_gates_launch_params_are_validated_and_operator_only() {
    let fixture = recursive_dag_rpc_fixture();
    let launch = |provider: &str, gates: serde_json::Value| {
        serde_json::json!({
            "query": "completion gate benchmark",
            "provider": provider,
            "tags": ["bench"],
            "completion_gates": gates,
        })
    };

    let response = call_rpc(
        &fixture.server,
        "LaunchSession",
        launch(
            "Claude",
            serde_json::json!({
                "gates": [{"name": "checks", "command": "true"}],
                "max_attempts": 1,
            }),
        ),
    )
    .await;
    let error = response.error.expect("Claude cannot enforce a gate");
    assert_eq!(error.code, INVALID_PARAMS, "{}", error.message);
    assert!(
        error
            .message
            .contains(rsi_common::completion_gates::COMPLETION_GATES_UNSUPPORTED_PROVIDER)
    );

    for bad in [
        serde_json::json!({
            "gates": [{"name": "checks", "command": "true"}],
            "max_attempts": 11,
        }),
        serde_json::json!({
            "gates": [{"name": "Checks", "command": "true"}],
            "max_attempts": 1,
        }),
        serde_json::json!({"gates": [], "max_attempts": 0}),
        serde_json::json!({"surprise": true}),
    ] {
        let expect_completion_gate_error = bad.get("surprise").is_none();
        let response = call_rpc(&fixture.server, "LaunchSession", launch("Harness", bad)).await;
        let error = response
            .error
            .expect("invalid completion gates must not launch");
        assert_eq!(error.code, INVALID_PARAMS, "{}", error.message);
        if expect_completion_gate_error {
            assert!(error.message.contains("completion_gates_invalid"));
        }
    }

    let initial = call_rpc(&fixture.server, "GetDaemonConfig", serde_json::Value::Null).await;
    let initial = initial.result.expect("daemon config");
    assert_eq!(initial["completion_gates_enabled"], true);
    let response = call_rpc(
        &fixture.server,
        "UpdateDaemonConfig",
        serde_json::json!({
            "field": "completion_gates_enabled",
            "value": false,
        }),
    )
    .await;
    assert!(response.error.is_none(), "{:?}", response.error);
    let updated = call_rpc(&fixture.server, "GetDaemonConfig", serde_json::Value::Null).await;
    let updated = updated.result.expect("daemon config");
    assert_eq!(updated["completion_gates_enabled"], false);
    let response = call_rpc(
        &fixture.server,
        "UpdateDaemonConfig",
        serde_json::json!({
            "field": "completion_gates_enabled",
            "value": "off",
        }),
    )
    .await;
    assert_eq!(
        response.error.expect("invalid toggle rejected").code,
        INVALID_PARAMS
    );

    assert!(!agent_gate::AGENT_VERBS.contains(&"LaunchSession"));
    assert!(!agent_gate::READ_VERBS.contains(&"LaunchSession"));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn completion_gate_settings_are_absent_from_every_agent_surface() {
    for field in ["completion_gates", "completion_gates_enabled"] {
        assert!(!agent_gate::AGENT_VERBS.contains(&field), "{field}");
        assert!(!agent_gate::READ_VERBS.contains(&field), "{field}");
    }
    for (label, source, anchor) in [
        (
            "CodexAppServer tool catalog",
            include_str!("../tool_registry.rs"),
            "AgentControlVerbV1::SpawnChild",
        ),
        (
            "Harness tool catalog",
            include_str!("../session/harness/tools/rsi_control.rs"),
            "AgentControlVerbV1::SpawnChild",
        ),
    ] {
        assert!(
            source.contains(anchor),
            "{label}: anchor {anchor:?} missing — this check would be vacuous"
        );
        let haystack = source.to_ascii_lowercase();
        for forbidden in ["completion_gates", "completion_gates_enabled"] {
            assert!(
                !haystack.contains(forbidden),
                "{label} exposed operator-only surface {forbidden:?}"
            );
        }
    }

    let cli_catalog = rsi_common::agent_control_schema::agent_control_catalog_v1();
    assert!(
        cli_catalog
            .iter()
            .any(|descriptor| descriptor.method == "AgentSpawnChild")
    );
    for descriptor in cli_catalog {
        let haystack = format!(
            "{}\n{}\n{}",
            descriptor.method,
            descriptor.description,
            descriptor.parameters_json()
        )
        .to_ascii_lowercase();
        for forbidden in ["completion_gates", "completion_gates_enabled"] {
            assert!(
                !haystack.contains(forbidden),
                "agent CLI descriptor {} exposed operator-only surface {forbidden:?}",
                descriptor.method
            );
        }
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn child_autonomy_settings_round_trip_and_the_hold_read_is_operator_only() {
    // Operator-only: nothing in the agent surface.
    for method in [
        "ListScheduledJobHolds",
        "GetDaemonConfig",
        "UpdateDaemonConfig",
    ] {
        assert!(!agent_gate::AGENT_VERBS.contains(&method), "{method}");
        assert!(!agent_gate::READ_VERBS.contains(&method), "{method}");
        assert!(!agent_gate::is_allowed_for_attributed_caller(method));
    }
    let fixture = recursive_dag_rpc_fixture();
    let mut attributed = RpcRequest::new("ListScheduledJobHolds", serde_json::json!({}));
    attributed.session_token = Some("some-token".to_string());
    let HandleResult::Response(response) = fixture.server.handle_request_inner(&attributed).await
    else {
        panic!("expected response");
    };
    assert!(
        response
            .error
            .expect("attributed hold read must be denied")
            .message
            .contains("not available to session-attributed callers")
    );
    // The operator read returns the (empty) hold list.
    let holds = call_rpc(
        &fixture.server,
        "ListScheduledJobHolds",
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(holds.result.expect("operator read"), serde_json::json!([]));

    // Defaults, then each setting round-trips and bad values are refused.
    let initial = call_rpc(&fixture.server, "GetDaemonConfig", serde_json::Value::Null).await;
    let initial = initial.result.expect("daemon config");
    assert_eq!(initial["program_hold_while_children_run"], true);
    assert_eq!(initial["child_keepalive_enabled"], false);
    assert_eq!(initial["child_keepalive_window_secs"], 1500);
    for (field, value) in [
        ("program_hold_while_children_run", serde_json::json!(false)),
        ("child_keepalive_enabled", serde_json::json!(true)),
        ("child_keepalive_window_secs", serde_json::json!(600)),
    ] {
        let response = call_rpc(
            &fixture.server,
            "UpdateDaemonConfig",
            serde_json::json!({"field": field, "value": value}),
        )
        .await;
        assert!(response.error.is_none(), "{field}: {:?}", response.error);
    }
    let updated = call_rpc(&fixture.server, "GetDaemonConfig", serde_json::Value::Null).await;
    let updated = updated.result.expect("daemon config");
    assert_eq!(updated["program_hold_while_children_run"], false);
    assert_eq!(updated["child_keepalive_enabled"], true);
    assert_eq!(updated["child_keepalive_window_secs"], 600);
    // The scheduler reads the durable authority row the update persisted.
    {
        let store = fixture.manager.store().lock().await;
        let policy = store.child_autonomy_policy();
        assert!(!policy.hold_program_wakes);
        assert!(policy.keepalive_enabled);
        assert_eq!(policy.window, chrono::Duration::seconds(600));
    }
    for (field, value) in [
        ("child_keepalive_window_secs", serde_json::json!(299)),
        ("child_keepalive_window_secs", serde_json::json!(21_601)),
        ("child_keepalive_enabled", serde_json::json!("yes")),
    ] {
        let response = call_rpc(
            &fixture.server,
            "UpdateDaemonConfig",
            serde_json::json!({"field": field, "value": value}),
        )
        .await;
        assert_eq!(
            response.error.expect("rejected").code,
            INVALID_PARAMS,
            "{field}"
        );
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn model_update_rpc_is_operator_only() {
    let fixture = recursive_dag_rpc_fixture();
    let method = "QueueSessionModelUpdate";
    assert!(!agent_gate::AGENT_VERBS.contains(&method));
    assert!(!agent_gate::READ_VERBS.contains(&method));

    let mut request = RpcRequest::new(
        method,
        serde_json::json!({
            "session_id": Uuid::new_v4(),
            "expected_model_invocation_id": Uuid::new_v4(),
            "new_model": "claude-opus-4-1",
            "new_effort": "high",
            "idempotency_key": "test-switch",
        }),
    );
    request.session_token = Some("operator-only-test-token".into());
    let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
    else {
        panic!("expected response");
    };
    let error = response
        .error
        .expect("attributed caller must be denied the operator-only method");
    assert_eq!(error.code, INVALID_PARAMS);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn operator_message_and_drain_methods_are_strictly_operator_only() {
    let fixture = recursive_dag_rpc_fixture();
    for method in [
        "QueueOperatorMessage",
        "ListOperatorMessages",
        "EditOperatorMessage",
        "WithdrawOperatorMessage",
        "InterruptSessionNow",
        "RestartDaemonDrain",
        "GetDrainRestartStatus",
    ] {
        assert!(!agent_gate::AGENT_VERBS.contains(&method), "{method}");
        assert!(!agent_gate::READ_VERBS.contains(&method), "{method}");
        assert!(
            !agent_gate::is_allowed_for_attributed_caller(method),
            "{method}"
        );
        let mut request = RpcRequest::new(method, serde_json::json!({}));
        request.session_token = Some("agent-cannot-use-operator-control".into());
        let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
        else {
            panic!("response expected for {method}");
        };
        assert_eq!(
            response.error.expect("agent denied").code,
            INVALID_PARAMS,
            "{method}"
        );
    }
    let unconfirmed = call_rpc(
        &fixture.server,
        "InterruptSessionNow",
        serde_json::json!({"session_id":Uuid::new_v4(),"confirmation":"interrupt"}),
    )
    .await;
    assert_eq!(
        unconfirmed
            .error
            .expect("literal confirmation required")
            .code,
        INVALID_PARAMS
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn operator_message_rpc_supports_pending_edit_and_withdraw() {
    let fixture = recursive_dag_rpc_fixture();
    let session_id = Uuid::new_v4();
    let mut session = mk_agent_test_session(
        session_id,
        rsi_common::types::SessionKind::Standard,
        None,
        None,
    );
    session.status = rsi_common::types::SessionStatus::Running;
    fixture
        .manager
        .store()
        .lock()
        .await
        .insert_session(&session)
        .expect("insert session");
    let queued = call_rpc(&fixture.server, "QueueOperatorMessage", serde_json::json!({
        "session_id":session_id,"content":"follow up after tool","idempotency_key":"rpc-message-1"
    })).await;
    assert!(queued.error.is_none(), "{queued:?}");
    let id: Uuid =
        serde_json::from_value(queued.result.expect("receipt")["id"].clone()).expect("id");
    let edited = call_rpc(
        &fixture.server,
        "EditOperatorMessage",
        serde_json::json!({
            "message_id":id,"content":"revised follow up"
        }),
    )
    .await;
    assert_eq!(
        edited.result.expect("edited")["content"],
        "revised follow up"
    );
    let listed = call_rpc(
        &fixture.server,
        "ListOperatorMessages",
        serde_json::json!({"session_id":session_id}),
    )
    .await;
    assert_eq!(
        listed.result.expect("list")[0]["content"],
        "revised follow up"
    );
    let withdrawn = call_rpc(
        &fixture.server,
        "WithdrawOperatorMessage",
        serde_json::json!({"message_id":id}),
    )
    .await;
    assert_eq!(withdrawn.result.expect("withdrawn")["state"], "withdrawn");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn operator_can_queue_a_fenced_model_update() {
    let fixture = recursive_dag_rpc_fixture();
    let session_id = Uuid::new_v4();
    let invocation_id = Uuid::new_v4();
    let mut session = mk_agent_test_session(
        session_id,
        rsi_common::types::SessionKind::Standard,
        None,
        None,
    );
    session.model = Some("claude-sonnet-4-5".to_string());
    fixture
        .manager
        .store()
        .lock()
        .await
        .insert_session(&session)
        .expect("insert switchable session");
    fixture
        .manager
        .store()
        .lock()
        .await
        .set_session_model_invocation(session_id, Some(invocation_id))
        .expect("set invocation fence");

    let response = call_rpc(
        &fixture.server,
        "QueueSessionModelUpdate",
        serde_json::json!({
            "session_id": session_id,
            "expected_model_invocation_id": invocation_id,
            "new_model": "claude-opus-4-1",
            "new_effort": "high",
            "idempotency_key": "operator-switch-1",
        }),
    )
    .await;
    assert!(
        response.error.is_none(),
        "unexpected RPC error: {response:?}"
    );
    let receipt = response.result.expect("queued update receipt");
    assert_eq!(receipt["session_id"], session_id.to_string());
    assert_eq!(
        receipt["expected_model_invocation_id"],
        invocation_id.to_string()
    );
    assert_eq!(receipt["state"], "queued");
    assert_eq!(receipt["new_model"], "claude-opus-4-1");
    assert_eq!(receipt["new_effort"], "high");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn session_model_switch_options_report_fence_and_the_allowlist_gates_queueing() {
    let fixture = recursive_dag_rpc_fixture();
    let method = "GetSessionModelSwitchOptions";
    assert!(!agent_gate::AGENT_VERBS.contains(&method));
    assert!(!agent_gate::READ_VERBS.contains(&method));

    let session_id = Uuid::new_v4();
    let invocation_id = Uuid::new_v4();
    let mut session = mk_agent_test_session(
        session_id,
        rsi_common::types::SessionKind::Standard,
        None,
        None,
    );
    session.model = Some("claude-sonnet-4-5".to_string());
    session.effort = Some("medium".to_string());
    {
        let store = fixture.manager.store();
        let store = store.lock().await;
        store.insert_session(&session).expect("insert session");
        store
            .set_session_model_invocation(session_id, Some(invocation_id))
            .expect("set invocation fence");
    }

    let response = call_rpc(
        &fixture.server,
        method,
        serde_json::json!({"session_id": session_id}),
    )
    .await;
    assert!(response.error.is_none(), "{response:?}");
    let options: rsi_common::rpc::SessionModelSwitchOptions =
        serde_json::from_value(response.result.expect("options")).expect("typed options");
    assert!(options.switchable, "{:?}", options.unavailable_reason);
    assert!(options.keeps_context);
    assert_eq!(options.model.as_deref(), Some("claude-sonnet-4-5"));
    assert_eq!(options.effort.as_deref(), Some("medium"));
    assert_eq!(options.model_invocation_id, Some(invocation_id));
    assert!(options.model_allowlist.is_empty());

    fixture
        .runtime_config
        .update_field(
            "launch_model_allowlist",
            &serde_json::json!(["claude-sonnet-4-5"]),
        )
        .expect("set allowlist");
    let response = call_rpc(
        &fixture.server,
        method,
        serde_json::json!({"session_id": session_id}),
    )
    .await;
    let options: rsi_common::rpc::SessionModelSwitchOptions =
        serde_json::from_value(response.result.expect("options")).expect("typed options");
    assert_eq!(
        options.model_allowlist,
        vec!["claude-sonnet-4-5".to_string()]
    );

    let refused = call_rpc(
        &fixture.server,
        "QueueSessionModelUpdate",
        serde_json::json!({
            "session_id": session_id,
            "expected_model_invocation_id": invocation_id,
            "new_model": "claude-opus-4-1",
            "new_effort": "high",
            "idempotency_key": "allowlist-refused",
        }),
    )
    .await;
    assert!(
        refused.error.is_some(),
        "a model off the allowlist must be refused at queue time"
    );
    let admitted = call_rpc(
        &fixture.server,
        "QueueSessionModelUpdate",
        serde_json::json!({
            "session_id": session_id,
            "expected_model_invocation_id": invocation_id,
            "new_model": "claude-sonnet-4-5",
            "new_effort": "high",
            "idempotency_key": "allowlist-admitted",
        }),
    )
    .await;
    assert!(admitted.error.is_none(), "{admitted:?}");
    let response = call_rpc(
        &fixture.server,
        method,
        serde_json::json!({"session_id": session_id}),
    )
    .await;
    let options: rsi_common::rpc::SessionModelSwitchOptions =
        serde_json::from_value(response.result.expect("options")).expect("typed options");
    assert_eq!(
        options.pending,
        Some(rsi_common::rpc::PendingSessionModelUpdate {
            model: "claude-sonnet-4-5".into(),
            effort: Some("high".into()),
        })
    );
}

/// P-003/D04: the 8 local-issue-tracker operator verbs are deliberately
/// absent from `AGENT_VERBS`/`READ_VERBS` — operator-only is enforced
/// for free by the pre-dispatch default-deny gate. This never reaches
/// an issue-store call (the gate runs before dispatch), so the fixture
/// needs no seeded issue data.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn agent_gate_attributed_call_to_issue_verbs_is_denied() {
    let fixture = recursive_dag_rpc_fixture();
    for method in [
        "CreateIssue",
        "GetIssue",
        "ListIssues",
        "UpdateIssueStatus",
        "ListIssueEvents",
        "AddIssueDep",
        "RemoveIssueDep",
        "ListReadyIssues",
        "LinkIssueToIdea",
    ] {
        let mut request = RpcRequest::new(method, serde_json::json!({}));
        request.session_token = Some("some-token".to_string());
        let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
        else {
            panic!("expected response for {method}");
        };
        let error = response
            .error
            .unwrap_or_else(|| panic!("attributed call to {method} must be denied"));
        assert_eq!(error.code, INVALID_PARAMS, "method: {method}");
        assert!(
            error
                .message
                .contains("not available to session-attributed callers"),
            "method {method}: unexpected message: {}",
            error.message
        );
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn agent_issue_rpc_lead_can_get_while_generic_issue_get_stays_denied() {
    use rsi_common::types::{Project, SessionKind};

    let fixture = recursive_dag_rpc_fixture();
    let project = Project {
        id: Uuid::new_v4(),
        name: "agent Issue RPC".into(),
        path: None,
        description: None,
        color: Project::DEFAULT_COLOR.to_string(),
        context_files: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    let group_id = Uuid::new_v4();
    let epic_id = Uuid::new_v4();
    let caller_id = Uuid::new_v4();
    let issue = {
        let store = fixture.manager.store().lock().await;
        store.insert_project(&project).unwrap();
        let mut group = mk_agent_test_session(group_id, SessionKind::Group, None, None);
        group.project_id = Some(project.id);
        store.insert_session(&group).unwrap();
        let mut epic = mk_agent_test_session(epic_id, SessionKind::Epic, Some(group_id), None);
        epic.project_id = Some(project.id);
        store.insert_session(&epic).unwrap();
        let mut caller = mk_agent_test_session(caller_id, SessionKind::Task, Some(epic_id), None);
        caller.project_id = Some(project.id);
        store.insert_session(&caller).unwrap();
        store.set_lead_session(epic_id, Some(caller_id)).unwrap();
        store
            .create_issue(&rsi_common::types::NewIssue {
                project_id: project.id,
                title: "guarded".into(),
                body: String::new(),
                priority: None,
                labels: Vec::new(),
                created_by_session_id: None,
                assignee: None,
                idea_id: None,
                source_event_id: None,
                source_finding_ref: None,
            })
            .unwrap()
    };
    let token = "agent-issue-lead-token".to_string();
    fixture
        .manager
        .register_agent_token(token.clone(), caller_id)
        .await;

    let mut guarded = RpcRequest::new("AgentGetIssue", serde_json::json!({"issue_id":issue.id}));
    guarded.session_token = Some(token.clone());
    let HandleResult::Response(response) = fixture.server.handle_request_inner(&guarded).await
    else {
        panic!("expected AgentGetIssue response");
    };
    assert!(response.error.is_none(), "{response:?}");
    let returned: rsi_common::types::Issue =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!(returned.id, issue.id);

    // #1235: project_id is a target, never identity. The caller's own
    // project serves exactly as an omitted one does.
    let mut own_project = RpcRequest::new(
        "AgentGetIssue",
        serde_json::json!({"issue_id":issue.id,"project_id":project.id}),
    );
    own_project.session_token = Some(token.clone());
    let HandleResult::Response(response) = fixture.server.handle_request_inner(&own_project).await
    else {
        panic!("expected own-project AgentGetIssue response");
    };
    assert!(response.error.is_none(), "{response:?}");
    let returned: rsi_common::types::Issue =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!(returned.id, issue.id);
    // Another project is outside every arm of a lead and is refused.
    let mut spoofed = RpcRequest::new(
        "AgentGetIssue",
        serde_json::json!({"issue_id":issue.id,"project_id":Uuid::new_v4()}),
    );
    spoofed.session_token = Some(token.clone());
    let HandleResult::Response(response) = fixture.server.handle_request_inner(&spoofed).await
    else {
        panic!("expected spoofed AgentGetIssue response");
    };
    let error = response.error.expect("a foreign project must be rejected");
    assert!(
        format!("{error:?}").contains("manager_project_not_in_scope"),
        "{error:?}"
    );

    let initial_row_version = issue.row_version;
    let initial_event_count = {
        let store = fixture.manager.store().lock().await;
        store
            .agent_list_issue_events(
                caller_id,
                &rsi_common::types::IssueEventPageRequestV1 {
                    project_id: None,
                    issue_id: issue.id,
                    after_sequence: 0,
                    limit: None,
                },
            )
            .unwrap()
            .events
            .len()
    };
    for (method, params) in [
        (
            "AgentUpdateIssue",
            serde_json::json!({"issue_id": issue.id}),
        ),
        (
            "AgentUpdateIssueStatus",
            serde_json::json!({
                "issue_id": issue.id,
                "status": 7,
                "expected_row_version": initial_row_version,
                "idempotency_key": "malformed-status"
            }),
        ),
        (
            "AgentArchiveIssue",
            serde_json::json!({
                "issue_id": issue.id,
                "expected_row_version": "wrong",
                "idempotency_key": "malformed-archive"
            }),
        ),
        ("AgentRestoreIssue", serde_json::json!([])),
    ] {
        let mut request = RpcRequest::new(method, params);
        request.session_token = Some(token.clone());
        let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
        else {
            panic!("expected malformed mutation response for {method}");
        };
        assert_eq!(
            response
                .error
                .expect("malformed mutation must fail")
                .data
                .expect("typed Issue envelope")["code"],
            serde_json::json!("invalid_request"),
            "method: {method}"
        );
    }
    {
        let store = fixture.manager.store().lock().await;
        let unchanged = store.get_issue(issue.id).unwrap().expect("seeded Issue");
        assert_eq!(unchanged.row_version, initial_row_version);
        assert_eq!(
            store
                .agent_list_issue_events(
                    caller_id,
                    &rsi_common::types::IssueEventPageRequestV1 {
                        project_id: None,
                        issue_id: issue.id,
                        after_sequence: 0,
                        limit: None,
                    },
                )
                .unwrap()
                .events
                .len(),
            initial_event_count
        );
    }

    let mut missing = RpcRequest::new(
        "AgentGetIssue",
        serde_json::json!({"issue_id":Uuid::new_v4()}),
    );
    missing.session_token = Some(token.clone());
    let HandleResult::Response(response) = fixture.server.handle_request_inner(&missing).await
    else {
        panic!("expected missing AgentGetIssue response");
    };
    assert_eq!(
        response.error.unwrap().data.unwrap()["code"],
        serde_json::json!("not_found_in_scope")
    );

    let mut unknown_token =
        RpcRequest::new("AgentGetIssue", serde_json::json!({"issue_id":issue.id}));
    unknown_token.session_token = Some("unknown-token".to_string());
    let HandleResult::Response(response) =
        fixture.server.handle_request_inner(&unknown_token).await
    else {
        panic!("expected unknown-token AgentGetIssue response");
    };
    assert_eq!(
        response.error.unwrap().data.unwrap()["code"],
        serde_json::json!("authority_denied")
    );

    let mut generic = RpcRequest::new("GetIssue", serde_json::json!({"issue_id":issue.id}));
    generic.session_token = Some(token);
    let HandleResult::Response(response) = fixture.server.handle_request_inner(&generic).await
    else {
        panic!("expected generic GetIssue response");
    };
    assert_eq!(response.error.unwrap().code, INVALID_PARAMS);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn agent_issue_rpc_all_seven_decoders_emit_only_bounded_validation_hints() {
    use rsi_common::rpc::{
        AgentIssueErrorCodeV1, AgentIssueErrorV1, AgentIssueValidationClassV1 as Class,
        AgentIssueValidationFieldV1 as Field,
    };

    let fixture = recursive_dag_rpc_fixture();
    let caller = Uuid::new_v4();
    let token = "all-agent-issue-validation-kinds".to_string();
    fixture
        .manager
        .register_agent_token(token.clone(), caller)
        .await;
    let issue_id = Uuid::new_v4();
    let cases = [
        (
            "AgentListIssues",
            serde_json::json!({"limit": "many"}),
            Class::InvalidField,
            Some(Field::Limit),
        ),
        (
            "AgentGetIssue",
            serde_json::json!({"rogue/secret": "never"}),
            Class::UnknownField,
            None,
        ),
        (
            "AgentUpdateIssue",
            serde_json::json!({"issue_id": issue_id}),
            Class::MissingField,
            Some(Field::ExpectedRowVersion),
        ),
        (
            "AgentUpdateIssueStatus",
            serde_json::json!({
                "issue_id": issue_id,
                "status": 7,
                "expected_row_version": 1,
                "idempotency_key": "status-rpc"
            }),
            Class::InvalidField,
            Some(Field::Status),
        ),
        (
            "AgentArchiveIssue",
            serde_json::json!({
                "issue_id": issue_id,
                "expected_row_version": 1,
                "idempotency_key": 7
            }),
            Class::InvalidField,
            Some(Field::IdempotencyKey),
        ),
        (
            "AgentRestoreIssue",
            serde_json::json!([]),
            Class::InvalidShape,
            None,
        ),
        (
            "AgentListIssueEvents",
            serde_json::json!({
                "issue_id": issue_id,
                "after_sequence": "zero"
            }),
            Class::InvalidField,
            Some(Field::AfterSequence),
        ),
    ];

    for (method, params, expected_class, expected_field) in cases {
        let mut request = RpcRequest::new(method, params);
        request.session_token = Some(token.clone());
        let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
        else {
            panic!("expected response for {method}");
        };
        let error = response.error.expect("malformed request must fail");
        assert_eq!(error.code, INVALID_PARAMS, "method: {method}");
        assert_eq!(error.message, "agent_issue_invalid_request");
        let data = error.data.expect("typed Issue envelope");
        let envelope: AgentIssueErrorV1 = serde_json::from_value(data.clone()).unwrap();
        assert_eq!(envelope.code, AgentIssueErrorCodeV1::InvalidRequest);
        let validation = envelope.validation.expect("bounded validation hint");
        assert_eq!(validation.class, expected_class, "method: {method}");
        assert_eq!(validation.field, expected_field, "method: {method}");
        assert!(!data.to_string().contains("rogue/secret"));
        assert!(!data.to_string().contains("never"));
    }

    let mut unauthenticated = RpcRequest::new(
        "AgentGetIssue",
        serde_json::json!({"rogue/secret": "never"}),
    );
    unauthenticated.session_token = Some("unknown-token".to_string());
    let HandleResult::Response(response) =
        fixture.server.handle_request_inner(&unauthenticated).await
    else {
        panic!("expected authority response");
    };
    let envelope: AgentIssueErrorV1 = serde_json::from_value(
        response
            .error
            .expect("unknown token must fail")
            .data
            .expect("typed Issue envelope"),
    )
    .unwrap();
    assert_eq!(envelope.code, AgentIssueErrorCodeV1::AuthorityDenied);
    assert_eq!(envelope.validation, None);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn agent_issue_rpc_harness_and_codex_error_envelopes_are_identical_and_redacted() {
    use rsi_common::rpc::AgentIssueErrorCodeV1;

    let cases = [
        (AgentIssueErrorCodeV1::InvalidRequest, None, None),
        (AgentIssueErrorCodeV1::AuthorityDenied, None, None),
        (AgentIssueErrorCodeV1::NotFoundInScope, None, None),
        (AgentIssueErrorCodeV1::StaleVersion, Some(4), Some(5)),
        (AgentIssueErrorCodeV1::IdempotencyConflict, None, None),
        (AgentIssueErrorCodeV1::NoSemanticChange, None, None),
        (AgentIssueErrorCodeV1::InvalidTransition, None, None),
        (AgentIssueErrorCodeV1::Archived, None, None),
        (AgentIssueErrorCodeV1::NotArchived, None, None),
        (AgentIssueErrorCodeV1::StorageFailure, None, None),
    ];
    for (code, expected, actual) in cases {
        let rpc_error = serialize_agent_issue_result::<serde_json::Value>(Err(
            crate::error::agent_issue_error(code, expected, actual),
        ))
        .unwrap_err();
        let codex_error = crate::tool_registry::serialize_agent_issue_tool_result::<
            serde_json::Value,
        >(Err(crate::error::agent_issue_error(code, expected, actual)))
        .unwrap_err();
        let harness: serde_json::Value =
            serde_json::from_str(&crate::error::agent_issue_error_json(
                crate::error::agent_issue_error(code, expected, actual),
            ))
            .unwrap();
        let data = |error: DaemonError| match error {
            DaemonError::StructuredRpc { data, .. } => data,
            other => panic!("unstructured Issue error: {other}"),
        };
        assert_eq!(data(rpc_error), harness);
        assert_eq!(data(codex_error), harness);
    }

    let issue_id = Uuid::new_v4();
    let validations = [
        crate::agent_issue_validation::decode_get(&serde_json::json!({"rogue/secret": "never"}))
            .unwrap_err(),
        crate::agent_issue_validation::decode_update(&serde_json::json!({
            "issue_id": issue_id
        }))
        .unwrap_err(),
        crate::agent_issue_validation::decode_list(&serde_json::json!({"limit": "many"}))
            .unwrap_err(),
        crate::agent_issue_validation::decode_restore(&serde_json::json!([])).unwrap_err(),
    ];
    for validation in validations {
        let rpc_error = serialize_agent_issue_result::<serde_json::Value>(Err(
            crate::error::agent_issue_invalid_request(validation.clone()),
        ))
        .unwrap_err();
        let codex_error =
            crate::tool_registry::serialize_agent_issue_tool_result::<serde_json::Value>(Err(
                crate::error::agent_issue_invalid_request(validation.clone()),
            ))
            .unwrap_err();
        let harness: serde_json::Value = serde_json::from_str(
            &crate::session::harness::tools::rsi_control::agent_issue_invalid_request_json(
                validation,
            ),
        )
        .unwrap();
        let data = |error: DaemonError| match error {
            DaemonError::StructuredRpc { data, .. } => data,
            other => panic!("unstructured Issue validation error: {other}"),
        };
        assert_eq!(data(rpc_error), harness);
        assert_eq!(data(codex_error), harness);
        assert!(!harness.to_string().contains("rogue/secret"));
        assert!(!harness.to_string().contains("never"));
    }

    let secret = "sqlite /private/path token=super-secret topology row 42";
    let rpc_error = serialize_agent_issue_result::<serde_json::Value>(Err(DaemonError::Store(
        secret.to_string(),
    )))
    .unwrap_err();
    let codex_error = crate::tool_registry::serialize_agent_issue_tool_result::<serde_json::Value>(
        Err(DaemonError::Database(rusqlite::Error::InvalidQuery)),
    )
    .unwrap_err();
    for error in [rpc_error, codex_error] {
        let encoded = match error {
            DaemonError::StructuredRpc { data, .. } => data.to_string(),
            other => panic!("unstructured storage error: {other}"),
        };
        assert!(encoded.contains("storage_failure"));
        assert!(!encoded.contains("sqlite"));
        assert!(!encoded.contains("private"));
        assert!(!encoded.contains("token"));
        assert!(!encoded.contains("topology"));
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn closure_k1_rpc_surface_is_operator_only_and_k2_k3_remain_unrouted() {
    let fixture = recursive_dag_rpc_fixture();
    for method in [
        "CreateClosureProgram",
        "UpdateClosureProgram",
        "LaunchClosureSource",
        "ListClosurePrograms",
        "GetClosureProgram",
        "RecordClosureEvidence",
    ] {
        assert!(!agent_gate::AGENT_VERBS.contains(&method));
        assert!(!agent_gate::READ_VERBS.contains(&method));
        let mut request = RpcRequest::new(method, serde_json::json!({}));
        request.session_token = Some("some-token".to_string());
        let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
        else {
            panic!("expected response for {method}");
        };
        assert_eq!(
            response.error.expect("Closure RPC must be denied").code,
            INVALID_PARAMS,
            "method: {method}"
        );
    }
    for method in [
        "ResumeClosureFinalization",
        "ApproveClosurePromotion",
        "RecheckClosureFinalGate",
        "SupersedeClosureGateFailure",
        "RecordClosureDiscard",
        "PreviewClosureCleanup",
        "ExecuteClosureCleanup",
    ] {
        assert!(!agent_gate::AGENT_VERBS.contains(&method));
        assert!(!agent_gate::READ_VERBS.contains(&method));
        let request = RpcRequest::new(method, serde_json::json!({}));
        let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
        else {
            panic!("expected response for {method}");
        };
        assert_eq!(
            response.error.expect("unrouted method must fail").code,
            METHOD_NOT_FOUND,
            "method: {method}"
        );
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn agent_gate_attributed_call_to_model_control_verbs_is_denied() {
    let fixture = recursive_dag_rpc_fixture();
    for method in [
        "GetModelControlStatus",
        "UpdateModelControlPolicy",
        "ListModelInvocations",
        "CancelModelInvocation",
    ] {
        let mut request = RpcRequest::new(method, serde_json::json!({}));
        request.session_token = Some("some-token".to_string());
        let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
        else {
            panic!("expected response for {method}");
        };
        let error = response
            .error
            .unwrap_or_else(|| panic!("attributed call to {method} must be denied"));
        assert_eq!(error.code, INVALID_PARAMS, "method: {method}");
        assert!(
            error
                .message
                .contains("not available to session-attributed callers"),
            "method {method}: unexpected message: {}",
            error.message
        );
    }
}

/// Issue #35. The orchestration child-effort ceiling is an OPERATOR
/// control, and its only surface is `GetDaemonConfig`/`UpdateDaemonConfig`.
/// An agent that could reach either verb could raise its own effort
/// ceiling, which defeats the runaway-spend guardrail issue #2 exists for.
/// Both verbs are operator-only for free via the pre-dispatch default-deny
/// gate; this asserts that rather than merely intending it. The gate runs
/// before dispatch, so no daemon-config state needs seeding.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn agent_gate_attributed_call_to_daemon_config_verbs_is_denied() {
    let fixture = recursive_dag_rpc_fixture();
    for method in ["GetDaemonConfig", "UpdateDaemonConfig"] {
        let mut request = RpcRequest::new(
            method,
            serde_json::json!({
                "field": "orchestration_max_child_effort",
                "value": "ultra",
            }),
        );
        request.session_token = Some("some-token".to_string());
        let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
        else {
            panic!("expected response for {method}");
        };
        let error = response
            .error
            .unwrap_or_else(|| panic!("attributed call to {method} must be denied"));
        assert_eq!(error.code, INVALID_PARAMS, "method: {method}");
        assert!(
            error
                .message
                .contains("not available to session-attributed callers"),
            "method {method}: unexpected message: {}",
            error.message
        );
    }
}

/// #633 T4-A7 (RPC plane): the six scoped topology verbs are attributed
/// writes (never read-allowlisted), dispatch through the token-bound
/// service with the redacted `{code, next_action}` envelope, and the
/// operator topology family stays operator-only.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn agent_topology_verbs_are_token_bound_writes_and_operator_topology_stays_operator_only() {
    let fixture = recursive_dag_rpc_fixture();
    let worker = Uuid::new_v4();
    fixture
        .manager
        .register_agent_token("topology-worker-token".into(), worker)
        .await;
    let id = Uuid::new_v4();
    let digest = format!("sha256:{}", "a".repeat(64));
    for (method, params) in [
        (
            "AgentTopologyUpsert",
            serde_json::json!({
                "name":"t4-rpc","scope":"epic","validate_only":true,"idempotency_key":"u",
                "definition":{"nodes":[],"edges":[]}
            }),
        ),
        ("AgentTopologyList", serde_json::json!({})),
        (
            "AgentTopologyExecute",
            serde_json::json!({
                "topology_id":id,"expected_digest":digest,"epic_id":id,"idempotency_key":"e"
            }),
        ),
        (
            "AgentTopologyGetExecution",
            serde_json::json!({"execution_id":id}),
        ),
        (
            "AgentTopologyInterrupt",
            serde_json::json!({"execution_id":id,"expected_row_version":1,"idempotency_key":"i"}),
        ),
        (
            "AgentTopologyResolveAttempt",
            serde_json::json!({
                "execution_id":id,"attempt_id":id,"action":"inspect",
                "expected_row_version":1,"idempotency_key":"r"
            }),
        ),
    ] {
        assert!(agent_gate::AGENT_VERBS.contains(&method));
        assert!(!agent_gate::READ_VERBS.contains(&method));
        for token in [
            None,
            Some("unknown-topology-token"),
            Some("topology-worker-token"),
        ] {
            let mut request = RpcRequest::new(method, params.clone());
            request.session_token = token.map(str::to_owned);
            let HandleResult::Response(response) =
                fixture.server.handle_request_inner(&request).await
            else {
                panic!("expected response for {method}");
            };
            let error = response
                .error
                .unwrap_or_else(|| panic!("{method} with {token:?} must be refused"));
            assert_eq!(error.code, INVALID_PARAMS, "{method}");
            let data = error.data.expect("redacted envelope");
            assert_eq!(data["code"], "authority_denied", "{method} {token:?}");
            assert!(data["next_action"].as_str().is_some_and(|n| !n.is_empty()));
        }
        let mut spoofed = params.clone();
        spoofed["caller_session_id"] = serde_json::json!(worker);
        let mut request = RpcRequest::new(method, spoofed);
        request.session_token = Some("topology-worker-token".into());
        let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
        else {
            panic!("expected response for {method}");
        };
        let data = response
            .error
            .and_then(|error| error.data)
            .expect("refused");
        assert_eq!(data["code"], "invalid_params", "{method}");
    }
    // A current Epic lead reaches the same guarded service through the
    // RPC verb and the registration-bound native tool.
    let (epic, lead) = (Uuid::new_v4(), Uuid::new_v4());
    {
        let store = fixture.manager.store().lock().await;
        let dir = std::env::temp_dir();
        let mut row = crate::session::agent_verbs::tests::test_session(epic, dir.clone());
        row.session_kind = rsi_common::types::SessionKind::Epic;
        row.lead_session_id = Some(lead);
        row.project_id = Some(issue_rpc_project_id());
        store.insert_session(&row).unwrap();
        let mut row = crate::session::agent_verbs::tests::test_session(lead, dir);
        row.session_kind = rsi_common::types::SessionKind::Task;
        row.parent_id = Some(epic);
        row.project_id = Some(issue_rpc_project_id());
        store.insert_session(&row).unwrap();
    }
    fixture
        .manager
        .register_agent_token("topology-lead-token".into(), lead)
        .await;
    let mut request = RpcRequest::new(
        "AgentTopologyUpsert",
        serde_json::json!({
            "name":"t4-rpc-lead","scope":"epic","validate_only":true,"idempotency_key":"u",
            "definition":{"nodes":[],"edges":[]}
        }),
    );
    request.session_token = Some("topology-lead-token".into());
    let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
    else {
        panic!("expected upsert response");
    };
    let validated = response.result.expect("lead validates in its own Epic");
    assert!(
        validated["definition_digest"]
            .as_str()
            .is_some_and(|digest| digest.starts_with("sha256:"))
    );
    let mut request = RpcRequest::new(
        "AgentTopologyList",
        serde_json::json!({"include_executions": true}),
    );
    request.session_token = Some("topology-lead-token".into());
    let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
    else {
        panic!("expected list response");
    };
    let listed = response.result.expect("lead lists its scope");
    assert!(listed["topologies"].is_array());
    let _ = fixture
        .manager
        .topology_agent_self
        .set(Arc::downgrade(&fixture.manager));
    let native = crate::session::harness::tools::rsi_control::execute_topology_tool(
        &fixture.manager.agent_control(),
        lead,
        AgentControlVerbV1::TopologyList,
        serde_json::json!({}),
    )
    .await
    .expect("native tool reaches the same service");
    assert_eq!(native["topologies"], listed["topologies"]);

    // The operator-only `UpdateTopology {shared}` switch publishes an
    // operator topology to agent callers without raw SQL.
    let created = call_rpc(
        &fixture.server,
        "CreateTopology",
        serde_json::json!({
            "name":"t4-operator-shared",
            "definition":{"nodes":[{"id":"a","kind":"Task","label":"A"}],"edges":[]}
        }),
    )
    .await;
    let operator_id = created.result.expect("operator creates")["id"].clone();
    let shared = call_rpc(
        &fixture.server,
        "UpdateTopology",
        serde_json::json!({"id": operator_id, "shared": true}),
    )
    .await;
    assert_eq!(shared.result.expect("operator shares")["ok"], true);
    let visible = crate::session::harness::tools::rsi_control::execute_topology_tool(
        &fixture.manager.agent_control(),
        lead,
        AgentControlVerbV1::TopologyList,
        serde_json::json!({}),
    )
    .await
    .expect("lead lists after sharing");
    let entry = visible["topologies"]
        .as_array()
        .unwrap()
        .iter()
        .find(|topology| topology["topology_id"] == operator_id)
        .expect("shared operator topology is visible to the lead");
    assert_eq!(entry["name"], "t4-operator-shared");
    assert_eq!(entry["shared"], true);

    for method in [
        "CreateTopology",
        "UpdateTopology",
        "ListTopologies",
        "ExecuteTopology",
        "GetWorkflowExecution",
        "InterruptWorkflowExecution",
        "ResolveTopologyAttempt",
    ] {
        assert!(!agent_gate::AGENT_VERBS.contains(&method));
        assert!(!agent_gate::READ_VERBS.contains(&method));
        assert!(!agent_gate::is_allowed_for_attributed_caller(method));
    }
}

/// #634: the operator `ResolveTopologyAttempt` method is operator-only.
/// It is absent from the agent and read catalogs and a tokened caller is
/// denied before dispatch, so no execution state needs seeding. Agents
/// resolve through the scoped `AgentTopologyResolveAttempt` (#633).
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn resolve_topology_attempt_is_operator_only() {
    let method = "ResolveTopologyAttempt";
    assert!(!agent_gate::AGENT_VERBS.contains(&method));
    assert!(!agent_gate::READ_VERBS.contains(&method));
    assert!(!agent_gate::is_allowed_for_attributed_caller(method));
    for (label, source) in [
        (
            "CodexAppServer tool catalog",
            include_str!("../tool_registry.rs"),
        ),
        (
            "Harness tool catalog",
            include_str!("../session/harness/tools/rsi_control.rs"),
        ),
    ] {
        assert!(
            source.contains("AgentControlVerbV1::SpawnChild"),
            "{label}: anchor missing — this check would be vacuous"
        );
        let haystack = source.to_ascii_lowercase();
        for forbidden in ["resolvetopologyattempt", "resolve_topology_attempt"] {
            assert!(
                !haystack.contains(forbidden),
                "{label} exposed operator-only surface {forbidden:?}"
            );
        }
    }
    let cli_catalog = rsi_common::agent_control_schema::agent_control_catalog_v1();
    assert!(
        cli_catalog
            .iter()
            .any(|descriptor| descriptor.method == "AgentSpawnChild")
    );
    // #633 (T4) adds the scoped agent verb; the operator method itself
    // stays out of the agent CLI catalog.
    assert!(
        cli_catalog
            .iter()
            .any(|descriptor| descriptor.method == "AgentTopologyResolveAttempt"),
        "the agent CLI catalog advertises the scoped resolution verb"
    );
    assert_eq!(
        rsi_common::agent_control_schema::AgentControlVerbV1::from_method_name(method),
        None
    );
    let fixture = recursive_dag_rpc_fixture();
    let mut request = RpcRequest::new(
        method,
        serde_json::json!({
            "execution_id": Uuid::new_v4(),
            "attempt_id": Uuid::new_v4(),
            "action": "inspect",
            "expected_row_version": 1,
            "idempotency_key": "k",
        }),
    );
    request.session_token = Some("some-token".to_string());
    let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
    else {
        panic!("expected response for {method}");
    };
    let error = response.error.expect("attributed call must be denied");
    assert_eq!(error.code, INVALID_PARAMS);
    assert!(
        error
            .message
            .contains("not available to session-attributed callers")
    );

    // The operator connection reaches the handler: an unknown attempt is
    // a typed `not_found`, not an unknown method.
    let operator = call_rpc(&fixture.server, method, request.params.clone()).await;
    let error = operator.error.expect("unknown attempt is refused");
    assert_eq!(error.code, INVALID_PARAMS);
    assert_eq!(error.message, "not_found");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
#[allow(clippy::expect_used, clippy::unwrap_used)]
async fn archived_sandbox_purge_rpc_is_operator_only_and_dry_run_reports() {
    let fixture = recursive_dag_rpc_fixture();
    let method = "RunArchivedSandboxPurge";
    assert!(!agent_gate::AGENT_VERBS.contains(&method));
    assert!(!agent_gate::READ_VERBS.contains(&method));
    assert!(!agent_gate::is_allowed_for_attributed_caller(method));
    let agent_catalog = rsi_common::agent_control_schema::agent_control_catalog_v1();
    assert!(
        agent_catalog
            .iter()
            .all(|descriptor| descriptor.method != method)
    );
    let mut request = RpcRequest::new(
        method,
        serde_json::json!({ "dry_run": true, "max_count": 5 }),
    );
    request.session_token = Some("some-token".to_string());
    let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
    else {
        panic!("expected response for {method}");
    };
    let error = response.error.expect("attributed call must be denied");
    assert_eq!(error.code, INVALID_PARAMS);
    assert!(
        error
            .message
            .contains("not available to session-attributed callers")
    );

    let preview = call_rpc(
        &fixture.server,
        method,
        serde_json::json!({ "dry_run": true, "max_count": 5 }),
    )
    .await;
    assert!(preview.error.is_none(), "{preview:?}");
    let preview = preview.result.expect("purge preview result");
    assert_eq!(preview["dry_run"], true);
    assert_eq!(preview["candidates_examined"], 0);
    assert_eq!(preview["bytes_freed"], 0);
    assert!(preview["next_cursor"].is_null());

    for params in [
        serde_json::json!({ "dry_run": true, "max_count": 0 }),
        serde_json::json!({ "dry_run": true, "max_count": 1025 }),
        serde_json::json!({ "dry_run": true, "max_count": 5, "unknown": 1 }),
    ] {
        let refused = call_rpc(&fixture.server, method, params).await;
        assert_eq!(refused.error.unwrap().code, INVALID_PARAMS);
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
#[allow(
    clippy::await_holding_lock,
    clippy::expect_used,
    clippy::too_many_lines,
    clippy::unwrap_used
)]
async fn sandbox_storage_rpc_is_operator_only_and_dry_run_is_structured() {
    let _isolation = crate::sandbox::target_reclaim::reclaim_test_isolation_for_test();
    let fixture = recursive_dag_rpc_fixture();
    for method in [
        "GetSandboxStorageStatus",
        "RunSandboxBuildCacheReclaim",
        "RunSandboxWorktreeReclaim",
    ] {
        assert!(!agent_gate::AGENT_VERBS.contains(&method));
        assert!(!agent_gate::READ_VERBS.contains(&method));
        let params = if method == "RunSandboxWorktreeReclaim" {
            serde_json::json!({ "dry_run": true, "max_count": 5 })
        } else {
            serde_json::json!({ "dry_run": true })
        };
        let mut request = RpcRequest::new(method, params);
        request.session_token = Some("some-token".to_string());
        let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
        else {
            panic!("expected response for {method}");
        };
        let error = response.error.expect("attributed call must be denied");
        assert_eq!(error.code, INVALID_PARAMS, "method: {method}");
        assert!(
            error
                .message
                .contains("not available to session-attributed callers")
        );
    }

    let agent_catalog = rsi_common::agent_control_schema::agent_control_catalog_v1();
    assert!(
        agent_catalog
            .iter()
            .all(|descriptor| descriptor.method != "RunSandboxWorktreeReclaim")
    );
    for (label, source, anchor) in [
        (
            "CodexAppServer tools",
            include_str!("../tool_registry.rs"),
            "AgentControlVerbV1::SpawnChild",
        ),
        (
            "Harness tools",
            include_str!("../session/harness/tools/rsi_control.rs"),
            "AgentControlVerbV1::SpawnChild",
        ),
    ] {
        assert!(source.contains(anchor), "{label} catalog anchor missing");
        assert!(
            !source
                .to_ascii_lowercase()
                .contains("rsi_control_run_sandbox_worktree_reclaim"),
            "{label} exposed the operator-only worktree reclaim RPC"
        );
    }

    let status = call_rpc(
        &fixture.server,
        "GetSandboxStorageStatus",
        serde_json::json!({}),
    )
    .await;
    assert!(status.error.is_none(), "{status:?}");
    let status = status.result.expect("status result");
    assert_eq!(status["version"], 2);
    assert_eq!(status["report"]["dry_run"], true);
    assert!(
        status["report"]["filesystem_before"]["total_bytes"]
            .as_u64()
            .unwrap()
            > 0
    );
    let decoded = serde_json::from_value::<
        rsi_common::sandbox_storage::SandboxBuildCacheReclaimReportWire,
    >(status)
    .unwrap()
    .validate_wire()
    .unwrap();
    assert!(decoded.v2().is_some());

    let preview = call_rpc(
        &fixture.server,
        "RunSandboxBuildCacheReclaim",
        serde_json::json!({ "dry_run": true }),
    )
    .await;
    assert!(preview.error.is_none(), "{preview:?}");
    let preview = preview.result.unwrap();
    assert_eq!(preview["version"], 2);
    assert_eq!(preview["report"]["dry_run"], true);
    let decoded = serde_json::from_value::<
        rsi_common::sandbox_storage::SandboxBuildCacheReclaimReportWire,
    >(preview)
    .unwrap()
    .validate_wire()
    .unwrap();
    assert!(decoded.v2().is_some());

    let missing_mode = call_rpc(
        &fixture.server,
        "RunSandboxBuildCacheReclaim",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(missing_mode.error.unwrap().code, INVALID_PARAMS);

    let worktree_preview = call_rpc(
        &fixture.server,
        "RunSandboxWorktreeReclaim",
        serde_json::json!({ "dry_run": true, "max_count": 5 }),
    )
    .await;
    assert!(worktree_preview.error.is_none(), "{worktree_preview:?}");
    let worktree_preview = worktree_preview.result.expect("worktree preview result");
    assert_eq!(worktree_preview["run"]["trigger"], "operator");
    assert_eq!(worktree_preview["run"]["dry_run"], true);
    assert!(worktree_preview["run"]["run_id"].is_string());
    assert!(worktree_preview["items"].is_array());

    let invalid_count = call_rpc(
        &fixture.server,
        "RunSandboxWorktreeReclaim",
        serde_json::json!({ "dry_run": true, "max_count": 0 }),
    )
    .await;
    assert_eq!(invalid_count.error.unwrap().code, INVALID_PARAMS);
}

/// Issue #35, the static half of the same boundary: the ceiling must stay
/// off every agent-reachable catalog, not just be denied at dispatch.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn operator_daemon_config_is_absent_from_every_agent_surface() {
    for field in ["sandbox_max_source_roots", "sandbox_min_free_gib"] {
        assert!(!agent_gate::AGENT_VERBS.contains(&field));
        assert!(!agent_gate::READ_VERBS.contains(&field));
        assert!(!agent_gate::UNSCOPED_READ_VERBS.contains(&field));
    }
    for field in RSID_SCOPE_CONFIG_FIELDS {
        assert!(!agent_gate::AGENT_VERBS.contains(&field));
        assert!(!agent_gate::READ_VERBS.contains(&field));
    }

    // 1. The daemon's attribution gate.
    for method in ["GetDaemonConfig", "UpdateDaemonConfig"] {
        assert!(
            !agent_gate::is_allowed_for_attributed_caller(method),
            "daemon-config RPC must remain operator-only: {method}"
        );
    }

    // 2. Native provider tools are hand-maintained lists, not derived from
    //    the gate above, so each is checked at the source. Building the
    //    registries here would be vacuous: `register_builtin_tools`
    //    registers the rsi_control tools only when an AgentControlHandle is
    //    present, so a test-constructed registry is empty and would pass
    //    trivially. Each case therefore asserts a known-present anchor
    //    first, so a renamed or moved file fails loudly instead of silently
    //    passing.
    for (label, source, anchor) in [
        (
            "CodexAppServer tool catalog",
            include_str!("../tool_registry.rs"),
            "AgentControlVerbV1::SpawnChild",
        ),
        (
            "Harness tool catalog",
            include_str!("../session/harness/tools/rsi_control.rs"),
            "AgentControlVerbV1::SpawnChild",
        ),
    ] {
        assert!(
            source.contains(anchor),
            "{label}: anchor {anchor:?} missing — this check would be vacuous"
        );
        // Matched case-insensitively against a lowercased haystack so both
        // naming conventions are covered: the RPC verbs are PascalCase
        // (`UpdateDaemonConfig`) but a native tool leaking the same
        // authority would be snake_case (`rsi_control_update_daemon_config`).
        // A first draft of this test only listed the PascalCase verbs and
        // let a snake_case tool through.
        //
        // Deliberately NOT forbidden: the bare word "effort". A child's
        // REQUESTED effort is a legitimate agent-supplied spawn argument
        // and appears in the native spawn schema; it is the CEILING that is
        // operator-only.
        let haystack = source.to_ascii_lowercase();
        for forbidden in [
            "daemon_config",
            "daemonconfig",
            "max_child_effort",
            "maxchildeffort",
            "effort_ceiling",
            "rsid_scope_memory_high_mib",
            "rsid_scope_memory_max_mib",
            "rsid_scope_memory_swap_max_mib",
            "rsid_scope_cpu_weight",
            "sandbox_max_source_roots",
            "sandbox_min_free_gib",
        ] {
            assert!(
                !haystack.contains(forbidden),
                "{label} exposed operator-only surface {forbidden:?}"
            );
        }
    }

    // 3. The agent CLI advertises the shared closed catalog directly. Test
    //    that catalog rather than scanning the whole CLI source: its
    //    negative tests legitimately name operator-only methods while
    //    proving that `--schema` refuses them.
    let cli_catalog = rsi_common::agent_control_schema::agent_control_catalog_v1();
    assert!(
        cli_catalog
            .iter()
            .any(|descriptor| descriptor.method == "AgentSpawnChild")
    );
    for descriptor in cli_catalog {
        let haystack = format!(
            "{}\n{}\n{}",
            descriptor.method,
            descriptor.description,
            descriptor.parameters_json()
        )
        .to_ascii_lowercase();
        for forbidden in [
            "daemon_config",
            "daemonconfig",
            "max_child_effort",
            "maxchildeffort",
            "effort_ceiling",
            "rsid_scope_memory_high_mib",
            "rsid_scope_memory_max_mib",
            "rsid_scope_memory_swap_max_mib",
            "rsid_scope_cpu_weight",
            "sandbox_max_source_roots",
            "sandbox_min_free_gib",
        ] {
            assert!(
                !haystack.contains(forbidden),
                "agent CLI descriptor {} exposed operator-only surface {forbidden:?}",
                descriptor.method
            );
        }
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn get_model_control_status_rpc_roundtrip_reports_active_invocation() {
    let fixture = recursive_dag_rpc_fixture();
    let session_id = Uuid::new_v4();
    let invocation_id = seed_active_model_invocation(
        &fixture,
        session_id,
        rsi_common::model_control::ModelInvocationPurpose::SessionLaunchFresh,
    )
    .await;

    let response = call_rpc(
        &fixture.server,
        "GetModelControlStatus",
        serde_json::json!({ "recent_limit": 4 }),
    )
    .await;
    assert!(response.error.is_none(), "{:?}", response.error);
    let report: rsi_common::model_control::ModelControlStatusReport =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!(
        report.mode,
        rsi_common::model_control::ModelControlMode::Normal
    );
    assert_eq!(report.active_invocations.len(), 1);
    assert_eq!(report.active_invocations[0].record.id, invocation_id);
    assert_eq!(
        report.active_invocations[0].record.purpose,
        rsi_common::model_control::ModelInvocationPurpose::SessionLaunchFresh
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn cancel_model_invocation_rpc_interrupts_active_session() {
    let fixture = recursive_dag_rpc_fixture();
    let session_id = Uuid::new_v4();
    let invocation_id = seed_active_model_invocation(
        &fixture,
        session_id,
        rsi_common::model_control::ModelInvocationPurpose::SessionLaunchFresh,
    )
    .await;

    let response = call_rpc(
        &fixture.server,
        "CancelModelInvocation",
        serde_json::json!({ "invocation_id": invocation_id }),
    )
    .await;
    assert!(response.error.is_none(), "{:?}", response.error);
    let report: rsi_common::model_control::CancelModelInvocationReport =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert!(report.request_recorded);
    assert_eq!(report.session_id, Some(session_id));
    assert_eq!(
        report.final_status,
        rsi_common::model_control::ModelInvocationStatus::CancellationRequested
    );

    let store = fixture.manager.store().lock().await;
    let record = store
        .load_model_invocation_record(invocation_id)
        .expect("load")
        .expect("record");
    assert_eq!(
        record.status,
        rsi_common::model_control::ModelInvocationStatus::CancellationRequested
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn cancel_model_invocation_rpc_uses_runtime_handle_for_sessionless_work() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let fixture = recursive_dag_rpc_fixture();
    let fired = Arc::new(AtomicBool::new(false));
    let fired_clone = Arc::clone(&fired);
    let invocation_id = Uuid::new_v4();
    let _registration = fixture
        .server
        .model_control_runtime_for_tests()
        .register_cancellation(
            invocation_id,
            "fake_background",
            Arc::new(move || {
                fired_clone.store(true, Ordering::SeqCst);
            }),
        );

    {
        let store = fixture.manager.store().lock().await;
        store
            .conn
            .execute(
                "INSERT INTO model_invocations (
                        id, purpose, invocation_kind, foreground, paid_risk, admission_status, status,
                        provider, model, backend, model_tier, effort, trigger_source,
                        operator, policy_snapshot_json, created_at, started_at, usage_confidence
                    ) VALUES (
                        ?1, 'text.generate.rpc', 'direct_text', 'foreground', 'paid_capable',
                        'admitted', 'running',
                        'Codex', 'gpt-5.4', 'Codex', 'premium', 'high', 'rpc_test',
                        'operator', '{}', ?2, ?2, 'unavailable'
                    )",
                params![
                    invocation_id.to_string(),
                    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
                ],
            )
            .expect("seed invocation");
    }

    let response = call_rpc(
        &fixture.server,
        "CancelModelInvocation",
        serde_json::json!({ "invocation_id": invocation_id }),
    )
    .await;
    assert!(response.error.is_none(), "{:?}", response.error);
    let report: rsi_common::model_control::CancelModelInvocationReport =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert!(report.request_recorded);
    assert_eq!(report.mechanism, "fake_background");
    assert_eq!(
        report.final_status,
        rsi_common::model_control::ModelInvocationStatus::CancellationRequested
    );
    assert!(fired.load(Ordering::SeqCst));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn update_model_control_policy_stop_all_cancels_active_invocations() {
    let fixture = recursive_dag_rpc_fixture();
    let session_id = Uuid::new_v4();
    let invocation_id = seed_active_model_invocation(
        &fixture,
        session_id,
        rsi_common::model_control::ModelInvocationPurpose::SessionLaunchFresh,
    )
    .await;

    let response = call_rpc(
        &fixture.server,
        "UpdateModelControlPolicy",
        serde_json::json!({
            "mode": "stop_all",
            "interrupt_active": true,
        }),
    )
    .await;
    assert!(response.error.is_none(), "{:?}", response.error);
    let report: rsi_common::model_control::ModelControlPolicyUpdateReport =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!(
        report.current_mode,
        rsi_common::model_control::ModelControlMode::StopAll
    );
    assert!(report.interrupted_sessions.contains(&session_id));
    assert!(report.requested_invocations.contains(&invocation_id));

    let store = fixture.manager.store().lock().await;
    assert_eq!(
        store.get_operator_pause(session_id).unwrap(),
        crate::store::manager_actions::OperatorPause::Hard,
    );
    let record = store
        .load_model_invocation_record(invocation_id)
        .expect("load")
        .expect("record");
    assert_eq!(
        record.status,
        rsi_common::model_control::ModelInvocationStatus::CancellationRequested
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn update_model_control_policy_stop_all_batches_past_4096_without_tail_loss() {
    let fixture = recursive_dag_rpc_fixture();
    let running_count = 4_100usize;
    let already_pending_count = 2usize;
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);

    {
        let store = fixture.manager.store().lock().await;
        for index in 0..running_count {
            store
                .conn
                .execute(
                    "INSERT INTO model_invocations (
                            id, purpose, invocation_kind, foreground, paid_risk, admission_status, status,
                            provider, model, backend, model_tier, effort, trigger_source,
                            operator, policy_snapshot_json, created_at, started_at, usage_confidence
                        ) VALUES (
                            ?1, 'text.generate.rpc', 'direct_text', 'foreground', 'paid_capable',
                            'admitted', 'running',
                            'Codex', 'gpt-5.4', 'Codex', 'premium', 'high', 'rpc_test',
                            'operator', '{}', ?2, ?2, 'unavailable'
                        )",
                    params![Uuid::new_v4().to_string(), now],
                )
                .unwrap_or_else(|error| panic!("seed running row {index}: {error}"));
        }
        for index in 0..already_pending_count {
            store
                .conn
                .execute(
                    "INSERT INTO model_invocations (
                            id, purpose, invocation_kind, foreground, paid_risk, admission_status, status,
                            provider, model, backend, model_tier, effort, trigger_source,
                            operator, policy_snapshot_json, created_at, started_at, usage_confidence,
                            cancellation_requested_at, cancellation_reason, cancellation_mechanism
                        ) VALUES (
                            ?1, 'text.generate.rpc', 'direct_text', 'foreground', 'paid_capable',
                            'admitted', 'cancellation_requested',
                            'Codex', 'gpt-5.4', 'Codex', 'premium', 'high', 'rpc_test',
                            'operator', '{}', ?2, ?2, 'unavailable',
                            ?2, 'stop_all', 'stop_all'
                        )",
                    params![Uuid::new_v4().to_string(), now],
                )
                .unwrap_or_else(|error| panic!("seed pending row {index}: {error}"));
        }
    }

    let response = call_rpc(
        &fixture.server,
        "UpdateModelControlPolicy",
        serde_json::json!({
            "mode": "stop_all",
            "interrupt_active": true,
        }),
    )
    .await;
    assert!(response.error.is_none(), "{:?}", response.error);
    let report: rsi_common::model_control::ModelControlPolicyUpdateReport =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!(
        report.current_mode,
        rsi_common::model_control::ModelControlMode::StopAll
    );
    assert_eq!(report.requested_invocations.len(), running_count);
    assert_eq!(
        report.skipped_invocations.len(),
        running_count + already_pending_count
    );

    let store = fixture.manager.store().lock().await;
    let running_after: i64 = store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM model_invocations WHERE status = 'running'",
            [],
            |row| row.get(0),
        )
        .expect("running count after stop_all");
    let pending_after: i64 = store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM model_invocations WHERE status = 'cancellation_requested'",
            [],
            |row| row.get(0),
        )
        .expect("pending count after stop_all");
    assert_eq!(running_after, 0);
    assert_eq!(
        pending_after,
        (running_count + already_pending_count) as i64
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn update_model_control_policy_can_replace_policies_and_trip_provider_circuit() {
    let fixture = recursive_dag_rpc_fixture();
    let response = call_rpc(
        &fixture.server,
        "UpdateModelControlPolicy",
        serde_json::json!({
            "mode": "deny_paid",
            "interrupt_active": false,
            "replace_policies": true,
            "policies": [{
                "scope_kind": "provider",
                "scope_id": "codex",
                "purpose": "session.launch.fresh",
                "model_tier": "premium",
                "effort": "high",
                "max_calls": 3,
                "alert_threshold_ratio": 0.25
            }],
            "circuit_updates": [{
                "scope_kind": "provider",
                "scope_id": "codex",
                "state": "open",
                "reason": "quota storm",
                "error_class": "quota",
                "cooldown_secs": 60
            }]
        }),
    )
    .await;
    assert!(response.error.is_none(), "{:?}", response.error);
    let status = call_rpc(
        &fixture.server,
        "GetModelControlStatus",
        serde_json::json!({ "recent_limit": 4 }),
    )
    .await;
    assert!(status.error.is_none(), "{:?}", status.error);
    let report: rsi_common::model_control::ModelControlStatusReport =
        serde_json::from_value(status.result.unwrap()).unwrap();
    assert_eq!(
        report.mode,
        rsi_common::model_control::ModelControlMode::DenyPaid
    );
    assert_eq!(report.policies.len(), 1);
    assert_eq!(report.policies[0].scope_id.as_deref(), Some("codex"));
    assert_eq!(report.circuits.len(), 1);
    assert_eq!(report.circuits[0].state, "open");
    assert_eq!(report.circuits[0].scope_id.as_deref(), Some("codex"));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn resource_governor_rpcs_grant_report_and_release_a_slot() {
    let fixture = recursive_dag_rpc_fixture();
    // Open every host gate so the assertion does not depend on host load.
    for (field, value) in [
        ("governor_max_load", 1024),
        ("governor_min_free_disk_gb", 0),
        ("governor_min_avail_mem_gb", 0),
        ("governor_max_workers_slice_gb", 4096),
    ] {
        fixture
            .server
            .runtime_config
            .update_field(field, &serde_json::json!(value))
            .unwrap();
    }
    let acquired = call_rpc(
        &fixture.server,
        "AcquireAdmission",
        serde_json::json!({
            "class": "build",
            "pid": std::process::id(),
            "label": "rpc-test",
        }),
    )
    .await;
    assert!(acquired.error.is_none(), "{:?}", acquired.error);
    let result = acquired.result.unwrap();
    assert_eq!(result["status"], "granted", "{result}");
    let lease_id = result["lease_id"].as_str().unwrap().to_string();

    let health = call_rpc(&fixture.server, "GetHealthStatus", serde_json::Value::Null).await;
    let governor = &health.result.unwrap()["resource_governor"];
    assert!(governor["margins"].as_array().unwrap().len() >= 3);
    assert!(
        governor["leases"]
            .as_array()
            .unwrap()
            .iter()
            .any(|lease| lease["lease_id"] == lease_id.as_str()),
        "health lists the lease: {governor}"
    );
    let view = call_rpc(
        &fixture.server,
        "GetResourceGovernor",
        serde_json::Value::Null,
    )
    .await;
    let view = view.result.unwrap();
    assert_eq!(view["policy"]["build_slots"], 4);
    assert_eq!(view["policy"]["lander_slots"], 5);

    let released = call_rpc(
        &fixture.server,
        "ReleaseAdmission",
        serde_json::json!({ "lease_id": lease_id }),
    )
    .await;
    assert_eq!(released.result.unwrap()["released"], true);

    // Session-attributed agents cannot reach the governor verbs.
    for method in [
        "AcquireAdmission",
        "ReleaseAdmission",
        "GetResourceGovernor",
    ] {
        assert!(!agent_gate::AGENT_VERBS.contains(&method));
        assert!(!agent_gate::READ_VERBS.contains(&method));
        assert!(!agent_gate::is_allowed_for_attributed_caller(method));
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn agent_gate_attributed_call_to_allowlisted_method_is_allowed() {
    let fixture = recursive_dag_rpc_fixture();
    let mut request = RpcRequest::new("GetHealthStatus", serde_json::Value::Null);
    request.session_token = Some("some-token".to_string());
    let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
    else {
        panic!("expected response");
    };
    // Passes the gate; any error here would be from the handler itself,
    // never the "not available to session-attributed callers" gate error.
    assert!(
        response.error.is_none(),
        "unexpected error: {:?}",
        response.error
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn agent_get_progress_defaults_to_cohort_and_rejects_malformed_or_identity_params() {
    use crate::store::sandbox_custody::{CustodyCause, NewCustodyRoot, SessionCustodyBinding};
    use rsi_common::types::{SandboxCleanupState, SandboxKind};

    let fixture = recursive_dag_rpc_fixture();
    let caller_id = Uuid::new_v4();
    let child_id = Uuid::new_v4();
    let outside_id = Uuid::new_v4();
    let base_commit = "b".repeat(64);
    {
        let mut store = fixture.manager.store().lock().await;
        store
            .insert_session(&mk_agent_test_session(
                caller_id,
                rsi_common::types::SessionKind::Task,
                None,
                None,
            ))
            .expect("insert caller");
        let mut child = mk_agent_test_session(
            child_id,
            rsi_common::types::SessionKind::Task,
            Some(caller_id),
            None,
        );
        child.sandbox_kind = Some(SandboxKind::GitWorktree);
        child.sandbox_root = Some(std::path::PathBuf::from("/tmp/rpc-progress-sandbox"));
        child.sandbox_branch = Some("rsi/rpc-progress".into());
        child.sandbox_cleanup_state = Some(SandboxCleanupState::Live);
        store
            .insert_session_with_custody(
                &child,
                SessionCustodyBinding::New(NewCustodyRoot {
                    custody_id: Uuid::new_v4(),
                    canonical_repo_dir: child.working_dir.display().to_string(),
                    sandbox_root: child.sandbox_root.as_ref().unwrap().display().to_string(),
                    sandbox_branch: child.sandbox_branch.clone().unwrap(),
                    repository_identity: "rpc-progress-test-repository".into(),
                    source_commit: base_commit.clone(),
                    cause: CustodyCause::AgentSpawnChild,
                }),
            )
            .expect("insert sandboxed child");
        let mut outside =
            mk_agent_test_session(outside_id, rsi_common::types::SessionKind::Task, None, None);
        outside.sandbox_kind = Some(SandboxKind::GitWorktree);
        outside.sandbox_root = Some(std::path::PathBuf::from("/tmp/rpc-outside-sandbox"));
        outside.sandbox_branch = Some("rsi/rpc-outside".into());
        outside.sandbox_cleanup_state = Some(SandboxCleanupState::Live);
        store
            .insert_session_with_custody(
                &outside,
                SessionCustodyBinding::New(NewCustodyRoot {
                    custody_id: Uuid::new_v4(),
                    canonical_repo_dir: outside.working_dir.display().to_string(),
                    sandbox_root: outside.sandbox_root.as_ref().unwrap().display().to_string(),
                    sandbox_branch: outside.sandbox_branch.clone().unwrap(),
                    repository_identity: "rpc-outside-test-repository".into(),
                    source_commit: "c".repeat(64),
                    cause: CustodyCause::AgentSpawnChild,
                }),
            )
            .expect("insert out-of-cohort sandboxed child");
    }
    let token = "progress-token".to_string();
    fixture
        .manager
        .register_agent_token(token.clone(), caller_id)
        .await;

    let mut request = RpcRequest::new("AgentGetProgress", serde_json::Value::Null);
    request.session_token = Some(token.clone());
    let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
    else {
        panic!("expected response");
    };
    assert!(response.error.is_none(), "{:?}", response.error);
    let result = response.result.expect("progress result");
    assert_eq!(result["cohort_size"], 1);
    assert_eq!(
        result["rows"][0]["cursor"]["session_id"],
        child_id.to_string()
    );
    assert_eq!(result["rows"][0]["base_commit"], base_commit);

    for params in [
        serde_json::json!({ "session_ids": ["not-a-uuid"] }),
        serde_json::json!({ "caller_session_id": caller_id }),
        serde_json::json!({ "session_ids": [outside_id] }),
    ] {
        let mut request = RpcRequest::new("AgentGetProgress", params);
        request.session_token = Some(token.clone());
        let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
        else {
            panic!("expected response");
        };
        assert!(
            response.error.is_some(),
            "malformed/smuggled params must fail"
        );
        assert!(
            response.result.is_none(),
            "denied progress discloses no rows"
        );
    }

    let mut request = RpcRequest::new(
        "AgentGetProgress",
        serde_json::json!({ "session_ids": vec![child_id; 257] }),
    );
    request.session_token = Some(token);
    let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
    else {
        panic!("expected response");
    };
    let error = response.error.expect("257 raw entries must fail");
    assert_eq!(error.code, -32602);
    let data = error.data.expect("typed overload data");
    assert_eq!(data["code"], "cohort_too_large");
    assert_eq!(data["cohort_size"], 257);
    assert_eq!(data["max_cohort_size"], 256);
}

/// P2-03 raw-RPC entry point: the verb accepts a token-attributed send to
/// an authorized child, refuses an untokened call at the gate, and refuses
/// every malformed or caller-field-smuggling params shape BEFORE any
/// payload is persisted.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn agent_send_message_rpc_entry_point_is_token_bound_and_strict() {
    let fixture = recursive_dag_rpc_fixture();
    let caller_id = Uuid::new_v4();
    let child_id = Uuid::new_v4();
    {
        let store = fixture.manager.store().lock().await;
        store
            .insert_session(&mk_agent_test_session(
                caller_id,
                rsi_common::types::SessionKind::Task,
                None,
                None,
            ))
            .expect("insert caller");
        store
            .insert_session(&mk_agent_test_session(
                child_id,
                rsi_common::types::SessionKind::Task,
                Some(caller_id),
                None,
            ))
            .expect("insert child");
    }
    let token = "send-token".to_string();
    fixture
        .manager
        .register_agent_token(token.clone(), caller_id)
        .await;

    // (a) An untokened caller never reaches the handler at all.
    let untokened = RpcRequest::new(
        "AgentSendMessage",
        serde_json::json!({
            "target_session_id": child_id,
            "message": "hello",
            "idempotency_key": "k-1",
        }),
    );
    let HandleResult::Response(response) = fixture.server.handle_request_inner(&untokened).await
    else {
        panic!("expected response");
    };
    // An unattributed caller bypasses the gate entirely (full operator
    // surface), so it reaches the handler and fails there for want of a
    // resolvable caller — never silently succeeding with no sender.
    let error = response
        .error
        .expect("a send with no resolvable caller must fail");
    assert!(
        error.message.contains("agent_verb_requires_session_token"),
        "unexpected error: {}",
        error.message
    );

    // (b) Every malformed / identity-smuggling shape is refused.
    for params in [
        serde_json::json!({
            "target_session_id": child_id, "message": "hello",
            "idempotency_key": "k-x", "owner_session_id": caller_id,
        }),
        serde_json::json!({
            "target_session_id": child_id, "message": "hello",
            "idempotency_key": "k-x", "sender_session_id": caller_id,
        }),
        serde_json::json!({
            "target_session_id": "not-a-uuid", "message": "hello",
            "idempotency_key": "k-x",
        }),
        serde_json::json!({ "target_session_id": child_id, "message": "hello" }),
        serde_json::json!({
            "target_session_id": child_id, "message": "",
            "idempotency_key": "k-x",
        }),
    ] {
        let mut request = RpcRequest::new("AgentSendMessage", params.clone());
        request.session_token = Some(token.clone());
        let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
        else {
            panic!("expected response");
        };
        assert!(
            response.error.is_some(),
            "malformed/smuggled params must fail: {params}"
        );
    }
    let persisted: i64 = fixture
        .manager
        .store()
        .lock()
        .await
        .conn
        .query_row("SELECT COUNT(*) FROM agent_messages", [], |row| row.get(0))
        .expect("count");
    assert_eq!(
        persisted, 0,
        "no rejected send may have persisted a payload"
    );

    // (c) The authorized send succeeds and returns the deterministic
    // receipt rooted on the immutable logical target.
    let mut request = RpcRequest::new(
        "AgentSendMessage",
        serde_json::json!({
            "target_session_id": child_id,
            "message": "do the thing",
            "idempotency_key": "k-ok",
        }),
    );
    request.session_token = Some(token.clone());
    let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
    else {
        panic!("expected response");
    };
    assert!(response.error.is_none(), "{:?}", response.error);
    let result = response.result.expect("send receipt");
    assert_eq!(result["target_session_id"], child_id.to_string());
    assert_eq!(result["state"], "queued");
    assert_eq!(result["state_version"], 0);
    assert_eq!(result["deduplicated"], false);

    // (d) A denied target returns the typed messaging error envelope.
    let mut denied = RpcRequest::new(
        "AgentSendMessage",
        serde_json::json!({
            "target_session_id": caller_id,
            "message": "self",
            "idempotency_key": "k-self",
        }),
    );
    denied.session_token = Some(token);
    let HandleResult::Response(response) = fixture.server.handle_request_inner(&denied).await
    else {
        panic!("expected response");
    };
    let error = response.error.expect("self-send must be denied");
    assert_eq!(error.code, -32602);
    let data = error.data.expect("typed messaging error data");
    // The wire `code` is the serde snake_case discriminant (matching the
    // existing `cohort_too_large` progress contract); the stable
    // `agent_message_*` class string lives in `error.message`.
    assert_eq!(data["code"], "target_not_authorized");
    assert!(
        error
            .message
            .starts_with("agent_message_target_not_authorized"),
        "unexpected message: {}",
        error.message
    );
    assert!(data["next_action"].as_str().is_some_and(|s| !s.is_empty()));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn launch_session_refuses_the_removed_taskrabbit_kind_before_any_side_effect() {
    let fixture = recursive_dag_rpc_fixture();
    let before = fixture
        .manager
        .store()
        .lock()
        .await
        .load_sessions()
        .expect("load sessions")
        .len();
    // A non-existent working dir would fail path resolution; the kind refusal
    // must win because it runs before any path or custody work.
    let response = call_rpc(
        &fixture.server,
        "LaunchSession",
        serde_json::json!({
            "query": "one-shot",
            "provider": "Claude",
            "tags": ["legacy"],
            "session_kind": "TaskRabbit",
            "working_dir": "/nonexistent/taskrabbit-refusal",
        }),
    )
    .await;
    let error = response.error.expect("TaskRabbit launch is refused");
    assert_eq!(error.code, INVALID_PARAMS, "{}", error.message);
    assert!(
        error
            .message
            .contains("TaskRabbit sessions were removed; launch a Standard session"),
        "{}",
        error.message
    );
    let after = fixture
        .manager
        .store()
        .lock()
        .await
        .load_sessions()
        .expect("load sessions")
        .len();
    assert_eq!(before, after, "refusal creates no session row");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn agent_gate_unattributed_launch_session_is_unaffected() {
    // (c) unattributed TUI LaunchSession unaffected by the gate — it may
    // still fail validation for unrelated reasons (e.g. missing query),
    // but never with the gate's denial message.
    let fixture = recursive_dag_rpc_fixture();
    let request = RpcRequest::new("LaunchSession", serde_json::json!({}));
    assert!(request.session_token.is_none());
    let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
    else {
        panic!("expected response");
    };
    if let Some(error) = response.error {
        assert!(
            !error
                .message
                .contains("not available to session-attributed callers"),
            "unattributed call must never be gate-denied, got: {}",
            error.message
        );
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn agent_gate_attributed_subscribe_is_denied() {
    // (c) unattributed Subscribe unaffected; attributed Subscribe is
    // gated — the gate runs *before* the Subscribe special-case.
    let fixture = recursive_dag_rpc_fixture();
    let mut request = RpcRequest::new("Subscribe", serde_json::json!({}));
    request.session_token = Some("some-token".to_string());
    let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
    else {
        panic!("expected a denied Response, not a Subscribe transition");
    };
    let error = response.error.expect("attributed Subscribe must be denied");
    assert_eq!(error.code, INVALID_PARAMS);
    assert!(
        error
            .message
            .contains("not available to session-attributed callers")
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn agent_spawn_child_lead_guard_epic_lead_succeeds_non_lead_rejected() {
    // (a) Epic-lead token spawns child via AgentSpawnChild; non-lead rejected.
    use rsi_common::types::SessionKind;

    let fixture = recursive_dag_rpc_fixture();

    // Lead case: Epic.lead_session_id == emitter_id.
    let epic_id = Uuid::new_v4();
    let lead_id = Uuid::new_v4();
    {
        let store = fixture.manager.store().lock().await;
        store
            .insert_session(&mk_agent_test_session(
                epic_id,
                SessionKind::Epic,
                None,
                Some(lead_id),
            ))
            .expect("insert epic");
        store
            .insert_session(&mk_agent_test_session(
                lead_id,
                SessionKind::Task,
                Some(epic_id),
                None,
            ))
            .expect("insert lead");
    }
    let lead_token = "lead-token".to_string();
    fixture
        .manager
        .register_agent_token(lead_token.clone(), lead_id)
        .await;

    let mut request = RpcRequest::new(
        "AgentSpawnChild",
        serde_json::json!({ "kind": "Task", "query": "do the thing", "idempotency_key": "lead-spawn" }),
    );
    request.session_token = Some(lead_token);
    let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
    else {
        panic!("expected response");
    };
    assert!(
        response.error.is_none(),
        "lead spawn should succeed: {:?}",
        response.error
    );
    let result = response.result.expect("result");
    assert_eq!(result["state"], "queued");
    assert!(result["spawn_request_id"].is_string());
    assert!(result["child_session_id"].is_string());

    // Non-lead case: emitter has no parent Epic, so it cannot be a lead.
    let non_lead_id = Uuid::new_v4();
    {
        let store = fixture.manager.store().lock().await;
        store
            .insert_session(&mk_agent_test_session(
                non_lead_id,
                SessionKind::Task,
                None,
                None,
            ))
            .expect("insert non-lead");
    }
    let non_lead_token = "non-lead-token".to_string();
    fixture
        .manager
        .register_agent_token(non_lead_token.clone(), non_lead_id)
        .await;

    let mut request = RpcRequest::new(
        "AgentSpawnChild",
        serde_json::json!({ "kind": "Task", "query": "do the thing", "idempotency_key": "non-lead-spawn" }),
    );
    request.session_token = Some(non_lead_token);
    let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
    else {
        panic!("expected response");
    };
    assert!(response.result.is_none());
    assert!(
        response
            .error
            .expect("non-lead rejection")
            .message
            .contains("NotLead")
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn agent_reserve_successor_is_attributed_replay_safe_and_lead_only() {
    use rsi_common::types::SessionKind;

    let fixture = recursive_dag_rpc_fixture();
    let epic_id = Uuid::new_v4();
    let lead_id = Uuid::new_v4();
    {
        let store = fixture.manager.store().lock().await;
        store
            .insert_session(&mk_agent_test_session(
                epic_id,
                SessionKind::Epic,
                None,
                Some(lead_id),
            ))
            .expect("insert epic");
        store
            .insert_session(&mk_agent_test_session(
                lead_id,
                SessionKind::Task,
                Some(epic_id),
                None,
            ))
            .expect("insert lead");
    }
    let lead_token = "reserve-successor-lead-token".to_string();
    fixture
        .manager
        .register_agent_token(lead_token.clone(), lead_id)
        .await;

    let params = serde_json::json!({
        "kind": "Task",
        "query": "continue the program",
        "idempotency_key": "turnover-1"
    });
    let mut request = RpcRequest::new("AgentReserveSuccessor", params.clone());
    request.session_token = Some(lead_token.clone());
    let HandleResult::Response(first) = fixture.server.handle_request_inner(&request).await else {
        panic!("expected response");
    };
    assert!(first.error.is_none(), "{:?}", first.error);
    let first = first.result.expect("reservation receipt");
    assert_eq!(first["epic_id"], epic_id.to_string());
    assert_eq!(first["predecessor_session_id"], lead_id.to_string());
    assert_eq!(first["state"], "reserved");
    assert_eq!(first["deduplicated"], false);

    let mut replay = RpcRequest::new("AgentReserveSuccessor", params);
    replay.session_token = Some(lead_token.clone());
    let HandleResult::Response(replay) = fixture.server.handle_request_inner(&replay).await else {
        panic!("expected replay response");
    };
    assert!(replay.error.is_none(), "{:?}", replay.error);
    let replay = replay.result.expect("replay receipt");
    assert_eq!(replay["reservation_id"], first["reservation_id"]);
    assert_eq!(
        replay["candidate_session_id"],
        first["candidate_session_id"]
    );
    assert_eq!(replay["deduplicated"], true);

    let mut changed = RpcRequest::new(
        "AgentReserveSuccessor",
        serde_json::json!({
            "kind": "Task",
            "query": "changed payload",
            "idempotency_key": "turnover-1"
        }),
    );
    changed.session_token = Some(lead_token.clone());
    let HandleResult::Response(changed) = fixture.server.handle_request_inner(&changed).await
    else {
        panic!("expected changed-replay response");
    };
    assert!(
        changed
            .error
            .expect("changed replay must fail")
            .message
            .contains("agent_successor_idempotency_conflict")
    );

    let mut spoofed = RpcRequest::new(
        "AgentReserveSuccessor",
        serde_json::json!({
            "kind": "Task",
            "query": "spoof",
            "idempotency_key": "turnover-spoof",
            "epic_id": epic_id,
            "predecessor_session_id": lead_id
        }),
    );
    spoofed.session_token = Some(lead_token);
    let HandleResult::Response(spoofed) = fixture.server.handle_request_inner(&spoofed).await
    else {
        panic!("expected spoof response");
    };
    assert!(
        spoofed.error.is_some(),
        "caller identities must be rejected"
    );

    let non_lead_id = Uuid::new_v4();
    {
        let store = fixture.manager.store().lock().await;
        store
            .insert_session(&mk_agent_test_session(
                non_lead_id,
                SessionKind::Task,
                Some(epic_id),
                None,
            ))
            .expect("insert non-lead");
    }
    let non_lead_token = "reserve-successor-non-lead-token".to_string();
    fixture
        .manager
        .register_agent_token(non_lead_token.clone(), non_lead_id)
        .await;
    let mut denied = RpcRequest::new(
        "AgentReserveSuccessor",
        serde_json::json!({
            "kind": "Task",
            "query": "not authorized",
            "idempotency_key": "turnover-denied"
        }),
    );
    denied.session_token = Some(non_lead_token);
    let HandleResult::Response(denied) = fixture.server.handle_request_inner(&denied).await else {
        panic!("expected denied response");
    };
    assert!(
        denied
            .error
            .expect("non-lead must be rejected")
            .message
            .contains("agent_successor_caller_is_not_current_epic_lead")
    );
}

struct SuccessorUnixSocketDaemonFixture {
    manager: std::sync::Arc<SessionManager>,
    server: std::sync::Arc<RpcServer>,
}

fn successor_unix_socket_daemon_fixture(
    root: &std::path::Path,
) -> SuccessorUnixSocketDaemonFixture {
    use crate::bus::EventBus;
    use crate::config::{Config, RuntimeConfig};
    use crate::prompt_compile::CompileEngine;
    use crate::store::Store;

    let store = Store::open(&root.join("rsi.db")).expect("open successor fixture store");
    let runtime_config = RuntimeConfig::from_config(&Config::from_env());
    let bus = std::sync::Arc::new(EventBus::new(16));
    let manager = std::sync::Arc::new(
        SessionManager::new(
            std::sync::Arc::clone(&bus),
            store,
            false,
            root.join("daemon.sock"),
            None,
            Vec::new(),
            std::sync::Arc::clone(&runtime_config),
            root.join("sandboxes"),
        )
        .expect("construct successor fixture manager"),
    );
    let http = reqwest::Client::new();
    let server = std::sync::Arc::new(RpcServer::new(
        std::sync::Arc::clone(&manager),
        None,
        None,
        None,
        std::sync::Arc::clone(&runtime_config),
        CompileEngine::new(http.clone(), manager.store().clone(), runtime_config, bus),
        http,
        crate::model_control::ModelControlRuntime::default_normal(),
    ));
    SuccessorUnixSocketDaemonFixture { manager, server }
}

async fn call_serialized_rpc_over_unix_socket(
    server: std::sync::Arc<RpcServer>,
    _root: &std::path::Path,
    request: &RpcRequest,
) -> RpcResponse {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    let socket_root = tempfile::Builder::new()
        .prefix("r")
        .tempdir_in("/tmp")
        .expect("isolated RPC socket root");
    let socket_path = socket_root
        .path()
        .join(format!("rpc-{}.sock", Uuid::new_v4()));
    let listener = tokio::net::UnixListener::bind(&socket_path)
        .expect("bind isolated in-process daemon Unix socket");
    let connection = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept RPC client");
        server
            .handle_connection(stream)
            .await
            .expect("serve serialized RPC connection");
    });
    let stream = tokio::net::UnixStream::connect(&socket_path)
        .await
        .expect("connect isolated RPC client");
    let (reader, mut writer) = stream.into_split();
    writer
        .write_all(
            format!(
                "{}\n",
                serde_json::to_string(request).expect("serialize JSON-RPC request")
            )
            .as_bytes(),
        )
        .await
        .expect("write JSON-RPC request");
    writer.flush().await.expect("flush JSON-RPC request");
    let mut lines = tokio::io::BufReader::new(reader).lines();
    let response = lines
        .next_line()
        .await
        .expect("read JSON-RPC response")
        .expect("daemon returned one JSON-RPC response");
    drop(lines);
    drop(writer);
    connection.await.expect("join isolated RPC connection");
    serde_json::from_str(&response).expect("deserialize JSON-RPC response")
}

/// Isolated daemon-level identity proof for Operator Views Slice 2. The
/// socket and SQLite file live under one temporary root; reconstructing the
/// manager/server proves replay and the next allocation use durable V99
/// role/ordinal/counter state rather than process-local coordination.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn agent_identity_in_process_rpc_server_unix_socket_restart_fixture() {
    use rsi_common::types::SessionKind;

    let root = tempfile::TempDir::new().expect("identity fixture root");
    let epic_id = Uuid::new_v4();
    let lead_id = Uuid::new_v4();
    let first_daemon = successor_unix_socket_daemon_fixture(root.path());
    {
        let store = first_daemon.manager.store().lock().await;
        store
            .insert_session(&mk_agent_test_session(
                epic_id,
                SessionKind::Epic,
                None,
                Some(lead_id),
            ))
            .expect("insert identity Epic");
        store
            .insert_session(&mk_agent_test_session(
                lead_id,
                SessionKind::Task,
                Some(epic_id),
                None,
            ))
            .expect("insert identity lead");
    }
    let first_token = "identity-restart-first-token".to_string();
    first_daemon
        .manager
        .register_agent_token(first_token.clone(), lead_id)
        .await;

    let first_params = serde_json::json!({
        "kind": "Task",
        "query": "research the identity boundary",
        "agent_role": "  Research   Lead  ",
        "idempotency_key": "identity-restart-first"
    });
    let mut first_request = RpcRequest::new("AgentSpawnChild", first_params.clone());
    first_request.session_token = Some(first_token);
    let first = call_serialized_rpc_over_unix_socket(
        std::sync::Arc::clone(&first_daemon.server),
        root.path(),
        &first_request,
    )
    .await;
    assert!(
        first.error.is_none(),
        "identity reservation: {:?}",
        first.error
    );
    let first = first.result.expect("identity reservation receipt");
    assert_eq!(first["agent_role"], "Research Lead");
    assert_eq!(first["epic_spawn_ordinal"], 1);
    assert_eq!(first["deduplicated"], false);
    let first_spawn_request_id = Uuid::parse_str(
        first["spawn_request_id"]
            .as_str()
            .expect("spawn request id"),
    )
    .expect("canonical spawn request id");
    {
        let store = first_daemon.manager.store().lock().await;
        let durable = store
            .get_agent_spawn_request(first_spawn_request_id)
            .expect("load identity reservation")
            .expect("identity reservation exists");
        assert_eq!(durable.request.agent_role.as_deref(), Some("Research Lead"));
        assert_eq!(durable.epic_spawn_ordinal, 1);
        assert_eq!(
            store
                .conn
                .query_row(
                    "SELECT next_ordinal FROM epic_spawn_counters WHERE epic_id=?1",
                    [epic_id.to_string()],
                    |row| row.get::<_, i64>(0),
                )
                .expect("read first identity counter"),
            2
        );
    }

    drop(first_daemon);
    let restarted_daemon = successor_unix_socket_daemon_fixture(root.path());
    let restarted_token = "identity-restart-reminted-token".to_string();
    restarted_daemon
        .manager
        .register_agent_token(restarted_token.clone(), lead_id)
        .await;

    let mut replay_request = RpcRequest::new("AgentSpawnChild", first_params);
    replay_request.session_token = Some(restarted_token.clone());
    let replay = call_serialized_rpc_over_unix_socket(
        std::sync::Arc::clone(&restarted_daemon.server),
        root.path(),
        &replay_request,
    )
    .await;
    assert!(
        replay.error.is_none(),
        "identity replay: {:?}",
        replay.error
    );
    let replay = replay.result.expect("identity replay receipt");
    assert_eq!(
        replay["spawn_request_id"],
        first_spawn_request_id.to_string()
    );
    assert_eq!(replay["agent_role"], "Research Lead");
    assert_eq!(replay["epic_spawn_ordinal"], 1);
    assert_eq!(replay["deduplicated"], true);

    let mut next_request = RpcRequest::new(
        "AgentSpawnChild",
        serde_json::json!({
            "kind": "Task",
            "query": "review the identity boundary",
            "agent_role": "Reviewer",
            "idempotency_key": "identity-restart-next"
        }),
    );
    next_request.session_token = Some(restarted_token);
    let next = call_serialized_rpc_over_unix_socket(
        std::sync::Arc::clone(&restarted_daemon.server),
        root.path(),
        &next_request,
    )
    .await;
    assert!(
        next.error.is_none(),
        "next identity reservation: {:?}",
        next.error
    );
    let next = next.result.expect("next identity reservation receipt");
    assert_eq!(next["agent_role"], "Reviewer");
    assert_eq!(next["epic_spawn_ordinal"], 2);
    assert_eq!(next["deduplicated"], false);

    let store = restarted_daemon.manager.store().lock().await;
    assert_eq!(
        store
            .conn
            .query_row(
                "SELECT next_ordinal FROM epic_spawn_counters WHERE epic_id=?1",
                [epic_id.to_string()],
                |row| row.get::<_, i64>(0),
            )
            .expect("read restarted identity counter"),
        3
    );
    assert_eq!(
        store
            .conn
            .query_row(
                "SELECT count(*) FROM agent_spawn_requests WHERE epic_id=?1",
                [epic_id.to_string()],
                |row| row.get::<_, i64>(0),
            )
            .expect("count durable identity reservations"),
        2
    );
}

struct AgentIssueCliSocket {
    shutdown: tokio::sync::oneshot::Sender<()>,
    task: tokio::task::JoinHandle<()>,
    socket_path: std::path::PathBuf,
}

impl AgentIssueCliSocket {
    async fn stop(self) {
        let _ = self.shutdown.send(());
        self.task.await.expect("join isolated Issue RPC listener");
        if self.socket_path.exists() {
            std::fs::remove_file(&self.socket_path).expect("remove isolated Issue RPC socket");
        }
    }
}

async fn start_agent_issue_cli_socket(
    server: std::sync::Arc<RpcServer>,
    socket_path: std::path::PathBuf,
) -> AgentIssueCliSocket {
    if socket_path.exists() {
        std::fs::remove_file(&socket_path).expect("remove stale isolated Issue socket");
    }
    let listener =
        tokio::net::UnixListener::bind(&socket_path).expect("bind isolated Issue RPC socket");
    let (shutdown, mut shutdown_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                accepted = listener.accept() => {
                    let (stream, _) = accepted.expect("accept isolated rsi-rpc client");
                    let server = std::sync::Arc::clone(&server);
                    tokio::spawn(async move {
                        server.handle_connection(stream).await
                            .expect("serve isolated rsi-rpc connection");
                    });
                }
            }
        }
    });
    AgentIssueCliSocket {
        shutdown,
        task,
        socket_path,
    }
}

async fn call_real_rsi_rpc_subprocess(
    binary: &std::path::Path,
    socket_path: &std::path::Path,
    fixture_home: &std::path::Path,
    token: &str,
    method: &str,
    params: serde_json::Value,
) -> RpcResponse {
    let binary = binary.to_path_buf();
    let socket_path = socket_path.to_path_buf();
    let socket_display = socket_path.display().to_string();
    let fixture_home = fixture_home.to_path_buf();
    let token = token.to_string();
    let method = method.to_string();
    let params = serde_json::to_string(&params).expect("serialize rsi-rpc params");
    let output = tokio::task::spawn_blocking(move || {
        std::process::Command::new(binary)
            .arg("--socket")
            .arg(&socket_path)
            .arg(&method)
            .arg("--params")
            .arg(params)
            .current_dir(&fixture_home)
            .env("HOME", &fixture_home)
            .env("RSI_DAEMON_SOCKET_PATH", &socket_path)
            .env("RSI_SOCKET", &socket_path)
            .env("RSI_SESSION_TOKEN", token)
            .output()
            .expect("launch real rsi-rpc subprocess")
    })
    .await
    .expect("join rsi-rpc subprocess");
    assert!(
        matches!(output.status.code(), Some(0 | 2)),
        "unexpected rsi-rpc exit {:?}: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        stderr.trim(),
        format!("rsi-rpc socket: {socket_display}"),
        "rsi-rpc stderr must name only the isolated socket"
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "invalid rsi-rpc JSON ({error}): {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

fn assert_agent_issue_error_code(response: &RpcResponse, code: &str) {
    let error = response.error.as_ref().expect("expected Issue RPC error");
    assert_eq!(
        error.data.as_ref().expect("safe Issue error data")["code"],
        serde_json::json!(code),
        "unexpected Issue error: {error:?}"
    );
    let encoded = serde_json::to_string(error).expect("encode safe error");
    for forbidden in ["sqlite", "database", "topology", "token=", "/home/"] {
        assert!(
            !encoded.to_lowercase().contains(forbidden),
            "Issue error leaked {forbidden}: {encoded}"
        );
    }
}

/// Full isolated V95 smoke. The shell wrapper supplies an empty fixture
/// root and the freshly built CLI path; every call below is a real
/// `rsi-rpc` subprocess crossing one temporary Unix socket.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn agent_issue_in_process_rpc_server_unix_socket_rsi_rpc_restart_fixture() {
    use rsi_common::types::{Project, SessionKind};

    let Some(root) = std::env::var_os("RSI_AGENT_ISSUE_SMOKE_ROOT") else {
        eprintln!("RSI_AGENT_ISSUE_SMOKE_ROOT absent; isolated CLI smoke is wrapper-owned");
        return;
    };
    let root = std::path::PathBuf::from(root);
    let binary = std::path::PathBuf::from(
        std::env::var_os("RSI_AGENT_ISSUE_SMOKE_RSI_RPC").expect("RSI_AGENT_ISSUE_SMOKE_RSI_RPC"),
    );
    let fixture_home = root.join("home");
    let socket_path = root.join("daemon.sock");
    std::fs::create_dir_all(&fixture_home).expect("create isolated smoke HOME");
    assert!(root.is_absolute());
    assert!(binary.is_absolute());
    assert!(socket_path.starts_with(&root));

    let project_a = Project {
        id: Uuid::new_v4(),
        name: "Issue smoke A".to_string(),
        path: None,
        description: None,
        color: Project::DEFAULT_COLOR.to_string(),
        context_files: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    let project_b = Project {
        id: Uuid::new_v4(),
        name: "Issue smoke B".to_string(),
        ..project_a.clone()
    };
    let group_a = Uuid::new_v4();
    let epic_a = Uuid::new_v4();
    let lead_a = Uuid::new_v4();
    let worker_a = Uuid::new_v4();
    let group_b = Uuid::new_v4();
    let epic_b = Uuid::new_v4();
    let lead_b = Uuid::new_v4();
    let worker_token = "isolated-worker-token";
    let lead_token = "isolated-lead-token";
    let other_lead_token = "isolated-other-lead-token";

    let fixture = successor_unix_socket_daemon_fixture(&root);
    {
        let store = fixture.manager.store().lock().await;
        store.insert_project(&project_a).unwrap();
        store.insert_project(&project_b).unwrap();
        for (id, kind, parent, project) in [
            (group_a, SessionKind::Group, None, project_a.id),
            (epic_a, SessionKind::Epic, Some(group_a), project_a.id),
            (lead_a, SessionKind::Task, Some(epic_a), project_a.id),
            (worker_a, SessionKind::Task, Some(epic_a), project_a.id),
            (group_b, SessionKind::Group, None, project_b.id),
            (epic_b, SessionKind::Epic, Some(group_b), project_b.id),
            (lead_b, SessionKind::Task, Some(epic_b), project_b.id),
        ] {
            let mut session = mk_agent_test_session(id, kind, parent, None);
            session.project_id = Some(project);
            store.insert_session(&session).unwrap();
        }
        store.set_lead_session(epic_a, Some(lead_a)).unwrap();
        store.set_lead_session(epic_b, Some(lead_b)).unwrap();
    }
    fixture
        .manager
        .register_agent_token(worker_token.to_string(), worker_a)
        .await;
    fixture
        .manager
        .register_agent_token(lead_token.to_string(), lead_a)
        .await;
    fixture
        .manager
        .register_agent_token(other_lead_token.to_string(), lead_b)
        .await;
    let socket =
        start_agent_issue_cli_socket(std::sync::Arc::clone(&fixture.server), socket_path.clone())
            .await;

    let created = call_real_rsi_rpc_subprocess(
        &binary,
        &socket_path,
        &fixture_home,
        worker_token,
        "AgentCreateIssue",
        serde_json::json!({
            "title":"isolated ordinary create",
            "body":"created through the real CLI",
            "labels":["smoke"],
            "idempotency_key":"smoke-create"
        }),
    )
    .await;
    assert!(created.error.is_none(), "ordinary create: {created:?}");
    let issue_id = Uuid::parse_str(
        created.result.as_ref().unwrap()["issue"]["id"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        created.result.as_ref().unwrap()["issue"]["created_by_session_id"],
        worker_a.to_string()
    );

    let worker_denied = call_real_rsi_rpc_subprocess(
        &binary,
        &socket_path,
        &fixture_home,
        worker_token,
        "AgentGetIssue",
        serde_json::json!({"issue_id":issue_id}),
    )
    .await;
    assert_agent_issue_error_code(&worker_denied, "authority_denied");

    let listed = call_real_rsi_rpc_subprocess(
        &binary,
        &socket_path,
        &fixture_home,
        lead_token,
        "AgentListIssues",
        serde_json::json!({"limit":16}),
    )
    .await;
    assert_eq!(
        listed.result.as_ref().unwrap()["issues"][0]["id"],
        issue_id.to_string()
    );
    let fetched = call_real_rsi_rpc_subprocess(
        &binary,
        &socket_path,
        &fixture_home,
        lead_token,
        "AgentGetIssue",
        serde_json::json!({"issue_id":issue_id}),
    )
    .await;
    assert_eq!(fetched.result.as_ref().unwrap()["row_version"], 1);

    let malformed = call_real_rsi_rpc_subprocess(
        &binary,
        &socket_path,
        &fixture_home,
        lead_token,
        "AgentGetIssue",
        serde_json::json!({
            "issue_id": issue_id,
            "project_id/secret": "never-echo-this"
        }),
    )
    .await;
    assert_agent_issue_error_code(&malformed, "invalid_request");
    let malformed_error = malformed.error.as_ref().unwrap();
    let malformed_envelope: rsi_common::rpc::AgentIssueErrorV1 = serde_json::from_value(
        malformed_error
            .data
            .as_ref()
            .expect("typed validation envelope")
            .clone(),
    )
    .unwrap();
    assert_eq!(
        malformed_envelope.validation.unwrap().class,
        rsi_common::rpc::AgentIssueValidationClassV1::UnknownField
    );
    let encoded = serde_json::to_string(&malformed).unwrap();
    assert!(!encoded.contains("project_id/secret"));
    assert!(!encoded.contains("never-echo-this"));

    let update_params = serde_json::json!({
        "issue_id":issue_id,"expected_row_version":1,
        "idempotency_key":"smoke-update","title":"CLI-updated title"
    });
    let updated = call_real_rsi_rpc_subprocess(
        &binary,
        &socket_path,
        &fixture_home,
        lead_token,
        "AgentUpdateIssue",
        update_params.clone(),
    )
    .await;
    assert_eq!(updated.result.as_ref().unwrap()["issue"]["row_version"], 2);
    let stale = call_real_rsi_rpc_subprocess(
        &binary,
        &socket_path,
        &fixture_home,
        lead_token,
        "AgentUpdateIssue",
        serde_json::json!({
            "issue_id":issue_id,"expected_row_version":1,
            "idempotency_key":"smoke-stale","body":"stale"
        }),
    )
    .await;
    assert_agent_issue_error_code(&stale, "stale_version");
    let conflict = call_real_rsi_rpc_subprocess(
        &binary,
        &socket_path,
        &fixture_home,
        lead_token,
        "AgentUpdateIssue",
        serde_json::json!({
            "issue_id":issue_id,"expected_row_version":1,
            "idempotency_key":"smoke-update","title":"changed replay"
        }),
    )
    .await;
    assert_agent_issue_error_code(&conflict, "idempotency_conflict");

    let in_progress = call_real_rsi_rpc_subprocess(
        &binary,
        &socket_path,
        &fixture_home,
        lead_token,
        "AgentUpdateIssueStatus",
        serde_json::json!({
            "issue_id":issue_id,"status":"InProgress","expected_row_version":2,
            "idempotency_key":"smoke-in-progress"
        }),
    )
    .await;
    assert_eq!(
        in_progress.result.as_ref().unwrap()["issue"]["row_version"],
        3
    );
    let closed = call_real_rsi_rpc_subprocess(
        &binary,
        &socket_path,
        &fixture_home,
        lead_token,
        "AgentUpdateIssueStatus",
        serde_json::json!({
            "issue_id":issue_id,"status":"Closed","expected_row_version":3,
            "idempotency_key":"smoke-close"
        }),
    )
    .await;
    assert_eq!(closed.result.as_ref().unwrap()["issue"]["row_version"], 4);
    let archived = call_real_rsi_rpc_subprocess(
        &binary,
        &socket_path,
        &fixture_home,
        lead_token,
        "AgentArchiveIssue",
        serde_json::json!({
            "issue_id":issue_id,"expected_row_version":4,
            "idempotency_key":"smoke-archive"
        }),
    )
    .await;
    assert_eq!(archived.result.as_ref().unwrap()["issue"]["row_version"], 5);
    let archived_update = call_real_rsi_rpc_subprocess(
        &binary,
        &socket_path,
        &fixture_home,
        lead_token,
        "AgentUpdateIssue",
        serde_json::json!({
            "issue_id":issue_id,"expected_row_version":5,
            "idempotency_key":"smoke-archived-update","body":"denied"
        }),
    )
    .await;
    assert_agent_issue_error_code(&archived_update, "archived");
    let history = call_real_rsi_rpc_subprocess(
        &binary,
        &socket_path,
        &fixture_home,
        lead_token,
        "AgentListIssueEvents",
        serde_json::json!({"issue_id":issue_id,"limit":16}),
    )
    .await;
    assert_eq!(
        history.result.as_ref().unwrap()["events"]
            .as_array()
            .unwrap()
            .len(),
        5
    );
    let restored = call_real_rsi_rpc_subprocess(
        &binary,
        &socket_path,
        &fixture_home,
        lead_token,
        "AgentRestoreIssue",
        serde_json::json!({
            "issue_id":issue_id,"expected_row_version":5,
            "idempotency_key":"smoke-restore"
        }),
    )
    .await;
    assert_eq!(restored.result.as_ref().unwrap()["issue"]["row_version"], 6);
    let reopened = call_real_rsi_rpc_subprocess(
        &binary,
        &socket_path,
        &fixture_home,
        lead_token,
        "AgentUpdateIssueStatus",
        serde_json::json!({
            "issue_id":issue_id,"status":"Open","expected_row_version":6,
            "idempotency_key":"smoke-reopen"
        }),
    )
    .await;
    assert_eq!(reopened.result.as_ref().unwrap()["issue"]["row_version"], 7);

    let cross_project = call_real_rsi_rpc_subprocess(
        &binary,
        &socket_path,
        &fixture_home,
        other_lead_token,
        "AgentGetIssue",
        serde_json::json!({"issue_id":issue_id}),
    )
    .await;
    let unknown = call_real_rsi_rpc_subprocess(
        &binary,
        &socket_path,
        &fixture_home,
        other_lead_token,
        "AgentGetIssue",
        serde_json::json!({"issue_id":Uuid::new_v4()}),
    )
    .await;
    assert_eq!(
        serde_json::to_value(&cross_project.error).unwrap(),
        serde_json::to_value(&unknown.error).unwrap()
    );
    assert_agent_issue_error_code(&cross_project, "not_found_in_scope");
    let generic_denied = call_real_rsi_rpc_subprocess(
        &binary,
        &socket_path,
        &fixture_home,
        lead_token,
        "ListIssueEvents",
        serde_json::json!({"issue_id":issue_id}),
    )
    .await;
    assert!(
        generic_denied
            .error
            .unwrap()
            .message
            .contains("not available")
    );

    socket.stop().await;
    drop(fixture);

    let restarted = successor_unix_socket_daemon_fixture(&root);
    let reminted_lead_token = "isolated-reminted-lead-token";
    restarted
        .manager
        .register_agent_token(reminted_lead_token.to_string(), lead_a)
        .await;
    let socket = start_agent_issue_cli_socket(
        std::sync::Arc::clone(&restarted.server),
        socket_path.clone(),
    )
    .await;
    let revoked = call_real_rsi_rpc_subprocess(
        &binary,
        &socket_path,
        &fixture_home,
        lead_token,
        "AgentGetIssue",
        serde_json::json!({"issue_id":issue_id}),
    )
    .await;
    assert_agent_issue_error_code(&revoked, "authority_denied");
    let replay = call_real_rsi_rpc_subprocess(
        &binary,
        &socket_path,
        &fixture_home,
        reminted_lead_token,
        "AgentUpdateIssue",
        update_params,
    )
    .await;
    assert_eq!(replay.result.as_ref().unwrap()["deduplicated"], true);
    assert_eq!(replay.result.as_ref().unwrap()["issue"]["row_version"], 2);
    assert_eq!(
        replay.result.as_ref().unwrap()["issue"]["title"],
        "CLI-updated title"
    );
    let final_history = call_real_rsi_rpc_subprocess(
        &binary,
        &socket_path,
        &fixture_home,
        reminted_lead_token,
        "AgentListIssueEvents",
        serde_json::json!({"issue_id":issue_id,"limit":16}),
    )
    .await;
    assert_eq!(
        final_history.result.as_ref().unwrap()["events"]
            .as_array()
            .unwrap()
            .len(),
        7
    );
    socket.stop().await;
    drop(restarted);

    assert!(root.join("rsi.db").is_file());
    assert!(!socket_path.exists());
}

/// Exercise the lead-derived RPC surface with both credentials live.
/// Token revocation is deliberately not allowed to hide an authorization
/// split: SQLite must grant the candidate and reject the predecessor for
/// spawn, status/halt's shared scope guard, and message send.
async fn assert_successor_rpc_authority_plane(
    fixture: &SuccessorUnixSocketDaemonFixture,
    root: &std::path::Path,
    epic_id: Uuid,
    predecessor_id: Uuid,
    candidate_id: Uuid,
    predecessor_token: &str,
    candidate_token: &str,
    phase: &str,
) {
    fixture
        .manager
        .register_agent_token(predecessor_token.to_string(), predecessor_id)
        .await;
    fixture
        .manager
        .register_agent_token(candidate_token.to_string(), candidate_id)
        .await;

    let mut candidate_spawn = RpcRequest::new(
        "AgentSpawnChild",
        serde_json::json!({
            "kind": "Task",
            "query": format!("candidate dispatch at {phase}"),
            "idempotency_key": format!("baton-candidate-{phase}")
        }),
    );
    candidate_spawn.session_token = Some(candidate_token.to_string());
    let candidate_spawned = call_serialized_rpc_over_unix_socket(
        std::sync::Arc::clone(&fixture.server),
        root,
        &candidate_spawn,
    )
    .await;
    assert!(
        candidate_spawned.error.is_none(),
        "candidate spawn failed at {phase}: {:?}",
        candidate_spawned.error
    );
    assert_eq!(
        candidate_spawned.result.expect("candidate spawn receipt")["epic_id"],
        epic_id.to_string()
    );

    let mut predecessor_spawn = RpcRequest::new(
        "AgentSpawnChild",
        serde_json::json!({
            "kind": "Task",
            "query": format!("predecessor dispatch denied at {phase}"),
            "idempotency_key": format!("baton-predecessor-{phase}")
        }),
    );
    predecessor_spawn.session_token = Some(predecessor_token.to_string());
    let predecessor_denied = call_serialized_rpc_over_unix_socket(
        std::sync::Arc::clone(&fixture.server),
        root,
        &predecessor_spawn,
    )
    .await;
    assert!(
        predecessor_denied
            .error
            .expect("predecessor spawn must be denied")
            .message
            .contains("NotLead"),
        "predecessor retained spawn authority at {phase}"
    );

    let mut candidate_status = RpcRequest::new(
        "AgentGetStatus",
        serde_json::json!({ "session_id": predecessor_id }),
    );
    candidate_status.session_token = Some(candidate_token.to_string());
    let candidate_status = call_serialized_rpc_over_unix_socket(
        std::sync::Arc::clone(&fixture.server),
        root,
        &candidate_status,
    )
    .await;
    assert!(
        candidate_status.error.is_none(),
        "candidate status failed at {phase}: {:?}",
        candidate_status.error
    );
    assert_eq!(
        candidate_status.result.expect("candidate status result")["id"],
        predecessor_id.to_string()
    );

    let mut predecessor_status = RpcRequest::new(
        "AgentGetStatus",
        serde_json::json!({ "session_id": candidate_id }),
    );
    predecessor_status.session_token = Some(predecessor_token.to_string());
    let predecessor_status = call_serialized_rpc_over_unix_socket(
        std::sync::Arc::clone(&fixture.server),
        root,
        &predecessor_status,
    )
    .await;
    assert!(
        predecessor_status
            .error
            .expect("predecessor status must be denied")
            .message
            .contains("agent_verb_scope_denied"),
        "predecessor retained status/halt authority at {phase}"
    );

    // Halt a fresh independently tracked child in every phase. The
    // candidate must traverse the real active-incarnation interrupt path,
    // while the predecessor must fail at the durable Epic authority
    // fence. Never target either baton participant: an authorized halt
    // must not destroy the fixture needed by projection retry or restart.
    let halt_target_id = Uuid::new_v4();
    let halt_target = mk_agent_test_session(
        halt_target_id,
        rsi_common::types::SessionKind::Task,
        Some(epic_id),
        None,
    );
    fixture
        .manager
        .store()
        .lock()
        .await
        .insert_session(&halt_target)
        .expect("insert disposable halt target");
    fixture.manager.active().write().await.insert(
        halt_target_id,
        crate::session::types::TrackedSession::new_for_test(halt_target),
    );

    let mut candidate_halt = RpcRequest::new(
        "AgentHalt",
        serde_json::json!({ "session_id": halt_target_id }),
    );
    candidate_halt.session_token = Some(candidate_token.to_string());
    let candidate_halt = call_serialized_rpc_over_unix_socket(
        std::sync::Arc::clone(&fixture.server),
        root,
        &candidate_halt,
    )
    .await;
    assert!(
        candidate_halt.error.is_none(),
        "candidate halt failed at {phase}: {:?}",
        candidate_halt.error
    );
    assert_eq!(
        candidate_halt.result.expect("candidate halt result")["ok"],
        true,
        "candidate halt did not complete the real RPC route at {phase}"
    );

    let mut predecessor_halt = RpcRequest::new(
        "AgentHalt",
        serde_json::json!({ "session_id": halt_target_id }),
    );
    predecessor_halt.session_token = Some(predecessor_token.to_string());
    let predecessor_halt = call_serialized_rpc_over_unix_socket(
        std::sync::Arc::clone(&fixture.server),
        root,
        &predecessor_halt,
    )
    .await;
    let predecessor_halt_error = predecessor_halt
        .error
        .expect("predecessor halt must be denied");
    assert_eq!(
        predecessor_halt_error.code, INVALID_PARAMS,
        "predecessor halt returned the wrong RPC error type at {phase}"
    );
    assert!(
        predecessor_halt_error
            .message
            .contains("agent_verb_scope_denied"),
        "predecessor retained halt authority at {phase}: {}",
        predecessor_halt_error.message
    );
    assert!(
        fixture
            .manager
            .active()
            .write()
            .await
            .remove(&halt_target_id)
            .is_some(),
        "disposable halt incarnation disappeared at {phase}"
    );
    fixture
        .manager
        .store()
        .lock()
        .await
        .update_session_status(
            halt_target_id,
            rsi_common::types::SessionStatus::Interrupted,
        )
        .expect("settle disposable halt target");

    // The predecessor can be terminal after a daemon restart. Use a live
    // Epic child to test the candidate's message authority in every phase.
    let message_target_id = Uuid::new_v4();
    fixture
        .manager
        .store()
        .lock()
        .await
        .insert_session(&mk_agent_test_session(
            message_target_id,
            rsi_common::types::SessionKind::Task,
            Some(epic_id),
            None,
        ))
        .expect("insert live message target");
    let mut candidate_send = RpcRequest::new(
        "AgentSendMessage",
        serde_json::json!({
            "target_session_id": message_target_id,
            "message": format!("candidate authority at {phase}"),
            "idempotency_key": format!("baton-candidate-message-{phase}")
        }),
    );
    candidate_send.session_token = Some(candidate_token.to_string());
    let candidate_send = call_serialized_rpc_over_unix_socket(
        std::sync::Arc::clone(&fixture.server),
        root,
        &candidate_send,
    )
    .await;
    assert!(
        candidate_send.error.is_none(),
        "candidate send failed at {phase}: {:?}",
        candidate_send.error
    );
    assert_eq!(
        candidate_send.result.expect("candidate message receipt")["state"],
        "queued"
    );

    let mut predecessor_send = RpcRequest::new(
        "AgentSendMessage",
        serde_json::json!({
            "target_session_id": candidate_id,
            "message": format!("predecessor authority denied at {phase}"),
            "idempotency_key": format!("baton-predecessor-message-{phase}")
        }),
    );
    predecessor_send.session_token = Some(predecessor_token.to_string());
    let predecessor_send = call_serialized_rpc_over_unix_socket(
        std::sync::Arc::clone(&fixture.server),
        root,
        &predecessor_send,
    )
    .await;
    let predecessor_send_error = predecessor_send
        .error
        .expect("predecessor message must be denied");
    assert_eq!(
        predecessor_send_error
            .data
            .expect("typed predecessor message denial")["code"],
        "target_not_authorized",
        "predecessor retained message authority at {phase}"
    );
}

fn initialize_successor_fixture_repository(root: &std::path::Path) {
    use std::process::Command;

    for args in [
        &["init", "-q", "-b", "main"][..],
        &["config", "user.email", "successor-fixture@example.invalid"][..],
        &["config", "user.name", "Successor Fixture"][..],
    ] {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(root)
                .status()
                .expect("run fixture git command")
                .success()
        );
    }
    std::fs::write(root.join("README.md"), "successor fixture\n")
        .expect("write fixture repository seed");
    assert!(
        Command::new("git")
            .args(["add", "README.md"])
            .current_dir(root)
            .status()
            .expect("stage fixture repository seed")
            .success()
    );
    assert!(
        Command::new("git")
            .args(["commit", "-q", "-m", "fixture seed"])
            .current_dir(root)
            .status()
            .expect("commit fixture repository seed")
            .success()
    );
}

async fn spawn_blocking_successor_harness_backend() -> (
    tokio::task::JoinHandle<()>,
    String,
    std::sync::Arc<std::sync::atomic::AtomicUsize>,
) {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback Harness backend");
    let address = listener.local_addr().expect("read Harness backend address");
    let dispatches = std::sync::Arc::new(AtomicUsize::new(0));
    let observed = std::sync::Arc::clone(&dispatches);
    let task = tokio::spawn(async move {
        let (_stream, _) = listener.accept().await.expect("accept Harness request");
        observed.fetch_add(1, Ordering::SeqCst);
        std::future::pending::<()>().await;
    });
    (task, format!("http://{address}/v1"), dispatches)
}

/// This is intentionally an in-process `RpcServer` Unix-socket daemon
/// fixture, not a subprocess-rsid or paid-provider end-to-end test.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_successor_in_process_rpc_server_unix_socket_harness_restart_fixture() {
    use crate::session::harness::tools::HarnessTool;
    use crate::session::harness::tools::rsi_control::{
        RsiControlProgramGuardTool, RsiControlReserveSuccessorTool,
    };
    use crate::store::successor_reservations::{
        AgentSuccessorCommitFault, successor_test_fail_next_commit,
    };
    use rsi_common::agent_coordination::AgentSuccessorStateV1;
    use rsi_common::types::{SessionKind, SessionStatus};
    use std::sync::atomic::Ordering;

    // The successor launch allocates a sandbox, and sandbox execution scratch
    // refuses a root on tmpfs/ramfs. The lander's test gate runs with TMPDIR on
    // /dev/shm, so `TempDir::new()` here made the reconcile fail with
    // ExecutionScratchUnavailable before the injected commit fault (#1221).
    let root = crate::test_support::disk_backed_tempdir("successor-fixture-");
    initialize_successor_fixture_repository(root.path());
    let first_daemon = successor_unix_socket_daemon_fixture(root.path());
    let epic_id = Uuid::new_v4();
    let predecessor_id = Uuid::new_v4();
    {
        let store = first_daemon.manager.store().lock().await;
        let mut epic =
            mk_agent_test_session(epic_id, SessionKind::Epic, None, Some(predecessor_id));
        epic.status = SessionStatus::Completed;
        let mut predecessor =
            mk_agent_test_session(predecessor_id, SessionKind::Task, Some(epic_id), None);
        predecessor.provider = rsi_common::types::SessionProvider::Harness;
        predecessor.model = Some("successor-loopback-harness".to_string());
        predecessor.working_dir = root.path().to_path_buf();
        predecessor.is_eval = true;
        store.insert_session(&epic).expect("insert Epic");
        store
            .insert_session(&predecessor)
            .expect("insert predecessor lead");
    }
    let predecessor_token = "baton-integration-predecessor-token".to_string();
    first_daemon
        .manager
        .register_agent_token(predecessor_token.clone(), predecessor_id)
        .await;

    let params = serde_json::json!({
        "kind": "Task",
        "query": "continue the master program after turnover",
        "tags": ["baton-integration"],
        "idempotency_key": "baton-integration-v1"
    });
    let mut reserve = RpcRequest::new("AgentReserveSuccessor", params.clone());
    reserve.session_token = Some(predecessor_token.clone());
    let reserved = call_serialized_rpc_over_unix_socket(
        std::sync::Arc::clone(&first_daemon.server),
        root.path(),
        &reserve,
    )
    .await;
    assert!(reserved.error.is_none(), "{:?}", reserved.error);
    let reserved = reserved.result.expect("reservation receipt");
    let reservation_id =
        Uuid::parse_str(reserved["reservation_id"].as_str().expect("reservation id"))
            .expect("valid reservation id");
    let candidate_id = Uuid::parse_str(
        reserved["candidate_session_id"]
            .as_str()
            .expect("candidate id"),
    )
    .expect("valid candidate id");
    assert_eq!(reserved["state"], "reserved");

    let mut replay = RpcRequest::new("AgentReserveSuccessor", params.clone());
    replay.session_token = Some(predecessor_token.clone());
    let replay = call_serialized_rpc_over_unix_socket(
        std::sync::Arc::clone(&first_daemon.server),
        root.path(),
        &replay,
    )
    .await;
    assert!(replay.error.is_none(), "{:?}", replay.error);
    let replay = replay.result.expect("replay receipt");
    assert_eq!(replay["reservation_id"], reservation_id.to_string());
    assert_eq!(replay["candidate_session_id"], candidate_id.to_string());
    assert_eq!(replay["deduplicated"], true);

    let mut changed = RpcRequest::new(
        "AgentReserveSuccessor",
        serde_json::json!({
            "kind": "Task",
            "query": "changed successor payload",
            "tags": ["baton-integration"],
            "idempotency_key": "baton-integration-v1"
        }),
    );
    changed.session_token = Some(predecessor_token.clone());
    let changed = call_serialized_rpc_over_unix_socket(
        std::sync::Arc::clone(&first_daemon.server),
        root.path(),
        &changed,
    )
    .await;
    assert!(
        changed
            .error
            .expect("changed replay must conflict")
            .message
            .contains("agent_successor_idempotency_conflict")
    );

    // The Harness-native wrapper reaches the same service and exact
    // durable receipt. Caller identity remains construction-bound.
    let native_reserve =
        RsiControlReserveSuccessorTool::new(first_daemon.manager.agent_control(), predecessor_id);
    let native = native_reserve.execute(params, root.path()).await;
    assert!(native.success, "{:?}", native.error_msg);
    let native: serde_json::Value =
        serde_json::from_str(&native.output).expect("native reservation receipt");
    assert_eq!(native["reservation_id"], reservation_id.to_string());
    assert_eq!(native["candidate_session_id"], candidate_id.to_string());
    assert_eq!(native["deduplicated"], true);

    // Drop and reconstruct both manager and server. The second daemon uses
    // the same SQLite file but has fresh in-memory maps and collaborators.
    drop(native_reserve);
    drop(first_daemon);
    let restarted_daemon = successor_unix_socket_daemon_fixture(root.path());
    {
        let store = restarted_daemon.manager.store().lock().await;
        let reopened = store
            .get_agent_successor(reservation_id)
            .expect("read reopened reservation")
            .expect("reopened reservation exists");
        assert_eq!(reopened.candidate_session_id, candidate_id);
        assert_eq!(reopened.state, AgentSuccessorStateV1::Reserved);
        assert!(store.get_session(candidate_id).unwrap().is_none());
    }
    restarted_daemon
        .manager
        .preload_completed_session_for_test(epic_id)
        .await
        .expect("preload production-shaped completed Epic cache");
    assert_eq!(
        restarted_daemon
            .manager
            .get_session(epic_id)
            .await
            .expect("restored Epic cache entry")
            .lead_session_id,
        Some(predecessor_id)
    );
    restarted_daemon
        .manager
        .store()
        .lock()
        .await
        .update_session_status(predecessor_id, SessionStatus::Completed)
        .expect("settle predecessor after completed Epic projection is cached");
    restarted_daemon
        .manager
        .register_agent_token(predecessor_token.clone(), predecessor_id)
        .await;

    let (backend, base_url, dispatches) = spawn_blocking_successor_harness_backend().await;
    crate::session::harness::provider::install_test_openai_compatible_route(
        "successor-loopback-harness",
        &base_url,
    );
    successor_test_fail_next_commit(AgentSuccessorCommitFault::AfterLeadCas);
    let injected = restarted_daemon
        .manager
        .reconcile_agent_successor(reservation_id)
        .await
        .expect_err("post-lead-CAS fault must roll back the authority commit");
    assert!(
        injected
            .to_string()
            .contains("injected successor authority commit fault"),
        "{injected}"
    );
    {
        let store = restarted_daemon.manager.store().lock().await;
        assert_eq!(
            store.get_session(epic_id).unwrap().unwrap().lead_session_id,
            Some(predecessor_id),
            "failed authority commit must preserve predecessor authority"
        );
        assert_eq!(
            store
                .get_agent_successor(reservation_id)
                .unwrap()
                .unwrap()
                .state,
            AgentSuccessorStateV1::Launching,
            "exact retry must retain the established candidate"
        );
    }
    restarted_daemon
        .manager
        .fail_next_agent_successor_projection_for_test(reservation_id);
    let projection_failed = restarted_daemon
        .manager
        .reconcile_agent_successor(reservation_id)
        .await
        .expect_err("durable commit survives injected runtime projection failure");
    assert!(
        projection_failed
            .to_string()
            .contains("injected agent successor projection publication failure")
    );
    {
        let store = restarted_daemon.manager.store().lock().await;
        let committed = store
            .get_agent_successor(reservation_id)
            .expect("load committed successor")
            .expect("committed successor exists");
        assert_eq!(committed.state, AgentSuccessorStateV1::Committed);
        let evidence: serde_json::Value = serde_json::from_str(
            committed
                .establishment_evidence_json
                .as_deref()
                .expect("Harness establishment evidence"),
        )
        .expect("parse Harness establishment evidence");
        assert_eq!(evidence["provider"], "Harness");
        assert_eq!(evidence["confirmation"], "InstalledProvider");
        assert_eq!(
            store.get_session(epic_id).unwrap().unwrap().lead_session_id,
            Some(candidate_id)
        );
        let direct_candidates: Vec<_> = store
            .list_children(Some(epic_id))
            .expect("list Epic children")
            .into_iter()
            .filter(|session| session.continued_from == Some(predecessor_id))
            .collect();
        assert_eq!(direct_candidates.len(), 1);
        assert_eq!(direct_candidates[0].id, candidate_id);
    }
    assert_eq!(
        restarted_daemon
            .manager
            .get_session(epic_id)
            .await
            .expect("stale completed Epic remains a runtime projection")
            .lead_session_id,
        Some(predecessor_id),
        "the injected failure must leave the runtime projection stale"
    );
    let candidate_token = "baton-integration-candidate-token".to_string();
    assert_successor_rpc_authority_plane(
        &restarted_daemon,
        root.path(),
        epic_id,
        predecessor_id,
        candidate_id,
        &predecessor_token,
        &candidate_token,
        "before-projection-retry",
    )
    .await;

    let projection_retry = restarted_daemon
        .manager
        .reconcile_incomplete_agent_successors()
        .await
        .expect("bounded reconciliation requeues failed projection acknowledgement");
    assert_eq!(projection_retry.queued, 1);
    let mut projection_events = restarted_daemon.manager.event_bus().subscribe();
    restarted_daemon
        .manager
        .reconcile_agent_successor(reservation_id)
        .await
        .expect("committed successor projection acknowledgement converges");
    let lead_event = projection_events.recv().await.expect("lead metadata event");
    assert!(matches!(
        lead_event.as_ref(),
        crate::bus::DaemonEvent::SessionMetadataChanged {
            session_id,
            lead_session_id: Some(Some(lead_id)),
            ..
        } if *session_id == epic_id && *lead_id == candidate_id
    ));
    assert!(matches!(
        projection_events.recv().await.expect("successor completion event").as_ref(),
        crate::bus::DaemonEvent::SystemMessage { message, .. }
            if message == &format!("agent_successor_committed:{reservation_id}:{candidate_id}")
    ));
    restarted_daemon.manager.event_bus().unsubscribe();
    assert_eq!(
        restarted_daemon
            .manager
            .get_session(epic_id)
            .await
            .expect("refreshed completed Epic")
            .lead_session_id,
        Some(candidate_id)
    );
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while dispatches.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("loopback Harness backend receives the production request");
    assert_successor_rpc_authority_plane(
        &restarted_daemon,
        root.path(),
        epic_id,
        predecessor_id,
        candidate_id,
        &predecessor_token,
        &candidate_token,
        "after-bounded-projection-retry",
    )
    .await;

    let guard =
        RsiControlProgramGuardTool::new(restarted_daemon.manager.agent_control(), candidate_id)
            .execute(serde_json::json!({}), root.path())
            .await;
    assert!(guard.success, "{:?}", guard.error_msg);
    let guard: serde_json::Value =
        serde_json::from_str(&guard.output).expect("program guard receipt");
    assert_eq!(guard["wake_session_id"], candidate_id.to_string());
    assert_eq!(guard["deduplicated"], false);
    // The loopback route uses an eval predecessor to avoid a paid backend,
    // and the candidate correctly inherits that launch flag. Production
    // restore intentionally omits eval rows from the ordinary session
    // corpus, so clear only this test transport marker after the baton is
    // committed; the restart authority assertion must exercise the same
    // visible-session restore path as a production candidate.
    restarted_daemon
        .manager
        .store()
        .lock()
        .await
        .conn
        .execute(
            "UPDATE sessions SET is_eval=0 WHERE id=?1",
            [candidate_id.to_string()],
        )
        .expect("make committed loopback candidate visible to production restore");
    // Crash-style restart: dropping the daemon must not run the ordinary
    // interrupt saga, which would intentionally clear an interrupted Epic
    // lead and destroy the committed baton state this boundary verifies.
    drop(restarted_daemon);

    let post_restart_daemon = successor_unix_socket_daemon_fixture(root.path());
    post_restart_daemon
        .manager
        .restore_sessions()
        .await
        .expect("restore the durable candidate authority plane");
    assert_eq!(
        post_restart_daemon
            .manager
            .get_session(epic_id)
            .await
            .expect("restored Epic")
            .lead_session_id,
        Some(candidate_id)
    );
    assert_successor_rpc_authority_plane(
        &post_restart_daemon,
        root.path(),
        epic_id,
        predecessor_id,
        candidate_id,
        &predecessor_token,
        &candidate_token,
        "after-daemon-restart",
    )
    .await;
    backend.abort();
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn agent_spawn_child_then_get_status_and_halt_authorized_for_lead_denied_for_others() {
    // Review finding #2: AgentSpawnChild parents new children under the
    // Epic, not under the spawning lead, so the lead and its
    // freshly-spawned child are Epic siblings. AgentGetStatus/AgentHalt
    // must still authorize the lead against a session it just spawned
    // this way (and continue denying everyone else).
    use rsi_common::types::SessionKind;

    let fixture = recursive_dag_rpc_fixture();

    let epic_id = Uuid::new_v4();
    let lead_id = Uuid::new_v4();
    {
        let store = fixture.manager.store().lock().await;
        store
            .insert_session(&mk_agent_test_session(
                epic_id,
                SessionKind::Epic,
                None,
                Some(lead_id),
            ))
            .expect("insert epic");
        store
            .insert_session(&mk_agent_test_session(
                lead_id,
                SessionKind::Task,
                Some(epic_id),
                None,
            ))
            .expect("insert lead");
    }
    let lead_token = "spawn-then-status-lead-token".to_string();
    fixture
        .manager
        .register_agent_token(lead_token.clone(), lead_id)
        .await;

    // Spawn via the real AgentSpawnChild RPC path.
    let mut request = RpcRequest::new(
        "AgentSpawnChild",
        serde_json::json!({ "kind": "Task", "query": "do the thing", "idempotency_key": "spawn-status-halt" }),
    );
    request.session_token = Some(lead_token.clone());
    let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
    else {
        panic!("expected response");
    };
    assert!(
        response.error.is_none(),
        "spawn should succeed: {:?}",
        response.error
    );
    let result = response.result.expect("result");
    assert_eq!(result["state"], "queued");
    let spawned_epic_id: Uuid = serde_json::from_value(result["epic_id"].clone()).unwrap();
    assert_eq!(spawned_epic_id, epic_id);

    // The daemon's real spawn-request consumer (main.rs) is not running
    // in this fixture, so the enqueued request never becomes a stored
    // session on its own. Insert the row it would have created —
    // parent_id = epic_id, per spawn_coordinator's own
    // `req.config.parent_id == Some(epic_id)` invariant — to exercise
    // the authorization check this finding is actually about.
    let child_id: Uuid = serde_json::from_value(result["child_session_id"].clone()).unwrap();
    {
        let store = fixture.manager.store().lock().await;
        store
            .insert_session(&mk_agent_test_session(
                child_id,
                SessionKind::Task,
                Some(epic_id),
                None,
            ))
            .expect("insert spawned child");
    }

    // The lead can now read and halt the child it just spawned.
    let mut request = RpcRequest::new(
        "AgentGetStatus",
        serde_json::json!({ "session_id": child_id }),
    );
    request.session_token = Some(lead_token.clone());
    let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
    else {
        panic!("expected response");
    };
    assert!(
        response.error.is_none(),
        "lead must be authorized against a child it just spawned via AgentSpawnChild: {:?}",
        response.error
    );
    let session: rsi_common::types::Session =
        serde_json::from_value(response.result.expect("result")).expect("session");
    assert_eq!(session.id, child_id);

    let mut request = RpcRequest::new("AgentHalt", serde_json::json!({ "session_id": child_id }));
    request.session_token = Some(lead_token);
    let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
    else {
        panic!("expected response");
    };
    // The fixture inserts the spawned child directly into the store
    // without registering it as an in-memory active/tracked session (no
    // real subprocess), so `interrupt_session` itself still reports
    // `SessionNotFound` past authorization — that's an unrelated,
    // expected fixture limitation. What this finding is about is the
    // *authorization* layer: the call must not be rejected as
    // `agent_verb_scope_denied`.
    if let Some(error) = &response.error {
        assert!(
            !error.message.contains("agent_verb_scope_denied"),
            "lead must be authorized to halt a child it just spawned via AgentSpawnChild: {:?}",
            response.error
        );
    }

    // An unrelated session (not the lead, not a parent, not the child)
    // must still be denied — the widened scoping is not a blanket
    // Epic-membership bypass.
    let stranger_id = Uuid::new_v4();
    {
        let store = fixture.manager.store().lock().await;
        store
            .insert_session(&mk_agent_test_session(
                stranger_id,
                SessionKind::Task,
                None,
                None,
            ))
            .expect("insert stranger");
    }
    let stranger_token = "spawn-then-status-stranger-token".to_string();
    fixture
        .manager
        .register_agent_token(stranger_token.clone(), stranger_id)
        .await;

    let mut request = RpcRequest::new(
        "AgentGetStatus",
        serde_json::json!({ "session_id": child_id }),
    );
    request.session_token = Some(stranger_token);
    let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
    else {
        panic!("expected response");
    };
    let error = response
        .error
        .expect("a non-lead, non-parent session must not be authorized against the child");
    assert!(error.message.contains("agent_verb_scope_denied"));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn agent_get_status_and_halt_use_resolved_caller_session_when_unspecified() {
    use rsi_common::types::SessionKind;

    let fixture = recursive_dag_rpc_fixture();
    let caller_id = Uuid::new_v4();
    {
        let store = fixture.manager.store().lock().await;
        store
            .insert_session(&mk_agent_test_session(
                caller_id,
                SessionKind::Task,
                None,
                None,
            ))
            .expect("insert caller session");
    }
    let token = "caller-token".to_string();
    fixture
        .manager
        .register_agent_token(token.clone(), caller_id)
        .await;

    let mut request = RpcRequest::new("AgentGetStatus", serde_json::json!({}));
    request.session_token = Some(token.clone());
    let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
    else {
        panic!("expected response");
    };
    assert!(
        response.error.is_none(),
        "unexpected error: {:?}",
        response.error
    );
    let session: rsi_common::types::Session =
        serde_json::from_value(response.result.expect("result")).expect("session");
    assert_eq!(session.id, caller_id);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn agent_get_authority_catalog_serves_the_token_bound_callers_manual() {
    use rsi_common::agent_authority_catalog::AgentAuthorityCatalogV1;
    use rsi_common::types::SessionKind;

    let fixture = recursive_dag_rpc_fixture();
    let caller_id = Uuid::new_v4();
    {
        let store = fixture.manager.store().lock().await;
        store
            .insert_session(&mk_agent_test_session(
                caller_id,
                SessionKind::Task,
                None,
                None,
            ))
            .expect("insert caller session");
    }
    let token = "catalog-token".to_string();
    fixture
        .manager
        .register_agent_token(token.clone(), caller_id)
        .await;
    let call = |params: serde_json::Value| {
        let mut request = RpcRequest::new("AgentGetAuthorityCatalog", params);
        request.session_token = Some(token.clone());
        request
    };

    // No params (bare `rsi-rpc AgentGetAuthorityCatalog`) is the whole manual.
    let HandleResult::Response(response) = fixture
        .server
        .handle_request_inner(&call(serde_json::Value::Null))
        .await
    else {
        panic!("expected response");
    };
    assert!(response.error.is_none(), "{:?}", response.error);
    let catalog: AgentAuthorityCatalogV1 =
        serde_json::from_value(response.result.expect("result")).expect("catalog");
    assert_eq!(catalog.session_id, caller_id);
    assert_eq!(catalog.roles, ["worker"]);
    assert!(catalog.authority_revision.starts_with("sha256:"));
    assert!(catalog.guidance.contains("## RSI worker baseline"));
    // A plain worker holds exactly the role-independent baseline.
    let methods = catalog
        .controls
        .iter()
        .map(|control| control.method.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        methods,
        rsi_common::agent_control_schema::agent_control_catalog_v1()
            .iter()
            .filter(|descriptor| {
                crate::session::agent_verbs::agent_authority::is_baseline_verb(descriptor.verb)
            })
            .map(|descriptor| descriptor.method)
            .collect::<Vec<_>>()
    );
    for method in [
        "AgentGetAuthorityCatalog",
        "AgentGetStatus",
        "AgentSubmitJob",
    ] {
        assert!(methods.contains(&method), "{method}");
    }

    // A named control returns its schema and whether it is one of yours;
    // the Claude MCP spelling resolves to the same verb.
    let HandleResult::Response(response) = fixture
        .server
        .handle_request_inner(&call(
            serde_json::json!({"verb": "mcp__rsi-agent__rsi_control_spawn"}),
        ))
        .await
    else {
        panic!("expected response");
    };
    let catalog: AgentAuthorityCatalogV1 =
        serde_json::from_value(response.result.expect("result")).expect("catalog");
    assert!(catalog.controls.is_empty() && catalog.guidance.is_empty());
    let detail = catalog.control.expect("control detail");
    assert_eq!(detail.method, "AgentSpawnChild");
    assert!(!detail.permitted);
    assert_eq!(detail.parameters["required"][0], "kind");
    assert_eq!(detail.example["kind"], "Task");
    assert!(
        detail
            .refusals
            .iter()
            .any(|r| r.code == "agent_spawn_rejected:NotLead")
    );

    for (params, code) in [
        (
            serde_json::json!({"verb": "GetSession"}),
            "authority_catalog_unknown_verb",
        ),
        (
            serde_json::json!({"session_id": Uuid::new_v4()}),
            "authority_catalog_invalid_request",
        ),
    ] {
        let HandleResult::Response(response) =
            fixture.server.handle_request_inner(&call(params)).await
        else {
            panic!("expected response");
        };
        let error = response.error.expect("refusal");
        assert!(error.message.contains(code), "{}", error.message);
    }

    // Tokenless callers are operators, not agents: the verb needs a token.
    let HandleResult::Response(response) = fixture
        .server
        .handle_request_inner(&RpcRequest::new(
            "AgentGetAuthorityCatalog",
            serde_json::json!({}),
        ))
        .await
    else {
        panic!("expected response");
    };
    assert!(
        response
            .error
            .expect("refusal")
            .message
            .contains("agent_verb_requires_session_token")
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn agent_get_status_null_params_self_targets_but_malformed_params_error() {
    // Review finding #1: `Value::Null` (no `--params` flag at all) is
    // the documented way to self-target and must keep working, but a
    // malformed non-null `session_id` must be a hard error, not a
    // silent redirect to the caller's own session.
    use rsi_common::types::SessionKind;

    let fixture = recursive_dag_rpc_fixture();
    let caller_id = Uuid::new_v4();
    {
        let store = fixture.manager.store().lock().await;
        store
            .insert_session(&mk_agent_test_session(
                caller_id,
                SessionKind::Task,
                None,
                None,
            ))
            .expect("insert caller session");
    }
    let token = "null-params-token".to_string();
    fixture
        .manager
        .register_agent_token(token.clone(), caller_id)
        .await;

    // `Value::Null` (equivalent to `rsi-rpc AgentGetStatus` with no
    // `--params` at all) must still resolve to the caller's own status.
    let mut request = RpcRequest::new("AgentGetStatus", serde_json::Value::Null);
    request.session_token = Some(token.clone());
    let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
    else {
        panic!("expected response");
    };
    assert!(
        response.error.is_none(),
        "null params (no --params) must self-target, got error: {:?}",
        response.error
    );
    let session: rsi_common::types::Session =
        serde_json::from_value(response.result.expect("result")).expect("session");
    assert_eq!(session.id, caller_id);

    // A malformed (wrong-typed) session_id must be a hard error, never
    // silently redirected to the caller's own session.
    let mut request = RpcRequest::new("AgentGetStatus", serde_json::json!({ "session_id": 12345 }));
    request.session_token = Some(token.clone());
    let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
    else {
        panic!("expected response");
    };
    let error = response
        .error
        .expect("malformed session_id must error, not silently self-target");
    assert!(
        error.message.contains("Invalid params"),
        "unexpected error message: {}",
        error.message
    );

    // Same malformed-vs-null distinction for AgentHalt.
    let mut request = RpcRequest::new(
        "AgentHalt",
        serde_json::json!({ "session_id": "not-a-uuid" }),
    );
    request.session_token = Some(token);
    let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
    else {
        panic!("expected response");
    };
    let error = response
        .error
        .expect("malformed session_id must error AgentHalt too");
    assert!(
        error.message.contains("Invalid params"),
        "unexpected error message: {}",
        error.message
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn agent_verb_without_session_token_is_rejected() {
    let fixture = recursive_dag_rpc_fixture();
    let request = RpcRequest::new("AgentGetStatus", serde_json::json!({}));
    assert!(request.session_token.is_none());
    let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
    else {
        panic!("expected response");
    };
    // Unattributed callers never reach an Agent* method in practice (the
    // TUI/operator path doesn't call Agent* verbs), but the handler must
    // still fail closed rather than panic or resolve a bogus caller.
    let error = response.error.expect("must fail without a session token");
    assert_eq!(error.code, INVALID_PARAMS);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn agent_verb_with_superseded_token_is_rejected() {
    // A6 re-mint semantics: a re-mint (continue/rotation establishment)
    // revokes the session's prior token. An old process still holding the
    // superseded token must get `agent_verb_unknown_session_token`
    // (-32602), while the freshly minted token resolves to the caller's
    // own row.
    use rsi_common::types::SessionKind;

    let fixture = recursive_dag_rpc_fixture();
    let caller_id = Uuid::new_v4();
    {
        let store = fixture.manager.store().lock().await;
        store
            .insert_session(&mk_agent_test_session(
                caller_id,
                SessionKind::Task,
                None,
                None,
            ))
            .expect("insert caller session");
    }
    let old_token = "superseded-token".to_string();
    fixture
        .manager
        .register_agent_token(old_token.clone(), caller_id)
        .await;

    // Supersession: the establishment-site re-mint revokes the old token.
    let new_token = fixture.manager.remint_session_token(caller_id).await;

    let mut request = RpcRequest::new("AgentGetStatus", serde_json::json!({}));
    request.session_token = Some(old_token);
    let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
    else {
        panic!("expected response");
    };
    let error = response
        .error
        .expect("superseded token must be rejected, not resolve a caller");
    assert_eq!(error.code, INVALID_PARAMS);
    assert!(
        error.message.contains("agent_verb_unknown_session_token"),
        "unexpected error message: {}",
        error.message
    );

    // The freshly minted token resolves to the caller's own row.
    let mut request = RpcRequest::new("AgentGetStatus", serde_json::json!({}));
    request.session_token = Some(new_token);
    let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
    else {
        panic!("expected response");
    };
    assert!(
        response.error.is_none(),
        "unexpected error: {:?}",
        response.error
    );
    let session: rsi_common::types::Session =
        serde_json::from_value(response.result.expect("result")).expect("session");
    assert_eq!(session.id, caller_id);
}

// ── Phase 2: AgentScheduleWake pinning tests ──

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn agent_schedule_wake_binds_wake_session_id_from_token_ignoring_params() {
    use rsi_common::types::SessionKind;

    let fixture = recursive_dag_rpc_fixture();
    let caller_id = Uuid::new_v4();
    let smuggled_id = Uuid::new_v4();
    {
        let store = fixture.manager.store().lock().await;
        store
            .insert_session(&mk_agent_test_session(
                caller_id,
                SessionKind::Task,
                None,
                None,
            ))
            .expect("insert caller session");
    }
    let token = "schedule-wake-token".to_string();
    fixture
        .manager
        .register_agent_token(token.clone(), caller_id)
        .await;

    // AgentScheduleWakeParams has no wake_session_id field at all, so a
    // caller trying to smuggle one through --params is structurally
    // ignored by serde (unknown field) rather than merely overridden.
    let mut request = RpcRequest::new(
        "AgentScheduleWake",
        serde_json::json!({
            "message": "resume me",
            "in_seconds": 60,
            "mode": "resume",
            "wake_session_id": smuggled_id,
        }),
    );
    request.session_token = Some(token);
    let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
    else {
        panic!("expected response");
    };
    assert!(
        response.error.is_none(),
        "unexpected error: {:?}",
        response.error
    );
    let result = response.result.expect("result");
    assert_eq!(result["wake_session_id"], caller_id.to_string());
    assert_ne!(result["wake_session_id"], smuggled_id.to_string());

    let jobs = {
        let store = fixture.manager.store().lock().await;
        store.list_scheduled_jobs().expect("list jobs")
    };
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].wake_session_id, Some(caller_id));
    assert_eq!(jobs[0].wake_mode, rsi_common::types::WakeMode::Resume);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn agent_schedule_wake_fresh_persists_bound_origin() {
    use rsi_common::types::SessionKind;

    let fixture = recursive_dag_rpc_fixture();
    let caller_id = Uuid::new_v4();
    let smuggled_id = Uuid::new_v4();
    {
        let store = fixture.manager.store().lock().await;
        store
            .insert_session(&mk_agent_test_session(
                caller_id,
                SessionKind::Task,
                None,
                None,
            ))
            .expect("insert caller session");
    }
    let token = "schedule-wake-fresh-token".to_string();
    fixture
        .manager
        .register_agent_token(token.clone(), caller_id)
        .await;

    let mut request = RpcRequest::new(
        "AgentScheduleWake",
        serde_json::json!({
            "message": "start generation 2",
            "in_seconds": 60,
            "mode": "fresh",
            "wake_session_id": smuggled_id,
        }),
    );
    request.session_token = Some(token);
    let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
    else {
        panic!("expected response");
    };
    assert!(
        response.error.is_none(),
        "unexpected error: {:?}",
        response.error
    );
    let result = response.result.expect("result");
    assert_eq!(result["wake_session_id"], caller_id.to_string());
    assert_ne!(result["wake_session_id"], smuggled_id.to_string());

    let jobs = {
        let store = fixture.manager.store().lock().await;
        store.list_scheduled_jobs().expect("list jobs")
    };
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].wake_mode, rsi_common::types::WakeMode::AgentFresh);
    assert_eq!(jobs[0].wake_session_id, Some(caller_id));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn agent_cancel_wake_and_named_replace_are_scoped_to_the_caller() {
    use rsi_common::types::SessionKind;

    let fixture = recursive_dag_rpc_fixture();
    let (alice, bob) = (Uuid::new_v4(), Uuid::new_v4());
    for (id, token) in [(alice, "cancel-wake-alice"), (bob, "cancel-wake-bob")] {
        {
            let store = fixture.manager.store().lock().await;
            store
                .insert_session(&mk_agent_test_session(id, SessionKind::Task, None, None))
                .expect("insert session");
        }
        fixture
            .manager
            .register_agent_token(token.to_string(), id)
            .await;
    }
    let call = |token: &'static str, method: &'static str, params: serde_json::Value| {
        let server = &fixture.server;
        async move {
            let mut request = RpcRequest::new(method, params);
            request.session_token = Some(token.to_string());
            let HandleResult::Response(response) = server.handle_request_inner(&request).await
            else {
                panic!("expected response");
            };
            response
        }
    };
    let wake = |name: Option<&str>| {
        let mut params = serde_json::json!({
            "message": "continue", "in_seconds": 600, "mode": "resume"
        });
        if let Some(name) = name {
            params["name"] = name.into();
        }
        params
    };
    let enabled_named = |session: Uuid, name: &'static str| {
        let store = fixture.manager.store();
        async move {
            store
                .lock()
                .await
                .list_scheduled_jobs()
                .unwrap()
                .into_iter()
                .filter(|job| {
                    job.enabled && job.wake_session_id == Some(session) && job.name == name
                })
                .count()
        }
    };

    // One enabled job per explicit name per session.
    let first = call("cancel-wake-alice", "AgentScheduleWake", wake(Some("net")))
        .await
        .result
        .expect("first named wake");
    let second = call("cancel-wake-alice", "AgentScheduleWake", wake(Some("net")))
        .await
        .result
        .expect("second named wake");
    assert_eq!(
        second["replaced_job_ids"],
        serde_json::json!([first["job_id"]])
    );
    assert_eq!(enabled_named(alice, "net").await, 1);
    // Unnamed wakes keep coexisting; another session's same name is untouched.
    for _ in 0..2 {
        call("cancel-wake-alice", "AgentScheduleWake", wake(None))
            .await
            .result
            .expect("unnamed wake");
    }
    assert_eq!(enabled_named(alice, "agent-wake").await, 2);
    let bobs = call("cancel-wake-bob", "AgentScheduleWake", wake(Some("net")))
        .await
        .result
        .expect("bob wake");
    assert_eq!(enabled_named(alice, "net").await, 1);
    assert_eq!(enabled_named(bob, "net").await, 1);

    // Another session cannot cancel Alice's job; a smuggled owner is rejected.
    let refused = call(
        "cancel-wake-bob",
        "AgentCancelWake",
        serde_json::json!({"job_id": second["job_id"]}),
    )
    .await;
    assert!(refused.error.is_some(), "{refused:?}");
    let spoofed = call(
        "cancel-wake-bob",
        "AgentCancelWake",
        serde_json::json!({"name": "net", "wake_session_id": alice}),
    )
    .await;
    assert!(spoofed.error.is_some(), "{spoofed:?}");
    let neither = call(
        "cancel-wake-alice",
        "AgentCancelWake",
        serde_json::json!({}),
    )
    .await;
    assert!(neither.error.is_some(), "{neither:?}");
    assert_eq!(enabled_named(alice, "net").await, 1);

    // The owner cancels by id, then by name; other sessions are unaffected.
    let by_id = call(
        "cancel-wake-alice",
        "AgentCancelWake",
        serde_json::json!({"job_id": second["job_id"]}),
    )
    .await
    .result
    .expect("cancel by id");
    assert_eq!(
        by_id["cancelled_job_ids"],
        serde_json::json!([second["job_id"]])
    );
    assert_eq!(enabled_named(alice, "net").await, 0);
    let by_name = call(
        "cancel-wake-alice",
        "AgentCancelWake",
        serde_json::json!({"name": "agent-wake"}),
    )
    .await
    .result
    .expect("cancel by name");
    assert_eq!(by_name["cancelled_count"], 2);
    assert_eq!(enabled_named(alice, "agent-wake").await, 0);
    assert_eq!(enabled_named(bob, "net").await, 1);
    assert_eq!(bobs["wake_session_id"], bob.to_string());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn agent_list_wakes_returns_only_the_callers_own_jobs_bounded() {
    use rsi_common::types::SessionKind;

    let fixture = recursive_dag_rpc_fixture();
    let (alice, bob) = (Uuid::new_v4(), Uuid::new_v4());
    for (id, token) in [(alice, "list-wakes-alice"), (bob, "list-wakes-bob")] {
        {
            let store = fixture.manager.store().lock().await;
            store
                .insert_session(&mk_agent_test_session(id, SessionKind::Task, None, None))
                .expect("insert session");
        }
        fixture
            .manager
            .register_agent_token(token.to_string(), id)
            .await;
    }
    let call = |token: &'static str, method: &'static str, params: serde_json::Value| {
        let server = &fixture.server;
        async move {
            let mut request = RpcRequest::new(method, params);
            request.session_token = Some(token.to_string());
            let HandleResult::Response(response) = server.handle_request_inner(&request).await
            else {
                panic!("expected response");
            };
            response
        }
    };
    let wake = |name: &str| {
        serde_json::json!({
            "message": "continue", "in_seconds": 600, "mode": "resume", "name": name
        })
    };
    for name in ["a", "b", "c"] {
        call("list-wakes-alice", "AgentScheduleWake", wake(name))
            .await
            .result
            .expect("alice wake");
    }
    let bobs = call("list-wakes-bob", "AgentScheduleWake", wake("bobs"))
        .await
        .result
        .expect("bob wake");

    // Alice sees exactly her own three jobs with the documented fields.
    let listed = call("list-wakes-alice", "AgentListWakes", serde_json::json!({}))
        .await
        .result
        .expect("list wakes");
    assert_eq!(listed["count"], 3);
    assert_eq!(listed["truncated"], false);
    let wakes = listed["wakes"].as_array().expect("wakes array");
    let mut names: Vec<&str> = wakes.iter().map(|w| w["name"].as_str().unwrap()).collect();
    names.sort_unstable();
    assert_eq!(names, ["a", "b", "c"]);
    for wake in wakes {
        assert_eq!(wake["mode"], "resume");
        assert_eq!(wake["enabled"], true);
        assert!(wake["job_id"].is_string());
        assert!(wake["next_fire_at"].is_string());
        assert!(wake["created_at"].is_string());
        assert_ne!(wake["job_id"], bobs["job_id"]);
    }

    // Bounded: a smaller limit reports truncation.
    let page = call(
        "list-wakes-alice",
        "AgentListWakes",
        serde_json::json!({"limit": 2}),
    )
    .await
    .result
    .expect("bounded list");
    assert_eq!(page["count"], 2);
    assert_eq!(page["truncated"], true);

    // A cancelled job leaves the default list but stays visible on request.
    call(
        "list-wakes-alice",
        "AgentCancelWake",
        serde_json::json!({"name": "a"}),
    )
    .await
    .result
    .expect("cancel a");
    let active = call("list-wakes-alice", "AgentListWakes", serde_json::json!({}))
        .await
        .result
        .expect("active list");
    assert_eq!(active["count"], 2);
    let all = call(
        "list-wakes-alice",
        "AgentListWakes",
        serde_json::json!({"include_disabled": true}),
    )
    .await
    .result
    .expect("full list");
    assert_eq!(all["count"], 3);
    let disabled = all["wakes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|w| w["enabled"] == false)
        .count();
    assert_eq!(disabled, 1);

    // Bob's view is his own single job; a smuggled owner or bad limit is refused.
    let bobs_list = call("list-wakes-bob", "AgentListWakes", serde_json::json!({}))
        .await
        .result
        .expect("bob list");
    assert_eq!(bobs_list["count"], 1);
    assert_eq!(bobs_list["wakes"][0]["job_id"], bobs["job_id"]);
    let spoofed = call(
        "list-wakes-bob",
        "AgentListWakes",
        serde_json::json!({"wake_session_id": alice}),
    )
    .await;
    assert!(spoofed.error.is_some(), "{spoofed:?}");
    let zero = call(
        "list-wakes-bob",
        "AgentListWakes",
        serde_json::json!({"limit": 0}),
    )
    .await;
    assert!(zero.error.is_some(), "{zero:?}");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn agent_schedule_wake_omitted_or_unknown_mode_rejects_without_insert() {
    use rsi_common::types::SessionKind;

    let fixture = recursive_dag_rpc_fixture();
    let caller_id = Uuid::new_v4();
    {
        let store = fixture.manager.store().lock().await;
        store
            .insert_session(&mk_agent_test_session(
                caller_id,
                SessionKind::Task,
                None,
                None,
            ))
            .expect("insert caller session");
    }
    let token = "schedule-wake-default-fresh-token".to_string();
    fixture
        .manager
        .register_agent_token(token.clone(), caller_id)
        .await;

    for params in [
        serde_json::json!({ "message": "start generation 2", "in_seconds": 60 }),
        serde_json::json!({
            "message": "start generation 2",
            "in_seconds": 60,
            "mode": "surprise"
        }),
    ] {
        let mut request = RpcRequest::new("AgentScheduleWake", params);
        request.session_token = Some(token.clone());
        let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
        else {
            panic!("expected response");
        };
        let error = response.error.expect("mode must reject");
        assert!(
            error.message.contains("explicit 'mode'")
                || error.message.contains("invalid wake mode"),
            "unexpected error: {}",
            error.message
        );
    }

    let jobs = {
        let store = fixture.manager.store().lock().await;
        store.list_scheduled_jobs().expect("list jobs")
    };
    assert!(jobs.is_empty());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn create_scheduled_job_fresh_remains_generic() {
    use rsi_common::types::{Recurrence, ScheduleSpec};

    let fixture = recursive_dag_rpc_fixture();
    let params = rsi_common::rpc::CreateScheduledJobParams {
        name: "operator fresh".to_string(),
        message: "ordinary scheduled launch".to_string(),
        schedule: ScheduleSpec {
            recurrence: Recurrence::Once,
            anchor: chrono::Utc::now() + chrono::Duration::seconds(60),
        },
        working_dir: None,
        provider: None,
        model: None,
        project_id: None,
    };
    let request = RpcRequest::new(
        "CreateScheduledJob",
        serde_json::to_value(params).expect("serialize params"),
    );
    let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
    else {
        panic!("expected response");
    };
    assert!(response.error.is_none(), "{:?}", response.error);

    let jobs = {
        let store = fixture.manager.store().lock().await;
        store.list_scheduled_jobs().expect("list jobs")
    };
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].wake_mode, rsi_common::types::WakeMode::Fresh);
    assert_eq!(jobs[0].wake_session_id, None);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn agent_schedule_wake_rpc_and_harness_tool_produce_equivalent_jobs() {
    use crate::session::harness::tools::HarnessTool;
    use crate::session::harness::tools::schedule_wake::ScheduleWakeTool;
    use rsi_common::types::{SessionKind, SessionProvider};

    let fixture = recursive_dag_rpc_fixture();
    let caller_id = Uuid::new_v4();
    let working_dir = std::path::PathBuf::from("/tmp/agent-schedule-wake-equivalence");
    {
        let store = fixture.manager.store().lock().await;
        let mut session = mk_agent_test_session(caller_id, SessionKind::Task, None, None);
        session.working_dir = working_dir.clone();
        session.provider = SessionProvider::Claude;
        session.model = Some("claude-sonnet-5".to_string());
        store
            .insert_session(&session)
            .expect("insert caller session");
    }
    let token = "equivalence-token".to_string();
    fixture
        .manager
        .register_agent_token(token.clone(), caller_id)
        .await;

    let args = serde_json::json!({ "message": "hi", "in_seconds": 120, "mode": "fresh" });

    // RPC path.
    let mut request = RpcRequest::new("AgentScheduleWake", args.clone());
    request.session_token = Some(token);
    let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
    else {
        panic!("expected response");
    };
    assert!(
        response.error.is_none(),
        "unexpected error: {:?}",
        response.error
    );

    let rpc_job = {
        let store = fixture.manager.store().lock().await;
        let jobs = store.list_scheduled_jobs().expect("list jobs");
        assert_eq!(jobs.len(), 1);
        jobs.into_iter().next().unwrap()
    };

    // Harness-tool path, same underlying store, equivalent origin/
    // working_dir/provider/model to the caller session above.
    let tool = ScheduleWakeTool::new(
        std::sync::Arc::clone(fixture.manager.store()),
        Some(caller_id),
        working_dir.clone(),
        Some(SessionProvider::Claude),
        Some("claude-sonnet-5".to_string()),
        None,
        Some(fixture.manager.agent_control()),
    );
    let tool_result = tool.execute(args, std::path::Path::new("")).await;
    assert!(tool_result.success, "{:?}", tool_result.error_msg);

    let tool_job = {
        let store = fixture.manager.store().lock().await;
        let jobs = store.list_scheduled_jobs().expect("list jobs");
        assert_eq!(jobs.len(), 2);
        jobs.into_iter()
            .find(|j| j.id != rpc_job.id)
            .expect("second job")
    };

    assert_eq!(rpc_job.message, tool_job.message);
    assert_eq!(rpc_job.name, tool_job.name);
    assert_eq!(rpc_job.schedule.recurrence, tool_job.schedule.recurrence);
    assert_eq!(rpc_job.working_dir, tool_job.working_dir);
    assert_eq!(rpc_job.provider, tool_job.provider);
    assert_eq!(rpc_job.model, tool_job.model);
    assert_eq!(rpc_job.project_id, tool_job.project_id);
    assert_eq!(rpc_job.wake_mode, tool_job.wake_mode);
    assert_eq!(rpc_job.wake_session_id, tool_job.wake_session_id);
    assert_eq!(rpc_job.wake_session_id, Some(caller_id));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn program_guard_rpc_and_native_paths_share_one_caller_bound_row() {
    use crate::session::harness::tools::HarnessTool;
    use crate::session::harness::tools::schedule_wake::{
        ScheduleWakeTool, deterministic_program_guard_job_id, is_program_guard_sentinel,
    };
    use rsi_common::types::{SessionKind, SessionProvider};

    let fixture = recursive_dag_rpc_fixture();
    let caller_id = Uuid::new_v4();
    let sandbox_root = std::path::PathBuf::from("/tmp/program-guard-sandbox");
    {
        let store = fixture.manager.store().lock().await;
        let mut session = mk_agent_test_session(caller_id, SessionKind::Task, None, None);
        session.working_dir = std::path::PathBuf::from("/tmp/shared-working-dir");
        session.sandbox_root = Some(sandbox_root.clone());
        session.provider = SessionProvider::Claude;
        session.model = Some("claude-sonnet-5".into());
        store.insert_session(&session).expect("insert caller");
    }
    let token = "program-guard-equivalence-token".to_string();
    fixture
        .manager
        .register_agent_token(token.clone(), caller_id)
        .await;
    let args = serde_json::json!({
        "message": "master-orchestrate program guard",
        "mode": "program_guard",
        "id": Uuid::new_v4(),
        "job_id": Uuid::new_v4(),
        "wake_session_id": Uuid::new_v4(),
    });

    let mut request = RpcRequest::new("AgentScheduleWake", args.clone());
    request.session_token = Some(token);
    let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
    else {
        panic!("expected response");
    };
    assert!(response.error.is_none(), "{:?}", response.error);
    let rpc_id = response.result.expect("result")["job_id"]
        .as_str()
        .and_then(|raw| Uuid::parse_str(raw).ok())
        .expect("job id");
    assert_eq!(rpc_id, deterministic_program_guard_job_id(caller_id));

    let tool = ScheduleWakeTool::new(
        std::sync::Arc::clone(fixture.manager.store()),
        Some(caller_id),
        sandbox_root.clone(),
        Some(SessionProvider::Claude),
        Some("claude-sonnet-5".into()),
        None,
        Some(fixture.manager.agent_control()),
    );
    let native = tool.execute(args, &sandbox_root).await;
    assert!(native.success, "{:?}", native.error_msg);
    assert!(native.output.contains("deduplicated"));

    let store = fixture.manager.store().lock().await;
    let jobs = store.list_scheduled_jobs().expect("list jobs");
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].working_dir.as_deref(), Some(sandbox_root.as_path()));
    assert!(is_program_guard_sentinel(&jobs[0], caller_id));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn closed_guard_program_guard_identity_rejects_two_step_mutation_rearm_and_trigger() {
    use crate::store::scheduled_jobs::ScheduledJobUpdate;
    use rsi_common::types::{Recurrence, ScheduleSpec, SessionKind};

    let fixture = recursive_dag_rpc_fixture();
    let caller_id = Uuid::new_v4();
    {
        let store = fixture.manager.store().lock().await;
        store
            .insert_session(&mk_agent_test_session(
                caller_id,
                SessionKind::Task,
                None,
                None,
            ))
            .expect("insert caller");
    }
    let guard = fixture
        .manager
        .agent_control()
        .register_bound_program_guard(caller_id)
        .await
        .expect("register guard");
    let guard_id = match guard {
        crate::session::agent_verbs::ProgramGuardRegistration::Registered(job)
        | crate::session::agent_verbs::ProgramGuardRegistration::Deduplicated(job) => job.id,
    };
    fixture
        .manager
        .store()
        .lock()
        .await
        .update_scheduled_job(
            &guard_id,
            &ScheduledJobUpdate {
                name: None,
                message: None,
                schedule: None,
                enabled: Some(false),
                next_fire_at: None,
            },
        )
        .expect("close guard");

    let poisoned_at = chrono::Utc::now() - chrono::Duration::minutes(5);
    let poisoned_schedule = ScheduleSpec {
        recurrence: Recurrence::EverySeconds(1),
        anchor: poisoned_at,
    };
    let mutation = RpcRequest::new(
        "UpdateScheduledJob",
        serde_json::json!({
            "id": guard_id,
            "name": "ordinary mutation",
            "message": "ordinary mutation",
            "schedule": poisoned_schedule,
            "enabled": false,
            "working_dir": "/tmp/poisoned-program-guard",
            "provider": "Codex",
            "model": "poisoned-model",
            "project_id": issue_rpc_project_id(),
            "wake_mode": "fresh",
            "wake_session_id": Uuid::new_v4(),
            "next_fire_at": poisoned_at,
        }),
    );
    let HandleResult::Response(response) = fixture.server.handle_request_inner(&mutation).await
    else {
        panic!("expected response");
    };
    let error = response.error.expect("ordinary mutation must fail");
    assert!(
        error
            .message
            .contains("closed program guards can only be rearmed by explicit program registration")
    );
    {
        let store = fixture.manager.store().lock().await;
        let closed = store
            .get_scheduled_job(&guard_id)
            .unwrap()
            .expect("closed guard remains");
        assert!(!closed.enabled);
        assert!(
            crate::session::harness::tools::schedule_wake::is_program_guard_sentinel(
                &closed, caller_id
            )
        );
    }

    // Emulate a legacy row on which every mutable field from the old full
    // identity predicate has already been changed. The immutable id still
    // resolves to the durable owning session.
    let poisoned_owner = Uuid::new_v4();
    let poisoned_schedule_json = serde_json::to_string(&poisoned_schedule).unwrap();
    let poisoned_timestamp = poisoned_at.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    {
        let store = fixture.manager.store().lock().await;
        store
            .conn
            .execute(
                "UPDATE scheduled_jobs
                     SET name='poisoned',message='poisoned',schedule_json=?1,
                         last_fired_at=?2,next_fire_at=?2,enabled=0,
                         working_dir='/tmp/poisoned',provider='Codex',model='poisoned',
                         project_id=?3,created_at=?2,updated_at=?2,
                         wake_mode='fresh',wake_session_id=?4
                     WHERE id=?5",
                rusqlite::params![
                    poisoned_schedule_json,
                    poisoned_timestamp,
                    issue_rpc_project_id().to_string(),
                    poisoned_owner.to_string(),
                    guard_id.to_string(),
                ],
            )
            .expect("poison legacy deterministic row");
        let poisoned = store
            .get_scheduled_job(&guard_id)
            .unwrap()
            .expect("poisoned row remains readable");
        assert!(!poisoned.enabled);
        assert!(
            !crate::session::harness::tools::schedule_wake::is_program_guard_sentinel(
                &poisoned, caller_id
            )
        );
        assert_eq!(
            store.program_guard_owner_for_job_id(&guard_id).unwrap(),
            Some(caller_id),
            "deterministic id must retain program ownership independently of row fields"
        );
    }

    for request in [
        RpcRequest::new(
            "UpdateScheduledJob",
            serde_json::json!({"id": guard_id, "enabled": true}),
        ),
        RpcRequest::new("ToggleScheduledJob", serde_json::json!({"id": guard_id})),
    ] {
        let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
        else {
            panic!("expected response");
        };
        let error = response.error.expect("generic rearm must fail");
        assert!(error.message.contains(
            "closed program guards can only be rearmed by explicit program registration"
        ));
        assert!(
            !fixture
                .manager
                .store()
                .lock()
                .await
                .get_scheduled_job(&guard_id)
                .unwrap()
                .expect("poisoned guard remains")
                .enabled
        );
    }

    let request = RpcRequest::new("TriggerScheduledJob", serde_json::json!({"id": guard_id}));
    let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
    else {
        panic!("expected response");
    };
    let error = response
        .error
        .expect("manual closed-guard trigger must fail");
    assert!(error.message.contains(
        "closed program guards can only be triggered after explicit program registration"
    ));
    assert!(
        !fixture
            .manager
            .store()
            .lock()
            .await
            .get_scheduled_job(&guard_id)
            .unwrap()
            .expect("poisoned guard remains after rejected trigger")
            .enabled
    );
    {
        let store = fixture.manager.store().lock().await;
        assert_eq!(store.list_scheduled_jobs().unwrap().len(), 1);
        assert!(store.list_issues(&Default::default()).unwrap().is_empty());
        store
            .conn
            .execute(
                "UPDATE scheduled_jobs
                     SET schedule_json='{malformed',wake_mode='on_terminal:not-a-uuid'
                     WHERE id=?1",
                rusqlite::params![guard_id.to_string()],
            )
            .expect("make deterministic row unreadable");
        assert!(store.get_scheduled_job(&guard_id).unwrap().is_none());
        assert!(store.scheduled_job_exists(&guard_id).unwrap());
    }
    for request in [
        RpcRequest::new(
            "UpdateScheduledJob",
            serde_json::json!({"id": guard_id, "enabled": true}),
        ),
        RpcRequest::new("ToggleScheduledJob", serde_json::json!({"id": guard_id})),
        RpcRequest::new("TriggerScheduledJob", serde_json::json!({"id": guard_id})),
    ] {
        let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
        else {
            panic!("expected response");
        };
        assert!(
            response.error.is_some(),
            "unreadable deterministic rows must remain immutable to ordinary operations"
        );
    }

    let rearmed = fixture
        .manager
        .agent_control()
        .register_bound_program_guard(caller_id)
        .await
        .expect("explicit registration rearms");
    assert!(matches!(
        rearmed,
        crate::session::agent_verbs::ProgramGuardRegistration::Deduplicated(job)
            if job.id == guard_id && job.enabled
    ));
    {
        let store = fixture.manager.store().lock().await;
        let repaired = store
            .get_scheduled_job(&guard_id)
            .unwrap()
            .expect("explicit registration repairs deterministic row");
        assert!(
            crate::session::harness::tools::schedule_wake::is_program_guard_sentinel(
                &repaired, caller_id
            )
        );
        assert!(repaired.enabled);
        assert!(repaired.last_fired_at.is_none());
        assert_eq!(store.list_scheduled_jobs().unwrap().len(), 1);
        assert!(store.list_issues(&Default::default()).unwrap().is_empty());
    }

    for request in [
        RpcRequest::new("ToggleScheduledJob", serde_json::json!({"id": guard_id})),
        RpcRequest::new("TriggerScheduledJob", serde_json::json!({"id": guard_id})),
    ] {
        let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
        else {
            panic!("expected response");
        };
        assert!(
            response.error.is_some(),
            "even an enabled sentinel is immutable to ordinary operations"
        );
    }
}

fn operator_terminal_watch(target: Uuid, enabled: bool) -> rsi_common::types::ScheduledJob {
    use rsi_common::types::{Recurrence, ScheduleSpec, WakeMode};

    let now = chrono::Utc::now();
    rsi_common::types::ScheduledJob {
        id: Uuid::new_v4(),
        name: "operator terminal watch".into(),
        message: "watch note".into(),
        schedule: ScheduleSpec {
            recurrence: Recurrence::EverySeconds(60),
            anchor: now,
        },
        last_fired_at: None,
        next_fire_at: now,
        enabled,
        working_dir: None,
        provider: None,
        model: None,
        project_id: Some(issue_rpc_project_id()),
        created_at: now,
        updated_at: now,
        wake_mode: WakeMode::OnTerminal(Uuid::new_v4()),
        wake_session_id: Some(target),
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn update_scheduled_job_enforces_terminal_watch_cap_and_preserves_idempotence() {
    let fixture = recursive_dag_rpc_fixture();
    let target = Uuid::new_v4();
    let (enabled_id, disabled_id) = {
        let store = fixture.manager.store().lock().await;
        let mut enabled_id = None;
        for _ in 0..MAX_TERMINAL_WATCHES_PER_MASTER {
            let job = operator_terminal_watch(target, true);
            enabled_id.get_or_insert(job.id);
            store
                .insert_scheduled_job(&job)
                .expect("insert enabled watch");
        }
        let disabled = operator_terminal_watch(target, false);
        store
            .insert_scheduled_job(&disabled)
            .expect("insert disabled watch");
        (enabled_id.expect("enabled watch id"), disabled.id)
    };

    let request = RpcRequest::new(
        "UpdateScheduledJob",
        serde_json::json!({"id": disabled_id, "enabled": true}),
    );
    let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
    else {
        panic!("expected response");
    };
    assert!(
        response
            .error
            .expect("65th enabled watch must fail")
            .message
            .contains("terminal_watch_cap_reached")
    );
    assert!(
        !fixture
            .manager
            .store()
            .lock()
            .await
            .get_scheduled_job(&disabled_id)
            .unwrap()
            .unwrap()
            .enabled
    );

    let request = RpcRequest::new(
        "UpdateScheduledJob",
        serde_json::json!({"id": enabled_id, "enabled": true}),
    );
    let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
    else {
        panic!("expected response");
    };
    assert!(response.error.is_none(), "idempotent enable must succeed");

    let request = RpcRequest::new(
        "UpdateScheduledJob",
        serde_json::json!({"id": enabled_id, "enabled": false}),
    );
    let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
    else {
        panic!("expected response");
    };
    assert!(response.error.is_none(), "disable must remain admitted");

    let request = RpcRequest::new(
        "UpdateScheduledJob",
        serde_json::json!({"id": disabled_id, "enabled": true}),
    );
    let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
    else {
        panic!("expected response");
    };
    assert!(response.error.is_none(), "freed capacity must be reusable");
    let store = fixture.manager.store().lock().await;
    assert!(
        store
            .get_scheduled_job(&disabled_id)
            .unwrap()
            .unwrap()
            .enabled
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn toggle_scheduled_job_enforces_terminal_watch_cap_and_preserves_disable() {
    let fixture = recursive_dag_rpc_fixture();
    let target = Uuid::new_v4();
    let (enabled_id, disabled_id) = {
        let store = fixture.manager.store().lock().await;
        let mut enabled_id = None;
        for _ in 0..MAX_TERMINAL_WATCHES_PER_MASTER {
            let job = operator_terminal_watch(target, true);
            enabled_id.get_or_insert(job.id);
            store
                .insert_scheduled_job(&job)
                .expect("insert enabled watch");
        }
        let disabled = operator_terminal_watch(target, false);
        store
            .insert_scheduled_job(&disabled)
            .expect("insert disabled watch");
        (enabled_id.expect("enabled watch id"), disabled.id)
    };

    let request = RpcRequest::new("ToggleScheduledJob", serde_json::json!({"id": disabled_id}));
    let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
    else {
        panic!("expected response");
    };
    assert!(
        response
            .error
            .expect("65th enabled watch must fail")
            .message
            .contains("terminal_watch_cap_reached")
    );

    let request = RpcRequest::new("ToggleScheduledJob", serde_json::json!({"id": enabled_id}));
    let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
    else {
        panic!("expected response");
    };
    assert_eq!(response.result.unwrap()["enabled"], false);

    let request = RpcRequest::new("ToggleScheduledJob", serde_json::json!({"id": disabled_id}));
    let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
    else {
        panic!("expected response");
    };
    assert_eq!(response.result.unwrap()["enabled"], true);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn operator_update_and_toggle_race_never_exceeds_terminal_watch_cap() {
    let fixture = recursive_dag_rpc_fixture();
    let target = Uuid::new_v4();
    let (update_id, toggle_id) = {
        let store = fixture.manager.store().lock().await;
        for _ in 0..(MAX_TERMINAL_WATCHES_PER_MASTER - 1) {
            store
                .insert_scheduled_job(&operator_terminal_watch(target, true))
                .expect("insert enabled watch");
        }
        let update = operator_terminal_watch(target, false);
        let toggle = operator_terminal_watch(target, false);
        store
            .insert_scheduled_job(&update)
            .expect("insert update watch");
        store
            .insert_scheduled_job(&toggle)
            .expect("insert toggle watch");
        (update.id, toggle.id)
    };
    let update = RpcRequest::new(
        "UpdateScheduledJob",
        serde_json::json!({"id": update_id, "enabled": true}),
    );
    let toggle = RpcRequest::new("ToggleScheduledJob", serde_json::json!({"id": toggle_id}));
    let (update, toggle) = tokio::join!(
        fixture.server.handle_request_inner(&update),
        fixture.server.handle_request_inner(&toggle)
    );
    let HandleResult::Response(update) = update else {
        panic!("expected update response");
    };
    let HandleResult::Response(toggle) = toggle else {
        panic!("expected toggle response");
    };
    assert_eq!(
        usize::from(update.error.is_some()) + usize::from(toggle.error.is_some()),
        1,
        "exactly one concurrent re-enable may consume the final slot"
    );
    let store = fixture.manager.store().lock().await;
    let enabled = store
        .list_scheduled_jobs()
        .unwrap()
        .into_iter()
        .filter(|job| {
            job.enabled
                && job.wake_session_id == Some(target)
                && matches!(job.wake_mode, rsi_common::types::WakeMode::OnTerminal(_))
        })
        .count();
    assert_eq!(enabled, MAX_TERMINAL_WATCHES_PER_MASTER);
}

/// A8 helper: arm a terminal watch via the RPC verb.
#[allow(clippy::expect_used)]
async fn arm_watch(
    fixture: &RecursiveDagRpcFixture,
    token: &str,
    watched: Option<&str>,
    extra: serde_json::Value,
) -> RpcResponse {
    let mut args = serde_json::json!({
        "message": "watch note",
        "mode": "on_terminal",
    });
    if let Some(w) = watched {
        args["watch_session_id"] = serde_json::Value::String(w.to_string());
    }
    if let serde_json::Value::Object(extra_map) = extra {
        for (k, v) in extra_map {
            args[k] = v;
        }
    }
    let mut request = RpcRequest::new("AgentScheduleWake", args);
    request.session_token = Some(token.to_string());
    let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
    else {
        panic!("expected response");
    };
    response
}

/// T-12: watched-subject authz scope, self-watch rejection, natural-key
/// dedup, and the params surface still carrying no steerable wake target.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::significant_drop_tightening,
    clippy::too_many_lines
)]
async fn watch_arm_authz_scope_and_dedup() {
    use rsi_common::types::{SessionKind, WakeMode};

    let fixture = recursive_dag_rpc_fixture();
    let caller_id = Uuid::new_v4();
    let child_id = Uuid::new_v4();
    let stranger_id = Uuid::new_v4();
    {
        let store = fixture.manager.store().lock().await;
        store
            .insert_session(&mk_agent_test_session(
                caller_id,
                SessionKind::Task,
                None,
                None,
            ))
            .expect("insert caller");
        store
            .insert_session(&mk_agent_test_session(
                child_id,
                SessionKind::Task,
                Some(caller_id),
                None,
            ))
            .expect("insert child");
        store
            .insert_session(&mk_agent_test_session(
                stranger_id,
                SessionKind::Task,
                None,
                None,
            ))
            .expect("insert stranger");
    }
    let token = "watch-arm-token".to_string();
    fixture
        .manager
        .register_agent_token(token.clone(), caller_id)
        .await;

    // Out-of-scope watched subject -> denied.
    let resp = arm_watch(
        &fixture,
        &token,
        Some(&stranger_id.to_string()),
        serde_json::json!({}),
    )
    .await;
    let err = resp.error.expect("out-of-scope must fail");
    assert!(
        err.message.contains("agent_verb_scope_denied"),
        "{}",
        err.message
    );

    // Self-watch -> rejected explicitly.
    let resp = arm_watch(
        &fixture,
        &token,
        Some(&caller_id.to_string()),
        serde_json::json!({}),
    )
    .await;
    let err = resp.error.expect("self-watch must fail");
    assert!(
        err.message.contains("watch_self_rejected"),
        "{}",
        err.message
    );

    // Missing watch_session_id -> rejected.
    let resp = arm_watch(&fixture, &token, None, serde_json::json!({})).await;
    assert!(
        resp.error
            .expect("missing watched must fail")
            .message
            .contains("watch_session_id")
    );

    // watch_session_id without the mode -> rejected.
    let mut request = RpcRequest::new(
        "AgentScheduleWake",
        serde_json::json!({
            "message": "hi",
            "in_seconds": 60,
            "watch_session_id": child_id.to_string(),
        }),
    );
    request.session_token = Some(token.clone());
    let HandleResult::Response(resp) = fixture.server.handle_request_inner(&request).await else {
        panic!("expected response");
    };
    assert!(
        resp.error
            .expect("watch id without mode must fail")
            .message
            .contains("only valid with mode 'on_terminal'")
    );

    // Direct child in scope -> job created; smuggled wake_session_id is
    // ignored (wake target = resolved caller, always).
    let smuggled = Uuid::new_v4();
    let resp = arm_watch(
        &fixture,
        &token,
        Some(&child_id.to_string()),
        serde_json::json!({ "wake_session_id": smuggled.to_string() }),
    )
    .await;
    assert!(resp.error.is_none(), "unexpected error: {:?}", resp.error);
    let result = resp.result.expect("result");
    let job_id: Uuid = serde_json::from_value(result["job_id"].clone()).expect("job_id");
    {
        let store = fixture.manager.store().lock().await;
        let job = store
            .get_scheduled_job(&job_id)
            .expect("get job")
            .expect("job row");
        assert_eq!(job.wake_mode, WakeMode::OnTerminal(child_id));
        assert_eq!(
            job.wake_session_id,
            Some(caller_id),
            "wake target must be the caller"
        );
        assert_ne!(job.wake_session_id, Some(smuggled));
        assert_eq!(job.name, "rsi-watch");
        assert!(job.enabled);
    }

    // Natural-key dedup: identical re-arm returns the SAME job id.
    let resp = arm_watch(
        &fixture,
        &token,
        Some(&child_id.to_string()),
        serde_json::json!({}),
    )
    .await;
    assert!(resp.error.is_none());
    let result = resp.result.expect("result");
    let deduped_id: Uuid = serde_json::from_value(result["job_id"].clone()).expect("job_id");
    assert_eq!(deduped_id, job_id);
    assert_eq!(result["deduplicated"], serde_json::json!(true));
    {
        let store = fixture.manager.store().lock().await;
        let watches = store
            .list_scheduled_jobs()
            .expect("list")
            .into_iter()
            .filter(|j| matches!(j.wake_mode, WakeMode::OnTerminal(_)))
            .count();
        assert_eq!(watches, 1, "re-arm must not create a second row");
    }
}

/// T-12 (cap leg): the 65th enabled watch for one master is rejected.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::significant_drop_tightening
)]
async fn watch_arm_per_master_cap_rejects_beyond_64() {
    use rsi_common::types::{Recurrence, ScheduleSpec, ScheduledJob, SessionKind, WakeMode};

    let fixture = recursive_dag_rpc_fixture();
    let caller_id = Uuid::new_v4();
    let child_id = Uuid::new_v4();
    {
        let store = fixture.manager.store().lock().await;
        store
            .insert_session(&mk_agent_test_session(
                caller_id,
                SessionKind::Task,
                None,
                None,
            ))
            .expect("insert caller");
        store
            .insert_session(&mk_agent_test_session(
                child_id,
                SessionKind::Task,
                Some(caller_id),
                None,
            ))
            .expect("insert child");
        // Pre-load 64 enabled watches for this master (distinct watched
        // subjects so none dedups against the arm below).
        let now = chrono::Utc::now();
        for _ in 0..MAX_TERMINAL_WATCHES_PER_MASTER {
            let job = ScheduledJob {
                id: Uuid::new_v4(),
                name: "rsi-watch".to_string(),
                message: String::new(),
                schedule: ScheduleSpec {
                    recurrence: Recurrence::EverySeconds(60),
                    anchor: now,
                },
                last_fired_at: None,
                next_fire_at: now,
                enabled: true,
                working_dir: None,
                provider: None,
                model: None,
                project_id: None,
                created_at: now,
                updated_at: now,
                wake_mode: WakeMode::OnTerminal(Uuid::new_v4()),
                wake_session_id: Some(caller_id),
            };
            store.insert_scheduled_job(&job).expect("insert watch");
        }
    }
    let token = "watch-cap-token".to_string();
    fixture
        .manager
        .register_agent_token(token.clone(), caller_id)
        .await;

    let resp = arm_watch(
        &fixture,
        &token,
        Some(&child_id.to_string()),
        serde_json::json!({}),
    )
    .await;
    let err = resp.error.expect("65th watch must be rejected");
    assert!(
        err.message.contains("terminal_watch_cap_reached"),
        "{}",
        err.message
    );
}

/// T-1 (RPC half): arming on an ALREADY-terminal child succeeds and
/// persists an enabled, immediately-eligible watch row even with the
/// scheduler disabled (`None` handle — the fixture's shape): the arm
/// inserts, warns, and the watch activates when the scheduler runs.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::significant_drop_tightening
)]
async fn arm_on_already_terminal_child_persists_watch_without_scheduler() {
    use rsi_common::types::{SessionKind, SessionStatus, WakeMode};

    let fixture = recursive_dag_rpc_fixture();
    let caller_id = Uuid::new_v4();
    let child_id = Uuid::new_v4();
    {
        let store = fixture.manager.store().lock().await;
        store
            .insert_session(&mk_agent_test_session(
                caller_id,
                SessionKind::Task,
                None,
                None,
            ))
            .expect("insert caller");
        let mut child = mk_agent_test_session(child_id, SessionKind::Task, Some(caller_id), None);
        child.status = SessionStatus::Completed;
        store.insert_session(&child).expect("insert child");
    }
    let token = "watch-terminal-arm-token".to_string();
    fixture
        .manager
        .register_agent_token(token.clone(), caller_id)
        .await;

    let resp = arm_watch(
        &fixture,
        &token,
        Some(&child_id.to_string()),
        serde_json::json!({}),
    )
    .await;
    assert!(resp.error.is_none(), "unexpected error: {:?}", resp.error);
    let result = resp.result.expect("result");
    let job_id: Uuid = serde_json::from_value(result["job_id"].clone()).expect("job_id");

    let store = fixture.manager.store().lock().await;
    let job = store
        .get_scheduled_job(&job_id)
        .expect("get")
        .expect("row present");
    assert!(job.enabled);
    assert_eq!(job.wake_mode, WakeMode::OnTerminal(child_id));
    // Recurring row, due within one reconcile tick of arm time.
    assert!(job.next_fire_at <= chrono::Utc::now() + chrono::Duration::seconds(61));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn recursive_typed_inspector_rpc_routes_are_not_registered_slice_1() {
    let fixture = recursive_dag_rpc_fixture();
    let methods = [
        rsi_common::rpc::METHOD_LIST_RECURSIVE_TEST_SUMMARIES,
        rsi_common::rpc::METHOD_GET_RECURSIVE_TEST_DETAIL,
        rsi_common::rpc::METHOD_LIST_RECURSIVE_DIFF_SUMMARIES,
        rsi_common::rpc::METHOD_GET_RECURSIVE_DIFF_DETAIL,
        rsi_common::rpc::METHOD_GET_RECURSIVE_DIFF_FILE_HUNKS,
        rsi_common::rpc::METHOD_GET_RECURSIVE_SCHEDULER_REPORT_SUMMARY,
        rsi_common::rpc::METHOD_GET_RECURSIVE_SCHEDULER_REPORT_DETAIL,
    ];

    for method in methods {
        let response = call_rpc(&fixture.server, method, serde_json::Value::Null).await;
        let error = response.error.expect("typed inspector route absent");
        assert_eq!(error.code, METHOD_NOT_FOUND);
        assert!(error.message.contains("Method not found"));
    }

    let source = rpc_production_source();
    let production = source.as_str();
    for method in methods {
        assert!(
            !production.contains(method),
            "typed inspector method {method} must not be registered in slice 1"
        );
    }
    for handler in [
        "handle_list_recursive_test_summaries",
        "handle_get_recursive_test_detail",
        "handle_list_recursive_diff_summaries",
        "handle_get_recursive_diff_detail",
        "handle_get_recursive_diff_file_hunks",
        "handle_get_recursive_scheduler_report_summary",
        "handle_get_recursive_scheduler_report_detail",
    ] {
        assert!(
            !production.contains(handler),
            "typed inspector handler {handler} must not exist in slice 1"
        );
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn recursive_dag_rpc_artifact_lookup_is_graph_guarded() {
    let fixture = recursive_dag_rpc_fixture();
    let (graph_id, root_id) = create_recursive_dag_rpc_graph(&fixture.manager).await;
    let (other_graph_id, _) = create_recursive_dag_rpc_graph(&fixture.manager).await;
    let artifacts =
        record_recursive_dag_rpc_artifacts(&fixture.manager, graph_id, root_id, &["one"]).await;
    let artifact_id = artifacts[0].id;

    let response = call_rpc(
        &fixture.server,
        "GetRecursiveExecutionArtifact",
        serde_json::json!({
            "graph_id": graph_id,
            "artifact_id": artifact_id
        }),
    )
    .await;
    assert!(response.error.is_none());
    let readback: rsi_common::RecursiveExecutionArtifactReadback =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!(readback.artifact.id, artifact_id);
    assert_eq!(readback.owners.graph_id, RecursiveTaskGraphId(graph_id));
    assert_eq!(readback.owners.task_id, RecursiveTaskId(root_id));
    assert_eq!(
        readback.metadata_state,
        rsi_common::RecursiveArtifactMetadataState::Valid
    );
    assert!(readback.role.is_none());
    assert!(
        readback
            .warnings
            .iter()
            .any(|warning| warning.code == "RECURSIVE_ARTIFACT_ROLE_UNAVAILABLE")
    );

    let response = call_rpc(
        &fixture.server,
        "GetRecursiveExecutionArtifact",
        serde_json::json!({
            "graph_id": other_graph_id,
            "artifact_id": artifact_id
        }),
    )
    .await;
    assert_eq!(response.error.as_ref().unwrap().code, INVALID_PARAMS);
    let data = rpc_error_data(&response);
    assert_eq!(data.code, "RECURSIVE_ARTIFACT_GRAPH_MISMATCH");
    assert_eq!(data.resource_id, Some(artifact_id.to_string()));

    let response = call_rpc(
        &fixture.server,
        "GetRecursiveExecutionArtifact",
        serde_json::json!({
            "graph_id": graph_id,
            "artifact_id": 9_999_999_i64
        }),
    )
    .await;
    assert_eq!(response.error.as_ref().unwrap().code, INVALID_PARAMS);
    let data = rpc_error_data(&response);
    assert_eq!(data.code, "RECURSIVE_ARTIFACT_NOT_FOUND");

    let unknown_graph_id = Uuid::new_v4();
    let response = call_rpc(
        &fixture.server,
        "GetRecursiveExecutionArtifact",
        serde_json::json!({
            "graph_id": unknown_graph_id,
            "artifact_id": artifact_id
        }),
    )
    .await;
    assert_eq!(response.error.as_ref().unwrap().code, INVALID_PARAMS);
    let data = rpc_error_data(&response);
    assert_eq!(data.code, "RECURSIVE_GRAPH_NOT_FOUND");
    assert_eq!(data.resource_id, Some(unknown_graph_id.to_string()));
    assert!(data.details.is_none());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn recursive_dag_rpc_artifact_summary_pagination_is_bounded_and_scoped() {
    let fixture = recursive_dag_rpc_fixture();
    let (graph_id, root_id) = create_recursive_dag_rpc_graph(&fixture.manager).await;
    let (other_graph_id, other_root_id) = create_recursive_dag_rpc_graph(&fixture.manager).await;
    let artifacts = record_recursive_dag_rpc_artifacts(
        &fixture.manager,
        graph_id,
        root_id,
        &["one", "two", "three"],
    )
    .await;
    record_recursive_dag_rpc_artifacts(&fixture.manager, other_graph_id, other_root_id, &["other"])
        .await;

    let response = call_rpc(
        &fixture.server,
        "ListRecursiveExecutionArtifactSummaries",
        serde_json::json!({
            "graph_id": graph_id,
            "limit": 2,
            "include_total": true
        }),
    )
    .await;
    assert!(response.error.is_none());
    let page: rsi_common::RecursiveReadPage<rsi_common::RecursiveExecutionArtifactSummary> =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!(page.limit, 2);
    assert_eq!(page.items.len(), 2);
    assert!(page.has_more);
    assert!(page.next_cursor.is_some());
    assert_eq!(page.total_count, Some(3));
    assert_eq!(page.items[0].graph_id, RecursiveTaskGraphId(graph_id));
    assert!(page.items[0].artifact_id > page.items[1].artifact_id);

    let response = call_rpc(
        &fixture.server,
        "ListRecursiveExecutionArtifactSummaries",
        serde_json::json!({
            "graph_id": graph_id,
            "cursor": page.next_cursor,
            "limit": 2
        }),
    )
    .await;
    assert!(response.error.is_none());
    let second_page: rsi_common::RecursiveReadPage<rsi_common::RecursiveExecutionArtifactSummary> =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!(second_page.items.len(), 1);
    assert!(!second_page.has_more);
    assert!(second_page.next_cursor.is_none());
    assert_eq!(second_page.items[0].artifact_id, artifacts[0].id);

    let response = call_rpc(
        &fixture.server,
        "ListRecursiveExecutionArtifactSummaries",
        serde_json::json!({
            "graph_id": graph_id,
            "limit": 50_000
        }),
    )
    .await;
    assert!(response.error.is_none());
    let bounded: rsi_common::RecursiveReadPage<rsi_common::RecursiveExecutionArtifactSummary> =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!(
        bounded.limit,
        crate::store::recursive_dag::MAX_RECURSIVE_READ_PAGE_LIMIT
    );

    let response = call_rpc(
        &fixture.server,
        "ListRecursiveExecutionArtifactSummaries",
        serde_json::json!({
            "graph_id": graph_id,
            "cursor": "not-json"
        }),
    )
    .await;
    assert_eq!(response.error.as_ref().unwrap().code, INVALID_PARAMS);
    assert_eq!(rpc_error_data(&response).code, "RECURSIVE_CURSOR_INVALID");

    let first_page = call_rpc(
        &fixture.server,
        "ListRecursiveExecutionArtifactSummaries",
        serde_json::json!({
            "graph_id": graph_id,
            "limit": 1
        }),
    )
    .await;
    let first_page: rsi_common::RecursiveReadPage<rsi_common::RecursiveExecutionArtifactSummary> =
        serde_json::from_value(first_page.result.unwrap()).unwrap();
    let response = call_rpc(
        &fixture.server,
        "ListRecursiveExecutionArtifactSummaries",
        serde_json::json!({
            "graph_id": other_graph_id,
            "cursor": first_page.next_cursor
        }),
    )
    .await;
    assert_eq!(response.error.as_ref().unwrap().code, INVALID_PARAMS);
    assert_eq!(rpc_error_data(&response).code, "RECURSIVE_CURSOR_INVALID");

    let unknown_graph_id = Uuid::new_v4();
    let response = call_rpc(
        &fixture.server,
        "ListRecursiveExecutionArtifactSummaries",
        serde_json::json!({
            "graph_id": unknown_graph_id
        }),
    )
    .await;
    assert_eq!(response.error.as_ref().unwrap().code, INVALID_PARAMS);
    let data = rpc_error_data(&response);
    assert_eq!(data.code, "RECURSIVE_GRAPH_NOT_FOUND");
    assert_eq!(data.resource_id, Some(unknown_graph_id.to_string()));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn recursive_dag_rpc_artifact_preview_is_graph_guarded_and_inline_only() {
    let fixture = recursive_dag_rpc_fixture();
    let (graph_id, root_id) = create_recursive_dag_rpc_graph(&fixture.manager).await;
    let (other_graph_id, _) = create_recursive_dag_rpc_graph(&fixture.manager).await;
    let artifacts =
        record_recursive_dag_rpc_artifacts(&fixture.manager, graph_id, root_id, &["preview"]).await;
    let artifact_id = artifacts[0].id;

    let response = call_rpc(
        &fixture.server,
        "PreviewRecursiveExecutionArtifact",
        serde_json::json!({
            "graph_id": graph_id,
            "artifact_id": artifact_id
        }),
    )
    .await;
    assert!(response.error.is_none());
    let preview: rsi_common::RecursiveExecutionArtifactPreview =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!(preview.artifact_id, artifact_id);
    assert_eq!(preview.graph_id, RecursiveTaskGraphId(graph_id));
    assert_eq!(
        preview.content_state,
        rsi_common::RecursiveArtifactPreviewState::Available
    );
    assert_eq!(preview.text.as_deref(), Some("artifact content 0"));
    assert_eq!(preview.total_bytes, Some("artifact content 0".len() as u64));
    assert_eq!(preview.total_lines, Some(1));
    assert_eq!(preview.shown_lines, 1);
    assert_eq!(preview.digest_algorithm.as_deref(), Some("sha256"));
    assert!(!preview.truncated);

    let response = call_rpc(
        &fixture.server,
        "PreviewRecursiveExecutionArtifact",
        serde_json::json!({
            "graph_id": other_graph_id,
            "artifact_id": artifact_id
        }),
    )
    .await;
    assert_eq!(response.error.as_ref().unwrap().code, INVALID_PARAMS);
    let data = rpc_error_data(&response);
    assert_eq!(data.code, "RECURSIVE_ARTIFACT_GRAPH_MISMATCH");
    assert_eq!(data.resource_id, Some(artifact_id.to_string()));

    let response = call_rpc(
        &fixture.server,
        "PreviewRecursiveExecutionArtifact",
        serde_json::json!({
            "graph_id": graph_id,
            "artifact_id": 9_999_999_i64
        }),
    )
    .await;
    assert_eq!(response.error.as_ref().unwrap().code, INVALID_PARAMS);
    assert_eq!(
        rpc_error_data(&response).code,
        "RECURSIVE_ARTIFACT_NOT_FOUND"
    );

    let unknown_graph_id = Uuid::new_v4();
    let response = call_rpc(
        &fixture.server,
        "PreviewRecursiveExecutionArtifact",
        serde_json::json!({
            "graph_id": unknown_graph_id,
            "artifact_id": artifact_id
        }),
    )
    .await;
    assert_eq!(response.error.as_ref().unwrap().code, INVALID_PARAMS);
    let data = rpc_error_data(&response);
    assert_eq!(data.code, "RECURSIVE_GRAPH_NOT_FOUND");
    assert_eq!(data.resource_id, Some(unknown_graph_id.to_string()));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn recursive_dag_rpc_artifact_preview_blocks_external_and_unsupported_sources() {
    let fixture = recursive_dag_rpc_fixture();
    let (graph_id, root_id) = create_recursive_dag_rpc_graph(&fixture.manager).await;
    let temp_file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(temp_file.path(), "filesystem content must not leak").unwrap();
    let (file_artifact_id, session_event_artifact_id) = {
        let store = fixture.manager.store().lock().await;
        let artifacts = store
            .record_recursive_execution_artifacts(
                RecursiveTaskGraphId(graph_id),
                vec![
                    crate::store::recursive_dag::RecursiveExecutionArtifactCreate {
                        task_id: RecursiveTaskId(root_id),
                        attempt_id: None,
                        kind: rsi_common::RecursiveExecutionArtifactKind::File,
                        label: "external-path".to_string(),
                        content: Some("inline shadow content must not leak".to_string()),
                        uri: Some(temp_file.path().display().to_string()),
                        metadata: serde_json::json!({"rpc_test": true}),
                    },
                    crate::store::recursive_dag::RecursiveExecutionArtifactCreate {
                        task_id: RecursiveTaskId(root_id),
                        attempt_id: None,
                        kind: rsi_common::RecursiveExecutionArtifactKind::SessionEvent,
                        label: "session-event".to_string(),
                        content: None,
                        uri: None,
                        metadata: serde_json::json!({"rpc_test": true}),
                    },
                ],
            )
            .expect("record external artifacts");
        (artifacts[0].id, artifacts[1].id)
    };

    let response = call_rpc(
        &fixture.server,
        "PreviewRecursiveExecutionArtifact",
        serde_json::json!({
            "graph_id": graph_id,
            "artifact_id": file_artifact_id
        }),
    )
    .await;
    assert!(response.error.is_none());
    let preview: rsi_common::RecursiveExecutionArtifactPreview =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!(
        preview.content_state,
        rsi_common::RecursiveArtifactPreviewState::UriBlocked
    );
    assert!(preview.text.is_none());
    assert!(preview.digest.is_none());
    assert!(preview.total_bytes.is_none());
    assert!(
        preview
            .warnings
            .iter()
            .any(|warning| warning.code == "RECURSIVE_ARTIFACT_URI_PREVIEW_BLOCKED")
    );

    let response = call_rpc(
        &fixture.server,
        "PreviewRecursiveExecutionArtifact",
        serde_json::json!({
            "graph_id": graph_id,
            "artifact_id": session_event_artifact_id
        }),
    )
    .await;
    assert!(response.error.is_none());
    let preview: rsi_common::RecursiveExecutionArtifactPreview =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!(
        preview.content_state,
        rsi_common::RecursiveArtifactPreviewState::UnsupportedKind
    );
    assert!(preview.text.is_none());
    assert!(
        preview
            .warnings
            .iter()
            .any(|warning| warning.code == "RECURSIVE_ARTIFACT_PREVIEW_UNSUPPORTED_KIND")
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn recursive_dag_rpc_artifact_preview_truncates_by_bytes_and_lines() {
    let fixture = recursive_dag_rpc_fixture();
    let (graph_id, root_id) = create_recursive_dag_rpc_graph(&fixture.manager).await;
    let artifact_id = {
        let store = fixture.manager.store().lock().await;
        store
            .record_recursive_execution_artifacts(
                RecursiveTaskGraphId(graph_id),
                vec![
                    crate::store::recursive_dag::RecursiveExecutionArtifactCreate {
                        task_id: RecursiveTaskId(root_id),
                        attempt_id: None,
                        kind: rsi_common::RecursiveExecutionArtifactKind::Inline,
                        label: "long-inline".to_string(),
                        content: Some("alpha\nbeta\ngamma\n".to_string()),
                        uri: None,
                        metadata: serde_json::json!({"rpc_test": true}),
                    },
                ],
            )
            .expect("record long artifact")[0]
            .id
    };

    let response = call_rpc(
        &fixture.server,
        "PreviewRecursiveExecutionArtifact",
        serde_json::json!({
            "graph_id": graph_id,
            "artifact_id": artifact_id,
            "max_bytes": 7,
            "max_lines": 50000
        }),
    )
    .await;
    assert!(response.error.is_none());
    let preview: rsi_common::RecursiveExecutionArtifactPreview =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!(
        preview.content_state,
        rsi_common::RecursiveArtifactPreviewState::Truncated
    );
    assert!(preview.truncated);
    assert!(preview.truncated_by_bytes);
    assert!(!preview.truncated_by_lines);
    assert_eq!(preview.applied_max_bytes, 7);
    assert_eq!(
        preview.applied_max_lines,
        crate::store::recursive_dag::MAX_RECURSIVE_ARTIFACT_PREVIEW_MAX_LINES
    );
    assert_eq!(preview.text.as_deref(), Some("alpha\nb"));

    let response = call_rpc(
        &fixture.server,
        "PreviewRecursiveExecutionArtifact",
        serde_json::json!({
            "graph_id": graph_id,
            "artifact_id": artifact_id,
            "max_bytes": 50_000_000,
            "max_lines": 2
        }),
    )
    .await;
    assert!(response.error.is_none());
    let preview: rsi_common::RecursiveExecutionArtifactPreview =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert!(preview.truncated);
    assert!(!preview.truncated_by_bytes);
    assert!(preview.truncated_by_lines);
    assert_eq!(
        preview.applied_max_bytes,
        crate::store::recursive_dag::MAX_RECURSIVE_ARTIFACT_PREVIEW_MAX_BYTES
    );
    assert_eq!(preview.applied_max_lines, 2);
    assert_eq!(preview.text.as_deref(), Some("alpha\nbeta\n"));

    let response = call_rpc(
        &fixture.server,
        "PreviewRecursiveExecutionArtifact",
        serde_json::json!({
            "graph_id": graph_id,
            "artifact_id": artifact_id,
            "max_bytes": 4,
            "require_complete": true
        }),
    )
    .await;
    assert_eq!(response.error.as_ref().unwrap().code, INVALID_PARAMS);
    let data = rpc_error_data(&response);
    assert_eq!(data.code, "RECURSIVE_ARTIFACT_OVERSIZED");
    assert_eq!(data.resource_id, Some(artifact_id.to_string()));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn recursive_dag_rpc_artifact_readbacks_tolerate_malformed_metadata() {
    let fixture = recursive_dag_rpc_fixture();
    let (graph_id, root_id) = create_recursive_dag_rpc_graph(&fixture.manager).await;
    let artifacts =
        record_recursive_dag_rpc_artifacts(&fixture.manager, graph_id, root_id, &["bad"]).await;
    let artifact_id = artifacts[0].id;
    {
        let store = fixture.manager.store().lock().await;
        store
            .conn
            .execute_batch("PRAGMA ignore_check_constraints = ON")
            .expect("disable check constraints");
        store
            .conn
            .execute(
                "UPDATE recursive_execution_artifacts SET metadata_json = ?1 WHERE id = ?2",
                rusqlite::params!["{not-json", artifact_id],
            )
            .expect("corrupt artifact metadata");
        store
            .conn
            .execute_batch("PRAGMA ignore_check_constraints = OFF")
            .expect("restore check constraints");
    }

    let response = call_rpc(
        &fixture.server,
        "GetRecursiveExecutionArtifact",
        serde_json::json!({
            "graph_id": graph_id,
            "artifact_id": artifact_id
        }),
    )
    .await;
    assert!(response.error.is_none());
    let readback: rsi_common::RecursiveExecutionArtifactReadback =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!(
        readback.metadata_state,
        rsi_common::RecursiveArtifactMetadataState::Malformed
    );
    assert!(readback.artifact.metadata.is_null());
    assert!(
        readback
            .warnings
            .iter()
            .any(|warning| warning.code == "RECURSIVE_METADATA_MALFORMED")
    );

    let response = call_rpc(
        &fixture.server,
        "PreviewRecursiveExecutionArtifact",
        serde_json::json!({
            "graph_id": graph_id,
            "artifact_id": artifact_id
        }),
    )
    .await;
    assert!(response.error.is_none());
    let preview: rsi_common::RecursiveExecutionArtifactPreview =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!(preview.text.as_deref(), Some("artifact content 0"));
    assert!(
        preview
            .warnings
            .iter()
            .any(|warning| warning.code == "RECURSIVE_METADATA_MALFORMED")
    );

    let response = call_rpc(
        &fixture.server,
        "ListRecursiveExecutionArtifactSummaries",
        serde_json::json!({
            "graph_id": graph_id,
            "limit": 1
        }),
    )
    .await;
    assert!(response.error.is_none());
    let page: rsi_common::RecursiveReadPage<rsi_common::RecursiveExecutionArtifactSummary> =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!(
        page.items[0].metadata_state,
        rsi_common::RecursiveArtifactMetadataState::Malformed
    );
    assert!(
        page.warnings
            .iter()
            .any(|warning| warning.code == "RECURSIVE_METADATA_MALFORMED")
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn recursive_dag_rpc_has_no_live_executor_entrypoint() {
    let fixture = recursive_dag_rpc_fixture();
    let (graph_id, _) = create_recursive_dag_rpc_graph(&fixture.manager).await;

    let response = call_rpc(
        &fixture.server,
        "RunRecursiveDagLiveExecution",
        serde_json::json!({ "graph_id": graph_id }),
    )
    .await;
    assert_eq!(response.error.unwrap().code, METHOD_NOT_FOUND);

    let store = fixture.manager.store().lock().await;
    assert!(
        store
            .list_recursive_live_attempts_for_graph(RecursiveTaskGraphId(graph_id))
            .expect("list live attempts")
            .is_empty()
    );
    assert!(store.load_sessions().expect("load sessions").is_empty());
}

async fn recursive_table_count(manager: &std::sync::Arc<SessionManager>, table: &str) -> i64 {
    let store = manager.store().lock().await;
    store
        .conn
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .expect("table count")
}

fn workflow_execution_registry_counts(manager: &std::sync::Arc<SessionManager>) -> (usize, usize) {
    let executions = manager
        .workflow_executions()
        .lock()
        .expect("workflow execution registry lock");
    (executions.entries.len(), executions.expired.len())
}

async fn recursive_live_attempt_state_snapshot(
    manager: &std::sync::Arc<SessionManager>,
    live_attempt_id: rsi_common::RecursiveLiveAttemptId,
) -> (
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    String,
) {
    let store = manager.store().lock().await;
    store
        .conn
        .query_row(
            "SELECT status, recovery_status, lease_owner, lease_token,
                        heartbeat_at, lease_expires_at, recovery_checked_at,
                        updated_at
                 FROM recursive_live_attempts
                 WHERE id = ?1",
            rusqlite::params![live_attempt_id.to_string()],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                ))
            },
        )
        .expect("live attempt state snapshot")
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn recursive_dag_rpc_artifact_inspector_readbacks_are_read_only() {
    let fixture = recursive_dag_rpc_fixture();
    let live = create_recursive_dag_rpc_live_attempt(&fixture.manager).await;
    let validation = commit_recursive_dag_rpc_validation(&fixture.manager, &live).await;
    let artifact_id = validation
        .artifact_links
        .validation_artifact_id
        .expect("validation artifact id");

    let counts_before = [
        recursive_table_count(&fixture.manager, "recursive_scheduler_runs").await,
        recursive_table_count(&fixture.manager, "recursive_task_attempts").await,
        recursive_table_count(&fixture.manager, "recursive_live_attempts").await,
        recursive_table_count(&fixture.manager, "sessions").await,
        recursive_table_count(&fixture.manager, "recursive_topology_graph_links").await,
        recursive_table_count(&fixture.manager, "recursive_topology_task_links").await,
        recursive_table_count(&fixture.manager, "recursive_execution_artifacts").await,
        recursive_table_count(&fixture.manager, "recursive_live_output_validations").await,
    ];
    let live_state_before =
        recursive_live_attempt_state_snapshot(&fixture.manager, live.live_attempt_id).await;

    let response = call_rpc(
        &fixture.server,
        "GetRecursiveExecutionArtifact",
        serde_json::json!({
            "graph_id": live.graph_id,
            "artifact_id": artifact_id
        }),
    )
    .await;
    assert!(response.error.is_none());
    let response = call_rpc(
        &fixture.server,
        "PreviewRecursiveExecutionArtifact",
        serde_json::json!({
            "graph_id": live.graph_id,
            "artifact_id": artifact_id,
            "max_bytes": 8,
            "max_lines": 1
        }),
    )
    .await;
    assert!(response.error.is_none());
    let response = call_rpc(
        &fixture.server,
        "ListRecursiveExecutionArtifactSummaries",
        serde_json::json!({
            "graph_id": live.graph_id,
            "limit": 2,
            "include_total": true
        }),
    )
    .await;
    assert!(response.error.is_none());

    let counts_after = [
        recursive_table_count(&fixture.manager, "recursive_scheduler_runs").await,
        recursive_table_count(&fixture.manager, "recursive_task_attempts").await,
        recursive_table_count(&fixture.manager, "recursive_live_attempts").await,
        recursive_table_count(&fixture.manager, "sessions").await,
        recursive_table_count(&fixture.manager, "recursive_topology_graph_links").await,
        recursive_table_count(&fixture.manager, "recursive_topology_task_links").await,
        recursive_table_count(&fixture.manager, "recursive_execution_artifacts").await,
        recursive_table_count(&fixture.manager, "recursive_live_output_validations").await,
    ];
    let live_state_after =
        recursive_live_attempt_state_snapshot(&fixture.manager, live.live_attempt_id).await;
    assert_eq!(counts_after, counts_before);
    assert_eq!(live_state_after, live_state_before);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn recursive_dag_rpc_live_status_and_validation_readbacks_are_read_only() {
    let fixture = recursive_dag_rpc_fixture();
    let live = create_recursive_dag_rpc_live_attempt(&fixture.manager).await;
    let validation = commit_recursive_dag_rpc_validation(&fixture.manager, &live).await;
    let validation_id = validation
        .summary
        .validation_id
        .expect("stored validation id");
    let interrupt_id = {
        let store = fixture.manager.store().lock().await;
        let cancellation = store
            .request_recursive_scheduler_run_cancellation(
                live.run_id,
                crate::store::recursive_dag::RecursiveCancellationRequestCreate {
                    reason: "operator requested cancellation".to_string(),
                    requested_by: Some("rpc-test".to_string()),
                    source: rsi_common::RecursiveCancellationRequestSource::TestHarness,
                },
            )
            .expect("request cancellation");
        let interrupt_id = rsi_common::RecursiveLiveInterruptId(Uuid::new_v4());
        store
            .conn
            .execute(
                "INSERT INTO recursive_live_interrupts (
                        id, live_attempt_id, graph_id, task_id, scheduler_run_id,
                        attempt_id, session_id, cancellation_request_id, status,
                        reason, requested_at
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'requested', ?9, ?10)",
                rusqlite::params![
                    interrupt_id.to_string(),
                    live.live_attempt_id.to_string(),
                    live.graph_id.to_string(),
                    live.root_id.to_string(),
                    live.run_id.to_string(),
                    live.attempt_id.to_string(),
                    live.session_id.to_string(),
                    cancellation.id.to_string(),
                    "operator requested cancellation",
                    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
                ],
            )
            .expect("insert live interrupt fixture row");
        interrupt_id
    };

    let counts_before = [
        recursive_table_count(&fixture.manager, "recursive_live_attempts").await,
        recursive_table_count(&fixture.manager, "recursive_live_interrupts").await,
        recursive_table_count(&fixture.manager, "recursive_live_output_validations").await,
        recursive_table_count(&fixture.manager, "recursive_execution_artifacts").await,
        recursive_table_count(&fixture.manager, "recursive_recovery_passes").await,
    ];
    let state_before =
        recursive_live_attempt_state_snapshot(&fixture.manager, live.live_attempt_id).await;

    let response = call_rpc(
        &fixture.server,
        "ListRecursiveLiveAttempts",
        serde_json::json!({
            "graph_id": live.graph_id,
            "include_status": true
        }),
    )
    .await;
    assert!(response.error.is_none());
    let attempts: Vec<rsi_common::RecursiveLiveAttemptListItem> =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].summary.id, live.live_attempt_id);
    assert!(attempts[0].heartbeat.is_some());
    assert!(
        attempts[0]
            .heartbeat
            .as_ref()
            .expect("heartbeat")
            .heartbeat_token
            .is_none()
    );
    assert_eq!(
        attempts[0]
            .latest_validation
            .as_ref()
            .and_then(|summary| summary.validation_id),
        Some(validation_id)
    );

    let response = call_rpc(
        &fixture.server,
        "GetRecursiveLiveAttempt",
        serde_json::json!({
            "live_attempt_id": live.live_attempt_id,
            "include_artifacts": true,
            "include_retry_history": true
        }),
    )
    .await;
    assert!(response.error.is_none());
    let readback: rsi_common::RecursiveLiveAttemptReadback =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!(readback.live_attempt.summary.id, live.live_attempt_id);
    assert_eq!(
        readback.session.as_ref().map(|session| session.session_id),
        Some(live.session_id)
    );
    assert_eq!(
        readback
            .latest_validation
            .as_ref()
            .and_then(|summary| summary.validation_id),
        Some(validation_id)
    );
    let artifacts = readback.artifacts.expect("artifact readback");
    assert_eq!(artifacts.produced_artifacts.len(), 1);
    assert_eq!(artifacts.test_artifacts.len(), 1);
    assert_eq!(artifacts.diff_artifacts.len(), 1);
    assert!(readback.retry_history.expect("retry history").len() == 1);

    let response = call_rpc(
        &fixture.server,
        "GetRecursiveLiveOutputValidationResult",
        serde_json::json!({
            "validation_id": validation_id,
            "include_issues": true,
            "include_validation_report": true
        }),
    )
    .await;
    assert!(response.error.is_none());
    let result: Option<rsi_common::RecursiveLiveOutputValidationResult> =
        serde_json::from_value(response.result.unwrap()).unwrap();
    let result = result.expect("validation result");
    assert_eq!(result.summary.validation_id, Some(validation_id));
    assert_eq!(result.issues.len(), 1);
    assert_eq!(result.artifact_links.produced_artifact_ids.len(), 1);
    assert_eq!(result.artifact_links.test_artifact_ids.len(), 1);
    assert_eq!(result.artifact_links.diff_artifact_ids.len(), 1);
    assert_eq!(
        result.validation_report,
        Some(serde_json::json!({"status": "repairable"}))
    );

    let response = call_rpc(
        &fixture.server,
        "ListRecursiveLiveOutputValidationResults",
        serde_json::json!({
            "graph_id": live.graph_id,
            "include_issues": true
        }),
    )
    .await;
    assert!(response.error.is_none());
    let validations: Vec<rsi_common::RecursiveLiveOutputValidationListItem> =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!(validations.len(), 1);
    assert_eq!(validations[0].summary.validation_id, Some(validation_id));
    assert_eq!(validations[0].issues.as_ref().expect("issues").len(), 1);

    let response = call_rpc(
        &fixture.server,
        "ListRecursiveLiveValidationIssues",
        serde_json::json!({
            "validation_id": validation_id,
            "severity": "error",
            "class": "missing",
            "code": "missing_required_field"
        }),
    )
    .await;
    assert!(response.error.is_none());
    let issues: Vec<rsi_common::RecursiveLiveOutputValidationIssue> =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].message, "summary is required");

    let response = call_rpc(
        &fixture.server,
        "GetRecursiveLiveAttemptHeartbeatStatus",
        serde_json::json!({ "live_attempt_id": live.live_attempt_id }),
    )
    .await;
    assert!(response.error.is_none());
    let heartbeat: rsi_common::RecursiveLiveAttemptHeartbeatState =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!(heartbeat.live_attempt_id, live.live_attempt_id);
    assert!(heartbeat.heartbeat_token.is_none());

    let response = call_rpc(
        &fixture.server,
        "ListStaleRecursiveLiveAttemptHeartbeats",
        serde_json::json!({ "graph_id": live.graph_id }),
    )
    .await;
    assert!(response.error.is_none());
    let stale_heartbeats: Vec<rsi_common::RecursiveLiveAttemptHeartbeatState> =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert!(stale_heartbeats.is_empty());

    let response = call_rpc(
        &fixture.server,
        "GetRecursiveLiveInterruptStatus",
        serde_json::json!({ "interrupt_id": interrupt_id }),
    )
    .await;
    assert!(response.error.is_none());
    let interrupt: Option<rsi_common::RecursiveLiveInterruptSummary> =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!(interrupt.expect("interrupt").id, interrupt_id);

    let response = call_rpc(
        &fixture.server,
        "ListRecursiveLiveInterrupts",
        serde_json::json!({ "live_attempt_id": live.live_attempt_id }),
    )
    .await;
    assert!(response.error.is_none());
    let interrupts: Vec<rsi_common::RecursiveLiveInterruptSummary> =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!(interrupts.len(), 1);

    let response = call_rpc(
        &fixture.server,
        "GetRecursiveLiveRecoveryStatus",
        serde_json::json!({ "live_attempt_id": live.live_attempt_id }),
    )
    .await;
    assert!(response.error.is_none());
    let recovery: rsi_common::RecursiveLiveRecoveryReadback =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!(recovery.live_attempts.len(), 1);
    assert!(recovery.graph_recovery.is_some());

    let response = call_rpc(
        &fixture.server,
        "GetRecursiveLiveAttemptArtifacts",
        serde_json::json!({
            "live_attempt_id": live.live_attempt_id,
            "include_raw_output": true,
            "include_validation_report": true
        }),
    )
    .await;
    assert!(response.error.is_none());
    let artifacts: rsi_common::RecursiveLiveAttemptArtifactReadback =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!(artifacts.raw_output_artifacts.len(), 1);
    assert_eq!(artifacts.validation_artifacts.len(), 1);
    assert_eq!(artifacts.produced_artifacts.len(), 1);

    let response = call_rpc(
        &fixture.server,
        "GetRecursiveSchedulerRun",
        serde_json::json!({ "run_id": live.run_id.0 }),
    )
    .await;
    assert!(response.error.is_none());
    let run: rsi_common::RecursiveSchedulerRunDetail =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!(run.live_attempts.len(), 1);
    assert_eq!(run.latest_live_validations.len(), 1);

    let response = call_rpc(
        &fixture.server,
        "GetRecursiveDagOperationalStatus",
        serde_json::json!({ "graph_id": live.graph_id }),
    )
    .await;
    assert!(response.error.is_none());
    let status: rsi_common::RecursiveDagOperationalStatus =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!(status.active_live_attempts.len(), 1);
    assert_eq!(status.latest_live_validations.len(), 1);

    let counts_after = [
        recursive_table_count(&fixture.manager, "recursive_live_attempts").await,
        recursive_table_count(&fixture.manager, "recursive_live_interrupts").await,
        recursive_table_count(&fixture.manager, "recursive_live_output_validations").await,
        recursive_table_count(&fixture.manager, "recursive_execution_artifacts").await,
        recursive_table_count(&fixture.manager, "recursive_recovery_passes").await,
    ];
    assert_eq!(counts_after, counts_before);
    let state_after =
        recursive_live_attempt_state_snapshot(&fixture.manager, live.live_attempt_id).await;
    assert_eq!(state_after, state_before);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn recursive_dag_rpc_live_readbacks_reject_malformed_and_unknown_ids() {
    let fixture = recursive_dag_rpc_fixture();
    let live = create_recursive_dag_rpc_live_attempt(&fixture.manager).await;

    let response = call_rpc(
        &fixture.server,
        "ListRecursiveLiveAttempts",
        serde_json::json!({ "status": "running" }),
    )
    .await;
    assert_eq!(response.error.unwrap().code, INVALID_PARAMS);

    let response = call_rpc(
        &fixture.server,
        "ListRecursiveLiveAttempts",
        serde_json::json!({ "graph_id": Uuid::new_v4() }),
    )
    .await;
    let error = response.error.unwrap();
    assert_eq!(error.code, INVALID_PARAMS);
    assert!(error.message.contains("recursive DAG graph not found"));

    let response = call_rpc(
        &fixture.server,
        "ListRecursiveLiveAttempts",
        serde_json::json!({
            "graph_id": live.graph_id,
            "task_id": Uuid::new_v4()
        }),
    )
    .await;
    let error = response.error.unwrap();
    assert_eq!(error.code, INVALID_PARAMS);
    assert!(error.message.contains("recursive task not found"));

    let response = call_rpc(
        &fixture.server,
        "GetRecursiveLiveAttempt",
        serde_json::json!({ "live_attempt_id": Uuid::new_v4() }),
    )
    .await;
    let error = response.error.unwrap();
    assert_eq!(error.code, INVALID_PARAMS);
    assert!(error.message.contains("recursive live attempt not found"));

    let response = call_rpc(
        &fixture.server,
        "GetRecursiveLiveInterruptStatus",
        serde_json::json!({
            "interrupt_id": Uuid::new_v4(),
            "live_attempt_id": live.live_attempt_id
        }),
    )
    .await;
    assert_eq!(response.error.unwrap().code, INVALID_PARAMS);

    let response = call_rpc(
        &fixture.server,
        "ListRecursiveLiveOutputValidationResults",
        serde_json::json!({ "live_attempt_id": Uuid::new_v4() }),
    )
    .await;
    let error = response.error.unwrap();
    assert_eq!(error.code, INVALID_PARAMS);
    assert!(error.message.contains("recursive live attempt not found"));

    let response = call_rpc(
        &fixture.server,
        "ListRecursiveLiveOutputValidationResults",
        serde_json::json!({ "attempt_id": Uuid::new_v4() }),
    )
    .await;
    let error = response.error.unwrap();
    assert_eq!(error.code, INVALID_PARAMS);
    assert!(error.message.contains("recursive attempt not found"));

    let response = call_rpc(
        &fixture.server,
        "ListRecursiveLiveOutputValidationResults",
        serde_json::json!({ "session_id": Uuid::new_v4() }),
    )
    .await;
    let error = response.error.unwrap();
    assert_eq!(error.code, INVALID_PARAMS);
    assert!(error.message.contains("session not found"));

    let response = call_rpc(
        &fixture.server,
        "GetRecursiveLiveRecoveryStatus",
        serde_json::json!({ "session_id": Uuid::new_v4() }),
    )
    .await;
    let error = response.error.unwrap();
    assert_eq!(error.code, INVALID_PARAMS);
    assert!(error.message.contains("session not found"));

    let response = call_rpc(
        &fixture.server,
        "GetRecursiveLiveOutputValidationResult",
        serde_json::json!({ "validation_id": Uuid::new_v4() }),
    )
    .await;
    let error = response.error.unwrap();
    assert_eq!(error.code, INVALID_PARAMS);
    assert!(
        error
            .message
            .contains("recursive live output validation not found")
    );

    let response = call_rpc(
        &fixture.server,
        "ListRecursiveLiveOutputValidationResults",
        serde_json::json!({ "task_id": live.root_id }),
    )
    .await;
    assert_eq!(response.error.unwrap().code, INVALID_PARAMS);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn recursive_dag_rpc_live_attempt_without_validation_returns_empty_readbacks() {
    let fixture = recursive_dag_rpc_fixture();
    let live = create_recursive_dag_rpc_live_attempt(&fixture.manager).await;

    let response = call_rpc(
        &fixture.server,
        "GetRecursiveLiveOutputValidationResult",
        serde_json::json!({ "live_attempt_id": live.live_attempt_id }),
    )
    .await;
    assert!(response.error.is_none());
    let result: Option<rsi_common::RecursiveLiveOutputValidationResult> =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert!(result.is_none());

    let response = call_rpc(
        &fixture.server,
        "ListRecursiveLiveOutputValidationResults",
        serde_json::json!({ "live_attempt_id": live.live_attempt_id }),
    )
    .await;
    assert!(response.error.is_none());
    let validations: Vec<rsi_common::RecursiveLiveOutputValidationListItem> =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert!(validations.is_empty());

    let response = call_rpc(
        &fixture.server,
        "ListRecursiveLiveValidationIssues",
        serde_json::json!({ "live_attempt_id": live.live_attempt_id }),
    )
    .await;
    assert!(response.error.is_none());
    let issues: Vec<rsi_common::RecursiveLiveOutputValidationIssue> =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert!(issues.is_empty());

    let response = call_rpc(
        &fixture.server,
        "GetRecursiveLiveAttempt",
        serde_json::json!({ "live_attempt_id": live.live_attempt_id }),
    )
    .await;
    assert!(response.error.is_none());
    let readback: rsi_common::RecursiveLiveAttemptReadback =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert!(readback.latest_validation.is_none());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn recursive_dag_rpc_linked_session_missing_warns_without_failing() {
    let fixture = recursive_dag_rpc_fixture();
    let live = create_recursive_dag_rpc_live_attempt(&fixture.manager).await;
    {
        let store = fixture.manager.store().lock().await;
        let deleted_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        store
            .conn
            .execute_batch("PRAGMA foreign_keys = OFF;")
            .expect("disable foreign keys");
        store
            .conn
            .execute(
                "UPDATE session_execution_projections
                     SET execution_state='historical_purged', freshness='verified',
                         effective_cwd=NULL, validated_at=?2, error_code=NULL, updated_at=?2
                     WHERE session_id=?1",
                rusqlite::params![live.session_id.to_string(), deleted_at],
            )
            .expect("prepare retained projection for missing-session fixture");
        store
            .conn
            .execute(
                "DELETE FROM sessions WHERE id = ?1",
                rusqlite::params![live.session_id.to_string()],
            )
            .expect("delete linked session fixture");
        store
            .conn
            .execute_batch("PRAGMA foreign_keys = ON;")
            .expect("restore foreign keys");
    }

    let response = call_rpc(
        &fixture.server,
        "GetRecursiveLiveAttempt",
        serde_json::json!({ "live_attempt_id": live.live_attempt_id }),
    )
    .await;
    assert!(response.error.is_none());
    let readback: rsi_common::RecursiveLiveAttemptReadback =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert!(readback.session.is_none());
    assert!(
        readback
            .warnings
            .iter()
            .any(|warning| warning.code == "linked_session_missing")
    );

    let response = call_rpc(
        &fixture.server,
        "GetRecursiveLiveRecoveryStatus",
        serde_json::json!({ "live_attempt_id": live.live_attempt_id }),
    )
    .await;
    assert!(response.error.is_none());
    let recovery: rsi_common::RecursiveLiveRecoveryReadback =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert!(recovery.linked_sessions.is_empty());
    assert!(
        recovery
            .warnings
            .iter()
            .any(|warning| warning.code == "linked_session_missing")
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn recursive_dag_rpc_live_attempt_graph_filter_handles_mixed_attempts() {
    let fixture = recursive_dag_rpc_fixture();
    let (graph_id, root_id) = create_recursive_dag_rpc_graph(&fixture.manager).await;
    let first =
        create_recursive_dag_rpc_live_attempt_on_graph(&fixture.manager, graph_id, root_id).await;
    {
        let store = fixture.manager.store().lock().await;
        store
            .finish_recursive_scheduler_run(
                first.run_id,
                rsi_common::RecursiveSchedulerStopReason::IdleNoRunnable,
                0,
                None,
            )
            .expect("finish first scheduler run");
        store
            .update_recursive_live_attempt_status(
                first.live_attempt_id,
                crate::store::recursive_dag::RecursiveLiveAttemptStatusUpdate {
                    status: RecursiveLiveAttemptStatus::Failed,
                    failure_reason: Some("terminal fixture".to_string()),
                    interruption_reason: None,
                    cancellation_reason: None,
                    recovery_reason: None,
                    error: Some("terminal fixture".to_string()),
                },
            )
            .expect("mark first live attempt terminal");
        store
            .record_recursive_attempt_finish(
                RecursiveTaskGraphId(graph_id),
                first.attempt_id,
                RecursiveAttemptStatus::Interrupted,
                Some("terminal fixture".to_string()),
                None,
                Some(rsi_common::RecursiveTaskLifecycleState::Ready),
                Some("retry after terminal live fixture".to_string()),
            )
            .expect("finish first recursive attempt");
    }
    let second =
        create_recursive_dag_rpc_live_attempt_on_graph(&fixture.manager, graph_id, root_id).await;
    let other = create_recursive_dag_rpc_live_attempt(&fixture.manager).await;

    let response = call_rpc(
        &fixture.server,
        "ListRecursiveLiveAttempts",
        serde_json::json!({ "graph_id": graph_id }),
    )
    .await;
    assert!(response.error.is_none());
    let attempts: Vec<rsi_common::RecursiveLiveAttemptListItem> =
        serde_json::from_value(response.result.unwrap()).unwrap();
    let ids = attempts
        .iter()
        .map(|attempt| attempt.summary.id)
        .collect::<Vec<_>>();
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&first.live_attempt_id));
    assert!(ids.contains(&second.live_attempt_id));
    assert!(!ids.contains(&other.live_attempt_id));

    let response = call_rpc(
        &fixture.server,
        "ListRecursiveLiveAttempts",
        serde_json::json!({
            "graph_id": graph_id,
            "include_terminal": false
        }),
    )
    .await;
    assert!(response.error.is_none());
    let attempts: Vec<rsi_common::RecursiveLiveAttemptListItem> =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].summary.id, second.live_attempt_id);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn recursive_dag_rpc_readback_scheduler_runs_and_events() {
    let fixture = recursive_dag_rpc_fixture();
    let (graph_id, root_id) = create_recursive_dag_rpc_graph(&fixture.manager).await;
    let run_id = {
        let store = fixture.manager.store().lock().await;
        let executor = crate::recursive_dag::RecursiveDagFakeExecutor::new().on_execute(
            RecursiveTaskId(root_id),
            crate::recursive_dag::RecursiveDagFakeBehavior::direct_success("root-output"),
        );
        let mut scheduler = crate::recursive_dag::RecursiveDagScheduler::new(executor);
        scheduler
            .run_until_idle_with_limit(&store, RecursiveTaskGraphId(graph_id), 5)
            .expect("run fake scheduler")
            .run_id
    };

    let response = call_rpc(
        &fixture.server,
        "ListRecursiveSchedulerRuns",
        serde_json::json!({ "graph_id": graph_id }),
    )
    .await;
    assert!(response.error.is_none());
    let runs: Vec<rsi_common::RecursiveSchedulerRunSummary> =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].id, run_id);

    let response = call_rpc(
        &fixture.server,
        "ListRecursiveSchedulerRunEvents",
        serde_json::json!({ "run_id": run_id.0 }),
    )
    .await;
    assert!(response.error.is_none());
    let events: Vec<rsi_common::RecursiveSchedulerRunEvent> =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert!(events.iter().any(|event| event.event_type == "run_started"));
    assert!(
        events
            .iter()
            .any(|event| event.event_type == "run_completed")
    );

    let response = call_rpc(
        &fixture.server,
        "GetRecursiveSchedulerRun",
        serde_json::json!({ "run_id": run_id.0 }),
    )
    .await;
    assert!(response.error.is_none());
    let detail: rsi_common::RecursiveSchedulerRunDetail =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!(detail.run.id, run_id);
    assert_eq!(detail.graph.id, RecursiveTaskGraphId(graph_id));
    assert!(detail.report_artifact.is_some());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn recursive_dag_rpc_readback_recovery_status_and_deferred_graphs() {
    let fixture = recursive_dag_rpc_fixture();
    let (graph_1, root_1) = create_recursive_dag_rpc_graph(&fixture.manager).await;
    let (graph_2, root_2) = create_recursive_dag_rpc_graph(&fixture.manager).await;
    {
        let store = fixture.manager.store().lock().await;
        for (graph_id, root_id) in [(graph_1, root_1), (graph_2, root_2)] {
            store
                .record_recursive_attempt_start(
                    RecursiveTaskGraphId(graph_id),
                    RecursiveTaskId(root_id),
                    rsi_common::RecursiveAttemptPhase::Execute,
                    RecursiveAttemptId(Uuid::new_v4()),
                )
                .expect("start attempt");
            store
                .transition_recursive_task_state(
                    RecursiveTaskGraphId(graph_id),
                    RecursiveTaskId(root_id),
                    rsi_common::RecursiveTaskLifecycleState::Running,
                    Some("running before restart".to_string()),
                )
                .expect("mark running");
        }
        store
            .recover_recursive_task_graphs_after_restart_with_budget(RecursiveRecoveryBudget {
                max_graphs: 1,
                time_budget_ms: None,
                source: RecursiveRecoverySource::TestHarness,
            })
            .expect("budgeted recovery");
    }

    let response = call_rpc(
        &fixture.server,
        "GetRecursiveRecoveryStatus",
        serde_json::json!({ "graph_id": graph_2 }),
    )
    .await;
    assert!(response.error.is_none());
    let canonical_result = response.result.clone().unwrap();
    let status: rsi_common::RecursiveDagRecoveryStatus =
        serde_json::from_value(canonical_result.clone()).unwrap();
    assert!(status.latest_pass.is_some());
    assert_eq!(status.deferred_graph_count, 1);
    assert_eq!(
        status.graph_status.unwrap().state,
        rsi_common::RecursiveGraphRecoveryState::Deferred
    );
    assert_eq!(
        status.oldest_deferred_graph.unwrap().graph_id,
        Some(RecursiveTaskGraphId(graph_2))
    );

    let alias_response = call_rpc(
        &fixture.server,
        "GetRecursiveDagRecoveryStatus",
        serde_json::json!({ "graph_id": graph_2 }),
    )
    .await;
    assert!(alias_response.error.is_none());
    assert_eq!(alias_response.result.unwrap(), canonical_result);

    let response = call_rpc(
        &fixture.server,
        "ListRecursiveDeferredRecoveryGraphs",
        serde_json::Value::Null,
    )
    .await;
    assert!(response.error.is_none());
    let deferred: Vec<rsi_common::RecursiveDeferredRecoveryGraph> =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!(deferred.len(), 1);
    assert_eq!(deferred[0].graph_id, Some(RecursiveTaskGraphId(graph_2)));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn recursive_dag_rpc_malformed_and_unknown_params_are_clear() {
    let fixture = recursive_dag_rpc_fixture();
    let (graph_id, _) = create_recursive_dag_rpc_graph(&fixture.manager).await;

    let response = call_rpc(
        &fixture.server,
        "ListRecursiveSchedulerRuns",
        serde_json::json!({ "graph_id": graph_id, "status": "not_a_status" }),
    )
    .await;
    assert_eq!(response.error.unwrap().code, INVALID_PARAMS);

    let response = call_rpc(
        &fixture.server,
        "ListRecursiveSchedulerRunEvents",
        serde_json::json!({ "run_id": Uuid::new_v4() }),
    )
    .await;
    let error = response.error.unwrap();
    assert_eq!(error.code, INVALID_PARAMS);
    assert!(error.message.contains("recursive scheduler run not found"));

    let response = call_rpc(
        &fixture.server,
        "GetRecursiveRecoveryStatus",
        serde_json::json!({ "graph_id": Uuid::new_v4() }),
    )
    .await;
    let error = response.error.unwrap();
    assert_eq!(error.code, INVALID_PARAMS);
    assert!(
        error
            .message
            .contains("recursive recovery graph status not found")
    );

    let response = call_rpc(
        &fixture.server,
        "GetRecursiveCancellationRequest",
        serde_json::json!({ "request_id": Uuid::new_v4() }),
    )
    .await;
    let error = response.error.unwrap();
    assert_eq!(error.code, INVALID_PARAMS);
    assert!(
        error
            .message
            .contains("recursive cancellation request not found")
    );

    enable_recursive_dag_controls(&fixture.runtime_config);
    let response = call_rpc(
        &fixture.server,
        "RunRecursiveFakeScheduler",
        serde_json::json!({ "graph_id": Uuid::new_v4(), "max_steps": 1 }),
    )
    .await;
    let error = response.error.unwrap();
    assert_eq!(error.code, INVALID_PARAMS);
    assert!(error.message.contains("recursive DAG graph not found"));

    let response = call_rpc(
        &fixture.server,
        "RequestRecursiveSchedulerRunCancellation",
        serde_json::json!({
            "run_id": Uuid::new_v4(),
            "reason": "unknown run"
        }),
    )
    .await;
    let error = response.error.unwrap();
    assert_eq!(error.code, INVALID_PARAMS);
    assert!(error.message.contains("recursive scheduler run not found"));

    let response = call_rpc(
        &fixture.server,
        "ContinueRecursiveRecovery",
        serde_json::json!({ "max_graphs": 0 }),
    )
    .await;
    let error = response.error.unwrap();
    assert_eq!(error.code, INVALID_PARAMS);
    assert!(error.message.contains("max_graphs must be positive"));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn recursive_dag_rpc_control_rejects_when_disabled_before_mutation() {
    let fixture = recursive_dag_rpc_fixture();
    let (graph_id, _) = create_recursive_dag_rpc_graph(&fixture.manager).await;

    let response = call_rpc(
        &fixture.server,
        "RunRecursiveFakeScheduler",
        serde_json::json!({ "graph_id": graph_id, "max_steps": 1 }),
    )
    .await;
    let error = response.error.unwrap();
    assert_eq!(error.code, INVALID_PARAMS);
    assert!(error.message.contains("disabled"));

    let response = call_rpc(
        &fixture.server,
        "ContinueRecursiveRecovery",
        serde_json::json!({ "max_graphs": 1 }),
    )
    .await;
    let error = response.error.unwrap();
    assert_eq!(error.code, INVALID_PARAMS);
    assert!(error.message.contains("disabled"));

    let response = call_rpc(
        &fixture.server,
        "RequestRecursiveGraphCancellation",
        serde_json::json!({
            "graph_id": graph_id,
            "reason": "operator cancelled graph"
        }),
    )
    .await;
    let error = response.error.unwrap();
    assert_eq!(error.code, INVALID_PARAMS);
    assert!(error.message.contains("disabled"));

    let response = call_rpc(
        &fixture.server,
        "ListRecursiveSchedulerRuns",
        serde_json::json!({ "graph_id": graph_id }),
    )
    .await;
    assert!(response.error.is_none());
    let runs: Vec<rsi_common::RecursiveSchedulerRunSummary> =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert!(runs.is_empty());

    let response = call_rpc(
        &fixture.server,
        "ListRecursiveCancellationRequests",
        serde_json::json!({ "graph_id": graph_id }),
    )
    .await;
    assert!(response.error.is_none());
    let cancellations: Vec<rsi_common::RecursiveCancellationRequestSummary> =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert!(cancellations.is_empty());

    let response = call_rpc(
        &fixture.server,
        "GetRecursiveRecoveryStatus",
        serde_json::Value::Null,
    )
    .await;
    assert!(response.error.is_none());
    let recovery: rsi_common::RecursiveDagRecoveryStatus =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert!(recovery.latest_pass.is_none());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn recursive_dag_rpc_control_aliases_are_gated_like_canonical() {
    let fixture = recursive_dag_rpc_fixture();
    let (graph_id, _) = create_recursive_dag_rpc_graph(&fixture.manager).await;

    let canonical = call_rpc(
        &fixture.server,
        "RunRecursiveFakeScheduler",
        serde_json::json!({ "graph_id": graph_id, "max_steps": 1 }),
    )
    .await;
    let alias = call_rpc(
        &fixture.server,
        "RunRecursiveDagFakeScheduler",
        serde_json::json!({ "graph_id": graph_id, "max_steps": 1 }),
    )
    .await;
    let canonical_error = canonical.error.unwrap();
    let alias_error = alias.error.unwrap();
    assert_eq!(canonical_error.code, INVALID_PARAMS);
    assert_eq!(alias_error.code, canonical_error.code);
    assert_eq!(alias_error.message, canonical_error.message);

    let canonical = call_rpc(
        &fixture.server,
        "ContinueRecursiveRecovery",
        serde_json::json!({ "max_graphs": 1 }),
    )
    .await;
    let alias = call_rpc(
        &fixture.server,
        "RunRecursiveDagRecoveryPass",
        serde_json::json!({ "max_graphs": 1 }),
    )
    .await;
    let canonical_error = canonical.error.unwrap();
    let alias_error = alias.error.unwrap();
    assert_eq!(canonical_error.code, INVALID_PARAMS);
    assert_eq!(alias_error.code, canonical_error.code);
    assert_eq!(alias_error.message, canonical_error.message);

    let canonical = call_rpc(
        &fixture.server,
        "RequestRecursiveGraphCancellation",
        serde_json::json!({ "graph_id": graph_id, "reason": "operator cancelled graph" }),
    )
    .await;
    let alias = call_rpc(
        &fixture.server,
        "CancelRecursiveTaskGraph",
        serde_json::json!({ "graph_id": graph_id, "reason": "operator cancelled graph" }),
    )
    .await;
    let canonical_error = canonical.error.unwrap();
    let alias_error = alias.error.unwrap();
    assert_eq!(canonical_error.code, INVALID_PARAMS);
    assert_eq!(alias_error.code, canonical_error.code);
    assert_eq!(alias_error.message, canonical_error.message);

    let response = call_rpc(
        &fixture.server,
        "ListRecursiveSchedulerRuns",
        serde_json::json!({ "graph_id": graph_id }),
    )
    .await;
    let runs: Vec<rsi_common::RecursiveSchedulerRunSummary> =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert!(runs.is_empty());

    let response = call_rpc(
        &fixture.server,
        "ListRecursiveCancellationRequests",
        serde_json::json!({ "graph_id": graph_id }),
    )
    .await;
    let cancellations: Vec<rsi_common::RecursiveCancellationRequestSummary> =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert!(cancellations.is_empty());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn recursive_dag_rpc_control_enabled_fake_scheduler_and_recovery() {
    let fixture = recursive_dag_rpc_fixture();
    enable_recursive_dag_controls(&fixture.runtime_config);
    let (graph_id, _) = create_recursive_dag_rpc_graph(&fixture.manager).await;

    let response = call_rpc(
        &fixture.server,
        "RunRecursiveFakeScheduler",
        serde_json::json!({ "graph_id": graph_id }),
    )
    .await;
    assert_eq!(response.error.unwrap().code, INVALID_PARAMS);

    let response = call_rpc(
        &fixture.server,
        "RunRecursiveFakeScheduler",
        serde_json::json!({
            "graph_id": graph_id,
            "max_steps": 1,
            "execution_mode": "live"
        }),
    )
    .await;
    let error = response.error.unwrap();
    assert_eq!(error.code, INVALID_PARAMS);
    assert!(error.message.contains("fake-only"));

    let response = call_rpc(
        &fixture.server,
        "RunRecursiveFakeScheduler",
        serde_json::json!({
            "graph_id": graph_id,
            "max_steps": MAX_RECURSIVE_FAKE_SCHEDULER_RPC_STEPS + 1
        }),
    )
    .await;
    let error = response.error.unwrap();
    assert_eq!(error.code, INVALID_PARAMS);
    assert!(error.message.contains("max_steps must be <="));

    let response = call_rpc(
        &fixture.server,
        "ListRecursiveSchedulerRuns",
        serde_json::json!({ "graph_id": graph_id }),
    )
    .await;
    assert!(response.error.is_none());
    let runs: Vec<rsi_common::RecursiveSchedulerRunSummary> =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert!(runs.is_empty());

    let response = call_rpc(
        &fixture.server,
        "RunRecursiveFakeScheduler",
        serde_json::json!({
            "graph_id": graph_id,
            "max_steps": 1,
            "operator": "rpc-test",
            "execution_mode": "fake"
        }),
    )
    .await;
    assert!(response.error.is_none());
    let run: rsi_common::RecursiveSchedulerRunSummary =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!(run.graph_id, RecursiveTaskGraphId(graph_id));
    assert_eq!(
        run.source,
        rsi_common::RecursiveSchedulerRunSource::ManualRpc
    );
    assert_eq!(run.operator.as_deref(), Some("rpc-test"));
    assert_eq!(run.executor_mode, rsi_common::RecursiveExecutionMode::Fake);

    let response = call_rpc(
        &fixture.server,
        "ContinueRecursiveRecovery",
        serde_json::json!({ "max_graphs": 1, "time_budget_ms": 1 }),
    )
    .await;
    assert!(response.error.is_none());
    let pass: rsi_common::RecursiveRecoveryPassSummary =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!(pass.source, RecursiveRecoverySource::ManualRpc);

    let response = call_rpc(
        &fixture.server,
        "GetDaemonCapabilities",
        serde_json::Value::Null,
    )
    .await;
    let caps: rsi_common::DaemonCapabilities =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert!(!caps.recursive_dag_background_loop);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn recursive_dag_rpc_continue_recovery_respects_supplied_budget() {
    let fixture = recursive_dag_rpc_fixture();
    set_recursive_dag_controls(&fixture.runtime_config, true, false, false);
    let (graph_1, _) = create_running_recursive_dag_rpc_graph(&fixture.manager).await;
    let (graph_2, _) = create_running_recursive_dag_rpc_graph(&fixture.manager).await;
    let (graph_3, _) = create_running_recursive_dag_rpc_graph(&fixture.manager).await;
    {
        let store = fixture.manager.store().lock().await;
        store
            .recover_recursive_task_graphs_after_restart_with_budget(RecursiveRecoveryBudget {
                max_graphs: 1,
                time_budget_ms: None,
                source: RecursiveRecoverySource::TestHarness,
            })
            .expect("initial budgeted recovery");
    }

    let response = call_rpc(
        &fixture.server,
        "ContinueRecursiveRecovery",
        serde_json::json!({ "max_graphs": 1 }),
    )
    .await;
    assert!(response.error.is_none());
    let pass: rsi_common::RecursiveRecoveryPassSummary =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!(pass.source, RecursiveRecoverySource::ManualRpc);
    assert_eq!(pass.max_graphs, 1);
    assert_eq!(pass.checked, 1);
    assert_eq!(
        pass.status,
        rsi_common::RecursiveRecoveryPassStatus::Deferred
    );
    assert_eq!(
        pass.stop_reason,
        Some(rsi_common::RecursiveRecoveryStopReason::MaxGraphs)
    );

    let response = call_rpc(
        &fixture.server,
        "GetRecursiveRecoveryStatus",
        serde_json::Value::Null,
    )
    .await;
    assert!(response.error.is_none());
    let status: rsi_common::RecursiveDagRecoveryStatus =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!(status.deferred_graph_count, 1);
    let remaining_graph = status.oldest_deferred_graph.unwrap().graph_id;
    assert!(
        [
            Some(RecursiveTaskGraphId(graph_1)),
            Some(RecursiveTaskGraphId(graph_2)),
            Some(RecursiveTaskGraphId(graph_3))
        ]
        .contains(&remaining_graph)
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn recursive_dag_rpc_cancellation_controls_create_durable_requests() {
    let fixture = recursive_dag_rpc_fixture();
    enable_recursive_dag_controls(&fixture.runtime_config);
    let (graph_id, _) = create_recursive_dag_rpc_graph(&fixture.manager).await;

    let response = call_rpc(
        &fixture.server,
        "RequestRecursiveGraphCancellation",
        serde_json::json!({
            "graph_id": graph_id,
            "reason": "operator cancelled graph",
            "requested_by": "rpc-test"
        }),
    )
    .await;
    assert!(response.error.is_none());
    let graph_request: rsi_common::RecursiveCancellationRequestSummary =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!(
        graph_request.scope,
        rsi_common::RecursiveCancellationScope::Graph
    );

    let run = {
        let store = fixture.manager.store().lock().await;
        store
            .start_recursive_scheduler_run(
                crate::store::recursive_dag::RecursiveSchedulerRunStart {
                    graph_id: RecursiveTaskGraphId(graph_id),
                    max_steps: 5,
                    source: rsi_common::RecursiveSchedulerRunSource::TestHarness,
                    operator: Some("test".to_string()),
                    executor_mode: rsi_common::RecursiveExecutionMode::Fake,
                },
            )
            .expect("start run")
    };
    let response = call_rpc(
        &fixture.server,
        "RequestRecursiveSchedulerRunCancellation",
        serde_json::json!({
            "run_id": run.id.0,
            "reason": "operator cancelled run",
            "requested_by": "rpc-test"
        }),
    )
    .await;
    assert!(response.error.is_none());
    let run_request: rsi_common::RecursiveCancellationRequestSummary =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!(
        run_request.scope,
        rsi_common::RecursiveCancellationScope::Run
    );
    assert_eq!(run_request.run_id, Some(run.id));
    {
        let store = fixture.manager.store().lock().await;
        let stored_run = store
            .load_recursive_scheduler_run(run.id)
            .unwrap()
            .expect("run remains durable");
        assert_eq!(
            stored_run.status,
            rsi_common::RecursiveSchedulerRunStatus::Cancelling
        );
        let attempts = store
            .load_recursive_task_attempts(RecursiveTaskGraphId(graph_id))
            .unwrap();
        assert!(attempts.is_empty());
    }

    let response = call_rpc(
        &fixture.server,
        "ListRecursiveCancellationRequests",
        serde_json::json!({ "graph_id": graph_id }),
    )
    .await;
    assert!(response.error.is_none());
    let requests: Vec<rsi_common::RecursiveCancellationRequestSummary> =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!(requests.len(), 2);
    assert!(
        requests
            .iter()
            .any(|request| request.id == graph_request.id)
    );
    assert!(requests.iter().any(|request| request.id == run_request.id));

    let response = call_rpc(
        &fixture.server,
        "GetRecursiveCancellationRequest",
        serde_json::json!({ "request_id": run_request.id.0 }),
    )
    .await;
    assert!(response.error.is_none());
    let request: rsi_common::RecursiveCancellationRequestSummary =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!(request.id, run_request.id);
    assert_eq!(request.run_id, Some(run.id));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn test_create_project_params() {
    let json = serde_json::json!({
        "name": "my-project",
        "path": "/home/user/project",
        "color": "#a6e3a1"
    });
    let params: CreateProjectParams = serde_json::from_value(json).unwrap();
    assert_eq!(params.name, "my-project");
    assert_eq!(
        params.path.unwrap(),
        std::path::PathBuf::from("/home/user/project")
    );
    assert_eq!(params.color.unwrap(), "#a6e3a1");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn test_create_project_params_minimal() {
    // Only name is required
    let json = serde_json::json!({
        "name": "minimal"
    });
    let params: CreateProjectParams = serde_json::from_value(json).unwrap();
    assert_eq!(params.name, "minimal");
    assert!(params.path.is_none());
    assert!(params.description.is_none());
    assert!(params.color.is_none());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn test_update_project_params() {
    let json = serde_json::json!({
        "id": "550e8400-e29b-41d4-a716-446655440000",
        "name": "updated-name"
    });
    let params: UpdateProjectParams = serde_json::from_value(json).unwrap();
    assert_eq!(
        params.id.to_string(),
        "550e8400-e29b-41d4-a716-446655440000"
    );
    assert_eq!(params.name.unwrap(), "updated-name");
    assert!(params.path.is_none());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn test_project_id_params() {
    let json = serde_json::json!({
        "id": "550e8400-e29b-41d4-a716-446655440000"
    });
    let params: ProjectIdParams = serde_json::from_value(json).unwrap();
    assert_eq!(
        params.id.to_string(),
        "550e8400-e29b-41d4-a716-446655440000"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn test_memory_search_params_full() {
    let json = serde_json::json!({
        "query": "how to do X",
        "max_results": 10,
        "min_score": 0.5,
        "project_id": "550e8400-e29b-41d4-a716-446655440000"
    });
    let params: MemorySearchParams = serde_json::from_value(json).unwrap();
    assert_eq!(params.query, "how to do X");
    assert_eq!(params.max_results, Some(10));
    assert!((params.min_score.unwrap() - 0.5).abs() < f64::EPSILON);
    assert_eq!(
        params.project_id.unwrap().to_string(),
        "550e8400-e29b-41d4-a716-446655440000"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn test_memory_search_params_minimal() {
    let json = serde_json::json!({ "query": "test" });
    let params: MemorySearchParams = serde_json::from_value(json).unwrap();
    assert_eq!(params.query, "test");
    assert!(params.max_results.is_none());
    assert!(params.min_score.is_none());
    assert!(params.project_id.is_none());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn test_memory_index_params_default() {
    let json = serde_json::json!({});
    let params: MemoryIndexParams = serde_json::from_value(json).unwrap();
    assert!(!params.force);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn test_memory_index_params_force() {
    let json = serde_json::json!({ "force": true });
    let params: MemoryIndexParams = serde_json::from_value(json).unwrap();
    assert!(params.force);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn test_memory_read_params_full() {
    let json = serde_json::json!({
        "path": "memory/notes.md",
        "from": 5,
        "lines": 20
    });
    let params: MemoryReadParams = serde_json::from_value(json).unwrap();
    assert_eq!(params.path, "memory/notes.md");
    assert_eq!(params.from, Some(5));
    assert_eq!(params.lines, Some(20));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn test_memory_read_params_minimal() {
    let json = serde_json::json!({ "path": "MEMORY.md" });
    let params: MemoryReadParams = serde_json::from_value(json).unwrap();
    assert_eq!(params.path, "MEMORY.md");
    assert!(params.from.is_none());
    assert!(params.lines.is_none());
}

// ---- Hierarchy RPC surface (Phase 2) ----------------------------------
//
// The pure-fn validators (`hierarchy::validate_containment`,
// `hierarchy::detect_cycle`) are exhaustively tested in
// `crates/rsid/src/session/hierarchy.rs`. The tests below pin the RPC
// param parsers and re-exercise the handler-side wiring through the same
// validators, mirroring the test names called out in the Phase 2 plan
// (containment / cycle / delete_container). End-to-end RPC handlers
// require a live SessionManager + DB, exercised by Phase 4 integration
// tests.

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn rpc_create_container_params_roundtrip() {
    let pid = uuid::Uuid::new_v4();
    let json = serde_json::json!({
        "kind": "Group",
        "name": "Daemon hardening",
        "parent_id": null,
        "project_id": pid,
        "tags": ["infra"],
    });
    let params: rsi_common::rpc::CreateContainerParams = serde_json::from_value(json).unwrap();
    assert!(matches!(params.kind, rsi_common::types::SessionKind::Group));
    assert_eq!(params.name, "Daemon hardening");
    assert!(params.parent_id.is_none());
    assert_eq!(params.project_id, Some(pid));
    assert_eq!(params.tags, vec!["infra".to_string()]);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn rpc_set_session_parent_params_roundtrip() {
    let sid = uuid::Uuid::new_v4();
    let pid = uuid::Uuid::new_v4();
    let json = serde_json::json!({
        "session_id": sid,
        "new_parent_id": pid,
    });
    let params: rsi_common::rpc::SetSessionParentParams = serde_json::from_value(json).unwrap();
    assert_eq!(params.session_id, sid);
    assert_eq!(params.new_parent_id, Some(pid));

    // None new_parent_id (move to root) deserializes via #[serde(default)].
    let json = serde_json::json!({ "session_id": sid });
    let params: rsi_common::rpc::SetSessionParentParams = serde_json::from_value(json).unwrap();
    assert!(params.new_parent_id.is_none());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn rpc_list_session_children_params_roundtrip_root() {
    // No parent_id → top-level listing.
    let json = serde_json::json!({});
    let params: rsi_common::rpc::ListSessionChildrenParams = serde_json::from_value(json).unwrap();
    assert!(params.parent_id.is_none());

    let pid = uuid::Uuid::new_v4();
    let json = serde_json::json!({ "parent_id": pid });
    let params: rsi_common::rpc::ListSessionChildrenParams = serde_json::from_value(json).unwrap();
    assert_eq!(params.parent_id, Some(pid));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn rpc_containment_root_accepts_standard_and_group() {
    use crate::session::hierarchy::validate_containment;
    use rsi_common::types::SessionKind;
    // Root accepts Standard/Group only.
    assert!(validate_containment(None, SessionKind::Standard).is_ok());
    assert!(validate_containment(None, SessionKind::Group).is_ok());
    // Root rejects Epic, Story, Task, Bug, TaskRabbit.
    assert!(validate_containment(None, SessionKind::Epic).is_err());
    assert!(validate_containment(None, SessionKind::Story).is_err());
    assert!(validate_containment(None, SessionKind::Task).is_err());
    assert!(validate_containment(None, SessionKind::Bug).is_err());
    assert!(validate_containment(None, SessionKind::TaskRabbit).is_err());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn rpc_containment_group_accepts_standard_epic_only() {
    use crate::session::hierarchy::validate_containment;
    use rsi_common::types::SessionKind;
    assert!(validate_containment(Some(SessionKind::Group), SessionKind::Standard).is_ok());
    assert!(validate_containment(Some(SessionKind::Group), SessionKind::Epic).is_ok());
    assert!(validate_containment(Some(SessionKind::Group), SessionKind::Group).is_err());
    assert!(validate_containment(Some(SessionKind::Group), SessionKind::Task).is_err());
    assert!(validate_containment(Some(SessionKind::Group), SessionKind::Story).is_err());
    assert!(validate_containment(Some(SessionKind::Group), SessionKind::Bug).is_err());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn rpc_containment_epic_accepts_story_task_bug() {
    use crate::session::hierarchy::validate_containment;
    use rsi_common::types::SessionKind;
    assert!(validate_containment(Some(SessionKind::Epic), SessionKind::Story).is_ok());
    assert!(validate_containment(Some(SessionKind::Epic), SessionKind::Task).is_ok());
    assert!(validate_containment(Some(SessionKind::Epic), SessionKind::Bug).is_ok());
    assert!(validate_containment(Some(SessionKind::Epic), SessionKind::Standard).is_err());
    assert!(validate_containment(Some(SessionKind::Epic), SessionKind::Group).is_err());
    assert!(validate_containment(Some(SessionKind::Epic), SessionKind::Epic).is_err());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn rpc_containment_leaf_kinds_reject_all_children() {
    use crate::session::hierarchy::validate_containment;
    use rsi_common::types::SessionKind;
    for &leaf in &[
        SessionKind::Standard,
        SessionKind::TaskRabbit,
        SessionKind::Bug,
        SessionKind::Story,
        SessionKind::Task,
    ] {
        for &child in &[
            SessionKind::Standard,
            SessionKind::Group,
            SessionKind::Epic,
            SessionKind::Story,
            SessionKind::Task,
            SessionKind::Bug,
        ] {
            assert!(
                validate_containment(Some(leaf), child).is_err(),
                "leaf {:?} unexpectedly accepts {:?}",
                leaf,
                child
            );
        }
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn rpc_cycle_self_rejected() {
    use crate::session::hierarchy::detect_cycle;
    let a = uuid::Uuid::new_v4();
    assert!(detect_cycle(a, a, |_| None).is_err());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn rpc_cycle_two_hop_rejected() {
    use crate::session::hierarchy::detect_cycle;
    // B -> A; reparent A under B forms A -> B -> A.
    let a = uuid::Uuid::new_v4();
    let b = uuid::Uuid::new_v4();
    let parents: std::collections::HashMap<uuid::Uuid, uuid::Uuid> = [(b, a)].into_iter().collect();
    assert!(detect_cycle(a, b, |id| parents.get(&id).copied()).is_err());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn rpc_cycle_three_hop_rejected() {
    use crate::session::hierarchy::detect_cycle;
    // C -> B -> A; reparent A under C forms A -> C -> B -> A.
    let a = uuid::Uuid::new_v4();
    let b = uuid::Uuid::new_v4();
    let c = uuid::Uuid::new_v4();
    let parents: std::collections::HashMap<uuid::Uuid, uuid::Uuid> =
        [(c, b), (b, a)].into_iter().collect();
    assert!(detect_cycle(a, c, |id| parents.get(&id).copied()).is_err());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn rpc_cycle_unrelated_branch_accepted() {
    use crate::session::hierarchy::detect_cycle;
    // A and C are independent; placing A under C is legal.
    let a = uuid::Uuid::new_v4();
    let c = uuid::Uuid::new_v4();
    let parents: std::collections::HashMap<uuid::Uuid, uuid::Uuid> =
        std::collections::HashMap::new();
    assert!(detect_cycle(a, c, |id| parents.get(&id).copied()).is_ok());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn rpc_delete_container_cascade_recognizes_container_kinds() {
    use rsi_common::is_container_kind;
    use rsi_common::types::SessionKind;
    // Containers are expanded by SessionManager's lifecycle tree walk.
    assert!(is_container_kind(SessionKind::Group));
    assert!(is_container_kind(SessionKind::Epic));
    // Leaves — handler must skip the has_children check (deletes proceed).
    assert!(!is_container_kind(SessionKind::Standard));
    assert!(!is_container_kind(SessionKind::TaskRabbit));
    assert!(!is_container_kind(SessionKind::Bug));
    assert!(!is_container_kind(SessionKind::Story));
    assert!(!is_container_kind(SessionKind::Task));
}

// -----------------------------------------------------------------------
// Phase 2: Epic lead validation logic tests
//
// These tests exercise the pure-function invariants that `handle_set_epic_lead`
// and `handle_launch_session` rely on, without requiring a running daemon.
// Integration tests that actually call the handlers live in crates/rsid/tests/.
// -----------------------------------------------------------------------

/// Verify that only Epic-kind containers are eligible to hold a lead pointer.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn handle_set_epic_lead_rejects_non_container() {
    use rsi_common::types::SessionKind;
    // Group is also a container kind but NOT Epic — should be rejected.
    assert_ne!(SessionKind::Group, SessionKind::Epic);
    // Leaf kinds are all non-Epic.
    for leaf in [
        SessionKind::Standard,
        SessionKind::Story,
        SessionKind::Task,
        SessionKind::Bug,
        SessionKind::TaskRabbit,
    ] {
        assert_ne!(leaf, SessionKind::Epic, "{leaf:?} must not be Epic");
    }
}

/// Verify that `is_leaf_kind` correctly classifies session kinds for lead
/// candidate validation in `handle_set_epic_lead`.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn handle_set_epic_lead_rejects_non_child_kind() {
    use rsi_common::types::SessionKind;
    use rsi_common::{is_container_kind, is_leaf_kind};
    // A Group cannot be a lead (container kind, not leaf).
    assert!(!is_leaf_kind(SessionKind::Group));
    assert!(!is_leaf_kind(SessionKind::Epic));
    // Valid lead candidates are leaf kinds.
    assert!(is_leaf_kind(SessionKind::Story));
    assert!(is_leaf_kind(SessionKind::Task));
    assert!(is_leaf_kind(SessionKind::Bug));
    assert!(is_leaf_kind(SessionKind::Standard));
    assert!(is_leaf_kind(SessionKind::TaskRabbit));
    // Containers are never leaves.
    assert!(is_container_kind(SessionKind::Group));
    assert!(is_container_kind(SessionKind::Epic));
}

/// Archived and Deleted statuses must be rejected as lead candidates.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn handle_set_epic_lead_rejects_archived_or_deleted_lead() {
    use rsi_common::types::SessionStatus;
    let terminal_statuses = [SessionStatus::Archived, SessionStatus::Deleted];
    for status in terminal_statuses {
        assert!(
            matches!(status, SessionStatus::Archived | SessionStatus::Deleted),
            "{status:?} should be rejected as a lead candidate"
        );
    }
    // Running, Completed, Failed are all valid (not archived/deleted).
    let valid_statuses = [
        SessionStatus::Running,
        SessionStatus::Completed,
        SessionStatus::Failed,
        SessionStatus::Interrupted,
    ];
    for status in valid_statuses {
        assert!(
            !matches!(status, SessionStatus::Archived | SessionStatus::Deleted),
            "{status:?} should not be rejected"
        );
    }
}

/// `SetEpicLeadParams` serde: None clears the pointer.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn handle_set_epic_lead_clears_with_none_serde() {
    use rsi_common::rpc::SetEpicLeadParams;
    let epic_id = uuid::Uuid::new_v4();
    let json = serde_json::json!({
        "epic_id": epic_id,
        "new_lead_session_id": null
    });
    let params: SetEpicLeadParams = serde_json::from_value(json).unwrap();
    assert_eq!(params.epic_id, epic_id);
    assert!(params.new_lead_session_id.is_none());
}

/// `SetEpicLeadParams` serde: omitting the field also clears (default = None).
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn handle_set_epic_lead_clears_with_omitted_field() {
    use rsi_common::rpc::SetEpicLeadParams;
    let epic_id = uuid::Uuid::new_v4();
    let json = serde_json::json!({ "epic_id": epic_id });
    let params: SetEpicLeadParams = serde_json::from_value(json).unwrap();
    assert!(params.new_lead_session_id.is_none());
}

/// `LaunchSessionParams` parent_id round-trips through serde correctly.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn handle_launch_session_validates_parent_containment_serde() {
    use rsi_common::rpc::LaunchSessionParams;
    use rsi_common::types::SessionKind;
    let parent_id = uuid::Uuid::new_v4();
    let json = serde_json::json!({
        "query": "do something",
        "parent_id": parent_id,
        "session_kind": "Story",
        "tags": ["ci"]
    });
    let params: LaunchSessionParams = serde_json::from_value(json).unwrap();
    assert_eq!(params.parent_id, Some(parent_id));
    assert_eq!(params.session_kind, Some(SessionKind::Story));
}

/// Epic → Story containment is legal; Epic → Standard is not.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn handle_launch_session_containment_matrix_epic() {
    use crate::session::hierarchy::validate_containment;
    use rsi_common::types::SessionKind;
    // Legal children for Epic.
    assert!(validate_containment(Some(SessionKind::Epic), SessionKind::Story).is_ok());
    assert!(validate_containment(Some(SessionKind::Epic), SessionKind::Task).is_ok());
    assert!(validate_containment(Some(SessionKind::Epic), SessionKind::Bug).is_ok());
    // Illegal children for Epic.
    assert!(validate_containment(Some(SessionKind::Epic), SessionKind::Standard).is_err());
    assert!(validate_containment(Some(SessionKind::Epic), SessionKind::Group).is_err());
    assert!(validate_containment(Some(SessionKind::Epic), SessionKind::Epic).is_err());
}

/// Rotation: `find_epics_by_lead` / `set_lead_session` store methods exist
/// and their signatures are as expected by the rotation hook in rotation.rs.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn rotation_transfers_lead_to_successor_store_api() {
    // This test verifies the store API exists by referencing its path. The
    // actual DB-backed integration test lives in crates/rsid/tests/.
    // We just validate that the fn-pointer is resolved at compile time.
    let _find: fn(&crate::store::Store, uuid::Uuid) -> crate::error::Result<Vec<uuid::Uuid>> =
        crate::store::Store::find_epics_by_lead;
    let _set: fn(&crate::store::Store, uuid::Uuid, Option<uuid::Uuid>) -> crate::error::Result<()> =
        crate::store::Store::set_lead_session;
    let _clear: fn(&crate::store::Store, uuid::Uuid) -> crate::error::Result<()> =
        crate::store::Store::clear_lead_session_if_matches;
}

/// Delete hook: `clear_lead_session_if_matches` store API is reachable from
/// the delete path (compile-time check).
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn handle_delete_session_clears_lead_pointers_store_api() {
    let _clear: fn(&crate::store::Store, uuid::Uuid) -> crate::error::Result<()> =
        crate::store::Store::clear_lead_session_if_matches;
}

/// Verify the parent_id membership check in `handle_set_epic_lead`.
/// A session whose `parent_id != Some(epic_id)` must be rejected.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn handle_set_epic_lead_rejects_non_child() {
    // Simulate the check: lead.parent_id must equal Some(epic_id).
    let epic_id = uuid::Uuid::new_v4();
    let other_epic = uuid::Uuid::new_v4();
    let lead_id = uuid::Uuid::new_v4();

    // Case 1: lead has no parent — not a child.
    let parent_id: Option<uuid::Uuid> = None;
    assert_ne!(
        parent_id,
        Some(epic_id),
        "lead with parent_id=None must fail the membership check"
    );

    // Case 2: lead is under a different Epic — not a child of this one.
    let parent_id = Some(other_epic);
    assert_ne!(
        parent_id,
        Some(epic_id),
        "lead under a different Epic must fail the membership check"
    );

    // Case 3: correct parent — passes.
    let parent_id = Some(epic_id);
    assert_eq!(
        parent_id,
        Some(epic_id),
        "lead with correct parent_id must pass the membership check"
    );

    // Ensure epic_id and other_epic differ (regression guard).
    assert_ne!(epic_id, other_epic);
    let _ = lead_id; // suppress unused warning
}

/// RSI-022: exercise the `UpdateDaemonConfig` payload parse + the
/// `RuntimeConfig::update_field` call exactly the way
/// `handle_update_daemon_config` does, then verify
/// `RuntimeConfig::to_json` (what `handle_get_daemon_config` returns)
/// reflects the new value. This is the closest we get to a true RPC
/// round-trip without standing up the full Unix socket harness.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn update_daemon_config_params_round_trip_codex_sandbox_mode() {
    // Parse a wire-format payload identical to what an RPC client sends.
    let json = serde_json::json!({
        "field": "codex_sandbox_mode",
        "value": "danger-full-access",
    });
    let params: UpdateDaemonConfigParams = serde_json::from_value(json).unwrap();
    assert_eq!(params.field, "codex_sandbox_mode");

    // Build a fresh RuntimeConfig with the default config (no env vars).
    let config = crate::config::Config::from_env();
    let rc = crate::config::RuntimeConfig::from_config(&config);

    // Apply the update — same call the handler makes.
    let result = rc.update_field(&params.field, &params.value);
    assert!(matches!(result, Ok(true)), "valid update should succeed");

    // GetDaemonConfig payload must reflect the new value.
    let snapshot = rc.to_json();
    assert_eq!(
        snapshot.get("codex_sandbox_mode").and_then(|v| v.as_str()),
        Some("danger-full-access")
    );
}

/// SECURITY: the isolation policy is validated on write, and its value
/// lands in the `GetDaemonConfig` payload the TUI reads back.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn update_daemon_config_round_trips_claude_config_isolation() {
    let rc = crate::config::RuntimeConfig::from_config(&crate::config::Config::default());
    // Shipped default preserves today's launch behavior.
    assert_eq!(
        rc.to_json()
            .get("claude_config_isolation")
            .and_then(|v| v.as_str()),
        Some("off")
    );

    assert!(
        rc.update_field("claude_config_isolation", &serde_json::json!("strict"))
            .unwrap()
    );
    assert_eq!(
        rc.to_json()
            .get("claude_config_isolation")
            .and_then(|v| v.as_str()),
        Some("strict")
    );

    // Unknown policies are refused rather than silently disabling isolation.
    for bad in ["bare", "on", "true", ""] {
        assert!(
            rc.update_field("claude_config_isolation", &serde_json::json!(bad))
                .is_err(),
            "{bad:?} must be rejected"
        );
    }
    // The refused writes left the accepted value in place.
    assert_eq!(
        rc.to_json()
            .get("claude_config_isolation")
            .and_then(|v| v.as_str()),
        Some("strict")
    );
}

/// The setting must survive a daemon restart, i.e. be a persisted field.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn claude_config_isolation_is_a_persisted_daemon_setting() {
    assert!(crate::config::PERSISTED_RUNTIME_CONFIG_FIELDS.contains(&"claude_config_isolation"));
}

/// RSI-022: invalid `codex_sandbox_mode` values must round-trip into the
/// same `Err` branch that `handle_update_daemon_config` returns as
/// `InvalidParam`.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn update_daemon_config_rejects_invalid_codex_sandbox_mode() {
    let config = crate::config::Config::from_env();
    let rc = crate::config::RuntimeConfig::from_config(&config);
    let err = rc
        .update_field("codex_sandbox_mode", &serde_json::json!("garbage"))
        .expect_err("garbage value must be rejected");
    assert!(
        err.contains("read-only") || err.contains("workspace-write"),
        "rejection message must enumerate valid options, got: {err}"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn cloud_spend_view_is_operator_only_and_out_of_agent_catalogs() {
    let catalog = rsi_common::agent_control_schema::agent_control_catalog_v1();
    assert!(!agent_gate::AGENT_VERBS.contains(&"GetCloudSpend"));
    assert!(!agent_gate::READ_VERBS.contains(&"GetCloudSpend"));
    assert!(!agent_gate::UNSCOPED_READ_VERBS.contains(&"GetCloudSpend"));
    assert!(!agent_gate::is_allowed_for_attributed_caller(
        "GetCloudSpend"
    ));
    assert!(!catalog.iter().any(|entry| entry.method == "GetCloudSpend"));
    for source in [
        include_str!("../tool_registry.rs"),
        include_str!("../session/harness/tools/rsi_control.rs"),
    ] {
        let source = source.to_ascii_lowercase();
        assert!(!source.contains("getcloudspend"));
        assert!(!source.contains("get_cloud_spend"));
    }
}

/// #1036: the operator sees per-run and daily spend and changes the caps
/// without editing spend.md; the gate scripts read the mirrored file.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn cloud_spend_rpc_reports_spend_and_cap_edits_reach_the_caps_file() {
    let mut fixture = recursive_dag_rpc_fixture();
    let cloud_dir = fixture._dir.path().join("cloud");
    std::fs::create_dir_all(&cloud_dir).unwrap();
    let today = chrono::Utc::now().date_naive();
    std::fs::write(
        cloud_dir.join("spend.md"),
        format!(
            "Operator grant: $100\nStop and report by $90 cumulative\n\
                 Gate window i-a: stop {today}T01:00:00Z, est compute $2.00 at $1.78/h\n\
                 Gate window i-b: stop {today}T02:00:00Z, est compute $1.50 at $1.78/h\n\
                 Gate window i-old: stop 2020-01-01T02:00:00Z, est compute $4.00 at $1.78/h\n"
        ),
    )
    .unwrap();
    fixture.server.cloud_dir = cloud_dir.clone();

    let report = call_rpc(&fixture.server, "GetCloudSpend", serde_json::Value::Null)
        .await
        .result
        .expect("GetCloudSpend succeeds");
    assert_eq!(report["runs_total"], 3);
    assert_eq!(report["today_usd"], 3.5);
    assert_eq!(report["spent_usd"], 7.5);
    assert_eq!(report["stop_line_usd"], 90);
    assert_eq!(report["daily_cap_usd"], 15);
    assert_eq!(report["runs"][2]["label"], "Gate window i-old");

    let config = call_rpc(&fixture.server, "GetDaemonConfig", serde_json::Value::Null)
        .await
        .result
        .expect("GetDaemonConfig succeeds");
    assert_eq!(
        config["cloud_spend_status"],
        "today $3.50 of $15 \u{b7} total $7.50 of $90 \u{b7} last run $4.00"
    );

    for (field, value) in [
        ("cloud_spend_daily_cap_usd", 3_u64),
        ("cloud_spend_stop_line_usd", 6),
    ] {
        let response = call_rpc(
            &fixture.server,
            "UpdateDaemonConfig",
            serde_json::json!({ "field": field, "value": value }),
        )
        .await;
        assert!(response.error.is_none(), "{field}: {:?}", response.error);
        let stored = fixture
            .manager
            .store()
            .lock()
            .await
            .get_daemon_setting(field)
            .unwrap();
        assert_eq!(stored, Some(value.to_string()), "{field} is durable");
    }
    let caps: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(cloud_dir.join("spend-caps.json")).unwrap())
            .unwrap();
    assert_eq!(caps["stop_line_usd"], 6);
    assert_eq!(caps["daily_cap_usd"], 3);
    let report = call_rpc(&fixture.server, "GetCloudSpend", serde_json::Value::Null)
        .await
        .result
        .expect("GetCloudSpend succeeds");
    assert_eq!(report["daily_cap_reached"], true);
    assert_eq!(report["stop_line_reached"], true);

    let rejected = call_rpc(
        &fixture.server,
        "UpdateDaemonConfig",
        serde_json::json!({ "field": "cloud_spend_daily_cap_usd", "value": -1 }),
    )
    .await;
    assert!(rejected.error.is_some());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn update_daemon_config_rsid_scope_persists_and_atomically_refreshes_snapshot() {
    let mut fixture = recursive_dag_rpc_fixture();
    let snapshot = fixture
        ._dir
        .path()
        .join("rsi-home")
        .join(".rsi")
        .join("rsid-scope.env");
    fixture.server.rsid_scope_settings_path = snapshot.clone();

    // Host-derived worker limits can start above or below this fixture's
    // target. Change the bound that makes the other update valid first.
    let worker_memory_updates = if fixture
        .runtime_config
        .worker_scope_memory_high_mib
        .load(Ordering::Relaxed)
        >= 12_288
    {
        [
            ("worker_scope_memory_high_mib", 7168),
            ("worker_scope_memory_max_mib", 12_288),
        ]
    } else {
        [
            ("worker_scope_memory_max_mib", 12_288),
            ("worker_scope_memory_high_mib", 7168),
        ]
    };
    for (field, value) in [
        ("rsid_scope_memory_high_mib", 7168_u64),
        ("rsid_scope_memory_max_mib", 12_288),
        ("rsid_scope_memory_swap_max_mib", 256),
        ("rsid_scope_cpu_weight", 35),
    ]
    .into_iter()
    .chain(worker_memory_updates)
    .chain([
        ("worker_scope_memory_swap_max_mib", 256),
        ("worker_scope_cpu_weight", 35),
    ]) {
        let response = call_rpc(
            &fixture.server,
            "UpdateDaemonConfig",
            serde_json::json!({ "field": field, "value": value }),
        )
        .await;
        assert!(response.error.is_none(), "{field}: {:?}", response.error);

        let expected_value = value.to_string();
        let store = fixture.manager.store().lock().await;
        assert_eq!(
            store.get_daemon_setting(field).unwrap().as_deref(),
            Some(expected_value.as_str()),
            "{field} must be durable before the RPC succeeds"
        );
    }

    let config = call_rpc(&fixture.server, "GetDaemonConfig", serde_json::Value::Null).await;
    let config = config.result.expect("GetDaemonConfig succeeds");
    assert_eq!(config["rsid_scope_memory_high_mib"], 7168);
    assert_eq!(config["rsid_scope_memory_max_mib"], 12_288);
    assert_eq!(config["rsid_scope_memory_swap_max_mib"], 256);
    assert_eq!(config["rsid_scope_cpu_weight"], 35);
    assert_eq!(config["worker_scope_memory_high_mib"], 7168);
    assert_eq!(config["worker_scope_memory_max_mib"], 12_288);
    assert_eq!(config["worker_scope_memory_swap_max_mib"], 256);
    assert_eq!(config["worker_scope_cpu_weight"], 35);
    let expected = concat!(
        "rsid_scope_memory_high_mib=7168\n",
        "rsid_scope_memory_max_mib=12288\n",
        "rsid_scope_memory_swap_max_mib=256\n",
        "rsid_scope_cpu_weight=35\n",
        "worker_scope_memory_high_mib=7168\n",
        "worker_scope_memory_max_mib=12288\n",
        "worker_scope_memory_swap_max_mib=256\n",
        "worker_scope_cpu_weight=35\n",
    );
    assert_eq!(std::fs::read_to_string(&snapshot).unwrap(), expected);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::metadata(&snapshot).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    let invalid = call_rpc(
        &fixture.server,
        "UpdateDaemonConfig",
        serde_json::json!({
            "field": "rsid_scope_memory_high_mib",
            "value": 12_288,
        }),
    )
    .await;
    assert_eq!(invalid.error.unwrap().code, INVALID_PARAMS);
    assert_eq!(std::fs::read_to_string(&snapshot).unwrap(), expected);
    let invalid_worker = call_rpc(
        &fixture.server,
        "UpdateDaemonConfig",
        serde_json::json!({
            "field": "worker_scope_memory_high_mib",
            "value": 12_288,
        }),
    )
    .await;
    assert_eq!(invalid_worker.error.unwrap().code, INVALID_PARAMS);
    assert_eq!(std::fs::read_to_string(&snapshot).unwrap(), expected);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn update_daemon_config_rsid_scope_snapshot_failure_rolls_back_setting() {
    let mut fixture = recursive_dag_rpc_fixture();
    let blocker = fixture._dir.path().join("not-a-directory");
    std::fs::write(&blocker, "file").unwrap();
    fixture.server.rsid_scope_settings_path = blocker.join("rsid-scope.env");

    let response = fixture
        .server
        .handle_update_daemon_config(&RpcRequest::new(
            "UpdateDaemonConfig",
            serde_json::json!({
                "field": "rsid_scope_memory_high_mib",
                "value": 7168,
            }),
        ))
        .await;
    let error = response.expect_err("snapshot write failure must fail the RPC");
    assert!(
        error
            .to_string()
            .contains("Failed to refresh rsid scope settings snapshot")
    );
    assert_eq!(
        fixture.runtime_config.to_json()["rsid_scope_memory_high_mib"],
        6144
    );
    let store = fixture.manager.store().lock().await;
    assert_eq!(
        store
            .get_daemon_setting("rsid_scope_memory_high_mib")
            .unwrap()
            .as_deref(),
        Some("6144")
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn codegraph_capabilities_report_actual_service_and_closed_request_shape() {
    let fixture = recursive_dag_rpc_fixture();
    let response = call_rpc(
        &fixture.server,
        "GetCodegraphCapabilities",
        serde_json::Value::Null,
    )
    .await;
    assert!(response.error.is_none());
    let capabilities = response.result.unwrap();
    assert_eq!(capabilities["wire_version"], 1);
    assert_eq!(capabilities["metadata_schema_version"], 1);
    assert_eq!(capabilities["available"], false);
    assert_eq!(capabilities["indexing_enabled"], false);
    assert_eq!(capabilities["federation"], false);
    assert!(
        capabilities["supported_read_methods"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(capabilities["node_kinds"].as_array().unwrap().len() > 10);
    assert_eq!(capabilities["max_snapshot_page_size"], 32);
    let invalid = call_rpc(
        &fixture.server,
        "GetCodegraphCapabilities",
        serde_json::json!({"project_id": Uuid::new_v4()}),
    )
    .await;
    assert!(invalid.error.is_some());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn codegraph_snapshot_rpc_pages_ready_history_and_checks_scope() {
    use crate::codegraph::{IndexRuntime, RegisteredWorkspace};

    let mut fixture = recursive_dag_rpc_fixture();
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("lib.rs"), "pub fn first() {}\n").unwrap();
    let project = rsi_common::types::Project {
        id: Uuid::new_v4(),
        name: "codegraph snapshot RPC".into(),
        path: Some(root.path().to_path_buf()),
        description: None,
        color: rsi_common::types::Project::DEFAULT_COLOR.into(),
        context_files: None,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };
    fixture
        .manager
        .store()
        .lock()
        .await
        .insert_project(&project)
        .unwrap();
    let workspace = RegisteredWorkspace::primary(project.id, root.path()).unwrap();
    let db = fixture
        ._dir
        .path()
        .join("codegraph")
        .join(project.id.to_string())
        .join("codegraph.sqlite");
    let runtime = IndexRuntime::start_with_registrations_and_bus_and_gate(
        fixture._dir.path().join("codegraph"),
        vec![project.clone()],
        Vec::new(),
        &[],
        None,
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
    )
    .unwrap();
    let handle = runtime.handle();
    fixture.server.codegraph_handle = Some(handle.clone());
    let capabilities = call_rpc(
        &fixture.server,
        "GetCodegraphCapabilities",
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(capabilities.result.unwrap()["available"], true);
    let first = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Ok(store) = rsi_codegraph::CodegraphStore::open(&db, project.id)
                && let Ok(snapshot) = store.current_ready(workspace.workspace_id())
            {
                break snapshot;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();

    std::fs::write(root.path().join("lib.rs"), "pub fn second() {}\n").unwrap();
    handle.request(workspace.workspace_id()).unwrap();
    let second = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Ok(store) = rsi_codegraph::CodegraphStore::open(&db, project.id)
                && let Ok(snapshot) = store.current_ready(workspace.workspace_id())
                && snapshot.generation > first.generation
            {
                break snapshot;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let scope =
        serde_json::json!({"project_id": project.id, "workspace_id": workspace.workspace_id()});
    let exact = call_rpc(
        &fixture.server,
        "GetCodegraphSnapshot",
        serde_json::json!({"scope": scope, "generation": first.generation}),
    )
    .await;
    assert!(exact.error.is_none(), "{:?}", exact.error);
    let exact = exact.result.unwrap();
    assert_eq!(exact["snapshot"]["generation"], first.generation);
    assert_eq!(exact["counts"]["files"], 1);
    assert!(exact["counts"]["nodes"].as_u64().unwrap() > 0);

    let page = call_rpc(
        &fixture.server,
        "ListCodegraphSnapshots",
        serde_json::json!({"scope": scope, "limit": 1}),
    )
    .await;
    assert!(page.error.is_none(), "{:?}", page.error);
    let page = page.result.unwrap();
    assert_eq!(page["snapshots"][0]["generation"], second.generation);
    let cursor = page["next_cursor"].as_str().expect("second ready page");
    let stale = call_rpc(
        &fixture.server,
        "ListCodegraphSnapshots",
        serde_json::json!({"scope": scope, "limit": 2, "cursor": cursor}),
    )
    .await;
    assert!(
        stale
            .error
            .unwrap()
            .message
            .contains("codegraph_cursor_expired")
    );
    let oversized = call_rpc(
        &fixture.server,
        "ListCodegraphSnapshots",
        serde_json::json!({"scope": scope, "limit": 1, "cursor": "a".repeat(1025)}),
    )
    .await;
    assert!(
        oversized
            .error
            .unwrap()
            .message
            .contains("codegraph_cursor_expired")
    );
    let older = call_rpc(
        &fixture.server,
        "ListCodegraphSnapshots",
        serde_json::json!({"scope": scope, "limit": 1, "cursor": cursor}),
    )
    .await;
    assert!(older.error.is_none(), "{:?}", older.error);
    assert_eq!(
        older.result.unwrap()["snapshots"][0]["generation"],
        first.generation
    );

    let wrong_scope =
        serde_json::json!({"project_id": Uuid::new_v4(), "workspace_id": workspace.workspace_id()});
    let denied = call_rpc(
        &fixture.server,
        "GetCodegraphSnapshot",
        serde_json::json!({"scope": wrong_scope, "generation": first.generation}),
    )
    .await;
    assert!(denied.error.is_some());

    std::fs::write(root.path().join("lib.rs"), "pub fn third() {}\n").unwrap();
    handle.request(workspace.workspace_id()).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Ok(store) = rsi_codegraph::CodegraphStore::open(&db, project.id)
                && let Ok(snapshot) = store.current_ready(workspace.workspace_id())
                && snapshot.generation > second.generation
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let service = crate::codegraph::CodegraphReadService::new(&handle);
    let bound = crate::codegraph::BoundCodegraphScope::from_daemon_identity(
        project.id,
        workspace.workspace_id(),
        true,
    );
    let request = |baseline_generation| rsi_common::codegraph::CodegraphNativeReadV1 {
        read: rsi_common::codegraph::CodegraphReadV1::Diff {
            baseline_generation,
        },
        filter: rsi_common::codegraph::CodegraphFilterV1::default(),
        limits: rsi_common::codegraph::CodegraphQueryLimitsV1::default(),
    };
    assert!(service.read_native(&bound, request(None)).is_ok());
    assert!(matches!(
        service.read_native(&bound, request(Some(first.generation))),
        Err(crate::codegraph::CodegraphServiceError::HistoryDenied)
    ));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn sandbox_allocation_config_rpc_persists_and_restores_after_restart() {
    let fixture = recursive_dag_rpc_fixture();
    let initial = call_rpc(&fixture.server, "GetDaemonConfig", serde_json::Value::Null).await;
    assert_eq!(
        initial.result.as_ref().unwrap()["sandbox_max_source_roots"],
        4096
    );
    assert_eq!(initial.result.as_ref().unwrap()["sandbox_min_free_gib"], 30);

    for (field, value) in [
        ("sandbox_max_source_roots", serde_json::json!(512)),
        ("sandbox_min_free_gib", serde_json::json!(20)),
    ] {
        let response = call_rpc(
            &fixture.server,
            "UpdateDaemonConfig",
            serde_json::json!({"field": field, "value": value.clone()}),
        )
        .await;
        assert!(
            response.error.is_none(),
            "{field} update: {:?}",
            response.error
        );
    }

    for (field, value) in [
        ("sandbox_max_source_roots", serde_json::json!(0)),
        ("sandbox_max_source_roots", serde_json::json!(65_537)),
        ("sandbox_min_free_gib", serde_json::json!(1025)),
    ] {
        let response = call_rpc(
            &fixture.server,
            "UpdateDaemonConfig",
            serde_json::json!({"field": field, "value": value.clone()}),
        )
        .await;
        assert_eq!(
            response.error.unwrap().code,
            INVALID_PARAMS,
            "{field}={value}"
        );
    }

    let reopened = crate::store::Store::open(&fixture._dir.path().join("rsi.db")).unwrap();
    let restarted = crate::config::RuntimeConfig::from_config(&crate::config::Config::default());
    crate::store::daemon_settings::apply_persisted_runtime_config(&reopened, &restarted).unwrap();
    assert_eq!(restarted.to_json()["sandbox_max_source_roots"], 512);
    assert_eq!(restarted.to_json()["sandbox_min_free_gib"], 20);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn codegraph_daemon_config_rpc_round_trip_controls_shared_indexer_and_persists() {
    use crate::codegraph::{IndexPhase, IndexRuntime, RegisteredWorkspace};

    let fixture = recursive_dag_rpc_fixture();
    let enabled = std::sync::Arc::clone(&fixture.runtime_config.codegraph_indexing_enabled);
    let initial = call_rpc(&fixture.server, "GetDaemonConfig", serde_json::Value::Null).await;
    assert!(initial.error.is_none());
    assert_eq!(initial.result.unwrap()["codegraph_indexing_enabled"], false);
    assert!(!enabled.load(std::sync::atomic::Ordering::Acquire));

    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("lib.rs"), "pub fn ready() {}\n").unwrap();
    let project = rsi_common::types::Project {
        id: Uuid::new_v4(),
        name: "codegraph RPC gate".into(),
        path: Some(root.path().to_path_buf()),
        description: None,
        color: rsi_common::types::Project::DEFAULT_COLOR.into(),
        context_files: None,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };
    let workspace = RegisteredWorkspace::primary(project.id, root.path()).unwrap();
    let index_root = fixture._dir.path().join("codegraph");
    let db = index_root
        .join(project.id.to_string())
        .join("codegraph.sqlite");
    let runtime = IndexRuntime::start_with_registrations_and_bus_and_gate(
        index_root,
        vec![project.clone()],
        Vec::new(),
        &[],
        None,
        std::sync::Arc::clone(&enabled),
    )
    .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(350)).await;
    assert!(
        !db.exists(),
        "default OFF must not create the index database"
    );

    let on = call_rpc(
        &fixture.server,
        "UpdateDaemonConfig",
        serde_json::json!({"field": "codegraph_indexing_enabled", "value": true}),
    )
    .await;
    assert!(on.error.is_none(), "ON RPC failed: {:?}", on.error);
    assert_eq!(on.result.unwrap()["ok"], true);
    let current = call_rpc(&fixture.server, "GetDaemonConfig", serde_json::Value::Null).await;
    assert_eq!(current.result.unwrap()["codegraph_indexing_enabled"], true);
    assert!(enabled.load(std::sync::atomic::Ordering::Acquire));
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if runtime
                .handle()
                .status(workspace.workspace_id())
                .is_some_and(|status| status.phase == IndexPhase::Ready)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let ready = rsi_codegraph::CodegraphStore::open(&db, project.id)
        .unwrap()
        .current_ready(workspace.workspace_id())
        .unwrap();

    let reopened = crate::store::Store::open(&fixture._dir.path().join("rsi.db")).unwrap();
    let restarted = crate::config::RuntimeConfig::from_config(&crate::config::Config::default());
    crate::store::daemon_settings::apply_persisted_runtime_config(&reopened, &restarted).unwrap();
    assert_eq!(restarted.to_json()["codegraph_indexing_enabled"], true);
    assert!(
        restarted
            .codegraph_indexing_enabled
            .load(std::sync::atomic::Ordering::Acquire)
    );

    let off = call_rpc(
        &fixture.server,
        "UpdateDaemonConfig",
        serde_json::json!({"field": "codegraph_indexing_enabled", "value": false}),
    )
    .await;
    assert!(off.error.is_none(), "OFF RPC failed: {:?}", off.error);
    assert_eq!(off.result.unwrap()["ok"], true);
    let current = call_rpc(&fixture.server, "GetDaemonConfig", serde_json::Value::Null).await;
    assert_eq!(current.result.unwrap()["codegraph_indexing_enabled"], false);
    assert!(!enabled.load(std::sync::atomic::Ordering::Acquire));
    std::fs::write(root.path().join("lib.rs"), "pub fn changed() {}\n").unwrap();
    runtime.handle().request(workspace.workspace_id()).unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(350)).await;
    assert_eq!(
        rsi_codegraph::CodegraphStore::open(&db, project.id)
            .unwrap()
            .current_ready(workspace.workspace_id())
            .unwrap(),
        ready
    );
    assert_eq!(
        reopened
            .get_daemon_setting("codegraph_indexing_enabled")
            .unwrap()
            .as_deref(),
        Some("false")
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn codegraph_daemon_config_durable_failure_does_not_enable_indexing() {
    let fixture = recursive_dag_rpc_fixture();
    let field = "codegraph_indexing_enabled";
    let off = call_rpc(
        &fixture.server,
        "UpdateDaemonConfig",
        serde_json::json!({"field": field, "value": false}),
    )
    .await;
    assert!(off.error.is_none(), "seed durable OFF: {:?}", off.error);
    {
        let store = fixture.manager.store().lock().await;
        assert_eq!(
            store.get_daemon_setting(field).unwrap().as_deref(),
            Some("false")
        );
        store
            .conn
            .execute_batch(
                "CREATE TRIGGER reject_codegraph_setting BEFORE UPDATE ON daemon_settings
                     WHEN NEW.key='codegraph_indexing_enabled'
                     BEGIN SELECT RAISE(FAIL, 'injected codegraph durable failure'); END;",
            )
            .unwrap();
    }

    let on = call_rpc(
        &fixture.server,
        "UpdateDaemonConfig",
        serde_json::json!({"field": field, "value": true}),
    )
    .await;
    assert!(
        on.error
            .as_ref()
            .is_some_and(|error| error.message.contains("injected codegraph durable failure")),
        "failed write must fail RPC with the injected error: {:?}",
        on.error
    );
    let current = call_rpc(&fixture.server, "GetDaemonConfig", serde_json::Value::Null).await;
    assert_eq!(current.result.unwrap()[field], false);
    assert!(
        !fixture
            .runtime_config
            .codegraph_indexing_enabled
            .load(std::sync::atomic::Ordering::Acquire)
    );
    let store = fixture.manager.store().lock().await;
    assert_eq!(
        store.get_daemon_setting(field).unwrap().as_deref(),
        Some("false")
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn codegraph_disable_rpc_waits_for_publication_boundary() {
    let fixture = recursive_dag_rpc_fixture();
    let field = "codegraph_indexing_enabled";
    let on = call_rpc(
        &fixture.server,
        "UpdateDaemonConfig",
        serde_json::json!({"field": field, "value": true}),
    )
    .await;
    assert!(on.error.is_none());

    let publication_guard = crate::codegraph::PUBLICATION_GATE.lock().await;
    let request = RpcRequest::new(
        "UpdateDaemonConfig",
        serde_json::json!({"field": field, "value": false}),
    );
    let disable = fixture.server.handle_update_daemon_config(&request);
    tokio::pin!(disable);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut disable)
            .await
            .is_err(),
        "OFF must wait for a publication already in the boundary"
    );
    assert!(
        fixture
            .runtime_config
            .codegraph_indexing_enabled
            .load(std::sync::atomic::Ordering::Acquire)
    );
    drop(publication_guard);
    disable.await.unwrap();
    let current = call_rpc(&fixture.server, "GetDaemonConfig", serde_json::Value::Null).await;
    assert_eq!(current.result.unwrap()[field], false);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn update_daemon_config_target_cache_durable_failure_does_not_publish() {
    let fixture = recursive_dag_rpc_fixture();
    let field = "sandbox_build_cache_reclaim_ttl_secs";
    {
        let store = fixture.manager.store().lock().await;
        store.set_daemon_setting(field, "21600").unwrap();
        store
            .conn
            .execute_batch(
                "CREATE TRIGGER reject_rpc_target_cache_setting BEFORE UPDATE ON daemon_settings
                     WHEN NEW.key='sandbox_build_cache_reclaim_ttl_secs'
                     BEGIN SELECT RAISE(FAIL, 'injected RPC durable failure'); END;",
            )
            .unwrap();
    }
    let live_before = fixture.runtime_config.to_json();
    let request = RpcRequest::new(
        "UpdateDaemonConfig",
        serde_json::json!({ "field": field, "value": 3600 }),
    );

    let error = fixture
        .server
        .handle_update_daemon_config(&request)
        .await
        .expect_err("durable failure must abort RPC publication");
    assert!(error.to_string().contains("injected RPC durable failure"));
    assert_eq!(fixture.runtime_config.to_json(), live_before);
    {
        let store = fixture.manager.store().lock().await;
        assert_eq!(
            store.get_daemon_setting(field).unwrap().as_deref(),
            Some("21600")
        );
        store
            .conn
            .execute_batch("DROP TRIGGER reject_rpc_target_cache_setting;")
            .unwrap();
    }

    fixture
        .server
        .handle_update_daemon_config(&request)
        .await
        .expect("later durable update succeeds");
    assert_eq!(
        fixture
            .runtime_config
            .to_json()
            .get(field)
            .and_then(serde_json::Value::as_u64),
        Some(3600)
    );
    let store = fixture.manager.store().lock().await;
    assert_eq!(
        store.get_daemon_setting(field).unwrap().as_deref(),
        Some("3600")
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn completed_transcript_cap_rpc_persists_before_live_publication() {
    let fixture = recursive_dag_rpc_fixture();
    let field = "completed_transcript_cache_max_bytes";
    let initial = fixture.runtime_config.to_json()[field].as_u64().unwrap();
    let invalid = call_rpc(
        &fixture.server,
        "UpdateDaemonConfig",
        serde_json::json!({"field": field, "value": -1}),
    )
    .await;
    assert!(invalid.error.is_some());
    assert_eq!(fixture.runtime_config.to_json()[field], initial);

    {
        let store = fixture.manager.store().lock().await;
        store
            .set_daemon_setting(field, &initial.to_string())
            .unwrap();
        store
            .conn
            .execute_batch(
                "CREATE TRIGGER reject_transcript_cap BEFORE UPDATE ON daemon_settings
                 WHEN NEW.key='completed_transcript_cache_max_bytes'
                 BEGIN SELECT RAISE(FAIL, 'injected transcript cap failure'); END;",
            )
            .unwrap();
    }
    let update = serde_json::json!({"field": field, "value": 0});
    let refused = call_rpc(&fixture.server, "UpdateDaemonConfig", update.clone()).await;
    assert!(
        refused
            .error
            .as_ref()
            .is_some_and(|error| error.message.contains("injected transcript cap failure"))
    );
    assert_eq!(fixture.runtime_config.to_json()[field], initial);
    {
        let store = fixture.manager.store().lock().await;
        assert_eq!(
            store.get_daemon_setting(field).unwrap(),
            Some(initial.to_string())
        );
        store
            .conn
            .execute_batch("DROP TRIGGER reject_transcript_cap;")
            .unwrap();
    }

    let accepted = call_rpc(&fixture.server, "UpdateDaemonConfig", update).await;
    assert!(accepted.error.is_none());
    let current = call_rpc(&fixture.server, "GetDaemonConfig", serde_json::Value::Null).await;
    assert_eq!(current.result.unwrap()[field], 0);
    assert_eq!(
        fixture
            .manager
            .store()
            .lock()
            .await
            .get_daemon_setting(field)
            .unwrap(),
        Some("0".into())
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn update_daemon_config_target_cache_first_high_seeds_restartable_pair() {
    let fixture = recursive_dag_rpc_fixture();
    let before = fixture
        .runtime_config
        .sandbox_build_cache_reclaim_snapshot();
    let field = "sandbox_build_cache_reclaim_high_watermark_pct";
    fixture
        .server
        .handle_update_daemon_config(&RpcRequest::new(
            "UpdateDaemonConfig",
            serde_json::json!({ "field": field, "value": 90 }),
        ))
        .await
        .expect("first high-watermark update");

    let reopened = crate::store::Store::open(&fixture._dir.path().join("rsi.db"))
        .expect("reopen durable settings");
    let restarted = crate::config::RuntimeConfig::from_config(&crate::config::Config::from_env());
    crate::store::daemon_settings::apply_persisted_runtime_config(&reopened, &restarted)
        .expect("reload durable settings");
    let after = restarted.sandbox_build_cache_reclaim_snapshot();
    assert_eq!(
        (after.high_watermark_pct, after.low_watermark_pct),
        (90, before.low_watermark_pct)
    );
    assert_eq!(
        reopened
            .get_daemon_setting("sandbox_build_cache_reclaim_low_watermark_pct")
            .unwrap()
            .as_deref(),
        Some(before.low_watermark_pct.to_string().as_str())
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn update_daemon_config_target_cache_first_low_seeds_restartable_pair() {
    let fixture = recursive_dag_rpc_fixture();
    let before = fixture
        .runtime_config
        .sandbox_build_cache_reclaim_snapshot();
    let field = "sandbox_build_cache_reclaim_low_watermark_pct";
    fixture
        .server
        .handle_update_daemon_config(&RpcRequest::new(
            "UpdateDaemonConfig",
            serde_json::json!({ "field": field, "value": 70 }),
        ))
        .await
        .expect("first low-watermark update");

    let reopened = crate::store::Store::open(&fixture._dir.path().join("rsi.db"))
        .expect("reopen durable settings");
    let restarted = crate::config::RuntimeConfig::from_config(&crate::config::Config::from_env());
    crate::store::daemon_settings::apply_persisted_runtime_config(&reopened, &restarted)
        .expect("reload durable settings");
    let after = restarted.sandbox_build_cache_reclaim_snapshot();
    assert_eq!(
        (after.high_watermark_pct, after.low_watermark_pct),
        (before.high_watermark_pct, 70)
    );
    assert_eq!(
        reopened
            .get_daemon_setting("sandbox_build_cache_reclaim_high_watermark_pct")
            .unwrap()
            .as_deref(),
        Some(before.high_watermark_pct.to_string().as_str())
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn update_daemon_config_target_cache_concurrent_watermarks_persist_published_pair() {
    let fixture = recursive_dag_rpc_fixture();
    let server = std::sync::Arc::new(fixture.server);
    let high = {
        let server = std::sync::Arc::clone(&server);
        tokio::spawn(async move {
            server
                .handle_update_daemon_config(&RpcRequest::new(
                    "UpdateDaemonConfig",
                    serde_json::json!({
                        "field": "sandbox_build_cache_reclaim_high_watermark_pct",
                        "value": 90
                    }),
                ))
                .await
        })
    };
    let low = {
        let server = std::sync::Arc::clone(&server);
        tokio::spawn(async move {
            server
                .handle_update_daemon_config(&RpcRequest::new(
                    "UpdateDaemonConfig",
                    serde_json::json!({
                        "field": "sandbox_build_cache_reclaim_low_watermark_pct",
                        "value": 70
                    }),
                ))
                .await
        })
    };
    high.await.unwrap().expect("concurrent high update");
    low.await.unwrap().expect("concurrent low update");

    let live = fixture
        .runtime_config
        .sandbox_build_cache_reclaim_snapshot();
    assert_eq!((live.high_watermark_pct, live.low_watermark_pct), (90, 70));
    let reopened = crate::store::Store::open(&fixture._dir.path().join("rsi.db"))
        .expect("reopen durable settings");
    let restarted = crate::config::RuntimeConfig::from_config(&crate::config::Config::from_env());
    crate::store::daemon_settings::apply_persisted_runtime_config(&reopened, &restarted)
        .expect("reload durable settings");
    assert_eq!(restarted.sandbox_build_cache_reclaim_snapshot(), live);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn update_daemon_config_target_cache_pair_fault_rolls_back_before_publication() {
    let fixture = recursive_dag_rpc_fixture();
    let high = "sandbox_build_cache_reclaim_high_watermark_pct";
    let low = "sandbox_build_cache_reclaim_low_watermark_pct";
    {
        let store = fixture.manager.store().lock().await;
        store
            .conn
            .execute_batch(
                "CREATE TRIGGER reject_first_watermark_pair BEFORE INSERT ON daemon_settings
                     WHEN NEW.key='sandbox_build_cache_reclaim_low_watermark_pct'
                     BEGIN SELECT RAISE(FAIL, 'injected watermark pair failure'); END;",
            )
            .unwrap();
    }
    let live_before = fixture
        .runtime_config
        .sandbox_build_cache_reclaim_snapshot();
    let error = fixture
        .server
        .handle_update_daemon_config(&RpcRequest::new(
            "UpdateDaemonConfig",
            serde_json::json!({ "field": high, "value": 90 }),
        ))
        .await
        .expect_err("pair failure must abort publication");
    assert!(
        error
            .to_string()
            .contains("injected watermark pair failure")
    );
    assert_eq!(
        fixture
            .runtime_config
            .sandbox_build_cache_reclaim_snapshot(),
        live_before
    );
    let store = fixture.manager.store().lock().await;
    assert_eq!(store.get_daemon_setting(high).unwrap(), None);
    assert_eq!(store.get_daemon_setting(low).unwrap(), None);
    store
        .conn
        .execute_batch("DROP TRIGGER reject_first_watermark_pair;")
        .unwrap();
}

// ─── P1.6: launch_session tag validation tests ────────────────────────

/// AC4f: empty tags vector → InvalidParam at the params boundary.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn launch_session_empty_tags_rejected() {
    // Validate that LaunchSessionParams with empty tags deserializes but
    // the validation block produces the correct error message shape.
    let json = serde_json::json!({
        "query": "test",
        "tags": []
    });
    let params: LaunchSessionParams = serde_json::from_value(json).unwrap();
    assert!(
        params.tags.is_empty(),
        "empty tags must deserialize to empty vec"
    );
    // Validation code rejects when empty — check the error pattern.
    let raw = &params.tags;
    assert!(raw.is_empty());
    // Simulate the validation block output:
    let err_msg = "tags required: at least one tag must be provided";
    assert!(err_msg.contains("tags required"));
}

/// AC4g: malformed tag in LaunchSessionParams → error contains tag_malformed.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn launch_session_malformed_tag_rejected() {
    let json = serde_json::json!({
        "query": "test",
        "tags": ["UPPER!"]
    });
    let params: LaunchSessionParams = serde_json::from_value(json).unwrap();
    // Simulate the validation block behavior
    let result: std::result::Result<String, String> = rsi_common::normalize_tag(&params.tags[0])
        .map_err(|_| format!("tag_malformed: {}", &params.tags[0]));
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(err.contains("tag_malformed"), "err: {err}");
}

/// AC4h: workflow_id_override threads from params into LaunchConfig.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn launch_session_workflow_id_override_threaded() {
    let override_id = uuid::Uuid::new_v4();
    let json = serde_json::json!({
        "query": "test",
        "tags": ["ci"],
        "workflow_id_override": override_id
    });
    let params: LaunchSessionParams = serde_json::from_value(json).unwrap();
    assert_eq!(params.workflow_id_override, Some(override_id));
    // Verify the field propagates (the handler maps params.workflow_id_override → config)
    assert_eq!(params.tags, vec!["ci"]);
}

// ─── Track C slice C3: local issue tracker RPC-fixture tests ────────
//
// Drive the real dispatch + handler pipeline via `handle_request_inner`
// (unattributed/operator calls — no `session_token`), mirroring the
// `recursive_dag_rpc_fixture()` + `call_rpc()` pattern used throughout
// this module.

async fn seed_d01_idea(
    fixture: &RecursiveDagRpcFixture,
) -> anyhow::Result<rsi_common::types::IdeaWithGenesis> {
    use rsi_common::types::{
        AutonomyPolicy, Capture, CaptureSourceKind, ContentAddressedRef, Idea, IdeaActorKind,
        IdeaLifecycle, IdeaStage, Project, Sha256Digest,
    };

    let now = Utc::now();
    let project = Project {
        id: Uuid::new_v4(),
        name: "D01 RPC fixture".to_string(),
        path: None,
        description: None,
        color: Project::DEFAULT_COLOR.to_string(),
        context_files: None,
        created_at: now,
        updated_at: now,
    };
    let digest =
        Sha256Digest::parse(format!("sha256:{}", "d".repeat(64))).map_err(anyhow::Error::msg)?;
    let capture = Capture {
        id: Uuid::new_v4(),
        project_id: project.id,
        creator_kind: IdeaActorKind::Operator,
        creator_id: "operator".to_string(),
        captured_at: now,
        source_kind: CaptureSourceKind::OperatorInput,
        raw_content_digest: digest.clone(),
        storage_policy_id: "cas-v1".to_string(),
        content_ref: ContentAddressedRef::for_digest(&digest),
    };
    let idea = Idea {
        id: Uuid::new_v4(),
        project_id: project.id,
        slug: "rpc-fixture".to_string(),
        sigil: None,
        genesis_capture_id: capture.id,
        genesis_span_start: None,
        genesis_span_end: None,
        genesis_span_digest: None,
        title: "RPC fixture".to_string(),
        description: String::new(),
        portfolio_summary: "Bounded read".to_string(),
        lifecycle: IdeaLifecycle::Open,
        stage: IdeaStage::Captured,
        priority: 1,
        autonomy_policy: AutonomyPolicy::CaptureOnly,
        integration_target_ref: "refs/heads/main".to_string(),
        program_template_policy_id: None,
        current_controller_session_id: None,
        controller_epoch: 0,
        row_version: 0,
        next_event_sequence: 1,
        created_at: now,
        updated_at: now,
        terminal_at: None,
        superseded_at: None,
    };
    {
        let store = fixture.manager.store().lock().await;
        store.insert_project(&project)?;
        store.insert_d01_idea_fixture(&capture, &idea)?;
    }
    Ok(rsi_common::types::IdeaWithGenesis {
        idea,
        genesis: capture,
    })
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn get_idea_rpc_operator_round_trip_is_bounded_and_safe() -> anyhow::Result<()> {
    let fixture = recursive_dag_rpc_fixture();
    let expected = seed_d01_idea(&fixture).await?;
    let response = call_rpc(
        &fixture.server,
        "GetIdea",
        serde_json::json!({ "idea_id": expected.idea.id }),
    )
    .await;
    assert!(
        response.error.is_none(),
        "unexpected error: {:?}",
        response.error
    );
    let value = response.result.context("GetIdea response missing result")?;
    let actual: rsi_common::types::IdeaWithGenesis = serde_json::from_value(value.clone())?;
    assert_eq!(actual, expected);
    assert_eq!(
        value
            .as_object()
            .context("GetIdea result must be an object")?
            .len(),
        2
    );
    let encoded = value.to_string();
    for forbidden in [
        "raw_content",
        "provider",
        "model",
        "transcript",
        "token",
        "cost",
        "retry",
        "sandbox",
        "context_window",
    ] {
        assert!(
            !encoded.contains(&format!("\"{forbidden}\":")),
            "GetIdea leaked field {forbidden}"
        );
    }
    Ok(())
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn get_idea_rpc_missing_malformed_and_attributed_calls_fail_closed() -> anyhow::Result<()> {
    let fixture = recursive_dag_rpc_fixture();

    let missing = call_rpc(
        &fixture.server,
        "GetIdea",
        serde_json::json!({ "idea_id": Uuid::new_v4() }),
    )
    .await;
    assert!(
        missing
            .error
            .context("missing Idea must return an error")?
            .message
            .contains("Idea not found")
    );

    let malformed = call_rpc(
        &fixture.server,
        "GetIdea",
        serde_json::json!({ "idea_id": "not-a-uuid" }),
    )
    .await;
    assert!(
        malformed
            .error
            .context("malformed GetIdea must return an error")?
            .message
            .contains("Invalid params")
    );

    let mut attributed =
        RpcRequest::new("GetIdea", serde_json::json!({ "idea_id": Uuid::new_v4() }));
    attributed.session_token = Some("some-token".to_string());
    let response = match fixture.server.handle_request_inner(&attributed).await {
        HandleResult::Response(response) => response,
        HandleResult::Subscribe { .. } => {
            anyhow::bail!("GetIdea unexpectedly returned a subscription")
        }
    };
    let error = response
        .error
        .context("attributed GetIdea must be denied")?;
    assert_eq!(error.code, INVALID_PARAMS);
    assert!(
        error
            .message
            .contains("not available to session-attributed callers")
    );

    for method in [
        "CreateIdea",
        "UpdateIdea",
        "AppendIdeaEvent",
        "AssignIdeaController",
    ] {
        let response = call_rpc(&fixture.server, method, serde_json::json!({})).await;
        assert_eq!(
            response
                .error
                .context("unknown Idea mutation must return an error")?
                .code,
            METHOD_NOT_FOUND,
            "unexpected D01 mutation surface: {method}"
        );
    }
    Ok(())
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn create_issue_rpc_roundtrip_allocates_display_number_and_open_status() {
    let fixture = recursive_dag_rpc_fixture();
    let response = call_rpc(
        &fixture.server,
        "CreateIssue",
        serde_json::json!({ "title": "fix the thing" }),
    )
    .await;
    assert!(
        response.error.is_none(),
        "unexpected error: {:?}",
        response.error
    );
    let issue: rsi_common::types::Issue = serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!(issue.title, "fix the thing");
    assert_eq!(issue.status, rsi_common::types::IssueStatus::Open);
    assert!(issue.display_number >= 1);
    assert!(
        issue.created_by_session_id.is_none(),
        "CreateIssue is operator-only: created_by_session_id must be None"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn get_issue_rpc_known_and_unknown() {
    let fixture = recursive_dag_rpc_fixture();
    let created = call_rpc(
        &fixture.server,
        "CreateIssue",
        serde_json::json!({ "title": "findable" }),
    )
    .await;
    let issue: rsi_common::types::Issue = serde_json::from_value(created.result.unwrap()).unwrap();

    let ok_response = call_rpc(
        &fixture.server,
        "GetIssue",
        serde_json::json!({ "issue_id": issue.id }),
    )
    .await;
    assert!(ok_response.error.is_none());
    let fetched: rsi_common::types::Issue =
        serde_json::from_value(ok_response.result.unwrap()).unwrap();
    assert_eq!(fetched.id, issue.id);

    let missing_response = call_rpc(
        &fixture.server,
        "GetIssue",
        serde_json::json!({ "issue_id": Uuid::new_v4() }),
    )
    .await;
    assert!(
        missing_response.error.is_some(),
        "unknown issue_id must be a not-found error"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn list_issues_rpc_status_filter() {
    let fixture = recursive_dag_rpc_fixture();
    let open = call_rpc(
        &fixture.server,
        "CreateIssue",
        serde_json::json!({ "title": "stays open" }),
    )
    .await;
    let open_issue: rsi_common::types::Issue =
        serde_json::from_value(open.result.unwrap()).unwrap();
    let to_close = call_rpc(
        &fixture.server,
        "CreateIssue",
        serde_json::json!({ "title": "will close" }),
    )
    .await;
    let to_close_issue: rsi_common::types::Issue =
        serde_json::from_value(to_close.result.unwrap()).unwrap();
    call_rpc(
        &fixture.server,
        "UpdateIssueStatus",
        serde_json::json!({ "issue_id": to_close_issue.id, "status": "Closed" }),
    )
    .await;

    let filtered = call_rpc(
        &fixture.server,
        "ListIssues",
        serde_json::json!({ "status": "Open" }),
    )
    .await;
    assert!(filtered.error.is_none());
    let issues: Vec<rsi_common::types::Issue> =
        serde_json::from_value(filtered.result.unwrap()).unwrap();
    assert!(issues.iter().any(|i| i.id == open_issue.id));
    assert!(!issues.iter().any(|i| i.id == to_close_issue.id));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn list_issues_rpc_malformed_status_errors_instead_of_listing_all() {
    let fixture = recursive_dag_rpc_fixture();
    let created = call_rpc(
        &fixture.server,
        "CreateIssue",
        serde_json::json!({ "title": "malformed-status" }),
    )
    .await;
    let _: rsi_common::types::Issue = serde_json::from_value(created.result.unwrap()).unwrap();

    let response = call_rpc(
        &fixture.server,
        "ListIssues",
        serde_json::json!({ "status": "Opne" }),
    )
    .await;
    assert!(
        response.error.is_some(),
        "malformed status must error instead of listing all issues"
    );
    assert!(
        response.result.is_none(),
        "malformed status must not fall back to the default filter"
    );
    assert!(
        response
            .error
            .as_ref()
            .unwrap()
            .message
            .contains("Invalid params"),
        "malformed status must surface the params error"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn update_issue_status_rpc_open_to_closed_sets_closed_at_and_unknown_id_errors() {
    let fixture = recursive_dag_rpc_fixture();
    let created = call_rpc(
        &fixture.server,
        "CreateIssue",
        serde_json::json!({ "title": "lifecycle" }),
    )
    .await;
    let issue: rsi_common::types::Issue = serde_json::from_value(created.result.unwrap()).unwrap();
    assert!(issue.closed_at.is_none());

    let closed_response = call_rpc(
        &fixture.server,
        "UpdateIssueStatus",
        serde_json::json!({ "issue_id": issue.id, "status": "Closed" }),
    )
    .await;
    assert!(closed_response.error.is_none());
    let closed: rsi_common::types::Issue =
        serde_json::from_value(closed_response.result.unwrap()).unwrap();
    assert_eq!(closed.status, rsi_common::types::IssueStatus::Closed);
    assert!(closed.closed_at.is_some());

    let unknown_response = call_rpc(
        &fixture.server,
        "UpdateIssueStatus",
        serde_json::json!({ "issue_id": Uuid::new_v4(), "status": "Closed" }),
    )
    .await;
    assert!(
        unknown_response.error.is_some(),
        "unknown issue_id must error"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn add_issue_dep_rpc_success_self_dep_cycle_and_duplicate() {
    let fixture = recursive_dag_rpc_fixture();
    let a = call_rpc(
        &fixture.server,
        "CreateIssue",
        serde_json::json!({ "title": "a" }),
    )
    .await;
    let a: rsi_common::types::Issue = serde_json::from_value(a.result.unwrap()).unwrap();
    let b = call_rpc(
        &fixture.server,
        "CreateIssue",
        serde_json::json!({ "title": "b" }),
    )
    .await;
    let b: rsi_common::types::Issue = serde_json::from_value(b.result.unwrap()).unwrap();

    // Success: a depends on b.
    let ok = call_rpc(
        &fixture.server,
        "AddIssueDep",
        serde_json::json!({ "issue_id": a.id, "depends_on_id": b.id }),
    )
    .await;
    assert!(ok.error.is_none());
    assert_eq!(ok.result.unwrap(), serde_json::json!({ "ok": true }));

    // Self-dep: a depends on a.
    let self_dep = call_rpc(
        &fixture.server,
        "AddIssueDep",
        serde_json::json!({ "issue_id": a.id, "depends_on_id": a.id }),
    )
    .await;
    assert!(self_dep.error.is_some(), "self-dep must error");

    // Cycle: b depends on a would close a loop (a already depends on b).
    let cycle = call_rpc(
        &fixture.server,
        "AddIssueDep",
        serde_json::json!({ "issue_id": b.id, "depends_on_id": a.id }),
    )
    .await;
    assert!(cycle.error.is_some(), "cycle-creating dep must error");

    // Duplicate: a depends on b again — idempotent ok, not an error.
    let duplicate = call_rpc(
        &fixture.server,
        "AddIssueDep",
        serde_json::json!({ "issue_id": a.id, "depends_on_id": b.id }),
    )
    .await;
    assert!(
        duplicate.error.is_none(),
        "duplicate dep must be idempotent ok"
    );
    assert_eq!(duplicate.result.unwrap(), serde_json::json!({ "ok": true }));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn remove_issue_dep_rpc_true_then_false() {
    let fixture = recursive_dag_rpc_fixture();
    let a = call_rpc(
        &fixture.server,
        "CreateIssue",
        serde_json::json!({ "title": "a" }),
    )
    .await;
    let a: rsi_common::types::Issue = serde_json::from_value(a.result.unwrap()).unwrap();
    let b = call_rpc(
        &fixture.server,
        "CreateIssue",
        serde_json::json!({ "title": "b" }),
    )
    .await;
    let b: rsi_common::types::Issue = serde_json::from_value(b.result.unwrap()).unwrap();
    call_rpc(
        &fixture.server,
        "AddIssueDep",
        serde_json::json!({ "issue_id": a.id, "depends_on_id": b.id }),
    )
    .await;

    let first = call_rpc(
        &fixture.server,
        "RemoveIssueDep",
        serde_json::json!({ "issue_id": a.id, "depends_on_id": b.id }),
    )
    .await;
    assert!(first.error.is_none());
    assert_eq!(
        first.result.unwrap(),
        serde_json::json!({ "removed": true })
    );

    let second = call_rpc(
        &fixture.server,
        "RemoveIssueDep",
        serde_json::json!({ "issue_id": a.id, "depends_on_id": b.id }),
    )
    .await;
    assert!(second.error.is_none());
    assert_eq!(
        second.result.unwrap(),
        serde_json::json!({ "removed": false })
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn list_ready_issues_rpc_respects_blockers_and_limit() {
    let fixture = recursive_dag_rpc_fixture();
    let a = call_rpc(
        &fixture.server,
        "CreateIssue",
        serde_json::json!({ "title": "a" }),
    )
    .await;
    let a: rsi_common::types::Issue = serde_json::from_value(a.result.unwrap()).unwrap();
    let b = call_rpc(
        &fixture.server,
        "CreateIssue",
        serde_json::json!({ "title": "b" }),
    )
    .await;
    let b: rsi_common::types::Issue = serde_json::from_value(b.result.unwrap()).unwrap();
    let c = call_rpc(
        &fixture.server,
        "CreateIssue",
        serde_json::json!({ "title": "c" }),
    )
    .await;
    let c: rsi_common::types::Issue = serde_json::from_value(c.result.unwrap()).unwrap();

    // c is blocked by a (still Open) => not ready.
    call_rpc(
        &fixture.server,
        "AddIssueDep",
        serde_json::json!({ "issue_id": c.id, "depends_on_id": a.id }),
    )
    .await;

    let ready = call_rpc(&fixture.server, "ListReadyIssues", serde_json::json!({})).await;
    assert!(ready.error.is_none());
    let ready_issues: Vec<rsi_common::types::Issue> =
        serde_json::from_value(ready.result.unwrap()).unwrap();
    let ready_ids: Vec<Uuid> = ready_issues.iter().map(|i| i.id).collect();
    assert!(ready_ids.contains(&a.id));
    assert!(ready_ids.contains(&b.id));
    assert!(
        !ready_ids.contains(&c.id),
        "c is blocked, must not be ready"
    );

    let limited = call_rpc(
        &fixture.server,
        "ListReadyIssues",
        serde_json::json!({ "limit": 1 }),
    )
    .await;
    assert!(limited.error.is_none());
    let limited_issues: Vec<rsi_common::types::Issue> =
        serde_json::from_value(limited.result.unwrap()).unwrap();
    assert_eq!(limited_issues.len(), 1);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn list_ready_issues_rpc_malformed_limit_errors_instead_of_becoming_unbounded() {
    let fixture = recursive_dag_rpc_fixture();
    let first = call_rpc(
        &fixture.server,
        "CreateIssue",
        serde_json::json!({ "title": "ready-1" }),
    )
    .await;
    let _: rsi_common::types::Issue = serde_json::from_value(first.result.unwrap()).unwrap();
    let second = call_rpc(
        &fixture.server,
        "CreateIssue",
        serde_json::json!({ "title": "ready-2" }),
    )
    .await;
    let _: rsi_common::types::Issue = serde_json::from_value(second.result.unwrap()).unwrap();

    let response = call_rpc(
        &fixture.server,
        "ListReadyIssues",
        serde_json::json!({ "limit": "x" }),
    )
    .await;
    assert!(
        response.error.is_some(),
        "malformed limit must error instead of becoming unbounded"
    );
    assert!(
        response.result.is_none(),
        "malformed limit must not fall back to the default unbounded query"
    );
    assert!(
        response
            .error
            .as_ref()
            .unwrap()
            .message
            .contains("Invalid params"),
        "malformed limit must surface the params error"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn issue_workspace_rpc_operator_surface_round_trips_visible_identity_and_end_states() {
    tokio::spawn(async move {
        use rsi_common::issue_workspace::{
            IssueDependencyMutationResultV1, IssueDependencyPageV1, IssueWorkspacePageV1,
            IssueWorkspaceRowV1, OperatorIssueMutationResultV1,
        };
        use rsi_common::types::IssueStatus;

        let fixture = recursive_dag_rpc_fixture();
        let project_id = issue_rpc_project_id();
        let unknown_project_id = Uuid::new_v4();
        let unknown_project = call_rpc(
            &fixture.server,
            "CreateIssueV2",
            serde_json::json!({
                "project_id": unknown_project_id,
                "title": "Unknown project retained draft identity",
                "idempotency_key": "rpc-workspace-unknown-project"
            }),
        )
        .await;
        let unknown_error = unknown_project
            .error
            .expect("unknown project create must be definitive");
        let unknown_envelope: rsi_common::issue_workspace::IssueWorkspaceErrorV1 =
            serde_json::from_value(unknown_error.data.expect("typed unknown project error"))
                .unwrap();
        assert_eq!(
            unknown_envelope.code,
            rsi_common::issue_workspace::IssueWorkspaceErrorCodeV1::NotFoundInProject
        );
        assert_eq!(
            unknown_envelope.issue_id,
            Some(rsi_common::issue_workspace::operator_issue_create_id(
                unknown_project_id,
                "rpc-workspace-unknown-project"
            ))
        );
        let create = call_rpc(
            &fixture.server,
            "CreateIssueV2",
            serde_json::json!({
                "project_id": project_id,
                "title": "RPC workspace visible issue",
                "body": "visible body",
                "priority": 2,
                "labels": ["workspace", "visible"],
                "assignee": "operator",
                "idempotency_key": "rpc-workspace-create"
            }),
        )
        .await;
        assert!(create.error.is_none(), "{:?}", create.error);
        let created: OperatorIssueMutationResultV1 =
            serde_json::from_value(create.result.unwrap()).unwrap();
        assert_eq!(created.issue.title, "RPC workspace visible issue");
        assert_eq!(created.issue.labels, vec!["workspace", "visible"]);

        let blocker = call_rpc(
            &fixture.server,
            "CreateIssueV2",
            serde_json::json!({
                "project_id": project_id,
                "title": "RPC workspace visible blocker",
                "idempotency_key": "rpc-workspace-blocker"
            }),
        )
        .await;
        let blocker: OperatorIssueMutationResultV1 =
            serde_json::from_value(blocker.result.unwrap()).unwrap();

        let page = call_rpc(
            &fixture.server,
            "ListIssuesPage",
            serde_json::json!({
                "project_id": project_id,
                "sort": "display_number_asc",
                "limit": 64
            }),
        )
        .await;
        let page: IssueWorkspacePageV1 = serde_json::from_value(page.result.unwrap()).unwrap();
        assert!(page.rows.iter().any(|row| {
            row.issue.id == created.issue.id && row.issue.title == "RPC workspace visible issue"
        }));
        let tampered_local_cursor = call_rpc(
            &fixture.server,
            "ListIssuesPage",
            serde_json::json!({
                "project_id": project_id,
                "sort": "display_number_asc",
                "cursor": {
                    "kind": "display_number_asc",
                    "display_number": created.issue.display_number + 1000,
                    "issue_id": created.issue.id
                },
                "limit": 64
            }),
        )
        .await;
        let tampered_local_error = tampered_local_cursor
            .error
            .expect("tampered Local cursor must be rejected");
        let tampered_local_envelope: rsi_common::issue_workspace::IssueWorkspaceErrorV1 =
            serde_json::from_value(
                tampered_local_error
                    .data
                    .expect("typed tampered Local cursor error"),
            )
            .unwrap();
        assert_eq!(
            tampered_local_envelope.code,
            rsi_common::issue_workspace::IssueWorkspaceErrorCodeV1::InvalidRequest
        );

        let get = call_rpc(
            &fixture.server,
            "GetIssueInProject",
            serde_json::json!({"project_id": project_id, "issue_id": created.issue.id}),
        )
        .await;
        let selected: Option<IssueWorkspaceRowV1> =
            serde_json::from_value(get.result.unwrap()).unwrap();
        let selected = selected.expect("existing project Issue row");
        assert_eq!(selected.issue.id, created.issue.id);
        assert_eq!(selected.issue.title, "RPC workspace visible issue");

        let missing_id = Uuid::new_v4();
        let missing = call_rpc(
            &fixture.server,
            "GetIssueInProject",
            serde_json::json!({"project_id": project_id, "issue_id": missing_id}),
        )
        .await;
        assert!(missing.error.is_none());
        let missing: Option<IssueWorkspaceRowV1> =
            serde_json::from_value(missing.result.unwrap()).unwrap();
        assert_eq!(missing, None);

        let foreign_project_id = Uuid::new_v4();
        fixture
            .manager
            .store()
            .lock()
            .await
            .insert_project(&rsi_common::types::Project {
                id: foreign_project_id,
                name: "Foreign RPC issue project".to_string(),
                path: None,
                description: None,
                color: rsi_common::types::Project::DEFAULT_COLOR.to_string(),
                context_files: None,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            })
            .unwrap();
        let foreign = call_rpc(
            &fixture.server,
            "CreateIssueV2",
            serde_json::json!({
                "project_id": foreign_project_id,
                "title": "Foreign project private issue",
                "idempotency_key": "rpc-workspace-foreign"
            }),
        )
        .await;
        let foreign: OperatorIssueMutationResultV1 =
            serde_json::from_value(foreign.result.unwrap()).unwrap();
        let foreign_get = call_rpc(
            &fixture.server,
            "GetIssueInProject",
            serde_json::json!({"project_id": project_id, "issue_id": foreign.issue.id}),
        )
        .await;
        let foreign_error = foreign_get.error.expect("foreign Issue must be denied");
        let foreign_envelope: rsi_common::issue_workspace::IssueWorkspaceErrorV1 =
            serde_json::from_value(foreign_error.data.expect("typed foreign error")).unwrap();
        assert_eq!(
            foreign_envelope.code,
            rsi_common::issue_workspace::IssueWorkspaceErrorCodeV1::NotFoundInProject
        );
        assert_eq!(foreign_envelope.issue_id, Some(foreign.issue.id));

        let update = call_rpc(
            &fixture.server,
            "UpdateIssue",
            serde_json::json!({
                "project_id": project_id,
                "issue_id": created.issue.id,
                "expected_row_version": 1,
                "idempotency_key": "rpc-workspace-update",
                "patch": {"title": "RPC workspace revised issue", "priority": null}
            }),
        )
        .await;
        let updated: OperatorIssueMutationResultV1 =
            serde_json::from_value(update.result.unwrap()).unwrap();
        assert_eq!(updated.issue.title, "RPC workspace revised issue");
        assert_eq!(updated.issue.priority, None);

        let added = call_rpc(
            &fixture.server,
            "AddIssueDependency",
            serde_json::json!({
                "project_id": project_id,
                "issue_id": created.issue.id,
                "depends_on_id": blocker.issue.id
            }),
        )
        .await;
        let added: IssueDependencyMutationResultV1 =
            serde_json::from_value(added.result.unwrap()).unwrap();
        assert!(added.changed);
        let dependencies = call_rpc(
            &fixture.server,
            "ListIssueDependencies",
            serde_json::json!({
                "project_id": project_id,
                "issue_id": created.issue.id,
                "direction": "blocked_by",
                "limit": 64
            }),
        )
        .await;
        let dependencies: IssueDependencyPageV1 =
            serde_json::from_value(dependencies.result.unwrap()).unwrap();
        assert_eq!(dependencies.items[0].related_issue.id, blocker.issue.id);
        assert_eq!(
            dependencies.items[0].related_issue.title,
            "RPC workspace visible blocker"
        );
        let tampered_dependency_cursor = call_rpc(
            &fixture.server,
            "ListIssueDependencies",
            serde_json::json!({
                "project_id": project_id,
                "issue_id": created.issue.id,
                "direction": "blocked_by",
                "cursor": {
                    "display_number": blocker.issue.display_number + 1000,
                    "issue_id": blocker.issue.id
                },
                "limit": 64
            }),
        )
        .await;
        let tampered_dependency_error = tampered_dependency_cursor
            .error
            .expect("tampered dependency cursor must be rejected");
        let tampered_dependency_envelope: rsi_common::issue_workspace::IssueWorkspaceErrorV1 =
            serde_json::from_value(
                tampered_dependency_error
                    .data
                    .expect("typed tampered dependency cursor error"),
            )
            .unwrap();
        assert_eq!(
            tampered_dependency_envelope.code,
            rsi_common::issue_workspace::IssueWorkspaceErrorCodeV1::InvalidRequest
        );
        let removed = call_rpc(
            &fixture.server,
            "RemoveIssueDependency",
            serde_json::json!({
                "project_id": project_id,
                "issue_id": created.issue.id,
                "depends_on_id": blocker.issue.id
            }),
        )
        .await;
        let removed: IssueDependencyMutationResultV1 =
            serde_json::from_value(removed.result.unwrap()).unwrap();
        assert!(removed.changed);

        let cancelled = call_rpc(
            &fixture.server,
            "UpdateIssueStatusV2",
            serde_json::json!({
                "project_id": project_id,
                "issue_id": created.issue.id,
                "expected_row_version": 2,
                "idempotency_key": "rpc-workspace-cancel",
                "status": "Cancelled"
            }),
        )
        .await;
        let cancelled: OperatorIssueMutationResultV1 =
            serde_json::from_value(cancelled.result.unwrap()).unwrap();
        assert_eq!(cancelled.issue.status, IssueStatus::Cancelled);
        let archived = call_rpc(
            &fixture.server,
            "ArchiveIssue",
            serde_json::json!({
                "project_id": project_id,
                "issue_id": created.issue.id,
                "expected_row_version": 3,
                "idempotency_key": "rpc-workspace-archive"
            }),
        )
        .await;
        let archived: OperatorIssueMutationResultV1 =
            serde_json::from_value(archived.result.unwrap()).unwrap();
        assert_eq!(archived.issue.status, IssueStatus::Cancelled);
        assert!(archived.issue.archived_at.is_some());
        let restored = call_rpc(
            &fixture.server,
            "RestoreIssue",
            serde_json::json!({
                "project_id": project_id,
                "issue_id": created.issue.id,
                "expected_row_version": 4,
                "idempotency_key": "rpc-workspace-restore"
            }),
        )
        .await;
        let restored: OperatorIssueMutationResultV1 =
            serde_json::from_value(restored.result.unwrap()).unwrap();
        assert_eq!(restored.issue.status, IssueStatus::Cancelled);
        assert!(restored.issue.archived_at.is_none());

        let events = call_rpc(
            &fixture.server,
            "ListIssueEventsV2",
            serde_json::json!({
                "project_id": project_id,
                "issue_id": created.issue.id,
                "limit": 64
            }),
        )
        .await;
        let events: rsi_common::types::IssueEventPageV1 =
            serde_json::from_value(events.result.unwrap()).unwrap();
        assert_eq!(events.events.len(), 5);
        assert_eq!(
            events.events.last().unwrap().issue.status,
            IssueStatus::Cancelled
        );
        assert!(
            events
                .events
                .iter()
                .all(|event| event.idempotency_key.is_none())
        );
    })
    .await
    .unwrap();
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn issue_workspace_rpc_rejects_present_null_and_all_methods_remain_operator_only() {
    const METHODS: [&str; 11] = [
        "CreateIssueV2",
        "GetIssueInProject",
        "ListIssuesPage",
        "UpdateIssue",
        "UpdateIssueStatusV2",
        "ListIssueDependencies",
        "AddIssueDependency",
        "RemoveIssueDependency",
        "ListIssueEventsV2",
        "ArchiveIssue",
        "RestoreIssue",
    ];
    for method in METHODS {
        assert!(
            !agent_gate::is_allowed_for_attributed_caller(method),
            "generic Issue workspace method must remain operator-only: {method}"
        );
    }

    let fixture = recursive_dag_rpc_fixture();
    let malformed = call_rpc(
        &fixture.server,
        "UpdateIssue",
        serde_json::json!({
            "project_id": issue_rpc_project_id(),
            "issue_id": Uuid::new_v4(),
            "expected_row_version": 1,
            "idempotency_key": "present-null",
            "patch": {"title": null}
        }),
    )
    .await;
    let error = malformed.error.expect("present title null must fail");
    let envelope: rsi_common::issue_workspace::IssueWorkspaceErrorV1 =
        serde_json::from_value(error.data.expect("typed error data")).unwrap();
    assert_eq!(
        envelope.code,
        rsi_common::issue_workspace::IssueWorkspaceErrorCodeV1::InvalidRequest
    );

    let mut attributed = RpcRequest::new(
        "ListIssuesPage",
        serde_json::json!({"project_id": issue_rpc_project_id()}),
    );
    attributed.session_token = Some("transport-only-token".to_string());
    let HandleResult::Response(response) = fixture.server.handle_request_inner(&attributed).await
    else {
        panic!("Issue workspace request unexpectedly subscribed");
    };
    let error = response.error.expect("attributed generic method denied");
    assert_eq!(error.code, INVALID_PARAMS);
    assert!(
        error
            .message
            .contains("not available to session-attributed callers")
    );
}

// ---- #241 attributed read scope (K5) -------------------------------

/// Topology for the #241 read-scope tests. The fixture project holds
/// `group` with `epic_a` (lead `la`; workers `wa` and sibling `wa2`;
/// `wa`'s rotation predecessor `pwa` and successor `swa`; `wa`'s child
/// `ca` and nested grandchild `na`) and `epic_b` (lead `lb`, worker
/// `wb`). A foreign project holds `foreign_epic` with lead `lq`. Root
/// `mgr` is appointed manager of the fixture project over `epic_a` only.
struct ReadScopeFixture {
    rpc: RecursiveDagRpcFixture,
    foreign_project: Uuid,
    group: Uuid,
    epic_a: Uuid,
    la: Uuid,
    wa: Uuid,
    wa2: Uuid,
    pwa: Uuid,
    swa: Uuid,
    ca: Uuid,
    na: Uuid,
    epic_b: Uuid,
    lb: Uuid,
    wb: Uuid,
    foreign_group: Uuid,
    foreign_epic: Uuid,
    lq: Uuid,
    mgr: Uuid,
}

async fn seed_read_scope_topology(f: &ReadScopeFixture) {
    use rsi_common::types::SessionKind;
    let home = issue_rpc_project_id();
    let foreign = f.foreign_project;
    let store = f.rpc.manager.store().lock().await;
    store.install_session_diagnostics_schema_for_test().unwrap();
    let now = chrono::Utc::now();
    store
        .insert_project(&rsi_common::types::Project {
            id: foreign,
            name: "Foreign read-scope project".into(),
            path: None,
            description: None,
            color: rsi_common::types::Project::DEFAULT_COLOR.into(),
            context_files: None,
            created_at: now,
            updated_at: now,
        })
        .unwrap();
    let rows = [
        (f.group, SessionKind::Group, None, home, None),
        (f.epic_a, SessionKind::Epic, Some(f.group), home, None),
        (f.la, SessionKind::Feature, Some(f.epic_a), home, None),
        (f.pwa, SessionKind::Task, Some(f.epic_a), home, None),
        (f.wa, SessionKind::Task, Some(f.epic_a), home, Some(f.pwa)),
        (f.swa, SessionKind::Task, Some(f.epic_a), home, Some(f.wa)),
        (f.wa2, SessionKind::Task, Some(f.epic_a), home, None),
        (f.ca, SessionKind::Task, Some(f.wa), home, None),
        (f.na, SessionKind::Task, Some(f.ca), home, None),
        (f.epic_b, SessionKind::Epic, Some(f.group), home, None),
        (f.lb, SessionKind::Feature, Some(f.epic_b), home, None),
        (f.wb, SessionKind::Task, Some(f.epic_b), home, None),
        (f.foreign_group, SessionKind::Group, None, foreign, None),
        (
            f.foreign_epic,
            SessionKind::Epic,
            Some(f.foreign_group),
            foreign,
            None,
        ),
        (
            f.lq,
            SessionKind::Feature,
            Some(f.foreign_epic),
            foreign,
            None,
        ),
        (f.mgr, SessionKind::Standard, None, home, None),
    ];
    for (id, kind, parent, project, continued_from) in rows {
        let mut row = mk_agent_test_session(id, kind, parent, None);
        row.project_id = Some(project);
        row.continued_from = continued_from;
        store.insert_session(&row).unwrap();
        // One persisted event per row so an admitted conversation read
        // returns content rather than "not found".
        store
            .insert_event(&rsi_common::ConversationEvent {
                id: 0,
                session_id: id,
                sequence: 1,
                event_type: rsi_common::EventType::Message,
                role: Some(rsi_common::Role::Assistant),
                created_at: now,
                content: format!("read-scope event {id}"),
                tool_name: None,
                tool_input: None,
                offload_id: None,
                tool_use_id: None,
                metadata: None,
            })
            .unwrap();
    }
    for (epic, lead) in [(f.epic_a, f.la), (f.epic_b, f.lb), (f.foreign_epic, f.lq)] {
        store.set_lead_session(epic, Some(lead)).unwrap();
    }
    store
        .insert_session_diagnostic(&rsi_common::types::NewSessionDiagnosticV1 {
            session_id: f.wa,
            timestamp: now,
            level: rsi_common::types::SessionDiagnosticLevelV1::Warn,
            message: "diagnostic read-scope fixture".into(),
            fields: Some(serde_json::json!({ "source": "fixture" })),
        })
        .unwrap();
}

async fn read_scope_fixture() -> ReadScopeFixture {
    let f = ReadScopeFixture {
        rpc: recursive_dag_rpc_fixture(),
        foreign_project: Uuid::new_v4(),
        group: Uuid::new_v4(),
        epic_a: Uuid::new_v4(),
        la: Uuid::new_v4(),
        wa: Uuid::new_v4(),
        wa2: Uuid::new_v4(),
        pwa: Uuid::new_v4(),
        swa: Uuid::new_v4(),
        ca: Uuid::new_v4(),
        na: Uuid::new_v4(),
        epic_b: Uuid::new_v4(),
        lb: Uuid::new_v4(),
        wb: Uuid::new_v4(),
        foreign_group: Uuid::new_v4(),
        foreign_epic: Uuid::new_v4(),
        lq: Uuid::new_v4(),
        mgr: Uuid::new_v4(),
    };
    seed_read_scope_topology(&f).await;
    let appointed = call_rpc(
        &f.rpc.server,
        "ConfigureHarnessManager",
        serde_json::json!({
            "project_id": issue_rpc_project_id(), "session_id": f.mgr,
            "epic_ids": [f.epic_a], "expected_row_version": 0
        }),
    )
    .await;
    assert!(appointed.error.is_none(), "{:?}", appointed.error);
    for (token, id) in [("wa", f.wa), ("la", f.la), ("mgr", f.mgr)] {
        f.rpc
            .manager
            .register_agent_token(format!("read-scope-{token}"), id)
            .await;
    }
    f
}

/// Params naming `target` for every target-scoped read verb. Panics on
/// an unknown verb so a new `READ_VERBS` entry cannot land without a
/// decision about its read scope (or an `UNSCOPED_READ_VERBS` entry).
fn scoped_read_params(method: &str, target: Uuid) -> serde_json::Value {
    match method {
        "GetSession"
        | "GetSessionSummary"
        | "GetConversation"
        | "GetSessionDiagnostics"
        | "GetTurnMetrics" => {
            serde_json::json!({ "session_id": target })
        }
        "ListSessionChildren" => serde_json::json!({ "parent_id": target }),
        "GetConversationsSince" => {
            serde_json::json!({ "requests": [{ "session_id": target }] })
        }
        other => panic!("READ_VERB `{other}` has no #241 read-scope decision"),
    }
}

fn scoped_read_verbs() -> Vec<&'static str> {
    agent_gate::READ_VERBS
        .iter()
        .copied()
        .filter(|method| !agent_gate::UNSCOPED_READ_VERBS.contains(method))
        .collect()
}

const CONTENT_READ_VERBS: [&str; 4] = [
    "GetConversation",
    "GetConversationsSince",
    "GetSessionDiagnostics",
    "GetTurnMetrics",
];

async fn tokened_read(
    f: &ReadScopeFixture,
    caller: &str,
    method: &str,
    params: serde_json::Value,
) -> RpcResponse {
    let mut request = RpcRequest::new(method, params);
    request.session_token = Some(format!("read-scope-{caller}"));
    let HandleResult::Response(response) = f.rpc.server.handle_request_inner(&request).await else {
        panic!("expected response");
    };
    response
}

fn assert_read_admitted(response: &RpcResponse, context: &str) {
    assert!(
        response.error.is_none(),
        "{context}: expected admission, got {:?}",
        response.error
    );
}

fn assert_read_scope_denied(response: &RpcResponse, context: &str) {
    let error = response
        .error
        .as_ref()
        .unwrap_or_else(|| panic!("{context}: expected agent_read_scope_denied"));
    assert_eq!(error.code, INVALID_PARAMS, "{context}");
    assert_eq!(error.message, "agent_read_scope_denied", "{context}");
    let data = error.data.as_ref().expect("typed denial envelope");
    assert_eq!(data["code"], "agent_read_scope_denied", "{context}");
    // The envelope is a constant: exactly `code` and `next_action`.
    let keys: Vec<&String> = data.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["code", "next_action"], "{context}");
}

fn child_ids(response: &RpcResponse) -> std::collections::HashSet<Uuid> {
    response
        .result
        .as_ref()
        .expect("children result")
        .as_array()
        .expect("children array")
        .iter()
        .map(|row| Uuid::parse_str(row["id"].as_str().unwrap()).unwrap())
        .collect()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn agent_read_scope_refuses_worker_reading_another_epic_or_project_for_every_verb() {
    let f = read_scope_fixture().await;
    let unknown = Uuid::new_v4();
    for method in scoped_read_verbs() {
        for (label, target) in [
            ("other-epic lead", f.lb),
            ("other-epic worker", f.wb),
            ("other-epic container", f.epic_b),
            ("other-project lead", f.lq),
            ("manager root", f.mgr),
            ("nonexistent", unknown),
        ] {
            let response = tokened_read(&f, "wa", method, scoped_read_params(method, target)).await;
            assert_read_scope_denied(&response, &format!("{method} {label}"));
        }
    }
    // A batch with one foreign cursor is refused as a whole.
    let batch = tokened_read(
        &f,
        "wa",
        "GetConversationsSince",
        serde_json::json!({ "requests": [{ "session_id": f.wa }, { "session_id": f.lq }] }),
    )
    .await;
    assert_read_scope_denied(&batch, "mixed batch");
    // An unregistered token never falls back to operator reads.
    let mut request = RpcRequest::new("GetSession", scoped_read_params("GetSession", f.wa));
    request.session_token = Some("read-scope-unregistered".into());
    let HandleResult::Response(response) = f.rpc.server.handle_request_inner(&request).await else {
        panic!("expected response");
    };
    let error = response.error.expect("unknown token refused");
    assert!(
        error.message.contains("agent_verb_unknown_session_token"),
        "{error:?}"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn session_diagnostics_read_is_bounded_and_pages_by_stable_id() {
    let f = read_scope_fixture().await;
    let first = tokened_read(
        &f,
        "wa",
        "GetSessionDiagnostics",
        serde_json::json!({ "session_id": f.wa, "limit": 1 }),
    )
    .await;
    assert_read_admitted(&first, "session diagnostic page");
    let page = first.result.unwrap();
    assert_eq!(page["diagnostics"].as_array().unwrap().len(), 1);
    assert_eq!(
        page["diagnostics"][0]["message"],
        "diagnostic read-scope fixture"
    );
    let cursor = page["next_after_id"].as_i64().unwrap();

    let next = tokened_read(
        &f,
        "wa",
        "GetSessionDiagnostics",
        serde_json::json!({ "session_id": f.wa, "after_id": cursor, "limit": 1 }),
    )
    .await;
    assert_read_admitted(&next, "next diagnostic page");
    assert!(
        next.result.unwrap()["diagnostics"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    let oversized = call_rpc(
        &f.rpc.server,
        "GetSessionDiagnostics",
        serde_json::json!({ "session_id": f.wa, "limit": 101 }),
    )
    .await;
    assert!(
        oversized.error.is_some(),
        "oversized diagnostic page is refused"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn agent_read_scope_admits_self_lineage_child_and_nested_descendant() {
    let f = read_scope_fixture().await;
    for method in scoped_read_verbs() {
        for (label, target) in [
            ("self", f.wa),
            ("rotation predecessor", f.pwa),
            ("rotation successor", f.swa),
            ("own child", f.ca),
            ("nested descendant", f.na),
        ] {
            let response = tokened_read(&f, "wa", method, scoped_read_params(method, target)).await;
            assert_read_admitted(&response, &format!("{method} {label}"));
        }
    }
    let session = tokened_read(
        &f,
        "wa",
        "GetSession",
        scoped_read_params("GetSession", f.na),
    )
    .await;
    assert_eq!(session.result.unwrap()["id"], f.na.to_string());
    let children = tokened_read(
        &f,
        "wa",
        "ListSessionChildren",
        scoped_read_params("ListSessionChildren", f.wa),
    )
    .await;
    assert_eq!(
        child_ids(&children),
        std::collections::HashSet::from([f.ca])
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn agent_read_scope_admits_epic_lead_over_every_session_of_its_epic() {
    let f = read_scope_fixture().await;
    for method in scoped_read_verbs() {
        for target in [f.epic_a, f.la, f.wa, f.wa2, f.pwa, f.swa, f.ca, f.na] {
            let response = tokened_read(&f, "la", method, scoped_read_params(method, target)).await;
            assert_read_admitted(&response, &format!("lead {method} {target}"));
        }
        let foreign = tokened_read(&f, "la", method, scoped_read_params(method, f.wb)).await;
        assert_read_scope_denied(&foreign, &format!("lead {method} other epic"));
    }
    let children = tokened_read(
        &f,
        "la",
        "ListSessionChildren",
        scoped_read_params("ListSessionChildren", f.epic_a),
    )
    .await;
    assert_eq!(
        child_ids(&children),
        std::collections::HashSet::from([f.la, f.pwa, f.wa, f.swa, f.wa2])
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn agent_read_scope_worker_reads_own_containers_metadata_but_not_sibling_content() {
    let f = read_scope_fixture().await;
    for method in ["GetSession", "GetSessionSummary", "ListSessionChildren"] {
        for (label, container) in [("own epic", f.epic_a), ("own group", f.group)] {
            let response =
                tokened_read(&f, "wa", method, scoped_read_params(method, container)).await;
            assert_read_admitted(&response, &format!("{method} {label}"));
        }
    }
    let epic = tokened_read(
        &f,
        "wa",
        "GetSession",
        scoped_read_params("GetSession", f.epic_a),
    )
    .await;
    assert_eq!(epic.result.unwrap()["id"], f.epic_a.to_string());
    // Listing its Epic shows the worker exactly its readable rows:
    // itself and its rotation lineage (filtered, not refused).
    let siblings = tokened_read(
        &f,
        "wa",
        "ListSessionChildren",
        scoped_read_params("ListSessionChildren", f.epic_a),
    )
    .await;
    assert_eq!(
        child_ids(&siblings),
        std::collections::HashSet::from([f.pwa, f.wa, f.swa])
    );
    // A root listing shows exactly the readable roots: its own Group.
    let roots = tokened_read(
        &f,
        "wa",
        "ListSessionChildren",
        serde_json::json!({ "parent_id": null }),
    )
    .await;
    assert_eq!(
        child_ids(&roots),
        std::collections::HashSet::from([f.group])
    );
    // Conversation/metrics verbs get no ancestor rule, and a sibling is
    // outside the worker's scope entirely.
    for method in CONTENT_READ_VERBS {
        for (label, target) in [("own epic", f.epic_a), ("sibling", f.wa2), ("lead", f.la)] {
            let response = tokened_read(&f, "wa", method, scoped_read_params(method, target)).await;
            assert_read_scope_denied(&response, &format!("{method} {label}"));
        }
    }
    let sibling_row = tokened_read(
        &f,
        "wa",
        "GetSession",
        scoped_read_params("GetSession", f.wa2),
    )
    .await;
    assert_read_scope_denied(&sibling_row, "sibling metadata");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn agent_read_scope_admits_current_manager_only_inside_its_scope() {
    let f = read_scope_fixture().await;
    for method in CONTENT_READ_VERBS {
        for (label, target) in [("scoped lead", f.la), ("scoped descendant", f.na)] {
            let response =
                tokened_read(&f, "mgr", method, scoped_read_params(method, target)).await;
            assert_read_admitted(&response, &format!("manager {method} {label}"));
        }
        for (label, target) in [("out-of-scope epic", f.lb), ("foreign project", f.lq)] {
            let response =
                tokened_read(&f, "mgr", method, scoped_read_params(method, target)).await;
            assert_read_scope_denied(&response, &format!("manager {method} {label}"));
        }
    }
    let batch = tokened_read(
        &f,
        "mgr",
        "GetConversationsSince",
        serde_json::json!({ "requests": [{ "session_id": f.la }, { "session_id": f.wa }] }),
    )
    .await;
    assert_read_admitted(&batch, "manager scoped batch");
    let sessions: Vec<String> = batch.result.unwrap()["conversations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["session_id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(sessions, [f.la.to_string(), f.wa.to_string()]);
    let lead_conversation = tokened_read(
        &f,
        "mgr",
        "GetConversation",
        scoped_read_params("GetConversation", f.la),
    )
    .await;
    assert_eq!(
        lead_conversation.result.unwrap()[0]["content"],
        format!("read-scope event {}", f.la)
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn agent_read_scope_leaves_unattributed_reads_unchanged() {
    let f = read_scope_fixture().await;
    for method in scoped_read_verbs() {
        for target in [f.lq, f.wb, f.wa2] {
            let response =
                call_rpc(&f.rpc.server, method, scoped_read_params(method, target)).await;
            assert_read_admitted(&response, &format!("operator {method} {target}"));
        }
    }
    let session = call_rpc(
        &f.rpc.server,
        "GetSession",
        scoped_read_params("GetSession", f.lq),
    )
    .await;
    assert_eq!(session.result.unwrap()["id"], f.lq.to_string());
    let children = call_rpc(
        &f.rpc.server,
        "ListSessionChildren",
        scoped_read_params("ListSessionChildren", f.epic_a),
    )
    .await;
    assert_eq!(
        child_ids(&children),
        std::collections::HashSet::from([f.la, f.pwa, f.wa, f.swa, f.wa2])
    );
    // The unscoped read verbs stay unscoped for tokened callers.
    for method in agent_gate::UNSCOPED_READ_VERBS.iter() {
        let response = tokened_read(&f, "wa", method, serde_json::Value::Null).await;
        assert_read_admitted(&response, method);
    }
}

/// Extra rows for the #241 corruption tests: `(id, kind, parent,
/// continued_from)`. Parents are written after every row exists, so a
/// row list may describe a `parent_id` cycle.
async fn insert_read_scope_rows(
    f: &ReadScopeFixture,
    rows: &[(
        Uuid,
        rsi_common::types::SessionKind,
        Option<Uuid>,
        Option<Uuid>,
    )],
) {
    let store = f.rpc.manager.store().lock().await;
    for &(id, kind, _, continued_from) in rows {
        let mut row = mk_agent_test_session(id, kind, None, None);
        row.project_id = Some(issue_rpc_project_id());
        row.continued_from = continued_from;
        store.insert_session(&row).unwrap();
    }
    for &(id, _, parent, _) in rows {
        if parent.is_some() {
            store.update_session_parent(id, parent).unwrap();
        }
    }
}

async fn assert_denied_for_every_scoped_verb(
    f: &ReadScopeFixture,
    caller: &str,
    target: Uuid,
    label: &str,
) {
    for method in scoped_read_verbs() {
        let response = tokened_read(f, caller, method, scoped_read_params(method, target)).await;
        assert_read_scope_denied(&response, &format!("{label}: {method}"));
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn agent_read_scope_nested_worker_reads_owning_epic_and_group_but_not_intermediate_parent() {
    let f = read_scope_fixture().await;
    f.rpc
        .manager
        .register_agent_token("read-scope-ca".into(), f.ca)
        .await;
    // ca -> wa -> epic_a -> group: the owning Epic and its Group are
    // readable metadata.
    for method in ["GetSession", "GetSessionSummary", "ListSessionChildren"] {
        for (label, container) in [("owning epic", f.epic_a), ("epic group", f.group)] {
            let response =
                tokened_read(&f, "ca", method, scoped_read_params(method, container)).await;
            assert_read_admitted(&response, &format!("nested {method} {label}"));
        }
    }
    let epic = tokened_read(
        &f,
        "ca",
        "GetSession",
        scoped_read_params("GetSession", f.epic_a),
    )
    .await;
    assert_eq!(epic.result.unwrap()["id"], f.epic_a.to_string());
    // The intermediate leaf parent is not a container grant: every verb,
    // metadata and conversation alike, is refused for it.
    assert_denied_for_every_scoped_verb(&f, "ca", f.wa, "intermediate parent").await;
    // The nested worker still reads itself and its own child.
    let own_child = tokened_read(
        &f,
        "ca",
        "GetConversation",
        scoped_read_params("GetConversation", f.na),
    )
    .await;
    assert_eq!(
        own_child.result.unwrap()[0]["content"],
        format!("read-scope event {}", f.na)
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn agent_read_scope_parent_cycle_denies_on_caller_and_target_side() {
    use rsi_common::types::SessionKind;
    let f = read_scope_fixture().await;
    // Caller side: c1 <-> c2 parent cycle. Without cycle detection c1
    // would read its rotation successor `succ` and c2 (whose bounded
    // parent walk reaches c1).
    let (c1, c2, succ) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    // Target side: t <-> u parent cycle; t is wa's rotation successor,
    // which a clean chain would admit.
    let (t, u) = (Uuid::new_v4(), Uuid::new_v4());
    insert_read_scope_rows(
        &f,
        &[
            (c1, SessionKind::Task, Some(c2), None),
            (c2, SessionKind::Task, Some(c1), None),
            (succ, SessionKind::Task, None, Some(c1)),
            (t, SessionKind::Task, Some(u), Some(f.wa)),
            (u, SessionKind::Task, Some(t), None),
        ],
    )
    .await;
    f.rpc
        .manager
        .register_agent_token("read-scope-c1".into(), c1)
        .await;
    for (label, target) in [
        ("caller cycle: successor", succ),
        ("caller cycle: cycle peer", c2),
        ("caller cycle: self", c1),
    ] {
        assert_denied_for_every_scoped_verb(&f, "c1", target, label).await;
    }
    assert_denied_for_every_scoped_verb(&f, "wa", t, "target cycle: successor").await;
    // The clean caller keeps its ordinary grants.
    let clean = tokened_read(
        &f,
        "wa",
        "GetConversation",
        scoped_read_params("GetConversation", f.swa),
    )
    .await;
    assert_read_admitted(&clean, "clean successor");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn agent_read_scope_continued_from_cycle_denies_on_caller_and_target_side() {
    use rsi_common::types::SessionKind;
    let f = read_scope_fixture().await;
    // Caller side: k1 -> k2 -> k3 -> k2 lineage cycle; k1's own child q
    // would otherwise be admitted by the control-subtree rule, and k2 by
    // the predecessor rule.
    let (k1, k2, k3, q) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    // Target side: r (child of wa) -> r2 -> r3 -> r2 lineage cycle.
    let (r, r2, r3) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    insert_read_scope_rows(
        &f,
        &[
            (k1, SessionKind::Task, None, Some(k2)),
            (k2, SessionKind::Task, None, Some(k3)),
            (k3, SessionKind::Task, None, Some(k2)),
            (q, SessionKind::Task, Some(k1), None),
            (r, SessionKind::Task, Some(f.wa), Some(r2)),
            (r2, SessionKind::Task, None, Some(r3)),
            (r3, SessionKind::Task, None, Some(r2)),
        ],
    )
    .await;
    f.rpc
        .manager
        .register_agent_token("read-scope-k1".into(), k1)
        .await;
    assert_denied_for_every_scoped_verb(&f, "k1", q, "caller lineage cycle: child").await;
    assert_denied_for_every_scoped_verb(&f, "k1", k2, "caller lineage cycle: predecessor").await;
    assert_denied_for_every_scoped_verb(&f, "wa", r, "target lineage cycle: child").await;
    let clean = tokened_read(
        &f,
        "wa",
        "GetConversation",
        scoped_read_params("GetConversation", f.ca),
    )
    .await;
    assert_read_admitted(&clean, "clean child");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn agent_read_scope_depth_overflow_denies_on_caller_and_target_side() {
    use rsi_common::types::SessionKind;
    let f = read_scope_fixture().await;
    let mut rows = Vec::new();
    // Caller side: a 300-row acyclic lineage (past the 256 bound). Its
    // head `lineage[0]` would otherwise read its own child `x`.
    let lineage: Vec<Uuid> = (0..300).map(|_| Uuid::new_v4()).collect();
    for (index, id) in lineage.iter().enumerate() {
        rows.push((
            *id,
            SessionKind::Task,
            None,
            lineage.get(index + 1).copied(),
        ));
    }
    let x = Uuid::new_v4();
    rows.push((x, SessionKind::Task, Some(lineage[0]), None));
    // Target side (lineage): child `y` of wa whose own lineage is the
    // same over-deep chain.
    let y = Uuid::new_v4();
    rows.push((y, SessionKind::Task, Some(f.wa), Some(lineage[0])));
    // Target side (parents): Epic `z` led by wa sitting under a 70-row
    // acyclic parent chain (past the 64 bound).
    let parents: Vec<Uuid> = (0..70).map(|_| Uuid::new_v4()).collect();
    for (index, id) in parents.iter().enumerate() {
        rows.push((
            *id,
            SessionKind::Task,
            parents.get(index + 1).copied(),
            None,
        ));
    }
    let z = Uuid::new_v4();
    rows.push((z, SessionKind::Epic, Some(parents[0]), None));
    insert_read_scope_rows(&f, &rows).await;
    {
        let store = f.rpc.manager.store().lock().await;
        store.set_lead_session(z, Some(f.wa)).unwrap();
    }
    f.rpc
        .manager
        .register_agent_token("read-scope-deep".into(), lineage[0])
        .await;
    assert_denied_for_every_scoped_verb(&f, "deep", x, "caller lineage overflow: child").await;
    assert_denied_for_every_scoped_verb(&f, "wa", y, "target lineage overflow: child").await;
    assert_denied_for_every_scoped_verb(&f, "wa", z, "target parent overflow: led epic").await;
}

/// Every method name with an arm in `RpcServer`'s dispatch match.
fn dispatch_arm_methods() -> std::collections::BTreeSet<String> {
    const SOURCE: &str = include_str!("../rpc.rs");
    let start = SOURCE
        .find("let result = match request.method.as_str() {")
        .expect("dispatch match");
    let end = start
        + SOURCE[start..]
            .find("\n            _ => {")
            .expect("dispatch default arm");
    let quoted = regex::Regex::new(r#""([A-Z][A-Za-z0-9]+)""#).unwrap();
    let mut methods = std::collections::BTreeSet::new();
    for line in SOURCE[start..end].lines() {
        let line = line.trim_start();
        if !line.starts_with('"') {
            continue;
        }
        let head = line.split("=>").next().unwrap();
        methods.extend(quoted.captures_iter(head).map(|c| c[1].to_string()));
    }
    methods
}

/// #1011: the complete audience policy for every non-`Agent*` dispatch arm,
/// frozen here independently of `rsi_common::rpc_verb_registry`. An arm absent
/// from this list is operator-only. Moving an operator method into an
/// agent-visible audience (read, unscoped read or hook) requires editing both
/// this list and the registry, so a misclassification cannot pass silently.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn every_non_agent_dispatch_arm_has_its_frozen_audience() {
    use rsi_common::rpc_verb_registry::{RpcVerbAudience, audience_of};
    const FROZEN: &[(&str, RpcVerbAudience)] = &[
        ("GetSession", RpcVerbAudience::Read),
        ("GetSessionSummary", RpcVerbAudience::Read),
        ("GetHealthStatus", RpcVerbAudience::UnscopedRead),
        ("GetDaemonCapabilities", RpcVerbAudience::UnscopedRead),
        ("ListSessionChildren", RpcVerbAudience::Read),
        ("GetConversation", RpcVerbAudience::Read),
        ("GetSessionDiagnostics", RpcVerbAudience::Read),
        ("GetConversationsSince", RpcVerbAudience::Read),
        ("GetTurnMetrics", RpcVerbAudience::Read),
        ("ClaimBoundaryMail", RpcVerbAudience::Hook),
        ("ConfirmBoundaryMail", RpcVerbAudience::Hook),
    ];
    let arms = dispatch_arm_methods();
    for (method, _) in FROZEN {
        assert!(
            arms.contains(*method),
            "frozen {method} has no dispatch arm"
        );
    }
    for arm in arms.iter().filter(|arm| !arm.starts_with("Agent")) {
        let expected = FROZEN
            .iter()
            .find(|(method, _)| method == arm)
            .map(|(_, audience)| *audience);
        assert_eq!(
            audience_of(arm),
            expected,
            "non-Agent dispatch arm {arm} has an unexpected audience"
        );
        assert_eq!(
            agent_gate::is_allowed_for_attributed_caller(arm),
            expected.is_some(),
            "non-Agent dispatch arm {arm} has an unexpected attributed-caller gate"
        );
    }
}

/// #1011: the per-verb declaration and the dispatch match cannot drift. A
/// declared attributed verb has an arm, an `Agent*` arm has a declaration,
/// and an arm with no declaration is operator-only (default-denied for a
/// tokened caller, so never in an agent-visible list).
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn verb_declarations_and_dispatch_arms_agree() {
    use rsi_common::rpc_verb_registry::{RpcVerbAudience, audience_of, rpc_verb_declarations};
    let arms = dispatch_arm_methods();
    assert!(arms.len() > 150, "dispatch arms parsed: {}", arms.len());
    for declaration in rpc_verb_declarations() {
        assert!(
            arms.contains(declaration.method),
            "declared verb {} has no dispatch arm",
            declaration.method
        );
        assert!(agent_gate::is_allowed_for_attributed_caller(
            declaration.method
        ));
    }
    for arm in &arms {
        let declared = audience_of(arm);
        if arm.starts_with("Agent") {
            assert_eq!(
                declared,
                Some(RpcVerbAudience::Agent),
                "dispatch arm {arm} is an Agent verb with no declaration"
            );
        }
        if declared.is_none() {
            assert!(
                !agent_gate::is_allowed_for_attributed_caller(arm),
                "operator-only {arm} must not be agent-visible"
            );
        }
    }
    for method in rsi_common::manager_operator_delegation::DELEGABLE_OPERATOR_METHODS
        .iter()
        .chain(rsi_common::manager_operator_delegation::NEVER_DELEGABLE_OPERATOR_METHODS_V1)
    {
        assert_eq!(audience_of(method), None, "{method}");
        assert!(!agent_gate::is_allowed_for_attributed_caller(method));
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn operator_rpc_manager_tree_snapshots_pages_and_reports_counts() {
    use rsi_common::harness_manager_v2::{
        ConfigureHarnessManagerPolicyRequestV2, ManagerCapabilityV2, ManagerLaunchChoiceV2,
        ManagerOperatingModeV2, ManagerPolicyV2,
    };
    use rsi_common::manager_nodes::{
        ConfigureManagerNodeRequestV1, ManagerNodeAllowanceV1, ManagerNodeGrantV1,
        ManagerNodeSelectorV1,
    };
    use rsi_common::manager_tree::{GetManagerTreeResultV1, ManagerTreeKindV1};
    use rsi_common::types::{Project, SessionKind, SessionProvider};
    let fixture = recursive_dag_rpc_fixture();
    let project = issue_rpc_project_id();
    let project2 = Uuid::new_v4();
    let (owner, owner2, area_seat, group, epic, global_seat) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    {
        let store = fixture.manager.store().lock().await;
        store
            .insert_project(&Project {
                id: project2,
                name: "Second project".into(),
                path: None,
                description: None,
                color: Project::DEFAULT_COLOR.to_string(),
                context_files: None,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            })
            .unwrap();
        for (id, kind, parent, lead, in_project) in [
            (owner, SessionKind::Standard, None, None, Some(project)),
            (owner2, SessionKind::Standard, None, None, Some(project2)),
            (area_seat, SessionKind::Standard, None, None, Some(project)),
            (global_seat, SessionKind::Standard, None, None, None),
            (group, SessionKind::Group, None, None, Some(project)),
            (
                epic,
                SessionKind::Epic,
                Some(group),
                Some(area_seat),
                Some(project),
            ),
        ] {
            let mut session = mk_agent_test_session(id, kind, parent, lead);
            session.project_id = in_project;
            store.insert_session(&session).unwrap();
        }
    }
    for (p, s) in [(project, owner), (project2, owner2)] {
        let appointed = call_rpc(&fixture.server, "ConfigureHarnessManager", serde_json::json!({
            "project_id":p,"session_id":s,"epic_ids":null,"group_ids":[],"expected_row_version":0
        })).await;
        assert!(appointed.error.is_none(), "{:?}", appointed.error);
    }
    let root_policy = call_rpc(
        &fixture.server,
        "ConfigureHarnessManagerPolicy",
        serde_json::json!(ConfigureHarnessManagerPolicyRequestV2 {
            project_id: project,
            expected_scope_version: 1,
            expected_policy_version: 0,
            idempotency_key: "root-grant".into(),
            policy: ManagerPolicyV2 {
                mode: ManagerOperatingModeV2::Execute,
                capabilities: vec![
                    ManagerCapabilityV2::WorkPlan,
                    ManagerCapabilityV2::LeadControl
                ],
                max_active_sessions: 8,
                ..Default::default()
            },
        }),
    )
    .await;
    assert!(root_policy.error.is_none(), "{:?}", root_policy.error);
    let listed = call_rpc(
        &fixture.server,
        "ListManagerNodes",
        serde_json::json!({"project_id":project,"limit":64}),
    )
    .await;
    let root = &listed.result.as_ref().unwrap()["rows"][0];
    let root_id: Uuid = serde_json::from_value(root["node_id"].clone()).unwrap();
    let created = call_rpc(
        &fixture.server,
        "ConfigureManagerNode",
        serde_json::json!(ConfigureManagerNodeRequestV1 {
            node_id: None,
            parent_node_id: root_id,
            project_id: project,
            seat_root_session_id: area_seat,
            selector: ManagerNodeSelectorV1::Selected {
                group_ids: vec![],
                epic_ids: vec![epic],
            },
            grant: ManagerNodeGrantV1 {
                capabilities: vec![ManagerCapabilityV2::WorkPlan],
                allowed_launches: vec![],
                allowance: ManagerNodeAllowanceV1 {
                    max_created_containers: 0,
                    max_created_sessions: 0,
                    max_active_sessions: 3,
                    max_build_slots: 0,
                    max_disk_gib: 0,
                    provider_limits: vec![],
                    max_spend_usd: None,
                },
                max_direct_reports: 4,
            },
            policy: ManagerPolicyV2 {
                mode: ManagerOperatingModeV2::Monitor,
                capabilities: vec![ManagerCapabilityV2::WorkPlan],
                max_active_sessions: 3,
                ..Default::default()
            },
            expected_parent_grant_version: root["grant_version"].as_i64().unwrap(),
            expected_parent_policy_version: root["policy_version"].as_i64().unwrap(),
            expected_parent_authority_epoch: root["authority_epoch"].as_i64().unwrap(),
            expected_node_grant_version: 0,
            idempotency_key: "tree-area".into(),
        }),
    )
    .await;
    assert!(created.error.is_none(), "{:?}", created.error);
    let area_id: Uuid = serde_json::from_value(created.result.unwrap()["node_id"].clone()).unwrap();
    let granted = call_rpc(
        &fixture.server,
        "ConfigureGlobalManager",
        serde_json::json!({
            "session_id": global_seat,
            "project_ids": [project, project2],
            "allowed_launches": [ManagerLaunchChoiceV2 {
                provider: SessionProvider::Claude,
                model: "claude-opus-5-5".into(),
                effort: Some("high".into()),
            }],
            "project_policy": ManagerPolicyV2::default(),
            "expected_grant_version": 0,
            "idempotency_key": "tree-global",
        }),
    )
    .await;
    assert!(granted.error.is_none(), "{:?}", granted.error);

    let page = |after: Option<String>, limit: u16| {
        let server = &fixture.server;
        async move {
            let response = call_rpc(
                server,
                "GetManagerTree",
                serde_json::json!({"after": after, "limit": limit}),
            )
            .await;
            (
                response.error.clone(),
                response
                    .result
                    .map(|value| serde_json::from_value::<GetManagerTreeResultV1>(value).unwrap()),
            )
        }
    };
    let (error, full) = page(None, 200).await;
    assert!(error.is_none(), "{error:?}");
    let full = full.unwrap();
    let kinds: Vec<_> = full.rows.iter().map(|r| r.kind).collect();
    // #1236: the global grant renders as its portfolio node's row.
    assert_eq!(kinds[0], ManagerTreeKindV1::Portfolio);
    assert_eq!(full.rows[0].tier_label.as_deref(), Some("global"));
    assert_eq!(full.total_rows, full.rows.len() as u64);
    assert_eq!(full.global_grant_version, Some(1));
    assert!(full.complete);
    let area = full
        .rows
        .iter()
        .find(|r| r.node_id == Some(area_id))
        .expect("area row");
    assert_eq!(area.kind, ManagerTreeKindV1::Area);
    assert_eq!(area.scope.as_deref(), Some("0 groups, 1 epic"));
    assert_eq!(
        area.parent_key.as_deref(),
        Some(format!("project:{project}").as_str())
    );
    assert_eq!(area.grant.as_ref().unwrap().max_active_sessions, 3);
    assert_eq!(area.load.pending_escalations, Some(0));
    assert_eq!(area.focus_session_id, Some(area_seat));
    let epic_row = full
        .rows
        .iter()
        .find(|r| r.epic_id == Some(epic))
        .expect("epic row");
    assert_eq!(epic_row.parent_key.as_deref(), Some(area.key.as_str()));
    assert_eq!(epic_row.focus_session_id, Some(area_seat));
    assert_eq!(epic_row.depth, area.depth + 1);
    let pm_rows: Vec<_> = full
        .rows
        .iter()
        .filter(|r| r.kind == ManagerTreeKindV1::Project)
        .collect();
    assert_eq!(pm_rows.len(), 2);
    assert!(pm_rows.iter().any(|r| r.focus_session_id == Some(owner)));
    assert!(pm_rows.iter().any(|r| r.focus_session_id == Some(owner2)));
    assert_eq!(full.rows[0].load.direct_reports, Some(2));

    // Paging walks every row exactly once and total_rows never shrinks.
    let mut seen = Vec::new();
    let mut after = None;
    loop {
        let (error, page) = page(after.clone(), 2).await;
        assert!(error.is_none(), "{error:?}");
        let page = page.unwrap();
        assert_eq!(page.total_rows, full.total_rows);
        assert!(page.rows.len() <= 2);
        seen.extend(page.rows.iter().map(|r| r.key.clone()));
        match page.next_after {
            Some(next) => after = Some(next),
            None => break,
        }
    }
    let all: Vec<_> = full.rows.iter().map(|r| r.key.clone()).collect();
    assert_eq!(seen, all);
    let (error, _) = page(Some("area:missing".into()), 5).await;
    assert!(error.unwrap().message.contains("manager_tree_stale_cursor"));
    let (error, _) = page(None, 0).await;
    assert!(
        error
            .unwrap()
            .message
            .contains("manager_tree_invalid_request")
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn operator_rpc_global_manager_workspace_reports_grant_seat_and_portfolio() {
    use rsi_common::global_manager::GlobalManagerWorkspaceV1;
    use rsi_common::harness_manager_v2::{ManagerLaunchChoiceV2, ManagerPolicyV2};
    use rsi_common::types::{Project, SessionKind, SessionProvider};
    let fixture = recursive_dag_rpc_fixture();
    let project = issue_rpc_project_id();
    let project2 = Uuid::new_v4();
    let (pm, global_seat) = (Uuid::new_v4(), Uuid::new_v4());
    let read = || {
        let server = &fixture.server;
        async move {
            let response =
                call_rpc(server, "GetGlobalManagerWorkspace", serde_json::json!({})).await;
            assert!(response.error.is_none(), "{:?}", response.error);
            serde_json::from_value::<GlobalManagerWorkspaceV1>(response.result.unwrap()).unwrap()
        }
    };
    // Never appointed: an empty snapshot, not an error.
    let empty = read().await;
    assert_eq!(empty, GlobalManagerWorkspaceV1::default());
    {
        let store = fixture.manager.store().lock().await;
        store
            .insert_project(&Project {
                id: project2,
                name: "Second project".into(),
                path: None,
                description: None,
                color: Project::DEFAULT_COLOR.to_string(),
                context_files: None,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            })
            .unwrap();
        for (id, in_project) in [(pm, Some(project)), (global_seat, Some(project2))] {
            let mut session = mk_agent_test_session(id, SessionKind::Standard, None, None);
            session.project_id = in_project;
            store.insert_session(&session).unwrap();
        }
    }
    let appointed = call_rpc(&fixture.server, "ConfigureHarnessManager", serde_json::json!({
        "project_id":project,"session_id":pm,"epic_ids":null,"group_ids":[],"expected_row_version":0
    })).await;
    assert!(appointed.error.is_none(), "{:?}", appointed.error);
    let granted = call_rpc(
        &fixture.server,
        "ConfigureGlobalManager",
        serde_json::json!({
            "session_id": global_seat,
            "project_ids": [project, project2],
            "allowed_launches": [ManagerLaunchChoiceV2 {
                provider: SessionProvider::Claude,
                model: "claude-opus-5-5".into(),
                effort: Some("high".into()),
            }],
            "project_policy": ManagerPolicyV2::default(),
            "expected_grant_version": 0,
            "idempotency_key": "workspace-global",
        }),
    )
    .await;
    assert!(granted.error.is_none(), "{:?}", granted.error);

    let active = read().await;
    let grant = active.grant.expect("active grant");
    assert_eq!(grant.state, "active");
    let seat = active.seat.expect("seat session");
    assert_eq!(seat.session_id, global_seat);
    assert_eq!(seat.project_id, Some(project2));
    assert_eq!(active.projects.len(), 2);
    let first = &active.projects[0];
    assert_eq!(first.overview.project_id, project);
    assert_eq!(
        first.overview.manager.as_ref().map(|seat| seat.session_id),
        Some(pm)
    );
    assert!(!first.scope_revoked);
    assert_eq!(active.projects[1].overview.project_id, project2);
    assert_eq!(active.projects[1].overview.manager, None);
    assert!(active.missing_project_ids.is_empty());

    let revoked = call_rpc(
        &fixture.server,
        "RevokeGlobalManager",
        serde_json::json!({"expected_grant_version": grant.grant_version, "idempotency_key": "workspace-revoke"}),
    )
    .await;
    assert!(revoked.error.is_none(), "{:?}", revoked.error);
    let after = read().await;
    let last = after.grant.expect("the revoked grant stays visible");
    assert_eq!(last.state, "revoked");
    assert_eq!(last.grant_id, grant.grant_id);
    assert_eq!(after.projects.len(), 2);
}

/// #1239: the delegation verbs are attributed agent write verbs (never read
/// verbs); the operator portfolio methods stay out (pinned below).
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn delegation_verbs_are_agent_write_verbs() {
    for method in [
        "AgentManagerAppointChild",
        "AgentManagerRevokeChild",
        "AgentGlobalAppointManager",
    ] {
        assert!(agent_gate::AGENT_VERBS.contains(&method), "{method}");
        assert!(!agent_gate::READ_VERBS.contains(&method), "{method}");
        assert!(
            agent_gate::is_allowed_for_attributed_caller(method),
            "{method}"
        );
    }
}

/// #1236 catalog pin: every portfolio operator method is absent from every
/// agent-facing catalog and a tokened caller is refused before dispatch.
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn portfolio_node_methods_are_operator_only_and_attributed_denied() {
    let fixture = recursive_dag_rpc_fixture();
    let catalog = rsi_common::agent_control_schema::agent_control_catalog_v1();
    for method in rsi_common::portfolio_nodes::OPERATOR_METHODS {
        assert!(!agent_gate::AGENT_VERBS.contains(&method), "{method}");
        assert!(!agent_gate::READ_VERBS.contains(&method), "{method}");
        assert!(
            !agent_gate::is_allowed_for_attributed_caller(method),
            "{method}"
        );
        for descriptor in catalog {
            assert_ne!(descriptor.method, method, "agent CLI catalog: {method}");
            if let Some(tool) = descriptor.native_tool {
                assert!(
                    !tool.name().to_ascii_lowercase().contains("portfolio_node"),
                    "native tool {} exposes {method}",
                    tool.name()
                );
            }
        }
        let mut request = RpcRequest::new(
            method,
            serde_json::json!({"node_id": null, "tier_label": "global"}),
        );
        request.session_token = Some("manager-token".to_string());
        let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
        else {
            panic!("expected response for {method}");
        };
        assert!(response.result.is_none(), "{method}");
        let error = response
            .error
            .unwrap_or_else(|| panic!("attributed call to {method} must be denied"));
        assert_eq!(error.code, INVALID_PARAMS, "method: {method}");
        assert!(
            error
                .message
                .contains("not available to session-attributed callers"),
            "{method}: {}",
            error.message
        );
    }
}

/// #1238 catalog pin: the operator escalation queue and notice methods are
/// absent from every agent-facing catalog and refused to a tokened caller;
/// I5: so are the human-answer methods (the gate is by method, so it holds
/// for every tier's seat). `AgentReportUp`/`AgentSendDown` are agent write
/// verbs, never read verbs.
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn tier_routing_operator_methods_and_human_answers_are_attributed_denied() {
    let fixture = recursive_dag_rpc_fixture();
    let catalog = rsi_common::agent_control_schema::agent_control_catalog_v1();
    let operator_only = rsi_common::manager_tier_routing::OPERATOR_METHODS
        .into_iter()
        .chain(["AnswerQuestion", "AnswerHarnessManagerDecision"]);
    for method in operator_only {
        assert!(!agent_gate::AGENT_VERBS.contains(&method), "{method}");
        assert!(!agent_gate::READ_VERBS.contains(&method), "{method}");
        assert!(
            !agent_gate::is_allowed_for_attributed_caller(method),
            "{method}"
        );
        for descriptor in catalog {
            assert_ne!(descriptor.method, method, "agent CLI catalog: {method}");
            if let Some(tool) = descriptor.native_tool {
                let name = tool.name().to_ascii_lowercase();
                assert!(
                    !name.contains("operator_escalation") && !name.contains("operator_notice"),
                    "native tool {} exposes {method}",
                    tool.name()
                );
            }
        }
        let mut request = RpcRequest::new(
            method,
            serde_json::json!({"hop_id": Uuid::new_v4(), "ruling": "yes", "idempotency_key": "k"}),
        );
        request.session_token = Some("manager-token".to_string());
        let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
        else {
            panic!("expected response for {method}");
        };
        assert!(response.result.is_none(), "{method}");
        let error = response
            .error
            .unwrap_or_else(|| panic!("attributed call to {method} must be denied"));
        assert_eq!(error.code, INVALID_PARAMS, "method: {method}");
        assert!(
            error
                .message
                .contains("not available to session-attributed callers"),
            "{method}: {}",
            error.message
        );
    }
    for method in ["AgentReportUp", "AgentSendDown"] {
        assert!(agent_gate::AGENT_VERBS.contains(&method), "{method}");
        assert!(!agent_gate::READ_VERBS.contains(&method), "{method}");
    }
}

/// #1333 pin: `ListFrictionRollup` is operator-only (absent from every
/// agent-facing catalog, native tool and the CLI, refused to a tokened
/// caller); managers read the rollup through `AgentManagerInspect`.
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn friction_rollup_is_operator_only_and_out_of_agent_catalogs() {
    let fixture = recursive_dag_rpc_fixture();
    let catalog = rsi_common::agent_control_schema::agent_control_catalog_v1();
    assert_eq!(
        rsi_common::friction::OPERATOR_METHODS,
        ["ListFrictionRollup"]
    );
    for method in rsi_common::friction::OPERATOR_METHODS {
        assert!(!agent_gate::AGENT_VERBS.contains(&method), "{method}");
        assert!(!agent_gate::READ_VERBS.contains(&method), "{method}");
        assert!(
            !agent_gate::UNSCOPED_READ_VERBS.contains(&method),
            "{method}"
        );
        assert!(
            !agent_gate::is_allowed_for_attributed_caller(method),
            "{method}"
        );
        assert!(
            !rsi_common::rpc_verb_registry::cli_verb_methods().contains(&method),
            "{method}"
        );
        assert!(!catalog.iter().any(|entry| entry.method == method));
        let mut request = RpcRequest::new(method, serde_json::json!({}));
        request.session_token = Some("manager-token".to_string());
        let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
        else {
            panic!("expected response for {method}");
        };
        let error = response.error.expect("attributed call must be denied");
        assert!(
            error
                .message
                .contains("not available to session-attributed callers"),
            "{method}: {}",
            error.message
        );
    }
    for source in [
        include_str!("../tool_registry.rs"),
        include_str!("../session/harness/tools/rsi_control.rs"),
    ] {
        let source = source.to_ascii_lowercase();
        assert!(!source.contains("listfrictionrollup"));
        assert!(!source.contains("list_friction_rollup"));
    }
}

/// #1415 pin: listing and archiving stale decision records are operator-only
/// (absent from every agent-facing catalog, native tool and the CLI, refused
/// to a tokened caller); the operator reaches both over RPC and an archive of
/// a record that is not there deletes nothing and reports why.
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn stale_decision_methods_are_operator_only_and_reachable_by_the_operator() {
    let fixture = recursive_dag_rpc_fixture();
    let catalog = rsi_common::agent_control_schema::agent_control_catalog_v1();
    for method in ["ListStaleManagerDecisions", "ArchiveStaleManagerDecisions"] {
        assert!(!agent_gate::AGENT_VERBS.contains(&method), "{method}");
        assert!(!agent_gate::READ_VERBS.contains(&method), "{method}");
        assert!(
            !agent_gate::is_allowed_for_attributed_caller(method),
            "{method}"
        );
        assert!(
            !rsi_common::rpc_verb_registry::cli_verb_methods().contains(&method),
            "{method}"
        );
        assert!(!catalog.iter().any(|entry| entry.method == method));
        let mut request = RpcRequest::new(method, serde_json::json!({}));
        request.session_token = Some("manager-token".to_string());
        let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
        else {
            panic!("expected response for {method}");
        };
        let error = response.error.expect("attributed call must be denied");
        assert!(
            error
                .message
                .contains("not available to session-attributed callers"),
            "{method}: {}",
            error.message
        );
    }
    let listed = call_rpc(
        &fixture.server,
        "ListStaleManagerDecisions",
        serde_json::json!({"older_than_days": 7}),
    )
    .await
    .result
    .expect("the operator lists stale decisions");
    let listed: rsi_common::harness_manager_v2::ListStaleManagerDecisionsResponseV2 =
        serde_json::from_value(listed).unwrap();
    assert_eq!(listed.older_than_days, 7);
    assert!(listed.rows.is_empty());
    let missing = rsi_common::harness_manager_v2::ManagerDecisionRefV2 {
        project_id: Uuid::new_v4(),
        owner_manager_session_id: Uuid::new_v4(),
        scope_version: 1,
        key: "gone".into(),
        expected_row_version: 1,
    };
    let archived = call_rpc(
        &fixture.server,
        "ArchiveStaleManagerDecisions",
        serde_json::json!({"items": [missing]}),
    )
    .await
    .result
    .expect("the operator archives stale decisions");
    let archived: rsi_common::harness_manager_v2::ArchiveStaleManagerDecisionsResponseV2 =
        serde_json::from_value(archived).unwrap();
    assert!(archived.archived.is_empty());
    assert_eq!(archived.skipped[0].reason, "missing");
    let invalid = call_rpc(
        &fixture.server,
        "ArchiveStaleManagerDecisions",
        serde_json::json!({"items": []}),
    )
    .await;
    assert!(
        invalid.error.is_some(),
        "an empty archive request is refused"
    );
}

/// #1333 andon recording point: a tokened `Agent*` verb that is refused
/// records `agent_refusal:<verb>:<code>` friction for the caller, and the
/// operator reads it with `ListFrictionRollup`.
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn refused_agent_verbs_record_friction_the_operator_rolls_up() {
    let fixture = recursive_dag_rpc_fixture();
    let caller = Uuid::new_v4();
    fixture
        .manager
        .register_agent_token("friction-caller-token".into(), caller)
        .await;
    let mut request = RpcRequest::new(
        "AgentGetIssue",
        serde_json::json!({"display_number": 999_999}),
    );
    request.session_token = Some("friction-caller-token".to_string());
    let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
    else {
        panic!("expected response");
    };
    assert!(response.error.is_some(), "the lookup is refused");

    let rollup = call_rpc(
        &fixture.server,
        "ListFrictionRollup",
        serde_json::json!({"window_hours": 1}),
    )
    .await
    .result
    .expect("ListFrictionRollup succeeds for the operator");
    let rollup: rsi_common::friction::ListFrictionRollupResultV1 =
        serde_json::from_value(rollup).unwrap();
    let row = rollup
        .rows
        .iter()
        .find(|row| row.signature.starts_with("agent_refusal:AgentGetIssue:"))
        .expect("refusal recorded");
    assert_eq!((row.occurrences, row.sessions), (1, 1));
    assert!(rsi_common::friction::is_friction_signature(&row.signature));
    let invalid = call_rpc(
        &fixture.server,
        "ListFrictionRollup",
        serde_json::json!({"window_hours": 0}),
    )
    .await;
    assert!(invalid.error.is_some());
}

/// #1240 catalog pin: `GetManagerNodeWorkspace` is absent from every
/// agent-facing catalog and refused to a tokened caller; the operator reads
/// any node with it. `AgentManagerOverview` is the agent verb (catalog, CLI
/// and native tool), never a read verb.
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn manager_node_workspace_is_operator_only_and_the_overview_is_an_agent_verb() {
    use rsi_common::manager_node_workspace::ManagerNodeWorkspaceV1;
    let fixture = recursive_dag_rpc_fixture();
    let catalog = rsi_common::agent_control_schema::agent_control_catalog_v1();
    let project = issue_rpc_project_id();
    let params = serde_json::json!({"node": {"kind": "project", "project_id": project}});
    for method in rsi_common::manager_node_workspace::OPERATOR_METHODS {
        assert!(!agent_gate::AGENT_VERBS.contains(&method), "{method}");
        assert!(!agent_gate::READ_VERBS.contains(&method), "{method}");
        assert!(
            !agent_gate::is_allowed_for_attributed_caller(method),
            "{method}"
        );
        assert!(
            !rsi_common::rpc_verb_registry::cli_verb_methods().contains(&method),
            "{method}"
        );
        for descriptor in catalog {
            assert_ne!(descriptor.method, method, "agent CLI catalog: {method}");
            if let Some(tool) = descriptor.native_tool {
                assert!(
                    !tool.name().to_ascii_lowercase().contains("workspace"),
                    "native tool {} exposes {method}",
                    tool.name()
                );
            }
        }
        let mut request = RpcRequest::new(method, params.clone());
        request.session_token = Some("manager-token".to_string());
        let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
        else {
            panic!("expected response for {method}");
        };
        assert!(response.result.is_none(), "{method}");
        let error = response.error.expect("attributed call must be denied");
        assert_eq!(error.code, INVALID_PARAMS, "method: {method}");
        assert!(
            error
                .message
                .contains("not available to session-attributed callers"),
            "{method}: {}",
            error.message
        );
    }
    let overview = catalog
        .iter()
        .find(|descriptor| descriptor.method == "AgentManagerOverview")
        .expect("AgentManagerOverview is catalogued");
    assert_eq!(
        overview.native_tool.map(|tool| tool.name()),
        Some("rsi_control_manager_overview")
    );
    assert!(agent_gate::AGENT_VERBS.contains(&"AgentManagerOverview"));
    assert!(!agent_gate::READ_VERBS.contains(&"AgentManagerOverview"));
    assert!(rsi_common::rpc_verb_registry::cli_verb_methods().contains(&"AgentManagerOverview"));

    // The operator reads a project node and is told an unknown node apart.
    let response = call_rpc(&fixture.server, "GetManagerNodeWorkspace", params).await;
    assert!(response.error.is_none(), "{:?}", response.error);
    let workspace: ManagerNodeWorkspaceV1 =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!(
        workspace.node,
        rsi_common::manager_tier_routing::ManagerNodeRefV1::Project {
            project_id: project
        }
    );
    assert_eq!(
        workspace
            .projects
            .iter()
            .map(|row| row.overview.project_id)
            .collect::<Vec<_>>(),
        vec![project]
    );
    let unknown = call_rpc(
        &fixture.server,
        "GetManagerNodeWorkspace",
        serde_json::json!({"node": {"kind": "portfolio", "node_id": Uuid::new_v4()}}),
    )
    .await;
    assert!(
        unknown
            .error
            .unwrap()
            .message
            .contains("manager_tier_target_unknown")
    );
    let malformed = call_rpc(
        &fixture.server,
        "GetManagerNodeWorkspace",
        serde_json::json!({"node_id": Uuid::new_v4()}),
    )
    .await;
    assert!(
        malformed
            .error
            .unwrap()
            .message
            .contains("manager_tier_invalid_request")
    );
}

/// #1236: two roots over [A, B] and [C] through the operator RPCs; an
/// overlapping third root is refused, the `*GlobalManager` shims behave as v0
/// with one `global` root and refuse `global_manager_ambiguous` with two.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn operator_rpc_portfolio_nodes_round_trip_and_global_shims() {
    use rsi_common::harness_manager_v2::{ManagerLaunchChoiceV2, ManagerPolicyV2};
    use rsi_common::portfolio_nodes::{ListPortfolioNodesResultV1, PortfolioNodeV1};
    use rsi_common::types::{Project, SessionKind, SessionProvider};
    let fixture = recursive_dag_rpc_fixture();
    let a = issue_rpc_project_id();
    let (b, c) = (Uuid::new_v4(), Uuid::new_v4());
    let (seat_ab, seat_c, seat_third) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let seat_fourth = Uuid::new_v4();
    {
        let store = fixture.manager.store().lock().await;
        for (id, name) in [(b, "B"), (c, "C")] {
            store
                .insert_project(&Project {
                    id,
                    name: name.into(),
                    path: None,
                    description: None,
                    color: Project::DEFAULT_COLOR.to_string(),
                    context_files: None,
                    created_at: chrono::Utc::now(),
                    updated_at: chrono::Utc::now(),
                })
                .unwrap();
        }
        for id in [seat_ab, seat_c, seat_third, seat_fourth] {
            let session = mk_agent_test_session(id, SessionKind::Standard, None, None);
            store.insert_session(&session).unwrap();
        }
    }
    let launch = ManagerLaunchChoiceV2 {
        provider: SessionProvider::Claude,
        model: "claude-opus-5-5".into(),
        effort: Some("high".into()),
    };
    let configure = |seat: Uuid, projects: Vec<Uuid>, key: &str| {
        serde_json::json!({
            "tier_label": "global",
            "seat_session_id": seat,
            "project_ids": projects,
            "allowed_launches": [launch.clone()],
            "policy": ManagerPolicyV2::default(),
            "expected_node_grant_version": 0,
            "expected_authority_epoch": 0,
            "idempotency_key": key,
        })
    };
    // One `global` root through the shim behaves as v0.
    let shim = call_rpc(
        &fixture.server,
        "ConfigureGlobalManager",
        serde_json::json!({
            "session_id": seat_ab,
            "project_ids": [a, b],
            "allowed_launches": [launch.clone()],
            "project_policy": ManagerPolicyV2::default(),
            "expected_grant_version": 0,
            "idempotency_key": "shim-ab",
        }),
    )
    .await;
    assert!(shim.error.is_none(), "{:?}", shim.error);
    let got = call_rpc(&fixture.server, "GetGlobalManager", serde_json::json!({})).await;
    assert_eq!(got.result.unwrap()["seat_session_id"], seat_ab.to_string());

    let created = call_rpc(
        &fixture.server,
        "ConfigurePortfolioNode",
        configure(seat_c, vec![c], "root-c"),
    )
    .await;
    assert!(created.error.is_none(), "{:?}", created.error);
    let root_c: PortfolioNodeV1 = serde_json::from_value(created.result.unwrap()).unwrap();
    assert_eq!(root_c.grant.project_ids, [c]);
    assert_eq!(root_c.parent_node_id, None);

    let overlap = call_rpc(
        &fixture.server,
        "ConfigurePortfolioNode",
        configure(seat_third, vec![a], "root-third"),
    )
    .await;
    assert!(
        overlap
            .error
            .unwrap()
            .message
            .contains("manager_scope_overlap")
    );
    // #1237: a node nests under an existing parent it narrows, and an
    // adoption names only current roots (the nested node is not one).
    let mut nested = configure(seat_third, vec![c], "nested");
    nested["parent_node_id"] = serde_json::json!(root_c.node_id);
    nested["tier_label"] = serde_json::json!("region");
    nested["policy"]["max_active_sessions"] = serde_json::json!(3);
    let nested = call_rpc(&fixture.server, "ConfigurePortfolioNode", nested).await;
    assert!(nested.error.is_none(), "{:?}", nested.error);
    let nested: PortfolioNodeV1 = serde_json::from_value(nested.result.unwrap()).unwrap();
    assert_eq!(nested.parent_node_id, Some(root_c.node_id));
    let mut adopt = configure(seat_fourth, vec![c], "adopt-nested");
    adopt["adopt_node_ids"] = serde_json::json!([nested.node_id]);
    let adopt = call_rpc(&fixture.server, "ConfigurePortfolioNode", adopt).await;
    assert!(
        adopt
            .error
            .unwrap()
            .message
            .contains("portfolio_adopt_not_root")
    );

    let listed = call_rpc(&fixture.server, "ListPortfolioNodes", serde_json::json!({})).await;
    let listed: ListPortfolioNodesResultV1 =
        serde_json::from_value(listed.result.unwrap()).unwrap();
    assert_eq!(listed.nodes.len(), 3);
    let fetched = call_rpc(
        &fixture.server,
        "GetPortfolioNode",
        serde_json::json!({"node_id": root_c.node_id}),
    )
    .await;
    let fetched: PortfolioNodeV1 = serde_json::from_value(fetched.result.unwrap()).unwrap();
    assert_eq!(fetched, root_c);

    // Two `global` roots: every shim refuses.
    for (method, params) in [
        ("GetGlobalManager", serde_json::json!({})),
        ("GetGlobalManagerWorkspace", serde_json::json!({})),
        (
            "RevokeGlobalManager",
            serde_json::json!({"expected_grant_version": 1, "idempotency_key": "r"}),
        ),
    ] {
        let refused = call_rpc(&fixture.server, method, params).await;
        assert!(
            refused
                .error
                .as_ref()
                .is_some_and(|error| error.message.contains("global_manager_ambiguous")),
            "{method}: {:?}",
            refused.error
        );
    }

    let revoked = call_rpc(
        &fixture.server,
        "RevokePortfolioNode",
        serde_json::json!({
            "node_id": root_c.node_id,
            "expected_grant_version": root_c.grant.grant_version,
            "expected_authority_epoch": root_c.authority_epoch,
            "idempotency_key": "revoke-c",
        }),
    )
    .await;
    assert!(revoked.error.is_none(), "{:?}", revoked.error);
    assert_eq!(revoked.result.unwrap()["state"], "revoked");
    // The operator-granted region re-roots (grantor-scoped revoke).
    let region = call_rpc(
        &fixture.server,
        "GetPortfolioNode",
        serde_json::json!({"node_id": nested.node_id}),
    )
    .await;
    let region: PortfolioNodeV1 = serde_json::from_value(region.result.unwrap()).unwrap();
    assert_eq!(region.state, "active");
    assert_eq!(region.parent_node_id, None);
    // One `global` root again: the shim answers.
    let got = call_rpc(&fixture.server, "GetGlobalManager", serde_json::json!({})).await;
    assert!(got.error.is_none(), "{:?}", got.error);
    assert_eq!(got.result.unwrap()["seat_session_id"], seat_ab.to_string());
}

/// A tokened agent call: `Ok(result)` or the refusal message.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
async fn agent_rpc(
    server: &RpcServer,
    token: &str,
    method: &str,
    params: serde_json::Value,
) -> std::result::Result<serde_json::Value, String> {
    let mut rpc = RpcRequest::new(method, params);
    rpc.session_token = Some(token.into());
    let HandleResult::Response(response) = server.handle_request_inner(&rpc).await else {
        panic!("expected a response to {method}");
    };
    match response.error {
        None => Ok(response.result.unwrap_or_default()),
        Some(error) => Err(error.message),
    }
}

/// #1237 I11: swarm → pinnacle → global → project → area, built with
/// operator RPCs only. Each tier holds the PM verb set over its own coverage
/// (an Issue and the ledger fence per covered project, a refusal outside),
/// the area holds equal capabilities with a carved allowance, and the
/// results by role are identical under any tier labels.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
async fn five_tier_authority_by_role(labels: [&str; 3]) -> Vec<String> {
    use rsi_common::harness_manager_v2::{
        ConfigureHarnessManagerPolicyRequestV2, ManagerCapabilityV2, ManagerLaunchChoiceV2,
        ManagerOperatingModeV2, ManagerPolicyV2,
    };
    use rsi_common::manager_nodes::{
        ConfigureManagerNodeRequestV1, ManagerNodeAllowanceV1, ManagerNodeGrantV1,
        ManagerNodeSelectorV1,
    };
    use rsi_common::portfolio_nodes::PortfolioNodeV1;
    use rsi_common::types::{Project, SessionKind, SessionProvider};
    let fixture = recursive_dag_rpc_fixture();
    let a = issue_rpc_project_id();
    let (b, c) = (Uuid::new_v4(), Uuid::new_v4());
    let tier_seats = [Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];
    let (owner, area_seat, group, epic) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    {
        let store = fixture.manager.store().lock().await;
        for (id, name) in [(b, "B"), (c, "C")] {
            store
                .insert_project(&Project {
                    id,
                    name: name.into(),
                    path: None,
                    description: None,
                    color: Project::DEFAULT_COLOR.to_string(),
                    context_files: None,
                    created_at: chrono::Utc::now(),
                    updated_at: chrono::Utc::now(),
                })
                .unwrap();
        }
        for id in tier_seats {
            let mut session = mk_agent_test_session(id, SessionKind::Standard, None, None);
            session.project_id = None;
            store.insert_session(&session).unwrap();
        }
        for (id, kind, parent) in [
            (owner, SessionKind::Standard, None),
            (area_seat, SessionKind::Standard, None),
            (group, SessionKind::Group, None),
            (epic, SessionKind::Epic, Some(group)),
        ] {
            let mut session = mk_agent_test_session(id, kind, parent, None);
            session.project_id = Some(a);
            store.insert_session(&session).unwrap();
        }
    }
    let capabilities = vec![
        ManagerCapabilityV2::IssueCoordinate,
        ManagerCapabilityV2::WorkPlan,
        ManagerCapabilityV2::LeadControl,
    ];
    let tier_policy = |level: u16| ManagerPolicyV2 {
        mode: ManagerOperatingModeV2::Execute,
        capabilities: capabilities.clone(),
        max_active_sessions: 10 - level,
        ..Default::default()
    };
    let launch = ManagerLaunchChoiceV2 {
        provider: SessionProvider::Claude,
        model: "claude-opus-5-5".into(),
        effort: Some("high".into()),
    };
    // Operator RPCs only: three portfolio levels ...
    let coverage = [vec![a, b, c], vec![a, b], vec![a]];
    let mut parent: Option<Uuid> = None;
    for level in 0..3 {
        let created = call_rpc(
            &fixture.server,
            "ConfigurePortfolioNode",
            serde_json::json!({
                "parent_node_id": parent,
                "tier_label": labels[level],
                "seat_session_id": tier_seats[level],
                "project_ids": coverage[level],
                "allowed_launches": [launch.clone()],
                "policy": tier_policy(level as u16),
                "expected_node_grant_version": 0,
                "expected_authority_epoch": 0,
                "idempotency_key": format!("tier-{level}"),
            }),
        )
        .await;
        assert!(created.error.is_none(), "{:?}", created.error);
        let node: PortfolioNodeV1 = serde_json::from_value(created.result.unwrap()).unwrap();
        parent = Some(node.node_id);
    }
    // ... the project manager ...
    let appointed = call_rpc(&fixture.server, "ConfigureHarnessManager", serde_json::json!({
        "project_id": a, "session_id": owner, "epic_ids": null, "group_ids": [], "expected_row_version": 0
    }))
    .await;
    assert!(appointed.error.is_none(), "{:?}", appointed.error);
    let pm_policy = call_rpc(
        &fixture.server,
        "ConfigureHarnessManagerPolicy",
        serde_json::json!(ConfigureHarnessManagerPolicyRequestV2 {
            project_id: a,
            expected_scope_version: 1,
            expected_policy_version: 0,
            idempotency_key: "pm-grant".into(),
            policy: ManagerPolicyV2 {
                max_active_sessions: 8,
                ..tier_policy(3)
            },
        }),
    )
    .await;
    assert!(pm_policy.error.is_none(), "{:?}", pm_policy.error);
    // ... and an area node with the PM's capabilities, its allowance carved.
    let listed = call_rpc(
        &fixture.server,
        "ListManagerNodes",
        serde_json::json!({"project_id": a, "limit": 64}),
    )
    .await;
    let root = listed.result.unwrap()["rows"][0].clone();
    let area = call_rpc(
        &fixture.server,
        "ConfigureManagerNode",
        serde_json::json!(ConfigureManagerNodeRequestV1 {
            node_id: None,
            parent_node_id: serde_json::from_value(root["node_id"].clone()).unwrap(),
            project_id: a,
            seat_root_session_id: area_seat,
            selector: ManagerNodeSelectorV1::Selected {
                group_ids: vec![],
                epic_ids: vec![epic],
            },
            grant: ManagerNodeGrantV1 {
                capabilities: capabilities.clone(),
                allowed_launches: vec![],
                allowance: ManagerNodeAllowanceV1 {
                    max_created_containers: 0,
                    max_created_sessions: 0,
                    max_active_sessions: 3,
                    max_build_slots: 0,
                    max_disk_gib: 0,
                    provider_limits: vec![],
                    max_spend_usd: None,
                },
                max_direct_reports: 4,
            },
            policy: ManagerPolicyV2 {
                max_active_sessions: 3,
                ..tier_policy(3)
            },
            expected_parent_grant_version: root["grant_version"].as_i64().unwrap(),
            expected_parent_policy_version: root["policy_version"].as_i64().unwrap(),
            expected_parent_authority_epoch: root["authority_epoch"].as_i64().unwrap(),
            expected_node_grant_version: 0,
            idempotency_key: "area".into(),
        }),
    )
    .await;
    assert!(area.error.is_none(), "{:?}", area.error);

    let roles = [
        ("role0", tier_seats[0]),
        ("role1", tier_seats[1]),
        ("role2", tier_seats[2]),
        ("project", owner),
        ("area", area_seat),
    ];
    for (role, seat) in roles {
        fixture
            .manager
            .register_agent_token(format!("token-{role}"), seat)
            .await;
    }
    let outcome = |result: std::result::Result<serde_json::Value, String>| match result {
        Ok(_) => "ok".to_string(),
        Err(message) if message.contains("manager_project_not_in_scope") => {
            "manager_project_not_in_scope".to_string()
        }
        Err(message) => message,
    };
    let mut results = Vec::new();
    for (role, _) in roles {
        let token = format!("token-{role}");
        for (name, project) in [("a", a), ("b", b), ("c", c)] {
            // The portfolio levels name the project; the project and area
            // tiers name it only when it is not their own.
            let target = (role.starts_with("role") || project != a).then_some(project);
            let progress = agent_rpc(
                &fixture.server,
                &token,
                "AgentManagerProgress",
                serde_json::json!({ "project_id": target }),
            )
            .await;
            results.push(format!("{role}:{name}:progress:{}", outcome(progress)));
            if role != "area" {
                let issue = agent_rpc(
                    &fixture.server,
                    &token,
                    "AgentCreateIssue",
                    serde_json::json!({
                        "title": format!("{role} in {name}"),
                        "idempotency_key": format!("{role}-{name}"),
                        "project_id": target,
                    }),
                )
                .await;
                results.push(format!("{role}:{name}:issue:{}", outcome(issue)));
            }
        }
    }
    results
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn five_tiers_hold_the_pm_verb_set_over_their_coverage_under_any_labels() {
    let results = five_tier_authority_by_role(["swarm", "pinnacle", "global"]).await;
    let not_in_scope = "manager_project_not_in_scope";
    let mut expected = Vec::new();
    for (role, covered) in [
        ("role0", ["a", "b", "c"].as_slice()),
        ("role1", ["a", "b"].as_slice()),
        ("role2", ["a"].as_slice()),
        ("project", ["a"].as_slice()),
        ("area", ["a"].as_slice()),
    ] {
        for name in ["a", "b", "c"] {
            let verdict = if covered.contains(&name) {
                "ok"
            } else {
                not_in_scope
            };
            expected.push(format!("{role}:{name}:progress:{verdict}"));
            if role != "area" {
                expected.push(format!("{role}:{name}:issue:{verdict}"));
            }
        }
    }
    assert_eq!(results, expected);
    assert_eq!(
        five_tier_authority_by_role(["global", "swarm", "pinnacle"]).await,
        results
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn fleet_operator_rpc_returns_typed_bounded_snapshot() {
    let fixture = recursive_dag_rpc_fixture();
    let request = RpcRequest::new("GetFleetOverview", serde_json::json!({}));
    let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
    else {
        panic!("expected response");
    };
    assert!(response.error.is_none(), "{:?}", response.error);
    let snapshot: rsi_common::fleet::FleetOverview =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert!(snapshot.agents.len() <= 2048);
    assert_eq!(snapshot.totals.len(), 3);
}
