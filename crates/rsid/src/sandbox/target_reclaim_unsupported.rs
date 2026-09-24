//! Fail-closed target-cache reclamation shim for platforms without Linux
//! `openat2` containment guarantees.
//!
//! Reclaiming a sandbox `target/` directory is intentionally unavailable here.
//! The Linux implementation relies on descriptor-relative `openat2` and
//! `renameat2` operations to prove containment before it mutates anything; a
//! pathname-based fallback would weaken that boundary.

use rsi_common::sandbox_storage::SandboxBuildCacheReclaimSkipReason as SkipReason;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

/// Mirrors the Linux `MAX_RECLAIM_PASS_DURATION`.
const UNSUPPORTED_PASS_STORE_WAIT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug)]
pub(crate) struct ReclaimWorkLimits {
    pub(crate) recovery_entries: u64,
    pub(crate) filesystem_entries: u64,
    pub(crate) allocated_bytes: u64,
    pub(crate) duration: Duration,
    pub(crate) depth: u32,
}

impl Default for ReclaimWorkLimits {
    fn default() -> Self {
        Self {
            recovery_entries: 0,
            filesystem_entries: 0,
            allocated_bytes: 0,
            duration: Duration::ZERO,
            depth: 0,
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct ReclaimPassState;

impl ReclaimPassState {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self)
    }

    pub(crate) fn checkpoint(&self, _depth: u32) -> Result<(), SkipReason> {
        Ok(())
    }

    /// Bounded Store-queue wait for phased reclaim callers. The shim tracks no
    /// elapsed budget (every reclaim refuses with `Openat2Unavailable`), so it
    /// reports the Linux per-pass cap: callers still reach the refusal path
    /// instead of a spurious `DurationBudget` stop, and never wait unbounded.
    pub(crate) fn remaining_duration(&self) -> Duration {
        UNSUPPORTED_PASS_STORE_WAIT
    }

    pub(crate) fn reclaimable_summary(&self) -> (u64, u32) {
        (0, 0)
    }

    pub(crate) fn entries_deleted(&self) -> u64 {
        0
    }

    #[cfg(test)]
    pub(crate) fn for_test(_limits: ReclaimWorkLimits) -> Arc<Self> {
        Self::new()
    }

    #[cfg(test)]
    pub(crate) fn set_elapsed_for_test(&self, _elapsed: Duration) {}
}

pub(crate) fn with_reclaim_pass_state<T>(
    _state: &Arc<ReclaimPassState>,
    operation: impl FnOnce() -> T,
) -> T {
    operation()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TargetReclaimKind {
    Inspected,
    StagedRemoved,
    StagedPending,
    RecoveredRemoved,
    RecoveredPending,
    Refused,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TargetReclaimOutcome {
    pub kind: TargetReclaimKind,
    pub bytes: u64,
    pub reason: Option<SkipReason>,
    pub terminal_rejection: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RegisteredTargetIntent {
    pub(crate) custody_id: Uuid,
    pub(crate) generation: u64,
    pub(crate) allocation_id: Uuid,
    pub(crate) bucket: u8,
    pub(crate) slot_name: String,
    pub(crate) expected_device: u64,
    pub(crate) expected_inode: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RegisteredNamespaceState {
    Absent,
    Expected,
    Different,
    Refused(SkipReason),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RegisteredTargetProbe {
    pub(crate) source: RegisteredNamespaceState,
    pub(crate) destination: RegisteredNamespaceState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RegisteredTargetIdentity {
    pub(crate) device: u64,
    pub(crate) inode: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TargetRecoverySweepEvidence {
    pub(crate) cycle_before: u64,
    pub(crate) cycle_after: u64,
    pub(crate) cursor_before: Option<String>,
    pub(crate) cursor_after: Option<String>,
    pub(crate) reserved: bool,
    pub(crate) wrapped: bool,
    pub(crate) entries_deleted: u64,
    pub(crate) residual_entries: Option<u64>,
    pub(crate) legacy_migrated: u32,
    pub(crate) nonprogress_count: u32,
}

#[derive(Debug, Default)]
pub(crate) struct TargetRecoveryRun {
    pub(crate) outcomes: Vec<TargetReclaimOutcome>,
    pub(crate) evidence: TargetRecoverySweepEvidence,
}

impl TargetReclaimOutcome {
    pub(crate) fn refused(reason: SkipReason) -> Self {
        Self {
            kind: TargetReclaimKind::Refused,
            bytes: 0,
            reason: Some(reason),
            terminal_rejection: false,
        }
    }
}

#[derive(Debug)]
pub(crate) struct PinnedSandboxRoot {
    root_path: PathBuf,
}

impl PinnedSandboxRoot {
    pub(crate) fn open(
        _sandbox_base: &Path,
        _sandbox_root: &Path,
        _allocation_id: Uuid,
    ) -> Result<Self, SkipReason> {
        Err(SkipReason::Openat2Unavailable)
    }

    pub(crate) fn root_path(&self) -> &Path {
        &self.root_path
    }

    pub(crate) fn inspect_target(&self) -> TargetReclaimOutcome {
        TargetReclaimOutcome::refused(SkipReason::Openat2Unavailable)
    }

    pub(crate) fn registered_target_identity(
        &self,
    ) -> Result<RegisteredTargetIdentity, SkipReason> {
        Err(SkipReason::Openat2Unavailable)
    }

    pub(crate) fn stage_registered_target(
        &self,
        _intent: &RegisteredTargetIntent,
    ) -> TargetReclaimOutcome {
        TargetReclaimOutcome::refused(SkipReason::Openat2Unavailable)
    }

    pub(crate) fn reclaim_target(
        &self,
        _custody_id: Uuid,
        _generation: u64,
    ) -> TargetReclaimOutcome {
        TargetReclaimOutcome::refused(SkipReason::Openat2Unavailable)
    }
}

pub(crate) fn probe_registered_target(
    _base: &Path,
    _intent: &RegisteredTargetIntent,
) -> RegisteredTargetProbe {
    RegisteredTargetProbe {
        source: RegisteredNamespaceState::Refused(SkipReason::Openat2Unavailable),
        destination: RegisteredNamespaceState::Refused(SkipReason::Openat2Unavailable),
    }
}

pub(crate) fn delete_registered_target(
    _base: &Path,
    _intent: &RegisteredTargetIntent,
    _dry_run: bool,
) -> TargetReclaimOutcome {
    TargetReclaimOutcome::refused(SkipReason::Openat2Unavailable)
}

pub(crate) fn replay_registered_publication(
    _base: &Path,
    _intent: &RegisteredTargetIntent,
) -> Result<(), SkipReason> {
    Err(SkipReason::Openat2Unavailable)
}

pub(crate) fn sync_registered_target_absence(
    _base: &Path,
    _intent: &RegisteredTargetIntent,
) -> Result<bool, SkipReason> {
    Err(SkipReason::Openat2Unavailable)
}

pub(crate) fn recover_staged_targets(_base: &Path, _dry_run: bool) -> Vec<TargetReclaimOutcome> {
    Vec::new()
}

pub(crate) fn recover_staged_targets_with_state(
    base: &Path,
    dry_run: bool,
    _state: &Arc<ReclaimPassState>,
) -> Vec<TargetReclaimOutcome> {
    recover_staged_targets(base, dry_run)
}

pub(crate) fn recover_staged_targets_run_with_state(
    base: &Path,
    dry_run: bool,
    state: &Arc<ReclaimPassState>,
) -> TargetRecoveryRun {
    TargetRecoveryRun {
        outcomes: recover_staged_targets_with_state(base, dry_run, state),
        ..TargetRecoveryRun::default()
    }
}

#[cfg(test)]
pub(crate) fn reclaim_test_isolation_for_test() -> std::sync::MutexGuard<'static, ()> {
    static ISOLATION: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    ISOLATION
        .get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
pub(crate) fn set_post_authentication_barrier_for_test(
    _custody_id: Uuid,
    _reached: Arc<std::sync::Barrier>,
    _release: Arc<std::sync::Barrier>,
) {
}

#[cfg(test)]
pub(crate) fn set_post_authentication_signal_for_test(
    _custody_id: Uuid,
    _reached: std::sync::mpsc::Sender<()>,
    _release: std::sync::mpsc::Receiver<()>,
) {
}

#[cfg(test)]
pub(crate) fn set_target_deletion_signal_for_test(
    _custody_id: Uuid,
    _reached: std::sync::mpsc::Sender<()>,
    _release: std::sync::mpsc::Receiver<()>,
) {
}

#[cfg(test)]
pub(crate) fn fail_registered_publication_replay_for_test(_intent: &RegisteredTargetIntent) {}

#[cfg(test)]
pub(crate) fn fail_registered_root_rename_for_test(_intent: &RegisteredTargetIntent) {}

#[cfg(test)]
pub(crate) fn fail_delete_after_for_test(_custody_id: uuid::Uuid, _generation: u64, _after: isize) {
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_reclaim_without_linux_openat2_containment() {
        let error = PinnedSandboxRoot::open(
            Path::new("/sandbox-base"),
            Path::new("/sandbox-root"),
            Uuid::nil(),
        )
        .expect_err("non-Linux target reclaim must fail closed");

        assert_eq!(error, SkipReason::Openat2Unavailable);
    }
}
