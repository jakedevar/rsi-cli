//! RSI-010 capability-class validator integration test.
//!
//! Verifies the declared-class → actual-model comparison logic used by the
//! monitor stream loop. The live validator lives inside a hot loop and is
//! tightly coupled to provider stream events, so this test exercises the
//! decision surface (classify + compare + dedupe) against a minimal fixture
//! rather than spinning up a full provider subprocess.

use rsi_common::types::CapabilityClass;
use rsid::bus::{DaemonEvent, EventBus};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast::error::TryRecvError;

/// Dedupe state for the per-session last-warn tuple (mirrors
/// `TrackedSession.last_mismatch_warn`).
type MismatchMemo = Option<(CapabilityClass, String)>;

/// Pure reproduction of the validator's decision rule so we can exercise
/// it without wiring up a provider subprocess. Any divergence from the
/// production hook in `crates/rsid/src/session/monitor.rs` is a bug.
fn maybe_warn(
    declared: Option<CapabilityClass>,
    model: &str,
    last: &mut MismatchMemo,
    event_bus: &EventBus,
    session_id: uuid::Uuid,
) -> bool {
    let Some(declared) = declared else {
        return false;
    };
    let Some(actual) = CapabilityClass::classify(model) else {
        return false; // unknown model → no comparison
    };
    if actual == declared {
        return false;
    }
    let duplicate = last
        .as_ref()
        .is_some_and(|(d, m)| *d == declared && m == model);
    if duplicate {
        return false;
    }
    event_bus.publish(DaemonEvent::SystemMessage {
        level: "warn".to_string(),
        message: format!(
            "session {} declared {:?} but running {} ({:?})",
            session_id, declared, model, actual
        ),
    });
    *last = Some((declared, model.to_string()));
    true
}

async fn recv_with_timeout(
    rx: &mut tokio::sync::broadcast::Receiver<Arc<DaemonEvent>>,
) -> Option<Arc<DaemonEvent>> {
    tokio::time::timeout(Duration::from_millis(200), rx.recv())
        .await
        .ok()
        .and_then(|r| r.ok())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn validator_warns_on_architect_declared_sonnet_actual() {
    let bus = Arc::new(EventBus::new(32));
    let mut rx = bus.subscribe();
    let mut memo: MismatchMemo = None;
    let session_id = uuid::Uuid::new_v4();

    let warned = maybe_warn(
        Some(CapabilityClass::Architect),
        "claude-sonnet-5",
        &mut memo,
        &bus,
        session_id,
    );
    assert!(warned, "declared=Architect + sonnet must warn");

    let event = recv_with_timeout(&mut rx)
        .await
        .expect("SystemMessage should be published");
    match &*event {
        DaemonEvent::SystemMessage { level, message } => {
            assert_eq!(level, "warn");
            assert!(message.contains("Architect"), "message: {}", message);
            assert!(message.contains("Implementer"), "message: {}", message);
            assert!(
                message.contains(&session_id.to_string()),
                "message should contain session id: {}",
                message
            );
        }
        other => panic!("expected SystemMessage, got {:?}", other),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn validator_silent_on_match() {
    let bus = Arc::new(EventBus::new(32));
    let mut rx = bus.subscribe();
    let mut memo: MismatchMemo = None;

    // Architect declared + Opus actual → match, no warn.
    let warned = maybe_warn(
        Some(CapabilityClass::Architect),
        "claude-opus-4-5",
        &mut memo,
        &bus,
        uuid::Uuid::new_v4(),
    );
    assert!(!warned);

    let got = recv_with_timeout(&mut rx).await;
    assert!(got.is_none(), "no SystemMessage should be published");
    match rx.try_recv() {
        Err(TryRecvError::Empty) => {}
        Err(TryRecvError::Closed) => {} // ok
        Err(TryRecvError::Lagged(_)) => panic!("unexpected lag"),
        Ok(_) => panic!("unexpected event emitted"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn validator_silent_on_no_declared_class() {
    let bus = Arc::new(EventBus::new(32));
    let mut rx = bus.subscribe();
    let mut memo: MismatchMemo = None;

    let warned = maybe_warn(
        None,
        "claude-opus-4-5",
        &mut memo,
        &bus,
        uuid::Uuid::new_v4(),
    );
    assert!(!warned);
    assert!(recv_with_timeout(&mut rx).await.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn validator_silent_on_unknown_model() {
    let bus = Arc::new(EventBus::new(32));
    let mut rx = bus.subscribe();
    let mut memo: MismatchMemo = None;

    // Unknown model → classify returns None → no comparison.
    let warned = maybe_warn(
        Some(CapabilityClass::Architect),
        "llama-3-70b",
        &mut memo,
        &bus,
        uuid::Uuid::new_v4(),
    );
    assert!(!warned);
    assert!(recv_with_timeout(&mut rx).await.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn validator_dedupes_repeated_emits() {
    let bus = Arc::new(EventBus::new(32));
    let mut rx = bus.subscribe();
    let mut memo: MismatchMemo = None;
    let sid = uuid::Uuid::new_v4();

    // First call: warn.
    let w1 = maybe_warn(
        Some(CapabilityClass::LookupFast),
        "claude-opus-4-5",
        &mut memo,
        &bus,
        sid,
    );
    assert!(w1);
    assert!(recv_with_timeout(&mut rx).await.is_some());

    // Second call with same (declared, model) tuple: silent.
    let w2 = maybe_warn(
        Some(CapabilityClass::LookupFast),
        "claude-opus-4-5",
        &mut memo,
        &bus,
        sid,
    );
    assert!(!w2);
    assert!(recv_with_timeout(&mut rx).await.is_none());

    // Third call with a different model: warn again (distinct tuple).
    let w3 = maybe_warn(
        Some(CapabilityClass::LookupFast),
        "claude-sonnet-5",
        &mut memo,
        &bus,
        sid,
    );
    assert!(w3);
    assert!(recv_with_timeout(&mut rx).await.is_some());
}
