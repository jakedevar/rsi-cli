//! #1172: the rotation and retry custody paths must never hold the Store while
//! they wait for a custody stripe. A maintenance proof (purge, archive cleanup,
//! settlement) can hold the live root's stripe for seconds; every other Store
//! user, the RPC handlers and the watchdog's Store probe included, must keep
//! being served meanwhile, and the path itself must finish once the proof ends.
//!
//! Each test holds the live root's stripe on a plain thread for 3 s (readiness
//! handshake), runs one path under a Store probe, and asserts the probe took
//! samples, stayed under its bound, and that the path finished only after the
//! stripe was released (`run_behind_held_stripes`).
#![allow(
    clippy::expect_used,
    clippy::significant_drop_tightening,
    clippy::large_futures
)]

use super::tests::{
    LiveRotationFixture, insert_rotation_invocation_fixture, persist_live_rotation_parent,
    rotation_manager,
};
use super::*;
use crate::sandbox::custody::{RotationBindFailure, RotationCustodyCandidate};
use crate::store::stripe_liveness_support::run_behind_held_stripes;
use rsi_common::types::SandboxCustodyErrorCodeV1;
use std::time::Duration;

const HOLD: Duration = Duration::from_secs(3);

struct ReservedSuccessor {
    candidate: RotationCustodyCandidate,
    child: Session,
    invocation_id: Uuid,
}

/// Prepare a rotation candidate for the live sandboxed `fixture.parent` and
/// persist its reserved, still unbound successor.
async fn reserve_live_successor(
    manager: &SessionManager,
    fixture: &LiveRotationFixture,
    rotation_id: &str,
) -> ReservedSuccessor {
    let runtime = manager.custody_execution_runtime();
    let candidate = runtime
        .prepare_rotation_successor(&fixture.parent)
        .await
        .expect("prepare the rotation candidate");
    let mut child = super::tests::test_session(Uuid::new_v4(), SessionStatus::Starting);
    child.provider = SessionProvider::Claude;
    child.working_dir = fixture.repo.clone();
    child.project_id = fixture.parent.project_id;
    child.continued_from = Some(fixture.parent.id);
    child.rotation_depth = 1;
    child.query = "stripe liveness rotation successor".into();
    crate::sandbox::custody::CustodyExecutionRuntime::apply_rotation_successor_tuple(
        &candidate,
        &fixture.parent,
        &mut child,
    )
    .expect("apply the authenticated successor tuple");
    let invocation_id = Uuid::new_v4();
    {
        let mut store = manager.store.lock().await;
        insert_rotation_invocation_fixture(&store, child.id, invocation_id, "running");
        store
            .insert_reserved_rotation_session_with_invocation(&child, invocation_id, rotation_id)
            .expect("persist the reserved successor");
    }
    ReservedSuccessor {
        candidate,
        child,
        invocation_id,
    }
}

async fn session_status(manager: &SessionManager, id: Uuid) -> SessionStatus {
    manager
        .store
        .lock()
        .await
        .get_session(id)
        .expect("read session")
        .expect("session row")
        .status
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rotation_bind_waits_for_a_held_stripe_without_pinning_the_store() {
    let (manager, dir) = rotation_manager();
    let fixture = persist_live_rotation_parent(&manager, dir.path(), None).await;
    let reserved = reserve_live_successor(&manager, &fixture, "stripe-bind").await;
    let runtime = manager.custody_execution_runtime();
    let run = run_behind_held_stripes(
        &manager.store,
        Some(vec![fixture.custody_id]),
        HOLD,
        runtime.bind_rotation_successor(reserved.candidate, &fixture.parent, &reserved.child),
    )
    .await;
    run.assert_store_stayed_free("rotation bind")
        .unwrap_or_else(|failure| {
            panic!("the bind must succeed once the stripe frees: {failure:?}")
        });
    let owner = manager
        .store
        .lock()
        .await
        .live_custody_for_session(reserved.child.id)
        .expect("the successor owns the transferred custody");
    assert_eq!(owner.custody_id, fixture.custody_id);
}

/// A bind that loses for an unrelated SQL reason leaves the predecessor's
/// custody live; classifying the refusal re-proves that predecessor under its
/// stripe. With the stripe busy the proof must drop the Store, wait, and still
/// classify the refusal `Restorable` once the stripe frees.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rotation_refusal_reauth_waits_for_a_held_stripe_without_pinning_the_store() {
    let (manager, dir) = rotation_manager();
    let fixture = persist_live_rotation_parent(&manager, dir.path(), None).await;
    let reserved = reserve_live_successor(&manager, &fixture, "stripe-refusal").await;
    let runtime = manager.custody_execution_runtime();
    let run = run_behind_held_stripes(
        &manager.store,
        Some(vec![fixture.custody_id]),
        HOLD,
        runtime.settle_and_classify_rotation_refusal_for_test(
            &reserved.candidate,
            &fixture.parent,
            reserved.child.id,
        ),
    )
    .await;
    let failure = run.assert_store_stayed_free("rotation refusal reauth");
    assert!(
        matches!(failure, RotationBindFailure::Restorable),
        "the live predecessor is still restorable after the refusal: {failure:?}"
    );
    assert_eq!(
        session_status(&manager, reserved.child.id).await,
        SessionStatus::Failed,
        "the refused successor is settled Failed"
    );
}

/// The refusal's authority fence runs before the Store is released for a busy
/// stripe, so a change to the predecessor during that wait (status, prompt,
/// model invocation) leaves the root untouched yet invalidates the durable
/// authority the candidate captured. After the wait the classifier must repeat
/// the full fence under the reacquired Store and return `Superseded`: a stale
/// `Restorable` would republish the old completed session over the change
/// (#1179). The mutation lands inside the acknowledged contention interval.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rotation_refusal_reauth_repeats_the_authority_fence_after_a_held_stripe_wait() {
    #[derive(Clone, Copy, Debug)]
    enum Mutation {
        Archived,
        ExecutionPrompt,
        ModelInvocation,
    }

    const CHANGED_PROMPT: &str = "predecessor prompt changed during the stripe wait";
    const CHANGED_INVOCATION: &str = "00000000-0000-4000-8000-000000000003";

    for mutation in [
        Mutation::Archived,
        Mutation::ExecutionPrompt,
        Mutation::ModelInvocation,
    ] {
        let (manager, dir) = rotation_manager();
        let fixture = persist_live_rotation_parent(&manager, dir.path(), None).await;
        let reserved = reserve_live_successor(&manager, &fixture, "stripe-refusal-fence").await;
        let runtime = manager.custody_execution_runtime();
        let contended =
            crate::store::custody_lock_order::admission_contention_signal(fixture.custody_id);
        let mutate = async {
            // Wait for the classifier to drop the Store on the busy stripe;
            // only then is the Store free for the change.
            let waited = tokio::time::Instant::now();
            while contended.try_recv().is_err() {
                assert!(
                    waited.elapsed() < Duration::from_secs(20),
                    "{mutation:?}: the classifier never contended on the held stripe"
                );
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            let store = manager.store.lock().await;
            match mutation {
                Mutation::Archived => store
                    .update_session_status(fixture.parent.id, SessionStatus::Archived)
                    .expect("archive the predecessor during the wait"),
                Mutation::ExecutionPrompt => {
                    store
                        .conn
                        .execute(
                            "UPDATE sessions SET query=?2, \
                             updated_at='2099-01-01T00:00:00.000000000Z' WHERE id=?1",
                            rusqlite::params![fixture.parent.id.to_string(), CHANGED_PROMPT],
                        )
                        .expect("change the predecessor prompt during the wait");
                }
                Mutation::ModelInvocation => {
                    store
                        .conn
                        .execute(
                            "UPDATE sessions SET model_invocation_id=?2, \
                             updated_at='2099-01-01T00:00:00.000000000Z' WHERE id=?1",
                            rusqlite::params![fixture.parent.id.to_string(), CHANGED_INVOCATION],
                        )
                        .expect("change the predecessor invocation during the wait");
                }
            }
        };
        let run = run_behind_held_stripes(
            &manager.store,
            Some(vec![fixture.custody_id]),
            Duration::from_secs(1),
            async {
                let (failure, ()) = tokio::join!(
                    runtime.settle_and_classify_rotation_refusal_for_test(
                        &reserved.candidate,
                        &fixture.parent,
                        reserved.child.id,
                    ),
                    mutate
                );
                failure
            },
        )
        .await;
        let failure = run.assert_store_stayed_free("rotation refusal authority fence");
        assert!(
            matches!(failure, RotationBindFailure::Superseded),
            "{mutation:?}: a predecessor changed during the stripe wait is superseded, \
             not restorable: {failure:?}"
        );
        assert_eq!(
            session_status(&manager, reserved.child.id).await,
            SessionStatus::Failed,
            "{mutation:?}: the refused successor stays settled Failed"
        );
        let store = manager.store.lock().await;
        let current = store
            .get_session(fixture.parent.id)
            .expect("read the predecessor")
            .expect("the predecessor row survives");
        match mutation {
            Mutation::Archived => assert_eq!(current.status, SessionStatus::Archived),
            Mutation::ExecutionPrompt => {
                assert_eq!(
                    current.query, CHANGED_PROMPT,
                    "the prompt change is preserved"
                );
            }
            Mutation::ModelInvocation => {
                let invocation: String = store
                    .conn
                    .query_row(
                        "SELECT model_invocation_id FROM sessions WHERE id=?1",
                        [fixture.parent.id.to_string()],
                        |row| row.get(0),
                    )
                    .expect("read the predecessor invocation");
                assert_eq!(
                    invocation, CHANGED_INVOCATION,
                    "the invocation change is preserved"
                );
            }
        }
        assert_eq!(
            store
                .live_custody_for_session(fixture.parent.id)
                .expect("the predecessor's root is untouched")
                .custody_id,
            fixture.custody_id
        );
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rotation_finalize_waits_for_a_held_stripe_without_pinning_the_store() {
    let (manager, dir) = rotation_manager();
    let fixture = persist_live_rotation_parent(&manager, dir.path(), None).await;
    let reserved = reserve_live_successor(&manager, &fixture, "stripe-finalize").await;
    let runtime = manager.custody_execution_runtime();
    let bound = runtime
        .bind_rotation_successor(reserved.candidate, &fixture.parent, &reserved.child)
        .await
        .unwrap_or_else(|failure| panic!("bind the live rotation successor: {failure:?}"));
    let run = run_behind_held_stripes(
        &manager.store,
        Some(vec![fixture.custody_id]),
        HOLD,
        runtime.finalize_rotation_predecessor(reserved.child.id, &bound),
    )
    .await;
    let finalized = run
        .assert_store_stayed_free("rotation finalize")
        .expect("the finalize completes once the stripe frees");
    assert!(finalized, "the predecessor is archived by the finalize");
    assert_eq!(
        session_status(&manager, fixture.parent.id).await,
        SessionStatus::Archived
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rotation_failure_settlement_waits_for_a_held_stripe_without_pinning_the_store() {
    let (manager, dir) = rotation_manager();
    let fixture = persist_live_rotation_parent(&manager, dir.path(), None).await;
    let reserved = reserve_live_successor(&manager, &fixture, "stripe-fail").await;
    let runtime = manager.custody_execution_runtime();
    let bound = runtime
        .bind_rotation_successor(reserved.candidate, &fixture.parent, &reserved.child)
        .await
        .unwrap_or_else(|failure| panic!("bind the live rotation successor: {failure:?}"));
    let run = run_behind_held_stripes(
        &manager.store,
        Some(vec![fixture.custody_id]),
        HOLD,
        runtime.settle_bound_rotation_failure(
            reserved.child.id,
            &bound,
            SandboxCustodyErrorCodeV1::PersistenceTransitionFailed,
        ),
    )
    .await;
    run.assert_store_stayed_free("rotation failure settlement")
        .expect("the settlement completes once the stripe frees");
    assert_eq!(
        session_status(&manager, reserved.child.id).await,
        SessionStatus::Failed
    );
}

/// The retry controller cleanup settles a bound retry successor Failed. The
/// retry fence needs a retry-cause transfer and a retry-purpose invocation, so
/// the fixture binds the reserved successor with a Retry transfer directly.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_successor_failure_waits_for_a_held_stripe_without_pinning_the_store() {
    use crate::store::sandbox_custody::{
        BoundRotationCustody, CustodyCause, SessionCustodyBinding,
        fail_established_retry_successor_store_first,
    };

    let (manager, dir) = rotation_manager();
    let fixture = persist_live_rotation_parent(&manager, dir.path(), None).await;
    let reserved = reserve_live_successor(&manager, &fixture, "stripe-retry").await;
    {
        let mut store = manager.store.lock().await;
        store
            .bind_reserved_session_custody(
                reserved.child.id,
                SessionCustodyBinding::Transfer {
                    custody_id: fixture.custody_id,
                    from_session_id: fixture.parent.id,
                    generation: 1,
                    cause: CustodyCause::Retry,
                    origin_session_id: Some(fixture.parent.id),
                    scheduled_job_id: None,
                },
            )
            .expect("bind the reserved successor with a retry transfer");
        store
            .conn
            .execute(
                "UPDATE model_invocations SET purpose='session.retry.auto' WHERE id=?1",
                [reserved.invocation_id.to_string()],
            )
            .expect("mark the invocation as a retry admission");
    }
    let run = run_behind_held_stripes(
        &manager.store,
        Some(vec![fixture.custody_id]),
        HOLD,
        fail_established_retry_successor_store_first(
            &manager.store,
            reserved.child.id,
            BoundRotationCustody::Transfer {
                custody_id: fixture.custody_id,
                predecessor_id: fixture.parent.id,
                generation: 2,
            },
            SandboxCustodyErrorCodeV1::PersistenceTransitionFailed,
        ),
    )
    .await;
    run.assert_store_stayed_free("retry successor failure")
        .expect("the settlement completes once the stripe frees");
    assert_eq!(
        session_status(&manager, reserved.child.id).await,
        SessionStatus::Failed
    );
}

/// A bound retry successor whose deferred provider launch failed its final
/// scratch revalidation, with its exact incarnation published in the active
/// registry (#1179).
struct DeferredScratchFailure {
    reserved: ReservedSuccessor,
    bound: crate::sandbox::custody::BoundRetryCustody,
    permit: crate::model_control::AdmissionPermit,
    custody_id: Uuid,
}

const DEFERRED_GENERATION: u64 = 7;

async fn bind_deferred_scratch_failure(
    manager: &SessionManager,
    fixture: &LiveRotationFixture,
) -> DeferredScratchFailure {
    use crate::store::sandbox_custody::{CustodyCause, SessionCustodyBinding};

    let reserved = reserve_live_successor(manager, fixture, "stripe-deferred").await;
    {
        let mut store = manager.store.lock().await;
        store
            .bind_reserved_session_custody(
                reserved.child.id,
                SessionCustodyBinding::Transfer {
                    custody_id: fixture.custody_id,
                    from_session_id: fixture.parent.id,
                    generation: 1,
                    cause: CustodyCause::Retry,
                    origin_session_id: Some(fixture.parent.id),
                    scheduled_job_id: None,
                },
            )
            .expect("bind the retry successor to the transferred root");
    }
    let mut tracked = super::super::types::TrackedSession::new_for_test(reserved.child.clone());
    tracked.spawn_generation = DEFERRED_GENERATION;
    manager
        .active
        .write()
        .await
        .insert(reserved.child.id, tracked);
    DeferredScratchFailure {
        bound: crate::sandbox::custody::BoundRetryCustody::transfer_for_test(
            fixture.custody_id,
            fixture.parent.id,
            2,
        ),
        permit: crate::model_control::AdmissionPermit::for_invocation_test(reserved.invocation_id),
        custody_id: fixture.custody_id,
        reserved,
    }
}

async fn settle_deferred_scratch_failure(
    manager: &SessionManager,
    failure: &DeferredScratchFailure,
) {
    let runtime = manager.custody_execution_runtime();
    super::super::launch::settle_failed_deferred_execution_scratch(
        &manager.active,
        &manager.store,
        manager.event_bus(),
        &manager.agent_tokens,
        &runtime,
        failure.reserved.child.id,
        &failure.permit,
        DEFERRED_GENERATION,
        "deferred-scratch-token",
        Some(&failure.bound),
        None,
        None,
    )
    .await;
}

fn active_registry_probe(
    manager: &SessionManager,
) -> (&'static str, crate::store::stripe_liveness_support::ProbeFn) {
    let active = Arc::clone(&manager.active);
    (
        "active registry",
        Box::new(move || drop(active.blocking_read())),
    )
}

/// The deferred failure settlement waits Store-free for the root's stripe, and
/// must not hold the global active registry across that wait: a maintenance
/// proof on the stripe may last a whole proof, and session listing and every
/// other registry user would stall behind it. Both the Store and the active
/// registry stay answerable, and the exact incarnation is still removed once
/// the stripe frees.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deferred_scratch_failure_waits_for_a_held_stripe_without_pinning_store_or_active_registry()
{
    let (manager, dir) = rotation_manager();
    let fixture = persist_live_rotation_parent(&manager, dir.path(), None).await;
    let failure = bind_deferred_scratch_failure(&manager, &fixture).await;
    let run = crate::store::stripe_liveness_support::run_behind_held_stripes_within(
        &manager.store,
        Some(vec![failure.custody_id]),
        Duration::from_secs(1),
        crate::store::stripe_liveness_support::ACKNOWLEDGEMENT_TIMEOUT,
        vec![active_registry_probe(&manager)],
        settle_deferred_scratch_failure(&manager, &failure),
    )
    .await;
    run.assert_store_stayed_free("deferred scratch failure settlement");
    assert_eq!(
        session_status(&manager, failure.reserved.child.id).await,
        SessionStatus::Failed
    );
    assert!(
        manager
            .active
            .read()
            .await
            .get(&failure.reserved.child.id)
            .is_none(),
        "the exact incarnation is removed once the settlement lands"
    );
}

/// The registry is free while the settlement waits, so a replacement
/// incarnation of the same Session can register meanwhile. The settlement must
/// then leave the replacement tracked: only the exact spawn generation and
/// invocation it fenced are removed.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deferred_scratch_failure_leaves_a_replacement_incarnation_tracked() {
    let (manager, dir) = rotation_manager();
    let fixture = persist_live_rotation_parent(&manager, dir.path(), None).await;
    let failure = bind_deferred_scratch_failure(&manager, &fixture).await;
    let contended =
        crate::store::custody_lock_order::admission_contention_signal(failure.custody_id);
    let replace = async {
        let waited = tokio::time::Instant::now();
        while contended.try_recv().is_err() {
            assert!(
                waited.elapsed() < Duration::from_secs(20),
                "the settlement never contended on the held stripe"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let mut replacement =
            super::super::types::TrackedSession::new_for_test(failure.reserved.child.clone());
        replacement.spawn_generation = DEFERRED_GENERATION + 1;
        manager
            .active
            .write()
            .await
            .insert(failure.reserved.child.id, replacement);
    };
    let run = run_behind_held_stripes(
        &manager.store,
        Some(vec![failure.custody_id]),
        Duration::from_secs(1),
        async { tokio::join!(settle_deferred_scratch_failure(&manager, &failure), replace) },
    )
    .await;
    run.assert_store_stayed_free("deferred scratch failure with a replacement");
    assert_eq!(
        session_status(&manager, failure.reserved.child.id).await,
        SessionStatus::Failed
    );
    let active = manager.active.read().await;
    let tracked = active
        .get(&failure.reserved.child.id)
        .expect("the replacement incarnation stays tracked");
    assert_eq!(tracked.spawn_generation, DEFERRED_GENERATION + 1);
}
