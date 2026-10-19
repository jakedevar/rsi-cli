//! Warm target pool status for the operator's Sandbox Storage settings (#1882).
//!
//! `scripts/target-pool summary` prints this shape; the daemon relays it through
//! `GetTargetPoolStatus` and the TUI renders [`TargetPoolStatus::summary_line`].

use serde::{Deserialize, Serialize};

/// The newest pool a sandbox can seed from.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TargetPoolNewest {
    pub sha: String,
    #[serde(default)]
    pub built_at: Option<f64>,
    /// The tip's tests did not compile: libraries and binaries only.
    #[serde(default)]
    pub degraded: bool,
    #[serde(default)]
    pub size_bytes: u64,
    /// Commits the pool trails `origin/rolling`; None when it cannot be counted.
    #[serde(default)]
    pub age_commits: Option<u64>,
}

/// The most recent pool build that failed outright.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TargetPoolFailure {
    pub sha: String,
    #[serde(default)]
    pub built_at: Option<f64>,
    /// Typed reason when the build was stopped on purpose, e.g.
    /// `pool_aborted_disk_floor` (#2773); absent for an ordinary build failure.
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub detail: Option<String>,
}

/// Typed reason a pool build did not start because free space could not cover the
/// workers' floor plus the pool's expected size (#2773).
pub const POOL_SKIPPED_DISK_HEADROOM: &str = "pool_skipped_disk_headroom";
/// Typed reason a running pool build was stopped and removed because it took the free
/// space below the workers' `cargo-slot` floor (#2773).
pub const POOL_ABORTED_DISK_FLOOR: &str = "pool_aborted_disk_floor";
/// Typed reason idle pools were evicted for a worker waiting on the disk floor (#2773).
pub const POOL_RECLAIMED_FOR_DISK_FLOOR: &str = "pool_reclaimed_for_disk_floor";

/// A pool decision a PM would otherwise not see: a build skipped for lack of headroom, or
/// idle pools reclaimed for a waiting worker (#2773). `detail` names the numbers.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TargetPoolEvent {
    pub reason: String,
    #[serde(default)]
    pub at: Option<f64>,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub free_gib: Option<f64>,
    #[serde(default)]
    pub floor_gib: Option<f64>,
    #[serde(default)]
    pub expected_gib: Option<f64>,
    #[serde(default)]
    pub needed_gib: Option<f64>,
    /// Pools evicted (a reclaim).
    #[serde(default)]
    pub removed: u64,
    #[serde(default)]
    pub freed_bytes: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TargetPoolStatus {
    /// Ready pools on disk.
    #[serde(default)]
    pub pools: u64,
    #[serde(default)]
    pub total_bytes: u64,
    #[serde(default)]
    pub newest: Option<TargetPoolNewest>,
    /// Failed builds newer than the newest ready pool.
    #[serde(default)]
    pub failed_newer: u64,
    #[serde(default)]
    pub failed_latest: Option<TargetPoolFailure>,
    /// Why the latest wanted build did not start (newer than the newest pool).
    #[serde(default)]
    pub last_skip: Option<TargetPoolEvent>,
    /// The latest eviction of idle pools for a worker waiting on the disk floor.
    #[serde(default)]
    pub last_reclaim: Option<TargetPoolEvent>,
}

fn short(sha: &str) -> &str {
    sha.get(..8).unwrap_or(sha)
}

fn gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / (1u64 << 30) as f64)
}

impl TargetPoolStatus {
    /// One line: newest good pool, its age in commits, pool count and total size.
    pub fn summary_line(&self) -> String {
        let mut line = self.pool_line();
        if let Some(failed) = self.failed_latest.as_ref().filter(|failed| {
            failed.reason.is_some()
                && self
                    .newest
                    .as_ref()
                    .is_none_or(|n| n.built_at < failed.built_at)
        }) {
            let reason = failed.reason.as_deref().unwrap_or_default();
            line.push_str(&format!(" · {reason}"));
        }
        if let Some(skip) = &self.last_skip {
            line.push_str(&format!(" · {}: {}", skip.reason, skip.detail));
        }
        if let Some(reclaim) = &self.last_reclaim {
            line.push_str(&format!(
                " · {}: {} pool(s)",
                reclaim.reason, reclaim.removed
            ));
        }
        line
    }

    fn pool_line(&self) -> String {
        let Some(newest) = &self.newest else {
            return match &self.failed_latest {
                Some(failed) => format!("No pool · last build {} failed", short(&failed.sha)),
                None => "No pool yet".to_string(),
            };
        };
        let age = match newest.age_commits {
            Some(0) => "at tip".to_string(),
            Some(commits) => format!("{commits} commits behind"),
            None => "age unknown".to_string(),
        };
        let degraded = if newest.degraded {
            " (libraries only)"
        } else {
            ""
        };
        let failed = if self.failed_newer > 0 {
            format!(" · {} newer build(s) failed", self.failed_newer)
        } else {
            String::new()
        };
        format!(
            "{}{degraded} {age} · {} pool(s), {}{failed}",
            short(&newest.sha),
            self.pools,
            gib(self.total_bytes)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn newest(age: Option<u64>, degraded: bool) -> TargetPoolNewest {
        TargetPoolNewest {
            sha: "0123456789abcdef".into(),
            built_at: Some(1.0),
            degraded,
            size_bytes: 11 << 30,
            age_commits: age,
        }
    }

    #[test]
    fn summary_line_names_the_newest_pool_its_age_and_total_size() {
        let status = TargetPoolStatus {
            pools: 2,
            total_bytes: 22 << 30,
            newest: Some(newest(Some(28), false)),
            ..TargetPoolStatus::default()
        };
        assert_eq!(
            status.summary_line(),
            "01234567 28 commits behind · 2 pool(s), 22.0 GiB"
        );
    }

    #[test]
    fn summary_line_flags_a_degraded_pool_and_newer_failures() {
        let status = TargetPoolStatus {
            pools: 1,
            total_bytes: 11 << 30,
            newest: Some(newest(Some(0), true)),
            failed_newer: 1,
            ..TargetPoolStatus::default()
        };
        assert_eq!(
            status.summary_line(),
            "01234567 (libraries only) at tip · 1 pool(s), 11.0 GiB · 1 newer build(s) failed"
        );
    }

    #[test]
    fn summary_line_without_a_pool_says_why() {
        assert_eq!(TargetPoolStatus::default().summary_line(), "No pool yet");
        let failed = TargetPoolStatus {
            failed_latest: Some(TargetPoolFailure {
                sha: "fedcba9876".into(),
                built_at: None,
                reason: None,
                detail: None,
            }),
            ..TargetPoolStatus::default()
        };
        assert_eq!(
            failed.summary_line(),
            "No pool · last build fedcba98 failed"
        );
    }

    #[test]
    fn the_script_summary_shape_decodes() {
        let status: TargetPoolStatus = serde_json::from_value(serde_json::json!({
            "pools": 1, "total_bytes": 5,
            "newest": {"sha": "abc", "built_at": 2.5, "degraded": false,
                       "size_bytes": 5, "age_commits": null},
            "failed_newer": 0, "failed_latest": null
        }))
        .unwrap();
        assert_eq!(status.newest.unwrap().age_commits, None);
    }

    #[test]
    fn summary_line_names_why_no_pool_was_built_and_what_was_reclaimed() {
        let status = TargetPoolStatus {
            last_skip: Some(TargetPoolEvent {
                reason: POOL_SKIPPED_DISK_HEADROOM.into(),
                detail: "only 36.9 GiB free; needs 56.0 GiB (floor 40.0 + expected pool 16.0)"
                    .into(),
                ..TargetPoolEvent::default()
            }),
            last_reclaim: Some(TargetPoolEvent {
                reason: POOL_RECLAIMED_FOR_DISK_FLOOR.into(),
                removed: 2,
                ..TargetPoolEvent::default()
            }),
            ..TargetPoolStatus::default()
        };
        assert_eq!(
            status.summary_line(),
            "No pool yet · pool_skipped_disk_headroom: only 36.9 GiB free; needs 56.0 GiB \
             (floor 40.0 + expected pool 16.0) · pool_reclaimed_for_disk_floor: 2 pool(s)"
        );
    }

    #[test]
    fn an_aborted_build_is_named_in_the_summary_line() {
        let status = TargetPoolStatus {
            failed_latest: Some(TargetPoolFailure {
                sha: "fedcba9876".into(),
                built_at: Some(5.0),
                reason: Some(POOL_ABORTED_DISK_FLOOR.into()),
                detail: Some("stopped at 29.1 GiB free".into()),
            }),
            ..TargetPoolStatus::default()
        };
        assert_eq!(
            status.summary_line(),
            "No pool · last build fedcba98 failed · pool_aborted_disk_floor"
        );
    }

    #[test]
    fn the_script_summary_decodes_skip_reclaim_and_abort_events() {
        let status: TargetPoolStatus = serde_json::from_value(serde_json::json!({
            "pools": 0, "total_bytes": 0, "newest": null, "failed_newer": 0,
            "failed_latest": {"sha": "abc", "built_at": 1.0,
                              "reason": "pool_aborted_disk_floor", "detail": "d"},
            "last_skip": {"at": 2.0, "reason": "pool_skipped_disk_headroom", "sha": "abc",
                          "free_gib": 36.9, "floor_gib": 40.0, "expected_gib": 16.0,
                          "needed_gib": 56.0, "expected_source": "default", "detail": "x"},
            "last_reclaim": {"at": 3.0, "reason": "pool_reclaimed_for_disk_floor",
                             "removed": 1, "freed_bytes": 1610612736, "detail": "y"}
        }))
        .unwrap();
        assert_eq!(status.last_skip.unwrap().needed_gib, Some(56.0));
        assert_eq!(status.last_reclaim.unwrap().freed_bytes, 1_610_612_736);
        assert_eq!(
            status.failed_latest.unwrap().reason.as_deref(),
            Some(POOL_ABORTED_DISK_FLOOR)
        );
    }
}
