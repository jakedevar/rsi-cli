//! Descriptor-pinned target-cache staging and deletion.
//!
//! This module owns no Store, Session, Git, branch, ref, source-worktree, or
//! custody mutation. Its only mutating authority is an already authenticated
//! `target` directory opened beneath `base/<allocation UUID>`.

#![cfg(target_os = "linux")]

use nix::dir::Dir;
use nix::errno::Errno;
use nix::fcntl::{AtFlags, OFlag, OpenHow, RenameFlags, ResolveFlag, open, openat2, renameat2};
use nix::sys::stat::{Mode, SFlag, fstat, fstatat, mkdirat};
use nix::unistd::{UnlinkatFlags, dup, fsync, unlinkat};
use rsi_common::sandbox_storage::SandboxBuildCacheReclaimSkipReason as SkipReason;
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, OsStr};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;

const QUEUE_NAME: &str = ".rsi-target-reclaim-v1";
const REJECTED_STAGE_PREFIX: &str = ".rsi-target-reclaim-rejected-v2_";
const ROTATED_STAGE_PREFIX: &str = "v2";
const BUCKET_PREFIX: &str = "bucket_";
const CURSOR_DIR: &str = ".recovery-cursor-v2";
// Ignored compatibility namespace from the rejected one-cycle backoff design.
const BACKOFF_DIR: &str = ".entry-backoff-v2";
const BUCKET_COUNT: u16 = 256;
const LEGACY_MIGRATION_ENTRIES_PER_PASS: u64 = 64;
#[cfg(test)]
const LEGACY_REJECTED_QUEUE_NAME: &str = ".rsi-target-reclaim-rejected-v1";
// Fixed safety ceilings, not operator policy. These deliberately stay out of
// daemon_settings: they bound hostile filesystem work rather than tune normal
// reclaim selection.
const MAX_RECOVERY_ENTRIES_PER_PASS: u64 = 1_024;
const MAX_FILESYSTEM_ENTRIES_PER_PASS: u64 = 500_000;
const MAX_ALLOCATED_BYTES_PER_PASS: u64 = 128 * 1024 * 1024 * 1024;
const MAX_RECLAIM_PASS_DURATION: Duration = Duration::from_secs(30);
const MAX_RECLAIM_DEPTH: u32 = 128;
const RESOLVE_FLAGS: ResolveFlag = ResolveFlag::RESOLVE_BENEATH
    .union(ResolveFlag::RESOLVE_NO_SYMLINKS)
    .union(ResolveFlag::RESOLVE_NO_MAGICLINKS)
    .union(ResolveFlag::RESOLVE_NO_XDEV);

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
            recovery_entries: MAX_RECOVERY_ENTRIES_PER_PASS,
            filesystem_entries: MAX_FILESYSTEM_ENTRIES_PER_PASS,
            allocated_bytes: MAX_ALLOCATED_BYTES_PER_PASS,
            duration: MAX_RECLAIM_PASS_DURATION,
            depth: MAX_RECLAIM_DEPTH,
        }
    }
}

#[derive(Debug)]
struct ReclaimWorkBudget {
    limits: ReclaimWorkLimits,
    started_at: Instant,
    recovery_entries: u64,
    filesystem_entries: u64,
    allocated_bytes: u64,
    entries_deleted: u64,
    elapsed_override: Option<Duration>,
}

impl ReclaimWorkBudget {
    fn new(limits: ReclaimWorkLimits) -> Self {
        Self {
            limits,
            started_at: Instant::now(),
            recovery_entries: 0,
            filesystem_entries: 0,
            allocated_bytes: 0,
            entries_deleted: 0,
            elapsed_override: None,
        }
    }

    fn elapsed(&self) -> Duration {
        self.elapsed_override
            .unwrap_or_else(|| self.started_at.elapsed())
    }

    fn checkpoint(&self, depth: u32) -> Result<(), SkipReason> {
        if self.elapsed() > self.limits.duration {
            return Err(SkipReason::DurationBudget);
        }
        if depth > self.limits.depth {
            return Err(SkipReason::DepthBudget);
        }
        Ok(())
    }

    fn charge_recovery_entry(&mut self) -> Result<(), SkipReason> {
        self.checkpoint(0)?;
        if self.recovery_entries >= self.limits.recovery_entries {
            return Err(SkipReason::RecoveryEntryBudget);
        }
        self.recovery_entries += 1;
        Ok(())
    }

    fn charge_filesystem_entry(&mut self, depth: u32) -> Result<(), SkipReason> {
        self.checkpoint(depth)?;
        if self.filesystem_entries >= self.limits.filesystem_entries {
            return Err(SkipReason::FilesystemEntryBudget);
        }
        self.filesystem_entries += 1;
        Ok(())
    }

    fn charge_allocated_bytes(&mut self, bytes: u64) -> Result<(), SkipReason> {
        self.checkpoint(0)?;
        if bytes
            > self
                .limits
                .allocated_bytes
                .saturating_sub(self.allocated_bytes)
        {
            return Err(SkipReason::ByteBudget);
        }
        self.allocated_bytes += bytes;
        Ok(())
    }

    fn observe_allocated_bytes(&mut self, bytes: u64) -> u64 {
        let observed = bytes.min(
            self.limits
                .allocated_bytes
                .saturating_sub(self.allocated_bytes),
        );
        self.allocated_bytes = self.allocated_bytes.saturating_add(observed);
        observed
    }

    fn record_entry_deleted(&mut self) {
        self.entries_deleted = self.entries_deleted.saturating_add(1);
    }
}

type InodeKey = (u64, u64);
type TargetKey = (u64, u64);

#[derive(Clone, Debug, PartialEq, Eq)]
struct InodeObservation {
    bytes: u64,
    link_count: u64,
    observed_links: u64,
    is_dir: bool,
}

#[derive(Clone, Debug, Default)]
struct TargetFootprint {
    entries: HashMap<InodeKey, InodeObservation>,
    bytes: u64,
    scanned_entries: u64,
    scanned_allocated_bytes: u64,
}

impl TargetFootprint {
    fn observe(&mut self, stat: &nix::libc::stat) {
        let key = (stat.st_dev as u64, stat.st_ino as u64);
        let bytes = allocated_stat_bytes(stat);
        self.scanned_entries = self.scanned_entries.saturating_add(1);
        self.scanned_allocated_bytes = self.scanned_allocated_bytes.saturating_add(bytes);
        let is_dir = SFlag::from_bits_truncate(stat.st_mode).contains(SFlag::S_IFDIR);
        let entry = self.entries.entry(key).or_insert_with(|| {
            self.bytes = self.bytes.saturating_add(bytes);
            InodeObservation {
                bytes,
                link_count: stat.st_nlink.max(1) as u64,
                observed_links: 0,
                is_dir,
            }
        });
        entry.bytes = entry.bytes.max(bytes);
        entry.link_count = entry.link_count.max(stat.st_nlink.max(1) as u64);
        entry.is_dir |= is_dir;
        entry.observed_links = entry.observed_links.saturating_add(1);
    }

    /// Retain only inode observations that are byte-for-byte stable across a
    /// second descriptor-relative walk.  A changed link count, observed-link
    /// count, allocation, type, disappearance, or replacement removes that
    /// inode from the point-in-time estimate.
    fn stable_against(&self, current: &Self) -> Self {
        let mut stable = Self {
            scanned_entries: current.scanned_entries,
            scanned_allocated_bytes: current.scanned_allocated_bytes,
            ..Self::default()
        };
        for (key, observation) in &self.entries {
            if current.entries.get(key) == Some(observation) {
                stable.entries.insert(*key, observation.clone());
                stable.bytes = stable.bytes.saturating_add(observation.bytes);
            }
        }
        stable
    }
}

#[derive(Debug, Default)]
struct DeletionSetLedger {
    targets: HashMap<TargetKey, TargetFootprint>,
}

impl DeletionSetLedger {
    fn insert(&mut self, target: TargetKey, footprint: TargetFootprint) {
        self.targets.insert(target, footprint);
    }

    fn reclaimable_summary(&self) -> (u64, u32) {
        #[derive(Default)]
        struct Aggregate {
            bytes: u64,
            link_count: u64,
            observed_links: u64,
            is_dir: bool,
            targets: HashSet<TargetKey>,
        }

        let mut inodes: HashMap<InodeKey, Aggregate> = HashMap::new();
        for (target, footprint) in &self.targets {
            for (inode, observation) in &footprint.entries {
                let aggregate = inodes.entry(*inode).or_default();
                aggregate.bytes = aggregate.bytes.max(observation.bytes);
                aggregate.link_count = aggregate.link_count.max(observation.link_count);
                aggregate.observed_links = aggregate
                    .observed_links
                    .saturating_add(observation.observed_links);
                aggregate.is_dir |= observation.is_dir;
                aggregate.targets.insert(*target);
            }
        }

        let mut bytes = 0_u64;
        let mut contributors = HashSet::new();
        for aggregate in inodes.values() {
            if aggregate.is_dir || aggregate.observed_links >= aggregate.link_count {
                bytes = bytes.saturating_add(aggregate.bytes);
                if aggregate.bytes > 0
                    && let Some(target) = aggregate.targets.iter().min()
                {
                    contributors.insert(*target);
                }
            }
        }
        (bytes, contributors.len().try_into().unwrap_or(u32::MAX))
    }
}

#[derive(Debug)]
pub(crate) struct ReclaimPassState {
    budget: Mutex<ReclaimWorkBudget>,
    deletion_set: Mutex<DeletionSetLedger>,
}

impl ReclaimPassState {
    pub(crate) fn new() -> Arc<Self> {
        Self::with_limits(ReclaimWorkLimits::default())
    }

    fn with_limits(limits: ReclaimWorkLimits) -> Arc<Self> {
        Arc::new(Self {
            budget: Mutex::new(ReclaimWorkBudget::new(limits)),
            deletion_set: Mutex::new(DeletionSetLedger::default()),
        })
    }

    pub(crate) fn checkpoint(&self, depth: u32) -> Result<(), SkipReason> {
        self.budget
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .checkpoint(depth)
    }

    pub(crate) fn remaining_duration(&self) -> Duration {
        let budget = self
            .budget
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        budget.limits.duration.saturating_sub(budget.elapsed())
    }

    fn charge_recovery_entry(&self) -> Result<(), SkipReason> {
        self.budget
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .charge_recovery_entry()
    }

    fn charge_filesystem_entry(&self, depth: u32) -> Result<(), SkipReason> {
        self.budget
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .charge_filesystem_entry(depth)
    }

    fn charge_allocated_bytes(&self, stat: &nix::libc::stat) -> Result<(), SkipReason> {
        self.budget
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .charge_allocated_bytes(allocated_stat_bytes(stat))
    }

    fn observe_allocated_bytes(&self, bytes: u64) -> u64 {
        self.budget
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .observe_allocated_bytes(bytes)
    }

    fn record_entry_deleted(&self) {
        self.budget
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .record_entry_deleted();
    }

    pub(crate) fn entries_deleted(&self) -> u64 {
        self.budget
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entries_deleted
    }

    fn include_target(&self, target: TargetKey, footprint: TargetFootprint) {
        self.deletion_set
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(target, footprint);
    }

    pub(crate) fn reclaimable_summary(&self) -> (u64, u32) {
        self.deletion_set
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .reclaimable_summary()
    }

    #[cfg(test)]
    pub(crate) fn for_test(limits: ReclaimWorkLimits) -> Arc<Self> {
        Self::with_limits(limits)
    }

    #[cfg(test)]
    pub(crate) fn set_elapsed_for_test(&self, elapsed: Duration) {
        self.budget
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .elapsed_override = Some(elapsed);
    }
}

thread_local! {
    static ACTIVE_PASS_STATE: RefCell<Option<Arc<ReclaimPassState>>> = const { RefCell::new(None) };
}

pub(crate) fn with_reclaim_pass_state<T>(
    state: &Arc<ReclaimPassState>,
    operation: impl FnOnce() -> T,
) -> T {
    let previous = ACTIVE_PASS_STATE.with(|active| active.replace(Some(Arc::clone(state))));
    let result = operation();
    ACTIVE_PASS_STATE.with(|active| {
        active.replace(previous);
    });
    result
}

fn current_pass_state() -> Arc<ReclaimPassState> {
    ACTIVE_PASS_STATE
        .with(|active| active.borrow().clone())
        .unwrap_or_else(ReclaimPassState::new)
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
    fn inspected(bytes: u64) -> Self {
        Self {
            kind: TargetReclaimKind::Inspected,
            bytes,
            reason: None,
            terminal_rejection: false,
        }
    }

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
    base_fd: OwnedFd,
    root_fd: OwnedFd,
    root_path: PathBuf,
    base_dev: u64,
}

#[derive(Debug)]
struct PinnedTarget {
    // Retain the authenticated descriptor until staging completes. The field
    // is intentionally unread: ownership, rather than its integer value, is
    // the pin that prevents target replacement from changing authority.
    _fd: OwnedFd,
    dev: u64,
    ino: u64,
    bytes: u64,
    footprint: TargetFootprint,
}

#[derive(Clone, Copy, Debug, Default)]
struct DeletionCredits {
    entries: u64,
    allocated_bytes: u64,
}

impl From<&TargetFootprint> for DeletionCredits {
    fn from(footprint: &TargetFootprint) -> Self {
        Self {
            entries: footprint.scanned_entries,
            allocated_bytes: footprint.scanned_allocated_bytes,
        }
    }
}

impl PinnedSandboxRoot {
    pub(crate) fn open(
        sandbox_base: &Path,
        sandbox_root: &Path,
        allocation_id: Uuid,
    ) -> Result<Self, SkipReason> {
        let base_path = std::fs::canonicalize(sandbox_base)
            .map_err(|_| SkipReason::GitOrRootIdentityRefusal)?;
        let base_raw = open(
            &base_path,
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW,
            Mode::empty(),
        )
        .map_err(map_open_error)?;
        // SAFETY: `open` returned a new owned descriptor.
        let base_fd = unsafe { OwnedFd::from_raw_fd(base_raw) };
        let base_stat =
            fstat(base_fd.as_raw_fd()).map_err(|_| SkipReason::GitOrRootIdentityRefusal)?;
        let component = allocation_id.to_string();
        let root_fd = open_beneath_dir(base_fd.as_raw_fd(), OsStr::new(&component))?;
        let root_stat =
            fstat(root_fd.as_raw_fd()).map_err(|_| SkipReason::GitOrRootIdentityRefusal)?;
        if root_stat.st_dev as u64 != base_stat.st_dev as u64 {
            return Err(SkipReason::MountOrDeviceCrossing);
        }
        let root_path = std::fs::canonicalize(sandbox_root)
            .map_err(|_| SkipReason::GitOrRootIdentityRefusal)?;
        if root_path != base_path.join(&component) || root_path != sandbox_root {
            return Err(SkipReason::GitOrRootIdentityRefusal);
        }
        let path_stat =
            std::fs::metadata(&root_path).map_err(|_| SkipReason::GitOrRootIdentityRefusal)?;
        use std::os::unix::fs::MetadataExt;
        if path_stat.dev() != root_stat.st_dev as u64 || path_stat.ino() != root_stat.st_ino as u64
        {
            return Err(SkipReason::GitOrRootIdentityRefusal);
        }
        Ok(Self {
            base_fd,
            root_fd,
            root_path,
            base_dev: base_stat.st_dev as u64,
        })
    }

    pub(crate) fn root_path(&self) -> &Path {
        &self.root_path
    }

    pub(crate) fn inspect_target(&self) -> TargetReclaimOutcome {
        match self.open_target() {
            Ok(target) => {
                let stable = revalidate_target_footprint(
                    target._fd.as_raw_fd(),
                    self.base_dev,
                    &target.footprint,
                );
                current_pass_state().include_target((target.dev, target.ino), stable);
                TargetReclaimOutcome::inspected(target.bytes)
            }
            Err(reason) => TargetReclaimOutcome::refused(reason),
        }
    }

    pub(crate) fn registered_target_identity(
        &self,
    ) -> Result<RegisteredTargetIdentity, SkipReason> {
        let stat = match fstatat(
            Some(self.root_fd.as_raw_fd()),
            "target",
            AtFlags::AT_SYMLINK_NOFOLLOW,
        ) {
            Err(Errno::ENOENT) => return Err(SkipReason::TargetAbsent),
            Ok(stat) if SFlag::from_bits_truncate(stat.st_mode).contains(SFlag::S_IFLNK) => {
                return Err(SkipReason::TargetSymlink);
            }
            Ok(stat) if !SFlag::from_bits_truncate(stat.st_mode).contains(SFlag::S_IFDIR) => {
                return Err(SkipReason::TargetNotDirectory);
            }
            Ok(stat) => stat,
            Err(_) => return Err(SkipReason::TargetIdentityChanged),
        };
        let target = open_beneath_dir(self.root_fd.as_raw_fd(), OsStr::new("target"))?;
        let opened = fstat(target.as_raw_fd()).map_err(|_| SkipReason::TargetIdentityChanged)?;
        if opened.st_dev != stat.st_dev || opened.st_ino != stat.st_ino {
            return Err(SkipReason::TargetIdentityChanged);
        }
        if opened.st_dev as u64 != self.base_dev {
            return Err(SkipReason::MountOrDeviceCrossing);
        }
        Ok(RegisteredTargetIdentity {
            device: opened.st_dev as u64,
            inode: opened.st_ino as u64,
        })
    }

    pub(crate) fn stage_registered_target(
        &self,
        intent: &RegisteredTargetIntent,
    ) -> TargetReclaimOutcome {
        if !registered_intent_names_are_canonical(intent) {
            return TargetReclaimOutcome::refused(SkipReason::InvalidRecoveryEntry);
        }
        let target = match self.open_target_identity() {
            Ok(target) => target,
            Err(reason) => return TargetReclaimOutcome::refused(reason),
        };
        if target.dev != intent.expected_device || target.ino != intent.expected_inode {
            return TargetReclaimOutcome::refused(SkipReason::TargetIdentityChanged);
        }
        if let Err(reason) = current_pass_state().checkpoint(0) {
            return TargetReclaimOutcome::refused(reason);
        }
        post_authentication_barrier_for_test(intent.custody_id);
        let queue_fd = match self.ensure_queue() {
            Ok(fd) => fd,
            Err(reason) => return TargetReclaimOutcome::refused(reason),
        };
        let bucket_fd = match ensure_private_child_directory(
            queue_fd.as_raw_fd(),
            &bucket_name(intent.bucket),
            self.base_dev,
        ) {
            Ok(fd) => fd,
            Err(reason) => return TargetReclaimOutcome::refused(reason),
        };
        if maybe_fail_registered_durability_for_test(
            intent,
            RegisteredDurabilitySeam::BucketPublication,
        )
        .is_err()
            || fsync(queue_fd.as_raw_fd()).is_err()
        {
            return TargetReclaimOutcome::refused(SkipReason::StagedDeletionIncomplete);
        }
        let slot_fd = match ensure_private_child_directory(
            bucket_fd.as_raw_fd(),
            &intent.slot_name,
            self.base_dev,
        ) {
            Ok(fd) => fd,
            Err(reason) => return TargetReclaimOutcome::refused(reason),
        };
        if maybe_fail_registered_durability_for_test(
            intent,
            RegisteredDurabilitySeam::SlotPublication,
        )
        .is_err()
            || fsync(bucket_fd.as_raw_fd()).is_err()
        {
            return TargetReclaimOutcome::refused(SkipReason::StagedDeletionIncomplete);
        }
        match fstatat(
            Some(slot_fd.as_raw_fd()),
            "payload",
            AtFlags::AT_SYMLINK_NOFOLLOW,
        ) {
            Err(Errno::ENOENT) => {}
            Ok(_) => return TargetReclaimOutcome::refused(SkipReason::StageConflict),
            Err(_) => {
                return TargetReclaimOutcome::refused(SkipReason::StagedDeletionIncomplete);
            }
        }
        let current = match fstatat(
            Some(self.root_fd.as_raw_fd()),
            "target",
            AtFlags::AT_SYMLINK_NOFOLLOW,
        ) {
            Ok(stat) => stat,
            Err(Errno::ENOENT) => return TargetReclaimOutcome::refused(SkipReason::TargetAbsent),
            Err(_) => return TargetReclaimOutcome::refused(SkipReason::TargetIdentityChanged),
        };
        if current.st_dev as u64 != target.dev || current.st_ino as u64 != target.ino {
            return TargetReclaimOutcome::refused(SkipReason::TargetIdentityChanged);
        }
        if let Err(error) = renameat2(
            Some(self.root_fd.as_raw_fd()),
            "target",
            Some(slot_fd.as_raw_fd()),
            "payload",
            RenameFlags::RENAME_NOREPLACE,
        ) {
            return TargetReclaimOutcome::refused(if error == Errno::EEXIST {
                SkipReason::StageConflict
            } else if error == Errno::EXDEV {
                SkipReason::MountOrDeviceCrossing
            } else {
                SkipReason::TargetIdentityChanged
            });
        }
        if maybe_fail_registered_durability_for_test(intent, RegisteredDurabilitySeam::RootRename)
            .is_err()
            || fsync(self.root_fd.as_raw_fd()).is_err()
            || maybe_fail_registered_durability_for_test(
                intent,
                RegisteredDurabilitySeam::SlotRename,
            )
            .is_err()
            || fsync(slot_fd.as_raw_fd()).is_err()
            || maybe_fail_registered_durability_for_test(
                intent,
                RegisteredDurabilitySeam::BucketRename,
            )
            .is_err()
            || fsync(bucket_fd.as_raw_fd()).is_err()
            || maybe_fail_registered_durability_for_test(
                intent,
                RegisteredDurabilitySeam::QueueRename,
            )
            .is_err()
            || fsync(queue_fd.as_raw_fd()).is_err()
        {
            return TargetReclaimOutcome {
                kind: TargetReclaimKind::StagedPending,
                bytes: target.bytes,
                reason: Some(SkipReason::StagedDeletionIncomplete),
                terminal_rejection: false,
            };
        }
        TargetReclaimOutcome {
            kind: TargetReclaimKind::StagedPending,
            bytes: target.bytes,
            reason: None,
            terminal_rejection: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn reclaim_target(&self, custody_id: Uuid, generation: u64) -> TargetReclaimOutcome {
        let target = match self.open_target() {
            Ok(target) => target,
            Err(reason) => return TargetReclaimOutcome::refused(reason),
        };
        if let Err(reason) = current_pass_state().checkpoint(0) {
            return TargetReclaimOutcome::refused(reason);
        }
        post_authentication_barrier_for_test(custody_id);
        let stable_footprint =
            revalidate_target_footprint(target._fd.as_raw_fd(), self.base_dev, &target.footprint);
        let queue_fd = match self.ensure_queue() {
            Ok(fd) => fd,
            Err(reason) => return TargetReclaimOutcome::refused(reason),
        };
        let current = match fstatat(
            Some(self.root_fd.as_raw_fd()),
            "target",
            AtFlags::AT_SYMLINK_NOFOLLOW,
        ) {
            Ok(stat) => stat,
            Err(Errno::ENOENT) => return TargetReclaimOutcome::refused(SkipReason::TargetAbsent),
            Err(_) => return TargetReclaimOutcome::refused(SkipReason::TargetIdentityChanged),
        };
        if current.st_dev as u64 != target.dev || current.st_ino as u64 != target.ino {
            return TargetReclaimOutcome::refused(SkipReason::TargetIdentityChanged);
        }
        let entry_name = stage_name(custody_id, generation, target.dev, target.ino);
        let bucket_index = stage_bucket_index(&entry_name);
        let bucket_fd = match ensure_private_child_directory(
            queue_fd.as_raw_fd(),
            &bucket_name(bucket_index),
            self.base_dev,
        ) {
            Ok(fd) => fd,
            Err(reason) => return TargetReclaimOutcome::refused(reason),
        };
        if let Err(reason) =
            ensure_recovery_cursor(queue_fd.as_raw_fd(), self.base_dev, bucket_index)
        {
            return TargetReclaimOutcome::refused(reason);
        }
        if let Err(error) = renameat2(
            Some(self.root_fd.as_raw_fd()),
            "target",
            Some(bucket_fd.as_raw_fd()),
            entry_name.as_str(),
            RenameFlags::RENAME_NOREPLACE,
        ) {
            return TargetReclaimOutcome::refused(if error == Errno::EEXIST {
                SkipReason::StageConflict
            } else if error == Errno::EXDEV {
                SkipReason::MountOrDeviceCrossing
            } else {
                SkipReason::TargetIdentityChanged
            });
        }
        current_pass_state().include_target((target.dev, target.ino), stable_footprint);

        let staged_fd = match open_beneath_dir(bucket_fd.as_raw_fd(), OsStr::new(&entry_name)) {
            Ok(fd) => fd,
            Err(_) => {
                return TargetReclaimOutcome {
                    kind: TargetReclaimKind::StagedPending,
                    bytes: target.bytes,
                    reason: Some(SkipReason::StagedDeletionIncomplete),
                    terminal_rejection: false,
                };
            }
        };
        let staged = match fstat(staged_fd.as_raw_fd()) {
            Ok(stat) => stat,
            Err(_) => {
                return TargetReclaimOutcome {
                    kind: TargetReclaimKind::StagedPending,
                    bytes: target.bytes,
                    reason: Some(SkipReason::StagedDeletionIncomplete),
                    terminal_rejection: false,
                };
            }
        };
        if staged.st_dev as u64 != target.dev || staged.st_ino as u64 != target.ino {
            return TargetReclaimOutcome {
                kind: TargetReclaimKind::StagedPending,
                bytes: target.bytes,
                reason: Some(SkipReason::TargetIdentityChanged),
                terminal_rejection: false,
            };
        }
        let namespace_sync_failed = fsync(self.root_fd.as_raw_fd()).is_err()
            || fsync(bucket_fd.as_raw_fd()).is_err()
            || fsync(queue_fd.as_raw_fd()).is_err();
        let post_sync_budget = current_pass_state().checkpoint(0);
        if namespace_sync_failed || post_sync_budget.is_err() {
            return TargetReclaimOutcome {
                kind: TargetReclaimKind::StagedPending,
                bytes: target.bytes,
                reason: Some(
                    post_sync_budget
                        .err()
                        .unwrap_or(SkipReason::StagedDeletionIncomplete),
                ),
                terminal_rejection: false,
            };
        }
        let outcome = finish_staged(
            bucket_fd.as_raw_fd(),
            &entry_name,
            staged_fd,
            target.bytes,
            false,
            self.base_dev,
            DeletionCredits::from(&target.footprint),
        );
        reject_depth_limited_stage(
            self.base_fd.as_raw_fd(),
            bucket_fd.as_raw_fd(),
            &entry_name,
            self.base_dev,
            outcome,
        )
    }

    fn open_target(&self) -> Result<PinnedTarget, SkipReason> {
        let mut target = self.open_target_identity()?;
        let footprint = allocated_tree_footprint(target._fd.as_raw_fd(), self.base_dev)?;
        target.bytes = footprint.bytes;
        target.footprint = footprint;
        Ok(target)
    }

    fn open_target_identity(&self) -> Result<PinnedTarget, SkipReason> {
        let fd = match open_beneath_dir(self.root_fd.as_raw_fd(), OsStr::new("target")) {
            Ok(fd) => fd,
            Err(SkipReason::Openat2Unavailable) => return Err(SkipReason::Openat2Unavailable),
            Err(SkipReason::MountOrDeviceCrossing) => {
                return Err(SkipReason::MountOrDeviceCrossing);
            }
            Err(_) => {
                return Err(
                    match fstatat(
                        Some(self.root_fd.as_raw_fd()),
                        "target",
                        AtFlags::AT_SYMLINK_NOFOLLOW,
                    ) {
                        Err(Errno::ENOENT) => SkipReason::TargetAbsent,
                        Ok(stat)
                            if SFlag::from_bits_truncate(stat.st_mode).contains(SFlag::S_IFLNK) =>
                        {
                            SkipReason::TargetSymlink
                        }
                        Ok(_) => SkipReason::TargetNotDirectory,
                        Err(_) => SkipReason::TargetIdentityChanged,
                    },
                );
            }
        };
        let stat = fstat(fd.as_raw_fd()).map_err(|_| SkipReason::TargetIdentityChanged)?;
        if stat.st_dev as u64 != self.base_dev {
            return Err(SkipReason::MountOrDeviceCrossing);
        }
        Ok(PinnedTarget {
            _fd: fd,
            dev: stat.st_dev as u64,
            ino: stat.st_ino as u64,
            bytes: 0,
            footprint: TargetFootprint::default(),
        })
    }

    fn ensure_queue(&self) -> Result<OwnedFd, SkipReason> {
        let created = match mkdirat(
            Some(self.base_fd.as_raw_fd()),
            QUEUE_NAME,
            Mode::from_bits_truncate(0o700),
        ) {
            Ok(()) => true,
            Err(Errno::EEXIST) => false,
            Err(Errno::EXDEV) => return Err(SkipReason::MountOrDeviceCrossing),
            Err(_) => return Err(SkipReason::RejectedRecoveryEntry),
        };
        let fd = open_beneath_dir(self.base_fd.as_raw_fd(), OsStr::new(QUEUE_NAME))?;
        let stat = fstat(fd.as_raw_fd()).map_err(|_| SkipReason::RejectedRecoveryEntry)?;
        let mode = stat.st_mode as u32 & 0o777;
        // SAFETY: `geteuid` has no preconditions.
        if stat.st_dev as u64 != self.base_dev
            || mode != 0o700
            || stat.st_uid != unsafe { nix::libc::geteuid() }
        {
            return Err(SkipReason::RejectedRecoveryEntry);
        }
        // The queue name lives in `base_fd`, not in the queue itself.  Order
        // that parent link durable before any target rename.  Repeating the
        // barrier for an existing queue also closes a prior failed-create
        // attempt whose in-memory namespace survived long enough to retry.
        if maybe_fail_base_fsync_for_test(self.base_fd.as_raw_fd(), created).is_err()
            || fsync(self.base_fd.as_raw_fd()).is_err()
        {
            return Err(SkipReason::StagedDeletionIncomplete);
        }
        Ok(fd)
    }
}

fn stage_bucket_index(entry_name: &str) -> u8 {
    Sha256::digest(entry_name.as_bytes())[0]
}

fn registered_bucket_index(custody_id: Uuid, generation: u64) -> u8 {
    let mut digest = Sha256::new();
    digest.update(b"rsi.target-reclaim-bucket.v3\0");
    digest.update(custody_id.as_bytes());
    digest.update(generation.to_be_bytes());
    digest.finalize()[0]
}

fn registered_intent_names_are_canonical(intent: &RegisteredTargetIntent) -> bool {
    intent.generation > 0
        && intent.expected_inode > 0
        && intent.bucket == registered_bucket_index(intent.custody_id, intent.generation)
        && intent.slot_name == format!("v3_{}_{}", intent.custody_id, intent.generation)
}

fn bucket_name(index: u8) -> String {
    format!("{BUCKET_PREFIX}{index:02x}")
}

fn parse_bucket_name(name: &[u8]) -> Option<u8> {
    let text = std::str::from_utf8(name).ok()?;
    let hex = text.strip_prefix(BUCKET_PREFIX)?;
    (hex.len() == 2)
        .then(|| u8::from_str_radix(hex, 16).ok())
        .flatten()
}

fn cursor_marker_name(cycle: u64, bucket: u8) -> String {
    format!("{cycle:020}_{bucket:02x}")
}

fn parse_cursor_marker(name: &[u8]) -> Option<(u64, u8)> {
    let text = std::str::from_utf8(name).ok()?;
    let (cycle, bucket) = text.split_once('_')?;
    if cycle.len() != 20 || bucket.len() != 2 {
        return None;
    }
    Some((cycle.parse().ok()?, u8::from_str_radix(bucket, 16).ok()?))
}

fn ensure_private_child_directory(
    parent_fd: RawFd,
    name: &str,
    base_dev: u64,
) -> Result<OwnedFd, SkipReason> {
    match mkdirat(Some(parent_fd), name, Mode::from_bits_truncate(0o700)) {
        Ok(()) | Err(Errno::EEXIST) => {}
        Err(Errno::EXDEV) => return Err(SkipReason::MountOrDeviceCrossing),
        Err(_) => return Err(SkipReason::RejectedRecoveryEntry),
    }
    let fd = open_beneath_dir(parent_fd, OsStr::new(name))?;
    authenticate_private_directory(fd.as_raw_fd(), base_dev)?;
    Ok(fd)
}

fn open_reclaim_base(base: &Path) -> Result<(OwnedFd, u64), SkipReason> {
    let canonical =
        std::fs::canonicalize(base).map_err(|_| SkipReason::GitOrRootIdentityRefusal)?;
    if canonical != base {
        return Err(SkipReason::GitOrRootIdentityRefusal);
    }
    let raw = open(
        &canonical,
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW,
        Mode::empty(),
    )
    .map_err(map_open_error)?;
    // SAFETY: `open` returned a new owned descriptor.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let stat = fstat(fd.as_raw_fd()).map_err(|_| SkipReason::GitOrRootIdentityRefusal)?;
    Ok((fd, stat.st_dev as u64))
}

fn optional_private_child_directory(
    parent_fd: RawFd,
    name: &OsStr,
    base_dev: u64,
) -> Result<Option<OwnedFd>, RegisteredNamespaceState> {
    match fstatat(Some(parent_fd), name, AtFlags::AT_SYMLINK_NOFOLLOW) {
        Err(Errno::ENOENT) => Ok(None),
        Err(_) => Err(RegisteredNamespaceState::Refused(
            SkipReason::UnreadableEntry,
        )),
        Ok(stat) => {
            if !SFlag::from_bits_truncate(stat.st_mode).contains(SFlag::S_IFDIR) {
                return Err(RegisteredNamespaceState::Different);
            }
            let fd =
                open_beneath_dir(parent_fd, name).map_err(RegisteredNamespaceState::Refused)?;
            authenticate_private_directory(fd.as_raw_fd(), base_dev)
                .map_err(RegisteredNamespaceState::Refused)?;
            Ok(Some(fd))
        }
    }
}

pub(crate) fn probe_registered_target(
    base: &Path,
    intent: &RegisteredTargetIntent,
) -> RegisteredTargetProbe {
    if !registered_intent_names_are_canonical(intent) {
        return RegisteredTargetProbe {
            source: RegisteredNamespaceState::Refused(SkipReason::InvalidRecoveryEntry),
            destination: RegisteredNamespaceState::Refused(SkipReason::InvalidRecoveryEntry),
        };
    }
    let (base_fd, base_dev) = match open_reclaim_base(base) {
        Ok(value) => value,
        Err(reason) => {
            return RegisteredTargetProbe {
                source: RegisteredNamespaceState::Refused(reason),
                destination: RegisteredNamespaceState::Refused(reason),
            };
        }
    };
    let source = probe_registered_source(base_fd.as_raw_fd(), base_dev, intent);
    let destination = probe_registered_destination(base_fd.as_raw_fd(), base_dev, intent);
    RegisteredTargetProbe {
        source,
        destination,
    }
}

fn probe_registered_source(
    base_fd: RawFd,
    base_dev: u64,
    intent: &RegisteredTargetIntent,
) -> RegisteredNamespaceState {
    let allocation = intent.allocation_id.to_string();
    let root = match fstatat(
        Some(base_fd),
        allocation.as_str(),
        AtFlags::AT_SYMLINK_NOFOLLOW,
    ) {
        Err(Errno::ENOENT) => return RegisteredNamespaceState::Absent,
        Err(_) => return RegisteredNamespaceState::Refused(SkipReason::UnreadableEntry),
        Ok(stat) if !SFlag::from_bits_truncate(stat.st_mode).contains(SFlag::S_IFDIR) => {
            return RegisteredNamespaceState::Different;
        }
        Ok(_) => match open_beneath_dir(base_fd, OsStr::new(&allocation)) {
            Ok(fd) => fd,
            Err(reason) => return RegisteredNamespaceState::Refused(reason),
        },
    };
    if fstat(root.as_raw_fd()).is_ok_and(|stat| stat.st_dev as u64 != base_dev) {
        return RegisteredNamespaceState::Refused(SkipReason::MountOrDeviceCrossing);
    }
    probe_expected_directory(
        root.as_raw_fd(),
        OsStr::new("target"),
        base_dev,
        intent.expected_device,
        intent.expected_inode,
    )
}

fn probe_registered_destination(
    base_fd: RawFd,
    base_dev: u64,
    intent: &RegisteredTargetIntent,
) -> RegisteredNamespaceState {
    let queue = match optional_private_child_directory(base_fd, OsStr::new(QUEUE_NAME), base_dev) {
        Ok(Some(fd)) => fd,
        Ok(None) => return RegisteredNamespaceState::Absent,
        Err(state) => return state,
    };
    let bucket_name = bucket_name(intent.bucket);
    let bucket = match optional_private_child_directory(
        queue.as_raw_fd(),
        OsStr::new(&bucket_name),
        base_dev,
    ) {
        Ok(Some(fd)) => fd,
        Ok(None) => return RegisteredNamespaceState::Absent,
        Err(state) => return state,
    };
    let slot = match optional_private_child_directory(
        bucket.as_raw_fd(),
        OsStr::new(&intent.slot_name),
        base_dev,
    ) {
        Ok(Some(fd)) => fd,
        Ok(None) => return RegisteredNamespaceState::Absent,
        Err(state) => return state,
    };
    probe_expected_directory(
        slot.as_raw_fd(),
        OsStr::new("payload"),
        base_dev,
        intent.expected_device,
        intent.expected_inode,
    )
}

fn probe_expected_directory(
    parent_fd: RawFd,
    name: &OsStr,
    base_dev: u64,
    expected_device: u64,
    expected_inode: u64,
) -> RegisteredNamespaceState {
    let stat = match fstatat(Some(parent_fd), name, AtFlags::AT_SYMLINK_NOFOLLOW) {
        Err(Errno::ENOENT) => return RegisteredNamespaceState::Absent,
        Err(_) => return RegisteredNamespaceState::Refused(SkipReason::UnreadableEntry),
        Ok(stat) => stat,
    };
    if !SFlag::from_bits_truncate(stat.st_mode).contains(SFlag::S_IFDIR)
        || stat.st_dev as u64 != expected_device
        || stat.st_ino as u64 != expected_inode
    {
        return RegisteredNamespaceState::Different;
    }
    match open_beneath_dir(parent_fd, name) {
        Ok(fd) => match fstat(fd.as_raw_fd()) {
            Ok(opened)
                if opened.st_dev as u64 == base_dev
                    && opened.st_dev as u64 == expected_device
                    && opened.st_ino as u64 == expected_inode =>
            {
                RegisteredNamespaceState::Expected
            }
            Ok(_) => RegisteredNamespaceState::Different,
            Err(_) => RegisteredNamespaceState::Refused(SkipReason::UnreadableEntry),
        },
        Err(reason) => RegisteredNamespaceState::Refused(reason),
    }
}

pub(crate) fn delete_registered_target(
    base: &Path,
    intent: &RegisteredTargetIntent,
    dry_run: bool,
) -> TargetReclaimOutcome {
    if !registered_intent_names_are_canonical(intent) {
        return TargetReclaimOutcome::refused(SkipReason::InvalidRecoveryEntry);
    }
    let (base_fd, base_dev) = match open_reclaim_base(base) {
        Ok(value) => value,
        Err(reason) => return TargetReclaimOutcome::refused(reason),
    };
    let hierarchy = open_registered_hierarchy(base_fd.as_raw_fd(), base_dev, intent);
    let (queue, bucket, slot) = match hierarchy {
        Ok(Some(value)) => value,
        Ok(None) => {
            // A status preview is read-only even if the payload raced away
            // between its namespace probe and this inspection. In particular,
            // it must not turn an absence observation into a durable deletion
            // proof by syncing any parent namespace.
            if dry_run {
                return TargetReclaimOutcome::refused(SkipReason::StagedDeletionIncomplete);
            }
            return if sync_registered_target_absence(base, intent).unwrap_or(false) {
                TargetReclaimOutcome {
                    kind: TargetReclaimKind::RecoveredRemoved,
                    bytes: 0,
                    reason: None,
                    terminal_rejection: false,
                }
            } else {
                TargetReclaimOutcome::refused(SkipReason::StagedDeletionIncomplete)
            };
        }
        Err(reason) => return TargetReclaimOutcome::refused(reason),
    };
    let payload = match authenticate_expected_stage(
        slot.as_raw_fd(),
        "payload",
        base_dev,
        intent.expected_device,
        intent.expected_inode,
    ) {
        Ok(fd) => fd,
        Err(_) => return TargetReclaimOutcome::refused(SkipReason::RejectedRecoveryEntry),
    };
    if dry_run {
        let footprint = match allocated_tree_footprint(payload.as_raw_fd(), base_dev) {
            Ok(footprint) => footprint,
            Err(reason) => return TargetReclaimOutcome::refused(reason),
        };
        return TargetReclaimOutcome::inspected(footprint.bytes);
    }
    // A recovered intent may have crossed the rename seam before the source
    // parent was durably synced. Re-establish that publication barrier before
    // removing the detached payload. This is deliberately descriptor-relative:
    // a path observation cannot prove that the old source name cannot return
    // after a later crash.
    if let Err(reason) =
        replay_registered_publication_from_base(base_fd.as_raw_fd(), base_dev, intent)
    {
        return TargetReclaimOutcome::refused(reason);
    }
    pause_target_deletion_signal_for_test(intent.custody_id);
    let outcome =
        with_registered_delete_identity_for_test((intent.custody_id, intent.generation), || {
            finish_staged(
                slot.as_raw_fd(),
                "payload",
                payload,
                0,
                true,
                base_dev,
                DeletionCredits::default(),
            )
        });
    if outcome.kind != TargetReclaimKind::RecoveredRemoved {
        return outcome;
    }
    drop(slot);
    if unlinkat(
        Some(bucket.as_raw_fd()),
        intent.slot_name.as_str(),
        UnlinkatFlags::RemoveDir,
    )
    .is_err()
        || fsync(bucket.as_raw_fd()).is_err()
        || fsync(queue.as_raw_fd()).is_err()
        || fsync(base_fd.as_raw_fd()).is_err()
    {
        return TargetReclaimOutcome {
            kind: TargetReclaimKind::RecoveredPending,
            bytes: outcome.bytes,
            reason: Some(SkipReason::StagedDeletionIncomplete),
            terminal_rejection: false,
        };
    }
    outcome
}

/// Replay the durability barriers for a registered rename without consulting
/// the current custody owner. Recovery must retain an exact detached payload
/// across lifecycle changes, so the journaled allocation identity is the only
/// source-parent authority used here.
pub(crate) fn replay_registered_publication(
    base: &Path,
    intent: &RegisteredTargetIntent,
) -> Result<(), SkipReason> {
    if !registered_intent_names_are_canonical(intent) {
        return Err(SkipReason::InvalidRecoveryEntry);
    }
    let (base_fd, base_dev) = open_reclaim_base(base)?;
    replay_registered_publication_from_base(base_fd.as_raw_fd(), base_dev, intent)
}

fn replay_registered_publication_from_base(
    base_fd: RawFd,
    base_dev: u64,
    intent: &RegisteredTargetIntent,
) -> Result<(), SkipReason> {
    sync_registered_source_parent(base_fd, base_dev, intent)?;
    let Some((queue, bucket, slot)) = open_registered_hierarchy(base_fd, base_dev, intent)? else {
        return Err(SkipReason::StagedDeletionIncomplete);
    };
    // Authenticate the exact payload again rather than trusting a prior
    // namespace probe made before the barrier descriptors were opened.
    let payload = authenticate_expected_stage(
        slot.as_raw_fd(),
        "payload",
        base_dev,
        intent.expected_device,
        intent.expected_inode,
    )?;
    drop(payload);
    if maybe_fail_registered_durability_for_test(
        intent,
        RegisteredDurabilitySeam::RecoveryPublication,
    )
    .is_err()
        || fsync(slot.as_raw_fd()).is_err()
        || fsync(bucket.as_raw_fd()).is_err()
        || fsync(queue.as_raw_fd()).is_err()
    {
        return Err(SkipReason::StagedDeletionIncomplete);
    }
    Ok(())
}

fn sync_registered_source_parent(
    base_fd: RawFd,
    base_dev: u64,
    intent: &RegisteredTargetIntent,
) -> Result<(), SkipReason> {
    let allocation = intent.allocation_id.to_string();
    let stat = match fstatat(
        Some(base_fd),
        allocation.as_str(),
        AtFlags::AT_SYMLINK_NOFOLLOW,
    ) {
        Err(Errno::ENOENT) => {
            if maybe_fail_registered_durability_for_test(
                intent,
                RegisteredDurabilitySeam::RecoveryPublication,
            )
            .is_err()
                || fsync(base_fd).is_err()
            {
                return Err(SkipReason::StagedDeletionIncomplete);
            }
            return match fstatat(
                Some(base_fd),
                allocation.as_str(),
                AtFlags::AT_SYMLINK_NOFOLLOW,
            ) {
                Err(Errno::ENOENT) => Ok(()),
                Ok(_) => Err(SkipReason::TargetIdentityChanged),
                Err(_) => Err(SkipReason::UnreadableEntry),
            };
        }
        Err(_) => return Err(SkipReason::UnreadableEntry),
        Ok(stat) if !SFlag::from_bits_truncate(stat.st_mode).contains(SFlag::S_IFDIR) => {
            return Err(SkipReason::TargetIdentityChanged);
        }
        Ok(stat) => stat,
    };
    let root = open_beneath_dir(base_fd, OsStr::new(&allocation))?;
    let opened = fstat(root.as_raw_fd()).map_err(|_| SkipReason::UnreadableEntry)?;
    if opened.st_dev != stat.st_dev
        || opened.st_ino != stat.st_ino
        || opened.st_dev as u64 != base_dev
    {
        return Err(SkipReason::TargetIdentityChanged);
    }
    if maybe_fail_registered_durability_for_test(
        intent,
        RegisteredDurabilitySeam::RecoveryPublication,
    )
    .is_err()
        || fsync(root.as_raw_fd()).is_err()
    {
        return Err(SkipReason::StagedDeletionIncomplete);
    }
    let current = fstatat(
        Some(base_fd),
        allocation.as_str(),
        AtFlags::AT_SYMLINK_NOFOLLOW,
    )
    .map_err(|_| SkipReason::TargetIdentityChanged)?;
    if current.st_dev != opened.st_dev || current.st_ino != opened.st_ino {
        return Err(SkipReason::TargetIdentityChanged);
    }
    Ok(())
}

fn open_registered_hierarchy(
    base_fd: RawFd,
    base_dev: u64,
    intent: &RegisteredTargetIntent,
) -> Result<Option<(OwnedFd, OwnedFd, OwnedFd)>, SkipReason> {
    let optional = |value: Result<Option<OwnedFd>, RegisteredNamespaceState>| match value {
        Ok(value) => Ok(value),
        Err(RegisteredNamespaceState::Refused(reason)) => Err(reason),
        Err(RegisteredNamespaceState::Different) => Err(SkipReason::RejectedRecoveryEntry),
        Err(RegisteredNamespaceState::Absent | RegisteredNamespaceState::Expected) => {
            unreachable!("optional directory returns only refusal or different errors")
        }
    };
    let Some(queue) = optional(optional_private_child_directory(
        base_fd,
        OsStr::new(QUEUE_NAME),
        base_dev,
    ))?
    else {
        return Ok(None);
    };
    let bucket_name = bucket_name(intent.bucket);
    let Some(bucket) = optional(optional_private_child_directory(
        queue.as_raw_fd(),
        OsStr::new(&bucket_name),
        base_dev,
    ))?
    else {
        return Ok(None);
    };
    let Some(slot) = optional(optional_private_child_directory(
        bucket.as_raw_fd(),
        OsStr::new(&intent.slot_name),
        base_dev,
    ))?
    else {
        return Ok(None);
    };
    Ok(Some((queue, bucket, slot)))
}

pub(crate) fn sync_registered_target_absence(
    base: &Path,
    intent: &RegisteredTargetIntent,
) -> Result<bool, SkipReason> {
    if !registered_intent_names_are_canonical(intent) {
        return Err(SkipReason::InvalidRecoveryEntry);
    }
    let (base_fd, base_dev) = open_reclaim_base(base)?;
    // Completion after an already-removed payload still needs the source
    // publication barrier. If the allocation was tombstoned, the authenticated
    // base directory is the parent that proves that absence instead.
    sync_registered_source_parent(base_fd.as_raw_fd(), base_dev, intent)?;
    let optional = |value: Result<Option<OwnedFd>, RegisteredNamespaceState>| match value {
        Ok(value) => Ok(value),
        Err(RegisteredNamespaceState::Refused(reason)) => Err(reason),
        Err(RegisteredNamespaceState::Different) => Err(SkipReason::RejectedRecoveryEntry),
        Err(RegisteredNamespaceState::Absent | RegisteredNamespaceState::Expected) => {
            unreachable!("optional directory returns only refusal or different errors")
        }
    };
    let Some(queue) = optional(optional_private_child_directory(
        base_fd.as_raw_fd(),
        OsStr::new(QUEUE_NAME),
        base_dev,
    ))?
    else {
        fsync(base_fd.as_raw_fd()).map_err(|_| SkipReason::StagedDeletionIncomplete)?;
        return Ok(true);
    };
    let bucket_name = bucket_name(intent.bucket);
    let Some(bucket) = optional(optional_private_child_directory(
        queue.as_raw_fd(),
        OsStr::new(&bucket_name),
        base_dev,
    ))?
    else {
        fsync(queue.as_raw_fd()).map_err(|_| SkipReason::StagedDeletionIncomplete)?;
        fsync(base_fd.as_raw_fd()).map_err(|_| SkipReason::StagedDeletionIncomplete)?;
        return Ok(true);
    };
    let Some(slot) = optional(optional_private_child_directory(
        bucket.as_raw_fd(),
        OsStr::new(&intent.slot_name),
        base_dev,
    ))?
    else {
        fsync(bucket.as_raw_fd()).map_err(|_| SkipReason::StagedDeletionIncomplete)?;
        fsync(queue.as_raw_fd()).map_err(|_| SkipReason::StagedDeletionIncomplete)?;
        fsync(base_fd.as_raw_fd()).map_err(|_| SkipReason::StagedDeletionIncomplete)?;
        return Ok(true);
    };
    match fstatat(
        Some(slot.as_raw_fd()),
        "payload",
        AtFlags::AT_SYMLINK_NOFOLLOW,
    ) {
        Err(Errno::ENOENT) => {}
        Ok(_) => return Ok(false),
        Err(_) => return Err(SkipReason::UnreadableEntry),
    }
    drop(slot);
    if unlinkat(
        Some(bucket.as_raw_fd()),
        intent.slot_name.as_str(),
        UnlinkatFlags::RemoveDir,
    )
    .is_err()
    {
        return Ok(false);
    }
    fsync(bucket.as_raw_fd()).map_err(|_| SkipReason::StagedDeletionIncomplete)?;
    fsync(queue.as_raw_fd()).map_err(|_| SkipReason::StagedDeletionIncomplete)?;
    fsync(base_fd.as_raw_fd()).map_err(|_| SkipReason::StagedDeletionIncomplete)?;
    Ok(true)
}

fn read_recovery_cursor(cursor_fd: RawFd) -> Result<Option<(u64, u8, String)>, SkipReason> {
    let dup_fd = dup(cursor_fd).map_err(|_| SkipReason::UnreadableEntry)?;
    let mut dir = Dir::from_fd(dup_fd).map_err(|_| SkipReason::UnreadableEntry)?;
    let mut marker = None;
    for entry in dir.iter() {
        let entry = entry.map_err(|_| SkipReason::UnreadableEntry)?;
        let name = entry.file_name();
        if name.to_bytes() == b"." || name.to_bytes() == b".." {
            continue;
        }
        if name.to_bytes() == BACKOFF_DIR.as_bytes() {
            continue;
        }
        let Some((cycle, bucket)) = parse_cursor_marker(name.to_bytes()) else {
            return Err(SkipReason::RejectedRecoveryEntry);
        };
        if marker.is_some() {
            return Err(SkipReason::RejectedRecoveryEntry);
        }
        marker = Some((cycle, bucket, name.to_string_lossy().into_owned()));
    }
    Ok(marker)
}

fn ensure_recovery_cursor(
    queue_fd: RawFd,
    base_dev: u64,
    initial_bucket: u8,
) -> Result<(OwnedFd, u64, u8, String), SkipReason> {
    let cursor_fd = ensure_private_child_directory(queue_fd, CURSOR_DIR, base_dev)?;
    if let Some((cycle, bucket, marker)) = read_recovery_cursor(cursor_fd.as_raw_fd())? {
        return Ok((cursor_fd, cycle, bucket, marker));
    }
    let marker = cursor_marker_name(0, initial_bucket);
    mkdirat(
        Some(cursor_fd.as_raw_fd()),
        marker.as_str(),
        Mode::from_bits_truncate(0o700),
    )
    .map_err(|error| {
        if error == Errno::EXDEV {
            SkipReason::MountOrDeviceCrossing
        } else {
            SkipReason::StagedDeletionIncomplete
        }
    })?;
    fsync(cursor_fd.as_raw_fd()).map_err(|_| SkipReason::StagedDeletionIncomplete)?;
    fsync(queue_fd).map_err(|_| SkipReason::StagedDeletionIncomplete)?;
    Ok((cursor_fd, 0, initial_bucket, marker))
}

/// Inspect or resume genuine staged entries before selecting new candidates.
#[cfg(test)]
pub(crate) fn recover_staged_targets(base: &Path, dry_run: bool) -> Vec<TargetReclaimOutcome> {
    let state = ReclaimPassState::new();
    recover_staged_targets_with_state(base, dry_run, &state)
}

#[cfg(test)]
pub(crate) fn recover_staged_targets_with_state(
    base: &Path,
    dry_run: bool,
    state: &Arc<ReclaimPassState>,
) -> Vec<TargetReclaimOutcome> {
    recover_staged_targets_run_with_state(base, dry_run, state).outcomes
}

pub(crate) fn recover_staged_targets_run_with_state(
    base: &Path,
    dry_run: bool,
    state: &Arc<ReclaimPassState>,
) -> TargetRecoveryRun {
    with_reclaim_pass_state(state, || recover_staged_targets_inner(base, dry_run))
}

fn recovery_failure(reason: SkipReason) -> TargetRecoveryRun {
    TargetRecoveryRun {
        outcomes: vec![TargetReclaimOutcome::refused(reason)],
        ..TargetRecoveryRun::default()
    }
}

fn recover_staged_targets_inner(base: &Path, dry_run: bool) -> TargetRecoveryRun {
    let pass = current_pass_state();
    if let Err(reason) = pass.checkpoint(0) {
        return recovery_failure(reason);
    }
    let base_path = match std::fs::canonicalize(base) {
        Ok(path) => path,
        Err(_) => {
            return recovery_failure(SkipReason::GitOrRootIdentityRefusal);
        }
    };
    let base_raw = match open(
        &base_path,
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(error) => return recovery_failure(map_open_error(error)),
    };
    // SAFETY: `open` returned a new owned descriptor.
    let base_fd = unsafe { OwnedFd::from_raw_fd(base_raw) };
    let base_stat = match fstat(base_fd.as_raw_fd()) {
        Ok(stat) => stat,
        Err(_) => return recovery_failure(SkipReason::GitOrRootIdentityRefusal),
    };
    let queue_lstat = match fstatat(
        Some(base_fd.as_raw_fd()),
        QUEUE_NAME,
        AtFlags::AT_SYMLINK_NOFOLLOW,
    ) {
        Ok(stat) => stat,
        Err(Errno::ENOENT) => return TargetRecoveryRun::default(),
        Err(_) => return recovery_failure(SkipReason::RejectedRecoveryEntry),
    };
    if !SFlag::from_bits_truncate(queue_lstat.st_mode).contains(SFlag::S_IFDIR) {
        return recovery_failure(SkipReason::RejectedRecoveryEntry);
    }
    let queue_fd = match open_beneath_dir(base_fd.as_raw_fd(), OsStr::new(QUEUE_NAME)) {
        Ok(fd) => fd,
        Err(reason) => return recovery_failure(reason),
    };
    let queue_stat = match fstat(queue_fd.as_raw_fd()) {
        Ok(stat) => stat,
        Err(_) => return recovery_failure(SkipReason::RejectedRecoveryEntry),
    };
    let mode = queue_stat.st_mode as u32 & 0o777;
    // SAFETY: `geteuid` has no preconditions.
    if queue_stat.st_dev as u64 != base_stat.st_dev as u64 {
        return recovery_failure(SkipReason::MountOrDeviceCrossing);
    }
    if mode != 0o700 || queue_stat.st_uid != unsafe { nix::libc::geteuid() } {
        return recovery_failure(SkipReason::RejectedRecoveryEntry);
    }

    let base_dev = base_stat.st_dev as u64;
    let mut outcomes = Vec::new();
    let mut migration = match migrate_legacy_entries(
        base_fd.as_raw_fd(),
        queue_fd.as_raw_fd(),
        base_dev,
        dry_run,
    ) {
        Ok(migration) => migration,
        Err(reason) => {
            outcomes.push(TargetReclaimOutcome::refused(reason));
            LegacyMigration::default()
        }
    };
    outcomes.append(&mut migration.outcomes);

    let cursor = if dry_run {
        match observe_recovery_cursor(queue_fd.as_raw_fd(), base_dev) {
            Ok(cursor) => cursor,
            Err(reason) => {
                outcomes.push(TargetReclaimOutcome::refused(reason));
                None
            }
        }
    } else {
        let initial = migration
            .first_bucket
            .or_else(|| {
                first_nonempty_bucket(queue_fd.as_raw_fd(), base_dev)
                    .ok()
                    .flatten()
            })
            .unwrap_or(0);
        match ensure_recovery_cursor(queue_fd.as_raw_fd(), base_dev, initial) {
            Ok(cursor) => Some(cursor),
            Err(reason) => {
                outcomes.push(TargetReclaimOutcome::refused(reason));
                None
            }
        }
    };

    let Some((cursor_fd, cycle_before, bucket, marker)) = cursor else {
        return TargetRecoveryRun {
            outcomes,
            evidence: TargetRecoverySweepEvidence {
                legacy_migrated: migration.migrated,
                residual_entries: None,
                ..TargetRecoverySweepEvidence::default()
            },
        };
    };

    let (cycle_after, next_bucket, wrapped) = if bucket == u8::MAX {
        (cycle_before.saturating_add(1), 0, true)
    } else {
        (cycle_before, bucket + 1, false)
    };
    let mut reserved = false;
    let mut cursor_after = Some(format!("{bucket:02x}"));
    if !dry_run {
        let next_marker = cursor_marker_name(cycle_after, next_bucket);
        match renameat2(
            Some(cursor_fd.as_raw_fd()),
            marker.as_str(),
            Some(cursor_fd.as_raw_fd()),
            next_marker.as_str(),
            RenameFlags::RENAME_NOREPLACE,
        ) {
            Ok(()) => {
                if maybe_fail_cursor_fsync_for_test(queue_fd.as_raw_fd()).is_ok()
                    && fsync(cursor_fd.as_raw_fd()).is_ok()
                    && fsync(queue_fd.as_raw_fd()).is_ok()
                {
                    reserved = true;
                    cursor_after = Some(format!("{next_bucket:02x}"));
                } else {
                    // The marker name changed, but durability is unknown. Do
                    // not report either the old or new cursor as established.
                    cursor_after = None;
                    outcomes.push(TargetReclaimOutcome::refused(
                        SkipReason::StagedDeletionIncomplete,
                    ));
                }
            }
            Err(Errno::EXDEV) => outcomes.push(TargetReclaimOutcome::refused(
                SkipReason::MountOrDeviceCrossing,
            )),
            Err(_) => outcomes.push(TargetReclaimOutcome::refused(
                SkipReason::StagedDeletionIncomplete,
            )),
        }
    }

    let deleted_before = pass.entries_deleted();
    let mut nonprogress_count = 0_u32;
    let residual_entries = None;
    if dry_run || reserved {
        match recover_bucket(
            base_fd.as_raw_fd(),
            queue_fd.as_raw_fd(),
            cursor_fd.as_raw_fd(),
            bucket,
            dry_run,
            base_dev,
        ) {
            Ok(bucket_run) => {
                nonprogress_count = bucket_run.nonprogress_count;
                outcomes.extend(bucket_run.outcomes);
            }
            Err(reason) => {
                outcomes.push(TargetReclaimOutcome::refused(reason));
            }
        }
    }
    let entries_deleted = pass.entries_deleted().saturating_sub(deleted_before);
    TargetRecoveryRun {
        outcomes,
        evidence: TargetRecoverySweepEvidence {
            cycle_before,
            cycle_after: if reserved { cycle_after } else { cycle_before },
            cursor_before: Some(format!("{bucket:02x}")),
            cursor_after,
            reserved,
            wrapped: reserved && wrapped,
            entries_deleted,
            residual_entries,
            legacy_migrated: migration.migrated,
            nonprogress_count,
        },
    }
}

#[derive(Default)]
struct LegacyMigration {
    migrated: u32,
    first_bucket: Option<u8>,
    outcomes: Vec<TargetReclaimOutcome>,
}

fn migrate_legacy_entries(
    base_fd: RawFd,
    queue_fd: RawFd,
    base_dev: u64,
    dry_run: bool,
) -> Result<LegacyMigration, SkipReason> {
    let dup_fd = dup(queue_fd).map_err(|_| SkipReason::UnreadableEntry)?;
    let mut dir = Dir::from_fd(dup_fd).map_err(|_| SkipReason::UnreadableEntry)?;
    let mut names = Vec::new();
    let scan_limit = u64::from(BUCKET_COUNT)
        .saturating_add(1)
        .saturating_add(LEGACY_MIGRATION_ENTRIES_PER_PASS);
    let mut scanned = 0_u64;
    for entry in dir.iter() {
        let entry = entry.map_err(|_| SkipReason::UnreadableEntry)?;
        let name = entry.file_name();
        if name.to_bytes() == b"." || name.to_bytes() == b".." {
            continue;
        }
        scanned = scanned.saturating_add(1);
        if scanned > scan_limit {
            break;
        }
        if name.to_bytes() == CURSOR_DIR.as_bytes() || parse_bucket_name(name.to_bytes()).is_some()
        {
            continue;
        }
        if names.len() as u64 >= LEGACY_MIGRATION_ENTRIES_PER_PASS {
            break;
        }
        names.push(name.to_owned());
    }
    names.sort_by(|a, b| a.to_bytes().cmp(b.to_bytes()));
    let mut result = LegacyMigration::default();
    for name in names {
        let Some(text) = name.to_str().ok() else {
            migrate_malformed_legacy_entry(
                &mut result,
                base_fd,
                queue_fd,
                &name,
                base_dev,
                dry_run,
                SkipReason::InvalidRecoveryEntry,
            );
            continue;
        };
        if let Err(reason) = authenticate_stage_from_name(queue_fd, text, base_dev) {
            migrate_malformed_legacy_entry(
                &mut result,
                base_fd,
                queue_fd,
                &name,
                base_dev,
                dry_run,
                reason,
            );
            continue;
        }
        let bucket = stage_bucket_index(text);
        result.first_bucket = Some(
            result
                .first_bucket
                .map_or(bucket, |current| current.min(bucket)),
        );
        if dry_run {
            continue;
        }
        let bucket_fd =
            match ensure_private_child_directory(queue_fd, &bucket_name(bucket), base_dev) {
                Ok(fd) => fd,
                Err(reason) => {
                    quarantine_legacy_stage(&mut result, base_fd, queue_fd, text, base_dev, reason);
                    continue;
                }
            };
        let rename = renameat2(
            Some(queue_fd),
            text,
            Some(bucket_fd.as_raw_fd()),
            text,
            RenameFlags::RENAME_NOREPLACE,
        );
        match rename {
            Ok(()) => {
                if fsync(bucket_fd.as_raw_fd()).is_ok() && fsync(queue_fd).is_ok() {
                    result.migrated = result.migrated.saturating_add(1);
                } else {
                    result.outcomes.push(TargetReclaimOutcome::refused(
                        SkipReason::StagedDeletionIncomplete,
                    ));
                }
            }
            Err(error) => {
                let reason = if error == Errno::EXDEV {
                    SkipReason::MountOrDeviceCrossing
                } else if error == Errno::EEXIST {
                    SkipReason::StageConflict
                } else {
                    SkipReason::StagedDeletionIncomplete
                };
                quarantine_legacy_stage(&mut result, base_fd, queue_fd, text, base_dev, reason);
            }
        }
    }
    Ok(result)
}

fn migrate_malformed_legacy_entry(
    result: &mut LegacyMigration,
    base_fd: RawFd,
    queue_fd: RawFd,
    name: &CStr,
    base_dev: u64,
    dry_run: bool,
    reason: SkipReason,
) {
    if dry_run {
        result.outcomes.push(TargetReclaimOutcome::refused(reason));
        return;
    }
    match move_malformed_stage_to_rejected(base_fd, queue_fd, name, base_dev) {
        Ok(()) => result.outcomes.push(TargetReclaimOutcome {
            kind: TargetReclaimKind::Refused,
            bytes: 0,
            reason: Some(reason),
            terminal_rejection: true,
        }),
        Err(move_reason) => {
            result
                .outcomes
                .push(TargetReclaimOutcome::refused(move_reason));
        }
    }
}

fn quarantine_legacy_stage(
    result: &mut LegacyMigration,
    base_fd: RawFd,
    queue_fd: RawFd,
    entry_name: &str,
    base_dev: u64,
    reason: SkipReason,
) {
    match move_stage_to_rejected(base_fd, queue_fd, entry_name, base_dev) {
        Ok(()) => result.outcomes.push(TargetReclaimOutcome {
            kind: TargetReclaimKind::Refused,
            bytes: 0,
            reason: Some(reason),
            terminal_rejection: true,
        }),
        Err(failure) => result
            .outcomes
            .push(TargetReclaimOutcome::refused(failure.reason)),
    }
}

fn authenticate_stage_from_name(
    parent_fd: RawFd,
    entry_name: &str,
    base_dev: u64,
) -> Result<OwnedFd, SkipReason> {
    let (_, _, expected_dev, expected_ino) =
        parse_stage_name(entry_name).ok_or(SkipReason::InvalidRecoveryEntry)?;
    authenticate_expected_stage(parent_fd, entry_name, base_dev, expected_dev, expected_ino)
}

fn observe_recovery_cursor(
    queue_fd: RawFd,
    base_dev: u64,
) -> Result<Option<(OwnedFd, u64, u8, String)>, SkipReason> {
    let cursor_fd = match open_beneath_dir(queue_fd, OsStr::new(CURSOR_DIR)) {
        Ok(fd) => fd,
        Err(SkipReason::GitOrRootIdentityRefusal) => return Ok(None),
        Err(reason) => return Err(reason),
    };
    authenticate_private_directory(cursor_fd.as_raw_fd(), base_dev)?;
    Ok(read_recovery_cursor(cursor_fd.as_raw_fd())?
        .map(|(cycle, bucket, marker)| (cursor_fd, cycle, bucket, marker)))
}

fn first_nonempty_bucket(queue_fd: RawFd, base_dev: u64) -> Result<Option<u8>, SkipReason> {
    for index in 0..BUCKET_COUNT {
        let index = index as u8;
        let fd = match open_beneath_dir(queue_fd, OsStr::new(&bucket_name(index))) {
            Ok(fd) => fd,
            Err(SkipReason::GitOrRootIdentityRefusal) => continue,
            Err(reason) => return Err(reason),
        };
        authenticate_private_directory(fd.as_raw_fd(), base_dev)?;
        let dup_fd = dup(fd.as_raw_fd()).map_err(|_| SkipReason::UnreadableEntry)?;
        let mut dir = Dir::from_fd(dup_fd).map_err(|_| SkipReason::UnreadableEntry)?;
        if dir.iter().any(|entry| {
            entry.ok().is_some_and(|entry| {
                let name = entry.file_name();
                name.to_bytes() != b"." && name.to_bytes() != b".."
            })
        }) {
            return Ok(Some(index));
        }
    }
    Ok(None)
}

struct BucketRecoveryRun {
    outcomes: Vec<TargetReclaimOutcome>,
    nonprogress_count: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RotationFaultPoint {
    BucketCreate,
    QueueParentFsync,
    Rename,
    SourceFsync,
    DestinationFsync,
}

#[cfg(test)]
fn rotation_faults() -> &'static Mutex<HashMap<String, RotationFaultPoint>> {
    static FAULTS: std::sync::OnceLock<Mutex<HashMap<String, RotationFaultPoint>>> =
        std::sync::OnceLock::new();
    FAULTS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(test)]
struct RotationFaultReset(String);

#[cfg(test)]
impl Drop for RotationFaultReset {
    fn drop(&mut self) {
        rotation_faults()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.0);
    }
}

#[cfg(test)]
fn set_rotation_fault_for_test(entry_name: &str, point: RotationFaultPoint) -> RotationFaultReset {
    rotation_faults()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(entry_name.to_string(), point);
    RotationFaultReset(entry_name.to_string())
}

#[cfg(test)]
fn maybe_fail_rotation_for_test(
    entry_name: &str,
    point: RotationFaultPoint,
) -> Result<(), SkipReason> {
    let mut faults = rotation_faults()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if faults.get(entry_name).copied() == Some(point) {
        faults.remove(entry_name);
        Err(SkipReason::StagedDeletionIncomplete)
    } else {
        Ok(())
    }
}

#[cfg(not(test))]
fn maybe_fail_rotation_for_test(
    _entry_name: &str,
    _point: RotationFaultPoint,
) -> Result<(), SkipReason> {
    Ok(())
}

fn recover_bucket(
    base_fd: RawFd,
    queue_fd: RawFd,
    _cursor_fd: RawFd,
    bucket: u8,
    dry_run: bool,
    base_dev: u64,
) -> Result<BucketRecoveryRun, SkipReason> {
    let bucket_fd = match open_beneath_dir(queue_fd, OsStr::new(&bucket_name(bucket))) {
        Ok(fd) => fd,
        Err(SkipReason::GitOrRootIdentityRefusal) => {
            return Ok(BucketRecoveryRun {
                outcomes: Vec::new(),
                nonprogress_count: 0,
            });
        }
        Err(reason) => return Err(reason),
    };
    authenticate_private_directory(bucket_fd.as_raw_fd(), base_dev)?;
    let pass = current_pass_state();
    let duplicate = dup(bucket_fd.as_raw_fd()).map_err(|_| SkipReason::UnreadableEntry)?;
    let mut dir = Dir::from_fd(duplicate).map_err(|_| SkipReason::UnreadableEntry)?;
    let mut entries = dir.iter();
    let mut names = Vec::new();
    let mut budget_stop = None;
    loop {
        if let Err(reason) = pass.charge_recovery_entry() {
            budget_stop = Some(reason);
            break;
        }
        let Some(entry) = entries.next() else {
            break;
        };
        let entry = entry.map_err(|_| SkipReason::UnreadableEntry)?;
        let name = entry.file_name();
        if name.to_bytes() == b"." || name.to_bytes() == b".." {
            continue;
        }
        names.push(name.to_owned());
    }
    let mut outcomes = Vec::new();
    let mut nonprogress_count = 0_u32;
    for name in names {
        if name.to_bytes().starts_with(b"v3_") {
            outcomes.push(TargetReclaimOutcome::refused(
                SkipReason::StagedDeletionIncomplete,
            ));
            nonprogress_count = nonprogress_count.saturating_add(1);
            continue;
        }
        let authentication = name
            .to_str()
            .map_err(|_| SkipReason::InvalidRecoveryEntry)
            .and_then(|text| {
                if stage_bucket_index(text) != bucket {
                    return Err(SkipReason::InvalidRecoveryEntry);
                }
                authenticate_stage_from_name(bucket_fd.as_raw_fd(), text, base_dev)
            });
        if let Err(reason) = authentication {
            if matches!(
                reason,
                SkipReason::MountOrDeviceCrossing | SkipReason::Openat2Unavailable
            ) {
                outcomes.push(TargetReclaimOutcome::refused(reason));
                if dry_run {
                    nonprogress_count = nonprogress_count.saturating_add(1);
                }
                continue;
            }
            let moved = !dry_run
                && move_malformed_stage_to_rejected(
                    base_fd,
                    bucket_fd.as_raw_fd(),
                    &name,
                    base_dev,
                )
                .is_ok();
            outcomes.push(TargetReclaimOutcome {
                kind: TargetReclaimKind::Refused,
                bytes: 0,
                reason: Some(if moved {
                    reason
                } else {
                    SkipReason::RejectedRecoveryEntry
                }),
                terminal_rejection: moved,
            });
            if dry_run && !moved {
                nonprogress_count = nonprogress_count.saturating_add(1);
            }
            continue;
        }
        let outcome = recover_one(base_fd, bucket_fd.as_raw_fd(), &name, dry_run, base_dev);
        if !dry_run
            && !outcome.terminal_rejection
            && matches!(
                outcome.kind,
                TargetReclaimKind::RecoveredPending | TargetReclaimKind::Refused
            )
        {
            if let Ok(text) = name.to_str() {
                let _ = rotate_stage(queue_fd, bucket_fd.as_raw_fd(), bucket, text, base_dev);
            }
        }
        outcomes.push(outcome);
    }
    if let Some(reason) = budget_stop {
        outcomes.push(TargetReclaimOutcome::refused(reason));
    }
    Ok(BucketRecoveryRun {
        outcomes,
        nonprogress_count,
    })
}

fn rotate_stage(
    queue_fd: RawFd,
    source_fd: RawFd,
    source_bucket: u8,
    entry_name: &str,
    base_dev: u64,
) -> Result<(), SkipReason> {
    let pinned = authenticate_stage_from_name(source_fd, entry_name, base_dev)?;
    authenticate_stage_from_name(source_fd, entry_name, base_dev)?;
    let destination_bucket = source_bucket.wrapping_add(1);
    let destination_name = rotated_stage_name(entry_name, destination_bucket)
        .ok_or(SkipReason::StagedDeletionIncomplete)?;
    maybe_fail_rotation_for_test(entry_name, RotationFaultPoint::BucketCreate)?;
    let destination_fd =
        ensure_private_child_directory(queue_fd, &bucket_name(destination_bucket), base_dev)?;
    // The bucket name lives in the queue directory. Publish that link before
    // moving the only active stage name into it.
    maybe_fail_rotation_for_test(entry_name, RotationFaultPoint::QueueParentFsync)?;
    fsync(queue_fd).map_err(|_| SkipReason::StagedDeletionIncomplete)?;
    maybe_fail_rotation_for_test(entry_name, RotationFaultPoint::Rename)?;
    renameat2(
        Some(source_fd),
        entry_name,
        Some(destination_fd.as_raw_fd()),
        destination_name.as_str(),
        RenameFlags::RENAME_NOREPLACE,
    )
    .map_err(|error| {
        if error == Errno::EXDEV {
            SkipReason::MountOrDeviceCrossing
        } else if error == Errno::EEXIST {
            SkipReason::StageConflict
        } else {
            SkipReason::StagedDeletionIncomplete
        }
    })?;
    maybe_fail_rotation_for_test(entry_name, RotationFaultPoint::SourceFsync)?;
    fsync(source_fd).map_err(|_| SkipReason::StagedDeletionIncomplete)?;
    maybe_fail_rotation_for_test(entry_name, RotationFaultPoint::DestinationFsync)?;
    fsync(destination_fd.as_raw_fd()).map_err(|_| SkipReason::StagedDeletionIncomplete)?;
    drop(pinned);
    Ok(())
}

fn move_malformed_stage_to_rejected(
    base_fd: RawFd,
    parent_fd: RawFd,
    name: &CStr,
    base_dev: u64,
) -> Result<(), SkipReason> {
    let stat = fstatat(Some(parent_fd), name, AtFlags::AT_SYMLINK_NOFOLLOW)
        .map_err(|_| SkipReason::RejectedRecoveryEntry)?;
    if stat.st_dev as u64 != base_dev {
        return Err(SkipReason::MountOrDeviceCrossing);
    }
    let digest = Sha256::digest(name.to_bytes());
    let terminal_name = format!(
        "{REJECTED_STAGE_PREFIX}malformed_{:x}_{}_{}",
        digest, stat.st_dev, stat.st_ino
    );
    let current = fstatat(Some(parent_fd), name, AtFlags::AT_SYMLINK_NOFOLLOW)
        .map_err(|_| SkipReason::RejectedRecoveryEntry)?;
    if current.st_dev != stat.st_dev || current.st_ino != stat.st_ino {
        return Err(SkipReason::RejectedRecoveryEntry);
    }
    renameat2(
        Some(parent_fd),
        name,
        Some(base_fd),
        terminal_name.as_str(),
        RenameFlags::RENAME_NOREPLACE,
    )
    .map_err(|error| {
        if error == Errno::EXDEV {
            SkipReason::MountOrDeviceCrossing
        } else if error == Errno::EEXIST {
            SkipReason::StageConflict
        } else {
            SkipReason::StagedDeletionIncomplete
        }
    })?;
    fsync(parent_fd).map_err(|_| SkipReason::StagedDeletionIncomplete)?;
    fsync(base_fd).map_err(|_| SkipReason::StagedDeletionIncomplete)?;
    Ok(())
}

fn recover_one(
    base_fd: RawFd,
    queue_fd: RawFd,
    name: &CStr,
    dry_run: bool,
    base_dev: u64,
) -> TargetReclaimOutcome {
    let Some(text) = name.to_str().ok() else {
        return TargetReclaimOutcome::refused(SkipReason::InvalidRecoveryEntry);
    };
    let Some((_custody, _generation, expected_dev, expected_ino)) = parse_stage_name(text) else {
        return TargetReclaimOutcome::refused(SkipReason::InvalidRecoveryEntry);
    };
    let fd = match open_beneath_dir(queue_fd, OsStr::new(text)) {
        Ok(fd) => fd,
        Err(reason) => return TargetReclaimOutcome::refused(reason),
    };
    let stat = match fstat(fd.as_raw_fd()) {
        Ok(stat) => stat,
        Err(_) => return TargetReclaimOutcome::refused(SkipReason::RejectedRecoveryEntry),
    };
    if stat.st_dev as u64 != base_dev
        || stat.st_dev as u64 != expected_dev
        || stat.st_ino as u64 != expected_ino
    {
        return TargetReclaimOutcome::refused(SkipReason::RejectedRecoveryEntry);
    }
    if dry_run {
        let footprint = match allocated_tree_footprint(fd.as_raw_fd(), base_dev) {
            Ok(footprint) => footprint,
            Err(reason) => return TargetReclaimOutcome::refused(reason),
        };
        let bytes = footprint.bytes;
        let stable = revalidate_target_footprint(fd.as_raw_fd(), base_dev, &footprint);
        current_pass_state().include_target((stat.st_dev as u64, stat.st_ino as u64), stable);
        return TargetReclaimOutcome {
            kind: TargetReclaimKind::Inspected,
            bytes,
            reason: None,
            terminal_rejection: false,
        };
    }
    // Actual recovery is incrementally destructive.  It deliberately skips a
    // complete sizing walk so a fresh fixed budget always reaches unlink work.
    let outcome = finish_staged(
        queue_fd,
        text,
        fd,
        0,
        true,
        base_dev,
        DeletionCredits::default(),
    );
    reject_depth_limited_stage(base_fd, queue_fd, text, base_dev, outcome)
}

fn reject_depth_limited_stage(
    base_fd: RawFd,
    queue_fd: RawFd,
    entry_name: &str,
    base_dev: u64,
    mut outcome: TargetReclaimOutcome,
) -> TargetReclaimOutcome {
    if outcome.reason != Some(SkipReason::DepthBudget) {
        return outcome;
    }
    if !matches!(
        outcome.kind,
        TargetReclaimKind::StagedPending | TargetReclaimKind::RecoveredPending
    ) {
        return outcome;
    }
    match move_stage_to_rejected(base_fd, queue_fd, entry_name, base_dev) {
        Ok(()) => {
            outcome.kind = TargetReclaimKind::Refused;
            outcome.terminal_rejection = true;
        }
        Err(StageRejectionFailure { reason }) => {
            // A failed transition is never terminal.  Before the rename, the
            // authenticated stage remains active.  After the rename, retain
            // every direct-terminal object for diagnosis: there is no atomic
            // compare-inode-and-rename operation that could make a name-based
            // rollback safe against a same-UID replacement.
            outcome.reason = Some(reason);
        }
    }
    outcome
}

struct StageRejectionFailure {
    reason: SkipReason,
}

impl StageRejectionFailure {
    fn new(reason: SkipReason) -> Self {
        Self { reason }
    }
}

fn move_stage_to_rejected(
    base_fd: RawFd,
    queue_fd: RawFd,
    entry_name: &str,
    base_dev: u64,
) -> Result<(), StageRejectionFailure> {
    let rejected = rejected_stage_name(entry_name)
        .ok_or_else(|| StageRejectionFailure::new(SkipReason::RejectedRecoveryEntry))?;
    let terminal_name = rejected.basename.as_str();
    let expected_dev = rejected.expected_dev;
    let expected_ino = rejected.expected_ino;
    let staged_fd =
        authenticate_expected_stage(queue_fd, entry_name, base_dev, expected_dev, expected_ino)
            .map_err(StageRejectionFailure::new)?;
    authenticate_rejection_base(base_fd, base_dev, entry_name)
        .map_err(StageRejectionFailure::new)?;
    authenticate_private_directory(queue_fd, base_dev).map_err(StageRejectionFailure::new)?;

    // Keep the original stage descriptor pinned while reauthenticating its
    // public source name immediately before the durability barriers.
    authenticate_expected_stage(queue_fd, entry_name, base_dev, expected_dev, expected_ino)
        .map_err(StageRejectionFailure::new)?;
    sync_rejection_namespaces(base_fd, queue_fd, entry_name, "before")
        .map_err(StageRejectionFailure::new)?;

    pause_rejection_transition_for_test(entry_name, "before_rename");
    rename_stage_to_rejected_for_test(queue_fd, entry_name, base_fd, terminal_name).map_err(
        |error| {
            StageRejectionFailure::new(if error == Errno::EEXIST {
                SkipReason::StageConflict
            } else if error == Errno::EXDEV {
                SkipReason::MountOrDeviceCrossing
            } else {
                SkipReason::StagedDeletionIncomplete
            })
        },
    )?;

    pause_rejection_transition_for_test(entry_name, "after_rename");
    let transition =
        authenticate_expected_stage(base_fd, terminal_name, base_dev, expected_dev, expected_ino)
            .map(|terminal_fd| {
                // The returned descriptor proves this resolution only.  Once it
                // is dropped, no later operation may move the terminal name.
                drop(terminal_fd);
                pause_rejection_transition_for_test(
                    entry_name,
                    "after_initial_direct_auth_before_failure",
                );
            })
            .and_then(|()| sync_rejection_namespaces(base_fd, queue_fd, entry_name, "after"))
            .and_then(|()| {
                pause_rejection_transition_for_test(entry_name, "before_final_direct_proof");
                authenticate_rejection_base(base_fd, base_dev, entry_name)?;
                // Terminal certification linearizes at this one direct-child openat2
                // beneath the already-pinned base.  fstat authenticates the descriptor
                // returned by that exact name resolution.
                authenticate_expected_stage(
                    base_fd,
                    terminal_name,
                    base_dev,
                    expected_dev,
                    expected_ino,
                )
                .map(|_| ())
            });
    if let Err(reason) = transition {
        // The successful rename has already removed the authenticated stage
        // from the active queue.  Retain the uncertain direct-terminal state;
        // never authenticate a name, discard that inode proof, and later move
        // whatever object happens to occupy the name.
        return Err(StageRejectionFailure::new(reason));
    }
    drop(staged_fd);
    Ok(())
}

struct RejectedStageName {
    basename: String,
    expected_dev: u64,
    expected_ino: u64,
}

fn rejected_stage_name(entry_name: &str) -> Option<RejectedStageName> {
    let (custody_id, generation, expected_dev, expected_ino) = parse_stage_name(entry_name)?;
    let canonical = stage_name(custody_id, generation, expected_dev, expected_ino);
    let basename = format!("{REJECTED_STAGE_PREFIX}{canonical}");
    if basename.len() > nix::libc::NAME_MAX as usize {
        return None;
    }
    Some(RejectedStageName {
        basename,
        expected_dev,
        expected_ino,
    })
}

fn authenticate_expected_stage(
    parent_fd: RawFd,
    entry_name: &str,
    base_dev: u64,
    expected_dev: u64,
    expected_ino: u64,
) -> Result<OwnedFd, SkipReason> {
    let fd = open_beneath_dir(parent_fd, OsStr::new(entry_name))?;
    let stat = fstat(fd.as_raw_fd()).map_err(|_| SkipReason::RejectedRecoveryEntry)?;
    if stat.st_dev as u64 != base_dev
        || stat.st_dev as u64 != expected_dev
        || stat.st_ino as u64 != expected_ino
    {
        return Err(SkipReason::RejectedRecoveryEntry);
    }
    Ok(fd)
}

fn authenticate_private_directory(fd: RawFd, base_dev: u64) -> Result<(), SkipReason> {
    let stat = fstat(fd).map_err(|_| SkipReason::RejectedRecoveryEntry)?;
    let mode = stat.st_mode as u32 & 0o777;
    // SAFETY: `geteuid` has no preconditions.
    if stat.st_dev as u64 != base_dev {
        return Err(SkipReason::MountOrDeviceCrossing);
    }
    if mode != 0o700 || stat.st_uid != unsafe { nix::libc::geteuid() } {
        return Err(SkipReason::RejectedRecoveryEntry);
    }
    Ok(())
}

fn authenticate_rejection_base(
    base_fd: RawFd,
    base_dev: u64,
    entry_name: &str,
) -> Result<(), SkipReason> {
    if let Some(reason) = forced_rejection_base_reason_for_test(entry_name) {
        return Err(reason);
    }
    authenticate_private_directory(base_fd, base_dev)
}

fn rename_stage_to_rejected_for_test(
    queue_fd: RawFd,
    entry_name: &str,
    base_fd: RawFd,
    terminal_name: &str,
) -> Result<(), Errno> {
    if force_rejection_rename_exdev_for_test(entry_name) {
        return Err(Errno::EXDEV);
    }
    renameat2(
        Some(queue_fd),
        entry_name,
        Some(base_fd),
        terminal_name,
        RenameFlags::RENAME_NOREPLACE,
    )
}

fn sync_rejection_namespaces(
    base_fd: RawFd,
    queue_fd: RawFd,
    entry_name: &str,
    phase: &'static str,
) -> Result<(), SkipReason> {
    for (label, fd) in [("queue", queue_fd), ("base", base_fd)] {
        if maybe_fail_rejection_sync_for_test(entry_name, phase, label) || fsync(fd).is_err() {
            return Err(SkipReason::StagedDeletionIncomplete);
        }
    }
    Ok(())
}

fn finish_staged(
    queue_fd: RawFd,
    entry_name: &str,
    staged_fd: OwnedFd,
    bytes: u64,
    recovered: bool,
    base_dev: u64,
    mut credits: DeletionCredits,
) -> TargetReclaimOutcome {
    pause_staged_deletion_for_test(entry_name, recovered);
    let mut observed_bytes = 0_u64;
    let mut observed_inodes = HashSet::new();
    if let Err(reason) = delete_dir_contents(
        staged_fd.as_raw_fd(),
        base_dev,
        entry_name,
        &mut credits,
        &mut observed_bytes,
        &mut observed_inodes,
    ) {
        return TargetReclaimOutcome {
            kind: if recovered {
                TargetReclaimKind::RecoveredPending
            } else {
                TargetReclaimKind::StagedPending
            },
            bytes: if recovered { observed_bytes } else { bytes },
            reason: Some(delete_failure_reason(reason)),
            terminal_rejection: false,
        };
    }
    drop(staged_fd);
    if let Err(reason) = current_pass_state().checkpoint(0) {
        return TargetReclaimOutcome {
            kind: if recovered {
                TargetReclaimKind::RecoveredPending
            } else {
                TargetReclaimKind::StagedPending
            },
            bytes: if recovered { observed_bytes } else { bytes },
            reason: Some(reason),
            terminal_rejection: false,
        };
    }
    if maybe_fail_delete_for_test(entry_name).is_err()
        || unlinkat(Some(queue_fd), entry_name, UnlinkatFlags::RemoveDir).is_err()
    {
        return TargetReclaimOutcome {
            kind: if recovered {
                TargetReclaimKind::RecoveredPending
            } else {
                TargetReclaimKind::StagedPending
            },
            bytes: if recovered { observed_bytes } else { bytes },
            reason: Some(SkipReason::StagedDeletionIncomplete),
            terminal_rejection: false,
        };
    }
    current_pass_state().record_entry_deleted();
    if fsync(queue_fd).is_err() {
        return TargetReclaimOutcome {
            kind: if recovered {
                TargetReclaimKind::RecoveredPending
            } else {
                TargetReclaimKind::StagedPending
            },
            bytes: if recovered { observed_bytes } else { bytes },
            reason: Some(SkipReason::StagedDeletionIncomplete),
            terminal_rejection: false,
        };
    }
    TargetReclaimOutcome {
        kind: if recovered {
            TargetReclaimKind::RecoveredRemoved
        } else {
            TargetReclaimKind::StagedRemoved
        },
        bytes: if recovered { observed_bytes } else { bytes },
        reason: None,
        terminal_rejection: false,
    }
}

fn open_beneath_dir(dirfd: RawFd, path: &OsStr) -> Result<OwnedFd, SkipReason> {
    if force_exdev_for_test(path) {
        return Err(SkipReason::MountOrDeviceCrossing);
    }
    let how = OpenHow::new()
        .flags(OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW)
        .resolve(RESOLVE_FLAGS);
    let raw = openat2(dirfd, path.as_bytes(), how).map_err(map_open_error)?;
    // SAFETY: `openat2` returned a new owned descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

#[cfg(test)]
enum PostAuthenticationHook {
    Barrier {
        reached: std::sync::Arc<std::sync::Barrier>,
        release: std::sync::Arc<std::sync::Barrier>,
    },
    Signal {
        reached: std::sync::mpsc::Sender<()>,
        release: std::sync::mpsc::Receiver<()>,
    },
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
fn post_authentication_hooks() -> &'static Mutex<HashMap<Uuid, PostAuthenticationHook>> {
    static HOOKS: std::sync::OnceLock<Mutex<HashMap<Uuid, PostAuthenticationHook>>> =
        std::sync::OnceLock::new();
    HOOKS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(test)]
pub(crate) fn set_post_authentication_barrier_for_test(
    custody_id: Uuid,
    reached: Arc<std::sync::Barrier>,
    release: Arc<std::sync::Barrier>,
) {
    post_authentication_hooks()
        .lock()
        .expect("post-authentication hook lock")
        .insert(
            custody_id,
            PostAuthenticationHook::Barrier { reached, release },
        );
}

#[cfg(test)]
pub(crate) fn set_post_authentication_signal_for_test(
    custody_id: Uuid,
    reached: std::sync::mpsc::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
) {
    post_authentication_hooks()
        .lock()
        .expect("post-authentication hook lock")
        .insert(
            custody_id,
            PostAuthenticationHook::Signal { reached, release },
        );
}

#[cfg(test)]
fn post_authentication_barrier_for_test(custody_id: Uuid) {
    let hook = post_authentication_hooks()
        .lock()
        .expect("post-authentication hook lock")
        .remove(&custody_id);
    match hook {
        Some(PostAuthenticationHook::Barrier { reached, release }) => {
            reached.wait();
            release.wait();
        }
        Some(PostAuthenticationHook::Signal { reached, release }) => {
            let _ = reached.send(());
            let _ = release.recv_timeout(std::time::Duration::from_secs(2));
        }
        None => {}
    }
}

#[cfg(not(test))]
fn post_authentication_barrier_for_test(_custody_id: Uuid) {}

#[cfg(test)]
fn pause_staged_deletion_for_test(entry_name: &str, _recovered: bool) {
    if let Some((custody_id, _, _, _)) = parse_stage_name(entry_name) {
        pause_target_deletion_signal_for_test(custody_id);
    }
}

#[cfg(test)]
struct TargetDeletionSignal {
    reached: std::sync::mpsc::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
}

#[cfg(test)]
fn target_deletion_signals() -> &'static Mutex<HashMap<Uuid, TargetDeletionSignal>> {
    static SIGNALS: std::sync::OnceLock<Mutex<HashMap<Uuid, TargetDeletionSignal>>> =
        std::sync::OnceLock::new();
    SIGNALS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Install a bounded signal at the exact custody-owned deletion seam.
///
/// Unlike the legacy Barrier hook this never strands a worker when a fixture
/// misses its expected seam: the worker's release wait is finite and failure
/// is reported by the test rather than hanging its runtime.
#[cfg(test)]
pub(crate) fn set_target_deletion_signal_for_test(
    custody_id: Uuid,
    reached: std::sync::mpsc::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
) {
    target_deletion_signals()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(custody_id, TargetDeletionSignal { reached, release });
}

#[cfg(test)]
fn pause_target_deletion_signal_for_test(custody_id: Uuid) {
    let hook = target_deletion_signals()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&custody_id);
    if let Some(hook) = hook {
        hook.reached
            .send(())
            .expect("target deletion signal receiver remains alive");
        hook.release
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("target deletion signal release arrives before test timeout");
    }
}

#[cfg(not(test))]
fn pause_target_deletion_signal_for_test(_custody_id: Uuid) {}

#[cfg(not(test))]
fn pause_staged_deletion_for_test(_entry_name: &str, _recovered: bool) {}

#[cfg(test)]
#[derive(Clone)]
struct RejectionTransitionHook {
    seam: &'static str,
    reached: Arc<std::sync::Barrier>,
    release: Arc<std::sync::Barrier>,
}

#[cfg(test)]
fn rejection_transition_hooks() -> &'static Mutex<HashMap<String, RejectionTransitionHook>> {
    static HOOKS: std::sync::OnceLock<Mutex<HashMap<String, RejectionTransitionHook>>> =
        std::sync::OnceLock::new();
    HOOKS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(test)]
struct RejectionTestReset {
    entry_name: String,
}

#[cfg(test)]
impl Drop for RejectionTestReset {
    fn drop(&mut self) {
        rejection_transition_hooks()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.entry_name);
        rejection_faults()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.entry_name);
    }
}

#[cfg(test)]
fn set_rejection_transition_barrier_for_test(
    entry_name: &str,
    seam: &'static str,
    reached: Arc<std::sync::Barrier>,
    release: Arc<std::sync::Barrier>,
) -> RejectionTestReset {
    rejection_transition_hooks()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            entry_name.to_string(),
            RejectionTransitionHook {
                seam,
                reached,
                release,
            },
        );
    RejectionTestReset {
        entry_name: entry_name.to_string(),
    }
}

#[cfg(test)]
fn pause_rejection_transition_for_test(entry_name: &str, seam: &'static str) {
    let hook = {
        let mut hooks = rejection_transition_hooks()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if hooks.get(entry_name).is_some_and(|hook| hook.seam == seam) {
            hooks.remove(entry_name)
        } else {
            None
        }
    };
    if let Some(hook) = hook {
        hook.reached.wait();
        hook.release.wait();
    }
}

#[cfg(not(test))]
fn pause_rejection_transition_for_test(_entry_name: &str, _seam: &'static str) {}

#[cfg(test)]
#[derive(Clone, Copy)]
enum RejectionFault {
    RenameCrossDevice,
    BaseOwner,
    Sync {
        phase: &'static str,
        directory: &'static str,
    },
}

#[cfg(test)]
fn rejection_faults() -> &'static Mutex<HashMap<String, RejectionFault>> {
    static FAULTS: std::sync::OnceLock<Mutex<HashMap<String, RejectionFault>>> =
        std::sync::OnceLock::new();
    FAULTS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(test)]
fn set_rejection_fault_for_test(entry_name: &str, fault: RejectionFault) -> RejectionTestReset {
    rejection_faults()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(entry_name.to_string(), fault);
    RejectionTestReset {
        entry_name: entry_name.to_string(),
    }
}

#[cfg(test)]
fn forced_rejection_base_reason_for_test(entry_name: &str) -> Option<SkipReason> {
    let mut faults = rejection_faults()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match faults.get(entry_name).copied() {
        Some(RejectionFault::BaseOwner) => {
            faults.remove(entry_name);
            Some(SkipReason::RejectedRecoveryEntry)
        }
        _ => None,
    }
}

#[cfg(not(test))]
fn forced_rejection_base_reason_for_test(_entry_name: &str) -> Option<SkipReason> {
    None
}

#[cfg(test)]
fn force_rejection_rename_exdev_for_test(entry_name: &str) -> bool {
    let mut faults = rejection_faults()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if matches!(
        faults.get(entry_name),
        Some(RejectionFault::RenameCrossDevice)
    ) {
        faults.remove(entry_name);
        true
    } else {
        false
    }
}

#[cfg(not(test))]
fn force_rejection_rename_exdev_for_test(_entry_name: &str) -> bool {
    false
}

#[cfg(test)]
fn maybe_fail_rejection_sync_for_test(
    entry_name: &str,
    phase: &'static str,
    directory: &'static str,
) -> bool {
    let mut faults = rejection_faults()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match faults.get(entry_name).copied() {
        Some(RejectionFault::Sync {
            phase: expected_phase,
            directory: expected_directory,
        }) if phase == expected_phase && directory == expected_directory => {
            faults.remove(entry_name);
            true
        }
        _ => false,
    }
}

#[cfg(not(test))]
fn maybe_fail_rejection_sync_for_test(
    _entry_name: &str,
    _phase: &'static str,
    _directory: &'static str,
) -> bool {
    false
}

#[cfg(test)]
fn forced_exdev_entries() -> &'static Mutex<HashSet<Vec<u8>>> {
    static ENTRIES: std::sync::OnceLock<Mutex<HashSet<Vec<u8>>>> = std::sync::OnceLock::new();
    ENTRIES.get_or_init(|| Mutex::new(HashSet::new()))
}

#[cfg(test)]
struct ExdevTestReset(Vec<u8>);

#[cfg(test)]
impl Drop for ExdevTestReset {
    fn drop(&mut self) {
        forced_exdev_entries()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.0);
    }
}

#[cfg(test)]
fn force_exdev_entry_for_test(path: &OsStr) -> ExdevTestReset {
    let name = path.as_bytes().to_vec();
    forced_exdev_entries()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(name.clone());
    ExdevTestReset(name)
}

#[cfg(test)]
fn force_exdev_for_test(path: &OsStr) -> bool {
    forced_exdev_entries()
        .lock()
        .expect("EXDEV failpoint lock")
        .contains(path.as_bytes())
}

#[cfg(not(test))]
fn force_exdev_for_test(_path: &OsStr) -> bool {
    false
}

#[cfg(test)]
#[derive(Clone, Copy, Default)]
struct BaseFsyncTestState {
    fail_next: bool,
    calls: usize,
    created_calls: usize,
}

#[cfg(test)]
fn base_fsync_test_states() -> &'static Mutex<HashMap<InodeKey, BaseFsyncTestState>> {
    static STATES: std::sync::OnceLock<Mutex<HashMap<InodeKey, BaseFsyncTestState>>> =
        std::sync::OnceLock::new();
    STATES.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(test)]
fn base_fsync_test_key(path: &Path) -> InodeKey {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::metadata(path).expect("base fsync test path");
    (metadata.dev(), metadata.ino())
}

#[cfg(test)]
fn install_base_fsync_fault_for_test(path: &Path) {
    base_fsync_test_states()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            base_fsync_test_key(path),
            BaseFsyncTestState {
                fail_next: true,
                ..BaseFsyncTestState::default()
            },
        );
}

#[cfg(test)]
fn base_fsync_counts_for_test(path: &Path) -> (usize, usize) {
    let state = base_fsync_test_states()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&base_fsync_test_key(path))
        .copied()
        .unwrap_or_default();
    (state.calls, state.created_calls)
}

#[cfg(test)]
fn maybe_fail_base_fsync_for_test(base_fd: RawFd, created: bool) -> Result<(), ()> {
    let stat = fstat(base_fd).map_err(|_| ())?;
    let mut states = base_fsync_test_states()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(state) = states.get_mut(&(stat.st_dev as u64, stat.st_ino as u64)) else {
        return Ok(());
    };
    state.calls += 1;
    if created {
        state.created_calls += 1;
    }
    if std::mem::take(&mut state.fail_next) {
        Err(())
    } else {
        Ok(())
    }
}

#[cfg(not(test))]
fn maybe_fail_base_fsync_for_test(_base_fd: RawFd, _created: bool) -> Result<(), ()> {
    Ok(())
}

#[cfg(test)]
fn cursor_fsync_faults() -> &'static Mutex<HashSet<InodeKey>> {
    static FAULTS: std::sync::OnceLock<Mutex<HashSet<InodeKey>>> = std::sync::OnceLock::new();
    FAULTS.get_or_init(|| Mutex::new(HashSet::new()))
}

#[cfg(test)]
fn install_cursor_fsync_fault_for_test(queue: &Path) {
    cursor_fsync_faults()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(base_fsync_test_key(queue));
}

#[cfg(test)]
fn maybe_fail_cursor_fsync_for_test(queue_fd: RawFd) -> Result<(), ()> {
    let stat = fstat(queue_fd).map_err(|_| ())?;
    if cursor_fsync_faults()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&(stat.st_dev as u64, stat.st_ino as u64))
    {
        Err(())
    } else {
        Ok(())
    }
}

#[cfg(not(test))]
fn maybe_fail_cursor_fsync_for_test(_queue_fd: RawFd) -> Result<(), ()> {
    Ok(())
}

fn map_open_error(error: Errno) -> SkipReason {
    match error {
        Errno::ENOSYS => SkipReason::Openat2Unavailable,
        Errno::EXDEV => SkipReason::MountOrDeviceCrossing,
        _ => SkipReason::GitOrRootIdentityRefusal,
    }
}

fn allocated_tree_footprint(fd: RawFd, base_dev: u64) -> Result<TargetFootprint, SkipReason> {
    let pass = current_pass_state();
    let root = fstat(fd).map_err(|_| SkipReason::UnreadableEntry)?;
    if root.st_dev as u64 != base_dev {
        return Err(SkipReason::MountOrDeviceCrossing);
    }
    pass.charge_filesystem_entry(0)?;
    pass.charge_allocated_bytes(&root)?;
    let mut footprint = TargetFootprint::default();
    footprint.observe(&root);
    allocated_tree_children_footprint(fd, base_dev, 1, &pass, &mut footprint)?;
    Ok(footprint)
}

/// Repeat the descendant observations at the capacity-estimate boundary
/// without charging the same entries to the production ceiling twice.  The
/// second walk is locally capped by the first walk's exact entry/byte charges
/// and still shares the pass duration/depth checkpoints.  Growth or any walk
/// failure therefore discards the estimate instead of expanding work.
fn revalidate_target_footprint(
    fd: RawFd,
    base_dev: u64,
    original: &TargetFootprint,
) -> TargetFootprint {
    let pass = current_pass_state();
    let mut entries_left = original.scanned_entries;
    let mut bytes_left = original.scanned_allocated_bytes;
    let current =
        revalidation_footprint_at_depth(fd, base_dev, 0, &pass, &mut entries_left, &mut bytes_left);
    current
        .map(|current| original.stable_against(&current))
        .unwrap_or_default()
}

fn revalidation_footprint_at_depth(
    fd: RawFd,
    base_dev: u64,
    depth: u32,
    pass: &ReclaimPassState,
    entries_left: &mut u64,
    bytes_left: &mut u64,
) -> Result<TargetFootprint, SkipReason> {
    pass.checkpoint(depth)?;
    let root = fstat(fd).map_err(|_| SkipReason::UnreadableEntry)?;
    if root.st_dev as u64 != base_dev {
        return Err(SkipReason::MountOrDeviceCrossing);
    }
    charge_revalidation_observation(&root, entries_left, bytes_left)?;
    let mut footprint = TargetFootprint::default();
    footprint.observe(&root);

    let dup_fd = dup(fd).map_err(|_| SkipReason::UnreadableEntry)?;
    let mut dir = Dir::from_fd(dup_fd).map_err(|_| SkipReason::UnreadableEntry)?;
    let mut names = Vec::new();
    for entry in dir.iter() {
        let entry = entry.map_err(|_| SkipReason::UnreadableEntry)?;
        let name = entry.file_name();
        if name.to_bytes() != b"." && name.to_bytes() != b".." {
            if names.len() as u64 >= *entries_left {
                return Err(SkipReason::FilesystemEntryBudget);
            }
            names.push(name.to_owned());
        }
    }
    names.sort_by(|a, b| a.to_bytes().cmp(b.to_bytes()));
    for name in names {
        pass.checkpoint(depth)?;
        let stat = fstatat(Some(fd), name.as_c_str(), AtFlags::AT_SYMLINK_NOFOLLOW)
            .map_err(|_| SkipReason::UnreadableEntry)?;
        if stat.st_dev as u64 != base_dev {
            return Err(SkipReason::MountOrDeviceCrossing);
        }
        if SFlag::from_bits_truncate(stat.st_mode).contains(SFlag::S_IFDIR) {
            let child = open_nested_dir(fd, OsStr::from_bytes(name.to_bytes()))?;
            let child_footprint = revalidation_footprint_at_depth(
                child.as_raw_fd(),
                base_dev,
                depth.saturating_add(1),
                pass,
                entries_left,
                bytes_left,
            )?;
            merge_footprints(&mut footprint, child_footprint);
        } else {
            charge_revalidation_observation(&stat, entries_left, bytes_left)?;
            footprint.observe(&stat);
        }
    }
    Ok(footprint)
}

fn charge_revalidation_observation(
    stat: &nix::libc::stat,
    entries_left: &mut u64,
    bytes_left: &mut u64,
) -> Result<(), SkipReason> {
    if *entries_left == 0 {
        return Err(SkipReason::FilesystemEntryBudget);
    }
    *entries_left -= 1;
    let bytes = allocated_stat_bytes(stat);
    if bytes > *bytes_left {
        return Err(SkipReason::ByteBudget);
    }
    *bytes_left -= bytes;
    Ok(())
}

fn merge_footprints(target: &mut TargetFootprint, child: TargetFootprint) {
    target.scanned_entries = target.scanned_entries.saturating_add(child.scanned_entries);
    target.scanned_allocated_bytes = target
        .scanned_allocated_bytes
        .saturating_add(child.scanned_allocated_bytes);
    for (key, observation) in child.entries {
        let entry = target.entries.entry(key).or_insert_with(|| {
            target.bytes = target.bytes.saturating_add(observation.bytes);
            InodeObservation {
                bytes: observation.bytes,
                link_count: observation.link_count,
                observed_links: 0,
                is_dir: observation.is_dir,
            }
        });
        entry.bytes = entry.bytes.max(observation.bytes);
        entry.link_count = entry.link_count.max(observation.link_count);
        entry.observed_links = entry
            .observed_links
            .saturating_add(observation.observed_links);
        entry.is_dir |= observation.is_dir;
    }
}

fn allocated_tree_children_footprint(
    fd: RawFd,
    base_dev: u64,
    depth: u32,
    pass: &ReclaimPassState,
    footprint: &mut TargetFootprint,
) -> Result<(), SkipReason> {
    pass.checkpoint(depth)?;
    let dup_fd = dup(fd).map_err(|_| SkipReason::UnreadableEntry)?;
    let mut dir = Dir::from_fd(dup_fd).map_err(|_| SkipReason::UnreadableEntry)?;
    let mut names = Vec::new();
    for entry in dir.iter() {
        let entry = entry.map_err(|_| SkipReason::UnreadableEntry)?;
        let name = entry.file_name();
        if name.to_bytes() != b"." && name.to_bytes() != b".." {
            // Charge before retaining the name so a hostile directory cannot
            // grow the sort vector beyond the pass-wide entry ceiling.
            pass.charge_filesystem_entry(depth)?;
            names.push(name.to_owned());
        }
    }
    names.sort_by(|a, b| a.to_bytes().cmp(b.to_bytes()));
    for name in names {
        let stat = fstatat(Some(fd), name.as_c_str(), AtFlags::AT_SYMLINK_NOFOLLOW)
            .map_err(|_| SkipReason::UnreadableEntry)?;
        if stat.st_dev as u64 != base_dev {
            return Err(SkipReason::MountOrDeviceCrossing);
        }
        pass.charge_allocated_bytes(&stat)?;
        footprint.observe(&stat);
        if SFlag::from_bits_truncate(stat.st_mode).contains(SFlag::S_IFDIR) {
            let child = open_nested_dir(fd, OsStr::from_bytes(name.to_bytes()))?;
            allocated_tree_children_footprint(
                child.as_raw_fd(),
                base_dev,
                depth.saturating_add(1),
                pass,
                footprint,
            )?;
        }
    }
    Ok(())
}

fn open_nested_dir(dirfd: RawFd, path: &OsStr) -> Result<OwnedFd, SkipReason> {
    open_beneath_dir(dirfd, path).map_err(|reason| match reason {
        SkipReason::Openat2Unavailable | SkipReason::MountOrDeviceCrossing => reason,
        _ => SkipReason::UnreadableEntry,
    })
}

fn allocated_stat_bytes(stat: &nix::libc::stat) -> u64 {
    (stat.st_blocks.max(0) as u64).saturating_mul(512)
}

fn delete_dir_contents(
    fd: RawFd,
    base_dev: u64,
    entry_name: &str,
    credits: &mut DeletionCredits,
    observed_bytes: &mut u64,
    observed_inodes: &mut HashSet<InodeKey>,
) -> Result<(), SkipReason> {
    let pass = current_pass_state();
    delete_dir_contents_at_depth(
        fd,
        base_dev,
        entry_name,
        0,
        &pass,
        credits,
        observed_bytes,
        observed_inodes,
        true,
    )
}

fn delete_dir_contents_at_depth(
    fd: RawFd,
    base_dev: u64,
    entry_name: &str,
    depth: u32,
    pass: &ReclaimPassState,
    credits: &mut DeletionCredits,
    observed_bytes: &mut u64,
    observed_inodes: &mut HashSet<InodeKey>,
    charge_root: bool,
) -> Result<(), SkipReason> {
    pass.checkpoint(depth)?;
    let root = fstat(fd).map_err(|_| SkipReason::StagedDeletionIncomplete)?;
    if root.st_dev as u64 != base_dev {
        return Err(SkipReason::MountOrDeviceCrossing);
    }
    if charge_root {
        charge_deletion_observation(
            &root,
            depth,
            pass,
            credits,
            observed_bytes,
            observed_inodes,
            false,
        )?;
    }
    let dup_fd = dup(fd).map_err(|_| SkipReason::StagedDeletionIncomplete)?;
    let mut dir = Dir::from_fd(dup_fd).map_err(|_| SkipReason::StagedDeletionIncomplete)?;
    for entry in dir.iter() {
        let entry = entry.map_err(|_| SkipReason::StagedDeletionIncomplete)?;
        let name = entry.file_name();
        if name.to_bytes() == b"." || name.to_bytes() == b".." {
            continue;
        }
        let stat = fstatat(Some(fd), name, AtFlags::AT_SYMLINK_NOFOLLOW)
            .map_err(|_| SkipReason::StagedDeletionIncomplete)?;
        if stat.st_dev as u64 != base_dev {
            return Err(SkipReason::MountOrDeviceCrossing);
        }
        let is_dir = SFlag::from_bits_truncate(stat.st_mode).contains(SFlag::S_IFDIR);
        charge_deletion_observation(
            &stat,
            depth.saturating_add(1),
            pass,
            credits,
            observed_bytes,
            observed_inodes,
            !is_dir,
        )?;
        if is_dir {
            let child = open_nested_dir(fd, OsStr::from_bytes(name.to_bytes()))
                .map_err(|_| SkipReason::StagedDeletionIncomplete)?;
            delete_dir_contents_at_depth(
                child.as_raw_fd(),
                base_dev,
                entry_name,
                depth.saturating_add(1),
                pass,
                credits,
                observed_bytes,
                observed_inodes,
                false,
            )?;
            drop(child);
            maybe_fail_delete_for_test(entry_name)?;
            unlinkat(Some(fd), name, UnlinkatFlags::RemoveDir)
                .map_err(|_| SkipReason::StagedDeletionIncomplete)?;
            pass.record_entry_deleted();
        } else {
            maybe_fail_delete_for_test(entry_name)?;
            unlinkat(Some(fd), name, UnlinkatFlags::NoRemoveDir)
                .map_err(|_| SkipReason::StagedDeletionIncomplete)?;
            pass.record_entry_deleted();
        }
    }
    Ok(())
}

fn charge_deletion_observation(
    stat: &nix::libc::stat,
    depth: u32,
    pass: &ReclaimPassState,
    credits: &mut DeletionCredits,
    observed_bytes: &mut u64,
    observed_inodes: &mut HashSet<InodeKey>,
    _leaf_unlink: bool,
) -> Result<(), SkipReason> {
    pass.checkpoint(depth)?;
    if credits.entries > 0 {
        credits.entries -= 1;
    } else {
        pass.charge_filesystem_entry(depth)?;
    }
    let bytes = allocated_stat_bytes(stat);
    let credited = bytes.min(credits.allocated_bytes);
    credits.allocated_bytes -= credited;
    let additional = bytes.saturating_sub(credited);
    // Directory allocation and leaf allocation are observations, not work
    // proportional to bytes. Charging either before a child unlink can strand
    // a large directory forever at the byte ceiling.
    let bounded_observation = credited.saturating_add(pass.observe_allocated_bytes(additional));
    if observed_inodes.insert((stat.st_dev as u64, stat.st_ino as u64)) {
        *observed_bytes = observed_bytes.saturating_add(bounded_observation);
    }
    Ok(())
}

fn delete_failure_reason(reason: SkipReason) -> SkipReason {
    match reason {
        SkipReason::RecoveryEntryBudget
        | SkipReason::FilesystemEntryBudget
        | SkipReason::ByteBudget
        | SkipReason::DurationBudget
        | SkipReason::DepthBudget
        | SkipReason::MountOrDeviceCrossing => reason,
        _ => SkipReason::StagedDeletionIncomplete,
    }
}

fn stage_name(custody_id: Uuid, generation: u64, dev: u64, ino: u64) -> String {
    format!("v1_{custody_id}_{generation}_{dev}_{ino}")
}

fn rotated_stage_name(entry_name: &str, destination_bucket: u8) -> Option<String> {
    let (custody_id, generation, dev, ino) = parse_stage_name(entry_name)?;
    for nonce in 0_u16..=u16::MAX {
        let candidate =
            format!("{ROTATED_STAGE_PREFIX}_{nonce:04x}_{custody_id}_{generation}_{dev}_{ino}");
        let bucket = stage_bucket_index(&candidate);
        if bucket == destination_bucket {
            return Some(candidate);
        }
    }
    None
}

fn parse_stage_name(name: &str) -> Option<(Uuid, u64, u64, u64)> {
    let mut parts = name.split('_');
    match parts.next()? {
        "v1" => {}
        ROTATED_STAGE_PREFIX => {
            let nonce = parts.next()?;
            if nonce.len() != 4 || u16::from_str_radix(nonce, 16).is_err() {
                return None;
            }
        }
        _ => return None,
    }
    let custody = Uuid::parse_str(parts.next()?).ok()?;
    let generation = parts.next()?.parse().ok()?;
    let dev = parts.next()?.parse().ok()?;
    let ino = parts.next()?.parse().ok()?;
    (parts.next().is_none()).then_some((custody, generation, dev, ino))
}

#[cfg(test)]
fn delete_faults() -> &'static Mutex<HashMap<(Uuid, u64), isize>> {
    static FAULTS: std::sync::OnceLock<Mutex<HashMap<(Uuid, u64), isize>>> =
        std::sync::OnceLock::new();
    FAULTS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum RegisteredDurabilitySeam {
    BucketPublication,
    SlotPublication,
    RootRename,
    SlotRename,
    BucketRename,
    QueueRename,
    RecoveryPublication,
}

#[cfg(test)]
fn registered_durability_faults() -> &'static Mutex<HashSet<(Uuid, u64, RegisteredDurabilitySeam)>>
{
    static FAULTS: std::sync::OnceLock<Mutex<HashSet<(Uuid, u64, RegisteredDurabilitySeam)>>> =
        std::sync::OnceLock::new();
    FAULTS.get_or_init(|| Mutex::new(HashSet::new()))
}

#[cfg(test)]
fn fail_registered_durability_for_test(
    intent: &RegisteredTargetIntent,
    seam: RegisteredDurabilitySeam,
) {
    registered_durability_faults()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert((intent.custody_id, intent.generation, seam));
}

#[cfg(test)]
pub(crate) fn fail_registered_publication_replay_for_test(intent: &RegisteredTargetIntent) {
    fail_registered_durability_for_test(intent, RegisteredDurabilitySeam::RecoveryPublication);
}

#[cfg(test)]
pub(crate) fn fail_registered_root_rename_for_test(intent: &RegisteredTargetIntent) {
    fail_registered_durability_for_test(intent, RegisteredDurabilitySeam::RootRename);
}

#[cfg(test)]
fn maybe_fail_registered_durability_for_test(
    intent: &RegisteredTargetIntent,
    seam: RegisteredDurabilitySeam,
) -> Result<(), SkipReason> {
    let expected = (intent.custody_id, intent.generation, seam);
    if registered_durability_faults()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&expected)
    {
        Err(SkipReason::StagedDeletionIncomplete)
    } else {
        Ok(())
    }
}

#[cfg(not(test))]
fn maybe_fail_registered_durability_for_test(
    _intent: &RegisteredTargetIntent,
    _seam: RegisteredDurabilitySeam,
) -> Result<(), SkipReason> {
    Ok(())
}

#[cfg(test)]
thread_local! {
    static REGISTERED_DELETE_IDENTITY: std::cell::Cell<Option<(Uuid, u64)>> = const {
        std::cell::Cell::new(None)
    };
}

#[cfg(test)]
fn with_registered_delete_identity_for_test<T>(
    identity: (Uuid, u64),
    operation: impl FnOnce() -> T,
) -> T {
    REGISTERED_DELETE_IDENTITY.with(|active| {
        let previous = active.replace(Some(identity));
        let result = operation();
        active.set(previous);
        result
    })
}

#[cfg(not(test))]
fn with_registered_delete_identity_for_test<T>(
    _identity: (Uuid, u64),
    operation: impl FnOnce() -> T,
) -> T {
    operation()
}

#[cfg(test)]
fn elapsed_after_delete_for_test() -> &'static Mutex<HashMap<(Uuid, u64), Duration>> {
    static ELAPSED: std::sync::OnceLock<Mutex<HashMap<(Uuid, u64), Duration>>> =
        std::sync::OnceLock::new();
    ELAPSED.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(test)]
pub(crate) fn fail_delete_after_for_test(custody_id: Uuid, generation: u64, after: isize) {
    delete_faults()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert((custody_id, generation), after);
}

#[cfg(test)]
fn set_elapsed_after_delete_for_test(custody_id: Uuid, generation: u64, elapsed: Duration) {
    elapsed_after_delete_for_test()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert((custody_id, generation), elapsed);
}

#[cfg(test)]
fn maybe_fail_delete_for_test(entry_name: &str) -> Result<(), SkipReason> {
    let identity = parse_stage_name(entry_name)
        .map(|(custody_id, generation, _, _)| (custody_id, generation))
        .or_else(|| REGISTERED_DELETE_IDENTITY.with(std::cell::Cell::get));
    let Some((custody_id, generation)) = identity else {
        return Ok(());
    };
    if let Some(elapsed) = elapsed_after_delete_for_test()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&(custody_id, generation))
    {
        current_pass_state().set_elapsed_for_test(elapsed);
    }
    let mut faults = delete_faults()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(value) = faults.get_mut(&(custody_id, generation)) else {
        return Ok(());
    };
    if *value == 0 {
        faults.remove(&(custody_id, generation));
        return Err(SkipReason::StagedDeletionIncomplete);
    }
    if *value > 0 {
        *value -= 1;
    }
    Ok(())
}

#[cfg(not(test))]
fn maybe_fail_delete_for_test(_entry_name: &str) -> Result<(), SkipReason> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
    use std::sync::{Arc, Barrier};

    fn registered_fixture(
        base: &Path,
        generation: u64,
    ) -> (PathBuf, PinnedSandboxRoot, RegisteredTargetIntent) {
        let allocation_id = Uuid::new_v4();
        let custody_id = Uuid::new_v4();
        let root = base.join(allocation_id.to_string());
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(root.join("target/cache"), b"registered").unwrap();
        let pinned = PinnedSandboxRoot::open(base, &root, allocation_id).unwrap();
        let identity = pinned.registered_target_identity().unwrap();
        let intent = RegisteredTargetIntent {
            custody_id,
            generation,
            allocation_id,
            bucket: registered_bucket_index(custody_id, generation),
            slot_name: format!("v3_{custody_id}_{generation}"),
            expected_device: identity.device,
            expected_inode: identity.inode,
        };
        (root, pinned, intent)
    }

    #[test]
    fn registered_stage_durability_seams_are_recoverable_and_delete_fault_retries() {
        for (ordinal, seam) in [
            RegisteredDurabilitySeam::BucketPublication,
            RegisteredDurabilitySeam::SlotPublication,
            RegisteredDurabilitySeam::RootRename,
            RegisteredDurabilitySeam::SlotRename,
            RegisteredDurabilitySeam::BucketRename,
            RegisteredDurabilitySeam::QueueRename,
        ]
        .into_iter()
        .enumerate()
        {
            let temp = tempdir().unwrap();
            let base = temp.path().join("base");
            std::fs::create_dir(&base).unwrap();
            let (_root, pinned, intent) = registered_fixture(&base, ordinal as u64 + 1);
            fail_registered_durability_for_test(&intent, seam);
            let first = pinned.stage_registered_target(&intent);
            assert_eq!(first.reason, Some(SkipReason::StagedDeletionIncomplete));
            let probe = probe_registered_target(&base, &intent);
            if probe.source == RegisteredNamespaceState::Expected {
                assert_eq!(probe.destination, RegisteredNamespaceState::Absent);
                let retried = pinned.stage_registered_target(&intent);
                assert_eq!(retried.kind, TargetReclaimKind::StagedPending);
                assert_eq!(retried.reason, None);
            } else {
                assert_eq!(probe.source, RegisteredNamespaceState::Absent);
                assert_eq!(probe.destination, RegisteredNamespaceState::Expected);
            }
            fail_delete_after_for_test(intent.custody_id, intent.generation, 0);
            assert_eq!(
                delete_registered_target(&base, &intent, false).kind,
                TargetReclaimKind::RecoveredPending
            );
            assert_eq!(
                delete_registered_target(&base, &intent, false).kind,
                TargetReclaimKind::RecoveredRemoved
            );
            assert!(sync_registered_target_absence(&base, &intent).unwrap());
        }
    }

    #[test]
    fn registered_dry_run_raced_absence_never_syncs_parent_namespaces() {
        let temp = tempdir().unwrap();
        let base = temp.path().join("base");
        std::fs::create_dir(&base).unwrap();
        let (_root, pinned, intent) = registered_fixture(&base, 63);
        assert_eq!(pinned.stage_registered_target(&intent).reason, None);
        let slot = base
            .join(QUEUE_NAME)
            .join(bucket_name(intent.bucket))
            .join(&intent.slot_name);
        std::fs::remove_dir_all(&slot).unwrap();
        fail_registered_publication_replay_for_test(&intent);

        assert_eq!(
            delete_registered_target(&base, &intent, true).reason,
            Some(SkipReason::StagedDeletionIncomplete)
        );
        // The armed seam remains for actual recovery. If preview had called the
        // absence synchronizer, this call would instead complete its proof.
        assert_eq!(
            delete_registered_target(&base, &intent, false).reason,
            Some(SkipReason::StagedDeletionIncomplete)
        );
    }

    #[test]
    fn registered_recovery_preserves_new_source_and_refuses_replaced_payload() {
        let temp = tempdir().unwrap();
        let base = temp.path().join("base");
        std::fs::create_dir(&base).unwrap();
        let (root, pinned, intent) = registered_fixture(&base, 31);
        let staged = pinned.stage_registered_target(&intent);
        assert_eq!(staged.reason, None);
        std::fs::create_dir(root.join("target")).unwrap();
        std::fs::write(root.join("target/new-owner"), b"preserve").unwrap();
        assert_eq!(
            probe_registered_target(&base, &intent),
            RegisteredTargetProbe {
                source: RegisteredNamespaceState::Different,
                destination: RegisteredNamespaceState::Expected,
            }
        );
        assert_eq!(
            delete_registered_target(&base, &intent, false).kind,
            TargetReclaimKind::RecoveredRemoved
        );
        assert_eq!(
            std::fs::read(root.join("target/new-owner")).unwrap(),
            b"preserve"
        );

        let (_root, pinned, replacement) = registered_fixture(&base, 32);
        assert_eq!(pinned.stage_registered_target(&replacement).reason, None);
        let slot = base
            .join(QUEUE_NAME)
            .join(bucket_name(replacement.bucket))
            .join(&replacement.slot_name);
        std::fs::rename(slot.join("payload"), slot.join("original-payload")).unwrap();
        std::fs::create_dir(slot.join("payload")).unwrap();
        std::fs::write(slot.join("payload/replacement"), b"do-not-delete").unwrap();
        assert_eq!(
            probe_registered_target(&base, &replacement).destination,
            RegisteredNamespaceState::Different
        );
        assert_eq!(
            delete_registered_target(&base, &replacement, false).kind,
            TargetReclaimKind::Refused
        );
        assert_eq!(
            std::fs::read(slot.join("payload/replacement")).unwrap(),
            b"do-not-delete"
        );
    }

    #[test]
    fn legacy_recovery_never_mutates_registered_v3_namespace() {
        let temp = tempdir().unwrap();
        let base = temp.path().join("base");
        std::fs::create_dir(&base).unwrap();
        let (_root, pinned, intent) = registered_fixture(&base, 41);
        assert_eq!(pinned.stage_registered_target(&intent).reason, None);
        let run = recover_staged_targets_run_with_state(&base, false, &ReclaimPassState::new());
        assert_eq!(
            probe_registered_target(&base, &intent).destination,
            RegisteredNamespaceState::Expected
        );
        assert_eq!(run.evidence.nonprogress_count, 1);
    }
    use tempfile::tempdir;

    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf, Uuid) {
        let temp = tempdir().unwrap();
        let base = temp.path().join("base");
        std::fs::create_dir(&base).unwrap();
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700)).unwrap();
        let id = Uuid::new_v4();
        let root = base.join(id.to_string());
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(root.join("target")).unwrap();
        (temp, base, root, id)
    }

    fn roomy_limits() -> ReclaimWorkLimits {
        ReclaimWorkLimits {
            recovery_entries: 100,
            filesystem_entries: 10_000,
            allocated_bytes: 1024 * 1024 * 1024,
            duration: Duration::from_secs(60),
            depth: 32,
        }
    }

    struct StagedDepthFixture {
        _temp: tempfile::TempDir,
        base: PathBuf,
        queue: PathBuf,
        stage: PathBuf,
        entry_name: String,
        dev: u64,
        ino: u64,
    }

    fn staged_depth_fixture() -> StagedDepthFixture {
        let temp = tempdir().unwrap();
        let base = temp.path().join("base");
        let queue = base.join(QUEUE_NAME);
        std::fs::create_dir_all(&queue).unwrap();
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&queue, std::fs::Permissions::from_mode(0o700)).unwrap();
        let provisional = queue.join(format!("provisional-{}", Uuid::new_v4()));
        std::fs::create_dir_all(provisional.join("one/two/three")).unwrap();
        std::fs::write(provisional.join("one/two/three/cache"), b"retain").unwrap();
        let metadata = std::fs::metadata(&provisional).unwrap();
        let dev = metadata.dev();
        let ino = metadata.ino();
        let entry_name = stage_name(Uuid::new_v4(), 1, dev, ino);
        let bucket = queue.join(bucket_name(stage_bucket_index(&entry_name)));
        std::fs::create_dir(&bucket).unwrap();
        std::fs::set_permissions(&bucket, std::fs::Permissions::from_mode(0o700)).unwrap();
        let stage = bucket.join(&entry_name);
        std::fs::rename(provisional, &stage).unwrap();
        StagedDepthFixture {
            _temp: temp,
            base,
            queue,
            stage,
            entry_name,
            dev,
            ino,
        }
    }

    fn recover_depth_limited(base: &Path) -> TargetReclaimOutcome {
        let mut limits = roomy_limits();
        limits.depth = 1;
        let pass = ReclaimPassState::for_test(limits);
        let mut outcomes = recover_staged_targets_with_state(base, false, &pass);
        assert_eq!(outcomes.len(), 1, "unexpected outcomes: {outcomes:?}");
        outcomes.remove(0)
    }

    fn active_stage_paths(base: &Path) -> Vec<PathBuf> {
        let queue = base.join(QUEUE_NAME);
        let Ok(entries) = std::fs::read_dir(&queue) else {
            return Vec::new();
        };
        let mut paths = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name
                .to_str()
                .is_some_and(|name| parse_stage_name(name).is_some())
            {
                paths.push(entry.path());
                continue;
            }
            if parse_bucket_name(name.as_encoded_bytes()).is_none() {
                continue;
            }
            let Ok(bucket_entries) = std::fs::read_dir(entry.path()) else {
                continue;
            };
            for bucket_entry in bucket_entries.flatten() {
                if bucket_entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| parse_stage_name(name).is_some())
                {
                    paths.push(bucket_entry.path());
                }
            }
        }
        paths.sort();
        paths
    }

    fn only_active_stage(base: &Path) -> PathBuf {
        let mut paths = active_stage_paths(base);
        assert_eq!(paths.len(), 1, "expected one active stage: {paths:?}");
        paths.pop().unwrap()
    }

    fn recovery_cursor_marker(base: &Path) -> String {
        let cursor = base.join(QUEUE_NAME).join(CURSOR_DIR);
        let mut markers = std::fs::read_dir(cursor)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().into_string().unwrap())
            .filter(|name| parse_cursor_marker(name.as_bytes()).is_some())
            .collect::<Vec<_>>();
        assert_eq!(
            markers.len(),
            1,
            "expected one recovery cursor: {markers:?}"
        );
        markers.pop().unwrap()
    }

    fn assert_stage_identity(path: &Path, dev: u64, ino: u64) {
        let metadata = std::fs::metadata(path).unwrap();
        assert_eq!((metadata.dev(), metadata.ino()), (dev, ino));
    }

    fn assert_pending_rejection(outcome: TargetReclaimOutcome, reason: SkipReason) {
        assert_eq!(outcome.kind, TargetReclaimKind::RecoveredPending);
        assert_eq!(outcome.reason, Some(reason));
        assert!(!outcome.terminal_rejection);
    }

    fn assert_terminal_depth_rejection(outcome: TargetReclaimOutcome) {
        assert_eq!(outcome.kind, TargetReclaimKind::Refused);
        assert_eq!(outcome.reason, Some(SkipReason::DepthBudget));
        assert!(outcome.terminal_rejection);
    }

    fn assert_hostile_direct_terminal_replacement_is_retained(seam: &'static str) {
        let fixture = staged_depth_fixture();
        let terminal = fixture
            .base
            .join(rejected_stage_name(&fixture.entry_name).unwrap().basename);
        let held = fixture
            .base
            .join(format!("{REJECTED_STAGE_PREFIX}held_{}", Uuid::new_v4()));
        let reached = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let _reset = set_rejection_transition_barrier_for_test(
            &fixture.entry_name,
            seam,
            Arc::clone(&reached),
            Arc::clone(&release),
        );
        let terminal_for_attacker = terminal.clone();
        let held_for_attacker = held.clone();
        let attacker = std::thread::spawn(move || {
            reached.wait();
            std::fs::rename(&terminal_for_attacker, &held_for_attacker).unwrap();
            std::fs::create_dir(&terminal_for_attacker).unwrap();
            std::fs::write(
                terminal_for_attacker.join("replacement-sentinel"),
                b"replacement",
            )
            .unwrap();
            release.wait();
        });

        let outcome = recover_depth_limited(&fixture.base);
        attacker.join().unwrap();
        assert_pending_rejection(outcome, SkipReason::RejectedRecoveryEntry);
        assert_eq!(active_stage_paths(&fixture.base).len(), 0);
        assert!(!fixture.stage.exists());
        assert_stage_identity(&held, fixture.dev, fixture.ino);
        assert!(held.join("one/two/three/cache").exists());
        let replacement = std::fs::metadata(&terminal).unwrap();
        assert_ne!(
            (replacement.dev(), replacement.ino()),
            (fixture.dev, fixture.ino)
        );
        assert_eq!(
            std::fs::read(terminal.join("replacement-sentinel")).unwrap(),
            b"replacement"
        );

        assert_stage_identity(&held, fixture.dev, fixture.ino);
        assert!(held.join("one/two/three/cache").exists());
        assert_eq!(
            std::fs::read(terminal.join("replacement-sentinel")).unwrap(),
            b"replacement"
        );
    }

    fn available_bytes(path: &Path) -> u64 {
        let stats = nix::sys::statvfs::statvfs(path).unwrap();
        (stats.blocks_available() as u64).saturating_mul(stats.fragment_size() as u64)
    }

    #[test]
    fn target_reclaim_sizes_allocated_blocks_without_following_nested_symlinks() {
        let (temp, base, root, id) = fixture();
        let outside = temp.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("sentinel"), vec![1_u8; 4096]).unwrap();
        symlink(&outside, root.join("target/external")).unwrap();
        let sparse = std::fs::File::create(root.join("target/sparse")).unwrap();
        sparse.set_len(128 * 1024 * 1024).unwrap();
        std::fs::write(root.join("target/data"), vec![2_u8; 4096]).unwrap();
        std::fs::hard_link(root.join("target/data"), root.join("target/data-link")).unwrap();

        let pinned = PinnedSandboxRoot::open(&base, &root, id).unwrap();
        let outcome = pinned.inspect_target();
        assert_eq!(outcome.kind, TargetReclaimKind::Inspected);
        assert!(outcome.bytes < 128 * 1024 * 1024);
        assert!(outside.join("sentinel").exists());
    }

    #[test]
    fn target_reclaim_ancestor_swap_deletes_only_pinned_original_target() {
        let (temp, base, root, id) = fixture();
        std::fs::write(root.join("source-survives"), b"source").unwrap();
        std::fs::write(root.join("target/cache"), b"cache").unwrap();
        let victim = temp.path().join("victim");
        std::fs::create_dir(&victim).unwrap();
        std::fs::create_dir(victim.join("target")).unwrap();
        std::fs::write(victim.join("target/outside-sentinel"), b"outside").unwrap();

        let pinned = PinnedSandboxRoot::open(&base, &root, id).unwrap();
        let relocated = temp.path().join("relocated");
        let reached = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        set_post_authentication_barrier_for_test(id, Arc::clone(&reached), Arc::clone(&release));
        let attacker_root = root.clone();
        let attacker_relocated = relocated.clone();
        let attacker_victim = victim.clone();
        let attacker = std::thread::spawn(move || {
            reached.wait();
            std::fs::rename(&attacker_root, &attacker_relocated).unwrap();
            symlink(&attacker_victim, &attacker_root).unwrap();
            release.wait();
        });

        let outcome = pinned.reclaim_target(id, 1);
        attacker.join().unwrap();
        assert_eq!(outcome.kind, TargetReclaimKind::StagedRemoved);
        assert!(victim.join("target/outside-sentinel").exists());
        assert!(root.is_symlink());
        assert!(relocated.join("source-survives").exists());
        assert!(!relocated.join("target").exists());
    }

    #[test]
    fn target_reclaim_target_swap_is_refused_without_touching_replacement() {
        let (_temp, base, root, id) = fixture();
        std::fs::write(root.join("target/original"), b"original").unwrap();

        let pinned = PinnedSandboxRoot::open(&base, &root, id).unwrap();
        let reached = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        set_post_authentication_barrier_for_test(id, Arc::clone(&reached), Arc::clone(&release));
        let attacker_root = root.clone();
        let attacker = std::thread::spawn(move || {
            reached.wait();
            std::fs::rename(
                attacker_root.join("target"),
                attacker_root.join("target-original"),
            )
            .unwrap();
            std::fs::create_dir(attacker_root.join("target")).unwrap();
            std::fs::write(attacker_root.join("target/replacement"), b"replacement").unwrap();
            release.wait();
        });

        let outcome = pinned.reclaim_target(id, 1);
        attacker.join().unwrap();
        assert_eq!(outcome.reason, Some(SkipReason::TargetIdentityChanged));
        assert!(root.join("target/replacement").exists());
        assert!(root.join("target-original/original").exists());
        assert!(
            std::fs::read_dir(base.join(QUEUE_NAME))
                .unwrap()
                .next()
                .is_none()
        );
    }

    #[test]
    fn target_reclaim_partial_delete_reopens_and_converges() {
        let (_temp, base, root, id) = fixture();
        std::fs::create_dir(root.join("target/nested")).unwrap();
        std::fs::write(root.join("target/a"), b"a").unwrap();
        std::fs::write(root.join("target/nested/b"), b"b").unwrap();
        fail_delete_after_for_test(id, 7, 1);
        let pinned = PinnedSandboxRoot::open(&base, &root, id).unwrap();
        let outcome = pinned.reclaim_target(id, 7);
        assert_eq!(outcome.kind, TargetReclaimKind::StagedPending);
        assert!(!root.join("target").exists());
        let recovered = recover_staged_targets(&base, false);
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].kind, TargetReclaimKind::RecoveredRemoved);
        assert!(recover_staged_targets(&base, false).is_empty());
    }

    #[test]
    fn target_reclaim_final_rmdir_failure_reopens_and_converges() {
        let (_temp, base, root, id) = fixture();
        fail_delete_after_for_test(id, 8, 0);
        let pinned = PinnedSandboxRoot::open(&base, &root, id).unwrap();
        let outcome = pinned.reclaim_target(id, 8);
        assert_eq!(outcome.kind, TargetReclaimKind::StagedPending);
        assert_eq!(outcome.reason, Some(SkipReason::StagedDeletionIncomplete));
        assert!(!root.join("target").exists());

        let recovered = recover_staged_targets(&base, false);
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].kind, TargetReclaimKind::RecoveredRemoved);
        assert!(recover_staged_targets(&base, false).is_empty());
    }

    #[test]
    fn target_reclaim_queue_collision_refuses_without_overwrite() {
        let (_temp, base, root, id) = fixture();
        std::fs::write(root.join("target/cache"), b"cache").unwrap();
        let pinned = PinnedSandboxRoot::open(&base, &root, id).unwrap();
        let target = pinned.open_target().unwrap();
        let queue = pinned.ensure_queue().unwrap();
        let collision = stage_name(id, 2, target.dev, target.ino);
        let bucket = ensure_private_child_directory(
            queue.as_raw_fd(),
            &bucket_name(stage_bucket_index(&collision)),
            target.dev,
        )
        .unwrap();
        mkdirat(
            Some(bucket.as_raw_fd()),
            collision.as_str(),
            Mode::from_bits_truncate(0o700),
        )
        .unwrap();

        let outcome = pinned.reclaim_target(id, 2);
        assert_eq!(outcome.reason, Some(SkipReason::StageConflict));
        assert!(root.join("target/cache").exists());
        assert!(
            base.join(QUEUE_NAME)
                .join(bucket_name(stage_bucket_index(&collision)))
                .join(collision)
                .is_dir()
        );
    }

    #[test]
    fn target_reclaim_rejects_direct_target_symlink_and_identity_drift() {
        let (temp, base, root, id) = fixture();
        std::fs::remove_dir(root.join("target")).unwrap();
        let victim = temp.path().join("victim");
        std::fs::create_dir(&victim).unwrap();
        symlink(&victim, root.join("target")).unwrap();
        let pinned = PinnedSandboxRoot::open(&base, &root, id).unwrap();
        assert_eq!(
            pinned.inspect_target().reason,
            Some(SkipReason::TargetSymlink)
        );
        assert!(victim.exists());
    }

    #[test]
    fn target_reclaim_rejects_root_escape_and_special_target() {
        let (temp, base, root, id) = fixture();
        let outside = temp.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        assert_eq!(
            PinnedSandboxRoot::open(&base, &outside, id).unwrap_err(),
            SkipReason::GitOrRootIdentityRefusal
        );

        std::fs::remove_dir(root.join("target")).unwrap();
        std::fs::write(root.join("target"), b"not a directory").unwrap();
        let pinned = PinnedSandboxRoot::open(&base, &root, id).unwrap();
        assert_eq!(
            pinned.inspect_target().reason,
            Some(SkipReason::TargetNotDirectory)
        );
    }

    #[test]
    fn target_reclaim_rejects_unreadable_nested_directory_before_staging() {
        let (_temp, base, root, id) = fixture();
        let unreadable = root.join("target/unreadable");
        std::fs::create_dir(&unreadable).unwrap();
        std::fs::write(unreadable.join("cache"), b"cache").unwrap();
        std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o000)).unwrap();

        let pinned = PinnedSandboxRoot::open(&base, &root, id).unwrap();
        let outcome = pinned.reclaim_target(id, 1);
        std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(outcome.reason, Some(SkipReason::UnreadableEntry));
        assert!(root.join("target/unreadable/cache").exists());
        assert!(!base.join(QUEUE_NAME).exists());
    }

    #[test]
    fn stage_name_round_trip_is_strict() {
        let id = Uuid::new_v4();
        assert_eq!(
            parse_stage_name(&stage_name(id, 9, 10, 11)),
            Some((id, 9, 10, 11))
        );
        assert!(parse_stage_name("v1_bad_9_10_11").is_none());
        assert!(parse_stage_name("v2_00000000-0000-0000-0000-000000000000_1_2_3").is_none());
        assert_eq!(
            map_open_error(Errno::ENOSYS),
            SkipReason::Openat2Unavailable
        );
    }

    #[test]
    fn rejected_stage_name_round_trip_and_length_are_strict() {
        let id = Uuid::nil();
        let active = stage_name(id, u64::MAX, u64::MAX, u64::MAX);
        let rejected = rejected_stage_name(&active).expect("strict active stage name");
        assert_eq!(
            rejected.basename,
            format!("{REJECTED_STAGE_PREFIX}{active}")
        );
        assert_eq!(
            rejected.basename.len(),
            REJECTED_STAGE_PREFIX.len() + active.len()
        );
        assert!(rejected.basename.len() <= nix::libc::NAME_MAX as usize);
        assert_eq!(
            parse_stage_name(
                rejected
                    .basename
                    .strip_prefix(REJECTED_STAGE_PREFIX)
                    .expect("exact direct-v2 prefix")
            ),
            Some((id, u64::MAX, u64::MAX, u64::MAX))
        );
        assert_eq!(rejected.expected_dev, u64::MAX);
        assert_eq!(rejected.expected_ino, u64::MAX);
        assert!(rejected_stage_name("v1_bad_1_2_3").is_none());
        assert!(rejected_stage_name(&format!("{active}_suffix")).is_none());
    }

    #[test]
    fn target_allocated_size_deduplicates_hard_links() {
        let (_temp, base, root, id) = fixture();
        let file = root.join("target/data");
        std::fs::write(&file, vec![0_u8; 8192]).unwrap();
        std::fs::hard_link(&file, root.join("target/other")).unwrap();
        let allocated = std::fs::metadata(&file).unwrap().blocks() * 512;
        let pinned = PinnedSandboxRoot::open(&base, &root, id).unwrap();
        let bytes = pinned.inspect_target().bytes;
        assert!(bytes >= allocated);
        assert!(bytes < allocated.saturating_mul(2).saturating_add(32 * 1024));
    }

    #[test]
    fn target_preview_excludes_externally_retained_hard_link_blocks() {
        let (temp, base, root, id) = fixture();
        let file = root.join("target/data");
        std::fs::write(&file, vec![7_u8; 8 * 1024 * 1024]).unwrap();
        std::fs::File::open(&file).unwrap().sync_all().unwrap();
        let external = temp.path().join("external-link");
        std::fs::hard_link(&file, &external).unwrap();
        let allocated = std::fs::metadata(&file).unwrap().blocks() * 512;

        let pass = ReclaimPassState::new();
        let pinned = PinnedSandboxRoot::open(&base, &root, id).unwrap();
        let inspected = with_reclaim_pass_state(&pass, || pinned.inspect_target());
        let (reclaimable, _) = pass.reclaimable_summary();
        assert_eq!(inspected.kind, TargetReclaimKind::Inspected);
        assert!(
            inspected.bytes.saturating_sub(reclaimable) >= allocated,
            "external file blocks must stay out of preview: {inspected:?}, reclaimable={reclaimable}, allocated={allocated}"
        );

        let before = available_bytes(&base);
        let actual_pass = ReclaimPassState::new();
        let removed = with_reclaim_pass_state(&actual_pass, || pinned.reclaim_target(id, 11));
        let after = available_bytes(&base);
        assert_eq!(removed.kind, TargetReclaimKind::StagedRemoved);
        assert!(external.exists());
        assert_eq!(
            std::fs::metadata(&external).unwrap().blocks() * 512,
            allocated
        );
        assert!(
            after.saturating_sub(before) < allocated,
            "statvfs must not report the externally retained file's blocks as reclaimed"
        );
    }

    #[test]
    fn post_sizing_hard_link_drift_is_discarded_before_capacity_estimation() {
        let (temp, base, root, id) = fixture();
        let file = root.join("target/raced");
        std::fs::write(&file, vec![8_u8; 8 * 1024 * 1024]).unwrap();
        std::fs::File::open(&file).unwrap().sync_all().unwrap();
        let allocated = std::fs::metadata(&file).unwrap().blocks() * 512;
        let external = temp.path().join("post-sizing-external-link");
        let pinned = PinnedSandboxRoot::open(&base, &root, id).unwrap();
        let pass = ReclaimPassState::new();
        let reached = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        set_post_authentication_barrier_for_test(id, Arc::clone(&reached), Arc::clone(&release));
        let raced_file = file.clone();
        let raced_external = external.clone();
        let attacker = std::thread::spawn(move || {
            reached.wait();
            std::fs::hard_link(raced_file, raced_external).unwrap();
            release.wait();
        });

        let outcome = with_reclaim_pass_state(&pass, || pinned.reclaim_target(id, 111));
        attacker.join().unwrap();
        let (estimate, _) = pass.reclaimable_summary();
        assert_eq!(outcome.kind, TargetReclaimKind::StagedRemoved);
        assert!(external.exists(), "external namespace must not be deleted");
        assert!(
            outcome.bytes.saturating_sub(estimate) >= allocated,
            "the drifted inode contributes zero estimated capacity: outcome={outcome:?}, estimate={estimate}, allocated={allocated}"
        );
    }

    #[test]
    fn pass_ledger_counts_cross_candidate_hard_link_once_when_all_links_delete() {
        let temp = tempdir().unwrap();
        let base = temp.path().join("base");
        std::fs::create_dir(&base).unwrap();
        let first_id = Uuid::new_v4();
        let second_id = Uuid::new_v4();
        let first_root = base.join(first_id.to_string());
        let second_root = base.join(second_id.to_string());
        std::fs::create_dir_all(first_root.join("target")).unwrap();
        std::fs::create_dir_all(second_root.join("target")).unwrap();
        let first_file = first_root.join("target/shared");
        let second_file = second_root.join("target/shared");
        std::fs::write(&first_file, vec![9_u8; 8 * 1024 * 1024]).unwrap();
        std::fs::File::open(&first_file)
            .unwrap()
            .sync_all()
            .unwrap();
        std::fs::hard_link(&first_file, &second_file).unwrap();
        let allocated = std::fs::metadata(&first_file).unwrap().blocks() * 512;
        let first = PinnedSandboxRoot::open(&base, &first_root, first_id).unwrap();
        let second = PinnedSandboxRoot::open(&base, &second_root, second_id).unwrap();

        let pass = ReclaimPassState::new();
        let first_outcome = with_reclaim_pass_state(&pass, || first.inspect_target());
        let (first_claim, _) = pass.reclaimable_summary();
        assert!(
            first_outcome.bytes.saturating_sub(first_claim) >= allocated,
            "one retaining candidate link is not yet the full deletion set"
        );
        let second_outcome = with_reclaim_pass_state(&pass, || second.inspect_target());
        let (complete_claim, _) = pass.reclaimable_summary();
        assert_eq!(
            complete_claim,
            first_outcome
                .bytes
                .saturating_add(second_outcome.bytes)
                .saturating_sub(allocated),
            "the shared inode contributes exactly one allocated-block charge"
        );

        let before = available_bytes(&base);
        let actual = ReclaimPassState::new();
        assert_eq!(
            with_reclaim_pass_state(&actual, || first.reclaim_target(first_id, 12)).kind,
            TargetReclaimKind::StagedRemoved
        );
        assert_eq!(
            with_reclaim_pass_state(&actual, || second.reclaim_target(second_id, 13)).kind,
            TargetReclaimKind::StagedRemoved
        );
        let after = available_bytes(&base);
        assert!(!first_file.exists() && !second_file.exists());
        assert!(
            after >= before,
            "actual statvfs availability cannot regress"
        );
    }

    #[test]
    fn newly_created_queue_requires_parent_fsync_before_staging_and_reopens_safely() {
        let (_temp, base, root, id) = fixture();
        std::fs::write(root.join("target/cache"), b"cache").unwrap();
        install_base_fsync_fault_for_test(&base);
        let pinned = PinnedSandboxRoot::open(&base, &root, id).unwrap();
        let refused = pinned.reclaim_target(id, 14);
        assert_eq!(refused.kind, TargetReclaimKind::Refused);
        assert_eq!(refused.reason, Some(SkipReason::StagedDeletionIncomplete));
        assert!(root.join("target/cache").exists());
        assert_eq!(base_fsync_counts_for_test(&base), (1, 1));

        drop(pinned);
        let reopened = PinnedSandboxRoot::open(&base, &root, id).unwrap();
        fail_delete_after_for_test(id, 14, 0);
        let staged = reopened.reclaim_target(id, 14);
        assert_eq!(staged.kind, TargetReclaimKind::StagedPending);
        assert!(!root.join("target").exists());
        assert_eq!(base_fsync_counts_for_test(&base), (2, 1));
        drop(reopened);

        let recovered = recover_staged_targets(&base, false);
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].kind, TargetReclaimKind::RecoveredRemoved);
        assert!(recover_staged_targets(&base, false).is_empty());
    }

    #[test]
    fn reclaim_budget_exact_limits_and_limit_plus_one_are_typed() {
        let limits = ReclaimWorkLimits {
            recovery_entries: 1,
            filesystem_entries: 1,
            allocated_bytes: 10,
            duration: Duration::from_secs(60),
            depth: 1,
        };

        let mut recovery = ReclaimWorkBudget::new(limits);
        assert_eq!(recovery.charge_recovery_entry(), Ok(()));
        assert_eq!(
            recovery.charge_recovery_entry(),
            Err(SkipReason::RecoveryEntryBudget)
        );

        let mut entries = ReclaimWorkBudget::new(limits);
        assert_eq!(entries.charge_filesystem_entry(1), Ok(()));
        assert_eq!(
            entries.charge_filesystem_entry(1),
            Err(SkipReason::FilesystemEntryBudget)
        );
        assert_eq!(entries.checkpoint(1), Ok(()));
        assert_eq!(entries.checkpoint(2), Err(SkipReason::DepthBudget));

        let mut bytes = ReclaimWorkBudget::new(limits);
        assert_eq!(bytes.charge_allocated_bytes(10), Ok(()));
        assert_eq!(bytes.charge_allocated_bytes(1), Err(SkipReason::ByteBudget));

        let mut duration = ReclaimWorkBudget::new(ReclaimWorkLimits {
            duration: Duration::from_nanos(10),
            ..limits
        });
        duration.elapsed_override = Some(Duration::from_nanos(10));
        assert_eq!(duration.checkpoint(0), Ok(()));
        duration.elapsed_override = Some(Duration::from_nanos(11));
        assert_eq!(duration.checkpoint(0), Err(SkipReason::DurationBudget));
    }

    #[test]
    fn recovery_bucket_cursor_eventually_reaches_later_stage() {
        let temp = tempdir().unwrap();
        let base = temp.path().join("base");
        std::fs::create_dir(&base).unwrap();
        for generation in [20_u64, 21] {
            let id = Uuid::new_v4();
            let root = base.join(id.to_string());
            std::fs::create_dir_all(root.join("target")).unwrap();
            std::fs::write(root.join("target/cache"), b"cache").unwrap();
            fail_delete_after_for_test(id, generation, 0);
            let pinned = PinnedSandboxRoot::open(&base, &root, id).unwrap();
            assert_eq!(
                pinned.reclaim_target(id, generation).kind,
                TargetReclaimKind::StagedPending
            );
        }
        let mut limits = roomy_limits();
        // The directory's `.` and `..` entries are raw observations too and
        // consume the same fixed recovery-entry ceiling.
        limits.recovery_entries = 3;
        let pass = ReclaimPassState::for_test(limits);
        let outcomes = recover_staged_targets_with_state(&base, false, &pass);
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| outcome.kind == TargetReclaimKind::RecoveredRemoved)
                .count(),
            1
        );
        assert_eq!(active_stage_paths(&base).len(), 1);
        let first_cursor = recovery_cursor_marker(&base);
        let mut cursor_advanced_after_restart = false;

        for _ in 0..BUCKET_COUNT.saturating_mul(2) {
            if active_stage_paths(&base).is_empty() {
                break;
            }
            let before = recovery_cursor_marker(&base);
            let fresh_pass = ReclaimPassState::new();
            recover_staged_targets_with_state(&base, false, &fresh_pass);
            cursor_advanced_after_restart |= recovery_cursor_marker(&base) != before;
        }
        assert!(active_stage_paths(&base).is_empty());
        assert!(cursor_advanced_after_restart);
        assert_ne!(recovery_cursor_marker(&base), first_cursor);
    }

    #[test]
    fn over_quantum_same_bucket_malformed_prefix_cannot_starve_valid_stage() {
        let temp = tempdir().unwrap();
        let base = temp.path().join("base");
        std::fs::create_dir(&base).unwrap();
        let valid = stage_flat_target(&base, 1, 203);
        let bucket = valid.parent().unwrap().to_path_buf();
        for index in 0..=MAX_RECOVERY_ENTRIES_PER_PASS {
            std::fs::write(bucket.join(format!("malformed-{index:04}")), b"retain").unwrap();
        }

        let first_pass = ReclaimPassState::new();
        let first = recover_staged_targets_with_state(&base, false, &first_pass);
        assert!(
            first
                .iter()
                .any(|outcome| { outcome.reason == Some(SkipReason::RecoveryEntryBudget) })
        );
        assert_eq!(
            std::fs::read_dir(&bucket).unwrap().count(),
            4,
            "one quantum must evacuate every examined malformed entry"
        );

        for _ in 0..BUCKET_COUNT.saturating_add(1) {
            if std::fs::read_dir(&bucket).unwrap().next().is_none() {
                break;
            }
            let fresh_manager_pass = ReclaimPassState::new();
            recover_staged_targets_with_state(&base, false, &fresh_manager_pass);
        }
        assert!(std::fs::read_dir(&bucket).unwrap().next().is_none());
        assert!(active_stage_paths(&base).is_empty());
    }

    #[test]
    fn stage_in_wrong_bucket_is_terminally_rejected_without_identity_loss() {
        let temp = tempdir().unwrap();
        let base = temp.path().join("base");
        std::fs::create_dir(&base).unwrap();
        let stage = stage_flat_target(&base, 1, 204);
        let entry_name = stage.file_name().unwrap().to_str().unwrap().to_string();
        let metadata = std::fs::metadata(&stage).unwrap();
        let correct_bucket = stage_bucket_index(&entry_name);
        let wrong_bucket = correct_bucket.wrapping_add(1);
        let wrong_dir = base.join(QUEUE_NAME).join(bucket_name(wrong_bucket));
        if !wrong_dir.exists() {
            std::fs::create_dir(&wrong_dir).unwrap();
            std::fs::set_permissions(&wrong_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let misplaced = wrong_dir.join(&entry_name);
        std::fs::rename(&stage, &misplaced).unwrap();

        for _ in 0..BUCKET_COUNT.saturating_add(1) {
            if !misplaced.exists() {
                break;
            }
            recover_staged_targets_with_state(&base, false, &ReclaimPassState::new());
        }

        assert!(!misplaced.exists());
        let rejected = std::fs::read_dir(&base)
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .find(|path| {
                std::fs::metadata(path)
                    .ok()
                    .is_some_and(|actual| actual.ino() == metadata.ino())
            })
            .expect("misplaced identity retained in terminal rejection namespace");
        assert!(
            rejected
                .file_name()
                .unwrap()
                .as_bytes()
                .starts_with(REJECTED_STAGE_PREFIX.as_bytes())
        );
        assert_stage_identity(&rejected, metadata.dev(), metadata.ino());
        assert!(rejected.join("entry-0000").exists());
    }

    fn create_legacy_stage(queue: &Path, generation: u64) -> (String, u64, u64) {
        let provisional = queue.join(format!("provisional-{}", Uuid::new_v4()));
        std::fs::create_dir(&provisional).unwrap();
        std::fs::write(provisional.join("cache"), b"cache").unwrap();
        let metadata = std::fs::metadata(&provisional).unwrap();
        let entry_name = stage_name(Uuid::new_v4(), generation, metadata.dev(), metadata.ino());
        std::fs::rename(&provisional, queue.join(&entry_name)).unwrap();
        (entry_name, metadata.dev(), metadata.ino())
    }

    #[test]
    fn over_quantum_legacy_conflict_and_malformed_prefix_cannot_starve_tail() {
        let temp = tempdir().unwrap();
        let base = temp.path().join("base");
        let queue = base.join(QUEUE_NAME);
        std::fs::create_dir_all(&queue).unwrap();
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&queue, std::fs::Permissions::from_mode(0o700)).unwrap();

        let (conflict, conflict_dev, conflict_ino) = create_legacy_stage(&queue, 205);
        let conflict_bucket = queue.join(bucket_name(stage_bucket_index(&conflict)));
        std::fs::create_dir(&conflict_bucket).unwrap();
        std::fs::set_permissions(&conflict_bucket, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(conflict_bucket.join(&conflict), b"collision").unwrap();
        for index in 0..=LEGACY_MIGRATION_ENTRIES_PER_PASS {
            std::fs::write(queue.join(format!("malformed-{index:04}")), b"retain").unwrap();
        }
        let (tail, _, _) = create_legacy_stage(&queue, 206);

        let flat_names = || {
            std::fs::read_dir(&queue)
                .unwrap()
                .flatten()
                .map(|entry| entry.file_name())
                .filter(|name| {
                    name.as_bytes() != CURSOR_DIR.as_bytes()
                        && parse_bucket_name(name.as_bytes()).is_none()
                })
                .collect::<Vec<_>>()
        };
        for _ in 0..BUCKET_COUNT.saturating_mul(2) {
            if flat_names().is_empty() {
                break;
            }
            recover_staged_targets_with_state(&base, false, &ReclaimPassState::new());
        }

        assert!(
            flat_names().is_empty(),
            "legacy tail must migrate across restarts"
        );
        assert!(!queue.join(&tail).exists());
        let rejected = base.join(rejected_stage_name(&conflict).unwrap().basename);
        assert_stage_identity(&rejected, conflict_dev, conflict_ino);
        assert!(rejected.join("cache").exists());
    }

    #[test]
    fn rotation_durability_seams_retain_one_authenticated_stage() {
        for point in [
            RotationFaultPoint::BucketCreate,
            RotationFaultPoint::QueueParentFsync,
            RotationFaultPoint::Rename,
            RotationFaultPoint::SourceFsync,
            RotationFaultPoint::DestinationFsync,
        ] {
            let fixture = staged_depth_fixture();
            let source_bucket = stage_bucket_index(&fixture.entry_name);
            let source = fixture.stage.parent().unwrap();
            let queue_raw = open(
                &fixture.queue,
                OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW,
                Mode::empty(),
            )
            .unwrap();
            // SAFETY: `open` returned a new owned descriptor.
            let queue_fd = unsafe { OwnedFd::from_raw_fd(queue_raw) };
            let source_raw = open(
                source,
                OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW,
                Mode::empty(),
            )
            .unwrap();
            // SAFETY: `open` returned a new owned descriptor.
            let source_fd = unsafe { OwnedFd::from_raw_fd(source_raw) };
            let _reset = set_rotation_fault_for_test(&fixture.entry_name, point);

            assert_eq!(
                rotate_stage(
                    queue_fd.as_raw_fd(),
                    source_fd.as_raw_fd(),
                    source_bucket,
                    &fixture.entry_name,
                    fixture.dev,
                ),
                Err(SkipReason::StagedDeletionIncomplete),
                "fault point {point:?}"
            );

            if matches!(
                point,
                RotationFaultPoint::BucketCreate
                    | RotationFaultPoint::QueueParentFsync
                    | RotationFaultPoint::Rename
            ) {
                assert_stage_identity(&fixture.stage, fixture.dev, fixture.ino);
                rotate_stage(
                    queue_fd.as_raw_fd(),
                    source_fd.as_raw_fd(),
                    source_bucket,
                    &fixture.entry_name,
                    fixture.dev,
                )
                .unwrap();
            } else {
                assert!(!fixture.stage.exists());
            }

            let active = only_active_stage(&fixture.base);
            assert_stage_identity(&active, fixture.dev, fixture.ino);
            let active_name = active.file_name().unwrap().to_str().unwrap();
            assert_eq!(
                stage_bucket_index(active_name),
                source_bucket.wrapping_add(1),
                "fault point {point:?}"
            );
            let expected_bucket = bucket_name(source_bucket.wrapping_add(1));
            assert_eq!(
                active.parent().unwrap().file_name().unwrap(),
                OsStr::new(&expected_bucket),
                "fault point {point:?}"
            );
        }
    }

    #[test]
    fn recovery_run_reports_bounded_deletion_progress() {
        let (_temp, base, root, id) = fixture();
        std::fs::write(root.join("target/cache"), b"cache").unwrap();
        fail_delete_after_for_test(id, 22, 0);
        let pinned = PinnedSandboxRoot::open(&base, &root, id).unwrap();
        assert_eq!(
            pinned.reclaim_target(id, 22).kind,
            TargetReclaimKind::StagedPending
        );

        let pass = ReclaimPassState::new();
        let run = recover_staged_targets_run_with_state(&base, false, &pass);
        assert!(
            run.outcomes
                .iter()
                .any(|outcome| outcome.kind == TargetReclaimKind::RecoveredRemoved)
        );
        assert!(run.evidence.reserved);
        assert!(run.evidence.entries_deleted >= 2);
        assert_eq!(run.evidence.residual_entries, None);
        assert_eq!(run.evidence.nonprogress_count, 0);
        assert!(run.evidence.cursor_before.is_some());
        assert!(run.evidence.cursor_after.is_some());
    }

    #[test]
    fn post_cursor_rename_fsync_uncertainty_never_claims_durable_progress() {
        let (_temp, base, root, id) = fixture();
        std::fs::write(root.join("target/cache"), b"cache").unwrap();
        fail_delete_after_for_test(id, 207, 0);
        let pinned = PinnedSandboxRoot::open(&base, &root, id).unwrap();
        assert_eq!(
            pinned.reclaim_target(id, 207).kind,
            TargetReclaimKind::StagedPending
        );
        install_cursor_fsync_fault_for_test(&base.join(QUEUE_NAME));

        let run = recover_staged_targets_run_with_state(&base, false, &ReclaimPassState::new());
        assert!(!run.evidence.reserved);
        assert!(run.evidence.cursor_before.is_some());
        assert_eq!(run.evidence.cursor_after, None);
        assert_eq!(run.evidence.cycle_after, run.evidence.cycle_before);
        assert_eq!(run.evidence.entries_deleted, 0);
        assert_eq!(run.evidence.residual_entries, None);
        assert_eq!(run.evidence.nonprogress_count, 0);
        assert!(
            run.outcomes
                .iter()
                .any(|outcome| { outcome.reason == Some(SkipReason::StagedDeletionIncomplete) })
        );

        for _ in 0..BUCKET_COUNT.saturating_add(1) {
            if active_stage_paths(&base).is_empty() {
                break;
            }
            recover_staged_targets_with_state(&base, false, &ReclaimPassState::new());
        }
        assert!(active_stage_paths(&base).is_empty());
    }

    fn stage_flat_target(base: &Path, file_count: usize, generation: u64) -> PathBuf {
        let id = Uuid::new_v4();
        let root = base.join(id.to_string());
        std::fs::create_dir_all(root.join("target")).unwrap();
        for index in 0..file_count {
            std::fs::write(root.join("target").join(format!("entry-{index:04}")), b"x").unwrap();
        }
        fail_delete_after_for_test(id, generation, 0);
        let pinned = PinnedSandboxRoot::open(base, &root, id).unwrap();
        assert_eq!(
            pinned.reclaim_target(id, generation).kind,
            TargetReclaimKind::StagedPending
        );
        only_active_stage(base)
    }

    #[test]
    fn actual_recovery_same_entry_ceiling_shrinks_and_converges() {
        let limits = ReclaimWorkLimits {
            recovery_entries: 3,
            filesystem_entries: 6,
            allocated_bytes: 1024 * 1024 * 1024,
            duration: Duration::from_secs(60),
            depth: 8,
        };
        for (case, files, expected_passes) in [
            ("exact", 5_usize, 1_usize),
            ("plus_one", 6, 2),
            ("greater_than_half", 9, 2),
        ] {
            let temp = tempdir().unwrap();
            let base = temp.path().join("base");
            std::fs::create_dir(&base).unwrap();
            stage_flat_target(&base, files, 200);
            let mut previous = std::fs::read_dir(only_active_stage(&base)).unwrap().count();
            let mut passes = 0;
            while !active_stage_paths(&base).is_empty() {
                passes += 1;
                let pass = ReclaimPassState::for_test(limits);
                let outcomes = recover_staged_targets_with_state(&base, false, &pass);
                let remaining = active_stage_paths(&base)
                    .first()
                    .map_or(0, |stage| std::fs::read_dir(stage).unwrap().count());
                if remaining > 0 {
                    assert!(
                        remaining < previous,
                        "{case} must make monotonic progress: before={previous}, after={remaining}, outcomes={outcomes:?}"
                    );
                    assert!(outcomes.iter().any(|outcome| {
                        outcome.kind == TargetReclaimKind::RecoveredPending
                            && outcome.reason == Some(SkipReason::FilesystemEntryBudget)
                    }));
                }
                previous = remaining;
                assert!(passes <= expected_passes, "{case} did not converge");
            }
            assert_eq!(passes, expected_passes, "{case} pass count");
        }
    }

    #[test]
    fn duration_budget_exhaustion_rotates_recovery_stage_with_other_bounds_intact() {
        let temp = tempdir().unwrap();
        let base = temp.path().join("base");
        std::fs::create_dir(&base).unwrap();
        let initial_stage = stage_flat_target(&base, 2, 207);
        let initial_metadata = std::fs::metadata(&initial_stage).unwrap();
        let initial_name = initial_stage.file_name().unwrap().to_str().unwrap();
        let (custody_id, generation, expected_dev, expected_ino) =
            parse_stage_name(initial_name).expect("staged entry name");

        let limits = ReclaimWorkLimits {
            recovery_entries: 3,
            filesystem_entries: 2,
            allocated_bytes: 0,
            duration: Duration::from_nanos(1),
            depth: 1,
        };
        let pass = ReclaimPassState::for_test(limits);
        pass.set_elapsed_for_test(Duration::ZERO);
        set_elapsed_after_delete_for_test(custody_id, generation, Duration::from_nanos(2));

        let first = recover_staged_targets_with_state(&base, false, &pass);
        assert_eq!(
            first
                .iter()
                .filter(|outcome| outcome.kind == TargetReclaimKind::RecoveredPending)
                .count(),
            1,
            "one recovery entry stays within its quantum"
        );
        let pending = first
            .iter()
            .find(|outcome| outcome.kind == TargetReclaimKind::RecoveredPending)
            .unwrap();
        assert_eq!(pending.reason, Some(SkipReason::DurationBudget));
        assert!(!pending.terminal_rejection);
        assert!(
            first
                .iter()
                .any(|outcome| outcome.reason == Some(SkipReason::RecoveryEntryBudget))
        );

        let rotated = only_active_stage(&base);
        let rotated_name = rotated.file_name().unwrap().to_str().unwrap();
        let (rotated_custody, rotated_generation, rotated_dev, rotated_ino) =
            parse_stage_name(rotated_name).expect("rotated staged entry name");
        assert_eq!(
            (
                rotated_custody,
                rotated_generation,
                rotated_dev,
                rotated_ino
            ),
            (custody_id, generation, expected_dev, expected_ino)
        );
        assert_stage_identity(&rotated, initial_metadata.dev(), initial_metadata.ino());
        assert_eq!(
            rotated.parent().unwrap().file_name().unwrap(),
            OsStr::new(&bucket_name(
                stage_bucket_index(initial_name).wrapping_add(1)
            ))
        );

        let resumed = recover_staged_targets_with_state(&base, false, &ReclaimPassState::new());
        assert_eq!(resumed.len(), 1);
        assert_eq!(resumed[0].kind, TargetReclaimKind::RecoveredRemoved);
        assert!(active_stage_paths(&base).is_empty());
    }

    #[test]
    fn actual_recovery_unlinks_leaf_larger_than_same_byte_ceiling() {
        let temp = tempdir().unwrap();
        let base = temp.path().join("base");
        std::fs::create_dir(&base).unwrap();
        let id = Uuid::new_v4();
        let root = base.join(id.to_string());
        std::fs::create_dir_all(root.join("target")).unwrap();
        let leaf = root.join("target/oversized-leaf");
        std::fs::write(&leaf, vec![0x69_u8; 16 * 1024]).unwrap();
        let leaf_bytes = std::fs::metadata(&leaf).unwrap().blocks() * 512;
        let root_bytes = std::fs::metadata(root.join("target")).unwrap().blocks() * 512;
        assert!(
            leaf_bytes > root_bytes,
            "fixture leaf must exceed test ceiling"
        );

        fail_delete_after_for_test(id, 201, 0);
        let pinned = PinnedSandboxRoot::open(&base, &root, id).unwrap();
        assert_eq!(
            pinned.reclaim_target(id, 201).kind,
            TargetReclaimKind::StagedPending
        );
        let stage = only_active_stage(&base);

        let mut limits = roomy_limits();
        limits.allocated_bytes = root_bytes;
        let pass = ReclaimPassState::for_test(limits);
        let outcomes = recover_staged_targets_with_state(&base, false, &pass);
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].kind, TargetReclaimKind::RecoveredRemoved);
        assert!(outcomes[0].bytes <= root_bytes);
        assert!(!stage.exists(), "oversized leaf must not repeat ByteBudget");

        let fresh_pass = ReclaimPassState::for_test(limits);
        assert!(recover_staged_targets_with_state(&base, false, &fresh_pass).is_empty());
    }

    #[test]
    fn overdepth_recovery_is_rejected_once_and_leaves_active_queue() {
        let temp = tempdir().unwrap();
        let base = temp.path().join("base");
        std::fs::create_dir(&base).unwrap();
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700)).unwrap();
        let id = Uuid::new_v4();
        let root = base.join(id.to_string());
        std::fs::create_dir_all(root.join("target/one/two/three")).unwrap();
        std::fs::write(root.join("target/one/two/three/cache"), b"cache").unwrap();

        fail_delete_after_for_test(id, 202, 0);
        let pinned = PinnedSandboxRoot::open(&base, &root, id).unwrap();
        assert_eq!(
            pinned.reclaim_target(id, 202).kind,
            TargetReclaimKind::StagedPending
        );
        let stage_name = only_active_stage(&base).file_name().unwrap().to_owned();

        let mut limits = roomy_limits();
        limits.depth = 1;
        let first_pass = ReclaimPassState::for_test(limits);
        let first = recover_staged_targets_with_state(&base, false, &first_pass);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].kind, TargetReclaimKind::Refused);
        assert_eq!(first[0].reason, Some(SkipReason::DepthBudget));
        assert!(first[0].terminal_rejection);
        assert_eq!(active_stage_paths(&base).len(), 0);
        let stage_name = stage_name.to_str().unwrap();
        let rejected = base.join(rejected_stage_name(stage_name).unwrap().basename);
        assert!(rejected.join("one/two/three/cache").exists());

        let second_pass = ReclaimPassState::for_test(limits);
        assert!(recover_staged_targets_with_state(&base, false, &second_pass).is_empty());
        assert!(
            rejected.join("one/two/three/cache").exists(),
            "terminal rejection requires operator diagnosis and is never auto-deleted"
        );
    }

    #[test]
    fn rejection_source_name_replacement_never_terminally_reports_wrong_inode() {
        let fixture = staged_depth_fixture();
        let held = fixture.queue.join(format!("held-{}", Uuid::new_v4()));
        let rejected = fixture
            .base
            .join(rejected_stage_name(&fixture.entry_name).unwrap().basename);
        let reached = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let _reset = set_rejection_transition_barrier_for_test(
            &fixture.entry_name,
            "before_rename",
            Arc::clone(&reached),
            Arc::clone(&release),
        );
        let stage = fixture.stage.clone();
        let held_for_attacker = held.clone();
        let attacker = std::thread::spawn(move || {
            reached.wait();
            std::fs::rename(&stage, &held_for_attacker).unwrap();
            std::fs::create_dir(&stage).unwrap();
            std::fs::write(stage.join("replacement-sentinel"), b"replacement").unwrap();
            release.wait();
        });

        let outcome = recover_depth_limited(&fixture.base);
        attacker.join().unwrap();
        assert_pending_rejection(outcome, SkipReason::RejectedRecoveryEntry);
        assert_eq!(active_stage_paths(&fixture.base).len(), 0);
        assert_stage_identity(&held, fixture.dev, fixture.ino);
        assert!(held.join("one/two/three/cache").exists());
        assert!(
            rejected.join("replacement-sentinel").exists(),
            "wrongly moved content must be retained for diagnosis"
        );

        assert_stage_identity(&held, fixture.dev, fixture.ino);
        assert!(held.join("one/two/three/cache").exists());
        assert!(rejected.join("replacement-sentinel").exists());
    }

    #[test]
    fn rejection_legacy_parent_displacement_cannot_redirect_direct_terminal() {
        let fixture = staged_depth_fixture();
        let legacy = fixture.base.join(LEGACY_REJECTED_QUEUE_NAME);
        std::fs::create_dir(&legacy).unwrap();
        std::fs::write(legacy.join("legacy-sentinel"), b"legacy").unwrap();
        let displaced = fixture
            .base
            .join(format!("legacy-displaced-{}", Uuid::new_v4()));
        let terminal = fixture
            .base
            .join(rejected_stage_name(&fixture.entry_name).unwrap().basename);
        let reached = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let _reset = set_rejection_transition_barrier_for_test(
            &fixture.entry_name,
            "after_rename",
            Arc::clone(&reached),
            Arc::clone(&release),
        );
        let legacy_for_attacker = legacy.clone();
        let displaced_for_attacker = displaced.clone();
        let attacker = std::thread::spawn(move || {
            reached.wait();
            std::fs::rename(&legacy_for_attacker, &displaced_for_attacker).unwrap();
            std::fs::create_dir(&legacy_for_attacker).unwrap();
            std::fs::write(
                legacy_for_attacker.join("replacement-sentinel"),
                b"replacement",
            )
            .unwrap();
            release.wait();
        });

        let outcome = recover_depth_limited(&fixture.base);
        attacker.join().unwrap();
        assert_terminal_depth_rejection(outcome);
        assert_eq!(active_stage_paths(&fixture.base).len(), 0);
        assert_stage_identity(&terminal, fixture.dev, fixture.ino);
        assert!(terminal.join("one/two/three/cache").exists());
        assert_eq!(
            std::fs::read(displaced.join("legacy-sentinel")).unwrap(),
            b"legacy"
        );
        assert_eq!(
            std::fs::read(legacy.join("replacement-sentinel")).unwrap(),
            b"replacement"
        );
        assert!(recover_staged_targets(&fixture.base, false).is_empty());
        assert!(terminal.join("one/two/three/cache").exists());
        assert!(displaced.join("legacy-sentinel").exists());
        assert!(legacy.join("replacement-sentinel").exists());
    }

    #[test]
    fn rejection_destination_collision_is_pending_and_never_overwrites() {
        let fixture = staged_depth_fixture();
        let collision = fixture
            .base
            .join(rejected_stage_name(&fixture.entry_name).unwrap().basename);
        std::fs::create_dir(&collision).unwrap();
        std::fs::write(collision.join("operator-sentinel"), b"retain").unwrap();

        let first = recover_depth_limited(&fixture.base);
        assert_pending_rejection(first, SkipReason::StageConflict);
        assert_eq!(active_stage_paths(&fixture.base).len(), 1);
        assert_stage_identity(&only_active_stage(&fixture.base), fixture.dev, fixture.ino);
        assert!(collision.join("operator-sentinel").exists());

        let next = recover_depth_limited(&fixture.base);
        assert_pending_rejection(next, SkipReason::StageConflict);
        assert_stage_identity(&only_active_stage(&fixture.base), fixture.dev, fixture.ino);
        assert!(collision.join("operator-sentinel").exists());
    }

    #[test]
    fn rejection_namespace_symlink_cross_device_owner_and_mode_are_refused() {
        for case in ["symlink", "cross_device", "owner", "mode"] {
            let fixture = staged_depth_fixture();
            let terminal = fixture
                .base
                .join(rejected_stage_name(&fixture.entry_name).unwrap().basename);
            let mut reset = None;
            let expected_reason = match case {
                "symlink" => {
                    let outside = fixture.base.join(format!("outside-{}", Uuid::new_v4()));
                    std::fs::create_dir(&outside).unwrap();
                    std::fs::write(outside.join("operator-sentinel"), b"retain").unwrap();
                    symlink(&outside, &terminal).unwrap();
                    SkipReason::StageConflict
                }
                "cross_device" => {
                    reset = Some(set_rejection_fault_for_test(
                        &fixture.entry_name,
                        RejectionFault::RenameCrossDevice,
                    ));
                    SkipReason::MountOrDeviceCrossing
                }
                "owner" => {
                    reset = Some(set_rejection_fault_for_test(
                        &fixture.entry_name,
                        RejectionFault::BaseOwner,
                    ));
                    SkipReason::RejectedRecoveryEntry
                }
                "mode" => {
                    std::fs::set_permissions(&fixture.base, std::fs::Permissions::from_mode(0o755))
                        .unwrap();
                    SkipReason::RejectedRecoveryEntry
                }
                _ => unreachable!(),
            };

            let first = recover_depth_limited(&fixture.base);
            drop(reset);
            assert_pending_rejection(first, expected_reason);
            assert_eq!(active_stage_paths(&fixture.base).len(), 1, "{case}");
            let active = only_active_stage(&fixture.base);
            assert_stage_identity(&active, fixture.dev, fixture.ino);
            assert!(active.join("one/two/three/cache").exists());

            match case {
                "symlink" => std::fs::remove_file(&terminal).unwrap(),
                "mode" => {
                    std::fs::set_permissions(&fixture.base, std::fs::Permissions::from_mode(0o700))
                        .unwrap()
                }
                _ => {}
            }
            let next = recover_depth_limited(&fixture.base);
            assert_terminal_depth_rejection(next);
            assert_eq!(active_stage_paths(&fixture.base).len(), 0, "{case}");
            assert_stage_identity(&terminal, fixture.dev, fixture.ino);
            assert!(terminal.join("one/two/three/cache").exists());
            assert!(recover_staged_targets(&fixture.base, false).is_empty());
            assert!(terminal.join("one/two/three/cache").exists());
        }
    }

    #[test]
    fn rejection_durability_failures_retry_before_move_and_retain_after_move() {
        for phase in ["before", "after"] {
            for directory in ["queue", "base"] {
                let fixture = staged_depth_fixture();
                let terminal = fixture
                    .base
                    .join(rejected_stage_name(&fixture.entry_name).unwrap().basename);
                let reset = set_rejection_fault_for_test(
                    &fixture.entry_name,
                    RejectionFault::Sync { phase, directory },
                );
                let first = recover_depth_limited(&fixture.base);
                drop(reset);
                assert_pending_rejection(first, SkipReason::StagedDeletionIncomplete);
                if phase == "before" {
                    assert_eq!(
                        active_stage_paths(&fixture.base).len(),
                        1,
                        "{phase}/{directory}"
                    );
                    assert!(!terminal.exists(), "{phase}/{directory}");
                    let active = only_active_stage(&fixture.base);
                    assert_stage_identity(&active, fixture.dev, fixture.ino);
                    assert!(active.join("one/two/three/cache").exists());

                    let next = recover_depth_limited(&fixture.base);
                    assert_terminal_depth_rejection(next);
                    assert_eq!(
                        active_stage_paths(&fixture.base).len(),
                        0,
                        "{phase}/{directory}"
                    );
                    assert_stage_identity(&terminal, fixture.dev, fixture.ino);
                    assert!(terminal.join("one/two/three/cache").exists());
                } else {
                    assert_eq!(
                        active_stage_paths(&fixture.base).len(),
                        0,
                        "{phase}/{directory}"
                    );
                    assert!(!fixture.stage.exists(), "{phase}/{directory}");
                    assert_stage_identity(&terminal, fixture.dev, fixture.ino);
                    assert!(terminal.join("one/two/three/cache").exists());
                }
                assert!(recover_staged_targets(&fixture.base, false).is_empty());
                assert!(terminal.join("one/two/three/cache").exists());
            }
        }
    }

    #[test]
    fn rejection_after_auth_before_failure_handling_never_rolls_back_by_name() {
        assert_hostile_direct_terminal_replacement_is_retained(
            "after_initial_direct_auth_before_failure",
        );
    }

    #[test]
    fn rejection_final_post_sync_direct_entry_alternation_is_non_terminal_and_retained() {
        assert_hostile_direct_terminal_replacement_is_retained("before_final_direct_proof");
    }

    #[test]
    fn direct_and_legacy_rejected_names_are_excluded_from_active_recovery() {
        let temp = tempdir().unwrap();
        let base = temp.path().join("base");
        let queue = base.join(QUEUE_NAME);
        std::fs::create_dir_all(&queue).unwrap();
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&queue, std::fs::Permissions::from_mode(0o700)).unwrap();

        let provisional = base.join("provisional-terminal");
        std::fs::create_dir(&provisional).unwrap();
        std::fs::write(provisional.join("sentinel"), b"valid-terminal").unwrap();
        let metadata = std::fs::metadata(&provisional).unwrap();
        let active_name = stage_name(Uuid::new_v4(), 7, metadata.dev(), metadata.ino());
        let valid_name = rejected_stage_name(&active_name).unwrap().basename;
        let valid = base.join(&valid_name);
        std::fs::rename(&provisional, &valid).unwrap();

        let malformed_names = [
            format!("{REJECTED_STAGE_PREFIX}held_{}", Uuid::new_v4()),
            format!("{REJECTED_STAGE_PREFIX}v1_bad_1_2_3"),
        ];
        for (index, name) in malformed_names.iter().enumerate() {
            let path = base.join(name);
            std::fs::create_dir(&path).unwrap();
            std::fs::write(path.join("sentinel"), format!("malformed-{index}")).unwrap();
        }
        let legacy = base.join(LEGACY_REJECTED_QUEUE_NAME);
        std::fs::create_dir(&legacy).unwrap();
        std::fs::write(legacy.join("sentinel"), b"legacy").unwrap();

        assert!(Uuid::parse_str(&valid_name).is_err());
        assert!(
            malformed_names
                .iter()
                .all(|name| Uuid::parse_str(name).is_err())
        );
        assert!(Uuid::parse_str(LEGACY_REJECTED_QUEUE_NAME).is_err());
        assert!(recover_staged_targets(&base, false).is_empty());
        assert_eq!(
            std::fs::read(valid.join("sentinel")).unwrap(),
            b"valid-terminal"
        );
        for (index, name) in malformed_names.iter().enumerate() {
            assert_eq!(
                std::fs::read(base.join(name).join("sentinel")).unwrap(),
                format!("malformed-{index}").as_bytes()
            );
        }
        assert_eq!(std::fs::read(legacy.join("sentinel")).unwrap(), b"legacy");
    }

    #[test]
    fn filesystem_byte_depth_and_duration_budgets_leave_recoverable_work() {
        let (_temp, base, root, id) = fixture();
        std::fs::write(root.join("target/a"), vec![1_u8; 4096]).unwrap();
        std::fs::write(root.join("target/b"), vec![2_u8; 4096]).unwrap();
        let target_bytes = std::fs::metadata(root.join("target")).unwrap().blocks() * 512
            + std::fs::metadata(root.join("target/a")).unwrap().blocks() * 512
            + std::fs::metadata(root.join("target/b")).unwrap().blocks() * 512;
        let target_dir_bytes = std::fs::metadata(root.join("target")).unwrap().blocks() * 512;
        let pinned = PinnedSandboxRoot::open(&base, &root, id).unwrap();

        let mut entry_limits = roomy_limits();
        // The exact preflight charges are reusable deletion credits, so the
        // same bounded tree is not charged twice before the first unlink.
        entry_limits.filesystem_entries = 4;
        let entry_pass = ReclaimPassState::for_test(entry_limits);
        let pending = with_reclaim_pass_state(&entry_pass, || pinned.reclaim_target(id, 30));
        assert_eq!(pending.kind, TargetReclaimKind::StagedRemoved);
        assert!(!root.join("target").exists());

        std::fs::create_dir(root.join("target")).unwrap();
        std::fs::write(root.join("target/a"), vec![3_u8; 4096]).unwrap();
        std::fs::write(root.join("target/b"), vec![4_u8; 4096]).unwrap();
        let pinned = PinnedSandboxRoot::open(&base, &root, id).unwrap();
        let mut byte_limits = roomy_limits();
        byte_limits.allocated_bytes = target_bytes.saturating_add(target_dir_bytes);
        let byte_pass = ReclaimPassState::for_test(byte_limits);
        let pending = with_reclaim_pass_state(&byte_pass, || pinned.reclaim_target(id, 31));
        assert_eq!(pending.kind, TargetReclaimKind::StagedRemoved);

        std::fs::create_dir_all(root.join("target/one/two")).unwrap();
        std::fs::write(root.join("target/one/two/cache"), b"cache").unwrap();
        let pinned = PinnedSandboxRoot::open(&base, &root, id).unwrap();
        let mut depth_limits = roomy_limits();
        depth_limits.depth = 1;
        let depth_pass = ReclaimPassState::for_test(depth_limits);
        let refused = with_reclaim_pass_state(&depth_pass, || pinned.reclaim_target(id, 32));
        assert_eq!(refused.kind, TargetReclaimKind::Refused);
        assert_eq!(refused.reason, Some(SkipReason::DepthBudget));
        assert!(root.join("target/one/two/cache").exists());

        let mut exact_duration_limits = roomy_limits();
        exact_duration_limits.duration = Duration::from_millis(1);
        let duration_pass = ReclaimPassState::for_test(exact_duration_limits);
        duration_pass.set_elapsed_for_test(Duration::from_millis(1));
        assert_eq!(
            with_reclaim_pass_state(&duration_pass, || pinned.inspect_target()).kind,
            TargetReclaimKind::Inspected
        );
        let duration_stop = ReclaimPassState::for_test(exact_duration_limits);
        duration_stop.set_elapsed_for_test(Duration::from_millis(1) + Duration::from_nanos(1));
        let refused = with_reclaim_pass_state(&duration_stop, || pinned.reclaim_target(id, 33));
        assert_eq!(refused.kind, TargetReclaimKind::Refused);
        assert_eq!(refused.reason, Some(SkipReason::DurationBudget));

        let later = ReclaimPassState::new();
        assert_eq!(
            with_reclaim_pass_state(&later, || pinned.reclaim_target(id, 33)).kind,
            TargetReclaimKind::StagedRemoved
        );
        assert!(recover_staged_targets(&base, false).is_empty());
    }

    #[test]
    fn target_reclaim_refuses_injected_nested_cross_device_before_staging() {
        let (_temp, base, root, id) = fixture();
        let entry = format!("foreign-device-{}", Uuid::new_v4());
        std::fs::create_dir(root.join("target").join(&entry)).unwrap();
        let _fault = force_exdev_entry_for_test(OsStr::new(&entry));

        let pinned = PinnedSandboxRoot::open(&base, &root, id).unwrap();
        let outcome = pinned.reclaim_target(id, 1);

        assert_eq!(outcome.reason, Some(SkipReason::MountOrDeviceCrossing));
        assert!(root.join("target").exists());
        assert!(!base.join(QUEUE_NAME).exists());
    }

    #[test]
    fn target_reclaim_refuses_foreign_device_recovery_and_then_converges() {
        let (_temp, base, root, id) = fixture();
        std::fs::write(root.join("target/cache"), b"cache").unwrap();
        fail_delete_after_for_test(id, 3, 0);
        let pinned = PinnedSandboxRoot::open(&base, &root, id).unwrap();
        assert_eq!(
            pinned.reclaim_target(id, 3).kind,
            TargetReclaimKind::StagedPending
        );
        let entry = only_active_stage(&base).file_name().unwrap().to_owned();
        let fault = force_exdev_entry_for_test(&entry);
        let refused = recover_staged_targets_run_with_state(&base, false, &ReclaimPassState::new());
        drop(fault);

        assert_eq!(refused.outcomes.len(), 1);
        assert_eq!(
            refused.outcomes[0].reason,
            Some(SkipReason::MountOrDeviceCrossing)
        );
        assert_eq!(
            refused.evidence.nonprogress_count, 0,
            "durable one-cycle backoff is forward progress, not nonprogress"
        );
        for _ in 0..BUCKET_COUNT.saturating_mul(2) {
            if active_stage_paths(&base).is_empty() {
                break;
            }
            recover_staged_targets(&base, false);
        }
        assert!(active_stage_paths(&base).is_empty());
    }

    #[test]
    fn target_reclaim_rejects_unsafe_recovery_queue() {
        let (_temp, base, _root, _id) = fixture();
        std::fs::create_dir(base.join(QUEUE_NAME)).unwrap();
        std::fs::set_permissions(
            base.join(QUEUE_NAME),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        let outcomes = recover_staged_targets(&base, false);
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].reason, Some(SkipReason::RejectedRecoveryEntry));
    }

    #[test]
    fn target_reclaim_rejects_staged_inode_mismatch() {
        let (_temp, base, _root, id) = fixture();
        let queue = base.join(QUEUE_NAME);
        std::fs::create_dir(&queue).unwrap();
        std::fs::set_permissions(&queue, std::fs::Permissions::from_mode(0o700)).unwrap();
        let stage = queue.join(stage_name(
            id,
            4,
            std::fs::metadata(&queue).unwrap().dev(),
            1,
        ));
        std::fs::create_dir(&stage).unwrap();
        std::fs::write(stage.join("sentinel"), b"retain").unwrap();

        let outcomes = recover_staged_targets(&base, false);
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].reason, Some(SkipReason::RejectedRecoveryEntry));
        assert!(!stage.exists());
    }
}
