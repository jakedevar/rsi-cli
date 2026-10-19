//! #1240 (hierarchy S6): the node workspace, `AgentManagerOverview` and the
//! v0 shims, asserted against the stated intent.

use std::path::PathBuf;
use std::sync::Arc;

use super::*;
use crate::session::agent_verbs::tests::{control_handle_with_store, test_session};
use crate::store::Store;
use crate::store::portfolio_nodes::PortfolioGrantor;
use rsi_common::agent_control_schema::AgentControlVerbV1 as Verb;
use rsi_common::global_manager::{
    AgentGlobalOverviewRequestV1, ConfigureGlobalManagerRequestV1, RevokeGlobalManagerRequestV1,
};
use rsi_common::harness_manager::ConfigureHarnessManagerRequestV1;
use rsi_common::harness_manager_v2::{
    ManagerCapabilityV2, ManagerLaunchChoiceV2, ManagerOperatingModeV2, ManagerPolicyV2,
};
use rsi_common::portfolio_nodes::ConfigurePortfolioNodeRequestV1;
use rsi_common::types::{Project, SessionKind, SessionProvider, SessionStatus};

type SharedStore = Arc<tokio::sync::Mutex<Store>>;

fn launch() -> ManagerLaunchChoiceV2 {
    ManagerLaunchChoiceV2 {
        provider: SessionProvider::Claude,
        model: "claude-opus-5-5".into(),
        effort: Some("high".into()),
    }
}

/// Each finite allowance under a quarter of its parent's per level, so
/// siblings fit under their parent; capabilities stay equal.
fn tier_policy(level: u16) -> ManagerPolicyV2 {
    ManagerPolicyV2 {
        mode: ManagerOperatingModeV2::Execute,
        capabilities: vec![ManagerCapabilityV2::WorkPlan],
        max_created_containers: (64 >> (2 * level)) - 1,
        max_created_sessions: (128 >> (2 * level)) - 1,
        max_active_sessions: (64 >> (2 * level)) - 1,
        ..ManagerPolicyV2::default()
    }
}

fn add_project(store: &Store, name: &str) -> Uuid {
    let id = Uuid::new_v4();
    let now = chrono::Utc::now();
    store
        .insert_project(&Project {
            id,
            name: name.into(),
            path: None,
            description: None,
            color: Project::DEFAULT_COLOR.into(),
            context_files: None,
            created_at: now,
            updated_at: now,
        })
        .unwrap();
    id
}

fn add_session(store: &Store, project: Option<Uuid>) -> Uuid {
    let id = Uuid::new_v4();
    let mut row = test_session(id, PathBuf::from("/tmp/node-overview"));
    row.project_id = project;
    row.session_kind = SessionKind::Standard;
    row.status = SessionStatus::Running;
    store.insert_session(&row).unwrap();
    id
}

fn appoint(store: &Store, project: Uuid, session: Uuid) {
    store
        .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
            project_id: project,
            session_id: session,
            epic_ids: None,
            group_ids: vec![],
            expected_row_version: 0,
        })
        .unwrap();
}

fn node(store: &Store, parent: Option<Uuid>, label: &str, seat: Uuid, projects: &[Uuid]) -> Uuid {
    let parent_version = parent.map(|id| {
        store
            .get_portfolio_node(id)
            .unwrap()
            .unwrap()
            .grant
            .grant_version
    });
    store
        .configure_portfolio_node(
            &ConfigurePortfolioNodeRequestV1 {
                node_id: None,
                parent_node_id: parent,
                adopt_node_ids: vec![],
                expected_parent_grant_version: parent_version,
                tier_label: label.into(),
                seat_session_id: seat,
                project_ids: projects.to_vec(),
                allowed_launches: vec![launch()],
                policy: tier_policy(u16::from(parent.is_some())),
                child_policy: None,
                max_direct_reports: if parent.is_some() { 4 } else { 5 },
                expected_node_grant_version: 0,
                expected_authority_epoch: 0,
                idempotency_key: format!("{label}-{seat}"),
            },
            PortfolioGrantor::Operator,
            "operator:test",
        )
        .unwrap()
        .node_id
}

fn code(error: DaemonError) -> String {
    match error {
        DaemonError::InvalidParam(code) => code,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn refs(workspace: &ManagerNodeWorkspaceV1) -> Vec<ManagerNodeRefV1> {
    workspace.children.iter().map(|child| child.node).collect()
}

fn project_ids(workspace: &ManagerNodeWorkspaceV1) -> Vec<Uuid> {
    workspace
        .projects
        .iter()
        .map(|project| project.overview.project_id)
        .collect()
}

/// pinnacle [A,B,C,D] -> global 1 [A,B], global 2 [C]; D is the pinnacle's
/// own project. A and C have PMs.
struct Tree {
    a: Uuid,
    b: Uuid,
    c: Uuid,
    d: Uuid,
    pm_a: Uuid,
    pm_c: Uuid,
    worker: Uuid,
    pinnacle: Uuid,
    pinnacle_seat: Uuid,
    global1: Uuid,
    global1_seat: Uuid,
    global2: Uuid,
    global2_seat: Uuid,
}

async fn tree(store: &SharedStore) -> Tree {
    let s = store.lock().await;
    let (a, b, c, d) = (
        add_project(&s, "A"),
        add_project(&s, "B"),
        add_project(&s, "C"),
        add_project(&s, "D"),
    );
    let (pm_a, pm_c, worker) = (
        add_session(&s, Some(a)),
        add_session(&s, Some(c)),
        add_session(&s, Some(b)),
    );
    appoint(&s, a, pm_a);
    appoint(&s, c, pm_c);
    let pinnacle_seat = add_session(&s, None);
    let pinnacle = node(&s, None, "pinnacle", pinnacle_seat, &[a, b, c, d]);
    let global1_seat = add_session(&s, None);
    let global1 = node(&s, Some(pinnacle), "global", global1_seat, &[a, b]);
    let global2_seat = add_session(&s, None);
    let global2 = node(&s, Some(pinnacle), "global", global2_seat, &[c]);
    Tree {
        a,
        b,
        c,
        d,
        pm_a,
        pm_c,
        worker,
        pinnacle,
        pinnacle_seat,
        global1,
        global1_seat,
        global2,
        global2_seat,
    }
}

/// Acceptance: from a global, `AgentManagerOverview` lists its PMs and its
/// projects.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn overview_from_a_global_lists_its_project_managers_and_projects() {
    let (control, store) = control_handle_with_store();
    let t = tree(&store).await;
    let overview = control
        .agent_manager_overview(t.global1_seat, AgentManagerOverviewRequestV1::default())
        .await
        .unwrap();
    assert_eq!(
        overview.node,
        ManagerNodeRefV1::Portfolio { node_id: t.global1 }
    );
    assert_eq!(overview.label, "global");
    assert_eq!(
        overview.parent,
        Some(ManagerNodeRefV1::Portfolio {
            node_id: t.pinnacle
        })
    );
    assert_eq!(
        overview.seat.as_ref().map(|seat| seat.session_id),
        Some(t.global1_seat)
    );
    assert_eq!(
        refs(&overview),
        vec![
            ManagerNodeRefV1::Project { project_id: t.a },
            ManagerNodeRefV1::Project { project_id: t.b },
        ]
    );
    let pm = &overview.children[0];
    assert_eq!(pm.seat.as_ref().map(|seat| seat.session_id), Some(t.pm_a));
    assert_eq!(
        pm.seat.as_ref().map(|seat| seat.status),
        Some(SessionStatus::Running)
    );
    assert_eq!(pm.state, "active");
    assert_eq!(overview.children[1].state, "vacant");
    assert_eq!(project_ids(&overview), vec![t.a, t.b]);
    assert_eq!(
        overview.projects[0]
            .overview
            .manager
            .as_ref()
            .map(|seat| seat.session_id),
        Some(t.pm_a)
    );
    // The fleet rollup covers A and B: the PM of A and the worker in B.
    assert_eq!(overview.fleet.active, 2);
    let _ = (t.worker, t.global2_seat);
}

/// #2052: the default overview carries a compact one-row-per-model fleet,
/// `full` is exactly the serialized workspace (the old shape) and `none`
/// drops the rollup.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn overview_fleet_detail_defaults_to_a_compact_summary() {
    use rsi_common::manager_node_workspace::ManagerOverviewFleetV1;
    let (control, store) = control_handle_with_store();
    let t = tree(&store).await;
    let call = |fleet| {
        control
            .agent_manager_overview_value(t.global1_seat, AgentManagerOverviewRequestV1 { fleet })
    };
    let default = control
        .agent_manager_overview_value(t.global1_seat, AgentManagerOverviewRequestV1::default())
        .await
        .unwrap();
    let summary = call(ManagerOverviewFleetV1::Summary).await.unwrap();
    let full = call(ManagerOverviewFleetV1::Full).await.unwrap();
    let none = call(ManagerOverviewFleetV1::None).await.unwrap();
    // `as_of` is the wall clock of each call.
    let norm = |mut value: serde_json::Value| {
        if let Some(fleet) = value["fleet"].as_object_mut() {
            fleet.insert("as_of".to_string(), serde_json::Value::Null);
        }
        value
    };
    assert_eq!(norm(default), norm(summary.clone()));
    // `full` is today's shape: the typed workspace, serialized unchanged.
    let typed = control
        .agent_manager_overview(t.global1_seat, AgentManagerOverviewRequestV1::default())
        .await
        .unwrap();
    assert_eq!(
        norm(full.clone()),
        norm(serde_json::to_value(&typed).unwrap())
    );
    assert!(full["fleet"]["groups"].is_array());
    assert_eq!(full["fleet"]["totals"].as_array().map(Vec::len), Some(3));
    // The summary keeps the rollup's headline and drops the groups.
    assert_eq!(summary["fleet"]["view"], "summary");
    assert_eq!(summary["fleet"]["active"], 2);
    assert!(summary["fleet"]["models"].is_array());
    assert!(summary["fleet"].get("groups").is_none());
    assert!(summary["fleet"]["totals"]["24h"]["invocations"].is_u64());
    assert!(none["fleet"].is_null());
    // Everything but `fleet` is identical in every mode.
    for value in [&summary, &full, &none] {
        let mut rest = value.clone();
        rest["fleet"] = serde_json::Value::Null;
        let mut base = serde_json::to_value(&typed).unwrap();
        base["fleet"] = serde_json::Value::Null;
        assert_eq!(rest, base);
    }
    // Size bound for the fleet part of the default payload: one row per model.
    let fleet_bytes = serde_json::to_vec(&summary["fleet"]).unwrap().len();
    assert!(fleet_bytes < 1024, "summary fleet is {fleet_bytes} bytes");
    let summary_bytes = serde_json::to_vec(&summary).unwrap().len();
    let full_bytes = serde_json::to_vec(&full).unwrap().len();
    assert!(
        summary_bytes < full_bytes,
        "{summary_bytes} vs {full_bytes}"
    );
}

/// Acceptance: from a pinnacle, the overview lists its globals' digests, not
/// their projects: only the project the pinnacle manages directly has a row.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn overview_from_a_pinnacle_lists_global_digests_not_their_projects() {
    let (control, store) = control_handle_with_store();
    let t = tree(&store).await;
    let overview = control
        .agent_manager_overview(t.pinnacle_seat, AgentManagerOverviewRequestV1::default())
        .await
        .unwrap();
    assert_eq!(overview.label, "pinnacle");
    assert_eq!(overview.parent, None);
    assert_eq!(
        refs(&overview),
        vec![
            ManagerNodeRefV1::Portfolio { node_id: t.global1 },
            ManagerNodeRefV1::Portfolio { node_id: t.global2 },
            ManagerNodeRefV1::Project { project_id: t.d },
        ]
    );
    assert_eq!(project_ids(&overview), vec![t.d]);
    let global1 = &overview.children[0];
    assert_eq!(global1.label, "global");
    assert_eq!(
        global1.seat.as_ref().map(|seat| seat.session_id),
        Some(t.global1_seat)
    );
    assert_eq!(global1.project_ids, vec![t.a, t.b]);
    let counts = global1.counts.expect("a digest carries summed counts");
    assert_eq!(counts.running_sessions, 2, "PM A and the worker in B");
    // A digest is exactly these fields: no project rows, no inbox.
    let keys: std::collections::BTreeSet<String> = serde_json::to_value(global1)
        .unwrap()
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    assert_eq!(
        keys,
        [
            "counts",
            "grant_version",
            "grantor",
            "label",
            "node",
            "pending_escalations",
            "project_ids",
            "seat",
            "state"
        ]
        .into_iter()
        .map(String::from)
        .collect()
    );
    // The fleet still rolls up the whole coverage.
    assert_eq!(overview.fleet.active, 3, "PM A, PM C and the worker in B");
    // The operator workspace of the same node lists the whole coverage.
    let workspace = control
        .operator_manager_node_workspace(ManagerNodeRefV1::Portfolio {
            node_id: t.pinnacle,
        })
        .await
        .unwrap();
    assert_eq!(project_ids(&workspace), vec![t.a, t.b, t.c, t.d]);
    assert_eq!(refs(&workspace), refs(&overview));
    let _ = t.pm_c;
}

/// Acceptance: the overview is refused for a caller that is no manager.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn overview_is_refused_for_a_non_manager_and_served_to_every_seat() {
    let (control, store) = control_handle_with_store();
    let t = tree(&store).await;
    let refused = control
        .agent_manager_overview(t.worker, AgentManagerOverviewRequestV1::default())
        .await
        .unwrap_err();
    assert_eq!(code(refused), MANAGER_TIER_NOT_NODE_SEAT);
    let pm = control
        .agent_manager_overview(t.pm_a, AgentManagerOverviewRequestV1::default())
        .await
        .unwrap();
    assert_eq!(pm.node, ManagerNodeRefV1::Project { project_id: t.a });
    assert_eq!(
        pm.parent,
        Some(ManagerNodeRefV1::Portfolio { node_id: t.global1 })
    );
    assert_eq!(project_ids(&pm), vec![t.a]);
    let guard = store.lock().await;
    for seat in [t.pinnacle_seat, t.global1_seat, t.pm_a] {
        let projection = guard.agent_authority_projection(seat).unwrap();
        assert!(projection.verbs.contains(&Verb::ManagerOverview));
    }
    let worker = guard.agent_authority_projection(t.worker).unwrap();
    assert!(!worker.verbs.contains(&Verb::ManagerOverview));
}

/// #2743: the project manager's overview shows whether rolling is red: its
/// open `qa` Issues and the green SHA state, per project. A project with no
/// open `qa` Issue shows none, and a closed one drops out.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn overview_shows_the_open_qa_issues_of_each_project_2743() {
    let (control, store) = control_handle_with_store();
    let t = tree(&store).await;
    let red = store
        .lock()
        .await
        .create_issue(&rsi_common::types::NewIssue {
            project_id: t.a,
            title: "Full library QA test red after landing a0a9a7e6c".into(),
            body: String::new(),
            priority: Some(1),
            labels: vec!["qa".into(), "bug".into()],
            created_by_session_id: None,
            assignee: None,
            idea_id: None,
            source_event_id: None,
            source_finding_ref: None,
        })
        .unwrap();
    let overview = control
        .agent_manager_overview_value(t.pm_a, AgentManagerOverviewRequestV1::default())
        .await
        .unwrap();
    let health = &overview["projects"][0]["overview"]["rolling_health"];
    assert_eq!(health["open_qa_issues_total"], 1, "{overview}");
    assert_eq!(health["open_qa_issues"][0]["number"], red.display_number);
    assert!(health["qa_green"].is_null(), "no green run recorded yet");

    store
        .lock()
        .await
        .update_issue_status(red.id, rsi_common::types::IssueStatus::Closed)
        .unwrap();
    let overview = control
        .agent_manager_overview_value(t.pm_a, AgentManagerOverviewRequestV1::default())
        .await
        .unwrap();
    let health = &overview["projects"][0]["overview"]["rolling_health"];
    assert_eq!(health["open_qa_issues_total"], 0, "{overview}");
}

/// Acceptance: for a single global, `GetGlobalManagerWorkspace` and
/// `AgentGlobalOverview` return the v0 data: the node snapshot's grant, seat,
/// projects and missing projects, active and after revocation.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn global_shims_return_the_v0_data_of_the_single_global() {
    let (control, store) = control_handle_with_store();
    let (a, b, gone, seat, pm_a) = {
        let s = store.lock().await;
        let (a, b) = (add_project(&s, "A"), add_project(&s, "B"));
        let seat = add_session(&s, None);
        let pm_a = add_session(&s, Some(a));
        appoint(&s, a, pm_a);
        (a, b, Uuid::new_v4(), seat, pm_a)
    };
    let grant = store
        .lock()
        .await
        .configure_global_manager(
            &ConfigureGlobalManagerRequestV1 {
                session_id: seat,
                project_ids: vec![a, b],
                allowed_launches: vec![launch()],
                project_policy: tier_policy(0),
                expected_grant_version: 0,
                idempotency_key: "single-global".into(),
            },
            "operator:test",
        )
        .unwrap();
    let node_id = store.lock().await.global_shim_node().unwrap().unwrap();
    let global = ManagerNodeRefV1::Portfolio { node_id };

    let shim = control.operator_global_workspace().await.unwrap();
    assert_eq!(shim.grant.as_ref(), Some(&grant));
    assert_eq!(shim.seat.as_ref().map(|s| s.session_id), Some(seat));
    assert_eq!(
        shim.projects
            .iter()
            .map(|p| p.overview.project_id)
            .collect::<Vec<_>>(),
        vec![a, b]
    );
    assert_eq!(
        shim.projects[0]
            .overview
            .manager
            .as_ref()
            .map(|m| m.session_id),
        Some(pm_a)
    );
    assert!(shim.missing_project_ids.is_empty());
    let workspace = control
        .operator_manager_node_workspace(global)
        .await
        .unwrap();
    assert_eq!(workspace.clone().into_global(), shim);

    let v0 = control
        .agent_global_overview(seat, AgentGlobalOverviewRequestV1 {})
        .await
        .unwrap();
    let overview = control
        .agent_manager_overview(seat, AgentManagerOverviewRequestV1::default())
        .await
        .unwrap();
    assert_eq!(v0.grant_version, grant.grant_version);
    assert_eq!(
        v0.projects,
        overview
            .projects
            .into_iter()
            .map(|project| project.overview)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        v0.projects,
        shim.projects
            .iter()
            .map(|project| project.overview.clone())
            .collect::<Vec<_>>()
    );

    // Revoked: the shim keeps showing the last grant, as the node does.
    store
        .lock()
        .await
        .revoke_global_manager(&RevokeGlobalManagerRequestV1 {
            expected_grant_version: grant.grant_version,
            idempotency_key: "revoke-single".into(),
        })
        .unwrap();
    let revoked = control.operator_global_workspace().await.unwrap();
    assert_eq!(
        revoked.grant.as_ref().map(|g| g.state.as_str()),
        Some("revoked")
    );
    let node_view = control
        .operator_manager_node_workspace(global)
        .await
        .unwrap();
    assert_eq!(node_view.state, "revoked");
    assert_eq!(node_view.into_global(), revoked);
    let _ = gone;
}
