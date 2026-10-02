//! RPC-level tests for the operator `ListScheduledJobs` paging and default
//! filter (Issue #954 B). Kept out of `rpc.rs`'s main tests block.

use super::*;
use chrono::{Duration, Utc};
use rsi_common::rpc::{
    LIST_SCHEDULED_JOBS_DEFAULT_LIMIT, LIST_SCHEDULED_JOBS_MAX_LIMIT, ListScheduledJobsResult,
};
use rsi_common::types::{Recurrence, ScheduleSpec, ScheduledJob, WakeMode};

struct Fixture {
    _dir: tempfile::TempDir,
    server: RpcServer,
    manager: std::sync::Arc<SessionManager>,
}

fn fixture() -> Fixture {
    use crate::bus::EventBus;
    use crate::config::{Config, RuntimeConfig};
    use crate::prompt_compile::CompileEngine;
    use crate::store::Store;

    let dir = tempfile::TempDir::new().unwrap();
    let store = Store::open(&dir.path().join("rsi.db")).expect("open store");
    let runtime_config = RuntimeConfig::from_config(&Config::from_env());
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
        .expect("manager"),
    );
    let http = reqwest::Client::new();
    let server = RpcServer::new(
        std::sync::Arc::clone(&manager),
        None,
        None,
        None,
        std::sync::Arc::clone(&runtime_config),
        CompileEngine::new(
            http.clone(),
            manager.store().clone(),
            std::sync::Arc::clone(&runtime_config),
            std::sync::Arc::clone(&bus),
        ),
        http,
        crate::model_control::ModelControlRuntime::default_normal(),
    );
    Fixture {
        _dir: dir,
        server,
        manager,
    }
}

async fn seed(fixture: &Fixture, count: i64, enabled: bool, age: Duration) -> Vec<Uuid> {
    let store = fixture.manager.store().lock().await;
    let base = Utc::now() - age;
    let mut ids = Vec::new();
    for index in 0..count {
        let at = base + Duration::milliseconds(index);
        let id = Uuid::new_v4();
        store
            .insert_scheduled_job(&ScheduledJob {
                id,
                name: format!("rpc-list-{id}"),
                message: String::new(),
                schedule: ScheduleSpec {
                    recurrence: Recurrence::Once,
                    anchor: at,
                },
                last_fired_at: None,
                next_fire_at: at,
                enabled,
                working_dir: None,
                provider: None,
                model: None,
                project_id: None,
                created_at: at,
                updated_at: at,
                wake_mode: WakeMode::Fresh,
                wake_session_id: None,
            })
            .expect("insert");
        ids.push(id);
    }
    ids
}

async fn list(fixture: &Fixture, params: serde_json::Value) -> ListScheduledJobsResult {
    let request = RpcRequest::new("ListScheduledJobs", params);
    let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
    else {
        panic!("expected response");
    };
    assert!(response.error.is_none(), "{:?}", response.error);
    serde_json::from_value(response.result.expect("result")).expect("result shape")
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn list_scheduled_jobs_without_params_returns_default_filter_first_page_and_cursor() {
    let fixture = fixture();
    let total = i64::from(LIST_SCHEDULED_JOBS_DEFAULT_LIMIT) + 10;
    seed(&fixture, total, true, Duration::hours(1)).await;

    for params in [serde_json::Value::Null, serde_json::json!({})] {
        let first = list(&fixture, params).await;
        assert_eq!(first.jobs.len(), LIST_SCHEDULED_JOBS_DEFAULT_LIMIT as usize);
        assert!(!first.include_history);
        let cursor = first.next_cursor.expect("cursor for the rest");

        let rest = list(&fixture, serde_json::json!({ "cursor": cursor })).await;
        assert_eq!(rest.jobs.len(), 10);
        assert!(rest.next_cursor.is_none());
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn list_scheduled_jobs_pages_without_duplicates_and_clamps_the_limit() {
    let fixture = fixture();
    let total = i64::from(LIST_SCHEDULED_JOBS_MAX_LIMIT) + 30;
    let mut expected = seed(&fixture, total, true, Duration::hours(1)).await;
    expected.reverse();

    let oversized = list(&fixture, serde_json::json!({ "limit": 1_000_000 })).await;
    assert_eq!(oversized.jobs.len(), LIST_SCHEDULED_JOBS_MAX_LIMIT as usize);

    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    let mut pages = 0;
    loop {
        let page = list(
            &fixture,
            serde_json::json!({ "limit": 100, "cursor": cursor }),
        )
        .await;
        pages += 1;
        seen.extend(page.jobs.iter().map(|job| job.id));
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    assert_eq!(pages, 3, "230 rows at 100 per page");
    assert_eq!(seen, expected);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn list_scheduled_jobs_hides_old_disabled_rows_unless_include_history() {
    let fixture = fixture();
    let enabled = seed(&fixture, 2, true, Duration::hours(3)).await;
    let old_disabled = seed(&fixture, 3, false, Duration::hours(3)).await;
    let recent_disabled = seed(&fixture, 1, false, Duration::minutes(2)).await;

    let default = list(&fixture, serde_json::json!({})).await;
    let shown: std::collections::HashSet<Uuid> = default.jobs.iter().map(|job| job.id).collect();
    let expected: std::collections::HashSet<Uuid> =
        enabled.iter().chain(&recent_disabled).copied().collect();
    assert_eq!(shown, expected);

    let history = list(&fixture, serde_json::json!({ "include_history": true })).await;
    assert!(history.include_history);
    let all: std::collections::HashSet<Uuid> = history.jobs.iter().map(|job| job.id).collect();
    assert_eq!(all.len(), 6);
    assert!(old_disabled.iter().all(|id| all.contains(id)));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn list_scheduled_jobs_refuses_a_forged_cursor() {
    let fixture = fixture();
    let request = RpcRequest::new(
        "ListScheduledJobs",
        serde_json::json!({ "cursor": "not-a-cursor" }),
    );
    let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
    else {
        panic!("expected response");
    };
    assert!(response.error.is_some());
    assert!(response.result.is_none());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn list_scheduled_jobs_stays_operator_only() {
    assert!(!agent_gate::AGENT_VERBS.contains(&"ListScheduledJobs"));
    assert!(!agent_gate::READ_VERBS.contains(&"ListScheduledJobs"));
}
