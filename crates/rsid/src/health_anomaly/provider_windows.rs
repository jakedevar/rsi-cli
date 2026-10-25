//! Advisory plan-window projections. Samples and episode acknowledgments are
//! bounded daemon-local state. Delivered episode subjects also live in wake
//! receipts, so a restart keeps deduplication and the window average.
use std::collections::{HashMap, HashSet, VecDeque};

use chrono::{DateTime, Duration, Utc};
use rsi_common::health_anomaly::{
    HEALTH_POLL_SECS, HealthAnomalySpec, HealthCondition, KIND_PROVIDER_WINDOW_PROJECTED_EXHAUSTION,
};
use rsi_common::rpc::ProviderRateLimitSnapshot;

#[derive(Debug, Default)]
pub(crate) struct WindowTracker {
    samples: HashMap<String, VecDeque<(DateTime<Utc>, f64)>>,
    reported: HashMap<String, i64>,
}

/// Account windows have no explicit start. Infer it only for known lengths;
/// Codex bucket names may prefix the length with `<limit id>:`.
fn window_seconds(key: &str) -> Option<i64> {
    let key = key
        .strip_suffix(":primary")
        .or_else(|| key.strip_suffix(":secondary"))
        .unwrap_or(key);
    match key.rsplit(':').next()? {
        "five_hour" => Some(5 * 3600),
        "seven_day" => Some(7 * 86400),
        key => key
            .strip_suffix('m')?
            .parse::<i64>()
            .ok()?
            .checked_mul(60)
            .filter(|n| *n > 0 && *n <= 366 * 86400),
    }
}

/// Compare to the accepted reset itself, never a rounded time bucket: even a
/// one-second drift can cross a minute/hour boundary. Keep the wire subject
/// unchanged so already-armed epoch ignores and delivered receipts still work.
/// Tolerance is at most one health-poll minute and a quarter of the known
/// window length (including numeric Codex windows), never a whole new cycle.
/// The anchor does not move as observations jitter, so drift cannot chain into
/// unbounded suppression. Provider and the complete window key stay exact.
fn same_episode(accepted: &str, observed: &str) -> bool {
    let Some((accepted_window, accepted_reset)) = accepted.rsplit_once(':') else {
        return false;
    };
    let Some((observed_window, observed_reset)) = observed.rsplit_once(':') else {
        return false;
    };
    if accepted_window != observed_window {
        return false;
    }
    let Some((_, window)) = accepted_window.split_once(':') else {
        return false;
    };
    let Some(length) = window_seconds(window) else {
        return false;
    };
    let (Ok(accepted_reset), Ok(observed_reset)) =
        (accepted_reset.parse::<i64>(), observed_reset.parse::<i64>())
    else {
        return false;
    };
    accepted_reset.abs_diff(observed_reset) <= (length / 4).min(HEALTH_POLL_SECS) as u64
}

/// Bounded compatibility for legacy `kind:provider:window:epoch` ignores.
/// Other health conditions continue using exact `HealthAnomalySpec::ignores`.
pub(super) fn ignored_episode(spec: &HealthAnomalySpec, condition: &HealthCondition) -> bool {
    condition.kind == KIND_PROVIDER_WINDOW_PROJECTED_EXHAUSTION
        && spec.ignore.iter().any(|key| {
            key.split_once(':').is_some_and(|(kind, subject)| {
                kind == condition.kind && same_episode(subject, &condition.subject)
            })
        })
}

impl WindowTracker {
    pub(crate) fn evaluate(
        &mut self,
        snapshots: &[ProviderRateLimitSnapshot],
        minimum_pct: u32,
        now: DateTime<Utc>,
    ) -> Vec<HealthCondition> {
        self.reported.retain(|_, reset| *reset > now.timestamp());
        let mut active = HashSet::new();
        let mut found = Vec::new();
        for snapshot in snapshots {
            for window in &snapshot.windows {
                let Some(reset) = window
                    .resets_at_epoch
                    .and_then(|at| DateTime::from_timestamp(at, 0))
                else {
                    continue;
                };
                let Some(length) = window_seconds(&window.window_key) else {
                    continue;
                };
                let Some(start) = reset.checked_sub_signed(Duration::seconds(length)) else {
                    continue;
                };
                let observed = snapshot.observed_at;
                let used = window.utilization;
                if reset <= now
                    || observed <= start
                    || observed > now
                    || observed >= reset
                    || !used.is_finite()
                    || !(0.0..=1.0).contains(&used)
                {
                    continue;
                }
                let subject = format!(
                    "{:?}:{}:{}",
                    snapshot.provider,
                    window.window_key,
                    reset.timestamp()
                );
                active.insert(subject.clone());
                let samples = self.samples.entry(subject.clone()).or_default();
                while samples
                    .front()
                    .is_some_and(|(at, _)| *at < observed - Duration::hours(24))
                {
                    samples.pop_front();
                }
                // Only advancing provider observations are samples. One per minute
                // bounds memory even when many sessions report the same account.
                let recent = samples.front().and_then(|(at, previous)| {
                    let elapsed = (observed - *at).num_seconds();
                    (elapsed >= 60 && used >= *previous).then(|| (used - previous) / elapsed as f64)
                });
                if samples
                    .back()
                    .is_none_or(|(at, _)| observed - *at >= Duration::minutes(1))
                {
                    samples.push_back((observed, used));
                }
                if minimum_pct == 0
                    || used * 100.0 < f64::from(minimum_pct)
                    || snapshot.stale_reason.is_some()
                    || self
                        .reported
                        .keys()
                        .any(|accepted| same_episode(accepted, &subject))
                {
                    continue;
                }
                let average = used / (observed - start).num_seconds().max(1) as f64;
                let burn = average.max(recent.unwrap_or(0.0));
                if burn <= 0.0 {
                    continue;
                }
                let remaining = ((1.0 - used) / burn).ceil();
                // Bound conversion and chrono arithmetic by the known reset.
                if remaining >= (reset - observed).num_seconds() as f64 {
                    continue;
                }
                let Some(exhaustion) =
                    observed.checked_add_signed(Duration::seconds(remaining as i64))
                else {
                    continue;
                };
                found.push(HealthCondition::new(
                    KIND_PROVIDER_WINDOW_PROJECTED_EXHAUSTION,
                    subject,
                    format!("provider={:?} window={} utilization={:.1}% projected_exhaustion={} resets_at={} observed_at={} burn_basis={}",
                        snapshot.provider, window.window_key, used * 100.0,
                        exhaustion.to_rfc3339(), reset.to_rfc3339(), observed.to_rfc3339(),
                        if recent.is_some_and(|rate| rate > average) { "recent_up_to_24h" } else { "window_average" }),
                ));
            }
        }
        self.samples.retain(|key, _| active.contains(key));
        found
    }

    /// Drop state whose account window has reset.
    pub(crate) fn retain_live(&mut self, now: DateTime<Utc>) -> bool {
        self.reported.retain(|_, reset| *reset > now.timestamp());
        self.samples.retain(|key, _| {
            key.rsplit(':')
                .next()
                .and_then(|epoch| epoch.parse::<i64>().ok())
                .is_some_and(|reset| reset > now.timestamp())
        });
        !self.reported.is_empty() || !self.samples.is_empty()
    }

    pub(crate) fn reported_subject(&mut self, subject: String) {
        if let Some(reset) = subject.rsplit(':').next().and_then(|s| s.parse().ok()) {
            self.reported.insert(subject, reset);
        }
    }
}

/// Retained across a manager re-arming its one-shot wake, separately from the
/// per-wake thermal/context clocks. Each manager gets its own notification.
pub(crate) fn trackers() -> &'static std::sync::Mutex<HashMap<uuid::Uuid, WindowTracker>> {
    static TRACKERS: std::sync::OnceLock<std::sync::Mutex<HashMap<uuid::Uuid, WindowTracker>>> =
        std::sync::OnceLock::new();
    TRACKERS.get_or_init(Default::default)
}
