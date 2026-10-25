//! #3338: exercise both bundled pre-land repair routes with real custody.
use super::*;

const ROUTES: [&str; 2] = [ISSUE_PIPELINE, ISSUE_PIPELINE_POST_REVIEW];

async fn pre_land(world: &World, starter: &str) -> (Uuid, String, rows::AttemptRow) {
    let issue = open_issue(world, "Repair an already reviewed landing").await;
    let (execution, head, _) =
        refused_for_review(world, starter_named(starter), &issue, "pre-land-repair").await;
    world.harness.script_reviews([ReviewScript::Accept]);
    let land = land_waiting_on(world, execution, "pre_land").await;
    (execution, head, land)
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn pre_land_refused_once_repairs_reviews_and_lands() {
    for starter in ROUTES {
        for reason in [
            "gate_failed: provisional declaration needs repair",
            "queue_batch_merge_conflict",
        ] {
            let world = World::new(pipeline_manager()).await;
            let (execution, head, land) = pre_land(&world, starter).await;
            world.harness.set_land_state(
                land.land_entry_id.unwrap(),
                LandStatus::Refused {
                    state: "refused".into(),
                    reason: reason.into(),
                },
            );
            let (repair, repaired, _) =
                deliver_verified(&world, execution, "pre_land_repair_1").await;
            assert_eq!(repair.base_commit, head);
            assert!(repair.query().contains(reason));
            assert!(
                repair
                    .query()
                    .contains(&land.land_entry_id.unwrap().to_string())
            );
            let review = waiting_review(&world, execution, "pre_land_review_1", 0).await;
            let requests = world.harness.review_requests();
            let request = &requests.last().unwrap().1;
            assert_eq!(request.source_commit, repaired);
            assert_eq!(request.of_node, "pre_land_repair_1");
            assert!(
                request
                    .query
                    .contains(&format!("ONLY the repair delta from {head} to {repaired}"))
            );
            world.harness.script_reviews([ReviewScript::Accept]);
            let landed = land_waiting_on(&world, execution, "pre_land_1").await;
            let requests = world.harness.land_requests();
            let request = &requests.last().unwrap().1;
            assert_eq!(request.source_commit, repaired);
            assert_eq!(
                request.acceptance,
                LandAcceptance::Review {
                    node: "pre_land_review_1".into(),
                    assignment_id: review.review_assignment_id.unwrap(),
                }
            );
            publish(&world, &landed, &"e".repeat(40));
            assert_eq!(world.harness.run_to_end(execution).await, Step::Done);
            assert_eq!(
                world.harness.status(execution).await,
                ExecutionStatus::Succeeded
            );
            assert_eq!(world.harness.review_requests().len(), 2);
            assert_eq!(world.harness.launches().len(), 2);
        }
    }
}

/// Infrastructure interruption keeps the accepted source in queue custody:
/// the same land attempt waits through recovery and publishes once.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn pre_land_interrupted_publication_waits_on_the_same_entry_then_lands() {
    use rsi_common::rolling_queue::{RollingQueueEntryState, RollingQueueOutcome};
    for starter in ROUTES {
        let world = World::new(pipeline_manager()).await;
        let (execution, _, land) = pre_land(&world, starter).await;
        let entry = land.land_entry_id.unwrap();
        let launches = world.harness.launches().len();
        for state in [
            RollingQueueEntryState::Gating,
            RollingQueueEntryState::Queued,
            RollingQueueEntryState::Gating,
        ] {
            world.harness.set_land_state(
                entry,
                LandStatus::from_queue(
                    state,
                    Some(RollingQueueOutcome {
                        refusal: Some("publication_unknown".into()),
                        detail: Some("landing interrupted".into()),
                        ..Default::default()
                    }),
                ),
            );
            for _ in 0..3 {
                world.harness.executor.advance(execution).await.unwrap();
            }
            let waiting = world.harness.attempt(execution, "pre_land", 0, 1).await;
            assert_eq!(waiting.status, AttemptStatus::Waiting);
            assert_eq!(waiting.id, land.id);
            assert_eq!(waiting.land_entry_id, Some(entry));
            assert_eq!(world.harness.launches().len(), launches);
            assert_eq!(world.harness.land_requests().len(), 1);
        }
        publish(&world, &land, &"e".repeat(40));
        assert_eq!(world.harness.run_to_end(execution).await, Step::Done);
        assert_eq!(
            world.harness.status(execution).await,
            ExecutionStatus::Succeeded
        );
        assert_eq!(world.harness.land_requests().len(), 1);
    }
}

/// Legacy terminal interruptions have no recovery writer: positive ancestry
/// resolves the land attempt, otherwise the existing repair route still runs.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn pre_land_legacy_interruption_resolves_landed_or_runs_repair() {
    use rsi_common::rolling_queue::{RollingQueueEntryState, RollingQueueOutcome};
    for starter in ROUTES {
        for landed in [true, false] {
            let world = World::new(pipeline_manager()).await;
            let (execution, head, land) = pre_land(&world, starter).await;
            let entry = land.land_entry_id.unwrap();
            let status = LandStatus::from_queue(
                RollingQueueEntryState::Failed,
                Some(RollingQueueOutcome {
                    refusal: Some("publication_unknown".into()),
                    detail: Some("landing interrupted".into()),
                    ..Default::default()
                }),
            );
            assert!(matches!(status, LandStatus::Refused { .. }));
            world.harness.set_land_state(entry, status);
            if landed {
                world.harness.set_source_landed(&head, &"e".repeat(40));
                assert_eq!(world.harness.run_to_end(execution).await, Step::Done);
                let resolved = world.harness.attempt(execution, "pre_land", 0, 1).await;
                assert_eq!(resolved.status, AttemptStatus::Succeeded);
                assert_eq!(
                    resolved.output.unwrap()["fields"]["landed_sha"],
                    "e".repeat(40)
                );
                assert_eq!(world.harness.land_requests().len(), 1);
            } else {
                let (repair, _, _) = deliver_verified(&world, execution, "pre_land_repair_1").await;
                assert_eq!(repair.base_commit, head);
                assert!(
                    repair
                        .query()
                        .contains("publication_unknown: landing interrupted")
                );
                assert!(repair.query().contains(&entry.to_string()));
            }
        }
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn pre_land_cancelled_entry_passes_the_cancel_reason_to_repair() {
    use rsi_common::rolling_queue::{RollingQueueEntryState, RollingQueueOutcome};
    for starter in ROUTES {
        let world = World::new(pipeline_manager()).await;
        let (execution, _, land) = pre_land(&world, starter).await;
        let detail = "Renumber tool refused: tests.rs declaration lacks path_template";
        world.harness.set_land_state(
            land.land_entry_id.unwrap(),
            LandStatus::from_queue(
                RollingQueueEntryState::Superseded,
                Some(RollingQueueOutcome {
                    refusal: Some("queue_entry_cancelled".into()),
                    detail: Some(detail.into()),
                    ..Default::default()
                }),
            ),
        );
        let (repair, _, _) = deliver_verified(&world, execution, "pre_land_repair_1").await;
        assert!(
            repair
                .query()
                .contains(&format!("queue_entry_cancelled: {detail}"))
        );
        assert!(
            repair
                .query()
                .contains(&land.land_entry_id.unwrap().to_string())
        );
        assert_eq!(
            world.harness.status(execution).await,
            ExecutionStatus::Running
        );
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn pre_land_repair_only_merging_rolling_skips_re_review() {
    for starter in ROUTES {
        let world = World::new(pipeline_manager()).await;
        let (execution, head, land) = pre_land(&world, starter).await;
        refuse(&world, &land);
        world.harness.executor.advance(execution).await.unwrap();
        let repair = world
            .harness
            .attempt(execution, "pre_land_repair_1", 0, 1)
            .await;
        let sandbox = world.harness.sandbox(repair.session_id);
        let tree = git(
            &sandbox,
            &["rev-parse", &format!("{}^{{tree}}", world.harness.base)],
        );
        let rolling = git(
            &sandbox,
            &[
                "commit-tree",
                &tree,
                "-p",
                &world.harness.base,
                "-m",
                "rolling advanced",
            ],
        );
        git(
            &sandbox,
            &["update-ref", "refs/remotes/origin/rolling", &rolling],
        );
        git(
            &sandbox,
            &["merge", "--no-ff", "-m", "merge rolling", &rolling],
        );
        let merged = git(&sandbox, &["rev-parse", "HEAD"]);
        world.harness.complete_without_commit(repair.session_id);
        scoped_test_job(
            &world.harness,
            repair.session_id,
            JobState::Succeeded,
            green_receipt(&merged),
        )
        .await;
        let next = land_waiting_on(&world, execution, "pre_land_1").await;
        let reviews = world.harness.review_requests();
        assert_eq!(reviews.len(), 1);
        let requests = world.harness.land_requests();
        assert_eq!(
            requests.last().unwrap().1.acceptance,
            LandAcceptance::RollingMerge {
                node: "pre_land_review_1".into(),
                assignment_id: reviews[0].0,
                reviewed_commit: head.clone(),
            }
        );
        let review = world
            .harness
            .attempt(execution, "pre_land_review_1", 0, 1)
            .await;
        assert_eq!(
            review.output.unwrap()["fields"]["review_skipped"],
            "only_rolling_merges"
        );
        assert!(crate::topology::custody::only_rolling_merges(
            &world.harness.repo,
            &head,
            &merged
        ));
        // A merge-shaped commit with extra bytes is not sufficient proof.
        std::fs::write(sandbox.join("resolution.txt"), "additional change").unwrap();
        git(&sandbox, &["add", "resolution.txt"]);
        let changed_tree = git(&sandbox, &["write-tree"]);
        let forged = git(
            &sandbox,
            &[
                "commit-tree",
                &changed_tree,
                "-p",
                &head,
                "-p",
                &rolling,
                "-m",
                "changed resolution",
            ],
        );
        assert!(!crate::topology::custody::only_rolling_merges(
            &world.harness.repo,
            &head,
            &forged
        ));
        // Queue admission independently rejects a claimed merge acceptance
        // whose tree includes an unreviewed resolution, even with the review
        // ledger admitted by the production test seam.
        let mut forged_request = requests.last().unwrap().1.clone();
        forged_request.source_commit = forged;
        let refused = land_control(&world)
            .enqueue_topology_admitted_for_test(&forged_request, true)
            .await
            .unwrap();
        assert!(
            matches!(refused, crate::topology::land::LandEnqueue::AdmissionLost(reason)
            if reason.contains("not followed solely by clean rolling merges"))
        );
        publish(&world, &next, &"f".repeat(40));
        assert_eq!(world.harness.run_to_end(execution).await, Step::Done);
        assert_eq!(
            world.harness.status(execution).await,
            ExecutionStatus::Succeeded
        );
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn pre_land_repair_exhaustion_fails_with_the_last_refusal() {
    for starter in ROUTES {
        let world = World::new(pipeline_manager()).await;
        let (execution, _, mut land) = pre_land(&world, starter).await;
        for i in 1..=3 {
            refuse(&world, &land);
            deliver_verified(&world, execution, &format!("pre_land_repair_{i}")).await;
            waiting_review(&world, execution, &format!("pre_land_review_{i}"), 0).await;
            world.harness.script_reviews([ReviewScript::Accept]);
            land = land_waiting_on(&world, execution, &format!("pre_land_{i}")).await;
        }
        let last = "gate_failed: third repair still has an invalid provisional declaration";
        world.harness.set_land_state(
            land.land_entry_id.unwrap(),
            LandStatus::Refused {
                state: "refused".into(),
                reason: last.into(),
            },
        );
        assert_eq!(world.harness.run_to_end(execution).await, Step::Done);
        assert_eq!(
            world.harness.status(execution).await,
            ExecutionStatus::Failed
        );
        let final_land = world.harness.attempt(execution, "pre_land_3", 0, 1).await;
        assert_eq!(
            final_land.error.as_deref(),
            Some(format!("merge queue entry refused: {last}").as_str())
        );
        let row = {
            let store = world.harness.executor.store.lock().await;
            rows::load_execution(&store, execution).unwrap().unwrap()
        };
        assert!(row.error.unwrap().contains(last));
        assert_eq!(world.harness.launches().len(), 4);
    }
}
