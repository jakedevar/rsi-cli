//! Compare #554's active-set rewrite with the exact pre-rewrite eligibility SQL.
use super::*;
use rsi_common::harness_manager::{ConfigureHarnessManagerRequestV1, HarnessManagerConfigV1};
use rsi_common::manager_operator_delegation::OperatorCallV1;

#[cfg(test)]
const ORIGINAL_CLAIM_ELIGIBLE_SQL: &str = "WITH eligible AS (
             SELECT o.id,o.project_id,o.not_before,
                    json_extract(o.payload_json,'$.origin.origin') AS origin,
                    json_extract(o.payload_json,'$.request.operation.action') AS action
               FROM harness_manager_v2_operations o WHERE o.kind=?1 AND o.state='queued' AND o.not_before<=?2
             AND (?6=0 OR (json_extract(o.payload_json,'$.request.operation.action')='operator_call' AND json_extract(o.payload_json,'$.request.operation.call.method')='RunSandboxBuildCacheReclaim'))
             AND (?3=0 OR json_extract(o.payload_json,'$.request.operation.action') NOT IN ('create_session','replace_lead','retry_lead','resume_lead'))
             AND (?5=0 OR json_extract(o.payload_json,'$.request.operation.action')<>'create_session'
                  OR json_extract(o.payload_json,'$.issue_binding.continue_from') IS NOT NULL)
             AND NOT EXISTS(SELECT 1 FROM harness_manager_v2_operations a WHERE a.project_id=o.project_id AND a.kind=?1
                AND a.id IS NOT json_extract(o.payload_json,'$.request.operation.operation_id') AND ((a.state='running' AND NOT (((json_extract(o.payload_json,'$.request.operation.action')='operator_call' AND json_extract(o.payload_json,'$.request.operation.call.method')='RunSandboxBuildCacheReclaim') OR (json_extract(o.payload_json,'$.request.operation.action')='create_session' AND json_extract(a.payload_json,'$.request.operation.parent_id') IS NOT json_extract(o.payload_json,'$.request.operation.parent_id'))) AND json_extract(a.payload_json,'$.request.operation.action')='create_session')) OR (a.state='uncertain' AND (a.target_session_id=o.target_session_id OR (json_extract(a.payload_json,'$.request.operation.epic_id')=json_extract(o.payload_json,'$.request.operation.epic_id') AND (
    json_extract(a.payload_json,'$.request.operation.epic_id') IS NULL OR
    EXISTS(SELECT 1 FROM sessions e JOIN epic_lead_generations g ON g.epic_id=e.id
      WHERE e.id=json_extract(a.payload_json,'$.request.operation.epic_id') AND
      (e.lead_session_id=a.target_session_id OR
       (e.lead_session_id IS json_extract(a.payload_json,'$.request.operation.expected.lead_session_id')
        AND g.generation=json_extract(a.payload_json,'$.request.operation.expected.lead_generation'))))))))))
             AND (json_extract(o.payload_json,'$.request.operation.action')<>'create_session' OR (
                  (SELECT COUNT(*) FROM harness_manager_v2_operations a WHERE a.project_id=o.project_id AND a.kind=?1 AND a.state='running')<?4
              AND (NOT EXISTS(SELECT 1 FROM harness_manager_v2_operations a WHERE a.project_id=o.project_id AND a.kind=?1 AND a.state='running')
                   OR NOT EXISTS(SELECT 1 FROM harness_manager_v2_operations q WHERE q.project_id=o.project_id AND q.kind=?1 AND q.state='queued'
                        AND json_extract(q.payload_json,'$.request.operation.action')<>'create_session' AND (q.not_before,q.id)<(o.not_before,o.id)))))
             AND NOT EXISTS(SELECT 1 FROM manager_root_successions r JOIN sessions p ON p.id=r.predecessor_session_id
                WHERE r.operation_id=o.id AND r.state='reserved'
                  AND (p.status IN ('Starting','Running','WaitingApproval')
                       OR EXISTS(SELECT 1 FROM daemon_settings d WHERE d.key='manager_operator_pause:'||p.id AND d.value<>'false'))
                  AND r.authority_epoch=(SELECT epoch FROM manager_authority_epochs WHERE project_id=r.project_id)
                  AND EXISTS(SELECT 1 FROM harness_manager_scopes h WHERE h.project_id=r.project_id AND h.row_version=r.scope_version))
             ), oldest AS (SELECT * FROM eligible ORDER BY not_before,id LIMIT 1)
             SELECT e.id,e.not_before FROM eligible e CROSS JOIN oldest first
              WHERE e.id=first.id OR (
                    first.origin='operating_intent'
                AND first.action IN ('resume_lead','retry_lead','replace_lead')
                AND e.project_id=first.project_id
                AND e.origin='agent' AND e.action='create_session')
              ORDER BY CASE WHEN e.id=first.id THEN 1 ELSE 0 END,e.not_before,e.id LIMIT 1";

struct Fixture {
    store: Store,
    config: HarnessManagerConfigV1,
}

impl Fixture {
    fn new() -> Self {
        let store = Store::open_in_memory().unwrap();
        let project = rsi_common::types::Project {
            id: Uuid::new_v4(),
            name: format!("claim eligibility {}", Uuid::new_v4()),
            path: None,
            description: None,
            color: rsi_common::types::Project::DEFAULT_COLOR.into(),
            context_files: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        store.insert_project(&project).unwrap();
        let mut manager = crate::test_support::make_test_session();
        manager.project_id = Some(project.id);
        store.insert_session(&manager).unwrap();
        let config = store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                project_id: project.id,
                session_id: manager.id,
                epic_ids: None,
                group_ids: vec![],
                expected_row_version: 0,
            })
            .unwrap();
        Self { store, config }
    }

    fn seed(
        &self,
        operation: ManagerActionV2,
        state: ManagerActionStateV2,
        target: Option<Uuid>,
    ) -> Uuid {
        let id = self
            .store
            .seed_manager_action_with_context_for_test(
                &self.config,
                operation,
                state,
                target,
                self.config.manager_session_id,
            )
            .unwrap();
        let operation = self.store.manager_action_operation(id).unwrap().unwrap();
        assert_eq!(operation.context.target_session_id, target);
        id
    }

    fn assert_selection(&self, expected: Option<Uuid>, holds: [bool; 3]) {
        // Fix the timestamp for both statements, including not_before in the
        // comparison. Exercise all three admission flags independently.
        let at = now();
        let args = params![
            ACTION_KIND,
            at,
            i64::from(holds[0]),
            MANAGER_CREATE_PARALLELISM,
            i64::from(holds[1]),
            i64::from(holds[2]),
        ];
        let original: Option<(String, String)> = self
            .store
            .conn
            .query_row(ORIGINAL_CLAIM_ELIGIBLE_SQL, args, |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()
            .unwrap();
        let rewritten: Option<(String, String)> = self
            .store
            .conn
            .query_row(CLAIM_ELIGIBLE_SQL, args, |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()
            .unwrap();
        assert_eq!(rewritten, original, "admission flags {holds:?}");
        assert_eq!(
            rewritten.map(|row| row.0),
            expected.map(|id| id.to_string())
        );
    }
}

fn create(parent_id: Uuid) -> ManagerActionV2 {
    ManagerActionV2::CreateSession {
        parent_id,
        kind: SessionKind::Task,
        query: "eligibility fixture".into(),
        launch: ManagerLaunchChoiceV2 {
            provider: rsi_common::types::SessionProvider::Codex,
            model: "fixture".into(),
            effort: None,
        },
        sandbox_source: None,
    }
}

fn operator_call(method: &str) -> ManagerActionV2 {
    ManagerActionV2::OperatorCall {
        call: OperatorCallV1 {
            method: method.into(),
            params: if method == "RunSandboxBuildCacheReclaim" {
                json!({"dry_run": true})
            } else {
                json!({})
            },
        },
        expected: None,
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn active_rewrite_changes_only_the_three_conflict_sources_554() {
    let restored = CLAIM_ELIGIBLE_SQL
        .replacen(
            "WITH active AS MATERIALIZED (\n             SELECT * FROM harness_manager_v2_operations\n              WHERE kind=?1 AND state IN ('running','uncertain')\n             ), eligible AS (",
            "WITH eligible AS (",
            1,
        )
        .replace("FROM active a WHERE", "FROM harness_manager_v2_operations a WHERE");
    assert_eq!(restored, ORIGINAL_CLAIM_ELIGIBLE_SQL);
    assert_eq!(CLAIM_ELIGIBLE_SQL.matches("FROM active a WHERE").count(), 3);
    // This fence is shared with other queries; the static SQL copy must track it.
    assert!(CLAIM_ELIGIBLE_SQL.contains(UNCERTAINTY_CURRENT_AUTHORITY));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn active_rewrite_matches_history_holds_due_time_and_ordering_554() {
    let f = Fixture::new();
    for state in [
        ManagerActionStateV2::Succeeded,
        ManagerActionStateV2::Failed,
        ManagerActionStateV2::Blocked,
        ManagerActionStateV2::Revoked,
    ] {
        f.seed(operator_call("ListSessions"), state, None);
    }
    let first = f.seed(create(Uuid::new_v4()), ManagerActionStateV2::Queued, None);
    let second = f.seed(
        operator_call("ListSessions"),
        ManagerActionStateV2::Queued,
        None,
    );
    for drain in [false, true] {
        for hold_creates in [false, true] {
            for reclaim_only in [false, true] {
                let expected = if reclaim_only {
                    None
                } else if drain || hold_creates {
                    Some(second)
                } else {
                    Some(first)
                };
                f.assert_selection(expected, [drain, hold_creates, reclaim_only]);
            }
        }
    }
    f.store.conn.execute(
        "UPDATE harness_manager_v2_operations SET not_before='2999-01-01T00:00:00.000000000Z' WHERE id=?1",
        [first.to_string()],
    ).unwrap();
    f.assert_selection(Some(second), [false; 3]);
    // Equal due times still use id as the tie breaker, regardless of insertion order.
    f.store.conn.execute(
        "UPDATE harness_manager_v2_operations SET not_before='2000-01-01T00:00:00.000000000Z' WHERE state='queued'",
        [],
    ).unwrap();
    f.assert_selection(Some(first.min(second)), [false; 3]);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn active_rewrite_matches_running_parent_parallelism_and_reclaim_fences_554() {
    for running in 0..=MANAGER_CREATE_PARALLELISM {
        for same_parent in [false, true] {
            let f = Fixture::new();
            let parent = Uuid::new_v4();
            for _ in 0..running {
                f.seed(
                    create(if same_parent { parent } else { Uuid::new_v4() }),
                    ManagerActionStateV2::Running,
                    None,
                );
            }
            let candidate = f.seed(create(parent), ManagerActionStateV2::Queued, None);
            let expected = if running == MANAGER_CREATE_PARALLELISM || (running > 0 && same_parent)
            {
                None
            } else {
                Some(candidate)
            };
            f.assert_selection(expected, [false; 3]);
            // The earlier create cannot pass the reclaim, but reclaim can pass
            // all running creates, even at the create parallelism limit.
            let reclaim = f.seed(
                operator_call("RunSandboxBuildCacheReclaim"),
                ManagerActionStateV2::Queued,
                None,
            );
            f.assert_selection(Some(reclaim), [false, false, true]);
            f.assert_selection(Some(reclaim), [true, true, true]);
        }
    }
    for method in ["ListSessions", "RunSandboxBuildCacheReclaim"] {
        let f = Fixture::new();
        f.seed(operator_call(method), ManagerActionStateV2::Running, None);
        f.seed(create(Uuid::new_v4()), ManagerActionStateV2::Queued, None);
        f.seed(
            operator_call("RunSandboxBuildCacheReclaim"),
            ManagerActionStateV2::Queued,
            None,
        );
        f.assert_selection(None, [false; 3]);
        f.assert_selection(None, [false, false, true]);
    }
    // Earlier queued non-creates retain priority when another create is running.
    let f = Fixture::new();
    f.seed(create(Uuid::new_v4()), ManagerActionStateV2::Running, None);
    f.seed(
        operator_call("ListSessions"),
        ManagerActionStateV2::Queued,
        None,
    );
    f.seed(create(Uuid::new_v4()), ManagerActionStateV2::Queued, None);
    f.assert_selection(None, [false; 3]);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn active_rewrite_matches_uncertain_targets_and_project_scope_554() {
    for state in [
        ManagerActionStateV2::Running,
        ManagerActionStateV2::Uncertain,
    ] {
        for same_project in [false, true] {
            for same_target in [false, true] {
                let f = Fixture::new();
                let foreign = Fixture::new();
                let mut scope = if same_project {
                    f.config.clone()
                } else {
                    // A real second project and manager in this same Store.
                    let project = foreign
                        .store
                        .get_project(foreign.config.project_id)
                        .unwrap()
                        .unwrap();
                    f.store.insert_project(&project).unwrap();
                    let manager = foreign
                        .store
                        .get_session(foreign.config.manager_session_id)
                        .unwrap()
                        .unwrap();
                    f.store.insert_session(&manager).unwrap();
                    foreign.config.clone()
                };
                // Prior scope/manager generations still fence within a project.
                scope.row_version += 1;
                let target = Uuid::new_v4();
                let active = f
                    .store
                    .seed_manager_action_with_context_for_test(
                        &scope,
                        operator_call("ListSessions"),
                        state,
                        Some(target),
                        scope.manager_session_id,
                    )
                    .unwrap();
                let candidate = f.seed(
                    operator_call("ListSessions"),
                    ManagerActionStateV2::Queued,
                    Some(if same_target { target } else { Uuid::new_v4() }),
                );
                let conflict =
                    same_project && (state == ManagerActionStateV2::Running || same_target);
                f.assert_selection(if conflict { None } else { Some(candidate) }, [false; 3]);
                // Settling this exact uncertain operation bypasses its own fence.
                if state == ManagerActionStateV2::Uncertain && same_project && same_target {
                    let settle = f.seed(
                        ManagerActionV2::SettleUncertainAction {
                            operation_id: active,
                            expected_row_version: 1,
                        },
                        ManagerActionStateV2::Queued,
                        Some(target),
                    );
                    f.assert_selection(Some(settle), [false; 3]);
                }
            }
        }
    }
}

const EXPLAIN_CLAIM_ELIGIBLE_SQL: &str = "EXPLAIN QUERY PLAN WITH active AS MATERIALIZED (
             SELECT * FROM harness_manager_v2_operations
              WHERE kind=?1 AND state IN ('running','uncertain')
             ), eligible AS (
             SELECT o.id,o.project_id,o.not_before,
                    json_extract(o.payload_json,'$.origin.origin') AS origin,
                    json_extract(o.payload_json,'$.request.operation.action') AS action
               FROM harness_manager_v2_operations o WHERE o.kind=?1 AND o.state='queued' AND o.not_before<=?2
             AND (?6=0 OR (json_extract(o.payload_json,'$.request.operation.action')='operator_call' AND json_extract(o.payload_json,'$.request.operation.call.method')='RunSandboxBuildCacheReclaim'))
             AND (?3=0 OR json_extract(o.payload_json,'$.request.operation.action') NOT IN ('create_session','replace_lead','retry_lead','resume_lead'))
             AND (?5=0 OR json_extract(o.payload_json,'$.request.operation.action')<>'create_session'
                  OR json_extract(o.payload_json,'$.issue_binding.continue_from') IS NOT NULL)
             AND NOT EXISTS(SELECT 1 FROM active a WHERE a.project_id=o.project_id AND a.kind=?1
                AND a.id IS NOT json_extract(o.payload_json,'$.request.operation.operation_id') AND ((a.state='running' AND NOT (((json_extract(o.payload_json,'$.request.operation.action')='operator_call' AND json_extract(o.payload_json,'$.request.operation.call.method')='RunSandboxBuildCacheReclaim') OR (json_extract(o.payload_json,'$.request.operation.action')='create_session' AND json_extract(a.payload_json,'$.request.operation.parent_id') IS NOT json_extract(o.payload_json,'$.request.operation.parent_id'))) AND json_extract(a.payload_json,'$.request.operation.action')='create_session')) OR (a.state='uncertain' AND (a.target_session_id=o.target_session_id OR (json_extract(a.payload_json,'$.request.operation.epic_id')=json_extract(o.payload_json,'$.request.operation.epic_id') AND (
    json_extract(a.payload_json,'$.request.operation.epic_id') IS NULL OR
    EXISTS(SELECT 1 FROM sessions e JOIN epic_lead_generations g ON g.epic_id=e.id
      WHERE e.id=json_extract(a.payload_json,'$.request.operation.epic_id') AND
      (e.lead_session_id=a.target_session_id OR
       (e.lead_session_id IS json_extract(a.payload_json,'$.request.operation.expected.lead_session_id')
        AND g.generation=json_extract(a.payload_json,'$.request.operation.expected.lead_generation'))))))))))
             AND (json_extract(o.payload_json,'$.request.operation.action')<>'create_session' OR (
                  (SELECT COUNT(*) FROM active a WHERE a.project_id=o.project_id AND a.kind=?1 AND a.state='running')<?4
              AND (NOT EXISTS(SELECT 1 FROM active a WHERE a.project_id=o.project_id AND a.kind=?1 AND a.state='running')
                   OR NOT EXISTS(SELECT 1 FROM harness_manager_v2_operations q WHERE q.project_id=o.project_id AND q.kind=?1 AND q.state='queued'
                        AND json_extract(q.payload_json,'$.request.operation.action')<>'create_session' AND (q.not_before,q.id)<(o.not_before,o.id)))))
             AND NOT EXISTS(SELECT 1 FROM manager_root_successions r JOIN sessions p ON p.id=r.predecessor_session_id
                WHERE r.operation_id=o.id AND r.state='reserved'
                  AND (p.status IN ('Starting','Running','WaitingApproval')
                       OR EXISTS(SELECT 1 FROM daemon_settings d WHERE d.key='manager_operator_pause:'||p.id AND d.value<>'false'))
                  AND r.authority_epoch=(SELECT epoch FROM manager_authority_epochs WHERE project_id=r.project_id)
                  AND EXISTS(SELECT 1 FROM harness_manager_scopes h WHERE h.project_id=r.project_id AND h.row_version=r.scope_version))
             ), oldest AS (SELECT * FROM eligible ORDER BY not_before,id LIMIT 1)
             SELECT e.id,e.not_before FROM eligible e CROSS JOIN oldest first
              WHERE e.id=first.id OR (
                    first.origin='operating_intent'
                AND first.action IN ('resume_lead','retry_lead','replace_lead')
                AND e.project_id=first.project_id
                AND e.origin='agent' AND e.action='create_session')
              ORDER BY CASE WHEN e.id=first.id THEN 1 ELSE 0 END,e.not_before,e.id LIMIT 1";

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn active_rewrite_uses_a_state_bounded_search_on_bundled_sqlite_554() {
    assert_eq!(
        EXPLAIN_CLAIM_ELIGIBLE_SQL.strip_prefix("EXPLAIN QUERY PLAN "),
        Some(CLAIM_ELIGIBLE_SQL)
    );
    let f = Fixture::new();
    for _ in 0..64 {
        f.seed(
            operator_call("ListSessions"),
            ManagerActionStateV2::Succeeded,
            None,
        );
    }
    f.seed(create(Uuid::new_v4()), ManagerActionStateV2::Running, None);
    f.seed(
        operator_call("ListSessions"),
        ManagerActionStateV2::Uncertain,
        Some(Uuid::new_v4()),
    );
    let candidate = f.seed(create(Uuid::new_v4()), ManagerActionStateV2::Queued, None);
    f.assert_selection(Some(candidate), [false; 3]);
    let mut statement = f.store.conn.prepare(EXPLAIN_CLAIM_ELIGIBLE_SQL).unwrap();
    let plan = statement
        .query_map(
            params![ACTION_KIND, now(), 0, MANAGER_CREATE_PARALLELISM, 0, 0],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    eprintln!("bundled SQLite {} plan: {plan:#?}", rusqlite::version());
    let materialization = plan
        .iter()
        .find(|row| row.2 == "MATERIALIZE active")
        .expect("active rows must be materialized once")
        .0;
    let sources: Vec<_> = plan.iter().filter(|row| row.1 == materialization).collect();
    assert_eq!(sources.len(), 1, "active materialization source: {plan:#?}");
    assert!(
        sources[0]
            .2
            .starts_with("SEARCH harness_manager_v2_operations "),
        "{plan:#?}"
    );
    assert!(
        sources[0].2.contains("(state=?)"),
        "active source must be state-bounded: {plan:#?}"
    );
    // SQLite can inline eligible into both consumers (six conflict nodes),
    // or materialize it (three). Every conflict node still reads the active
    // set through its automatic index, independent of that planner choice.
    let conflicts: Vec<_> = plan
        .iter()
        .filter(|row| row.2.starts_with("SEARCH a ") || row.2 == "SCAN a")
        .collect();
    assert!(conflicts.len() >= 3, "{plan:#?}");
    assert!(
        conflicts
            .iter()
            .all(|row| row.2.starts_with("SEARCH a USING AUTOMATIC")),
        "{plan:#?}"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn active_rewrite_matches_uncertain_current_lead_generation_fences_554() {
    for installed_target in [false, true] {
        for current_generation in [false, true] {
            for same_epic in [false, true] {
                let f = Fixture::new();
                let mut epic = crate::test_support::make_test_session();
                epic.project_id = Some(f.config.project_id);
                epic.session_kind = SessionKind::Epic;
                epic.lead_session_id = Some(f.config.manager_session_id);
                f.store.insert_session(&epic).unwrap();
                let mut other_epic = epic.clone();
                other_epic.id = Uuid::new_v4();
                f.store.insert_session(&other_epic).unwrap();
                let generation: i64 = f
                    .store
                    .conn
                    .query_row(
                        "SELECT generation FROM epic_lead_generations WHERE epic_id=?1",
                        [epic.id.to_string()],
                        |r| r.get(0),
                    )
                    .unwrap();
                let expected = ManagerLeadFenceV2 {
                    lead_session_id: Some(f.config.manager_session_id),
                    lead_generation: if current_generation {
                        generation
                    } else {
                        generation + 1
                    },
                    event_sequence: 0,
                    custody_generation: None,
                };
                f.seed(
                    ManagerActionV2::PauseLead {
                        epic_id: epic.id,
                        expected: expected.clone(),
                        reason: "uncertain predecessor".into(),
                    },
                    ManagerActionStateV2::Uncertain,
                    Some(if installed_target {
                        f.config.manager_session_id
                    } else {
                        Uuid::new_v4()
                    }),
                );
                let candidate = f.seed(
                    ManagerActionV2::PauseLead {
                        epic_id: if same_epic { epic.id } else { other_epic.id },
                        expected,
                        reason: "candidate".into(),
                    },
                    ManagerActionStateV2::Queued,
                    Some(Uuid::new_v4()),
                );
                let conflict = same_epic && (installed_target || current_generation);
                f.assert_selection(if conflict { None } else { Some(candidate) }, [false; 3]);
            }
        }
    }
}
