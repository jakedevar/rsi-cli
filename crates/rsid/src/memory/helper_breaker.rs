//! A circuit breaker per local helper-LLM endpoint (#261).
//!
//! Titles, session summaries, observation extraction and prompt compile call a
//! helper model on a loopback endpoint (Ollama on `localhost:11434` by
//! default). On a host where that endpoint is not installed or not listening,
//! every call failed `transport_error` forever: hundreds of failed summaries,
//! a friction event every hour and nothing that told the operator what to fix.
//!
//! The breaker classifies "nothing listens there" (connection refused, no
//! route, connect timeout) apart from a transient failure. The first such
//! failure OPENS the endpoint's breaker: one log line, one friction event
//! (`helper_llm:<purpose>:endpoint_unreachable`) and one `helper_llm_unreachable`
//! health condition naming the purpose, the setting key, the model and the
//! fix. While open, callers are refused at once (no admission, no model call)
//! until the backoff window ends; then exactly one caller is let through as
//! the half-open probe. A failed probe doubles the window (60 s up to 30
//! minutes); any answer from the endpoint closes the breaker, so helpers
//! resume without a restart. The breaker never reroutes a call: choosing a
//! paid remote helper stays the operator's decision.
//!
//! State is in memory and per endpoint (`host:port`, loopback hosts folded to
//! `localhost`), shared by every purpose that uses that endpoint. Remote
//! endpoints are not gated.

use chrono::{DateTime, Utc};
use rsi_common::friction::{FrictionKind, NewFrictionEventV1};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::error::DaemonError;
use crate::health_anomaly::HelperOutage;

/// The first backoff window after the breaker opens.
pub(crate) const INITIAL_BACKOFF: Duration = Duration::from_secs(60);
/// The longest backoff window: at most one probe per half hour.
pub(crate) const MAX_BACKOFF: Duration = Duration::from_secs(30 * 60);
/// A probe that never reported (its caller was dropped) stops holding the
/// half-open slot after this long.
pub(crate) const PROBE_STALE_AFTER: Duration = Duration::from_secs(5 * 60);
/// How long the "does anything listen there" check waits for a connect.
pub(crate) const LISTEN_CHECK_TIMEOUT: Duration = Duration::from_secs(2);
/// The refusal code a call refused by an open breaker carries.
pub(crate) const REFUSAL_CODE: &str = "helper_endpoint_unreachable";
/// Other purposes remembered on one outage (the opener is always named).
const MAX_OTHER_PURPOSES: usize = 8;

/// Who is calling: the helper purpose, the setting that chose the model and
/// the model. `gate: false` (an operator's explicit test call) always runs and
/// only reports its outcome.
#[derive(Debug, Clone, Copy)]
pub struct HelperCall<'a> {
    pub purpose: &'a str,
    pub setting_key: &'a str,
    pub model: &'a str,
    pub gate: bool,
}

/// What the breaker lets a caller do now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// Closed (or an ungated call): run normally.
    Pass,
    /// Open and its window is over: this caller is the one half-open probe.
    Probe,
    /// Open: do not call; the next probe is due in `retry_in`.
    Reject { retry_in: Duration },
}

/// What one call learned about the endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The endpoint answered (a reply, any HTTP status, or it accepts connects).
    Reachable,
    /// Nothing listens there: refused, no route, connect timeout.
    Unreachable,
    /// No verdict (cancelled, a read timeout after connecting, ...).
    Unknown,
}

/// What [`HelperEndpointBreaker::report`] changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition {
    None,
    /// The breaker just opened: a new outage.
    Opened,
    /// A probe failed; the next window is `backoff`.
    StillOpen {
        backoff: Duration,
    },
    /// The endpoint answered again after `after` of outage.
    Recovered {
        after: Duration,
    },
}

#[derive(Debug, Clone)]
struct Outage {
    purpose: String,
    setting_key: String,
    model: String,
    others: Vec<(String, String)>,
    opened_at: Instant,
    opened_wall: DateTime<Utc>,
    backoff: Duration,
    next_probe_at: Instant,
    probing_since: Option<Instant>,
    failed_probes: u32,
}

#[derive(Default)]
pub struct HelperEndpointBreaker {
    endpoints: Mutex<HashMap<String, Outage>>,
    pending_friction: Mutex<Vec<NewFrictionEventV1>>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl HelperEndpointBreaker {
    pub fn global() -> &'static Self {
        static BREAKER: OnceLock<HelperEndpointBreaker> = OnceLock::new();
        BREAKER.get_or_init(Self::default)
    }

    /// Whether `call` may reach `endpoint` now. A closed endpoint and an
    /// ungated call pass; an open one rejects until its window is over, then
    /// admits exactly one probe at a time.
    pub fn admit(&self, endpoint: &str, call: &HelperCall<'_>, now: Instant) -> Admission {
        let mut endpoints = lock(&self.endpoints);
        let Some(outage) = endpoints.get_mut(endpoint) else {
            return Admission::Pass;
        };
        if !call.gate {
            return Admission::Pass;
        }
        if !outage.others.iter().any(|(p, _)| p == call.purpose)
            && outage.purpose != call.purpose
            && outage.others.len() < MAX_OTHER_PURPOSES
        {
            outage
                .others
                .push((call.purpose.to_owned(), call.setting_key.to_owned()));
        }
        if let Some(since) = outage.probing_since
            && now.saturating_duration_since(since) < PROBE_STALE_AFTER
        {
            return Admission::Reject {
                retry_in: outage.backoff,
            };
        }
        if now < outage.next_probe_at {
            return Admission::Reject {
                retry_in: outage.next_probe_at.saturating_duration_since(now),
            };
        }
        outage.probing_since = Some(now);
        Admission::Probe
    }

    /// Record what a call admitted as `admission` learned.
    pub fn report(
        &self,
        endpoint: &str,
        call: &HelperCall<'_>,
        admission: Admission,
        outcome: Outcome,
        now: Instant,
        wall_now: DateTime<Utc>,
    ) -> Transition {
        let mut endpoints = lock(&self.endpoints);
        match outcome {
            Outcome::Reachable => endpoints
                .remove(endpoint)
                .map_or(Transition::None, |outage| Transition::Recovered {
                    after: now.saturating_duration_since(outage.opened_at),
                }),
            Outcome::Unknown => {
                if admission == Admission::Probe
                    && let Some(outage) = endpoints.get_mut(endpoint)
                {
                    // No verdict: free the probe slot; the window is already over.
                    outage.probing_since = None;
                }
                Transition::None
            }
            Outcome::Unreachable => match endpoints.get_mut(endpoint) {
                None => {
                    endpoints.insert(
                        endpoint.to_owned(),
                        Outage {
                            purpose: call.purpose.to_owned(),
                            setting_key: call.setting_key.to_owned(),
                            model: call.model.to_owned(),
                            others: Vec::new(),
                            opened_at: now,
                            opened_wall: wall_now,
                            backoff: INITIAL_BACKOFF,
                            next_probe_at: now + INITIAL_BACKOFF,
                            probing_since: None,
                            failed_probes: 0,
                        },
                    );
                    lock(&self.pending_friction).push(outage_friction_event(call.purpose));
                    Transition::Opened
                }
                // Only the probe moves the window: calls already in flight when
                // the breaker opened say nothing new.
                Some(outage) if admission == Admission::Probe => {
                    outage.backoff = (outage.backoff * 2).min(MAX_BACKOFF);
                    outage.next_probe_at = now + outage.backoff;
                    outage.probing_since = None;
                    outage.failed_probes = outage.failed_probes.saturating_add(1);
                    Transition::StillOpen {
                        backoff: outage.backoff,
                    }
                }
                Some(_) => Transition::None,
            },
        }
    }

    /// Every open endpoint, for the health view.
    pub fn outages(&self, now: Instant) -> Vec<HelperOutage> {
        let endpoints = lock(&self.endpoints);
        let mut outages: Vec<HelperOutage> = endpoints
            .iter()
            .map(|(endpoint, outage)| HelperOutage {
                endpoint: endpoint.clone(),
                purpose: outage.purpose.clone(),
                setting_key: outage.setting_key.clone(),
                model: outage.model.clone(),
                other_purposes: outage
                    .others
                    .iter()
                    .map(|(purpose, setting)| format!("{purpose}({setting})"))
                    .collect(),
                since: outage.opened_wall,
                next_probe_in_secs: outage
                    .next_probe_at
                    .saturating_duration_since(now)
                    .as_secs(),
                failed_probes: outage.failed_probes,
            })
            .collect();
        outages.sort_by(|a, b| a.endpoint.cmp(&b.endpoint));
        outages
    }

    /// The friction events of outages opened since the last call (one per
    /// outage), for a caller that holds the store.
    pub fn take_pending_friction(&self) -> Vec<NewFrictionEventV1> {
        std::mem::take(&mut *lock(&self.pending_friction))
    }

    /// Run `attempt` behind the breaker: refused while open, the one probe
    /// when due, and its outcome recorded and logged.
    pub async fn guarded<T>(
        &self,
        endpoint: &str,
        call: &HelperCall<'_>,
        attempt: impl std::future::Future<Output = (T, Outcome)>,
    ) -> Guarded<T> {
        let admission = self.admit(endpoint, call, Instant::now());
        if let Admission::Reject { retry_in } = admission {
            return Guarded::Rejected(refusal_message(endpoint, call, retry_in));
        }
        let (value, outcome) = attempt.await;
        self.record(endpoint, call, admission, outcome);
        Guarded::Ran { value }
    }

    /// [`Self::report`] now, logging the transition (an outage once when it
    /// opens, once when it recovers).
    pub fn record(
        &self,
        endpoint: &str,
        call: &HelperCall<'_>,
        admission: Admission,
        outcome: Outcome,
    ) -> Transition {
        let transition = self.report(
            endpoint,
            call,
            admission,
            outcome,
            Instant::now(),
            Utc::now(),
        );
        log_transition(endpoint, call, transition);
        transition
    }
}

/// What [`HelperEndpointBreaker::guarded`] did.
#[derive(Debug)]
pub enum Guarded<T> {
    /// The breaker is open; the message is the refusal to surface.
    Rejected(String),
    Ran {
        value: T,
    },
}

/// The friction event for one outage: `helper_llm:<purpose>:endpoint_unreachable`.
pub(crate) fn outage_friction_event(purpose: &str) -> NewFrictionEventV1 {
    let purpose = purpose.replace(['.', '-'], "_");
    NewFrictionEventV1::new(FrictionKind::HelperLlm, &[&purpose, "endpoint_unreachable"])
}

/// What an operator does about an outage.
pub(crate) fn fix_text(setting_key: &str, model: &str) -> String {
    format!(
        "install Ollama and run `ollama pull {model}` (or start it), or pick another helper \
         model for {setting_key} in Settings"
    )
}

/// The refusal a caller surfaces while the breaker is open.
pub(crate) fn refusal_message(endpoint: &str, call: &HelperCall<'_>, retry_in: Duration) -> String {
    format!(
        "{REFUSAL_CODE}: helper endpoint {endpoint} is not listening ({purpose}, {setting}={model}); \
         next probe in {secs}s; fix: {fix}",
        purpose = call.purpose,
        setting = call.setting_key,
        model = call.model,
        secs = retry_in.as_secs(),
        fix = fix_text(call.setting_key, call.model),
    )
}

/// True for an error an open breaker raised (not a model failure).
pub(crate) fn is_refusal(error: &DaemonError) -> bool {
    match error {
        DaemonError::PolicyDenied(message)
        | DaemonError::Store(message)
        | DaemonError::Rpc(message) => message.contains(REFUSAL_CODE),
        _ => false,
    }
}

fn log_transition(endpoint: &str, call: &HelperCall<'_>, transition: Transition) {
    match transition {
        Transition::None => {}
        Transition::Opened => tracing::error!(
            endpoint,
            purpose = call.purpose,
            setting = call.setting_key,
            model = call.model,
            fix = %fix_text(call.setting_key, call.model),
            "helper LLM endpoint is not listening; backing off (one probe per window, \
             up to 30 minutes) until it answers"
        ),
        Transition::StillOpen { backoff } => tracing::debug!(
            endpoint,
            purpose = call.purpose,
            backoff_secs = backoff.as_secs(),
            "helper LLM endpoint probe failed; still backing off"
        ),
        Transition::Recovered { after } => tracing::info!(
            endpoint,
            purpose = call.purpose,
            outage_secs = after.as_secs(),
            "helper LLM endpoint answers again; helper calls resume"
        ),
    }
}

/// The breaker key of a loopback endpoint URL (`localhost:<port>`), or `None`
/// for a remote or unparsable URL, which is not gated.
pub(crate) fn loopback_endpoint_key(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url.trim()).ok()?;
    let host = parsed.host_str()?;
    let bare = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    let loopback = bare.eq_ignore_ascii_case("localhost")
        || bare
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    if !loopback {
        return None;
    }
    let port = parsed.port_or_known_default()?;
    Some(format!("localhost:{port}"))
}

/// Whether anything accepts a TCP connect on a `localhost:<port>` key (IPv4
/// or IPv6 loopback). Used to tell "nothing listens" apart from a transient
/// transport failure when the HTTP client's error does not say.
pub(crate) async fn listening(endpoint: &str) -> bool {
    let Some(port) = endpoint
        .rsplit_once(':')
        .and_then(|(_, port)| port.parse::<u16>().ok())
    else {
        return false;
    };
    let addresses = [
        std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        std::net::SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, port)),
    ];
    for address in addresses {
        if let Ok(Ok(_)) = tokio::time::timeout(
            LISTEN_CHECK_TIMEOUT,
            tokio::net::TcpStream::connect(address),
        )
        .await
        {
            return true;
        }
    }
    false
}

/// Record the friction of outages opened since the last drain.
pub(crate) async fn drain_friction(store: &rsid_core::store_lock::Mutex<crate::store::Store>) {
    let events = HelperEndpointBreaker::global().take_pending_friction();
    if events.is_empty() {
        return;
    }
    let guard = store.lock().await;
    for event in &events {
        crate::friction::note_locked(&guard, event);
    }
}

#[cfg(test)]
mod tests;
