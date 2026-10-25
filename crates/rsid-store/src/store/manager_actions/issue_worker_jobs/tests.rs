use super::*;
use crate::store::agent_jobs::NewAgentJob;
use crate::store::manager_actions::{
    ManagerActionClaimV2, ManagerActionSourceV2, ManagerIssueBindingV1,
};
use crate::store::manager_obligations::test_support as ob;
use crate::store::sandbox_custody::{CustodyCause, NewCustodyRoot, SessionCustodyBinding};
use rsi_common::agent_jobs::{AgentJobResultV1, JobState, JobWake};
use rsi_common::harness_manager::{ConfigureHarnessManagerRequestV1, HarnessManagerConfigV1};
use rsi_common::harness_manager_v2::ManagerLaunchChoiceV2;
use rsi_common::types::{SandboxCleanupState, SandboxKind, SessionKind, SessionProvider};
use rsi_common::wake_predicate::WakePredicate;

struct World {
    store: Store,
    config: HarnessManagerConfigV1,
    issue: Uuid,
    predecessor: Session,
    successor: Session,
    launch: Uuid,
    _dir: tempfile::TempDir,
}

impl World {
    fn new() -> Self {
        let dir = crate::test_support::disk_backed_tempdir("worker-job-continuation");
        let mut store = Store::open_in_memory().unwrap();
        let project = crate::store::d04_test_project_id();
        let mut manager = crate::test_support::make_test_session();
        manager.project_id = Some(project);
        store.insert_session(&manager).unwrap();
        let config = store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                project_id: project,
                session_id: manager.id,
                epic_ids: None,
                group_ids: vec![],
                expected_row_version: 0,
            })
            .unwrap();
        let issue = store
            .create_issue(&rsi_common::types::NewIssue {
                project_id: project,
                title: "continuation jobs".into(),
                body: String::new(),
                priority: None,
                labels: vec![],
                created_by_session_id: None,
                assignee: None,
                idea_id: None,
                source_event_id: None,
                source_finding_ref: None,
            })
            .unwrap()
            .id;
        let predecessor = Self::session(&mut store, &config, dir.path(), None);
        let successor = Self::session(&mut store, &config, dir.path(), Some(predecessor.id));
        let mut w = Self {
            store,
            config,
            issue,
            predecessor,
            successor,
            launch: Uuid::nil(),
            _dir: dir,
        };
        w.seed_launch(w.predecessor.id, None);
        w.launch = w.seed_launch(w.successor.id, Some(&w.predecessor));
        w
    }

    fn session(
        store: &mut Store,
        config: &HarnessManagerConfigV1,
        dir: &std::path::Path,
        previous: Option<Uuid>,
    ) -> Session {
        let mut s = crate::test_support::make_test_session();
        s.project_id = Some(config.project_id);
        s.status = SessionStatus::Completed;
        s.session_kind = SessionKind::Task;
        s.continued_from = previous;
        s.working_dir = dir.join("repo");
        s.sandbox_root = Some(dir.join(s.id.to_string()));
        s.sandbox_branch = Some(format!("rsi/{}", s.id));
        s.sandbox_kind = Some(SandboxKind::GitWorktree);
        s.sandbox_cleanup_state = Some(SandboxCleanupState::Live);
        store
            .insert_session_with_custody(
                &s,
                SessionCustodyBinding::New(NewCustodyRoot {
                    custody_id: Uuid::new_v4(),
                    canonical_repo_dir: s.working_dir.to_string_lossy().into_owned(),
                    sandbox_root: s
                        .sandbox_root
                        .as_ref()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    sandbox_branch: s.sandbox_branch.clone().unwrap(),
                    repository_identity: "worker-fixture".into(),
                    source_commit: "a".repeat(40),
                    cause: CustodyCause::FreshLaunch,
                }),
            )
            .unwrap();
        s
    }

    fn seed_launch(&self, target: Uuid, previous: Option<&Session>) -> Uuid {
        let id = self
            .store
            .seed_manager_action_with_context_for_test(
                &self.config,
                ManagerActionV2::CreateSession {
                    parent_id: self.config.manager_session_id,
                    kind: SessionKind::Task,
                    query: "continue verification".into(),
                    sandbox_source: None,
                    launch: ManagerLaunchChoiceV2 {
                        provider: SessionProvider::Codex,
                        model: "fixture".into(),
                        effort: None,
                    },
                },
                ManagerActionStateV2::Succeeded,
                Some(target),
                self.config.manager_session_id,
            )
            .unwrap();
        let mut op = self.store.manager_action_operation(id).unwrap().unwrap();
        op.context.issue_binding = Some(ManagerIssueBindingV1 {
            issue_id: self.issue,
            display_number: 1,
            continue_from: previous.map(|s| s.id),
            qa_lane: false,
            review_of: None,
            review_source_commit: None,
            related: vec![],
            brief_refs: vec![],
            carried_related: vec![],
            skipped_related: vec![],
            capacity_retry: None,
        });
        op.context.source = previous.map(|s| ManagerActionSourceV2 {
            parent_working_dir: None,
            session_id: self.config.manager_session_id,
            working_dir: s.working_dir.clone(),
            sandbox_root: None,
            commit: "a".repeat(40),
            custody_generation: Some(1),
            historical_commit: false,
            branch: s.sandbox_branch.clone(),
            sandbox_source: Some(ManagerFrozenSandboxSourceV1::Path {
                path: s.sandbox_root.clone().unwrap(),
                source_dirty: false,
            }),
        });
        // Use the production context writer; the operation's binding snapshot
        // is also how real Issue-worker admission journals the binding.
        self.store
            .manager_v2_put_record(
                &self.config,
                super::super::CONTEXT_KIND,
                &id.to_string(),
                None,
                1,
                &serde_json::to_value(&op.context).unwrap(),
            )
            .unwrap();
        self.store.conn.execute(
            "UPDATE harness_manager_v2_operations SET payload_json=json_set(payload_json,'$.issue_binding',json(?2)) WHERE id=?1",
            params![id.to_string(), serde_json::to_string(&op.context.issue_binding).unwrap()],
        ).unwrap();
        id
    }

    fn job(&self, owner: &Session, wake: JobWake) -> NewAgentJob {
        let mut job = ob::job(owner.id);
        job.project_id = owner.project_id;
        job.cwd = owner
            .sandbox_root
            .as_ref()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        job.wake = wake;
        job.idempotency_key = Some(format!("verification-{}", job.id));
        self.store
            .insert_agent_job(&job, chrono::Utc::now())
            .unwrap();
        job
    }

    fn publish(&self) {
        let boot = Uuid::new_v4();
        self.store
            .conn
            .execute(
                "UPDATE harness_manager_v2_operations SET state='running',claim_boot_id=?2,
               outcome_json=json_set(outcome_json,'$.state','running') WHERE id=?1",
                params![self.launch.to_string(), boot.to_string()],
            )
            .unwrap();
        let claim = ManagerActionClaimV2 {
            operation: self
                .store
                .manager_action_operation(self.launch)
                .unwrap()
                .unwrap(),
            boot_id: boot,
        };
        self.store
            .finish_manager_action(&claim, ManagerActionStateV2::Succeeded, "launched")
            .unwrap();
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn successor_reads_and_watches_predecessor_evidence_without_changing_job_identity() {
    let w = World::new();
    let live = w.job(&w.predecessor, JobWake::None);
    let settled = w.job(&w.predecessor, JobWake::None);
    let receipt = serde_json::json!({"head":"b".repeat(40),"ok":true,"completed":2});
    w.store
        .settle_agent_job(
            settled.id,
            JobState::Succeeded,
            &AgentJobResultV1 {
                exit_code: Some(0),
                receipt: Some(receipt.clone()),
                ..Default::default()
            },
            false,
            chrono::Utc::now(),
        )
        .unwrap();
    for id in [live.id, settled.id] {
        let observed = w
            .store
            .get_agent_job_for_reader(w.successor.id, id)
            .unwrap()
            .unwrap();
        assert_eq!(observed.job.owner_session_id, w.predecessor.id);
        assert_eq!(observed.job.cwd, live.cwd);
        assert!(w.store.job_watchable_by(w.successor.id, id).unwrap());
    }
    assert_eq!(
        w.store
            .get_agent_job_for_reader(w.successor.id, settled.id)
            .unwrap()
            .unwrap()
            .job
            .result
            .unwrap()
            .receipt,
        Some(receipt)
    );
    assert_eq!(
        w.store
            .list_agent_jobs_for_reader(w.successor.id, 1)
            .unwrap()[0]
            .job
            .id,
        settled.id
    );
    let outsider = Uuid::new_v4();
    assert!(
        w.store
            .get_agent_job_for_reader(outsider, live.id)
            .unwrap()
            .is_none()
    );
    assert!(!w.store.job_watchable_by(outsider, live.id).unwrap());
    // Exact replay still resolves under the ORIGINAL owner and key.
    let (replayed, duplicate) = w.store.insert_agent_job(&live, chrono::Utc::now()).unwrap();
    assert!(duplicate);
    assert_eq!(replayed.job.id, live.id);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn publication_carries_one_batch_and_late_owner_completion_without_resubmitting_jobs() {
    let w = World::new();
    let a = w.job(&w.predecessor, JobWake::None);
    let b = w.job(&w.predecessor, JobWake::None);
    let owner_job = w.job(&w.predecessor, JobWake::Owner);
    let batch = ob::wake_row(
        w.predecessor.id,
        "resume",
        "focused-verification",
        None,
        None,
    );
    let state = ob::state(WakePredicate {
        jobs_terminal: Some(vec![a.id, b.id]),
        ..Default::default()
    });
    w.store.insert_wake_when(&batch, &state, false).unwrap();
    let generic = ob::wake_row(w.predecessor.id, "resume", "unrelated-backstop", None, None);
    w.store.insert_scheduled_job(&generic).unwrap();
    w.publish();
    assert_eq!(
        w.store
            .get_scheduled_job(&batch.id)
            .unwrap()
            .unwrap()
            .wake_session_id,
        Some(w.successor.id)
    );
    assert_eq!(w.store.wake_when_state(batch.id).unwrap().unwrap(), state);
    assert_eq!(
        w.store
            .get_scheduled_job(&generic.id)
            .unwrap()
            .unwrap()
            .wake_session_id,
        Some(w.predecessor.id)
    );
    assert!(
        w.store
            .owner_running_job_has_completion_wake(w.successor.id)
            .unwrap()
    );
    for id in [a.id, b.id] {
        w.store
            .settle_agent_job(
                id,
                JobState::Succeeded,
                &AgentJobResultV1::default(),
                false,
                chrono::Utc::now(),
            )
            .unwrap();
    }
    assert!(
        w.store
            .owner_job_result_wake_pending(w.successor.id)
            .unwrap()
    );
    let wake = w
        .store
        .settle_agent_job(
            owner_job.id,
            JobState::Succeeded,
            &AgentJobResultV1::default(),
            true,
            chrono::Utc::now(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        w.store
            .get_scheduled_job(&wake)
            .unwrap()
            .unwrap()
            .wake_session_id,
        Some(w.successor.id)
    );
    assert_eq!(
        w.store
            .settle_agent_job(
                owner_job.id,
                JobState::Succeeded,
                &AgentJobResultV1::default(),
                true,
                chrono::Utc::now()
            )
            .unwrap(),
        None
    );
    assert_eq!(
        w.store
            .list_agent_jobs(w.predecessor.id, 100)
            .unwrap()
            .len(),
        3
    );
    assert_eq!(
        w.store
            .list_owned_scheduled_jobs(w.successor.id, false, 100)
            .unwrap()
            .len(),
        2
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn source_custody_project_and_publication_fences_deny_inherited_evidence() {
    let w = World::new();
    let job = w.job(&w.predecessor, JobWake::None);
    let denied = || {
        assert!(
            w.store
                .get_agent_job_for_reader(w.successor.id, job.id)
                .unwrap()
                .is_none()
        );
        assert!(!w.store.job_watchable_by(w.successor.id, job.id).unwrap());
        assert!(
            w.store
                .list_agent_jobs_for_reader(w.successor.id, 100)
                .unwrap()
                .is_empty()
        );
    };
    w.store
        .conn
        .execute(
            "UPDATE agent_jobs SET project_id=NULL WHERE id=?1",
            [job.id.to_string()],
        )
        .unwrap();
    denied();
    w.store
        .conn
        .execute(
            "UPDATE agent_jobs SET project_id=?2,cwd='foreign-root' WHERE id=?1",
            params![job.id.to_string(), w.config.project_id.to_string()],
        )
        .unwrap();
    denied();
    w.store
        .conn
        .execute(
            "UPDATE agent_jobs SET cwd=?2 WHERE id=?1",
            params![job.id.to_string(), job.cwd],
        )
        .unwrap();
    w.store
        .conn
        .execute(
            "UPDATE sessions SET continued_from=NULL WHERE id=?1",
            [w.successor.id.to_string()],
        )
        .unwrap();
    denied();
    w.store
        .conn
        .execute(
            "UPDATE sessions SET continued_from=?2 WHERE id=?1",
            params![w.successor.id.to_string(), w.predecessor.id.to_string()],
        )
        .unwrap();
    w.store.conn.execute("UPDATE sandbox_custody_roots SET validation_state='unverified',validated_generation=NULL,validated_at=NULL WHERE owner_session_id=?1", [w.successor.id.to_string()]).unwrap();
    denied();
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn unpublished_or_superseded_launch_grants_nothing_and_settled_wake_carries_once() {
    let w = World::new();
    w.store.conn.execute("UPDATE harness_manager_v2_operations SET outcome_json=json_set(outcome_json,'$.state','running'),state='running' WHERE id=?1", [w.launch.to_string()]).unwrap();
    let job = w.job(&w.predecessor, JobWake::Owner);
    let wake = w
        .store
        .settle_agent_job(
            job.id,
            JobState::Succeeded,
            &AgentJobResultV1::default(),
            true,
            chrono::Utc::now(),
        )
        .unwrap()
        .unwrap();
    assert!(
        w.store
            .get_agent_job_for_reader(w.successor.id, job.id)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        w.store
            .get_scheduled_job(&wake)
            .unwrap()
            .unwrap()
            .wake_session_id,
        Some(w.predecessor.id)
    );
    w.publish();
    assert_eq!(
        w.store
            .get_scheduled_job(&wake)
            .unwrap()
            .unwrap()
            .wake_session_id,
        Some(w.successor.id)
    );
    let tx = rusqlite::Transaction::new_unchecked(
        &w.store.conn,
        rusqlite::TransactionBehavior::Immediate,
    )
    .unwrap();
    w.store.carry_worker_job_wakes_on(w.successor.id).unwrap();
    tx.commit().unwrap();
    assert_eq!(
        w.store
            .list_owned_scheduled_jobs(w.successor.id, false, 100)
            .unwrap()
            .len(),
        1
    );
    let later = w.seed_launch(Uuid::new_v4(), None);
    w.store
        .conn
        .execute(
            "UPDATE harness_manager_v2_operations SET state='failed' WHERE id=?1",
            [later.to_string()],
        )
        .unwrap();
    assert!(
        w.store
            .get_agent_job_for_reader(w.successor.id, job.id)
            .unwrap()
            .is_none()
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn repeated_continuations_keep_original_evidence_and_reject_a_different_source_pin() {
    let mut w = World::new();
    let job = w.job(&w.predecessor, JobWake::None);
    let tip = World::session(&mut w.store, &w.config, w._dir.path(), Some(w.successor.id));
    let launch = w.seed_launch(tip.id, Some(&w.successor));
    assert!(
        w.store
            .get_agent_job_for_reader(tip.id, job.id)
            .unwrap()
            .is_some()
    );
    assert!(
        w.store
            .get_agent_job_for_reader(w.successor.id, job.id)
            .unwrap()
            .is_none()
    );
    // Keep the allocated custody immutable. A mismatched frozen launch pin
    // must not grant access to evidence from the original allocation.
    let mut op = w.store.manager_action_operation(launch).unwrap().unwrap();
    op.context.source.as_mut().unwrap().commit = "b".repeat(40);
    let context_kind = super::super::CONTEXT_KIND;
    let record = w
        .store
        .manager_v2_record(&w.config, context_kind, &launch.to_string())
        .unwrap()
        .unwrap();
    w.store
        .manager_v2_put_record(
            &w.config,
            context_kind,
            &launch.to_string(),
            None,
            record.row_version,
            &serde_json::to_value(&op.context).unwrap(),
        )
        .unwrap();
    assert!(
        w.store
            .get_agent_job_for_reader(tip.id, job.id)
            .unwrap()
            .is_none()
    );
    assert!(!w.store.job_watchable_by(tip.id, job.id).unwrap());
}
