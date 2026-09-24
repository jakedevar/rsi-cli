use super::*;
use crate::{
    app_server_control::AppServerControlPlane,
    bus::EventBus,
    codex_app_server::{CodexAppServerSession, route_app_server_message},
    config::{Config, RuntimeConfig},
    model_control::{
        AdmissionDecision, ModelAdmissionRequest, admit_invocation,
        call_control::StoreBackedModelCallControl, registry::RuntimeExecutionRoute,
    },
    store::Store,
};
use rsi_common::{
    harness_manager::{ConfigureHarnessManagerRequestV1, HarnessManagerConfigV1},
    harness_manager_v2::*,
    model_control::{InvocationOwner, ModelInvocationPurpose},
};
use tokio::sync::mpsc;

struct World {
    manager: Arc<SessionManager>,
    config: HarnessManagerConfigV1,
    session: Uuid,
    invocation: Uuid,
    events: mpsc::Sender<crate::claude::StreamEvent>,
    writes: mpsc::Receiver<Vec<u8>>,
    monitor: Option<tokio::task::JoinHandle<()>>,
    stop: mpsc::Sender<()>,
    _dir: tempfile::TempDir,
}

impl World {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("approvals.db")).unwrap();
        let (config, mut lead) = crate::store::pending_approvals::approval_test_fixture(
            &store,
            ManagerPolicyV2::default(),
        );
        lead.provider = SessionProvider::CodexAppServer;
        lead.status = SessionStatus::Running;
        lead.model = Some("gpt-5.4".into());
        lead.working_dir = dir.path().to_path_buf();
        store.conn.execute("UPDATE sessions SET provider='CodexAppServer',status='Running',model='gpt-5.4',working_dir=?2 WHERE id=?1",
            rusqlite::params![lead.id.to_string(),dir.path().display().to_string()]).unwrap();
        let manager = Arc::new(
            SessionManager::new(
                Arc::new(EventBus::new(64)),
                store,
                false,
                dir.path().join("unused.sock"),
                None,
                vec![],
                RuntimeConfig::from_config(&Config::from_env()),
                dir.path().join("sandboxes"),
            )
            .unwrap(),
        );
        let purpose = ModelInvocationPurpose::SessionLaunchFresh;
        let owner = InvocationOwner {
            session_id: Some(lead.id),
            project_id: Some(config.project_id),
            ..Default::default()
        };
        let request = ModelAdmissionRequest {
            purpose,
            provider: Some("CodexAppServer".into()),
            model: lead.model.clone(),
            backend: Some("CodexAppServer".into()),
            effort: None,
            trigger: "approval-controlled-transport".into(),
            owner: owner.clone(),
            dedup_key: Some(format!("approval-root:{}", lead.id)),
            request_fingerprint: Some("sha256:approval-controlled-transport".into()),
            parent_invocation_id: None,
            retry_of_invocation_id: None,
            expected_usage: Some(crate::model_control::explicit_expected_usage(
                purpose,
                Some("CodexAppServer"),
                Some("CodexAppServer"),
                Some("gpt-5.4"),
            )),
            baseline_input_tokens: 0,
            baseline_output_tokens: 0,
            baseline_cache_creation_tokens: 0,
            baseline_cache_read_tokens: 0,
            baseline_reasoning_tokens: 0,
            baseline_embedding_input_count: 0,
            baseline_wall_time_ms: 0,
        };
        let AdmissionDecision::Admitted(permit) =
            admit_invocation(&manager.store, request, &manager.event_bus)
                .await
                .unwrap()
        else {
            panic!("new real admission")
        };
        let invocation = permit.invocation_id();
        manager
            .store
            .lock()
            .await
            .set_session_model_invocation(lead.id, Some(invocation))
            .unwrap();
        let control = Arc::new(StoreBackedModelCallControl::new(
            manager.store.clone(),
            manager.event_bus.clone(),
            manager.model_call_settlements.handle().unwrap(),
            owner,
            "CodexAppServer",
            lead.model.clone(),
            "CodexAppServer",
            None,
            "approval-controlled-transport",
            permit,
            ModelInvocationPurpose::SessionCodexAppServerTurn,
            None,
            RuntimeExecutionRoute::CodexAppServer,
        ));
        let (provider, events, mut writes) =
            CodexAppServerSession::approval_test_transport(control)
                .await
                .unwrap();
        let initial: Value = serde_json::from_slice(&writes.recv().await.unwrap()).unwrap();
        assert_eq!(
            initial["method"], "thread/start",
            "fixture must execute the real Model Control dispatch boundary"
        );
        let (stop, stop_rx) = mpsc::channel(1);
        let mut tracked = TrackedSession::new_for_test(lead.clone());
        tracked.stop_tx = stop.clone();
        manager.active.write().await.insert(lead.id, tracked);
        let m = manager.clone();
        let session = lead.id;
        let monitor = tokio::spawn(async move {
            SessionManager::monitor_session(
                session,
                0,
                Box::new(provider),
                m.active.clone(),
                m.completed.clone(),
                m.event_bus.clone(),
                stop_rx,
                m.store.clone(),
                m.model_call_settlements.handle().unwrap(),
                m.persistence.clone(),
                0,
                false,
                m.socket_path.clone(),
                m.token_counter.clone(),
                None,
                m.retry_tx.clone(),
                m.tool_registry.clone(),
                crate::turn_controller::TurnController::new(
                    crate::turn_controller::ContinuationPolicy::Single,
                ),
                m.runtime_config.clone(),
                m.spawn_coordinator.clone(),
                m.agent_tokens.clone(),
                m.spawn_epoch.clone(),
                m.agent_message_arbiter.clone(),
                m.codegraph_handle.clone(),
                m.custody_execution_runtime(),
            )
            .await;
        });
        Self {
            manager,
            config,
            session,
            invocation,
            events,
            writes,
            monitor: Some(monitor),
            stop,
            _dir: dir,
        }
    }

    fn inject(&self, id: Value, method: &str) {
        self.inject_params(
            id,
            method,
            crate::codex_app_server::approval_protocol_tests::params(),
        );
    }
    fn inject_params(&self, id: Value, method: &str, params: Value) {
        let (responses, _responses) = mpsc::channel(4);
        let (notifications, _notifications) = mpsc::channel(4);
        route_app_server_message(
            json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}),
            &AppServerControlPlane::default(),
            &responses,
            &notifications,
            &self.events,
            &mut None,
        );
    }

    async fn publication(&self, prior: Option<&Value>, state: &str) -> Value {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let row = self.manager.store.lock().await.conn.query_row(
                    "SELECT state,target_json FROM appserver_approval_publications WHERE session_id=?1 ORDER BY created_at DESC,publication_id DESC LIMIT 1",[self.session.to_string()],
                    |r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?))).ok();
                if let Some((actual,raw)) = row {
                    let target:Value = serde_json::from_str(&raw).unwrap();
                    if actual==state && prior.is_none_or(|old| old["publication_id"]!=target["publication_id"]) {return target;}
                }
                tokio::task::yield_now().await;
            }
        }).await.expect("monitor must publish actual approval frame")
    }

    async fn request(&self, key: &str, answer: &str) -> AnswerHarnessManagerDecisionRequestV2 {
        let target: Value = {
            let store = self.manager.store.lock().await;
            let raw:String=store.conn.query_row("SELECT target_json FROM appserver_approval_publications WHERE session_id=?1 ORDER BY created_at DESC,publication_id DESC LIMIT 1",[self.session.to_string()],|r|r.get(0)).unwrap();
            serde_json::from_str(&raw).unwrap()
        };
        self.request_for(&target, key, answer).await
    }

    async fn request_for(
        &self,
        target: &Value,
        key: &str,
        answer: &str,
    ) -> AnswerHarnessManagerDecisionRequestV2 {
        let store = self.manager.store.lock().await;
        store
            .manager_v2_refresh_question_decisions(&self.config)
            .unwrap();
        let key_decision = crate::store::pending_approvals::approval_decision_key(target);
        let row = store
            .manager_v2_record(&self.config, "decision", &key_decision)
            .unwrap()
            .unwrap();
        assert_eq!(row.payload["status"], "pending", "{:?}", row.payload);
        AnswerHarnessManagerDecisionRequestV2 {
            project_id: self.config.project_id,
            fence: ManagerFenceV2 {
                scope_version: self.config.row_version,
                policy_version: 1,
            },
            decision_key: key_decision,
            expected_row_version: row.row_version,
            target_digest: row.payload["target_digest"].as_str().unwrap().into(),
            answer: answer.into(),
            idempotency_key: key.into(),
        }
    }
    async fn queue(&self, key: &str, answer: &str) -> AnswerHarnessManagerDecisionRequestV2 {
        let request = self.request(key, answer).await;
        self.manager
            .answer_harness_manager_decision(request.clone())
            .await
            .unwrap();
        request
    }
    async fn deliveries(&self) -> Vec<ManagerDecisionDeliveryV2> {
        self.manager
            .store
            .lock()
            .await
            .manager_v2_records_of_kind(&self.config, "decision_delivery")
            .unwrap()
            .into_iter()
            .map(|r| serde_json::from_value(r.payload).unwrap())
            .collect()
    }
    async fn response(&mut self, approved: bool, id: impl Into<Value>) {
        let bytes = tokio::time::timeout(Duration::from_secs(5), self.writes.recv())
            .await
            .unwrap()
            .unwrap();
        let id = id.into();
        let method = self.manager.store.lock().await.conn.query_row(
            "SELECT json_extract(target_json,'$.method') FROM appserver_approval_publications WHERE session_id=?1 AND request_id_json=?2 ORDER BY created_at DESC LIMIT 1",rusqlite::params![self.session.to_string(),id.to_string()],|r|r.get::<_,String>(0)).unwrap();
        let response: Value = serde_json::from_slice(&bytes).unwrap();
        crate::codex_app_server::approval_protocol_tests::assert_response(&method, &id, &response);
        assert_eq!(
            response,
            json!({"jsonrpc":"2.0","id":id,"result":{"decision":if approved {"accept"} else {"decline"}}})
        );
    }
    async fn quiesce(&mut self) {
        // Close the real provider ingress, then observe its production monitor
        // and writer lease settle before modeling a new daemon boot.
        self.events = mpsc::channel(1).0;
        let _ = self.stop.send(()).await;
        if let Some(monitor) = self.monitor.take() {
            tokio::time::timeout(Duration::from_secs(10), monitor)
                .await
                .expect("observe monitor termination")
                .unwrap();
        }
    }
    async fn finish(mut self) {
        self.quiesce().await;
    }
}
const METHOD: &str = "item/commandExecution/requestApproval";

#[tokio::test]
async fn appserver_approval_monitor_operator_writer_roundtrip_and_exact_replay_emit_once() {
    let mut w = World::new().await;
    w.inject(json!(71), METHOD);
    let target = w.publication(None, "published").await;
    assert_eq!(target["model_invocation_id"], w.invocation.to_string());
    assert!(target["event_id"].as_i64().unwrap() > 0);
    {
        let store = w.manager.store.lock().await;
        assert_eq!(store.get_pending_approvals(w.session).unwrap().len(), 1);
        assert!(store.manager_action_human_gate(w.session).is_err());
    }
    let request = w.queue("native-approve", "approve").await;
    w.manager.reconcile_harness_managers_once().await.unwrap();
    w.response(true, 71).await;
    let rows = w.deliveries().await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].state, "enqueued");
    assert!(
        rows[0]
            .outcome
            .as_ref()
            .unwrap()
            .contains("consumption unconfirmed")
    );
    assert!(
        w.manager
            .answer_harness_manager_decision(request.clone())
            .await
            .unwrap()
            .deduplicated
    );
    let mut changed = request;
    changed.answer = "deny".into();
    assert!(
        w.manager
            .answer_harness_manager_decision(changed)
            .await
            .is_err()
    );
    w.manager.reconcile_harness_managers_once().await.unwrap();
    assert!(matches!(
        w.writes.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    w.inject(json!(72), "item/fileChange/requestApproval");
    w.publication(Some(&target), "published").await;
    w.queue("native-deny", "deny").await;
    w.manager.reconcile_harness_managers_once().await.unwrap();
    w.response(false, 72).await;
    w.finish().await;
}

#[tokio::test]
async fn appserver_approval_new_same_text_reused_id_and_stale_clear_preserve_latest_gate() {
    let mut w = World::new().await;
    w.inject(json!(81), METHOD);
    let old = w.publication(None, "published").await;
    w.queue("old-answer", "approve").await;
    let claim = w
        .manager
        .store
        .lock()
        .await
        .manager_v2_claim_decision_delivery(&w.config, w.manager.program_run_boot_id)
        .unwrap()
        .unwrap();
    let started = w
        .manager
        .store
        .lock()
        .await
        .manager_v2_set_decision_delivery(&claim, "running", true, None)
        .unwrap();
    w.inject(json!(81), METHOD);
    let new = w.publication(Some(&old), "published").await;
    assert_ne!(old["event_id"], new["event_id"]);
    assert_eq!(old["params"], new["params"]);
    assert!(w.manager.deliver_manager_decision(claim).await.is_err());
    let store = w.manager.store.lock().await;
    assert!(store.finish_appserver_approval_enqueue(&started).is_err());
    assert_eq!(
        store.pending_appserver_approval_target(w.session).unwrap(),
        Some(new)
    );
    assert_eq!(store.get_pending_approvals(w.session).unwrap().len(), 1);
    drop(store);
    assert!(matches!(
        w.writes.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    w.finish().await;
}

#[tokio::test]
async fn appserver_approval_new_unpersisted_frame_invalidates_old_runtime_and_failed_event_gate() {
    let mut w = World::new().await;
    w.inject(json!(91), METHOD);
    let old = w.publication(None, "published").await;
    w.queue("old-before-failure", "approve").await;
    let claim = w
        .manager
        .store
        .lock()
        .await
        .manager_v2_claim_decision_delivery(&w.config, w.manager.program_run_boot_id)
        .unwrap()
        .unwrap();
    // Force the exact publication/event split: the producer updates its runtime
    // witness while FIFO persistence waits for Store ownership.
    let store = w.manager.store.lock().await;
    store.conn.execute_batch("CREATE TRIGGER approval_event_fail BEFORE INSERT ON conversation_events BEGIN SELECT RAISE(ABORT,'controlled event failure'); END;").unwrap();
    w.inject(json!(91), METHOD);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if w.manager
                .active
                .read()
                .await
                .get(&w.session)
                .unwrap()
                .events
                .len()
                >= 2
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        store.pending_appserver_approval_target(w.session).unwrap(),
        Some(old.clone()),
        "old durable identity exists during producer race"
    );
    drop(store);
    assert!(w.manager.deliver_manager_decision(claim).await.is_err());
    let target = w.publication(Some(&old), "unresolved").await;
    assert_eq!(target["event_id"], 0);
    let store = w.manager.store.lock().await;
    assert!(store.pending_appserver_approval_target(w.session).is_err());
    assert!(store.manager_action_human_gate(w.session).is_err());
    store
        .conn
        .execute_batch("DROP TRIGGER approval_event_fail;")
        .unwrap();
    drop(store);
    assert!(matches!(
        w.writes.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    w.finish().await;
}

#[tokio::test]
async fn appserver_approval_wrong_writer_incarnation_refuses_and_keeps_human_gate() {
    let mut w = World::new().await;
    w.inject(json!(101), METHOD);
    let old = w.publication(None, "published").await;
    w.queue("incarnation", "approve").await;
    let claim = w
        .manager
        .store
        .lock()
        .await
        .manager_v2_claim_decision_delivery(&w.config, w.manager.program_run_boot_id)
        .unwrap()
        .unwrap();
    let old_runtime = WRITERS
        .lock()
        .unwrap()
        .get(&w.session)
        .and_then(Weak::upgrade)
        .unwrap();
    let replacement = register_writer(w.session, 1, old_runtime.writer.clone()).unwrap();
    assert_ne!(
        replacement.runtime.incarnation.to_string(),
        old["incarnation_id"]
    );
    assert!(register_writer(w.session, 0, old_runtime.writer.clone()).is_none());
    assert_eq!(
        WRITERS
            .lock()
            .unwrap()
            .get(&w.session)
            .and_then(Weak::upgrade)
            .unwrap()
            .incarnation,
        replacement.runtime.incarnation
    );
    assert!(w.manager.deliver_manager_decision(claim).await.is_err());
    w.manager.reconcile_harness_managers_once().await.unwrap();
    assert!(
        w.manager
            .store
            .lock()
            .await
            .pending_appserver_approval_target(w.session)
            .is_err()
    );
    assert!(matches!(
        w.writes.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    drop(replacement);
    w.finish().await;
}

#[tokio::test]
async fn appserver_approval_revocation_after_claim_prevents_writer_effect() {
    let mut w = World::new().await;
    w.inject(json!(111), METHOD);
    w.publication(None, "published").await;
    w.queue("revoked-answer", "approve").await;
    let claim = w
        .manager
        .store
        .lock()
        .await
        .manager_v2_claim_decision_delivery(&w.config, w.manager.program_run_boot_id)
        .unwrap()
        .unwrap();
    w.manager
        .store
        .lock()
        .await
        .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
            group_ids: Vec::new(),
            project_id: w.config.project_id,
            session_id: w.config.manager_session_id,
            epic_ids: Some(vec![]),
            expected_row_version: w.config.row_version,
        })
        .unwrap();
    assert!(w.manager.deliver_manager_decision(claim).await.is_err());
    assert!(matches!(
        w.writes.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    assert!(
        w.manager
            .store
            .lock()
            .await
            .manager_action_human_gate(w.session)
            .is_err()
    );
    w.finish().await;
}

#[tokio::test]
async fn appserver_approval_post_enqueue_failure_is_uncertain_and_restart_never_resends() {
    let mut w = World::new().await;
    w.inject(json!(121), METHOD);
    w.publication(None, "published").await;
    let request = w.queue("uncertain-answer", "approve").await;
    w.manager.store.lock().await.conn.execute_batch("CREATE TRIGGER approval_finish_fail BEFORE UPDATE ON appserver_approval_publications WHEN NEW.state='enqueued' BEGIN SELECT RAISE(ABORT,'controlled finish failure'); END;").unwrap();
    w.manager.reconcile_harness_managers_once().await.unwrap();
    w.response(true, 121).await;
    assert_eq!(w.deliveries().await[0].state, "uncertain");
    assert!(
        w.manager
            .store
            .lock()
            .await
            .manager_action_human_gate(w.session)
            .is_err()
    );
    w.manager
        .store
        .lock()
        .await
        .conn
        .execute_batch("DROP TRIGGER approval_finish_fail;")
        .unwrap();
    assert!(
        w.manager
            .answer_harness_manager_decision(request)
            .await
            .unwrap()
            .deduplicated
    );
    let store = w.manager.store.lock().await;
    store
        .manager_v2_recover_decision_deliveries(Uuid::new_v4())
        .unwrap();
    assert!(
        store
            .manager_v2_claim_decision_delivery(&w.config, Uuid::new_v4())
            .unwrap()
            .is_none()
    );
    drop(store);
    w.manager.reconcile_harness_managers_once().await.unwrap();
    assert!(matches!(
        w.writes.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    w.finish().await;
}

#[tokio::test]
async fn appserver_approval_writer_closed_before_enqueue_is_blocked_without_effect() {
    let mut w = World::new().await;
    w.inject(json!(131), METHOD);
    w.publication(None, "published").await;
    w.queue("closed-answer", "deny").await;
    w.writes.close();
    w.manager.reconcile_harness_managers_once().await.unwrap();
    let rows = w.deliveries().await;
    assert_eq!(rows[0].state, "blocked");
    assert!(!rows[0].effect_started);
    assert!(
        w.manager
            .store
            .lock()
            .await
            .manager_action_human_gate(w.session)
            .is_err()
    );
    w.finish().await;
}

#[tokio::test]
async fn appserver_approval_reopen_expires_exact_enqueued_writer_and_preserves_delivery_id() {
    let mut w = World::new().await;
    w.inject(json!(141), METHOD);
    let target = w.publication(None, "published").await;
    w.queue("reopen-answer", "approve").await;
    w.manager.reconcile_harness_managers_once().await.unwrap();
    w.response(true, 141).await;
    let delivery = w.deliveries().await.remove(0);
    w.quiesce().await;
    let reopened = Store::open(&w._dir.path().join("approvals.db")).unwrap();
    reopened.expire_appserver_approvals(&[]).unwrap();
    reopened
        .manager_v2_refresh_question_decisions(&w.config)
        .unwrap();
    let decision = reopened
        .manager_v2_record(&w.config, "decision", &delivery.decision_key)
        .unwrap()
        .unwrap();
    assert_eq!(decision.payload["status"], "blocked");
    assert_eq!(decision.payload["delivery"]["delivery_id"], delivery.key);
    assert_eq!(
        decision.payload["provider_request"]["publication_id"],
        target["publication_id"]
    );
    assert!(
        decision.payload["outcome"]
            .as_str()
            .unwrap()
            .contains("consumption unconfirmed")
    );
    assert!(reopened.manager_action_human_gate(w.session).is_err());
    assert!(
        reopened
            .pending_appserver_approval_target(w.session)
            .is_err()
    );
    assert!(
        reopened
            .manager_v2_claim_decision_delivery(&w.config, Uuid::new_v4())
            .unwrap()
            .is_none()
    );
    assert!(matches!(
        w.writes.try_recv(),
        Err(mpsc::error::TryRecvError::Disconnected)
    ));
    drop(reopened);
    w.finish().await;
}

#[tokio::test]
async fn appserver_approval_missing_id_is_visible_unresolved_not_request_zero() {
    let mut w = World::new().await;
    w.inject(Value::Null, METHOD);
    let target = w.publication(None, "unresolved").await;
    assert_eq!(target["request_id"], Value::Null);
    let store = w.manager.store.lock().await;
    store
        .manager_v2_refresh_question_decisions(&w.config)
        .unwrap();
    let row = store
        .manager_v2_record(
            &w.config,
            "decision",
            &crate::store::pending_approvals::approval_decision_key(&target),
        )
        .unwrap()
        .unwrap();
    assert_eq!(row.payload["status"], "blocked");
    assert!(
        row.payload["question"]
            .as_str()
            .unwrap()
            .contains("Approve the same operation")
    );
    assert!(store.manager_action_human_gate(w.session).is_err());
    drop(store);
    assert!(matches!(
        w.writes.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    w.finish().await;
}

#[tokio::test]
async fn appserver_approval_public_delivery_and_actual_invocation_fences_hold_under_spawn_guard() {
    let mut w = World::new().await;
    w.inject(json!(151), METHOD);
    w.publication(None, "published").await;
    w.queue("current-fences", "approve").await;
    let claim = w
        .manager
        .store
        .lock()
        .await
        .manager_v2_claim_decision_delivery(&w.config, w.manager.program_run_boot_id)
        .unwrap()
        .unwrap();
    {
        let store = w.manager.store.lock().await;
        let record = store
            .manager_v2_record(&w.config, "decision", &claim.decision_key)
            .unwrap()
            .unwrap();
        let mut changed = record.payload.clone();
        changed["delivery"]["delivery_id"] = json!(Uuid::new_v4());
        store
            .manager_v2_put_record(
                &w.config,
                "decision",
                &claim.decision_key,
                record.epic_id,
                record.row_version,
                &changed,
            )
            .unwrap();
    }
    assert!(
        w.manager
            .deliver_manager_decision(claim.clone())
            .await
            .is_err()
    );
    {
        let store = w.manager.store.lock().await;
        let record = store
            .manager_v2_record(&w.config, "decision", &claim.decision_key)
            .unwrap()
            .unwrap();
        let mut restored = record.payload;
        restored["delivery"]["delivery_id"] = json!(claim.key);
        store
            .manager_v2_put_record(
                &w.config,
                "decision",
                &claim.decision_key,
                record.epic_id,
                record.row_version,
                &restored,
            )
            .unwrap();
        store.set_session_model_invocation(w.session, None).unwrap();
    }
    assert!(w.manager.deliver_manager_decision(claim).await.is_err());
    assert!(matches!(
        w.writes.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    assert!(
        w.manager
            .store
            .lock()
            .await
            .manager_action_human_gate(w.session)
            .is_err()
    );
    w.finish().await;
}

#[tokio::test]
async fn appserver_approval_queued_restart_loses_writer_and_stays_explicitly_blocked() {
    let mut w = World::new().await;
    w.inject(json!(161), METHOD);
    w.publication(None, "published").await;
    w.queue("queued-restart", "deny").await;
    let delivery = w.deliveries().await.remove(0);
    w.quiesce().await;
    let reopened = Store::open(&w._dir.path().join("approvals.db")).unwrap();
    reopened
        .expire_appserver_approvals(&live_incarnations())
        .unwrap();
    reopened
        .manager_v2_refresh_question_decisions(&w.config)
        .unwrap();
    reopened
        .manager_v2_recover_decision_deliveries(Uuid::new_v4())
        .unwrap();
    assert!(
        reopened
            .manager_v2_claim_decision_delivery(&w.config, Uuid::new_v4())
            .unwrap()
            .is_none()
    );
    let record = reopened
        .manager_v2_record(&w.config, "decision", &delivery.decision_key)
        .unwrap()
        .unwrap();
    assert_eq!(record.payload["status"], "blocked");
    assert_eq!(record.payload["delivery"]["delivery_id"], delivery.key);
    assert_eq!(record.payload["provider_request"]["request_id"], 161);
    assert_eq!(record.payload["route_state"], "unavailable");
    assert!(reopened.manager_action_human_gate(w.session).is_err());
    assert!(matches!(
        w.writes.try_recv(),
        Err(mpsc::error::TryRecvError::Disconnected)
    ));
    drop(reopened);
    w.finish().await;
}

#[tokio::test]
async fn appserver_approval_current_schema_transport_matrix_preserves_numeric_and_string_ids() {
    let mut w = World::new().await;
    let mut prior = None;
    let mut ordinal = 0;
    for method in [
        "item/commandExecution/requestApproval",
        "item/fileChange/requestApproval",
    ] {
        for string_id in [false, true] {
            for approve in [true, false] {
                ordinal += 1;
                let id = if string_id {
                    json!(format!("approval-{ordinal}"))
                } else {
                    json!(200 + ordinal)
                };
                let params = crate::codex_app_server::approval_protocol_tests::params();
                crate::codex_app_server::approval_protocol_tests::assert_request(
                    method, &id, &params,
                );
                w.inject_params(id.clone(), method, params);
                let target = w.publication(prior.as_ref(), "published").await;
                assert_eq!(target["request_id"], id);
                let request = w
                    .queue(
                        &format!("schema-roundtrip-{ordinal}"),
                        if approve { "approve" } else { "deny" },
                    )
                    .await;
                w.manager.reconcile_harness_managers_once().await.unwrap();
                w.response(approve, id).await;
                assert!(
                    w.manager
                        .answer_harness_manager_decision(request)
                        .await
                        .unwrap()
                        .deduplicated
                );
                w.manager.reconcile_harness_managers_once().await.unwrap();
                assert!(matches!(
                    w.writes.try_recv(),
                    Err(mpsc::error::TryRecvError::Empty)
                ));
                prior = Some(target);
            }
        }
    }
    assert_eq!(w.deliveries().await.len(), 8);
    w.finish().await;
}

#[tokio::test]
async fn appserver_approval_offered_choices_gate_operator_admission_and_reused_string_request() {
    let mut w = World::new().await;
    let id = json!("opaque request/α");
    let mut p = crate::codex_app_server::approval_protocol_tests::params();
    p["availableDecisions"] = json!(["decline"]);
    w.inject_params(id.clone(), METHOD, p.clone());
    let first = w.publication(None, "published").await;
    let approve = w.request("unoffered-approve", "approve").await;
    assert!(
        w.manager
            .answer_harness_manager_decision(approve)
            .await
            .unwrap_err()
            .to_string()
            .contains("decision_not_available")
    );
    assert_eq!(w.deliveries().await.len(), 0);
    w.queue("offered-deny", "deny").await;
    w.manager.reconcile_harness_managers_once().await.unwrap();
    w.response(false, id.clone()).await;
    p["availableDecisions"] = json!(["accept"]);
    w.inject_params(id.clone(), METHOD, p.clone());
    let second = w.publication(Some(&first), "published").await;
    w.queue("before-choice-change", "approve").await;
    p["availableDecisions"] = json!(["decline"]);
    w.inject_params(id.clone(), METHOD, p);
    w.publication(Some(&second), "published").await;
    w.manager.reconcile_harness_managers_once().await.unwrap();
    assert!(matches!(
        w.writes.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    w.queue("after-choice-change", "deny").await;
    w.manager.reconcile_harness_managers_once().await.unwrap();
    w.response(false, id).await;
    w.finish().await;
}

#[tokio::test]
async fn appserver_approval_unsupported_protocols_and_unoffered_binary_choices_stay_visible() {
    let mut w = World::new().await;
    let mut prior = None;
    for (index, method) in [
        "item/permissions/requestApproval",
        "item/tool/requestUserInput",
        "mcpServer/elicitation/request",
        "execCommandApproval",
        "applyPatchApproval",
        "item/fileWrite/requestApproval",
    ]
    .into_iter()
    .enumerate()
    {
        let id = json!(format!("unsupported-{index}"));
        w.inject(id.clone(), method);
        let target = w.publication(prior.as_ref(), "unresolved").await;
        let store = w.manager.store.lock().await;
        store
            .manager_v2_refresh_question_decisions(&w.config)
            .unwrap();
        let row = store
            .manager_v2_record(
                &w.config,
                "decision",
                &crate::store::pending_approvals::approval_decision_key(&target),
            )
            .unwrap()
            .unwrap();
        assert_eq!(row.payload["status"], "blocked");
        assert_eq!(row.payload["provider_request"]["request_id"], id);
        assert_eq!(row.payload["provider_request"]["method"], method);
        assert_eq!(row.payload["route_state"], "unavailable");
        assert!(store.manager_action_human_gate(w.session).is_err());
        drop(store);
        prior = Some(target);
    }
    for choices in [json!([]), json!({}), json!(["cancel"])] {
        let mut p = crate::codex_app_server::approval_protocol_tests::params();
        p["availableDecisions"] = choices;
        w.inject_params(json!("unoffered"), METHOD, p);
        prior = Some(w.publication(prior.as_ref(), "unresolved").await);
    }
    assert_eq!(w.deliveries().await.len(), 0);
    assert!(matches!(
        w.writes.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    w.finish().await;
}

#[path = "lifecycle_tests.rs"]
mod lifecycle_tests;
