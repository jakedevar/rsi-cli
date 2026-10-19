//! Codex plan-window usage, parity with Claude's `rate_limit_event` (#2757).
//!
//! Claude reports its account's plan windows on its own stream, and the daemon
//! stores them in `provider_rate_limit_windows`. Codex has no such stream; its
//! app-server answers `account/rateLimits/read` instead. This module polls that
//! read on the operator setting `codex_rate_limit_poll_secs` (0 = off) and
//! stores the answer in the SAME table, keyed `(provider = Codex, window_key)`,
//! so launch admission (`manager_v2_provider_usage_gate`), the manager capacity
//! view, `GetHealthStatus.rate_limits`, `GetProviderRateLimits` and the TUI
//! status bar treat the two providers alike.
//!
//! * One read is a short-lived `codex app-server` over stdio (no model turn)
//!   that inherits `CODEX_HOME` like a launch, so it reads the login a launch
//!   uses ([`crate::codex_live_catalog::probe_app_server`]).
//! * Window keys follow the window length, so a five-hour and a seven-day
//!   window read exactly like Claude's (`five_hour`, `seven_day`); another
//!   length is `<mins>m`; a bucket other than the default is prefixed with its
//!   limit id.
//! * A failed read never errors a launch and never rewrites a stored row: the
//!   previous windows stay and the [`PollTracker`] overlays a `stale_reason` on
//!   what readers get back. A logged-out login stores nothing and is no error.

use crate::bus::{DaemonEvent, EventBus};
use crate::codex_live_catalog::{
    REASON_EXITED, REASON_MALFORMED, REASON_UNAUTHORIZED, probe_app_server, response_for,
    rpc_error_reason, send,
};
use crate::process_control::{BoundedLines, PROVIDER_MAX_LINE_BYTES};
use crate::store::Store;
use chrono::{DateTime, Utc};
use rsi_common::rpc::{
    ProviderRateLimitPollStatus, ProviderRateLimitSnapshot, ProviderRateLimitWindow,
};
use rsi_common::types::SessionProvider;
use serde_json::{Value, json};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, LazyLock, Mutex as StdMutex};
use std::time::Duration;
use tokio::sync::Mutex;

/// The read succeeded but the answer could not be stored.
pub(crate) const REASON_PERSIST_FAILED: &str = "persist_failed";

/// Wall time of one read.
const PROBE_TIMEOUT: Duration = Duration::from_secs(8);
/// While the poll is off or not yet due the loop wakes this often, so a changed
/// setting applies within one tick.
const IDLE_TICK: Duration = Duration::from_secs(15);
/// After a failed read the next one is not earlier than this (capped by the
/// poll interval), so a broken CLI costs one spawn per minute, not one per tick.
const FAILURE_RETRY_SECS: i64 = 60;

/// `status` values the snapshot carries (Claude's vocabulary).
const STATUS_ALLOWED: &str = "allowed";
const STATUS_REJECTED: &str = "rejected";

/// The `RateLimitReachedType` values of the Codex app-server protocol. The typed
/// value is a closed set; anything else is ignored rather than guessed at.
pub(crate) const RATE_LIMIT_REACHED_TYPES: [&str; 5] = [
    "rate_limit_reached",
    "workspace_owner_credits_depleted",
    "workspace_member_credits_depleted",
    "workspace_owner_usage_limit_reached",
    "workspace_member_usage_limit_reached",
];

/// The closed `RateLimitReachedType` a JSON value names, as its static string.
pub(crate) fn known_reached_type(value: &Value) -> Option<&'static str> {
    let text = value.as_str()?;
    RATE_LIMIT_REACHED_TYPES
        .into_iter()
        .find(|known| *known == text)
}

/// What one read found.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Reading {
    /// `None` when the login has no plan account (`account: null`) or the
    /// answer carried no window: nothing to store, and not an error.
    pub(crate) snapshot: Option<ProviderRateLimitSnapshot>,
    pub(crate) logged_out: bool,
}

fn window_base_key(window: &Value, slot: &str) -> String {
    match window.get("windowDurationMins").and_then(Value::as_i64) {
        Some(300) => "five_hour".to_string(),
        Some(10_080) => "seven_day".to_string(),
        Some(mins) if mins > 0 => format!("{mins}m"),
        _ => slot.to_string(),
    }
}

/// The windows of one `RateLimitSnapshot` object, keyed uniquely within `out`.
fn collect_windows(snapshot: &Value, prefix: Option<&str>, out: &mut Vec<ProviderRateLimitWindow>) {
    for slot in ["primary", "secondary"] {
        let Some(window) = snapshot.get(slot).filter(|window| window.is_object()) else {
            continue;
        };
        let Some(used_percent) = window.get("usedPercent").and_then(Value::as_f64) else {
            continue;
        };
        let mut key = window_base_key(window, slot);
        if let Some(prefix) = prefix {
            key = format!("{prefix}:{key}");
        }
        if out.iter().any(|existing| existing.window_key == key) {
            key = format!("{key}:{slot}");
        }
        out.push(ProviderRateLimitWindow {
            window_key: key,
            utilization: (used_percent / 100.0).max(0.0),
            resets_at_epoch: window.get("resetsAt").and_then(Value::as_i64),
        });
    }
}

fn overage_status(snapshot: &Value) -> Option<&'static str> {
    let credits = snapshot
        .get("credits")
        .filter(|credits| credits.is_object())?;
    Some(
        if credits.get("unlimited").and_then(Value::as_bool) == Some(true) {
            "unlimited"
        } else if credits.get("hasCredits").and_then(Value::as_bool) == Some(true) {
            "credits_available"
        } else {
            "no_credits"
        },
    )
}

/// Parse a `GetAccountRateLimitsResponse` result into an account snapshot.
///
/// `rateLimits` is the default bucket; every other bucket of
/// `rateLimitsByLimitId` adds its windows under a `<limit id>:` prefix. Returns
/// `Ok(None)` for an answer with no window and `Err` for one that is not a
/// rate-limit response at all.
pub(crate) fn parse_rate_limits(
    result: &Value,
    observed_at: DateTime<Utc>,
) -> Result<Option<ProviderRateLimitSnapshot>, &'static str> {
    let main = result
        .get("rateLimits")
        .filter(|main| main.is_object())
        .ok_or(REASON_MALFORMED)?;
    let main_id = main.get("limitId").and_then(Value::as_str);

    let mut windows = Vec::new();
    collect_windows(main, None, &mut windows);
    if let Some(buckets) = result.get("rateLimitsByLimitId").and_then(Value::as_object) {
        let mut ids: Vec<&String> = buckets.keys().collect();
        ids.sort();
        for id in ids {
            let bucket = &buckets[id];
            // The default bucket appears again under its own id.
            let is_default = main_id == Some(id.as_str())
                || (main_id.is_none() && id == "codex")
                || bucket == main;
            if is_default || !bucket.is_object() {
                continue;
            }
            collect_windows(bucket, Some(id), &mut windows);
        }
    }
    if windows.is_empty() {
        return Ok(None);
    }

    let reached = main
        .get("rateLimitReachedType")
        .and_then(known_reached_type);
    let blocked = reached.is_some()
        || result.get("ordinaryUsageAllowed").and_then(Value::as_bool) == Some(false)
        || main.get("spendControlReached").and_then(Value::as_bool) == Some(true)
        || result.get("spendControlReached").and_then(Value::as_bool) == Some(true)
        || windows.iter().any(|window| window.utilization >= 1.0);
    let binding = windows
        .iter()
        .max_by(|a, b| a.utilization.total_cmp(&b.utilization))
        .map(|window| window.window_key.clone());

    Ok(Some(ProviderRateLimitSnapshot {
        provider: SessionProvider::Codex,
        status: Some(
            if blocked {
                STATUS_REJECTED
            } else {
                STATUS_ALLOWED
            }
            .to_string(),
        ),
        // The provider's own typed reason when a window was hit, else the window
        // that will throttle first (Claude's `rateLimitType` meaning).
        rate_limit_type: reached.map(str::to_string).or(binding),
        overage_status: overage_status(main).map(str::to_string),
        is_using_overage: false,
        observed_at,
        windows,
        stale_reason: None,
    }))
}

/// Whether an `account/rateLimits/read` error means "no plan account here" (a
/// logged-out login, or an API-key login that has no plan windows).
fn error_means_no_plan_account(error: &Value) -> bool {
    let text = error.to_string().to_ascii_lowercase();
    rpc_error_reason(error) == REASON_UNAUTHORIZED || text.contains("authentication required")
}

async fn drive_rate_limit_read(
    mut stdin: tokio::process::ChildStdin,
    stdout: tokio::process::ChildStdout,
) -> Result<Reading, &'static str> {
    let mut lines = BoundedLines::new(stdout, PROVIDER_MAX_LINE_BYTES);
    send(
        &mut stdin,
        json!({
            "id": 1,
            "method": "initialize",
            "params": {"clientInfo": {"name": "rsid", "version": env!("CARGO_PKG_VERSION")}},
        }),
    )
    .await?;
    let init = response_for(&mut lines, 1).await?;
    if let Some(error) = init.get("error") {
        return Err(rpc_error_reason(error));
    }
    send(&mut stdin, json!({"method": "initialized"})).await?;

    // A login with no account has no windows to read: say so before asking.
    send(
        &mut stdin,
        json!({"id": 2, "method": "account/read", "params": {"refreshToken": false}}),
    )
    .await?;
    let account = response_for(&mut lines, 2).await?;
    if let Some(error) = account.get("error") {
        return Err(rpc_error_reason(error));
    }
    if account
        .pointer("/result/account")
        .is_none_or(Value::is_null)
    {
        return Ok(Reading {
            snapshot: None,
            logged_out: true,
        });
    }

    send(
        &mut stdin,
        json!({"id": 3, "method": "account/rateLimits/read"}),
    )
    .await?;
    let response = response_for(&mut lines, 3).await?;
    if let Some(error) = response.get("error") {
        if error_means_no_plan_account(error) {
            return Ok(Reading {
                snapshot: None,
                logged_out: true,
            });
        }
        return Err(rpc_error_reason(error));
    }
    let result = response.get("result").ok_or(REASON_EXITED)?;
    Ok(Reading {
        snapshot: parse_rate_limits(result, Utc::now())?,
        logged_out: false,
    })
}

/// Read the plan windows once through `codex app-server`.
pub(crate) async fn probe(binary: &Path, timeout: Duration) -> Result<Reading, &'static str> {
    probe_app_server(binary, timeout, drive_rate_limit_read).await
}

#[derive(Debug, Default)]
struct PollState {
    last_attempt: Option<DateTime<Utc>>,
    last_success: Option<DateTime<Utc>>,
    last_error: Option<&'static str>,
    logged_out: bool,
}

/// What the poll loop has seen, shared by the loop (writer) and the readers
/// that overlay staleness. In memory on purpose: the stored rows stay the last
/// good observation, and a restarted daemon polls again at once.
#[derive(Debug, Default)]
pub(crate) struct PollTracker {
    state: StdMutex<PollState>,
}

impl PollTracker {
    fn lock(&self) -> std::sync::MutexGuard<'_, PollState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn record_success(&self, at: DateTime<Utc>, logged_out: bool) {
        let mut state = self.lock();
        state.last_attempt = Some(at);
        state.last_success = Some(at);
        state.last_error = None;
        state.logged_out = logged_out;
    }

    fn record_failure(&self, at: DateTime<Utc>, reason: &'static str) {
        let mut state = self.lock();
        state.last_attempt = Some(at);
        state.last_error = Some(reason);
    }

    /// The reason the stored Codex windows are last-known rather than current.
    pub(crate) fn stale_reason(&self) -> Option<&'static str> {
        self.lock().last_error
    }

    /// Whether the loop should read now: never read, or the interval (shorter
    /// after a failure) has passed since the last attempt.
    fn due(&self, poll_secs: u32, now: DateTime<Utc>) -> bool {
        let state = self.lock();
        let Some(last) = state.last_attempt else {
            return true;
        };
        let wait = if state.last_error.is_some() {
            i64::from(poll_secs).min(FAILURE_RETRY_SECS)
        } else {
            i64::from(poll_secs)
        };
        (now - last).num_seconds() >= wait
    }

    /// Mark every Codex snapshot stale when the latest read failed.
    pub(crate) fn apply(&self, snapshots: &mut [ProviderRateLimitSnapshot]) {
        let Some(reason) = self.stale_reason() else {
            return;
        };
        for snapshot in snapshots
            .iter_mut()
            .filter(|snapshot| snapshot.provider == SessionProvider::Codex)
        {
            snapshot.stale_reason = Some(reason.to_string());
        }
    }

    pub(crate) fn status(&self, poll_secs: u32) -> ProviderRateLimitPollStatus {
        self.status_gated(poll_secs, crate::offline::is_offline())
    }

    /// [`Self::status`] with the offline mode (#130) injected: offline, the
    /// poll is off (`poll_secs: 0`) and `last_error` reads `offline`.
    pub(crate) fn status_gated(
        &self,
        poll_secs: u32,
        offline: bool,
    ) -> ProviderRateLimitPollStatus {
        let state = self.lock();
        ProviderRateLimitPollStatus {
            provider: SessionProvider::Codex,
            poll_secs: effective_poll_secs(poll_secs, offline),
            last_attempt_at: state.last_attempt,
            last_success_at: state.last_success,
            last_error: if offline {
                Some(crate::offline::REASON_OFFLINE.to_string())
            } else {
                state.last_error.map(str::to_string)
            },
            logged_out: state.logged_out,
        }
    }
}

static TRACKER: LazyLock<PollTracker> = LazyLock::new(PollTracker::default);

/// The daemon's one tracker.
pub(crate) fn tracker() -> &'static PollTracker {
    &TRACKER
}

/// How one poll ended.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PollOutcome {
    /// New windows were stored.
    Stored(Box<ProviderRateLimitSnapshot>),
    /// No plan account or no window: nothing stored, nothing wrong.
    NothingToStore { logged_out: bool },
    /// The read or the write failed; the previous windows stay, marked stale.
    Failed(&'static str),
}

async fn load_codex_snapshot(store: &Arc<Mutex<Store>>) -> Option<ProviderRateLimitSnapshot> {
    let store = Arc::clone(store);
    tokio::task::spawn_blocking(move || {
        let store = store.blocking_lock();
        store
            .load_provider_rate_limit_snapshots()
            .ok()
            .and_then(|all| {
                all.into_iter()
                    .find(|snapshot| snapshot.provider == SessionProvider::Codex)
            })
    })
    .await
    .ok()
    .flatten()
}

/// One read, one store write, one live event. Never returns an error: a
/// failure is an outcome, so nothing here can fail a launch.
pub(crate) async fn poll_once(
    tracker: &PollTracker,
    binary: &Path,
    timeout: Duration,
    store: &Arc<Mutex<Store>>,
    bus: &EventBus,
) -> PollOutcome {
    let attempted_at = Utc::now();
    let reading = match probe(binary, timeout).await {
        Ok(reading) => reading,
        Err(reason) => return fail(tracker, attempted_at, reason, store, bus).await,
    };
    let Some(snapshot) = reading.snapshot else {
        tracker.record_success(attempted_at, reading.logged_out);
        return PollOutcome::NothingToStore {
            logged_out: reading.logged_out,
        };
    };

    let to_store = snapshot.clone();
    let stored = tokio::task::spawn_blocking({
        let store = Arc::clone(store);
        move || {
            let store = store.blocking_lock();
            store.upsert_provider_rate_limit_snapshot(&to_store, None)
        }
    })
    .await;
    if !matches!(stored, Ok(Ok(()))) {
        tracing::warn!("Failed to persist Codex plan-window snapshot");
        return fail(tracker, attempted_at, REASON_PERSIST_FAILED, store, bus).await;
    }
    tracker.record_success(attempted_at, false);
    bus.publish(DaemonEvent::ProviderRateLimitUpdated {
        snapshot: snapshot.clone(),
    });
    PollOutcome::Stored(Box::new(snapshot))
}

async fn fail(
    tracker: &PollTracker,
    at: DateTime<Utc>,
    reason: &'static str,
    store: &Arc<Mutex<Store>>,
    bus: &EventBus,
) -> PollOutcome {
    let was = tracker.stale_reason();
    tracker.record_failure(at, reason);
    tracing::info!(
        reason,
        "Codex plan-window read failed; keeping the last windows"
    );
    // Tell live readers once per change, with the last known windows.
    if was != Some(reason)
        && let Some(mut snapshot) = load_codex_snapshot(store).await
    {
        tracker.apply(std::slice::from_mut(&mut snapshot));
        bus.publish(DaemonEvent::ProviderRateLimitUpdated { snapshot });
    }
    PollOutcome::Failed(reason)
}

/// The poll interval that applies: the setting, or 0 (off) while the daemon
/// is offline (#130), so an offline daemon never starts the app-server read.
pub(crate) const fn effective_poll_secs(configured: u32, offline: bool) -> u32 {
    if offline { 0 } else { configured }
}

/// The daemon's poll loop. The interval is read every cycle (live setting; 0
/// leaves the stored windows alone and stops reading).
pub async fn run_codex_rate_limit_poll_loop(
    store: Arc<Mutex<Store>>,
    bus: Arc<EventBus>,
    settings: Arc<crate::config::RuntimeConfig>,
) {
    loop {
        let secs = poll_tick(
            settings.codex_rate_limit_poll_secs.load(Ordering::Relaxed),
            crate::offline::is_offline(),
            || crate::provider_cli::resolve("codex"),
            &store,
            &bus,
        )
        .await;
        let wait = if secs == 0 {
            IDLE_TICK
        } else {
            IDLE_TICK.min(Duration::from_secs(u64::from(secs)))
        };
        tokio::time::sleep(wait).await;
    }
}

/// One cycle of the poll loop: read the windows when the interval that
/// applies ([`effective_poll_secs`]) is due. Returns that interval. Offline
/// (#130) it is 0, so neither the CLI lookup nor the app-server read runs.
pub(crate) async fn poll_tick(
    configured_secs: u32,
    offline: bool,
    resolve: impl FnOnce() -> Option<std::path::PathBuf> + Send + 'static,
    store: &Arc<Mutex<Store>>,
    bus: &EventBus,
) -> u32 {
    let secs = effective_poll_secs(configured_secs, offline);
    if secs != 0 && tracker().due(secs, Utc::now()) {
        let binary = tokio::task::spawn_blocking(resolve).await.ok().flatten();
        match binary {
            Some(binary) => {
                poll_once(tracker(), &binary, PROBE_TIMEOUT, store, bus).await;
            }
            // No Codex CLI on this host: nothing to read, and the stored
            // rows (if any) are not made stale by a host that never polled.
            None => tracker().record_success(Utc::now(), false),
        }
    }
    secs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex_live_catalog::StubCodex;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-16T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn plan_response() -> Value {
        json!({
            "rateLimits": {
                "limitId": "codex",
                "primary": {"usedPercent": 27, "windowDurationMins": 300, "resetsAt": 1_788_402_000},
                "secondary": {"usedPercent": 5, "windowDurationMins": 10080, "resetsAt": 1_788_900_000},
                "credits": {"hasCredits": true, "unlimited": false, "balance": "12"},
                "planType": "pro",
                "rateLimitReachedType": null
            },
            "rateLimitsByLimitId": {
                "codex": {
                    "limitId": "codex",
                    "primary": {"usedPercent": 27, "windowDurationMins": 300, "resetsAt": 1_788_402_000}
                },
                "codex_other": {
                    "limitId": "codex_other",
                    "primary": {"usedPercent": 61, "windowDurationMins": 60, "resetsAt": 1_788_300_000}
                }
            }
        })
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn plan_windows_become_rows_with_fractional_utilization() {
        let snapshot = parse_rate_limits(&plan_response(), now())
            .unwrap()
            .expect("a window");
        assert_eq!(snapshot.provider, SessionProvider::Codex);
        assert_eq!(snapshot.status.as_deref(), Some("allowed"));
        assert_eq!(
            snapshot.overage_status.as_deref(),
            Some("credits_available")
        );
        let rows: Vec<(&str, f64, Option<i64>)> = snapshot
            .windows
            .iter()
            .map(|w| (w.window_key.as_str(), w.utilization, w.resets_at_epoch))
            .collect();
        assert_eq!(
            rows,
            vec![
                ("five_hour", 0.27, Some(1_788_402_000)),
                ("seven_day", 0.05, Some(1_788_900_000)),
                ("codex_other:60m", 0.61, Some(1_788_300_000)),
            ],
            "the default bucket's repeat under its own id adds no row"
        );
        assert_eq!(snapshot.rate_limit_type.as_deref(), Some("codex_other:60m"));
        assert!(!snapshot.is_stale());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn a_reached_window_is_typed_and_rejected() {
        let mut response = plan_response();
        response["rateLimits"]["rateLimitReachedType"] = json!("workspace_member_credits_depleted");
        response["rateLimits"]["primary"]["usedPercent"] = json!(100);
        let snapshot = parse_rate_limits(&response, now()).unwrap().unwrap();
        assert_eq!(snapshot.status.as_deref(), Some("rejected"));
        assert_eq!(
            snapshot.rate_limit_type.as_deref(),
            Some("workspace_member_credits_depleted")
        );
        assert_eq!(snapshot.windows[0].utilization, 1.0);

        // A window at 100% is rejected even when the provider named no type.
        let mut response = plan_response();
        response["rateLimits"]["primary"]["usedPercent"] = json!(100);
        let snapshot = parse_rate_limits(&response, now()).unwrap().unwrap();
        assert_eq!(snapshot.status.as_deref(), Some("rejected"));
        assert_eq!(snapshot.rate_limit_type.as_deref(), Some("five_hour"));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn an_answer_without_windows_is_empty_and_a_non_answer_is_malformed() {
        let empty = json!({"rateLimits": {"planType": "free"}});
        assert_eq!(parse_rate_limits(&empty, now()), Ok(None));
        assert_eq!(
            parse_rate_limits(&json!({"nope": 1}), now()),
            Err(REASON_MALFORMED)
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn two_windows_of_one_length_keep_distinct_keys() {
        let response = json!({"rateLimits": {
            "primary": {"usedPercent": 10, "windowDurationMins": 300},
            "secondary": {"usedPercent": 20, "windowDurationMins": 300}
        }});
        let snapshot = parse_rate_limits(&response, now()).unwrap().unwrap();
        let keys: Vec<&str> = snapshot
            .windows
            .iter()
            .map(|w| w.window_key.as_str())
            .collect();
        assert_eq!(keys, ["five_hour", "five_hour:secondary"]);
        let unknown = json!({"rateLimits": {"primary": {"usedPercent": 10}}});
        let snapshot = parse_rate_limits(&unknown, now()).unwrap().unwrap();
        assert_eq!(snapshot.windows[0].window_key, "primary");
    }

    fn answering_stub(rate_limits: &str) -> StubCodex {
        StubCodex::with_app_server(&format!(
            r#"while IFS= read -r line; do
      case "$line" in
        *'"initialize"'*) echo '{{"id":1,"result":{{}}}}' ;;
        *'"account/read"'*) echo '{{"id":2,"result":{{"account":{{"type":"chatgpt","email":"x@example.test","planType":"pro"}},"requiresOpenaiAuth":true}}}}' ;;
        *'"account/rateLimits/read"'*) echo '{rate_limits}' ;;
      esac
    done"#
        ))
    }

    fn plan_reply() -> String {
        json!({"id": 3, "result": plan_response()}).to_string()
    }

    async fn store_and_bus() -> (Arc<Mutex<Store>>, Arc<EventBus>) {
        (
            Arc::new(Mutex::new(Store::open_in_memory().unwrap())),
            Arc::new(EventBus::new(16)),
        )
    }

    async fn stored_codex(store: &Arc<Mutex<Store>>) -> Option<ProviderRateLimitSnapshot> {
        load_codex_snapshot(store).await
    }

    /// #130: an offline daemon's poll cycle neither looks the CLI up nor
    /// starts the app-server read, and the status reports the typed offline
    /// state instead of an interval.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn an_offline_poll_cycle_starts_no_read_and_reports_offline() {
        let stub = answering_stub(&plan_reply());
        let (store, bus) = store_and_bus().await;
        let resolved = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let path = stub.path();
        let seen = Arc::clone(&resolved);
        let secs = poll_tick(
            30,
            true,
            move || {
                seen.store(true, std::sync::atomic::Ordering::SeqCst);
                Some(path)
            },
            &store,
            &bus,
        )
        .await;
        assert_eq!(secs, 0);
        assert!(!resolved.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(stub.probes(), 0);
        assert!(stored_codex(&store).await.is_none());

        let status = PollTracker::default().status_gated(300, true);
        assert_eq!(status.poll_secs, 0);
        assert_eq!(
            status.last_error.as_deref(),
            Some(crate::offline::REASON_OFFLINE)
        );
        assert_eq!(effective_poll_secs(300, false), 300);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn a_stub_app_server_answer_is_stored_beside_claude_and_published() {
        let stub = answering_stub(&plan_reply());
        let (store, bus) = store_and_bus().await;
        // A Claude row already in the table must be untouched.
        store
            .lock()
            .await
            .upsert_provider_rate_limit_snapshot(
                &ProviderRateLimitSnapshot {
                    provider: SessionProvider::Claude,
                    status: Some("allowed".into()),
                    rate_limit_type: Some("five_hour".into()),
                    overage_status: None,
                    is_using_overage: false,
                    observed_at: now(),
                    windows: vec![ProviderRateLimitWindow {
                        window_key: "five_hour".into(),
                        utilization: 0.4,
                        resets_at_epoch: Some(1),
                    }],
                    stale_reason: None,
                },
                None,
            )
            .unwrap();
        let mut events = bus.subscribe();
        let tracker = PollTracker::default();

        let outcome = poll_once(&tracker, &stub.path(), PROBE_TIMEOUT, &store, &bus).await;
        assert!(matches!(outcome, PollOutcome::Stored(_)), "{outcome:?}");

        let all = store
            .lock()
            .await
            .load_provider_rate_limit_snapshots()
            .unwrap();
        let providers: Vec<SessionProvider> = all.iter().map(|s| s.provider).collect();
        assert!(providers.contains(&SessionProvider::Claude));
        let codex = all
            .iter()
            .find(|s| s.provider == SessionProvider::Codex)
            .expect("a Codex snapshot");
        let mut rows: Vec<(String, f64)> = codex
            .windows
            .iter()
            .map(|w| (w.window_key.clone(), w.utilization))
            .collect();
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            rows,
            vec![
                ("codex_other:60m".to_string(), 0.61),
                ("five_hour".to_string(), 0.27),
                ("seven_day".to_string(), 0.05),
            ]
        );
        assert_eq!(codex.status.as_deref(), Some("allowed"));
        assert_eq!(
            codex
                .windows
                .iter()
                .find(|w| w.window_key == "five_hour")
                .and_then(|w| w.resets_at_epoch),
            Some(1_788_402_000)
        );
        let claude = all
            .iter()
            .find(|s| s.provider == SessionProvider::Claude)
            .unwrap();
        assert_eq!(claude.windows[0].utilization, 0.4);

        let event = tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .expect("an event")
            .expect("open bus");
        assert!(
            matches!(&*event, DaemonEvent::ProviderRateLimitUpdated { snapshot }
                if snapshot.provider == SessionProvider::Codex && snapshot.windows.len() == 3),
            "{event:?}"
        );
        assert_eq!(tracker.stale_reason(), None);
        assert!(tracker.status(300).last_success_at.is_some());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn a_failed_probe_keeps_the_last_snapshot_and_marks_it_stale_then_recovers() {
        let good = answering_stub(&plan_reply());
        let (store, bus) = store_and_bus().await;
        let tracker = PollTracker::default();
        let outcome = poll_once(&tracker, &good.path(), PROBE_TIMEOUT, &store, &bus).await;
        assert!(matches!(outcome, PollOutcome::Stored(_)));
        let before = stored_codex(&store).await.expect("stored");

        // The CLI now dies at spawn: the stored windows stay, marked stale.
        let missing = Path::new("/nonexistent/rsi-test/codex");
        let outcome = poll_once(&tracker, missing, PROBE_TIMEOUT, &store, &bus).await;
        assert_eq!(outcome, PollOutcome::Failed("probe_spawn_failed"));
        let after = stored_codex(&store).await.expect("rows survive a failure");
        assert_eq!(after.windows, before.windows);
        assert_eq!(
            after.observed_at, before.observed_at,
            "no row was rewritten"
        );
        let mut view = vec![after];
        tracker.apply(&mut view);
        assert_eq!(view[0].stale_reason.as_deref(), Some("probe_spawn_failed"));
        assert_eq!(
            tracker.status(300).last_error.as_deref(),
            Some("probe_spawn_failed")
        );

        // A later good read clears the mark.
        let outcome = poll_once(&tracker, &good.path(), PROBE_TIMEOUT, &store, &bus).await;
        assert!(matches!(outcome, PollOutcome::Stored(_)));
        let mut view = vec![stored_codex(&store).await.unwrap()];
        tracker.apply(&mut view);
        assert_eq!(view[0].stale_reason, None);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn a_logged_out_login_stores_no_rows_and_is_no_error() {
        let stub = StubCodex::with_app_server(
            r#"while IFS= read -r line; do
      case "$line" in
        *'"initialize"'*) echo '{"id":1,"result":{}}' ;;
        *'"account/read"'*) echo '{"id":2,"result":{"account":null,"requiresOpenaiAuth":true}}' ;;
        *'"account/rateLimits/read"'*) echo '{"id":3,"error":{"code":-32600,"message":"must not be asked"}}' ;;
      esac
    done"#,
        );
        let (store, bus) = store_and_bus().await;
        let tracker = PollTracker::default();
        let outcome = poll_once(&tracker, &stub.path(), PROBE_TIMEOUT, &store, &bus).await;
        assert_eq!(outcome, PollOutcome::NothingToStore { logged_out: true });
        assert!(
            store
                .lock()
                .await
                .load_provider_rate_limit_snapshots()
                .unwrap()
                .is_empty()
        );
        assert_eq!(tracker.stale_reason(), None);
        assert!(tracker.status(300).logged_out);
        assert_eq!(tracker.status(300).last_error, None);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn an_authentication_error_on_the_read_is_no_plan_account_not_a_failure() {
        let stub = answering_stub(
            r#"{"id":3,"error":{"code":-32600,"message":"codex account authentication required to read rate limits"}}"#,
        );
        let (store, bus) = store_and_bus().await;
        let tracker = PollTracker::default();
        let outcome = poll_once(&tracker, &stub.path(), PROBE_TIMEOUT, &store, &bus).await;
        assert_eq!(outcome, PollOutcome::NothingToStore { logged_out: true });
        assert_eq!(tracker.stale_reason(), None);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn an_rpc_error_or_garbled_answer_fails_without_touching_the_store() {
        for reply in [
            r#"{"id":3,"error":{"code":-32603,"message":"upstream exploded"}}"#,
            r#"{"id":3,"result":{"unexpected":true}}"#,
        ] {
            let stub = answering_stub(reply);
            let (store, bus) = store_and_bus().await;
            let tracker = PollTracker::default();
            let outcome = poll_once(&tracker, &stub.path(), PROBE_TIMEOUT, &store, &bus).await;
            assert!(matches!(outcome, PollOutcome::Failed(_)), "{outcome:?}");
            assert!(stored_codex(&store).await.is_none());
            assert!(tracker.stale_reason().is_some());
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn the_poll_is_due_by_interval_and_retries_a_failure_sooner() {
        let tracker = PollTracker::default();
        let t0 = now();
        assert!(tracker.due(300, t0), "never read: due at once");
        tracker.record_success(t0, false);
        assert!(!tracker.due(300, t0 + chrono::Duration::seconds(299)));
        assert!(tracker.due(300, t0 + chrono::Duration::seconds(300)));
        tracker.record_failure(t0, "probe_timeout");
        assert!(!tracker.due(300, t0 + chrono::Duration::seconds(59)));
        assert!(
            tracker.due(300, t0 + chrono::Duration::seconds(60)),
            "a failure retries after the shorter of the interval and a minute"
        );
        assert!(
            tracker.due(30, t0 + chrono::Duration::seconds(30)),
            "never later than the interval itself"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn only_codex_snapshots_take_the_stale_mark() {
        let tracker = PollTracker::default();
        tracker.record_failure(now(), "probe_timeout");
        let base = |provider| ProviderRateLimitSnapshot {
            provider,
            status: None,
            rate_limit_type: None,
            overage_status: None,
            is_using_overage: false,
            observed_at: now(),
            windows: Vec::new(),
            stale_reason: None,
        };
        let mut view = vec![base(SessionProvider::Claude), base(SessionProvider::Codex)];
        tracker.apply(&mut view);
        assert_eq!(view[0].stale_reason, None);
        assert_eq!(view[1].stale_reason.as_deref(), Some("probe_timeout"));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn only_the_closed_reached_types_are_typed() {
        for known in RATE_LIMIT_REACHED_TYPES {
            assert_eq!(known_reached_type(&json!(known)), Some(known));
        }
        assert_eq!(known_reached_type(&json!("something_new")), None);
        assert_eq!(known_reached_type(&json!(3)), None);
    }
}
