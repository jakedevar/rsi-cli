//! Post-land review #1272 of #1235: the global seat's launches obey its
//! granted resource limits and every ancestor's (#1274), and lead actions
//! flow down like every other mutation (#1276). On the fixtures of
//! `manager_global_pm_verbs.rs`.

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
use super::*;

/// Re-grant the portfolio's seat over A and B with `policy`; returns the
/// request builder's fence source.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
async fn regrant(g: &Portfolio, policy: ManagerPolicyV2, key: &str) -> GlobalManagerGrantV1 {
    configure_grant(
        &g.p,
        g.seat,
        vec![g.p.project, g.b],
        policy,
        g.grant.grant_version,
        key,
    )
    .await
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
fn control_at(
    grant: &GlobalManagerGrantV1,
    project: Uuid,
    key: &str,
    operation: ManagerActionV2,
) -> AgentManagerControlRequestV2 {
    AgentManagerControlRequestV2 {
        project_id: Some(project),
        fence: ManagerFenceV2 {
            scope_version: grant.grant_version,
            policy_version: grant.grant_version,
        },
        idempotency_key: key.into(),
        operation,
    }
}

/// #1274: in a project with no PM the granted concurrency cap holds, at
/// admission and at the model-call boundary of a new child.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn a_global_launch_obeys_the_granted_concurrency_cap_without_a_pm() {
    let g = portfolio().await;
    let p = &g.p;
    let mut policy = global_policy(p, ManagerOperatingModeV2::Execute);
    policy.max_active_sessions = 1;
    let grant = regrant(&g, policy, "grant-active-1").await;
    live_leaf(p, Uuid::new_v4(), g.b, g.b_epic).await;
    let error = p
        .manager
        .agent_control()
        .agent_manager_control(
            g.seat,
            control_at(&grant, g.b, "g-b-over", create_session(p, g.b_epic)),
        )
        .await
        .unwrap_err();
    assert!(
        code(&error).contains("manager_v2_concurrency_capacity"),
        "{error}"
    );
    let error = p
        .manager
        .store
        .lock()
        .await
        .manager_v2_resource_gate_for_parent(g.b_epic, p.policy.allowed_launches[0].provider)
        .unwrap_err();
    assert!(
        code(&error).contains("manager_v2_concurrency_capacity"),
        "{error}"
    );
}

/// #1274: a positive spend cap the project's work has already exhausted
/// refuses the global's launch in a project with no PM.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn a_global_launch_obeys_an_exhausted_granted_spend_cap() {
    let g = portfolio().await;
    let p = &g.p;
    let mut policy = global_policy(p, ManagerOperatingModeV2::Execute);
    policy.max_spend_usd = Some(1.0);
    let grant = regrant(&g, policy, "grant-spend-1").await;
    {
        let mut row = bare_session(Uuid::new_v4());
        row.project_id = Some(g.b);
        row.working_dir = p.repo.clone();
        row.session_kind = SessionKind::Task;
        row.parent_id = Some(g.b_epic);
        row.status = SessionStatus::Completed;
        row.cost_usd = Some(2.5);
        p.manager.store.lock().await.insert_session(&row).unwrap();
    }
    let error = p
        .manager
        .agent_control()
        .agent_manager_control(
            g.seat,
            control_at(&grant, g.b, "g-b-spend", create_session(p, g.b_epic)),
        )
        .await
        .unwrap_err();
    assert!(
        code(&error).contains("manager_v2_spend_exhausted"),
        "{error}"
    );
}

/// #1274: where a PM exists, a global cap tighter than the PM's governs the
/// global's own launch and, as an ancestor's cap, the PM's launch too.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn a_global_cap_tighter_than_the_pm_cap_governs_both() {
    let g = portfolio().await;
    let p = &g.p;
    // The running global seat is the one active session under A's Epic, so
    // a global cap of one is full while the PM's cap of four is not.
    assert_eq!(p.policy.max_active_sessions, 4);
    let mut policy = global_policy(p, ManagerOperatingModeV2::Execute);
    policy.max_active_sessions = 1;
    let grant = regrant(&g, policy, "grant-tight").await;
    let control = p.manager.agent_control();
    let error = control
        .agent_manager_control(
            g.seat,
            control_at(&grant, p.project, "g-a-over", create_session(p, p.epic)),
        )
        .await
        .unwrap_err();
    assert!(
        code(&error).contains("manager_v2_concurrency_capacity"),
        "{error}"
    );
    let error = control
        .agent_manager_control(
            p.owner,
            pm_control(p, 2, "pm-a-over", create_session(p, p.epic)),
        )
        .await
        .unwrap_err();
    assert!(
        code(&error).contains("manager_v2_concurrency_capacity"),
        "{error}"
    );
}

/// A lead action naming `epic` at its current fence, from the PM.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
async fn pm_pause_lead(p: &Pilot, key: &str) -> Result<ManagerActionReceiptV2> {
    let operation = ManagerActionV2::PauseLead {
        epic_id: p.epic,
        expected: p.fence().await,
        reason: "pause the lead".into(),
    };
    p.manager
        .agent_control()
        .agent_manager_control(p.owner, pm_control(p, 2, key, operation))
        .await
}

/// #1276: the PM may not run a lead action on a lead the global created.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn a_lead_action_on_an_ancestor_owned_lead_is_refused_at_admission() {
    let g = portfolio().await;
    let p = &g.p;
    let by_global = p
        .manager
        .agent_control()
        .agent_manager_control(
            g.seat,
            global_control(&g, Some(p.project), "g-lead", create_session(p, p.epic)),
        )
        .await
        .unwrap()
        .target_session_id
        .unwrap();
    live_leaf(p, by_global, p.project, p.epic).await;
    p.manager
        .store
        .lock()
        .await
        .set_lead_session(p.epic, Some(by_global))
        .unwrap();
    let error = pm_pause_lead(p, "pm-pause-global-lead").await.unwrap_err();
    assert!(
        code(&error).contains(MANAGER_TARGET_OWNED_BY_ANCESTOR),
        "{error}"
    );
    // Assigning the global's worker as lead is a mutation of it too.
    let error = p
        .manager
        .agent_control()
        .agent_manager_control(
            p.owner,
            pm_control(
                p,
                2,
                "pm-assign-global-worker",
                ManagerActionV2::AssignLead {
                    epic_id: p.epic,
                    expected: p.fence().await,
                    session_id: Some(by_global),
                },
            ),
        )
        .await
        .unwrap_err();
    assert!(
        code(&error).contains(MANAGER_TARGET_OWNED_BY_ANCESTOR),
        "{error}"
    );
}

/// #1276: ownership is checked again at effect. A lead that becomes a
/// portfolio seat between admission and effect refuses the admitted action.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn a_lead_action_is_refused_at_effect_when_the_lead_becomes_ancestor_owned() {
    let g = portfolio().await;
    let p = &g.p;
    let admitted = pm_pause_lead(p, "pm-pause-own-lead").await.unwrap();
    // The operator makes the PM's lead the seat of another (disjoint) root.
    p.manager
        .store
        .lock()
        .await
        .configure_portfolio_node(
            &rsi_common::portfolio_nodes::ConfigurePortfolioNodeRequestV1 {
                node_id: None,
                parent_node_id: None,
                adopt_node_ids: Vec::new(),
                expected_parent_grant_version: None,
                tier_label: "global".into(),
                seat_session_id: p.lead,
                project_ids: vec![g.c],
                allowed_launches: p.policy.allowed_launches.clone(),
                policy: global_policy(p, ManagerOperatingModeV2::Execute),
                child_policy: None,
                max_direct_reports: 5,
                expected_node_grant_version: 0,
                expected_authority_epoch: 0,
                idempotency_key: "root-lead-seat".into(),
            },
            crate::store::portfolio_nodes::PortfolioGrantor::Operator,
            "operator:test",
        )
        .unwrap();
    let result = p.execute().await;
    let receipt = p.receipt(admitted.operation_id).await;
    assert_ne!(receipt.state, ManagerActionStateV2::Succeeded);
    let text = format!("{result:?} {}", serde_json::to_string(&receipt).unwrap());
    assert!(text.contains(MANAGER_TARGET_OWNED_BY_ANCESTOR), "{text}");
}
