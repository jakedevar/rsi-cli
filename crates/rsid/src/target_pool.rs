//! Warm target pool lifecycle on the daemon side (#1882): garbage collection
//! after each sandbox reclaim pass and the status the operator reads.
//!
//! The pool itself is `scripts/target-pool`; this module only schedules it and
//! relays its report. A pool a seed is reading is never removed (the script's
//! use lock), and a missing script is not an error. Each pass also collects a
//! pool beyond the seed distance of the rolling tip (the script's `gc` default) and
//! then asks `refresh` for a build at the tip (#2707), so a pool that fell behind
//! never holds the disk a new one needs.

use rsi_common::target_pool::TargetPoolStatus;

/// What one reclaim pass asks of the pool: keep `keep` pools, or only the
/// newest under disk pressure (the same signal that reclaims shared targets).
pub fn gc_request(keep: u64, pressure: bool) -> (u64, bool) {
    if pressure {
        (1, true)
    } else {
        (keep.max(1), false)
    }
}

/// Run one pool GC pass and log what it removed. Blocking.
pub fn run_gc_and_log(trigger: &'static str, keep: u64, pressure: bool) {
    let (keep, pressure) = gc_request(keep, pressure);
    let Some(report) = crate::sandbox::execution_scratch::run_target_pool_gc(keep, pressure) else {
        return;
    };
    let names = |key: &str| report[key].as_array().map(|names| names.len()).unwrap_or(0);
    let (removed, skipped) = (names("removed"), names("skipped_in_use"));
    if removed > 0 || skipped > 0 {
        tracing::info!(
            trigger,
            keep,
            pressure,
            removed,
            skipped_in_use = skipped,
            "Warm target pool garbage collection completed"
        );
    }
}

/// The pool directories whose refresh started a build, from `target-pool refresh`'s
/// report (`{"refresh": [{"result": "started <unit>"}, ...]}`). A skipped or fresh
/// directory has some other `result`.
pub fn refresh_started(report: &serde_json::Value) -> usize {
    report["refresh"].as_array().map_or(0, |entries| {
        entries
            .iter()
            .filter(|entry| {
                entry["result"]
                    .as_str()
                    .is_some_and(|result| result.starts_with("started"))
            })
            .count()
    })
}

/// The refresh results that name a disk reason (#2773): a build skipped for lack of
/// headroom carries `skipped: pool_skipped_disk_headroom: ...` with the free, floor and
/// expected sizes, which the operator would otherwise never see (the pool script runs
/// detached). Returned verbatim, without the `skipped: ` prefix.
pub fn refresh_disk_skips(report: &serde_json::Value) -> Vec<String> {
    let prefix = format!(
        "skipped: {}",
        rsi_common::target_pool::POOL_SKIPPED_DISK_HEADROOM
    );
    report["refresh"]
        .as_array()
        .map_or_else(Vec::new, |entries| {
            entries
                .iter()
                .filter_map(|entry| entry["result"].as_str())
                .filter(|result| result.starts_with(&prefix))
                .map(|result| result.trim_start_matches("skipped: ").to_string())
                .collect()
        })
}

/// Start a pool build at the rolling tip when the pools trail it (#2707). The pass
/// that just collected stale pools freed their disk, so the floor check in the
/// script sees it. Skipped under disk pressure. Blocking, but never waits for the
/// build itself.
pub fn run_refresh_and_log(
    trigger: &'static str,
    runtime: &crate::config::RuntimeConfig,
    keep: u64,
    pressure: bool,
) {
    if pressure {
        return;
    }
    let Some(report) =
        crate::sandbox::execution_scratch::run_target_pool_refresh(runtime, keep.max(1))
    else {
        return;
    };
    for reason in refresh_disk_skips(&report) {
        tracing::info!(
            trigger,
            reason,
            "Warm target pool refresh did not start a build: the workers' disk floor comes first"
        );
    }
    let started = refresh_started(&report);
    if started > 0 {
        tracing::info!(
            trigger,
            started,
            "Warm target pool refresh started a build at the rolling tip"
        );
    }
}

/// The operator-visible pool state. None when the script is not installed.
pub async fn status() -> Option<TargetPoolStatus> {
    let value = tokio::task::spawn_blocking(crate::sandbox::execution_scratch::target_pool_summary)
        .await
        .ok()??;
    serde_json::from_value(value).ok()
}

/// QA uses the worker disk-floor reclaim path, restricted to the filesystem
/// the governor measures (`/`). Pool management is Linux-only; other hosts
/// explicitly report no reclaimable pool and perform no reclaim.
#[cfg(target_os = "linux")]
fn qa_pool_command(action: &str) -> Option<std::process::Command> {
    let script = dirs::home_dir()?.join(".rsi/bin/target-pool");
    let mut command = std::process::Command::new(script);
    command
        .arg(action)
        .args(["--filesystem", "/"])
        .stdin(std::process::Stdio::null())
        .env_remove("RSI_SESSION_TOKEN");
    Some(command)
}

pub(crate) async fn qa_reclaimable_bytes() -> u64 {
    #[cfg(target_os = "linux")]
    {
        tokio::task::spawn_blocking(|| {
            let output = qa_pool_command("summary")?.output().ok()?;
            if !output.status.success() {
                return None;
            }
            let report: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
            report["reclaimable_bytes"].as_u64()
        })
        .await
        .ok()
        .flatten()
        .unwrap_or(0)
    }
    #[cfg(not(target_os = "linux"))]
    {
        0
    }
}

/// Blocking. Never trusts the reported freed size: seeded targets may still
/// hold hardlinks, or a reader may have taken a lease since the health sample.
pub(crate) fn reclaim_idle_for_qa() {
    #[cfg(target_os = "linux")]
    if let Some(mut command) = qa_pool_command("reclaim-idle") {
        // An accepted QA start needs its disk now, even if a worker reclaimed
        // a different pool recently. The exclusive use lock still applies.
        if let Err(error) = command.args(["--min-interval", "0"]).output() {
            tracing::warn!(%error, "hub QA run: idle pool reclaim failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn disk_pressure_trims_to_the_newest_pool_otherwise_the_operator_keep_applies() {
        assert_eq!(gc_request(3, false), (3, false));
        assert_eq!(gc_request(3, true), (1, true));
        assert_eq!(gc_request(0, false), (1, false));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_refresh_report_counts_only_the_directories_that_started_a_build() {
        let report = serde_json::json!({"refresh": [
            {"result": "started rsi-target-pool-abc-1"},
            {"result": "fresh"},
            {"result": "skipped: pool_skipped_disk_headroom: only 30.0 GiB free; needs 56.0 GiB"},
            {"result": "started"},
        ]});
        assert_eq!(refresh_started(&report), 2);
        assert_eq!(refresh_started(&serde_json::json!({})), 0);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_refresh_skipped_for_disk_headroom_is_named_with_its_numbers() {
        let report = serde_json::json!({"refresh": [
            {"result": "skipped: pool_skipped_disk_headroom: only 36.9 GiB free; needs 56.0 GiB \
                        (floor 40.0 + expected pool 16.0, default)"},
            {"result": "skipped: no checkout is known for these pools"},
            {"result": "started rsi-target-pool-abc-1"},
        ]});
        assert_eq!(
            refresh_disk_skips(&report),
            vec![
                "pool_skipped_disk_headroom: only 36.9 GiB free; needs 56.0 GiB \
                 (floor 40.0 + expected pool 16.0, default)"
                    .to_string()
            ]
        );
        assert!(refresh_disk_skips(&serde_json::json!({})).is_empty());
    }
}
