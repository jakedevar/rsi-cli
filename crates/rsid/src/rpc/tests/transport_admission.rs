//! #1711 copied resolution, commit ordering and structural adopted refusal.
use super::*;
use crate::session::rpc_admission::{RPC_ADMISSION, RpcAdmission};
use rsi_common::types::SessionKind;
use std::time::Duration;

async fn fixture() -> (RecursiveDagRpcFixture, Uuid, Uuid, String) {
    let fixture = recursive_dag_rpc_fixture();
    let caller = Uuid::new_v4();
    let child = Uuid::new_v4();
    {
        let store = fixture.manager.store().lock().await;
        store
            .insert_session(&mk_agent_test_session(
                caller,
                SessionKind::Task,
                None,
                None,
            ))
            .unwrap();
        store
            .insert_session(&mk_agent_test_session(
                child,
                SessionKind::Task,
                Some(caller),
                None,
            ))
            .unwrap();
    }
    let token = crate::test_support::agent_token("rpc-admission-fixture");
    fixture
        .manager
        .register_agent_token(token.clone(), caller)
        .await;
    (fixture, caller, child, token)
}

async fn capture(f: &RecursiveDagRpcFixture, token: &str) -> RpcAdmission {
    RpcAdmission {
        caller: f.manager.resolve_bound_agent_caller(token).await.unwrap(),
        registry: Arc::clone(&f.manager.agent_tokens),
    }
}

fn request(method: &str, params: serde_json::Value, token: &str) -> RpcRequest {
    let mut request = RpcRequest::new(method, params);
    request.session_token = Some(token.to_owned().into());
    request
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn unresolved_bearer_can_read_unscoped_health_but_not_scoped_session() {
    let (f, caller, _, revoked_token) = fixture().await;
    f.manager.agent_tokens.write().await.revoke_session(caller);
    let unregistered_token = crate::test_support::agent_token("unregistered-rpc-admission");

    for token in [&unregistered_token, &revoked_token] {
        let health_request = request("GetHealthStatus", serde_json::Value::Null, token);
        let HandleResult::Response(health) = f.server.handle_request_inner(&health_request).await
        else {
            panic!("health returns a finite response");
        };
        assert!(health.error.is_none(), "{:?}", health.error);
        let health: rsi_common::rpc::HealthStatusResponse =
            serde_json::from_value(health.result.unwrap()).unwrap();
        assert_eq!(health.daemon_pid, Some(std::process::id()));

        let scoped_request = request(
            "GetSession",
            serde_json::json!({"session_id": Uuid::new_v4()}),
            token,
        );
        let HandleResult::Response(scoped) = f.server.handle_request_inner(&scoped_request).await
        else {
            panic!("scoped refusal is a finite response");
        };
        assert!(
            scoped
                .error
                .unwrap()
                .message
                .contains("agent_verb_unknown_session_token")
        );
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn rpc_admission_revoke_and_same_bearer_remint_win_store_contention() {
    let (f, caller, child, token) = fixture().await;
    let wake = operator_terminal_watch(caller, true);
    f.manager
        .store()
        .lock()
        .await
        .insert_scheduled_job(&wake)
        .unwrap();
    // Each real handler waits for Store after the copied resolution. Revoke
    // must obtain A6 while that caller waits, then the admission must refuse.
    for remint in [false, true] {
        for method in [
            "GetSession",
            "AgentGetAuthorityCatalog",
            "AgentCancelWake",
            "AgentSendMessage",
        ] {
            f.manager.register_agent_token(token.clone(), caller).await;
            let admission = capture(&f, &token).await;
            let params = match method {
                "GetSession" => serde_json::json!({"session_id": caller}),
                "AgentCancelWake" => serde_json::json!({"job_id": wake.id}),
                "AgentSendMessage" => {
                    serde_json::json!({"target_session_id":child,"message":"bounded mail","idempotency_key":"race-mail"})
                }
                _ => serde_json::json!({}),
            };
            let request = request(method, params, &token);
            let store = f.manager.store().lock().await;
            let (resolved_tx, resolved_rx) = tokio::sync::oneshot::channel();
            let call = RPC_ADMISSION.scope(Some(admission), async {
                assert_eq!(
                    f.server
                        .resolve_bound_caller(&request)
                        .await
                        .unwrap()
                        .session_id(),
                    caller
                );
                resolved_tx.send(()).unwrap();
                match method {
                    "GetSession" => f.server.handle_get_session(&request).await,
                    "AgentGetAuthorityCatalog" => {
                        f.server.handle_agent_get_authority_catalog(&request).await
                    }
                    "AgentCancelWake" => f.server.handle_agent_cancel_wake(&request).await,
                    "AgentSendMessage" => f.server.handle_agent_send_message(&request).await,
                    _ => unreachable!(),
                }
            });
            let revoke = async {
                resolved_rx.await.unwrap();
                let mut registry =
                    tokio::time::timeout(Duration::from_secs(2), f.manager.agent_tokens.write())
                        .await
                        .expect("resolution must release A6 before Store contention");
                if remint {
                    registry.insert(token.clone(), caller);
                } else {
                    registry.revoke_session(caller);
                }
                drop(registry);
                drop(store);
            };
            let (result, ()) =
                tokio::time::timeout(Duration::from_secs(5), async { tokio::join!(call, revoke) })
                    .await
                    .expect("admission ordering must not deadlock");
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("agent_verb_unknown_session_token"),
                "{method}"
            );
        }
    }
    let store = f.manager.store().lock().await;
    assert!(store.get_scheduled_job(&wake.id).unwrap().unwrap().enabled);
    let messages: i64 = store
        .conn
        .query_row("SELECT COUNT(*) FROM agent_messages", [], |row| row.get(0))
        .unwrap();
    assert_eq!(messages, 0);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn rpc_admission_committed_effect_survives_later_revoke_and_scope_does_not_escape() {
    let (f, caller, child, token) = fixture().await;
    let admission = capture(&f, &token).await;
    RPC_ADMISSION.scope(Some(admission.clone()), async {
        assert!(crate::session::rpc_admission::current().is_some());
        assert!(tokio::spawn(async {crate::session::rpc_admission::current().is_none()}).await.unwrap());
        let request = request("AgentSendMessage", serde_json::json!({"target_session_id":child,"message":"admitted first","idempotency_key":"committed-mail"}), &token);
        let result = f.server.handle_agent_send_message(&request).await.unwrap();
        assert_eq!(result["state"], "queued");
        let store = f.manager.store().lock().await;
        let registry = admission.admit(caller).await.unwrap();
        assert!(f.manager.agent_tokens.try_write().is_err(), "commit guard serializes revoke");
        let messages: i64 = store.conn.query_row("SELECT COUNT(*) FROM agent_messages", [], |row| row.get(0)).unwrap();
        assert_eq!(messages, 1);
        drop(registry);
        drop(store);
    }).await;
    assert!(crate::session::rpc_admission::current().is_none());
    f.manager.agent_tokens.write().await.revoke_session(caller);
    let messages: i64 = f
        .manager
        .store()
        .lock()
        .await
        .conn
        .query_row("SELECT COUNT(*) FROM agent_messages", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        messages, 1,
        "revocation cannot replay or undo the durable admitted intent"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn rpc_admission_adopted_callers_cannot_enter_any_legacy_agent_read_or_hook_route() {
    let (f, caller, _, token) = fixture().await;
    f.manager
        .agent_tokens
        .write()
        .await
        .mark_adopted_for_test(caller);
    assert!(f.manager.resolve_bound_agent_caller(&token).await.is_some());
    assert_eq!(f.manager.resolve_agent_token(&token).await, None);
    // Enumerate scoped catalog routes so newly added/unconverted routes fail
    // closed, including hooks that never used the UUID resolver. Unscoped reads
    // carry no caller authority and remain available to adopted bearers.
    for method in agent_gate::AGENT_VERBS
        .iter()
        .chain(agent_gate::READ_VERBS.iter())
        .chain(agent_gate::HOOK_VERBS.iter())
        .filter(|method| !agent_gate::UNSCOPED_READ_VERBS.contains(method))
    {
        let request = request(method, serde_json::json!({}), &token);
        assert!(
            f.server
                .resolve_caller_session_id(&request)
                .await
                .unwrap_err()
                .to_string()
                .contains("agent_verb_unknown_session_token")
        );
        let HandleResult::Response(response) = f.server.handle_request_inner(&request).await else {
            panic!("finite refusal")
        };
        if matches!(
            *method,
            "AgentListIssues"
                | "AgentGetIssue"
                | "AgentUpdateIssue"
                | "AgentUpdateIssueStatus"
                | "AgentArchiveIssue"
                | "AgentRestoreIssue"
                | "AgentListIssueEvents"
        ) {
            assert_agent_issue_error_code(&response, "authority_denied");
            assert_eq!(
                response.error.unwrap().data.unwrap(),
                serde_json::to_value(crate::error::agent_issue_error_envelope(
                    rsi_common::rpc::AgentIssueErrorCodeV1::AuthorityDenied,
                    None,
                    None,
                ))
                .unwrap(),
                "{method}: adopted callers receive only a safe authority refusal",
            );
        } else if matches!(
            *method,
            "AgentTopologyUpsert"
                | "AgentTopologyList"
                | "AgentTopologyGet"
                | "AgentTopologyExecute"
                | "AgentTopologyGetExecution"
                | "AgentTopologyInterrupt"
                | "AgentTopologyResolveAttempt"
        ) {
            let error = response.error.expect("adopted topology caller refused");
            assert_eq!(error.code, INVALID_PARAMS, "{method}");
            let DaemonError::StructuredRpc { data, .. } =
                crate::session::topology_agent_verbs::unattributed()
            else {
                panic!("typed topology authority envelope");
            };
            assert_eq!(error.data, Some(data), "{method}: safe authority envelope");
        } else {
            assert_eq!(
                response.error.unwrap().message,
                "Invalid parameter: agent_verb_unknown_session_token",
                "{method}"
            );
        }
    }
    // Fresh establishment keeps existing reads/callers working.
    f.manager.register_agent_token(token.clone(), caller).await;
    let request = request(
        "GetSession",
        serde_json::json!({"session_id":caller}),
        &token,
    );
    let HandleResult::Response(response) = f.server.handle_request_inner(&request).await else {
        panic!("finite snapshot")
    };
    assert_eq!(response.result.unwrap()["id"], caller.to_string());
}
