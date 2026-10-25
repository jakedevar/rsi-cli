//! #3417: operator provider spend grants on the project policy.

use super::Store;
use super::manager_coordinator::tests::fixture;
use crate::test_support::test_session;
use chrono::{DateTime, Duration, Utc};
use rsi_common::harness_manager::HarnessManagerConfigV1;
use rsi_common::harness_manager_v2::{
    ConfigureHarnessManagerPolicyRequestV2, ManagerLaunchChoiceV2, ManagerPolicyV2,
};
use rsi_common::provider_grant::ManagerProviderGrantV2;
use rsi_common::types::SessionProvider;
use std::path::PathBuf;
use uuid::Uuid;

fn grant(budget_usd: f64) -> ManagerProviderGrantV2 {
    ManagerProviderGrantV2 {
        provider: SessionProvider::OpenRouter,
        budget_usd,
        expires_at: None,
        efforts: vec!["high".into()],
        enable_bench: false,
        // A client value is ignored: the daemon assigns the baseline.
        granted_at: Some(DateTime::<Utc>::UNIX_EPOCH),
    }
}

fn launch(provider: SessionProvider, model: &str, effort: &str) -> ManagerLaunchChoiceV2 {
    ManagerLaunchChoiceV2 {
        provider,
        model: model.into(),
        effort: Some(effort.into()),
    }
}

fn openrouter(model: &str, effort: &str) -> ManagerLaunchChoiceV2 {
    launch(SessionProvider::OpenRouter, model, effort)
}

/// A project with a saved policy carrying `grants` and `allowed`.
fn project(
    store: &Store,
    grants: Vec<ManagerProviderGrantV2>,
    allowed: Vec<ManagerLaunchChoiceV2>,
) -> HarnessManagerConfigV1 {
    let policy = ManagerPolicyV2 {
        provider_grants: grants,
        allowed_launches: allowed,
        ..ManagerPolicyV2::default()
    };
    fixture(store, policy).0
}

/// Settle `cost` USD of OpenRouter spend now for `project` (an invocation
/// recorded with the project and no session).
fn spend(store: &Store, project: Uuid, cost: f64) {
    invocation(store, Some(project), None, "OpenRouter", cost);
}

/// One admitted, settled invocation of `provider` costing `cost`.
fn invocation(
    store: &Store,
    project: Option<Uuid>,
    session: Option<Uuid>,
    provider: &str,
    cost: f64,
) {
    let at = Utc::now() + Duration::seconds(1);
    store
        .conn
        .execute(
            "INSERT INTO model_invocations(id, purpose, invocation_kind, foreground,
                                           paid_risk, admission_status, status, provider,
                                           model, trigger_source, estimated_cost_usd,
                                           project_id, session_id, created_at, completed_at)
             VALUES(?1, 'bench.task', 'session_lifecycle', 'foreground',
                    'paid_capable', 'admitted', 'completed', ?5,
                    'z-ai/glm-5.3-flash', 'test', ?2, ?3, ?4, ?6, ?6)",
            rusqlite::params![
                Uuid::new_v4().to_string(),
                cost,
                project.map(|id| id.to_string()),
                session.map(|id| id.to_string()),
                provider,
                at.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
            ],
        )
        .unwrap();
}

/// A session of `project` (`None`: of no project) with `cost` of `provider`
/// usage recorded against it only (no project on the invocation row).
fn session_spend(store: &Store, project: Option<Uuid>, provider: &str, cost: f64) -> Uuid {
    let mut session = test_session(Uuid::new_v4(), PathBuf::from("/var/tmp/grant-spend"));
    session.project_id = project;
    store.insert_session(&session).unwrap();
    invocation(store, None, Some(session.id), provider, cost);
    session.id
}

/// A bench run of `project` whose one task session (no project of its own)
/// settled `cost`.
fn bench_spend(store: &Store, project: Uuid, cost: f64) {
    let session = session_spend(store, None, "Harness", cost);
    let run = Uuid::new_v4().to_string();
    let stamp = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    store
        .conn
        .execute(
            "INSERT INTO bench_runs(id, project_id, config_path, config_digest, dataset_commit,
                                    docker_host, model, effort, budget_usd, per_task_cap_usd,
                                    concurrency, tasks_json, loop_knobs_json, state, created_at)
             VALUES(?1, ?2, 'c', 'd', 'x', 'h', 'm', 'high', 10, 1, 1, '[]', '{}', 'active', ?3)",
            rusqlite::params![run, project.to_string(), stamp],
        )
        .unwrap();
    store
        .conn
        .execute(
            "INSERT INTO bench_tasks(run_id, trial, task, attempt, session_id, compose_project,
                                     container, workdir, agent_timeout_sec, request_digest,
                                     state, started_at)
             VALUES(?1, 't1', 'task', 1, ?2, 'cp', 'c', '/w', 60, 'rd', 'finished', ?3)",
            rusqlite::params![run, session.to_string(), stamp],
        )
        .unwrap();
}

fn gate(
    store: &Store,
    config: &HarnessManagerConfigV1,
    launch: &ManagerLaunchChoiceV2,
) -> Result<(), String> {
    let own = store
        .get_harness_manager_policy(config.project_id)
        .unwrap()
        .unwrap()
        .policy
        .allowed_launches;
    store
        .manager_launch_policy_gate(config, &own, launch)
        .map_err(|error| error.to_string())
}

fn resave(store: &Store, config: &HarnessManagerConfigV1, edit: impl FnOnce(&mut ManagerPolicyV2)) {
    let mut saved = store
        .get_harness_manager_policy(config.project_id)
        .unwrap()
        .unwrap();
    edit(&mut saved.policy);
    store
        .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
            project_id: config.project_id,
            expected_scope_version: config.row_version,
            expected_policy_version: saved.row_version,
            idempotency_key: Uuid::new_v4().to_string(),
            policy: saved.policy,
        })
        .unwrap();
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn a_grant_admits_any_model_of_its_provider_at_its_efforts_without_an_exact_tuple() {
    let store = Store::open_in_memory().unwrap();
    let config = project(&store, vec![grant(100.0)], vec![]);
    gate(&store, &config, &openrouter("z-ai/glm-5.3-flash", "high")).unwrap();
    gate(&store, &config, &openrouter("never/listed-model", "high")).unwrap();

    // An effort outside the grant is refused, naming the grant and its efforts.
    let refusal = gate(&store, &config, &openrouter("z-ai/glm-5.3-flash", "low")).unwrap_err();
    assert!(
        refusal.contains("manager_v2_provider_grant_effort"),
        "{refusal}"
    );
    assert!(refusal.contains("OpenRouter"), "{refusal}");
    assert!(refusal.contains("[high]"), "{refusal}");

    // Another provider has no grant: the ordinary refusal, no grant text.
    let other = gate(
        &store,
        &config,
        &launch(SessionProvider::Claude, "m", "high"),
    )
    .unwrap_err();
    assert!(other.contains("manager_v2_launch_not_granted"), "{other}");
    assert!(!other.contains("provider_grant"), "{other}");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn the_daemon_assigns_the_spend_baseline_and_a_raise_keeps_it() {
    let store = Store::open_in_memory().unwrap();
    let config = project(&store, vec![grant(100.0)], vec![]);
    let first = store
        .project_provider_grants(config.project_id)
        .unwrap()
        .remove(0);
    let granted_at = first.granted_at.unwrap();
    assert!(
        granted_at > Utc::now() - Duration::minutes(5),
        "a client baseline is ignored: {granted_at}"
    );

    resave(&store, &config, |policy| {
        policy.provider_grants[0].budget_usd = 150.0;
        policy.provider_grants[0].granted_at = Some(DateTime::<Utc>::UNIX_EPOCH);
    });
    let raised = store
        .project_provider_grants(config.project_id)
        .unwrap()
        .remove(0);
    assert_eq!(raised.budget_usd, 150.0);
    assert_eq!(raised.granted_at, Some(granted_at));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn spend_is_charged_from_settled_cost_and_the_budget_refuses_with_one_operator_todo() {
    let store = Store::open_in_memory().unwrap();
    let config = project(&store, vec![grant(100.0)], vec![]);
    spend(&store, config.project_id, 60.0);
    spend(&store, config.project_id, 39.99);
    gate(&store, &config, &openrouter("z-ai/glm-5.3-flash", "high")).unwrap();

    spend(&store, config.project_id, 0.01);
    let model = openrouter("z-ai/glm-5.3-flash", "high");
    let refusal = gate(&store, &config, &model).unwrap_err();
    assert!(
        refusal.contains("manager_v2_provider_grant_exhausted"),
        "{refusal}"
    );
    assert!(refusal.contains("$100.00 of $100 used"), "{refusal}");
    assert!(
        refusal.contains("Policy) > Provider grants > OpenRouter grant budget (USD)"),
        "{refusal}"
    );
    // A second refusal files nothing new.
    gate(&store, &config, &model).unwrap_err();
    let todos = store
        .operator_todo_open_system_keys("provider_grant_exhausted:")
        .unwrap();
    assert_eq!(todos.len(), 1, "{todos:?}");

    // Raising the grant keeps the spend and admits again.
    resave(&store, &config, |policy| {
        policy.provider_grants[0].budget_usd = 150.0;
    });
    gate(&store, &config, &model).unwrap();
    spend(&store, config.project_id, 50.0);
    gate(&store, &config, &model).unwrap_err();
    assert_eq!(
        store
            .operator_todo_open_system_keys("provider_grant_exhausted:")
            .unwrap()
            .len(),
        2,
        "the raised grant files its own item when it is spent"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn an_expired_grant_behaves_like_no_grant() {
    let store = Store::open_in_memory().unwrap();
    let mut expired = grant(100.0);
    expired.expires_at = Some(Utc::now() - Duration::hours(1));
    let config = project(&store, vec![expired], vec![]);
    assert!(
        store
            .project_provider_grant_launches(config.project_id)
            .unwrap()
            .is_empty()
    );
    let refusal = gate(&store, &config, &openrouter("z-ai/glm-5.3-flash", "high")).unwrap_err();
    assert!(
        refusal.contains("manager_v2_provider_grant_expired"),
        "{refusal}"
    );
    assert!(
        store
            .operator_todo_open_system_keys("provider_grant_exhausted:")
            .unwrap()
            .is_empty(),
        "an expiry files no spent-grant item"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn exact_allowed_launch_tuples_keep_working_unchanged() {
    let store = Store::open_in_memory().unwrap();
    let exact = openrouter("deepseek/deepseek-v4.1-flash", "high");
    let config = project(&store, vec![grant(10.0)], vec![exact.clone()]);
    spend(&store, config.project_id, 10.0);
    // The grant is spent: its any-model path is closed, the exact tuple is not.
    gate(&store, &config, &exact).unwrap();
    gate(&store, &config, &openrouter("other/model", "high")).unwrap_err();

    // With no grant at all the exact list behaves exactly as before.
    let plain = project(&store, vec![], vec![exact.clone()]);
    gate(&store, &plain, &exact).unwrap();
    let refusal = gate(&store, &plain, &openrouter("other/model", "high")).unwrap_err();
    assert!(
        refusal.contains("manager_v2_launch_not_granted"),
        "{refusal}"
    );
    assert!(!refusal.contains("provider_grant"), "{refusal}");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn the_resources_row_shows_each_grant_with_its_spend_and_the_bench_setting() {
    let store = Store::open_in_memory().unwrap();
    let config = project(&store, vec![grant(100.0)], vec![]);
    spend(&store, config.project_id, 12.4);
    let row = store.manager_v2_resource_snapshot(&config).unwrap();
    let grants = row["provider_grants"].as_array().unwrap();
    assert_eq!(grants.len(), 1);
    assert_eq!(
        grants[0]["summary"],
        "OpenRouter: $12.40 of $100 used, no expiry"
    );
    assert_eq!(row["bench_enabled"], false);
    store.set_daemon_setting("bench.enabled", "true").unwrap();
    let row = store.manager_v2_resource_snapshot(&config).unwrap();
    assert_eq!(row["bench_enabled"], true);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn a_grant_is_charged_only_the_granting_projects_usage() {
    let store = Store::open_in_memory().unwrap();
    let config = project(&store, vec![grant(100.0)], vec![]);
    let other = project(&store, vec![grant(100.0)], vec![]);

    // Another project's OpenRouter and Harness usage, however it is
    // recorded (project on the row, project session, bench run), is not
    // charged to this grant.
    spend(&store, other.project_id, 30.0);
    session_spend(&store, Some(other.project_id), "OpenRouter", 20.0);
    session_spend(&store, Some(other.project_id), "Harness", 10.0);
    bench_spend(&store, other.project_id, 15.0);
    // A project-less session belongs to nobody's grant.
    session_spend(&store, None, "OpenRouter", 5.0);
    let spent = |config: &HarnessManagerConfigV1| {
        let grants = store.project_provider_grants(config.project_id).unwrap();
        store
            .provider_grant_spend_usd(config.project_id, &grants[0])
            .unwrap()
    };
    assert_eq!(spent(&config), 0.0);
    gate(&store, &config, &openrouter("z-ai/glm-5.3-flash", "high")).unwrap();
    assert!((spent(&other) - 75.0).abs() < 1e-9, "{}", spent(&other));

    // This project's own usage is charged: its row, its session, its bench.
    spend(&store, config.project_id, 40.0);
    session_spend(&store, Some(config.project_id), "OpenRouter", 25.0);
    assert!((spent(&config) - 65.0).abs() < 1e-9, "{}", spent(&config));
    gate(&store, &config, &openrouter("z-ai/glm-5.3-flash", "high")).unwrap();
    bench_spend(&store, config.project_id, 35.0);
    assert!((spent(&config) - 100.0).abs() < 1e-9, "{}", spent(&config));
    let refusal = gate(&store, &config, &openrouter("z-ai/glm-5.3-flash", "high")).unwrap_err();
    assert!(
        refusal.contains("manager_v2_provider_grant_exhausted"),
        "{refusal}"
    );
    // The other project, with 75 of its 100 charged, still launches.
    gate(&store, &other, &openrouter("z-ai/glm-5.3-flash", "high")).unwrap();
}
