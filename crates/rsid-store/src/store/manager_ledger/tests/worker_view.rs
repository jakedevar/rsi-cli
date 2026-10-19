use super::*;
use crate::store::agent_jobs::NewAgentJob;
use crate::store::rolling_queue::NewQueueEntry;
use rsi_common::agent_jobs::{JobParams, JobState, JobWake};
use rsi_common::rolling_queue::RollingQueueBinding;

fn worker(f: &Fixture, status: SessionStatus, recent: bool, result: bool) -> Uuid {
    let mut s = f.store.get_session(f.lead).unwrap().unwrap();
    s.id = Uuid::new_v4();
    s.session_kind = SessionKind::Task;
    s.status = status;
    if !recent {
        s.updated_at = Utc::now() - chrono::Duration::days(30);
        s.created_at = s.updated_at;
    }
    s.sandbox_branch = result.then(|| format!("rsi/{}", s.id));
    f.store.insert_session(&s).unwrap();
    s.id
}

fn query(history: bool, limit: u16) -> AgentManagerInspectRequestV2 {
    AgentManagerInspectRequestV2 {
        section: ManagerInspectSectionV2::Workers,
        include_history: history,
        limit,
        ..Default::default()
    }
}

fn ids(rows: &[Value]) -> Vec<Uuid> {
    rows.iter()
        .filter_map(|r| r["session_id"].as_str())
        .map(|id| Uuid::parse_str(id).unwrap())
        .collect()
}

fn pages(f: &Fixture, mut q: AgentManagerInspectRequestV2) -> Vec<Value> {
    let mut rows = Vec::new();
    for _ in 0..100 {
        let page = f.store.manager_v2_inspect(f.manager, &q).unwrap();
        assert!(page.rows.len() <= usize::from(q.limit));
        rows.extend(page.rows);
        if let Some(cursor) = page.next_cursor {
            assert!(cursor.len() <= 512);
            q.cursor = Some(cursor);
        } else {
            assert!(page.complete);
            return rows;
        }
    }
    panic!("worker pages did not finish");
}

fn landing(f: &Fixture, source: Uuid, hash: char) -> Uuid {
    f.store
        .enqueue_rolling_queue_source(
            &NewQueueEntry {
                project_id: Some(f.project),
                repo_path: "/var/tmp/worker-view".into(),
                source_commit: hash.to_string().repeat(40),
                source_session_id: source,
                owner_epic_id: Some(f.epic),
                binding: RollingQueueBinding::Unbound,
                work_key: None,
                migration_version: None,
                hot_files: vec![],
                test_filters: vec![],
                fixes_tests: Vec::new(),
                idempotency_key: source.to_string(),
            },
            Utc::now(),
        )
        .unwrap()
        .0
        .id
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn actionable_workers_skip_large_history_and_page_newest_first() {
    let f = fixture();
    let mut history = vec![f.lead];
    for _ in 0..256 {
        history.push(worker(&f, SessionStatus::Archived, false, false));
    }
    for status in [
        SessionStatus::Deleted,
        SessionStatus::Completed,
        SessionStatus::Interrupted,
        SessionStatus::Failed,
    ] {
        history.push(worker(&f, status, false, false));
    }
    // Recent no-work completion belongs in history as well.
    history.push(worker(&f, SessionStatus::Completed, true, false));
    let first = worker(&f, SessionStatus::Running, true, false);
    let second = worker(&f, SessionStatus::Starting, true, false);
    let third = worker(&f, SessionStatus::WaitingApproval, true, false);
    let q = query(false, 2);
    let page = f.store.manager_v2_inspect(f.manager, &q).unwrap();
    assert_eq!(ids(&page.rows), vec![third, second]);
    let cursor = page.next_cursor.unwrap();
    let position: Value = serde_json::from_str(
        serde_json::from_str::<Value>(&cursor).unwrap()["after"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert!(position["t"].as_i64().is_some());
    let late = worker(&f, SessionStatus::Running, true, false);
    let rest = pages(
        &f,
        AgentManagerInspectRequestV2 {
            cursor: Some(cursor.clone()),
            ..q.clone()
        },
    );
    assert_eq!(ids(&rest), vec![first]);
    assert_eq!(ids(&pages(&f, q)), vec![late, third, second, first]);
    let changed_view = AgentManagerInspectRequestV2 {
        cursor: Some(cursor),
        ..query(true, 2)
    };
    assert!(
        f.store
            .manager_v2_inspect(f.manager, &changed_view)
            .unwrap_err()
            .to_string()
            .contains("manager_v2_cursor_changed")
    );
    history.extend([first, second, third, late]);
    assert_eq!(ids(&pages(&f, query(true, 32))), history);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn actionable_workers_retain_pending_work_jobs_landings_and_recent_results() {
    let f = fixture();
    let result = worker(&f, SessionStatus::Completed, true, true);
    let artifact = worker(&f, SessionStatus::Completed, true, false);
    f.store
        .conn
        .execute(
            "UPDATE sessions SET pipeline_artifact=?2 WHERE id=?1",
            params![
                artifact.to_string(),
                "thoughts/shared/notes/worker-result.md"
            ],
        )
        .unwrap();
    let question = worker(&f, SessionStatus::Completed, false, false);
    f.store
        .conn
        .execute(
            "UPDATE sessions SET pending_question_json=?2 WHERE id=?1",
            params![
                question.to_string(),
                json!({"questions":[{
                    "question":"Which work needs attention?", "header":"Work", "options":[]
                }]})
                .to_string()
            ],
        )
        .unwrap();
    // A stale result without a durable obligation is historical.
    worker(&f, SessionStatus::Completed, false, true);
    let interrupted = worker(&f, SessionStatus::Interrupted, true, false);
    let failed = worker(&f, SessionStatus::Failed, true, false);
    let pending_work = worker(&f, SessionStatus::Completed, false, false);
    work(&f, "unlanded");
    f.store
        .conn
        .execute(
            "UPDATE harness_manager_v2_work_facts SET payload_json=json_set(payload_json,
            '$.source_session_id',?1,'$.source_commit',?2)
            WHERE project_id=?3 AND kind='work' AND record_key='unlanded'",
            params![
                pending_work.to_string(),
                "b".repeat(40),
                f.project.to_string()
            ],
        )
        .unwrap();
    let pending_landing = worker(&f, SessionStatus::Completed, false, false);
    landing(&f, pending_landing, 'c');
    let mut job_workers = Vec::new();
    for state in [JobState::Queued, JobState::Running] {
        let source = worker(&f, SessionStatus::Completed, false, false);
        job_workers.push(source);
        let id = Uuid::new_v4();
        let params: JobParams = serde_json::from_value(json!({
            "kind":"test", "package":"rsi-common", "filters":["worker_test"]
        }))
        .unwrap();
        f.store
            .insert_agent_job_in_state(
                &NewAgentJob {
                    id,
                    owner_session_id: source,
                    project_id: Some(f.project),
                    name: None,
                    params,
                    cwd: "/var/tmp/worker-view".into(),
                    unit_name: format!("worker-view-{id}"),
                    log_path: format!("/tmp/{id}.log"),
                    status_path: format!("/tmp/{id}.status"),
                    idempotency_key: None,
                    wake: JobWake::None,
                },
                Utc::now(),
                state,
            )
            .unwrap();
    }
    let published = worker(&f, SessionStatus::Completed, true, true);
    let entry = landing(&f, published, 'd');
    f.store
        .conn
        .execute(
            "UPDATE rolling_queue_entries SET state='published',finished_at=?2 WHERE id=?1",
            params![entry.to_string(), now()],
        )
        .unwrap();
    let rows = pages(&f, query(false, 2));
    assert_eq!(
        ids(&rows),
        vec![
            job_workers[1],
            job_workers[0],
            pending_landing,
            pending_work,
            failed,
            interrupted,
            question,
            artifact,
            result
        ]
    );
    assert_eq!(rows[0]["pending_jobs"], 1);
    assert_eq!(rows[2]["pending_landings"], 1);
    assert_eq!(rows[3]["pending_work"], 1);
    assert!(ids(&pages(&f, query(true, 32))).contains(&published));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn actionable_worker_keyset_rechecks_scope_and_current_status() {
    let f = fixture();
    let mut outside = f.store.get_session(f.epic).unwrap().unwrap();
    outside.id = Uuid::new_v4();
    outside.lead_session_id = None;
    f.store.insert_session(&outside).unwrap();
    let first = worker(&f, SessionStatus::Running, true, false);
    let moved = worker(&f, SessionStatus::Running, true, false);
    let archived = worker(&f, SessionStatus::Running, true, false);
    let newest = worker(&f, SessionStatus::Running, true, false);
    let foreign = worker(&f, SessionStatus::Running, true, false);
    f.store
        .conn
        .execute(
            "UPDATE sessions SET parent_id=?2 WHERE id=?1",
            params![foreign.to_string(), outside.id.to_string()],
        )
        .unwrap();
    let q = query(false, 1);
    let page = f.store.manager_v2_inspect(f.manager, &q).unwrap();
    assert_eq!(ids(&page.rows), vec![newest]);
    f.store
        .conn
        .execute(
            "UPDATE sessions SET parent_id=?2 WHERE id=?1",
            params![moved.to_string(), outside.id.to_string()],
        )
        .unwrap();
    f.store
        .update_session_status(archived, SessionStatus::Archived)
        .unwrap();
    let rest = pages(
        &f,
        AgentManagerInspectRequestV2 {
            cursor: page.next_cursor,
            ..q
        },
    );
    assert_eq!(ids(&rest), vec![first]);
    assert_eq!(
        ids(&pages(
            &f,
            AgentManagerInspectRequestV2 {
                epic_id: Some(f.epic),
                ..query(false, 32)
            }
        )),
        vec![newest, first]
    );
    assert_eq!(
        ids(&pages(&f, query(true, 32))),
        vec![f.lead, first, archived, newest]
    );
    assert!(
        f.store
            .manager_v2_inspect(
                f.manager,
                &AgentManagerInspectRequestV2 {
                    epic_id: Some(outside.id),
                    ..query(true, 32)
                }
            )
            .unwrap_err()
            .to_string()
            .contains("manager_v2_epic_out_of_scope")
    );
}
