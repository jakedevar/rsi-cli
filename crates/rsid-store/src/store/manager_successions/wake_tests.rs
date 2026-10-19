//! #2652: how a `succeed_manager` publication settles the seat's resume wakes.
//!
//! Kept out of `tests.rs` on purpose: a change to that file selects the
//! released-migration slow tier, and these fixtures need none of it. The
//! fixture is the unsandboxed slice of `tests.rs`'s `World`.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
use super::*;
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
use crate::store::sandbox_custody::{CustodyCause, NewCustodyRoot, SessionCustodyBinding};
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
use rsi_common::harness_manager::ConfigureHarnessManagerRequestV1;
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
use rsi_common::types::{Project, SandboxCleanupState, SandboxKind};
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
use tempfile::TempDir;

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
struct World {
    dir: TempDir,
    store: Store,
    project: Uuid,
    owner: Uuid,
    handoff: ManagerCommittedHandoffV2,
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
impl World {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_test_fixture(&dir.path().join("store.db")).unwrap();
        let project = Uuid::new_v4();
        let now = chrono::Utc::now();
        store
            .insert_project(&Project {
                id: project,
                name: "wake fixture".into(),
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
        store.insert_session(&session).unwrap();
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
            max_active_sessions: 1,
            allow_create_groups: true,
            max_recovery_attempts: 0,
            allowed_launches: vec![ManagerLaunchChoiceV2 {
                provider: SessionProvider::Codex,
                model: "gpt-6-astra".into(),
                effort: Some("medium".into()),
            }],
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
            ..AgentManagerControlRequestV2::new(
                ManagerFenceV2 {
                    scope_version: config.row_version,
                    policy_version: grant.row_version,
                },
                key.into(),
                ManagerActionV2::SucceedManager {
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
            )
        }
    }
    fn proof(&self) -> VerifiedManagerHandoff {
        let source = self.store.get_session(self.owner).unwrap().unwrap();
        VerifiedManagerHandoff::from_authenticated_source(&source, &self.handoff, None).unwrap()
    }
    fn enqueue(&self, key: &str) -> ManagerActionReceiptV2 {
        self.store
            .enqueue_manager_succession(self.owner, &self.request(key), &self.proof())
            .unwrap()
    }
    fn claim(&self, receipt: &ManagerActionReceiptV2) -> ManagerSuccessionClaim {
        self.store
            .update_session_status(self.owner, SessionStatus::Completed)
            .unwrap();
        let action = self
            .store
            .claim_manager_action(Uuid::new_v4())
            .unwrap()
            .unwrap();
        assert_eq!(action.id(), receipt.operation_id);
        let claim = self.store.claim_manager_succession(&action).unwrap();
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
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
/// A resume wake armed for `owner`; `name` is its identity for the carry rules.
fn arm_resume(w: &World, owner: Uuid, name: &str) -> Uuid {
    let job = crate::store::manager_obligations::test_support::wake_row(
        owner,
        "resume",
        name,
        Some(600),
        None,
    );
    w.store.insert_scheduled_job(&job).unwrap();
    job.id
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
/// `(enabled, owner)` of a wake: rows are disabled or retargeted, never deleted.
fn wake_state(w: &World, id: Uuid) -> (bool, Option<Uuid>) {
    let job = w
        .store
        .get_scheduled_job(&id)
        .unwrap()
        .expect("a wake is disabled or retargeted, never deleted");
    (job.enabled, job.wake_session_id)
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
fn wakes_events(w: &World, operation: Uuid) -> Vec<serde_json::Value> {
    let mut statement = w
        .store
        .conn
        .prepare("SELECT payload_json FROM harness_manager_v2_events WHERE kind='succession_wakes_settled' AND record_key=?1")
        .unwrap();
    statement
        .query_map([operation.to_string()], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|payload| serde_json::from_str(&payload.unwrap()).unwrap())
        .collect()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
fn ids(value: &serde_json::Value) -> Vec<Uuid> {
    let mut out: Vec<Uuid> = serde_json::from_value(value.clone()).unwrap();
    out.sort();
    out
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
fn sorted(mut ids: Vec<Uuid>) -> Vec<Uuid> {
    ids.sort();
    ids
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
/// The stored receipt of the succession's journaled operation, read the way
/// `AgentManagerGetAction` reads it.
fn stored_receipt(w: &World, operation: Uuid) -> ManagerActionReceiptV2 {
    w.store
        .manager_action_operation(operation)
        .unwrap()
        .unwrap()
        .receipt
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn root_succession_cancels_the_seats_backstops_hands_over_obligations_and_names_the_ids() {
    use crate::store::manager_obligations::test_support as ob;
    let mut w = World::new();
    let mut epic = crate::store::tests::make_test_session();
    epic.project_id = Some(w.project);
    w.store.insert_session(&epic).unwrap();
    // Every durable obligation the seat can owe, its own generic keep-alive, a
    // protected manager watch, and two more ordinary backstops.
    let armed = ob::arm(&w.store, w.owner, Some((w.project, epic.id)));
    let net = arm_resume(&w, w.owner, "safety-net");
    let check_in = arm_resume(&w, w.owner, "check-in");
    let mut foreign_row = crate::store::tests::make_test_session();
    foreign_row.project_id = None;
    w.store.insert_session(&foreign_row).unwrap();
    let foreign = arm_resume(&w, foreign_row.id, "foreign-net");
    let backstops = sorted(vec![armed.keepalive, net, check_in]);

    // The seat's own ordinary wakes no longer refuse the handoff, and admission
    // leaves every one of them armed on the outgoing seat.
    let receipt = w.enqueue("with-wakes");
    assert_eq!(receipt.state, ManagerActionStateV2::Queued);
    assert_eq!(receipt.cancelled_wakes, None, "nothing is cancelled yet");
    for id in &backstops {
        assert_eq!(wake_state(&w, *id), (true, Some(w.owner)));
    }
    assert!(wakes_events(&w, receipt.operation_id).is_empty());

    let claim = w.claim(&receipt);
    let claim = w.admit(&claim);
    let claim = w.bound(&claim);
    let candidate = claim.reservation.candidate_session_id;
    let published = w
        .store
        .commit_manager_succession(&claim, &World::publication(&claim))
        .unwrap();
    assert_eq!(published.state, ManagerActionStateV2::Succeeded);

    // The caller's final receipt names the cancelled wakes and the reason.
    let named = published
        .cancelled_wakes
        .clone()
        .expect("the final receipt names the cancelled wakes");
    assert_eq!(named.reason, "manager_succession_displaced_seat");
    assert_eq!(named.job_ids, backstops);
    // Every later read of the action (AgentManagerGetAction) returns the same
    // receipt, and a metadata refresh keeps the ids.
    let operation = w
        .store
        .manager_action_operation(receipt.operation_id)
        .unwrap()
        .unwrap();
    assert_eq!(operation.receipt, published);
    let mut refreshed = operation.receipt.clone();
    refreshed.refresh_action_metadata(&operation.context.request.operation);
    assert_eq!(refreshed.cancelled_wakes, Some(named.clone()));

    // Effective effects, read back from the wake rows themselves: the
    // ordinary backstops are disabled (retained, not deleted) on the displaced seat ...
    for id in &backstops {
        assert_eq!(wake_state(&w, *id), (false, Some(w.owner)));
    }
    // ... while every durable obligation reached the successor, armed.
    for (label, id) in &armed.obligations {
        assert_eq!(
            wake_state(&w, *id),
            (true, Some(candidate)),
            "{label} is owned by the successor"
        );
    }
    assert_eq!(
        w.store
            .get_agent_job(armed.live_job)
            .unwrap()
            .unwrap()
            .job
            .owner_session_id,
        candidate
    );
    assert_eq!(
        w.store
            .get_agent_deploy(armed.live_deploy)
            .unwrap()
            .unwrap()
            .owner_session_id,
        Some(candidate)
    );
    // Protected manager obligations and a foreign seat's wakes are untouched.
    let protected = armed.protected.expect("a protected manager watch");
    assert_eq!(wake_state(&w, protected), (true, Some(w.owner)));
    assert_eq!(wake_state(&w, foreign), (true, Some(foreign_row.id)));
    assert!(!named.job_ids.contains(&protected));
    assert!(!named.job_ids.contains(&foreign));

    // The audit event holds the same lists.
    let events = wakes_events(&w, receipt.operation_id);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["reason"], "manager_succession_displaced_seat");
    assert_eq!(events[0]["predecessor_session_id"], json!(w.owner));
    assert_eq!(events[0]["successor_session_id"], json!(candidate));
    assert_eq!(ids(&events[0]["retired_wake_job_ids"]), backstops);
    let transferred = ids(&events[0]["transferred_wake_job_ids"]);
    for (label, id) in &armed.obligations {
        if *label != "terminal watch" {
            assert!(
                transferred.contains(id),
                "{label} is recorded as transferred"
            );
        }
    }
    assert!(backstops.iter().all(|id| !transferred.contains(id)));

    // A replayed publication changes nothing and records nothing more.
    assert!(
        w.store
            .commit_manager_succession(&claim, &World::publication(&claim))
            .is_err()
    );
    assert_eq!(wakes_events(&w, receipt.operation_id).len(), 1);
    assert_eq!(stored_receipt(&w, receipt.operation_id), published);
    assert_eq!(wake_state(&w, net), (false, Some(w.owner)));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn root_succession_without_seat_wakes_cancels_nothing_and_names_nothing() {
    let mut w = World::new();
    let receipt = w.enqueue("no-wakes");
    let claim = w.claim(&receipt);
    let claim = w.admit(&claim);
    let claim = w.bound(&claim);
    let published = w
        .store
        .commit_manager_succession(&claim, &World::publication(&claim))
        .unwrap();
    assert_eq!(published.state, ManagerActionStateV2::Succeeded);
    assert_eq!(published.cancelled_wakes, None);
    assert!(wakes_events(&w, receipt.operation_id).is_empty());
    let wire = serde_json::to_value(stored_receipt(&w, receipt.operation_id)).unwrap();
    assert!(wire.get("cancelled_wakes").is_none());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn automatic_rotation_still_carries_the_seats_backstops() {
    // The same seat change recorded as an automatic rotation (#2511) hands the
    // generic backstop to the successor; only an explicit succession cancels it.
    let mut w = World::new();
    let net = arm_resume(&w, w.owner, "safety-net");
    let receipt = w.enqueue("rotation-contrast");
    let claim = w.claim(&receipt);
    let claim = w.admit(&claim);
    let claim = w.bound(&claim);
    let candidate = claim.reservation.candidate_session_id;
    w.store
        .conn
        .execute(
            "UPDATE sessions SET status='Archived' WHERE id=?1",
            [w.owner.to_string()],
        )
        .unwrap();
    let tx = Transaction::new_unchecked(&w.store.conn, TransactionBehavior::Immediate).unwrap();
    assert!(Store::record_harness_manager_rotation_on(&tx, w.owner, candidate).unwrap());
    tx.commit().unwrap();
    assert_eq!(wake_state(&w, net), (true, Some(candidate)));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn root_succession_still_refuses_daemon_owned_resume_wakes_and_real_owners() {
    // Daemon-owned resume rows (a child keep-alive valve, a background-process
    // wake) are not the seat's own backstop: they still hold the handoff.
    for prefix in ["keepalive-", "background-process-"] {
        let w = World::new();
        let own = arm_resume(&w, w.owner, "safety-net");
        let mut daemon = crate::store::manager_obligations::test_support::wake_row(
            w.owner,
            "resume",
            "placeholder",
            None,
            None,
        );
        daemon.name = format!("{prefix}{}", w.owner);
        w.store.insert_scheduled_job(&daemon).unwrap();
        let operations = w.count("harness_manager_v2_operations");
        let error = w
            .store
            .enqueue_manager_succession(w.owner, &w.request("daemon-owned"), &w.proof())
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("manager_v2_human_or_recovery_owner"),
            "{error}"
        );
        assert_eq!(w.count("manager_root_successions"), 0);
        assert_eq!(w.count("harness_manager_v2_operations"), operations);
        assert_eq!(wake_state(&w, own), (true, Some(w.owner)));
        assert_eq!(wake_state(&w, daemon.id), (true, Some(w.owner)));
    }
    // A pending human question holds the handoff however many ordinary wakes the
    // seat owns: the wake exception is not a blanket one.
    let w = World::new();
    let own = arm_resume(&w, w.owner, "safety-net");
    w.store
        .conn
        .execute(
            "UPDATE sessions SET pending_question_json='{\"question\":\"approve?\"}' WHERE id=?1",
            [w.owner.to_string()],
        )
        .unwrap();
    let error = w
        .store
        .enqueue_manager_succession(w.owner, &w.request("human-question"), &w.proof())
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("manager_v2_human_or_recovery_owner"),
        "{error}"
    );
    assert_eq!(w.count("manager_root_successions"), 0);
    assert_eq!(wake_state(&w, own), (true, Some(w.owner)));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn root_succession_refused_for_disabled_rotation_leaves_the_seats_wakes_armed() {
    let w = World::new();
    let own = arm_resume(&w, w.owner, "safety-net");
    w.store.conn.execute("INSERT INTO daemon_settings(key,value,updated_at) VALUES('context_rotation_enabled','false',?1)",[now()]).unwrap();
    let error = w
        .store
        .enqueue_manager_succession(w.owner, &w.request("rotation-off"), &w.proof())
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("manager_succession_rotation_disabled"),
        "{error}"
    );
    assert_eq!(w.count("manager_root_successions"), 0);
    assert_eq!(wake_state(&w, own), (true, Some(w.owner)));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn overview_preflight_names_disabled_rotation_before_any_reservation_and_enabled_still_admits() {
    let w = World::new();
    let own = arm_resume(&w, w.owner, "safety-net");
    let config = w.store.get_harness_manager(w.project).unwrap().unwrap();
    let epoch = w.store.manager_authority_epoch(w.project).unwrap();
    // No persisted row and an explicit 'true' are both enabled.
    for value in [None, Some("true")] {
        if let Some(value) = value {
            w.store.conn.execute("INSERT OR REPLACE INTO daemon_settings(key,value,updated_at) VALUES('context_rotation_enabled',?1,?2)", params![value, now()]).unwrap();
        }
        assert!(matches!(
            w.store.manager_succession_preflight(&config).unwrap(),
            ManagerSuccessionPreflightV2::Eligible { .. }
        ));
    }
    w.store
        .conn
        .execute(
            "UPDATE daemon_settings SET value='false' WHERE key='context_rotation_enabled'",
            [],
        )
        .unwrap();
    assert_eq!(
        serde_json::to_value(w.store.manager_succession_preflight(&config).unwrap()).unwrap(),
        json!({
            "eligibility":"ineligible",
            "logical_manager_session_id":w.owner,
            "current_session_id":w.owner,
            "denial":{
                "reason":"context_rotation_disabled",
                "required_action":"operator_enable_context_rotation"
            }
        })
    );
    // Admission agrees with the readiness it advertised: refused before any
    // reservation, with the seat, epoch and wakes untouched.
    let error = w
        .store
        .enqueue_manager_succession(w.owner, &w.request("preflight-off"), &w.proof())
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("manager_succession_rotation_disabled"),
        "{error}"
    );
    assert_eq!(w.count("manager_root_successions"), 0);
    assert_eq!(w.store.manager_authority_epoch(w.project).unwrap(), epoch);
    assert_eq!(
        w.store
            .get_harness_manager(w.project)
            .unwrap()
            .unwrap()
            .current_session_id,
        Some(w.owner)
    );
    assert_eq!(wake_state(&w, own), (true, Some(w.owner)));
    // Re-enabled: readiness and normal admission both succeed.
    w.store
        .conn
        .execute(
            "UPDATE daemon_settings SET value='true' WHERE key='context_rotation_enabled'",
            [],
        )
        .unwrap();
    assert!(matches!(
        w.store.manager_succession_preflight(&config).unwrap(),
        ManagerSuccessionPreflightV2::Eligible { .. }
    ));
    let receipt = w.enqueue("preflight-on");
    assert_eq!(receipt.state, ManagerActionStateV2::Queued);
    assert_eq!(w.count("manager_root_successions"), 1);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn root_succession_publication_failure_cancels_nothing_and_names_nothing() {
    let mut w = World::new();
    let net = arm_resume(&w, w.owner, "safety-net");
    let check_in = arm_resume(&w, w.owner, "check-in");
    let receipt = w.enqueue("wakes-atomic");
    let claim = w.claim(&receipt);
    let claim = w.admit(&claim);
    let claim = w.bound(&claim);
    // The fault fires after the wakes were settled, in the same transaction.
    w.store.conn.execute_batch("CREATE TEMP TRIGGER wakes_test_abort BEFORE INSERT ON harness_manager_v2_events WHEN NEW.kind='action_result' BEGIN SELECT RAISE(ABORT,'publication fault'); END;").unwrap();
    let error = w
        .store
        .commit_manager_succession(&claim, &World::publication(&claim))
        .unwrap_err();
    assert!(error.to_string().contains("publication fault"), "{error}");
    for id in [net, check_in] {
        assert_eq!(wake_state(&w, id), (true, Some(w.owner)));
    }
    assert!(wakes_events(&w, receipt.operation_id).is_empty());
    let pending = stored_receipt(&w, receipt.operation_id);
    assert_eq!(pending.state, ManagerActionStateV2::Running);
    assert_eq!(pending.cancelled_wakes, None, "a failure names no wake");
    w.store
        .conn
        .execute_batch("DROP TRIGGER wakes_test_abort;")
        .unwrap();
    let published = w
        .store
        .commit_manager_succession(&claim, &World::publication(&claim))
        .unwrap();
    assert_eq!(
        published
            .cancelled_wakes
            .as_ref()
            .map(|w| w.job_ids.clone()),
        Some(sorted(vec![net, check_in]))
    );
    for id in [net, check_in] {
        assert_eq!(wake_state(&w, id), (false, Some(w.owner)));
    }
    assert_eq!(wakes_events(&w, receipt.operation_id).len(), 1);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
fn project_job(project: Uuid, owner: Uuid) -> crate::store::agent_jobs::NewAgentJob {
    let mut job = crate::store::manager_obligations::test_support::job(owner);
    job.project_id = Some(project);
    job
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
/// A second project with its own live manager seat: the unrelated manager.
fn unrelated_seat(w: &World) -> Uuid {
    let now = chrono::Utc::now();
    let project = Uuid::new_v4();
    w.store
        .insert_project(&Project {
            id: project,
            name: "unrelated manager".into(),
            path: Some(w.dir.path().join("other")),
            description: None,
            color: Project::DEFAULT_COLOR.into(),
            context_files: None,
            created_at: now,
            updated_at: now,
        })
        .unwrap();
    let mut session = crate::store::tests::make_test_session();
    session.project_id = Some(project);
    session.working_dir = w.dir.path().join("other");
    session.status = SessionStatus::Running;
    session.session_kind = SessionKind::Standard;
    session.parent_id = None;
    session.provider = SessionProvider::Codex;
    session.model = Some("gpt-6-astra".into());
    session.effort = Some("high".into());
    session.cost_usd = None;
    w.store.insert_session(&session).unwrap();
    w.store
        .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
            project_id: project,
            session_id: session.id,
            epic_ids: None,
            group_ids: vec![],
            expected_row_version: 0,
        })
        .unwrap();
    session.id
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
/// Two settled jobs of the seat's first owner, as the seat held them when its
/// successor was published: one succeeded, one failed.
fn settled_pair(w: &World) -> [Uuid; 2] {
    use rsi_common::agent_jobs::{AgentJobResultV1, JobState};
    [JobState::Succeeded, JobState::Failed].map(|state| {
        let job = project_job(w.project, w.owner);
        w.store.insert_agent_job(&job, chrono::Utc::now()).unwrap();
        w.store
            .settle_agent_job(
                job.id,
                state,
                &AgentJobResultV1::default(),
                false,
                chrono::Utc::now(),
            )
            .unwrap();
        job.id
    })
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
/// What `AgentGetJob`/`AgentListJobs` and a `jobs_terminal` arm decide for
/// `caller` over `ids`, as `(readable, listed, watchable)` per id.
fn observation(w: &World, caller: Uuid, ids: &[Uuid]) -> Vec<(bool, bool, bool)> {
    let listed: Vec<Uuid> = w
        .store
        .list_agent_jobs_for_reader(caller, 100)
        .unwrap()
        .into_iter()
        .map(|row| row.job.id)
        .collect();
    ids.iter()
        .map(|id| {
            (
                w.store
                    .get_agent_job_for_reader(caller, *id)
                    .unwrap()
                    .is_some(),
                listed.contains(id),
                w.store.job_watchable_by(caller, *id).unwrap(),
            )
        })
        .collect()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
fn arm_jobs_terminal(w: &World, owner: Uuid, name: &str, ids: &[Uuid]) -> crate::error::Result<()> {
    use crate::store::manager_obligations::test_support as ob;
    use rsi_common::wake_predicate::WakePredicate;
    let row = ob::wake_row(owner, "resume", name, None, None);
    w.store
        .insert_wake_when(
            &row,
            &ob::state(WakePredicate {
                jobs_terminal: Some(ids.to_vec()),
                ..WakePredicate::default()
            }),
            false,
        )
        .map(|_| ())
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn a_published_successor_observes_jobs_settled_before_publication_132() {
    // The seat's jobs settled before the successor existed: the original owner,
    // start and terminal receipt stay; the successor reads, lists and watches
    // them, an unrelated manager and the project's own workers learn nothing.
    let mut w = World::new();
    let settled = settled_pair(&w);
    let live = project_job(w.project, w.owner);
    w.store.insert_agent_job(&live, chrono::Utc::now()).unwrap();
    let before: Vec<_> = settled
        .iter()
        .map(|id| serde_json::to_value(&w.store.get_agent_job(*id).unwrap().unwrap().job).unwrap())
        .collect();
    let outsider = unrelated_seat(&w);

    let receipt = w.enqueue("job-observation");
    let claim = w.claim(&receipt);
    let claim = w.admit(&claim);
    let claim = w.bound(&claim);
    let successor = claim.reservation.candidate_session_id;
    w.store
        .commit_manager_succession(&claim, &World::publication(&claim))
        .unwrap();

    assert_eq!(
        observation(&w, successor, &settled),
        vec![(true, true, true); 2]
    );
    assert_eq!(
        observation(&w, outsider, &settled),
        vec![(false, false, false); 2]
    );
    // Ownership, identity and receipt of the settled jobs are untouched; the
    // live job followed the seat.
    let after: Vec<_> = settled
        .iter()
        .map(|id| serde_json::to_value(&w.store.get_agent_job(*id).unwrap().unwrap().job).unwrap())
        .collect();
    assert_eq!(before, after);
    assert!(
        after
            .iter()
            .all(|job| job["owner_session_id"] == json!(w.owner))
    );
    assert_eq!(
        w.store
            .get_agent_job(live.id)
            .unwrap()
            .unwrap()
            .job
            .owner_session_id,
        successor
    );
    // The watch is the successor's own: a mixed batch arms, the unrelated
    // manager is refused as for an unknown job.
    let batch = [settled[0], settled[1], live.id];
    arm_jobs_terminal(&w, successor, "mixed", &batch).unwrap();
    let refused = arm_jobs_terminal(&w, outsider, "mixed", &settled).unwrap_err();
    assert!(refused.to_string().contains("wake_when_job_not_found"));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn repeated_successions_keep_jobs_settled_under_any_earlier_seat_observable_132() {
    // Seat s0 -> s1 -> s2, each published rotation edge a hop; a job settled
    // under each seat is observable by the live seat and by no one else.
    let w = World::new();
    let outsider = unrelated_seat(&w);
    let mut seats = vec![w.owner];
    let mut settled = Vec::new();
    for hop in 0..3 {
        let seat = *seats.last().unwrap();
        let job = project_job(w.project, seat);
        w.store.insert_agent_job(&job, chrono::Utc::now()).unwrap();
        w.store
            .settle_agent_job(
                job.id,
                rsi_common::agent_jobs::JobState::Succeeded,
                &rsi_common::agent_jobs::AgentJobResultV1::default(),
                false,
                chrono::Utc::now(),
            )
            .unwrap();
        settled.push(job.id);
        if hop == 2 {
            break;
        }
        let mut next = w.store.get_session(seat).unwrap().unwrap();
        next.id = Uuid::new_v4();
        next.status = SessionStatus::Completed;
        next.continued_from = Some(seat);
        next.rotation_depth += 1;
        w.store.insert_session(&next).unwrap();
        w.store
            .update_session_status(seat, SessionStatus::Archived)
            .unwrap();
        assert!(
            w.store
                .record_harness_manager_rotation(seat, next.id)
                .unwrap()
        );
        seats.push(next.id);
    }
    let live_seat = seats[2];
    assert_eq!(
        observation(&w, live_seat, &settled),
        vec![(true, true, true); 3]
    );
    // A retired seat sees only what it owns; the unrelated manager nothing.
    assert_eq!(
        observation(&w, seats[1], &settled),
        vec![
            (false, false, false),
            (true, true, true),
            (false, false, false)
        ]
    );
    assert_eq!(
        observation(&w, outsider, &settled),
        vec![(false, false, false); 3]
    );
    arm_jobs_terminal(&w, live_seat, "all-hops", &settled).unwrap();
    assert!(arm_jobs_terminal(&w, outsider, "all-hops", &settled[..1]).is_err());
    assert!(arm_jobs_terminal(&w, seats[1], "stale", &settled[..1]).is_err());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
/// Publish the seat's successor through the real succession path and return it.
fn publish_successor(w: &mut World, key: &str) -> Uuid {
    let receipt = w.enqueue(key);
    let claim = w.claim(&receipt);
    let claim = w.admit(&claim);
    let claim = w.bound(&claim);
    w.store
        .commit_manager_succession(&claim, &World::publication(&claim))
        .unwrap();
    claim.reservation.candidate_session_id
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn a_predecessor_job_in_another_project_is_neither_readable_nor_watchable_132() {
    // The `project_matches` term of `seat_predecessor_job_observable`: a settled
    // job of a real predecessor of the seat, in a different project, is hidden
    // from the successor, while the same predecessor's same-project job is not.
    use rsi_common::agent_jobs::{AgentJobResultV1, JobState};
    let mut w = World::new();
    let outsider = unrelated_seat(&w);
    let foreign_project = w
        .store
        .get_session(outsider)
        .unwrap()
        .unwrap()
        .project_id
        .unwrap();
    assert_ne!(foreign_project, w.project);
    let successor = publish_successor(&mut w, "foreign-project-job");

    let settle = |job: &crate::store::agent_jobs::NewAgentJob| {
        w.store.insert_agent_job(job, chrono::Utc::now()).unwrap();
        w.store
            .settle_agent_job(
                job.id,
                JobState::Succeeded,
                &AgentJobResultV1::default(),
                false,
                chrono::Utc::now(),
            )
            .unwrap();
        job.id
    };
    let own = settle(&project_job(w.project, w.owner));
    let foreign = settle(&project_job(foreign_project, w.owner));
    assert_eq!(
        w.store
            .get_agent_job(foreign)
            .unwrap()
            .unwrap()
            .job
            .owner_session_id,
        w.owner,
        "the job is still owned by the real predecessor"
    );

    assert_eq!(observation(&w, successor, &[own]), vec![(true, true, true)]);
    let seen = observation(&w, successor, &[foreign]);
    assert_eq!((seen[0].0, seen[0].2), (false, false));
    let refused = arm_jobs_terminal(&w, successor, "foreign", &[foreign]).unwrap_err();
    assert!(refused.to_string().contains("wake_when_job_not_found"));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn a_queued_predecessor_job_is_readable_but_its_watch_is_refused_132() {
    // The `queued | running` branch of `job_watchable_by`: a live job still
    // owned by a predecessor (inserted after publication, so succession did
    // not move it) is readable by the successor but cannot be armed.
    use rsi_common::agent_jobs::JobState;
    let mut w = World::new();
    let successor = publish_successor(&mut w, "queued-predecessor-job");

    let queued = project_job(w.project, w.owner);
    w.store
        .insert_agent_job_in_state(&queued, chrono::Utc::now(), JobState::Queued)
        .unwrap();
    let row = w.store.get_agent_job(queued.id).unwrap().unwrap();
    assert_eq!(row.job.owner_session_id, w.owner);
    assert_eq!(row.job.state, JobState::Queued);

    assert!(
        w.store
            .get_agent_job_for_reader(successor, queued.id)
            .unwrap()
            .is_some(),
        "the successor reads a predecessor's live job"
    );
    assert!(!w.store.job_watchable_by(successor, queued.id).unwrap());
    let refused = arm_jobs_terminal(&w, successor, "queued", &[queued.id]).unwrap_err();
    assert!(refused.to_string().contains("wake_when_job_not_found"));
}
