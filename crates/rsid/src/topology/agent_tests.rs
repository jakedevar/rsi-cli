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
use super::tests::{Harness, error_code, git};
use crate::error::{DaemonError, Result};

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
        let request = AgentTopologyExecuteRequestV1 {
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
