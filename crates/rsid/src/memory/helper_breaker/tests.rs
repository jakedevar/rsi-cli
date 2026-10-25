//! #261: the helper endpoint breaker's states, driven by explicit clocks and a
//! fake transport (a future that counts its calls and returns a scripted
//! outcome); no network.

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-memory-01"))]
use super::*;
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-memory-01"))]
use std::sync::atomic::{AtomicU32, Ordering};

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-memory-01"))]
const ENDPOINT: &str = "localhost:11434";

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-memory-01"))]
fn call(purpose: &str) -> HelperCall<'_> {
    HelperCall {
        purpose,
        setting_key: "memory_model_fallback_provider",
        model: "gemma4:e4b",
        gate: true,
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-memory-01"))]
/// One failed probe or opening call at `now`.
fn fail(breaker: &HelperEndpointBreaker, admission: Admission, now: Instant) -> Transition {
    breaker.report(
        ENDPOINT,
        &call("session.summary"),
        admission,
        Outcome::Unreachable,
        now,
        Utc::now(),
    )
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-memory-01"))]
#[test]
fn a_closed_breaker_passes_and_the_first_unreachable_call_opens_it_once() {
    let breaker = HelperEndpointBreaker::default();
    let start = Instant::now();
    let summary = call("session.summary");
    assert_eq!(breaker.admit(ENDPOINT, &summary, start), Admission::Pass);
    assert_eq!(fail(&breaker, Admission::Pass, start), Transition::Opened);
    // Calls already in flight when it opened change nothing.
    assert_eq!(fail(&breaker, Admission::Pass, start), Transition::None);
    assert_eq!(
        breaker.admit(ENDPOINT, &summary, start),
        Admission::Reject {
            retry_in: INITIAL_BACKOFF
        }
    );
    // Exactly one friction event for the outage, and it drains once.
    let friction = breaker.take_pending_friction();
    assert_eq!(friction.len(), 1);
    assert_eq!(
        friction[0].signature,
        "helper_llm:session_summary:endpoint_unreachable"
    );
    assert!(breaker.take_pending_friction().is_empty());
    let outages = breaker.outages(start);
    assert_eq!(outages.len(), 1);
    assert_eq!(outages[0].endpoint, ENDPOINT);
    assert_eq!(outages[0].setting_key, "memory_model_fallback_provider");
    assert_eq!(outages[0].next_probe_in_secs, INITIAL_BACKOFF.as_secs());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-memory-01"))]
#[test]
fn an_open_breaker_admits_one_probe_per_window_and_backs_off_to_the_cap() {
    let breaker = HelperEndpointBreaker::default();
    let summary = call("session.summary");
    let mut now = Instant::now();
    fail(&breaker, Admission::Pass, now);
    let mut window = INITIAL_BACKOFF;
    let mut seen = Vec::new();
    for _ in 0..10 {
        // Inside the window: refused, however many callers ask.
        for _ in 0..5 {
            assert!(matches!(
                breaker.admit(ENDPOINT, &summary, now + window / 2),
                Admission::Reject { .. }
            ));
        }
        now += window;
        assert_eq!(breaker.admit(ENDPOINT, &summary, now), Admission::Probe);
        // While the probe runs, nobody else gets through.
        assert!(matches!(
            breaker.admit(ENDPOINT, &summary, now),
            Admission::Reject { .. }
        ));
        let Transition::StillOpen { backoff } = fail(&breaker, Admission::Probe, now) else {
            panic!("a failed probe keeps the breaker open");
        };
        assert_eq!(backoff, (window * 2).min(MAX_BACKOFF));
        window = backoff;
        seen.push(backoff.as_secs());
    }
    assert_eq!(&seen[..6], &[120, 240, 480, 960, 1800, 1800]);
    assert!(seen.iter().all(|secs| *secs <= MAX_BACKOFF.as_secs()));
    // Still one friction event for the whole outage.
    assert_eq!(breaker.take_pending_friction().len(), 1);
    assert_eq!(breaker.outages(now)[0].failed_probes, 10);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-memory-01"))]
#[test]
fn an_answer_closes_the_breaker_and_a_later_outage_is_new() {
    let breaker = HelperEndpointBreaker::default();
    let summary = call("session.summary");
    let start = Instant::now();
    fail(&breaker, Admission::Pass, start);
    let due = start + INITIAL_BACKOFF;
    assert_eq!(breaker.admit(ENDPOINT, &summary, due), Admission::Probe);
    assert_eq!(
        breaker.report(
            ENDPOINT,
            &summary,
            Admission::Probe,
            Outcome::Reachable,
            due,
            Utc::now()
        ),
        Transition::Recovered {
            after: INITIAL_BACKOFF
        }
    );
    assert_eq!(breaker.admit(ENDPOINT, &summary, due), Admission::Pass);
    assert!(breaker.outages(due).is_empty());
    assert_eq!(fail(&breaker, Admission::Pass, due), Transition::Opened);
    assert_eq!(breaker.take_pending_friction().len(), 2);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-memory-01"))]
#[test]
fn a_probe_without_a_verdict_frees_its_slot_and_a_lost_probe_goes_stale() {
    let breaker = HelperEndpointBreaker::default();
    let summary = call("session.summary");
    let start = Instant::now();
    fail(&breaker, Admission::Pass, start);
    let due = start + INITIAL_BACKOFF;
    assert_eq!(breaker.admit(ENDPOINT, &summary, due), Admission::Probe);
    breaker.report(
        ENDPOINT,
        &summary,
        Admission::Probe,
        Outcome::Unknown,
        due,
        Utc::now(),
    );
    assert_eq!(breaker.admit(ENDPOINT, &summary, due), Admission::Probe);
    // That probe's caller vanished: the slot is held until it goes stale.
    assert!(matches!(
        breaker.admit(ENDPOINT, &summary, due + PROBE_STALE_AFTER / 2),
        Admission::Reject { .. }
    ));
    assert_eq!(
        breaker.admit(ENDPOINT, &summary, due + PROBE_STALE_AFTER),
        Admission::Probe
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-memory-01"))]
#[test]
fn an_ungated_call_always_runs_and_other_purposes_are_named() {
    let breaker = HelperEndpointBreaker::default();
    let start = Instant::now();
    fail(&breaker, Admission::Pass, start);
    let operator = HelperCall {
        gate: false,
        ..call("text.generate.rpc")
    };
    assert_eq!(breaker.admit(ENDPOINT, &operator, start), Admission::Pass);
    let title = HelperCall {
        setting_key: "title_model_provider",
        ..call("session.title")
    };
    assert!(matches!(
        breaker.admit(ENDPOINT, &title, start),
        Admission::Reject { .. }
    ));
    let outage = &breaker.outages(start)[0];
    assert_eq!(outage.purpose, "session.summary");
    assert_eq!(
        outage.other_purposes,
        vec!["session.title(title_model_provider)".to_owned()]
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-memory-01"))]
#[test]
fn loopback_urls_share_one_key_and_remote_urls_are_not_gated() {
    for url in [
        "http://localhost:11434/v1",
        "http://127.0.0.1:11434/api/generate",
        "http://[::1]:11434/v1",
    ] {
        assert_eq!(
            loopback_endpoint_key(url).as_deref(),
            Some("localhost:11434"),
            "{url}"
        );
    }
    assert_eq!(
        loopback_endpoint_key("http://localhost/v1").as_deref(),
        Some("localhost:80")
    );
    assert_eq!(loopback_endpoint_key("https://api.example.test/v1"), None);
    assert_eq!(loopback_endpoint_key("not a url"), None);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-memory-01"))]
#[test]
fn the_refusal_names_the_setting_and_the_fix_and_is_recognised() {
    let message = refusal_message(ENDPOINT, &call("session.summary"), Duration::from_secs(90));
    assert!(message.starts_with(REFUSAL_CODE), "{message}");
    assert!(
        message.contains("memory_model_fallback_provider=gemma4:e4b"),
        "{message}"
    );
    assert!(message.contains("next probe in 90s"), "{message}");
    assert!(message.contains("ollama pull gemma4:e4b"), "{message}");
    assert!(is_refusal(&DaemonError::PolicyDenied(message.clone())));
    assert!(is_refusal(&DaemonError::Store(message)));
    assert!(!is_refusal(&DaemonError::PolicyDenied("other".into())));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-memory-01"))]
/// The fake transport: counts its calls and answers with a scripted outcome.
struct FakeEndpoint {
    calls: AtomicU32,
    up: std::sync::atomic::AtomicBool,
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-memory-01"))]
impl FakeEndpoint {
    async fn call(&self) -> (Result<&'static str, &'static str>, Outcome) {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.up.load(Ordering::SeqCst) {
            (Ok("summary"), Outcome::Reachable)
        } else {
            (Err("connection refused"), Outcome::Unreachable)
        }
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-memory-01"))]
#[tokio::test]
async fn many_summary_thresholds_against_a_dead_endpoint_reach_it_once_per_window() {
    let breaker = HelperEndpointBreaker::default();
    let fake = FakeEndpoint {
        calls: AtomicU32::new(0),
        up: std::sync::atomic::AtomicBool::new(false),
    };
    let summary = call("session.summary");
    // 50 summary thresholds inside the first window: one call reaches the
    // endpoint (the one that opens the breaker), the rest are refused.
    let mut refused = 0;
    for _ in 0..50 {
        match breaker.guarded(ENDPOINT, &summary, fake.call()).await {
            Guarded::Rejected(message) => {
                assert!(message.starts_with(REFUSAL_CODE));
                refused += 1;
            }
            Guarded::Ran { value } => assert_eq!(value, Err("connection refused")),
        }
    }
    assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
    assert_eq!(refused, 49);
    assert_eq!(breaker.take_pending_friction().len(), 1);

    // The endpoint comes back; once the window is over the probe gets through
    // and closes the breaker, with no restart.
    fake.up.store(true, Ordering::SeqCst);
    let due = Instant::now() + INITIAL_BACKOFF;
    assert_eq!(breaker.admit(ENDPOINT, &summary, due), Admission::Probe);
    let (reply, outcome) = fake.call().await;
    // The outage opened a moment before `due - INITIAL_BACKOFF`.
    let transition = breaker.report(
        ENDPOINT,
        &summary,
        Admission::Probe,
        outcome,
        due,
        Utc::now(),
    );
    assert!(
        matches!(transition, Transition::Recovered { after } if after >= INITIAL_BACKOFF),
        "{transition:?}"
    );
    assert_eq!(reply, Ok("summary"));
    let Guarded::Ran { value } = breaker.guarded(ENDPOINT, &summary, fake.call()).await else {
        panic!("a closed breaker runs the call");
    };
    assert_eq!(value, Ok("summary"));
    assert!(breaker.outages(Instant::now()).is_empty());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-memory-01"))]
#[tokio::test]
async fn the_listen_check_tells_a_closed_port_from_an_open_one() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback listener");
    let port = listener.local_addr().expect("address").port();
    assert!(listening(&format!("localhost:{port}")).await);
    drop(listener);
    assert!(!listening(&format!("localhost:{port}")).await);
}
