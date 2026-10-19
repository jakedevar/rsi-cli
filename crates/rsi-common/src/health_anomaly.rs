//! `when.health_anomaly` (#2747): one daemon-evaluated predicate that replaces
//! a manager's hourly "safety net" model turn.
//!
//! The daemon checks the same facts a safety-net turn used to read (the PM
//! slot, context fill against the rotation threshold, blocked or overrun
//! topology work, host disk and temperature, provider faults, a red rolling QA
//! with no Issue) on a timer, with no model call. It resumes the owner once,
//! carrying one typed line per condition (`kind` and `subject`), only when an
//! anomaly holds. The thresholds are daemon settings (`health_disk_used_pct`,
//! `health_thermal_c`, `health_stuck_minutes`; operator RPC and TUI Settings).

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The appointed PM seat is vacant, `Failed` or `Completed` while its newest
/// root succession is refused (`blocked`; the #2667 class). Subject: project id.
pub const KIND_PM_SEAT_BLOCKED: &str = "pm_seat_succession_blocked";
/// A PM seat's `context_fill_pct` is at or above its `rotation_threshold_pct`
/// with no succession queued. Subject: the PM session id.
pub const KIND_PM_CONTEXT_FULL: &str = "pm_context_over_threshold";
/// A topology execution has been `blocked` for longer than
/// `health_stuck_minutes`. Subject: the execution id.
pub const KIND_EXECUTION_BLOCKED: &str = "topology_execution_blocked";
/// A node attempt is still `running` past its execution's wall limit
/// (`deadline_at`) by more than `health_stuck_minutes`. Subject: attempt id.
pub const KIND_ATTEMPT_PAST_WALL_LIMIT: &str = "node_attempt_past_wall_limit";
/// The hub root filesystem is at or above `health_disk_used_pct`. Subject: `/`.
pub const KIND_HUB_DISK_HIGH: &str = "hub_disk_high";
/// A satellite's free disk is below its disk floor. Subject: the peer id.
pub const KIND_SATELLITE_DISK_LOW: &str = "satellite_disk_low";
/// The hottest CPU sensor held at or above `health_thermal_c` for
/// [`THERMAL_SUSTAIN_SECS`]. Subject: `hub`.
pub const KIND_CPU_HOT: &str = "cpu_hot";
/// A provider refused a model as unavailable (`model_unavailable`). Subject:
/// `<host>:<provider>:<model>` where host is `hub` or the satellite peer id.
pub const KIND_PROVIDER_MODEL_UNAVAILABLE: &str = "provider_model_unavailable";
/// A provider rejected the credential (401-class) on a launch. Subject:
/// `<host>:<provider>`.
pub const KIND_PROVIDER_AUTH_FAILURE: &str = "provider_auth_failure";
/// The newest full-library QA run of `rolling` is red, older than
/// `health_stuck_minutes`, and no open Issue tracks it. Subject: the tested SHA.
pub const KIND_ROLLING_QA_RED: &str = "rolling_qa_red_untracked";
/// This daemon was built for a platform without sandbox build-cache reclaim
/// (the fail-closed `target_reclaim_unsupported.rs` shim: not Linux, not
/// macOS), so every reclaim pass refuses every candidate and finished workers'
/// `target/` directories are never removed automatically. Persistent while the
/// build is unsupported; never raised on Linux or macOS. Subject: `hub`.
pub const KIND_RECLAIM_UNSUPPORTED: &str = "reclaim_unsupported";

/// Every kind, for docs and the closed-set tests.
pub const ALL_KINDS: [&str; 11] = [
    KIND_PM_SEAT_BLOCKED,
    KIND_PM_CONTEXT_FULL,
    KIND_EXECUTION_BLOCKED,
    KIND_ATTEMPT_PAST_WALL_LIMIT,
    KIND_HUB_DISK_HIGH,
    KIND_SATELLITE_DISK_LOW,
    KIND_CPU_HOT,
    KIND_PROVIDER_MODEL_UNAVAILABLE,
    KIND_PROVIDER_AUTH_FAILURE,
    KIND_ROLLING_QA_RED,
    KIND_RECLAIM_UNSUPPORTED,
];

/// How long the CPU must stay hot before [`KIND_CPU_HOT`] fires ("sustained").
pub const THERMAL_SUSTAIN_SECS: i64 = 180;
/// How long a PM seat may sit at or above its rotation threshold with no
/// succession queued before [`KIND_PM_CONTEXT_FULL`] fires: the daemon's own
/// rotation gets this long to act first.
pub const CONTEXT_FULL_GRACE_SECS: i64 = 600;
/// The daemon evaluates a pending `health_anomaly` predicate this often.
pub const HEALTH_POLL_SECS: i64 = 60;
/// Most condition lines one wake message carries.
pub const HEALTH_MAX_CONDITIONS: usize = 24;
/// Most `ignore` keys one predicate may carry, and the longest key.
pub const HEALTH_MAX_IGNORE: usize = 32;
pub const HEALTH_MAX_IGNORE_KEY_BYTES: usize = 200;

/// `health_disk_used_pct`: hub root filesystem use (percent) at or above which
/// the predicate fires. 0 turns the disk condition off.
pub const HEALTH_DISK_USED_PCT_DEFAULT: u32 = 90;
pub const HEALTH_DISK_USED_PCT_MIN: u32 = 50;
pub const HEALTH_DISK_USED_PCT_MAX: u32 = 99;
/// `health_thermal_c`: sustained CPU temperature (C) that fires; 0 off.
pub const HEALTH_THERMAL_C_DEFAULT: u32 = 80;
pub const HEALTH_THERMAL_C_MIN: u32 = 60;
pub const HEALTH_THERMAL_C_MAX: u32 = 110;
/// `health_stuck_minutes`: how long work may sit blocked, overrun its wall
/// limit or leave a red QA untracked before the predicate fires; 0 off.
pub const HEALTH_STUCK_MINUTES_DEFAULT: u32 = 30;
pub const HEALTH_STUCK_MINUTES_MIN: u32 = 5;
pub const HEALTH_STUCK_MINUTES_MAX: u32 = 720;

/// One typed anomaly: what is wrong and which object it is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthCondition {
    /// One of the `KIND_*` constants.
    pub kind: &'static str,
    /// The id (or `host:name`) the condition is about.
    pub subject: String,
    /// Short human detail; never a secret or a path.
    pub detail: String,
}

impl HealthCondition {
    #[must_use]
    pub fn new(kind: &'static str, subject: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            kind,
            subject: subject.into(),
            detail: detail.into(),
        }
    }

    /// The dedupe/ignore key: `kind:subject`.
    #[must_use]
    pub fn key(&self) -> String {
        format!("{}:{}", self.kind, self.subject)
    }

    /// The wake-message line: the exact `ignore` key and the detail as
    /// separate fields, so the end of the subject is never a guess (#3032).
    /// The detail is quoted with escapes, so it cannot run into the key.
    #[must_use]
    pub fn line(&self) -> String {
        format!(
            "- condition={} subject={} ignore_key={} detail={:?}",
            self.kind,
            self.subject,
            self.key(),
            self.detail
        )
    }
}

/// The `when.health_anomaly` payload. `ignore` lists `kind:subject` keys the
/// owner has already seen and accepts (a persisting condition would otherwise
/// fire again on every re-arm). `project` and `projects` are filled by the
/// daemon at arm time from the caller's seat (a PM watches its own `project`,
/// a portfolio seat watches the `projects` its grant covers); a client-supplied
/// value is overwritten. The stored scope is only an upper bound: every
/// evaluation intersects it with the owner's current grant (#2788), so a
/// narrowed or revoked grant narrows what the wake observes and reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct HealthAnomalySpec {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignore: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub projects: Vec<Uuid>,
}

impl HealthAnomalySpec {
    /// Bounded `ignore` list of well-formed `kind:subject` keys.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.ignore.len() <= HEALTH_MAX_IGNORE
            && self.ignore.iter().all(|key| {
                !key.is_empty()
                    && key.len() <= HEALTH_MAX_IGNORE_KEY_BYTES
                    && !key.chars().any(char::is_control)
                    && key.split_once(':').is_some_and(|(kind, subject)| {
                        ALL_KINDS.contains(&kind) && !subject.is_empty()
                    })
            })
    }

    /// True when `condition` is one the owner asked not to hear about again.
    #[must_use]
    pub fn ignores(&self, condition: &HealthCondition) -> bool {
        let key = condition.key();
        self.ignore.iter().any(|ignored| *ignored == key)
    }
}

/// Whether `value` is an accepted `health_disk_used_pct`: 0 (off) or in range.
#[must_use]
pub const fn is_valid_disk_used_pct(value: u64) -> bool {
    value == 0
        || (value >= HEALTH_DISK_USED_PCT_MIN as u64 && value <= HEALTH_DISK_USED_PCT_MAX as u64)
}

/// Whether `value` is an accepted `health_thermal_c`: 0 (off) or in range.
#[must_use]
pub const fn is_valid_thermal_c(value: u64) -> bool {
    value == 0 || (value >= HEALTH_THERMAL_C_MIN as u64 && value <= HEALTH_THERMAL_C_MAX as u64)
}

/// Whether `value` is an accepted `health_stuck_minutes`: 0 (off) or in range.
#[must_use]
pub const fn is_valid_stuck_minutes(value: u64) -> bool {
    value == 0
        || (value >= HEALTH_STUCK_MINUTES_MIN as u64 && value <= HEALTH_STUCK_MINUTES_MAX as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_accepts_only_known_kinds_and_bounded_keys() {
        assert!(HealthAnomalySpec::default().is_valid());
        let ok = HealthAnomalySpec {
            ignore: vec![format!("{KIND_HUB_DISK_HIGH}:/")],
            ..HealthAnomalySpec::default()
        };
        assert!(ok.is_valid());
        for bad in [
            "",
            "hub_disk_high",
            "hub_disk_high:",
            "nonsense:/",
            "hub_disk_high:\n",
        ] {
            let spec = HealthAnomalySpec {
                ignore: vec![bad.to_string()],
                ..HealthAnomalySpec::default()
            };
            assert!(!spec.is_valid(), "{bad:?}");
        }
        let many = HealthAnomalySpec {
            ignore: (0..=HEALTH_MAX_IGNORE)
                .map(|n| format!("{KIND_CPU_HOT}:{n}"))
                .collect(),
            ..HealthAnomalySpec::default()
        };
        assert!(!many.is_valid());
    }

    #[test]
    fn ignore_matches_the_exact_kind_and_subject() {
        let spec = HealthAnomalySpec {
            ignore: vec![format!("{KIND_HUB_DISK_HIGH}:/")],
            ..HealthAnomalySpec::default()
        };
        assert!(spec.ignores(&HealthCondition::new(KIND_HUB_DISK_HIGH, "/", "98%")));
        assert!(!spec.ignores(&HealthCondition::new(KIND_HUB_DISK_HIGH, "/home", "98%")));
        assert!(!spec.ignores(&HealthCondition::new(KIND_CPU_HOT, "/", "90 C")));
    }

    #[test]
    fn setting_validators_accept_zero_or_the_range() {
        assert!(is_valid_disk_used_pct(0) && is_valid_disk_used_pct(90));
        assert!(!is_valid_disk_used_pct(49) && !is_valid_disk_used_pct(100));
        assert!(is_valid_thermal_c(0) && is_valid_thermal_c(80));
        assert!(!is_valid_thermal_c(59) && !is_valid_thermal_c(111));
        assert!(is_valid_stuck_minutes(0) && is_valid_stuck_minutes(30));
        assert!(!is_valid_stuck_minutes(4) && !is_valid_stuck_minutes(721));
    }

    #[test]
    fn a_condition_line_prints_the_exact_ignore_key_and_a_quoted_detail() {
        let condition = HealthCondition::new(KIND_PM_SEAT_BLOCKED, "abc", "status=Failed");
        assert_eq!(condition.key(), "pm_seat_succession_blocked:abc");
        assert_eq!(
            condition.line(),
            r#"- condition=pm_seat_succession_blocked subject=abc ignore_key=pm_seat_succession_blocked:abc detail="status=Failed""#
        );
    }

    /// Cuts the printed `ignore_key=` field out of a wake line the way a manager does.
    fn printed_key(line: &str) -> String {
        let rest = line
            .split_once(" ignore_key=")
            .expect("an ignore_key field")
            .1;
        let (key, _) = rest.split_once(" detail=").expect("a detail field");
        key.to_string()
    }

    #[test]
    fn the_printed_key_round_trips_into_an_ignore_list() {
        let condition = HealthCondition::new(
            KIND_PROVIDER_AUTH_FAILURE,
            "hub:Codex",
            "auth rejected (401-class) on launch",
        );
        let line = condition.line();
        assert_eq!(
            line,
            r#"- condition=provider_auth_failure subject=hub:Codex ignore_key=provider_auth_failure:hub:Codex detail="auth rejected (401-class) on launch""#
        );
        let spec = HealthAnomalySpec {
            ignore: vec![printed_key(&line)],
            ..HealthAnomalySpec::default()
        };
        assert!(spec.is_valid());
        assert!(spec.ignores(&condition));
        let other = HealthCondition::new(KIND_PROVIDER_AUTH_FAILURE, "hub:Claude", "x");
        assert!(!spec.ignores(&other));
    }

    #[test]
    fn a_detail_with_quotes_or_newlines_cannot_forge_a_second_field() {
        let condition = HealthCondition::new(KIND_CPU_HOT, "hub", "a\"\n- ignore_key=cpu_hot:x");
        let line = condition.line();
        assert!(!line.contains('\n'), "{line}");
        assert_eq!(printed_key(&line), "cpu_hot:hub");
    }
}
