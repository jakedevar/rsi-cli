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
    let store = Store::open_test_fixture(&dir.path().join("rsi.db")).unwrap();
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
async fn archive_recent_activity_refusal_names_eligible_at_and_a_bulk_pass_skips() {
    let p = op_pilot(ManagerOperatingModeV2::Execute).await;
    let active_at = hours_ago(1);
    let recent = leaf(&p, None, SessionStatus::Completed, 25).await;
    insert_event_at(
        &p,
        recent,
        1,
        &active_at.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
    );
    let idle = leaf(&p, None, SessionStatus::Completed, 25).await;
    insert_event_at(&p, idle, 1, &hours_ago(30).to_rfc3339());

    // The single-call refusal keeps its stable code and names when the
    // retention window ends (last activity + 24 h).
    let error = control(&p, "eligible-at-refusal", archive(&p, recent).await)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("manager_v2_retention_recent_activity"),
        "{error}"
    );
    let raw = error
        .split("eligible_at=")
        .nth(1)
        .unwrap_or_else(|| panic!("no eligible_at in {error}"))
        .trim();
    let eligible = chrono::DateTime::parse_from_rfc3339(raw).expect("RFC3339 eligible_at");
    assert_eq!(
        eligible.with_timezone(&chrono::Utc),
        active_at + chrono::Duration::hours(24)
    );

    // A bulk pass lists first and skips every row that reports `eligible_at`
    // with a per-item reason, archiving only the rest, instead of failing a
    // call per not-yet-eligible session.
    let queued = control(
        &p,
        "eligible-at-list",
        call("ListSessions", json!({}), None),
    )
    .await
    .unwrap();
    p.execute().await.unwrap();
    let Some(OperatorCallResultV1::Page { rows, .. }) = p
        .receipt(queued.operation_id)
        .await
        .operator_result
        .map(|r| *r)
    else {
        panic!("page result");
    };
    let listed = rows.iter().find(|r| r.id == recent).unwrap();
    assert_eq!(
        listed.eligible_at.as_deref().map(|t| {
            chrono::DateTime::parse_from_rfc3339(t)
                .unwrap()
                .with_timezone(&chrono::Utc)
        }),
        Some(eligible.with_timezone(&chrono::Utc))
    );
    assert_eq!(
        rows.iter().find(|r| r.id == idle).unwrap().eligible_at,
        None
    );
    let mut skipped = Vec::new();
    for r in rows
        .iter()
        .filter(|r| r.status == SessionStatus::Completed && (r.id == recent || r.id == idle))
    {
        if let Some(at) = &r.eligible_at {
            skipped.push((r.id, format!("retention window ends {at}")));
            continue;
        }
        let q = control(&p, "eligible-at-bulk", archive(&p, r.id).await)
            .await
            .unwrap();
        p.execute().await.unwrap();
        assert_eq!(
            p.receipt(q.operation_id).await.state,
            ManagerActionStateV2::Succeeded
        );
    }
    assert_eq!(skipped.len(), 1);
    assert_eq!(skipped[0].0, recent);
    assert_eq!(row(&p, recent).await.status, SessionStatus::Completed);
    assert_eq!(row(&p, idle).await.status, SessionStatus::Archived);
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

/// #434: replay both a queued disk hold and the observed already-running
/// launch claim. Reclaim must finish without first settling that launch.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn storage_reclaim_finishes_while_same_manager_launch_is_disk_held() {
    for running in [false, true] {
        let p = grant_pilot(&[ManagerCapabilityV2::StorageControl]).await;
        let launch = control(
            &p,
            "disk-held-launch",
            ManagerActionV2::CreateSession {
                parent_id: p.epic,
                kind: SessionKind::Task,
                query: "worker waiting for disk".into(),
                launch: p.default_launch(),
                sandbox_source: None,
            },
        )
        .await
        .unwrap();
        // Claim before the floor closes, as in the reported running action.
        // Retain the claim without executing it to model its stalled launch.
        let held_claim = if running { Some(p.claim().await) } else { None };
        let disk_hold = crate::host_pressure::PressureHold::DiskPressure {
            free_gb: 26.6,
            min_gb: 30,
            reserve_gb: 0,
        };
        p.manager
            .host_load()
            .set_pressure_source(std::sync::Arc::new(move || Some(disk_hold)));
        if !running {
            p.manager.reconcile_manager_actions_once().await.unwrap();
        }

        let reclaim = control(
            &p,
            "disk-freeing-reclaim",
            call(
                "RunSandboxBuildCacheReclaim",
                json!({"dry_run": false}),
                None,
            ),
        )
        .await
        .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            if running {
                // This fixture retains the running launch claim itself.
                // A new reconcile pass would correctly recover it as
                // abandoned; exercise its concurrent claim/executor path
                // directly instead. The full-pool test drives the live
                // coordinator with real in-flight launch futures.
                let claim = p.claim().await;
                assert_eq!(claim.id(), reclaim.operation_id);
                p.manager.run_claimed_manager_action(claim).await.unwrap();
            } else {
                p.manager.reconcile_manager_actions_once().await.unwrap();
            }
        })
        .await
        .expect("reclaim must not wait for disk-held launch");
        let receipt = p.receipt(reclaim.operation_id).await;
        assert_eq!(receipt.state, ManagerActionStateV2::Succeeded);
        let Some(OperatorCallResultV1::Scalar { method, result }) =
            receipt.operator_result.map(|r| *r)
        else {
            panic!("completed reclaim report");
        };
        assert_eq!(method, "RunSandboxBuildCacheReclaim");
        let report = result.get("report").unwrap_or(&result);
        assert_eq!(report["dry_run"], false);
        assert_eq!(actor(&p, reclaim.operation_id), Some(p.owner.to_string()));
        assert_eq!(p.manager.host_load().pressure_now(), Some(disk_hold));
        assert_eq!(
            p.receipt(launch.operation_id).await.state,
            if running {
                ManagerActionStateV2::Running
            } else {
                ManagerActionStateV2::Queued
            }
        );
        if let Some(claim) = held_claim {
            p.manager
                .store
                .lock()
                .await
                .manager_action_runtime_gate(&claim, false)
                .unwrap();
        } else {
            let view = p
                .manager
                .agent_control()
                .agent_manager_get_action_view(
                    p.owner,
                    AgentManagerGetActionRequestV2 {
                        project_id: None,
                        operation_id: launch.operation_id,
                    },
                )
                .await
                .unwrap();
            assert_eq!(view.held.unwrap().reason, "disk_pressure");
        }
    }
}

/// Fill every ordinary executor slot with a real launch paused by the test
/// seam, then close the disk floor. Reclaim uses its reserved slot while all
/// launches remain parked, including after the ordinary batch budget is spent.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn storage_reclaim_has_executor_capacity_when_all_launch_slots_are_held() {
    let p = pilot().await;
    let mut policy = p.policy.clone();
    policy
        .capabilities
        .push(ManagerCapabilityV2::StorageControl);
    policy.max_active_sessions = 12;
    reconfigure(&p, 1, policy);
    p.manager
        .runtime_config
        .host_load_admission_threshold
        .store(0, std::sync::atomic::Ordering::Relaxed);

    let mut parents = vec![p.epic];
    for i in 1..3 {
        let parent = control(
            &p,
            &format!("parent-{i}"),
            ManagerActionV2::CreateContainer {
                parent_id: Some(p.group),
                kind: SessionKind::Epic,
                name: format!("Launch parent {i}"),
                tags: vec!["manager".into()],
            },
        )
        .await
        .unwrap();
        p.execute().await.unwrap();
        parents.push(parent.target_session_id.unwrap());
    }
    let mut launches = Vec::new();
    let mut reached = Vec::new();
    let mut releases = Vec::new();
    let mut processes = Vec::new();
    for (i, parent_id) in parents.into_iter().enumerate() {
        let receipt = control(
            &p,
            &format!("full-pool-launch-{i}"),
            ManagerActionV2::CreateSession {
                parent_id,
                kind: SessionKind::Task,
                query: "held launch".into(),
                launch: p.default_launch(),
                sandbox_source: None,
            },
        )
        .await
        .unwrap();
        let child = receipt.target_session_id.unwrap();
        processes.push(crate::session::launch::install_controller_candidate_test_process(child));
        let (parked, release) = crate::session::launch::install_controller_candidate_test_pause(
            child,
            crate::session::launch::ControllerCandidateTestPhase::ManagerActionAfterProviderStart,
        );
        launches.push(receipt.operation_id);
        reached.push(parked);
        releases.push(release);
    }
    let pass = p.manager.reconcile_manager_actions_once();
    tokio::pin!(pass);
    tokio::select! {
        result = &mut pass => panic!("launches must remain parked: {result:?}"),
        result = tokio::time::timeout(std::time::Duration::from_secs(15), futures::future::try_join_all(reached)) => {
            result.expect("all ordinary slots reached their launch pause").unwrap();
        }
    }
    let disk_hold = crate::host_pressure::PressureHold::DiskPressure {
        free_gb: 26.6,
        min_gb: 30,
        reserve_gb: 0,
    };
    p.manager
        .host_load()
        .set_pressure_source(std::sync::Arc::new(move || Some(disk_hold)));
    // More than BATCH successful reclaims ensures the reserved lane stays
    // available even after the pass's ordinary completion budget is spent.
    for i in 0..5 {
        let receipt = control(
            &p,
            &format!("full-pool-reclaim-{i}"),
            call(
                "RunSandboxBuildCacheReclaim",
                json!({"dry_run": false}),
                None,
            ),
        )
        .await
        .unwrap();
        let settled = async {
            loop {
                let receipt = p.receipt(receipt.operation_id).await;
                if receipt.state == ManagerActionStateV2::Succeeded {
                    break;
                }
                assert!(
                    matches!(
                        receipt.state,
                        ManagerActionStateV2::Queued | ManagerActionStateV2::Running
                    ),
                    "{receipt:?}"
                );
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        };
        tokio::select! {
            result = &mut pass => panic!("reclaim completed the parked launches: {result:?}"),
            result = tokio::time::timeout(std::time::Duration::from_secs(15), settled) => {
                result.expect("reclaim finishes while the executor's launch slots remain occupied");
            }
        }
        assert_eq!(p.manager.host_load().pressure_now(), Some(disk_hold));
        for launch in &launches {
            assert_eq!(
                p.receipt(*launch).await.state,
                ManagerActionStateV2::Running
            );
        }
    }
    p.manager
        .host_load()
        .set_pressure_source(std::sync::Arc::new(|| None));
    for release in releases {
        release.send(()).unwrap();
    }
    tokio::time::timeout(std::time::Duration::from_secs(15), pass)
        .await
        .expect("released launches finish")
        .unwrap();
    for launch in launches {
        assert_eq!(
            p.receipt(launch).await.state,
            ManagerActionStateV2::Succeeded
        );
    }
    for process in processes {
        assert_eq!(
            process
                .productive_start_count
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }
}

/// Only reclaim bypasses creates: status/other actions keep their fence, and
/// two reclaims never run together even through the reserved-slot entry.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn storage_reclaim_bypass_preserves_other_action_ordering() {
    let p = grant_pilot(&[ManagerCapabilityV2::StorageControl]).await;
    let launch = control(
        &p,
        "launch",
        ManagerActionV2::CreateSession {
            parent_id: p.epic,
            kind: SessionKind::Task,
            query: "slow worker".into(),
            launch: p.default_launch(),
            sandbox_source: None,
        },
    )
    .await
    .unwrap();
    let launch_claim = p.claim().await;
    assert_eq!(launch_claim.id(), launch.operation_id);
    let status = control(
        &p,
        "status",
        call("GetSandboxStorageStatus", json!({}), None),
    )
    .await
    .unwrap();
    let first = control(
        &p,
        "reclaim-1",
        call(
            "RunSandboxBuildCacheReclaim",
            json!({"dry_run": false}),
            None,
        ),
    )
    .await
    .unwrap();
    let second = control(
        &p,
        "reclaim-2",
        call(
            "RunSandboxBuildCacheReclaim",
            json!({"dry_run": false}),
            None,
        ),
    )
    .await
    .unwrap();
    let claim = p.claim().await;
    assert_eq!(claim.id(), first.operation_id);
    {
        let store = p.manager.store.lock().await;
        assert!(
            store
                .claim_manager_build_cache_reclaim(p.manager.program_run_boot_id)
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .claim_manager_action(p.manager.program_run_boot_id)
                .unwrap()
                .is_none()
        );
        store
            .finish_manager_action(&claim, ManagerActionStateV2::Succeeded, "test_reclaimed")
            .unwrap();
    }
    let claim = p.claim().await;
    assert_eq!(claim.id(), second.operation_id);
    {
        let store = p.manager.store.lock().await;
        store
            .finish_manager_action(&claim, ManagerActionStateV2::Succeeded, "test_reclaimed")
            .unwrap();
        store
            .finish_manager_action(
                &launch_claim,
                ManagerActionStateV2::Succeeded,
                "test_launched",
            )
            .unwrap();
    }
    let status_claim = p.claim().await;
    assert_eq!(status_claim.id(), status.operation_id);
    let third = control(
        &p,
        "reclaim-3",
        call(
            "RunSandboxBuildCacheReclaim",
            json!({"dry_run": false}),
            None,
        ),
    )
    .await
    .unwrap();
    let store = p.manager.store.lock().await;
    assert!(
        store
            .claim_manager_build_cache_reclaim(p.manager.program_run_boot_id)
            .unwrap()
            .is_none()
    );
    store
        .finish_manager_action(
            &status_claim,
            ManagerActionStateV2::Succeeded,
            "test_status",
        )
        .unwrap();
    assert_eq!(
        store
            .claim_manager_build_cache_reclaim(p.manager.program_run_boot_id)
            .unwrap()
            .unwrap()
            .id(),
        third.operation_id
    );
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
        .filter(|(_, setting)| setting.default_bounds.is_none())
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

/// #1977: the host-load threshold and queue batch size are curated with
/// built-in default bounds (8..=64, 1..=8): inside applies, outside is refused.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn host_load_and_batch_size_apply_inside_default_bounds() {
    let p = settings_pilot(&[]).await;
    let queued = control(
        &p,
        "host-load",
        propose("host_load_admission_threshold", 24, "operator load guard"),
    )
    .await
    .unwrap();
    p.execute().await.unwrap();
    let receipt = p.receipt(queued.operation_id).await;
    assert_eq!(receipt.state, ManagerActionStateV2::Succeeded);
    assert_eq!(
        p.manager.runtime_config.to_json()["host_load_admission_threshold"],
        24
    );
    let queued = control(
        &p,
        "build-one",
        propose("governor_build_slots", 1, "keep the desktop cool"),
    )
    .await
    .unwrap();
    p.execute().await.unwrap();
    let receipt = p.receipt(queued.operation_id).await;
    assert_eq!(receipt.state, ManagerActionStateV2::Succeeded);
    assert_eq!(
        p.manager.runtime_config.to_json()["governor_build_slots"],
        1
    );
    let queued = control(
        &p,
        "disk-floor",
        propose("governor_min_free_disk_gb", 40, "disk is tight"),
    )
    .await
    .unwrap();
    p.execute().await.unwrap();
    let receipt = p.receipt(queued.operation_id).await;
    assert_eq!(receipt.state, ManagerActionStateV2::Succeeded);
    assert_eq!(
        p.manager.runtime_config.to_json()["governor_min_free_disk_gb"],
        40
    );
    for (key, name, value) in [
        ("host_load_admission_threshold", "load-low", 7),
        ("host_load_admission_threshold", "load-high", 65),
        ("rolling_queue_batch_size", "batch-low", 0),
        ("rolling_queue_batch_size", "batch-high", 9),
        ("governor_build_slots", "build-high", 5),
        ("hub_build_cpu_percent", "hub-high", 51),
        ("hub_build_cpu_percent", "hub-low", 9),
        ("governor_lander_slots", "lander-high", 6),
        ("governor_min_free_disk_gb", "floor-low", 29),
        ("governor_min_free_disk_gb", "floor-high", 101),
        ("rolling_queue_full_qa_interval_mins", "qa-low", 29),
        ("rolling_queue_gate_timeout_mins", "gate-timeout-low", 44),
        ("rolling_queue_gate_timeout_mins", "gate-timeout-high", 181),
    ] {
        refused_with(
            &p,
            name,
            propose(key, value, "try"),
            "manager_v2_daemon_setting_out_of_bounds",
        )
        .await;
    }
}

/// #2166: a manager proposal for `hub_build_cpu_percent` runs the same
/// post-save hook as the operator path (the test seam records the call and
/// never writes the real slices).
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn hub_build_cpu_percent_proposal_rewrites_the_build_slice() {
    let p = settings_pilot(&[("hub_build_cpu_percent", 10, 90)]).await;
    let queued = control(
        &p,
        "settings-hub-cpu",
        propose("hub_build_cpu_percent", 25, "overnight cap"),
    )
    .await
    .unwrap();
    p.execute().await.unwrap();
    let receipt = p.receipt(queued.operation_id).await;
    assert_eq!(receipt.state, ManagerActionStateV2::Succeeded);
    assert!(
        crate::build_budget::APPLY_REASONS
            .lock()
            .unwrap()
            .contains(&"manager_setting")
    );
    let Some(OperatorCallResultV1::Scalar { result, .. }) = receipt.operator_result.map(|r| *r)
    else {
        panic!("scalar proposal result");
    };
    assert!(result.get("build_slice").is_some(), "{result}");
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

/// Record that a full-library QA run proved `sha` green for `project`.
async fn record_qa_green(p: &Pilot, project: Uuid, sha: &str) {
    use crate::store::rolling_queue::{BoundedQaRun, NewQueueEntry};
    use rsi_common::rolling_queue::{
        RollingQueueBinding, RollingQueueEntryState, RollingQueueOutcome,
    };
    let store = p.manager.store.lock().await;
    let now = chrono::Utc::now();
    store
        .enqueue_rolling_queue_source(
            &NewQueueEntry {
                project_id: Some(project),
                repo_path: "/tmp/qa-green-repo".into(),
                source_commit: sha.into(),
                source_session_id: Uuid::new_v4(),
                owner_epic_id: None,
                binding: RollingQueueBinding::Unbound,
                work_key: None,
                migration_version: None,
                hot_files: vec![],
                test_filters: vec![],
                fixes_tests: Vec::new(),
                idempotency_key: "qa-green".into(),
            },
            now,
        )
        .unwrap();
    let batch = store.claim_rolling_queue_batch(now, 1).unwrap().unwrap();
    store
        .settle_rolling_queue_entries(
            &[(
                batch.entries[0].id,
                RollingQueueEntryState::Published,
                RollingQueueOutcome {
                    landed_sha: Some(sha.into()),
                    detail: Some("gate_timing=selection=bounded selected=400 reached=7613".into()),
                    ..Default::default()
                },
            )],
            now,
        )
        .unwrap();
    let work = store.bounded_qa_landings().unwrap();
    store
        .finish_bounded_qa(
            &work,
            Uuid::new_v4(),
            sha,
            true,
            &BoundedQaRun::default(),
            None,
            now,
        )
        .unwrap();
}

/// #2743: a deploy of a SHA the latest full-library-green SHA does not cover
/// is accepted with a `not_qa_green` receipt warning that names the project's
/// open `qa` Issues; once the SHA is proven green the same request carries no
/// warning. The deploy is never refused for it.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn a_deploy_that_qa_has_not_proven_green_warns_in_its_receipt_2743() {
    let dirs = deploy_dirs(true, DEPLOY_SHA, 999);
    let p = grant_pilot(&[ManagerCapabilityV2::Deploy]).await;
    let project = p.project;
    let red = p
        .manager
        .store
        .lock()
        .await
        .create_issue(&rsi_common::types::NewIssue {
            project_id: project,
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

    let receipt = request_deploy(&p, p.owner, &dirs, deploy_request(&dirs, "qa-red"))
        .await
        .unwrap();
    assert_eq!(receipt.state, rsi_common::agent_deploy::DeployState::Staged);
    assert_eq!(receipt.qa_warnings.len(), 1, "{receipt:?}");
    let warning = &receipt.qa_warnings[0];
    assert_eq!(
        warning.code,
        rsi_common::rolling_queue::DEPLOY_WARNING_NOT_QA_GREEN
    );
    assert_eq!(warning.sha, DEPLOY_SHA);
    assert!(warning.qa_green.is_none());
    assert_eq!(warning.open_qa_issues[0].number, red.display_number);

    // QA proves the SHA green: the same request (a replay) carries no warning.
    record_qa_green(&p, project, DEPLOY_SHA).await;
    let replay = request_deploy(&p, p.owner, &dirs, deploy_request(&dirs, "qa-red"))
        .await
        .unwrap();
    assert!(replay.replayed);
    assert!(replay.qa_warnings.is_empty(), "{replay:?}");
}

/// #2203: exercise staging and both admission surfaces before any deploy poll.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn deploy_staging_holds_jobs_and_queue_admission_before_the_first_poll() {
    use crate::agent_jobs::{JobRuntime, JobTools, LaunchSpec};
    use rsi_common::agent_jobs::{AgentSubmitJobRequestV1, JobKind, JobState, JobWake};
    use rsi_common::rolling_queue::{RollingQueueBinding, RollingQueueEntryState};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct Recorder(AtomicUsize);
    impl JobRuntime for Recorder {
        fn launch(&self, _: &LaunchSpec) -> std::result::Result<(), String> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
        fn unit_active(&self, _: &str) -> bool {
            false
        }
    }

    for enabled in [true, false] {
        let p = grant_pilot(&[ManagerCapabilityV2::Deploy]).await;
        let dirs = deploy_dirs(true, DEPLOY_SHA, 999);
        let config = &p.manager.runtime_config;
        config
            .deploy_drain_enabled
            .store(enabled, Ordering::Relaxed);
        // The job hold survives a zero worker-start cap and setting-off.
        config.deploy_drain_hold_secs.store(0, Ordering::Relaxed);
        let mut request = deploy_request(&dirs, "stage-hold");
        request.interrupt_workers = Some(true);
        let staged = request_deploy(&p, p.owner, &dirs, request).await.unwrap();
        let drain = p.manager.deploy_drain();
        assert!(drain.holds_job_start(Some(p.owner), false));
        assert_eq!(drain.hold_cap(), Some(chrono::Duration::zero()));
        assert_eq!(drain.hold_queue_admission(true), enabled);
        let runtime = Arc::new(Recorder::default());
        for kind in [JobKind::Test, JobKind::Build] {
            let job = p
                .manager
                .agent_control()
                .agent_submit_job(
                    p.owner,
                    AgentSubmitJobRequestV1 {
                        kind,
                        params: if kind == JobKind::Test {
                            json!({"package":"rsid","filters":["stage-hold"]})
                        } else {
                            json!({"command":"check","workspace":true})
                        },
                        name: None,
                        idempotency_key: None,
                        worktree: Some(p.repo.to_string_lossy().into_owned()),
                        wake: Some(JobWake::None),
                        project_id: None,
                        sandbox_session_id: None,
                    },
                    runtime.clone(),
                    JobTools {
                        cargo_slot: "/unused/cargo-slot".into(),
                        lander: "/unused/lander".into(),
                    },
                )
                .await
                .unwrap();
            assert_eq!(job.job.state, JobState::Queued);
            assert_eq!(job.job.held.as_deref(), Some("held_for_deploy"));
            assert_eq!(job.job.started_at, None);
            assert_eq!(job.release_by.as_deref(), Some(staged.deadline_at.as_str()));
        }
        assert_eq!(runtime.0.load(Ordering::Relaxed), 0);

        if enabled {
            let entry = p
                .manager
                .store
                .lock()
                .await
                .enqueue_rolling_queue_source(
                    &crate::store::rolling_queue::NewQueueEntry {
                        project_id: Some(p.project),
                        repo_path: p.repo.to_string_lossy().into_owned(),
                        source_commit: git(&p.repo, &["rev-parse", "HEAD"]),
                        source_session_id: p.owner,
                        owner_epic_id: None,
                        binding: RollingQueueBinding::Unbound,
                        work_key: None,
                        migration_version: None,
                        hot_files: Vec::new(),
                        test_filters: Vec::new(),
                        fixes_tests: Vec::new(),
                        idempotency_key: "stage-hold-queue".into(),
                    },
                    chrono::Utc::now(),
                )
                .unwrap()
                .0;
            let admitted = crate::rolling_queue::run_drain_aware_batch(
                &p.manager.store,
                &crate::rolling_queue::LanderLauncher::new(dirs._dir.path().join("unused-lander")),
                1,
                &drain,
            )
            .await
            .unwrap();
            assert!(admitted.is_empty());
            assert_eq!(
                p.manager
                    .store
                    .lock()
                    .await
                    .get_rolling_queue_entry(entry.id)
                    .unwrap()
                    .unwrap()
                    .state,
                RollingQueueEntryState::Queued
            );
        }
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn deploy_staging_applies_the_live_worker_hold_settings() {
    use std::sync::atomic::Ordering;
    for enabled in [true, false] {
        let p = grant_pilot(&[ManagerCapabilityV2::Deploy]).await;
        let dirs = deploy_dirs(true, DEPLOY_SHA, 999);
        p.manager
            .runtime_config
            .deploy_drain_enabled
            .store(enabled, Ordering::Relaxed);
        p.manager
            .runtime_config
            .deploy_drain_hold_secs
            .store(90, Ordering::Relaxed);
        let staged = request_deploy(&p, p.owner, &dirs, deploy_request(&dirs, "stage-settings"))
            .await
            .unwrap();
        let drain = p.manager.deploy_drain();
        assert_eq!(drain.holds_job_start(Some(p.lead), true), enabled);
        assert_eq!(drain.hold_queue_admission(true), enabled);
        let created = p
            .manager
            .store
            .lock()
            .await
            .agent_deploy_created_at(staged.deploy_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            drain.release_by(),
            enabled.then_some((staged.deploy_id, created + chrono::Duration::seconds(90)))
        );
    }
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
/// so a key conflict).
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn deploy_request_interrupt_workers_is_recorded_and_replays() {
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
}

/// #2587: a satellite deploy forwards `interrupt_workers` (the laptop's one
/// long worker turn no longer times the deploy out); a satellite build that
/// does not advertise the capability is refused with a stable code and is sent
/// nothing, and a request that does not ask keeps the old wire shape.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn satellite_deploy_forwards_interrupt_workers_and_refuses_an_older_satellite() {
    let dirs = deploy_dirs(false, DEPLOY_SHA, 999);
    let p = grant_pilot(&[ManagerCapabilityV2::Deploy]).await;
    let sat = fake_satellite(&p, true, true).await;

    let mut ask = satellite_deploy_request(&dirs, sat.peer, "sat-interrupt");
    ask.interrupt_workers = Some(true);
    ask.max_wait_secs = Some(3000);
    request_satellite_deploy(&p, p.owner, &dirs, &sat, ask)
        .await
        .unwrap();
    request_satellite_deploy(
        &p,
        p.owner,
        &dirs,
        &sat,
        satellite_deploy_request(&dirs, sat.peer, "sat-plain"),
    )
    .await
    .unwrap();
    {
        let seen = sat.deploys.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].interrupt_workers, Some(true));
        assert_eq!(seen[0].max_wait_secs, Some(3000));
        assert!(seen[0].as_local_request().interrupts_workers());
        assert_eq!(seen[1].interrupt_workers, Some(true));
    }

    sat.legacy_no_interrupt
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let mut ask = satellite_deploy_request(&dirs, sat.peer, "sat-legacy");
    ask.interrupt_workers = Some(true);
    let error = request_satellite_deploy(&p, p.owner, &dirs, &sat, ask)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("satellite_deploy_interrupt_unsupported"),
        "{error}"
    );
    assert_eq!(sat.deploys.lock().unwrap().len(), 2, "nothing was sent");
    // Omission now also requires the interrupt capability.
    let error = request_satellite_deploy(
        &p,
        p.owner,
        &dirs,
        &sat,
        satellite_deploy_request(&dirs, sat.peer, "sat-legacy-plain"),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("satellite_deploy_interrupt_unsupported"),
        "{error}"
    );
    assert_eq!(sat.deploys.lock().unwrap().len(), 2);
    // An explicit `interrupt_workers:false` asks for nothing: it is sent with
    // no field, so the older satellite is not refused (#2605).
    let mut ask = satellite_deploy_request(&dirs, sat.peer, "sat-legacy-false");
    ask.interrupt_workers = Some(false);
    request_satellite_deploy(&p, p.owner, &dirs, &sat, ask)
        .await
        .unwrap();
    {
        let seen = sat.deploys.lock().unwrap();
        assert_eq!(seen.len(), 3);
        assert_eq!(seen[2].interrupt_workers, None);
        let body = serde_json::to_string(&seen[2]).unwrap();
        assert!(!body.contains("interrupt_workers"), "{body}");
    }
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

/// #2553: a replayed cancel of an older, already cancelled deploy A answers with
/// A's settled row and releases A's hold; it must not drop the retry hold that
/// the newer staged `interrupt_workers` deploy B owns. B's own cancel does.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn a_replayed_cancel_of_an_older_deploy_leaves_the_newer_deploys_retry_hold() {
    use rsi_common::agent_deploy::{DEPLOY_CANCELLED, DeployState};
    let dirs = deploy_dirs(true, DEPLOY_SHA, 999);
    let p = grant_pilot(&[ManagerCapabilityV2::Deploy]).await;
    let drain = p.manager.deploy_drain();
    let cancel_of = |key: &str| {
        let mut cancel = deploy_request(&dirs, key);
        cancel.binaries_dir = None;
        cancel.cancel = Some(true);
        cancel
    };
    let stage = |key: &str| {
        let mut request = deploy_request(&dirs, key);
        request.interrupt_workers = Some(true);
        request
    };

    let older = request_deploy(&p, p.owner, &dirs, stage("older"))
        .await
        .unwrap();
    assert_eq!(drain.retry_hold_owner(), Some(older.deploy_id));
    let cancelled = request_deploy(&p, p.owner, &dirs, cancel_of("older"))
        .await
        .unwrap();
    assert_eq!(cancelled.reason.as_deref(), Some(DEPLOY_CANCELLED));
    assert_eq!(
        drain.retry_hold_owner(),
        None,
        "A's cancel released A's hold"
    );

    let newer = request_deploy(&p, p.owner, &dirs, stage("newer"))
        .await
        .unwrap();
    assert_eq!(newer.state, DeployState::Staged);
    assert_eq!(drain.retry_hold_owner(), Some(newer.deploy_id));
    assert!(drain.holds_job_start(Some(p.owner), false));

    // The replay returns A's terminal row and calls `release(A)`.
    let replay = request_deploy(&p, p.owner, &dirs, cancel_of("older"))
        .await
        .unwrap();
    assert_eq!(replay.deploy_id, older.deploy_id);
    assert_eq!(replay.reason.as_deref(), Some(DEPLOY_CANCELLED));
    assert_eq!(
        drain.retry_hold_owner(),
        Some(newer.deploy_id),
        "B's retry hold survives A's replayed cancel"
    );
    assert!(drain.holds_job_start(Some(p.owner), false));

    request_deploy(&p, p.owner, &dirs, cancel_of("newer"))
        .await
        .unwrap();
    assert_eq!(drain.retry_hold_owner(), None);
    assert!(!drain.holds_job_start(Some(p.owner), false));
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

    // Verify that deploy_sha_mismatch error includes both requested and embedded SHAs
    let mismatch_error = request_deploy(
        &p,
        p.owner,
        &mismatch,
        deploy_request(&mismatch, "sha-details"),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(
        mismatch_error.contains("deploy_sha_mismatch"),
        "{mismatch_error}"
    );
    assert!(
        mismatch_error.contains("f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0"),
        "requested SHA not in error: {mismatch_error}"
    );
    assert!(
        mismatch_error.contains(DEPLOY_SHA),
        "embedded SHA not in error: {mismatch_error}"
    );

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
    policy_store: std::sync::Arc<rsid_core::store_lock::Mutex<Store>>,
    policy_received: std::sync::Arc<tokio::sync::Notify>,
    _temp: tempfile::TempDir,
    root: std::path::PathBuf,
    peer: Uuid,
    calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    deploys: std::sync::Arc<
        std::sync::Mutex<Vec<rsi_common::satellite_dispatch::SatelliteDeployRequestV1>>,
    >,
    refuse: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    build_sha: std::sync::Arc<std::sync::Mutex<String>>,
    /// #2750: the reseat requests the satellite admitted.
    reseats: std::sync::Arc<
        std::sync::Mutex<Vec<rsi_common::manager_reseat::SatelliteManagerReseatRequestV1>>,
    >,
    /// #3291: how long the satellite takes to answer an admitted reseat, and
    /// whether it closes the connection instead of answering.
    reseat_delay_ms: std::sync::Arc<std::sync::atomic::AtomicU64>,
    reseat_drop_reply: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// An older satellite build: its identity does not advertise `deploy_interrupt`.
    legacy_no_interrupt: std::sync::Arc<std::sync::atomic::AtomicBool>,
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
    let temp = crate::test_support::socket_tempdir("sat-deploy-");
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
    let legacy_no_interrupt = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // A real satellite policy store, served over the deploy fixture's socket.
    let policy_store = Store::open_in_memory().unwrap();
    {
        let hub = p.manager.store().lock().await;
        policy_store
            .insert_project(&hub.get_project(p.project).unwrap().unwrap())
            .unwrap();
        policy_store
            .insert_session(&hub.get_session(p.owner).unwrap().unwrap())
            .unwrap();
    }
    policy_store
        .configure_harness_manager(
            &rsi_common::harness_manager::ConfigureHarnessManagerRequestV1 {
                project_id: p.project,
                session_id: p.owner,
                epic_ids: None,
                group_ids: Vec::new(),
                expected_row_version: 0,
            },
        )
        .unwrap();
    policy_store
        .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
            project_id: p.project,
            expected_scope_version: 1,
            expected_policy_version: 0,
            idempotency_key: "satellite-operator-grant".into(),
            policy: ManagerPolicyV2::default(),
        })
        .unwrap();
    let hub_id = p
        .manager
        .store()
        .lock()
        .await
        .satellite_installation_id()
        .unwrap();
    policy_store
        .put_satellite_inbound_policy(&rsi_common::satellite_dispatch::SatelliteInboundPolicyV1 {
            allowed_hub_installations: vec![SatelliteUuidV1(hub_id)],
            scope_roots: vec![SatelliteUuidV1(p.owner)],
            ..Default::default()
        })
        .unwrap();
    let policy_store = Arc::new(rsid_core::store_lock::Mutex::new(policy_store));
    let remote_policy_store = Arc::clone(&policy_store);
    let policy_received = Arc::new(tokio::sync::Notify::new());
    let remote_policy_received = Arc::clone(&policy_received);
    let reseats = Arc::new(Mutex::new(Vec::new()));
    let remote_reseats = Arc::clone(&reseats);
    let reseat_delay_ms = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let reseat_drop_reply = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let remote_reseat_delay_ms = Arc::clone(&reseat_delay_ms);
    let remote_reseat_drop_reply = Arc::clone(&reseat_drop_reply);
    let (c, d, r, b, legacy) = (
        Arc::clone(&calls),
        Arc::clone(&deploys),
        Arc::clone(&refuse),
        Arc::clone(&build_sha),
        Arc::clone(&legacy_no_interrupt),
    );
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let (c, d, r, b, legacy) = (
                Arc::clone(&c),
                Arc::clone(&d),
                Arc::clone(&r),
                Arc::clone(&b),
                Arc::clone(&legacy),
            );
            let policy_store = Arc::clone(&remote_policy_store);
            let policy_received = Arc::clone(&remote_policy_received);
            let reseats = Arc::clone(&remote_reseats);
            let reseat_delay_ms = Arc::clone(&remote_reseat_delay_ms);
            let reseat_drop_reply = Arc::clone(&remote_reseat_drop_reply);
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
                            identity.capabilities.deploy_interrupt = !legacy.load(Ordering::SeqCst);
                            // Keep the fake identity independent of policy
                            // writes: the pending-link test holds this store
                            // lock until a policy request reaches the socket.
                            identity.health = Some(SatelliteHealthV1 {
                                platform: None,
                                daemon_version: "1.0.0".into(),
                                binary_sha256: Some("cd".repeat(32)),
                                uptime_seconds: 42,
                                schema_version: Some(146),
                                sessions_running: 0,
                                workers_parked: None,
                                open_jobs: None,
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
                                    blockers: Vec::new(),
                                    waiting_since: None,
                                    finished_at: None,
                                }),
                                missing_provider_clis: Vec::new(),
                                manager_policies: Some(Vec::new()),
                                manager_launches: Vec::new(),
                                governor_disk: None,
                                qa_admission: None,
                                provider_logins: Vec::new(),
                                provider_faults: Vec::new(),
                                store_probe_elapsed_ms: None,
                                usage: Vec::new(),
                                latest_watchdog_trip: None,
                            });
                            rsi_common::rpc::RpcResponse::success(
                                id,
                                serde_json::to_value(identity).unwrap(),
                            )
                        }
                        "RequestHubManagerPolicy" => {
                            let wire: rsi_common::satellite_manager_policy::SatelliteManagerPolicyRequestV1 = serde_json::from_value(request.params.clone()).unwrap();
                            policy_received.notify_one();
                            match policy_store.lock().await.apply_hub_manager_policy(&wire) {
                                Ok(receipt) => rsi_common::rpc::RpcResponse::success(
                                    id,
                                    serde_json::to_value(receipt).unwrap(),
                                ),
                                Err(error) => rsi_common::rpc::RpcResponse::error(
                                    id,
                                    rsi_common::RpcError {
                                        code: -32000,
                                        message: error.to_string(),
                                        data: match &error {
                                            DaemonError::StructuredRpc { data, .. } => {
                                                Some(data.clone())
                                            }
                                            _ => None,
                                        },
                                    },
                                ),
                            }
                        }
                        "RequestHubManagerReseat" => {
                            // The satellite's own admission gate; the launch itself
                            // needs a session manager, so the receipt is canned.
                            let wire: rsi_common::manager_reseat::SatelliteManagerReseatRequestV1 =
                                serde_json::from_value(request.params.clone()).unwrap();
                            match policy_store.lock().await.hub_manager_reseat_project(&wire) {
                                Ok(project_id) => {
                                    let result =
                                        rsi_common::manager_reseat::ReseatHarnessManagerResult {
                                            project_id,
                                            predecessor_session_id: wire
                                                .request
                                                .remote_session_id
                                                .0,
                                            predecessor_provider:
                                                rsi_common::types::SessionProvider::Codex,
                                            successor_session_id: Uuid::new_v4(),
                                            provider: wire.request.provider,
                                            model: wire.request.model.clone(),
                                            effort: wire.request.effort.clone(),
                                            reason: wire.request.reason.clone(),
                                            scope_version: 2,
                                            policy_version: Some(2),
                                        };
                                    let receipt =
                                        rsi_common::manager_reseat::SatelliteManagerReseatReceiptV1 {
                                            hub_installation_id: wire.hub_installation_id,
                                            sender_session_id: wire.sender_session_id,
                                            result,
                                        };
                                    reseats.lock().unwrap().push(wire);
                                    let delay = reseat_delay_ms.load(Ordering::SeqCst);
                                    if delay > 0 {
                                        tokio::time::sleep(std::time::Duration::from_millis(delay))
                                            .await;
                                    }
                                    if reseat_drop_reply.load(Ordering::SeqCst) {
                                        return;
                                    }
                                    rsi_common::rpc::RpcResponse::success(
                                        id,
                                        serde_json::to_value(receipt).unwrap(),
                                    )
                                }
                                Err(error) => rsi_common::rpc::RpcResponse::error(
                                    id,
                                    rsi_common::RpcError {
                                        code: -32000,
                                        message: error.to_string(),
                                        data: None,
                                    },
                                ),
                            }
                        }
                        "RequestHubDeploy" => {
                            let wire: rsi_common::satellite_dispatch::SatelliteDeployRequestV1 =
                                serde_json::from_value(request.params.clone()).unwrap();
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
                                let cancelled = wire.cancel == Some(true);
                                let receipt =
                                    rsi_common::agent_deploy::AgentRequestDeployReceiptV1 {
                                        deploy_id: Uuid::new_v4(),
                                        state: if cancelled {
                                            rsi_common::agent_deploy::DeployState::Failed
                                        } else {
                                            rsi_common::agent_deploy::DeployState::Staged
                                        },
                                        sha: wire.sha.clone(),
                                        deadline_at: "2026-09-29T12:15:00.000000000Z".into(),
                                        binaries: Vec::new(),
                                        skipped: Vec::new(),
                                        replayed: false,
                                        reason: cancelled.then_some(
                                            rsi_common::agent_deploy::DEPLOY_CANCELLED.into(),
                                        ),
                                        interrupt_workers: false,
                                        interrupted_workers: Vec::new(),
                                        expected_interrupted_workers: Vec::new(),
                                        expected_requeued_jobs: Vec::new(),
                                        requeued_jobs: Vec::new(),
                                        expected_surviving_jobs: Vec::new(),
                                        surviving_jobs: Vec::new(),
                                        job_grace_secs: None,
                                        cancelled_jobs: Vec::new(),
                                        blockers: Vec::new(),
                                        waiting_since: None,
                                        qa_warnings: Vec::new(),
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
                .put_satellite_peer_scope(revision, peer, &[SatelliteUuidV1(p.owner)])
                .unwrap();
        }
    }
    FakeSatellite {
        policy_store,
        policy_received,
        _temp: temp,
        root,
        peer,
        calls,
        deploys,
        refuse,
        build_sha,
        reseats,
        reseat_delay_ms,
        reseat_drop_reply,
        legacy_no_interrupt,
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
async fn satellite_deploy_refuses_non_absolute_paths_before_contacting_the_peer() {
    let dirs = deploy_dirs(false, DEPLOY_SHA, 999);
    let p = grant_pilot(&[ManagerCapabilityV2::Deploy]).await;
    let sat = fake_satellite(&p, true, true).await;
    // An unavailable socket proves the refusal precedes the round trip.
    std::fs::remove_file(sat.root.join("peer.sock")).unwrap();
    for path in ["relative/bin", "~/.rsi/staging/bin-test", ""] {
        let mut request = satellite_deploy_request(&dirs, sat.peer, "relative");
        request.binaries_dir = Some(path.into());
        let error = request_satellite_deploy(&p, p.owner, &dirs, &sat, request)
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            format!(
                "Invalid parameter: {}",
                rsi_common::agent_deploy::DEPLOY_BINARIES_DIR_NOT_ABSOLUTE
            )
        );
    }
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

/// #2772: a satellite deploy (`peer_id`) of a SHA that QA has not proven green
/// carries the same `not_qa_green` receipt warning a hub deploy does, naming
/// the open `qa` Issue; once the SHA is proven green it carries none, and a
/// cancel never warns. The deploy is never refused for it.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn a_satellite_deploy_that_qa_has_not_proven_green_warns_in_its_receipt_2772() {
    let dirs = deploy_dirs(false, DEPLOY_SHA, 999);
    let p = grant_pilot(&[ManagerCapabilityV2::Deploy]).await;
    let sat = fake_satellite(&p, true, true).await;
    let red = p
        .manager
        .store
        .lock()
        .await
        .create_issue(&rsi_common::types::NewIssue {
            project_id: p.project,
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

    let receipt = request_satellite_deploy(
        &p,
        p.owner,
        &dirs,
        &sat,
        satellite_deploy_request(&dirs, sat.peer, "sat-qa"),
    )
    .await
    .unwrap();
    assert_eq!(receipt.state, rsi_common::agent_deploy::DeployState::Staged);
    assert_eq!(receipt.qa_warnings.len(), 1, "{receipt:?}");
    let warning = &receipt.qa_warnings[0];
    assert_eq!(
        warning.code,
        rsi_common::rolling_queue::DEPLOY_WARNING_NOT_QA_GREEN
    );
    assert_eq!(warning.sha, DEPLOY_SHA);
    assert_eq!(warning.open_qa_issues[0].number, red.display_number);

    let mut cancel = satellite_deploy_request(&dirs, sat.peer, "sat-qa");
    cancel.binaries_dir = None;
    cancel.cancel = Some(true);
    let cancelled = request_satellite_deploy(&p, p.owner, &dirs, &sat, cancel)
        .await
        .unwrap();
    assert!(cancelled.qa_warnings.is_empty(), "{cancelled:?}");

    record_qa_green(&p, p.project, DEPLOY_SHA).await;
    let green = request_satellite_deploy(
        &p,
        p.owner,
        &dirs,
        &sat,
        satellite_deploy_request(&dirs, sat.peer, "sat-qa-green"),
    )
    .await
    .unwrap();
    assert!(green.qa_warnings.is_empty(), "{green:?}");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn satellite_deploy_cancel_is_forwarded_and_satellite_refusal_is_typed() {
    let dirs = deploy_dirs(false, DEPLOY_SHA, 999);
    let p = grant_pilot(&[ManagerCapabilityV2::Deploy]).await;
    let sat = fake_satellite(&p, true, true).await;
    let mut request = satellite_deploy_request(&dirs, sat.peer, "sat-cancel-1");
    request.binaries_dir = None;
    request.cancel = Some(true);

    let receipt = request_satellite_deploy(&p, p.owner, &dirs, &sat, request.clone())
        .await
        .unwrap();
    assert_eq!(receipt.reason.as_deref(), Some("deploy_cancelled"));
    let seen = sat.deploys.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].cancel, Some(true));
    assert_eq!(seen[0].idempotency_key, "sat-cancel-1");
    drop(seen);
    assert!(
        p.manager
            .store()
            .lock()
            .await
            .latest_agent_deploy()
            .unwrap()
            .is_none()
    );

    *sat.refuse.lock().unwrap() = Some("Policy denied: deploy_cancel_too_late".into());
    let error = request_satellite_deploy(&p, p.owner, &dirs, &sat, request)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("deploy_cancel_too_late"), "{error}");
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
    assert_eq!(row.manager_policies, Some(Vec::new()));
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

fn satellite_policy_request(
    p: &Pilot,
    sat: &FakeSatellite,
) -> rsi_common::satellite_manager_policy::AgentPushSatelliteManagerPolicyRequestV1 {
    use rsi_common::satellite_manager_policy::*;
    AgentPushSatelliteManagerPolicyRequestV1 {
        project_id: None,
        peer_id: rsi_common::satellite::SatelliteUuidV1(sat.peer),
        remote_session_id: rsi_common::satellite::SatelliteUuidV1(p.owner),
        expected_scope_version: 1,
        expected_policy_version: 1,
        idempotency_key: "push-1".into(),
        policy: SatelliteManagerPolicyPatchV1 {
            allowed_launches: p.policy.allowed_launches.clone(),
            max_created_sessions: 3,
            created_sessions_window_hours: None,
            max_active_sessions: 2,
            provider_limits: Vec::new(),
        },
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn satellite_policy_pending_link_leaves_the_hub_store_usable() {
    let p = grant_pilot(&[ManagerCapabilityV2::Deploy]).await;
    let sat = fake_satellite(&p, true, true).await;
    let request = satellite_policy_request(&p, &sat);
    let control = p.manager.agent_control();
    // Hold the satellite's response until an unrelated hub store read finishes.
    let satellite_guard = sat.policy_store.lock().await;
    let push = control.push_satellite_manager_policy_via(p.owner, request, &sat.root);
    let read_while_pending = async {
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            sat.policy_received.notified(),
        )
        .await
        .expect("policy request reached the link");
        let hub = tokio::time::timeout(std::time::Duration::from_secs(1), p.manager.store().lock())
            .await
            .expect("hub store is available while the satellite response is pending");
        assert_eq!(hub.get_project(p.project).unwrap().unwrap().id, p.project);
        drop(hub);
        drop(satellite_guard);
    };
    let (result, ()) = tokio::join!(push, read_while_pending);
    assert_eq!(result.unwrap().config.row_version, 2);
}

fn satellite_reseat_request(
    p: &Pilot,
    sat: &FakeSatellite,
) -> rsi_common::manager_reseat::AgentReseatSatelliteManagerRequestV1 {
    let launch = p.policy.allowed_launches[0].clone();
    rsi_common::manager_reseat::AgentReseatSatelliteManagerRequestV1 {
        project_id: None,
        peer_id: rsi_common::satellite::SatelliteUuidV1(sat.peer),
        remote_session_id: rsi_common::satellite::SatelliteUuidV1(p.owner),
        provider: launch.provider,
        model: Some(launch.model),
        effort: launch.effort,
        reason: "codex login expired (401)".into(),
        provider_unavailable: true,
    }
}

/// #2750: the hub manager's reseat reaches a paired, dispatch-enabled satellite
/// through the deploy link under Deploy authority, names the hub's declared
/// address, and never asks the satellite for a launch outside the hub's grant.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn satellite_reseat_reaches_the_paired_satellite_for_the_deploy_manager_within_its_grant() {
    use rsi_common::manager_reseat::*;
    let p = grant_pilot(&[ManagerCapabilityV2::Deploy]).await;
    let sat = fake_satellite(&p, true, true).await;
    let control = p.manager.agent_control();
    let request = satellite_reseat_request(&p, &sat);

    let receipt = control
        .reseat_satellite_manager_via(p.owner, request.clone(), &sat.root)
        .await
        .unwrap();
    assert_eq!(receipt.sender_session_id, p.owner);
    assert_eq!(receipt.result.project_id, p.project);
    assert_eq!(receipt.result.reason, "codex login expired (401)");
    {
        let seen = sat.reseats.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].request, request);
        assert_eq!(seen[0].sender_session_id, p.owner);
    }

    // A launch outside the hub's own grant is refused before any link call.
    let calls_before = sat.calls.load(std::sync::atomic::Ordering::SeqCst);
    let mut wider = request.clone();
    wider.model = Some("ungranted-model".into());
    let error = control
        .reseat_satellite_manager_via(p.owner, wider, &sat.root)
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains(SATELLITE_RESEAT_BOUNDS),
        "{error}"
    );
    assert_eq!(
        sat.calls.load(std::sync::atomic::Ordering::SeqCst),
        calls_before
    );
    // An undeclared address is the same target_not_authorized as every
    // other peer or target refusal.
    let mut elsewhere = request.clone();
    elsewhere.remote_session_id = rsi_common::satellite::SatelliteUuidV1(Uuid::new_v4());
    let error = control
        .reseat_satellite_manager_via(p.owner, elsewhere, &sat.root)
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("target_not_authorized"),
        "{error}"
    );
    // A seat without the Deploy grant cannot reach the satellite.
    let plain = grant_pilot(&[]).await;
    let plain_sat = fake_satellite(&plain, true, true).await;
    let error = plain
        .manager
        .agent_control()
        .reseat_satellite_manager_via(
            plain.owner,
            satellite_reseat_request(&plain, &plain_sat),
            &plain_sat.root,
        )
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("target_not_authorized"),
        "{error}"
    );
    assert!(plain_sat.reseats.lock().unwrap().is_empty());
}

/// Hub #2964: run one reseat against a hub whose effective grant is exactly
/// `launches` and report whether it reached the satellite (`Ok`) or was
/// refused `satellite_reseat_bounds_exceeded` with nothing sent.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
async fn reseat_against_grant(
    launches: Vec<rsi_common::harness_manager_v2::ManagerLaunchChoiceV2>,
    model: Option<&str>,
    effort: Option<&str>,
) -> bool {
    use rsi_common::manager_reseat::SATELLITE_RESEAT_BOUNDS;
    let p = grant_pilot(&[ManagerCapabilityV2::Deploy]).await;
    let mut policy = p.policy.clone();
    policy.mode = ManagerOperatingModeV2::Execute;
    policy.capabilities.push(ManagerCapabilityV2::Deploy);
    policy.allowed_launches = launches;
    reconfigure(&p, 2, policy);
    let sat = fake_satellite(&p, true, true).await;
    let mut request = satellite_reseat_request(&p, &sat);
    request.model = model.map(str::to_owned);
    request.effort = effort.map(str::to_owned);
    let result = p
        .manager
        .agent_control()
        .reseat_satellite_manager_via(p.owner, request, &sat.root)
        .await;
    let sent = sat.reseats.lock().unwrap().len();
    match result {
        Ok(_) => {
            assert_eq!(sent, 1);
            true
        }
        Err(error) => {
            assert!(
                error.to_string().contains(SATELLITE_RESEAT_BOUNDS),
                "{error}"
            );
            assert_eq!(sent, 0, "a refused reseat sends nothing");
            false
        }
    }
}

/// Hub #2964: a reseat that omits the model is bound-checked, not waved
/// through by any entry for the provider. `ManagerLaunchChoiceV2` has no
/// any-model entry (the model is a required exact id), so an omitted model is
/// refused whatever the grant holds, and a given model still needs an entry
/// that allows its exact provider, model and effort.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn satellite_reseat_bounds_check_the_model_and_effort_against_the_grant() {
    use rsi_common::harness_manager_v2::ManagerLaunchChoiceV2;
    let base = pilot().await.policy.allowed_launches[0].clone();
    let entry = |effort: Option<&str>| ManagerLaunchChoiceV2 {
        effort: effort.map(str::to_owned),
        ..base.clone()
    };
    let model = base.model.as_str();

    // Model omitted: a model-pinned entry (exact or any-effort) is refused.
    assert!(!reseat_against_grant(vec![entry(Some("high"))], None, Some("high")).await);
    assert!(!reseat_against_grant(vec![entry(None)], None, None).await);
    // Model given: the entry must allow provider, model and effort.
    assert!(reseat_against_grant(vec![entry(Some("high"))], Some(model), Some("high")).await);
    assert!(reseat_against_grant(vec![entry(None)], Some(model), Some("high")).await);
    assert!(!reseat_against_grant(vec![entry(Some("high"))], Some(model), Some("low")).await);
    assert!(!reseat_against_grant(vec![entry(Some("high"))], Some(model), None).await);
    assert!(
        !reseat_against_grant(vec![entry(Some("high"))], Some("other-model"), Some("high")).await
    );
}

/// #3291: a reseat launches a session on the satellite, so its reply can take
/// longer than the 3 s read deadline. The hub waits for it and returns the
/// successor instead of reporting the completed reseat as unreachable.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn satellite_reseat_waits_for_a_reply_slower_than_the_read_deadline() {
    let p = grant_pilot(&[ManagerCapabilityV2::Deploy]).await;
    let sat = fake_satellite(&p, true, true).await;
    sat.reseat_delay_ms
        .store(4_000, std::sync::atomic::Ordering::SeqCst);
    let receipt = p
        .manager
        .agent_control()
        .reseat_satellite_manager_via(p.owner, satellite_reseat_request(&p, &sat), &sat.root)
        .await
        .expect("a slow reply is still the satellite's receipt");
    assert_eq!(receipt.result.predecessor_session_id, p.owner);
    assert_eq!(sat.reseats.lock().unwrap().len(), 1);
}

/// #3291: when the reply is lost after the satellite admitted the request, the
/// caller is told the outcome is uncertain, never that the satellite was
/// unreachable.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn satellite_reseat_with_a_lost_reply_is_uncertain_not_unreachable() {
    let p = grant_pilot(&[ManagerCapabilityV2::Deploy]).await;
    let sat = fake_satellite(&p, true, true).await;
    sat.reseat_drop_reply
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let error = p
        .manager
        .agent_control()
        .reseat_satellite_manager_via(p.owner, satellite_reseat_request(&p, &sat), &sat.root)
        .await
        .unwrap_err();
    let text = error.to_string();
    assert!(
        text.contains("satellite_reseat_uncertain reason=reply_lost"),
        "{text}"
    );
    assert!(!text.contains("satellite_reseat_unreachable"), "{text}");
    assert_eq!(sat.reseats.lock().unwrap().len(), 1);
}

/// #2750: the satellite refuses a reseat from a hub it does not allowlist,
/// for an address outside its inbound scope, and passes only stable codes back.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn satellite_reseat_is_refused_by_the_satellite_for_an_unallowed_hub_with_a_stable_code() {
    let p = grant_pilot(&[ManagerCapabilityV2::Deploy]).await;
    let sat = fake_satellite(&p, true, true).await;
    sat.policy_store
        .lock()
        .await
        .put_satellite_inbound_policy(&rsi_common::satellite_dispatch::SatelliteInboundPolicyV1 {
            allowed_hub_installations: vec![rsi_common::satellite::SatelliteUuidV1(Uuid::new_v4())],
            scope_roots: vec![rsi_common::satellite::SatelliteUuidV1(p.owner)],
            ..Default::default()
        })
        .unwrap();
    let error = p
        .manager
        .agent_control()
        .reseat_satellite_manager_via(p.owner, satellite_reseat_request(&p, &sat), &sat.root)
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("target_not_authorized"),
        "{error}"
    );
    assert!(sat.reseats.lock().unwrap().is_empty());
}

/// #1928: the hub declares the satellite's PROJECT (not a pinned session id)
/// and so does the satellite's inbound policy. An operator re-appointment on
/// the satellite (a new manager session, not a rotation) moves the seat; the
/// same declarations keep admitting policy pushes with no scope edit on either
/// host.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn satellite_project_scope_follows_the_satellite_seat_across_reappointment() {
    use rsi_common::harness_manager::ConfigureHarnessManagerRequestV1;
    use rsi_common::harness_manager_v2::ConfigureHarnessManagerPolicyRequestV2;
    use rsi_common::satellite::SatelliteUuidV1;
    use rsi_common::satellite_dispatch::SatelliteInboundPolicyV1;
    let p = grant_pilot(&[ManagerCapabilityV2::Deploy]).await;
    let sat = fake_satellite(&p, true, false).await;
    let hub_id = {
        let hub = p.manager.store().lock().await;
        let revision = hub.satellite_registry_revision().unwrap();
        hub.put_satellite_peer_scope_entries(
            revision,
            sat.peer,
            &[],
            &[SatelliteUuidV1(p.project)],
        )
        .unwrap();
        hub.satellite_installation_id().unwrap()
    };
    sat.policy_store
        .lock()
        .await
        .put_satellite_inbound_policy(&SatelliteInboundPolicyV1 {
            allowed_hub_installations: vec![SatelliteUuidV1(hub_id)],
            scope_projects: vec![SatelliteUuidV1(p.project)],
            ..Default::default()
        })
        .unwrap();
    let control = p.manager.agent_control();
    let mut request = satellite_policy_request(&p, &sat);
    request.remote_session_id = SatelliteUuidV1(p.project);
    let first = control
        .push_satellite_manager_policy_via(p.owner, request.clone(), &sat.root)
        .await
        .unwrap();
    assert_eq!(first.config.policy.max_created_sessions, 3);

    // The operator seats a NEW manager on the satellite and re-saves its policy.
    let (scope_version, policy_version) = {
        let store = sat.policy_store.lock().await;
        let mut seat = rsid_store::test_support::make_test_session();
        seat.project_id = Some(p.project);
        seat.status = SessionStatus::Completed;
        store.insert_session(&seat).unwrap();
        let current = store.get_harness_manager(p.project).unwrap().unwrap();
        let scope = store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                project_id: p.project,
                session_id: seat.id,
                epic_ids: None,
                group_ids: Vec::new(),
                expected_row_version: current.row_version,
            })
            .unwrap();
        assert_eq!(scope.current_session_id, Some(seat.id));
        let policy_version = store
            .get_harness_manager_policy(p.project)
            .unwrap()
            .unwrap()
            .row_version;
        store
            .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
                project_id: p.project,
                expected_scope_version: scope.row_version,
                expected_policy_version: policy_version,
                idempotency_key: "operator-regrant".into(),
                policy: ManagerPolicyV2::default(),
            })
            .unwrap();
        let policy = store
            .get_harness_manager_policy(p.project)
            .unwrap()
            .unwrap();
        (scope.row_version, policy.row_version)
    };
    request.idempotency_key = "push-after-reappointment".into();
    request.expected_scope_version = scope_version;
    request.expected_policy_version = policy_version;
    request.policy.max_created_sessions = 2;
    let second = control
        .push_satellite_manager_policy_via(p.owner, request, &sat.root)
        .await
        .unwrap();
    assert_eq!(second.config.policy.max_created_sessions, 2);
}

/// #1928: hub deploy authorization needs a declared scope of either kind.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn satellite_deploy_is_authorized_by_a_project_only_scope() {
    use rsi_common::satellite::SatelliteUuidV1;
    let dirs = deploy_dirs(false, DEPLOY_SHA, 999);
    let p = grant_pilot(&[ManagerCapabilityV2::Deploy]).await;
    let sat = fake_satellite(&p, true, false).await;
    // No scope declared: refused before any link call.
    let error = request_satellite_deploy(
        &p,
        p.owner,
        &dirs,
        &sat,
        satellite_deploy_request(&dirs, sat.peer, "none"),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("target_not_authorized"), "{error}");
    {
        let hub = p.manager.store().lock().await;
        let revision = hub.satellite_registry_revision().unwrap();
        hub.put_satellite_peer_scope_entries(
            revision,
            sat.peer,
            &[],
            &[SatelliteUuidV1(p.project)],
        )
        .unwrap();
    }
    let receipt = request_satellite_deploy(
        &p,
        p.owner,
        &dirs,
        &sat,
        satellite_deploy_request(&dirs, sat.peer, "project"),
    )
    .await
    .unwrap();
    assert_eq!(receipt.state, rsi_common::agent_deploy::DeployState::Staged);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn satellite_policy_link_applies_records_replays_and_preserves_operator_save() {
    use rsi_common::satellite_manager_policy::*;
    let p = grant_pilot(&[ManagerCapabilityV2::Deploy]).await;
    let sat = fake_satellite(&p, true, true).await;
    let request = satellite_policy_request(&p, &sat);
    let control = p.manager.agent_control();
    let receipt = control
        .push_satellite_manager_policy_via(p.owner, request.clone(), &sat.root)
        .await
        .unwrap();
    assert_eq!(receipt.config.row_version, 2);
    assert_eq!(
        receipt.config.policy.allowed_launches,
        request.policy.allowed_launches
    );
    assert_eq!(receipt.config.policy.max_created_sessions, 3);
    assert_eq!(receipt.config.policy.max_active_sessions, 2);
    assert_eq!(receipt.config.policy.mode, ManagerOperatingModeV2::Status);
    assert_eq!(receipt.sender_session_id, p.owner);
    assert_eq!(
        receipt.hub_installation_id.0,
        p.manager
            .store()
            .lock()
            .await
            .satellite_installation_id()
            .unwrap()
    );
    let mut changed = request.clone();
    changed.policy.max_active_sessions = 1;
    let error = control
        .push_satellite_manager_policy_via(p.owner, changed.clone(), &sat.root)
        .await
        .unwrap_err();
    assert!(error.to_string().contains(POLICY_KEY_CONFLICT));
    changed.idempotency_key = "stale".into();
    assert!(
        control
            .push_satellite_manager_policy_via(p.owner, changed.clone(), &sat.root)
            .await
            .unwrap_err()
            .to_string()
            .contains(POLICY_STALE)
    );
    {
        let store = sat.policy_store.lock().await;
        let mut policy = receipt.config.policy.clone();
        policy.max_active_sessions = 1;
        store
            .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
                project_id: p.project,
                expected_scope_version: 1,
                expected_policy_version: 2,
                idempotency_key: "operator-later-save".into(),
                policy,
            })
            .unwrap();
        let provenance: String = store
            .conn
            .query_row(
                "SELECT payload_json FROM harness_manager_v2_events WHERE kind='satellite_policy'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let recorded: SatelliteManagerPolicyReceiptV1 = serde_json::from_str(&provenance).unwrap();
        assert_eq!(recorded.sender_session_id, receipt.sender_session_id);
        assert_eq!(recorded.hub_installation_id, receipt.hub_installation_id);
        assert!(!recorded.deduplicated);
    }
    let retry = control
        .push_satellite_manager_policy_via(p.owner, request.clone(), &sat.root)
        .await
        .unwrap();
    assert!(retry.deduplicated);
    assert_eq!(retry.config.row_version, 2);
    let current = sat
        .policy_store
        .lock()
        .await
        .get_harness_manager_policy(p.project)
        .unwrap()
        .unwrap();
    assert_eq!(current.row_version, 3);
    assert_eq!(current.policy.max_active_sessions, 1);
    changed.expected_scope_version = 2;
    assert!(
        control
            .push_satellite_manager_policy_via(p.owner, changed, &sat.root)
            .await
            .unwrap_err()
            .to_string()
            .contains(POLICY_STALE)
    );

    // A scope edit bumps its version. Replays still find the original receipt;
    // fresh writes carrying the old scope remain stale.
    let (group, epic) = {
        let store = p.manager.store().lock().await;
        (
            store.get_session(p.group).unwrap().unwrap(),
            store.get_session(p.epic).unwrap().unwrap(),
        )
    };
    {
        let store = sat.policy_store.lock().await;
        store.insert_session(&group).unwrap();
        store.insert_session(&epic).unwrap();
        let config = store
            .configure_harness_manager(
                &rsi_common::harness_manager::ConfigureHarnessManagerRequestV1 {
                    project_id: p.project,
                    session_id: p.owner,
                    epic_ids: Some(vec![p.epic]),
                    group_ids: Vec::new(),
                    expected_row_version: 1,
                },
            )
            .unwrap();
        assert_eq!(config.row_version, 2);
    }
    let rotated_retry = control
        .push_satellite_manager_policy_via(p.owner, request.clone(), &sat.root)
        .await
        .unwrap();
    assert!(rotated_retry.deduplicated);
    assert_eq!(
        serde_json::to_value(&rotated_retry.config).unwrap(),
        serde_json::to_value(&receipt.config).unwrap()
    );
    let mut stale = request;
    stale.idempotency_key = "fresh-after-scope-edit".into();
    assert!(
        control
            .push_satellite_manager_policy_via(p.owner, stale, &sat.root)
            .await
            .unwrap_err()
            .to_string()
            .contains(POLICY_STALE)
    );
    let store = sat.policy_store.lock().await;
    let current = store
        .get_harness_manager_policy(p.project)
        .unwrap()
        .unwrap();
    assert_eq!(current.row_version, 3);
    assert_eq!(current.policy.max_active_sessions, 1);
    let stored: String = store
        .conn
        .query_row(
            "SELECT outcome_json FROM harness_manager_v2_operations WHERE kind='satellite_policy'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&stored).unwrap()["deduplicated"],
        false
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn stale_satellite_policy_push_preserves_actual_versions_counts_and_seat_over_the_link() {
    use rsi_common::satellite_manager_policy::POLICY_STALE;
    let p = grant_pilot(&[ManagerCapabilityV2::Deploy]).await;
    let sat = fake_satellite(&p, true, true).await;
    let control = p.manager.agent_control();
    let request = satellite_policy_request(&p, &sat);
    // Both scope and policy are stale; the satellite still reports both actuals.
    let mut stale = request.clone();
    for (key, scope, policy, launches) in [("stale-scope", 2, 0, 0), ("stale-policy", 1, 1, 1)] {
        stale.idempotency_key = key.into();
        stale.expected_scope_version = scope;
        stale.expected_policy_version = policy;
        let error = control
            .push_satellite_manager_policy_via(p.owner, stale.clone(), &sat.root)
            .await
            .unwrap_err();
        let text = error.to_string();
        let DaemonError::StructuredRpc { data, .. } = error else {
            panic!("stale refusal must carry typed data: {text}")
        };
        assert_eq!(data["code"], POLICY_STALE);
        assert_eq!(data["actual_scope_version"], 1);
        assert_eq!(data["actual_policy_version"], launches + 1);
        assert_eq!(data["allowed_launches"], launches);
        assert_eq!(data["manager_session_id"], p.owner.to_string());
        for field in [
            "actual_scope_version=1",
            &format!("actual_policy_version={}", launches + 1),
            &format!("allowed_launches={launches}"),
            &format!("manager_session_id={}", p.owner),
        ] {
            assert!(text.contains(field), "{text}");
        }
        if launches == 0 {
            // A valid push changes only the policy version and saved list.
            control
                .push_satellite_manager_policy_via(p.owner, request.clone(), &sat.root)
                .await
                .unwrap();
        }
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn satellite_policy_any_effort_hub_grant_admits_a_pinned_effort_before_link_effect() {
    // The default fixture grants launch effort as "any effort" (#1905): a push
    // that pins one real effort narrows the grant and is within bounds.
    let p = grant_pilot(&[ManagerCapabilityV2::Deploy]).await;
    assert_eq!(p.policy.allowed_launches[0].effort, None);
    let sat = fake_satellite(&p, true, true).await;
    let mut request = satellite_policy_request(&p, &sat);
    request.policy.allowed_launches[0].effort = Some("high".into());
    let receipt = p
        .manager
        .agent_control()
        .push_satellite_manager_policy_via(p.owner, request.clone(), &sat.root)
        .await
        .unwrap();
    assert_eq!(
        receipt.config.policy.allowed_launches,
        request.policy.allowed_launches
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn satellite_policy_hub_authority_scope_and_grant_bounds_refuse_before_link_effect() {
    use rsi_common::satellite_manager_policy::*;
    // Pin the hub grant to an exact effort: an any-effort entry (the default
    // fixture) admits every effort, so an effort edit would narrow, not widen
    // (#1905). An exact entry admits only its own effort.
    let mut p = pilot().await;
    p.policy.allowed_launches[0].effort = Some("granted-effort".into());
    let mut granted = p.policy.clone();
    granted.mode = ManagerOperatingModeV2::Execute;
    granted.capabilities.push(ManagerCapabilityV2::Deploy);
    reconfigure(&p, 1, granted);
    let sat = fake_satellite(&p, true, true).await;
    let request = satellite_policy_request(&p, &sat);
    let control = p.manager.agent_control();
    for caller in [p.lead, Uuid::new_v4()] {
        assert!(
            control
                .push_satellite_manager_policy_via(caller, request.clone(), &sat.root)
                .await
                .unwrap_err()
                .to_string()
                .contains("target_not_authorized")
        );
    }
    for dimension in 0..5 {
        let mut wider = request.clone();
        match dimension {
            0 => wider.policy.allowed_launches[0].model = "ungranted-model".into(),
            1 => wider.policy.allowed_launches[0].effort = Some("ungranted-effort".into()),
            // An exact-effort grant cannot hand out an any-effort entry.
            2 => wider.policy.allowed_launches[0].effort = None,
            3 => wider.policy.max_created_sessions = 13,
            _ => wider.policy.max_active_sessions = 5,
        }
        assert!(
            control
                .push_satellite_manager_policy_via(p.owner, wider, &sat.root)
                .await
                .unwrap_err()
                .to_string()
                .contains(POLICY_BOUNDS)
        );
    }
    let mut outside = request.clone();
    outside.remote_session_id.0 = Uuid::new_v4();
    assert!(
        control
            .push_satellite_manager_policy_via(p.owner, outside, &sat.root)
            .await
            .unwrap_err()
            .to_string()
            .contains("target_not_authorized")
    );
    assert_eq!(sat.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    let mut paused = p.policy.clone();
    paused.capabilities.push(ManagerCapabilityV2::Deploy);
    paused.paused = true;
    reconfigure(&p, 2, paused);
    assert!(
        control
            .push_satellite_manager_policy_via(p.owner, request, &sat.root)
            .await
            .unwrap_err()
            .to_string()
            .contains("target_not_authorized")
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn satellite_policy_inbound_hub_allowlist_and_scope_fail_closed() {
    let p = grant_pilot(&[ManagerCapabilityV2::Deploy]).await;
    let sat = fake_satellite(&p, true, true).await;
    for remove_hub in [true, false] {
        let store = sat.policy_store.lock().await;
        let mut inbound = store.satellite_inbound_policy().unwrap();
        if remove_hub {
            inbound.allowed_hub_installations.clear();
        } else {
            inbound.allowed_hub_installations = vec![rsi_common::satellite::SatelliteUuidV1(
                p.manager
                    .store()
                    .lock()
                    .await
                    .satellite_installation_id()
                    .unwrap(),
            )];
            inbound.scope_roots.clear();
        }
        store.put_satellite_inbound_policy(&inbound).unwrap();
        drop(store);
        assert!(
            p.manager
                .agent_control()
                .push_satellite_manager_policy_via(
                    p.owner,
                    satellite_policy_request(&p, &sat),
                    &sat.root
                )
                .await
                .unwrap_err()
                .to_string()
                .contains("target_not_authorized")
        );
    }
    assert_eq!(
        sat.policy_store
            .lock()
            .await
            .get_harness_manager_policy(p.project)
            .unwrap()
            .unwrap()
            .row_version,
        1
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn satellite_policy_portfolio_grant_and_pm_parent_ceilings_are_live() {
    use rsi_common::global_manager::ConfigureGlobalManagerRequestV1;
    use rsi_common::satellite_manager_policy::*;
    let p = grant_pilot(&[ManagerCapabilityV2::Deploy]).await;
    let sat = fake_satellite(&p, true, true).await;
    let seat = Uuid::new_v4();
    let grant_version = {
        let store = p.manager.store().lock().await;
        let mut row = bare_session(seat);
        row.session_kind = SessionKind::Standard;
        row.status = SessionStatus::Running;
        store.insert_session(&row).unwrap();
        let mut policy = p.policy.clone();
        policy.group_ids.clear();
        policy.allow_create_groups = false;
        policy.capabilities.push(ManagerCapabilityV2::Deploy);
        policy.max_created_sessions = 4;
        policy.max_active_sessions = 3;
        policy.provider_limits = vec![rsi_common::harness_manager_v2::ManagerProviderLimitV2 {
            provider: SessionProvider::Claude,
            max_active: 2,
        }];
        store
            .configure_global_manager_confirmed(
                &ConfigureGlobalManagerRequestV1 {
                    session_id: seat,
                    project_ids: vec![p.project],
                    allowed_launches: p.policy.allowed_launches.clone(),
                    project_policy: policy,
                    expected_grant_version: 0,
                    idempotency_key: "satellite-policy-global".into(),
                },
                "operator:test",
                true,
            )
            .unwrap()
            .grant_version
    };
    let mut request = satellite_policy_request(&p, &sat);
    request.project_id = Some(p.project);
    let control = p.manager.agent_control();
    assert!(
        control
            .push_satellite_manager_policy_via(seat, request.clone(), &sat.root)
            .await
            .unwrap_err()
            .to_string()
            .contains(POLICY_BOUNDS)
    );
    request.policy.provider_limits = vec![rsi_common::harness_manager_v2::ManagerProviderLimitV2 {
        provider: SessionProvider::Claude,
        max_active: 1,
    }];
    let mut outside = request.clone();
    outside.project_id = Some(Uuid::new_v4());
    assert!(
        control
            .push_satellite_manager_policy_via(seat, outside, &sat.root)
            .await
            .unwrap_err()
            .to_string()
            .contains("target_not_authorized")
    );
    let receipt = control
        .push_satellite_manager_policy_via(seat, request.clone(), &sat.root)
        .await
        .unwrap();
    assert_eq!(receipt.sender_session_id, seat);
    assert_eq!(
        receipt.config.policy.provider_limits,
        request.policy.provider_limits
    );
    // The PM's own policy allows 12 creates, but the live parent grants four.
    let mut wider = request.clone();
    wider.project_id = None;
    wider.policy.max_created_sessions = 5;
    assert!(
        control
            .push_satellite_manager_policy_via(p.owner, wider, &sat.root)
            .await
            .unwrap_err()
            .to_string()
            .contains(POLICY_BOUNDS)
    );
    // A parent launch edit applies to the PM immediately, even if the PM's
    // own saved launch choice has not changed.
    {
        let store = p.manager.store().lock().await;
        let mut launch = p.policy.allowed_launches[0].clone();
        launch.model = "new-parent-model".into();
        let mut policy = p.policy.clone();
        policy.allowed_launches = vec![launch.clone()];
        policy.group_ids.clear();
        policy.allow_create_groups = false;
        policy.capabilities.push(ManagerCapabilityV2::Deploy);
        store
            .configure_global_manager_confirmed(
                &ConfigureGlobalManagerRequestV1 {
                    session_id: seat,
                    project_ids: vec![p.project],
                    allowed_launches: vec![launch],
                    project_policy: policy,
                    expected_grant_version: grant_version,
                    idempotency_key: "satellite-policy-parent-edit".into(),
                },
                "operator:test",
                true,
            )
            .unwrap();
    }
    request.project_id = None;
    assert!(
        control
            .push_satellite_manager_policy_via(p.owner, request, &sat.root)
            .await
            .unwrap_err()
            .to_string()
            .contains(POLICY_BOUNDS)
    );
}
