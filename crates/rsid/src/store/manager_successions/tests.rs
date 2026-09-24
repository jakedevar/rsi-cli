//! Store boundary fixtures deliberately do not assert physical process proof.
//! Runtime/provider fault tests are the Phase3 integration dependency.
use super::*;
use crate::store::sandbox_custody::{CustodyCause, NewCustodyRoot, SessionCustodyBinding};
use rsi_common::harness_manager::ConfigureHarnessManagerRequestV1;
use rsi_common::types::{Project, SandboxCleanupState, SandboxKind};
use tempfile::TempDir;

struct World {
    dir: TempDir,
    store: Store,
    project: Uuid,
    owner: Uuid,
    handoff: ManagerCommittedHandoffV2,
}
impl World {
    fn new(sandbox: bool) -> Self {
        Self::with_active_limit(sandbox, 1)
    }
    fn with_active_limit(sandbox: bool, max_active_sessions: u16) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("store.db")).unwrap();
        let project = Uuid::new_v4();
        let now = chrono::Utc::now();
        store
            .insert_project(&Project {
                id: project,
                name: "root fixture".into(),
                path: Some(dir.path().join("repo")),
                description: None,
                color: Project::DEFAULT_COLOR.into(),
                context_files: None,
                created_at: now,
                updated_at: now,
            })
            .unwrap();
        let mut session = crate::store::tests::make_test_session();
        session.project_id = Some(project);
        session.working_dir = dir.path().join("repo");
        session.status = SessionStatus::Running;
        session.session_kind = SessionKind::Standard;
        session.parent_id = None;
        session.provider = SessionProvider::Codex;
        session.model = Some("gpt-6-astra".into());
        session.effort = Some("high".into());
        session.cost_usd = None;
        if sandbox {
            let binding = Self::new_binding(&dir, &mut session);
            store
                .insert_session_with_custody(&session, binding)
                .unwrap();
        } else {
            store.insert_session(&session).unwrap();
        }
        store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                project_id: project,
                session_id: session.id,
                epic_ids: None,
                group_ids: vec![],
                expected_row_version: 0,
            })
            .unwrap();
        let policy = ManagerPolicyV2 {
            mode: ManagerOperatingModeV2::Execute,
            capabilities: vec![
                ManagerCapabilityV2::SelfSuccession,
                ManagerCapabilityV2::Topology,
            ],
            max_created_sessions: 3,
            max_created_containers: 3,
            max_active_sessions,
            allow_create_groups: true,
            max_recovery_attempts: 0,
            ..Default::default()
        };
        store
            .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
                project_id: project,
                expected_scope_version: 1,
                expected_policy_version: 0,
                idempotency_key: "grant".into(),
                policy,
            })
            .unwrap();
        Self {
            dir,
            store,
            project,
            owner: session.id,
            handoff: ManagerCommittedHandoffV2 {
                source_commit: "a".repeat(40),
                relative_path: "handoff.md".into(),
                blob_oid: "b".repeat(40),
            },
        }
    }
    fn new_binding(dir: &TempDir, row: &mut Session) -> SessionCustodyBinding {
        let root = dir.path().join(row.id.to_string());
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("dirty-work"), "retained uncommitted bytes").unwrap();
        row.sandbox_root = Some(root.clone());
        row.sandbox_branch = Some(format!("rsi/{}", row.id));
        row.sandbox_kind = Some(SandboxKind::GitWorktree);
        row.sandbox_cleanup_state = Some(SandboxCleanupState::Live);
        SessionCustodyBinding::New(NewCustodyRoot {
            custody_id: Uuid::new_v4(),
            canonical_repo_dir: dir.path().join("repo").to_string_lossy().into_owned(),
            sandbox_root: root.to_string_lossy().into_owned(),
            sandbox_branch: row.sandbox_branch.clone().unwrap(),
            repository_identity: "fixture-repository".into(),
            source_commit: "a".repeat(40),
            cause: CustodyCause::FreshLaunch,
        })
    }
    fn request(&self, key: &str) -> AgentManagerControlRequestV2 {
        let grant = self
            .store
            .get_harness_manager_policy(self.project)
            .unwrap()
            .unwrap();
        let config = self
            .store
            .get_harness_manager(self.project)
            .unwrap()
            .unwrap();
        AgentManagerControlRequestV2 {
            fence: ManagerFenceV2 {
                scope_version: config.row_version,
                policy_version: grant.row_version,
            },
            idempotency_key: key.into(),
            operation: ManagerActionV2::SucceedManager {
                expected: self
                    .store
                    .manager_succession_observation(self.owner)
                    .unwrap()
                    .expected,
                launch: ManagerLaunchChoiceV2 {
                    provider: SessionProvider::Codex,
                    model: "gpt-6-astra".into(),
                    effort: Some("medium".into()),
                },
                handoff: self.handoff.clone(),
            },
        }
    }
    fn proof(&self) -> VerifiedManagerHandoff {
        let source = self.store.get_session(self.owner).unwrap().unwrap();
        let custody = source
            .sandbox_root
            .as_ref()
            .map(|_| self.store.live_custody_for_session(self.owner).unwrap());
        VerifiedManagerHandoff::from_authenticated_source(&source, &self.handoff, custody.as_ref())
            .unwrap()
    }
    fn enqueue(&self, key: &str) -> ManagerActionReceiptV2 {
        self.store
            .enqueue_manager_succession(self.owner, &self.request(key), &self.proof())
            .unwrap()
    }
    fn claim(&self, receipt: &ManagerActionReceiptV2) -> ManagerSuccessionClaim {
        let claim = self.unsettled_claim(receipt);
        let row = self.store.get_session(self.owner).unwrap().unwrap();
        let proof = ManagerPredecessorSettledWitness::after_checked_drain(
            &row,
            claim.reservation.frozen.predecessor_invocation_id,
            claim.action.boot_id,
        )
        .unwrap();
        self.store
            .record_manager_succession_predecessor_settled(&claim, &proof)
            .unwrap()
    }
    fn unsettled_claim(&self, receipt: &ManagerActionReceiptV2) -> ManagerSuccessionClaim {
        self.store
            .update_session_status(self.owner, SessionStatus::Completed)
            .unwrap();
        let action = self
            .store
            .claim_manager_action(Uuid::new_v4())
            .unwrap()
            .unwrap();
        assert_eq!(action.id(), receipt.operation_id);
        self.store.claim_manager_succession(&action).unwrap()
    }
    /// Seed the exact already-admitted ledger row inside the same transaction as
    /// the storage callback. Production Model Control gates are Phase3-owned.
    fn admit(&self, claim: &ManagerSuccessionClaim) -> ManagerSuccessionClaim {
        let tx =
            Transaction::new_unchecked(&self.store.conn, TransactionBehavior::Immediate).unwrap();
        let r = &claim.reservation;
        tx.execute("INSERT INTO model_invocations(id,purpose,invocation_kind,foreground,paid_risk,admission_status,status,provider,model,effort,trigger_source,session_id,project_id,parent_invocation_id,dedup_key,request_fingerprint,created_at) VALUES(?1,'session.rotate.child','session_lifecycle','foreground','paid_capable','admitted','running','Codex',?2,?3,'manager_self_succession',?4,?5,?6,?7,?8,?9)",params![r.model_invocation_id.to_string(),r.frozen.launch.model,r.frozen.launch.effort,r.candidate_session_id.to_string(),r.project_id.to_string(),r.frozen.predecessor_invocation_id.map(|id|id.to_string()),r.invocation_dedup_key(),r.invocation_fingerprint().unwrap(),now()]).unwrap();
        let claim = self
            .store
            .record_manager_succession_admission_on(claim)
            .unwrap();
        tx.commit().unwrap();
        claim
    }
    fn bound(&mut self, claim: &ManagerSuccessionClaim) -> ManagerSuccessionClaim {
        let root = &claim.reservation;
        let mut candidate = root.frozen.predecessor.clone();
        candidate.id = root.candidate_session_id;
        candidate.status = SessionStatus::Starting;
        candidate.continued_from = Some(root.predecessor_session_id);
        candidate.rotation_depth += 1;
        candidate.model = Some(root.frozen.launch.model.clone());
        candidate.effort = root.frozen.launch.effort.clone();
        let binding = Self::new_binding(&self.dir, &mut candidate);
        self.store
            .insert_direct_session_with_custody_and_invocation(
                &candidate,
                binding,
                root.model_invocation_id,
            )
            .unwrap();
        let bound = self
            .store
            .record_manager_succession_candidate_bound(claim)
            .unwrap();
        self.store
            .claim_manager_succession_provider_effect(&bound)
            .unwrap()
    }
    fn publication(claim: &ManagerSuccessionClaim) -> ManagerSuccessionPublicationWitness {
        let r = &claim.reservation;
        ManagerSuccessionPublicationWitness::after_provider_established(
            r.candidate_session_id,
            r.model_invocation_id,
            r.launch_attempt_id,
            claim.action.boot_id,
        )
        .unwrap()
    }
    fn count(&self, table: &str) -> i64 {
        self.store
            .conn
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }
    fn root_snapshot(&self, id: Uuid) -> Vec<rusqlite::types::Value> {
        let mut statement = self
            .store
            .conn
            .prepare("SELECT * FROM manager_root_successions WHERE operation_id=?1")
            .unwrap();
        let columns = statement.column_count();
        statement
            .query_row([id.to_string()], |row| {
                (0..columns).map(|i| row.get(i)).collect()
            })
            .unwrap()
    }
}

#[test]
fn root_succession_drain_defers_same_occurrence_after_v110_migration_and_reopen() {
    let w = World::new(false);
    // Exercise the authentic released predecessor, not a version-only rewind.
    crate::store::tests::rewind_store_to_schema_version(&w.store.conn, 110);
    w.store.init_schema().unwrap();
    // Complete legacy invocation repair before freezing the source identity.
    let other = Store::open(&w.dir.path().join("store.db")).unwrap();
    let receipt = w.enqueue("drain-reopen");
    let original = w
        .store
        .manager_succession(receipt.operation_id)
        .unwrap()
        .unwrap();
    w.store
        .record_manager_root_spend_floor(w.owner, 7.25)
        .unwrap();
    let invocation_count = w.count("model_invocations");
    let mut audit = vec![(1, "reserved".to_string())];
    for attempt in 1..=3 {
        let claim = w.unsettled_claim(&receipt);
        other.defer_manager_succession_drain(&claim).unwrap();
        let op = w
            .store
            .manager_action_operation(receipt.operation_id)
            .unwrap()
            .unwrap();
        let root = w
            .store
            .manager_succession(receipt.operation_id)
            .unwrap()
            .unwrap();
        assert_eq!(op.receipt.state, ManagerActionStateV2::Queued);
        assert_eq!(
            op.receipt.outcome.as_deref(),
            Some("awaiting_predecessor_settlement")
        );
        assert_eq!(root.state, ManagerRootState::Reserved);
        assert_eq!((op.claim_boot_id, root.claim_boot_id), (None, None));
        assert_eq!(op.receipt.row_version, 1 + 2 * attempt);
        assert_eq!(root.row_version, op.receipt.row_version);
        assert_eq!(
            (
                root.candidate_session_id,
                root.launch_attempt_id,
                root.model_invocation_id
            ),
            (
                original.candidate_session_id,
                original.launch_attempt_id,
                original.model_invocation_id
            )
        );
        assert_eq!(
            serde_json::to_value(&root.frozen).unwrap(),
            serde_json::to_value(&original.frozen).unwrap()
        );
        assert_eq!(root.authority_epoch, original.authority_epoch);
        assert!(!root.admission_recorded && !root.effect_claimed && !op.effect_started);
        assert_eq!(w.count("model_invocations"), invocation_count);
        assert!(
            w.store
                .get_session(root.candidate_session_id)
                .unwrap()
                .is_none()
        );
        audit.extend([
            (2 * attempt, "executing".into()),
            (2 * attempt + 1, "reserved".into()),
        ]);
        assert!(w.store.defer_manager_succession_drain(&claim).is_err());
    }
    let reopened = Store::open(&w.dir.path().join("store.db")).unwrap();
    assert_eq!(
        reopened
            .recover_manager_actions_startup(Uuid::new_v4())
            .unwrap(),
        0
    );
    let rows: Vec<(i64, String)> = reopened.conn.prepare(
        "SELECT row_version,state FROM manager_root_transitions WHERE operation_id=?1 ORDER BY sequence"
    ).unwrap().query_map([receipt.operation_id.to_string()], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().collect::<std::result::Result<_,_>>().unwrap();
    assert_eq!(rows, audit);
    let quantities: (i64, i64, i64) = reopened.conn.query_row(
        "SELECT count(*),sum(creation_quantity),sum(recovery_quantity) FROM manager_root_successions", [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    ).unwrap();
    assert_eq!(quantities, (1, 1, 0));
    let origins = reopened
        .manager_root_resource_origins(w.project, None, 256)
        .unwrap();
    assert_eq!(origins.len(), 1);
    assert_eq!(origins[0].known_floor_usd, 7.25);
    assert!(!origins[0].zero_origin);
    let replay = reopened
        .enqueue_manager_succession(w.owner, &w.request("drain-reopen"), &w.proof())
        .unwrap();
    assert_eq!(replay.operation_id, receipt.operation_id);
    assert_eq!(replay.state, ManagerActionStateV2::Queued);
    assert!(replay.deduplicated);
    // A later real claim can advance to settlement; old claims stay spent.
    let settled = w.claim(&receipt);
    assert_eq!(settled.reservation.state, ManagerRootState::Executing);
    w.store.manager_succession_effect_gate(&settled).unwrap();
}

#[test]
fn root_succession_drain_stale_claim_and_changed_policy_cannot_requeue() {
    let w = World::new(false);
    let receipt = w.enqueue("drain-stale");
    let old = w.unsettled_claim(&receipt);
    w.store.defer_manager_succession_drain(&old).unwrap();
    let current = w.unsettled_claim(&receipt);
    let assert_running = || {
        assert_eq!(
            w.store
                .manager_succession(receipt.operation_id)
                .unwrap()
                .unwrap()
                .row_version,
            current.reservation.row_version
        );
        assert_eq!(
            w.store
                .manager_action_operation(receipt.operation_id)
                .unwrap()
                .unwrap()
                .receipt
                .state,
            ManagerActionStateV2::Running
        );
    };
    assert!(
        w.store
            .defer_manager_succession_drain(&old)
            .unwrap_err()
            .to_string()
            .contains("manager_v2_claim_changed")
    );
    let mut wrong_boot = current.clone();
    wrong_boot.action.boot_id = Uuid::new_v4();
    assert!(w.store.defer_manager_succession_drain(&wrong_boot).is_err());
    let mut wrong_root_version = current.clone();
    wrong_root_version.reservation.row_version -= 1;
    assert!(
        w.store
            .defer_manager_succession_drain(&wrong_root_version)
            .unwrap_err()
            .to_string()
            .contains("manager_succession_claim_changed")
    );
    assert_running();
    let grant = w
        .store
        .get_harness_manager_policy(w.project)
        .unwrap()
        .unwrap();
    w.store
        .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
            project_id: w.project,
            expected_scope_version: grant.scope_version,
            expected_policy_version: grant.row_version,
            idempotency_key: "operator-pause".into(),
            policy: ManagerPolicyV2 {
                paused: true,
                ..grant.policy
            },
        })
        .unwrap();
    assert!(
        w.store
            .defer_manager_succession_drain(&current)
            .unwrap_err()
            .to_string()
            .contains("manager_v2_policy_changed")
    );
    assert_running();
}

#[test]
fn root_succession_drain_refuses_after_settlement_admission_and_provider_ownership() {
    for phase in ["settled", "admitted", "effect"] {
        let mut w = World::new(true);
        let receipt = w.enqueue(phase);
        let mut claim = w.claim(&receipt);
        if phase != "settled" {
            claim = w.admit(&claim);
        }
        if phase == "effect" {
            claim = w.bound(&claim);
        }
        let before = w.root_snapshot(receipt.operation_id);
        let transitions = w.count("manager_root_transitions");
        let invocations = w.count("model_invocations");
        let error = w.store.defer_manager_succession_drain(&claim).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("manager_succession_cleanup_required"),
            "{phase}: {error}"
        );
        // Even a direct transition cannot circumvent the V111 state guard.
        let error = w.store.conn.execute(
            "UPDATE manager_root_successions SET state='reserved',claim_boot_id=NULL,row_version=row_version+1 WHERE operation_id=?1",
            [receipt.operation_id.to_string()],
        ).unwrap_err();
        assert!(
            error.to_string().contains("manager_root_state"),
            "{phase}: {error}"
        );
        assert_eq!(w.root_snapshot(receipt.operation_id), before);
        assert_eq!(
            w.store
                .manager_action_operation(receipt.operation_id)
                .unwrap()
                .unwrap()
                .receipt
                .state,
            ManagerActionStateV2::Running
        );
        assert_eq!(w.count("manager_root_transitions"), transitions);
        assert_eq!(w.count("model_invocations"), invocations);
    }
}

#[test]
fn root_succession_drain_guard_rejects_partial_effect_witnesses() {
    for assignment in [
        "admission_recorded=1",
        "effect_claimed=1",
        "settled_json='{}'",
        "establishment_json='{}'",
        "published_epoch=1",
    ] {
        let w = World::new(false);
        let receipt = w.enqueue("partial-witness");
        let claim = w.unsettled_claim(&receipt);
        w.store.conn.execute(
            &format!("UPDATE manager_root_successions SET {assignment},row_version=row_version+1 WHERE operation_id=?1"),
            [receipt.operation_id.to_string()],
        ).unwrap();
        let claim = w.store.refreshed_manager_succession_claim(&claim).unwrap();
        assert!(
            w.store.defer_manager_succession_drain(&claim).is_err(),
            "{assignment}"
        );
        assert!(w.store.conn.execute(
            "UPDATE manager_root_successions SET state='reserved',claim_boot_id=NULL,row_version=row_version+1 WHERE operation_id=?1",
            [receipt.operation_id.to_string()],
        ).unwrap_err().to_string().contains("manager_root_state"), "{assignment}");
        assert_eq!(
            w.store
                .manager_succession(receipt.operation_id)
                .unwrap()
                .unwrap()
                .row_version,
            claim.reservation.row_version
        );
    }
}

#[test]
fn root_succession_drain_failure_rolls_back_journal_root_and_audit_together() {
    let w = World::new(false);
    let receipt = w.enqueue("drain-fault");
    let claim = w.unsettled_claim(&receipt);
    let before = w.root_snapshot(receipt.operation_id);
    let transitions = w.count("manager_root_transitions");
    w.store.conn.execute_batch("CREATE TEMP TRIGGER drain_abort BEFORE UPDATE OF state ON manager_root_successions WHEN NEW.state='reserved' BEGIN SELECT RAISE(ABORT,'drain fault'); END;").unwrap();
    assert!(
        w.store
            .defer_manager_succession_drain(&claim)
            .unwrap_err()
            .to_string()
            .contains("drain fault")
    );
    assert_eq!(w.root_snapshot(receipt.operation_id), before);
    assert_eq!(
        serde_json::to_value(
            w.store
                .manager_action_operation(receipt.operation_id)
                .unwrap()
                .unwrap()
                .receipt
        )
        .unwrap(),
        serde_json::to_value(&claim.action.operation.receipt).unwrap()
    );
    assert_eq!(w.count("manager_root_transitions"), transitions);
    w.store
        .conn
        .execute_batch("DROP TRIGGER drain_abort;")
        .unwrap();
    w.store.defer_manager_succession_drain(&claim).unwrap();
    assert_eq!(
        w.store
            .manager_action_operation(receipt.operation_id)
            .unwrap()
            .unwrap()
            .receipt
            .state,
        ManagerActionStateV2::Queued
    );
    assert_eq!(
        w.store
            .manager_succession(receipt.operation_id)
            .unwrap()
            .unwrap()
            .state,
        ManagerRootState::Reserved
    );
}

#[test]
fn root_succession_replay_conflict_and_wait_do_not_charge_or_claim_twice() {
    let w = World::new(false);
    let req = w.request("same");
    let receipt = w
        .store
        .enqueue_manager_succession(w.owner, &req, &w.proof())
        .unwrap();
    let replay = w
        .store
        .enqueue_manager_succession(w.owner, &req, &w.proof())
        .unwrap();
    assert_eq!(replay.operation_id, receipt.operation_id);
    assert!(replay.deduplicated);
    assert!(
        w.store
            .claim_manager_action(Uuid::new_v4())
            .unwrap()
            .is_none()
    );
    let mut changed = req.clone();
    if let ManagerActionV2::SucceedManager { launch, .. } = &mut changed.operation {
        launch.effort = None;
    }
    assert!(
        w.store
            .enqueue_manager_succession(w.owner, &changed, &w.proof())
            .is_err()
    );
    assert!(
        w.store
            .enqueue_manager_succession(w.owner, &w.request("competitor"), &w.proof())
            .is_err()
    );
    assert_eq!(w.count("manager_root_successions"), 1);
    assert_eq!(w.count("model_invocations"), 0);
    let grant = w
        .store
        .get_harness_manager_policy(w.project)
        .unwrap()
        .unwrap();
    assert_eq!(grant.policy.max_recovery_attempts, 0);
    let original = w
        .store
        .manager_root_resource_origins(w.project, None, 256)
        .unwrap();
    assert_eq!(original.len(), 1);
    assert!(!original[0].zero_origin);
    assert_eq!(original[0].known_floor_usd, 0.0);
}

#[test]
fn root_succession_active_wait_skips_project_queue_and_intents_have_no_authority() {
    let w = World::new(false);
    w.enqueue("root");
    let mut request = w.request("ordinary");
    request.operation = ManagerActionV2::CreateContainer {
        parent_id: None,
        kind: SessionKind::Group,
        name: "ordinary group".into(),
        tags: vec!["manager-created".into()],
    };
    let receipt = w
        .store
        .enqueue_manager_action(ManagerActionOriginV2::Agent { caller: w.owner }, request)
        .unwrap();
    assert_eq!(
        w.store
            .claim_manager_action(Uuid::new_v4())
            .unwrap()
            .unwrap()
            .id(),
        receipt.operation_id
    );
    assert!(
        w.store
            .enqueue_manager_action(
                ManagerActionOriginV2::OperatingIntent {
                    project_id: w.project,
                    intent_id: Uuid::new_v4()
                },
                w.request("intent")
            )
            .is_err()
    );
}

#[test]
fn root_succession_two_connections_reopen_share_reservation_and_single_claim() {
    let w = World::new(false);
    // Legacy-open repair may attach the previously absent invocation. Observe
    // both connections before admission; a later identity change is tested below.
    let other = Store::open(&w.dir.path().join("store.db")).unwrap();
    let receipt = w.enqueue("first");
    assert!(
        other
            .enqueue_manager_succession(w.owner, &w.request("second"), &w.proof())
            .is_err()
    );
    let claim = w.claim(&receipt);
    assert!(
        other
            .claim_manager_action(Uuid::new_v4())
            .unwrap()
            .is_none()
    );
    assert_eq!(
        other
            .manager_succession(receipt.operation_id)
            .unwrap()
            .unwrap()
            .model_invocation_id,
        claim.reservation.model_invocation_id
    );
    assert_eq!(
        other
            .recover_manager_actions_startup(Uuid::new_v4())
            .unwrap(),
        1
    );
    let recovered = other
        .manager_succession(receipt.operation_id)
        .unwrap()
        .unwrap();
    assert_eq!(recovered.state, ManagerRootState::CleanupRequired);
    assert_eq!(
        other
            .manager_action_operation(receipt.operation_id)
            .unwrap()
            .unwrap()
            .receipt
            .state,
        ManagerActionStateV2::Uncertain
    );
    assert!(w.store.manager_succession_effect_gate(&claim).is_err());
}

#[test]
fn root_succession_distinct_publication_preserves_logical_policy_work_and_dirty_custody() {
    let mut w = World::new(true);
    let config = w.store.get_harness_manager(w.project).unwrap().unwrap();
    let grant =
        serde_json::to_value(w.store.get_harness_manager_policy(w.project).unwrap()).unwrap();
    w.store
        .manager_v2_put_record(
            &config,
            "test_pending_exchange",
            "mail",
            None,
            0,
            &json!({"actor":w.owner,"pending":true}),
        )
        .unwrap();
    let before = w.store.live_custody_for_session(w.owner).unwrap();
    let receipt = w.enqueue("publish");
    let claim = w.claim(&receipt);
    let claim = w.admit(&claim);
    let claim = w.bound(&claim);
    let candidate = claim.reservation.candidate_session_id;
    assert!(w.store.manager_config_for_caller(candidate).is_err());
    let published = w
        .store
        .commit_manager_succession(&claim, &World::publication(&claim))
        .unwrap();
    assert_eq!(published.state, ManagerActionStateV2::Succeeded);
    let after = w.store.get_harness_manager(w.project).unwrap().unwrap();
    assert_eq!(after.manager_session_id, w.owner);
    assert_eq!(after.current_session_id, Some(candidate));
    assert_eq!(after.row_version, config.row_version);
    assert_eq!(
        serde_json::to_value(w.store.get_harness_manager_policy(w.project).unwrap()).unwrap(),
        grant
    );
    assert_eq!(
        w.store
            .manager_v2_record(&after, "test_pending_exchange", "mail")
            .unwrap()
            .unwrap()
            .payload,
        json!({"actor":w.owner,"pending":true})
    );
    assert!(w.store.manager_config_for_caller(w.owner).is_err());
    assert_eq!(
        w.store.get_session(w.owner).unwrap().unwrap().status,
        SessionStatus::Archived
    );
    let kept = w.store.live_custody_for_session(w.owner).unwrap();
    assert_eq!(kept.owner_session_id, before.owner_session_id);
    assert_eq!(kept.generation, before.generation);
    assert_eq!(kept.sandbox_root, before.sandbox_root);
    assert_eq!(
        std::fs::read_to_string(PathBuf::from(kept.sandbox_root).join("dirty-work")).unwrap(),
        "retained uncommitted bytes"
    );
    assert_ne!(
        w.store
            .live_custody_for_session(candidate)
            .unwrap()
            .custody_id,
        before.custody_id
    );
    let actor: String = w
        .store
        .conn
        .query_row(
            "SELECT actor_session_id FROM harness_manager_v2_operations WHERE id=?1",
            [receipt.operation_id.to_string()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(actor, w.owner.to_string());
    let root = w
        .store
        .manager_succession(receipt.operation_id)
        .unwrap()
        .unwrap();
    assert!(root.published_epoch.unwrap() > root.authority_epoch);
    assert!(
        w.store
            .enqueue_manager_succession(w.owner, &root.frozen.request, &w.proof())
            .is_err()
    );
}

#[test]
fn root_succession_publishes_after_normal_result_telemetry_write() {
    let mut w = World::new(true);
    let receipt = w.enqueue("result-telemetry");
    let mut predecessor = w.store.get_session(w.owner).unwrap().unwrap();
    for field in [
        predecessor.thinking_tokens,
        predecessor.cache_creation_1h_tokens,
        predecessor.cache_creation_5m_tokens,
        predecessor.permission_denial_count,
        predecessor.queued_turn_count,
    ] {
        assert_eq!(field, None, "source was frozen before result telemetry");
    }
    assert_eq!(predecessor.service_tier, None);
    assert_eq!(predecessor.subagent_stats_json, None);
    assert_eq!(predecessor.terminal_reason, None);

    let result = crate::claude::StreamEvent {
        event_type: "result".into(),
        data: json!({
            "duration_ms": 4210,
            "total_cost_usd": 0.0421,
            "num_turns": 1,
            "stop_reason": "end_turn",
            "terminal_reason": "completed",
            "queued_turn_count": 0,
            "permission_denials": [],
            "subagent_stats": {"spawned": 0},
            "usage": {
                "input_tokens": 12,
                "output_tokens": 34,
                "service_tier": "standard",
                "output_tokens_details": {"thinking_tokens": 0},
                "cache_creation": {
                    "ephemeral_1h_input_tokens": 7411,
                    "ephemeral_5m_input_tokens": 0
                }
            }
        }),
    };
    let meta = crate::monitor::extract_result_metadata(&result, predecessor.model.as_deref());
    predecessor.duration_ms = meta.duration_ms;
    predecessor.cost_usd = meta.cost_usd;
    predecessor.num_turns = meta.num_turns;
    predecessor.input_tokens = meta.final_input_tokens;
    predecessor.output_tokens = meta.final_output_tokens;
    predecessor.stop_reason = meta.stop_reason;
    predecessor.thinking_tokens = meta.thinking_tokens;
    predecessor.service_tier = meta.service_tier;
    predecessor.cache_creation_1h_tokens = meta.cache_creation_1h_tokens;
    predecessor.cache_creation_5m_tokens = meta.cache_creation_5m_tokens;
    predecessor.permission_denial_count = meta.permission_denial_count;
    predecessor.subagent_stats_json = meta.subagent_stats_json;
    predecessor.queued_turn_count = meta.queued_turn_count;
    predecessor.terminal_reason = meta.terminal_reason;
    w.store.update_session_metadata(&predecessor).unwrap();
    assert_eq!(
        w.store
            .get_session(w.owner)
            .unwrap()
            .unwrap()
            .thinking_tokens,
        Some(0)
    );

    let claim = w.claim(&receipt);
    let claim = w.admit(&claim);
    let claim = w.bound(&claim);
    let candidate = claim.reservation.candidate_session_id;
    let published = w
        .store
        .commit_manager_succession(&claim, &World::publication(&claim))
        .unwrap();
    assert_eq!(published.state, ManagerActionStateV2::Succeeded);
    assert_eq!(
        w.store
            .get_harness_manager(w.project)
            .unwrap()
            .unwrap()
            .current_session_id,
        Some(candidate)
    );
}

#[test]
fn root_succession_refuses_authority_change_after_enqueue() {
    let w = World::new(false);
    let receipt = w.enqueue("model-change");
    w.store
        .conn
        .execute(
            "UPDATE sessions SET model='changed' WHERE id=?1",
            [w.owner.to_string()],
        )
        .unwrap();
    let claim = w.unsettled_claim(&receipt);
    let row = w.store.get_session(w.owner).unwrap().unwrap();
    let proof = ManagerPredecessorSettledWitness::after_checked_drain(
        &row,
        claim.reservation.frozen.predecessor_invocation_id,
        claim.action.boot_id,
    )
    .unwrap();
    assert!(
        w.store
            .record_manager_succession_predecessor_settled(&claim, &proof)
            .unwrap_err()
            .to_string()
            .contains("manager_succession_source_changed")
    );
}

#[test]
fn root_succession_publication_failure_rolls_back_edge_archive_epoch_root_and_journal() {
    let mut w = World::new(true);
    let receipt = w.enqueue("atomic");
    let claim = w.claim(&receipt);
    let claim = w.admit(&claim);
    let claim = w.bound(&claim);
    let epoch = w.store.manager_authority_epoch(w.project).unwrap();
    w.store.conn.execute_batch("CREATE TEMP TRIGGER root_test_abort BEFORE INSERT ON harness_manager_v2_events WHEN NEW.kind='action_result' BEGIN SELECT RAISE(ABORT,'publication fault'); END;").unwrap();
    assert!(
        w.store
            .commit_manager_succession(&claim, &World::publication(&claim))
            .is_err()
    );
    assert_eq!(
        w.store.get_session(w.owner).unwrap().unwrap().status,
        SessionStatus::Completed
    );
    assert_eq!(w.store.manager_authority_epoch(w.project).unwrap(), epoch);
    assert_eq!(
        w.store
            .manager_succession(receipt.operation_id)
            .unwrap()
            .unwrap()
            .row_version,
        claim.reservation.row_version
    );
    assert_eq!(
        w.store
            .manager_action_operation(receipt.operation_id)
            .unwrap()
            .unwrap()
            .receipt
            .state,
        ManagerActionStateV2::Running
    );
    assert_eq!(w.count("harness_manager_rotation_edges"), 0);
    w.store
        .conn
        .execute_batch("DROP TRIGGER root_test_abort;")
        .unwrap();
    assert_eq!(
        w.store
            .commit_manager_succession(&claim, &World::publication(&claim))
            .unwrap()
            .state,
        ManagerActionStateV2::Succeeded
    );
}

#[test]
fn root_succession_epoch_covers_normal_rotation_restore_retirement_foreign_project_and_overflow() {
    let w = World::new(false);
    let before = w.store.manager_authority_epoch(w.project).unwrap();
    let mut next = w.store.get_session(w.owner).unwrap().unwrap();
    next.id = Uuid::new_v4();
    next.continued_from = Some(w.owner);
    next.rotation_depth += 1;
    next.status = SessionStatus::Starting;
    w.store.insert_session(&next).unwrap();
    w.store
        .update_session_status(w.owner, SessionStatus::Archived)
        .unwrap();
    assert!(
        w.store
            .record_harness_manager_rotation(w.owner, next.id)
            .unwrap()
    );
    let rotated = w.store.manager_authority_epoch(w.project).unwrap();
    assert!(rotated > before);
    w.store
        .update_session_status(w.owner, SessionStatus::Completed)
        .unwrap();
    assert!(w.store.manager_authority_epoch(w.project).unwrap() > rotated);
    w.store
        .update_session_status(w.owner, SessionStatus::Archived)
        .unwrap();
    assert!(
        w.store
            .record_harness_manager_rotation(w.owner, next.id)
            .is_err()
    );
    let another = World::new(false);
    assert_eq!(
        another
            .store
            .manager_authority_epoch(another.project)
            .unwrap(),
        before
    );
    w.store
        .conn
        .execute(
            "UPDATE manager_authority_epochs SET epoch=?2 WHERE project_id=?1",
            params![w.project.to_string(), i64::MAX],
        )
        .unwrap();
    assert!(
        w.store
            .update_session_status(w.owner, SessionStatus::Completed)
            .is_err()
    );
    assert_eq!(
        w.store.get_session(w.owner).unwrap().unwrap().status,
        SessionStatus::Archived
    );
    assert_eq!(
        w.store.manager_authority_epoch(w.project).unwrap(),
        i64::MAX
    );
}

#[test]
fn root_succession_scope_change_blocks_effect_and_retains_creation_charge() {
    let w = World::new(false);
    let receipt = w.enqueue("scope");
    let claim = w.claim(&receipt);
    let policy = w
        .store
        .get_harness_manager_policy(w.project)
        .unwrap()
        .unwrap()
        .policy;
    w.store
        .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
            project_id: w.project,
            session_id: w.owner,
            epic_ids: Some(vec![]),
            group_ids: vec![],
            expected_row_version: 1,
        })
        .unwrap();
    assert!(w.store.manager_succession_effect_gate(&claim).is_err());
    w.store
        .finish_manager_action(
            &claim.action,
            ManagerActionStateV2::Revoked,
            "scope_changed",
        )
        .unwrap();
    w.store
        .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
            project_id: w.project,
            expected_scope_version: 2,
            expected_policy_version: 1,
            idempotency_key: "new-scope".into(),
            policy: ManagerPolicyV2 {
                max_created_sessions: 1,
                ..policy
            },
        })
        .unwrap();
    assert!(
        w.store
            .enqueue_manager_succession(w.owner, &w.request("second"), &w.proof())
            .is_err()
    );
    assert_eq!(w.count("manager_root_successions"), 1);
}

#[test]
fn root_succession_admission_and_journal_recovery_are_atomic_and_usage_stays_unknown() {
    let w = World::new(false);
    let receipt = w.enqueue("accounting");
    let claim = w.claim(&receipt);
    assert!(
        w.store
            .record_manager_succession_admission_on(&claim)
            .is_err()
    );
    let admitted = w.admit(&claim);
    let tx = Transaction::new_unchecked(&w.store.conn, TransactionBehavior::Immediate).unwrap();
    let replay = w
        .store
        .record_manager_succession_admission_on(&admitted)
        .unwrap();
    assert_eq!(
        replay.reservation.row_version,
        admitted.reservation.row_version
    );
    tx.commit().unwrap();
    assert_eq!(w.count("model_invocations"), 1);
    assert_eq!(w.count("manager_root_resource_origins"), 2);
    w.store
        .recover_manager_actions_startup(Uuid::new_v4())
        .unwrap();
    let root = w
        .store
        .manager_succession(receipt.operation_id)
        .unwrap()
        .unwrap();
    assert!(
        w.store
            .settle_manager_succession(
                &root,
                &ManagerSuccessionCleanupWitness::after_checked_settlement(&root)
            )
            .is_err()
    );
    w.store
        .conn
        .execute(
            "UPDATE model_invocations SET status='failed' WHERE id=?1",
            [root.model_invocation_id.to_string()],
        )
        .unwrap();
    let receipt = w
        .store
        .settle_manager_succession(
            &root,
            &ManagerSuccessionCleanupWitness::after_checked_settlement(&root),
        )
        .unwrap();
    assert_eq!(receipt.state, ManagerActionStateV2::Failed);
    assert_eq!(w.count("manager_root_resource_origins"), 2);
    let unknown: Option<i64> = w
        .store
        .conn
        .query_row(
            "SELECT input_tokens FROM model_invocations WHERE id=?1",
            [root.model_invocation_id.to_string()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(unknown, None);
}

#[test]
fn root_succession_invalid_metadata_and_early_publication_do_not_confer_authority() {
    let w = World::new(false);
    let receipt = w.enqueue("guard");
    let claim = w.claim(&receipt);
    assert!(
        w.store
            .commit_manager_succession(&claim, &World::publication(&claim))
            .is_err()
    );
    assert!(
        w.store
            .finish_manager_action(&claim.action, ManagerActionStateV2::Succeeded, "pretend")
            .is_err()
    );
    for assignment in [
        "candidate_session_id='spoof'",
        "launch_attempt_id='spoof'",
        "recovery_quantity=1",
        "authority_epoch=authority_epoch+1",
        "state='committed'",
    ] {
        assert!(w.store.conn.execute(&format!("UPDATE manager_root_successions SET {assignment},row_version=row_version+1 WHERE operation_id=?1"),[receipt.operation_id.to_string()]).is_err(),"{assignment}");
    }
    assert!(
        w.store
            .conn
            .execute(
                "DELETE FROM manager_root_successions WHERE operation_id=?1",
                [receipt.operation_id.to_string()]
            )
            .is_err()
    );
    assert_eq!(
        w.store
            .get_harness_manager(w.project)
            .unwrap()
            .unwrap()
            .current_session_id,
        Some(w.owner)
    );
}

#[test]
fn root_succession_new_event_policy_and_operator_pause_invalidate_settlement() {
    let w = World::new(false);
    let receipt = w.enqueue("pause");
    let claim = w.claim(&receipt);
    w.store
        .record_manager_operator_pause(w.owner, true)
        .unwrap();
    assert!(w.store.manager_succession_effect_gate(&claim).is_err());
    w.store
        .record_manager_operator_pause(w.owner, false)
        .unwrap();
    assert!(w.store.manager_succession_effect_gate(&claim).is_ok());
    let mut row = w.store.get_session(w.owner).unwrap().unwrap();
    row.query = "new operator request".into();
    w.store
        .conn
        .execute(
            "UPDATE sessions SET query=?2 WHERE id=?1",
            params![row.id.to_string(), row.query],
        )
        .unwrap();
    assert!(w.store.manager_succession_effect_gate(&claim).is_err());
}

#[test]
fn root_succession_concurrent_connections_admit_only_one_occurrence() {
    let w = World::new(false);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let paths = [w.dir.path().join("store.db"), w.dir.path().join("store.db")];
    let handles: Vec<_> = paths
        .into_iter()
        .enumerate()
        .map(|(index, path)| {
            let request = w.request(&format!("writer-{index}"));
            let proof = w.proof();
            let barrier = barrier.clone();
            let caller = w.owner;
            std::thread::spawn(move || {
                let store = Store::open(&path).unwrap();
                barrier.wait();
                store
                    .enqueue_manager_succession(caller, &request, &proof)
                    .is_ok()
            })
        })
        .collect();
    assert_eq!(
        handles
            .into_iter()
            .filter(|h| h.thread().id() != std::thread::current().id())
            .map(|h| usize::from(h.join().unwrap()))
            .sum::<usize>(),
        1
    );
    assert_eq!(w.count("manager_root_successions"), 1);
    assert_eq!(w.count("harness_manager_v2_operations"), 2); // policy save + root action
}

#[test]
fn root_succession_real_pending_mail_survives_publication_and_reopen() {
    use rsi_common::harness_manager::{AgentManagerInboxRequestV1, AgentManagerSendRequestV1};
    // Both the retained live lead and the new manager consume capacity.
    let mut w = World::with_active_limit(true, 2);
    let mut group = crate::store::tests::make_test_session();
    group.project_id = Some(w.project);
    group.session_kind = SessionKind::Group;
    group.parent_id = None;
    w.store.insert_session(&group).unwrap();
    let mut epic = group.clone();
    epic.id = Uuid::new_v4();
    epic.session_kind = SessionKind::Epic;
    epic.parent_id = Some(group.id);
    w.store.insert_session(&epic).unwrap();
    let mut lead = epic.clone();
    lead.id = Uuid::new_v4();
    lead.session_kind = SessionKind::Feature;
    lead.parent_id = Some(epic.id);
    w.store.insert_session(&lead).unwrap();
    w.store.set_lead_session(epic.id, Some(lead.id)).unwrap();
    let mail = w
        .store
        .manager_send(
            w.owner,
            &AgentManagerSendRequestV1 {
                epic_id: epic.id,
                message: "pending scoped exchange".into(),
                idempotency_key: "pending-mail".into(),
            },
        )
        .unwrap();
    let receipt = w.enqueue("with-mail");
    let claim = w.claim(&receipt);
    let claim = w.admit(&claim);
    let claim = w.bound(&claim);
    w.store
        .commit_manager_succession(&claim, &World::publication(&claim))
        .unwrap();
    let reopened = Store::open(&w.dir.path().join("store.db")).unwrap();
    let inbox = reopened
        .manager_inbox(
            claim.reservation.candidate_session_id,
            &AgentManagerInboxRequestV1 {
                request_id: Some(mail.message_id),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(inbox.messages.len(), 1);
    assert_eq!(inbox.messages[0].message, "pending scoped exchange");
    assert_eq!(inbox.messages[0].sender_session_id, w.owner);
    let retained: (String, i64) = reopened
        .conn
        .query_row(
            "SELECT manager_session_id,scope_version FROM harness_manager_messages WHERE id=?1",
            [mail.message_id.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(retained, (w.owner.to_string(), 1));
    assert!(
        reopened
            .manager_inbox(w.owner, &Default::default())
            .is_err()
    );
}

#[test]
fn root_succession_admission_failure_rolls_back_invocation_origin_and_marker() {
    let w = World::new(false);
    let receipt = w.enqueue("admission-fault");
    let claim = w.claim(&receipt);
    w.store.conn.execute_batch("CREATE TEMP TRIGGER root_admission_abort BEFORE UPDATE OF admission_recorded ON manager_root_successions BEGIN SELECT RAISE(ABORT,'admission fault'); END;").unwrap();
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| w.admit(&claim))).is_err());
    assert_eq!(w.count("model_invocations"), 0);
    assert_eq!(w.count("manager_root_resource_origins"), 1);
    assert!(
        !w.store
            .manager_succession(receipt.operation_id)
            .unwrap()
            .unwrap()
            .admission_recorded
    );
    assert_eq!(
        w.store
            .manager_succession(receipt.operation_id)
            .unwrap()
            .unwrap()
            .row_version,
        claim.reservation.row_version
    );
}

#[test]
fn root_succession_manual_disable_preserved_and_checked_failed_interrupted_are_supported() {
    for status in [SessionStatus::Failed, SessionStatus::Interrupted] {
        let w = World::new(false);
        w.store
            .conn
            .execute(
                "UPDATE sessions SET rotation_disabled_at=?2 WHERE id=?1",
                params![w.owner.to_string(), now()],
            )
            .unwrap();
        let receipt = w.enqueue("manual");
        w.store.update_session_status(w.owner, status).unwrap();
        let action = w
            .store
            .claim_manager_action(Uuid::new_v4())
            .unwrap()
            .unwrap();
        assert_eq!(action.id(), receipt.operation_id);
        let claim = w.store.claim_manager_succession(&action).unwrap();
        let row = w.store.get_session(w.owner).unwrap().unwrap();
        assert!(
            claim
                .reservation
                .frozen
                .predecessor
                .rotation_disabled_at
                .is_some()
        );
        let proof =
            ManagerPredecessorSettledWitness::after_checked_drain(&row, None, action.boot_id)
                .unwrap();
        let claim = w
            .store
            .record_manager_succession_predecessor_settled(&claim, &proof)
            .unwrap();
        assert!(w.store.manager_succession_effect_gate(&claim).is_ok());
        w.store.conn.execute("INSERT INTO daemon_settings(key,value,updated_at) VALUES('context_rotation_enabled','false',?1)",[now()]).unwrap();
        assert!(w.store.manager_succession_effect_gate(&claim).is_err());
    }
}

#[test]
fn root_succession_reopen_changed_legacy_invocation_refuses_old_settlement() {
    let w = World::new(false);
    let receipt = w.enqueue("legacy-reopen");
    let original = w
        .store
        .manager_succession(receipt.operation_id)
        .unwrap()
        .unwrap();
    assert_eq!(original.frozen.predecessor_invocation_id, None);
    let other = Store::open(&w.dir.path().join("store.db")).unwrap();
    w.store
        .update_session_status(w.owner, SessionStatus::Completed)
        .unwrap();
    let action = w
        .store
        .claim_manager_action(Uuid::new_v4())
        .unwrap()
        .unwrap();
    let claim = w.store.claim_manager_succession(&action).unwrap();
    let (row, _, invocation, _) =
        super::super::sandbox_custody::load_rotation_authority_session_on(&other.conn, w.owner)
            .unwrap()
            .unwrap();
    assert!(invocation.is_some());
    let actual =
        ManagerPredecessorSettledWitness::after_checked_drain(&row, invocation, action.boot_id)
            .unwrap();
    assert!(
        w.store
            .record_manager_succession_predecessor_settled(&claim, &actual)
            .is_err()
    );
    assert!(
        !w.store
            .manager_succession(receipt.operation_id)
            .unwrap()
            .unwrap()
            .effect_claimed
    );
    assert_eq!(
        w.store
            .get_harness_manager(w.project)
            .unwrap()
            .unwrap()
            .current_session_id,
        Some(w.owner)
    );
}

#[test]
fn root_succession_original_spend_floor_never_resets_or_asserts_zero_history() {
    let w = World::new(false);
    w.enqueue("floor");
    w.store
        .record_manager_root_spend_floor(w.owner, 3.25)
        .unwrap();
    w.store
        .record_manager_root_spend_floor(w.owner, 0.5)
        .unwrap();
    assert!(
        w.store
            .record_manager_root_spend_floor(w.owner, f64::NAN)
            .is_err()
    );
    assert!(
        w.store
            .record_manager_root_spend_floor(w.owner, f64::INFINITY)
            .is_err()
    );
    let origins = w
        .store
        .manager_root_resource_origins(w.project, None, 1)
        .unwrap();
    assert_eq!(origins[0].known_floor_usd, 3.25);
    assert!(!origins[0].zero_origin);
    let reopened = Store::open(&w.dir.path().join("store.db")).unwrap();
    assert_eq!(
        reopened
            .manager_root_resource_origins(w.project, None, 256)
            .unwrap()[0]
            .known_floor_usd,
        3.25
    );
}

#[test]
fn root_succession_invocation_replay_requires_exact_frozen_choice_and_only_one_row() {
    let w = World::new(false);
    let receipt = w.enqueue("exact-invocation");
    let claim = w.claim(&receipt);
    let claim = w.admit(&claim);
    let r = &claim.reservation;
    assert!(w.store.manager_succession_check_invocation(r).is_ok());
    w.store
        .conn
        .execute(
            "UPDATE model_invocations SET effort=NULL WHERE id=?1",
            [r.model_invocation_id.to_string()],
        )
        .unwrap();
    assert!(w.store.manager_succession_check_invocation(r).is_err());
    w.store
        .conn
        .execute(
            "UPDATE model_invocations SET effort=?2 WHERE id=?1",
            params![r.model_invocation_id.to_string(), r.frozen.launch.effort],
        )
        .unwrap();
    w.store.conn.execute("INSERT INTO model_invocations(id,purpose,invocation_kind,foreground,paid_risk,admission_status,status,trigger_source,session_id,created_at) VALUES(?1,'session.rotate.child','session_lifecycle','foreground','paid_capable','admitted','running','competing',?2,?3)",params![Uuid::new_v4().to_string(),r.candidate_session_id.to_string(),now()]).unwrap();
    assert!(w.store.manager_succession_check_invocation(r).is_err());
    assert_eq!(
        w.store
            .manager_succession(receipt.operation_id)
            .unwrap()
            .unwrap()
            .model_invocation_id,
        r.model_invocation_id
    );
}

#[test]
fn root_succession_lost_owner_failure_rolls_back_both_aggregates() {
    let w = World::new(false);
    let receipt = w.enqueue("recovery-fault");
    let claim = w.claim(&receipt);
    w.store.conn.execute_batch("CREATE TEMP TRIGGER root_recovery_abort BEFORE INSERT ON harness_manager_v2_events WHEN NEW.kind='action_result' BEGIN SELECT RAISE(ABORT,'recovery fault'); END;").unwrap();
    assert!(
        w.store
            .recover_manager_actions_startup(Uuid::new_v4())
            .is_err()
    );
    assert_eq!(
        w.store
            .manager_succession(receipt.operation_id)
            .unwrap()
            .unwrap()
            .row_version,
        claim.reservation.row_version
    );
    assert_eq!(
        w.store
            .manager_action_operation(receipt.operation_id)
            .unwrap()
            .unwrap()
            .receipt
            .state,
        ManagerActionStateV2::Running
    );
    w.store
        .conn
        .execute_batch("DROP TRIGGER root_recovery_abort;")
        .unwrap();
    assert_eq!(
        w.store
            .recover_manager_actions_startup(Uuid::new_v4())
            .unwrap(),
        1
    );
    assert_eq!(
        w.store
            .manager_succession(receipt.operation_id)
            .unwrap()
            .unwrap()
            .state,
        ManagerRootState::CleanupRequired
    );
    assert_eq!(
        w.store
            .manager_action_operation(receipt.operation_id)
            .unwrap()
            .unwrap()
            .receipt
            .state,
        ManagerActionStateV2::Uncertain
    );
}
