//! K14a (#672): the delegated `operator_call` manager action under
//! `OperatorDelegation`. Real journal, admission, runtime gates, projection,
//! retention and receipts; the RPC surface is exercised read-only.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::large_futures,
    clippy::significant_drop_tightening,
    clippy::too_many_lines
)]

use super::*;
use rsi_common::manager_operator_delegation::{
    DAEMON_SETTINGS_OPERATOR_METHODS, DELEGABLE_OPERATOR_METHODS, DELEGATED_PAGE_MAX_BYTES,
    DELEGATED_PAGE_MAX_ROWS, DelegatedListSessionsParamsV1, NEVER_DELEGABLE_OPERATOR_METHODS_V1,
    OPERATOR_METHOD_NOT_DELEGABLE, OPERATOR_PARAMS_INVALID, OperatorCallFenceV1,
    OperatorCallResultV1, OperatorCallV1,
};
use rsi_common::types::{SandboxCleanupState, WakeMode};
use serde_json::{Value, json};

const RPC_SOURCE: &str = include_str!("../../rpc.rs");

/// Pilot plus an `OperatorDelegation` grant (policy version 2).
async fn op_pilot(mode: ManagerOperatingModeV2) -> Pilot {
    let p = pilot().await;
    let mut policy = p.policy.clone();
    policy.mode = mode;
    policy
        .capabilities
        .push(ManagerCapabilityV2::OperatorDelegation);
    reconfigure(&p, 1, policy);
    p
}

fn reconfigure(p: &Pilot, expected_policy_version: i64, policy: ManagerPolicyV2) {
    p.manager
        .store
        .try_lock()
        .unwrap()
        .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
            project_id: p.project,
            expected_scope_version: 1,
            expected_policy_version,
            idempotency_key: format!("k14-policy-{expected_policy_version}"),
            policy,
        })
        .unwrap();
}

/// A project leaf whose last activity is `age_hours` ago.
async fn leaf(p: &Pilot, parent: Option<Uuid>, status: SessionStatus, age_hours: i64) -> Uuid {
    let id = Uuid::new_v4();
    let mut row = bare_session(id);
    row.project_id = Some(p.project);
    row.working_dir = p.repo.clone();
    row.session_kind = SessionKind::Task;
    row.parent_id = parent;
    row.status = status;
    row.title = Some("K14 archive candidate".into());
    row.updated_at = chrono::Utc::now() - chrono::Duration::hours(age_hours);
    p.manager.store.lock().await.insert_session(&row).unwrap();
    p.manager
        .completed
        .write()
        .await
        .insert(id, CompletedSession::for_test(row));
    id
}

async fn row(p: &Pilot, id: Uuid) -> Session {
    p.manager
        .store
        .lock()
        .await
        .get_session(id)
        .unwrap()
        .unwrap()
}

fn archive_worktree(p: &Pilot) -> std::path::PathBuf {
    let path = p._dir.path().join(format!("archive-{}", Uuid::new_v4()));
    let branch = format!("archive-{}", Uuid::new_v4());
    git(
        &p.repo,
        &["worktree", "add", "-qb", &branch, path.to_str().unwrap()],
    );
    path
}

async fn mark_live_worktree(p: &Pilot, id: Uuid, path: &std::path::Path) {
    p.manager
        .store
        .lock()
        .await
        .conn
        .execute(
            "UPDATE sessions SET sandbox_kind='GitWorktree',sandbox_root=?2,sandbox_cleanup_state='Live' WHERE id=?1",
            rusqlite::params![id.to_string(), path.to_str().unwrap()],
        )
        .unwrap();
}

fn call(
    method: &str,
    params: Value,
    fence: Option<chrono::DateTime<chrono::Utc>>,
) -> ManagerActionV2 {
    ManagerActionV2::OperatorCall {
        call: OperatorCallV1 {
            method: method.into(),
            params,
        },
        expected: fence.map(|session_updated_at| OperatorCallFenceV1 { session_updated_at }),
    }
}

async fn archive(p: &Pilot, id: Uuid) -> ManagerActionV2 {
    call(
        "ArchiveSession",
        json!({ "session_id": id }),
        Some(row(p, id).await.updated_at),
    )
}

async fn control_as(
    p: &Pilot,
    caller: Uuid,
    policy_version: i64,
    key: &str,
    operation: ManagerActionV2,
) -> Result<ManagerActionReceiptV2> {
    let mut request = p.request(key, operation);
    request.fence.policy_version = policy_version;
    p.manager
        .agent_control()
        .agent_manager_control(caller, request)
        .await
}

async fn control(
    p: &Pilot,
    key: &str,
    operation: ManagerActionV2,
) -> Result<ManagerActionReceiptV2> {
    control_as(p, p.owner, 2, key, operation).await
}

async fn refused_with(p: &Pilot, key: &str, operation: ManagerActionV2, code: &str) {
    let error = control(p, key, operation).await.unwrap_err().to_string();
    assert!(error.contains(code), "expected {code}, got {error}");
}

fn actor(p: &Pilot, operation: Uuid) -> Option<String> {
    p.manager
        .store
        .try_lock()
        .unwrap()
        .conn
        .query_row(
            "SELECT actor_session_id FROM harness_manager_v2_operations WHERE id=?1",
            [operation.to_string()],
            |r| r.get(0),
        )
        .unwrap()
}

/// Every method name dispatched by `RpcServer::handle_request_inner_unboxed`,
/// plus the pre-dispatch `Subscribe` special case.
fn rpc_catalog() -> Vec<String> {
    let start = RPC_SOURCE
        .find("let result = match request.method.as_str() {")
        .expect("dispatch match");
    let end = start
        + RPC_SOURCE[start..]
            .find("\n            _ => {")
            .expect("dispatch default arm");
    let quoted = regex::Regex::new(r#""([A-Z][A-Za-z0-9]+)""#).unwrap();
    let mut methods = vec!["Subscribe".to_string()];
    for line in RPC_SOURCE[start..end].lines() {
        let line = line.trim_start();
        if !line.starts_with('"') {
            continue;
        }
        let head = line.split("=>").next().unwrap();
        methods.extend(quoted.captures_iter(head).map(|c| c[1].to_string()));
    }
    methods.sort();
    methods.dedup();
    methods
}

/// The attributed-caller verb lists, as `agent_gate` derives them from the
/// single registry declaration (#1011).
fn registry_list(methods: &[&str]) -> Vec<String> {
    methods.iter().map(|m| (*m).to_string()).collect()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn operator_call_catalog_refuses_every_non_allowlisted_rpc_method() {
    let p = op_pilot(ManagerOperatingModeV2::Execute).await;
    let journaled = || -> i64 {
        p.manager
            .store
            .try_lock()
            .unwrap()
            .conn
            .query_row(
                "SELECT count(*) FROM harness_manager_v2_operations",
                [],
                |r| r.get(0),
            )
            .unwrap()
    };
    let before = journaled();
    let catalog = rpc_catalog();
    assert!(
        catalog.len() > 150,
        "dispatch catalog parsed: {}",
        catalog.len()
    );
    for method in DELEGABLE_OPERATOR_METHODS {
        // #1046: `ProposeDaemonSetting` is a delegated-only method with no
        // operator RPC arm (the operator sets its bounds through
        // `ConfigureHarnessManagerPolicy`); it is not dispatchable at all.
        if DAEMON_SETTINGS_OPERATOR_METHODS.contains(method) {
            assert!(!catalog.iter().any(|m| m == method), "{method}");
            continue;
        }
        assert!(catalog.iter().any(|m| m == method), "allowlisted {method}");
    }
    // K14b: the v2 allowlist entry is a real operator method and delegable.
    assert!(DELEGABLE_OPERATOR_METHODS.contains(&"UnarchiveSession"));
    assert!(catalog.iter().any(|m| m == "UnarchiveSession"));
    for method in NEVER_DELEGABLE_OPERATOR_METHODS_V1 {
        assert!(
            catalog.iter().any(|m| m == method),
            "never-delegable {method} is a real method"
        );
        assert!(!DELEGABLE_OPERATOR_METHODS.contains(method), "{method}");
    }
    for (i, method) in catalog
        .iter()
        .filter(|m| !DELEGABLE_OPERATOR_METHODS.contains(&m.as_str()))
        .enumerate()
    {
        let error = control(&p, &format!("k14-deny-{i}"), call(method, json!({}), None))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(OPERATOR_METHOD_NOT_DELEGABLE),
            "{method}: {error}"
        );
    }
    // Nothing was journaled by any refusal.
    assert_eq!(journaled(), before);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn operator_call_leaves_the_tokened_gate_and_agent_catalog_unchanged() {
    // The delegated path adds no socket verb: every attributed-caller verb is
    // an `Agent*` wrapper (other Epics add their own), none is an operator
    // method, and READ_VERBS is pinned exactly.
    let agent_verbs = registry_list(rsi_common::rpc_verb_registry::agent_verb_methods());
    let read_verbs = registry_list(rsi_common::rpc_verb_registry::read_verb_methods());
    assert!(
        agent_verbs.iter().all(|verb| verb.starts_with("Agent")),
        "{agent_verbs:?}"
    );
    assert_eq!(
        read_verbs,
        vec![
            "GetSession",
            "GetSessionSummary",
            "GetHealthStatus",
            "GetDaemonCapabilities",
            "ListSessionChildren",
            "GetConversation",
            "GetSessionDiagnostics",
            "GetConversationsSince",
            "GetTurnMetrics",
        ]
    );
    assert!(agent_verbs.contains(&"AgentManagerControl".to_string()));
    assert!(agent_verbs.contains(&"AgentManagerGetAction".to_string()));
    for method in DELEGABLE_OPERATOR_METHODS
        .iter()
        .chain(NEVER_DELEGABLE_OPERATOR_METHODS_V1)
    {
        assert!(!agent_verbs.iter().any(|v| v == method), "{method}");
        assert!(!read_verbs.iter().any(|v| v == method), "{method}");
    }
    assert_eq!(
        rsi_common::agent_control_schema::agent_control_catalog_v1().len(),
        agent_verbs.len()
    );

    // End to end: a tokened `ArchiveSession` still meets the attribution gate.
    let dir = TempDir::new().unwrap();
    let store = Store::open(&dir.path().join("rsi.db")).unwrap();
    let runtime_config = std::sync::Arc::new(RuntimeConfig::from_config(&Config::from_env()));
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
        .unwrap(),
    );
    let http = reqwest::Client::new();
    let server = crate::rpc::RpcServer::new(
        std::sync::Arc::clone(&manager),
        None,
        None,
        None,
        std::sync::Arc::clone(&runtime_config),
        crate::prompt_compile::CompileEngine::new(
            http.clone(),
            manager.store().clone(),
            std::sync::Arc::clone(&runtime_config),
            std::sync::Arc::clone(&bus),
        ),
        http,
        crate::model_control::ModelControlRuntime::default_normal(),
    );
    let (client, daemon) = tokio::net::UnixStream::pair().unwrap();
    let serve = tokio::spawn(async move { server.handle_connection(daemon).await });
    let (reader, mut writer) = client.into_split();
    let request = json!({"jsonrpc":"2.0","id":1,"method":"ArchiveSession",
        "params":{"session_id":Uuid::new_v4()},"session_token":"agent-token"});
    tokio::io::AsyncWriteExt::write_all(&mut writer, format!("{request}\n").as_bytes())
        .await
        .unwrap();
    let mut lines = tokio::io::AsyncBufReadExt::lines(tokio::io::BufReader::new(reader));
    let response: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    let message = response["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("not available to session-attributed callers"),
        "{response}"
    );
    drop(writer);
    serve.abort();
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn operator_call_grants_nothing_to_non_managers_or_ungranted_policies() {
    let p = op_pilot(ManagerOperatingModeV2::Execute).await;
    let target = leaf(&p, None, SessionStatus::Completed, 25).await;
    // A lead (or any non-manager caller) gains nothing from the grant.
    let error = control_as(&p, p.lead, 2, "k14-lead", archive(&p, target).await)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("manager_v2_capability_denied"), "{error}");
    // Monitor mode keeps the grant inert.
    let mut monitor = p.policy.clone();
    monitor.mode = ManagerOperatingModeV2::Monitor;
    monitor
        .capabilities
        .push(ManagerCapabilityV2::OperatorDelegation);
    reconfigure(&p, 2, monitor);
    let error = control_as(&p, p.owner, 3, "k14-monitor", archive(&p, target).await)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("manager_v2_execute_required"), "{error}");
    // A paused policy refuses.
    let mut paused = p.policy.clone();
    paused.paused = true;
    paused
        .capabilities
        .push(ManagerCapabilityV2::OperatorDelegation);
    reconfigure(&p, 3, paused);
    let error = control_as(&p, p.owner, 4, "k14-paused", archive(&p, target).await)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("manager_v2_policy_paused"), "{error}");
    // Without the grant the manager itself is refused.
    reconfigure(&p, 4, p.policy.clone());
    let error = control_as(&p, p.owner, 5, "k14-ungranted", archive(&p, target).await)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("manager_v2_capability_denied"), "{error}");
    assert_eq!(row(&p, target).await.status, SessionStatus::Completed);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn operator_call_revoked_after_queueing_settles_revoked_and_keeps_the_session() {
    let p = op_pilot(ManagerOperatingModeV2::Execute).await;
    let target = leaf(&p, None, SessionStatus::Completed, 25).await;
    let queued = control(&p, "k14-revoke", archive(&p, target).await)
        .await
        .unwrap();
    assert_eq!(queued.state, ManagerActionStateV2::Queued);
    reconfigure(&p, 2, p.policy.clone());
    p.manager.reconcile_manager_actions_once().await.unwrap();
    let receipt = p.receipt(queued.operation_id).await;
    assert_eq!(receipt.state, ManagerActionStateV2::Revoked);
    assert_eq!(receipt.operator_result, None);
    assert_eq!(row(&p, target).await.status, SessionStatus::Completed);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn operator_call_archives_an_idle_project_leaf_logically_with_manager_attribution() {
    let p = op_pilot(ManagerOperatingModeV2::Execute).await;
    // Outside the manager's Group scope, inside its project.
    let target = leaf(&p, None, SessionStatus::Completed, 25).await;
    let sandbox = p.repo.join("k14-sandbox");
    std::fs::create_dir(&sandbox).unwrap();
    p.manager
        .store
        .lock()
        .await
        .conn
        .execute(
            "UPDATE sessions SET sandbox_root=?2,sandbox_branch='rsi/k14' WHERE id=?1",
            rusqlite::params![target.to_string(), sandbox.to_string_lossy()],
        )
        .unwrap();
    let queued = control(&p, "k14-archive", archive(&p, target).await)
        .await
        .unwrap();
    assert_eq!(queued.action_kind, ManagerActionKindV2::OperatorCall);
    assert_eq!(
        queued.target_type,
        ManagerActionTargetTypeV2::ProviderSession
    );
    assert_eq!(queued.target_session_id, Some(target));
    p.execute().await.unwrap();
    let receipt = p.receipt(queued.operation_id).await;
    assert_eq!(receipt.state, ManagerActionStateV2::Succeeded);
    assert_eq!(
        receipt.result,
        Some(ManagerActionResultV2::OperatorCallSucceeded)
    );
    let Some(OperatorCallResultV1::Scalar { method, result }) =
        receipt.operator_result.clone().map(|r| *r)
    else {
        panic!("scalar operator result: {receipt:?}");
    };
    assert_eq!(method, "ArchiveSession");
    assert_eq!(result["disposition"], "no_cleanup_required");
    assert_eq!(actor(&p, queued.operation_id), Some(p.owner.to_string()));
    // Logical only: the row, its sandbox fields and the worktree remain.
    let archived = row(&p, target).await;
    assert_eq!(archived.status, SessionStatus::Archived);
    assert_eq!(archived.sandbox_root.as_deref(), Some(sandbox.as_path()));
    assert_eq!(archived.sandbox_branch.as_deref(), Some("rsi/k14"));
    assert!(sandbox.is_dir());
    assert!(!p.manager.completed.read().await.contains_key(&target));
    // The same receipt is readable through AgentManagerGetAction and replay.
    let read = p
        .manager
        .agent_control()
        .agent_manager_get_action(
            p.owner,
            AgentManagerGetActionRequestV2 {
                project_id: None,
                operation_id: queued.operation_id,
            },
        )
        .await
        .unwrap();
    assert_eq!(read.operator_result, receipt.operator_result);
    let replay = control(
        &p,
        "k14-archive",
        call(
            "ArchiveSession",
            json!({ "session_id": target }),
            Some(row(&p, target).await.updated_at),
        ),
    )
    .await;
    // A changed payload under the same key is a conflict, never a new effect.
    assert!(replay.is_err() || replay.unwrap().operation_id == queued.operation_id);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn operator_call_reads_archive_cleanup_status_for_a_project_session() {
    let p = op_pilot(ManagerOperatingModeV2::Execute).await;
    let target = leaf(&p, None, SessionStatus::Completed, 1).await;
    let queued = control(
        &p,
        "k14-status",
        call(
            "GetArchiveCleanupStatus",
            json!({ "session_id": target }),
            None,
        ),
    )
    .await
    .unwrap();
    assert_eq!(queued.target_session_id, Some(target));
    p.execute().await.unwrap();
    let receipt = p.receipt(queued.operation_id).await;
    assert_eq!(receipt.state, ManagerActionStateV2::Succeeded);
    let Some(OperatorCallResultV1::Scalar { method, result }) = receipt.operator_result.map(|r| *r)
    else {
        panic!("scalar status result");
    };
    assert_eq!(method, "GetArchiveCleanupStatus");
    assert_eq!(result["session_id"], json!(target));
    // A read never changes the session.
    assert_eq!(row(&p, target).await.status, SessionStatus::Completed);
}

fn insert_job(p: &Pilot, wake_session: Uuid, wake_mode: WakeMode) {
    use crate::session::harness::tools::schedule_wake::{
        ScheduleWakeRequest, build_agent_scheduled_job,
    };
    let mut job = build_agent_scheduled_job(ScheduleWakeRequest {
        message: "Continue".into(),
        in_seconds: Some(60),
        at: None,
        name: None,
        every_seconds: None,
        mode: Some("resume".into()),
        working_dir: p.repo.clone(),
        provider: Some(SessionProvider::Claude),
        model: Some("manager-scripted-provider".into()),
        project_id: Some(p.project),
        origin_session_id: Some(wake_session),
        watch_session_id: None,
    })
    .unwrap();
    job.wake_session_id = Some(wake_session);
    job.wake_mode = wake_mode;
    job.enabled = true;
    p.manager
        .store
        .try_lock()
        .unwrap()
        .insert_scheduled_job(&job)
        .unwrap();
}

fn insert_review(p: &Pilot, work_key: &str, author: Uuid, reviewer: Option<Uuid>) {
    let stamp = "2026-09-23T00:00:00.000000000Z";
    let store = p.manager.store.try_lock().unwrap();
    store.conn.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
    store
        .conn
        .execute(
            "INSERT INTO manager_review_assignments(assignment_id,project_id,epic_id,
                manager_session_id,scope_version,work_key,spec_revision,author_session_id,
                source_sha,reviewer_session_id,action_operation_id,state,row_version,
                request_json,request_fingerprint,created_at,updated_at)
             VALUES(?1,?2,?3,?4,1,?12,1,?5,?6,?7,?8,?9,1,'{}',?10,?11,?11)",
            rusqlite::params![
                Uuid::new_v4().to_string(),
                p.project.to_string(),
                p.epic.to_string(),
                p.owner.to_string(),
                author.to_string(),
                "a".repeat(40),
                reviewer.map(|r| r.to_string()),
                reviewer.map(|_| Uuid::new_v4().to_string()),
                if reviewer.is_some() {
                    "allocating"
                } else {
                    "reserved"
                },
                format!("sha256:{}", "b".repeat(64)),
                stamp,
                work_key
            ],
        )
        .unwrap();
    store.conn.execute_batch("PRAGMA foreign_keys=ON").unwrap();
}

fn insert_work(p: &Pilot, key: &str, source: Uuid, payload_extra: &Value) {
    let stamp = "2026-09-23T00:00:00.000000000Z";
    let mut payload = json!({"key": key, "epic_id": p.epic, "source_session_id": source});
    for (k, v) in payload_extra.as_object().unwrap() {
        payload[k] = v.clone();
    }
    p.manager
        .store
        .try_lock()
        .unwrap()
        .conn
        .execute(
            "INSERT INTO harness_manager_v2_work_facts(project_id,kind,record_key,epic_id,
                work_key,row_version,payload_json,archived,manager_session_id,scope_version,
                policy_version,created_at,updated_at)
             VALUES(?1,'work',?2,?3,?2,1,?4,0,?5,1,2,?6,?6)",
            rusqlite::params![
                p.project.to_string(),
                key,
                p.epic.to_string(),
                payload.to_string(),
                p.owner.to_string(),
                stamp
            ],
        )
        .unwrap();
}

fn insert_event(p: &Pilot, session: Uuid, hours_ago: i64) {
    let at = (chrono::Utc::now() - chrono::Duration::hours(hours_ago))
        .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    p.manager
        .store
        .try_lock()
        .unwrap()
        .conn
        .execute(
            "INSERT INTO conversation_events(session_id,sequence,event_type,role,content,created_at)
             VALUES(?1,1,'Message','assistant','done',?2)",
            rusqlite::params![session.to_string(), at],
        )
        .unwrap();
}

/// One event with an explicit sequence and stored timestamp text.
fn insert_event_at(p: &Pilot, session: Uuid, sequence: i64, created_at: &str) {
    p.manager
        .store
        .try_lock()
        .unwrap()
        .conn
        .execute(
            "INSERT INTO conversation_events(session_id,sequence,event_type,role,content,created_at)
             VALUES(?1,?2,'Message','assistant','done',?3)",
            rusqlite::params![session.to_string(), sequence, created_at],
        )
        .unwrap();
}

fn hours_ago(hours: i64) -> chrono::DateTime<chrono::Utc> {
    chrono::Utc::now() - chrono::Duration::hours(hours)
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn operator_call_archive_uses_the_latest_event_time_not_the_latest_sequence() {
    let p = op_pilot(ManagerOperatingModeV2::Execute).await;
    // Review K14_RETENTION_EVENT_MAX: sequence 1 is recent, sequence 2 is old,
    // and the two stored texts use different RFC3339 offset spellings.
    let recent = leaf(&p, None, SessionStatus::Completed, 25).await;
    insert_event_at(&p, recent, 1, &hours_ago(1).to_rfc3339());
    insert_event_at(
        &p,
        recent,
        2,
        &hours_ago(48).to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
    );
    refused_with(
        &p,
        "k14-event-max",
        archive(&p, recent).await,
        "manager_v2_retention_recent_activity",
    )
    .await;
    assert_eq!(row(&p, recent).await.status, SessionStatus::Completed);

    // An unparseable event time fails closed as recent activity.
    let unknown = leaf(&p, None, SessionStatus::Completed, 25).await;
    insert_event_at(&p, unknown, 1, "not-a-timestamp");
    refused_with(
        &p,
        "k14-event-unknown",
        archive(&p, unknown).await,
        "manager_v2_retention_recent_activity",
    )
    .await;
    assert_eq!(row(&p, unknown).await.status, SessionStatus::Completed);

    // Review 23382ccc: an offsetless event that SQLite would read as OLDER than
    // a valid 30-hour-old event still fails closed (chrono rejects it).
    let offsetless = leaf(&p, None, SessionStatus::Completed, 25).await;
    insert_event_at(
        &p,
        offsetless,
        1,
        &hours_ago(48).format("%Y-%m-%dT%H:%M:%S").to_string(),
    );
    insert_event_at(&p, offsetless, 2, &hours_ago(30).to_rfc3339());
    refused_with(
        &p,
        "k14-event-offsetless",
        archive(&p, offsetless).await,
        "manager_v2_retention_recent_activity",
    )
    .await;
    assert_eq!(row(&p, offsetless).await.status, SessionStatus::Completed);

    // Control: both events older than 24 h, so the idle session archives.
    let idle = leaf(&p, None, SessionStatus::Completed, 25).await;
    insert_event_at(&p, idle, 1, &hours_ago(30).to_rfc3339());
    insert_event_at(
        &p,
        idle,
        2,
        &hours_ago(48).to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
    );
    let queued = control(&p, "k14-event-idle", archive(&p, idle).await)
        .await
        .unwrap();
    p.execute().await.unwrap();
    assert_eq!(
        p.receipt(queued.operation_id).await.state,
        ManagerActionStateV2::Succeeded
    );
    assert_eq!(row(&p, idle).await.status, SessionStatus::Archived);

    // The same maximum is applied by the effect-time re-check.
    let raced = leaf(&p, None, SessionStatus::Completed, 25).await;
    insert_event_at(
        &p,
        raced,
        1,
        &hours_ago(30).to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
    );
    control(&p, "k14-event-race", archive(&p, raced).await)
        .await
        .unwrap();
    insert_event_at(&p, raced, 0, &hours_ago(2).to_rfc3339());
    let error = p.execute().await.unwrap_err().to_string();
    assert!(
        error.contains("manager_v2_retention_recent_activity"),
        "{error}"
    );
    assert_eq!(row(&p, raced).await.status, SessionStatus::Completed);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn operator_call_archive_refuses_every_retention_exclusion() {
    let p = op_pilot(ManagerOperatingModeV2::Execute).await;

    let pinned = leaf(&p, None, SessionStatus::Completed, 25).await;
    p.manager.toggle_pin(pinned).await.unwrap();
    // The current lead of a live Epic (the pilot lead, idle for a day).
    p.manager
        .store
        .lock()
        .await
        .conn
        .execute(
            "UPDATE sessions SET updated_at=?2 WHERE id=?1",
            rusqlite::params![
                p.lead.to_string(),
                (chrono::Utc::now() - chrono::Duration::hours(25)).to_rfc3339()
            ],
        )
        .unwrap();
    // A non-resume wake remains a retention gate; archive cancels resume wakes.
    let wake_own = leaf(&p, None, SessionStatus::Completed, 25).await;
    insert_job(&p, wake_own, WakeMode::Fresh);
    let wake_resume = leaf(&p, None, SessionStatus::Completed, 25).await;
    insert_job(&p, wake_resume, WakeMode::Resume);
    let wake_watched = leaf(&p, None, SessionStatus::Completed, 25).await;
    insert_job(&p, p.owner, WakeMode::OnTerminal(wake_watched));
    let author = leaf(&p, None, SessionStatus::Completed, 25).await;
    insert_review(&p, "k14-review-author", author, None);
    let reviewer = leaf(&p, None, SessionStatus::Completed, 25).await;
    insert_review(&p, "k14-review-reviewer", p.lead, Some(reviewer));
    let sealed = leaf(&p, None, SessionStatus::Completed, 25).await;
    insert_work(
        &p,
        "k14-sealed",
        sealed,
        &json!({"source_commit": "c".repeat(40)}),
    );
    let accepted = leaf(&p, None, SessionStatus::Completed, 25).await;
    insert_work(
        &p,
        "k14-accepted",
        accepted,
        &json!({"acceptance": {"method": "review"}}),
    );
    let recent_event = leaf(&p, None, SessionStatus::Completed, 25).await;
    insert_event(&p, recent_event, 23);
    let recent_row = leaf(&p, None, SessionStatus::Completed, 23).await;
    let worktree = leaf(&p, None, SessionStatus::Completed, 25).await;
    p.manager
        .store
        .lock()
        .await
        .conn
        .execute(
            "UPDATE sessions SET sandbox_kind='GitWorktree',sandbox_cleanup_state='Live' WHERE id=?1",
            [worktree.to_string()],
        )
        .unwrap();
    let running = leaf(&p, None, SessionStatus::Running, 25).await;

    for (id, code) in [
        (pinned, "manager_v2_retention_pinned"),
        (p.lead, "manager_v2_session_is_lead"),
        (wake_own, "manager_v2_retention_enabled_wake"),
        (wake_watched, "manager_v2_retention_enabled_wake"),
        (author, "manager_v2_retention_live_review"),
        (reviewer, "manager_v2_retention_live_review"),
        (sealed, "manager_v2_retention_sealed_source"),
        (accepted, "manager_v2_retention_sealed_source"),
        (recent_event, "manager_v2_retention_recent_activity"),
        (recent_row, "manager_v2_retention_recent_activity"),
        (worktree, "manager_v2_retention_live_worktree"),
        (running, "manager_v2_session_not_terminal"),
    ] {
        let before = row(&p, id).await.status;
        refused_with(&p, &format!("k14-keep-{id}"), archive(&p, id).await, code).await;
        assert_eq!(row(&p, id).await.status, before, "{code}");
    }

    control(&p, "k14-cancel-resume-wake", archive(&p, wake_resume).await)
        .await
        .unwrap();
    p.execute().await.unwrap();
    assert_eq!(row(&p, wake_resume).await.status, SessionStatus::Archived);
    let enabled_resume_wakes: i64 = p
        .manager
        .store
        .lock()
        .await
        .conn
        .query_row(
            "SELECT count(*) FROM scheduled_jobs
             WHERE wake_session_id=?1 AND wake_mode='resume' AND enabled=1",
            [wake_resume.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(enabled_resume_wakes, 0);

    // Positive control: integrated work no longer protects its source.
    let integrated = leaf(&p, None, SessionStatus::Completed, 25).await;
    insert_work(
        &p,
        "k14-integrated",
        integrated,
        &json!({"source_commit": "d".repeat(40), "integration": {"source_commit": "d".repeat(40)}}),
    );
    control(&p, "k14-integrated", archive(&p, integrated).await)
        .await
        .unwrap();
    p.execute().await.unwrap();
    assert_eq!(row(&p, integrated).await.status, SessionStatus::Archived);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn operator_call_archives_clean_tracked_live_worktree_without_changing_custody() {
    let p = op_pilot(ManagerOperatingModeV2::Execute).await;
    let target = leaf(&p, None, SessionStatus::Completed, 25).await;
    let worktree = archive_worktree(&p);
    mark_live_worktree(&p, target, &worktree).await;
    insert_work(
        &p,
        "archive-unmerged-source",
        target,
        &json!({"source_commit": git(&worktree, &["rev-parse", "HEAD"])}),
    );
    std::fs::write(worktree.join("untracked"), "retained\n").unwrap();
    let branch = git(&worktree, &["branch", "--show-current"]);
    let head = git(&worktree, &["rev-parse", "HEAD"]);

    let queued = control(&p, "archive-clean-worktree", archive(&p, target).await)
        .await
        .unwrap();
    p.execute().await.unwrap();
    assert_eq!(
        p.receipt(queued.operation_id).await.state,
        ManagerActionStateV2::Succeeded
    );
    let archived = row(&p, target).await;
    assert_eq!(archived.status, SessionStatus::Archived);
    assert_eq!(archived.sandbox_root.as_deref(), Some(worktree.as_path()));
    assert_eq!(
        archived.sandbox_cleanup_state,
        Some(SandboxCleanupState::Live)
    );
    assert_eq!(git(&worktree, &["branch", "--show-current"]), branch);
    assert_eq!(git(&worktree, &["rev-parse", "HEAD"]), head);
    assert_eq!(
        std::fs::read_to_string(worktree.join("untracked")).unwrap(),
        "retained\n"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn operator_call_refuses_dirty_or_in_progress_live_worktree_at_admission_and_effect() {
    let p = op_pilot(ManagerOperatingModeV2::Execute).await;
    let target = leaf(&p, None, SessionStatus::Completed, 25).await;
    let worktree = archive_worktree(&p);
    mark_live_worktree(&p, target, &worktree).await;
    std::fs::write(worktree.join("source"), "dirty tracked source\n").unwrap();
    refused_with(
        &p,
        "archive-dirty-worktree",
        archive(&p, target).await,
        "manager_v2_retention_live_worktree",
    )
    .await;
    git(&worktree, &["restore", "source"]);

    let git_dir = std::path::PathBuf::from(git(&worktree, &["rev-parse", "--absolute-git-dir"]));
    std::fs::write(
        git_dir.join("MERGE_HEAD"),
        git(&worktree, &["rev-parse", "HEAD"]),
    )
    .unwrap();
    refused_with(
        &p,
        "archive-merging-worktree",
        archive(&p, target).await,
        "manager_v2_retention_live_worktree",
    )
    .await;
    std::fs::remove_file(git_dir.join("MERGE_HEAD")).unwrap();

    let queued = control(
        &p,
        "archive-worktree-effect-race",
        archive(&p, target).await,
    )
    .await
    .unwrap();
    std::fs::write(worktree.join("source"), "dirty after admission\n").unwrap();
    let error = p.execute().await.unwrap_err().to_string();
    assert!(
        error.contains("manager_v2_retention_live_worktree"),
        "{error}"
    );
    assert_eq!(
        p.receipt(queued.operation_id).await.state,
        ManagerActionStateV2::Running
    );
    assert_eq!(row(&p, target).await.status, SessionStatus::Completed);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn manager_paused_terminal_epic_archives_clean_live_worktree_without_resuming() {
    let p = op_pilot(ManagerOperatingModeV2::Execute).await;
    let worktree = archive_worktree(&p);
    mark_live_worktree(&p, p.lead, &worktree).await;
    let epic = row(&p, p.epic).await;
    let action = ManagerActionV2::ArchiveContainer {
        container_id: p.epic,
        expected_updated_at: epic.updated_at,
    };
    std::fs::write(worktree.join("source"), "dirty tracked source\n").unwrap();
    refused_with(
        &p,
        "archive-container-dirty-worktree",
        action.clone(),
        "manager_v2_retention_live_worktree",
    )
    .await;
    git(&worktree, &["restore", "source"]);
    let git_dir = std::path::PathBuf::from(git(&worktree, &["rev-parse", "--absolute-git-dir"]));
    std::fs::write(
        git_dir.join("CHERRY_PICK_HEAD"),
        git(&worktree, &["rev-parse", "HEAD"]),
    )
    .unwrap();
    refused_with(
        &p,
        "archive-container-cherry-pick",
        action.clone(),
        "manager_v2_retention_live_worktree",
    )
    .await;
    std::fs::remove_file(git_dir.join("CHERRY_PICK_HEAD")).unwrap();
    {
        let store = p.manager.store.lock().await;
        let config = store.get_harness_manager(p.project).unwrap().unwrap();
        store
            .manager_v2_set_lead_pause(
                &config,
                p.epic,
                Uuid::new_v4(),
                Some(p.owner),
                Some(p.lead),
                "terminal archive",
            )
            .unwrap();
    }
    let queued = control(&p, "archive-paused-terminal-epic", action)
        .await
        .unwrap();
    p.execute().await.unwrap();
    assert_eq!(
        p.receipt(queued.operation_id).await.state,
        ManagerActionStateV2::Succeeded
    );
    assert_eq!(row(&p, p.epic).await.status, SessionStatus::Archived);
    assert_eq!(row(&p, p.lead).await.status, SessionStatus::Archived);
    let store = p.manager.store.lock().await;
    let config = store.get_harness_manager(p.project).unwrap().unwrap();
    assert!(store.manager_v2_lead_pause(&config, p.epic).unwrap().1);
    assert!(worktree.exists());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn lead_pause_allows_delegated_cleanup_and_keeps_continuations_stopped() {
    let p = op_pilot(ManagerOperatingModeV2::Execute).await;
    let target = leaf(&p, Some(p.epic), SessionStatus::Completed, 30).await;
    insert_job(&p, p.lead, WakeMode::Resume);
    {
        let store = p.manager.store.lock().await;
        store
            .conn
            .execute(
                "UPDATE scheduled_jobs SET enabled=0 WHERE wake_session_id=?1",
                [p.lead.to_string()],
            )
            .unwrap();
        let config = store.get_harness_manager(p.project).unwrap().unwrap();
        store
            .manager_v2_set_lead_pause(
                &config,
                p.epic,
                Uuid::new_v4(),
                Some(p.owner),
                Some(p.lead),
                "stop lead continuations",
            )
            .unwrap();
        assert!(store.manager_v2_lead_pause(&config, p.epic).unwrap().1);
    }

    // A lead pause still fences new work under the Epic.
    let create = control(
        &p,
        "paused-create-session",
        ManagerActionV2::CreateSession {
            parent_id: p.epic,
            kind: SessionKind::Task,
            query: "new work".into(),
            launch: p.policy.allowed_launches[0].clone(),
            sandbox_source: None,
        },
    )
    .await
    .unwrap();
    let claim = p.claim().await;
    let error = p
        .manager
        .check_manager_action_runtime(&claim, false)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("manager_v2_manager_paused"));
    p.manager
        .store
        .lock()
        .await
        .finish_manager_action(
            &claim,
            ManagerActionStateV2::Blocked,
            "manager_v2_manager_paused",
        )
        .unwrap();
    assert_eq!(
        p.receipt(create.operation_id).await.state,
        ManagerActionStateV2::Blocked
    );

    for (key, action, method) in [
        (
            "paused-list",
            call("ListSessions", json!({"limit": 64}), None),
            "ListSessions",
        ),
        (
            "paused-status",
            call(
                "GetArchiveCleanupStatus",
                json!({"session_id": target}),
                None,
            ),
            "GetArchiveCleanupStatus",
        ),
        (
            "paused-archive",
            archive(&p, target).await,
            "ArchiveSession",
        ),
    ] {
        let queued = control(&p, key, action).await.unwrap();
        p.execute().await.unwrap();
        let receipt = p.receipt(queued.operation_id).await;
        assert_eq!(receipt.state, ManagerActionStateV2::Succeeded, "{method}");
        let returned_method = match *receipt.operator_result.unwrap() {
            OperatorCallResultV1::Scalar { method, .. }
            | OperatorCallResultV1::Page { method, .. } => method,
        };
        assert_eq!(returned_method, method);
    }
    assert_eq!(row(&p, target).await.status, SessionStatus::Archived);
    {
        let store = p.manager.store.lock().await;
        let config = store.get_harness_manager(p.project).unwrap().unwrap();
        assert!(store.manager_v2_lead_pause(&config, p.epic).unwrap().1);
        let enabled_wakes: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM scheduled_jobs WHERE wake_session_id=?1 AND enabled=1",
                [p.lead.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(enabled_wakes, 0);
    }
    let queued = control(&p, "paused-unarchive", unarchive(&p, target).await)
        .await
        .unwrap();
    p.execute().await.unwrap();
    assert_eq!(
        p.receipt(queued.operation_id).await.state,
        ManagerActionStateV2::Succeeded
    );
    assert_eq!(row(&p, target).await.status, SessionStatus::Completed);

    let store = p.manager.store.lock().await;
    let config = store.get_harness_manager(p.project).unwrap().unwrap();
    assert!(store.manager_v2_lead_pause(&config, p.epic).unwrap().1);
    let disabled_wakes: i64 = store
        .conn
        .query_row(
            "SELECT count(*) FROM scheduled_jobs WHERE wake_session_id=?1 AND enabled=0",
            [p.lead.to_string()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(disabled_wakes, 1);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn operator_policy_pause_still_refuses_delegated_cleanup() {
    let p = op_pilot(ManagerOperatingModeV2::Execute).await;
    let target = leaf(&p, Some(p.epic), SessionStatus::Completed, 30).await;
    let mut policy = p.policy.clone();
    policy
        .capabilities
        .push(ManagerCapabilityV2::OperatorDelegation);
    policy.paused_epic_ids.push(p.epic);
    reconfigure(&p, 2, policy.clone());
    let error = control_as(
        &p,
        p.owner,
        3,
        "paused-epic-archive",
        archive(&p, target).await,
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("manager_v2_policy_paused"));
    assert_eq!(row(&p, target).await.status, SessionStatus::Completed);

    policy.paused_epic_ids.clear();
    policy.paused = true;
    reconfigure(&p, 3, policy);
    let error = control_as(
        &p,
        p.owner,
        4,
        "paused-global-list",
        call("ListSessions", json!({}), None),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("manager_v2_policy_paused"));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn operator_call_archive_rechecks_retention_inside_the_effect_transaction() {
    let p = op_pilot(ManagerOperatingModeV2::Execute).await;
    let target = leaf(&p, None, SessionStatus::Completed, 25).await;
    let queued = control(&p, "k14-race", archive(&p, target).await)
        .await
        .unwrap();
    // A terminal watch armed after admission is caught at effect time.
    insert_job(&p, p.owner, WakeMode::OnTerminal(target));
    let error = p.execute().await.unwrap_err().to_string();
    assert!(
        error.contains("manager_v2_retention_enabled_wake"),
        "{error}"
    );
    assert_eq!(row(&p, target).await.status, SessionStatus::Completed);
    assert_eq!(
        p.receipt(queued.operation_id).await.state,
        ManagerActionStateV2::Running
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn operator_call_reach_is_the_grant_project_only() {
    let p = op_pilot(ManagerOperatingModeV2::Execute).await;
    let foreign_project = Uuid::new_v4();
    let foreign = Uuid::new_v4();
    {
        let store = p.manager.store.lock().await;
        store
            .insert_project(&Project {
                id: foreign_project,
                name: "Foreign project".into(),
                path: None,
                description: None,
                color: Project::DEFAULT_COLOR.into(),
                context_files: None,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            })
            .unwrap();
        let mut row = bare_session(foreign);
        row.project_id = Some(foreign_project);
        row.session_kind = SessionKind::Task;
        row.updated_at = chrono::Utc::now() - chrono::Duration::hours(48);
        store.insert_session(&row).unwrap();
    }
    refused_with(
        &p,
        "k14-foreign",
        archive(&p, foreign).await,
        "manager_v2_target_out_of_project",
    )
    .await;
    refused_with(
        &p,
        "k14-foreign-status",
        call(
            "GetArchiveCleanupStatus",
            json!({ "session_id": foreign }),
            None,
        ),
        "manager_v2_target_out_of_project",
    )
    .await;
    assert_eq!(row(&p, foreign).await.status, SessionStatus::Completed);

    let mine = leaf(&p, None, SessionStatus::Completed, 25).await;
    let queued = control(&p, "k14-list", call("ListSessions", json!({}), None))
        .await
        .unwrap();
    assert_eq!(queued.target_type, ManagerActionTargetTypeV2::Project);
    p.execute().await.unwrap();
    let Some(OperatorCallResultV1::Page { rows, .. }) = p
        .receipt(queued.operation_id)
        .await
        .operator_result
        .map(|r| *r)
    else {
        panic!("page result");
    };
    let ids: Vec<Uuid> = rows.iter().map(|r| r.id).collect();
    assert!(ids.contains(&mine) && ids.contains(&p.lead) && ids.contains(&p.epic));
    assert!(rows.iter().all(|r| r.id != foreign));
    let listed = rows.iter().find(|r| r.id == mine).unwrap();
    assert_eq!(listed.archive_blocker, None);
    let lead = rows.iter().find(|r| r.id == p.lead).unwrap();
    assert_eq!(
        lead.archive_blocker.as_deref(),
        Some("manager_v2_session_is_lead")
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn operator_call_list_sessions_pages_are_byte_bounded_and_complete() {
    let p = op_pilot(ManagerOperatingModeV2::Execute).await;
    for _ in 0..200 {
        leaf(&p, Some(p.epic), SessionStatus::Completed, 30).await;
    }
    let expected: Vec<(String, Uuid)> = {
        let store = p.manager.store.lock().await;
        let mut stmt = store
            .conn
            .prepare(
                "SELECT updated_at,id FROM sessions WHERE project_id=?1 ORDER BY updated_at,id",
            )
            .unwrap();
        stmt.query_map([p.project.to_string()], |r| {
            Ok((
                r.get::<_, String>(0)?,
                Uuid::parse_str(&r.get::<_, String>(1)?).unwrap(),
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
    };
    assert_eq!(expected.len(), 204);
    let mut seen = Vec::new();
    let mut params = DelegatedListSessionsParamsV1::default();
    let mut pages = 0;
    loop {
        let page = p
            .manager
            .store
            .lock()
            .await
            .delegated_list_sessions(p.project, &params, chrono::Utc::now())
            .unwrap();
        let bytes = serde_json::to_vec(&page).unwrap().len();
        assert!(bytes <= DELEGATED_PAGE_MAX_BYTES, "page is {bytes} bytes");
        let OperatorCallResultV1::Page {
            rows,
            next_after,
            row_count,
            ..
        } = page
        else {
            panic!("page");
        };
        assert!(rows.len() <= usize::from(DELEGATED_PAGE_MAX_ROWS));
        assert_eq!(usize::from(row_count), rows.len());
        seen.extend(rows.iter().map(|r| (r.updated_at.clone(), r.id)));
        pages += 1;
        match next_after {
            Some(cursor) => {
                // A full page names its last row as the cursor.
                assert_eq!(cursor.id, rows.last().unwrap().id);
                params.after = Some(cursor);
            }
            None => break,
        }
        assert!(pages < 50, "pagination terminates");
    }
    assert!(pages > 1);
    assert_eq!(seen, expected);

    // One page also travels end to end through the receipt.
    let queued = control(
        &p,
        "k14-page",
        call("ListSessions", json!({"limit": 64}), None),
    )
    .await
    .unwrap();
    p.execute().await.unwrap();
    let receipt = p.receipt(queued.operation_id).await;
    let Some(OperatorCallResultV1::Page {
        rows, next_after, ..
    }) = receipt.operator_result.map(|r| *r)
    else {
        panic!("page receipt");
    };
    assert!(!rows.is_empty());
    assert_eq!(next_after.unwrap().id, rows.last().unwrap().id);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn operator_call_fences_and_backpressure_are_enforced() {
    let p = op_pilot(ManagerOperatingModeV2::Execute).await;
    let target = leaf(&p, None, SessionStatus::Completed, 25).await;
    refused_with(
        &p,
        "k14-unfenced",
        call("ArchiveSession", json!({ "session_id": target }), None),
        "manager_v2_operator_fence_required",
    )
    .await;
    refused_with(
        &p,
        "k14-stale",
        call(
            "ArchiveSession",
            json!({ "session_id": target }),
            Some(chrono::Utc::now()),
        ),
        "manager_v2_session_changed",
    )
    .await;
    refused_with(
        &p,
        "k14-bad-params",
        call("ArchiveSession", json!({ "session": target }), None),
        "manager_v2_operator_params_invalid",
    )
    .await;
    for i in 0..64 {
        control(
            &p,
            &format!("k14-fill-{i}"),
            call("ListSessions", json!({}), None),
        )
        .await
        .unwrap();
    }
    refused_with(
        &p,
        "k14-overflow",
        call("ListSessions", json!({}), None),
        "manager_v2_action_queue_full",
    )
    .await;
    assert_eq!(row(&p, target).await.status, SessionStatus::Completed);
}

async fn unarchive(p: &Pilot, id: Uuid) -> ManagerActionV2 {
    call(
        "UnarchiveSession",
        json!({ "session_id": id }),
        Some(row(p, id).await.updated_at),
    )
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn operator_call_archive_preserves_marker_through_operator_unarchive() {
    let p = op_pilot(ManagerOperatingModeV2::Execute).await;
    let target = leaf(&p, None, SessionStatus::Interrupted, 25).await;
    p.manager
        .store
        .lock()
        .await
        .record_manager_operator_pause(target, true)
        .unwrap();

    let queued = control(&p, "k14-interrupted-archive", archive(&p, target).await)
        .await
        .unwrap();
    p.execute().await.unwrap();
    assert_eq!(
        p.receipt(queued.operation_id).await.state,
        ManagerActionStateV2::Succeeded
    );
    assert_eq!(row(&p, target).await.status, SessionStatus::Archived);
    assert!(
        p.manager
            .store
            .lock()
            .await
            .manager_action_operator_paused(target)
            .unwrap()
    );

    p.manager.unarchive_session(target).await.unwrap();
    assert_eq!(row(&p, target).await.status, SessionStatus::Completed);
    assert!(
        p.manager
            .store
            .lock()
            .await
            .manager_action_operator_paused(target)
            .unwrap()
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn delegated_operator_archive_cancels_retry_without_provider_launch() {
    let p = op_pilot(ManagerOperatingModeV2::Execute).await;
    let target = leaf(&p, None, SessionStatus::Failed, 0).await;
    p.manager
        .store
        .lock()
        .await
        .conn
        .execute(
            "UPDATE sessions SET retry_attempt=0,max_retries=2 WHERE id=?1",
            [target.to_string()],
        )
        .unwrap();
    let (retry_cancel, mut retry_observer) = tokio::sync::oneshot::channel();
    p.manager
        .completed
        .write()
        .await
        .get_mut(&target)
        .unwrap()
        .retry_cancel = Some(retry_cancel);

    let queued = control(
        &p,
        "delegated-archive-cancels-retry",
        archive(&p, target).await,
    )
    .await
    .unwrap();
    p.execute().await.unwrap();

    let row = row(&p, target).await;
    assert_eq!(row.status, SessionStatus::Archived);
    assert_eq!(row.retry_attempt, Some(2));
    assert!(retry_observer.try_recv().is_ok());
    let receipt = p.receipt(queued.operation_id).await;
    assert_eq!(receipt.state, ManagerActionStateV2::Succeeded);
    let invocations: i64 = p
        .manager
        .store
        .lock()
        .await
        .conn
        .query_row("SELECT count(*) FROM model_invocations", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(invocations, 0);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn operator_call_unarchive_restores_an_archived_project_leaf_logically() {
    let p = op_pilot(ManagerOperatingModeV2::Execute).await;
    let target = leaf(&p, None, SessionStatus::Completed, 25).await;
    let sandbox = p.repo.join("k14b-sandbox");
    std::fs::create_dir(&sandbox).unwrap();
    p.manager
        .store
        .lock()
        .await
        .conn
        .execute(
            "UPDATE sessions SET sandbox_root=?2,sandbox_branch='rsi/k14b' WHERE id=?1",
            rusqlite::params![target.to_string(), sandbox.to_string_lossy()],
        )
        .unwrap();
    control(&p, "k14b-archive", archive(&p, target).await)
        .await
        .unwrap();
    p.execute().await.unwrap();
    assert_eq!(row(&p, target).await.status, SessionStatus::Archived);

    let queued = control(&p, "k14b-unarchive", unarchive(&p, target).await)
        .await
        .unwrap();
    assert_eq!(queued.target_session_id, Some(target));
    assert_eq!(
        queued.target_type,
        ManagerActionTargetTypeV2::ProviderSession
    );
    p.execute().await.unwrap();
    let receipt = p.receipt(queued.operation_id).await;
    assert_eq!(receipt.state, ManagerActionStateV2::Succeeded);
    assert_eq!(
        receipt.result,
        Some(ManagerActionResultV2::OperatorCallSucceeded)
    );
    let Some(OperatorCallResultV1::Scalar { method, result }) = receipt.operator_result.map(|r| *r)
    else {
        panic!("scalar unarchive result");
    };
    assert_eq!(method, "UnarchiveSession");
    assert_eq!(result["status"], "Completed");
    assert_eq!(actor(&p, queued.operation_id), Some(p.owner.to_string()));
    // Back to its prior non-archived status; row, sandbox fields and worktree
    // are unchanged, and the session is hydrated in memory again.
    let restored = row(&p, target).await;
    assert_eq!(restored.status, SessionStatus::Completed);
    assert!(!restored.pending_archive);
    assert_eq!(restored.sandbox_root.as_deref(), Some(sandbox.as_path()));
    assert_eq!(restored.sandbox_branch.as_deref(), Some("rsi/k14b"));
    assert_eq!(restored.title.as_deref(), Some("K14 archive candidate"));
    assert!(sandbox.is_dir());
    let hydrated = p.manager.completed.read().await;
    assert_eq!(
        hydrated.get(&target).map(|c| c.session.status),
        Some(SessionStatus::Completed)
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn operator_call_unarchive_refusals_leave_the_row_archived() {
    let p = op_pilot(ManagerOperatingModeV2::Execute).await;
    let target = leaf(&p, None, SessionStatus::Archived, 30).await;

    // Another project's archived session.
    let foreign_project = Uuid::new_v4();
    let foreign = Uuid::new_v4();
    {
        let store = p.manager.store.lock().await;
        store
            .insert_project(&Project {
                id: foreign_project,
                name: "Foreign project".into(),
                path: None,
                description: None,
                color: Project::DEFAULT_COLOR.into(),
                context_files: None,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            })
            .unwrap();
        let mut row = bare_session(foreign);
        row.project_id = Some(foreign_project);
        row.session_kind = SessionKind::Task;
        row.status = SessionStatus::Archived;
        store.insert_session(&row).unwrap();
    }
    refused_with(
        &p,
        "k14b-foreign",
        unarchive(&p, foreign).await,
        "manager_v2_target_out_of_project",
    )
    .await;
    // Fence mismatch and a missing fence.
    refused_with(
        &p,
        "k14b-stale",
        call(
            "UnarchiveSession",
            json!({ "session_id": target }),
            Some(chrono::Utc::now()),
        ),
        "manager_v2_session_changed",
    )
    .await;
    refused_with(
        &p,
        "k14b-unfenced",
        call("UnarchiveSession", json!({ "session_id": target }), None),
        "manager_v2_operator_fence_required",
    )
    .await;
    // A lead or a worker caller gains nothing: a lead is not the manager
    // (capability denied); a worker has no manager scope at all.
    let worker = leaf(&p, Some(p.epic), SessionStatus::Completed, 30).await;
    for (key, caller, code) in [
        ("k14b-lead", p.lead, "manager_v2_capability_denied"),
        ("k14b-worker", worker, "manager_scope_denied"),
    ] {
        let error = control_as(&p, caller, 2, key, unarchive(&p, target).await)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains(code), "{key}: {error}");
    }
    // Monitor mode, then a paused policy, then no grant.
    let mut monitor = p.policy.clone();
    monitor.mode = ManagerOperatingModeV2::Monitor;
    monitor
        .capabilities
        .push(ManagerCapabilityV2::OperatorDelegation);
    reconfigure(&p, 2, monitor);
    let error = control_as(&p, p.owner, 3, "k14b-monitor", unarchive(&p, target).await)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("manager_v2_execute_required"), "{error}");
    let mut paused = p.policy.clone();
    paused.paused = true;
    paused
        .capabilities
        .push(ManagerCapabilityV2::OperatorDelegation);
    reconfigure(&p, 3, paused);
    let error = control_as(&p, p.owner, 4, "k14b-paused", unarchive(&p, target).await)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("manager_v2_policy_paused"), "{error}");
    reconfigure(&p, 4, p.policy.clone());
    let error = control_as(
        &p,
        p.owner,
        5,
        "k14b-ungranted",
        unarchive(&p, target).await,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("manager_v2_capability_denied"), "{error}");

    assert_eq!(row(&p, target).await.status, SessionStatus::Archived);
    assert_eq!(row(&p, foreign).await.status, SessionStatus::Archived);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn operator_call_unarchive_of_a_non_archived_session_is_refused_not_a_no_op() {
    // Chosen behaviour: refused with a typed code (not a silent success), so a
    // manager never reads a succeeded receipt for a restore that did nothing.
    let p = op_pilot(ManagerOperatingModeV2::Execute).await;
    let completed = leaf(&p, None, SessionStatus::Completed, 30).await;
    refused_with(
        &p,
        "k14b-not-archived",
        unarchive(&p, completed).await,
        "manager_v2_session_state_changed",
    )
    .await;
    assert_eq!(row(&p, completed).await.status, SessionStatus::Completed);
}

/// Pilot plus exactly the given grants (policy version 2), Execute mode.
async fn grant_pilot(grants: &[ManagerCapabilityV2]) -> Pilot {
    let p = pilot().await;
    let mut policy = p.policy.clone();
    policy.mode = ManagerOperatingModeV2::Execute;
    policy.capabilities.extend_from_slice(grants);
    reconfigure(&p, 1, policy);
    p
}

fn journaled_operations(p: &Pilot) -> i64 {
    p.manager
        .store
        .try_lock()
        .unwrap()
        .conn
        .query_row(
            "SELECT count(*) FROM harness_manager_v2_operations",
            [],
            |r| r.get(0),
        )
        .unwrap()
}

fn storage_calls() -> [(&'static str, ManagerActionV2); 3] {
    [
        ("status", call("GetSandboxStorageStatus", json!({}), None)),
        (
            "preview",
            call(
                "RunSandboxBuildCacheReclaim",
                json!({"dry_run": true}),
                None,
            ),
        ),
        (
            "real",
            call(
                "RunSandboxBuildCacheReclaim",
                json!({"dry_run": false}),
                None,
            ),
        ),
    ]
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn storage_control_is_denied_without_the_capability() {
    // OperatorDelegation alone never reaches the storage methods, and no
    // refusal is journaled.
    let p = grant_pilot(&[ManagerCapabilityV2::OperatorDelegation]).await;
    let before = journaled_operations(&p);
    for (name, operation) in storage_calls() {
        refused_with(
            &p,
            &format!("storage-denied-{name}"),
            operation,
            "manager_v2_capability_denied",
        )
        .await;
    }
    assert_eq!(journaled_operations(&p), before);
    // A lead gains nothing from a StorageControl grant either.
    let p = grant_pilot(&[ManagerCapabilityV2::StorageControl]).await;
    for (name, operation) in storage_calls() {
        let error = control_as(&p, p.lead, 2, &format!("storage-lead-{name}"), operation)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("manager_v2_capability_denied"), "{error}");
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn storage_control_runs_status_preview_and_real_reclaim_and_journals_each() {
    // StorageControl alone (no OperatorDelegation) is enough for the two
    // storage methods.
    let p = grant_pilot(&[ManagerCapabilityV2::StorageControl]).await;
    let before = journaled_operations(&p);
    for (name, operation) in storage_calls() {
        let queued = control(&p, &format!("storage-{name}"), operation)
            .await
            .unwrap();
        p.execute().await.unwrap();
        let receipt = p.receipt(queued.operation_id).await;
        assert_eq!(receipt.state, ManagerActionStateV2::Succeeded, "{name}");
        let Some(OperatorCallResultV1::Scalar { method, result }) =
            receipt.operator_result.map(|r| *r)
        else {
            panic!("{name}: scalar storage report");
        };
        assert_eq!(
            method,
            if name == "status" {
                "GetSandboxStorageStatus"
            } else {
                "RunSandboxBuildCacheReclaim"
            }
        );
        let report = result.get("report").unwrap_or(&result);
        assert_eq!(report["dry_run"], json!(name != "real"), "{name}");
        // The manager is the journaled actor; daemon settings supplied the
        // limits (the report echoes the configured watermarks).
        assert_eq!(actor(&p, queued.operation_id), Some(p.owner.to_string()));
        assert!(report.get("config").is_some(), "{name}: {result}");
    }
    assert_eq!(journaled_operations(&p), before + 3);
    // StorageControl does not unlock the session methods.
    let target = leaf(&p, None, SessionStatus::Completed, 25).await;
    refused_with(
        &p,
        "storage-not-archive",
        archive(&p, target).await,
        "manager_v2_capability_denied",
    )
    .await;
    // Params are closed: no caller-chosen watermark or limit.
    for (key, params) in [
        ("storage-bad-empty", json!({})),
        (
            "storage-bad-extra",
            json!({"dry_run": true, "high_watermark_pct": 1}),
        ),
    ] {
        refused_with(
            &p,
            key,
            call("RunSandboxBuildCacheReclaim", params, None),
            OPERATOR_PARAMS_INVALID,
        )
        .await;
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn storage_control_keeps_the_execute_and_pause_gates() {
    let p = grant_pilot(&[ManagerCapabilityV2::StorageControl]).await;
    let mut monitor = p.policy.clone();
    monitor.mode = ManagerOperatingModeV2::Monitor;
    monitor
        .capabilities
        .push(ManagerCapabilityV2::StorageControl);
    reconfigure(&p, 2, monitor);
    let error = control_as(
        &p,
        p.owner,
        3,
        "storage-monitor",
        call("GetSandboxStorageStatus", json!({}), None),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("manager_v2_execute_required"), "{error}");
    let mut paused = p.policy.clone();
    paused.paused = true;
    paused
        .capabilities
        .push(ManagerCapabilityV2::StorageControl);
    reconfigure(&p, 3, paused);
    let error = control_as(
        &p,
        p.owner,
        4,
        "storage-paused",
        call(
            "RunSandboxBuildCacheReclaim",
            json!({"dry_run": false}),
            None,
        ),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("manager_v2_policy_paused"), "{error}");
}

/// #1046: a pilot holding `DaemonSettings` and the given operator bounds.
async fn settings_pilot(bounds: &[(&str, u64, u64)]) -> Pilot {
    let p = pilot().await;
    let mut policy = p.policy.clone();
    policy.mode = ManagerOperatingModeV2::Execute;
    policy
        .capabilities
        .push(ManagerCapabilityV2::DaemonSettings);
    policy.daemon_setting_bounds = bounds
        .iter()
        .map(
            |(key, min, max)| rsi_common::manager_daemon_settings::ManagerDaemonSettingBoundV2 {
                key: (*key).into(),
                min: *min,
                max: *max,
            },
        )
        .collect();
    reconfigure(&p, 1, policy);
    p
}

fn propose(key: &str, value: u64, reason: &str) -> ManagerActionV2 {
    call(
        "ProposeDaemonSetting",
        json!({"key": key, "value": value, "reason": reason}),
        None,
    )
}

fn persisted_setting(p: &Pilot, key: &str) -> Option<String> {
    p.manager
        .store
        .try_lock()
        .unwrap()
        .conn
        .query_row(
            "SELECT value FROM daemon_settings WHERE key=?1",
            [key],
            |r| r.get(0),
        )
        .ok()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn daemon_settings_is_denied_without_the_capability() {
    // OperatorDelegation and StorageControl never reach ProposeDaemonSetting,
    // even when the operator bounded the key, and nothing is journaled.
    let p = pilot().await;
    let mut policy = p.policy.clone();
    policy.mode = ManagerOperatingModeV2::Execute;
    policy.capabilities.extend([
        ManagerCapabilityV2::OperatorDelegation,
        ManagerCapabilityV2::StorageControl,
    ]);
    policy.daemon_setting_bounds = vec![
        rsi_common::manager_daemon_settings::ManagerDaemonSettingBoundV2 {
            key: "sandbox_max_source_roots".into(),
            min: 1,
            max: 65_536,
        },
    ];
    reconfigure(&p, 1, policy);
    let before = journaled_operations(&p);
    refused_with(
        &p,
        "settings-denied",
        propose("sandbox_max_source_roots", 16_384, "wrapped"),
        "manager_v2_capability_denied",
    )
    .await;
    assert_eq!(journaled_operations(&p), before);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn daemon_settings_default_bounds_are_none_so_every_key_is_refused() {
    let p = settings_pilot(&[]).await;
    for (i, setting) in rsi_common::manager_daemon_settings::MANAGER_ADJUSTABLE_DAEMON_SETTINGS
        .iter()
        .enumerate()
    {
        refused_with(
            &p,
            &format!("settings-unbounded-{i}"),
            propose(setting.key, setting.hard_max, "try"),
            "manager_v2_daemon_setting_not_adjustable",
        )
        .await;
    }
}

/// #1254: the worker context cap is a curated key; a manager proposal inside
/// the operator's bounds applies and persists, and one outside them, or in
/// the gap between the percentage and token ranges, is refused.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn worker_context_cap_proposal_applies_inside_operator_bounds() {
    let p = settings_pilot(&[("worker_context_cap_tokens", 40, 300_000)]).await;
    let queued = control(
        &p,
        "worker-cap",
        propose(
            "worker_context_cap_tokens",
            50,
            "workers compact before 60%",
        ),
    )
    .await
    .unwrap();
    p.execute().await.unwrap();
    let receipt = p.receipt(queued.operation_id).await;
    assert_eq!(receipt.state, ManagerActionStateV2::Succeeded);
    assert_eq!(
        p.manager.runtime_config.to_json()["worker_context_cap_tokens"],
        50
    );
    assert_eq!(
        persisted_setting(&p, "worker_context_cap_tokens").as_deref(),
        Some("50")
    );
    for (key, value) in [
        ("worker-cap-gap", 101),
        ("worker-cap-high", 300_001),
        ("worker-cap-low", 39),
    ] {
        refused_with(
            &p,
            key,
            propose("worker_context_cap_tokens", value, "try"),
            "manager_v2_daemon_setting_out_of_bounds",
        )
        .await;
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn daemon_settings_change_inside_bounds_is_applied_persisted_and_journaled() {
    let p = settings_pilot(&[("sandbox_max_source_roots", 4096, 32_768)]).await;
    // The 2026-09-29 incident: the setting wrapped down while roots were live.
    p.manager
        .runtime_config
        .update_field("sandbox_max_source_roots", &json!(512))
        .unwrap();
    let before = journaled_operations(&p);
    let queued = control(
        &p,
        "settings-roots",
        propose(
            "sandbox_max_source_roots",
            16_384,
            "wrapped to 512 with 1490 live roots",
        ),
    )
    .await
    .unwrap();
    p.execute().await.unwrap();
    let receipt = p.receipt(queued.operation_id).await;
    assert_eq!(receipt.state, ManagerActionStateV2::Succeeded);
    let Some(OperatorCallResultV1::Scalar { method, result }) = receipt.operator_result.map(|r| *r)
    else {
        panic!("scalar proposal result");
    };
    assert_eq!(method, "ProposeDaemonSetting");
    assert_eq!(result["key"], "sandbox_max_source_roots");
    assert_eq!(result["previous"], 512);
    assert_eq!(result["value"], 16_384);
    assert_eq!(result["reason"], "wrapped to 512 with 1490 live roots");
    // Live value, durable value and journal all reflect the change.
    assert_eq!(
        p.manager.runtime_config.to_json()["sandbox_max_source_roots"],
        16_384
    );
    assert_eq!(
        persisted_setting(&p, "sandbox_max_source_roots").as_deref(),
        Some("16384")
    );
    assert_eq!(journaled_operations(&p), before + 1);
    assert_eq!(actor(&p, queued.operation_id), Some(p.owner.to_string()));
    let stored: String = p
        .manager
        .store
        .try_lock()
        .unwrap()
        .conn
        .query_row(
            "SELECT payload_json FROM harness_manager_v2_operations WHERE id=?1",
            [queued.operation_id.to_string()],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        stored.contains("wrapped to 512 with 1490 live roots"),
        "{stored}"
    );
    // The same idempotency key replays without a second effect or journal row.
    p.manager
        .runtime_config
        .update_field("sandbox_max_source_roots", &json!(8192))
        .unwrap();
    let replay = control(
        &p,
        "settings-roots",
        propose(
            "sandbox_max_source_roots",
            16_384,
            "wrapped to 512 with 1490 live roots",
        ),
    )
    .await
    .unwrap();
    assert_eq!(replay.operation_id, queued.operation_id);
    assert_eq!(
        p.manager.runtime_config.to_json()["sandbox_max_source_roots"],
        8192
    );
    assert_eq!(journaled_operations(&p), before + 1);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn daemon_settings_out_of_bounds_and_off_allowlist_are_refused_untouched() {
    let p = settings_pilot(&[
        ("sandbox_max_source_roots", 4096, 32_768),
        ("sandbox_min_free_gib", 5, 50),
    ])
    .await;
    let before = journaled_operations(&p);
    let roots = p.manager.runtime_config.to_json()["sandbox_max_source_roots"].clone();
    for (key, name, value, code) in [
        ("sandbox_max_source_roots", "low", 4095, "out_of_bounds"),
        ("sandbox_max_source_roots", "high", 32_769, "out_of_bounds"),
        ("sandbox_min_free_gib", "zero", 0, "out_of_bounds"),
        // In the registry and the daemon range, but not bounded by the operator.
        (
            "sandbox_build_cache_reclaim_high_watermark_pct",
            "unbounded",
            90,
            "not_adjustable",
        ),
        // Spend, credentials and any other daemon setting are off the allowlist.
        ("max_spend_usd", "spend", 1, "not_allowlisted"),
        ("governor_max_load", "governor", 1, "not_allowlisted"),
    ] {
        refused_with(
            &p,
            &format!("settings-refused-{name}"),
            propose(key, value, "try"),
            &format!("manager_v2_daemon_setting_{code}"),
        )
        .await;
    }
    assert_eq!(journaled_operations(&p), before);
    assert_eq!(
        p.manager.runtime_config.to_json()["sandbox_max_source_roots"],
        roots
    );
    // A missing reason is refused as malformed params.
    refused_with(
        &p,
        "settings-no-reason",
        call(
            "ProposeDaemonSetting",
            json!({"key": "sandbox_min_free_gib", "value": 10}),
            None,
        ),
        OPERATOR_PARAMS_INVALID,
    )
    .await;
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn daemon_settings_reclaim_watermarks_apply_through_the_validated_path() {
    let p = settings_pilot(&[
        ("sandbox_build_cache_reclaim_high_watermark_pct", 50, 95),
        ("sandbox_build_cache_reclaim_low_watermark_pct", 10, 95),
    ])
    .await;
    let queued = control(
        &p,
        "settings-high",
        propose(
            "sandbox_build_cache_reclaim_high_watermark_pct",
            92,
            "disk pressure",
        ),
    )
    .await
    .unwrap();
    p.execute().await.unwrap();
    let receipt = p.receipt(queued.operation_id).await;
    assert_eq!(receipt.state, ManagerActionStateV2::Succeeded);
    let snapshot = p
        .manager
        .runtime_config
        .sandbox_build_cache_reclaim_snapshot();
    assert_eq!(snapshot.high_watermark_pct, 92);
    assert_eq!(
        persisted_setting(&p, "sandbox_build_cache_reclaim_high_watermark_pct").as_deref(),
        Some("92")
    );
    // The daemon's own invariant (low below high) still refuses an in-bounds
    // value, and the live config keeps its previous value.
    let queued = control(
        &p,
        "settings-low",
        propose(
            "sandbox_build_cache_reclaim_low_watermark_pct",
            94,
            "too high",
        ),
    )
    .await
    .unwrap();
    let _ = p.execute().await;
    let receipt = p.receipt(queued.operation_id).await;
    assert_ne!(
        receipt.state,
        ManagerActionStateV2::Succeeded,
        "{receipt:?}"
    );
    assert_eq!(
        p.manager
            .runtime_config
            .sandbox_build_cache_reclaim_snapshot()
            .low_watermark_pct,
        snapshot.low_watermark_pct
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn daemon_settings_keep_the_execute_gate_and_a_tightened_bound_refuses_queued_work() {
    let p = settings_pilot(&[("sandbox_min_free_gib", 5, 50)]).await;
    let mut monitor = p.policy.clone();
    monitor.mode = ManagerOperatingModeV2::Monitor;
    monitor
        .capabilities
        .push(ManagerCapabilityV2::DaemonSettings);
    monitor.daemon_setting_bounds = vec![
        rsi_common::manager_daemon_settings::ManagerDaemonSettingBoundV2 {
            key: "sandbox_min_free_gib".into(),
            min: 5,
            max: 50,
        },
    ];
    reconfigure(&p, 2, monitor);
    let error = control_as(
        &p,
        p.owner,
        3,
        "settings-monitor",
        propose("sandbox_min_free_gib", 10, "disk"),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("manager_v2_execute_required"), "{error}");
}

// ---- #1045 slice 2: AgentRequestDeploy ----------------------------------

const DEPLOY_SHA: &str = "0123456789abcdef0123456789abcdef01234567";

struct DeployDirs {
    _dir: TempDir,
    source: std::path::PathBuf,
    install: std::path::PathBuf,
    service: crate::deploy::DeployService,
}

fn deploy_dirs(supervised: bool, probe_sha: &'static str, probe_schema: i64) -> DeployDirs {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("build");
    let install = dir.path().join("install");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::create_dir_all(&install).unwrap();
    std::fs::write(source.join("rsid"), b"new-rsid").unwrap();
    std::fs::write(install.join("rsid"), b"old-rsid").unwrap();
    let service = crate::deploy::DeployService::new(
        install.clone(),
        vec![dir.path().to_path_buf()],
        Box::new(move || supervised),
        std::sync::Arc::new(move |_: &std::path::Path| Ok((probe_sha.to_string(), probe_schema))),
    );
    DeployDirs {
        _dir: dir,
        source,
        install,
        service,
    }
}

fn deploy_request(
    dirs: &DeployDirs,
    key: &str,
) -> rsi_common::agent_deploy::AgentRequestDeployRequestV1 {
    rsi_common::agent_deploy::AgentRequestDeployRequestV1 {
        project_id: None,
        sha: DEPLOY_SHA.into(),
        binaries_dir: Some(dirs.source.to_string_lossy().into_owned()),
        build: None,
        idempotency_key: key.into(),
        max_wait_secs: None,
        peer_id: None,
        cancel: None,
        interrupt_workers: None,
    }
}

async fn request_deploy(
    p: &Pilot,
    caller: Uuid,
    dirs: &DeployDirs,
    request: rsi_common::agent_deploy::AgentRequestDeployRequestV1,
) -> crate::error::Result<rsi_common::agent_deploy::AgentRequestDeployReceiptV1> {
    p.manager
        .agent_control()
        .agent_request_deploy_with(caller, request, &dirs.service, chrono::Utc::now())
        .await
}

fn staged_leftovers(install: &std::path::Path) -> usize {
    std::fs::read_dir(install)
        .unwrap()
        .filter(|entry| {
            entry
                .as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".deploy-")
        })
        .count()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn deploy_needs_the_capability_execute_mode_and_the_manager_seat() {
    let dirs = deploy_dirs(true, DEPLOY_SHA, 999);

    // Execute mode but no Deploy grant.
    let p = grant_pilot(&[]).await;
    let error = request_deploy(&p, p.owner, &dirs, deploy_request(&dirs, "cap"))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("deploy_capability_required"), "{error}");

    // A scoped worker and an Epic lead are never the deploy caller.
    let worker = leaf(&p, Some(p.epic), SessionStatus::Running, 0).await;
    for caller in [worker, p.lead] {
        let error = request_deploy(&p, caller, &dirs, deploy_request(&dirs, "seat"))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("deploy_not_authorized"), "{error}");
    }

    // Granted, but not in Execute mode.
    let mut status_mode = p.policy.clone();
    status_mode.mode = ManagerOperatingModeV2::Status;
    status_mode.capabilities.push(ManagerCapabilityV2::Deploy);
    reconfigure(&p, 2, status_mode);
    let error = request_deploy(&p, p.owner, &dirs, deploy_request(&dirs, "mode"))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("deploy_execute_required"), "{error}");

    // Granted and paused.
    let mut paused = p.policy.clone();
    paused.mode = ManagerOperatingModeV2::Execute;
    paused.paused = true;
    paused.capabilities.push(ManagerCapabilityV2::Deploy);
    reconfigure(&p, 3, paused);
    let error = request_deploy(&p, p.owner, &dirs, deploy_request(&dirs, "paused"))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("deploy_execute_required"), "{error}");

    // Granted, Execute, not paused: accepted.
    let mut open = p.policy.clone();
    open.mode = ManagerOperatingModeV2::Execute;
    open.capabilities.push(ManagerCapabilityV2::Deploy);
    reconfigure(&p, 4, open);
    let receipt = request_deploy(&p, p.owner, &dirs, deploy_request(&dirs, "ok"))
        .await
        .unwrap();
    assert_eq!(receipt.state, rsi_common::agent_deploy::DeployState::Staged);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn deploy_request_stages_verified_binaries_and_replays_by_key() {
    let dirs = deploy_dirs(true, DEPLOY_SHA, 999);
    let p = grant_pilot(&[ManagerCapabilityV2::Deploy]).await;

    let first = request_deploy(&p, p.owner, &dirs, deploy_request(&dirs, "stage"))
        .await
        .unwrap();
    assert!(!first.replayed);
    assert_eq!(first.sha, DEPLOY_SHA);
    assert_eq!(first.binaries.len(), 1);
    assert_eq!(first.binaries[0].name, "rsid");
    assert_eq!(first.binaries[0].sha256.len(), 64);
    assert_eq!(staged_leftovers(&dirs.install), 1);
    // Nothing is installed until the quiet point.
    assert_eq!(
        std::fs::read(dirs.install.join("rsid")).unwrap(),
        b"old-rsid"
    );

    let replay = request_deploy(&p, p.owner, &dirs, deploy_request(&dirs, "stage"))
        .await
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.deploy_id, first.deploy_id);
    assert_eq!(staged_leftovers(&dirs.install), 1);

    // The same key for a different request is a conflict.
    let mut other = deploy_request(&dirs, "stage");
    other.max_wait_secs = Some(60);
    let error = request_deploy(&p, p.owner, &dirs, other)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("deploy_idempotency_key_conflict"), "{error}");

    // One live deploy at a time; the refused attempt leaves no staged copy.
    let error = request_deploy(&p, p.owner, &dirs, deploy_request(&dirs, "second"))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("deploy_already_in_progress"), "{error}");
    assert_eq!(staged_leftovers(&dirs.install), 1);
}

/// #1461: `interrupt_workers` is accepted from the Deploy manager, echoed in the
/// receipt and kept on replay; a replay cannot flip it (a different request,
/// so a key conflict); a satellite deploy has no such option.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn deploy_request_interrupt_workers_is_recorded_replays_and_is_refused_for_a_peer() {
    let dirs = deploy_dirs(true, DEPLOY_SHA, 999);
    let p = grant_pilot(&[ManagerCapabilityV2::Deploy]).await;
    let mut ask = deploy_request(&dirs, "interrupt");
    ask.interrupt_workers = Some(true);

    let first = request_deploy(&p, p.owner, &dirs, ask.clone())
        .await
        .unwrap();
    assert!(!first.replayed);
    assert!(first.interrupt_workers);
    assert!(first.interrupted_workers.is_empty());

    let replay = request_deploy(&p, p.owner, &dirs, ask).await.unwrap();
    assert!(replay.replayed);
    assert!(replay.interrupt_workers);
    assert_eq!(replay.deploy_id, first.deploy_id);

    let error = request_deploy(&p, p.owner, &dirs, deploy_request(&dirs, "interrupt"))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("deploy_idempotency_key_conflict"), "{error}");

    let mut peer = deploy_request(&dirs, "interrupt-peer");
    peer.interrupt_workers = Some(true);
    peer.peer_id = Some(rsi_common::satellite::SatelliteUuidV1(Uuid::new_v4()));
    let error = request_deploy(&p, p.owner, &dirs, peer)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("deploy_invalid_request"), "{error}");
}

/// #1320/#1311: the deploying manager cancels its own waiting deploy. The hold
/// on new launches ends at once, the staged copies go, nothing is installed,
/// no outcome wake is due (the caller holds the answer) and a new deploy can
/// be requested.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn deploy_owner_cancels_its_waiting_deploy_and_the_hold_ends_at_once() {
    use rsi_common::agent_deploy::{DEPLOY_CANCELLED, DeployState};
    let dirs = deploy_dirs(true, DEPLOY_SHA, 999);
    let p = grant_pilot(&[ManagerCapabilityV2::Deploy]).await;
    let staged = request_deploy(&p, p.owner, &dirs, deploy_request(&dirs, "cancel-me"))
        .await
        .unwrap();
    let drain = p.manager.deploy_drain();
    let live = p
        .manager
        .store()
        .lock()
        .await
        .live_agent_deploy()
        .unwrap()
        .expect("live deploy");
    drain.sync(Some(&live), true, chrono::Utc::now());
    assert!(drain.is_draining(), "the runner engaged the hold");

    let mut cancel = deploy_request(&dirs, "cancel-me");
    cancel.binaries_dir = None;
    cancel.cancel = Some(true);
    let cancelled = request_deploy(&p, p.owner, &dirs, cancel.clone())
        .await
        .unwrap();
    assert_eq!(cancelled.deploy_id, staged.deploy_id);
    assert_eq!(cancelled.state, DeployState::Failed);
    assert_eq!(cancelled.reason.as_deref(), Some(DEPLOY_CANCELLED));
    assert!(!drain.is_draining(), "the hold ended with the cancel");
    assert_eq!(staged_leftovers(&dirs.install), 0);
    assert_eq!(
        std::fs::read(dirs.install.join("rsid")).unwrap(),
        b"old-rsid"
    );
    let wake = p
        .manager
        .store()
        .lock()
        .await
        .list_scheduled_jobs()
        .unwrap()
        .into_iter()
        .find(|job| job.name == format!("deploy-{}", staged.deploy_id))
        .expect("the outcome wake row is recorded");
    assert!(!wake.enabled, "a cancel does not resume its own caller");

    // A replay returns the settled deploy; another key or sha finds nothing.
    let again = request_deploy(&p, p.owner, &dirs, cancel.clone())
        .await
        .unwrap();
    assert_eq!(again.deploy_id, staged.deploy_id);
    assert_eq!(again.reason.as_deref(), Some(DEPLOY_CANCELLED));
    let mut unknown = cancel.clone();
    unknown.idempotency_key = "never-requested".into();
    let error = request_deploy(&p, p.owner, &dirs, unknown)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("deploy_not_found"), "{error}");
    let mut other_sha = cancel;
    other_sha.sha = "f".repeat(40);
    let error = request_deploy(&p, p.owner, &dirs, other_sha)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("deploy_not_found"), "{error}");

    let next = request_deploy(&p, p.owner, &dirs, deploy_request(&dirs, "after-cancel"))
        .await
        .unwrap();
    assert_eq!(next.state, DeployState::Staged);
}

/// A cancel after the restart began is refused: the swap may be under way.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn deploy_cancel_after_the_restart_began_is_too_late() {
    let dirs = deploy_dirs(true, DEPLOY_SHA, 999);
    let p = grant_pilot(&[ManagerCapabilityV2::Deploy]).await;
    let staged = request_deploy(&p, p.owner, &dirs, deploy_request(&dirs, "late"))
        .await
        .unwrap();
    assert!(
        p.manager
            .store()
            .lock()
            .await
            .mark_agent_deploy_restarting(staged.deploy_id, chrono::Utc::now())
            .unwrap()
    );
    let mut cancel = deploy_request(&dirs, "late");
    cancel.cancel = Some(true);
    let error = request_deploy(&p, p.owner, &dirs, cancel)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("deploy_cancel_too_late"), "{error}");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn deploy_request_refusals_are_typed_and_leave_nothing_staged() {
    let p = grant_pilot(&[ManagerCapabilityV2::Deploy]).await;

    let mismatch = deploy_dirs(true, "f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0", 999);
    let downgrade = deploy_dirs(true, DEPLOY_SHA, 1);
    let unsupervised = deploy_dirs(false, DEPLOY_SHA, 999);
    let plain = deploy_dirs(true, DEPLOY_SHA, 999);
    for (dirs, key, code) in [
        (&mismatch, "sha", "deploy_sha_mismatch"),
        (&downgrade, "schema", "deploy_schema_downgrade"),
        (&unsupervised, "sup", "deploy_needs_supervisor"),
    ] {
        let error = request_deploy(&p, p.owner, dirs, deploy_request(dirs, key))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains(code), "{code}: {error}");
        assert_eq!(staged_leftovers(&dirs.install), 0, "{code}");
    }

    // Directory outside the allowed roots, a missing rsid, and `build:true`.
    let mut outside = deploy_request(&plain, "outside");
    outside.binaries_dir = Some("/tmp".into());
    let error = request_deploy(&p, p.owner, &plain, outside)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("deploy_directory_not_allowed"), "{error}");
    std::fs::remove_file(plain.source.join("rsid")).unwrap();
    let error = request_deploy(&p, p.owner, &plain, deploy_request(&plain, "missing"))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("deploy_binary_missing"), "{error}");
    let mut build = deploy_request(&plain, "build");
    build.binaries_dir = None;
    build.build = Some(true);
    let error = request_deploy(&p, p.owner, &plain, build)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("deploy_build_not_supported"), "{error}");
    assert_eq!(staged_leftovers(&plain.install), 0);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn deploy_is_advertised_only_to_a_manager_holding_the_capability() {
    use rsi_common::agent_control_schema::AgentControlVerbV1::RequestDeploy;
    let p = grant_pilot(&[]).await;
    let advertised = |caller: Uuid| {
        p.manager
            .store
            .try_lock()
            .unwrap()
            .agent_authority_projection(caller)
            .unwrap()
            .verbs
            .contains(&RequestDeploy)
    };
    assert!(!advertised(p.owner));
    let mut granted = p.policy.clone();
    granted.mode = ManagerOperatingModeV2::Execute;
    granted.capabilities.push(ManagerCapabilityV2::Deploy);
    reconfigure(&p, 2, granted);
    assert!(advertised(p.owner));
    assert!(!advertised(p.lead));
}

// ---- #1017 slice 2: hub-initiated satellite deploy over the link ---------

/// A fake satellite: answers `GetSatelliteIdentity` (with health) and
/// `RequestHubDeploy`, and records every deploy request it receives.
struct FakeSatellite {
    _temp: tempfile::TempDir,
    root: std::path::PathBuf,
    peer: Uuid,
    calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    deploys: std::sync::Arc<
        std::sync::Mutex<Vec<rsi_common::satellite_dispatch::SatelliteDeployRequestV1>>,
    >,
    refuse: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    build_sha: std::sync::Arc<std::sync::Mutex<String>>,
}

async fn fake_satellite(p: &Pilot, dispatch: bool, scoped: bool) -> FakeSatellite {
    use rsi_common::satellite::{
        SatelliteHealthV1, SatelliteLinkConfigV1, SatelliteLinkDirectionV1, SatellitePeerConfigV1,
        SatellitePutLinkRequestV1, SatellitePutPeerRequestV1, SatelliteUuidV1,
    };
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let temp = tempfile::Builder::new()
        .prefix("sat-deploy-")
        .tempdir_in("/tmp")
        .unwrap();
    let root = temp.path().join("satellites");
    std::fs::create_dir(&root).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    let socket = root.join("peer.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
    let installation = Uuid::new_v4();
    let incarnation = Uuid::new_v4();
    let calls = Arc::new(AtomicUsize::new(0));
    let deploys = Arc::new(Mutex::new(Vec::new()));
    let refuse: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let build_sha = Arc::new(Mutex::new("b".repeat(40)));
    let (c, d, r, b) = (
        Arc::clone(&calls),
        Arc::clone(&deploys),
        Arc::clone(&refuse),
        Arc::clone(&build_sha),
    );
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let (c, d, r, b) = (
                Arc::clone(&c),
                Arc::clone(&d),
                Arc::clone(&r),
                Arc::clone(&b),
            );
            tokio::spawn(async move {
                let mut stream = tokio::io::BufReader::new(stream);
                let mut line = String::new();
                loop {
                    line.clear();
                    if stream.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    c.fetch_add(1, Ordering::SeqCst);
                    let request: rsi_common::rpc::RpcRequest = serde_json::from_str(&line).unwrap();
                    let id = Some(request.id.clone().into());
                    let response = match request.method.as_str() {
                        "GetSatelliteIdentity" => {
                            let mut identity =
                                crate::satellite::identity(installation, incarnation);
                            identity.health = Some(SatelliteHealthV1 {
                                daemon_version: "1.0.0".into(),
                                binary_sha256: Some("cd".repeat(32)),
                                uptime_seconds: 42,
                                schema_version: Some(146),
                                sessions_running: 0,
                                sessions_waiting_approval: 0,
                                disk_free_bytes: None,
                                load_avg_1m_milli: None,
                                build_sha: Some(b.lock().unwrap().clone()),
                                started_at: Some("2026-09-29T12:00:00.000000000Z".into()),
                                supervisor_mode: Some("rsid-supervisor.sh".into()),
                                last_deploy: Some(rsi_common::satellite::SatelliteDeployStatusV1 {
                                    deploy_id: SatelliteUuidV1(Uuid::new_v4()),
                                    state: rsi_common::agent_deploy::DeployState::Succeeded,
                                    sha: "b".repeat(40),
                                }),
                                missing_provider_clis: Vec::new(),
                            });
                            rsi_common::rpc::RpcResponse::success(
                                id,
                                serde_json::to_value(identity).unwrap(),
                            )
                        }
                        "RequestHubDeploy" => {
                            if let Some(message) = r.lock().unwrap().clone() {
                                rsi_common::rpc::RpcResponse::error(
                                    id,
                                    rsi_common::RpcError {
                                        code: -32000,
                                        message,
                                        data: None,
                                    },
                                )
                            } else {
                                let wire: rsi_common::satellite_dispatch::SatelliteDeployRequestV1 =
                                    serde_json::from_value(request.params.clone()).unwrap();
                                let receipt =
                                    rsi_common::agent_deploy::AgentRequestDeployReceiptV1 {
                                        deploy_id: Uuid::new_v4(),
                                        state: rsi_common::agent_deploy::DeployState::Staged,
                                        sha: wire.sha.clone(),
                                        deadline_at: "2026-09-29T12:15:00.000000000Z".into(),
                                        binaries: Vec::new(),
                                        skipped: Vec::new(),
                                        replayed: false,
                                        reason: None,
                                        interrupt_workers: false,
                                        interrupted_workers: Vec::new(),
                                    };
                                d.lock().unwrap().push(wire);
                                rsi_common::rpc::RpcResponse::success(
                                    id,
                                    serde_json::to_value(receipt).unwrap(),
                                )
                            }
                        }
                        other => panic!("hub called a method outside the allowlist: {other}"),
                    };
                    let mut bytes = serde_json::to_vec(&response).unwrap();
                    bytes.push(b'\n');
                    stream.get_mut().write_all(&bytes).await.unwrap();
                }
            });
        }
    });
    let peer = Uuid::new_v4();
    {
        let store = p.manager.store().lock().await;
        let revision = store.satellite_registry_revision().unwrap();
        store
            .put_satellite_peer(
                &SatellitePutPeerRequestV1 {
                    expected_registry_revision: revision,
                    peer: SatellitePeerConfigV1 {
                        peer_id: SatelliteUuidV1(peer),
                        label: "laptop".into(),
                        expected_installation_id: Some(SatelliteUuidV1(installation)),
                        enabled: true,
                        read_enabled: true,
                        dispatch_enabled: dispatch,
                    },
                    repair_quarantine: false,
                },
                &root,
            )
            .unwrap();
        let revision = store.satellite_registry_revision().unwrap();
        store
            .put_satellite_link(
                &SatellitePutLinkRequestV1 {
                    expected_registry_revision: revision,
                    peer_id: SatelliteUuidV1(peer),
                    link: SatelliteLinkConfigV1 {
                        link_id: SatelliteUuidV1(Uuid::new_v4()),
                        direction: SatelliteLinkDirectionV1::DialHomeReverse,
                        socket_path: socket.to_string_lossy().into_owned(),
                        ssh_target: None,
                        trust_reference: "ssh-config:peer".into(),
                        enabled: true,
                        priority: 0,
                    },
                },
                &root,
            )
            .unwrap();
        if scoped {
            let revision = store.satellite_registry_revision().unwrap();
            store
                .put_satellite_peer_scope(revision, peer, &[SatelliteUuidV1(Uuid::new_v4())])
                .unwrap();
        }
    }
    FakeSatellite {
        _temp: temp,
        root,
        peer,
        calls,
        deploys,
        refuse,
        build_sha,
    }
}

fn satellite_deploy_request(
    dirs: &DeployDirs,
    peer: Uuid,
    key: &str,
) -> rsi_common::agent_deploy::AgentRequestDeployRequestV1 {
    let mut request = deploy_request(dirs, key);
    request.binaries_dir = Some("/home/laptop/.rsi/staging/bin-x".into());
    request.peer_id = Some(rsi_common::satellite::SatelliteUuidV1(peer));
    request
}

async fn request_satellite_deploy(
    p: &Pilot,
    caller: Uuid,
    dirs: &DeployDirs,
    sat: &FakeSatellite,
    request: rsi_common::agent_deploy::AgentRequestDeployRequestV1,
) -> crate::error::Result<rsi_common::agent_deploy::AgentRequestDeployReceiptV1> {
    p.manager
        .agent_control()
        .agent_request_deploy_via(
            caller,
            request,
            &dirs.service,
            chrono::Utc::now(),
            &sat.root,
        )
        .await
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn satellite_deploy_reaches_the_paired_satellite_for_the_deploy_manager_only() {
    // The local daemon is not supervised: a local deploy would refuse, so a
    // receipt proves the request went over the link and not through the local
    // flow.
    let dirs = deploy_dirs(false, DEPLOY_SHA, 999);
    let p = grant_pilot(&[ManagerCapabilityV2::Deploy]).await;
    let sat = fake_satellite(&p, true, true).await;

    let receipt = request_satellite_deploy(
        &p,
        p.owner,
        &dirs,
        &sat,
        satellite_deploy_request(&dirs, sat.peer, "sat-1"),
    )
    .await
    .unwrap();
    assert_eq!(receipt.state, rsi_common::agent_deploy::DeployState::Staged);
    assert_eq!(receipt.sha, DEPLOY_SHA);
    {
        let seen = sat.deploys.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].sha, DEPLOY_SHA);
        assert_eq!(seen[0].binaries_dir, "/home/laptop/.rsi/staging/bin-x");
        assert_eq!(seen[0].idempotency_key, "sat-1");
        assert_eq!(seen[0].sender_session_id, p.owner);
    }
    // The hub kept no deploy row: the satellite's own row is the record.
    assert!(
        p.manager
            .store()
            .lock()
            .await
            .latest_agent_deploy()
            .unwrap()
            .is_none()
    );

    // A scoped worker and an Epic lead are refused before any link call.
    let before = sat.calls.load(std::sync::atomic::Ordering::SeqCst);
    let worker = leaf(&p, Some(p.epic), SessionStatus::Running, 0).await;
    for caller in [worker, p.lead] {
        let error = request_satellite_deploy(
            &p,
            caller,
            &dirs,
            &sat,
            satellite_deploy_request(&dirs, sat.peer, "no"),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("deploy_not_authorized"), "{error}");
    }
    assert_eq!(sat.calls.load(std::sync::atomic::Ordering::SeqCst), before);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn satellite_deploy_needs_the_deploy_grant_and_an_operator_scoped_peer() {
    let dirs = deploy_dirs(false, DEPLOY_SHA, 999);

    // A manager without the Deploy grant is refused, and nothing is sent.
    let no_grant = grant_pilot(&[]).await;
    let sat = fake_satellite(&no_grant, true, true).await;
    let error = request_satellite_deploy(
        &no_grant,
        no_grant.owner,
        &dirs,
        &sat,
        satellite_deploy_request(&dirs, sat.peer, "cap"),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("deploy_capability_required"), "{error}");
    assert!(sat.deploys.lock().unwrap().is_empty());

    // Dispatch off, no declared scope and an unknown peer are one refusal.
    let p = grant_pilot(&[ManagerCapabilityV2::Deploy]).await;
    let dispatch_off = fake_satellite(&p, false, true).await;
    let unscoped = fake_satellite(&p, true, false).await;
    let mut refusals = Vec::new();
    for (sat, peer) in [
        (&dispatch_off, dispatch_off.peer),
        (&unscoped, unscoped.peer),
        (&dispatch_off, Uuid::new_v4()),
    ] {
        let error = request_satellite_deploy(
            &p,
            p.owner,
            &dirs,
            sat,
            satellite_deploy_request(&dirs, peer, "scope"),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("target_not_authorized"), "{error}");
        assert!(sat.deploys.lock().unwrap().is_empty());
        refusals.push(error);
    }
    assert!(
        refusals.windows(2).all(|pair| pair[0] == pair[1]),
        "{refusals:?}"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn satellite_deploy_carries_only_the_satellites_stable_refusal_codes() {
    let dirs = deploy_dirs(false, DEPLOY_SHA, 999);
    let p = grant_pilot(&[ManagerCapabilityV2::Deploy]).await;
    let sat = fake_satellite(&p, true, true).await;
    for (remote, expected) in [
        (
            "Policy denied: deploy_needs_supervisor",
            "deploy_needs_supervisor",
        ),
        (
            "Policy denied: target_not_authorized",
            "target_not_authorized",
        ),
        (
            "Invalid parameter: /home/x/secret failed",
            "satellite_deploy_refused",
        ),
    ] {
        *sat.refuse.lock().unwrap() = Some(remote.into());
        let error = request_satellite_deploy(
            &p,
            p.owner,
            &dirs,
            &sat,
            satellite_deploy_request(&dirs, sat.peer, "codes"),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains(expected), "{expected}: {error}");
    }
    // A link that no longer exists is `satellite_deploy_unreachable`.
    std::fs::remove_file(sat.root.join("peer.sock")).unwrap();
    let error = request_satellite_deploy(
        &p,
        p.owner,
        &dirs,
        &sat,
        satellite_deploy_request(&dirs, sat.peer, "dark"),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("satellite_deploy_unreachable"), "{error}");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn daemon_info_lists_satellites_read_over_the_link_for_the_manager_only() {
    let p = grant_pilot(&[]).await;
    let sat = fake_satellite(&p, false, false).await;
    let service = crate::daemon_info::DaemonInfoService::new(
        std::env::temp_dir(),
        std::env::temp_dir(),
        std::path::PathBuf::from("/proc/self/exe"),
    );
    let info = p
        .manager
        .agent_control()
        .agent_get_daemon_info_via(p.owner, &service, &sat.root)
        .await
        .unwrap();
    assert_eq!(info.satellites.len(), 1);
    let row = &info.satellites[0];
    assert_eq!(row.peer_id.0, sat.peer);
    assert_eq!(row.label, "laptop");
    assert!(row.reachable);
    assert_eq!(row.build_sha.as_deref(), Some("b".repeat(40).as_str()));
    assert_eq!(row.supervisor_mode.as_deref(), Some("rsid-supervisor.sh"));
    assert_eq!(row.schema_version, Some(146));
    assert_eq!(
        row.last_deploy.as_ref().map(|deploy| deploy.state),
        Some(rsi_common::agent_deploy::DeployState::Succeeded)
    );

    // A deploy that lands changes the read; the next call sees the new build.
    *sat.build_sha.lock().unwrap() = "c".repeat(40);
    let after = p
        .manager
        .agent_control()
        .agent_get_daemon_info_via(p.owner, &service, &sat.root)
        .await
        .unwrap();
    assert_eq!(
        after.satellites[0].build_sha.as_deref(),
        Some("c".repeat(40).as_str())
    );

    // The Epic lead sees the hub only.
    let lead = p
        .manager
        .agent_control()
        .agent_get_daemon_info_via(p.lead, &service, &sat.root)
        .await
        .unwrap();
    assert!(lead.satellites.is_empty());

    // A dark satellite is `reachable:false` with a stable code, no identity.
    std::fs::remove_file(sat.root.join("peer.sock")).unwrap();
    let dark = p
        .manager
        .agent_control()
        .agent_get_daemon_info_via(p.owner, &service, &sat.root)
        .await
        .unwrap();
    assert!(!dark.satellites[0].reachable);
    assert_eq!(
        dark.satellites[0].error.as_deref(),
        Some("satellite_unreachable")
    );
    assert!(dark.satellites[0].build_sha.is_none());
}
