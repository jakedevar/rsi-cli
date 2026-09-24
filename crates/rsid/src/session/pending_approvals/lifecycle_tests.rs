use super::*;
use crate::store::pending_approvals::approval_decision_key;

impl World {
    fn resolve(&self, id: Value) {
        self.resolve_on_thread(id, "approval-transport-thread");
    }
    fn resolve_on_thread(&self, id: Value, thread: &str) {
        let (responses, _) = mpsc::channel(1);
        let (notifications, _) = mpsc::channel(1);
        assert_eq!(
            route_app_server_message(
                json!({"jsonrpc":"2.0","method":"serverRequest/resolved","params":{"threadId":thread,"requestId":id}}),
                &AppServerControlPlane::default(),
                &responses,
                &notifications,
                &self.events,
                &mut None
            ),
            crate::codex_app_server::IngressOutcome::Continue
        );
    }
    async fn closure(&self, target: &Value, expected: &str) -> Value {
        tokio::time::timeout(Duration::from_secs(10),async {
            loop {
                let (state,raw):(String,Option<String>)=self.manager.store.lock().await.conn.query_row("SELECT closure_state,closure_json FROM appserver_approval_publications WHERE publication_id=?1",[target["publication_id"].as_str().unwrap()],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
                let evidence:Value=raw.map(|s|serde_json::from_str(&s).unwrap()).unwrap_or(Value::Null);
                if state==expected && evidence["event_id"].as_i64().is_some_and(|id|id>0) { return evidence; }
                tokio::task::yield_now().await;
            }
        }).await.expect("exact monitor closure must reach Store")
    }
    async fn decision(&self, target: &Value) -> Value {
        let store = self.manager.store.lock().await;
        store
            .manager_v2_refresh_question_decisions(&self.config)
            .unwrap();
        store
            .manager_v2_record(&self.config, "decision", &approval_decision_key(target))
            .unwrap()
            .unwrap()
            .payload
    }
    async fn assert_actionable(&self, target: &Value) {
        let request = self
            .request_for(target, "inspect-actionable", "approve")
            .await;
        let store = self.manager.store.lock().await;
        let board = store
            .manager_v2_inspect_operator(
                self.config.project_id,
                &AgentManagerInspectRequestV2 {
                    section: ManagerInspectSectionV2::Decisions,
                    limit: 64,
                    ..Default::default()
                },
            )
            .unwrap();
        let row = board
            .rows
            .iter()
            .find(|r| r["key"] == request.decision_key)
            .expect("board retains every exact native decision");
        assert_eq!(row["status"], "pending");
        assert_eq!(row["route_state"], "live_appserver_writer");
        assert_eq!(row["provider_request"], *target);
        assert!(
            row["available_answers"]
                .as_array()
                .unwrap()
                .contains(&json!("approve"))
        );
    }
    async fn answer_exact(
        &mut self,
        target: &Value,
        key: &str,
        approve: bool,
    ) -> AnswerHarnessManagerDecisionRequestV2 {
        let request = self
            .request_for(target, key, if approve { "approve" } else { "deny" })
            .await;
        self.manager
            .answer_harness_manager_decision(request.clone())
            .await
            .unwrap();
        self.manager
            .reconcile_harness_managers_once()
            .await
            .unwrap();
        self.response(approve, target["request_id"].clone()).await;
        request
    }
    async fn assert_status(&self, status: SessionStatus) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let persisted = self
                    .manager
                    .store
                    .lock()
                    .await
                    .get_session(self.session)
                    .unwrap()
                    .unwrap()
                    .status;
                let runtime = self
                    .manager
                    .active
                    .read()
                    .await
                    .get(&self.session)
                    .unwrap()
                    .session
                    .status;
                if persisted == status && runtime == status {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("runtime and persisted approval state agree");
    }
}

#[tokio::test]
async fn appserver_approval_concurrent_occurrences_are_actionable_in_both_orders_and_id_types() {
    for ids in [
        [json!(171), json!(172)],
        [json!("171"), json!("172")],
        [json!(171), json!("171")],
    ] {
        for reverse in [false, true] {
            let mut w = World::new().await;
            w.inject(ids[0].clone(), METHOD);
            let a = w.publication(None, "published").await;
            w.inject(ids[1].clone(), "item/fileChange/requestApproval");
            let b = w.publication(Some(&a), "published").await;
            assert_ne!(approval_decision_key(&a), approval_decision_key(&b));
            w.assert_actionable(&a).await;
            w.assert_actionable(&b).await;
            let (first, second) = if reverse { (&b, &a) } else { (&a, &b) };
            w.answer_exact(first, "first", false).await;
            w.assert_actionable(second).await;
            w.assert_status(SessionStatus::WaitingApproval).await;
            w.resolve(first["request_id"].clone());
            w.closure(first, "closed").await;
            w.assert_actionable(second).await;
            w.assert_status(SessionStatus::WaitingApproval).await;
            w.answer_exact(second, "second", true).await;
            w.assert_status(SessionStatus::WaitingApproval).await;
            w.resolve(second["request_id"].clone());
            w.closure(second, "closed").await;
            w.assert_status(SessionStatus::Running).await;
            assert!(
                w.manager
                    .store
                    .lock()
                    .await
                    .get_pending_approvals(w.session)
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(w.deliveries().await.len(), 2);
            for delivery in w.deliveries().await {
                assert_eq!(delivery.state, "enqueued");
                assert!(
                    delivery
                        .outcome
                        .unwrap()
                        .contains("consumption unconfirmed")
                );
            }
            w.finish().await;
        }
    }
}

#[tokio::test]
async fn appserver_approval_closure_before_answer_invalidates_exact_gate_without_fabricating_acceptance()
 {
    let mut w = World::new().await;
    w.inject(json!(201), METHOD);
    let target = w.publication(None, "published").await;
    let stale = w.request_for(&target, "before-closure", "approve").await;
    w.resolve(json!(201));
    let evidence = w.closure(&target, "closed").await;
    assert_eq!(evidence["notification"]["requestId"], 201);
    assert_eq!(evidence["publication_id"], target["publication_id"]);
    let decision = w.decision(&target).await;
    assert_eq!(decision["status"], "resolved");
    assert!(decision["delivery"].is_null());
    assert!(
        w.manager
            .answer_harness_manager_decision(stale)
            .await
            .is_err()
    );
    assert_eq!(w.deliveries().await.len(), 0);
    let status: String = w
        .manager
        .store
        .lock()
        .await
        .conn
        .query_row(
            "SELECT status FROM approvals WHERE id=?1",
            [target["publication_id"].as_str().unwrap()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        status, "Pending",
        "provider clearing a request is not an operator approve or deny"
    );
    w.assert_status(SessionStatus::Running).await;
    assert!(matches!(
        w.writes.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    w.finish().await;
}

#[tokio::test]
async fn appserver_approval_closure_while_answer_queued_prevents_effect_and_preserves_other_gate() {
    let mut w = World::new().await;
    w.inject(json!(211), METHOD);
    let a = w.publication(None, "published").await;
    let request = w.request_for(&a, "queued-before-close", "approve").await;
    w.manager
        .answer_harness_manager_decision(request.clone())
        .await
        .unwrap();
    w.inject(json!(212), METHOD);
    let b = w.publication(Some(&a), "published").await;
    w.resolve(json!(211));
    w.closure(&a, "closed").await;
    w.manager.reconcile_harness_managers_once().await.unwrap();
    assert!(w.deliveries().await.iter().all(|d| !d.effect_started));
    assert_eq!(w.decision(&a).await["status"], "resolved");
    w.assert_actionable(&b).await;
    assert!(matches!(
        w.writes.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    w.answer_exact(&b, "other-gate", false).await;
    assert!(
        w.manager
            .answer_harness_manager_decision(request)
            .await
            .unwrap()
            .deduplicated
    );
    w.finish().await;
}

#[tokio::test]
async fn appserver_approval_resolved_after_uncertain_enqueue_keeps_uncertainty_across_reopen() {
    let mut w = World::new().await;
    w.inject(json!(221), METHOD);
    let target = w.publication(None, "published").await;
    let request = w.queue("uncertain-closed", "approve").await;
    w.manager.store.lock().await.conn.execute_batch("CREATE TRIGGER close_uncertain BEFORE UPDATE ON appserver_approval_publications WHEN NEW.state='enqueued' BEGIN SELECT RAISE(ABORT,'controlled finish failure'); END;").unwrap();
    w.manager.reconcile_harness_managers_once().await.unwrap();
    w.response(true, 221).await;
    let delivery = w.deliveries().await.remove(0);
    assert_eq!(delivery.state, "uncertain");
    w.manager
        .store
        .lock()
        .await
        .conn
        .execute_batch("DROP TRIGGER close_uncertain;")
        .unwrap();
    w.resolve(json!(221));
    w.closure(&target, "closed").await;
    assert_eq!(w.deliveries().await.remove(0).state, "uncertain");
    assert_eq!(
        w.decision(&target).await["delivery"]["delivery_id"],
        delivery.key
    );
    w.quiesce().await;
    let reopened = Store::open(&w._dir.path().join("approvals.db")).unwrap();
    reopened.expire_appserver_approvals(&[]).unwrap();
    reopened
        .manager_v2_refresh_question_decisions(&w.config)
        .unwrap();
    let row = reopened
        .manager_v2_record(&w.config, "decision", &request.decision_key)
        .unwrap()
        .unwrap();
    assert_eq!(row.payload["status"], "resolved");
    assert_eq!(row.payload["delivery"]["state"], "uncertain");
    assert!(
        reopened
            .manager_v2_claim_decision_delivery(&w.config, Uuid::new_v4())
            .unwrap()
            .is_none()
    );
    drop(reopened);
    w.finish().await;
}

#[tokio::test]
async fn appserver_approval_ordered_close_then_same_id_new_occurrence_retains_ambiguous_late_closure()
 {
    let mut w = World::new().await;
    w.inject(json!("reuse"), METHOD);
    let old = w.publication(None, "published").await;
    let stale = w.request_for(&old, "old-occurrence", "approve").await;
    // Both frames traverse the production router without awaiting persistence.
    w.resolve(json!("reuse"));
    w.inject(json!("reuse"), METHOD);
    let new = w.publication(Some(&old), "published").await;
    w.closure(&old, "closed").await;
    w.assert_actionable(&new).await;
    assert!(
        w.manager
            .answer_harness_manager_decision(stale)
            .await
            .is_err()
    );
    w.resolve(json!("reuse"));
    let evidence = w.closure(&new, "ambiguous").await;
    assert_eq!(evidence["request_id_reused"], true);
    assert_eq!(w.decision(&new).await["status"], "blocked");
    assert!(
        w.manager
            .store
            .lock()
            .await
            .manager_action_human_gate(w.session)
            .is_err()
    );
    assert!(matches!(
        w.writes.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    w.finish().await;
}

#[tokio::test]
async fn appserver_approval_closure_failure_retains_witness_and_retries_only_persistence() {
    let mut w = World::new().await;
    w.inject(json!(231), METHOD);
    let target = w.publication(None, "published").await;
    let stale = w.request_for(&target, "closure-race", "approve").await;
    w.manager.store.lock().await.conn.execute_batch("CREATE TRIGGER approval_closure_event_fail BEFORE INSERT ON conversation_events WHEN NEW.tool_name='serverRequest/resolved' BEGIN SELECT RAISE(ABORT,'controlled closure failure'); END;").unwrap();
    w.resolve(json!(231));
    tokio::time::timeout(Duration::from_secs(5),async {loop {let state:String=w.manager.store.lock().await.conn.query_row("SELECT closure_state FROM appserver_approval_publications WHERE publication_id=?1",[target["publication_id"].as_str().unwrap()],|r|r.get(0)).unwrap();if state=="ambiguous" {break;}tokio::task::yield_now().await;}}).await.unwrap();
    assert_eq!(w.decision(&target).await["status"], "blocked");
    assert!(
        w.manager
            .answer_harness_manager_decision(stale)
            .await
            .is_err()
    );
    w.manager
        .store
        .lock()
        .await
        .conn
        .execute_batch("DROP TRIGGER approval_closure_event_fail;")
        .unwrap();
    w.manager.reconcile_harness_managers_once().await.unwrap();
    w.closure(&target, "closed").await;
    w.assert_status(SessionStatus::Running).await;
    assert_eq!(w.deliveries().await.len(), 0);
    assert!(matches!(
        w.writes.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    w.finish().await;
}

#[tokio::test]
async fn appserver_approval_concurrent_reopen_preserves_each_identity_as_unavailable_without_writer()
 {
    let mut w = World::new().await;
    w.inject(json!(241), METHOD);
    let a = w.publication(None, "published").await;
    w.inject(json!("241"), METHOD);
    let b = w.publication(Some(&a), "published").await;
    w.assert_actionable(&a).await;
    w.assert_actionable(&b).await;
    w.quiesce().await;
    let reopened = Store::open(&w._dir.path().join("approvals.db")).unwrap();
    reopened.expire_appserver_approvals(&[]).unwrap();
    reopened
        .manager_v2_refresh_question_decisions(&w.config)
        .unwrap();
    for target in [&a, &b] {
        let row = reopened
            .manager_v2_record(&w.config, "decision", &approval_decision_key(target))
            .unwrap()
            .unwrap();
        assert_eq!(row.payload["provider_request"], *target);
        assert_eq!(row.payload["status"], "blocked");
        assert_eq!(row.payload["route_state"], "unavailable");
    }
    assert_eq!(reopened.get_pending_approvals(w.session).unwrap().len(), 2);
    assert!(reopened.manager_action_human_gate(w.session).is_err());
    drop(reopened);
    w.finish().await;
}

impl World {
    async fn restart_writer(&mut self) {
        self.quiesce().await;
        let manager = self.manager.clone();
        let config = self.config.clone();
        let mut lead = manager
            .store
            .lock()
            .await
            .get_session(self.session)
            .unwrap()
            .unwrap();
        lead.status = SessionStatus::Running;
        let start_sequence: i32 = manager
            .store
            .lock()
            .await
            .conn
            .query_row(
                "SELECT coalesce(max(sequence),0) FROM conversation_events WHERE session_id=?1",
                [lead.id.to_string()],
                |r| r.get(0),
            )
            .unwrap();
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
            dedup_key: Some(format!("approval-restart:{}:{}", lead.id, Uuid::new_v4())),
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
        tracked.spawn_generation = 1;
        manager.active.write().await.insert(lead.id, tracked);
        let m = manager.clone();
        let session = lead.id;
        let monitor = tokio::spawn(async move {
            SessionManager::monitor_session(
                session,
                1,
                Box::new(provider),
                m.active.clone(),
                m.completed.clone(),
                m.event_bus.clone(),
                stop_rx,
                m.store.clone(),
                m.model_call_settlements.handle().unwrap(),
                m.persistence.clone(),
                start_sequence,
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

        self.invocation = invocation;
        self.events = events;
        self.writes = writes;
        self.monitor = Some(monitor);
        self.stop = stop;
    }
}

#[tokio::test]
async fn appserver_approval_new_writer_same_id_fences_old_lease_and_old_operator_answer() {
    let mut w = World::new().await;
    w.inject(json!(251), METHOD);
    let old = w.publication(None, "published").await;
    let stale = w.request_for(&old, "old-writer-answer", "approve").await;
    let old_runtime = WRITERS
        .lock()
        .unwrap()
        .get(&w.session)
        .and_then(Weak::upgrade)
        .unwrap();
    w.restart_writer().await;
    w.inject(json!(251), METHOD);
    let new = w.publication(Some(&old), "published").await;
    assert_ne!(new["incarnation_id"], old["incarnation_id"]);
    w.assert_actionable(&new).await;
    let old_lease = ApprovalLease {
        session: w.session,
        runtime: old_runtime,
    };
    let mut sequence = 1000;
    assert!(
        resolve_monitor_approval(
            Some(&old_lease),
            &w.manager.active,
            &w.manager.persistence,
            w.session,
            0,
            &mut sequence,
            &json!({"threadId":"approval-transport-thread","requestId":251})
        )
        .await
        .is_err()
    );
    drop(old_lease);
    assert!(
        w.manager
            .answer_harness_manager_decision(stale)
            .await
            .is_err()
    );
    w.assert_actionable(&new).await;
    w.answer_exact(&new, "new-writer-answer", true).await;
    w.resolve(json!(251));
    let evidence = w.closure(&new, "closed").await;
    assert_eq!(
        evidence["request_id_reused"], false,
        "reuse in a different proven incarnation is independent"
    );
    w.finish().await;
}

#[tokio::test]
async fn appserver_approval_failed_closure_persistence_reopen_retains_exact_uncertainty() {
    let mut w = World::new().await;
    w.inject(json!(261), METHOD);
    let target = w.publication(None, "published").await;
    let queued = w
        .request_for(&target, "queued-during-closure-failure", "approve")
        .await;
    w.manager
        .answer_harness_manager_decision(queued)
        .await
        .unwrap();
    w.manager.store.lock().await.conn.execute_batch("CREATE TRIGGER closure_reopen_fail BEFORE INSERT ON conversation_events WHEN NEW.tool_name='serverRequest/resolved' BEGIN SELECT RAISE(ABORT,'closure persistence unavailable'); END;").unwrap();
    w.resolve(json!(261));
    tokio::time::timeout(Duration::from_secs(5),async {loop {let state:String=w.manager.store.lock().await.conn.query_row("SELECT closure_state FROM appserver_approval_publications WHERE publication_id=?1",[target["publication_id"].as_str().unwrap()],|r|r.get(0)).unwrap();if state=="ambiguous" {break;}tokio::task::yield_now().await;}}).await.unwrap();
    w.quiesce().await;
    let reopened = Store::open(&w._dir.path().join("approvals.db")).unwrap();
    reopened.expire_appserver_approvals(&[]).unwrap();
    reopened
        .manager_v2_refresh_question_decisions(&w.config)
        .unwrap();
    let row = reopened
        .manager_v2_record(&w.config, "decision", &approval_decision_key(&target))
        .unwrap()
        .unwrap();
    assert_eq!(row.payload["status"], "blocked");
    assert_eq!(row.payload["closure_state"], "ambiguous");
    assert_eq!(
        row.payload["closure_evidence"]["publication_id"],
        target["publication_id"]
    );
    assert_eq!(row.payload["closure_evidence"]["event_id"], 0);
    assert!(row.payload["delivery"]["delivery_id"].is_string());
    assert!(reopened.manager_action_human_gate(w.session).is_err());
    drop(reopened);
    w.finish().await;
}

#[tokio::test]
async fn appserver_approval_wrong_thread_and_duplicate_resolution_leave_other_requests_actionable()
{
    let mut w = World::new().await;
    w.inject(json!(271), METHOD);
    let a = w.publication(None, "published").await;
    w.resolve_on_thread(json!(271), "different-thread");
    w.inject(json!(272), METHOD);
    let b = w.publication(Some(&a), "published").await;
    w.assert_actionable(&a).await;
    w.assert_actionable(&b).await;
    w.resolve(json!(271));
    let closure = w.closure(&a, "closed").await;
    w.resolve(json!(271));
    w.inject(json!(273), METHOD);
    let c = w.publication(Some(&b), "published").await;
    assert_eq!(w.closure(&a, "closed").await, closure);
    w.assert_actionable(&b).await;
    w.assert_actionable(&c).await;
    let before = w
        .manager
        .store
        .lock()
        .await
        .manager_v2_refresh_approval_decisions(&w.config)
        .unwrap();
    let after = w
        .manager
        .store
        .lock()
        .await
        .manager_v2_refresh_approval_decisions(&w.config)
        .unwrap();
    assert_eq!(
        (before, after),
        (0, 0),
        "stable closure and active projections generate no duplicate semantic updates"
    );
    w.finish().await;
}

#[tokio::test]
async fn appserver_approval_active_limit_retains_all_gates_and_explicit_overflow_before_checked_stop()
 {
    let mut w = World::new().await;
    let mut targets = Vec::new();
    for id in 0..crate::store::pending_approvals::MAX_PENDING_APPROVALS {
        w.inject(json!(id as i64), METHOD);
        let target = w.publication(targets.last(), "published").await;
        targets.push(target);
    }
    for target in &targets {
        let request = w.request_for(target, "bound-inspection", "approve").await;
        assert_eq!(
            request.target_digest,
            crate::store::harness_manager_v2::fingerprint(target).unwrap()
        );
    }
    let runtime = WRITERS
        .lock()
        .unwrap()
        .get(&w.session)
        .and_then(Weak::upgrade)
        .unwrap();
    assert_eq!(runtime.pending.lock().await.len(), 64);
    w.inject(json!(999), METHOD);
    let overflow = w.publication(targets.last(), "unresolved").await;
    assert_eq!(overflow["overflow"], true);
    assert_eq!(
        runtime.pending.lock().await.len(),
        64,
        "no active witness was evicted to admit overflow"
    );
    tokio::time::timeout(Duration::from_secs(10), w.monitor.take().unwrap())
        .await
        .expect("capacity failure owns checked monitor settlement")
        .unwrap();
    w.manager.reconcile_harness_managers_once().await.unwrap();
    let store = w.manager.store.lock().await;
    let count: i64 = store
        .conn
        .query_row(
            "SELECT count(*) FROM appserver_approval_publications WHERE session_id=?1",
            [w.session.to_string()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        count, 65,
        "all 64 prior gates and the bounded explicit overflow record survive"
    );
    for target in &targets {
        let raw: String = store
            .conn
            .query_row(
                "SELECT target_json FROM appserver_approval_publications WHERE publication_id=?1",
                [target["publication_id"].as_str().unwrap()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(serde_json::from_str::<Value>(&raw).unwrap(), *target);
    }
    assert!(store.manager_action_human_gate(w.session).is_err());
    let terminal = store.get_session(w.session).unwrap().unwrap();
    assert_eq!(terminal.status, SessionStatus::Failed);
    drop(store);
    assert!(!runtime.live.load(Ordering::SeqCst));
    assert!(matches!(
        w.writes.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    w.finish().await;
}

#[test]
fn appserver_approval_resolution_queue_overflow_is_loss_sensitive_and_preserves_prior_frame() {
    let (events, mut rx) = mpsc::channel(1);
    let (responses, _) = mpsc::channel(1);
    let (notifications, _) = mpsc::channel(1);
    let params = crate::codex_app_server::approval_protocol_tests::params();
    let plane = AppServerControlPlane::default();
    assert_eq!(
        route_app_server_message(
            json!({"id":281,"method":METHOD,"params":params}),
            &plane,
            &responses,
            &notifications,
            &events,
            &mut None
        ),
        crate::codex_app_server::IngressOutcome::Continue
    );
    assert_eq!(
        route_app_server_message(
            json!({"method":"serverRequest/resolved","params":{"threadId":"approval-transport-thread","requestId":281}}),
            &plane,
            &responses,
            &notifications,
            &events,
            &mut None
        ),
        crate::codex_app_server::IngressOutcome::TerminateIngress
    );
    assert_eq!(rx.try_recv().unwrap().data["request_id"], 281);
}
