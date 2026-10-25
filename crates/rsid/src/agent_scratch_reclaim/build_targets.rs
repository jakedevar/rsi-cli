//! Manager-visible state of the registered build targets (#3261): which
//! allocations under `<data dir>/build-targets` are live, which the scratch
//! proof would reclaim, and why each kept one is kept. Read-only: the preview
//! is a dry run of the same pass that owns deletion.

use super::{BuildTargetReport, Decision, ScratchConfig, ScratchReport, cargo_targets};
use rsi_common::sandbox_storage::{
    SANDBOX_BUILD_TARGETS_SAMPLES_MAX, SandboxBuildTargetState, SandboxBuildTargetV1,
    SandboxBuildTargetsV1,
};
use std::time::SystemTime;

/// A dry run over the build-targets root alone with the standard policy.
/// `None` when the root does not exist or holds no allocation.
#[must_use]
pub fn build_targets_status(
    open_jobs: Option<Vec<super::OpenJob>>,
) -> Option<SandboxBuildTargetsV1> {
    let mut config = ScratchConfig::standard();
    config.open_jobs = open_jobs;
    let root = cargo_targets::build_targets_root();
    config.roots.retain(|(path, _)| *path == root);
    status_from(&super::run(&config, SystemTime::now(), true))
}

fn state_of(decision: Decision) -> SandboxBuildTargetState {
    match decision {
        Decision::Held => SandboxBuildTargetState::Live,
        Decision::Reclaim => SandboxBuildTargetState::Reclaimable,
        _ => SandboxBuildTargetState::Kept,
    }
}

/// The wire view of a pass report: bytes per state, largest target first.
pub(super) fn status_from(report: &ScratchReport) -> Option<SandboxBuildTargetsV1> {
    if report.build_targets_total == 0 {
        return None;
    }
    let mut targets: Vec<&BuildTargetReport> = report.build_targets.iter().collect();
    targets.sort_by_key(|target| std::cmp::Reverse(target.bytes.unwrap_or(0)));
    let mut status = SandboxBuildTargetsV1 {
        total_count: report.build_targets_total,
        ..SandboxBuildTargetsV1::default()
    };
    for target in targets {
        let state = state_of(target.decision);
        let bytes = target.bytes.unwrap_or(0);
        let total = match state {
            SandboxBuildTargetState::Live => &mut status.live_bytes,
            SandboxBuildTargetState::Reclaimable => &mut status.reclaimable_bytes,
            SandboxBuildTargetState::Kept => &mut status.kept_bytes,
        };
        *total = total.saturating_add(bytes);
        if status.entries.len() < SANDBOX_BUILD_TARGETS_SAMPLES_MAX {
            status.entries.push(SandboxBuildTargetV1 {
                name: target.name.clone(),
                state,
                reason: target.reason.to_string(),
                idle_secs: target.idle_secs,
                bytes: target.bytes,
                worktree: target.worktree.clone(),
                job_id: target.job_id.clone(),
            });
        }
    }
    Some(status)
}
