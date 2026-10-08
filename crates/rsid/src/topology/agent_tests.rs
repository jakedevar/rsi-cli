//! T4 acceptance tests for the scoped agent topology verbs (#633, plan §7
//! T4-A1..A7, A9..A11). The durable executor runs against the provider-free
//! T2 harness; authority, policy and audit run for real on its store.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use rsi_common::harness_manager::ConfigureHarnessManagerRequestV1;
use rsi_common::harness_manager_v2::{
    ConfigureHarnessManagerPolicyRequestV2, ManagerCapabilityV2, ManagerLaunchChoiceV2,
    ManagerOperatingModeV2, ManagerPolicyV2,
};
use rsi_common::rpc::{ResolveTopologyAttemptParams, TopologyAttemptAction};
use rsi_common::topology_agent::{
    AgentTopologyExecuteRequestV1, AgentTopologyGetExecutionRequestV1,
    AgentTopologyInterruptRequestV1, AgentTopologyListRequestV1, AgentTopologyScopeV1,
    AgentTopologyUpsertRequestV1, AgentTopologyUpsertResultV1,
};
use rsi_common::types::{
    Project, SessionKind, SessionProvider, SessionStatus, Topology, TopologyDefinition,
};
use uuid::Uuid;

use super::agent::{self, AgentKnobs};
use super::executor::Step;
use super::store::{self as rows, AttemptStatus, ExecutionStatus};
use super::tests::{Harness, ReviewScript, error_code, git};
use crate::error::{DaemonError, Result};

#[path = "starters_tests.rs"]
mod starters_tests;

const KNOBS: AgentKnobs = AgentKnobs {
    executor_enabled: true,
    bulk_fanout_min_openrouter: agent::DEFAULT_BULK_FANOUT_MIN_OPENROUTER,
};

fn luna() -> ManagerLaunchChoiceV2 {
    ManagerLaunchChoiceV2 {
        provider: SessionProvider::Codex,
        model: "gpt-6-luna".into(),
        effort: Some("medium".into()),
    }
}

fn glm() -> ManagerLaunchChoiceV2 {
    ManagerLaunchChoiceV2 {
        provider: SessionProvider::OpenRouter,
        model: "z-ai/glm-5.3-flashx".into(),
        effort: Some("medium".into()),
    }
}

fn policy(
    capabilities: &[ManagerCapabilityV2],
    allowed: Vec<ManagerLaunchChoiceV2>,
    max_created_sessions: u16,
) -> ManagerPolicyV2 {
    ManagerPolicyV2 {
        mode: ManagerOperatingModeV2::Execute,
        capabilities: capabilities.to_vec(),
        allowed_launches: allowed,
        max_created_sessions,
        max_active_sessions: 32,
        ..ManagerPolicyV2::default()
    }
}

/// One project with a manager scoped to Epics A and B, an unscoped Epic C,
/// a lead per Epic, and an ordinary worker under A.
struct World {
    harness: Harness,
    project: Uuid,
    manager: Uuid,
    epic_a: Uuid,
    epic_b: Uuid,
    epic_c: Uuid,
    lead_a: Uuid,
    lead_b: Uuid,
    worker: Uuid,
}

impl World {
    async fn new(grant: ManagerPolicyV2) -> Self {
        let harness = Harness::new();
        let project = Uuid::new_v4();
        let (manager, group) = (Uuid::new_v4(), Uuid::new_v4());
        let (epic_a, epic_b, epic_c) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let (lead_a, lead_b, lead_c) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let worker = Uuid::new_v4();
        {
            let store = harness.executor.store.lock().await;
            let now = chrono::Utc::now();
            store
                .insert_project(&Project {
                    id: project,
                    name: "Topology agents".into(),
                    path: Some(harness.repo.clone()),
                    description: None,
                    color: Project::DEFAULT_COLOR.into(),
                    context_files: None,
                    created_at: now,
                    updated_at: now,
                })
                .unwrap();
            let insert = |id: Uuid, kind: SessionKind, parent: Option<Uuid>, lead: Option<Uuid>| {
                let mut row =
                    crate::session::agent_verbs::tests::test_session(id, harness.repo.clone());
                row.project_id = Some(project);
                row.session_kind = kind;
                row.parent_id = parent;
                row.lead_session_id = lead;
                row.status = SessionStatus::Running;
                store.insert_session(&row).unwrap();
            };
            insert(manager, SessionKind::Standard, None, None);
            insert(group, SessionKind::Group, None, None);
            for (epic, lead) in [(epic_a, lead_a), (epic_b, lead_b), (epic_c, lead_c)] {
                insert(epic, SessionKind::Epic, Some(group), Some(lead));
                insert(lead, SessionKind::Task, Some(epic), None);
            }
            insert(worker, SessionKind::Task, Some(epic_a), None);
            store
                .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                    group_ids: vec![],
                    project_id: project,
                    session_id: manager,
                    epic_ids: Some(vec![epic_a, epic_b]),
                    expected_row_version: 0,
                })
                .unwrap();
        }
        let world = Self {
            harness,
            project,
            manager,
            epic_a,
            epic_b,
            epic_c,
            lead_a,
            lead_b,
            worker,
        };
        world.grant(grant).await;
        world
    }

    /// Replace the operator policy grant (the TUI/RPC operator surface).
    async fn grant(&self, policy: ManagerPolicyV2) {
        let store = self.harness.executor.store.lock().await;
        let current = store
            .get_harness_manager_policy(self.project)
            .unwrap()
            .map_or(0, |grant| grant.row_version);
        let scope = store.get_harness_manager(self.project).unwrap().unwrap();
        store
            .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
                project_id: self.project,
                expected_scope_version: scope.row_version,
                expected_policy_version: current,
                idempotency_key: Uuid::new_v4().to_string(),
                policy,
            })
            .unwrap();
    }

    async fn upsert(
        &self,
        caller: Uuid,
        request: &AgentTopologyUpsertRequestV1,
    ) -> Result<AgentTopologyUpsertResultV1> {
        self.upsert_with(caller, request, KNOBS).await
    }

    async fn upsert_with(
        &self,
        caller: Uuid,
        request: &AgentTopologyUpsertRequestV1,
        knobs: AgentKnobs,
    ) -> Result<AgentTopologyUpsertResultV1> {
        let store = self.harness.executor.store.lock().await;
        agent::upsert(&store, knobs, caller, request).map(|(result, _)| result)
    }

    async fn execute(
        &self,
        caller: Uuid,
        topology: &AgentTopologyUpsertResultV1,
        epic: Uuid,
        key: &str,
    ) -> Result<rsi_common::topology_agent::AgentTopologyExecuteResultV1> {
        self.execute_on_call(caller, topology, epic, key, None)
            .await
    }

    async fn execute_on_call(
        &self,
        caller: Uuid,
        topology: &AgentTopologyUpsertResultV1,
        epic: Uuid,
        key: &str,
        on_call: Option<rsi_common::types::TopologyOnCallSeat>,
    ) -> Result<rsi_common::topology_agent::AgentTopologyExecuteResultV1> {
        let request = AgentTopologyExecuteRequestV1 {
            project_id: None,
            on_call,
            topology_id: topology.topology_id.unwrap(),
            expected_digest: topology.definition_digest.clone(),
            epic_id: epic,
            inputs: serde_json::Value::Null,
            base_commit: Some(self.harness.base.clone()),
            idempotency_key: key.into(),
        };
        agent::execute(&self.harness.executor.store, || KNOBS, caller, &request)
            .await
            .map(|executed| executed.result)
    }

    async fn list(
        &self,
        caller: Uuid,
        request: &AgentTopologyListRequestV1,
    ) -> Result<Vec<String>> {
        let store = self.harness.executor.store.lock().await;
        agent::list(&store, caller, request)
            .map(|page| page.topologies.into_iter().map(|t| t.name).collect())
    }

    async fn get(
        &self,
        caller: Uuid,
        execution_id: Uuid,
    ) -> Result<rsi_common::topology_agent::AgentTopologyGetExecutionResultV1> {
        let store = self.harness.executor.store.lock().await;
        agent::get_execution(
            &store,
            caller,
            &AgentTopologyGetExecutionRequestV1 {
                project_id: None,
                execution_id,
                after_sequence: None,
                limit: None,
            },
        )
    }

    async fn interrupt(
        &self,
        caller: Uuid,
        execution_id: Uuid,
        expected_row_version: i64,
        key: &str,
    ) -> Result<rsi_common::topology_agent::AgentTopologyInterruptResultV1> {
        let store = self.harness.executor.store.lock().await;
        agent::interrupt(
            &store,
            caller,
            &AgentTopologyInterruptRequestV1 {
                project_id: None,
                execution_id,
                expected_row_version,
                idempotency_key: key.into(),
            },
        )
        .map(|(result, _)| result)
    }

    async fn resolve(
        &self,
        caller: Uuid,
        execution_id: Uuid,
        attempt_id: Uuid,
        action: TopologyAttemptAction,
        key: &str,
        confirm: Option<&str>,
    ) -> Result<rsi_common::rpc::ResolveTopologyAttemptResponse> {
        let params = ResolveTopologyAttemptParams {
            execution_id,
            attempt_id,
            action,
            expected_row_version: self.harness.row_version(execution_id).await,
            idempotency_key: key.into(),
            confirm_preserved_commit: confirm.map(str::to_owned),
        };
        agent::resolve(&self.harness.executor, caller, &params).await
    }

    /// Launch the single node `A`, let it commit, then interrupt it: the
    /// execution blocks on verified preserved work (plan §3.4).
    async fn blocked_on_preserved_work(&self, execution_id: Uuid) -> rows::AttemptRow {
        assert_eq!(
            self.harness.executor.advance(execution_id).await.unwrap(),
            Step::Wait
        );
        let running = self.harness.attempt(execution_id, "A", 0, 1).await;
        let head = self
            .harness
            .commit_and_complete(running.session_id, &format!("work-{execution_id}"));
        self.harness
            .set_status(running.session_id, SessionStatus::Interrupted);
        assert_eq!(
            self.harness.executor.advance(execution_id).await.unwrap(),
            Step::Done
        );
        assert_eq!(
            self.harness.status(execution_id).await,
            ExecutionStatus::Blocked
        );
        let blocked = self.harness.attempt(execution_id, "A", 0, 1).await;
        assert_eq!(blocked.status, AttemptStatus::Blocked);
        assert_eq!(blocked.preserved_commit.as_deref(), Some(head.as_str()));
        blocked
    }

    /// `(kind, actor_kind, actor_session_id)` of every audit event.
    async fn events(&self, execution_id: Uuid) -> Vec<(String, String, Option<String>)> {
        let store = self.harness.executor.store.lock().await;
        let mut statement = store
            .conn
            .prepare(
                "SELECT kind,actor_kind,actor_session_id FROM topology_events \
                 WHERE execution_id=?1 ORDER BY execution_seq",
            )
            .unwrap();
        statement
            .query_map([execution_id.to_string()], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    }

    async fn created_usage(&self) -> i64 {
        let store = self.harness.executor.store.lock().await;
        let config = store.get_harness_manager(self.project).unwrap().unwrap();
        store.manager_v2_created_usage(&config, false).unwrap()
    }
}

/// A topology of session nodes, each `(id, launch)`, with forward edges.
fn definition(
    nodes: &[(&str, &ManagerLaunchChoiceV2)],
    edges: &[(&str, &str)],
) -> TopologyDefinition {
    let provider = |launch: &ManagerLaunchChoiceV2| match launch.provider {
        SessionProvider::Codex => "codex",
        SessionProvider::OpenRouter => "openrouter",
        SessionProvider::Claude => "claude",
        other => panic!("fixture provider {other:?}"),
    };
    serde_json::from_value(serde_json::json!({
        "nodes": nodes.iter().map(|(id, launch)| serde_json::json!({
            "id": id,
            "kind": "Task",
            "label": id,
            "params": {
                "instructions": format!("do {id}"),
                "provider": provider(launch),
                "model": launch.model,
                "effort": launch.effort,
                "step": {"kind": "session", "expects_commit": false, "pass_content": false},
            },
        })).collect::<Vec<_>>(),
        "edges": edges.iter().map(|(from, to)| serde_json::json!({"from": from, "to": to})).collect::<Vec<_>>(),
    }))
    .unwrap()
}

fn upsert_request(name: &str, definition: TopologyDefinition) -> AgentTopologyUpsertRequestV1 {
    AgentTopologyUpsertRequestV1 {
        project_id: None,
        name: name.into(),
        definition,
        scope: AgentTopologyScopeV1::Epic,
        epic_id: None,
        expected_revision: None,
        validate_only: false,
        idempotency_key: format!("upsert-{name}"),
    }
}

fn diagnostics(error: &DaemonError) -> Vec<String> {
    match error {
        DaemonError::StructuredRpc { data, .. } => data["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry.as_str().unwrap().to_owned())
            .collect(),
        other => panic!("expected a typed refusal, got {other}"),
    }
}

fn automation() -> ManagerPolicyV2 {
    policy(&[ManagerCapabilityV2::Automation], vec![luna(), glm()], 64)
}

// ─── A1: every non-lead, non-manager caller is refused ─────────────────────

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn t4_a1_worker_and_node_tokens_refused_on_all_six_verbs() {
    let world = World::new(automation()).await;
    let topology = world
        .upsert(
            world.lead_a,
            &upsert_request("a-flow", definition(&[("A", &luna())], &[])),
        )
        .await
        .unwrap();
    let execution = world
        .execute(world.lead_a, &topology, world.epic_a, "run-1")
        .await
        .unwrap()
        .execution_id;
    assert_eq!(
        world.harness.executor.advance(execution).await.unwrap(),
        Step::Wait
    );
    let attempt = world.harness.attempt(execution, "A", 0, 1).await;
    // The node session is an ordinary leaf under the Epic: it cannot re-enter
    // the executor (plan §5.2).
    {
        let store = world.harness.executor.store.lock().await;
        let mut node = crate::session::agent_verbs::tests::test_session(
            attempt.session_id,
            world.harness.repo.clone(),
        );
        node.project_id = Some(world.project);
        node.parent_id = Some(world.epic_a);
        node.status = SessionStatus::Running;
        store.insert_session(&node).unwrap();
    }
    for caller in [world.worker, attempt.session_id, Uuid::new_v4()] {
        let mut codes = Vec::new();
        codes.push(error_code(
            &world
                .upsert(
                    caller,
                    &upsert_request("w-flow", definition(&[("A", &luna())], &[])),
                )
                .await
                .unwrap_err(),
        ));
        codes.push(error_code(
            &world
                .list(caller, &AgentTopologyListRequestV1::default())
                .await
                .unwrap_err(),
        ));
        codes.push(error_code(
            &world
                .execute(caller, &topology, world.epic_a, "worker-run")
                .await
                .unwrap_err(),
        ));
        codes.push(error_code(&world.get(caller, execution).await.unwrap_err()));
        codes.push(error_code(
            &world
                .interrupt(
                    caller,
                    execution,
                    world.harness.row_version(execution).await,
                    "stop",
                )
                .await
                .unwrap_err(),
        ));
        codes.push(error_code(
            &world
                .resolve(
                    caller,
                    execution,
                    attempt.id,
                    TopologyAttemptAction::Inspect,
                    "look",
                    None,
                )
                .await
                .unwrap_err(),
        ));
        assert_eq!(codes, vec!["authority_denied"; 6], "caller {caller}");
    }
    assert_eq!(
        world.harness.status(execution).await,
        ExecutionStatus::Running
    );
}

// ─── A2: a lead is confined to its Epic ────────────────────────────────────

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn t4_a2_lead_is_confined_to_its_epic() {
    let world = World::new(automation()).await;
    let a_flow = world
        .upsert(
            world.lead_a,
            &upsert_request("a-flow", definition(&[("A", &luna())], &[])),
        )
        .await
        .unwrap();
    assert_eq!(a_flow.revision, Some(1));
    world
        .upsert(
            world.lead_b,
            &upsert_request("b-flow", definition(&[("A", &luna())], &[])),
        )
        .await
        .unwrap();

    // Authoring outside its Epic or at manager scope is refused.
    let mut foreign = upsert_request("a-into-b", definition(&[("A", &luna())], &[]));
    foreign.epic_id = Some(world.epic_b);
    assert_eq!(
        error_code(&world.upsert(world.lead_a, &foreign).await.unwrap_err()),
        "not_found_in_scope"
    );
    let mut manager_scope = upsert_request("a-manager", definition(&[("A", &luna())], &[]));
    manager_scope.scope = AgentTopologyScopeV1::Manager;
    assert_eq!(
        error_code(
            &world
                .upsert(world.lead_a, &manager_scope)
                .await
                .unwrap_err()
        ),
        "authority_denied"
    );

    // Each lead lists exactly its own Epic's topologies.
    let request = AgentTopologyListRequestV1::default();
    assert_eq!(
        world.list(world.lead_a, &request).await.unwrap(),
        vec!["a-flow"]
    );
    assert_eq!(
        world.list(world.lead_b, &request).await.unwrap(),
        vec!["b-flow"]
    );

    // A's topology runs only on A, only by A's lead.
    assert_eq!(
        error_code(
            &world
                .execute(world.lead_b, &a_flow, world.epic_b, "b-steals")
                .await
                .unwrap_err()
        ),
        "not_found_in_scope"
    );
    assert_eq!(
        error_code(
            &world
                .execute(world.lead_a, &a_flow, world.epic_b, "a-elsewhere")
                .await
                .unwrap_err()
        ),
        "not_found_in_scope"
    );
    let accepted = world
        .execute(world.lead_a, &a_flow, world.epic_a, "a-run")
        .await
        .unwrap();
    let view = world
        .get(world.lead_a, accepted.execution_id)
        .await
        .unwrap();
    assert_eq!(view.execution.requested_by_kind, "epic_lead");
    assert_eq!(view.execution.epic_id, Some(world.epic_a));
    assert_eq!(view.execution.topology_revision, Some(1));
    assert_eq!(
        world.events(accepted.execution_id).await[0],
        (
            "accepted".to_owned(),
            "epic_lead".to_owned(),
            Some(world.lead_a.to_string())
        )
    );
    for denied in [
        world
            .get(world.lead_b, accepted.execution_id)
            .await
            .map(drop),
        world
            .interrupt(world.lead_b, accepted.execution_id, 1, "b-stop")
            .await
            .map(drop),
    ] {
        assert_eq!(error_code(&denied.unwrap_err()), "not_found_in_scope");
    }

    // An operator topology becomes lead-executable only once shared.
    let operator = Topology {
        id: Uuid::new_v4(),
        name: "operator-flow".into(),
        definition: definition(&[("A", &luna())], &[]),
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };
    {
        let store = world.harness.executor.store.lock().await;
        store.insert_topology(&operator).unwrap();
        assert!(agent::set_shared(&store, operator.id, true).unwrap());
    }
    let listed = world.list(world.lead_b, &request).await.unwrap();
    assert_eq!(listed, vec!["b-flow", "operator-flow"]);
    let digest = {
        let store = world.harness.executor.store.lock().await;
        agent::list(&store, world.lead_b, &request)
            .unwrap()
            .topologies
            .into_iter()
            .find(|t| t.name == "operator-flow")
            .unwrap()
            .definition_digest
    };
    let shared = AgentTopologyUpsertResultV1 {
        topology_id: Some(operator.id),
        revision: Some(1),
        definition_digest: digest,
        diagnostics: vec![],
        deduplicated: false,
    };
    let run = world
        .execute(world.lead_b, &shared, world.epic_b, "b-shared")
        .await
        .unwrap();
    assert_eq!(
        world
            .get(world.lead_b, run.execution_id)
            .await
            .unwrap()
            .execution
            .epic_id,
        Some(world.epic_b)
    );
}

// ─── A3: the manager needs Automation and in-scope targets ─────────────────

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn t4_a3_manager_needs_automation_and_in_scope_targets() {
    let world = World::new(policy(
        &[ManagerCapabilityV2::SessionControl],
        vec![luna()],
        64,
    ))
    .await;
    assert_eq!(
        error_code(
            &world
                .list(world.manager, &AgentTopologyListRequestV1::default())
                .await
                .unwrap_err()
        ),
        "capability_denied"
    );
    world
        .grant(policy(&[ManagerCapabilityV2::Automation], vec![luna()], 64))
        .await;

    let mut shared = upsert_request("manager-flow", definition(&[("A", &luna())], &[]));
    shared.scope = AgentTopologyScopeV1::Manager;
    let manager_flow = world.upsert(world.manager, &shared).await.unwrap();
    let mut on_b = upsert_request("b-owned", definition(&[("A", &luna())], &[]));
    on_b.epic_id = Some(world.epic_b);
    world.upsert(world.manager, &on_b).await.unwrap();
    let mut on_c = upsert_request("c-owned", definition(&[("A", &luna())], &[]));
    on_c.epic_id = Some(world.epic_c);
    assert_eq!(
        error_code(&world.upsert(world.manager, &on_c).await.unwrap_err()),
        "not_found_in_scope"
    );
    assert_eq!(
        world
            .list(world.manager, &AgentTopologyListRequestV1::default())
            .await
            .unwrap(),
        vec!["b-owned", "manager-flow"]
    );

    // Out-of-scope Epic C is refused; in-scope A runs as `manager`.
    assert_eq!(
        error_code(
            &world
                .execute(world.manager, &manager_flow, world.epic_c, "on-c")
                .await
                .unwrap_err()
        ),
        "not_found_in_scope"
    );
    let run = world
        .execute(world.manager, &manager_flow, world.epic_a, "on-a")
        .await
        .unwrap();
    let view = world.get(world.manager, run.execution_id).await.unwrap();
    assert_eq!(view.execution.requested_by_kind, "manager");

    // A paused Epic takes no new effect; the manager can still stop work.
    let mut paused = policy(&[ManagerCapabilityV2::Automation], vec![luna()], 64);
    paused.paused_epic_ids = vec![world.epic_a];
    world.grant(paused).await;
    assert_eq!(
        error_code(
            &world
                .execute(world.manager, &manager_flow, world.epic_a, "paused")
                .await
                .unwrap_err()
        ),
        "paused"
    );
    let version = world.harness.row_version(run.execution_id).await;
    let stopped = world
        .interrupt(world.manager, run.execution_id, version, "stop")
        .await
        .unwrap();
    assert_eq!(
        world.harness.status(run.execution_id).await,
        ExecutionStatus::Cancelling
    );
    assert!(!stopped.deduplicated);
}

// ─── A4: model constraints at upsert, execute and launch ───────────────────

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn t4_a4_triple_outside_allowed_launches_refused_at_upsert_and_execute() {
    let world = World::new(automation()).await;
    let opus = ManagerLaunchChoiceV2 {
        provider: SessionProvider::Claude,
        model: "claude-opus-5-5".into(),
        effort: Some("high".into()),
    };
    let refused = world
        .upsert(
            world.lead_a,
            &upsert_request("opus-flow", definition(&[("A", &opus)], &[])),
        )
        .await
        .unwrap_err();
    assert_eq!(error_code(&refused), "policy_refused");
    assert_eq!(
        diagnostics(&refused),
        vec![
            "node A: launch Claude/claude-opus-5-5/high is not in the operator's allowed_launches"
        ]
    );
    let mut probe = upsert_request("opus-flow", definition(&[("A", &opus)], &[]));
    probe.validate_only = true;
    assert_eq!(
        world
            .upsert(world.lead_a, &probe)
            .await
            .unwrap()
            .diagnostics,
        diagnostics(&refused)
    );

    // Accepted under the grant, refused at execute once the grant narrows.
    let flow = world
        .upsert(
            world.lead_a,
            &upsert_request("luna-flow", definition(&[("A", &luna())], &[])),
        )
        .await
        .unwrap();
    let queued = world
        .execute(world.lead_a, &flow, world.epic_a, "before-narrowing")
        .await
        .unwrap();
    world
        .grant(policy(&[ManagerCapabilityV2::Automation], vec![glm()], 64))
        .await;
    let refused = world
        .execute(world.lead_a, &flow, world.epic_a, "after-narrowing")
        .await
        .unwrap_err();
    assert_eq!(error_code(&refused), "policy_refused");

    // The already-accepted execution re-checks at launch: blocked, no session.
    assert_eq!(
        world
            .harness
            .executor
            .advance(queued.execution_id)
            .await
            .unwrap(),
        Step::Done
    );
    let attempt = world.harness.attempt(queued.execution_id, "A", 0, 1).await;
    assert_eq!(attempt.status, AttemptStatus::Blocked);
    assert_eq!(attempt.failure_class.as_deref(), Some("policy_refused"));
    assert_eq!(attempt.error.as_deref(), Some("launch_not_granted"));
    assert_eq!(
        world.harness.status(queued.execution_id).await,
        ExecutionStatus::Blocked
    );
    assert_eq!(world.harness.launches().len(), 0);

    // An empty grant list fails closed for agents.
    world
        .grant(policy(&[ManagerCapabilityV2::Automation], vec![], 64))
        .await;
    let refused = world
        .execute(world.lead_a, &flow, world.epic_a, "empty-grant")
        .await
        .unwrap_err();
    assert_eq!(
        diagnostics(&refused)[0],
        "no allowed_launches are granted by the operator; agent session nodes fail closed"
    );
}

// ─── A5: max_created_sessions is charged ───────────────────────────────────

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn t4_a5_max_created_sessions_is_charged() {
    let world = World::new(policy(&[ManagerCapabilityV2::Automation], vec![luna()], 2)).await;
    let mut request = upsert_request("charged", definition(&[("A", &luna())], &[]));
    request.scope = AgentTopologyScopeV1::Manager;
    let flow = world.upsert(world.manager, &request).await.unwrap();
    // Three admissions fit the budget before anything launched.
    let mut runs = Vec::new();
    for key in ["one", "two", "three"] {
        runs.push(
            world
                .execute(world.manager, &flow, world.epic_a, key)
                .await
                .unwrap()
                .execution_id,
        );
    }
    assert_eq!(world.created_usage().await, 0);
    // Each launch is charged; the third launch finds the quota spent.
    for run in &runs[..2] {
        assert_eq!(
            world.harness.executor.advance(*run).await.unwrap(),
            Step::Wait
        );
    }
    assert_eq!(world.created_usage().await, 2);
    assert_eq!(
        world.harness.executor.advance(runs[2]).await.unwrap(),
        Step::Done
    );
    let third = world.harness.attempt(runs[2], "A", 0, 1).await;
    assert_eq!(third.failure_class.as_deref(), Some("policy_refused"));
    assert_eq!(third.error.as_deref(), Some("creation_limit"));
    assert_eq!(world.harness.launches().len(), 2);
    // A new admission is refused up front.
    assert_eq!(
        error_code(
            &world
                .execute(world.manager, &flow, world.epic_a, "four")
                .await
                .unwrap_err()
        ),
        "creation_limit"
    );
}

// ─── A6: an idempotent execute is deduplicated ─────────────────────────────

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn t4_a6_idempotent_execute_is_deduplicated() {
    let world = World::new(automation()).await;
    let flow = world
        .upsert(
            world.lead_a,
            &upsert_request("once", definition(&[("A", &luna())], &[])),
        )
        .await
        .unwrap();
    let first = world
        .execute(world.lead_a, &flow, world.epic_a, "same-key")
        .await
        .unwrap();
    assert!(!first.deduplicated);
    let replay = world
        .execute(world.lead_a, &flow, world.epic_a, "same-key")
        .await
        .unwrap();
    assert!(replay.deduplicated);
    assert_eq!(replay.execution_id, first.execution_id);
    assert_eq!(replay.base_commit, first.base_commit);
    let executions: i64 = {
        let store = world.harness.executor.store.lock().await;
        store
            .conn
            .query_row("SELECT count(*) FROM topology_executions", [], |row| {
                row.get(0)
            })
            .unwrap()
    };
    assert_eq!(executions, 1);
    // The same key with a different request conflicts.
    let changed = AgentTopologyExecuteRequestV1 {
        project_id: None,
        on_call: None,
        topology_id: flow.topology_id.unwrap(),
        expected_digest: flow.definition_digest.clone(),
        epic_id: world.epic_a,
        inputs: serde_json::json!({"area": "other"}),
        base_commit: Some(world.harness.base.clone()),
        idempotency_key: "same-key".into(),
    };
    let conflict = agent::execute(
        &world.harness.executor.store,
        || KNOBS,
        world.lead_a,
        &changed,
    )
    .await
    .err()
    .unwrap();
    assert_eq!(error_code(&conflict), "idempotency_conflict");
    // Upsert replays by content and revises only under the current revision.
    let again = world
        .upsert(
            world.lead_a,
            &upsert_request("once", definition(&[("A", &luna())], &[])),
        )
        .await
        .unwrap();
    assert!(again.deduplicated);
    assert_eq!(again.revision, Some(1));
    let mut revised = upsert_request(
        "once",
        definition(&[("A", &luna()), ("B", &luna())], &[("A", "B")]),
    );
    // A revision is a new request: it needs a key of its own. A refused
    // request never binds its key.
    revised.idempotency_key = "upsert-once-v2".into();
    assert_eq!(
        error_code(&world.upsert(world.lead_a, &revised).await.unwrap_err()),
        "stale_revision"
    );
    revised.expected_revision = Some(1);
    assert_eq!(
        world.upsert(world.lead_a, &revised).await.unwrap().revision,
        Some(2)
    );
}

// ─── interrupt: CAS, idempotency, audit ────────────────────────────────────

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn t4_interrupt_is_cas_fenced_idempotent_and_audited() {
    let world = World::new(automation()).await;
    let flow = world
        .upsert(
            world.lead_a,
            &upsert_request("stoppable", definition(&[("A", &luna())], &[])),
        )
        .await
        .unwrap();
    let run = world
        .execute(world.lead_a, &flow, world.epic_a, "run")
        .await
        .unwrap()
        .execution_id;
    assert_eq!(
        world.harness.executor.advance(run).await.unwrap(),
        Step::Wait
    );
    let version = world.harness.row_version(run).await;
    assert_eq!(
        error_code(
            &world
                .interrupt(world.lead_a, run, version - 1, "stale")
                .await
                .unwrap_err()
        ),
        "stale_row_version"
    );
    let first = world
        .interrupt(world.lead_a, run, version, "stop")
        .await
        .unwrap();
    let replay = world
        .interrupt(world.lead_a, run, version, "stop")
        .await
        .unwrap();
    assert!(!first.deduplicated);
    assert!(replay.deduplicated);
    let lead = Some(world.lead_a.to_string());
    let events = world.events(run).await;
    assert!(events.contains(&(
        "agent_request_refused".to_owned(),
        "epic_lead".to_owned(),
        lead.clone()
    )));
    assert!(events.contains(&("cancelling".to_owned(), "epic_lead".to_owned(), lead)));
    let attempt = world.harness.attempt(run, "A", 0, 1).await;
    world
        .harness
        .set_status(attempt.session_id, SessionStatus::Interrupted);
    for _ in 0..4 {
        if world.harness.executor.advance(run).await.unwrap() == Step::Done {
            break;
        }
    }
    assert_eq!(world.harness.status(run).await, ExecutionStatus::Cancelled);
}

// ─── A9: preserved work, lead scope ────────────────────────────────────────

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn t4_a9_lead_resolves_preserved_work_in_own_epic_only() {
    let world = World::new(automation()).await;
    let flow = world
        .upsert(
            world.lead_a,
            &upsert_request("preserving", definition(&[("A", &luna())], &[])),
        )
        .await
        .unwrap();
    let first = world
        .execute(world.lead_a, &flow, world.epic_a, "first")
        .await
        .unwrap()
        .execution_id;
    let blocked = world.blocked_on_preserved_work(first).await;
    let preserved = blocked.preserved_commit.clone().unwrap();

    // Another Epic's lead cannot see it.
    assert_eq!(
        error_code(
            &world
                .resolve(
                    world.lead_b,
                    first,
                    blocked.id,
                    TopologyAttemptAction::Inspect,
                    "b",
                    None
                )
                .await
                .unwrap_err()
        ),
        "not_found_in_scope"
    );
    // The owning lead inspects, cannot discard, and retries.
    let inspected = world
        .resolve(
            world.lead_a,
            first,
            blocked.id,
            TopologyAttemptAction::Inspect,
            "look",
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        inspected.report.unwrap().preserved_commit.as_deref(),
        Some(preserved.as_str())
    );
    assert_eq!(
        error_code(
            &world
                .resolve(
                    world.lead_a,
                    first,
                    blocked.id,
                    TopologyAttemptAction::Discard,
                    "drop",
                    Some(&preserved)
                )
                .await
                .unwrap_err()
        ),
        "discard_requires_manager"
    );
    let retried = world
        .resolve(
            world.lead_a,
            first,
            blocked.id,
            TopologyAttemptAction::Retry,
            "again",
            None,
        )
        .await
        .unwrap();
    assert_eq!(retried.attempt.resolution.as_deref(), Some("retried"));
    let retry = world.harness.attempt(first, "A", 0, 2).await;
    assert_eq!(retry.base_commit, preserved);

    // The owning lead accepts preserved work in a second execution.
    let second = world
        .execute(world.lead_a, &flow, world.epic_a, "second")
        .await
        .unwrap()
        .execution_id;
    let blocked = world.blocked_on_preserved_work(second).await;
    let accepted = world
        .resolve(
            world.lead_a,
            second,
            blocked.id,
            TopologyAttemptAction::Accept,
            "keep",
            None,
        )
        .await
        .unwrap();
    assert_eq!(accepted.attempt.resolution.as_deref(), Some("accepted"));

    // Every lead action is audited with its actor.
    let lead = Some(world.lead_a.to_string());
    let first_events = world.events(first).await;
    for kind in [
        "preserved_work_inspected",
        "agent_request_refused",
        "preserved_work_retried",
    ] {
        assert!(
            first_events.contains(&(kind.to_owned(), "epic_lead".to_owned(), lead.clone())),
            "{kind} audited"
        );
    }
    assert!(world.events(second).await.contains(&(
        "preserved_work_accepted".to_owned(),
        "epic_lead".to_owned(),
        lead
    )));
    let resolved_by: (String, String) = {
        let store = world.harness.executor.store.lock().await;
        store
            .conn
            .query_row(
                "SELECT resolved_by_kind,resolved_by_session_id FROM topology_node_attempts WHERE id=?1",
                [blocked.id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap()
    };
    assert_eq!(
        resolved_by,
        ("epic_lead".to_owned(), world.lead_a.to_string())
    );
}

// ─── A10: whole-workflow custody plan at upsert ────────────────────────────

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn t4_a10_upsert_rejects_unresolved_custody_plan() {
    let world = World::new(automation()).await;
    let fan_in = definition(
        &[("A", &luna()), ("B", &luna()), ("C", &luna())],
        &[("A", "C"), ("B", "C")],
    );
    let refused = world
        .upsert(world.lead_a, &upsert_request("fan-in", fan_in.clone()))
        .await
        .unwrap_err();
    assert_eq!(error_code(&refused), "invalid_definition");
    let reported = diagnostics(&refused);
    assert_eq!(reported.len(), 1);
    assert!(reported[0].contains("custody"), "{reported:?}");
    let mut probe = upsert_request("fan-in", fan_in);
    probe.validate_only = true;
    assert_eq!(
        world
            .upsert(world.lead_a, &probe)
            .await
            .unwrap()
            .diagnostics,
        reported
    );
    let rows: i64 = {
        let store = world.harness.executor.store.lock().await;
        store
            .conn
            .query_row(
                "SELECT count(*) FROM topologies WHERE name='fan-in'",
                [],
                |row| row.get(0),
            )
            .unwrap()
    };
    assert_eq!(rows, 0);
}

// ─── A11: discard needs the manager and the exact preserved commit ─────────

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn t4_a11_manager_discard_requires_exact_confirmation() {
    let world = World::new(automation()).await;
    let flow = world
        .upsert(
            world.lead_a,
            &upsert_request("discardable", definition(&[("A", &luna())], &[])),
        )
        .await
        .unwrap();
    let run = world
        .execute(world.lead_a, &flow, world.epic_a, "discard")
        .await
        .unwrap()
        .execution_id;
    let blocked = world.blocked_on_preserved_work(run).await;
    let preserved = blocked.preserved_commit.clone().unwrap();
    let preserved_ref = blocked.preserved_ref.clone().unwrap();
    let manager = world.manager;
    for (confirm, code) in [
        (None, "invalid_params"),
        (Some("0".repeat(40)), "preserved_commit_mismatch"),
    ] {
        assert_eq!(
            error_code(
                &world
                    .resolve(
                        manager,
                        run,
                        blocked.id,
                        TopologyAttemptAction::Discard,
                        "d",
                        confirm.as_deref()
                    )
                    .await
                    .unwrap_err()
            ),
            code
        );
    }
    assert_eq!(
        error_code(
            &world
                .resolve(
                    manager,
                    run,
                    blocked.id,
                    TopologyAttemptAction::Inspect,
                    "i",
                    Some(&preserved)
                )
                .await
                .unwrap_err()
        ),
        "invalid_params"
    );
    assert_eq!(
        git(&world.harness.repo, &["rev-parse", &preserved_ref]),
        preserved
    );
    let discarded = world
        .resolve(
            manager,
            run,
            blocked.id,
            TopologyAttemptAction::Discard,
            "confirmed",
            Some(&preserved),
        )
        .await
        .unwrap();
    assert_eq!(discarded.attempt.resolution.as_deref(), Some("discarded"));
    let listed = git(
        &world.harness.repo,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/rsi/topology-preserved",
        ],
    );
    assert_eq!(listed, "");
    assert!(world.events(run).await.contains(&(
        "preserved_work_discarded".to_owned(),
        "manager".to_owned(),
        Some(manager.to_string())
    )));

    // The operator surface enforces the same three cases.
    let operator_run = world
        .execute(world.lead_a, &flow, world.epic_a, "operator")
        .await
        .unwrap()
        .execution_id;
    let blocked = world.blocked_on_preserved_work(operator_run).await;
    let preserved = blocked.preserved_commit.clone().unwrap();
    let operator = |confirm: Option<String>, key: &str| ResolveTopologyAttemptParams {
        execution_id: operator_run,
        attempt_id: blocked.id,
        action: TopologyAttemptAction::Discard,
        expected_row_version: 0,
        idempotency_key: key.into(),
        confirm_preserved_commit: confirm,
    };
    let version = world.harness.row_version(operator_run).await;
    for (confirm, code) in [
        (None, "invalid_params"),
        (Some("f".repeat(40)), "preserved_commit_mismatch"),
    ] {
        let mut params = operator(confirm, code);
        params.expected_row_version = version;
        assert_eq!(
            error_code(
                &world
                    .harness
                    .executor
                    .resolve_attempt(&params)
                    .await
                    .unwrap_err()
            ),
            code
        );
    }
    let mut params = operator(Some(preserved), "operator-confirmed");
    params.expected_row_version = version;
    let discarded = world
        .harness
        .executor
        .resolve_attempt(&params)
        .await
        .unwrap();
    assert_eq!(discarded.attempt.resolution.as_deref(), Some("discarded"));
}

// ─── plan §5.3 bulk fan-out and parallel cap ───────────────────────────────

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn t4_bulk_fanout_requires_openrouter_at_the_operator_threshold() {
    let world = World::new(automation()).await;
    let codex_fanout = definition(
        &[
            ("A", &luna()),
            ("B", &luna()),
            ("C", &luna()),
            ("D", &luna()),
        ],
        &[],
    );
    let refused = world
        .upsert(
            world.lead_a,
            &upsert_request("wide-codex", codex_fanout.clone()),
        )
        .await
        .unwrap_err();
    assert_eq!(error_code(&refused), "policy_refused");
    assert_eq!(
        diagnostics(&refused),
        vec![
            "layer 0: 4 parallel Task session nodes need provider openrouter \
             (topology_bulk_fanout_min_openrouter=4); not openrouter: A, B, C, D"
        ]
    );
    // Operator knob 0 turns the rule off; OpenRouter satisfies it.
    let off = AgentKnobs {
        bulk_fanout_min_openrouter: 0,
        ..KNOBS
    };
    let accepted = world
        .upsert_with(
            world.lead_a,
            &upsert_request("wide-codex", codex_fanout),
            off,
        )
        .await
        .unwrap();
    assert_eq!(accepted.revision, Some(1));
    let wide = world
        .upsert(
            world.lead_a,
            &upsert_request(
                "wide-glm",
                definition(
                    &[("A", &glm()), ("B", &glm()), ("C", &glm()), ("D", &glm())],
                    &[],
                ),
            ),
        )
        .await
        .unwrap();

    // At most three session nodes of an agent execution are in flight.
    let run = world
        .execute(world.lead_a, &wide, world.epic_a, "wide")
        .await
        .unwrap()
        .execution_id;
    assert_eq!(
        world.harness.executor.advance(run).await.unwrap(),
        Step::Wait
    );
    assert_eq!(world.harness.attempts(run).await.len(), 3);
    assert_eq!(world.harness.launches().len(), 3);
    let first = world.harness.attempts(run).await[0].clone();
    world
        .harness
        .commit_and_complete(first.session_id, "first-done");
    assert_eq!(
        world.harness.executor.advance(run).await.unwrap(),
        Step::Wait
    );
    assert_eq!(world.harness.attempts(run).await.len(), 4);
    assert_eq!(world.harness.launches().len(), 4);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn t4_reviewer_vendor_family_must_differ_from_author() {
    let sol = ManagerLaunchChoiceV2 {
        provider: SessionProvider::Codex,
        model: "gpt-6-sol".into(),
        effort: Some("high".into()),
    };
    let opus = ManagerLaunchChoiceV2 {
        provider: SessionProvider::Claude,
        model: "claude-opus-5-5".into(),
        effort: Some("high".into()),
    };
    assert!(agent::reviewer_family_independent(&opus, &sol));
    assert!(agent::reviewer_family_independent(&luna(), &glm()));
    // Same family, or an unclassifiable reviewer, cannot prove independence.
    assert!(!agent::reviewer_family_independent(&luna(), &sol));
    let unknown = ManagerLaunchChoiceV2 {
        provider: SessionProvider::OpenRouter,
        model: "unknown/model".into(),
        effort: None,
    };
    assert!(!agent::reviewer_family_independent(&opus, &unknown));
}

// ─── §5.3: agent executions run at most 3 session nodes at once ────────────

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn t4_agent_execution_runs_at_most_three_session_nodes_at_once() {
    let world = World::new(automation()).await;
    let glm = glm();
    let wide = [
        ("A", &glm),
        ("B", &glm),
        ("C", &glm),
        ("D", &glm),
        ("E", &glm),
    ];
    let flow = world
        .upsert(
            world.lead_a,
            &upsert_request("wide", definition(&wide, &[])),
        )
        .await
        .unwrap();
    let run = world
        .execute(world.lead_a, &flow, world.epic_a, "wide-run")
        .await
        .unwrap()
        .execution_id;
    assert_eq!(
        world.harness.executor.advance(run).await.unwrap(),
        Step::Wait
    );
    assert_eq!(
        world.harness.launches().len(),
        agent::AGENT_MAX_PARALLEL_NODES
    );
    assert_eq!(
        world.harness.attempts(run).await.len(),
        agent::AGENT_MAX_PARALLEL_NODES
    );
}

// ─── Review round 1: upsert idempotency, execute recheck, ledger ──────────

/// Stored `(name, revision, definition_digest)` of every topology the lead
/// of Epic A owns.
async fn owned_by_epic_a(world: &World) -> Vec<(String, i64, String)> {
    let store = world.harness.executor.store.lock().await;
    let mut statement = store
        .conn
        .prepare(
            "SELECT name,revision,definition_digest FROM topologies \
             WHERE owner_kind='epic' AND epic_id=?1 ORDER BY name",
        )
        .unwrap();
    statement
        .query_map([world.epic_a.to_string()], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

/// Ledger `(verb, caller_kind, outcome, code, idempotency_key)` rows of one caller.
async fn ledger(
    world: &World,
    caller: Uuid,
) -> Vec<(
    String,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
)> {
    let store = world.harness.executor.store.lock().await;
    let mut statement = store
        .conn
        .prepare(
            "SELECT verb,caller_kind,outcome,code,idempotency_key FROM topology_agent_requests \
             WHERE caller_session_id=?1 ORDER BY id",
        )
        .unwrap();
    statement
        .query_map([caller.to_string()], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

async fn execution_count(world: &World) -> i64 {
    let store = world.harness.executor.store.lock().await;
    store
        .conn
        .query_row("SELECT count(*) FROM topology_executions", [], |row| {
            row.get(0)
        })
        .unwrap()
}

/// R1 [upsert-idempotency]: a key binds the first request. An identical
/// replay returns the original receipt; a changed name, definition or
/// `expected_revision` under the same key is `idempotency_conflict` and the
/// stored topology stays exactly as first written.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn t4_r1_upsert_key_replays_identical_and_conflicts_on_changed_content() {
    let world = World::new(automation()).await;
    let original = upsert_request("keyed", definition(&[("A", &luna())], &[]));
    let first = world.upsert(world.lead_a, &original).await.unwrap();
    assert_eq!(first.revision, Some(1));
    assert!(!first.deduplicated);

    let replay = world.upsert(world.lead_a, &original).await.unwrap();
    assert!(replay.deduplicated);
    assert_eq!(replay.topology_id, first.topology_id);
    assert_eq!(replay.revision, first.revision);
    assert_eq!(replay.definition_digest, first.definition_digest);

    let mut renamed = original.clone();
    renamed.name = "keyed-renamed".into();
    let mut redefined = original.clone();
    redefined.definition = definition(&[("A", &luna()), ("B", &luna())], &[("A", "B")]);
    redefined.expected_revision = Some(1);
    let mut refenced = original.clone();
    refenced.expected_revision = Some(1);
    for changed in [renamed, redefined, refenced] {
        let error = world.upsert(world.lead_a, &changed).await.unwrap_err();
        assert_eq!(
            error_code(&error),
            "idempotency_conflict",
            "{}",
            changed.name
        );
    }
    assert_eq!(
        owned_by_epic_a(&world).await,
        vec![("keyed".to_owned(), 1, first.definition_digest.clone())]
    );

    // A fresh key revises normally.
    let mut revised = upsert_request(
        "keyed",
        definition(&[("A", &luna()), ("B", &luna())], &[("A", "B")]),
    );
    revised.expected_revision = Some(1);
    revised.idempotency_key = "upsert-keyed-v2".into();
    let second = world.upsert(world.lead_a, &revised).await.unwrap();
    assert_eq!(second.revision, Some(2));
    assert_eq!(
        owned_by_epic_a(&world).await,
        vec![("keyed".to_owned(), 2, second.definition_digest.clone())]
    );

    let rows = ledger(&world, world.lead_a).await;
    let lead = Some("epic_lead".to_owned());
    let bind = |key: &str| {
        (
            "upsert".to_owned(),
            lead.clone(),
            "accepted".to_owned(),
            None,
            Some(key.to_owned()),
        )
    };
    let conflict = (
        "upsert".to_owned(),
        lead.clone(),
        "refused".to_owned(),
        Some("idempotency_conflict".to_owned()),
        None,
    );
    assert_eq!(
        rows,
        vec![
            bind("upsert-keyed"),
            (
                "upsert".to_owned(),
                lead.clone(),
                "deduplicated".to_owned(),
                None,
                None
            ),
            conflict.clone(),
            conflict.clone(),
            conflict,
            bind("upsert-keyed-v2"),
        ]
    );
}

/// R1 [execute-stale-definition]: the accepting lock reloads the topology.
/// Unsharing it or revising it while the Git work runs refuses and persists
/// no execution; an unchanged topology is accepted.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn t4_r1_execute_rechecks_topology_after_custody_resolution() {
    let world = World::new(automation()).await;
    let request = |topology: Uuid, digest: &str, key: &str| AgentTopologyExecuteRequestV1 {
        project_id: None,
        on_call: None,
        topology_id: topology,
        expected_digest: digest.to_owned(),
        epic_id: world.epic_a,
        inputs: serde_json::Value::Null,
        base_commit: Some(world.harness.base.clone()),
        idempotency_key: key.into(),
    };
    let prepare = |request: AgentTopologyExecuteRequestV1| {
        let store = std::sync::Arc::clone(&world.harness.executor.store);
        let caller = world.lead_a;
        async move {
            match agent::prepare_execute(&store, KNOBS, caller, &request)
                .await
                .unwrap()
            {
                agent::Prepared::Ready(prepared) => (request, *prepared),
                agent::Prepared::Replay(_) => panic!("fresh key must not replay"),
            }
        }
    };
    let accept = |request: &AgentTopologyExecuteRequestV1, prepared| {
        let store = std::sync::Arc::clone(&world.harness.executor.store);
        let caller = world.lead_a;
        let request = request.clone();
        async move {
            let store = store.lock().await;
            agent::accept_prepared(&store, KNOBS, caller, &request, prepared)
        }
    };

    // Unshared during the Git work.
    let operator = Topology {
        id: Uuid::new_v4(),
        name: "operator-stale".into(),
        definition: definition(&[("A", &luna())], &[]),
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };
    let operator_digest = {
        let store = world.harness.executor.store.lock().await;
        store.insert_topology(&operator).unwrap();
        assert!(agent::set_shared(&store, operator.id, true).unwrap());
        agent::list(&store, world.lead_a, &AgentTopologyListRequestV1::default())
            .unwrap()
            .topologies
            .into_iter()
            .find(|topology| topology.topology_id == operator.id)
            .unwrap()
            .definition_digest
    };
    let (unshared, prepared) = prepare(request(operator.id, &operator_digest, "unshared")).await;
    {
        let store = world.harness.executor.store.lock().await;
        assert!(agent::set_shared(&store, operator.id, false).unwrap());
    }
    let error = accept(&unshared, prepared).await.unwrap_err();
    assert_eq!(error_code(&error), "topology_not_visible");
    assert_eq!(execution_count(&world).await, 0);

    // Revised during the Git work.
    let flow = world
        .upsert(
            world.lead_a,
            &upsert_request("stale", definition(&[("A", &luna())], &[])),
        )
        .await
        .unwrap();
    let flow_id = flow.topology_id.unwrap();
    let (edited, prepared) = prepare(request(flow_id, &flow.definition_digest, "edited")).await;
    let mut revision = upsert_request("stale", definition(&[("A", &glm())], &[]));
    revision.expected_revision = Some(1);
    revision.idempotency_key = "stale-v2".into();
    let revised = world.upsert(world.lead_a, &revision).await.unwrap();
    assert_eq!(revised.revision, Some(2));
    let error = accept(&edited, prepared).await.unwrap_err();
    assert_eq!(error_code(&error), "topology_changed");
    assert_eq!(execution_count(&world).await, 0);

    // Unchanged: accepted.
    let (steady, prepared) = prepare(request(flow_id, &revised.definition_digest, "steady")).await;
    let accepted = accept(&steady, prepared).await.unwrap();
    assert!(!accepted.result.deduplicated);
    assert_eq!(execution_count(&world).await, 1);
    assert_eq!(
        world
            .harness
            .row_version(accepted.result.execution_id)
            .await,
        1
    );
}

/// R1 [durable-audit-gap]: every agent topology call after token resolution
/// appends one ledger row, including refusals of callers without authority
/// and pre-admission refusals.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn t4_r1_upsert_list_and_refusals_each_append_one_ledger_row() {
    let world = World::new(automation()).await;
    let request = upsert_request("audited", definition(&[("A", &luna())], &[]));
    world.upsert(world.worker, &request).await.unwrap_err();
    let mut probe = request.clone();
    probe.validate_only = true;
    world.upsert(world.lead_a, &probe).await.unwrap();
    assert_eq!(
        ledger(&world, world.worker).await,
        vec![(
            "upsert".to_owned(),
            None,
            "refused".to_owned(),
            Some("authority_denied".to_owned()),
            None
        )]
    );
    assert_eq!(
        ledger(&world, world.lead_a).await,
        vec![(
            "upsert".to_owned(),
            Some("epic_lead".to_owned()),
            "accepted".to_owned(),
            None,
            None
        )]
    );
}

// ─── Review round 2: live authority under the mutation lock ───────────────

/// Replace the manager's scope with exactly `epics`, then re-grant `policy`
/// against the new scope version (the operator's scope + policy surfaces).
async fn rescope(world: &World, epics: Vec<Uuid>, policy: ManagerPolicyV2) {
    {
        let store = world.harness.executor.store.lock().await;
        let scope = store.get_harness_manager(world.project).unwrap().unwrap();
        store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                group_ids: vec![],
                project_id: world.project,
                session_id: world.manager,
                epic_ids: Some(epics),
                expected_row_version: scope.row_version,
            })
            .unwrap();
    }
    world.grant(policy).await;
}

/// Ledger rows of `caller` for one verb.
async fn ledger_for(
    world: &World,
    caller: Uuid,
    verb: &str,
) -> Vec<(
    String,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
)> {
    ledger(world, caller)
        .await
        .into_iter()
        .filter(|row| row.0 == verb)
        .collect()
}

#[derive(Clone, Copy, Debug)]
enum Revocation {
    EpicOutOfScope,
    AutomationRevoked,
    ManagerPaused,
    LeadReplaced,
}

/// R2 [resolve-authority-race]: authority is re-checked under the guard
/// that records the resolution. Each revocation landing during the
/// resolution's await refuses, leaves the attempt blocked on its preserved
/// work, and appends one refused ledger row; the unchanged case retries.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn t4_r2_resolve_rechecks_live_authority_under_the_recording_lock() {
    let cases: [(Option<Revocation>, &str, Option<&str>); 5] = [
        (
            Some(Revocation::EpicOutOfScope),
            "not_found_in_scope",
            Some("manager"),
        ),
        (
            Some(Revocation::AutomationRevoked),
            "capability_denied",
            None,
        ),
        (Some(Revocation::ManagerPaused), "paused", Some("manager")),
        (Some(Revocation::LeadReplaced), "authority_denied", None),
        (None, "", None),
    ];
    for (revocation, code, kind) in cases {
        let world = World::new(automation()).await;
        let flow = world
            .upsert(
                world.lead_a,
                &upsert_request("raced", definition(&[("A", &luna())], &[])),
            )
            .await
            .unwrap();
        let run = world
            .execute(world.lead_a, &flow, world.epic_a, "raced")
            .await
            .unwrap()
            .execution_id;
        let blocked = world.blocked_on_preserved_work(run).await;
        let caller = if matches!(revocation, Some(Revocation::LeadReplaced)) {
            world.lead_a
        } else {
            world.manager
        };
        let entered = std::sync::Arc::new(tokio::sync::Notify::new());
        let go = std::sync::Arc::new(tokio::sync::Notify::new());
        *world.harness.executor.effects.record_hold.lock().unwrap() =
            Some((std::sync::Arc::clone(&entered), std::sync::Arc::clone(&go)));
        let params = ResolveTopologyAttemptParams {
            execution_id: run,
            attempt_id: blocked.id,
            action: TopologyAttemptAction::Retry,
            expected_row_version: world.harness.row_version(run).await,
            idempotency_key: "raced-retry".into(),
            confirm_preserved_commit: None,
        };
        let task = {
            let executor = world.harness.executor.clone();
            tokio::spawn(async move { agent::resolve(&executor, caller, &params).await })
        };
        entered.notified().await;
        match revocation {
            Some(Revocation::EpicOutOfScope) => {
                rescope(&world, vec![world.epic_b], automation()).await;
            }
            Some(Revocation::AutomationRevoked) => {
                world.grant(policy(&[], vec![luna(), glm()], 64)).await;
            }
            Some(Revocation::ManagerPaused) => {
                world
                    .grant(ManagerPolicyV2 {
                        paused: true,
                        ..automation()
                    })
                    .await;
            }
            Some(Revocation::LeadReplaced) => {
                let store = world.harness.executor.store.lock().await;
                store
                    .conn
                    .execute(
                        "UPDATE sessions SET lead_session_id=?1 WHERE id=?2",
                        [Uuid::new_v4().to_string(), world.epic_a.to_string()],
                    )
                    .unwrap();
            }
            None => {}
        }
        go.notify_one();
        let outcome = task.await.unwrap();
        let attempts = world.harness.attempts(run).await;
        let first = world.harness.attempt(run, "A", 0, 1).await;
        let rows = ledger_for(&world, caller, "resolve_attempt").await;
        if let Some(revocation) = revocation {
            let error = outcome.unwrap_err();
            assert_eq!(error_code(&error), code, "{revocation:?}");
            assert_eq!(attempts.len(), 1, "{revocation:?}: no retry attempt");
            assert_eq!(first.status, AttemptStatus::Blocked, "{revocation:?}");
            assert_eq!(
                first.failure_class.as_deref(),
                Some("preserved_work"),
                "{revocation:?}"
            );
            assert_eq!(first.resolution, None, "{revocation:?}");
            assert_eq!(first.preserved_commit, blocked.preserved_commit);
            assert_eq!(
                git(
                    &world.harness.repo,
                    &["rev-parse", blocked.preserved_ref.as_deref().unwrap()]
                ),
                blocked.preserved_commit.clone().unwrap(),
                "{revocation:?}: the preservation ref is kept"
            );
            assert_eq!(
                rows,
                vec![(
                    "resolve_attempt".to_owned(),
                    kind.map(str::to_owned),
                    "refused".to_owned(),
                    Some(code.to_owned()),
                    None
                )],
                "{revocation:?}"
            );
        } else {
            let response = outcome.unwrap();
            assert!(!response.deduplicated);
            assert_eq!(first.resolution.as_deref(), Some("retried"));
            assert_eq!(attempts.len(), 2);
            assert_eq!(
                rows,
                vec![(
                    "resolve_attempt".to_owned(),
                    Some("manager".to_owned()),
                    "accepted".to_owned(),
                    None,
                    None
                )]
            );
        }
    }
}

/// R2 [execute-replay-bypasses-scope]: an exact Execute replay passes the
/// same live authority and Epic scope as a fresh request. In scope it
/// replays the original execution; after the manager is rescoped to another
/// Epic (still holding `Automation`) it is refused with the code a fresh
/// request gets, and the ledger records the refusal.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn t4_r2_execute_replay_is_authorized_before_it_replays() {
    let world = World::new(automation()).await;
    let mut request = upsert_request("replayed", definition(&[("A", &luna())], &[]));
    request.scope = AgentTopologyScopeV1::Manager;
    let flow = world.upsert(world.manager, &request).await.unwrap();
    let first = world
        .execute(world.manager, &flow, world.epic_a, "replayed")
        .await
        .unwrap();
    let replay = world
        .execute(world.manager, &flow, world.epic_a, "replayed")
        .await
        .unwrap();
    assert!(replay.deduplicated);
    assert_eq!(replay.execution_id, first.execution_id);
    assert_eq!(replay.base_commit, first.base_commit);

    rescope(&world, vec![world.epic_b], automation()).await;
    let refused = world
        .execute(world.manager, &flow, world.epic_a, "replayed")
        .await
        .unwrap_err();
    let fresh = world
        .execute(world.manager, &flow, world.epic_a, "fresh-after-rescope")
        .await
        .unwrap_err();
    assert_eq!(error_code(&refused), "not_found_in_scope");
    assert_eq!(error_code(&fresh), error_code(&refused));
    // Still authorized elsewhere: the same flow runs on the in-scope Epic.
    let elsewhere = world
        .execute(world.manager, &flow, world.epic_b, "in-scope")
        .await
        .unwrap();
    assert!(!elsewhere.deduplicated);

    let outcomes = ledger_for(&world, world.manager, "execute")
        .await
        .into_iter()
        .map(|row| (row.2, row.3))
        .collect::<Vec<_>>();
    assert_eq!(
        outcomes,
        vec![
            ("accepted".to_owned(), None),
            ("deduplicated".to_owned(), None),
            ("refused".to_owned(), Some("not_found_in_scope".to_owned())),
            ("refused".to_owned(), Some("not_found_in_scope".to_owned())),
            ("accepted".to_owned(), None),
        ]
    );
}

/// R3 [execute-policy-drift]: a grant changed during base resolution must
/// be read again by the accepting guard before it persists an execution.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn t4_r3_execute_rechecks_allowed_launches_after_custody_resolution() {
    let world = World::new(automation()).await;
    let flow = world
        .upsert(
            world.lead_a,
            &upsert_request("grant-race", definition(&[("A", &luna())], &[])),
        )
        .await
        .unwrap();
    let request = AgentTopologyExecuteRequestV1 {
        project_id: None,
        on_call: None,
        topology_id: flow.topology_id.unwrap(),
        expected_digest: flow.definition_digest,
        epic_id: world.epic_a,
        inputs: serde_json::Value::Null,
        base_commit: Some(world.harness.base.clone()),
        idempotency_key: "grant-race".into(),
    };
    let prepared =
        match agent::prepare_execute(&world.harness.executor.store, KNOBS, world.lead_a, &request)
            .await
            .unwrap()
        {
            agent::Prepared::Ready(prepared) => *prepared,
            agent::Prepared::Replay(_) => panic!("fresh key must prepare"),
        };
    let mut changed = automation();
    changed.allowed_launches = vec![glm()];
    world.grant(changed).await;
    let store = world.harness.executor.store.lock().await;
    let error =
        agent::accept_prepared(&store, KNOBS, world.lead_a, &request, prepared).unwrap_err();
    assert_eq!(error_code(&error), "policy_refused");
    assert_eq!(
        store
            .conn
            .query_row("SELECT count(*) FROM topology_executions", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    drop(store);
}

/// R3 [execute-policy-drift]: the live fan-out threshold is read again under
/// the accepting guard, even when the topology definition did not change.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn t4_r3_execute_rechecks_fanout_knob_after_custody_resolution() {
    let world = World::new(automation()).await;
    let flow = world
        .upsert(
            world.lead_a,
            &upsert_request(
                "fanout-race",
                definition(&[("A", &luna()), ("B", &luna()), ("C", &luna())], &[]),
            ),
        )
        .await
        .unwrap();
    let request = |key: &str| AgentTopologyExecuteRequestV1 {
        project_id: None,
        on_call: None,
        topology_id: flow.topology_id.unwrap(),
        expected_digest: flow.definition_digest.clone(),
        epic_id: world.epic_a,
        inputs: serde_json::Value::Null,
        base_commit: Some(world.harness.base.clone()),
        idempotency_key: key.into(),
    };
    let changed = request("fanout-changed");
    let prepared =
        match agent::prepare_execute(&world.harness.executor.store, KNOBS, world.lead_a, &changed)
            .await
            .unwrap()
        {
            agent::Prepared::Ready(prepared) => *prepared,
            agent::Prepared::Replay(_) => panic!("fresh key must prepare"),
        };
    let store = world.harness.executor.store.lock().await;
    let error = agent::accept_prepared(
        &store,
        AgentKnobs {
            bulk_fanout_min_openrouter: 2,
            ..KNOBS
        },
        world.lead_a,
        &changed,
        prepared,
    )
    .unwrap_err();
    assert_eq!(error_code(&error), "policy_refused");
    assert_eq!(
        store
            .conn
            .query_row("SELECT count(*) FROM topology_executions", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    drop(store);

    let steady = request("fanout-steady");
    let prepared =
        match agent::prepare_execute(&world.harness.executor.store, KNOBS, world.lead_a, &steady)
            .await
            .unwrap()
        {
            agent::Prepared::Ready(prepared) => *prepared,
            agent::Prepared::Replay(_) => panic!("fresh key must prepare"),
        };
    let store = world.harness.executor.store.lock().await;
    let accepted = agent::accept_prepared(&store, KNOBS, world.lead_a, &steady, prepared).unwrap();
    assert!(!accepted.result.deduplicated);
    assert_eq!(
        store
            .conn
            .query_row("SELECT count(*) FROM topology_executions", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    drop(store);
}

/// The production execute path records one refused request after a knob
/// change between its two guarded policy reads.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn t4_r3_execute_policy_race_records_refused_request() {
    let world = World::new(automation()).await;
    let flow = world
        .upsert(
            world.lead_a,
            &upsert_request(
                "fanout-ledger",
                definition(&[("A", &luna()), ("B", &luna()), ("C", &luna())], &[]),
            ),
        )
        .await
        .unwrap();
    let request = AgentTopologyExecuteRequestV1 {
        project_id: None,
        on_call: None,
        topology_id: flow.topology_id.unwrap(),
        expected_digest: flow.definition_digest,
        epic_id: world.epic_a,
        inputs: serde_json::Value::Null,
        base_commit: Some(world.harness.base.clone()),
        idempotency_key: "fanout-ledger".into(),
    };
    let reads = std::sync::atomic::AtomicUsize::new(0);
    let error = agent::execute(
        &world.harness.executor.store,
        || {
            if reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed) == 0 {
                KNOBS
            } else {
                AgentKnobs {
                    bulk_fanout_min_openrouter: 2,
                    ..KNOBS
                }
            }
        },
        world.lead_a,
        &request,
    )
    .await
    .unwrap_err();
    assert_eq!(error_code(&error), "policy_refused");
    assert_eq!(reads.load(std::sync::atomic::Ordering::Relaxed), 2);
    assert_eq!(execution_count(&world).await, 0);
    let rows = ledger_for(&world, world.lead_a, "execute").await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].2, "refused");
    assert_eq!(rows[0].3.as_deref(), Some("policy_refused"));
}

/// R3 [cross-scope-resolution-audit]: a foreign lead's refused call may
/// enter its request ledger but cannot append to the foreign execution.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn t4_r3_cross_scope_resolve_and_interrupt_only_append_request_ledger() {
    let world = World::new(automation()).await;
    let flow = world
        .upsert(
            world.lead_a,
            &upsert_request("scope-audit", definition(&[("A", &luna())], &[])),
        )
        .await
        .unwrap();
    let execution = world
        .execute(world.lead_a, &flow, world.epic_a, "scope-audit")
        .await
        .unwrap()
        .execution_id;
    let before = {
        let store = world.harness.executor.store.lock().await;
        store
            .conn
            .query_row(
                "SELECT count(*) FROM topology_events WHERE execution_id=?1",
                [execution.to_string()],
                |row| row.get::<_, i64>(0),
            )
            .unwrap()
    };
    let interrupted = world
        .interrupt(world.lead_b, execution, 1, "foreign-interrupt")
        .await
        .unwrap_err();
    assert_eq!(error_code(&interrupted), "not_found_in_scope");
    let resolved = world
        .resolve(
            world.lead_b,
            execution,
            Uuid::new_v4(),
            TopologyAttemptAction::Inspect,
            "foreign-resolve",
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(error_code(&resolved), "not_found_in_scope");
    let store = world.harness.executor.store.lock().await;
    let after = store
        .conn
        .query_row(
            "SELECT count(*) FROM topology_events WHERE execution_id=?1",
            [execution.to_string()],
            |row| row.get::<_, i64>(0),
        )
        .unwrap();
    assert_eq!(after, before);
    drop(store);
    for verb in ["interrupt", "resolve_attempt"] {
        let rows = ledger_for(&world, world.lead_b, verb).await;
        assert_eq!(rows.len(), 1, "{verb}");
        assert_eq!(rows[0].2, "refused");
        assert_eq!(rows[0].3.as_deref(), Some("not_found_in_scope"));
    }
}

// ─── #1235: the global seat holds the topology verbs in its grant ──────────

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn global_seat_runs_topology_in_a_granted_project_and_is_refused_outside_it() {
    use rsi_common::global_manager::{
        ConfigureGlobalManagerRequestV1, MANAGER_PROJECT_NOT_IN_SCOPE,
    };
    let world = World::new(automation()).await;
    let (hub, seat, foreign, foreign_epic) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    {
        let store = world.harness.executor.store.lock().await;
        let now = chrono::Utc::now();
        for (id, name) in [(hub, "Hub"), (foreign, "Foreign")] {
            store
                .insert_project(&Project {
                    id,
                    name: name.into(),
                    path: Some(world.harness.repo.clone()),
                    description: None,
                    color: Project::DEFAULT_COLOR.into(),
                    context_files: None,
                    created_at: now,
                    updated_at: now,
                })
                .unwrap();
        }
        let foreign_group = Uuid::new_v4();
        for (id, project, kind, parent) in [
            (seat, hub, SessionKind::Standard, None),
            (foreign_group, foreign, SessionKind::Group, None),
            (
                foreign_epic,
                foreign,
                SessionKind::Epic,
                Some(foreign_group),
            ),
        ] {
            let mut row =
                crate::session::agent_verbs::tests::test_session(id, world.harness.repo.clone());
            row.project_id = Some(project);
            row.session_kind = kind;
            row.parent_id = parent;
            row.status = SessionStatus::Running;
            store.insert_session(&row).unwrap();
        }
        store
            .configure_global_manager(
                &ConfigureGlobalManagerRequestV1 {
                    session_id: seat,
                    project_ids: vec![world.project],
                    allowed_launches: vec![luna()],
                    project_policy: automation(),
                    expected_grant_version: 0,
                    idempotency_key: "global-topology".into(),
                },
                "operator:test",
            )
            .unwrap();
    }
    // Upsert and execute on Epic C, which the PM's own scope does not hold:
    // the global covers the whole granted project.
    let mut on_c = upsert_request("global-flow", definition(&[("A", &luna())], &[]));
    on_c.project_id = Some(world.project);
    on_c.epic_id = Some(world.epic_c);
    let flow = world.upsert(seat, &on_c).await.unwrap();
    let request = AgentTopologyExecuteRequestV1 {
        project_id: Some(world.project),
        on_call: None,
        topology_id: flow.topology_id.unwrap(),
        expected_digest: flow.definition_digest.clone(),
        epic_id: world.epic_c,
        inputs: serde_json::Value::Null,
        base_commit: Some(world.harness.base.clone()),
        idempotency_key: "global-run".into(),
    };
    let run = agent::execute(&world.harness.executor.store, || KNOBS, seat, &request)
        .await
        .unwrap()
        .result;
    let view = world.get(seat, run.execution_id).await.unwrap();
    assert_eq!(view.execution.requested_by_kind, "manager");
    // The launch gate admits the node under the global grant.
    assert_eq!(
        world
            .harness
            .executor
            .advance(run.execution_id)
            .await
            .unwrap(),
        Step::Wait
    );
    let attempt = world.harness.attempt(run.execution_id, "A", 0, 1).await;
    assert_eq!(attempt.status, AttemptStatus::Running);

    // A project outside the grant is refused on every topology verb.
    let mut outside = upsert_request("foreign-flow", definition(&[("A", &luna())], &[]));
    outside.project_id = Some(foreign);
    outside.epic_id = Some(foreign_epic);
    assert_eq!(
        error_code(&world.upsert(seat, &outside).await.unwrap_err()),
        MANAGER_PROJECT_NOT_IN_SCOPE
    );
    let mut list = AgentTopologyListRequestV1::default();
    list.project_id = Some(foreign);
    assert_eq!(
        error_code(&world.list(seat, &list).await.unwrap_err()),
        MANAGER_PROJECT_NOT_IN_SCOPE
    );
    let mut foreign_run = request.clone();
    foreign_run.project_id = Some(foreign);
    foreign_run.epic_id = foreign_epic;
    foreign_run.idempotency_key = "foreign-run".into();
    assert_eq!(
        error_code(
            &agent::execute(&world.harness.executor.store, || KNOBS, seat, &foreign_run)
                .await
                .unwrap_err()
        ),
        MANAGER_PROJECT_NOT_IN_SCOPE
    );
    // An Epic of another project under a granted project_id is not in scope.
    let mut mixed = upsert_request("mixed-flow", definition(&[("A", &luna())], &[]));
    mixed.project_id = Some(world.project);
    mixed.epic_id = Some(foreign_epic);
    assert_eq!(
        error_code(&world.upsert(seat, &mixed).await.unwrap_err()),
        "not_found_in_scope"
    );
    // The PM cannot name another project.
    let mut pm_foreign = upsert_request("pm-foreign", definition(&[("A", &luna())], &[]));
    pm_foreign.project_id = Some(foreign);
    pm_foreign.epic_id = Some(foreign_epic);
    assert_eq!(
        error_code(&world.upsert(world.manager, &pm_foreign).await.unwrap_err()),
        MANAGER_PROJECT_NOT_IN_SCOPE
    );
}

// ─── #1275: topology launches are charged up the portfolio chain ───────────

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn topology_launches_inherit_and_recheck_live_ancestor_grants() {
    use rsi_common::global_manager::ConfigureGlobalManagerRequestV1;
    for copied in [false, true] {
        let own = if copied { vec![luna(), glm()] } else { vec![] };
        let world = World::new(policy(&[ManagerCapabilityV2::Automation], own, 64)).await;
        let seat = Uuid::new_v4();
        let grant = |allowed, version, key: &str| ConfigureGlobalManagerRequestV1 {
            session_id: seat,
            project_ids: vec![world.project],
            allowed_launches: allowed,
            project_policy: policy(&[ManagerCapabilityV2::Automation], vec![], 64),
            expected_grant_version: version,
            idempotency_key: key.into(),
        };
        {
            let store = world.harness.executor.store.lock().await;
            let mut row =
                crate::session::agent_verbs::tests::test_session(seat, world.harness.repo.clone());
            row.project_id = Some(world.project);
            row.status = SessionStatus::Running;
            store.insert_session(&row).unwrap();
            store
                .configure_global_manager(&grant(vec![luna(), glm()], 0, "wide"), "operator:test")
                .unwrap();
        }
        // Both the PM and the Epic lead inherit the same live ceiling.
        let mut request = upsert_request("live-launches", definition(&[("A", &luna())], &[]));
        request.scope = AgentTopologyScopeV1::Manager;
        let flow = world.upsert(world.manager, &request).await.unwrap();
        let queued = world
            .execute(world.manager, &flow, world.epic_a, "queued")
            .await
            .unwrap();
        world
            .upsert(
                world.lead_a,
                &upsert_request("lead-live-launches", definition(&[("A", &luna())], &[])),
            )
            .await
            .unwrap();
        {
            let store = world.harness.executor.store.lock().await;
            let version = store.active_global_grant().unwrap().unwrap().grant_version;
            store
                .configure_global_manager(&grant(vec![glm()], version, "narrow"), "operator:test")
                .unwrap();
        }
        // No PM policy edit: the accepted execution must recheck the ancestor.
        assert_eq!(
            world
                .harness
                .executor
                .advance(queued.execution_id)
                .await
                .unwrap(),
            Step::Done
        );
        let attempt = world.harness.attempt(queued.execution_id, "A", 0, 1).await;
        assert_eq!(attempt.status, AttemptStatus::Blocked);
        assert_eq!(attempt.error.as_deref(), Some("launch_not_granted"));
        assert_eq!(world.harness.launches().len(), 0);
        assert_eq!(
            error_code(
                &world
                    .execute(world.manager, &flow, world.epic_a, "after")
                    .await
                    .unwrap_err()
            ),
            "policy_refused"
        );
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn topology_launches_are_charged_against_every_ancestor_allowance() {
    use rsi_common::global_manager::{
        ConfigureGlobalManagerRequestV1, MANAGER_ANCESTOR_ALLOWANCE_EXCEEDED,
    };
    // The PM's own allowance is ample; a global above it allows two.
    let world = World::new(policy(&[ManagerCapabilityV2::Automation], vec![luna()], 64)).await;
    {
        let store = world.harness.executor.store.lock().await;
        let seat = Uuid::new_v4();
        let mut row =
            crate::session::agent_verbs::tests::test_session(seat, world.harness.repo.clone());
        row.project_id = Some(world.project);
        row.status = SessionStatus::Running;
        store.insert_session(&row).unwrap();
        store
            .configure_global_manager_confirmed(
                &ConfigureGlobalManagerRequestV1 {
                    session_id: seat,
                    project_ids: vec![world.project],
                    allowed_launches: vec![luna()],
                    project_policy: policy(&[ManagerCapabilityV2::Automation], vec![luna()], 2),
                    expected_grant_version: 0,
                    idempotency_key: "global-cap-2".into(),
                },
                "operator:test",
                // The operator reviewed the cap preview (#1398, #1562).
                true,
            )
            .unwrap();
    }
    let mut request = upsert_request("ancestor-charged", definition(&[("A", &luna())], &[]));
    request.scope = AgentTopologyScopeV1::Manager;
    let flow = world.upsert(world.manager, &request).await.unwrap();
    let mut runs = Vec::new();
    for key in ["one", "two", "three"] {
        runs.push(
            world
                .execute(world.manager, &flow, world.epic_a, key)
                .await
                .unwrap()
                .execution_id,
        );
    }
    for run in &runs[..2] {
        assert_eq!(
            world.harness.executor.advance(*run).await.unwrap(),
            Step::Wait
        );
    }
    // The third launch finds the global's allowance spent by topology alone.
    assert_eq!(
        world.harness.executor.advance(runs[2]).await.unwrap(),
        Step::Done
    );
    let third = world.harness.attempt(runs[2], "A", 0, 1).await;
    assert_eq!(third.failure_class.as_deref(), Some("policy_refused"));
    assert_eq!(
        third.error.as_deref(),
        Some(MANAGER_ANCESTOR_ALLOWANCE_EXCEEDED)
    );
    assert_eq!(world.harness.launches().len(), 2);
    // A new admission is refused up front by the ancestor's allowance.
    assert_eq!(
        error_code(
            &world
                .execute(world.manager, &flow, world.epic_a, "four")
                .await
                .unwrap_err()
        ),
        MANAGER_ANCESTOR_ALLOWANCE_EXCEEDED
    );
    // A lifecycle creation counts the topology launches too.
    let store = world.harness.executor.store.lock().await;
    let config = store.get_harness_manager(world.project).unwrap().unwrap();
    let error = store
        .manager_ancestor_creation_allowance(&config, false, 1, 0)
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains(MANAGER_ANCESTOR_ALLOWANCE_EXCEEDED),
        "{error}"
    );
}

// ─── #1641 S1a: review nodes under agent policy ─────────────────────────────

fn opus() -> ManagerLaunchChoiceV2 {
    ManagerLaunchChoiceV2 {
        provider: SessionProvider::Claude,
        model: "claude-opus-5-5".into(),
        effort: Some("high".into()),
    }
}

fn sonnet() -> ManagerLaunchChoiceV2 {
    ManagerLaunchChoiceV2 {
        provider: SessionProvider::Claude,
        model: "claude-sonnet-5-5".into(),
        effort: Some("high".into()),
    }
}

/// An author session `A` (the given launch, `expects_commit`) feeding a
/// review node `R` whose reviewer is `reviewer`.
fn review_definition(
    author: &ManagerLaunchChoiceV2,
    reviewer: &ManagerLaunchChoiceV2,
) -> TopologyDefinition {
    serde_json::from_value(serde_json::json!({
        "nodes": [
            {"id": "A", "kind": "Task", "label": "A", "params": {
                "instructions": "do A",
                "provider": "claude",
                "model": author.model,
                "effort": author.effort,
                "step": {"kind": "session", "expects_commit": true},
            }},
            {"id": "R", "kind": "Task", "label": "R", "params": {
                "step": {"kind": "review", "of": "A", "reviewer": {
                    "provider": format!("{:?}", reviewer.provider).to_lowercase(),
                    "model": reviewer.model,
                    "effort": reviewer.effort,
                }},
            }},
        ],
        "edges": [{"from": "A", "to": "R"}],
    }))
    .unwrap()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn reviewer_same_family_as_author_is_policy_refused() {
    let world = World::new(policy(
        &[ManagerCapabilityV2::Automation],
        vec![opus(), sonnet(), luna()],
        64,
    ))
    .await;
    let mut same_family = upsert_request("same-family", review_definition(&opus(), &sonnet()));
    same_family.validate_only = true;
    let refused = world
        .upsert(world.lead_a, &same_family)
        .await
        .unwrap()
        .diagnostics;
    assert_eq!(refused.len(), 1, "{refused:?}");
    assert!(
        refused[0].contains("must be a different vendor family than the author node A"),
        "{refused:?}"
    );

    // A reviewer of another family under the grant is accepted.
    let mut independent = upsert_request("independent", review_definition(&opus(), &luna()));
    independent.validate_only = true;
    assert_eq!(
        world
            .upsert(world.lead_a, &independent)
            .await
            .unwrap()
            .diagnostics,
        Vec::<String>::new()
    );

    // The reviewer triple is an explicit grant like any session node's.
    let world = World::new(policy(&[ManagerCapabilityV2::Automation], vec![opus()], 64)).await;
    let mut ungranted = upsert_request("ungranted", review_definition(&opus(), &luna()));
    ungranted.validate_only = true;
    let diagnostics = world
        .upsert(world.lead_a, &ungranted)
        .await
        .unwrap()
        .diagnostics;
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    assert!(
        diagnostics[0].contains("is not in the operator's allowed_launches"),
        "{diagnostics:?}"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn agent_review_request_rechecks_live_policy_before_asking_for_a_reviewer() {
    let world = World::new(policy(
        &[ManagerCapabilityV2::Automation],
        vec![sonnet(), luna()],
        64,
    ))
    .await;
    let flow = world
        .upsert(
            world.lead_a,
            &upsert_request("gated-review", review_definition(&sonnet(), &luna())),
        )
        .await
        .unwrap();
    let run = world
        .execute(world.lead_a, &flow, world.epic_a, "gated-review")
        .await
        .unwrap();
    assert_eq!(
        world
            .harness
            .executor
            .advance(run.execution_id)
            .await
            .unwrap(),
        Step::Wait
    );
    let author = world.harness.attempt(run.execution_id, "A", 0, 1).await;
    world
        .harness
        .commit_and_complete(author.session_id, "author");
    // The operator withdraws the reviewer's launch after the execution was
    // accepted: the review re-checks it before any reviewer is requested.
    world
        .grant(policy(
            &[ManagerCapabilityV2::Automation],
            vec![sonnet()],
            64,
        ))
        .await;
    assert_eq!(
        world
            .harness
            .executor
            .advance(run.execution_id)
            .await
            .unwrap(),
        Step::Done
    );
    let review = world.harness.attempt(run.execution_id, "R", 0, 1).await;
    assert_eq!(review.status, AttemptStatus::Blocked);
    assert_eq!(review.failure_class.as_deref(), Some("policy_refused"));
    assert_eq!(review.error.as_deref(), Some("launch_not_granted"));
    assert!(world.harness.review_requests().is_empty());
}

// ─── #1641 S2: landing on behalf of the execution's owner ───────────────────

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn landing_gate_follows_the_owners_live_authority() {
    let world = World::new(policy(
        &[ManagerCapabilityV2::Automation],
        vec![sonnet(), luna()],
        64,
    ))
    .await;
    let flow = world
        .upsert(
            world.lead_a,
            &upsert_request("landing", review_definition(&sonnet(), &luna())),
        )
        .await
        .unwrap();
    let gate = |execution: Uuid| {
        let world = &world;
        async move {
            let store = world.harness.executor.store.lock().await;
            let execution = rows::load_execution(&store, execution).unwrap().unwrap();
            agent::landing_gate(&store, &execution).unwrap()
        }
    };

    // An Epic lead's execution may land while that lead still leads the Epic.
    let by_lead = world
        .execute(world.lead_a, &flow, world.epic_a, "lead-run")
        .await
        .unwrap();
    assert_eq!(gate(by_lead.execution_id).await, None);
    {
        let store = world.harness.executor.store.lock().await;
        store
            .conn
            .execute(
                "UPDATE sessions SET lead_session_id=?1 WHERE id=?2",
                rusqlite::params![world.worker.to_string(), world.epic_a.to_string()],
            )
            .unwrap();
    }
    assert_eq!(
        gate(by_lead.execution_id).await,
        Some("requester_unauthorized")
    );

    // The project manager's execution may land in an Epic it covers while its
    // grant is in force and unpaused; a pause or a moved scope stops it.
    let by_manager = world
        .execute(world.manager, &flow, world.epic_a, "manager-run")
        .await
        .unwrap();
    assert_eq!(gate(by_manager.execution_id).await, None);
    world
        .grant(ManagerPolicyV2 {
            paused: true,
            ..policy(
                &[ManagerCapabilityV2::Automation],
                vec![sonnet(), luna()],
                64,
            )
        })
        .await;
    assert!(gate(by_manager.execution_id).await.is_some());
    world
        .grant(policy(
            &[ManagerCapabilityV2::Automation],
            vec![sonnet(), luna()],
            64,
        ))
        .await;
    assert_eq!(gate(by_manager.execution_id).await, None);
    rescope(
        &world,
        vec![world.epic_b],
        policy(
            &[ManagerCapabilityV2::Automation],
            vec![sonnet(), luna()],
            64,
        ),
    )
    .await;
    assert!(gate(by_manager.execution_id).await.is_some());
}

// ─── #1687: a cancellation during the land probe stops the enqueue ──────────

/// A land attempt of a running agent execution, with its enqueue request.
async fn land_target(world: &World, key: &str) -> (Uuid, crate::topology::land::LandRequest) {
    let flow = world
        .upsert(
            world.lead_a,
            &upsert_request(key, review_definition(&sonnet(), &luna())),
        )
        .await
        .unwrap();
    let execution = world
        .execute(world.lead_a, &flow, world.epic_a, key)
        .await
        .unwrap()
        .execution_id;
    let store = world.harness.executor.store.lock().await;
    store
        .conn
        .execute(
            "UPDATE topology_executions SET status='running' WHERE id=?1",
            [execution.to_string()],
        )
        .unwrap();
    rows::reserve_attempts(
        &store,
        execution,
        &[rows::NewAttempt {
            node_id: "Land".into(),
            iteration: 0,
            attempt_no: 1,
            base_commit: world.harness.base.clone(),
            query: String::new(),
            node_kind: "land",
            catalog_op: None,
            effect_class: None,
        }],
    )
    .unwrap();
    let (_, attempt) = rows::load_attempts(&store, execution)
        .unwrap()
        .into_iter()
        .map(|attempt| (execution, attempt))
        .find(|(_, attempt)| attempt.node_id == "Land")
        .unwrap();
    let request = crate::topology::land::LandRequest {
        execution_id: execution,
        attempt_id: attempt.id,
        node_id: "Land".into(),
        dedup_key: attempt.dedup_key,
        project_id: Some(world.project),
        epic_id: Some(world.epic_a),
        repo_root: world.harness.repo.clone(),
        source_commit: world.harness.base.clone(),
        review_node: "R".into(),
        review_assignment_id: Uuid::new_v4(),
        author_session_id: world.lead_a,
        test_filters: Vec::new(),
    };
    (execution, request)
}

fn land_control(world: &World) -> crate::session::agent_verbs::AgentControlHandle {
    let (spawn_tx, _spawn_rx) = tokio::sync::mpsc::channel(4);
    crate::session::agent_verbs::AgentControlHandle::new(
        Default::default(),
        Default::default(),
        std::sync::Arc::clone(&world.harness.executor.store),
        std::sync::Arc::new(crate::bus::EventBus::new(16)),
        std::sync::Arc::new(crate::session::spawn_coordinator::SpawnCoordinator::new(
            spawn_tx,
        )),
    )
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn cancellation_during_the_land_probe_enqueues_nothing_and_cancels_the_attempt() {
    use crate::topology::land::LandEnqueue;
    let world = World::new(policy(
        &[ManagerCapabilityV2::Automation],
        vec![sonnet(), luna()],
        64,
    ))
    .await;
    let control = land_control(&world);
    let entries = |world: &World| {
        let store = world.harness.executor.store.clone();
        async move {
            store
                .lock()
                .await
                .list_rolling_queue_entries(None, 10)
                .unwrap()
        }
    };

    // The owner's interrupt commits between the first admission and the
    // insert (while the Git probe runs): nothing is enqueued.
    let (execution, request) = land_target(&world, "probe-race").await;
    let lead = world.lead_a;
    let answer = control
        .enqueue_topology_racing_for_test(&request, move |store| {
            let row_version = rows::load_execution(store, execution)
                .unwrap()
                .unwrap()
                .row_version;
            agent::interrupt(
                store,
                lead,
                &AgentTopologyInterruptRequestV1 {
                    project_id: None,
                    execution_id: execution,
                    expected_row_version: row_version,
                    idempotency_key: "interrupt-during-probe".into(),
                },
            )
            .unwrap();
        })
        .await
        .unwrap();
    assert_eq!(answer, LandEnqueue::Cancelled);
    assert!(entries(&world).await.is_empty());

    // An entry created before the cancellation is still adopted.
    let (execution, request) = land_target(&world, "replay-after-cancel").await;
    let first = control
        .enqueue_topology_admitted_for_test(&request, true)
        .await
        .unwrap();
    let LandEnqueue::Queued(entry) = first else {
        panic!("{first:?}");
    };
    {
        let store = world.harness.executor.store.lock().await;
        let row_version = rows::load_execution(&store, execution)
            .unwrap()
            .unwrap()
            .row_version;
        agent::interrupt(
            &store,
            lead,
            &AgentTopologyInterruptRequestV1 {
                project_id: None,
                execution_id: execution,
                expected_row_version: row_version,
                idempotency_key: "interrupt-after-enqueue".into(),
            },
        )
        .unwrap();
    }
    assert_eq!(
        control
            .enqueue_topology_admitted_for_test(&request, true)
            .await
            .unwrap(),
        LandEnqueue::Queued(entry)
    );
    assert_eq!(entries(&world).await.len(), 1);
}

// ─── S3a: the on-call seat and the visible wait (#1641) ────────────────────

/// A manager-scope upsert request.
fn manager_upsert(name: &str, definition: TopologyDefinition) -> AgentTopologyUpsertRequestV1 {
    let mut request = upsert_request(name, definition);
    request.scope = AgentTopologyScopeV1::Manager;
    request
}

/// A single-node topology run by the manager on Epic A.
async fn on_call_run(
    world: &World,
    on_call: Option<rsi_common::types::TopologyOnCallSeat>,
    key: &str,
) -> Uuid {
    let flow = world
        .upsert(
            world.manager,
            &manager_upsert(
                &format!("on-call-{key}"),
                definition(&[("A", &luna())], &[]),
            ),
        )
        .await
        .unwrap();
    world
        .execute_on_call(world.manager, &flow, world.epic_a, key, on_call)
        .await
        .unwrap()
        .execution_id
}

async fn resolve_on_call(world: &World, run: Uuid) -> super::oncall::OnCall {
    let store = world.harness.executor.store.lock().await;
    let execution = rows::load_execution(&store, run).unwrap().unwrap();
    super::oncall::resolve(
        &store,
        execution.project_id,
        &super::oncall::seat_of(execution.input.as_ref()),
    )
    .unwrap()
}

async fn set_session(world: &World, session: Uuid, status: SessionStatus) {
    let store = world.harness.executor.store.lock().await;
    store.update_session_status(session, status).unwrap();
}

/// Seat a root portfolio node over the world's project; returns `(node, seat)`.
async fn portfolio_over(world: &World) -> (Uuid, Uuid) {
    use rsi_common::global_manager::ConfigureGlobalManagerRequestV1;
    let seat = Uuid::new_v4();
    let store = world.harness.executor.store.lock().await;
    let mut row =
        crate::session::agent_verbs::tests::test_session(seat, world.harness.repo.clone());
    row.project_id = Some(world.project);
    row.status = SessionStatus::Running;
    store.insert_session(&row).unwrap();
    store
        .configure_global_manager(
            &ConfigureGlobalManagerRequestV1 {
                session_id: seat,
                project_ids: vec![world.project],
                allowed_launches: vec![luna()],
                project_policy: policy(&[ManagerCapabilityV2::Automation], vec![], 64),
                expected_grant_version: 0,
                idempotency_key: "portfolio-over".into(),
            },
            "operator:test",
        )
        .unwrap();
    let node = store
        .portfolio_node_for_seat(seat)
        .unwrap()
        .unwrap()
        .node_id;
    (node, seat)
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn on_call_defaults_to_project_manager() {
    use rsi_common::types::TopologyOnCallSeat;
    let world = World::new(automation()).await;
    let run = on_call_run(&world, None, "default").await;
    let current = {
        let store = world.harness.executor.store.lock().await;
        let config = store.get_harness_manager(world.project).unwrap().unwrap();
        assert!(
            rows::load_execution(&store, run)
                .unwrap()
                .unwrap()
                .input
                .is_none(),
            "an omitted seat adds nothing to the input"
        );
        config.current_session_id.expect("the PM has a seat")
    };
    assert_eq!(
        resolve_on_call(&world, run).await,
        super::oncall::OnCall::Live {
            seat: TopologyOnCallSeat::ProjectManager,
            session_id: current,
        }
    );

    // A named seat is stored as a reference under the reserved key, never a
    // session id, and never reaches the node's prompt.
    let named = on_call_run(&world, Some(TopologyOnCallSeat::ProjectManager), "named").await;
    let stored = {
        let store = world.harness.executor.store.lock().await;
        rows::load_execution(&store, named)
            .unwrap()
            .unwrap()
            .input
            .unwrap()
    };
    assert_eq!(
        stored,
        serde_json::json!({"_rsi_on_call": {"kind": "project_manager"}})
    );
    world.harness.executor.advance(named).await.unwrap();
    let attempt = world.harness.attempt(named, "A", 0, 1).await;
    assert!(!attempt.query().contains("_rsi_on_call"));
    assert!(!attempt.query().contains("project_manager"));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn on_call_falls_back_to_covering_portfolio_seat() {
    use super::oncall::{NO_LIVE_PROJECT_MANAGER, OnCall, PORTFOLIO_SEAT_NOT_LIVE};
    use rsi_common::types::TopologyOnCallSeat;
    let world = World::new(automation()).await;
    let run = on_call_run(&world, None, "fallback").await;

    // No live PM and no covering portfolio seat: nobody can answer.
    set_session(&world, world.manager, SessionStatus::Archived).await;
    assert_eq!(
        resolve_on_call(&world, run).await,
        OnCall::Unavailable {
            reason: NO_LIVE_PROJECT_MANAGER
        }
    );

    // The covering portfolio seat answers instead (PM ruling P1).
    let (node, seat) = portfolio_over(&world).await;
    assert_eq!(
        resolve_on_call(&world, run).await,
        OnCall::Live {
            seat: TopologyOnCallSeat::Portfolio { node_id: node },
            session_id: seat,
        }
    );

    // A live PM wins over the portfolio seat.
    set_session(&world, world.manager, SessionStatus::Running).await;
    assert!(matches!(
        resolve_on_call(&world, run).await,
        OnCall::Live {
            seat: TopologyOnCallSeat::ProjectManager,
            ..
        }
    ));

    // A named portfolio seat is exactly that seat and never falls back.
    let named = on_call_run(
        &world,
        Some(TopologyOnCallSeat::Portfolio { node_id: node }),
        "named-portfolio",
    )
    .await;
    assert!(matches!(
        resolve_on_call(&world, named).await,
        OnCall::Live {
            seat: TopologyOnCallSeat::Portfolio { .. },
            ..
        }
    ));
    set_session(&world, seat, SessionStatus::Archived).await;
    assert_eq!(
        resolve_on_call(&world, named).await,
        OnCall::Unavailable {
            reason: PORTFOLIO_SEAT_NOT_LIVE
        }
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn on_call_seat_must_cover_the_project_and_not_collide_with_inputs() {
    use rsi_common::types::TopologyOnCallSeat;
    let world = World::new(automation()).await;
    let flow = world
        .upsert(
            world.manager,
            &manager_upsert("on-call-refusals", definition(&[("A", &luna())], &[])),
        )
        .await
        .unwrap();
    let uncovered = world
        .execute_on_call(
            world.manager,
            &flow,
            world.epic_a,
            "uncovered",
            Some(TopologyOnCallSeat::Portfolio {
                node_id: Uuid::new_v4(),
            }),
        )
        .await
        .unwrap_err();
    assert_eq!(error_code(&uncovered), "on_call_not_covering");
    assert_eq!(execution_count(&world).await, 0);

    let mut request = AgentTopologyExecuteRequestV1 {
        project_id: None,
        on_call: None,
        topology_id: flow.topology_id.unwrap(),
        expected_digest: flow.definition_digest.clone(),
        epic_id: world.epic_a,
        inputs: serde_json::json!({"_rsi_on_call": {"kind": "project_manager"}}),
        base_commit: None,
        idempotency_key: "spoof".into(),
    };
    assert_eq!(request.validate(), Err("topology_invalid_inputs"));
    request.inputs = serde_json::Value::Null;
    request.on_call = Some(TopologyOnCallSeat::Portfolio {
        node_id: Uuid::nil(),
    });
    assert_eq!(request.validate(), Err("topology_invalid_on_call"));
}

async fn snapshot_of(world: &World, run: Uuid) -> rsi_common::types::WorkflowExecutionSnapshot {
    let store = world.harness.executor.store.lock().await;
    rows::execution_snapshot(&store, run).unwrap().unwrap()
}

/// The event kinds of one execution, oldest first.
async fn event_kinds(world: &World, run: Uuid) -> Vec<String> {
    world
        .events(run)
        .await
        .into_iter()
        .map(|(kind, _, _)| kind)
        .collect()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn no_live_on_call_records_visible_wait_once() {
    use super::oncall::NO_LIVE_PROJECT_MANAGER;
    let mut world = World::new(automation()).await;
    let run = on_call_run(&world, None, "wait").await;
    world.harness.executor.advance(run).await.unwrap();
    let session = world.harness.launches()[0].1;
    let waiting = |kind: &[String], what: &str| kind.iter().filter(|k| *k == what).count();

    // A live on-call manager: the node's question is the manager's to answer,
    // so there is nothing to show the operator.
    world
        .harness
        .set_status(session, SessionStatus::WaitingApproval);
    for _ in 0..3 {
        world.harness.executor.advance(run).await.unwrap();
    }
    assert!(snapshot_of(&world, run).await.waiting.is_none());
    assert!(world.harness.operator_notices().is_empty());

    // The PM goes away: the node keeps waiting, and the operator sees why,
    // once, however many ticks pass.
    set_session(&world, world.manager, SessionStatus::Archived).await;
    for _ in 0..4 {
        world.harness.executor.advance(run).await.unwrap();
    }
    let kinds = event_kinds(&world, run).await;
    assert_eq!(waiting(&kinds, "on_call_unavailable"), 1);
    let wait = snapshot_of(&world, run)
        .await
        .waiting
        .expect("a visible wait");
    assert_eq!(wait.node_id, "A");
    assert_eq!(wait.reason, NO_LIVE_PROJECT_MANAGER);
    assert_eq!(
        wait.on_call,
        rsi_common::types::TopologyOnCallSeat::ProjectManager
    );
    let notices = world.harness.operator_notices();
    assert_eq!(notices.len(), 1, "one attention message per transition");
    assert!(notices[0].1.contains(NO_LIVE_PROJECT_MANAGER));
    assert_eq!(
        world.harness.attempt(run, "A", 0, 1).await.status,
        AttemptStatus::Running,
        "the node waits; nothing is failed or resolved for it"
    );

    // The wait survives a daemon restart as the same single wait.
    world.harness.restart();
    world.harness.executor.advance(run).await.unwrap();
    assert_eq!(
        waiting(&event_kinds(&world, run).await, "on_call_unavailable"),
        1
    );
    assert!(
        world.harness.operator_notices().is_empty(),
        "the new incarnation does not repeat the message"
    );
    assert!(snapshot_of(&world, run).await.waiting.is_some());

    // A seat is live again: the wait clears, with its own event.
    set_session(&world, world.manager, SessionStatus::Running).await;
    world.harness.executor.advance(run).await.unwrap();
    assert!(snapshot_of(&world, run).await.waiting.is_none());
    let kinds = event_kinds(&world, run).await;
    assert_eq!(waiting(&kinds, "on_call_restored"), 1);

    // A later outage is a new transition: a second wait, a second message.
    set_session(&world, world.manager, SessionStatus::Archived).await;
    world.harness.executor.advance(run).await.unwrap();
    assert_eq!(
        waiting(&event_kinds(&world, run).await, "on_call_unavailable"),
        2
    );
    assert_eq!(world.harness.operator_notices().len(), 1);

    // The node stops needing a manager (its question is answered): cleared.
    world.harness.set_status(session, SessionStatus::Running);
    world.harness.executor.advance(run).await.unwrap();
    assert!(snapshot_of(&world, run).await.waiting.is_none());
    assert_eq!(
        waiting(&event_kinds(&world, run).await, "on_call_restored"),
        2
    );
}

fn hours_ago(hours: i64) -> String {
    (chrono::Utc::now() - chrono::Duration::hours(hours))
        .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
}

/// Pretend the run's node attempt started `hours` ago.
async fn backdate_attempt(world: &World, run: Uuid, hours: i64) {
    let store = world.harness.executor.store.lock().await;
    store
        .conn
        .execute(
            "UPDATE topology_node_attempts SET started_at=?2 WHERE execution_id=?1",
            rusqlite::params![run.to_string(), hours_ago(hours)],
        )
        .unwrap();
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn waiting_approval_time_does_not_count_toward_node_wall_time() {
    let mut world = World::new(automation()).await;
    let waited = on_call_run(&world, None, "waited").await;
    let control = on_call_run(&world, None, "control").await;
    world.harness.executor.advance(waited).await.unwrap();
    world.harness.executor.advance(control).await.unwrap();
    let waited_session = world.harness.attempt(waited, "A", 0, 1).await.session_id;

    // Both nodes started five hours ago (the wall limit is four). One of them
    // spent the last two of those hours waiting on an answer: the session
    // reports when its wait began and the executor sees it waiting.
    backdate_attempt(&world, waited, 5).await;
    backdate_attempt(&world, control, 5).await;
    world.harness.set_waiting_since(
        waited_session,
        chrono::Utc::now() - chrono::Duration::hours(2),
    );
    world
        .harness
        .set_status(waited_session, SessionStatus::WaitingApproval);
    world.harness.executor.advance(waited).await.unwrap();
    // While it waits, the paused time keeps the node inside its limit.
    world.harness.executor.advance(waited).await.unwrap();
    assert_eq!(
        world.harness.attempt(waited, "A", 0, 1).await.status,
        AttemptStatus::Running,
        "a node waiting on an answer is not timed out"
    );

    // The answer arrives: the wait closes with its length and the node runs on.
    world
        .harness
        .set_status(waited_session, SessionStatus::Running);
    world.harness.executor.advance(waited).await.unwrap();
    world.harness.executor.advance(waited).await.unwrap();
    assert_eq!(
        world.harness.attempt(waited, "A", 0, 1).await.status,
        AttemptStatus::Running
    );
    let attempt_id = world.harness.attempt(waited, "A", 0, 1).await.id;
    {
        let store = world.harness.executor.store.lock().await;
        let waited_for = rows::node_waited(&store, attempt_id).unwrap();
        assert!(waited_for.open_since.is_none());
        assert!(waited_for.closed >= chrono::TimeDelta::minutes(119));
        assert!(waited_for.closed <= chrono::TimeDelta::minutes(121));
    }
    let kinds = event_kinds(&world, waited).await;
    assert_eq!(
        kinds.iter().filter(|k| *k == "node_wait_started").count(),
        1
    );
    assert_eq!(kinds.iter().filter(|k| *k == "node_wait_ended").count(), 1);

    // The paused time survives a restart: it is read from rows, not memory.
    world.harness.restart();
    world.harness.executor.advance(waited).await.unwrap();
    assert_eq!(
        world.harness.attempt(waited, "A", 0, 1).await.status,
        AttemptStatus::Running
    );

    // The same five hours without a wait is the wall-time timeout, as before.
    world.harness.executor.advance(control).await.unwrap();
    let timed_out = world.harness.attempt(control, "A", 0, 1).await;
    assert_eq!(timed_out.status, AttemptStatus::Failed);
    assert_eq!(timed_out.failure_class.as_deref(), Some("timeout"));
}

/// Pretend the run's node attempt started `seconds` ago.
async fn backdate_attempt_seconds(world: &World, run: Uuid, seconds: i64) {
    let store = world.harness.executor.store.lock().await;
    store
        .conn
        .execute(
            "UPDATE topology_node_attempts SET started_at=?2 WHERE execution_id=?1",
            rusqlite::params![
                run.to_string(),
                (chrono::Utc::now() - chrono::Duration::seconds(seconds))
                    .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
            ],
        )
        .unwrap();
}

/// #1704: a node enters its wait just under the wall limit and is first seen
/// waiting just past it. The pause counts from the wait's real start, not from
/// the first observation, so the node is not timed out.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn wait_first_observed_after_the_limit_is_paused_from_its_real_start() {
    let mut world = World::new(automation()).await;
    let run = on_call_run(&world, None, "late-observation").await;
    world.harness.executor.advance(run).await.unwrap();
    let session = world.harness.attempt(run, "A", 0, 1).await.session_id;
    // Started 4h01m ago; it ran 3h58m30s, then waited for the last 90s.
    backdate_attempt_seconds(&world, run, 4 * 3600 + 60).await;
    world
        .harness
        .set_waiting_since(session, chrono::Utc::now() - chrono::Duration::seconds(90));
    world
        .harness
        .set_status(session, SessionStatus::WaitingApproval);
    world.harness.executor.advance(run).await.unwrap();
    assert_eq!(
        world.harness.attempt(run, "A", 0, 1).await.status,
        AttemptStatus::Running,
        "a node first seen waiting is paused from when the wait began"
    );
    let attempt_id = world.harness.attempt(run, "A", 0, 1).await.id;
    {
        let store = world.harness.executor.store.lock().await;
        let waited = rows::node_waited(&store, attempt_id).unwrap();
        let open = waited.open_since.expect("the wait is open");
        let age = chrono::Utc::now() - open;
        assert!(age >= chrono::TimeDelta::seconds(89) && age <= chrono::TimeDelta::seconds(100));
    }
    // The answer arrives; the node's productive time is still under the limit.
    world.harness.set_status(session, SessionStatus::Running);
    world.harness.executor.advance(run).await.unwrap();
    world.harness.executor.advance(run).await.unwrap();
    assert_eq!(
        world.harness.attempt(run, "A", 0, 1).await.status,
        AttemptStatus::Running
    );
}

/// #1704: with no wait start from the session, the wait is taken to begin when
/// the node was last known running, so the pause is never undercounted.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn wait_without_a_reported_start_is_paused_from_the_last_known_running_time() {
    let mut world = World::new(automation()).await;
    let run = on_call_run(&world, None, "unknown-start").await;
    world.harness.executor.advance(run).await.unwrap();
    let session = world.harness.attempt(run, "A", 0, 1).await.session_id;
    backdate_attempt_seconds(&world, run, 4 * 3600 + 60).await;
    world
        .harness
        .set_status(session, SessionStatus::WaitingApproval);
    world.harness.executor.advance(run).await.unwrap();
    let attempt = world.harness.attempt(run, "A", 0, 1).await;
    assert_eq!(attempt.status, AttemptStatus::Running);
    let store = world.harness.executor.store.lock().await;
    let open = rows::node_waited(&store, attempt.id)
        .unwrap()
        .open_since
        .expect("the wait is open");
    let started = attempt.started_at.unwrap();
    assert!((open - started).abs() <= chrono::TimeDelta::seconds(1));
}

// ─── #1641 S3c: a node's question and an exhausted review reach the on-call ──

/// Automation to run nodes, WorkPlan to rule on decisions.
fn ruling_policy() -> ManagerPolicyV2 {
    policy(
        &[
            ManagerCapabilityV2::Automation,
            ManagerCapabilityV2::WorkPlan,
        ],
        vec![luna(), glm()],
        64,
    )
}

/// The project manager's `topology:` decision records, oldest key first.
async fn topology_decisions(
    world: &World,
) -> Vec<crate::store::harness_manager_v2::ManagerRecordV2> {
    let store = world.harness.executor.store.lock().await;
    let config = store.get_harness_manager(world.project).unwrap().unwrap();
    store
        .manager_v2_records_of_kind(&config, "decision")
        .unwrap()
        .into_iter()
        .filter(|record| record.key.starts_with("topology:"))
        .collect()
}

fn refusal_code(error: DaemonError) -> String {
    match error {
        DaemonError::InvalidParam(code) => code,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// The project manager rules on a pending decision.
async fn pm_rule(
    world: &World,
    key: &str,
    answer: &str,
    idempotency: &str,
) -> Result<rsi_common::harness_manager_v2::ManagerMutationReceiptV2> {
    use rsi_common::harness_manager_v2::{
        AgentManagerUpdateRequestV2, ManagerFenceV2, ManagerUpdateV2,
    };
    let store = world.harness.executor.store.lock().await;
    let config = store.get_harness_manager(world.project).unwrap().unwrap();
    let grant = store
        .get_harness_manager_policy(world.project)
        .unwrap()
        .unwrap();
    let record = store
        .manager_v2_record(&config, "decision", key)
        .unwrap()
        .unwrap();
    store.manager_v2_commit_update(
        world.manager,
        &AgentManagerUpdateRequestV2 {
            project_id: None,
            fence: ManagerFenceV2 {
                scope_version: config.row_version,
                policy_version: grant.row_version,
            },
            idempotency_key: idempotency.into(),
            change: ManagerUpdateV2::DecisionRuling {
                key: key.into(),
                expected_row_version: record.row_version,
                target_digest: record.payload["target_digest"].as_str().unwrap().into(),
                answer: answer.into(),
                owner_manager_session_id: None,
            },
        },
        &crate::store::manager_ledger::LedgerObservation::default(),
    )
}

/// The decision key a parked attempt waits on.
fn parked_on(attempt: &super::store::AttemptRow) -> String {
    assert_eq!(attempt.status, AttemptStatus::Waiting, "{attempt:?}");
    attempt.output.as_ref().unwrap()["decision_key"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn blocked_handoff_with_question_waits_and_continues_same_session_on_ruling() {
    let mut world = World::new(ruling_policy()).await;
    let run = on_call_run(&world, None, "ask").await;
    world.harness.executor.advance(run).await.unwrap();
    let session = world.harness.attempt(run, "A", 0, 1).await.session_id;

    // The node stops on a BLOCKED handoff that names its question.
    world.harness.complete_blocked(
        session,
        "technical_impasse",
        Some("Keep the old field or rename it?"),
    );
    for _ in 0..3 {
        world.harness.executor.advance(run).await.unwrap();
    }
    // It waits (neither failed nor blocked), on one decision, however many ticks.
    let parked = world.harness.attempt(run, "A", 0, 1).await;
    let key = parked_on(&parked);
    assert_eq!(parked.session_id, session);
    assert_eq!(world.harness.status(run).await, ExecutionStatus::Running);
    assert_eq!(world.harness.launches().len(), 1);
    let decisions = topology_decisions(&world).await;
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].key, key);
    assert_eq!(decisions[0].payload["status"], "pending");
    assert_eq!(
        decisions[0].payload["question"],
        "Keep the old field or rename it?"
    );
    assert_eq!(decisions[0].payload["gate"], serde_json::Value::Null);
    assert_eq!(
        decisions[0].payload["asked_by"]["kind"],
        "topology_executor"
    );
    assert!(world.harness.continues().is_empty());

    // The run view names the parked node and lists the pending question.
    {
        let mut view = snapshot_of(&world, run).await;
        let store = world.harness.executor.store.lock().await;
        crate::session::graph_executions::attach_run_view(&store, &mut view).unwrap();
        assert_eq!(view.current_nodes, vec!["A".to_string()]);
        assert_eq!(view.rulings.len(), 1);
        assert_eq!(view.rulings[0].decision_key, key);
        assert_eq!(view.rulings[0].node_id, "A");
        assert_eq!(view.rulings[0].status, "pending");
        assert!(view.on_call.is_some());
    }

    // A restart in the middle of the wait changes nothing: no second record.
    world.harness.restart();
    world.harness.executor.advance(run).await.unwrap();
    assert_eq!(topology_decisions(&world).await.len(), 1);
    assert_eq!(
        world.harness.attempt(run, "A", 0, 1).await.status,
        AttemptStatus::Waiting
    );

    // The ruling continues the SAME session with the answer, once.
    pm_rule(&world, &key, "Keep it", "rule-keep").await.unwrap();
    world.harness.executor.advance(run).await.unwrap();
    assert_eq!(
        world.harness.continues(),
        vec![(session, "My answer to your question: Keep it".to_owned())]
    );
    let resumed = world.harness.attempt(run, "A", 0, 1).await;
    assert_eq!(resumed.status, AttemptStatus::Running);
    assert_eq!(resumed.session_id, session);
    assert!(
        event_kinds(&world, run)
            .await
            .contains(&"node_continued_after_decision".to_owned())
    );
    world.harness.executor.advance(run).await.unwrap();
    assert_eq!(world.harness.continues().len(), 1, "never continued twice");

    // The continued session finishes and the execution succeeds.
    world.harness.commit_and_complete(session, "answered");
    assert_eq!(
        world.harness.executor.advance(run).await.unwrap(),
        Step::Done
    );
    assert_eq!(world.harness.status(run).await, ExecutionStatus::Succeeded);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn blocked_handoff_production_class_goes_to_operator() {
    use rsi_common::harness_manager_v2::{AnswerHarnessManagerDecisionRequestV2, ManagerFenceV2};
    let world = World::new(ruling_policy()).await;
    let run = on_call_run(&world, None, "gated").await;
    world.harness.executor.advance(run).await.unwrap();
    let session = world.harness.attempt(run, "A", 0, 1).await.session_id;
    world.harness.complete_blocked(
        session,
        "production",
        Some("Which tag should the release use?"),
    );
    world.harness.executor.advance(run).await.unwrap();

    // The question is parked like any other, but declares its gate.
    let key = parked_on(&world.harness.attempt(run, "A", 0, 1).await);
    let record = topology_decisions(&world).await.remove(0);
    assert_eq!(record.payload["gate"], "main_or_release");
    // The operator is told; the on-call manager is refused.
    let notices = world.harness.operator_notices();
    assert_eq!(notices.len(), 1);
    assert!(notices[0].1.contains("only you can answer"), "{notices:?}");
    assert_eq!(
        refusal_code(
            pm_rule(&world, &key, "v1.2", "rule-prod")
                .await
                .unwrap_err()
        ),
        "manager_v2_decision_operator_gate"
    );
    world.harness.executor.advance(run).await.unwrap();
    assert!(world.harness.continues().is_empty());
    assert_eq!(
        world.harness.attempt(run, "A", 0, 1).await.status,
        AttemptStatus::Waiting
    );

    // The operator answers; the same session continues with the answer.
    {
        let store = world.harness.executor.store.lock().await;
        let config = store.get_harness_manager(world.project).unwrap().unwrap();
        let grant = store
            .get_harness_manager_policy(world.project)
            .unwrap()
            .unwrap();
        store
            .manager_v2_prepare_decision_answer(
                &AnswerHarnessManagerDecisionRequestV2 {
                    project_id: world.project,
                    fence: ManagerFenceV2 {
                        scope_version: config.row_version,
                        policy_version: grant.row_version,
                    },
                    decision_key: key.clone(),
                    expected_row_version: record.row_version,
                    target_digest: record.payload["target_digest"].as_str().unwrap().into(),
                    answer: "Use v1.2".into(),
                    idempotency_key: "operator-answer".into(),
                },
                |_, _, _, _| unreachable!("not an acceptance"),
            )
            .unwrap();
    }
    world.harness.executor.advance(run).await.unwrap();
    assert_eq!(
        world.harness.continues(),
        vec![(session, "My answer to your question: Use v1.2".to_owned())]
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn ordinary_blocked_handoffs_and_dead_ends_still_block() {
    // Gates do not loosen: a BLOCKED handoff with no question blocks as ever,
    // and a question nobody can be asked (no ledger) blocks too.
    let world = World::new(ruling_policy()).await;
    let plain = on_call_run(&world, None, "plain").await;
    world.harness.executor.advance(plain).await.unwrap();
    let session = world.harness.attempt(plain, "A", 0, 1).await.session_id;
    world
        .harness
        .complete_blocked(session, "technical_impasse", None);
    world.harness.executor.advance(plain).await.unwrap();
    let blocked = world.harness.attempt(plain, "A", 0, 1).await;
    assert_eq!(blocked.status, AttemptStatus::Blocked);
    assert_eq!(
        blocked.failure_class.as_deref(),
        Some(rows::failure::HANDOFF_BLOCKED)
    );
    assert_eq!(world.harness.status(plain).await, ExecutionStatus::Blocked);
    assert!(topology_decisions(&world).await.is_empty());

    // A question that exceeds the contract's 2 KiB is a malformed handoff.
    let long = on_call_run(&world, None, "long").await;
    world.harness.executor.advance(long).await.unwrap();
    let session = world.harness.attempt(long, "A", 0, 1).await.session_id;
    world
        .harness
        .complete_blocked(session, "technical_impasse", Some(&"q".repeat(2100)));
    world.harness.executor.advance(long).await.unwrap();
    let invalid = world.harness.attempt(long, "A", 0, 1).await;
    assert_eq!(invalid.status, AttemptStatus::Failed);
    assert_eq!(
        invalid.failure_class.as_deref(),
        Some(rows::failure::HANDOFF_INVALID)
    );
    assert!(topology_decisions(&world).await.is_empty());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn withdrawn_or_undeliverable_answer_blocks_visibly() {
    let world = World::new(ruling_policy()).await;
    // A withdrawn decision blocks the attempt as a BLOCKED handoff does.
    let withdrawn = on_call_run(&world, None, "withdrawn").await;
    world.harness.executor.advance(withdrawn).await.unwrap();
    let session = world.harness.attempt(withdrawn, "A", 0, 1).await.session_id;
    world
        .harness
        .complete_blocked(session, "technical_impasse", Some("Which one?"));
    world.harness.executor.advance(withdrawn).await.unwrap();
    let key = parked_on(&world.harness.attempt(withdrawn, "A", 0, 1).await);
    {
        let store = world.harness.executor.store.lock().await;
        let config = store.get_harness_manager(world.project).unwrap().unwrap();
        assert!(
            store
                .manager_v2_withdraw_topology_decision(&config, &key, "no longer relevant")
                .unwrap()
        );
    }
    world.harness.executor.advance(withdrawn).await.unwrap();
    let blocked = world.harness.attempt(withdrawn, "A", 0, 1).await;
    assert_eq!(blocked.status, AttemptStatus::Blocked);
    assert_eq!(
        blocked.failure_class.as_deref(),
        Some(rows::failure::HANDOFF_BLOCKED)
    );
    assert!(blocked.error.unwrap().contains("withdrawn"));

    // A provider that refuses to continue is retried a few times, then blocks
    // with the reason; the answer is never silently dropped.
    let refused = on_call_run(&world, None, "refused").await;
    world.harness.executor.advance(refused).await.unwrap();
    let session = world.harness.attempt(refused, "A", 0, 1).await.session_id;
    world
        .harness
        .complete_blocked(session, "technical_impasse", Some("Which one?"));
    world.harness.executor.advance(refused).await.unwrap();
    let key = parked_on(&world.harness.attempt(refused, "A", 0, 1).await);
    pm_rule(&world, &key, "The first", "rule-refused")
        .await
        .unwrap();
    world.harness.refuse_continues(true);
    for _ in 0..2 {
        world.harness.executor.advance(refused).await.unwrap();
        assert_eq!(
            world.harness.attempt(refused, "A", 0, 1).await.status,
            AttemptStatus::Waiting
        );
    }
    world.harness.executor.advance(refused).await.unwrap();
    let blocked = world.harness.attempt(refused, "A", 0, 1).await;
    assert_eq!(blocked.status, AttemptStatus::Blocked);
    assert!(blocked.error.unwrap().contains("could not be delivered"));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn cancelling_a_waiting_node_withdraws_its_decision() {
    let world = World::new(ruling_policy()).await;
    let run = on_call_run(&world, None, "cancel").await;
    world.harness.executor.advance(run).await.unwrap();
    let session = world.harness.attempt(run, "A", 0, 1).await.session_id;
    world
        .harness
        .complete_blocked(session, "technical_impasse", Some("Which one?"));
    world.harness.executor.advance(run).await.unwrap();
    parked_on(&world.harness.attempt(run, "A", 0, 1).await);
    {
        let store = world.harness.executor.store.lock().await;
        let version = rows::current_row_version(&store, run).unwrap();
        agent::interrupt(
            &store,
            world.manager,
            &AgentTopologyInterruptRequestV1 {
                project_id: None,
                execution_id: run,
                expected_row_version: version,
                idempotency_key: "cancel-waiting".into(),
            },
        )
        .unwrap();
    }
    world.harness.executor.advance(run).await.unwrap();
    assert_eq!(
        world.harness.attempt(run, "A", 0, 1).await.status,
        AttemptStatus::Cancelled
    );
    assert_eq!(
        topology_decisions(&world).await[0].payload["status"],
        "withdrawn",
        "a cancelled node must not leave a pending record holding the Epic"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn waiting_node_is_registered_as_exempt_from_stall_remediation() {
    // #1702 finding 3: the stall detector asks this registry.
    let world = World::new(ruling_policy()).await;
    let run = on_call_run(&world, None, "stall").await;
    world.harness.executor.advance(run).await.unwrap();
    let session = world.harness.attempt(run, "A", 0, 1).await.session_id;
    assert!(!super::oncall::awaits_on_call_answer(session));
    world
        .harness
        .set_status(session, SessionStatus::WaitingApproval);
    world.harness.executor.advance(run).await.unwrap();
    assert!(super::oncall::awaits_on_call_answer(session));
    // The answer arrives and the node runs again: the exemption ends.
    world.harness.set_status(session, SessionStatus::Running);
    world.harness.executor.advance(run).await.unwrap();
    assert!(!super::oncall::awaits_on_call_answer(session));
}

/// Park a single-node run on its question and rule on it; returns the run,
/// the parked attempt and the answer delivery the executor would make.
async fn answered_node(
    world: &World,
    key: &str,
) -> (
    Uuid,
    super::store::AttemptRow,
    super::executor::AnswerContinuation,
) {
    let run = on_call_run(world, None, key).await;
    world.harness.executor.advance(run).await.unwrap();
    let session = world.harness.attempt(run, "A", 0, 1).await.session_id;
    world
        .harness
        .complete_blocked(session, "technical_impasse", Some("Which one?"));
    world.harness.executor.advance(run).await.unwrap();
    let parked = world.harness.attempt(run, "A", 0, 1).await;
    let decision = parked_on(&parked);
    pm_rule(world, &decision, "Keep it", &format!("rule-{key}"))
        .await
        .unwrap();
    let delivery = super::executor::AnswerContinuation {
        session_id: session,
        prompt: "My answer to your question: Keep it".into(),
        binding: super::store::AnswerBinding {
            execution_id: run,
            attempt_id: parked.id,
            decision_key: decision,
        },
    };
    (run, parked, delivery)
}

/// #1715 finding 2: a cancellation that commits after the executor read the
/// answer but before the provider effect stops the delivery at the
/// continuation's own fence. No turn starts, and the next tick cancels the node.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn cancellation_during_answer_delivery_starts_no_provider_turn() {
    let world = World::new(ruling_policy()).await;
    let run = on_call_run(&world, None, "cancel-in-flight").await;
    world.harness.executor.advance(run).await.unwrap();
    let session = world.harness.attempt(run, "A", 0, 1).await.session_id;
    world
        .harness
        .complete_blocked(session, "technical_impasse", Some("Which one?"));
    world.harness.executor.advance(run).await.unwrap();
    let key = parked_on(&world.harness.attempt(run, "A", 0, 1).await);
    pm_rule(&world, &key, "Keep it", "rule-cancel-in-flight")
        .await
        .unwrap();

    // The executor reads the answer, decides to deliver it, and is held there.
    let entered = std::sync::Arc::new(tokio::sync::Notify::new());
    let go = std::sync::Arc::new(tokio::sync::Notify::new());
    *world.harness.executor.effects.answer_hold.lock().unwrap() =
        Some((std::sync::Arc::clone(&entered), std::sync::Arc::clone(&go)));
    let tick = world.harness.executor.clone();
    let tick = tokio::spawn(async move { tick.advance(run).await });
    entered.notified().await;
    // Cancellation is accepted while the answer is in flight.
    {
        let store = world.harness.executor.store.lock().await;
        let version = rows::current_row_version(&store, run).unwrap();
        agent::interrupt(
            &store,
            world.manager,
            &AgentTopologyInterruptRequestV1 {
                project_id: None,
                execution_id: run,
                expected_row_version: version,
                idempotency_key: "cancel-in-flight".into(),
            },
        )
        .unwrap();
    }
    go.notify_one();
    tick.await.unwrap().unwrap();
    assert!(
        world.harness.continues().is_empty(),
        "the continuation fence stops the provider effect after cancellation"
    );
    world.harness.executor.advance(run).await.unwrap();
    assert_eq!(
        world.harness.attempt(run, "A", 0, 1).await.status,
        AttemptStatus::Cancelled
    );
    assert!(world.harness.continues().is_empty());
}

/// #1715 finding 2: the continuation binding is exact. An attempt that no
/// longer waits on that decision, or a settled execution, refuses the answer.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn answer_continuation_refuses_a_binding_that_is_no_longer_live() {
    use crate::topology::executor::NodeEffects;
    let world = World::new(ruling_policy()).await;
    let (_, _, delivery) = answered_node(&world, "binding").await;
    let effects = &world.harness.executor.effects;

    let mut other_decision = delivery.clone();
    other_decision.binding.decision_key.push_str(":q2");
    let error = effects
        .continue_with_answer(other_decision)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains(super::store::ANSWER_REVOKED), "{error}");

    let mut other_attempt = delivery.clone();
    other_attempt.binding.attempt_id = Uuid::new_v4();
    let error = effects
        .continue_with_answer(other_attempt)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains(super::store::ANSWER_REVOKED), "{error}");
    assert!(world.harness.continues().is_empty());

    // The exact binding is accepted, once.
    effects
        .continue_with_answer(delivery.clone())
        .await
        .unwrap();
    assert_eq!(world.harness.continues().len(), 1);
    let error = effects
        .continue_with_answer(delivery)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains(super::store::ANSWER_UNCERTAIN), "{error}");
    assert_eq!(world.harness.continues().len(), 1, "never delivered twice");
}

/// #1715 finding 1: the provider started with the answer, then the daemon
/// stopped before the attempt moved to `running`, and the resumed turn ended
/// without publishing anything new. Recovery finds a durable claim and a
/// session that shows no sign of the answer: uncertain and shown, never resent.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn crash_after_the_provider_took_the_answer_is_uncertain_and_never_resent() {
    use crate::topology::executor::NodeEffects;
    let mut world = World::new(ruling_policy()).await;
    let (run, parked, delivery) = answered_node(&world, "crash-uncertain").await;
    let session = delivery.session_id;
    world
        .harness
        .executor
        .effects
        .continue_with_answer(delivery)
        .await
        .unwrap();
    assert_eq!(world.harness.continues().len(), 1);
    // The resumed turn was interrupted before it published another message.
    world
        .harness
        .set_status(session, SessionStatus::Interrupted);
    world.harness.restart();
    for _ in 0..3 {
        world.harness.executor.advance(run).await.unwrap();
    }
    assert_eq!(
        world.harness.continues().len(),
        1,
        "the answer is never sent a second time"
    );
    let blocked = world.harness.attempt(run, "A", 0, 1).await;
    assert_eq!(blocked.id, parked.id);
    assert_eq!(blocked.status, AttemptStatus::Blocked);
    assert!(
        blocked
            .error
            .as_deref()
            .unwrap()
            .contains(super::store::ANSWER_UNCERTAIN),
        "{blocked:?}"
    );
    let notices = world.harness.operator_notices();
    assert!(
        notices
            .iter()
            .any(|(_, text)| text.contains("not sent again")),
        "the operator is told: {notices:?}"
    );
}

/// #1715 finding 1: a claimed delivery whose session did take the answer (it
/// is live, or finished with new output) resumes the attempt without resending.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn crash_after_a_delivered_answer_resumes_without_resending() {
    use crate::topology::executor::NodeEffects;
    let mut world = World::new(ruling_policy()).await;
    let (live, _, delivery) = answered_node(&world, "crash-live").await;
    world
        .harness
        .executor
        .effects
        .continue_with_answer(delivery)
        .await
        .unwrap();
    let (finished, _, delivery) = answered_node(&world, "crash-finished").await;
    let session = delivery.session_id;
    world
        .harness
        .executor
        .effects
        .continue_with_answer(delivery)
        .await
        .unwrap();
    world.harness.commit_and_complete(session, "answered");
    world.harness.restart();
    for run in [live, finished] {
        world.harness.executor.advance(run).await.unwrap();
        assert_ne!(
            world.harness.attempt(run, "A", 0, 1).await.status,
            AttemptStatus::Blocked,
            "a delivered answer is not uncertain"
        );
    }
    assert_eq!(
        world.harness.attempt(live, "A", 0, 1).await.status,
        AttemptStatus::Running
    );
    assert_eq!(world.harness.continues().len(), 2, "never resent");
}

/// #1715 finding 1: a delivery that fails after the provider effect started is
/// uncertain at once, not retried like a refusal.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn delivery_failing_after_the_effect_started_is_not_retried() {
    let world = World::new(ruling_policy()).await;
    let (run, _, _) = answered_node(&world, "effect-failure").await;
    world
        .harness
        .executor
        .effects
        .fail_answer_after_effect
        .store(true, std::sync::atomic::Ordering::SeqCst);
    for _ in 0..4 {
        world.harness.executor.advance(run).await.unwrap();
    }
    assert_eq!(world.harness.continues().len(), 1, "sent exactly once");
    let blocked = world.harness.attempt(run, "A", 0, 1).await;
    assert_eq!(blocked.status, AttemptStatus::Blocked);
    assert!(
        blocked
            .error
            .as_deref()
            .unwrap()
            .contains(super::store::ANSWER_UNCERTAIN)
    );
}

/// Make the run's absolute deadline already passed.
async fn expire_execution(world: &World, run: Uuid) {
    let store = world.harness.executor.store.lock().await;
    store
        .conn
        .execute(
            "UPDATE topology_executions SET deadline_at=?2 WHERE id=?1",
            rusqlite::params![run.to_string(), hours_ago(1)],
        )
        .unwrap();
}

/// #1715 finding 3: a parked decision does not outlive the execution deadline.
/// An unanswered question is withdrawn and its wait closed; a late answer is
/// never delivered, so no provider turn starts after the deadline.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn parked_decisions_obey_the_absolute_execution_deadline() {
    let world = World::new(ruling_policy()).await;

    // Unanswered handoff question.
    let open = on_call_run(&world, None, "deadline-open").await;
    world.harness.executor.advance(open).await.unwrap();
    let session = world.harness.attempt(open, "A", 0, 1).await.session_id;
    world
        .harness
        .complete_blocked(session, "technical_impasse", Some("Which one?"));
    world.harness.executor.advance(open).await.unwrap();
    let parked = world.harness.attempt(open, "A", 0, 1).await;
    let key = parked_on(&parked);
    expire_execution(&world, open).await;
    world.harness.executor.advance(open).await.unwrap();
    let timed_out = world.harness.attempt(open, "A", 0, 1).await;
    assert_eq!(timed_out.status, AttemptStatus::Failed);
    assert_eq!(timed_out.failure_class.as_deref(), Some("timeout"));
    let record = topology_decisions(&world)
        .await
        .into_iter()
        .find(|record| record.key == key)
        .unwrap();
    assert_eq!(record.payload["status"], "withdrawn");
    {
        let store = world.harness.executor.store.lock().await;
        assert!(
            rows::node_waited(&store, parked.id)
                .unwrap()
                .open_since
                .is_none()
        );
    }

    // An answer that arrives after the deadline is not delivered.
    let late = on_call_run(&world, None, "deadline-late").await;
    world.harness.executor.advance(late).await.unwrap();
    let session = world.harness.attempt(late, "A", 0, 1).await.session_id;
    world
        .harness
        .complete_blocked(session, "technical_impasse", Some("Which one?"));
    world.harness.executor.advance(late).await.unwrap();
    let key = parked_on(&world.harness.attempt(late, "A", 0, 1).await);
    pm_rule(&world, &key, "Keep it", "rule-late").await.unwrap();
    expire_execution(&world, late).await;
    world.harness.executor.advance(late).await.unwrap();
    assert!(
        world.harness.continues().is_empty(),
        "no turn after the deadline"
    );
    let late_attempt = world.harness.attempt(late, "A", 0, 1).await;
    assert_eq!(late_attempt.status, AttemptStatus::Failed);
    assert_eq!(late_attempt.failure_class.as_deref(), Some("timeout"));

    // An exhausted review waiting for a ruling.
    let world = World::new(policy(
        &[
            ManagerCapabilityV2::Automation,
            ManagerCapabilityV2::WorkPlan,
        ],
        vec![sonnet(), luna()],
        64,
    ))
    .await;
    let (review, key) = exhausted_review(&world, "deadline-review").await;
    expire_execution(&world, review).await;
    world.harness.executor.advance(review).await.unwrap();
    let attempt = world.harness.attempt(review, "R", 0, 1).await;
    assert_eq!(attempt.status, AttemptStatus::Failed);
    assert_eq!(attempt.failure_class.as_deref(), Some("timeout"));
    let record = topology_decisions(&world)
        .await
        .into_iter()
        .find(|record| record.key == key)
        .unwrap();
    assert_eq!(record.payload["status"], "withdrawn");
}

/// #1715 finding 5: the wait for a ruling starts with the parking transition,
/// not with the first later observation. A daemon that is down for hours after
/// the node parked, and then finds the answer already present, must charge
/// those hours to the wait: the node did one productive hour, not six.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn ruling_wait_starts_at_parking_and_survives_downtime() {
    let mut world = World::new(ruling_policy()).await;
    let run = on_call_run(&world, None, "park-clock").await;
    world.harness.executor.advance(run).await.unwrap();
    let session = world.harness.attempt(run, "A", 0, 1).await.session_id;
    world
        .harness
        .complete_blocked(session, "technical_impasse", Some("Which one?"));
    world.harness.executor.advance(run).await.unwrap();
    let parked = world.harness.attempt(run, "A", 0, 1).await;
    let key = parked_on(&parked);
    // The wait is already open, from the parking transition alone.
    {
        let store = world.harness.executor.store.lock().await;
        assert!(
            rows::node_waited(&store, parked.id)
                .unwrap()
                .open_since
                .is_some(),
            "parking opens the wait atomically"
        );
    }
    assert_eq!(
        event_kinds(&world, run)
            .await
            .iter()
            .filter(|kind| *kind == "node_wait_started")
            .count(),
        1
    );

    // One productive hour, then five hours with the daemon down.
    backdate_attempt(&world, run, 6).await;
    {
        let store = world.harness.executor.store.lock().await;
        store
            .conn
            .execute(
                "UPDATE topology_events SET payload_json=json_set(payload_json,'$.detail.since',?2) \
                 WHERE attempt_id=?1 AND kind='node_wait_started'",
                rusqlite::params![parked.id.to_string(), hours_ago(5)],
            )
            .unwrap();
    }
    world.harness.restart();
    pm_rule(&world, &key, "Keep it", "rule-downtime")
        .await
        .unwrap();
    world.harness.executor.advance(run).await.unwrap();
    assert_eq!(world.harness.continues().len(), 1);
    let resumed = world.harness.attempt(run, "A", 0, 1).await;
    assert_eq!(resumed.status, AttemptStatus::Running);
    {
        let store = world.harness.executor.store.lock().await;
        let waited = rows::node_waited(&store, resumed.id).unwrap();
        assert!(waited.open_since.is_none(), "the answer closes the wait");
        assert!(waited.closed >= chrono::TimeDelta::minutes(299));
    }
    // Six hours since the start, five of them waiting: still inside the limit.
    world.harness.executor.advance(run).await.unwrap();
    assert_eq!(
        world.harness.attempt(run, "A", 0, 1).await.status,
        AttemptStatus::Running,
        "downtime waiting for a ruling is not productive node time"
    );
}

/// #1715 finding 6: a node that is first seen waiting after its productive time
/// already ran out settles `timeout` at once. Its stall exemption must go with
/// the attempt: the session is not a live topology wait any more.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn timed_out_waiting_node_does_not_keep_its_stall_exemption() {
    let world = World::new(ruling_policy()).await;
    let run = on_call_run(&world, None, "stall-timeout").await;
    world.harness.executor.advance(run).await.unwrap();
    let session = world.harness.attempt(run, "A", 0, 1).await.session_id;
    // Ten hours of work, then a wait that begins now: nothing left to pause.
    backdate_attempt(&world, run, 10).await;
    world.harness.set_waiting_since(session, chrono::Utc::now());
    world
        .harness
        .set_status(session, SessionStatus::WaitingApproval);
    world.harness.executor.advance(run).await.unwrap();
    let settled = world.harness.attempt(run, "A", 0, 1).await;
    assert_eq!(settled.status, AttemptStatus::Failed);
    assert_eq!(settled.failure_class.as_deref(), Some("timeout"));
    assert!(
        !super::oncall::awaits_on_call_answer(session),
        "a settled attempt owns no answer wait"
    );
}

/// An author `A` feeding a one-round review `R`, run as Epic A's lead.
async fn exhausted_review(world: &World, key: &str) -> (Uuid, String) {
    let mut definition = review_definition(&sonnet(), &luna());
    definition.nodes[1].params.get_mut("step").unwrap()["max_rounds"] = serde_json::json!(1);
    let flow = world
        .upsert(world.lead_a, &upsert_request(key, definition))
        .await
        .unwrap();
    let run = world
        .execute(world.lead_a, &flow, world.epic_a, key)
        .await
        .unwrap()
        .execution_id;
    world.harness.executor.advance(run).await.unwrap();
    let author = world.harness.attempt(run, "A", 0, 1).await;
    world
        .harness
        .commit_and_complete(author.session_id, "author");
    world
        .harness
        .script_reviews([ReviewScript::Changes(vec!["rename the helper"])]);
    for _ in 0..4 {
        world.harness.executor.advance(run).await.unwrap();
    }
    let parked = world.harness.attempt(run, "R", 0, 1).await;
    let decision = parked_on(&parked);
    (run, decision)
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn review_exhaustion_ruling_one_more_round_reopens_review() {
    let world = World::new(policy(
        &[
            ManagerCapabilityV2::Automation,
            ManagerCapabilityV2::WorkPlan,
        ],
        vec![sonnet(), luna()],
        64,
    ))
    .await;
    let (run, key) = exhausted_review(&world, "reopen").await;
    // Not blocked: the on-call manager is asked what to do.
    assert_eq!(world.harness.status(run).await, ExecutionStatus::Running);
    let record = topology_decisions(&world).await.remove(0);
    assert_eq!(record.key, key);
    assert_eq!(record.payload["gate"], serde_json::Value::Null);
    assert!(
        record.payload["question"]
            .as_str()
            .unwrap()
            .contains("`round`")
    );
    assert_eq!(world.harness.review_requests().len(), 1);

    // "round": a new review assignment for the same commit.
    world
        .harness
        .script_reviews([ReviewScript::Changes(vec!["still the helper"])]);
    pm_rule(&world, &key, "round", "rule-round").await.unwrap();
    for _ in 0..4 {
        world.harness.executor.advance(run).await.unwrap();
    }
    let first = world.harness.attempt(run, "R", 0, 1).await;
    assert_eq!(first.status, AttemptStatus::Failed);
    assert_eq!(first.failure_class.as_deref(), Some("review_reopened"));
    assert_eq!(world.harness.review_requests().len(), 2);
    let second = world.harness.attempt(run, "R", 0, 2).await;
    assert_ne!(second.review_assignment_id, first.review_assignment_id);

    // The second exhaustion is final: it blocks, and does not ask again.
    assert_eq!(second.status, AttemptStatus::Blocked);
    assert_eq!(
        second.failure_class.as_deref(),
        Some("review_rounds_exhausted")
    );
    assert_eq!(world.harness.status(run).await, ExecutionStatus::Blocked);
    assert_eq!(topology_decisions(&world).await.len(), 1);
}

fn review_world_policy() -> ManagerPolicyV2 {
    policy(
        &[
            ManagerCapabilityV2::Automation,
            ManagerCapabilityV2::WorkPlan,
        ],
        vec![sonnet(), luna()],
        64,
    )
}

/// #1715 finding 4: `round` is offered only when the review ledger would open
/// it. When it would not, the question says so and a `round` ruling stops the
/// execution with the reason, instead of reopening a review that fails as
/// `review_unsettled`.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn exhausted_review_shows_round_unavailable_when_the_ledger_refuses_it() {
    use super::review::ExtraRound;
    let world = World::new(review_world_policy()).await;
    world.harness.script_extra_round(ExtraRound::Unavailable(
        "manager_review_round_budget".into(),
    ));
    let (run, key) = exhausted_review(&world, "round-budget").await;
    let record = topology_decisions(&world).await.remove(0);
    let question = record.payload["question"].as_str().unwrap().to_owned();
    assert!(!question.contains("`round`"), "{question}");
    assert!(question.contains("not available"), "{question}");
    assert!(question.contains("all the rounds"), "{question}");
    assert_eq!(
        record.payload["gate"],
        serde_json::Value::Null,
        "an ordinary question the on-call manager rules on"
    );
    let asks = world.harness.extra_round_asks();
    assert_eq!(asks.len(), 1);
    assert_eq!(asks[0].node_id, "R");

    pm_rule(&world, &key, "round", "rule-unavailable-round")
        .await
        .unwrap();
    world.harness.executor.advance(run).await.unwrap();
    let review = world.harness.attempt(run, "R", 0, 1).await;
    assert_eq!(review.status, AttemptStatus::Blocked);
    assert_eq!(
        review.failure_class.as_deref(),
        Some("review_rounds_exhausted")
    );
    assert!(
        review
            .error
            .unwrap()
            .contains("manager_review_round_budget"),
        "the stop names why no round was opened"
    );
    assert_eq!(world.harness.review_requests().len(), 1, "nothing reopened");
}

/// #1715 finding 4: where the store requires a closing reviewer, the offered
/// round names it and the reopened review is requested with exactly that
/// reviewer.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn reopened_review_uses_the_closing_reviewer_the_ledger_names() {
    use super::review::ExtraRound;
    let world = World::new(review_world_policy()).await;
    let closing = rsi_common::types::ReviewerLaunch {
        provider: rsi_common::types::SessionProvider::Claude,
        model: "closing-reviewer-model".into(),
        effort: "high".into(),
    };
    world
        .harness
        .script_extra_round(ExtraRound::Closure(closing.clone()));
    let (run, key) = exhausted_review(&world, "closing").await;
    let record = topology_decisions(&world).await.remove(0);
    let question = record.payload["question"].as_str().unwrap();
    assert!(question.contains("`round`"), "{question}");
    assert!(question.contains("closing-reviewer-model"), "{question}");

    world
        .harness
        .script_reviews([ReviewScript::Changes(vec!["still the helper"])]);
    pm_rule(&world, &key, "round", "rule-closing-round")
        .await
        .unwrap();
    for _ in 0..4 {
        world.harness.executor.advance(run).await.unwrap();
    }
    let requests = world.harness.review_requests();
    assert_eq!(requests.len(), 2);
    assert_ne!(requests[0].1.reviewer.model, "closing-reviewer-model");
    assert_eq!(requests[1].1.reviewer, closing);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn review_exhaustion_rulings_accept_and_stop() {
    let world = World::new(policy(
        &[
            ManagerCapabilityV2::Automation,
            ManagerCapabilityV2::WorkPlan,
        ],
        vec![sonnet(), luna()],
        64,
    ))
    .await;
    // "accept": the review node succeeds with the ruling recorded.
    let (run, key) = exhausted_review(&world, "accept").await;
    pm_rule(&world, &key, "accept - cosmetic only", "rule-accept")
        .await
        .unwrap();
    world.harness.executor.advance(run).await.unwrap();
    let review = world.harness.attempt(run, "R", 0, 1).await;
    assert_eq!(review.status, AttemptStatus::Succeeded);
    {
        // #1715: the review's wait for the ruling opened at parking and the
        // answer closed it.
        let kinds = event_kinds(&world, run).await;
        assert_eq!(
            kinds.iter().filter(|k| *k == "node_wait_started").count(),
            1
        );
        assert_eq!(kinds.iter().filter(|k| *k == "node_wait_ended").count(), 1);
        let store = world.harness.executor.store.lock().await;
        assert!(
            rows::node_waited(&store, review.id)
                .unwrap()
                .open_since
                .is_none()
        );
    }
    let output = review.output.unwrap();
    assert_eq!(output["fields"]["verdict"], "accepted");
    assert_eq!(output["fields"]["ruling"], "accepted_as_is");
    assert_eq!(
        review.result_commit.as_deref(),
        Some(review.base_commit.as_str())
    );
    // #1740: the acceptance is a review-ledger fact for this assignment and
    // exactly the reviewed commit, recorded when the attempt settled.
    let accepted = world.harness.oncall_acceptances();
    assert_eq!(accepted.len(), 1);
    assert_eq!(Some(accepted[0].assignment_id), review.review_assignment_id);
    assert_eq!(accepted[0].commit, review.base_commit);
    assert_eq!(accepted[0].attempt_id, review.id);
    assert_eq!(accepted[0].execution_id, run);

    // "stop" (and anything not recognised): blocked exactly as before.
    let (run, key) = exhausted_review(&world, "stop").await;
    pm_rule(&world, &key, "stop", "rule-stop").await.unwrap();
    world.harness.executor.advance(run).await.unwrap();
    let review = world.harness.attempt(run, "R", 0, 1).await;
    assert_eq!(review.status, AttemptStatus::Blocked);
    assert_eq!(
        review.failure_class.as_deref(),
        Some("review_rounds_exhausted")
    );
    assert!(review.error.unwrap().contains("ruled stop"));
    assert_eq!(world.harness.status(run).await, ExecutionStatus::Blocked);
    assert_eq!(
        world.harness.oncall_acceptances().len(),
        1,
        "a stop ruling admits nothing"
    );
}

/// #1740 (#1742 P2): the answer is delivered unless a delivery was claimed.
/// An operator who manually continued the parked session with an unrelated
/// prompt before the ruling arrived did not deliver the answer: the session is
/// running, but nothing was claimed, so the answer is still sent.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn unrelated_session_activity_is_not_a_delivered_answer() {
    let world = World::new(ruling_policy()).await;
    let run = on_call_run(&world, None, "unrelated-activity").await;
    world.harness.executor.advance(run).await.unwrap();
    let session = world.harness.attempt(run, "A", 0, 1).await.session_id;
    world
        .harness
        .complete_blocked(session, "technical_impasse", Some("Which one?"));
    world.harness.executor.advance(run).await.unwrap();
    let key = parked_on(&world.harness.attempt(run, "A", 0, 1).await);
    // The operator continues the session by hand; it is running again.
    world.harness.set_status(session, SessionStatus::Running);
    pm_rule(&world, &key, "Keep it", "rule-unrelated-activity")
        .await
        .unwrap();
    world.harness.executor.advance(run).await.unwrap();
    let delivered = world.harness.continues();
    assert_eq!(delivered.len(), 1, "the answer reached the session");
    assert!(delivered[0].1.contains("Keep it"), "{delivered:?}");
    assert_eq!(
        world.harness.attempt(run, "A", 0, 1).await.status,
        AttemptStatus::Running
    );
}

/// #1704: a wait that begins and ends wholly between two observations leaves
/// no open wait for the executor to see. The session's own total of ended
/// waits carries it, so the node clock still pauses for it, and nothing
/// backdates a `node_wait_started`.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn wait_completed_between_observations_still_pauses_the_node_clock() {
    let mut world = World::new(automation()).await;
    let waited = on_call_run(&world, None, "between").await;
    let control = on_call_run(&world, None, "between-control").await;
    world.harness.executor.advance(waited).await.unwrap();
    world.harness.executor.advance(control).await.unwrap();
    let session = world.harness.attempt(waited, "A", 0, 1).await.session_id;
    // Observed running just under the wall limit.
    backdate_attempt_seconds(&world, waited, 4 * 3600 - 10).await;
    backdate_attempt_seconds(&world, control, 4 * 3600 - 10).await;
    world.harness.executor.advance(waited).await.unwrap();
    world.harness.executor.advance(control).await.unwrap();
    assert_eq!(
        world.harness.attempt(waited, "A", 0, 1).await.status,
        AttemptStatus::Running
    );
    // Before the next observation the node waited for nine seconds and was
    // answered; the clock reads four hours and a second at the next look.
    backdate_attempt_seconds(&world, waited, 4 * 3600 + 1).await;
    backdate_attempt_seconds(&world, control, 4 * 3600 + 1).await;
    world.harness.set_waited_ms(session, 9_000);
    world.harness.executor.advance(waited).await.unwrap();
    world.harness.executor.advance(control).await.unwrap();
    assert_eq!(
        world.harness.attempt(waited, "A", 0, 1).await.status,
        AttemptStatus::Running,
        "the unseen nine-second wait keeps the node inside its limit"
    );
    let attempt_id = world.harness.attempt(waited, "A", 0, 1).await.id;
    {
        let store = world.harness.executor.store.lock().await;
        let waited_for = rows::node_waited(&store, attempt_id).unwrap();
        assert!(waited_for.open_since.is_none());
        assert_eq!(waited_for.closed, chrono::TimeDelta::seconds(9));
    }
    let kinds = event_kinds(&world, waited).await;
    assert_eq!(
        kinds.iter().filter(|k| *k == "node_wait_started").count(),
        0,
        "the unseen wait is recorded as a length, not as a backdated start"
    );
    assert_eq!(kinds.iter().filter(|k| *k == "node_wait_ended").count(), 1);
    // Observing the same total again accounts for nothing twice, also after a
    // restart.
    world.harness.restart();
    world.harness.executor.advance(waited).await.unwrap();
    {
        let store = world.harness.executor.store.lock().await;
        assert_eq!(
            rows::node_waited(&store, attempt_id).unwrap().closed,
            chrono::TimeDelta::seconds(9)
        );
    }
    // Without the wait the same clock is the timeout.
    assert_eq!(
        world.harness.attempt(control, "A", 0, 1).await.status,
        AttemptStatus::Failed
    );
}

/// #1725: a resumed session's wait counter restarts at zero. The attempt's
/// baseline follows it, so a later wait that ends between two observations is
/// credited in full instead of only its growth past the old total.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn counter_reset_to_zero_moves_the_wait_baseline() {
    let mut world = World::new(automation()).await;
    let run = on_call_run(&world, None, "reset-baseline").await;
    world.harness.executor.advance(run).await.unwrap();
    let attempt = world.harness.attempt(run, "A", 0, 1).await;
    let (attempt_id, session) = (attempt.id, attempt.session_id);
    world.harness.set_waited_ms(session, 9_000);
    world.harness.executor.advance(run).await.unwrap();
    // The session resumed: its subprocess counter restarts at zero.
    world.harness.set_waited_ms(session, 0);
    world.harness.executor.advance(run).await.unwrap();
    {
        let store = world.harness.executor.store.lock().await;
        assert_eq!(
            rows::node_waited(&store, attempt_id)
                .unwrap()
                .session_seen_ms,
            0
        );
    }
    // Then a ten-second wait begins and ends between two observations.
    world.harness.set_waited_ms(session, 10_000);
    world.harness.executor.advance(run).await.unwrap();
    let store = world.harness.executor.store.lock().await;
    let waited = rows::node_waited(&store, attempt_id).unwrap();
    assert!(waited.open_since.is_none());
    assert_eq!(waited.closed, chrono::TimeDelta::milliseconds(19_000));
    assert_eq!(waited.session_seen_ms, 10_000);
}

/// #1704: an unseen finished wait followed by a wait that is open at the next
/// observation: both are paused, and the open one keeps its real start.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn unseen_wait_before_an_open_wait_keeps_both() {
    let mut world = World::new(automation()).await;
    let run = on_call_run(&world, None, "between-then-open").await;
    world.harness.executor.advance(run).await.unwrap();
    let session = world.harness.attempt(run, "A", 0, 1).await.session_id;
    backdate_attempt_seconds(&world, run, 600).await;
    world.harness.set_waited_ms(session, 5_000);
    world
        .harness
        .set_waiting_since(session, chrono::Utc::now() - chrono::Duration::seconds(30));
    world
        .harness
        .set_status(session, SessionStatus::WaitingApproval);
    world.harness.executor.advance(run).await.unwrap();
    let attempt_id = world.harness.attempt(run, "A", 0, 1).await.id;
    let store = world.harness.executor.store.lock().await;
    let waited = rows::node_waited(&store, attempt_id).unwrap();
    assert_eq!(waited.closed, chrono::TimeDelta::seconds(5));
    let age = chrono::Utc::now() - waited.open_since.expect("the wait is open");
    assert!(age >= chrono::TimeDelta::seconds(29) && age <= chrono::TimeDelta::seconds(40));
}

// ─── #1746: an operator start is owned by the Epic's project manager ───────

async fn operator_owner(
    world: &World,
    project: Option<Uuid>,
    parent: Option<Uuid>,
) -> Result<super::store::OperatorOwner> {
    let store = world.harness.executor.store.lock().await;
    agent::resolve_operator_owner(&store, project, parent)
}

fn owner_code(error: DaemonError) -> String {
    match error {
        DaemonError::StructuredRpc { data, .. } => data["code"].as_str().unwrap_or_default().into(),
        other => panic!("expected a typed refusal, got {other:?}"),
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn operator_start_resolves_the_projects_manager_from_the_epic() {
    let world = World::new(policy(
        &[ManagerCapabilityV2::Automation],
        vec![sonnet(), luna()],
        64,
    ))
    .await;
    // A managed project: the owner is the appointed manager, derived from the
    // Epic's project.
    let owner = operator_owner(&world, Some(world.project), Some(world.epic_a))
        .await
        .unwrap();
    assert_eq!(owner.manager_session_id, world.manager);
    assert_eq!(owner.epic_id, world.epic_a);

    // Nothing in the request can name another project or a non-Epic parent.
    let code = |result: Result<super::store::OperatorOwner>| owner_code(result.unwrap_err());
    assert_eq!(
        code(operator_owner(&world, Some(world.project), None).await),
        "operator_owner_epic_required"
    );
    assert_eq!(
        code(operator_owner(&world, Some(world.project), Some(world.lead_a)).await),
        "operator_owner_epic_required"
    );
    assert_eq!(
        code(operator_owner(&world, Some(Uuid::new_v4()), Some(world.epic_a)).await),
        "operator_owner_project_mismatch"
    );
    assert_eq!(
        code(operator_owner(&world, None, Some(world.epic_a)).await),
        "operator_owner_project_mismatch"
    );

    // An Epic the manager does not cover, or a paused manager, has no owner.
    rescope(
        &world,
        vec![world.epic_b],
        policy(
            &[ManagerCapabilityV2::Automation],
            vec![sonnet(), luna()],
            64,
        ),
    )
    .await;
    assert_eq!(
        code(operator_owner(&world, Some(world.project), Some(world.epic_a)).await),
        "operator_owner_manager_unauthorized"
    );
    rescope(
        &world,
        vec![world.epic_a],
        ManagerPolicyV2 {
            paused: true,
            ..policy(
                &[ManagerCapabilityV2::Automation],
                vec![sonnet(), luna()],
                64,
            )
        },
    )
    .await;
    assert_eq!(
        code(operator_owner(&world, Some(world.project), Some(world.epic_a)).await),
        "operator_owner_manager_unauthorized"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn operator_start_without_a_live_manager_is_refused() {
    let world = World::new(policy(
        &[ManagerCapabilityV2::Automation],
        vec![sonnet(), luna()],
        64,
    ))
    .await;
    set_session(&world, world.manager, SessionStatus::Archived).await;
    let error = operator_owner(&world, Some(world.project), Some(world.epic_a))
        .await
        .unwrap_err();
    assert_eq!(owner_code(error), "operator_owner_no_manager");

    // A project with no appointed manager at all is refused the same way.
    let bare = Uuid::new_v4();
    {
        let store = world.harness.executor.store.lock().await;
        store
            .conn
            .execute(
                "UPDATE sessions SET project_id=?1 WHERE id=?2",
                rusqlite::params![bare.to_string(), world.epic_c.to_string()],
            )
            .unwrap();
    }
    let error = operator_owner(&world, Some(bare), Some(world.epic_c))
        .await
        .unwrap_err();
    assert_eq!(owner_code(error), "operator_owner_no_manager");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn operator_owned_execution_lands_under_the_managers_live_authority() {
    let world = World::new(policy(
        &[ManagerCapabilityV2::Automation],
        vec![sonnet(), luna()],
        64,
    ))
    .await;
    let workflow = {
        let mut node = rsi_graph::format::NodeDef::action("A", "A");
        node.instructions = "do work".into();
        rsi_graph::format::WorkflowDefinition {
            version: "1.0".into(),
            name: "operator-owned".into(),
            description: String::new(),
            nodes: vec![node],
            edges: vec![],
            metadata: Default::default(),
        }
    };
    let start = |owner: Option<super::store::OperatorOwner>| {
        let world = &world;
        let workflow = workflow.clone();
        async move {
            let custody_plan =
                crate::session::graph_runner::plan_workflow_custody(&workflow).unwrap();
            let new = super::store::NewExecution {
                id: Uuid::new_v4(),
                topology_id: None,
                workflow_id: Uuid::new_v4(),
                definition: workflow,
                custody_plan,
                project_id: Some(world.project),
                parent_session_id: Some(world.epic_a),
                repo_root: world.harness.repo.clone(),
                base_commit: git(&world.harness.repo, &["rev-parse", "HEAD"]),
                input: None,
                requester: None,
                owner,
            };
            let store = world.harness.executor.store.lock().await;
            rows::insert_execution(&store, &new).unwrap();
            rows::load_execution(&store, new.id).unwrap().unwrap()
        }
    };
    let gate = |execution: super::store::ExecutionRow| {
        let world = &world;
        async move {
            let store = world.harness.executor.store.lock().await;
            agent::landing_gate(&store, &execution).unwrap()
        }
    };

    // No owner: nobody lands for the execution, as before.
    let unowned = start(None).await;
    assert_eq!(unowned.requested_by_kind, "operator");
    assert_eq!(gate(unowned).await, Some("owner_required"));

    // The daemon-resolved owner is recorded on the row and lands for it.
    let owner = operator_owner(&world, Some(world.project), Some(world.epic_a))
        .await
        .unwrap();
    let owned = start(Some(owner)).await;
    assert_eq!(owned.requested_by_kind, "operator");
    assert_eq!(owned.requested_by_session_id, Some(world.manager));
    assert_eq!(owned.epic_id, Some(world.epic_a));
    assert_eq!(gate(owned.clone()).await, None);

    // The owner's authority is re-derived at every enqueue.
    world
        .grant(ManagerPolicyV2 {
            paused: true,
            ..policy(
                &[ManagerCapabilityV2::Automation],
                vec![sonnet(), luna()],
                64,
            )
        })
        .await;
    assert!(gate(owned).await.is_some());
}
