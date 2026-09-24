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
    DELEGABLE_OPERATOR_METHODS, DELEGATED_PAGE_MAX_BYTES, DELEGATED_PAGE_MAX_ROWS,
    DelegatedListSessionsParamsV1, NEVER_DELEGABLE_OPERATOR_METHODS_V1,
    OPERATOR_METHOD_NOT_DELEGABLE, OperatorCallFenceV1, OperatorCallResultV1, OperatorCallV1,
};
use rsi_common::types::WakeMode;
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

/// The string entries of one `const NAME: &[&str] = &[ ... ];` in rpc.rs.
fn rpc_const_list(name: &str) -> Vec<String> {
    let start = RPC_SOURCE
        .find(&format!("const {name}: &[&str] = &["))
        .unwrap_or_else(|| panic!("{name} declaration"));
    let end = start + RPC_SOURCE[start..].find("];").unwrap();
    let quoted = regex::Regex::new(r#""([A-Za-z]+)""#).unwrap();
    quoted
        .captures_iter(&RPC_SOURCE[start..end])
        .map(|c| c[1].to_string())
        .collect()
}

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

#[tokio::test]
async fn operator_call_leaves_the_tokened_gate_and_agent_catalog_unchanged() {
    // The delegated path adds no socket verb: every attributed-caller verb is
    // an `Agent*` wrapper (other Epics add their own), none is an operator
    // method, and READ_VERBS is pinned exactly.
    let agent_verbs = rpc_const_list("AGENT_VERBS");
    let read_verbs = rpc_const_list("READ_VERBS");
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
    // A non-resume wake bound to the session (a resume wake is already a
    // recovery-owner gate, asserted below).
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
        (wake_resume, "manager_v2_human_or_recovery_owner"),
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
