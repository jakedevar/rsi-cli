//! Durable full-library QA obligations for bounded gates (#2092): bounded
//! library tests or bounded bin/integration targets (#2094), and every publish
//! gated on filters the queue derived (#2112, a `bounded_qa_obliged` event).
//! Reuse queue audit events and agent jobs; terminal queue rows stay immutable.

use super::*;
use crate::store::issues::{IssueWriteActor, issue_write_before_commit};
use crate::store::manager_notices::QaIssueNotice;
use rsi_common::types::NewIssue;

#[derive(Clone, Debug)]
pub struct BoundedQaLanding {
    pub entry: RollingQueueEntryV1,
    pub batch_id: Uuid,
    pub project_id: Option<Uuid>,
    pub repo_path: String,
    pub job_id: Option<Uuid>,
    pub qa_sha: Option<String>,
    /// Where the bound run executes (#2120); `None` before dispatch and for
    /// receipts written before #2120 (those are hub jobs).
    pub placement: Option<BoundedQaPlacement>,
    /// When the bound run was dispatched (#2120 receipts only).
    pub requested_at: Option<DateTime<Utc>>,
    /// #2155: a start recorded before it was sent whose binding never
    /// followed (the hub stopped, or the binding write failed, in between).
    /// Always `None` once `job_id` is set.
    pub intent: Option<BoundedQaIntent>,
}

/// The satellite start the hub recorded before sending it (#2155): the exact
/// peer, run id and commit. The satellite may have accepted the start, so an
/// unresolved intent is reconciled with that peer under that run id and is
/// never placed elsewhere.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoundedQaIntent {
    pub run_id: Uuid,
    pub sha: String,
    pub peer_id: Uuid,
    pub peer_label: String,
}

/// Where one full-library QA attempt runs (#2120). Serialized into the
/// `bounded_qa_requested` and `bounded_qa_finished` receipts as `ran_on` plus
/// its fields.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "ran_on", rename_all = "snake_case")]
pub enum BoundedQaPlacement {
    /// This daemon's own recipe job; `reason` says why no satellite took it.
    Hub { reason: String },
    /// A paired project satellite's recipe job, polled over its link.
    /// `job_id` in the receipt is the hub's run id; `remote_job_id` is the
    /// satellite's own job, `None` while an uncertain dispatch (a lost reply,
    /// #180) is unconfirmed: the run stays bound to this peer and run id.
    Satellite {
        peer_id: Uuid,
        peer_label: String,
        remote_job_id: Option<Uuid>,
    },
}

/// The settled attempt's evidence beside `passed` (#2120): where it ran, the
/// exit code, the failing tests and the job log, recorded on every covered
/// landing's `bounded_qa_finished` receipt.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BoundedQaRun {
    pub placement: Option<BoundedQaPlacement>,
    pub exit_code: Option<i32>,
    pub failing_tests: Vec<String>,
    pub log_path: Option<String>,
}

/// Most failing test names one finished receipt stores (the Issue body names
/// them all).
const RECEIPT_MAX_FAILING_TESTS: usize = 50;

/// `base` (a JSON object) with the placement's `ran_on` fields merged in.
fn with_placement(
    mut base: serde_json::Value,
    placement: Option<&BoundedQaPlacement>,
) -> Result<serde_json::Value> {
    let Some(placement) = placement else {
        return Ok(base);
    };
    let serde_json::Value::Object(fields) = serde_json::to_value(placement)? else {
        return Ok(base);
    };
    if let Some(object) = base.as_object_mut() {
        object.extend(fields);
    }
    Ok(base)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::types::Project;

    fn fixture() -> (Store, Vec<BoundedQaLanding>) {
        fixture_with(&[
            "gate_timing=selection=bounded selected=400 reached=7613",
            "gate_timing=selection=bounded selected=400 reached=7613",
            "gate_timing=selection=closure selected=1 reached=1",
        ])
    }

    /// One published landing per gate detail, in one batch.
    fn fixture_with(details: &[&str]) -> (Store, Vec<BoundedQaLanding>) {
        fixture_owned(details, None)
    }

    /// `fixture_with`, every entry enqueued by `owner` when given (else each
    /// by its own random session).
    fn fixture_owned(details: &[&str], owner: Option<Uuid>) -> (Store, Vec<BoundedQaLanding>) {
        let store = Store::open_in_memory().unwrap();
        let project = Uuid::new_v4();
        let now = Utc::now();
        store
            .insert_project(&Project {
                id: project,
                name: "bounded-qa-fixture".into(),
                path: None,
                description: None,
                color: Project::DEFAULT_COLOR.into(),
                context_files: None,
                created_at: now,
                updated_at: now,
            })
            .unwrap();
        let work = published_landings(&store, project, details, owner);
        (store, work)
    }

    /// One published landing per gate detail of `project`, in one batch.
    fn published_landings(
        store: &Store,
        project: Uuid,
        details: &[&str],
        owner: Option<Uuid>,
    ) -> Vec<BoundedQaLanding> {
        let now = Utc::now();
        for (index, source) in ['a', 'b', 'c'].into_iter().take(details.len()).enumerate() {
            store
                .enqueue_rolling_queue_source(
                    &NewQueueEntry {
                        project_id: Some(project),
                        repo_path: "/tmp/qa-repo".into(),
                        source_commit: source.to_string().repeat(40),
                        source_session_id: owner.unwrap_or_else(Uuid::new_v4),
                        owner_epic_id: None,
                        binding: rsi_common::rolling_queue::RollingQueueBinding::Unbound,
                        work_key: None,
                        migration_version: None,
                        hot_files: vec![],
                        test_filters: vec![],
                        fixes_tests: Vec::new(),
                        idempotency_key: format!("source-{index}"),
                    },
                    now,
                )
                .unwrap();
        }
        let batch = store
            .claim_rolling_queue_batch(now, details.len())
            .unwrap()
            .unwrap();
        let outcomes: Vec<_> = batch
            .entries
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                (
                    entry.id,
                    RollingQueueEntryState::Published,
                    RollingQueueOutcome {
                        landed_sha: Some("d".repeat(40)),
                        detail: Some(details[index].into()),
                        ..Default::default()
                    },
                )
            })
            .collect();
        store.settle_rolling_queue_entries(&outcomes, now).unwrap();
        store.bounded_qa_landings().unwrap()
    }

    /// #2743: a red full-library run files its `qa` Issue and the current
    /// manager seat hears of it at once (Issue, failing tests, rolling SHA);
    /// a replayed settle adds no second notice. The project's rolling health
    /// then lists the open Issue and the green SHA with its age, and closing
    /// the Issue clears it.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_red_run_notifies_the_manager_and_shows_in_the_rolling_health() {
        use crate::store::manager_coordinator::tests::fixture as manager_fixture;
        use rsi_common::harness_manager_v2::{ManagerCapabilityV2, ManagerPolicyV2};
        let store = Store::open_in_memory().unwrap();
        let (config, _lead) = manager_fixture(
            &store,
            ManagerPolicyV2 {
                capabilities: vec![ManagerCapabilityV2::WorkPlan],
                ..Default::default()
            },
        );
        let project = config.project_id;
        let work = published_landings(
            &store,
            project,
            &["gate_timing=selection=bounded selected=400 reached=7613"; 2],
            None,
        );
        let now = Utc::now();
        let (red_sha, green_sha) = ("e".repeat(40), "f".repeat(40));
        let health = store.rolling_health_for_project(project, now).unwrap();
        assert!(!health.is_red() && health.qa_green.is_none());

        let run = BoundedQaRun {
            exit_code: Some(100),
            failing_tests: vec!["store::tests::red".into(), "store::tests::red_too".into()],
            ..Default::default()
        };
        let red_job = Uuid::new_v4();
        let number = store
            .finish_bounded_qa(
                &work[..1],
                red_job,
                &red_sha,
                false,
                &run,
                Some(&issue(&work)),
                now,
            )
            .unwrap()
            .unwrap();
        // The same settle again is the restart replay: still one notice.
        let replayed = store
            .finish_bounded_qa(
                &work[..1],
                red_job,
                &red_sha,
                false,
                &run,
                Some(&issue(&work)),
                now,
            )
            .unwrap();
        assert_eq!(replayed, Some(number));
        let manager = config.current_session_id.unwrap();
        let (notices, _) = store
            .retrieve_manager_notices(&config, manager, true, 32, None)
            .unwrap();
        let subject = format!("qa_issue:{number}");
        let found: Vec<_> = notices.iter().filter(|n| n.subject_id == subject).collect();
        assert_eq!(found.len(), 1, "{notices:?}");
        let state = &found[0].state;
        assert_eq!(state["issue_number"], number);
        assert_eq!(state["rolling_sha"], red_sha);
        assert_eq!(state["outcome"], "test_red");
        assert_eq!(state["failing_tests"][0], "store::tests::red");
        assert_eq!(state["failing_tests_total"], 2);

        // A green run on a newer snapshot: qa_green moves, the red stays open.
        store
            .finish_bounded_qa(
                &work[1..2],
                Uuid::new_v4(),
                &green_sha,
                true,
                &BoundedQaRun::default(),
                None,
                now,
            )
            .unwrap();
        let later = now + chrono::Duration::hours(30);
        let health = store.rolling_health_for_project(project, later).unwrap();
        assert!(health.is_red());
        assert_eq!(
            health
                .open_qa_issues
                .iter()
                .map(|i| i.number)
                .collect::<Vec<_>>(),
            vec![number]
        );
        assert_eq!(health.qa_green.as_ref().unwrap().sha, green_sha);
        assert_eq!(health.qa_green_age_secs, Some(30 * 3600));
        let block = health.render_block();
        assert!(
            block.contains(&green_sha) && block.contains("30h00m"),
            "{block}"
        );
        assert!(block.contains(&format!("#{number}")), "{block}");
    }

    /// #2770/#2771: an overdue coverage-gap Issue reaches the live manager seat
    /// as a typed `qa_issue` notice (once, replay-safe), and the notice commits
    /// or rolls back with the Issue and its receipt, for the overdue report and
    /// for a red run alike: a failure before the commit loses neither.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn an_overdue_gap_notifies_the_manager_and_the_notice_commits_with_its_issue() {
        use crate::store::issues::{IssueWriteFault, fail_next_issue_write};
        use crate::store::manager_coordinator::tests::fixture as manager_fixture;
        use rsi_common::harness_manager_v2::{ManagerCapabilityV2, ManagerPolicyV2};
        let store = Store::open_in_memory().unwrap();
        let (config, _lead) = manager_fixture(
            &store,
            ManagerPolicyV2 {
                capabilities: vec![ManagerCapabilityV2::WorkPlan],
                ..Default::default()
            },
        );
        let project = config.project_id;
        let manager = config.current_session_id.unwrap();
        let work = published_landings(
            &store,
            project,
            &["gate_timing=selection=bounded selected=400 reached=7613"; 2],
            None,
        );
        // Read the recorded notices directly: `retrieve_manager_notices` hands
        // each notice over once.
        let notices = |subject_prefix: &str| -> Vec<serde_json::Value> {
            let mut statement = store
                .conn
                .prepare(
                    "SELECT state_json FROM harness_manager_notices
                     WHERE project_id=?1 AND manager_session_id=?2 AND subject_id LIKE ?3
                     ORDER BY sequence",
                )
                .unwrap();
            statement
                .query_map(
                    rusqlite::params![
                        project.to_string(),
                        manager.to_string(),
                        format!("{subject_prefix}%")
                    ],
                    |row| row.get::<_, String>(0),
                )
                .unwrap()
                .map(|state| serde_json::from_str(&state.unwrap()).unwrap())
                .collect()
        };
        let issue_count = || -> i64 {
            store
                .conn
                .query_row("SELECT count(*) FROM issues", [], |r| r.get(0))
                .unwrap()
        };

        // A failure before the commit rolls the Issue, the receipt and the
        // notice back together; the retry then records all three.
        fail_next_issue_write(IssueWriteFault::BeforeCommit);
        assert!(
            store
                .report_bounded_qa_overdue(&work[0], &issue(&work), Utc::now())
                .is_err()
        );
        assert_eq!(issue_count(), 0);
        assert!(notices("qa_issue:").is_empty());
        for _ in 0..2 {
            store
                .report_bounded_qa_overdue(&work[0], &issue(&work), Utc::now())
                .unwrap();
        }
        assert_eq!(issue_count(), 1);
        let found = notices("qa_issue:");
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0]["record_kind"], "qa_issue");
        assert_eq!(found[0]["outcome"], "coverage_incomplete");
        assert_eq!(found[0]["rolling_sha"], work[0].entry.source_commit);
        assert_eq!(found[0]["title"], "QA red for landing");

        // A red run: a failure before the commit loses neither the Issue nor
        // its notice; the retry files both.
        let run = BoundedQaRun {
            exit_code: Some(100),
            failing_tests: vec!["store::tests::red".into()],
            ..Default::default()
        };
        let (job, sha) = (Uuid::new_v4(), "e".repeat(40));
        let red = NewIssue {
            title: "Full library QA test red".into(),
            ..issue(&work[1..])
        };
        fail_next_issue_write(IssueWriteFault::BeforeCommit);
        assert!(
            store
                .finish_bounded_qa(&work[1..2], job, &sha, false, &run, Some(&red), Utc::now())
                .is_err()
        );
        assert_eq!(issue_count(), 1);
        assert_eq!(notices("qa_issue:").len(), 1);
        let number = store
            .finish_bounded_qa(&work[1..2], job, &sha, false, &run, Some(&red), Utc::now())
            .unwrap()
            .unwrap();
        let red_notice = notices(&format!("qa_issue:{number}"));
        assert_eq!(red_notice.len(), 1, "{red_notice:?}");
        assert_eq!(red_notice[0]["outcome"], "test_red");
        assert_eq!(notices("qa_issue:").len(), 2);
    }

    fn hub() -> BoundedQaPlacement {
        BoundedQaPlacement::Hub {
            reason: "no_satellite_peer".into(),
        }
    }

    /// #2120: a satellite placement is read back from the request receipt
    /// (so a restart polls the satellite, not a local job), and the finished
    /// receipt records where it ran, the exit code, the failing tests, the
    /// log and every covered landing.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_satellite_run_is_bound_and_settled_with_its_receipt() {
        let (store, work) = fixture();
        let run = Uuid::new_v4();
        let peer = Uuid::new_v4();
        let remote = Uuid::new_v4();
        let placement = BoundedQaPlacement::Satellite {
            peer_id: peer,
            peer_label: "laptop".into(),
            remote_job_id: Some(remote),
        };
        let now = Utc::now();
        store
            .request_bounded_qa(&work, run, &"d".repeat(40), &placement, now)
            .unwrap();
        let pending = store.bounded_qa_landings().unwrap();
        assert!(pending.iter().all(|w| w.job_id == Some(run)
            && w.placement.as_ref() == Some(&placement)
            && w.requested_at == Some(now)));
        let outcome = BoundedQaRun {
            placement: Some(placement),
            exit_code: Some(100),
            failing_tests: vec!["store::tests::red".into()],
            log_path: Some("/laptop/jobs/x.log".into()),
        };
        store
            .finish_bounded_qa(
                &pending,
                run,
                &"d".repeat(40),
                false,
                &outcome,
                Some(&issue(&work)),
                now,
            )
            .unwrap();
        assert!(store.bounded_qa_landings().unwrap().is_empty());
        let receipt: String = store
            .conn
            .query_row(
                "SELECT detail_json FROM rolling_queue_events
                 WHERE kind='bounded_qa_finished' ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let receipt: serde_json::Value = serde_json::from_str(&receipt).unwrap();
        assert_eq!(receipt["ran_on"], "satellite");
        assert_eq!(receipt["peer_id"], peer.to_string());
        assert_eq!(receipt["remote_job_id"], remote.to_string());
        assert_eq!(receipt["exit_code"], 100);
        assert_eq!(receipt["failing_tests"][0], "store::tests::red");
        assert_eq!(receipt["log_path"], "/laptop/jobs/x.log");
        for landing in &work {
            assert!(
                receipt["landings"]
                    .as_array()
                    .unwrap()
                    .contains(&serde_json::json!(landing.entry.id.to_string()))
            );
        }
    }

    /// #2120: the hub fallback records `ran_on: hub` and why.
    /// #180: an uncertain dispatch binds the peer and run with the satellite
    /// job still unknown, and reads back that way after a restart.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn an_unconfirmed_satellite_dispatch_keeps_its_peer_and_run() {
        let (store, work) = fixture();
        let run = Uuid::new_v4();
        let placement = BoundedQaPlacement::Satellite {
            peer_id: Uuid::new_v4(),
            peer_label: "laptop".into(),
            remote_job_id: None,
        };
        store
            .request_bounded_qa(&work, run, &"d".repeat(40), &placement, Utc::now())
            .unwrap();
        let pending = store.bounded_qa_landings().unwrap();
        assert_eq!(pending.len(), work.len());
        assert!(
            pending
                .iter()
                .all(|w| w.job_id == Some(run) && w.placement.as_ref() == Some(&placement))
        );
    }

    /// #2155: the peer, run id and commit of a satellite start are recorded
    /// before it is sent, so a hub that stops after the satellite accepted it
    /// finds the intent again. A refusal that proves a non-effect withdraws it
    /// (the next peer's intent then holds); the binding supersedes it, and a
    /// bound landing is never dispatched again.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_recorded_dispatch_intent_holds_until_it_is_withdrawn_or_bound() {
        let (store, work) = fixture();
        assert!(work.iter().all(|w| w.intent.is_none()));
        let (run, peer, other) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let sha = "d".repeat(40);
        let now = Utc::now();
        store
            .begin_bounded_qa_dispatch(&work, run, &sha, peer, "laptop", now)
            .unwrap();
        let expected = |peer_id: Uuid, label: &str| BoundedQaIntent {
            run_id: run,
            sha: sha.clone(),
            peer_id,
            peer_label: label.into(),
        };
        let held = store.bounded_qa_landings().unwrap();
        assert_eq!(held.len(), work.len());
        assert!(
            held.iter()
                .all(|w| w.job_id.is_none() && w.intent == Some(expected(peer, "laptop")))
        );
        store
            .withdraw_bounded_qa_dispatch(&work, run, peer, now)
            .unwrap();
        assert!(
            store
                .bounded_qa_landings()
                .unwrap()
                .iter()
                .all(|w| w.intent.is_none() && w.job_id.is_none())
        );
        store
            .begin_bounded_qa_dispatch(&work, run, &sha, other, "desktop", now)
            .unwrap();
        assert!(
            store
                .bounded_qa_landings()
                .unwrap()
                .iter()
                .all(|w| w.intent == Some(expected(other, "desktop")))
        );
        let placement = BoundedQaPlacement::Satellite {
            peer_id: other,
            peer_label: "desktop".into(),
            remote_job_id: None,
        };
        store
            .request_bounded_qa(&work, run, &sha, &placement, now)
            .unwrap();
        let bound = store.bounded_qa_landings().unwrap();
        assert!(
            bound
                .iter()
                .all(|w| w.job_id == Some(run) && w.intent.is_none())
        );
        assert!(
            store
                .begin_bounded_qa_dispatch(&work, Uuid::new_v4(), &sha, peer, "laptop", now)
                .is_err(),
            "a bound landing is never dispatched again"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_hub_run_records_its_fallback_reason() {
        let (store, work) = fixture();
        store
            .request_bounded_qa(&work, Uuid::new_v4(), &"d".repeat(40), &hub(), Utc::now())
            .unwrap();
        let pending = store.bounded_qa_landings().unwrap();
        assert_eq!(pending[0].placement, Some(hub()));
        let receipt: String = store
            .conn
            .query_row(
                "SELECT detail_json FROM rolling_queue_events
                 WHERE kind='bounded_qa_requested' ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let receipt: serde_json::Value = serde_json::from_str(&receipt).unwrap();
        assert_eq!(receipt["ran_on"], "hub");
        assert_eq!(receipt["reason"], "no_satellite_peer");
    }

    fn issue(work: &[BoundedQaLanding]) -> NewIssue {
        NewIssue {
            project_id: work[0].project_id.unwrap(),
            title: "QA red for landing".into(),
            body: format!("landing {}: test::red", work[0].entry.source_commit),
            priority: Some(1),
            labels: vec!["qa".into()],
            created_by_session_id: None,
            assignee: None,
            idea_id: None,
            source_event_id: None,
            source_finding_ref: None,
        }
    }

    /// The Issue a red run files for `tests`, in the qa format the filer writes.
    fn issue_naming(work: &[BoundedQaLanding], tests: &[String]) -> Option<NewIssue> {
        Some(NewIssue {
            project_id: work[0].project_id?,
            title: format!("QA red: {}", tests.join(", ")),
            body: format!(
                "Ran on the hub.\nFailing tests: {}\nRefusal: none",
                tests.join(", ")
            ),
            priority: Some(1),
            labels: vec!["qa".into(), "bug".into()],
            created_by_session_id: None,
            assignee: None,
            idea_id: None,
            source_event_id: None,
            source_finding_ref: None,
        })
    }

    /// One red settle of `tests` as job `job` on the landing `work[at]`.
    fn red_run(
        store: &Store,
        work: &[BoundedQaLanding],
        at: usize,
        job: Uuid,
        tests: &[&str],
    ) -> Option<i64> {
        let run = BoundedQaRun {
            exit_code: Some(100),
            failing_tests: tests.iter().map(|t| (*t).to_string()).collect(),
            ..Default::default()
        };
        let make = |named: &[String]| issue_naming(work, named);
        store
            .finish_bounded_qa_with(
                &work[at..=at],
                job,
                &"e".repeat(40),
                false,
                &run,
                Some(&make as &dyn Fn(&[String]) -> Option<NewIssue>),
                Utc::now(),
            )
            .unwrap()
    }

    fn all_issues(store: &Store, work: &[BoundedQaLanding]) -> Vec<rsi_common::types::Issue> {
        store
            .list_issues(&rsi_common::types::IssueFilter {
                project_id: work[0].project_id,
                ..Default::default()
            })
            .unwrap()
    }

    /// Hub #3054: a second red run of a test an open `qa` Issue already names
    /// files nothing and records the run on that Issue; a replay of the same
    /// job adds nothing more.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_second_red_run_of_a_tracked_test_files_nothing_and_records_the_run() {
        let (store, work) =
            fixture_with(&["gate_timing=selection=bounded selected=1 reached=1"; 2]);
        let first = red_run(
            &store,
            &work,
            0,
            Uuid::new_v4(),
            &["a::b::red", "a::b::red_2"],
        )
        .unwrap();
        let second_job = Uuid::new_v4();
        let second = red_run(&store, &work, 1, second_job, &["a::b::red"]).unwrap();
        assert_eq!(second, first, "the receipt names the tracking Issue");
        let issues = all_issues(&store, &work);
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert!(
            issues[0].body.contains(&second_job.to_string()),
            "{}",
            issues[0].body
        );
        assert!(issues[0].body.contains(&"e".repeat(40)));
        // The same job settled again appends nothing.
        let version = issues[0].row_version;
        red_run(&store, &work, 1, second_job, &["a::b::red"]);
        assert_eq!(all_issues(&store, &work)[0].row_version, version);
    }

    /// Hub #3054: one tracked and one new test files one Issue naming only the
    /// new test; the tracking Issue still records the run.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_run_with_one_tracked_and_one_new_test_files_only_the_new_one() {
        let (store, work) =
            fixture_with(&["gate_timing=selection=bounded selected=1 reached=1"; 2]);
        let first = red_run(&store, &work, 0, Uuid::new_v4(), &["a::tracked"]).unwrap();
        let job = Uuid::new_v4();
        let second = red_run(&store, &work, 1, job, &["a::new_one", "a::tracked"]).unwrap();
        assert_ne!(second, first);
        let issues = all_issues(&store, &work);
        assert_eq!(issues.len(), 2, "{issues:?}");
        let filed = issues.iter().find(|i| i.display_number == second).unwrap();
        let listed = rsi_common::rolling_queue::failing_tests_listed(&filed.body);
        assert_eq!(listed, vec!["a::new_one".to_string()], "{}", filed.body);
        let tracking = issues.iter().find(|i| i.display_number == first).unwrap();
        assert!(
            tracking.body.contains(&job.to_string()),
            "{}",
            tracking.body
        );
    }

    /// Hub #3054 (#3073): tests owned by two different open `qa` Issues, each
    /// naming only some of them, are together fully tracked: a run of their
    /// union files nothing and records the run on both Issues.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_run_of_tests_owned_by_two_open_issues_files_nothing_and_records_on_both() {
        let (store, work) =
            fixture_with(&["gate_timing=selection=bounded selected=1 reached=1"; 3]);
        let first = red_run(&store, &work, 0, Uuid::new_v4(), &["a::one", "a::two"]).unwrap();
        let second = red_run(&store, &work, 1, Uuid::new_v4(), &["b::three", "b::four"]).unwrap();
        assert_ne!(second, first);
        assert_eq!(all_issues(&store, &work).len(), 2);
        let job = Uuid::new_v4();
        let third = red_run(
            &store,
            &work,
            2,
            job,
            &["a::one", "b::three", "b::four", "a::two"],
        )
        .unwrap();
        let issues = all_issues(&store, &work);
        assert_eq!(issues.len(), 2, "no duplicate Issue: {issues:?}");
        assert!([first, second].contains(&third));
        for issue in &issues {
            assert!(
                issue.body.contains(&job.to_string()),
                "run recorded on #{}: {}",
                issue.display_number,
                issue.body
            );
        }
    }

    /// Hub #3054: a name that only contains a tracked name is not tracked.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_substring_match_does_not_count_as_tracked() {
        let (store, work) =
            fixture_with(&["gate_timing=selection=bounded selected=1 reached=1"; 2]);
        let first = red_run(&store, &work, 0, Uuid::new_v4(), &["a::a_test_2"]).unwrap();
        let second = red_run(&store, &work, 1, Uuid::new_v4(), &["a::a_test"]).unwrap();
        assert_ne!(second, first);
        assert_eq!(all_issues(&store, &work).len(), 2);
        // And the longer name is not covered by the shorter one's Issue either.
        let third = red_run(
            &store,
            &work,
            1,
            Uuid::new_v4(),
            &["a::a_test_2", "a::a_test"],
        )
        .unwrap();
        assert_eq!(all_issues(&store, &work).len(), 2, "both are tracked now");
        assert_eq!(third, first);
    }

    /// Hub #3054: a Closed `qa` Issue does not suppress filing.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_closed_qa_issue_does_not_suppress_filing() {
        let (store, work) =
            fixture_with(&["gate_timing=selection=bounded selected=1 reached=1"; 2]);
        let first = red_run(&store, &work, 0, Uuid::new_v4(), &["a::red"]).unwrap();
        let issues = all_issues(&store, &work);
        store
            .update_issue_status(issues[0].id, rsi_common::types::IssueStatus::Closed)
            .unwrap();
        let second = red_run(&store, &work, 1, Uuid::new_v4(), &["a::red"]).unwrap();
        assert_ne!(second, first);
        assert_eq!(all_issues(&store, &work).len(), 2);
    }

    /// #2094: a gate that ran every reached library test but bounded its
    /// bin and integration targets owes the same async full run.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn bounded_targets_alone_create_a_full_qa_obligation() {
        let (_store, work) = fixture_with(&[
            "gate_timing=selection=closure selected=3 reached=3 targets=bounded selected=5 reached=40",
            "gate_timing=selection=closure selected=3 reached=3",
        ]);
        assert_eq!(work.len(), 1);
        assert_eq!(work[0].entry.source_commit, "a".repeat(40));
    }

    /// #2286: a gate that excluded the released-migration tier over rsid or
    /// rsid-store library code (`slow_tier=excluded` in its evidence) owes the
    /// full run whatever its selection was.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn an_excluded_slow_tier_gate_over_store_code_is_listed_as_an_obligation() {
        let (_store, work) = fixture_with(&[
            "gate_timing=selection=closure reason=explicit_filters slow_tier=excluded",
            "gate_timing=selection=closure selected=1 reached=1 slow_tier=excluded",
            "gate_timing=selection=closure selected=1 reached=1",
        ]);
        assert_eq!(
            work.iter()
                .map(|w| w.entry.source_commit.clone())
                .collect::<Vec<_>>(),
            ["a".repeat(40), "b".repeat(40)]
        );
    }

    /// #2112: the lander reports queue-derived filters as explicit, so the
    /// queue's own `bounded_qa_obliged` receipt is what obliges the full run;
    /// a closure publish without it (operator explicit filters) owes nothing.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_queue_obligation_makes_an_explicit_filter_closure_publish_owe_the_full_run() {
        let explicit = "gate_timing=selection=closure reason=explicit_filters";
        let (store, work) = fixture_with(&[explicit, explicit]);
        assert!(work.is_empty());
        let row = |source: char| -> (Uuid, Uuid) {
            let (id, batch): (String, String) = store
                .conn
                .query_row(
                    "SELECT id, batch_id FROM rolling_queue_entries WHERE source_commit=?1",
                    [source.to_string().repeat(40)],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            (
                Uuid::parse_str(&id).unwrap(),
                Uuid::parse_str(&batch).unwrap(),
            )
        };
        let ((derived, batch_id), (operator, _)) = (row('a'), row('b'));
        for _ in 0..2 {
            store
                .oblige_bounded_qa(batch_id, &[derived], "derived_filters", Utc::now())
                .unwrap();
        }
        let work = store.bounded_qa_landings().unwrap();
        assert_eq!(
            work.iter().map(|w| w.entry.id).collect::<Vec<_>>(),
            [derived]
        );
        assert_ne!(work[0].entry.id, operator);
        let receipts: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM rolling_queue_events WHERE kind='bounded_qa_obliged'
                 AND json_extract(detail_json,'$.reason')='derived_filters'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(receipts, 1);
        // The gate outcome itself is never rewritten.
        assert_eq!(
            store
                .get_rolling_queue_entry(derived)
                .unwrap()
                .unwrap()
                .outcome
                .unwrap()
                .detail
                .as_deref(),
            Some(explicit)
        );
    }

    /// #2112: the minimum start interval is measured from the project's newest
    /// dispatch receipt, which names every landing the run covers.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn the_last_dispatch_is_the_newest_request_of_the_landings_project() {
        let (store, work) = fixture();
        let project = work[0].project_id;
        assert_eq!(store.last_bounded_qa_dispatch(project).unwrap(), None);
        let first = Utc::now() - chrono::Duration::minutes(90);
        store
            .request_bounded_qa(&work[..1], Uuid::new_v4(), &"d".repeat(40), &hub(), first)
            .unwrap();
        let second = first + chrono::Duration::minutes(30);
        store
            .request_bounded_qa(&work, Uuid::new_v4(), &"d".repeat(40), &hub(), second)
            .unwrap();
        assert_eq!(
            store.last_bounded_qa_dispatch(project).unwrap(),
            Some(second)
        );
        assert_eq!(
            store
                .last_bounded_qa_dispatch(Some(Uuid::new_v4()))
                .unwrap(),
            None
        );
        let covered: String = store
            .conn
            .query_row(
                "SELECT json_extract(detail_json,'$.landings') FROM rolling_queue_events
                 WHERE kind='bounded_qa_requested' ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        for landing in &work {
            assert!(covered.contains(&landing.entry.id.to_string()), "{covered}");
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn bounded_obligations_survive_dispatch_and_coalesce_without_rewriting_the_gate() {
        let (store, work) = fixture();
        assert_eq!(work.len(), 2);
        let job = Uuid::new_v4();
        for _ in 0..2 {
            store
                .request_bounded_qa(&work, job, &"d".repeat(40), &hub(), Utc::now())
                .unwrap();
        }
        // A fresh read has the persisted binding, so boot does not redispatch.
        let pending = store.bounded_qa_landings().unwrap();
        assert!(
            pending
                .iter()
                .all(|w| w.job_id == Some(job) && w.qa_sha.as_deref() == Some(&"d".repeat(40)))
        );
        let requests: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM rolling_queue_events WHERE kind='bounded_qa_requested'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(requests, 2);
        store
            .finish_bounded_qa(
                &pending,
                job,
                &"d".repeat(40),
                true,
                &BoundedQaRun::default(),
                None,
                Utc::now(),
            )
            .unwrap();
        assert!(store.bounded_qa_landings().unwrap().is_empty());
        let entry = store
            .get_rolling_queue_entry(work[0].entry.id)
            .unwrap()
            .unwrap();
        assert_eq!(entry.outcome, work[0].entry.outcome);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn bounded_qa_red_files_one_issue_and_one_receipt_per_landing_on_replay() {
        let (store, work) = fixture();
        let job = Uuid::new_v4();
        let issue = issue(&work);
        let run = BoundedQaRun::default();
        let first = store
            .finish_bounded_qa(
                &work,
                job,
                &"d".repeat(40),
                false,
                &run,
                Some(&issue),
                Utc::now(),
            )
            .unwrap();
        let replay = store
            .finish_bounded_qa(
                &work,
                job,
                &"d".repeat(40),
                false,
                &run,
                Some(&issue),
                Utc::now(),
            )
            .unwrap();
        assert_eq!(first, replay);
        let count: i64 = store
            .conn
            .query_row("SELECT count(*) FROM issues", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
        let receipts: i64 = store.conn.query_row("SELECT count(*) FROM rolling_queue_events WHERE kind='bounded_qa_finished' AND json_extract(detail_json,'$.passed')=0 AND json_extract(detail_json,'$.issue') IS NOT NULL", [], |r| r.get(0)).unwrap();
        assert_eq!(receipts, 2);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn bounded_qa_issue_and_receipts_roll_back_together() {
        let (store, work) = fixture();
        crate::store::issues::fail_next_issue_write(
            crate::store::issues::IssueWriteFault::BeforeCommit,
        );
        assert!(
            store
                .finish_bounded_qa(
                    &work,
                    Uuid::new_v4(),
                    &"d".repeat(40),
                    false,
                    &BoundedQaRun::default(),
                    Some(&issue(&work)),
                    Utc::now()
                )
                .is_err()
        );
        let count: i64 = store
            .conn
            .query_row("SELECT count(*) FROM issues", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
        assert_eq!(store.bounded_qa_landings().unwrap().len(), 2);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn bounded_qa_overdue_files_once_and_preserves_the_pending_obligation() {
        let (store, work) = fixture();
        for _ in 0..2 {
            store
                .report_bounded_qa_overdue(&work[0], &issue(&work), Utc::now())
                .unwrap();
        }
        let count: i64 = store
            .conn
            .query_row("SELECT count(*) FROM issues", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
        assert_eq!(store.bounded_qa_landings().unwrap().len(), 2);
    }

    /// #2149: a run that ended on capacity records one audit receipt per
    /// landing and no Issue; the landings are pending and unbound again, a new
    /// run binds them, and finishing that run closes the obligation.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_capacity_deferral_unbinds_the_landings_files_no_issue_and_replays_once() {
        let (store, work) = fixture();
        let sha = "d".repeat(40);
        let job = Uuid::new_v4();
        store
            .request_bounded_qa(&work, job, &sha, &hub(), Utc::now())
            .unwrap();
        let bound = store.bounded_qa_landings().unwrap();
        assert!(bound.iter().all(|w| w.job_id == Some(job)));
        let run = BoundedQaRun {
            placement: Some(hub()),
            exit_code: Some(124),
            failing_tests: vec![],
            log_path: Some("/jobs/qa.log".into()),
        };
        for _ in 0..2 {
            store
                .defer_bounded_qa(
                    &bound,
                    job,
                    &sha,
                    "job_admission_timed_out",
                    &run,
                    Utc::now(),
                )
                .unwrap();
        }
        let pending = store.bounded_qa_landings().unwrap();
        assert_eq!(pending.len(), 2);
        assert!(
            pending
                .iter()
                .all(|w| w.job_id.is_none() && w.qa_sha.is_none())
        );
        let count = |sql: &str| -> i64 { store.conn.query_row(sql, [], |r| r.get(0)).unwrap() };
        assert_eq!(count("SELECT count(*) FROM issues"), 0);
        assert_eq!(
            count("SELECT count(*) FROM rolling_queue_events WHERE kind='bounded_qa_finished'"),
            0
        );
        assert_eq!(
            count(
                "SELECT count(*) FROM rolling_queue_events WHERE kind='bounded_qa_capacity_deferred'"
            ),
            2,
            "one receipt per landing, however often it replays"
        );
        let receipt: String = store
            .conn
            .query_row(
                "SELECT detail_json FROM rolling_queue_events
                 WHERE kind='bounded_qa_capacity_deferred' ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let receipt: serde_json::Value = serde_json::from_str(&receipt).unwrap();
        assert_eq!(receipt["refusal"], "job_admission_timed_out");
        assert_eq!(receipt["job_id"], job.to_string());
        assert_eq!(receipt["ran_on"], "hub");
        assert_eq!(receipt["log_path"], "/jobs/qa.log");
        for landing in &work {
            assert!(
                receipt["landings"]
                    .as_array()
                    .unwrap()
                    .contains(&serde_json::json!(landing.entry.id.to_string()))
            );
            assert_eq!(
                store
                    .bounded_qa_capacity_deferrals(landing.entry.id)
                    .unwrap(),
                1
            );
        }
        // The next attempt binds the same landings to a new run.
        let next = Uuid::new_v4();
        store
            .request_bounded_qa(&pending, next, &sha, &hub(), Utc::now())
            .unwrap();
        let rebound = store.bounded_qa_landings().unwrap();
        assert!(rebound.iter().all(|w| w.job_id == Some(next)));
        // A stale replay of the first run's deferral does not unbind the new run.
        store
            .defer_bounded_qa(
                &bound,
                job,
                &sha,
                "job_admission_timed_out",
                &run,
                Utc::now(),
            )
            .unwrap();
        assert!(
            store
                .bounded_qa_landings()
                .unwrap()
                .iter()
                .all(|w| w.job_id == Some(next))
        );
        store
            .finish_bounded_qa(&rebound, next, &sha, true, &run, None, Utc::now())
            .unwrap();
        assert!(store.bounded_qa_landings().unwrap().is_empty());
    }

    /// #2149 with #2155: a capacity deferral retires the finished attempt's
    /// recorded dispatch intent as well as its binding, so it is never
    /// reconciled again, while the next attempt's intent (recorded after the
    /// deferral) is accepted and is the one that holds the landings.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_capacity_deferral_retires_the_old_dispatch_intent_and_admits_a_new_one() {
        let (store, work) = fixture();
        let (first, second, peer) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let sha = "d".repeat(40);
        let placement = BoundedQaPlacement::Satellite {
            peer_id: peer,
            peer_label: "laptop".into(),
            remote_job_id: None,
        };
        let now = Utc::now();
        store
            .begin_bounded_qa_dispatch(&work, first, &sha, peer, "laptop", now)
            .unwrap();
        store
            .request_bounded_qa(&work, first, &sha, &placement, now)
            .unwrap();
        let bound = store.bounded_qa_landings().unwrap();
        assert!(bound.iter().all(|w| w.job_id == Some(first)));
        store
            .defer_bounded_qa(
                &bound,
                first,
                &sha,
                "job_admission_timed_out",
                &BoundedQaRun::default(),
                now,
            )
            .unwrap();
        let pending = store.bounded_qa_landings().unwrap();
        assert!(
            pending
                .iter()
                .all(|w| w.job_id.is_none() && w.intent.is_none()),
            "the deferred attempt's intent is not resurrected"
        );
        // The landings are free to be dispatched again, and the new intent holds.
        store
            .begin_bounded_qa_dispatch(&pending, second, &sha, peer, "laptop", now)
            .unwrap();
        assert!(
            store
                .bounded_qa_landings()
                .unwrap()
                .iter()
                .all(|w| w.job_id.is_none()
                    && w.intent.as_ref().map(|intent| intent.run_id) == Some(second))
        );
        // Binding that attempt supersedes its intent; a bound landing is never
        // dispatched again.
        store
            .request_bounded_qa(&pending, second, &sha, &placement, now)
            .unwrap();
        assert!(
            store
                .bounded_qa_landings()
                .unwrap()
                .iter()
                .all(|w| w.job_id == Some(second) && w.intent.is_none())
        );
        assert!(
            store
                .begin_bounded_qa_dispatch(&pending, Uuid::new_v4(), &sha, peer, "laptop", now)
                .is_err()
        );
    }

    /// #2747: a red full-library run is "untracked" once nothing open tracks
    /// it; an open Issue, or a later green run, clears it.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_red_receipt_with_no_open_issue_is_untracked_2747() {
        let (store, work) =
            fixture_with(&["gate_timing=selection=bounded selected=400 reached=7613"; 2]);
        let project = work[0].project_id;
        let now = Utc::now();
        assert_eq!(store.untracked_red_bounded_qa(project).unwrap(), None);
        let sha = "d".repeat(40);
        let run = BoundedQaRun::default();
        // Red with its Issue open: tracked.
        let number = store
            .finish_bounded_qa(
                &work[..1],
                Uuid::new_v4(),
                &sha,
                false,
                &run,
                Some(&issue(&work)),
                now,
            )
            .unwrap()
            .expect("the red run filed its Issue");
        assert_eq!(store.untracked_red_bounded_qa(project).unwrap(), None);
        // Its Issue is closed while the failure stands: untracked.
        store
            .conn
            .execute(
                "UPDATE issues SET status='Closed' WHERE project_id=?1 AND display_number=?2",
                params![project.unwrap().to_string(), number],
            )
            .unwrap();
        let (found_sha, found_at) = store
            .untracked_red_bounded_qa(project)
            .unwrap()
            .expect("closed Issue leaves the red untracked");
        assert_eq!(found_sha, sha);
        assert!((found_at - now).num_seconds().abs() < 5);
        // Another project's scope sees nothing.
        assert_eq!(
            store
                .untracked_red_bounded_qa(Some(Uuid::new_v4()))
                .unwrap(),
            None
        );
        // #2788: a set narrows the same way; only a set holding the receipt's
        // project (or no narrowing at all) sees it, and an empty set none.
        let covered = [Uuid::new_v4(), project.unwrap()];
        assert!(
            store
                .untracked_red_bounded_qa_in(Some(&covered))
                .unwrap()
                .is_some()
        );
        assert!(store.untracked_red_bounded_qa_in(None).unwrap().is_some());
        assert_eq!(
            store
                .untracked_red_bounded_qa_in(Some(&[Uuid::new_v4(), Uuid::new_v4()]))
                .unwrap(),
            None
        );
        assert_eq!(store.untracked_red_bounded_qa_in(Some(&[])).unwrap(), None);
        // A later green run supersedes it.
        store
            .finish_bounded_qa(&work[1..2], Uuid::new_v4(), &sha, true, &run, None, now)
            .unwrap();
        assert_eq!(store.untracked_red_bounded_qa(project).unwrap(), None);
        // A red run that filed no Issue at all is untracked.
        let (store, work) =
            fixture_with(&["gate_timing=selection=bounded selected=400 reached=7613"; 2]);
        store
            .finish_bounded_qa(&work[..1], Uuid::new_v4(), &sha, false, &run, None, now)
            .unwrap();
        assert!(store.untracked_red_bounded_qa(None).unwrap().is_some());
    }

    fn close_issue(store: &Store, work: &[BoundedQaLanding], number: i64) {
        store
            .conn
            .execute(
                "UPDATE issues SET status='Closed' WHERE project_id=?1 AND display_number=?2",
                params![work[0].project_id.unwrap().to_string(), number],
            )
            .unwrap();
    }

    /// Hub #3083: a red covered by two open Issues stays tracked when the
    /// receipt's Issue (the lower number) is closed as a duplicate; it is
    /// untracked only once no open Issue lists one of the failing tests, and
    /// a newer passing run supersedes it.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_duplicate_closing_beside_its_open_twin_leaves_the_red_tracked_3083() {
        let (store, work) =
            fixture_with(&["gate_timing=selection=bounded selected=1 reached=1"; 3]);
        let project = work[0].project_id;
        let first = red_run(&store, &work, 0, Uuid::new_v4(), &["a::one", "b::two"]).unwrap();
        let twin = red_run(&store, &work, 1, Uuid::new_v4(), &["c::three"]).unwrap();
        assert_ne!(first, twin);
        // The twin also lists the first Issue's tests (it duplicates them).
        store
            .conn
            .execute(
                "UPDATE issues SET body=body||char(10)||'Failing tests: a::one, b::two'
                 WHERE project_id=?1 AND display_number=?2",
                params![project.unwrap().to_string(), twin],
            )
            .unwrap();
        // The newest red covers the union; its receipt names the lowest Issue.
        let receipt = red_run(
            &store,
            &work,
            2,
            Uuid::new_v4(),
            &["a::one", "b::two", "c::three"],
        )
        .unwrap();
        assert_eq!(receipt, first);
        assert_eq!(store.untracked_red_bounded_qa(project).unwrap(), None);
        // The receipt's Issue closes as a duplicate: the twin still owns all.
        close_issue(&store, &work, first);
        assert_eq!(store.untracked_red_bounded_qa(project).unwrap(), None);
        // A named test with no open owner is untracked.
        store
            .conn
            .execute(
                "UPDATE issues SET body='no tests here' WHERE project_id=?1 AND display_number=?2",
                params![project.unwrap().to_string(), twin],
            )
            .unwrap();
        assert!(store.untracked_red_bounded_qa(project).unwrap().is_some());
        // Another project's scope still sees nothing.
        assert_eq!(
            store
                .untracked_red_bounded_qa(Some(Uuid::new_v4()))
                .unwrap(),
            None
        );
    }

    /// Hub #3083: a closed owner whose tests a published entry claims fixed
    /// (after the run) beside a still-open owner of the rest leaves nothing
    /// untracked; a fix published before the run, or one that claims only some
    /// of the tests, does not.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_published_fix_beside_an_open_owner_leaves_the_red_tracked_3083() {
        let (store, work) =
            fixture_with(&["gate_timing=selection=bounded selected=1 reached=1"; 3]);
        let project = work[0].project_id;
        let starter = red_run(
            &store,
            &work,
            0,
            Uuid::new_v4(),
            &["s::one", "s::two", "s::three"],
        )
        .unwrap();
        let delegation = red_run(&store, &work, 1, Uuid::new_v4(), &["d::vm_cost"]).unwrap();
        assert_ne!(starter, delegation);
        let receipt = red_run(
            &store,
            &work,
            2,
            Uuid::new_v4(),
            &["s::one", "s::two", "s::three", "d::vm_cost"],
        )
        .unwrap();
        assert_eq!(receipt, starter);
        // The starter Issue closes with no fix published: its tests are bare.
        close_issue(&store, &work, starter);
        assert!(store.untracked_red_bounded_qa(project).unwrap().is_some());
        let publish_fix = |key: &str, claims: &[&str], finished: DateTime<Utc>| {
            let entry = store
                .enqueue_rolling_queue_source(
                    &NewQueueEntry {
                        project_id: project,
                        repo_path: "/tmp/qa-repo".into(),
                        source_commit: format!("{:0<40}", key.len()),
                        source_session_id: Uuid::new_v4(),
                        owner_epic_id: None,
                        binding: rsi_common::rolling_queue::RollingQueueBinding::Unbound,
                        work_key: None,
                        migration_version: None,
                        hot_files: vec![],
                        test_filters: vec![],
                        fixes_tests: claims.iter().map(|c| (*c).to_string()).collect(),
                        idempotency_key: key.into(),
                    },
                    Utc::now(),
                )
                .unwrap();
            store
                .conn
                .execute(
                    "UPDATE rolling_queue_entries SET state='published', finished_at=?2 WHERE id=?1",
                    params![entry.0.id.to_string(), stamp(finished)],
                )
                .unwrap();
        };
        let (before, after) = (
            Utc::now() - chrono::Duration::hours(1),
            Utc::now() + chrono::Duration::minutes(5),
        );
        // A fix published before the run cannot be the answer to it.
        publish_fix("fix-old", &["s::one", "s::two", "s::three"], before);
        assert!(store.untracked_red_bounded_qa(project).unwrap().is_some());
        // A partial fix after the run leaves a test with no owner.
        publish_fix("fix-partial", &["s::one", "s::two"], after);
        assert!(store.untracked_red_bounded_qa(project).unwrap().is_some());
        // The rest of the claims arrive; the open delegation Issue owns its test.
        publish_fix("fix-rest", &["s::three"], after);
        assert_eq!(store.untracked_red_bounded_qa(project).unwrap(), None);
        // The delegation Issue closes with no fix: its test is uncovered.
        close_issue(&store, &work, delegation);
        assert!(store.untracked_red_bounded_qa(project).unwrap().is_some());
    }

    /// #2646: the latest full-library-green rolling SHA is one read away,
    /// from the queue's own passing receipts: a red run never moves it, the
    /// newest pass wins, and a caller outside the entries' scope sees none.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn the_latest_green_full_library_sha_reads_back_from_passing_receipts() {
        let caller = Uuid::new_v4();
        let (store, work) = fixture_owned(
            &["gate_timing=selection=bounded selected=400 reached=7613"; 2],
            Some(caller),
        );
        assert!(work.len() >= 2);
        let now = Utc::now();
        assert_eq!(
            store.rolling_qa_green_for_agent(caller, None).unwrap(),
            None
        );
        let (first, second) = ("e".repeat(40), "f".repeat(40));
        let (red_job, green_job, newer_job) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let run = BoundedQaRun::default();
        store
            .finish_bounded_qa(
                &work[..1],
                red_job,
                &first,
                false,
                &run,
                Some(&issue(&work)),
                now,
            )
            .unwrap();
        assert_eq!(
            store.rolling_qa_green_for_agent(caller, None).unwrap(),
            None,
            "a red run is not green"
        );
        store
            .finish_bounded_qa(&work[1..2], green_job, &first, true, &run, None, now)
            .unwrap();
        let green = store
            .rolling_qa_green_for_agent(caller, None)
            .unwrap()
            .unwrap();
        assert_eq!(
            (green.sha.as_str(), green.job_id),
            (first.as_str(), Some(green_job))
        );
        // A later passing run on a newer snapshot replaces it.
        let (store, work) = fixture_owned(
            &["gate_timing=selection=bounded selected=400 reached=7613"; 2],
            Some(caller),
        );
        store
            .finish_bounded_qa(&work[..1], green_job, &first, true, &run, None, now)
            .unwrap();
        store
            .finish_bounded_qa(&work[1..2], newer_job, &second, true, &run, None, now)
            .unwrap();
        let green = store
            .rolling_qa_green_for_agent(caller, None)
            .unwrap()
            .unwrap();
        assert_eq!(
            (green.sha.as_str(), green.job_id),
            (second.as_str(), Some(newer_job))
        );
        assert_eq!(
            store
                .rolling_qa_green_for_agent(Uuid::new_v4(), None)
                .unwrap(),
            None,
            "a session that owns none of the entries reads nothing"
        );
    }
}

impl Store {
    /// #2747: the newest full-library QA receipt of `rolling` (narrowed to
    /// `project`) when it is red and nothing tracks it, as
    /// `(tested sha, finished at)`. A later passing receipt supersedes a red
    /// one. A red is tracked while the receipt's own Issue is open, or (hub
    /// #3083) while every failing test the receipt names is owned: listed on a
    /// `Failing tests:` or `Test:` line of an open Issue of the project, or
    /// claimed fixed (`fixes_test`) by an entry published after the tested
    /// run was dispatched. A duplicate closed beside its surviving twin, or a
    /// fix published beside a still-open owner, therefore leaves nothing
    /// untracked; a closed Issue with no owner left, an unlisted test, or a
    /// receipt that cannot name its tests (none recorded, or cut at the
    /// receipt cap) stays untracked.
    pub fn untracked_red_bounded_qa(
        &self,
        project: Option<Uuid>,
    ) -> Result<Option<(String, DateTime<Utc>)>> {
        self.untracked_red_bounded_qa_in(project.as_ref().map(std::slice::from_ref))
    }

    /// [`Self::untracked_red_bounded_qa`] narrowed to a set of projects
    /// (#2788): `None` = every project, `Some(&[])` = none, so a receipt of a
    /// project outside the set is never the newest one considered.
    pub fn untracked_red_bounded_qa_in(
        &self,
        projects: Option<&[Uuid]>,
    ) -> Result<Option<(String, DateTime<Utc>)>> {
        let projects_json = projects
            .map(|set| serde_json::to_string(&set.iter().map(Uuid::to_string).collect::<Vec<_>>()))
            .transpose()?;
        let newest = self
            .conn
            .query_row(
                "SELECT e.at, COALESCE(json_extract(e.detail_json,'$.passed'),0),
                        json_extract(e.detail_json,'$.sha'), json_extract(e.detail_json,'$.issue'),
                        q.project_id, json_extract(e.detail_json,'$.failing_tests'),
                        json_extract(e.detail_json,'$.failing_tests_total'),
                        json_extract(e.detail_json,'$.job_id')
                 FROM rolling_queue_events e JOIN rolling_queue_entries q ON q.id=e.entry_id
                 WHERE e.kind='bounded_qa_finished'
                   AND (?1 IS NULL OR q.project_id IN (SELECT value FROM json_each(?1)))
                 ORDER BY e.id DESC LIMIT 1",
                params![projects_json],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, Option<i64>>(6)?,
                        row.get::<_, Option<String>>(7)?,
                    ))
                },
            )
            .optional()?;
        let Some((at, passed, sha, issue, entry_project, tests, total, job)) = newest else {
            return Ok(None);
        };
        let (Ok(finished_at), Some(sha)) = (DateTime::parse_from_rfc3339(&at), sha) else {
            return Ok(None);
        };
        if passed != 0 {
            return Ok(None);
        }
        let finished_at = finished_at.with_timezone(&Utc);
        let project = entry_project.and_then(|text| Uuid::parse_str(&text).ok());
        if let (Some(number), Some(project)) =
            (issue.and_then(|number| u32::try_from(number).ok()), project)
            && matches!(
                self.issue_status_by_number(project, number)?,
                Some(
                    rsi_common::types::IssueStatus::Open
                        | rsi_common::types::IssueStatus::InProgress
                )
            )
        {
            return Ok(None);
        }
        // The receipt's own Issue does not track it. The red is still tracked
        // when each failing test has another owner (hub #3083).
        let failing: Vec<String> = tests
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default();
        let complete = !failing.is_empty()
            && total.is_none_or(|total| usize::try_from(total).is_ok_and(|n| n <= failing.len()));
        if let (true, Some(project)) = (complete, project)
            && self.every_failing_test_owned(project, &failing, job.as_deref(), finished_at)?
        {
            return Ok(None);
        }
        Ok(Some((sha, finished_at)))
    }

    /// Hub #3083: whether each of `failing` (a red QA receipt's tests, whole
    /// names) is owned in `project`: listed by an open (`Open`/`InProgress`,
    /// unarchived) Issue's `Failing tests:` or `Test:` line, the format
    /// `fixes_issue` and the #3054 union already read, or claimed in
    /// `fixes_test` by a published queue entry that finished at or after the
    /// run was dispatched (the gate refuses a claim that is still red, so a
    /// publication after dispatch is a fix the tested snapshot cannot carry).
    /// Free prose is never read.
    fn every_failing_test_owned(
        &self,
        project: Uuid,
        failing: &[String],
        job_id: Option<&str>,
        finished_at: DateTime<Utc>,
    ) -> Result<bool> {
        let project_text = project.to_string();
        let mut owned: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut bodies = self.conn.prepare(
            "SELECT body FROM issues
             WHERE project_id=?1 AND status IN ('Open','InProgress') AND archived_at IS NULL
               AND (instr(body,'Failing tests:')>0 OR instr(body,'Test:')>0)",
        )?;
        for body in bodies.query_map([&project_text], |row| row.get::<_, String>(0))? {
            owned.extend(rsi_common::rolling_queue::failing_tests_listed(&body?));
        }
        if failing.iter().all(|name| owned.contains(name)) {
            return Ok(true);
        }
        let dispatched: Option<String> = match job_id {
            Some(job) => self.conn.query_row(
                "SELECT MIN(at) FROM rolling_queue_events
                 WHERE kind='bounded_qa_requested' AND json_extract(detail_json,'$.job_id')=?1",
                [job],
                |row| row.get(0),
            )?,
            None => None,
        };
        let since = dispatched
            .and_then(|at| DateTime::parse_from_rfc3339(&at).ok())
            .map_or(finished_at, |at| at.with_timezone(&Utc));
        let mut entries = self.conn.prepare(
            "SELECT test_filters_json, finished_at FROM rolling_queue_entries
             WHERE project_id=?1 AND state='published' AND finished_at IS NOT NULL
               AND instr(test_filters_json, ?2)>0",
        )?;
        let rows = entries.query_map(
            params![
                project_text,
                rsi_common::rolling_queue::FIX_CLAIM_FILTER_PREFIX
            ],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )?;
        for row in rows {
            let (filters, published) = row?;
            let after = DateTime::parse_from_rfc3339(&published)
                .is_ok_and(|at| at.with_timezone(&Utc) >= since);
            if after && let Ok(stored) = serde_json::from_str::<Vec<String>>(&filters) {
                owned.extend(rsi_common::rolling_queue::split_fix_claims(stored).1);
            }
        }
        Ok(failing.iter().all(|name| owned.contains(name)))
    }

    /// An overdue receipt reports a coverage gap without retiring it. Once
    /// admission recovers the same pending landing is still dispatched.
    pub fn report_bounded_qa_overdue(
        &self,
        landing: &BoundedQaLanding,
        new_issue: &NewIssue,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM rolling_queue_events WHERE entry_id=?1 AND kind='bounded_qa_overdue')",
            [landing.entry.id.to_string()], |row| row.get(0),
        )?;
        if !exists {
            let issue = Self::idempotent_issue_in_tx(
                &tx,
                Uuid::new_v5(&landing.entry.id, b"bounded-library-qa-overdue"),
                new_issue,
                IssueWriteActor::System {
                    label: "rsi:bounded-library-qa".into(),
                },
            )?
            .issue
            .display_number;
            record_event(
                &tx,
                landing.batch_id,
                "bounded_qa_overdue",
                Some(landing.entry.id),
                Some(serde_json::json!({"issue":issue})),
                now,
            )?;
            // #2770: the overdue coverage gap is a `qa` Issue too, so the live
            // manager seat hears of it like a red or incomplete run (the
            // receipt above makes this once per landing; the notice subject
            // is the Issue, so a replay adds nothing).
            if let Some(project) = landing.project_id {
                self.record_qa_issue_notice_in_tx(
                    project,
                    &QaIssueNotice {
                        issue_number: issue,
                        title: new_issue.title.clone(),
                        outcome: "coverage_incomplete",
                        rolling_sha: landing.entry.source_commit.clone(),
                        failing_tests: Vec::new(),
                    },
                )?;
            }
            issue_write_before_commit()?;
        }
        tx.commit()?;
        Ok(())
    }
    /// #2112: record that each published `entries` member owes the async
    /// full-library run (`reason` names why, e.g. `derived_filters`). The
    /// queue writes it before the members settle, so a crash in between keeps
    /// the obligation; a member that never publishes is never discovered.
    pub fn oblige_bounded_qa(
        &self,
        batch_id: Uuid,
        entries: &[Uuid],
        reason: &str,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        for entry in entries {
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM rolling_queue_events
                 WHERE entry_id=?1 AND kind='bounded_qa_obliged')",
                [entry.to_string()],
                |row| row.get(0),
            )?;
            if !exists {
                record_event(
                    &tx,
                    batch_id,
                    "bounded_qa_obliged",
                    Some(*entry),
                    Some(serde_json::json!({ "reason": reason })),
                    now,
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// #2394: record why a pending, unbound landing is not being started
    /// (`reason` is a stable code: `deploy_drain`, `host_load`,
    /// `min_interval`). One `bounded_qa_held` receipt per change of reason
    /// since the landing's last receipt, so a stall is audited as a typed
    /// cause instead of looking like silence; a run dispatched or deferred in
    /// between starts a new record. Returns whether a receipt was written.
    pub fn note_bounded_qa_hold(
        &self,
        landing: &BoundedQaLanding,
        reason: &str,
        now: DateTime<Utc>,
    ) -> Result<bool> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let latest: Option<(String, Option<String>)> = tx
            .query_row(
                "SELECT kind, json_extract(detail_json,'$.reason') FROM rolling_queue_events
                 WHERE entry_id=?1 AND kind IN
                   ('bounded_qa_held','bounded_qa_requested','bounded_qa_capacity_deferred')
                 ORDER BY id DESC LIMIT 1",
                [landing.entry.id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let same = latest.is_some_and(|(kind, held)| {
            kind == "bounded_qa_held" && held.as_deref() == Some(reason)
        });
        if !same {
            record_event(
                &tx,
                landing.batch_id,
                "bounded_qa_held",
                Some(landing.entry.id),
                Some(serde_json::json!({"reason": reason})),
                now,
            )?;
        }
        tx.commit()?;
        Ok(!same)
    }

    /// #2112: when the newest full-library run for `project`'s landings was
    /// dispatched (its first `bounded_qa_requested` receipt), so the next
    /// start can wait out the operator's minimum interval. A run a deploy
    /// stopped (`deploy_preempted`, #2149) ran no tests and does not count
    /// (#2394): the next start does not wait out an interval for it.
    pub fn last_bounded_qa_dispatch(&self, project: Option<Uuid>) -> Result<Option<DateTime<Utc>>> {
        let at: Option<String> = self
            .conn
            .query_row(
                "SELECT ev.at FROM rolling_queue_events ev
                 JOIN rolling_queue_entries e ON e.id=ev.entry_id
                 WHERE ev.kind='bounded_qa_requested' AND e.project_id IS ?1
                   AND NOT EXISTS (
                     SELECT 1 FROM rolling_queue_events d
                     WHERE d.entry_id=ev.entry_id AND d.kind='bounded_qa_capacity_deferred'
                       AND json_extract(d.detail_json,'$.job_id')=json_extract(ev.detail_json,'$.job_id')
                       AND json_extract(d.detail_json,'$.refusal')='deploy_preempted')
                 ORDER BY ev.id DESC LIMIT 1",
                [project.map(|id| id.to_string())],
                |row| row.get(0),
            )
            .optional()?;
        at.map(|text| {
            DateTime::parse_from_rfc3339(&text)
                .map(|at| at.with_timezone(&Utc))
                .map_err(|e| DaemonError::Store(format!("bounded QA dispatch time: {e}")))
        })
        .transpose()
    }

    /// Published bounded gates remain discoverable after a daemon restart,
    /// including the interval between publication and QA dispatch. Finished
    /// obligations have a separate receipt; nothing rewrites the gate outcome.
    pub fn bounded_qa_landings(&self) -> Result<Vec<BoundedQaLanding>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {ENTRY_COLUMNS},
             (SELECT detail_json FROM rolling_queue_events
              WHERE entry_id=rolling_queue_entries.id AND kind='bounded_qa_requested'
                AND id > COALESCE((SELECT MAX(id) FROM rolling_queue_events
                    WHERE entry_id=rolling_queue_entries.id
                      AND kind='bounded_qa_capacity_deferred'), 0)
              ORDER BY id DESC LIMIT 1),
             (SELECT detail_json FROM rolling_queue_events
              WHERE entry_id=rolling_queue_entries.id
                AND kind IN ('bounded_qa_dispatching','bounded_qa_dispatch_withdrawn')
                AND id > COALESCE((SELECT MAX(id) FROM rolling_queue_events
                    WHERE entry_id=rolling_queue_entries.id
                      AND kind='bounded_qa_capacity_deferred'), 0)
              ORDER BY id DESC LIMIT 1)
             FROM rolling_queue_entries
             WHERE state='published' AND batch_id IS NOT NULL
               AND (instr(json_extract(outcome_json,'$.detail'),'selection=bounded ')>0
                    OR instr(json_extract(outcome_json,'$.detail'),' targets=bounded ')>0
                    OR instr(json_extract(outcome_json,'$.detail'),'selection=unknown ')>0
                    OR instr(json_extract(outcome_json,'$.detail'),' slow_tier=excluded')>0
                    OR EXISTS (SELECT 1 FROM rolling_queue_events
                        WHERE entry_id=rolling_queue_entries.id AND kind='bounded_qa_obliged'))
               AND NOT EXISTS (SELECT 1 FROM rolling_queue_events
                   WHERE entry_id=rolling_queue_entries.id AND kind='bounded_qa_finished')
             ORDER BY finished_at, sequence"
        ))?; // sql-dynamic-ok: static column list; no input interpolated
        let rows = stmt.query_map([], |row| {
            let entry = map_entry(row)?;
            let json_column = |column: usize| -> rusqlite::Result<Option<serde_json::Value>> {
                let text: Option<String> = row.get(column)?;
                text.map(|text| {
                    serde_json::from_str::<serde_json::Value>(&text).map_err(|e| {
                        rusqlite::Error::FromSqlConversionFailure(
                            column,
                            rusqlite::types::Type::Text,
                            Box::new(e),
                        )
                    })
                })
                .transpose()
            };
            let detail = json_column(18)?;
            let dispatch = json_column(19)?;
            let job = detail
                .as_ref()
                .and_then(|d| d.get("job_id"))
                .and_then(serde_json::Value::as_str);
            let sha = detail
                .as_ref()
                .and_then(|d| d.get("sha"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
            // #2120: receipts written before placement existed carry no
            // `ran_on`; they are hub jobs and parse as `None`.
            let placement = detail
                .as_ref()
                .filter(|d| d.get("ran_on").is_some())
                .and_then(|d| serde_json::from_value::<BoundedQaPlacement>(d.clone()).ok());
            let requested_at = detail
                .as_ref()
                .and_then(|d| d.get("requested_at"))
                .and_then(serde_json::Value::as_str)
                .and_then(|text| DateTime::parse_from_rfc3339(text).ok())
                .map(|at| at.with_timezone(&Utc));
            // #2155: the latest dispatch record names a pending intent only
            // while no binding exists and it was not withdrawn.
            let intent = match (&detail, &dispatch) {
                (None, Some(dispatch))
                    if dispatch.get("state").and_then(serde_json::Value::as_str)
                        == Some("dispatching") =>
                {
                    let text = |key: &str| {
                        dispatch
                            .get(key)
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned)
                    };
                    let invalid = |what: &str| {
                        rusqlite::Error::FromSqlConversionFailure(
                            19,
                            rusqlite::types::Type::Text,
                            what.to_owned().into(),
                        )
                    };
                    Some(BoundedQaIntent {
                        run_id: parse_uuid(&text("run_id").ok_or_else(|| invalid("run_id"))?)?,
                        sha: text("sha").ok_or_else(|| invalid("sha"))?,
                        peer_id: parse_uuid(&text("peer_id").ok_or_else(|| invalid("peer_id"))?)?,
                        peer_label: text("peer_label").ok_or_else(|| invalid("peer_label"))?,
                    })
                }
                _ => None,
            };
            Ok(BoundedQaLanding {
                entry: entry.entry,
                batch_id: entry.batch_id.expect("query excludes NULL batch"),
                project_id: entry.project_id.as_deref().map(parse_uuid).transpose()?,
                repo_path: entry.repo_path,
                job_id: job.map(parse_uuid).transpose()?,
                qa_sha: sha,
                placement,
                requested_at,
                intent,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// #2155: durably record, before the hub sends a satellite start, which
    /// peer it is about to start `run_id` of `sha` on for these `landings`.
    /// The satellite may accept the start and the hub stop before it records
    /// the binding; the next pass finds this record and reconciles that exact
    /// peer and run id instead of placing the obligation again. Append-only:
    /// each attempt adds one record per landing.
    pub fn begin_bounded_qa_dispatch(
        &self,
        landings: &[BoundedQaLanding],
        run_id: Uuid,
        sha: &str,
        peer_id: Uuid,
        peer_label: &str,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let covered: Vec<String> = landings.iter().map(|l| l.entry.id.to_string()).collect();
        let detail = serde_json::json!({
            "state": "dispatching",
            "run_id": run_id,
            "sha": sha,
            "peer_id": peer_id,
            "peer_label": peer_label,
            "landings": covered,
        });
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        for landing in landings {
            // A capacity deferral (#2149) retires the earlier request and
            // intent: the landing may be dispatched anew.
            let settled: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM rolling_queue_events
                 WHERE entry_id=?1 AND (kind='bounded_qa_finished'
                   OR (kind='bounded_qa_requested'
                       AND id > COALESCE((SELECT MAX(id) FROM rolling_queue_events
                           WHERE entry_id=?1 AND kind='bounded_qa_capacity_deferred'), 0))))",
                [landing.entry.id.to_string()],
                |row| row.get(0),
            )?;
            if settled {
                return Err(DaemonError::Store(format!(
                    "bounded QA landing {} is already bound; it is not dispatched again",
                    landing.entry.id
                )));
            }
            record_event(
                &tx,
                landing.batch_id,
                "bounded_qa_dispatching",
                Some(landing.entry.id),
                Some(detail.clone()),
                now,
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// #2155: the satellite answered `run_id`'s start with a refusal that
    /// proves it started nothing, so the recorded intent no longer holds the
    /// obligation and it may be placed on the next peer or the hub.
    pub fn withdraw_bounded_qa_dispatch(
        &self,
        landings: &[BoundedQaLanding],
        run_id: Uuid,
        peer_id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let detail = serde_json::json!({
            "state": "withdrawn",
            "run_id": run_id,
            "peer_id": peer_id,
        });
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        for landing in landings {
            record_event(
                &tx,
                landing.batch_id,
                "bounded_qa_dispatch_withdrawn",
                Some(landing.entry.id),
                Some(detail.clone()),
                now,
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Bind all verified ancestors to one full run atomically. Retrying after
    /// a crash between job submission and this write uses the same job key.
    /// Each receipt names every landing the run covers (#2112) and where the
    /// run executes (#2120: `ran_on` hub with its reason, or the satellite).
    pub fn request_bounded_qa(
        &self,
        landings: &[BoundedQaLanding],
        job_id: Uuid,
        sha: &str,
        placement: &BoundedQaPlacement,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let covered: Vec<String> = landings.iter().map(|l| l.entry.id.to_string()).collect();
        let detail = with_placement(
            serde_json::json!({
                "job_id": job_id,
                "sha": sha,
                "landings": covered,
                "requested_at": stamp(now),
            }),
            Some(placement),
        )?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        for landing in landings {
            // A capacity deferral (#2149) retires the earlier request: the
            // landing may bind a new run.
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM rolling_queue_events
                 WHERE entry_id=?1 AND kind='bounded_qa_requested'
                   AND id > COALESCE((SELECT MAX(id) FROM rolling_queue_events
                       WHERE entry_id=?1 AND kind='bounded_qa_capacity_deferred'), 0))",
                [landing.entry.id.to_string()],
                |row| row.get(0),
            )?;
            if !exists {
                record_event(
                    &tx,
                    landing.batch_id,
                    "bounded_qa_requested",
                    Some(landing.entry.id),
                    Some(detail.clone()),
                    now,
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// #2232: whether `job_id` is the job some automatic full-library QA
    /// obligation was bound to (a `bounded_qa_requested` receipt names it).
    /// The binding is the daemon's own record, so no caller can claim or shed
    /// it through a job name or parameters: it marks the job as a daemon-owned
    /// obligation whatever session the job row is attributed to.
    pub fn bounded_qa_job_bound(&self, job_id: Uuid) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM rolling_queue_events
             WHERE kind='bounded_qa_requested' AND json_extract(detail_json,'$.job_id')=?1)",
            [job_id.to_string()],
            |row| row.get(0),
        )?)
    }

    /// #2149: how many runs for `entry` ended on capacity (a deferral receipt
    /// each). A new run for the same anchor needs a distinct job identity.
    pub fn bounded_qa_capacity_deferrals(&self, entry: Uuid) -> Result<u32> {
        let count: i64 = self.conn.query_row(
            "SELECT count(*) FROM rolling_queue_events
             WHERE entry_id=?1 AND kind='bounded_qa_capacity_deferred'",
            [entry.to_string()],
            |row| row.get(0),
        )?;
        Ok(u32::try_from(count).unwrap_or(u32::MAX))
    }

    /// #2149: a run that ended on capacity (`refusal` is `job_admission_timed_out`
    /// or `job_start_timed_out`: it never ran tests) files no Issue and
    /// finishes nothing. Each covered landing gets one
    /// `bounded_qa_capacity_deferred` receipt naming every landing, and goes
    /// back to pending, unbound, to join the next coalesced attempt (which
    /// still waits out the operator's minimum interval). A replay for the same
    /// run adds nothing. The overdue rule keeps counting from publication.
    pub fn defer_bounded_qa(
        &self,
        landings: &[BoundedQaLanding],
        job_id: Uuid,
        sha: &str,
        refusal: &str,
        run: &BoundedQaRun,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let covered: Vec<String> = landings.iter().map(|l| l.entry.id.to_string()).collect();
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        for landing in landings {
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM rolling_queue_events
                 WHERE entry_id=?1 AND kind='bounded_qa_capacity_deferred'
                   AND json_extract(detail_json,'$.job_id')=?2)",
                params![landing.entry.id.to_string(), job_id.to_string()],
                |row| row.get(0),
            )?;
            if !exists {
                record_event(
                    &tx,
                    landing.batch_id,
                    "bounded_qa_capacity_deferred",
                    Some(landing.entry.id),
                    Some(with_placement(
                        serde_json::json!({
                            "job_id": job_id,
                            "sha": sha,
                            "refusal": refusal,
                            "exit_code": run.exit_code,
                            "log_path": run.log_path,
                            "landings": covered,
                        }),
                        run.placement.as_ref(),
                    )?),
                    now,
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// A red/incomplete run files its attributed Issue in the same transaction
    /// as its receipt. Replay cannot lose the Issue or file a duplicate. The
    /// receipt carries `run`'s placement, exit code, failing tests (at most
    /// 50; the Issue names them all), log path and the covered landings.
    #[allow(clippy::too_many_arguments)]
    pub fn finish_bounded_qa(
        &self,
        landings: &[BoundedQaLanding],
        job_id: Uuid,
        sha: &str,
        passed: bool,
        run: &BoundedQaRun,
        new_issue: Option<&NewIssue>,
        now: DateTime<Utc>,
    ) -> Result<Option<i64>> {
        let fixed = |_: &[String]| new_issue.cloned();
        let make: Option<&dyn Fn(&[String]) -> Option<NewIssue>> = new_issue.map(|_| &fixed as _);
        self.finish_bounded_qa_with(landings, job_id, sha, passed, run, make, now)
    }

    /// [`Self::finish_bounded_qa`] where the Issue is built by `make_issue`
    /// from the tests it must name (hub #3054). A red run whose every failing
    /// test an open (`Open` or `InProgress`) `qa` Issue of the project already
    /// lists on a `Failing tests:` or `Test:` line (whole names, never
    /// substrings) files nothing and sends no notice: the run is appended to
    /// each such Issue and the receipt names the first. A run with some tracked
    /// tests files one Issue for the rest only, and appends to the tracking
    /// Issues too. Without an open `qa` Issue naming a test, or for a
    /// `coverage-incomplete` Issue, the full list is filed as before.
    #[allow(clippy::too_many_arguments)]
    pub fn finish_bounded_qa_with(
        &self,
        landings: &[BoundedQaLanding],
        job_id: Uuid,
        sha: &str,
        passed: bool,
        run: &BoundedQaRun,
        make_issue: Option<&dyn Fn(&[String]) -> Option<NewIssue>>,
        now: DateTime<Utc>,
    ) -> Result<Option<i64>> {
        let covered: Vec<String> = landings.iter().map(|l| l.entry.id.to_string()).collect();
        let failing: Vec<&String> = run
            .failing_tests
            .iter()
            .take(RECEIPT_MAX_FAILING_TESTS)
            .collect();
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let filing = make_issue
            .map(|make| Self::file_or_track_qa_red_in_tx(&tx, job_id, sha, run, make, now))
            .transpose()?
            .flatten();
        let issue = filing.as_ref().map(|filing| filing.number);
        for landing in landings {
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM rolling_queue_events
                 WHERE entry_id=?1 AND kind='bounded_qa_finished')",
                [landing.entry.id.to_string()],
                |row| row.get(0),
            )?;
            if !exists {
                record_event(
                    &tx,
                    landing.batch_id,
                    "bounded_qa_finished",
                    Some(landing.entry.id),
                    Some(with_placement(
                        serde_json::json!({
                            "job_id": job_id,
                            "sha": sha,
                            "passed": passed,
                            "issue": issue,
                            "exit_code": run.exit_code,
                            "failing_tests": failing,
                            "failing_tests_total": run.failing_tests.len(),
                            "log_path": run.log_path,
                            "landings": covered,
                        }),
                        run.placement.as_ref(),
                    )?),
                    now,
                )?;
            }
        }
        // #2743/#2771: tell the current manager seat in the same transaction as
        // the Issue and the receipt; a replay of the same settle finds the
        // notice recorded and adds nothing, and a failure loses neither.
        if let (
            Some(QaFiling {
                number,
                filed: Some(filed),
            }),
            Some(project),
        ) = (filing.as_ref(), landings.first().and_then(|l| l.project_id))
        {
            let new = &filed.issue;
            let notice = QaIssueNotice {
                issue_number: *number,
                title: new.title.clone(),
                outcome: if new
                    .labels
                    .iter()
                    .any(|label| label == "coverage-incomplete")
                {
                    "coverage_incomplete"
                } else {
                    "test_red"
                },
                rolling_sha: sha.to_string(),
                failing_tests: filed.tests.clone(),
            };
            self.record_qa_issue_notice_in_tx(project, &notice)?;
        }
        issue_write_before_commit()?;
        tx.commit()?;
        Ok(issue)
    }

    /// Settle a red run's Issue inside the receipt transaction (hub #3054):
    /// file it, or record the run on the open `qa` Issues that already name
    /// its tests. `None` when `make` has no project to file in.
    fn file_or_track_qa_red_in_tx(
        tx: &Transaction<'_>,
        job_id: Uuid,
        sha: &str,
        run: &BoundedQaRun,
        make: &dyn Fn(&[String]) -> Option<NewIssue>,
        now: DateTime<Utc>,
    ) -> Result<Option<QaFiling>> {
        let Some(full) = make(&run.failing_tests) else {
            return Ok(None);
        };
        let issue_id = Uuid::new_v5(&job_id, b"bounded-library-qa-red");
        let tracked = if run.failing_tests.is_empty()
            || full.labels.iter().any(|l| l == "coverage-incomplete")
            || Self::get_issue_tx(tx, issue_id)?.is_some()
        {
            // An incomplete run tracks no test, and a replay finds its own Issue.
            Vec::new()
        } else {
            Self::open_qa_issues_naming_tx(tx, full.project_id, &run.failing_tests)?
        };
        let tracked_tests: std::collections::HashSet<&str> = tracked
            .iter()
            .flat_map(|(_, names)| names.iter().map(String::as_str))
            .collect();
        let untracked: Vec<String> = run
            .failing_tests
            .iter()
            .filter(|name| !tracked_tests.contains(name.as_str()))
            .cloned()
            .collect();
        let to_file = match (tracked.is_empty(), untracked.is_empty()) {
            (true, _) => Some(full),
            (false, true) => None,
            (false, false) => make(&untracked),
        };
        let filed = to_file
            .map(|new| {
                let created = Self::idempotent_issue_in_tx(
                    tx,
                    issue_id,
                    &new,
                    IssueWriteActor::System {
                        label: QA_ACTOR_LABEL.into(),
                    },
                )?;
                Ok::<_, DaemonError>((created.issue.display_number, new))
            })
            .transpose()?;
        let filed_number = filed.as_ref().map(|(number, _)| *number);
        for (issue, names) in &tracked {
            Self::append_qa_run_in_tx(tx, issue, names, job_id, sha, filed_number, now)?;
        }
        let number = filed_number.or_else(|| tracked.first().map(|(i, _)| i.display_number));
        Ok(number.map(|number| QaFiling {
            number,
            filed: filed.map(|(_, issue)| FiledQaIssue {
                tests: if tracked.is_empty() {
                    run.failing_tests.clone()
                } else {
                    untracked
                },
                issue,
            }),
        }))
    }

    /// The open `qa` Issues of `project` that name at least one of `failing`
    /// on a `Failing tests:` or `Test:` line, each with the names of `failing`
    /// it carries (whole names: `a_test` is not `a_test_2`), lowest number
    /// first. A test already carried by an earlier Issue is credited to that
    /// one only.
    fn open_qa_issues_naming_tx(
        tx: &Transaction<'_>,
        project: Uuid,
        failing: &[String],
    ) -> Result<Vec<(rsi_common::types::Issue, Vec<String>)>> {
        let mut claimed: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let mut named = Vec::new();
        for issue in Self::open_labelled_issues_tx(tx, project, "qa")? {
            let listed = rsi_common::rolling_queue::failing_tests_listed(&issue.body);
            let names: Vec<String> = failing
                .iter()
                .filter(|name| listed.contains(name) && claimed.insert(name.as_str()))
                .cloned()
                .collect();
            if !names.is_empty() {
                named.push((issue, names));
            }
        }
        Ok(named)
    }

    /// Record this run on `issue`, one section per job (a replay finds the job
    /// id in the body and adds nothing). A body with no room left is skipped:
    /// the receipt still names the Issue.
    fn append_qa_run_in_tx(
        tx: &Transaction<'_>,
        issue: &rsi_common::types::Issue,
        names: &[String],
        job_id: Uuid,
        sha: &str,
        filed: Option<i64>,
        now: DateTime<Utc>,
    ) -> Result<()> {
        if issue.body.contains(&job_id.to_string()) {
            return Ok(());
        }
        let others = filed
            .map(|number| format!(" The run's other failing tests are filed on #{number}."))
            .unwrap_or_default();
        let section = format!(
            "\n\n---\nAutomatic full-library QA run: rolling {sha}, job {job_id}, {}. Still red on {} test(s) this Issue names: {}. No new Issue was filed for them.{others}\n",
            now.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
            names.len(),
            names.join(", "),
        );
        Self::append_issue_body_as_system_tx(tx, issue, &section, QA_ACTOR_LABEL)?;
        Ok(())
    }
}

const QA_ACTOR_LABEL: &str = "rsi:bounded-library-qa";

/// What a red run's Issue step did: the Issue the receipt names, and the new
/// Issue it filed (`None` when open `qa` Issues already tracked every test).
struct QaFiling {
    number: i64,
    filed: Option<FiledQaIssue>,
}

struct FiledQaIssue {
    issue: NewIssue,
    /// The tests the new Issue names.
    tests: Vec<String>,
}
