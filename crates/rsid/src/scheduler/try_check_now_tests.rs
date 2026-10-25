//! hub #3631: the nudge after a durable wake arm never blocks the receipt.

use super::*;

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn try_check_now_queues_a_check_when_there_is_room() {
    let (tx, mut rx) = mpsc::channel::<SchedulerCommand>(1);
    let handle = SchedulerHandle::new(tx);
    assert!(handle.try_check_now());
    assert!(matches!(rx.recv().await, Some(SchedulerCommand::CheckNow)));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn try_check_now_answers_at_once_while_the_scheduler_queue_is_full() {
    let (tx, _rx) = mpsc::channel::<SchedulerCommand>(1);
    let handle = SchedulerHandle::new(tx);
    assert!(handle.try_check_now());
    // The queue is full and nothing drains it: an awaiting nudge would hang
    // here, the non-blocking one returns at once with nothing queued.
    assert!(!handle.try_check_now());
    // The awaiting form really would have blocked on the same state.
    let blocked = tokio::time::timeout(std::time::Duration::from_millis(50), handle.check_now())
        .await
        .is_err();
    assert!(blocked);
}
