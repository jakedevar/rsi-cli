use super::*;
use rsi_common::host_tuning::*;

async fn configure(fixture: &RecursiveDagRpcFixture, key: &str, value: u64) -> TuningView {
    let current = call_rpc(&fixture.server, "GetHostTuning", serde_json::json!({}))
        .await
        .result
        .unwrap();
    let view: TuningView = serde_json::from_value(current).unwrap();
    let previous = view.actual[key];
    let configuration = TuningConfiguration {
        profile: HostProfile::Shared {
            reserve_percent: 15,
        },
        bounds: [(
            key.into(),
            Bounds {
                min: previous.min(value),
                max: previous.max(value),
            },
        )]
        .into(),
        presets: [("day/shared".into(), [(key.into(), value)].into())].into(),
        live_session_cap_guidance: Some(4),
    };
    let result = call_rpc(
        &fixture.server,
        "ConfigureHostTuning",
        serde_json::json!({"expected_revision":view.state.revision,"configuration":configuration}),
    )
    .await;
    assert!(result.error.is_none(), "{:?}", result.error);
    serde_json::from_value(result.result.unwrap()).unwrap()
}
async fn switch(fixture: &RecursiveDagRpcFixture, action: serde_json::Value) -> SwitchReceipt {
    let result = call_rpc(&fixture.server, "SwitchHostPreset", action).await;
    assert!(result.error.is_none(), "{:?}", result.error);
    serde_json::from_value(result.result.unwrap()).unwrap()
}
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn apply_undo_round_trip_reapply_is_idempotent_and_state_is_durable() {
    let fixture = recursive_dag_rpc_fixture();
    let key = "host_load_admission_threshold";
    let initial = fixture.runtime_config.to_json()[key].as_u64().unwrap();
    let target = if initial == 16 { 24 } else { 16 };
    configure(&fixture, key, target).await;
    let first = switch(
        &fixture,
        serde_json::json!({"action":"apply","name":"day/shared"}),
    )
    .await;
    assert!(first.complete && first.changed);
    assert_eq!(first.view.actual[key], target);
    assert_eq!(first.view.state.undo.as_ref().unwrap().values[key], initial);
    let again = switch(
        &fixture,
        serde_json::json!({"action":"apply","name":"day/shared"}),
    )
    .await;
    assert!(again.complete);
    assert_eq!(again.changed, false);
    assert_eq!(again.view.state.revision, first.view.state.revision);
    assert_eq!(again.view.state.undo.unwrap().values[key], initial);
    let restored = switch(&fixture, serde_json::json!({"action":"undo"})).await;
    assert!(restored.complete);
    assert_eq!(restored.view.actual[key], initial);
    let twice = switch(&fixture, serde_json::json!({"action":"undo"})).await;
    assert_eq!(twice.changed, false);
    let raw = fixture
        .manager
        .store()
        .lock()
        .await
        .get_daemon_setting(STORAGE_KEY)
        .unwrap()
        .unwrap();
    let state: TuningState = serde_json::from_str(&raw).unwrap();
    assert_eq!(state.revision, restored.view.state.revision);
    assert_eq!(state.pending, false);
}
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn failed_persistence_reports_actual_state_and_keeps_original_undo_for_retry() {
    let fixture = recursive_dag_rpc_fixture();
    let key = "host_load_admission_threshold";
    let initial = fixture.runtime_config.to_json()[key].as_u64().unwrap();
    let target = if initial == 16 { 24 } else { 16 };
    configure(&fixture, key, target).await;
    {
        let store = fixture.manager.store().lock().await;
        store.conn.execute_batch("CREATE TRIGGER reject_tuning_setting BEFORE INSERT ON daemon_settings WHEN NEW.key='host_load_admission_threshold' BEGIN SELECT RAISE(FAIL, 'injected preset persistence failure'); END;").unwrap();
    }
    let failed = switch(
        &fixture,
        serde_json::json!({"action":"apply","name":"day/shared"}),
    )
    .await;
    assert_eq!(failed.complete, false);
    assert!(
        failed
            .error
            .unwrap()
            .contains("injected preset persistence failure")
    );
    assert_eq!(failed.view.actual[key], initial);
    assert!(failed.view.state.pending);
    assert_eq!(failed.view.state.undo.unwrap().values[key], initial);
    fixture
        .manager
        .store()
        .lock()
        .await
        .conn
        .execute_batch("DROP TRIGGER reject_tuning_setting")
        .unwrap();
    let retry = switch(
        &fixture,
        serde_json::json!({"action":"apply","name":"day/shared"}),
    )
    .await;
    assert!(retry.complete);
    assert_eq!(retry.view.actual[key], target);
    assert_eq!(retry.view.state.undo.unwrap().values[key], initial);
    let undo = switch(&fixture, serde_json::json!({"action":"undo"})).await;
    assert_eq!(undo.view.actual[key], initial);
}
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn unavailable_slice_reports_saved_values_as_partial_and_retains_recovery() {
    let fixture = recursive_dag_rpc_fixture();
    let key = "hub_build_cpu_percent";
    let previous = fixture.runtime_config.to_json()[key].as_u64().unwrap();
    let target = if previous == 40 { 50 } else { 40 };
    configure(&fixture, key, target).await;
    let failed = switch(
        &fixture,
        serde_json::json!({"action":"apply","name":"day/shared"}),
    )
    .await;
    assert_eq!(failed.view.actual[key], target);
    assert_eq!(failed.complete, false);
    assert!(failed.error.unwrap().contains("slice unavailable"));
    assert_eq!(
        failed.settings_result.unwrap()["build_slice"]["state"],
        "unavailable"
    );
    assert!(failed.view.state.pending);
    let restored = switch(&fixture, serde_json::json!({"action":"undo"})).await;
    assert_eq!(restored.view.actual[key], previous);
    assert!(restored.view.state.undo.is_some());
}
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn editing_uses_revision_and_undo_honors_tightened_operator_bounds() {
    let fixture = recursive_dag_rpc_fixture();
    let key = "host_load_admission_threshold";
    let initial = fixture.runtime_config.to_json()[key].as_u64().unwrap();
    let target = if initial == 16 { 24 } else { 16 };
    configure(&fixture, key, target).await;
    let applied = switch(
        &fixture,
        serde_json::json!({"action":"apply","name":"day/shared"}),
    )
    .await;
    let mut configuration = applied.view.state.configuration;
    configuration.bounds.insert(
        key.into(),
        Bounds {
            min: target,
            max: target,
        },
    );
    let params = serde_json::json!({"expected_revision":applied.view.state.revision,"configuration":configuration});
    let updated = call_rpc(&fixture.server, "ConfigureHostTuning", params.clone()).await;
    assert!(updated.error.is_none());
    let stale = call_rpc(&fixture.server, "ConfigureHostTuning", params).await;
    assert!(stale.error.unwrap().message.contains("revision_changed"));
    let refused = call_rpc(
        &fixture.server,
        "SwitchHostPreset",
        serde_json::json!({"action":"undo"}),
    )
    .await;
    assert!(
        refused
            .error
            .unwrap()
            .message
            .contains("outside operator bounds")
    );
    assert_eq!(fixture.runtime_config.to_json()[key], target);
}
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn only_current_executing_manager_can_switch_and_agents_cannot_edit() {
    use rsi_common::harness_manager_v2::{ManagerOperatingModeV2, ManagerPolicyV2};
    let fixture = recursive_dag_rpc_fixture();
    let seat = Uuid::new_v4();
    {
        let store = fixture.manager.store().lock().await;
        let mut session =
            mk_agent_test_session(seat, rsi_common::types::SessionKind::Standard, None, None);
        session.project_id = Some(issue_rpc_project_id());
        store.insert_session(&session).unwrap();
    }
    let token = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    fixture
        .manager
        .register_agent_token(token.into(), seat)
        .await;
    configure(&fixture, "host_load_admission_threshold", 16).await;
    for method in ["GetHostTuning", "ConfigureHostTuning", "SwitchHostPreset"] {
        assert_eq!(agent_gate::is_allowed_for_attributed_caller(method), false);
        let mut request = RpcRequest::new(method, serde_json::json!({}));
        request.session_token = Some(token.into());
        let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
        else {
            panic!("response");
        };
        assert!(response.error.is_some(), "{method}");
    }
    let mut request = RpcRequest::new(
        "AgentSwitchHostPreset",
        serde_json::json!({"action":"apply","name":"day/shared"}),
    );
    request.session_token = Some(token.into());
    let HandleResult::Response(worker) = fixture.server.handle_request_inner(&request).await else {
        panic!("response");
    };
    assert!(worker.error.is_some());
    let policy = ManagerPolicyV2 {
        mode: ManagerOperatingModeV2::Execute,
        ..Default::default()
    };
    let appointed = call_rpc(&fixture.server, "ConfigureHarnessManagerWithPolicy", serde_json::json!({
        "scope":{"project_id":issue_rpc_project_id(),"session_id":seat,"expected_row_version":0},
        "expected_policy_version":0,"idempotency_key":"host-tuning-seat","policy":policy
    })).await;
    assert!(appointed.error.is_none(), "{:?}", appointed.error);
    let HandleResult::Response(manager) = fixture.server.handle_request_inner(&request).await
    else {
        panic!("response");
    };
    assert!(manager.error.is_none(), "{:?}", manager.error);
    assert_eq!(
        manager.result.unwrap()["view"]["actual"]["host_load_admission_threshold"],
        16
    );
    request.params = serde_json::json!({"action":"apply","name":"day/shared","configuration":{}});
    let HandleResult::Response(raw) = fixture.server.handle_request_inner(&request).await else {
        panic!("response");
    };
    assert!(raw.error.is_some());
    let mut policy = policy;
    policy.paused = true;
    let paused = call_rpc(&fixture.server, "ConfigureHarnessManagerPolicy", serde_json::json!({
        "project_id":issue_rpc_project_id(),"expected_scope_version":1,"expected_policy_version":1,
        "idempotency_key":"host-tuning-paused","policy":policy
    })).await;
    assert!(paused.error.is_none(), "{:?}", paused.error);
    request.params = serde_json::json!({"action":"undo"});
    let HandleResult::Response(paused) = fixture.server.handle_request_inner(&request).await else {
        panic!("response");
    };
    assert!(paused.error.is_some());
}
