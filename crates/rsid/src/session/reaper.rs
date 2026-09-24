//! Teardown / resume process-reaper.
//!
//! Closes the *untracked-orphan* half of the 2026-07-02 dual-resume incident
//! (A5). Two independent, additive mechanisms:
//!
//! - [`settle_process_ownership`] (Change 1): after a terminal candidate or
//!   requested user/stall interrupt, verify the provider actually died and,
//!   if it survived the grace window, escalate to SIGKILL **before**
//!   `finalize_session` drops the in-memory handle untracked. Acts on the
//!   known, still-tracked `Child` — no scanning.
//! - [`reap_orphans_for_session`] (Change 2): a surgical `/proc/*/environ`
//!   scan that SIGKILLs any live process byte-exact-stamped
//!   `RSI_SESSION_ID=<session_id>`, run in `continue_session` immediately
//!   before spawning so a *cross-restart* orphan (the actual incident, where
//!   the daemon's in-memory handle is long gone) can never coexist with the
//!   fresh child. The `RSI_SESSION_ID` env stamp is the only identity that
//!   survives a daemon restart, so an env-scan — not a retained PID — is the
//!   sole mechanism that closes the real case.
//!
//! Both are inert on the well-behaved path: [`settle_process_ownership`]
//! returns the instant the process is already dead and sends no signal; a
//! resume with no orphan present matches nothing and kills nothing. Exact
//! orphan repair is Linux-only; non-Linux Unix builds intentionally no-op
//! instead of substituting an unpinned numeric-PID signal or refusing boot.

use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs::File;
use std::io::Read;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
use std::time::{Duration, Instant as StdInstant};
use tokio::sync::RwLock;
use tokio::time::Instant;
use uuid::Uuid;

use super::types::{ProcessSettlementMode, ProcessSettlementOutcome, TrackedSession};
use crate::sandbox::git_worktree::QuarantineTreeProof;

/// Bounded grace for a SIGINT'd provider to exit before we SIGKILL. A
/// well-behaved provider exits far sooner — the first `is_alive()` probe reads
/// dead and [`settle_process_ownership`] returns instantly — so only a
/// *surviving* (wedged / SIGSTOP-frozen) provider waits the full grace.
///
/// NOTE: a single settlement attempt spends this grace **twice** — once in
/// `wait_for_exit` before escalating, and again in `wait_for_taken_process_exit`
/// confirming the SIGKILL landed. See [`SETTLEMENT_ATTEMPT_WORST_CASE`], which
/// is the number callers budgeting a teardown deadline actually want.
pub(super) const TEARDOWN_KILL_GRACE: Duration = Duration::from_secs(2);

/// Poll cadence for the liveness re-check inside the grace window. Never held
/// under a lock.
pub(super) const TEARDOWN_KILL_POLL: Duration = Duration::from_millis(100);

/// Result candidate drain window. The monitor continues consuming and
/// acknowledging provider events throughout this interval.
pub(super) const TERMINAL_DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Once the process/task is settled, allow its producer tasks a final bounded
/// interval to close their shared event sender.
pub(super) const TERMINAL_STREAM_CLOSE_GRACE: Duration = Duration::from_secs(2);

const STARTUP_ORPHAN_PROC_ENTRY_MAX: usize = 131_072;
const STARTUP_ORPHAN_STAMP_TUPLE_MAX: usize = crate::store::STARTUP_PROVIDER_CANDIDATE_MAX;
const STARTUP_ORPHAN_ENV_MAX_BYTES: usize = 2 * 1024 * 1024;
const STARTUP_ORPHAN_STAT_MAX_BYTES: usize = 16 * 1024;
const STARTUP_ORPHAN_CGROUP_MAX_BYTES: usize = 16 * 1024;
const STARTUP_ORPHAN_POST_KILL_GRACE: Duration = Duration::from_secs(2);
const STARTUP_ORPHAN_POST_KILL_POLL: Duration = Duration::from_millis(10);
const STARTUP_ORPHAN_FIXED_POINT_MAX_PASSES: usize = 8;
const STARTUP_ORPHAN_TOTAL_DEADLINE: Duration = Duration::from_secs(10);
const QUARANTINE_HOLDER_FIXED_POINT_PASSES: usize = 2;
const QUARANTINE_HOLDER_TOTAL_DEADLINE: Duration = Duration::from_secs(10);
const QUARANTINE_HOLDER_PROC_ENTRY_MAX: usize = 131_072;
const QUARANTINE_HOLDER_TASK_ENTRY_MAX: usize = 1_000_000;
const QUARANTINE_HOLDER_STATUS_MAX_BYTES: usize = 64 * 1024;
const QUARANTINE_HOLDER_FD_ENTRY_MAX: usize = 1_000_000;
const QUARANTINE_HOLDER_FDINFO_MAX_BYTES: usize = 16 * 1024 * 1024;
const QUARANTINE_HOLDER_IO_URING_USER_FILES_MAX: usize = 1_000_000;
const QUARANTINE_HOLDER_MAPS_MAX_BYTES: usize = 16 * 1024 * 1024;
const QUARANTINE_HOLDER_MAPS_MAX_RECORDS: usize = 1_000_000;
const QUARANTINE_HOLDER_MOUNTINFO_MAX_BYTES: usize = 16 * 1024 * 1024;
const QUARANTINE_HOLDER_MOUNTINFO_MAX_RECORDS: usize = 131_072;
const QUARANTINE_PLATFORM_CMDLINE_MAX_BYTES: usize = 4 * 1024;
const PORTAL_EXECUTABLE: &str = "/usr/lib/xdg-document-portal";

#[derive(Clone, Copy)]
struct QuarantineHolderScanLimits {
    fixed_point_passes: usize,
    total_deadline: Duration,
    max_proc_entries: usize,
    max_task_entries: usize,
    max_status_bytes: usize,
    max_fd_entries: usize,
    max_fdinfo_bytes: usize,
    max_io_uring_user_files: usize,
    max_maps_bytes: usize,
    max_maps_records: usize,
    max_mountinfo_bytes: usize,
    max_mountinfo_records: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct QuarantineHolderProof {
    fixed_point_passes: u32,
    trusted_platform_exemptions: u32,
    trusted_platform_exemptions_digest: String,
    evidence_digest: String,
}

impl QuarantineHolderProof {
    pub(super) const fn fixed_point_passes(&self) -> u32 {
        self.fixed_point_passes
    }

    pub(super) const fn trusted_platform_exemptions(&self) -> u32 {
        self.trusted_platform_exemptions
    }

    pub(super) fn trusted_platform_exemptions_digest(&self) -> &str {
        &self.trusted_platform_exemptions_digest
    }

    pub(super) fn evidence_digest(&self) -> &str {
        &self.evidence_digest
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ProcTaskCredentials {
    real: u32,
    effective: u32,
    saved: u32,
    filesystem: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ProcTaskStatus {
    pid: i32,
    tgid: i32,
    credentials: ProcTaskCredentials,
}

impl ProcTaskCredentials {
    const fn contains(self, uid: u32) -> bool {
        self.real == uid || self.effective == uid || self.saved == uid || self.filesystem == uid
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TrustedPlatformProcessClass {
    SystemdUserManager,
    SystemdSdPam,
    PortalFuseMountHelper,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TrustedPlatformProcessIdentity {
    class: TrustedPlatformProcessClass,
    start_time: u64,
    state: u8,
    uid: u32,
    parent_pid: i32,
    directory_device: u64,
    directory_inode: u64,
    parent_start_time: Option<u64>,
    parent_state: Option<u8>,
    parent_directory_device: Option<u64>,
    parent_directory_inode: Option<u64>,
    ancestor_pid: Option<i32>,
    ancestor_start_time: Option<u64>,
    ancestor_state: Option<u8>,
    ancestor_directory_device: Option<u64>,
    ancestor_directory_inode: Option<u64>,
    platform_binary_device: Option<u64>,
    platform_binary_inode: Option<u64>,
}

#[derive(Clone, Copy)]
enum QuarantineInventoryClass {
    Cwd = 0,
    Root = 1,
    Fd = 2,
    FdInfo = 3,
    Maps = 4,
    MountNamespace = 5,
    MountInfo = 6,
}

const QUARANTINE_INVENTORY_CLASS_COUNT: usize = 7;
const QUARANTINE_INVENTORY_CLASS_NAMES: [&[u8]; QUARANTINE_INVENTORY_CLASS_COUNT] = [
    b"cwd",
    b"root",
    b"fd",
    b"fdinfo",
    b"maps",
    b"mount-namespace",
    b"mountinfo",
];

#[derive(Default)]
struct QuarantineInventoryFailures {
    first: Option<String>,
    all_permission_denied: bool,
    permission_denied_classes: u8,
    mount_namespace_permission_denied: bool,
}

impl QuarantineInventoryFailures {
    fn record_io(
        &mut self,
        class: QuarantineInventoryClass,
        error: &std::io::Error,
        message: String,
    ) {
        let permission_denied = error.kind() == std::io::ErrorKind::PermissionDenied;
        let permission_only_before = self.first.is_some() && self.all_permission_denied;
        if self.first.is_none() || (permission_only_before && !permission_denied) {
            self.all_permission_denied = permission_denied;
            self.first = Some(message);
        } else {
            self.all_permission_denied &= permission_denied;
        }
        if permission_denied {
            self.permission_denied_classes |= 1_u8 << class as u8;
            if matches!(class, QuarantineInventoryClass::MountNamespace) {
                self.mount_namespace_permission_denied = true;
            }
        }
    }

    fn record_non_permission(&mut self, message: String) {
        if self.first.is_none() || self.all_permission_denied {
            self.first = Some(message);
        }
        self.all_permission_denied = false;
    }
}

#[derive(Default)]
struct TrustedPlatformExemptionCounts {
    systemd_user_manager: u32,
    systemd_sd_pam: u32,
    portal_fuse_mount_helper: u32,
    permission_denied_classes: [u32; QUARANTINE_INVENTORY_CLASS_COUNT],
}

impl TrustedPlatformExemptionCounts {
    fn record(
        &mut self,
        class: TrustedPlatformProcessClass,
        permission_denied_classes: u8,
    ) -> crate::error::Result<()> {
        let count = match class {
            TrustedPlatformProcessClass::SystemdUserManager => &mut self.systemd_user_manager,
            TrustedPlatformProcessClass::SystemdSdPam => &mut self.systemd_sd_pam,
            TrustedPlatformProcessClass::PortalFuseMountHelper => {
                &mut self.portal_fuse_mount_helper
            }
        };
        *count = count.checked_add(1).ok_or_else(|| {
            crate::error::DaemonError::Process(
                "quarantine trusted-platform exemption count overflowed".into(),
            )
        })?;
        for (index, count) in self.permission_denied_classes.iter_mut().enumerate() {
            if permission_denied_classes & (1_u8 << index) == 0 {
                continue;
            }
            *count = count.checked_add(1).ok_or_else(|| {
                crate::error::DaemonError::Process(
                    "quarantine trusted-platform denied-class count overflowed".into(),
                )
            })?;
        }
        Ok(())
    }

    fn total(&self) -> crate::error::Result<u32> {
        self.systemd_user_manager
            .checked_add(self.systemd_sd_pam)
            .and_then(|count| count.checked_add(self.portal_fuse_mount_helper))
            .ok_or_else(|| {
                crate::error::DaemonError::Process(
                    "quarantine trusted-platform exemption total overflowed".into(),
                )
            })
    }

    fn digest(&self, fixed_point_passes: u32) -> [u8; 32] {
        let mut digest = Sha256::new();
        digest.update(b"rsi-quarantine-trusted-platform-exemptions-v2\0");
        digest.update(fixed_point_passes.to_be_bytes());
        digest.update(b"systemd-user-manager\0");
        digest.update(self.systemd_user_manager.to_be_bytes());
        digest.update(b"systemd-sd-pam\0");
        digest.update(self.systemd_sd_pam.to_be_bytes());
        digest.update(b"portal-fusermount3\0");
        digest.update(self.portal_fuse_mount_helper.to_be_bytes());
        for (name, count) in QUARANTINE_INVENTORY_CLASS_NAMES
            .iter()
            .zip(self.permission_denied_classes)
        {
            digest.update(name);
            digest.update(b"\0");
            digest.update(count.to_be_bytes());
        }
        digest.finalize().into()
    }
}

impl Default for QuarantineHolderScanLimits {
    fn default() -> Self {
        Self {
            fixed_point_passes: QUARANTINE_HOLDER_FIXED_POINT_PASSES,
            total_deadline: QUARANTINE_HOLDER_TOTAL_DEADLINE,
            max_proc_entries: QUARANTINE_HOLDER_PROC_ENTRY_MAX,
            max_task_entries: QUARANTINE_HOLDER_TASK_ENTRY_MAX,
            max_status_bytes: QUARANTINE_HOLDER_STATUS_MAX_BYTES,
            max_fd_entries: QUARANTINE_HOLDER_FD_ENTRY_MAX,
            max_fdinfo_bytes: QUARANTINE_HOLDER_FDINFO_MAX_BYTES,
            max_io_uring_user_files: QUARANTINE_HOLDER_IO_URING_USER_FILES_MAX,
            max_maps_bytes: QUARANTINE_HOLDER_MAPS_MAX_BYTES,
            max_maps_records: QUARANTINE_HOLDER_MAPS_MAX_RECORDS,
            max_mountinfo_bytes: QUARANTINE_HOLDER_MOUNTINFO_MAX_BYTES,
            max_mountinfo_records: QUARANTINE_HOLDER_MOUNTINFO_MAX_RECORDS,
        }
    }
}

#[cfg(test)]
thread_local! {
    static QUARANTINE_HOLDER_BETWEEN_PASSES_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
    static QUARANTINE_HOLDER_FD_BEFORE_READ_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
    static QUARANTINE_HOLDER_FD_REVALIDATE_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
    static QUARANTINE_HOLDER_FD_REVALIDATE_BETWEEN_READS_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
    static QUARANTINE_HOLDER_FD_FINAL_CONFIRM_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
    static QUARANTINE_HOLDER_MOUNT_NAMESPACE_REVALIDATE_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
}

// --- Derived teardown budget ---------------------------------------------
//
// Callers that need to *wait out* a teardown (rather than perform one) should
// budget against these, never against the raw graces above. Deriving them here
// is deliberate: the original `continue_session` race came from a deadline that
// was hand-computed from the wrong constants and then drifted.

/// Worst-case wall-clock of ONE [`settle_process_ownership`] attempt on the
/// interrupt-driven teardown path.
///
/// Derivation — both bounded waits in a single attempt take `interrupt_grace`
/// (= [`TEARDOWN_KILL_GRACE`]): the pre-escalation `wait_for_exit` and the
/// post-SIGKILL `wait_for_taken_process_exit`. Each checks the deadline
/// *before* sleeping, so each can overshoot by just under one
/// [`TEARDOWN_KILL_POLL`] tick.
///
/// [`TERMINAL_DRAIN_GRACE`] and [`TERMINAL_STREAM_CLOSE_GRACE`] deliberately do
/// NOT appear here: on an interrupt the monitor's `stop_rx` arm clears the
/// terminal drain deadline and sets `abandon_open_producer_on_stop`, which
/// short-circuits the stream-close wait. They apply only to *natural*
/// termination.
pub(super) const SETTLEMENT_ATTEMPT_WORST_CASE: Duration = Duration::from_millis(
    2 * (TEARDOWN_KILL_GRACE.as_millis() as u64 + TEARDOWN_KILL_POLL.as_millis() as u64),
);

/// The monitor restarts a settlement task that died with a `JoinError` exactly
/// once before collapsing the outcome to `EscalationFailed`, so a teardown can
/// span at most this many settlement attempts.
pub(super) const MAX_SETTLEMENT_ATTEMPTS: u64 = 2;

/// Worst-case wall-clock from SIGINT to the terminal `SessionStatusChanged`
/// that `continue_session`'s interrupt-then-wait branch blocks on.
///
/// Any RPC deadline covering that wait MUST exceed this with margin, or a
/// *correct but slow* teardown is reported to the user as a failure. This is
/// the single value to budget against; it moves automatically if the graces or
/// the retry bound change.
pub(super) const TEARDOWN_TERMINAL_WORST_CASE: Duration = Duration::from_millis(
    SETTLEMENT_ATTEMPT_WORST_CASE.as_millis() as u64 * MAX_SETTLEMENT_ATTEMPTS,
);

/// Outcome of a teardown kill-verify. `Debug` for structured `tracing`.
#[cfg(test)]
#[derive(Debug, PartialEq, Eq)]
pub(super) enum ReapOutcome {
    /// No process handle present (task-based provider, or already dropped).
    NoHandle,
    /// Died from the earlier SIGINT within the grace window — no SIGKILL sent.
    /// The normal, common path.
    ExitedGracefully,
    /// Survived the grace window — SIGKILL + reap succeeded.
    Escalated,
    /// Survived the grace window and the SIGKILL escalation returned an error.
    EscalationFailed,
}

enum ProcessProbe {
    GenerationChanged,
    NoHandle,
    Alive,
    Dead,
}

async fn probe_process(
    active: &RwLock<HashMap<Uuid, TrackedSession>>,
    session_id: Uuid,
    expected_generation: Option<u64>,
) -> ProcessProbe {
    let mut guard = active.write().await;
    let Some(tracked) = guard.get_mut(&session_id) else {
        return ProcessProbe::GenerationChanged;
    };
    if expected_generation.is_some_and(|expected| tracked.spawn_generation != expected) {
        return ProcessProbe::GenerationChanged;
    }
    let Some(process) = tracked.process.as_mut() else {
        return ProcessProbe::NoHandle;
    };
    if process.is_alive() {
        ProcessProbe::Alive
    } else {
        tracked.exit_code = process.try_exit_status();
        ProcessProbe::Dead
    }
}

async fn wait_for_exit(
    active: &RwLock<HashMap<Uuid, TrackedSession>>,
    session_id: Uuid,
    expected_generation: Option<u64>,
    grace: Duration,
    poll: Duration,
) -> ProcessProbe {
    let deadline = Instant::now() + grace;
    loop {
        let probe = probe_process(active, session_id, expected_generation).await;
        if !matches!(probe, ProcessProbe::Alive) || Instant::now() >= deadline {
            return probe;
        }
        tokio::time::sleep(poll).await;
    }
}

/// Take the exact generation's process handle for an awaited escalation.
///
/// `ProviderProcess::kill` must remain async for subprocess reaping, so the
/// handle is temporarily owned by the settlement task rather than holding the
/// daemon-wide active-session map lock across that await.  A matching active
/// entry remains in place throughout; it is restored before the task reports
/// an outcome.
async fn take_process_for_escalation(
    active: &RwLock<HashMap<Uuid, TrackedSession>>,
    session_id: Uuid,
    expected_generation: u64,
) -> std::result::Result<super::types::ProviderProcess, ProcessSettlementOutcome> {
    let mut guard = active.write().await;
    let Some(tracked) = guard.get_mut(&session_id) else {
        return Err(ProcessSettlementOutcome::GenerationChanged);
    };
    if tracked.spawn_generation != expected_generation {
        return Err(ProcessSettlementOutcome::GenerationChanged);
    }
    tracked
        .process
        .take()
        .ok_or(ProcessSettlementOutcome::NoHandle)
}

/// Restore a process extracted for escalation and record its final exit code
/// only when the same generation still owns the map entry.  A process is never
/// reported settled merely because `kill()` accepted the signal: task-backed
/// Local/Harness handles are observed until `is_finished()` is true as well.
async fn restore_escalated_process(
    active: &RwLock<HashMap<Uuid, TrackedSession>>,
    session_id: Uuid,
    expected_generation: u64,
    mut process: super::types::ProviderProcess,
    outcome: ProcessSettlementOutcome,
) -> ProcessSettlementOutcome {
    let exit_code = (!process.is_alive())
        .then(|| process.try_exit_status())
        .flatten();
    let mut guard = active.write().await;
    let Some(tracked) = guard.get_mut(&session_id) else {
        return ProcessSettlementOutcome::GenerationChanged;
    };
    if tracked.spawn_generation != expected_generation {
        return ProcessSettlementOutcome::GenerationChanged;
    }
    // A matching monitor is the only owner permitted to extract this handle.
    // Do not overwrite a concurrently restored handle if that invariant is
    // ever violated; retain running ownership instead.
    if tracked.process.is_some() {
        return ProcessSettlementOutcome::EscalationFailed;
    }
    tracked.exit_code = exit_code.or(tracked.exit_code);
    tracked.process = Some(process);
    outcome
}

async fn wait_for_taken_process_exit(
    process: &mut super::types::ProviderProcess,
    grace: Duration,
    poll: Duration,
) -> bool {
    let deadline = Instant::now() + grace;
    loop {
        if !process.is_alive() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(poll).await;
    }
}

/// Settle the exact tracked provider incarnation before terminal ownership is
/// removed. Natural exit, graceful interruption, escalation, and failure are
/// distinct outcomes so monitor status truth never has to infer ownership.
///
/// No map lock is held across either grace sleep. The generation is checked on
/// every probe and immediately before interrupt/escalation, preventing a stale
/// monitor from signaling a newer incarnation.
#[allow(clippy::too_many_arguments)]
pub(super) async fn settle_process_ownership(
    active: &RwLock<HashMap<Uuid, TrackedSession>>,
    session_id: Uuid,
    expected_generation: u64,
    mode: ProcessSettlementMode,
    natural_grace: Duration,
    interrupt_grace: Duration,
    poll: Duration,
) -> ProcessSettlementOutcome {
    match probe_process(active, session_id, Some(expected_generation)).await {
        ProcessProbe::GenerationChanged => return ProcessSettlementOutcome::GenerationChanged,
        ProcessProbe::NoHandle => return ProcessSettlementOutcome::NoHandle,
        ProcessProbe::Dead => return ProcessSettlementOutcome::AlreadyExited,
        ProcessProbe::Alive => {}
    }

    if matches!(mode, ProcessSettlementMode::AwaitNatural) {
        match wait_for_exit(
            active,
            session_id,
            Some(expected_generation),
            natural_grace,
            poll,
        )
        .await
        {
            ProcessProbe::GenerationChanged => {
                return ProcessSettlementOutcome::GenerationChanged;
            }
            ProcessProbe::NoHandle => return ProcessSettlementOutcome::NoHandle,
            ProcessProbe::Dead => return ProcessSettlementOutcome::ExitedNaturally,
            ProcessProbe::Alive => {}
        }
    }

    let mut interrupt_failed = false;
    if !matches!(mode, ProcessSettlementMode::AlreadyInterrupted) {
        let interrupt_result = {
            let guard = active.write().await;
            match guard.get(&session_id) {
                Some(tracked)
                    if tracked.spawn_generation == expected_generation
                        && tracked.process.is_some() =>
                {
                    tracked
                        .process
                        .as_ref()
                        .expect("checked process")
                        .interrupt()
                }
                Some(tracked) if tracked.spawn_generation != expected_generation => {
                    return ProcessSettlementOutcome::GenerationChanged;
                }
                _ => return ProcessSettlementOutcome::NoHandle,
            }
        };
        interrupt_failed = interrupt_result.is_err();
    }

    match wait_for_exit(
        active,
        session_id,
        Some(expected_generation),
        interrupt_grace,
        poll,
    )
    .await
    {
        ProcessProbe::GenerationChanged => return ProcessSettlementOutcome::GenerationChanged,
        ProcessProbe::NoHandle => return ProcessSettlementOutcome::NoHandle,
        ProcessProbe::Dead => return ProcessSettlementOutcome::ExitedAfterInterrupt,
        ProcessProbe::Alive => {}
    }

    let mut process =
        match take_process_for_escalation(active, session_id, expected_generation).await {
            Ok(process) => process,
            Err(outcome) => return outcome,
        };
    let expected_outcome = if interrupt_failed {
        ProcessSettlementOutcome::EscalatedAfterInterruptFailure
    } else {
        ProcessSettlementOutcome::Escalated
    };
    let outcome = match process.kill().await {
        Ok(()) if wait_for_taken_process_exit(&mut process, interrupt_grace, poll).await => {
            expected_outcome
        }
        Ok(()) | Err(_) => ProcessSettlementOutcome::EscalationFailed,
    };
    restore_escalated_process(active, session_id, expected_generation, process, outcome).await
}

/// Verify a torn-down session's provider process is dead; if it survived the
/// SIGINT grace, escalate to SIGKILL (`ProviderProcess::kill` = tokio
/// `child.kill().await` = SIGKILL + reap, which terminates even a SIGSTOP-frozen
/// task).
///
/// The `active` write lock is held only for instantaneous probes and for the
/// take/restore boundaries.  The async kill/reap and task-finish verification
/// run under a settlement-task-owned handle, never under the daemon-wide map
/// lock.
///
/// Targets the single tracked handle keyed by `session_id`; it can neither touch
/// another session (different key) nor the daemon (not in the map). This session
/// is mid-finalize, so its slot holds exactly the outgoing handle until finalize
/// removes it — a concurrently spawned new child for this id cannot exist here.
#[cfg(test)]
pub(super) async fn ensure_process_dead(
    active: &RwLock<HashMap<Uuid, TrackedSession>>,
    session_id: Uuid,
    grace: Duration,
    poll: Duration,
) -> ReapOutcome {
    let expected_generation = active
        .read()
        .await
        .get(&session_id)
        .map(|tracked| tracked.spawn_generation)
        .unwrap_or_default();
    match settle_process_ownership(
        active,
        session_id,
        expected_generation,
        ProcessSettlementMode::AlreadyInterrupted,
        Duration::ZERO,
        grace,
        poll,
    )
    .await
    {
        ProcessSettlementOutcome::NoHandle => ReapOutcome::NoHandle,
        ProcessSettlementOutcome::AlreadyExited
        | ProcessSettlementOutcome::ExitedNaturally
        | ProcessSettlementOutcome::ExitedAfterInterrupt => ReapOutcome::ExitedGracefully,
        ProcessSettlementOutcome::Escalated
        | ProcessSettlementOutcome::EscalatedAfterInterruptFailure => ReapOutcome::Escalated,
        ProcessSettlementOutcome::GenerationChanged
        | ProcessSettlementOutcome::EscalationFailed => ReapOutcome::EscalationFailed,
    }
}

/// Reap one Session's exact daemon-owned process cohort to a two-empty-pass
/// fixed point. Linux uses the same bounded inventory, full-tuple reproof,
/// start-time proof, and pidfd signal primitive as startup recovery. Other Unix
/// platforms intentionally no-op because they do not expose Linux `/proc` and
/// pidfds; lack of those facilities must not prevent the daemon from booting.
pub fn reap_orphans_for_session(session_id: Uuid) -> crate::error::Result<usize> {
    #[cfg(test)]
    take_runtime_orphan_reap_failure(session_id)?;
    #[cfg(not(target_os = "linux"))]
    {
        let _ = session_id;
        return Ok(0);
    }
    #[cfg(all(test, target_os = "linux"))]
    if let Some(lease) = take_runtime_reap_proc_root(session_id) {
        return reap_orphans_for_session_with_runtime_reap_proc_root_lease(session_id, lease);
    }
    #[cfg(target_os = "linux")]
    reap_startup_owned_orphans_checked(&StartupProcessOwnership::for_sessions(HashSet::from([
        session_id,
    ])))
}

#[cfg(all(test, target_os = "linux"))]
pub(super) fn prepare_runtime_reap_proc_root_operation(
    session_id: Uuid,
) -> crate::error::Result<Option<Box<dyn FnOnce() -> crate::error::Result<usize> + Send>>> {
    // This must precede permit consumption: an injected failure leaves the
    // guard pending so normal Drop removes it without changing test custody.
    take_runtime_orphan_reap_failure(session_id)?;
    Ok(take_runtime_reap_proc_root(session_id).map(|lease| {
        Box::new(move || {
            reap_orphans_for_session_with_runtime_reap_proc_root_lease(session_id, lease)
        }) as Box<dyn FnOnce() -> crate::error::Result<usize> + Send>
    }))
}

#[cfg(all(test, target_os = "linux"))]
fn reap_orphans_for_session_with_runtime_reap_proc_root_lease(
    session_id: Uuid,
    lease: RuntimeReapProcRootLease,
) -> crate::error::Result<usize> {
    reap_startup_owned_orphans_at(
        &StartupProcessOwnership::for_sessions(HashSet::from([session_id])),
        lease.proc_root(),
        None,
    )
}

#[cfg(test)]
static RUNTIME_REAP_FAILURE: std::sync::LazyLock<std::sync::Mutex<Option<Uuid>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(None));

#[cfg(test)]
fn take_runtime_orphan_reap_failure(session_id: Uuid) -> crate::error::Result<()> {
    let mut guard = RUNTIME_REAP_FAILURE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if *guard == Some(session_id) {
        *guard = None;
        return Err(crate::error::DaemonError::Process(
            "injected runtime orphan reap failure".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn fail_runtime_orphan_reap_for_test(session_id: Uuid) {
    *RUNTIME_REAP_FAILURE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(session_id);
}

#[cfg(test)]
static CAPACITY_REAP_FAILURE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
pub(super) fn fail_next_capacity_orphan_reap() {
    CAPACITY_REAP_FAILURE.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// Proof-bearing capacity-only orphan exclusion. Unlike ordinary resume's
/// best-effort cleanup, any enumeration, signal, or post-kill verification
/// failure refuses the provider boundary.
pub(super) fn reap_capacity_orphans_checked(session_id: Uuid) -> crate::error::Result<usize> {
    #[cfg(test)]
    if CAPACITY_REAP_FAILURE.swap(false, std::sync::atomic::Ordering::SeqCst) {
        return Err(crate::error::DaemonError::Process(
            "injected capacity orphan reap failure".into(),
        ));
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = session_id;
        return Ok(0);
    }
    #[cfg(target_os = "linux")]
    reap_startup_owned_orphans_checked(&StartupProcessOwnership::for_sessions(HashSet::from([
        session_id,
    ])))
}

#[cfg(test)]
static STARTUP_PROVIDER_REAP_FAILURE: std::sync::Mutex<Option<Uuid>> = std::sync::Mutex::new(None);
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum StartupSettlementFaultKind {
    Reap,
    ScanProof,
}

#[cfg(test)]
#[derive(Default)]
struct StartupSettlementFaultRegistry {
    next_token: u64,
    pending: HashMap<(StartupSettlementFaultKind, Uuid), Vec<u64>>,
}

#[cfg(test)]
static STARTUP_SETTLEMENT_FAULTS: std::sync::LazyLock<
    std::sync::Mutex<StartupSettlementFaultRegistry>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(StartupSettlementFaultRegistry::default()));

#[cfg(test)]
pub(super) struct StartupSettlementFaultGuard {
    kind: StartupSettlementFaultKind,
    candidate_id: Uuid,
    token: u64,
}

#[cfg(test)]
impl Drop for StartupSettlementFaultGuard {
    fn drop(&mut self) {
        let mut registry = STARTUP_SETTLEMENT_FAULTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let key = (self.kind, self.candidate_id);
        if let Some(tokens) = registry.pending.get_mut(&key)
            && let Some(index) = tokens.iter().position(|token| *token == self.token)
        {
            tokens.remove(index);
            if tokens.is_empty() {
                registry.pending.remove(&key);
            }
        }
    }
}

#[cfg(test)]
pub(super) fn fail_startup_provider_orphan_reap_for(session_id: Uuid) {
    *STARTUP_PROVIDER_REAP_FAILURE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(session_id);
}

#[cfg(test)]
fn scoped_startup_settlement_fault(
    kind: StartupSettlementFaultKind,
    candidate_id: Uuid,
) -> StartupSettlementFaultGuard {
    let mut registry = STARTUP_SETTLEMENT_FAULTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    registry.next_token = registry.next_token.wrapping_add(1);
    let token = registry.next_token;
    registry
        .pending
        .entry((kind, candidate_id))
        .or_default()
        .push(token);
    StartupSettlementFaultGuard {
        kind,
        candidate_id,
        token,
    }
}

#[cfg(test)]
pub(super) fn scoped_startup_settlement_reap_failure(
    candidate_id: Uuid,
) -> StartupSettlementFaultGuard {
    scoped_startup_settlement_fault(StartupSettlementFaultKind::Reap, candidate_id)
}

#[cfg(test)]
pub(super) fn scoped_startup_settlement_scan_proof_failure(
    candidate_id: Uuid,
) -> StartupSettlementFaultGuard {
    scoped_startup_settlement_fault(StartupSettlementFaultKind::ScanProof, candidate_id)
}

type StartupAfterScanHook = Option<Box<dyn FnOnce()>>;

#[cfg(test)]
pub(super) struct StartupReaperChild {
    child: Option<std::process::Child>,
    proc_entry: Option<PathBuf>,
}

#[cfg(test)]
impl StartupReaperChild {
    pub(super) fn unregistered(child: std::process::Child) -> Self {
        Self {
            child: Some(child),
            proc_entry: None,
        }
    }

    pub(super) fn pid(&self) -> i32 {
        self.child
            .as_ref()
            .expect("startup fixture child owner")
            .id() as i32
    }

    fn register_proc_entry(&mut self, proc_entry: PathBuf) {
        assert!(self.proc_entry.replace(proc_entry).is_none());
    }

    pub(super) fn wait_for_environment(&self, needle: &str) {
        wait_for_process_environment_until(
            self,
            needle,
            StdInstant::now() + Duration::from_secs(2),
        )
        .unwrap_or_else(|error| panic!("{error}"));
    }

    pub(super) fn wait_signalled(&mut self, label: &str) {
        let status = self
            .child
            .take()
            .expect("startup fixture child owner")
            .wait()
            .expect("wait startup fixture child");
        self.remove_entry();
        assert!(status.code().is_none(), "{label}: expected signal exit");
    }

    pub(super) fn is_alive(&mut self, label: &str) -> bool {
        self.child
            .as_mut()
            .expect("startup fixture child owner")
            .try_wait()
            .unwrap_or_else(|error| panic!("{label}: probe startup fixture child: {error}"))
            .is_none()
    }

    pub(super) fn assert_alive(&mut self, label: &str) {
        assert!(self.is_alive(label), "{label}: child unexpectedly exited");
    }

    fn remove_entry(&mut self) {
        if let Some(entry) = self.proc_entry.take() {
            let _ = std::fs::remove_dir_all(entry);
        }
    }
}

#[cfg(test)]
fn wait_for_process_environment_until(
    child: &StartupReaperChild,
    needle: &str,
    deadline: StdInstant,
) -> Result<(), String> {
    let path = format!("/proc/{}/environ", child.pid());
    loop {
        if std::fs::read(&path).is_ok_and(|environment| {
            environment
                .split(|byte| *byte == 0)
                .any(|token| token == needle.as_bytes())
        }) {
            return Ok(());
        }
        if StdInstant::now() >= deadline {
            return Err(format!("late process environment never exposed {needle}"));
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[cfg(test)]
impl Drop for StartupReaperChild {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.remove_entry();
    }
}

#[cfg(all(test, target_os = "linux"))]
#[derive(Clone)]
pub(super) struct StartupReaperFixture {
    inner: std::sync::Arc<StartupReaperFixtureInner>,
}

#[cfg(all(test, target_os = "linux"))]
struct StartupReaperFixtureInner {
    _directory: tempfile::TempDir,
    proc_root: PathBuf,
    domain: StartupProcessDomain,
}

#[cfg(all(test, target_os = "linux"))]
#[derive(Clone, Copy)]
enum StartupReaperFixtureRegistrationFault {
    AfterStatLink,
}

#[cfg(all(test, target_os = "linux"))]
struct StartupReaperFixtureSpawnError {
    message: String,
    pid: Option<i32>,
    entry: Option<PathBuf>,
}

#[cfg(all(test, target_os = "linux"))]
impl StartupReaperFixture {
    pub(super) fn new() -> Self {
        let directory = tempfile::tempdir().expect("startup reaper fixture directory");
        let proc_root = directory.path().join("proc");
        std::fs::create_dir_all(proc_root.join("self")).expect("fixture proc self");
        let socket = directory.path().join("daemon.sock");
        Self {
            inner: std::sync::Arc::new(StartupReaperFixtureInner {
                _directory: directory,
                proc_root,
                domain: StartupProcessDomain::for_socket(&socket),
            }),
        }
    }

    pub(super) fn proc_root(&self) -> &Path {
        &self.inner.proc_root
    }

    fn domain(&self) -> StartupProcessDomain {
        self.inner.domain.clone()
    }

    fn spawn(
        &self,
        session_id: Option<Uuid>,
        invocation_id: Option<Uuid>,
        domain: &StartupProcessDomain,
        namespaced: bool,
    ) -> StartupReaperChild {
        self.spawn_with_options(
            session_id,
            invocation_id,
            domain,
            namespaced,
            StdInstant::now() + Duration::from_secs(2),
            None,
        )
        .unwrap_or_else(|error| panic!("startup fixture child setup failed: {}", error.message))
    }

    pub(super) fn spawn_runtime_session(&self, session_id: Uuid) -> StartupReaperChild {
        self.spawn(
            Some(session_id),
            None,
            &StartupProcessDomain::default_domain(),
            true,
        )
    }

    pub(super) fn spawn_unreadable_runtime_session(&self, session_id: Uuid) -> StartupReaperChild {
        let child = self.spawn_runtime_session(session_id);
        let environ = self
            .proc_root()
            .join(child.pid().to_string())
            .join("environ");
        std::fs::remove_file(&environ).expect("remove fixture environ symlink");
        std::fs::write(&environ, b"unreadable fixture environment")
            .expect("write fixture unreadable environ");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&environ, std::fs::Permissions::from_mode(0o000))
            .expect("make fixture environ unreadable");
        child
    }

    pub(super) fn scoped_runtime_reap_root(
        &self,
        session_id: Uuid,
    ) -> Result<RuntimeReapProcRootGuard, String> {
        register_runtime_reap_proc_root(session_id, std::sync::Arc::clone(&self.inner))
    }

    fn spawn_with_options(
        &self,
        session_id: Option<Uuid>,
        invocation_id: Option<Uuid>,
        domain: &StartupProcessDomain,
        namespaced: bool,
        deadline: StdInstant,
        registration_fault: Option<StartupReaperFixtureRegistrationFault>,
    ) -> Result<StartupReaperChild, StartupReaperFixtureSpawnError> {
        let mut command = std::process::Command::new("sleep");
        command
            .arg("30")
            .env_remove(rsi_common::identity::ENV_SESSION_ID)
            .env_remove(rsi_common::identity::ENV_MODEL_INVOCATION_ID)
            .env_remove(rsi_common::identity::ENV_PROCESS_OWNERSHIP_NAMESPACE)
            .env_remove(rsi_common::identity::ENV_SOCKET);
        if let Some(session_id) = session_id {
            command.env(rsi_common::identity::ENV_SESSION_ID, session_id.to_string());
        }
        if let Some(invocation_id) = invocation_id {
            command.env(
                rsi_common::identity::ENV_MODEL_INVOCATION_ID,
                invocation_id.to_string(),
            );
        }
        if namespaced {
            command.env(
                rsi_common::identity::ENV_PROCESS_OWNERSHIP_NAMESPACE,
                OsString::from_vec(domain.namespace.clone()),
            );
        }
        command.env(
            rsi_common::identity::ENV_SOCKET,
            OsString::from_vec(domain.legacy_socket.clone()),
        );
        let mut child = StartupReaperChild::unregistered(command.spawn().map_err(|error| {
            StartupReaperFixtureSpawnError {
                message: format!("spawn startup fixture child: {error}"),
                pid: None,
                entry: None,
            }
        })?);
        wait_for_exact_process_environment_until(
            &child,
            session_id,
            invocation_id,
            domain,
            namespaced,
            deadline,
        )
        .map_err(|message| StartupReaperFixtureSpawnError {
            message,
            pid: Some(child.pid()),
            entry: None,
        })?;
        let pid = child.pid();
        let entry = self.inner.proc_root.join(pid.to_string());
        child.register_proc_entry(entry.clone());
        std::fs::create_dir(&entry).map_err(|error| StartupReaperFixtureSpawnError {
            message: format!("create fixture process entry: {error}"),
            pid: Some(pid),
            entry: Some(entry.clone()),
        })?;
        std::os::unix::fs::symlink(
            Path::new("/proc").join(pid.to_string()).join("stat"),
            entry.join("stat"),
        )
        .map_err(|error| StartupReaperFixtureSpawnError {
            message: format!("link live fixture stat: {error}"),
            pid: Some(pid),
            entry: Some(entry.clone()),
        })?;
        if matches!(
            registration_fault,
            Some(StartupReaperFixtureRegistrationFault::AfterStatLink)
        ) {
            return Err(StartupReaperFixtureSpawnError {
                message: "injected fixture registration failure after stat link".into(),
                pid: Some(pid),
                entry: Some(entry.clone()),
            });
        }
        std::os::unix::fs::symlink(
            Path::new("/proc").join(pid.to_string()).join("environ"),
            entry.join("environ"),
        )
        .map_err(|error| StartupReaperFixtureSpawnError {
            message: format!("link live fixture environ: {error}"),
            pid: Some(pid),
            entry: Some(entry),
        })?;
        Ok(child)
    }
}

#[cfg(all(test, target_os = "linux"))]
fn wait_for_exact_process_environment_until(
    child: &StartupReaperChild,
    session_id: Option<Uuid>,
    invocation_id: Option<Uuid>,
    domain: &StartupProcessDomain,
    namespaced: bool,
    deadline: StdInstant,
) -> Result<(), String> {
    let path = format!("/proc/{}/environ", child.pid());
    loop {
        if StdInstant::now() >= deadline {
            return Err("exact process environment never became ready".into());
        }
        let matches = std::fs::read(&path)
            .ok()
            .and_then(|environment| parse_startup_stamp_observation(&environment).ok())
            .is_some_and(|stamps| {
                let raw_matches = |field: &StartupObservedRawField, expected: Option<&[u8]>| {
                    field.token_count == usize::from(expected.is_some())
                        && field.values.len() == usize::from(expected.is_some())
                        && field.values.first().map(Vec::as_slice) == expected
                };
                let stamp_matches = |field: &StartupObservedStampField, expected: Option<Uuid>| {
                    !field.malformed
                        && field.token_count == usize::from(expected.is_some())
                        && field.canonical_values.as_slice()
                            == expected.as_ref().map(std::slice::from_ref).unwrap_or(&[])
                };
                raw_matches(
                    &stamps.namespace,
                    namespaced.then_some(domain.namespace.as_slice()),
                ) && raw_matches(&stamps.socket, Some(domain.legacy_socket.as_slice()))
                    && stamp_matches(&stamps.session, session_id)
                    && stamp_matches(&stamps.invocation, invocation_id)
            });
        if matches {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[cfg(all(test, target_os = "linux"))]
#[derive(Clone, Copy, PartialEq, Eq)]
enum RuntimeReapProcRootEntryState {
    Pending,
    Leased,
}

#[cfg(all(test, target_os = "linux"))]
struct RuntimeReapProcRootEntry {
    token: u64,
    fixture: std::sync::Arc<StartupReaperFixtureInner>,
    state: RuntimeReapProcRootEntryState,
}

#[cfg(all(test, target_os = "linux"))]
struct RuntimeReapProcRootRegistry {
    next_token: u64,
    entries: HashMap<Uuid, Vec<RuntimeReapProcRootEntry>>,
}

#[cfg(all(test, target_os = "linux"))]
static RUNTIME_REAP_PROC_ROOTS: std::sync::LazyLock<std::sync::Mutex<RuntimeReapProcRootRegistry>> =
    std::sync::LazyLock::new(|| {
        std::sync::Mutex::new(RuntimeReapProcRootRegistry {
            next_token: 0,
            entries: HashMap::new(),
        })
    });

#[cfg(all(test, target_os = "linux"))]
pub(super) struct RuntimeReapProcRootGuard {
    session_id: Uuid,
    token: u64,
    fixture: std::sync::Arc<StartupReaperFixtureInner>,
}

#[cfg(all(test, target_os = "linux"))]
struct RuntimeReapProcRootLease {
    session_id: Uuid,
    token: u64,
    fixture: std::sync::Arc<StartupReaperFixtureInner>,
}

#[cfg(all(test, target_os = "linux"))]
fn register_runtime_reap_proc_root(
    session_id: Uuid,
    fixture: std::sync::Arc<StartupReaperFixtureInner>,
) -> Result<RuntimeReapProcRootGuard, String> {
    let mut registry = RUNTIME_REAP_PROC_ROOTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if registry.entries.get(&session_id).is_some_and(|entries| {
        entries
            .iter()
            .any(|entry| !std::sync::Arc::ptr_eq(&entry.fixture, &fixture))
    }) {
        return Err("runtime reap proc-root collision for session".into());
    }
    let token = registry
        .next_token
        .checked_add(1)
        .ok_or_else(|| "runtime reap proc-root token counter exhausted".to_string())?;
    registry.next_token = token;
    registry
        .entries
        .entry(session_id)
        .or_default()
        .push(RuntimeReapProcRootEntry {
            token,
            fixture: std::sync::Arc::clone(&fixture),
            state: RuntimeReapProcRootEntryState::Pending,
        });
    Ok(RuntimeReapProcRootGuard {
        session_id,
        token,
        fixture,
    })
}

#[cfg(all(test, target_os = "linux"))]
fn take_runtime_reap_proc_root(session_id: Uuid) -> Option<RuntimeReapProcRootLease> {
    let mut registry = RUNTIME_REAP_PROC_ROOTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let entry = registry
        .entries
        .get_mut(&session_id)?
        .iter_mut()
        .rev()
        .find(|entry| entry.state == RuntimeReapProcRootEntryState::Pending)?;
    entry.state = RuntimeReapProcRootEntryState::Leased;
    Some(RuntimeReapProcRootLease {
        session_id,
        token: entry.token,
        fixture: std::sync::Arc::clone(&entry.fixture),
    })
}

#[cfg(all(test, target_os = "linux"))]
fn remove_runtime_reap_proc_root_entry(
    session_id: Uuid,
    token: u64,
    state: RuntimeReapProcRootEntryState,
) {
    let mut registry = RUNTIME_REAP_PROC_ROOTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(entries) = registry.entries.get_mut(&session_id) else {
        return;
    };
    if let Some(index) = entries
        .iter()
        .position(|entry| entry.token == token && entry.state == state)
    {
        entries.remove(index);
    }
    if entries.is_empty() {
        registry.entries.remove(&session_id);
    }
}

#[cfg(all(test, target_os = "linux"))]
impl RuntimeReapProcRootLease {
    fn proc_root(&self) -> &Path {
        &self.fixture.proc_root
    }
}

#[cfg(all(test, target_os = "linux"))]
impl Drop for RuntimeReapProcRootGuard {
    fn drop(&mut self) {
        let _ = &self.fixture;
        remove_runtime_reap_proc_root_entry(
            self.session_id,
            self.token,
            RuntimeReapProcRootEntryState::Pending,
        );
    }
}

#[cfg(all(test, target_os = "linux"))]
impl Drop for RuntimeReapProcRootLease {
    fn drop(&mut self) {
        remove_runtime_reap_proc_root_entry(
            self.session_id,
            self.token,
            RuntimeReapProcRootEntryState::Leased,
        );
    }
}

fn run_startup_provider_after_scan_hook(hook: &mut StartupAfterScanHook) -> bool {
    if let Some(hook) = hook.take() {
        hook();
        true
    } else {
        false
    }
}

#[cfg(not(test))]
fn take_startup_settlement_fault(_: (), _: &[Uuid]) -> bool {
    false
}

#[cfg(test)]
fn take_startup_settlement_fault(kind: StartupSettlementFaultKind, candidate_ids: &[Uuid]) -> bool {
    let mut registry = STARTUP_SETTLEMENT_FAULTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for candidate_id in candidate_ids {
        let key = (kind, *candidate_id);
        if let Some(tokens) = registry.pending.get_mut(&key)
            && tokens.pop().is_some()
        {
            if tokens.is_empty() {
                registry.pending.remove(&key);
            }
            return true;
        }
    }
    false
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StartupOwnershipProof {
    Namespaced,
    LegacySocketAndStoreId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct StartupExactStamp {
    proof: StartupOwnershipProof,
    namespace: Option<Vec<u8>>,
    socket: Option<Vec<u8>>,
    session_id: Option<Uuid>,
    invocation_id: Option<Uuid>,
}

#[derive(Clone, Debug, Default)]
struct StartupObservedStampField {
    token_count: usize,
    canonical_values: Vec<Uuid>,
    malformed: bool,
}

#[derive(Clone, Debug, Default)]
struct StartupObservedRawField {
    token_count: usize,
    values: Vec<Vec<u8>>,
}

#[derive(Clone, Debug, Default)]
struct StartupStampObservation {
    namespace: StartupObservedRawField,
    socket: StartupObservedRawField,
    session: StartupObservedStampField,
    invocation: StartupObservedStampField,
}

impl StartupStampObservation {
    fn token_count(&self) -> crate::error::Result<usize> {
        self.namespace
            .token_count
            .checked_add(self.socket.token_count)
            .and_then(|count| count.checked_add(self.session.token_count))
            .and_then(|count| count.checked_add(self.invocation.token_count))
            .ok_or_else(|| {
                crate::error::DaemonError::Process(
                    "startup provider stamp token count overflowed".into(),
                )
            })
    }

    fn is_empty(&self) -> bool {
        self.namespace.token_count == 0
            && self.socket.token_count == 0
            && self.session.token_count == 0
            && self.invocation.token_count == 0
    }

    fn has_owned_id(&self, ownership: &StartupProcessOwnership) -> bool {
        self.session
            .canonical_values
            .iter()
            .any(|id| ownership.session_ids.contains(id))
            || self
                .invocation
                .canonical_values
                .iter()
                .any(|id| ownership.invocation_ids.contains(id))
    }

    fn authorized_exact_stamp(
        &self,
        pid: i32,
        ownership: &StartupProcessOwnership,
    ) -> crate::error::Result<Option<StartupExactStamp>> {
        let namespace_matches = self
            .namespace
            .values
            .iter()
            .any(|value| value == &ownership.domain.namespace);
        if namespace_matches {
            let namespace = exact_startup_raw_field(
                pid,
                rsi_common::identity::ENV_PROCESS_OWNERSHIP_NAMESPACE,
                &self.namespace,
            )?;
            if namespace.as_deref() != Some(ownership.domain.namespace.as_slice()) {
                return Err(crate::error::DaemonError::Process(format!(
                    "startup provider process {pid} changed its ownership namespace"
                )));
            }
            let session_id = exact_startup_stamp_field(
                pid,
                rsi_common::identity::ENV_SESSION_ID,
                &self.session,
            )?;
            if ownership
                .namespaced_session_ids
                .as_ref()
                .is_some_and(|ids| !session_id.is_some_and(|session_id| ids.contains(&session_id)))
            {
                return Ok(None);
            }
            return Ok(Some(StartupExactStamp {
                proof: StartupOwnershipProof::Namespaced,
                namespace,
                socket: exact_startup_raw_field(
                    pid,
                    rsi_common::identity::ENV_SOCKET,
                    &self.socket,
                )?,
                session_id,
                invocation_id: exact_startup_stamp_field(
                    pid,
                    rsi_common::identity::ENV_MODEL_INVOCATION_ID,
                    &self.invocation,
                )?,
            }));
        }

        // A namespace-bearing process from another daemon domain is foreign
        // even when a copied database contains the same Session/invocation
        // UUID. Never fall back from an explicit foreign namespace to legacy
        // UUID ownership.
        if self.namespace.token_count != 0 {
            return Ok(None);
        }
        let socket_matches = self
            .socket
            .values
            .iter()
            .any(|value| value == &ownership.domain.legacy_socket);
        if !socket_matches || !self.has_owned_id(ownership) {
            return Ok(None);
        }
        let socket = exact_startup_raw_field(pid, rsi_common::identity::ENV_SOCKET, &self.socket)?;
        if socket.as_deref() != Some(ownership.domain.legacy_socket.as_slice()) {
            return Err(crate::error::DaemonError::Process(format!(
                "startup provider process {pid} changed its legacy socket identity"
            )));
        }
        Ok(Some(StartupExactStamp {
            proof: StartupOwnershipProof::LegacySocketAndStoreId,
            namespace: None,
            socket,
            session_id: exact_startup_stamp_field(
                pid,
                rsi_common::identity::ENV_SESSION_ID,
                &self.session,
            )?,
            invocation_id: exact_startup_stamp_field(
                pid,
                rsi_common::identity::ENV_MODEL_INVOCATION_ID,
                &self.invocation,
            )?,
        }))
    }
}

#[derive(Clone, Debug)]
struct StartupProcessDomain {
    namespace: Vec<u8>,
    legacy_socket: Vec<u8>,
}

impl StartupProcessDomain {
    fn for_socket(socket_path: &Path) -> Self {
        Self {
            namespace: rsi_common::identity::process_ownership_namespace_for_socket(socket_path)
                .into_bytes(),
            legacy_socket: socket_path.as_os_str().as_bytes().to_vec(),
        }
    }

    fn default_domain() -> Self {
        let socket_path = rsi_common::identity::default_socket_path();
        Self {
            namespace: rsi_common::identity::process_ownership_namespace().into_bytes(),
            legacy_socket: socket_path.as_os_str().as_bytes().to_vec(),
        }
    }
}

#[derive(Clone, Debug)]
struct StartupProcessOwnership {
    domain: StartupProcessDomain,
    /// `None` authorizes every exact namespace member at daemon boot. `Some`
    /// restricts runtime/settlement repair to exact Session stamps in the set.
    namespaced_session_ids: Option<HashSet<Uuid>>,
    session_ids: HashSet<Uuid>,
    invocation_ids: HashSet<Uuid>,
}

impl StartupProcessOwnership {
    fn is_empty(&self) -> bool {
        self.namespaced_session_ids
            .as_ref()
            .is_some_and(HashSet::is_empty)
            && self.session_ids.is_empty()
            && self.invocation_ids.is_empty()
    }

    fn for_sessions(session_ids: HashSet<Uuid>) -> Self {
        Self::for_sessions_in_domain(session_ids, StartupProcessDomain::default_domain())
    }

    fn for_sessions_in_domain(session_ids: HashSet<Uuid>, domain: StartupProcessDomain) -> Self {
        Self {
            domain,
            namespaced_session_ids: Some(session_ids.clone()),
            session_ids,
            invocation_ids: HashSet::new(),
        }
    }
}

#[derive(Clone, Debug)]
struct StartupObservedProcess {
    pid: i32,
    start_time: u64,
    stamps: StartupStampObservation,
}

#[derive(Debug)]
struct StartupProcessInventory {
    processes: Vec<StartupObservedProcess>,
    session_ids: HashSet<Uuid>,
    invocation_ids: HashSet<Uuid>,
}

#[derive(Clone, Debug)]
struct StartupOrphanIdentity {
    pid: i32,
    start_time: u64,
    stamps: StartupExactStamp,
}

/// Perform bounded, proof-bearing fixed-point `/proc` passes for the retained
/// settlement cohort. Recovery may perform Git effects only after two
/// consecutive complete empty inventories prove that every exact-stamped
/// same-UID provider is absent, PID-reused, or dead after SIGKILL.
pub(super) fn reap_startup_settlement_orphans_checked(
    candidate_ids: &[Uuid],
) -> crate::error::Result<usize> {
    #[cfg(test)]
    if take_startup_settlement_fault(StartupSettlementFaultKind::Reap, candidate_ids) {
        return Err(crate::error::DaemonError::Process(
            "injected startup settlement orphan reap failure".into(),
        ));
    }
    reap_startup_owned_orphans_checked(&StartupProcessOwnership::for_sessions(
        candidate_ids.iter().copied().collect(),
    ))
}

/// Compatibility seam for focused tests and the older session-only recovery
/// path. Daemon boot uses [`reap_startup_process_ownership_checked`] so process
/// discovery precedes Store authorization and includes invocation-only work.
pub(super) fn reap_startup_provider_orphans_checked(
    candidate_ids: &[Uuid],
) -> crate::error::Result<usize> {
    #[cfg(test)]
    {
        let mut guard = STARTUP_PROVIDER_REAP_FAILURE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard.is_some_and(|session_id| candidate_ids.contains(&session_id)) {
            *guard = None;
            return Err(crate::error::DaemonError::Process(
                "injected startup provider orphan inventory failure".into(),
            ));
        }
    }
    reap_startup_owned_orphans_checked(&StartupProcessOwnership::for_sessions(
        candidate_ids.iter().copied().collect(),
    ))
}

/// Process-first startup ownership repair. Every bounded fixed-point pass
/// inventories process-carried identity before accepting this daemon's exact
/// socket/database namespace. Namespace-less legacy tuples are authorized by
/// one deadline-bounded Store snapshot and additionally require the exact
/// legacy socket. This catches late provider/tool children without allowing a
/// copied database's overlapping UUIDs to cross daemon domains. The complete
/// observed tuple is pinned for the final start-time + pidfd reproof.
#[doc(hidden)]
pub fn reap_startup_process_ownership_checked(
    store: &crate::store::Store,
) -> crate::error::Result<usize> {
    #[cfg(not(target_os = "linux"))]
    {
        // macOS and other Unix targets have neither Linux `/proc` process
        // environments nor pidfds. Startup repair is therefore unavailable,
        // but the daemon remains usable and performs no numeric-PID fallback.
        let _ = store;
        return Ok(0);
    }
    #[cfg(target_os = "linux")]
    reap_startup_process_ownership_inner(store).map_err(|error| match error {
        error @ crate::error::DaemonError::StartupProviderInventory(_) => error,
        error => crate::error::DaemonError::StartupProviderInventory(error.to_string()),
    })
}

fn reap_startup_process_ownership_inner(
    store: &crate::store::Store,
) -> crate::error::Result<usize> {
    reap_startup_process_ownership_for_domain_at(
        store,
        StartupProcessDomain::default_domain(),
        Path::new("/proc"),
        None,
    )
}

fn reap_startup_process_ownership_for_domain_at(
    store: &crate::store::Store,
    domain: StartupProcessDomain,
    proc_root: &Path,
    mut after_scan_hook: StartupAfterScanHook,
) -> crate::error::Result<usize> {
    let self_pid = std::process::id() as i32;
    let self_uid = std::fs::metadata(proc_root.join("self"))
        .map_err(|error| {
            crate::error::DaemonError::Process(format!(
                "startup provider inventory self identity failed: {error}"
            ))
        })?
        .uid();
    let deadline = StdInstant::now() + STARTUP_ORPHAN_TOTAL_DEADLINE;
    let mut reaped = 0_usize;
    let mut consecutive_empty_passes = 0_u8;
    for _ in 0..STARTUP_ORPHAN_FIXED_POINT_MAX_PASSES {
        if StdInstant::now() >= deadline {
            return Err(crate::error::DaemonError::Process(
                "startup provider inventory exceeded its deadline".into(),
            ));
        }
        let inventory = scan_startup_process_inventory(proc_root, self_pid, self_uid, deadline)?;
        let mut observed_session_ids = inventory.session_ids.into_iter().collect::<Vec<_>>();
        let mut observed_invocation_ids = inventory.invocation_ids.into_iter().collect::<Vec<_>>();
        observed_session_ids.sort_unstable();
        observed_invocation_ids.sort_unstable();
        let (session_ids, invocation_ids) = store.authorize_startup_process_ids(
            &observed_session_ids,
            &observed_invocation_ids,
            deadline,
        )?;
        if StdInstant::now() >= deadline {
            return Err(crate::error::DaemonError::Process(
                "startup provider ownership lookup exceeded its deadline".into(),
            ));
        }
        let ownership = StartupProcessOwnership {
            domain: domain.clone(),
            namespaced_session_ids: None,
            session_ids,
            invocation_ids,
        };
        let mut observed = Vec::new();
        for process in inventory.processes {
            if let Some(stamps) = process
                .stamps
                .authorized_exact_stamp(process.pid, &ownership)?
            {
                observed.push(StartupOrphanIdentity {
                    pid: process.pid,
                    start_time: process.start_time,
                    stamps,
                });
            }
        }

        #[cfg(test)]
        {
            let mut guard = STARTUP_PROVIDER_REAP_FAILURE
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if guard.is_some_and(|session_id| ownership.session_ids.contains(&session_id)) {
                *guard = None;
                return Err(crate::error::DaemonError::Process(
                    "injected startup provider orphan inventory failure".into(),
                ));
            }
        }

        let test_inventory_changed = run_startup_provider_after_scan_hook(&mut after_scan_hook);
        if observed.is_empty() {
            consecutive_empty_passes = if test_inventory_changed {
                0
            } else {
                consecutive_empty_passes.saturating_add(1)
            };
            if consecutive_empty_passes >= 2 {
                return Ok(reaped);
            }
            continue;
        }
        consecutive_empty_passes = 0;
        reaped = reaped
            .checked_add(kill_startup_provider_orphans(
                proc_root, observed, &ownership, deadline,
            )?)
            .ok_or_else(|| {
                crate::error::DaemonError::Process(
                    "startup provider orphan reap count overflowed".into(),
                )
            })?;
    }
    Err(crate::error::DaemonError::Process(
        "startup provider inventory did not reach a fixed point".into(),
    ))
}

fn reap_startup_owned_orphans_checked(
    ownership: &StartupProcessOwnership,
) -> crate::error::Result<usize> {
    reap_startup_owned_orphans_at(ownership, Path::new("/proc"), None)
}

fn reap_startup_owned_orphans_at(
    ownership: &StartupProcessOwnership,
    proc_root: &Path,
    mut after_scan_hook: StartupAfterScanHook,
) -> crate::error::Result<usize> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = ownership;
        return Ok(0);
    }
    if ownership.session_ids.len() > crate::store::STARTUP_PROVIDER_CANDIDATE_MAX
        || ownership.invocation_ids.len() > crate::store::STARTUP_PROVIDER_CANDIDATE_MAX
    {
        return Err(crate::error::DaemonError::Process(format!(
            "startup provider candidate bound exceeded ({})",
            crate::store::STARTUP_PROVIDER_CANDIDATE_MAX
        )));
    }
    if ownership.is_empty() {
        return Ok(0);
    }

    let self_pid = std::process::id() as i32;
    let self_uid = std::fs::metadata(proc_root.join("self"))
        .map_err(|error| {
            crate::error::DaemonError::Process(format!(
                "startup provider inventory self identity failed: {error}"
            ))
        })?
        .uid();
    let deadline = StdInstant::now() + STARTUP_ORPHAN_TOTAL_DEADLINE;
    let mut reaped = 0_usize;
    let mut consecutive_empty_passes = 0_u8;
    for _ in 0..STARTUP_ORPHAN_FIXED_POINT_MAX_PASSES {
        if StdInstant::now() >= deadline {
            return Err(crate::error::DaemonError::Process(
                "startup provider inventory exceeded its deadline".into(),
            ));
        }
        let inventory = scan_startup_process_inventory(proc_root, self_pid, self_uid, deadline)?;
        let mut observed = Vec::new();
        for process in inventory.processes {
            if let Some(stamps) = process
                .stamps
                .authorized_exact_stamp(process.pid, ownership)?
            {
                observed.push(StartupOrphanIdentity {
                    pid: process.pid,
                    start_time: process.start_time,
                    stamps,
                });
            }
        }
        let test_inventory_changed = run_startup_provider_after_scan_hook(&mut after_scan_hook);
        if observed.is_empty() {
            consecutive_empty_passes = if test_inventory_changed {
                0
            } else {
                consecutive_empty_passes.saturating_add(1)
            };
            if consecutive_empty_passes >= 2 {
                return Ok(reaped);
            }
            continue;
        }
        consecutive_empty_passes = 0;
        reaped = reaped
            .checked_add(kill_startup_provider_orphans(
                proc_root, observed, ownership, deadline,
            )?)
            .ok_or_else(|| {
                crate::error::DaemonError::Process(
                    "startup provider orphan reap count overflowed".into(),
                )
            })?;
    }
    Err(crate::error::DaemonError::Process(
        "startup provider inventory did not reach a fixed point".into(),
    ))
}

/// Prove that the same bounded process inventory needed by the destructive
/// reaper is readable without sending a signal.  Fresh settlement uses this
/// before durable intent so an unknowable same-UID process produces zero
/// database, Git, or process effects.  The process set is re-read and killed
/// only after the journal exists, closing scan drift without an unjournaled
/// process effect.
pub(super) fn prove_startup_settlement_orphan_scan_readable(
    candidate_ids: &[Uuid],
) -> crate::error::Result<()> {
    #[cfg(test)]
    if take_startup_settlement_fault(StartupSettlementFaultKind::ScanProof, candidate_ids) {
        return Err(crate::error::DaemonError::Process(
            "injected startup settlement orphan scan proof failure".into(),
        ));
    }
    if candidate_ids.is_empty() {
        return Ok(());
    }
    #[cfg(not(target_os = "linux"))]
    {
        // No Linux process inventory exists on this target; match the startup
        // and runtime no-op rather than making the whole daemon unbootable.
        let _ = candidate_ids;
        return Ok(());
    }
    let ownership = StartupProcessOwnership::for_sessions(candidate_ids.iter().copied().collect());
    let proc_root = Path::new("/proc");
    let self_pid = std::process::id() as i32;
    let self_uid = std::fs::metadata(proc_root.join("self"))
        .map_err(|error| {
            crate::error::DaemonError::Process(format!(
                "startup settlement self identity failed: {error}"
            ))
        })?
        .uid();
    let deadline = StdInstant::now() + STARTUP_ORPHAN_TOTAL_DEADLINE;
    let inventory = scan_startup_process_inventory(proc_root, self_pid, self_uid, deadline)?;
    for process in inventory.processes {
        process
            .stamps
            .authorized_exact_stamp(process.pid, &ownership)?;
    }
    Ok(())
}

/// Read the same bounded provider inventory as startup settlement, but require
/// that no exact-stamped provider for the selected terminal Sessions exists.
/// Ordinary archive cleanup never kills a process to manufacture eligibility.
pub(super) fn prove_archive_cleanup_has_no_provider_processes(
    candidate_ids: &[Uuid],
) -> crate::error::Result<()> {
    if candidate_ids.is_empty() {
        return Ok(());
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = candidate_ids;
        return Ok(());
    }
    #[cfg(target_os = "linux")]
    {
        let ownership =
            StartupProcessOwnership::for_sessions(candidate_ids.iter().copied().collect());
        let proc_root = Path::new("/proc");
        let self_pid = std::process::id() as i32;
        let self_uid = std::fs::metadata(proc_root.join("self"))
            .map_err(|error| {
                crate::error::DaemonError::Process(format!(
                    "archive cleanup process inventory identity failed: {error}"
                ))
            })?
            .uid();
        let deadline = StdInstant::now() + STARTUP_ORPHAN_TOTAL_DEADLINE;
        let inventory = scan_startup_process_inventory(proc_root, self_pid, self_uid, deadline)?;
        for process in inventory.processes {
            if process
                .stamps
                .authorized_exact_stamp(process.pid, &ownership)?
                .is_some()
            {
                return Err(crate::error::DaemonError::Process(
                    "archive cleanup provider process is still live".into(),
                ));
            }
        }
        Ok(())
    }
}

/// Prove, without signaling anything, that no untrusted same-UID task retains
/// a path or inode inside the quarantined worktree. Two complete empty `/proc`
/// passes are required. The exact systemd user-manager and sd-pam classes may
/// carry explicit, recorded permission-only inventory exemptions; every
/// readable inventory is still scanned and any observed holder, malformed or
/// drifting inventory, work bound, or deadline failure retains the quarantine.
/// This proves absence only from the bounded kernel inventories named here,
/// not universal kernel-reference absence. Queued SCM_RIGHTS and in-flight
/// AIO/io_uring references that are not reported through those inventories are
/// accepted limitations of the non-adversarial single-user threat boundary.
pub(super) fn prove_quarantine_has_no_untrusted_same_uid_holders(
    tree: &QuarantineTreeProof,
) -> crate::error::Result<QuarantineHolderProof> {
    #[cfg(test)]
    if let Some(context) = current_quarantine_holder_test_proc_context() {
        return prove_quarantine_has_no_untrusted_same_uid_holders_at(
            &context.proc_root,
            context.uid,
            tree,
            QuarantineHolderScanLimits::default(),
        );
    }
    let proc_root = Path::new("/proc");
    let self_uid = std::fs::metadata(proc_root.join("self"))
        .map_err(|error| {
            crate::error::DaemonError::Process(format!(
                "quarantine holder self identity failed: {error}"
            ))
        })?
        .uid();
    prove_quarantine_has_no_untrusted_same_uid_holders_at(
        proc_root,
        self_uid,
        tree,
        QuarantineHolderScanLimits::default(),
    )
}

#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq)]
struct QuarantineHolderTestProcContext {
    proc_root: std::path::PathBuf,
    uid: u32,
}

#[cfg(test)]
thread_local! {
    static QUARANTINE_HOLDER_TEST_PROC_CONTEXTS: std::cell::RefCell<Vec<QuarantineHolderTestProcContext>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
fn current_quarantine_holder_test_proc_context() -> Option<QuarantineHolderTestProcContext> {
    QUARANTINE_HOLDER_TEST_PROC_CONTEXTS.with(|contexts| contexts.borrow().last().cloned())
}

#[cfg(test)]
struct QuarantineHolderTestProcContextGuard {
    depth: usize,
}

#[cfg(test)]
impl Drop for QuarantineHolderTestProcContextGuard {
    fn drop(&mut self) {
        QUARANTINE_HOLDER_TEST_PROC_CONTEXTS.with(|contexts| {
            let mut contexts = contexts.borrow_mut();
            assert_eq!(
                contexts.len(),
                self.depth,
                "test proc contexts dropped out of order"
            );
            contexts.pop();
        });
    }
}

#[cfg(test)]
pub(super) fn with_quarantine_holder_test_proc<T>(
    proc_root: &Path,
    uid: u32,
    body: impl FnOnce() -> T,
) -> T {
    let depth = QUARANTINE_HOLDER_TEST_PROC_CONTEXTS.with(|contexts| {
        let mut contexts = contexts.borrow_mut();
        contexts.push(QuarantineHolderTestProcContext {
            proc_root: proc_root.to_path_buf(),
            uid,
        });
        contexts.len()
    });
    let _guard = QuarantineHolderTestProcContextGuard { depth };
    body()
}

#[cfg(test)]
pub(super) struct SyntheticQuarantineHolderProc {
    temp_dir: tempfile::TempDir,
    uid: u32,
    namespace_root: std::path::PathBuf,
}

#[cfg(test)]
impl SyntheticQuarantineHolderProc {
    pub(super) fn new() -> Self {
        let temp_dir = tempfile::tempdir().expect("synthetic holder proc root");
        let proc_root = temp_dir.path().to_path_buf();
        let (uid, namespace_root) = initialize_fake_holder_proc(&proc_root);
        Self {
            temp_dir,
            uid,
            namespace_root,
        }
    }

    pub(super) fn proc_root(&self) -> &Path {
        self.temp_dir.path()
    }

    pub(super) const fn uid(&self) -> u32 {
        self.uid
    }

    pub(super) fn add_fd_holder(&self, path: &Path) {
        let process = write_fake_holder_process(
            self.proc_root(),
            991,
            &self.namespace_root,
            &self.namespace_root,
        );
        std::os::unix::fs::symlink(path, process.join("fd/3"))
            .expect("synthetic retained quarantine fd");
    }
}

fn prove_quarantine_has_no_untrusted_same_uid_holders_at(
    proc_root: &Path,
    self_uid: u32,
    tree: &QuarantineTreeProof,
    limits: QuarantineHolderScanLimits,
) -> crate::error::Result<QuarantineHolderProof> {
    if limits.fixed_point_passes < 2 {
        return Err(crate::error::DaemonError::Process(
            "quarantine holder proof requires at least two fixed-point passes".into(),
        ));
    }
    let deadline = StdInstant::now() + limits.total_deadline;
    let mut trusted_platform_exemptions = TrustedPlatformExemptionCounts::default();
    for pass in 0..limits.fixed_point_passes {
        prove_quarantine_root_identity(tree)?;
        scan_quarantine_holders(
            proc_root,
            self_uid,
            tree,
            deadline,
            limits,
            &mut trusted_platform_exemptions,
        )?;
        prove_quarantine_root_identity(tree)?;
        #[cfg(test)]
        if pass == 0 {
            QUARANTINE_HOLDER_BETWEEN_PASSES_HOOK.with(|slot| {
                if let Some(hook) = slot.borrow_mut().take() {
                    hook();
                }
            });
        }
        if pass + 1 < limits.fixed_point_passes {
            std::thread::yield_now();
        }
    }
    if StdInstant::now() >= deadline {
        return Err(crate::error::DaemonError::Process(
            "quarantine holder proof exceeded its deadline".into(),
        ));
    }
    prove_quarantine_root_identity(tree)?;
    let fixed_point_passes = u32::try_from(limits.fixed_point_passes).map_err(|_| {
        crate::error::DaemonError::Process(
            "quarantine holder fixed-point pass count exceeds u32".into(),
        )
    })?;
    let trusted_platform_exemption_total = trusted_platform_exemptions.total()?;
    let trusted_platform_exemptions_digest = trusted_platform_exemptions.digest(fixed_point_passes);
    let root = tree.root_identity();
    let mut digest = Sha256::new();
    digest.update(b"rsi-quarantine-holder-proof-v2\0");
    digest.update(root.device().to_be_bytes());
    digest.update(root.inode().to_be_bytes());
    digest.update(fixed_point_passes.to_be_bytes());
    digest.update(trusted_platform_exemption_total.to_be_bytes());
    digest.update(trusted_platform_exemptions_digest);
    Ok(QuarantineHolderProof {
        fixed_point_passes,
        trusted_platform_exemptions: trusted_platform_exemption_total,
        trusted_platform_exemptions_digest: format!(
            "sha256:{}",
            hex::encode(trusted_platform_exemptions_digest)
        ),
        evidence_digest: format!("sha256:{:x}", digest.finalize()),
    })
}

fn prove_quarantine_root_identity(tree: &QuarantineTreeProof) -> crate::error::Result<()> {
    let metadata = std::fs::symlink_metadata(tree.root()).map_err(|error| {
        crate::error::DaemonError::Process(format!(
            "quarantine root identity became unavailable: {error}"
        ))
    })?;
    let expected = tree.root_identity();
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.dev() != expected.device()
        || metadata.ino() != expected.inode()
    {
        return Err(crate::error::DaemonError::Process(
            "quarantine root identity drifted during holder proof".into(),
        ));
    }
    Ok(())
}

fn read_quarantine_task_start_identity(
    task_root: &Path,
    tgid: i32,
    tid: i32,
) -> crate::error::Result<Option<(u64, u8)>> {
    let Some(stat) = read_bounded_proc_file(&task_root.join("stat"), STARTUP_ORPHAN_STAT_MAX_BYTES)
        .map_err(|error| {
            crate::error::DaemonError::Process(format!(
                "quarantine holder task stat failed for tgid {tgid} tid {tid}: {error}"
            ))
        })?
    else {
        return Ok(None);
    };
    let stat = std::str::from_utf8(&stat).map_err(|error| {
        crate::error::DaemonError::Process(format!(
            "quarantine holder task stat is not UTF-8 for tgid {tgid} tid {tid}: {error}"
        ))
    })?;
    let (identity, tail) = stat.rsplit_once(") ").ok_or_else(|| {
        crate::error::DaemonError::Process(format!(
            "quarantine holder task stat is malformed for tgid {tgid} tid {tid}"
        ))
    })?;
    identity
        .split_once(" (")
        .and_then(|(value, _)| value.parse::<i32>().ok())
        .filter(|value| *value == tid)
        .ok_or_else(|| {
            crate::error::DaemonError::Process(format!(
                "quarantine holder task stat tid is invalid for tgid {tgid} tid {tid}"
            ))
        })?;
    let mut fields = tail.split_whitespace();
    let state = fields
        .next()
        .and_then(|field| field.as_bytes().first())
        .copied()
        .ok_or_else(|| {
            crate::error::DaemonError::Process(format!(
                "quarantine holder task stat state is missing for tgid {tgid} tid {tid}"
            ))
        })?;
    let start_time = fields
        .nth(18)
        .ok_or_else(|| {
            crate::error::DaemonError::Process(format!(
                "quarantine holder task stat start time is missing for tgid {tgid} tid {tid}"
            ))
        })?
        .parse::<u64>()
        .map_err(|error| {
            crate::error::DaemonError::Process(format!(
                "quarantine holder task stat start time is invalid for tgid {tgid} tid {tid}: {error}"
            ))
        })?;
    Ok(Some((start_time, state)))
}

fn read_quarantine_task_status(
    task_root: &Path,
    tgid: i32,
    tid: i32,
    max_bytes: usize,
) -> crate::error::Result<Option<ProcTaskStatus>> {
    let Some(status) =
        read_bounded_proc_file(&task_root.join("status"), max_bytes).map_err(|error| {
            crate::error::DaemonError::Process(format!(
                "quarantine holder task status failed for tgid {tgid} tid {tid}: {error}"
            ))
        })?
    else {
        return Ok(None);
    };
    let mut status_pid = None;
    let mut status_tgid = None;
    let mut credentials = None;
    for line in status.split(|byte| *byte == b'\n') {
        if let Some(value) = line.strip_prefix(b"Pid:") {
            if status_pid.is_some() {
                return Err(malformed_quarantine_task_status(tgid, tid));
            }
            status_pid = Some(parse_quarantine_status_id(value, "Pid", tgid, tid)?);
        } else if let Some(value) = line.strip_prefix(b"Tgid:") {
            if status_tgid.is_some() {
                return Err(malformed_quarantine_task_status(tgid, tid));
            }
            status_tgid = Some(parse_quarantine_status_id(value, "Tgid", tgid, tid)?);
        } else if let Some(value) = line.strip_prefix(b"Uid:") {
            if credentials.is_some() {
                return Err(malformed_quarantine_task_status(tgid, tid));
            }
            let mut fields = value
                .split(|byte| byte.is_ascii_whitespace())
                .filter(|field| !field.is_empty());
            let mut next_uid = || {
                fields
                    .next()
                    .ok_or_else(|| malformed_quarantine_task_status(tgid, tid))
                    .and_then(|field| parse_quarantine_status_uid(field, tgid, tid))
            };
            let parsed = ProcTaskCredentials {
                real: next_uid()?,
                effective: next_uid()?,
                saved: next_uid()?,
                filesystem: next_uid()?,
            };
            if fields.next().is_some() {
                return Err(malformed_quarantine_task_status(tgid, tid));
            }
            credentials = Some(parsed);
        }
    }
    Ok(Some(ProcTaskStatus {
        pid: status_pid.ok_or_else(|| malformed_quarantine_task_status(tgid, tid))?,
        tgid: status_tgid.ok_or_else(|| malformed_quarantine_task_status(tgid, tid))?,
        credentials: credentials.ok_or_else(|| malformed_quarantine_task_status(tgid, tid))?,
    }))
}

fn parse_quarantine_status_id(
    value: &[u8],
    label: &str,
    tgid: i32,
    tid: i32,
) -> crate::error::Result<i32> {
    let value = trim_ascii_whitespace(value);
    if value.is_empty() || value.len() > 10 || !value.iter().all(u8::is_ascii_digit) {
        return Err(malformed_quarantine_task_status(tgid, tid));
    }
    std::str::from_utf8(value)
        .ok()
        .and_then(|value| value.parse::<i32>().ok())
        .filter(|value| *value > 0)
        .ok_or_else(|| {
            crate::error::DaemonError::Process(format!(
                "quarantine holder task status {label} is invalid for tgid {tgid} tid {tid}"
            ))
        })
}

fn parse_quarantine_status_uid(value: &[u8], tgid: i32, tid: i32) -> crate::error::Result<u32> {
    if value.is_empty() || value.len() > 10 || !value.iter().all(u8::is_ascii_digit) {
        return Err(malformed_quarantine_task_status(tgid, tid));
    }
    std::str::from_utf8(value)
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .ok_or_else(|| malformed_quarantine_task_status(tgid, tid))
}

fn malformed_quarantine_task_status(tgid: i32, tid: i32) -> crate::error::DaemonError {
    crate::error::DaemonError::Process(format!(
        "quarantine holder task status is malformed for tgid {tgid} tid {tid}"
    ))
}

fn quarantine_task_incarnation_is_gone(
    task_root: &Path,
    tgid: i32,
    tid: i32,
    expected_start_time: u64,
) -> crate::error::Result<bool> {
    Ok(
        read_quarantine_task_start_identity(task_root, tgid, tid)?.is_none_or(
            |(start_time, state)| {
                start_time != expected_start_time || matches!(state, b'Z' | b'X' | b'x')
            },
        ),
    )
}

fn revalidate_quarantine_task(
    task_root: &Path,
    tgid: i32,
    tid: i32,
    expected_start_time: u64,
    expected_credentials: ProcTaskCredentials,
    max_status_bytes: usize,
) -> crate::error::Result<bool> {
    if quarantine_task_incarnation_is_gone(task_root, tgid, tid, expected_start_time)? {
        return Ok(false);
    }
    let metadata = std::fs::symlink_metadata(task_root).map_err(|error| {
        crate::error::DaemonError::Process(format!(
            "quarantine holder task identity revalidation failed for tgid {tgid} tid {tid}: {error}"
        ))
    })?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(crate::error::DaemonError::Process(format!(
            "quarantine holder task identity changed for tgid {tgid} tid {tid}"
        )));
    }
    let status = match read_quarantine_task_status(task_root, tgid, tid, max_status_bytes) {
        Ok(Some(status)) => status,
        Ok(None) => {
            if quarantine_task_incarnation_is_gone(task_root, tgid, tid, expected_start_time)? {
                return Ok(false);
            }
            return Err(crate::error::DaemonError::Process(format!(
                "quarantine holder task status disappeared for tgid {tgid} tid {tid}"
            )));
        }
        Err(error) => {
            if quarantine_task_incarnation_is_gone(task_root, tgid, tid, expected_start_time)? {
                return Ok(false);
            }
            return Err(error);
        }
    };
    if status.pid != tid || status.tgid != tgid || status.credentials != expected_credentials {
        return Err(crate::error::DaemonError::Process(format!(
            "quarantine holder task credentials drifted for tgid {tgid} tid {tid}"
        )));
    }
    if quarantine_task_incarnation_is_gone(task_root, tgid, tid, expected_start_time)? {
        return Ok(false);
    }
    Ok(true)
}

#[cfg(test)]
fn initialize_fake_holder_proc(proc_root: &Path) -> (u32, std::path::PathBuf) {
    let self_namespace = proc_root.join("self/ns");
    std::fs::create_dir_all(&self_namespace).expect("fake self mount namespace");
    std::fs::write(self_namespace.join("mnt"), "shared namespace\n")
        .expect("fake self mount namespace identity");
    let namespace_root = proc_root.join("namespace-root");
    std::fs::create_dir(&namespace_root).expect("fake process namespace root");
    let uid = std::fs::metadata(proc_root)
        .expect("fake proc identity")
        .uid();
    (uid, namespace_root)
}

#[cfg(test)]
fn write_fake_holder_process(
    proc_root: &Path,
    pid: i32,
    namespace_root: &Path,
    cwd: &Path,
) -> std::path::PathBuf {
    write_fake_holder_task(proc_root, pid, pid, namespace_root, cwd, b'S', 424242)
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn write_fake_holder_task(
    proc_root: &Path,
    tgid: i32,
    tid: i32,
    namespace_root: &Path,
    cwd: &Path,
    state: u8,
    start_time: u64,
) -> std::path::PathBuf {
    let process = proc_root.join(tgid.to_string());
    let task = process.join("task").join(tid.to_string());
    std::fs::create_dir_all(task.join("fd")).expect("fake fd inventory");
    std::fs::create_dir_all(task.join("fdinfo")).expect("fake fdinfo inventory");
    std::fs::create_dir_all(task.join("ns")).expect("fake namespace inventory");
    std::fs::hard_link(proc_root.join("self/ns/mnt"), task.join("ns/mnt"))
        .expect("shared fake mount namespace");
    std::os::unix::fs::symlink(namespace_root, task.join("root")).expect("fake process root");
    std::os::unix::fs::symlink(cwd, task.join("cwd")).expect("fake process cwd");
    let mut stat_fields = vec!["0".to_string(); 20];
    stat_fields[0] = char::from(state).to_string();
    stat_fields[1] = "1".into();
    stat_fields[19] = start_time.to_string();
    let stat = format!("{tid} (holder) {}\n", stat_fields.join(" "));
    std::fs::write(task.join("stat"), &stat).expect("fake task stat");
    if tid == tgid {
        std::fs::write(process.join("stat"), stat).expect("fake process stat");
    }
    let uid = std::fs::metadata(proc_root)
        .expect("fake proc identity")
        .uid();
    std::fs::write(
        task.join("status"),
        format!("Name:\tholder\nTgid:\t{tgid}\nPid:\t{tid}\nUid:\t{uid}\t{uid}\t{uid}\t{uid}\n"),
    )
    .expect("fake task status");
    std::fs::write(task.join("maps"), "").expect("fake process maps");
    std::fs::write(
        task.join("mountinfo"),
        "1 0 00:00 / / rw - rootfs rootfs rw\n",
    )
    .expect("fake process mountinfo");
    task
}

fn scan_quarantine_holders(
    proc_root: &Path,
    self_uid: u32,
    tree: &QuarantineTreeProof,
    deadline: StdInstant,
    limits: QuarantineHolderScanLimits,
    trusted_platform_exemptions: &mut TrustedPlatformExemptionCounts,
) -> crate::error::Result<()> {
    let _self_mount_namespace =
        std::fs::metadata(proc_root.join("self/ns/mnt")).map_err(|error| {
            crate::error::DaemonError::Process(format!(
                "quarantine holder self mount namespace failed: {error}"
            ))
        })?;
    let processes = std::fs::read_dir(proc_root).map_err(|error| {
        crate::error::DaemonError::Process(format!(
            "quarantine holder /proc enumeration failed: {error}"
        ))
    })?;
    let mut process_entries = 0_usize;
    let mut task_entries = 0_usize;
    let mut fd_entries = 0_usize;
    for process in processes {
        if StdInstant::now() >= deadline {
            return Err(crate::error::DaemonError::Process(
                "quarantine holder proof exceeded its deadline".into(),
            ));
        }
        process_entries = process_entries.checked_add(1).ok_or_else(|| {
            crate::error::DaemonError::Process("quarantine holder process count overflowed".into())
        })?;
        if process_entries > limits.max_proc_entries {
            return Err(crate::error::DaemonError::Process(
                "quarantine holder process count exceeded bound".into(),
            ));
        }
        let process = process.map_err(|error| {
            crate::error::DaemonError::Process(format!(
                "quarantine holder process entry failed: {error}"
            ))
        })?;
        let Some(pid) = process
            .file_name()
            .to_str()
            .and_then(|value| value.parse::<i32>().ok())
        else {
            continue;
        };
        let process_root = proc_root.join(pid.to_string());
        let leader_identity = read_proc_start_identity(proc_root, pid)?;
        let tasks = match std::fs::read_dir(process_root.join("task")) {
            Ok(tasks) => tasks,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let current = read_proc_start_identity(proc_root, pid)?;
                if current.is_none() {
                    continue;
                }
                return Err(crate::error::DaemonError::Process(format!(
                    "quarantine holder task enumeration disappeared for a live or reused tgid {pid} (initial identity {leader_identity:?}, current identity {current:?})"
                )));
            }
            Err(error) => {
                return Err(crate::error::DaemonError::Process(format!(
                    "quarantine holder task enumeration failed for tgid {pid}: {error}"
                )));
            }
        };
        for task in tasks {
            if StdInstant::now() >= deadline {
                return Err(crate::error::DaemonError::Process(
                    "quarantine holder proof exceeded its deadline".into(),
                ));
            }
            task_entries = task_entries.checked_add(1).ok_or_else(|| {
                crate::error::DaemonError::Process("quarantine holder task count overflowed".into())
            })?;
            if task_entries > limits.max_task_entries {
                return Err(crate::error::DaemonError::Process(
                    "quarantine holder task count exceeded bound".into(),
                ));
            }
            let task = task.map_err(|error| {
                crate::error::DaemonError::Process(format!(
                    "quarantine holder task entry failed for tgid {pid}: {error}"
                ))
            })?;
            let Some(tid) = task
                .file_name()
                .to_str()
                .and_then(|value| value.parse::<i32>().ok())
            else {
                continue;
            };
            let task_root = process_root.join("task").join(tid.to_string());
            let Some((start_time, state)) =
                read_quarantine_task_start_identity(&task_root, pid, tid)?
            else {
                continue;
            };
            if matches!(state, b'Z' | b'X' | b'x') {
                continue;
            }
            match std::fs::symlink_metadata(&task_root) {
                Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
                Ok(_) => {
                    return Err(crate::error::DaemonError::Process(format!(
                        "quarantine holder task identity is not a directory for tgid {pid} tid {tid}"
                    )));
                }
                Err(error) => {
                    if quarantine_task_incarnation_is_gone(&task_root, pid, tid, start_time)? {
                        continue;
                    }
                    return Err(crate::error::DaemonError::Process(format!(
                        "quarantine holder task identity failed for tgid {pid} tid {tid}: {error}"
                    )));
                }
            }
            let status = match read_quarantine_task_status(
                &task_root,
                pid,
                tid,
                limits.max_status_bytes,
            ) {
                Ok(Some(status)) => status,
                Ok(None) => {
                    if quarantine_task_incarnation_is_gone(&task_root, pid, tid, start_time)? {
                        continue;
                    }
                    return Err(crate::error::DaemonError::Process(format!(
                        "quarantine holder task credentials disappeared for tgid {pid} tid {tid}"
                    )));
                }
                Err(error) => {
                    if quarantine_task_incarnation_is_gone(&task_root, pid, tid, start_time)? {
                        continue;
                    }
                    return Err(error);
                }
            };
            if status.pid != tid || status.tgid != pid {
                return Err(crate::error::DaemonError::Process(format!(
                    "quarantine holder task status identity drifted for tgid {pid} tid {tid}"
                )));
            }
            let credentials = status.credentials;
            if !credentials.contains(self_uid) {
                revalidate_quarantine_task(
                    &task_root,
                    pid,
                    tid,
                    start_time,
                    credentials,
                    limits.max_status_bytes,
                )?;
                continue;
            }
            let trusted_platform_identity =
                authenticate_trusted_platform_process(proc_root, pid, self_uid)?;
            scan_quarantine_task_inventory(
                proc_root,
                &task_root,
                pid,
                tid,
                start_time,
                credentials,
                self_uid,
                tree,
                deadline,
                limits,
                &mut fd_entries,
                trusted_platform_identity,
                trusted_platform_exemptions,
            )?;
        }
    }
    if StdInstant::now() >= deadline {
        return Err(crate::error::DaemonError::Process(
            "quarantine holder proof exceeded its deadline".into(),
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn scan_quarantine_task_inventory(
    proc_root: &Path,
    process_root: &Path,
    tgid: i32,
    pid: i32,
    start_time: u64,
    credentials: ProcTaskCredentials,
    self_uid: u32,
    tree: &QuarantineTreeProof,
    deadline: StdInstant,
    limits: QuarantineHolderScanLimits,
    fd_entries: &mut usize,
    trusted_platform_identity: Option<TrustedPlatformProcessIdentity>,
    trusted_platform_exemptions: &mut TrustedPlatformExemptionCounts,
) -> crate::error::Result<()> {
    let mut holder = None;
    let mut inventory_failures = QuarantineInventoryFailures::default();
    let mut mountinfo_complete = false;
    for (name, kind) in [("cwd", "cwd"), ("root", "root")] {
        let class = if name == "cwd" {
            QuarantineInventoryClass::Cwd
        } else {
            QuarantineInventoryClass::Root
        };
        match std::fs::metadata(process_root.join(name)) {
            Ok(target) if tree.contains_identity(target.dev(), target.ino()) => {
                holder = Some(kind);
            }
            Ok(_) => {}
            Err(error) => {
                inventory_failures.record_io(
                    class,
                    &error,
                    format!("{kind} identity read failed for pid {pid}: {error}"),
                );
            }
        };
    }

    match std::fs::read_dir(process_root.join("fd")) {
        Ok(descriptors) => {
            for descriptor in descriptors {
                if StdInstant::now() >= deadline {
                    return Err(crate::error::DaemonError::Process(
                        "quarantine holder proof exceeded its deadline".into(),
                    ));
                }
                *fd_entries = fd_entries.checked_add(1).ok_or_else(|| {
                    crate::error::DaemonError::Process(
                        "quarantine holder fd count overflowed".into(),
                    )
                })?;
                if *fd_entries > limits.max_fd_entries {
                    return Err(crate::error::DaemonError::Process(
                        "quarantine holder fd count exceeded bound".into(),
                    ));
                }
                let descriptor = match descriptor {
                    Ok(descriptor) => descriptor,
                    Err(error) => {
                        inventory_failures.record_io(
                            QuarantineInventoryClass::Fd,
                            &error,
                            format!("fd enumeration failed for pid {pid}: {error}"),
                        );
                        continue;
                    }
                };
                let descriptor_path = descriptor.path();
                #[cfg(test)]
                QUARANTINE_HOLDER_FD_BEFORE_READ_HOOK.with(|slot| {
                    if let Some(hook) = slot.borrow_mut().take() {
                        hook();
                    }
                });
                let mut descriptor_target_not_found = false;
                let descriptor_target = match std::fs::read_link(&descriptor_path) {
                    Ok(target) => Some(target),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        descriptor_target_not_found = true;
                        None
                    }
                    Err(error) => {
                        inventory_failures.record_io(
                            QuarantineInventoryClass::Fd,
                            &error,
                            format!("fd target read failed for pid {pid}: {error}"),
                        );
                        None
                    }
                };
                let mut descriptor_identity_not_found = false;
                let descriptor_identity = match std::fs::metadata(&descriptor_path) {
                    Ok(target) => {
                        if tree.contains_identity(target.dev(), target.ino()) {
                            holder = Some("fd");
                        }
                        Some((target.dev(), target.ino()))
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        descriptor_identity_not_found = true;
                        None
                    }
                    Err(error) => {
                        inventory_failures.record_io(
                            QuarantineInventoryClass::Fd,
                            &error,
                            format!("fd identity read failed for pid {pid}: {error}"),
                        );
                        None
                    }
                };
                let initially_closed = descriptor_target_not_found && descriptor_identity_not_found;
                let descriptor_is_io_uring = descriptor_target.as_ref().is_some_and(|target| {
                    target.as_os_str().as_bytes() == b"anon_inode:[io_uring]"
                });
                let mut fdinfo_not_found = false;
                if !initially_closed && (descriptor_is_io_uring || descriptor_target.is_none()) {
                    let fdinfo_path = process_root.join("fdinfo").join(descriptor.file_name());
                    match read_bounded_proc_file(&fdinfo_path, limits.max_fdinfo_bytes) {
                        Ok(Some(fdinfo)) => {
                            let has_user_files = fdinfo
                                .split(|byte| *byte == b'\n')
                                .any(|line| line.starts_with(b"UserFiles:\t"));
                            if descriptor_is_io_uring || has_user_files {
                                match process_io_uring_fdinfo_holds_tree(
                                    &process_root,
                                    &fdinfo,
                                    tree,
                                    limits.max_io_uring_user_files,
                                    deadline,
                                ) {
                                    Ok(true) => holder = Some("io_uring fixed file"),
                                    Ok(false) => {}
                                    Err(error) => {
                                        inventory_failures.record_non_permission(format!(
                                            "io_uring fdinfo proof failed for pid {pid}: {error}"
                                        ));
                                    }
                                }
                            }
                        }
                        Ok(None) => {
                            fdinfo_not_found = true;
                        }
                        Err(error) => {
                            inventory_failures.record_io(
                                QuarantineInventoryClass::FdInfo,
                                &error,
                                format!("io_uring fdinfo failed for pid {pid}: {error}"),
                            );
                        }
                    }
                }
                #[cfg(test)]
                QUARANTINE_HOLDER_FD_REVALIDATE_HOOK.with(|slot| {
                    if let Some(hook) = slot.borrow_mut().take() {
                        hook();
                    }
                });
                let mut revalidated_target_not_found = false;
                let revalidated_target = match std::fs::read_link(&descriptor_path) {
                    Ok(target) => Some(target),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        revalidated_target_not_found = true;
                        None
                    }
                    Err(error) => {
                        inventory_failures.record_io(
                            QuarantineInventoryClass::Fd,
                            &error,
                            format!("fd target revalidation failed for pid {pid}: {error}"),
                        );
                        None
                    }
                };
                #[cfg(test)]
                QUARANTINE_HOLDER_FD_REVALIDATE_BETWEEN_READS_HOOK.with(|slot| {
                    if let Some(hook) = slot.borrow_mut().take() {
                        hook();
                    }
                });
                let mut revalidated_identity_not_found = false;
                let revalidated_identity = match std::fs::metadata(&descriptor_path) {
                    Ok(revalidated) => {
                        let revalidated_identity = (revalidated.dev(), revalidated.ino());
                        if tree.contains_identity(revalidated_identity.0, revalidated_identity.1) {
                            holder = Some("fd");
                        }
                        Some(revalidated_identity)
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        revalidated_identity_not_found = true;
                        None
                    }
                    Err(error) => {
                        inventory_failures.record_io(
                            QuarantineInventoryClass::Fd,
                            &error,
                            format!("fd identity revalidation failed for pid {pid}: {error}"),
                        );
                        None
                    }
                };
                let revalidated_closed =
                    revalidated_target_not_found && revalidated_identity_not_found;
                if revalidated_closed {
                    continue;
                }
                let revalidated_partially_closed =
                    revalidated_target_not_found ^ revalidated_identity_not_found;
                if revalidated_partially_closed {
                    #[cfg(test)]
                    QUARANTINE_HOLDER_FD_FINAL_CONFIRM_HOOK.with(|slot| {
                        if let Some(hook) = slot.borrow_mut().take() {
                            hook();
                        }
                    });
                    let mut confirmed_target_not_found = false;
                    match std::fs::read_link(&descriptor_path) {
                        Ok(_) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            confirmed_target_not_found = true;
                        }
                        Err(error) => {
                            inventory_failures.record_io(
                                QuarantineInventoryClass::Fd,
                                &error,
                                format!(
                                    "fd target final close confirmation failed for pid {pid}: {error}"
                                ),
                            );
                        }
                    }
                    let mut confirmed_identity_not_found = false;
                    match std::fs::metadata(&descriptor_path) {
                        Ok(confirmed) => {
                            if tree.contains_identity(confirmed.dev(), confirmed.ino()) {
                                holder = Some("fd");
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            confirmed_identity_not_found = true;
                        }
                        Err(error) => {
                            inventory_failures.record_io(
                                QuarantineInventoryClass::Fd,
                                &error,
                                format!(
                                    "fd identity final close confirmation failed for pid {pid}: {error}"
                                ),
                            );
                        }
                    }
                    if confirmed_target_not_found && confirmed_identity_not_found {
                        continue;
                    }
                    if fdinfo_not_found {
                        inventory_failures.record_non_permission(format!(
                            "io_uring fdinfo disappeared for a live or reused fd in pid {pid}"
                        ));
                        fdinfo_not_found = false;
                    }
                    inventory_failures.record_non_permission(format!(
                        "fd remained live or was reused after a partial close observation for pid {pid}"
                    ));
                }
                if fdinfo_not_found {
                    inventory_failures.record_non_permission(format!(
                        "io_uring fdinfo disappeared for a live or reused fd in pid {pid}"
                    ));
                }
                if let Some(revalidated) = revalidated_target.as_ref() {
                    if descriptor_target.as_ref() != Some(revalidated) {
                        inventory_failures.record_non_permission(format!(
                            "fd target changed during proof for pid {pid}"
                        ));
                    }
                } else if revalidated_target_not_found {
                    inventory_failures.record_non_permission(format!(
                        "fd target disappeared without a complete close proof for pid {pid}"
                    ));
                }
                if let Some(revalidated) = revalidated_identity {
                    if descriptor_identity != Some(revalidated) {
                        inventory_failures.record_non_permission(format!(
                            "fd identity changed during proof for pid {pid}"
                        ));
                    }
                } else if revalidated_identity_not_found {
                    inventory_failures.record_non_permission(format!(
                        "fd identity disappeared without a complete close proof for pid {pid}"
                    ));
                }
            }
        }
        Err(error) => {
            inventory_failures.record_io(
                QuarantineInventoryClass::Fd,
                &error,
                format!("fd inventory failed for pid {pid}: {error}"),
            );
        }
    }

    match read_bounded_proc_file(&process_root.join("maps"), limits.max_maps_bytes) {
        Ok(Some(maps)) => {
            let mut map_records = 0_usize;
            for line in maps.split(|byte| *byte == b'\n') {
                if StdInstant::now() >= deadline {
                    return Err(crate::error::DaemonError::Process(
                        "quarantine holder proof exceeded its deadline".into(),
                    ));
                }
                if line.is_empty() {
                    continue;
                }
                map_records = map_records.checked_add(1).ok_or_else(|| {
                    crate::error::DaemonError::Process(
                        "quarantine holder map count overflowed".into(),
                    )
                })?;
                if map_records > limits.max_maps_records {
                    return Err(crate::error::DaemonError::Process(
                        "quarantine holder map count exceeded bound".into(),
                    ));
                }
                match parse_proc_map_identity(line) {
                    Ok(Some((device, inode))) if tree.contains_identity(device, inode) => {
                        holder = Some("mmap");
                    }
                    Ok(_) => {}
                    Err(error) => {
                        inventory_failures.record_non_permission(format!(
                            "maps inventory proof failed for pid {pid}: {error}"
                        ));
                        break;
                    }
                }
            }
        }
        Ok(None) => {
            inventory_failures
                .record_non_permission(format!("maps inventory disappeared for pid {pid}"));
        }
        Err(error) => {
            inventory_failures.record_io(
                QuarantineInventoryClass::Maps,
                &error,
                format!("maps inventory failed for pid {pid}: {error}"),
            );
        }
    }

    let mount_namespace_path = process_root.join("ns/mnt");
    let mount_namespace_identity = match std::fs::metadata(&mount_namespace_path) {
        Ok(namespace) => Some((namespace.dev(), namespace.ino())),
        Err(error) => {
            inventory_failures.record_io(
                QuarantineInventoryClass::MountNamespace,
                &error,
                format!("mount namespace identity failed for pid {pid}: {error}"),
            );
            None
        }
    };
    match read_bounded_proc_file(&process_root.join("mountinfo"), limits.max_mountinfo_bytes) {
        Ok(Some(mountinfo)) => {
            match process_mountinfo_holds_tree(
                &process_root,
                &mountinfo,
                tree,
                limits.max_mountinfo_records,
                deadline,
            ) {
                Ok(observation) => {
                    mountinfo_complete = true;
                    if observation.holder {
                        holder = Some("mount");
                    }
                    if observation.resolution_permission_denied {
                        let error = std::io::Error::new(
                            std::io::ErrorKind::PermissionDenied,
                            "mount namespace path resolution denied",
                        );
                        inventory_failures.record_io(
                            QuarantineInventoryClass::MountInfo,
                            &error,
                            format!("mount namespace path resolution denied for pid {pid}"),
                        );
                    }
                }
                Err(error) => {
                    inventory_failures.record_non_permission(format!(
                        "mount inventory proof failed for pid {pid}: {error}"
                    ));
                }
            }
        }
        Ok(None) => {
            inventory_failures
                .record_non_permission(format!("mount inventory disappeared for pid {pid}"));
        }
        Err(error) => {
            inventory_failures.record_io(
                QuarantineInventoryClass::MountInfo,
                &error,
                format!("mount inventory failed for pid {pid}: {error}"),
            );
        }
    }

    if !revalidate_quarantine_task(
        process_root,
        tgid,
        pid,
        start_time,
        credentials,
        limits.max_status_bytes,
    )? {
        return Ok(());
    }
    #[cfg(test)]
    QUARANTINE_HOLDER_MOUNT_NAMESPACE_REVALIDATE_HOOK.with(|slot| {
        if let Some(hook) = slot.borrow_mut().take() {
            hook();
        }
    });
    if let Some((expected_device, expected_inode)) = mount_namespace_identity {
        match std::fs::metadata(&mount_namespace_path) {
            Ok(namespace)
                if namespace.dev() == expected_device && namespace.ino() == expected_inode => {}
            Ok(_) => {
                return Err(crate::error::DaemonError::Process(format!(
                    "quarantine holder mount namespace drifted for tgid {tgid} tid {pid}"
                )));
            }
            Err(error) => {
                return Err(crate::error::DaemonError::Process(format!(
                    "quarantine holder mount namespace revalidation failed for tgid {tgid} tid {pid}: {error}"
                )));
            }
        }
    }
    if let Some(kind) = holder {
        return Err(crate::error::DaemonError::Process(format!(
            "same-UID task tgid {tgid} tid {pid} retains quarantine through {kind}"
        )));
    }
    if let Some(trusted_platform_identity) = trusted_platform_identity {
        let current = authenticate_trusted_platform_process(proc_root, tgid, self_uid)?;
        if current != Some(trusted_platform_identity) {
            return Err(crate::error::DaemonError::Process(format!(
                "quarantine trusted-platform identity drifted for tgid {tgid} tid {pid}"
            )));
        }
    }
    if let Some(failure) = inventory_failures.first {
        if inventory_failures.all_permission_denied
            && (!inventory_failures.mount_namespace_permission_denied || mountinfo_complete)
            && let Some(identity) = trusted_platform_identity
        {
            trusted_platform_exemptions
                .record(identity.class, inventory_failures.permission_denied_classes)?;
            return Ok(());
        }
        return Err(crate::error::DaemonError::Process(format!(
            "quarantine holder inventory is incomplete: {failure}"
        )));
    }
    Ok(())
}

fn parse_proc_map_identity(line: &[u8]) -> crate::error::Result<Option<(u64, u64)>> {
    let mut fields = line
        .split(|byte| byte.is_ascii_whitespace())
        .filter(|field| !field.is_empty());
    let _address = fields.next().ok_or_else(malformed_proc_map)?;
    let _permissions = fields.next().ok_or_else(malformed_proc_map)?;
    let _offset = fields.next().ok_or_else(malformed_proc_map)?;
    let device = fields.next().ok_or_else(malformed_proc_map)?;
    let inode = fields.next().ok_or_else(malformed_proc_map)?;
    let device = std::str::from_utf8(device).map_err(|_| {
        crate::error::DaemonError::Process("quarantine holder maps device is not ASCII".into())
    })?;
    let (major, minor) = device.split_once(':').ok_or_else(|| {
        crate::error::DaemonError::Process("quarantine holder maps device is malformed".into())
    })?;
    let major = u64::from_str_radix(major, 16).map_err(|_| {
        crate::error::DaemonError::Process("quarantine holder maps major device is invalid".into())
    })?;
    let minor = u64::from_str_radix(minor, 16).map_err(|_| {
        crate::error::DaemonError::Process("quarantine holder maps minor device is invalid".into())
    })?;
    let inode = std::str::from_utf8(inode)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| {
            crate::error::DaemonError::Process("quarantine holder maps inode is invalid".into())
        })?;
    if inode == 0 {
        return Ok(None);
    }
    Ok(Some((proc_map_device_id(major, minor)?, inode)))
}

fn proc_map_device_id(major: u64, minor: u64) -> crate::error::Result<u64> {
    #[cfg(target_os = "linux")]
    {
        Ok(nix::sys::stat::makedev(major, minor))
    }
    #[cfg(target_os = "macos")]
    {
        let major = u8::try_from(major).map_err(|_| {
            crate::error::DaemonError::Process(
                "quarantine holder maps major device exceeds the platform range".into(),
            )
        })?;
        if minor > 0x00ff_ffff {
            return Err(crate::error::DaemonError::Process(
                "quarantine holder maps minor device exceeds the platform range".into(),
            ));
        }
        let minor = i32::try_from(minor).map_err(|_| {
            crate::error::DaemonError::Process(
                "quarantine holder maps minor device exceeds the platform range".into(),
            )
        })?;
        Ok(nix::libc::makedev(i32::from(major), minor) as u64)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (major, minor);
        Err(crate::error::DaemonError::Process(
            "quarantine holder maps device identities are unsupported on this platform".into(),
        ))
    }
}

fn malformed_proc_map() -> crate::error::DaemonError {
    crate::error::DaemonError::Process("quarantine holder maps record is malformed".into())
}

fn process_io_uring_fdinfo_holds_tree(
    process_root: &Path,
    fdinfo: &[u8],
    tree: &QuarantineTreeProof,
    max_user_files: usize,
    deadline: StdInstant,
) -> crate::error::Result<bool> {
    let mut declared_user_files = None;
    let mut records = 0_usize;
    let mut last_index = None;
    let mut found_terminator = false;
    for line in fdinfo.split(|byte| *byte == b'\n') {
        if StdInstant::now() >= deadline {
            return Err(crate::error::DaemonError::Process(
                "quarantine holder proof exceeded its deadline".into(),
            ));
        }
        let Some(declared) = declared_user_files else {
            let Some(count) = line.strip_prefix(b"UserFiles:\t") else {
                continue;
            };
            let count = parse_bounded_ascii_usize(count, "io_uring UserFiles count")?;
            if count > max_user_files {
                return Err(crate::error::DaemonError::Process(
                    "quarantine holder io_uring UserFiles count exceeded bound".into(),
                ));
            }
            declared_user_files = Some(count);
            continue;
        };
        if let Some(user_bufs) = line.strip_prefix(b"UserBufs:\t") {
            let _ = parse_bounded_ascii_usize(user_bufs, "io_uring UserBufs count")?;
            found_terminator = true;
            break;
        }
        if line.is_empty() {
            return Err(malformed_io_uring_fdinfo());
        }
        records = records.checked_add(1).ok_or_else(|| {
            crate::error::DaemonError::Process(
                "quarantine holder io_uring UserFiles count overflowed".into(),
            )
        })?;
        if records > declared || records > max_user_files {
            return Err(crate::error::DaemonError::Process(
                "quarantine holder io_uring UserFiles records exceeded bound".into(),
            ));
        }
        let separator = line
            .iter()
            .position(|byte| *byte == b':')
            .ok_or_else(malformed_io_uring_fdinfo)?;
        let (index, path) = line.split_at(separator);
        let index = trim_ascii_whitespace(index);
        let index = parse_bounded_ascii_usize(index, "io_uring UserFiles index")?;
        if index >= declared || last_index.is_some_and(|last| index <= last) {
            return Err(malformed_io_uring_fdinfo());
        }
        last_index = Some(index);
        let path = path
            .strip_prefix(b": ")
            .filter(|path| !path.is_empty())
            .ok_or_else(malformed_io_uring_fdinfo)?;
        let path = decode_proc_mount_path(path)?;
        if path.ends_with(b" (deleted)") {
            return Err(crate::error::DaemonError::Process(
                "quarantine holder io_uring fixed file is deleted or unresolvable".into(),
            ));
        }
        let target = process_namespace_path_metadata(process_root, &path, "io_uring fixed file")?;
        if tree.contains_identity(target.dev(), target.ino()) {
            return Ok(true);
        }
    }
    if declared_user_files.is_none() || !found_terminator {
        return Err(malformed_io_uring_fdinfo());
    }
    Ok(false)
}

fn parse_bounded_ascii_usize(bytes: &[u8], label: &str) -> crate::error::Result<usize> {
    if bytes.is_empty() || bytes.len() > 20 || !bytes.iter().all(u8::is_ascii_digit) {
        return Err(crate::error::DaemonError::Process(format!(
            "quarantine holder {label} is malformed"
        )));
    }
    std::str::from_utf8(bytes)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .ok_or_else(|| {
            crate::error::DaemonError::Process(format!(
                "quarantine holder {label} exceeds its numeric bound"
            ))
        })
}

fn trim_ascii_whitespace(mut bytes: &[u8]) -> &[u8] {
    while bytes.first().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[1..];
    }
    while bytes.last().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

fn malformed_io_uring_fdinfo() -> crate::error::DaemonError {
    crate::error::DaemonError::Process(
        "quarantine holder io_uring fdinfo is malformed or incomplete".into(),
    )
}

#[derive(Default)]
struct QuarantineMountinfoObservation {
    holder: bool,
    resolution_permission_denied: bool,
}

fn process_mountinfo_holds_tree(
    process_root: &Path,
    mountinfo: &[u8],
    tree: &QuarantineTreeProof,
    max_records: usize,
    deadline: StdInstant,
) -> crate::error::Result<QuarantineMountinfoObservation> {
    let mut observation = QuarantineMountinfoObservation::default();
    let mut records = 0_usize;
    for line in mountinfo.split(|byte| *byte == b'\n') {
        if StdInstant::now() >= deadline {
            return Err(crate::error::DaemonError::Process(
                "quarantine holder proof exceeded its deadline".into(),
            ));
        }
        if line.is_empty() {
            continue;
        }
        records = records.checked_add(1).ok_or_else(|| {
            crate::error::DaemonError::Process("quarantine holder mount count overflowed".into())
        })?;
        if records > max_records {
            return Err(crate::error::DaemonError::Process(
                "quarantine holder mount count exceeded bound".into(),
            ));
        }
        let mut fields = line
            .split(|byte| byte.is_ascii_whitespace())
            .filter(|field| !field.is_empty());
        let mount_id = fields.next().ok_or_else(malformed_proc_mount)?;
        let parent_id = fields.next().ok_or_else(malformed_proc_mount)?;
        let device = fields.next().ok_or_else(malformed_proc_mount)?;
        let root = fields.next().ok_or_else(malformed_proc_mount)?;
        let mountpoint = fields.next().ok_or_else(malformed_proc_mount)?;
        let options = fields.next().ok_or_else(malformed_proc_mount)?;
        validate_proc_mount_decimal(mount_id)?;
        validate_proc_mount_decimal(parent_id)?;
        let device_separator = device
            .iter()
            .position(|byte| *byte == b':')
            .ok_or_else(malformed_proc_mount)?;
        let (major, minor) = device.split_at(device_separator);
        let minor = minor.get(1..).ok_or_else(malformed_proc_mount)?;
        validate_proc_mount_decimal(major)?;
        validate_proc_mount_decimal(minor)?;
        if options.is_empty() {
            return Err(malformed_proc_mount());
        }
        let mut optional_fields = 0_usize;
        loop {
            let field = fields.next().ok_or_else(malformed_proc_mount)?;
            if field == b"-" {
                break;
            }
            optional_fields = optional_fields
                .checked_add(1)
                .ok_or_else(malformed_proc_mount)?;
            if optional_fields > 64 {
                return Err(malformed_proc_mount());
            }
        }
        let filesystem_type = fields.next().ok_or_else(malformed_proc_mount)?;
        let mount_source = fields.next().ok_or_else(malformed_proc_mount)?;
        let super_options = fields.next().ok_or_else(malformed_proc_mount)?;
        if filesystem_type.is_empty()
            || mount_source.is_empty()
            || super_options.is_empty()
            || fields.next().is_some()
        {
            return Err(malformed_proc_mount());
        }
        // Both path fields are decoded and validated. The mountpoint reached
        // through `/proc/<pid>/root` is the kernel-visible identity of the
        // mounted root, including bind mounts of the quarantined directory.
        let root = decode_proc_mount_path(root)?;
        let mountpoint = decode_proc_mount_path(mountpoint)?;
        if mount_path_lexically_names_tree(&root, tree)
            || mount_path_lexically_names_tree(&mountpoint, tree)
        {
            observation.holder = true;
            continue;
        }
        match process_namespace_mount_path_metadata(process_root, &mountpoint, "mountpoint")? {
            NamespaceMountPathMetadata::Present(mount_target) => {
                if tree.contains_identity(mount_target.dev(), mount_target.ino()) {
                    observation.holder = true;
                }
            }
            NamespaceMountPathMetadata::PermissionDenied => {
                observation.resolution_permission_denied = true;
            }
            NamespaceMountPathMetadata::NotFound => {
                return Err(crate::error::DaemonError::Process(
                    "quarantine holder mountpoint disappeared".into(),
                ));
            }
        }
        // `root` is relative to the mounted filesystem and is not guaranteed
        // to have a namespace-visible pathname. Resolve it when it does; the
        // mountpoint identity above remains the authoritative mounted-root
        // proof for ordinary and bind mounts.
        match process_namespace_mount_path_metadata(process_root, &root, "mount root") {
            Ok(NamespaceMountPathMetadata::Present(source_root)) => {
                if tree.contains_identity(source_root.dev(), source_root.ino()) {
                    observation.holder = true;
                }
            }
            Ok(NamespaceMountPathMetadata::PermissionDenied) => {
                observation.resolution_permission_denied = true;
            }
            Ok(NamespaceMountPathMetadata::NotFound) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(observation)
}

fn validate_proc_mount_decimal(value: &[u8]) -> crate::error::Result<()> {
    if value.is_empty()
        || value.len() > 20
        || !value.iter().all(u8::is_ascii_digit)
        || std::str::from_utf8(value)
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .is_none()
    {
        return Err(malformed_proc_mount());
    }
    Ok(())
}

fn mount_path_lexically_names_tree(path: &[u8], tree: &QuarantineTreeProof) -> bool {
    let path = Path::new(std::ffi::OsStr::from_bytes(path));
    path.is_absolute() && path.starts_with(tree.root())
}

enum NamespaceMountPathMetadata {
    Present(std::fs::Metadata),
    PermissionDenied,
    NotFound,
}

fn process_namespace_mount_path_metadata(
    process_root: &Path,
    absolute: &[u8],
    label: &str,
) -> crate::error::Result<NamespaceMountPathMetadata> {
    let relative = absolute.strip_prefix(b"/").ok_or_else(|| {
        crate::error::DaemonError::Process(format!("quarantine holder {label} is not absolute"))
    })?;
    let relative = Path::new(&OsString::from_vec(relative.to_vec())).to_path_buf();
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(crate::error::DaemonError::Process(format!(
            "quarantine holder {label} is not normalized"
        )));
    }
    match std::fs::metadata(process_root.join("root").join(relative)) {
        Ok(metadata) => Ok(NamespaceMountPathMetadata::Present(metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            Ok(NamespaceMountPathMetadata::PermissionDenied)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(NamespaceMountPathMetadata::NotFound)
        }
        Err(error) => Err(crate::error::DaemonError::Process(format!(
            "quarantine holder {label} identity failed: {error}"
        ))),
    }
}

fn process_namespace_path_metadata(
    process_root: &Path,
    absolute: &[u8],
    label: &str,
) -> crate::error::Result<std::fs::Metadata> {
    let relative = absolute.strip_prefix(b"/").ok_or_else(|| {
        crate::error::DaemonError::Process(format!("quarantine holder {label} is not absolute"))
    })?;
    let relative = Path::new(&OsString::from_vec(relative.to_vec())).to_path_buf();
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(crate::error::DaemonError::Process(format!(
            "quarantine holder {label} is not normalized"
        )));
    }
    std::fs::metadata(process_root.join("root").join(relative)).map_err(|error| {
        crate::error::DaemonError::Process(format!(
            "quarantine holder {label} identity failed: {error}"
        ))
    })
}

fn malformed_proc_mount() -> crate::error::DaemonError {
    crate::error::DaemonError::Process("quarantine holder mount record is malformed".into())
}

fn decode_proc_mount_path(encoded: &[u8]) -> crate::error::Result<Vec<u8>> {
    let mut decoded = Vec::with_capacity(encoded.len());
    let mut index = 0_usize;
    while index < encoded.len() {
        if encoded[index] != b'\\' {
            decoded.push(encoded[index]);
            index += 1;
            continue;
        }
        let escape = encoded.get(index + 1..index + 4).ok_or_else(|| {
            crate::error::DaemonError::Process(
                "quarantine holder mount path has a truncated escape".into(),
            )
        })?;
        let value = match escape {
            b"040" => b' ',
            b"011" => b'\t',
            b"012" => b'\n',
            b"134" => b'\\',
            _ => {
                return Err(crate::error::DaemonError::Process(
                    "quarantine holder mount path has an unsupported escape".into(),
                ));
            }
        };
        decoded.push(value);
        index += 4;
    }
    Ok(decoded)
}

fn scan_startup_process_inventory(
    proc_root: &Path,
    self_pid: i32,
    self_uid: u32,
    deadline: StdInstant,
) -> crate::error::Result<StartupProcessInventory> {
    let entries = std::fs::read_dir(proc_root).map_err(|error| {
        crate::error::DaemonError::Process(format!(
            "startup provider /proc enumeration failed: {error}"
        ))
    })?;
    let mut processes = Vec::new();
    let mut session_ids = HashSet::new();
    let mut invocation_ids = HashSet::new();
    let mut stamp_token_count = 0_usize;
    let mut entry_count = 0_usize;
    for entry in entries {
        if StdInstant::now() >= deadline {
            return Err(crate::error::DaemonError::Process(
                "startup provider inventory exceeded its deadline".into(),
            ));
        }
        entry_count = entry_count.checked_add(1).ok_or_else(|| {
            crate::error::DaemonError::Process(
                "startup provider /proc entry count overflowed".into(),
            )
        })?;
        if entry_count > STARTUP_ORPHAN_PROC_ENTRY_MAX {
            return Err(crate::error::DaemonError::Process(
                "startup provider /proc entry bound exceeded".into(),
            ));
        }
        let entry = entry.map_err(|error| {
            crate::error::DaemonError::Process(format!(
                "startup provider /proc entry failed: {error}"
            ))
        })?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|value| value.parse::<i32>().ok())
        else {
            continue;
        };
        if pid == self_pid {
            continue;
        }
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(crate::error::DaemonError::Process(format!(
                    "startup provider process identity failed for pid {pid}: {error}"
                )));
            }
        };
        if metadata.uid() != self_uid {
            continue;
        }
        let Some((start_time, state)) = read_proc_start_identity(proc_root, pid)? else {
            continue;
        };
        // A zombie (or already-dead task) cannot execute against a settlement
        // root and often exposes an unreadable empty environ.  The stat state
        // is the bounded kernel proof needed to exclude it without weakening
        // the fail-closed policy for an unknown live same-UID process.
        if matches!(state, b'Z' | b'X' | b'x') {
            continue;
        }
        let environ_path = proc_root.join(pid.to_string()).join("environ");
        let environ = match read_bounded_proc_file(&environ_path, STARTUP_ORPHAN_ENV_MAX_BYTES) {
            Ok(Some(environ)) => environ,
            Ok(None) => continue,
            Err(error)
                if error.kind() == std::io::ErrorKind::PermissionDenied
                    && process_is_proven_systemd_user_manager_pair(proc_root, pid, self_uid)? =>
            {
                continue;
            }
            Err(error) => {
                return Err(crate::error::DaemonError::Process(format!(
                    "startup provider environ read failed for pid {pid}: {error}"
                )));
            }
        };
        let stamps = parse_startup_stamp_observation(&environ)?;
        if stamps.is_empty() {
            continue;
        }
        stamp_token_count = stamp_token_count
            .checked_add(stamps.token_count()?)
            .ok_or_else(|| {
                crate::error::DaemonError::Process(
                    "startup provider stamp tuple count overflowed".into(),
                )
            })?;
        if stamp_token_count > STARTUP_ORPHAN_STAMP_TUPLE_MAX {
            return Err(crate::error::DaemonError::Process(format!(
                "startup provider stamp tuple bound exceeded at pid {pid} ({STARTUP_ORPHAN_STAMP_TUPLE_MAX})"
            )));
        }
        if processes.len() >= STARTUP_ORPHAN_STAMP_TUPLE_MAX {
            return Err(crate::error::DaemonError::Process(format!(
                "startup provider stamped-process bound exceeded at pid {pid} ({STARTUP_ORPHAN_STAMP_TUPLE_MAX})"
            )));
        }
        for session_id in &stamps.session.canonical_values {
            session_ids.insert(*session_id);
        }
        for invocation_id in &stamps.invocation.canonical_values {
            invocation_ids.insert(*invocation_id);
        }
        if session_ids.len() > STARTUP_ORPHAN_STAMP_TUPLE_MAX
            || invocation_ids.len() > STARTUP_ORPHAN_STAMP_TUPLE_MAX
        {
            return Err(crate::error::DaemonError::Process(format!(
                "startup provider canonical identity bound exceeded at pid {pid} ({STARTUP_ORPHAN_STAMP_TUPLE_MAX})"
            )));
        }
        processes.push(StartupObservedProcess {
            pid,
            start_time,
            stamps,
        });
    }
    Ok(StartupProcessInventory {
        processes,
        session_ids,
        invocation_ids,
    })
}

#[cfg(target_os = "linux")]
fn kill_startup_provider_orphans(
    proc_root: &Path,
    observed: Vec<StartupOrphanIdentity>,
    ownership: &StartupProcessOwnership,
    overall_deadline: StdInstant,
) -> crate::error::Result<usize> {
    let mut signaled = Vec::new();
    for identity in observed {
        if StdInstant::now() >= overall_deadline {
            return Err(crate::error::DaemonError::Process(
                "startup provider inventory exceeded its deadline".into(),
            ));
        }
        let Some(pid) = rustix::process::Pid::from_raw(identity.pid) else {
            return Err(crate::error::DaemonError::Process(format!(
                "startup provider observed invalid pid {}",
                identity.pid
            )));
        };
        // Pin the kernel process identity before the final start-time and
        // environment proof. A later numeric-PID reuse cannot redirect the
        // signal sent through this descriptor.
        let pidfd = match rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty()) {
            Ok(pidfd) => pidfd,
            Err(rustix::io::Errno::SRCH) => continue,
            Err(error) => {
                return Err(crate::error::DaemonError::Process(format!(
                    "startup provider pidfd_open failed for pid {}: {error}",
                    identity.pid
                )));
            }
        };
        let Some((current_start_time, _)) = read_proc_start_identity(proc_root, identity.pid)?
        else {
            continue;
        };
        if current_start_time != identity.start_time {
            continue;
        }
        let environ_path = proc_root.join(identity.pid.to_string()).join("environ");
        let environ = read_bounded_proc_file(&environ_path, STARTUP_ORPHAN_ENV_MAX_BYTES).map_err(
            |error| {
                crate::error::DaemonError::Process(format!(
                    "startup provider pre-kill environ failed for pid {}: {error}",
                    identity.pid
                ))
            },
        )?;
        let Some(environ) = environ else {
            if read_proc_start_identity(proc_root, identity.pid)?
                .is_some_and(|(start_time, _)| start_time == identity.start_time)
            {
                return Err(crate::error::DaemonError::Process(format!(
                    "startup provider process {} lost identity before kill",
                    identity.pid
                )));
            }
            continue;
        };
        let stamps = parse_startup_stamp_observation(&environ)?
            .authorized_exact_stamp(identity.pid, ownership)?;
        if stamps.as_ref() != Some(&identity.stamps) {
            return Err(crate::error::DaemonError::Process(format!(
                "startup provider process {} changed exact ownership tuple before kill",
                identity.pid
            )));
        }
        let Some((final_start_time, final_state)) =
            read_proc_start_identity(proc_root, identity.pid)?
        else {
            continue;
        };
        if final_start_time != identity.start_time || matches!(final_state, b'Z' | b'X' | b'x') {
            continue;
        }
        match rustix::process::pidfd_send_signal(&pidfd, rustix::process::Signal::KILL) {
            Ok(()) => signaled.push(identity),
            Err(rustix::io::Errno::SRCH) => {}
            Err(error) => {
                return Err(crate::error::DaemonError::Process(format!(
                    "startup provider pidfd SIGKILL failed for pid {}: {error}",
                    identity.pid
                )));
            }
        }
    }

    let reaped = signaled.len();
    let deadline = std::cmp::min(
        StdInstant::now() + STARTUP_ORPHAN_POST_KILL_GRACE,
        overall_deadline,
    );
    let mut pending = signaled;
    while !pending.is_empty() && StdInstant::now() < deadline {
        let mut still_live = Vec::new();
        for identity in pending {
            match read_proc_start_identity(proc_root, identity.pid)? {
                None => {}
                Some((start_time, _)) if start_time != identity.start_time => {}
                Some((_, state)) if matches!(state, b'Z' | b'X' | b'x') => {}
                Some(_) => still_live.push(identity),
            }
        }
        pending = still_live;
        if !pending.is_empty() {
            std::thread::sleep(STARTUP_ORPHAN_POST_KILL_POLL);
        }
    }
    if let Some(identity) = pending.first() {
        return Err(crate::error::DaemonError::Process(format!(
            "startup provider orphan remained live after SIGKILL for pid {}",
            identity.pid
        )));
    }
    Ok(reaped)
}

#[cfg(not(target_os = "linux"))]
fn kill_startup_provider_orphans(
    _proc_root: &Path,
    _observed: Vec<StartupOrphanIdentity>,
    _ownership: &StartupProcessOwnership,
    _overall_deadline: StdInstant,
) -> crate::error::Result<usize> {
    // All non-Linux entry points are deliberate no-ops. Keep this fallback
    // inert as defense in depth: never substitute an unpinned numeric signal.
    Ok(0)
}

fn read_bounded_proc_file(path: &Path, max_bytes: usize) -> std::io::Result<Option<Vec<u8>>> {
    let Some(file) = normalize_proc_open(File::open(path))? else {
        return Ok(None);
    };
    read_bounded_proc_reader(file, max_bytes)
}

fn normalize_proc_open<T>(result: std::io::Result<T>) -> std::io::Result<Option<T>> {
    match result {
        Ok(file) => Ok(Some(file)),
        Err(error) if proc_task_disappeared(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

fn proc_task_disappeared(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::NotFound
        || (cfg!(target_os = "linux") && error.raw_os_error() == Some(nix::libc::ESRCH))
}

fn read_bounded_proc_reader(
    reader: impl Read,
    max_bytes: usize,
) -> std::io::Result<Option<Vec<u8>>> {
    let mut bytes = Vec::with_capacity(max_bytes.min(64 * 1024));
    match reader
        .take(max_bytes.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
    {
        Ok(_) => {}
        // Linux procfs may report either ENOENT or ESRCH when a task exits
        // during open/read. Both mean the inventory entry disappeared, not
        // that an unknown live process escaped the ownership proof.
        Err(error) if proc_task_disappeared(&error) => return Ok(None),
        Err(error) => return Err(error),
    }
    if bytes.len() > max_bytes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "bounded /proc field exceeds byte limit",
        ));
    }
    Ok(Some(bytes))
}

fn read_proc_start_identity(proc_root: &Path, pid: i32) -> crate::error::Result<Option<(u64, u8)>> {
    let Some(stat) = read_bounded_proc_file(
        &proc_root.join(pid.to_string()).join("stat"),
        STARTUP_ORPHAN_STAT_MAX_BYTES,
    )
    .map_err(|error| {
        crate::error::DaemonError::Process(format!(
            "startup provider stat read failed for pid {pid}: {error}"
        ))
    })?
    else {
        return Ok(None);
    };
    let stat = std::str::from_utf8(&stat).map_err(|error| {
        crate::error::DaemonError::Process(format!(
            "startup provider stat is not UTF-8 for pid {pid}: {error}"
        ))
    })?;
    let tail = stat
        .rsplit_once(") ")
        .map(|(_, tail)| tail)
        .ok_or_else(|| {
            crate::error::DaemonError::Process(format!(
                "startup provider stat is malformed for pid {pid}"
            ))
        })?;
    let fields = tail.split_whitespace().collect::<Vec<_>>();
    let state = fields
        .first()
        .and_then(|field| field.as_bytes().first())
        .copied()
        .ok_or_else(|| {
            crate::error::DaemonError::Process(format!(
                "startup provider stat state is missing for pid {pid}"
            ))
        })?;
    let start_time = fields
        .get(19)
        .ok_or_else(|| {
            crate::error::DaemonError::Process(format!(
                "startup provider stat start time is missing for pid {pid}"
            ))
        })?
        .parse::<u64>()
        .map_err(|error| {
            crate::error::DaemonError::Process(format!(
                "startup provider stat start time is invalid for pid {pid}: {error}"
            ))
        })?;
    Ok(Some((start_time, state)))
}

fn process_is_proven_systemd_user_manager_pair(
    proc_root: &Path,
    pid: i32,
    uid: u32,
) -> crate::error::Result<bool> {
    let Some((comm, parent_pid)) = read_proc_comm_and_parent(proc_root, pid)? else {
        return Ok(false);
    };
    if !process_is_exact_systemd_user_init_scope(proc_root, pid, uid)? {
        return Ok(false);
    }
    if comm == "systemd" && parent_pid == 1 {
        return Ok(true);
    }
    if !matches!(comm.as_str(), "(sd-pam)" | "sd-pam") || parent_pid <= 1 {
        return Ok(false);
    }
    let parent_metadata = match std::fs::metadata(proc_root.join(parent_pid.to_string())) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(crate::error::DaemonError::Process(format!(
                "startup provider user-manager parent identity failed for pid {parent_pid}: {error}"
            )));
        }
    };
    if parent_metadata.uid() != uid {
        return Ok(false);
    }
    let Some((parent_comm, grandparent_pid)) = read_proc_comm_and_parent(proc_root, parent_pid)?
    else {
        return Ok(false);
    };
    Ok(parent_comm == "systemd"
        && grandparent_pid == 1
        && process_is_exact_systemd_user_init_scope(proc_root, parent_pid, uid)?)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PortalFuseMountHelperObservation {
    helper_start_time: u64,
    helper_state: u8,
    helper_directory_device: u64,
    helper_directory_inode: u64,
    parent_pid: i32,
    parent_start_time: u64,
    parent_state: u8,
    parent_directory_device: u64,
    parent_directory_inode: u64,
    manager_pid: i32,
    manager_start_time: u64,
    manager_state: u8,
    manager_directory_device: u64,
    manager_directory_inode: u64,
    portal_binary_device: u64,
    portal_binary_inode: u64,
}

#[derive(Clone, Copy)]
struct PortalTrustBaseline<'a> {
    privileged_uid: u32,
    portal_executable: &'a Path,
}

impl PortalTrustBaseline<'static> {
    fn production() -> Self {
        Self {
            privileged_uid: 0,
            portal_executable: Path::new(PORTAL_EXECUTABLE),
        }
    }
}

fn observe_exact_portal_fuse_mount_helper(
    proc_root: &Path,
    pid: i32,
    uid: u32,
    baseline: PortalTrustBaseline<'_>,
) -> crate::error::Result<Option<PortalFuseMountHelperObservation>> {
    // The helper is setuid-root, so the user's daemon cannot dereference its
    // `/proc/<pid>/exe`. Bind it instead to exact root-effective credentials,
    // byte-exact argv/cgroup, and the root-owned executable identity of its
    // authenticated document-portal parent and systemd --user ancestor.
    let Some((comm, parent_pid)) = read_proc_comm_and_parent(proc_root, pid)? else {
        return Ok(None);
    };
    if comm != "fusermount3" || parent_pid <= 1 {
        return Ok(None);
    }
    let process_root = proc_root.join(pid.to_string());
    let helper_metadata = match std::fs::symlink_metadata(&process_root) {
        Ok(metadata)
            if metadata.is_dir()
                && !metadata.file_type().is_symlink()
                && metadata.uid() == baseline.privileged_uid =>
        {
            metadata
        }
        Ok(_) => return Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(crate::error::DaemonError::Process(format!(
                "quarantine portal mount-helper identity failed for pid {pid}: {error}"
            )));
        }
    };
    let Some((helper_start_time, helper_state)) = read_proc_start_identity(proc_root, pid)? else {
        return Ok(None);
    };
    if matches!(helper_state, b'Z' | b'X' | b'x') {
        return Ok(None);
    }
    let Some(helper_status) =
        read_quarantine_task_status(&process_root, pid, pid, QUARANTINE_HOLDER_STATUS_MAX_BYTES)?
    else {
        return Ok(None);
    };
    if helper_status.pid != pid
        || helper_status.tgid != pid
        || !portal_fuse_mount_helper_credentials_are_exact(
            helper_status.credentials,
            uid,
            baseline.privileged_uid,
        )
        || !process_has_exact_cgroup(
            proc_root,
            pid,
            &portal_service_cgroup(uid),
            "portal mount-helper",
        )?
        || !process_has_exact_cmdline(
            proc_root,
            pid,
            &portal_fuse_mount_helper_cmdline(uid),
            "portal mount-helper",
        )?
    {
        return Ok(None);
    }

    let Some(parent) = observe_exact_document_portal(proc_root, parent_pid, uid, baseline)? else {
        return Ok(None);
    };
    Ok(Some(PortalFuseMountHelperObservation {
        helper_start_time,
        helper_state,
        helper_directory_device: helper_metadata.dev(),
        helper_directory_inode: helper_metadata.ino(),
        parent_pid,
        parent_start_time: parent.start_time,
        parent_state: parent.state,
        parent_directory_device: parent.directory_device,
        parent_directory_inode: parent.directory_inode,
        manager_pid: parent.manager_pid,
        manager_start_time: parent.manager_start_time,
        manager_state: parent.manager_state,
        manager_directory_device: parent.manager_directory_device,
        manager_directory_inode: parent.manager_directory_inode,
        portal_binary_device: parent.binary_device,
        portal_binary_inode: parent.binary_inode,
    }))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DocumentPortalObservation {
    start_time: u64,
    state: u8,
    directory_device: u64,
    directory_inode: u64,
    manager_pid: i32,
    manager_start_time: u64,
    manager_state: u8,
    manager_directory_device: u64,
    manager_directory_inode: u64,
    binary_device: u64,
    binary_inode: u64,
}

fn observe_exact_document_portal(
    proc_root: &Path,
    pid: i32,
    uid: u32,
    baseline: PortalTrustBaseline<'_>,
) -> crate::error::Result<Option<DocumentPortalObservation>> {
    let Some((comm, manager_pid)) = read_proc_comm_and_parent(proc_root, pid)? else {
        return Ok(None);
    };
    if comm != "xdg-document-po" || manager_pid <= 1 {
        return Ok(None);
    }
    let process_root = proc_root.join(pid.to_string());
    let metadata = match std::fs::symlink_metadata(&process_root) {
        Ok(metadata)
            if metadata.is_dir() && !metadata.file_type().is_symlink() && metadata.uid() == uid =>
        {
            metadata
        }
        Ok(_) => return Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(crate::error::DaemonError::Process(format!(
                "quarantine document-portal identity failed for pid {pid}: {error}"
            )));
        }
    };
    let Some((start_time, state)) = read_proc_start_identity(proc_root, pid)? else {
        return Ok(None);
    };
    if matches!(state, b'Z' | b'X' | b'x') {
        return Ok(None);
    }
    let Some(status) =
        read_quarantine_task_status(&process_root, pid, pid, QUARANTINE_HOLDER_STATUS_MAX_BYTES)?
    else {
        return Ok(None);
    };
    let ordinary_credentials = ProcTaskCredentials {
        real: uid,
        effective: uid,
        saved: uid,
        filesystem: uid,
    };
    if status.pid != pid
        || status.tgid != pid
        || status.credentials != ordinary_credentials
        || !process_has_exact_cgroup(
            proc_root,
            pid,
            &portal_service_cgroup(uid),
            "document portal",
        )?
        || !process_has_exact_cmdline(
            proc_root,
            pid,
            &nul_terminated_path(baseline.portal_executable),
            "document portal",
        )?
    {
        return Ok(None);
    }
    let Some((binary_device, binary_inode)) =
        exact_document_portal_binary(proc_root, pid, baseline)?
    else {
        return Ok(None);
    };
    let Some(manager) = authenticate_trusted_platform_process(proc_root, manager_pid, uid)? else {
        return Ok(None);
    };
    if manager.class != TrustedPlatformProcessClass::SystemdUserManager {
        return Ok(None);
    }
    Ok(Some(DocumentPortalObservation {
        start_time,
        state,
        directory_device: metadata.dev(),
        directory_inode: metadata.ino(),
        manager_pid,
        manager_start_time: manager.start_time,
        manager_state: manager.state,
        manager_directory_device: manager.directory_device,
        manager_directory_inode: manager.directory_inode,
        binary_device,
        binary_inode,
    }))
}

fn exact_document_portal_binary(
    proc_root: &Path,
    pid: i32,
    baseline: PortalTrustBaseline<'_>,
) -> crate::error::Result<Option<(u64, u64)>> {
    let expected = match std::fs::canonicalize(baseline.portal_executable) {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(crate::error::DaemonError::Process(format!(
                "quarantine document-portal executable baseline failed: {error}"
            )));
        }
    };
    let observed = match std::fs::canonicalize(proc_root.join(pid.to_string()).join("exe")) {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(crate::error::DaemonError::Process(format!(
                "quarantine document-portal executable identity failed for pid {pid}: {error}"
            )));
        }
    };
    if observed != expected {
        return Ok(None);
    }
    let metadata = std::fs::metadata(&expected).map_err(|error| {
        crate::error::DaemonError::Process(format!(
            "quarantine document-portal executable metadata failed: {error}"
        ))
    })?;
    if !metadata.is_file()
        || metadata.uid() != baseline.privileged_uid
        || metadata.mode() & 0o022 != 0
    {
        return Ok(None);
    }
    Ok(Some((metadata.dev(), metadata.ino())))
}

fn portal_service_cgroup(uid: u32) -> String {
    format!(
        "0::/user.slice/user-{uid}.slice/user@{uid}.service/session.slice/xdg-document-portal.service"
    )
}

fn portal_fuse_mount_helper_cmdline(uid: u32) -> Vec<u8> {
    format!(
        "fusermount3\0-o\0rw,nosuid,nodev,fsname=portal,auto_unmount,subtype=portal\0--\0/run/user/{uid}/doc\0"
    )
    .into_bytes()
}

fn nul_terminated_path(path: &Path) -> Vec<u8> {
    let mut bytes = path.as_os_str().as_bytes().to_vec();
    bytes.push(0);
    bytes
}

fn portal_fuse_mount_helper_credentials_are_exact(
    credentials: ProcTaskCredentials,
    uid: u32,
    privileged_uid: u32,
) -> bool {
    credentials
        == (ProcTaskCredentials {
            real: uid,
            effective: privileged_uid,
            saved: privileged_uid,
            filesystem: privileged_uid,
        })
}

fn process_has_exact_cmdline(
    proc_root: &Path,
    pid: i32,
    expected: &[u8],
    label: &str,
) -> crate::error::Result<bool> {
    let Some(cmdline) = read_bounded_proc_file(
        &proc_root.join(pid.to_string()).join("cmdline"),
        QUARANTINE_PLATFORM_CMDLINE_MAX_BYTES,
    )
    .map_err(|error| {
        crate::error::DaemonError::Process(format!(
            "quarantine {label} command line failed for pid {pid}: {error}"
        ))
    })?
    else {
        return Ok(false);
    };
    Ok(cmdline == expected)
}

fn process_has_exact_cgroup(
    proc_root: &Path,
    pid: i32,
    expected: &str,
    label: &str,
) -> crate::error::Result<bool> {
    let Some(cgroup) = read_bounded_proc_file(
        &proc_root.join(pid.to_string()).join("cgroup"),
        STARTUP_ORPHAN_CGROUP_MAX_BYTES,
    )
    .map_err(|error| {
        crate::error::DaemonError::Process(format!(
            "quarantine {label} cgroup failed for pid {pid}: {error}"
        ))
    })?
    else {
        return Ok(false);
    };
    let cgroup = std::str::from_utf8(&cgroup).map_err(|error| {
        crate::error::DaemonError::Process(format!(
            "quarantine {label} cgroup is not UTF-8 for pid {pid}: {error}"
        ))
    })?;
    let mut lines = cgroup.lines();
    Ok(lines.next() == Some(expected) && lines.next().is_none())
}

fn authenticate_portal_fuse_mount_helper(
    proc_root: &Path,
    pid: i32,
    uid: u32,
) -> crate::error::Result<Option<TrustedPlatformProcessIdentity>> {
    authenticate_portal_fuse_mount_helper_at(proc_root, pid, uid, PortalTrustBaseline::production())
}

fn authenticate_portal_fuse_mount_helper_at(
    proc_root: &Path,
    pid: i32,
    uid: u32,
    baseline: PortalTrustBaseline<'_>,
) -> crate::error::Result<Option<TrustedPlatformProcessIdentity>> {
    let Some(observed) = observe_exact_portal_fuse_mount_helper(proc_root, pid, uid, baseline)?
    else {
        return Ok(None);
    };
    if observe_exact_portal_fuse_mount_helper(proc_root, pid, uid, baseline)? != Some(observed) {
        return Ok(None);
    }
    Ok(Some(TrustedPlatformProcessIdentity {
        class: TrustedPlatformProcessClass::PortalFuseMountHelper,
        start_time: observed.helper_start_time,
        state: observed.helper_state,
        uid,
        parent_pid: observed.parent_pid,
        directory_device: observed.helper_directory_device,
        directory_inode: observed.helper_directory_inode,
        parent_start_time: Some(observed.parent_start_time),
        parent_state: Some(observed.parent_state),
        parent_directory_device: Some(observed.parent_directory_device),
        parent_directory_inode: Some(observed.parent_directory_inode),
        ancestor_pid: Some(observed.manager_pid),
        ancestor_start_time: Some(observed.manager_start_time),
        ancestor_state: Some(observed.manager_state),
        ancestor_directory_device: Some(observed.manager_directory_device),
        ancestor_directory_inode: Some(observed.manager_directory_inode),
        platform_binary_device: Some(observed.portal_binary_device),
        platform_binary_inode: Some(observed.portal_binary_inode),
    }))
}

/// Authenticate the three narrow trusted platform classes admitted by the
/// quarantine policy. This does not prove that they hold no references:
/// manager FDSTORE/transient stdio/RootDirectory descriptors, sd-pam inherited
/// descriptors, the exact portal fusermount helper's privileged descriptors,
/// inotify/path watches, queued SCM_RIGHTS, and in-flight AIO/io_uring
/// references can remain unobservable. It only permits explicitly recorded
/// `PermissionDenied` inventory fields under the non-adversarial single-user
/// platform boundary.
fn authenticate_trusted_platform_process(
    proc_root: &Path,
    pid: i32,
    uid: u32,
) -> crate::error::Result<Option<TrustedPlatformProcessIdentity>> {
    if let Some(identity) = authenticate_portal_fuse_mount_helper(proc_root, pid, uid)? {
        return Ok(Some(identity));
    }
    let Some((comm, parent_pid)) = read_proc_comm_and_parent(proc_root, pid)? else {
        return Ok(None);
    };
    let class = if comm == "systemd" && parent_pid == 1 {
        TrustedPlatformProcessClass::SystemdUserManager
    } else if matches!(comm.as_str(), "(sd-pam)" | "sd-pam") && parent_pid > 1 {
        TrustedPlatformProcessClass::SystemdSdPam
    } else {
        return Ok(None);
    };
    if !process_is_exact_systemd_user_init_scope(proc_root, pid, uid)? {
        return Ok(None);
    }
    let process_root = proc_root.join(pid.to_string());
    let metadata = match std::fs::symlink_metadata(&process_root) {
        Ok(metadata)
            if metadata.is_dir() && !metadata.file_type().is_symlink() && metadata.uid() == uid =>
        {
            metadata
        }
        Ok(_) => return Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(crate::error::DaemonError::Process(format!(
                "quarantine trusted-platform identity failed for pid {pid}: {error}"
            )));
        }
    };
    let Some((start_time, state)) = read_proc_start_identity(proc_root, pid)? else {
        return Ok(None);
    };
    if matches!(state, b'Z' | b'X' | b'x') {
        return Ok(None);
    }

    let mut parent_start_time = None;
    let mut parent_state = None;
    let mut parent_directory_device = None;
    let mut parent_directory_inode = None;
    if matches!(class, TrustedPlatformProcessClass::SystemdSdPam) {
        let parent_root = proc_root.join(parent_pid.to_string());
        let parent_metadata = match std::fs::symlink_metadata(&parent_root) {
            Ok(metadata)
                if metadata.is_dir()
                    && !metadata.file_type().is_symlink()
                    && metadata.uid() == uid =>
            {
                metadata
            }
            Ok(_) => return Ok(None),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(crate::error::DaemonError::Process(format!(
                    "quarantine trusted-platform parent identity failed for pid {parent_pid}: {error}"
                )));
            }
        };
        let Some((parent_comm, grandparent_pid)) =
            read_proc_comm_and_parent(proc_root, parent_pid)?
        else {
            return Ok(None);
        };
        let Some((observed_parent_start, observed_parent_state)) =
            read_proc_start_identity(proc_root, parent_pid)?
        else {
            return Ok(None);
        };
        if parent_comm != "systemd"
            || grandparent_pid != 1
            || matches!(observed_parent_state, b'Z' | b'X' | b'x')
            || !process_is_exact_systemd_user_init_scope(proc_root, parent_pid, uid)?
        {
            return Ok(None);
        }
        parent_start_time = Some(observed_parent_start);
        parent_state = Some(observed_parent_state);
        parent_directory_device = Some(parent_metadata.dev());
        parent_directory_inode = Some(parent_metadata.ino());
    }

    let Some((current_start_time, current_state)) = read_proc_start_identity(proc_root, pid)?
    else {
        return Ok(None);
    };
    let current_metadata = match std::fs::symlink_metadata(&process_root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(crate::error::DaemonError::Process(format!(
                "quarantine trusted-platform identity revalidation failed for pid {pid}: {error}"
            )));
        }
    };
    if current_start_time != start_time
        || current_state != state
        || current_metadata.uid() != uid
        || current_metadata.dev() != metadata.dev()
        || current_metadata.ino() != metadata.ino()
        || read_proc_comm_and_parent(proc_root, pid)? != Some((comm.clone(), parent_pid))
        || !process_is_exact_systemd_user_init_scope(proc_root, pid, uid)?
    {
        return Ok(None);
    }
    if let Some(expected_parent_start) = parent_start_time {
        let parent_root = proc_root.join(parent_pid.to_string());
        let Some((current_parent_start, current_parent_state)) =
            read_proc_start_identity(proc_root, parent_pid)?
        else {
            return Ok(None);
        };
        let current_parent_metadata = match std::fs::symlink_metadata(&parent_root) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(crate::error::DaemonError::Process(format!(
                    "quarantine trusted-platform parent revalidation failed for pid {parent_pid}: {error}"
                )));
            }
        };
        if current_parent_start != expected_parent_start
            || Some(current_parent_state) != parent_state
            || current_parent_metadata.uid() != uid
            || Some(current_parent_metadata.dev()) != parent_directory_device
            || Some(current_parent_metadata.ino()) != parent_directory_inode
            || read_proc_comm_and_parent(proc_root, parent_pid)? != Some(("systemd".into(), 1))
            || !process_is_exact_systemd_user_init_scope(proc_root, parent_pid, uid)?
        {
            return Ok(None);
        }
    }
    Ok(Some(TrustedPlatformProcessIdentity {
        class,
        start_time,
        state,
        uid,
        parent_pid,
        directory_device: metadata.dev(),
        directory_inode: metadata.ino(),
        parent_start_time,
        parent_state,
        parent_directory_device,
        parent_directory_inode,
        ancestor_pid: None,
        ancestor_start_time: None,
        ancestor_state: None,
        ancestor_directory_device: None,
        ancestor_directory_inode: None,
        platform_binary_device: None,
        platform_binary_inode: None,
    }))
}

fn process_is_exact_systemd_user_init_scope(
    proc_root: &Path,
    pid: i32,
    uid: u32,
) -> crate::error::Result<bool> {
    let expected = format!("0::/user.slice/user-{uid}.slice/user@{uid}.service/init.scope");
    process_has_exact_cgroup(proc_root, pid, &expected, "systemd user manager")
}

fn read_proc_comm_and_parent(
    proc_root: &Path,
    pid: i32,
) -> crate::error::Result<Option<(String, i32)>> {
    let Some(stat) = read_bounded_proc_file(
        &proc_root.join(pid.to_string()).join("stat"),
        STARTUP_ORPHAN_STAT_MAX_BYTES,
    )
    .map_err(|error| {
        crate::error::DaemonError::Process(format!(
            "startup provider stat read failed for pid {pid}: {error}"
        ))
    })?
    else {
        return Ok(None);
    };
    let stat = std::str::from_utf8(&stat).map_err(|error| {
        crate::error::DaemonError::Process(format!(
            "startup provider stat is not UTF-8 for pid {pid}: {error}"
        ))
    })?;
    let open = stat.find('(').ok_or_else(|| {
        crate::error::DaemonError::Process(format!(
            "startup provider stat comm is missing for pid {pid}"
        ))
    })?;
    let (head, tail) = stat.rsplit_once(") ").ok_or_else(|| {
        crate::error::DaemonError::Process(format!(
            "startup provider stat is malformed for pid {pid}"
        ))
    })?;
    let comm = head
        .get(open + 1..)
        .ok_or_else(|| {
            crate::error::DaemonError::Process(format!(
                "startup provider stat comm is malformed for pid {pid}"
            ))
        })?
        .to_string();
    let parent_pid = tail
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| {
            crate::error::DaemonError::Process(format!(
                "startup provider stat parent is missing for pid {pid}"
            ))
        })?
        .parse::<i32>()
        .map_err(|error| {
            crate::error::DaemonError::Process(format!(
                "startup provider stat parent is invalid for pid {pid}: {error}"
            ))
        })?;
    Ok(Some((comm, parent_pid)))
}

fn parse_startup_stamp_observation(
    environ: &[u8],
) -> crate::error::Result<StartupStampObservation> {
    let namespace_prefix = format!("{}=", rsi_common::identity::ENV_PROCESS_OWNERSHIP_NAMESPACE);
    let socket_prefix = format!("{}=", rsi_common::identity::ENV_SOCKET);
    let session_prefix = format!("{}=", rsi_common::identity::ENV_SESSION_ID);
    let invocation_prefix = format!("{}=", rsi_common::identity::ENV_MODEL_INVOCATION_ID);
    let mut observation = StartupStampObservation::default();
    for token in environ.split(|byte| *byte == 0) {
        if let Some(value) = token.strip_prefix(namespace_prefix.as_bytes()) {
            observation.namespace.token_count = observation
                .namespace
                .token_count
                .checked_add(1)
                .ok_or_else(|| {
                    crate::error::DaemonError::Process(
                        "startup provider process namespace count overflowed".into(),
                    )
                })?;
            observation.namespace.values.push(value.to_vec());
            continue;
        }
        if let Some(value) = token.strip_prefix(socket_prefix.as_bytes()) {
            observation.socket.token_count = observation
                .socket
                .token_count
                .checked_add(1)
                .ok_or_else(|| {
                    crate::error::DaemonError::Process(
                        "startup provider process socket count overflowed".into(),
                    )
                })?;
            observation.socket.values.push(value.to_vec());
            continue;
        }
        let (field, value) = if let Some(value) = token.strip_prefix(session_prefix.as_bytes()) {
            (&mut observation.session, value)
        } else if let Some(value) = token.strip_prefix(invocation_prefix.as_bytes()) {
            (&mut observation.invocation, value)
        } else {
            continue;
        };
        field.token_count = field.token_count.checked_add(1).ok_or_else(|| {
            crate::error::DaemonError::Process(
                "startup provider process stamp count overflowed".into(),
            )
        })?;
        let Some(value) = std::str::from_utf8(value).ok() else {
            field.malformed = true;
            continue;
        };
        match Uuid::parse_str(value) {
            Ok(id) if id.to_string() == value => field.canonical_values.push(id),
            _ => field.malformed = true,
        }
    }
    Ok(observation)
}

fn exact_startup_raw_field(
    pid: i32,
    name: &str,
    field: &StartupObservedRawField,
) -> crate::error::Result<Option<Vec<u8>>> {
    if field.token_count > 1 {
        return Err(crate::error::DaemonError::Process(format!(
            "startup provider process {pid} has duplicate {name} stamps"
        )));
    }
    if field.values.len() != field.token_count {
        return Err(crate::error::DaemonError::Process(format!(
            "startup provider process {pid} has malformed {name} stamp"
        )));
    }
    Ok(field.values.first().cloned())
}

fn exact_startup_stamp_field(
    pid: i32,
    name: &str,
    field: &StartupObservedStampField,
) -> crate::error::Result<Option<Uuid>> {
    if field.token_count > 1 {
        return Err(crate::error::DaemonError::Process(format!(
            "startup provider process {pid} has duplicate {name} stamps"
        )));
    }
    if field.malformed || field.canonical_values.len() != field.token_count {
        return Err(crate::error::DaemonError::Process(format!(
            "startup provider process {pid} has noncanonical {name} stamp"
        )));
    }
    Ok(field.canonical_values.first().copied())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    struct VanishedProcReader(i32);

    impl Read for VanishedProcReader {
        fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::from_raw_os_error(self.0))
        }
    }

    #[test]
    fn bounded_proc_open_and_reader_treat_task_disappearance_as_absent() {
        let esrch = io::Error::from_raw_os_error(nix::libc::ESRCH);
        assert!(
            normalize_proc_open::<File>(Err(esrch))
                .expect("proc open must recognize ESRCH as task disappearance")
                .is_none()
        );
        for errno in [nix::libc::ENOENT, nix::libc::ESRCH] {
            assert_eq!(
                read_bounded_proc_reader(VanishedProcReader(errno), 16)
                    .expect("a vanished proc task is an absent inventory entry"),
                None
            );
        }
    }

    fn write_fake_proc_identity(
        proc_root: &Path,
        pid: i32,
        comm: &str,
        parent_pid: i32,
        cgroup: &str,
    ) {
        let process_root = proc_root.join(pid.to_string());
        std::fs::create_dir_all(&process_root).expect("create fake process root");
        std::fs::write(
            process_root.join("stat"),
            format!("{pid} ({comm}) S {parent_pid}\n"),
        )
        .expect("write fake process stat");
        std::fs::write(process_root.join("cgroup"), format!("{cgroup}\n"))
            .expect("write fake process cgroup");
    }

    fn write_fake_platform_stat(
        process_root: &Path,
        pid: i32,
        comm: &str,
        parent_pid: i32,
        start_time: u64,
    ) {
        let mut fields = vec!["0".to_string(); 20];
        fields[0] = "S".into();
        fields[1] = parent_pid.to_string();
        fields[19] = start_time.to_string();
        std::fs::write(
            process_root.join("stat"),
            format!("{pid} ({comm}) {}\n", fields.join(" ")),
        )
        .expect("write fake platform stat");
    }

    fn write_fake_platform_status(
        process_root: &Path,
        pid: i32,
        real_uid: u32,
        privileged_uid: u32,
    ) {
        std::fs::write(
            process_root.join("status"),
            format!(
                "Name:\tplatform\nTgid:\t{pid}\nPid:\t{pid}\nUid:\t{real_uid}\t{privileged_uid}\t{privileged_uid}\t{privileged_uid}\n"
            ),
        )
        .expect("write fake platform status");
    }

    fn insert_running_invocation(
        store: &crate::store::Store,
        invocation_id: Uuid,
        purpose: rsi_common::model_control::ModelInvocationPurpose,
        kind: &str,
    ) {
        const NOW: &str = "2026-08-24T20:00:00.000000000Z";
        store
            .conn
            .execute(
                "INSERT INTO model_invocations
                 (id,purpose,invocation_kind,foreground,paid_risk,admission_status,status,
                  trigger_source,policy_snapshot_json,usage_confidence,created_at,started_at)
                 VALUES (?1,?2,?3,'foreground','paid_capable','admitted','running',
                         'startup-process-test','{}','unavailable',?4,?4)",
                rusqlite::params![invocation_id.to_string(), purpose.as_str(), kind, NOW],
            )
            .expect("insert durable running invocation");
    }

    fn holder_tree_fixture() -> (tempfile::TempDir, QuarantineTreeProof, PathBuf) {
        let temp = tempfile::tempdir().expect("holder tree fixture");
        let tree_root = temp.path().join("quarantine");
        std::fs::create_dir(&tree_root).expect("quarantine tree root");
        let file = tree_root.join("mapped");
        std::fs::write(&file, "holder bytes\n").expect("quarantine tree file");
        let proof = crate::sandbox::git_worktree::prove_quarantine_tree_safe(&tree_root)
            .expect("safe holder tree");
        (temp, proof, file)
    }

    #[test]
    fn quarantine_holder_test_proc_context_restores_nested_scope() {
        let outer = SyntheticQuarantineHolderProc::new();
        let inner = SyntheticQuarantineHolderProc::new();

        with_quarantine_holder_test_proc(outer.proc_root(), outer.uid(), || {
            assert_eq!(
                current_quarantine_holder_test_proc_context()
                    .expect("outer test proc context")
                    .proc_root,
                outer.proc_root()
            );
            with_quarantine_holder_test_proc(inner.proc_root(), inner.uid(), || {
                assert_eq!(
                    current_quarantine_holder_test_proc_context()
                        .expect("inner test proc context")
                        .proc_root,
                    inner.proc_root()
                );
            });
            assert_eq!(
                current_quarantine_holder_test_proc_context()
                    .expect("restored outer test proc context")
                    .proc_root,
                outer.proc_root()
            );
        });
        assert!(current_quarantine_holder_test_proc_context().is_none());
    }

    #[test]
    fn quarantine_holder_test_proc_context_restores_after_panic() {
        let outer = SyntheticQuarantineHolderProc::new();
        let inner = SyntheticQuarantineHolderProc::new();

        with_quarantine_holder_test_proc(outer.proc_root(), outer.uid(), || {
            let panic = std::panic::catch_unwind(|| {
                with_quarantine_holder_test_proc(inner.proc_root(), inner.uid(), || {
                    panic!("test context unwind");
                });
            });
            assert!(panic.is_err());
            assert_eq!(
                current_quarantine_holder_test_proc_context()
                    .expect("outer context restored after panic")
                    .proc_root,
                outer.proc_root()
            );
        });
        assert!(current_quarantine_holder_test_proc_context().is_none());
    }

    #[test]
    fn quarantine_holder_test_proc_context_is_thread_local() {
        let fixture = SyntheticQuarantineHolderProc::new();
        with_quarantine_holder_test_proc(fixture.proc_root(), fixture.uid(), || {
            let worker = std::thread::spawn(current_quarantine_holder_test_proc_context);
            assert!(
                worker
                    .join()
                    .expect("test context worker must not panic")
                    .is_none(),
                "a test proc context must not leak to another thread"
            );
        });
    }

    #[test]
    fn quarantine_holder_test_proc_context_drives_wrapper_and_fixture_holder() {
        let (_tree_temp, tree, retained) = holder_tree_fixture();
        let fixture = SyntheticQuarantineHolderProc::new();

        with_quarantine_holder_test_proc(fixture.proc_root(), fixture.uid(), || {
            prove_quarantine_has_no_untrusted_same_uid_holders(&tree)
                .expect("empty synthetic holder inventory");
        });
        fixture.add_fd_holder(&retained);
        let error = with_quarantine_holder_test_proc(fixture.proc_root(), fixture.uid(), || {
            prove_quarantine_has_no_untrusted_same_uid_holders(&tree)
                .expect_err("synthetic retained fd must block quarantine removal")
        });
        assert!(error.to_string().contains("through fd"), "{error}");
    }

    #[test]
    fn proc_map_device_identity_uses_platform_encoding() {
        let file = tempfile::NamedTempFile::new().expect("mapped file fixture");
        let metadata = file.as_file().metadata().expect("mapped file identity");
        let record = format!(
            "1000-2000 rw-s 00000000 {:x}:{:x} {} /mapped\n",
            nix::libc::major(metadata.dev() as _),
            nix::libc::minor(metadata.dev() as _),
            metadata.ino(),
        );

        let identity = parse_proc_map_identity(record.as_bytes())
            .expect("valid proc maps identity")
            .expect("nonzero mapped inode");

        assert_eq!(identity, (metadata.dev(), metadata.ino()));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn proc_map_device_identity_rejects_oversized_macos_components() {
        for (major, minor) in [(0x100, 0), (0, 0x0100_0000)] {
            assert!(
                proc_map_device_id(major, minor).is_err(),
                "oversized macOS device component {major:x}:{minor:x} must fail closed"
            );
        }
    }

    #[test]
    fn quarantine_holder_proof_detects_divergent_namespace_holders() {
        for kind in ["cwd", "fd", "mmap", "mount", "io_uring"] {
            let (tree_temp, tree, file) = holder_tree_fixture();
            let proc_temp = tempfile::tempdir().expect("fake proc root");
            let (uid, namespace_root) = initialize_fake_holder_proc(proc_temp.path());
            let cwd = if kind == "cwd" {
                tree.root()
            } else {
                namespace_root.as_path()
            };
            let process = write_fake_holder_process(proc_temp.path(), 101, &namespace_root, cwd);
            std::fs::remove_file(process.join("ns/mnt")).expect("remove shared namespace identity");
            std::fs::write(process.join("ns/mnt"), "private namespace\n")
                .expect("write divergent namespace identity");
            match kind {
                "fd" => std::os::unix::fs::symlink(&file, process.join("fd/3"))
                    .expect("fake retained fd"),
                "mmap" => {
                    let metadata = std::fs::metadata(&file).expect("mapped file identity");
                    std::fs::write(
                        process.join("maps"),
                        format!(
                            "1000-2000 rw-s 00000000 {:x}:{:x} {} {}\n",
                            nix::libc::major(metadata.dev() as _),
                            nix::libc::minor(metadata.dev() as _),
                            metadata.ino(),
                            file.display()
                        ),
                    )
                    .expect("fake shared mapping");
                }
                "mount" => {
                    std::os::unix::fs::symlink(tree.root(), namespace_root.join("alias"))
                        .expect("fake bind-mount target");
                    std::fs::write(
                        process.join("mountinfo"),
                        format!(
                            "2 1 00:00 {} /alias rw - none none rw\n",
                            tree.root().display()
                        ),
                    )
                    .expect("fake bind mount inventory");
                }
                "io_uring" => {
                    std::os::unix::fs::symlink("anon_inode:[io_uring]", process.join("fd/7"))
                        .expect("fake io_uring descriptor");
                    std::os::unix::fs::symlink(&file, namespace_root.join("registered"))
                        .expect("fake registered file namespace path");
                    std::fs::write(
                        process.join("fdinfo/7"),
                        "pos:\t0\nflags:\t02000002\nUserFiles:\t1\n    0: /registered\nUserBufs:\t0\n",
                    )
                    .expect("fake io_uring fdinfo");
                }
                "cwd" => {}
                _ => unreachable!(),
            }

            let error = prove_quarantine_has_no_untrusted_same_uid_holders_at(
                proc_temp.path(),
                uid,
                &tree,
                QuarantineHolderScanLimits::default(),
            )
            .expect_err("same-UID tree holder must retain quarantine");
            assert!(
                error.to_string().contains(kind),
                "{kind} holder evidence was not identified: {error}"
            );
            drop(tree_temp);
        }
    }

    #[test]
    fn io_uring_fdinfo_proof_fails_closed_on_ambiguous_or_bounded_inventory() {
        let (_tree_temp, tree, file) = holder_tree_fixture();
        let proc_temp = tempfile::tempdir().expect("fake proc root");
        let (_uid, namespace_root) = initialize_fake_holder_proc(proc_temp.path());
        let process =
            write_fake_holder_process(proc_temp.path(), 151, &namespace_root, &namespace_root);
        std::os::unix::fs::symlink(&file, namespace_root.join("registered"))
            .expect("fake registered file namespace path");
        let deadline = StdInstant::now() + Duration::from_secs(1);

        let outside = namespace_root.join("outside");
        std::fs::write(&outside, "outside\n").expect("outside fixed file");
        let ordinary = b"UserFiles:\t2\n    0: /outside\n    1: /registered\nUserBufs:\t0\n";
        assert!(
            process_io_uring_fdinfo_holds_tree(&process, ordinary, &tree, 2, deadline)
                .expect("canonical io_uring fdinfo"),
            "a registered quarantine inode must be detected"
        );
        assert!(
            process_io_uring_fdinfo_holds_tree(&process, ordinary, &tree, 1, deadline).is_err(),
            "the declared fixed-file table must be bounded"
        );
        for malformed in [
            b"UserBufs:\t0\n".as_slice(),
            b"UserFiles:\t1\n    0: /outside\n".as_slice(),
            b"UserFiles:\t1\n    0: /registered\\040(deleted)\nUserBufs:\t0\n".as_slice(),
            b"UserFiles:\t2\n    1: /outside\n    0: /registered\nUserBufs:\t0\n".as_slice(),
        ] {
            assert!(
                process_io_uring_fdinfo_holds_tree(&process, malformed, &tree, 2, deadline)
                    .is_err(),
                "ambiguous io_uring fdinfo must fail closed"
            );
        }

        std::os::unix::fs::symlink("anon_inode:[io_uring]", process.join("fd/9"))
            .expect("fake unreadable io_uring descriptor");
        let error = prove_quarantine_has_no_untrusted_same_uid_holders_at(
            proc_temp.path(),
            std::fs::metadata(proc_temp.path())
                .expect("fake proc identity")
                .uid(),
            &tree,
            QuarantineHolderScanLimits::default(),
        )
        .expect_err("missing io_uring fdinfo must fail closed");
        assert!(
            error.to_string().contains("fd target disappeared")
                || error.to_string().contains("fdinfo"),
            "{error}"
        );
    }

    #[test]
    fn quarantine_holder_fd_target_reuse_fails_the_pass() {
        let (_tree_temp, tree, _file) = holder_tree_fixture();
        let proc_temp = tempfile::tempdir().expect("fake proc root");
        let (uid, namespace_root) = initialize_fake_holder_proc(proc_temp.path());
        let process =
            write_fake_holder_process(proc_temp.path(), 171, &namespace_root, &namespace_root);
        let first = namespace_root.join("first");
        let second = namespace_root.join("second");
        std::fs::write(&first, "first\n").expect("first fd target");
        std::fs::write(&second, "second\n").expect("second fd target");
        let descriptor = process.join("fd/4");
        std::os::unix::fs::symlink(&first, &descriptor).expect("initial fd target");
        let replacement = descriptor.clone();
        QUARANTINE_HOLDER_FD_REVALIDATE_HOOK.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                std::fs::remove_file(&replacement).expect("remove initial fd target");
                std::os::unix::fs::symlink(&second, &replacement)
                    .expect("replace fd target during proof");
            }));
        });
        let error = prove_quarantine_has_no_untrusted_same_uid_holders_at(
            proc_temp.path(),
            uid,
            &tree,
            QuarantineHolderScanLimits::default(),
        )
        .expect_err("fd target reuse must fail the holder pass");
        assert!(error.to_string().contains("fd target changed"), "{error}");

        std::fs::remove_file(&descriptor).expect("remove replacement fd target");
        std::os::unix::fs::symlink(&first, &descriptor).expect("restored fd target");
        let replaced_path = first.clone();
        let retired_path = namespace_root.join("retired-first");
        QUARANTINE_HOLDER_FD_REVALIDATE_HOOK.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                std::fs::rename(&replaced_path, retired_path)
                    .expect("retire original same-path fd target");
                std::fs::write(replaced_path, "replacement\n")
                    .expect("replace same-path fd identity");
            }));
        });
        let error = prove_quarantine_has_no_untrusted_same_uid_holders_at(
            proc_temp.path(),
            uid,
            &tree,
            QuarantineHolderScanLimits::default(),
        )
        .expect_err("same-label fd reuse must fail the holder pass");
        assert!(error.to_string().contains("fd identity changed"), "{error}");
    }

    #[test]
    fn quarantine_holder_accepts_fd_closed_before_initial_read() {
        let (_tree_temp, tree, _file) = holder_tree_fixture();
        let proc_temp = tempfile::tempdir().expect("fake proc root");
        let (uid, namespace_root) = initialize_fake_holder_proc(proc_temp.path());
        let process =
            write_fake_holder_process(proc_temp.path(), 172, &namespace_root, &namespace_root);
        let outside = namespace_root.join("outside");
        std::fs::write(&outside, "outside\n").expect("outside fd target");
        let descriptor = process.join("fd/4");
        std::os::unix::fs::symlink(&outside, &descriptor).expect("initial fd target");
        let closing = descriptor.clone();
        QUARANTINE_HOLDER_FD_BEFORE_READ_HOOK.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                std::fs::remove_file(closing).expect("close fd before initial read");
            }));
        });

        prove_quarantine_has_no_untrusted_same_uid_holders_at(
            proc_temp.path(),
            uid,
            &tree,
            QuarantineHolderScanLimits::default(),
        )
        .expect("a descriptor proven closed before its initial read is not incomplete inventory");
    }

    #[test]
    fn quarantine_holder_accepts_fd_closed_after_identity_read() {
        let (_tree_temp, tree, _file) = holder_tree_fixture();
        let proc_temp = tempfile::tempdir().expect("fake proc root");
        let (uid, namespace_root) = initialize_fake_holder_proc(proc_temp.path());
        let process =
            write_fake_holder_process(proc_temp.path(), 173, &namespace_root, &namespace_root);
        let outside = namespace_root.join("outside");
        std::fs::write(&outside, "outside\n").expect("outside fd target");
        let descriptor = process.join("fd/4");
        std::os::unix::fs::symlink(&outside, &descriptor).expect("initial fd target");
        let closing = descriptor.clone();
        QUARANTINE_HOLDER_FD_REVALIDATE_HOOK.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                std::fs::remove_file(closing).expect("close fd after identity read");
            }));
        });

        prove_quarantine_has_no_untrusted_same_uid_holders_at(
            proc_temp.path(),
            uid,
            &tree,
            QuarantineHolderScanLimits::default(),
        )
        .expect("a descriptor proven closed during revalidation is not incomplete inventory");
    }

    #[test]
    fn quarantine_holder_accepts_fd_closed_between_revalidation_reads() {
        let (_tree_temp, tree, _file) = holder_tree_fixture();
        let proc_temp = tempfile::tempdir().expect("fake proc root");
        let (uid, namespace_root) = initialize_fake_holder_proc(proc_temp.path());
        let process =
            write_fake_holder_process(proc_temp.path(), 175, &namespace_root, &namespace_root);
        let outside = namespace_root.join("outside");
        std::fs::write(&outside, "outside\n").expect("outside fd target");
        let descriptor = process.join("fd/4");
        std::os::unix::fs::symlink(&outside, &descriptor).expect("initial fd target");
        let closing = descriptor.clone();
        QUARANTINE_HOLDER_FD_REVALIDATE_BETWEEN_READS_HOOK.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                std::fs::remove_file(closing).expect("close fd after revalidated target read");
            }));
        });

        prove_quarantine_has_no_untrusted_same_uid_holders_at(
            proc_temp.path(),
            uid,
            &tree,
            QuarantineHolderScanLimits::default(),
        )
        .expect("a final both-ENOENT observation must confirm the fd closed");
    }

    #[test]
    fn quarantine_holder_accepts_fd_closed_after_missing_revalidation_target() {
        let (_tree_temp, tree, _file) = holder_tree_fixture();
        let proc_temp = tempfile::tempdir().expect("fake proc root");
        let (uid, namespace_root) = initialize_fake_holder_proc(proc_temp.path());
        let process =
            write_fake_holder_process(proc_temp.path(), 176, &namespace_root, &namespace_root);
        let outside = namespace_root.join("outside");
        std::fs::write(&outside, "outside\n").expect("outside fd target");
        let descriptor = process.join("fd/4");
        std::os::unix::fs::symlink(&outside, &descriptor).expect("initial fd target");
        let removed_before_target = descriptor.clone();
        QUARANTINE_HOLDER_FD_REVALIDATE_HOOK.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                std::fs::remove_file(removed_before_target)
                    .expect("close fd before revalidated target read");
            }));
        });
        let reopened_before_identity = descriptor.clone();
        let reopened_target = outside.clone();
        QUARANTINE_HOLDER_FD_REVALIDATE_BETWEEN_READS_HOOK.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                std::os::unix::fs::symlink(reopened_target, reopened_before_identity)
                    .expect("reopen fd before revalidated identity read");
            }));
        });
        let closing = descriptor.clone();
        QUARANTINE_HOLDER_FD_FINAL_CONFIRM_HOOK.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                std::fs::remove_file(closing).expect("close fd before final confirmation");
            }));
        });

        prove_quarantine_has_no_untrusted_same_uid_holders_at(
            proc_temp.path(),
            uid,
            &tree,
            QuarantineHolderScanLimits::default(),
        )
        .expect("a final both-ENOENT observation must confirm the reopened fd closed");
    }

    #[test]
    fn quarantine_holder_partial_close_reuse_to_nonholder_fails() {
        let (_tree_temp, tree, _file) = holder_tree_fixture();
        let proc_temp = tempfile::tempdir().expect("fake proc root");
        let (uid, namespace_root) = initialize_fake_holder_proc(proc_temp.path());
        let process =
            write_fake_holder_process(proc_temp.path(), 177, &namespace_root, &namespace_root);
        let first = namespace_root.join("first");
        let second = namespace_root.join("second");
        std::fs::write(&first, "first\n").expect("first fd target");
        std::fs::write(&second, "second\n").expect("second fd target");
        let descriptor = process.join("fd/4");
        std::os::unix::fs::symlink(&first, &descriptor).expect("initial fd target");
        let closing = descriptor.clone();
        QUARANTINE_HOLDER_FD_REVALIDATE_BETWEEN_READS_HOOK.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                std::fs::remove_file(closing).expect("close fd after revalidated target read");
            }));
        });
        let reused = descriptor.clone();
        QUARANTINE_HOLDER_FD_FINAL_CONFIRM_HOOK.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                std::os::unix::fs::symlink(second, reused)
                    .expect("reuse fd number for a nonholder");
            }));
        });

        let error = prove_quarantine_has_no_untrusted_same_uid_holders_at(
            proc_temp.path(),
            uid,
            &tree,
            QuarantineHolderScanLimits::default(),
        )
        .expect_err("a live or reused nonholder fd must fail the close proof");
        assert!(error.to_string().contains("live or was reused"), "{error}");
    }

    #[test]
    fn quarantine_holder_partial_close_reuse_to_holder_is_holder_winning() {
        let (_tree_temp, tree, retained) = holder_tree_fixture();
        let proc_temp = tempfile::tempdir().expect("fake proc root");
        let (uid, namespace_root) = initialize_fake_holder_proc(proc_temp.path());
        let process =
            write_fake_holder_process(proc_temp.path(), 178, &namespace_root, &namespace_root);
        let outside = namespace_root.join("outside");
        std::fs::write(&outside, "outside\n").expect("outside fd target");
        let descriptor = process.join("fd/4");
        std::os::unix::fs::symlink(&outside, &descriptor).expect("initial fd target");
        let closing = descriptor.clone();
        QUARANTINE_HOLDER_FD_REVALIDATE_BETWEEN_READS_HOOK.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                std::fs::remove_file(closing).expect("close fd after revalidated target read");
            }));
        });
        let reused = descriptor.clone();
        QUARANTINE_HOLDER_FD_FINAL_CONFIRM_HOOK.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                std::os::unix::fs::symlink(retained, reused)
                    .expect("reuse fd number for quarantine holder");
            }));
        });

        let error = prove_quarantine_has_no_untrusted_same_uid_holders_at(
            proc_temp.path(),
            uid,
            &tree,
            QuarantineHolderScanLimits::default(),
        )
        .expect_err("a holder observed during final confirmation must retain quarantine");
        assert!(error.to_string().contains("through fd"), "{error}");
    }

    #[test]
    fn quarantine_holder_confirmed_close_preserves_prior_fdinfo_failure() {
        let (_tree_temp, tree, _file) = holder_tree_fixture();
        let proc_temp = tempfile::tempdir().expect("fake proc root");
        let (uid, namespace_root) = initialize_fake_holder_proc(proc_temp.path());
        let process =
            write_fake_holder_process(proc_temp.path(), 179, &namespace_root, &namespace_root);
        let descriptor = process.join("fd/4");
        std::os::unix::fs::symlink("anon_inode:[io_uring]", &descriptor)
            .expect("fake io_uring descriptor");
        std::fs::write(process.join("fdinfo/4"), "UserFiles:\t0\n")
            .expect("malformed io_uring fdinfo");
        let closing = descriptor.clone();
        QUARANTINE_HOLDER_FD_REVALIDATE_BETWEEN_READS_HOOK.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                std::fs::remove_file(closing)
                    .expect("close io_uring fd after revalidated target read");
            }));
        });

        let error = prove_quarantine_has_no_untrusted_same_uid_holders_at(
            proc_temp.path(),
            uid,
            &tree,
            QuarantineHolderScanLimits::default(),
        )
        .expect_err("a confirmed close must not erase prior malformed fdinfo evidence");
        assert!(error.to_string().contains("io_uring fdinfo"), "{error}");
    }

    #[test]
    fn quarantine_holder_fd_number_reuse_opened_to_holder_is_holder_winning() {
        let (_tree_temp, tree, retained) = holder_tree_fixture();
        let proc_temp = tempfile::tempdir().expect("fake proc root");
        let (uid, namespace_root) = initialize_fake_holder_proc(proc_temp.path());
        let process =
            write_fake_holder_process(proc_temp.path(), 174, &namespace_root, &namespace_root);
        let outside = namespace_root.join("outside");
        std::fs::write(&outside, "outside\n").expect("outside fd target");
        let descriptor = process.join("fd/4");
        std::os::unix::fs::symlink(&outside, &descriptor).expect("initial fd target");
        let reused = descriptor.clone();
        QUARANTINE_HOLDER_FD_REVALIDATE_HOOK.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                std::fs::remove_file(&reused).expect("close initial fd");
                std::os::unix::fs::symlink(retained, reused)
                    .expect("reuse fd number for quarantine holder");
            }));
        });

        let error = prove_quarantine_has_no_untrusted_same_uid_holders_at(
            proc_temp.path(),
            uid,
            &tree,
            QuarantineHolderScanLimits::default(),
        )
        .expect_err("a reused fd number opened to the quarantine must retain it");
        assert!(error.to_string().contains("through fd"), "{error}");
    }

    #[test]
    fn quarantine_holder_proof_requires_two_empty_passes_and_stable_task_namespace() {
        let (_tree_temp, tree, file) = holder_tree_fixture();
        let proc_temp = tempfile::tempdir().expect("fake proc root");
        let (uid, namespace_root) = initialize_fake_holder_proc(proc_temp.path());
        let process =
            write_fake_holder_process(proc_temp.path(), 202, &namespace_root, &namespace_root);

        let first_proof = prove_quarantine_has_no_untrusted_same_uid_holders_at(
            proc_temp.path(),
            uid,
            &tree,
            QuarantineHolderScanLimits::default(),
        )
        .expect("two complete empty holder passes");
        assert_eq!(first_proof.fixed_point_passes(), 2);
        assert!(first_proof.evidence_digest().starts_with("sha256:"));
        let replayed_proof = prove_quarantine_has_no_untrusted_same_uid_holders_at(
            proc_temp.path(),
            uid,
            &tree,
            QuarantineHolderScanLimits::default(),
        )
        .expect("replay identical empty holder proof");
        assert_eq!(replayed_proof, first_proof);

        let late_fd = process.join("fd/9");
        let late_file = file.clone();
        QUARANTINE_HOLDER_BETWEEN_PASSES_HOOK.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                std::os::unix::fs::symlink(late_file, late_fd).expect("late retained descriptor");
            }));
        });
        assert!(
            prove_quarantine_has_no_untrusted_same_uid_holders_at(
                proc_temp.path(),
                uid,
                &tree,
                QuarantineHolderScanLimits::default(),
            )
            .is_err(),
            "a holder introduced between empty passes must be observed"
        );
        std::fs::remove_file(process.join("fd/9")).expect("remove late fd fixture");

        std::fs::remove_file(process.join("ns/mnt")).expect("remove shared namespace marker");
        std::fs::write(process.join("ns/mnt"), "private namespace\n")
            .expect("divergent namespace marker");
        let divergent_proof = prove_quarantine_has_no_untrusted_same_uid_holders_at(
            proc_temp.path(),
            uid,
            &tree,
            QuarantineHolderScanLimits::default(),
        )
        .expect("a stable divergent namespace with complete empty inventories is safe");
        assert_eq!(divergent_proof, first_proof);
    }

    #[test]
    fn quarantine_holder_proof_rejects_mount_namespace_drift() {
        let (_tree_temp, tree, _file) = holder_tree_fixture();
        let proc_temp = tempfile::tempdir().expect("fake proc root");
        let (uid, namespace_root) = initialize_fake_holder_proc(proc_temp.path());
        let process =
            write_fake_holder_process(proc_temp.path(), 203, &namespace_root, &namespace_root);
        let namespace = process.join("ns/mnt");
        QUARANTINE_HOLDER_MOUNT_NAMESPACE_REVALIDATE_HOOK.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                std::fs::remove_file(&namespace).expect("remove initial namespace identity");
                std::fs::write(&namespace, "replacement namespace\n")
                    .expect("replace namespace identity");
            }));
        });

        let error = prove_quarantine_has_no_untrusted_same_uid_holders_at(
            proc_temp.path(),
            uid,
            &tree,
            QuarantineHolderScanLimits::default(),
        )
        .expect_err("mount namespace drift during inventory must fail closed");
        assert!(
            error.to_string().contains("mount namespace drifted"),
            "{error}"
        );
    }

    #[test]
    fn quarantine_holder_proof_fails_closed_on_proc_denial_and_work_bounds() {
        let (_tree_temp, tree, _file) = holder_tree_fixture();
        let proc_temp = tempfile::tempdir().expect("fake proc root");
        let (uid, namespace_root) = initialize_fake_holder_proc(proc_temp.path());
        let process =
            write_fake_holder_process(proc_temp.path(), 303, &namespace_root, &namespace_root);
        std::fs::remove_file(process.join("maps")).expect("remove maps inventory");
        assert!(
            prove_quarantine_has_no_untrusted_same_uid_holders_at(
                proc_temp.path(),
                uid,
                &tree,
                QuarantineHolderScanLimits::default(),
            )
            .is_err(),
            "an unreadable live inventory must fail closed"
        );
        std::fs::write(process.join("maps"), "").expect("restore maps inventory");
        assert!(
            prove_quarantine_has_no_untrusted_same_uid_holders_at(
                proc_temp.path(),
                uid,
                &tree,
                QuarantineHolderScanLimits {
                    max_proc_entries: 0,
                    ..QuarantineHolderScanLimits::default()
                },
            )
            .is_err(),
            "a process work-bound overrun must fail closed"
        );
        assert!(
            prove_quarantine_has_no_untrusted_same_uid_holders_at(
                proc_temp.path(),
                uid,
                &tree,
                QuarantineHolderScanLimits {
                    max_task_entries: 0,
                    ..QuarantineHolderScanLimits::default()
                },
            )
            .is_err(),
            "a task work-bound overrun must fail closed"
        );
        assert!(
            prove_quarantine_has_no_untrusted_same_uid_holders_at(
                proc_temp.path(),
                uid,
                &tree,
                QuarantineHolderScanLimits {
                    max_maps_records: 0,
                    ..QuarantineHolderScanLimits::default()
                },
            )
            .is_ok(),
            "an empty maps inventory remains within a zero-record bound"
        );
        assert!(
            prove_quarantine_has_no_untrusted_same_uid_holders_at(
                proc_temp.path(),
                uid,
                &tree,
                QuarantineHolderScanLimits {
                    total_deadline: Duration::ZERO,
                    ..QuarantineHolderScanLimits::default()
                },
            )
            .is_err(),
            "an exhausted holder-proof deadline must fail closed"
        );
    }

    #[test]
    fn quarantine_holder_scans_live_siblings_after_leader_zombie() {
        let (_tree_temp, tree, file) = holder_tree_fixture();
        let proc_temp = tempfile::tempdir().expect("fake proc root");
        let (uid, namespace_root) = initialize_fake_holder_proc(proc_temp.path());
        write_fake_holder_task(
            proc_temp.path(),
            404,
            404,
            &namespace_root,
            &namespace_root,
            b'Z',
            5001,
        );
        let worker = write_fake_holder_task(
            proc_temp.path(),
            404,
            405,
            &namespace_root,
            &namespace_root,
            b'S',
            5002,
        );
        std::os::unix::fs::symlink(file, worker.join("fd/8")).expect("worker-private retained fd");

        let error = prove_quarantine_has_no_untrusted_same_uid_holders_at(
            proc_temp.path(),
            uid,
            &tree,
            QuarantineHolderScanLimits::default(),
        )
        .expect_err("a live sibling must be scanned after its leader becomes a zombie");
        assert!(error.to_string().contains("tid 405"), "{error}");
    }

    #[test]
    fn quarantine_holder_rejects_ambiguous_task_group_binding() {
        let (_tree_temp, tree, _file) = holder_tree_fixture();
        let proc_temp = tempfile::tempdir().expect("fake proc root");
        let (uid, namespace_root) = initialize_fake_holder_proc(proc_temp.path());
        let task =
            write_fake_holder_process(proc_temp.path(), 406, &namespace_root, &namespace_root);
        std::fs::write(
            task.join("status"),
            format!("Pid:\t406\nTgid:\t999\nUid:\t{uid}\t{uid}\t{uid}\t{uid}\n"),
        )
        .expect("drifting task status");

        let error = prove_quarantine_has_no_untrusted_same_uid_holders_at(
            proc_temp.path(),
            uid,
            &tree,
            QuarantineHolderScanLimits::default(),
        )
        .expect_err("a task may not inherit a different TGID's inventory or trust class");
        assert!(
            error.to_string().contains("status identity drifted"),
            "{error}"
        );
    }

    #[test]
    fn trusted_systemd_permission_exemptions_are_explicit_and_holder_losing() {
        let (_tree_temp, tree, _file) = holder_tree_fixture();
        let proc_temp = tempfile::tempdir().expect("fake proc root");
        let (uid, namespace_root) = initialize_fake_holder_proc(proc_temp.path());
        let task =
            write_fake_holder_process(proc_temp.path(), 407, &namespace_root, &namespace_root);
        let process = proc_temp.path().join("407");
        let mut stat_fields = vec!["0".to_string(); 20];
        stat_fields[0] = "S".into();
        stat_fields[1] = "1".into();
        stat_fields[19] = "6001".into();
        std::fs::write(
            process.join("stat"),
            format!("407 (systemd) {}\n", stat_fields.join(" ")),
        )
        .expect("trusted systemd stat");
        std::fs::write(
            process.join("cgroup"),
            format!("0::/user.slice/user-{uid}.slice/user@{uid}.service/init.scope\n"),
        )
        .expect("trusted systemd cgroup");

        let denied = proc_temp.path().join("denied");
        std::fs::create_dir(&denied).expect("denied inventory parent");
        std::fs::write(denied.join("target"), "denied\n").expect("denied inventory target");
        std::fs::remove_file(task.join("cwd")).expect("replace cwd inventory");
        std::fs::remove_file(task.join("root")).expect("replace root inventory");
        std::fs::remove_file(task.join("ns/mnt")).expect("replace namespace inventory");
        std::os::unix::fs::symlink(denied.join("target"), task.join("cwd"))
            .expect("denied cwd inventory");
        std::os::unix::fs::symlink(denied.join("target"), task.join("root"))
            .expect("denied root inventory");
        std::os::unix::fs::symlink(denied.join("target"), task.join("ns/mnt"))
            .expect("denied namespace inventory");
        std::fs::set_permissions(&denied, std::fs::Permissions::from_mode(0o000))
            .expect("deny inventory traversal");
        std::fs::set_permissions(task.join("fd"), std::fs::Permissions::from_mode(0o000))
            .expect("deny fd inventory");
        std::fs::set_permissions(task.join("maps"), std::fs::Permissions::from_mode(0o000))
            .expect("deny maps inventory");

        let proof = prove_quarantine_has_no_untrusted_same_uid_holders_at(
            proc_temp.path(),
            uid,
            &tree,
            QuarantineHolderScanLimits::default(),
        )
        .expect("exact systemd class may record permission-only platform exemptions");
        assert_eq!(proof.trusted_platform_exemptions(), 2);
        assert!(
            proof
                .trusted_platform_exemptions_digest()
                .starts_with("sha256:")
        );

        std::fs::write(task.join("mountinfo"), "2 1 00:00 / / rw\n")
            .expect("malformed trusted mount inventory");
        let error = prove_quarantine_has_no_untrusted_same_uid_holders_at(
            proc_temp.path(),
            uid,
            &tree,
            QuarantineHolderScanLimits::default(),
        )
        .expect_err("malformed readable inventory may not receive a platform exemption");
        assert!(
            error.to_string().contains("mount inventory proof"),
            "{error}"
        );

        std::fs::write(
            task.join("mountinfo"),
            format!(
                "2 1 00:00 {} {} rw - none none rw\n",
                tree.root().display(),
                tree.root().display()
            ),
        )
        .expect("lexical quarantine mount");
        let error = prove_quarantine_has_no_untrusted_same_uid_holders_at(
            proc_temp.path(),
            uid,
            &tree,
            QuarantineHolderScanLimits::default(),
        )
        .expect_err("an observed holder must win over trusted-platform permission exemptions");
        assert!(error.to_string().contains("mount"), "{error}");

        std::fs::set_permissions(&denied, std::fs::Permissions::from_mode(0o700))
            .expect("restore denied fixture parent");
        std::fs::set_permissions(task.join("fd"), std::fs::Permissions::from_mode(0o700))
            .expect("restore fd fixture");
        std::fs::set_permissions(task.join("maps"), std::fs::Permissions::from_mode(0o600))
            .expect("restore maps fixture");
    }

    #[test]
    fn portal_fusermount_exemption_requires_exact_privilege_argv_cgroup_and_ancestry() {
        let proc_temp = tempfile::tempdir().expect("fake portal proc root");
        let proc_root = proc_temp.path();
        let uid = std::fs::metadata(proc_root).expect("fake proc uid").uid();
        let privileged_uid = uid;
        let manager_pid = 700;
        let portal_pid = 701;
        let helper_pid = 702;
        let manager_root = proc_root.join(manager_pid.to_string());
        let portal_root = proc_root.join(portal_pid.to_string());
        let helper_root = proc_root.join(helper_pid.to_string());
        std::fs::create_dir_all(&manager_root).expect("fake manager root");
        std::fs::create_dir_all(&portal_root).expect("fake portal root");
        std::fs::create_dir_all(&helper_root).expect("fake helper root");

        write_fake_platform_stat(&manager_root, manager_pid, "systemd", 1, 10_001);
        std::fs::write(
            manager_root.join("cgroup"),
            format!("0::/user.slice/user-{uid}.slice/user@{uid}.service/init.scope\n"),
        )
        .expect("fake manager cgroup");

        let portal_binary = proc_temp.path().join("xdg-document-portal");
        std::fs::write(&portal_binary, "trusted portal binary\n").expect("fake portal binary");
        write_fake_platform_stat(
            &portal_root,
            portal_pid,
            "xdg-document-po",
            manager_pid,
            10_002,
        );
        write_fake_platform_status(&portal_root, portal_pid, uid, uid);
        std::fs::write(
            portal_root.join("cgroup"),
            format!("{}\n", portal_service_cgroup(uid)),
        )
        .expect("fake portal cgroup");
        std::fs::write(
            portal_root.join("cmdline"),
            nul_terminated_path(&portal_binary),
        )
        .expect("fake portal argv");
        std::os::unix::fs::symlink(&portal_binary, portal_root.join("exe"))
            .expect("fake portal exe");

        write_fake_platform_stat(&helper_root, helper_pid, "fusermount3", portal_pid, 10_003);
        write_fake_platform_status(&helper_root, helper_pid, uid, privileged_uid);
        std::fs::write(
            helper_root.join("cgroup"),
            format!("{}\n", portal_service_cgroup(uid)),
        )
        .expect("fake helper cgroup");
        let exact_cmdline = portal_fuse_mount_helper_cmdline(uid);
        std::fs::write(helper_root.join("cmdline"), &exact_cmdline).expect("fake helper argv");
        let baseline = PortalTrustBaseline {
            privileged_uid,
            portal_executable: &portal_binary,
        };
        let exact = authenticate_portal_fuse_mount_helper_at(proc_root, helper_pid, uid, baseline)
            .expect("authenticate exact fake portal helper")
            .expect("exact fake portal helper is trusted");
        assert_eq!(
            exact.class,
            TrustedPlatformProcessClass::PortalFuseMountHelper
        );
        assert_eq!(exact.ancestor_pid, Some(manager_pid));
        assert!(exact.platform_binary_inode.is_some());
        let mut counts = TrustedPlatformExemptionCounts::default();
        counts
            .record(
                TrustedPlatformProcessClass::PortalFuseMountHelper,
                1_u8 << QuarantineInventoryClass::Cwd as u8,
            )
            .expect("record portal helper exemption");
        assert_eq!(counts.total().expect("portal helper count"), 1);
        assert_ne!(
            counts.digest(2),
            TrustedPlatformExemptionCounts::default().digest(2)
        );

        for hostile_cmdline in [
            b"fusermount3\0-o\0rw\0--\0/tmp/quarantine\0".as_slice(),
            b"fusermount3\0-o\0rw,nosuid,nodev,fsname=portal,auto_unmount,subtype=portal\0--\0/run/user/0/doc\0extra\0"
                .as_slice(),
            exact_cmdline.strip_suffix(&[0]).expect("trailing NUL"),
        ] {
            std::fs::write(helper_root.join("cmdline"), hostile_cmdline)
                .expect("hostile helper argv");
            assert!(
                authenticate_portal_fuse_mount_helper_at(
                    proc_root,
                    helper_pid,
                    uid,
                    baseline,
                )
                .expect("reject hostile helper argv")
                .is_none(),
                "helper argv must match byte-for-byte"
            );
        }
        std::fs::write(helper_root.join("cmdline"), &exact_cmdline).expect("restore helper argv");
        std::fs::write(
            helper_root.join("cgroup"),
            format!("{}\n0::/hostile.scope\n", portal_service_cgroup(uid)),
        )
        .expect("hostile helper cgroup");
        assert!(
            authenticate_portal_fuse_mount_helper_at(proc_root, helper_pid, uid, baseline)
                .expect("reject hostile helper cgroup")
                .is_none()
        );
        std::fs::write(
            helper_root.join("cgroup"),
            format!("{}\n", portal_service_cgroup(uid)),
        )
        .expect("restore helper cgroup");
        std::fs::write(
            portal_root.join("cmdline"),
            [nul_terminated_path(&portal_binary), b"extra\0".to_vec()].concat(),
        )
        .expect("hostile portal argv");
        assert!(
            authenticate_portal_fuse_mount_helper_at(proc_root, helper_pid, uid, baseline)
                .expect("reject hostile portal argv")
                .is_none()
        );
        std::fs::write(
            portal_root.join("cmdline"),
            nul_terminated_path(&portal_binary),
        )
        .expect("restore portal argv");
        write_fake_platform_stat(&portal_root, portal_pid, "xdg-document-po", 999, 10_002);
        assert!(
            authenticate_portal_fuse_mount_helper_at(proc_root, helper_pid, uid, baseline)
                .expect("reject hostile portal ancestry")
                .is_none()
        );
        write_fake_platform_stat(
            &portal_root,
            portal_pid,
            "xdg-document-po",
            manager_pid,
            10_002,
        );
        std::fs::write(
            manager_root.join("cgroup"),
            format!(
                "0::/user.slice/user-{uid}.slice/user@{uid}.service/init.scope\n0::/hostile.scope\n"
            ),
        )
        .expect("hostile manager cgroup");
        assert!(
            authenticate_portal_fuse_mount_helper_at(proc_root, helper_pid, uid, baseline)
                .expect("reject hostile manager ancestry")
                .is_none()
        );

        assert!(portal_fuse_mount_helper_credentials_are_exact(
            ProcTaskCredentials {
                real: 1000,
                effective: 0,
                saved: 0,
                filesystem: 0,
            },
            1000,
            0,
        ));
        for credentials in [
            ProcTaskCredentials {
                real: 1000,
                effective: 1000,
                saved: 1000,
                filesystem: 1000,
            },
            ProcTaskCredentials {
                real: 1000,
                effective: 0,
                saved: 0,
                filesystem: 1000,
            },
            ProcTaskCredentials {
                real: 1001,
                effective: 0,
                saved: 0,
                filesystem: 0,
            },
        ] {
            assert!(!portal_fuse_mount_helper_credentials_are_exact(
                credentials,
                1000,
                0,
            ));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn live_exact_portal_fusermount_helper_does_not_block_an_empty_tree_proof() {
        let proc_root = Path::new("/proc");
        let uid = std::fs::metadata(proc_root.join("self"))
            .expect("live self identity")
            .uid();
        let mut exact_helper = None;
        for entry in std::fs::read_dir(proc_root).expect("live proc inventory") {
            let entry = entry.expect("live proc entry");
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|value| value.parse::<i32>().ok())
            else {
                continue;
            };
            if !matches!(
                read_proc_comm_and_parent(proc_root, pid),
                Ok(Some((ref comm, _))) if comm == "fusermount3"
            ) {
                continue;
            }
            if authenticate_portal_fuse_mount_helper(proc_root, pid, uid)
                .expect("authenticate live platform candidate")
                .is_some()
            {
                exact_helper = Some(pid);
                break;
            }
        }
        let Some(_pid) = exact_helper else {
            return;
        };
        let (_temp, tree, _file) = holder_tree_fixture();
        let proof = prove_quarantine_has_no_untrusted_same_uid_holders_at(
            proc_root,
            uid,
            &tree,
            QuarantineHolderScanLimits::default(),
        )
        .expect("the exact live portal helper receives a recorded permission-only exemption");
        assert!(proof.trusted_platform_exemptions() >= 2);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn quarantine_holder_scans_a_thread_private_file_table_when_unshare_is_available() {
        let (_tree_temp, tree, file) = holder_tree_fixture();
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            // SAFETY: CLONE_FILES affects only the calling thread's descriptor
            // table and the test keeps the thread alive until proof completes.
            if unsafe { nix::libc::unshare(nix::libc::CLONE_FILES) } != 0 {
                ready_tx
                    .send(Err(std::io::Error::last_os_error()))
                    .expect("report unshare result");
                return;
            }
            let retained = File::open(file).expect("open thread-private quarantine fd");
            // SAFETY: gettid has no pointer arguments or memory side effects.
            let tid = unsafe { nix::libc::syscall(nix::libc::SYS_gettid) as i32 };
            ready_tx.send(Ok(tid)).expect("report worker tid");
            release_rx.recv().expect("release worker");
            drop(retained);
        });
        let tid = match ready_rx.recv().expect("receive unshare result") {
            Ok(tid) => tid,
            Err(error)
                if error.raw_os_error().is_some_and(|code| {
                    [nix::libc::EPERM, nix::libc::EINVAL, nix::libc::ENOSYS].contains(&code)
                }) =>
            {
                worker.join().expect("join unsupported unshare worker");
                return;
            }
            Err(error) => panic!("unexpected CLONE_FILES unshare failure: {error}"),
        };
        let tgid = std::process::id() as i32;
        let task_root = Path::new("/proc")
            .join(tgid.to_string())
            .join("task")
            .join(tid.to_string());
        let (start_time, _) = read_quarantine_task_start_identity(&task_root, tgid, tid)
            .expect("read worker task identity")
            .expect("live worker task identity");
        let status =
            read_quarantine_task_status(&task_root, tgid, tid, QUARANTINE_HOLDER_STATUS_MAX_BYTES)
                .expect("read worker task status")
                .expect("live worker task status");
        let mut fd_entries = 0;
        let result = scan_quarantine_task_inventory(
            Path::new("/proc"),
            &task_root,
            tgid,
            tid,
            start_time,
            status.credentials,
            status.credentials.filesystem,
            &tree,
            StdInstant::now() + Duration::from_secs(5),
            QuarantineHolderScanLimits::default(),
            &mut fd_entries,
            None,
            &mut TrustedPlatformExemptionCounts::default(),
        );
        release_tx.send(()).expect("release worker");
        worker.join().expect("join unshared file-table worker");
        let error = result.expect_err("thread-private fd inventory must be scanned");
        assert!(error.to_string().contains("fd"), "{error}");
    }

    #[test]
    fn grace_exceeds_poll() {
        // Sanity: the grace window must span multiple poll ticks so a wedged
        // provider is re-checked before escalation, not killed on tick zero.
        assert!(TEARDOWN_KILL_GRACE > TEARDOWN_KILL_POLL);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn reap_unknown_session_reaps_nothing() {
        let fixture = StartupReaperFixture::new();
        let ownership = StartupProcessOwnership::for_sessions_in_domain(
            HashSet::from([Uuid::new_v4()]),
            fixture.domain(),
        );
        // A fresh, never-stamped session id matches no process in the explicit
        // empty view. The production wrapper is covered separately.
        let reaped = reap_startup_owned_orphans_at(&ownership, fixture.proc_root(), None)
            .expect("bounded filtered runtime inventory");
        assert_eq!(reaped, 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn startup_reaper_fixtures_have_unique_domains_and_filtered_proc_views() {
        let local = StartupReaperFixture::new();
        let foreign = StartupReaperFixture::new();
        assert_ne!(local.domain().namespace, foreign.domain().namespace);
        let child = local.spawn(Some(Uuid::new_v4()), None, &local.domain(), true);
        let entries = std::fs::read_dir(local.proc_root())
            .expect("read filtered proc")
            .filter_map(|entry| {
                entry
                    .expect("fixture proc entry")
                    .file_name()
                    .into_string()
                    .ok()
            })
            .collect::<Vec<_>>();
        assert!(entries.contains(&child.pid().to_string()));
        assert!(!foreign.proc_root().join(child.pid().to_string()).exists());
        drop(child);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn startup_reaper_child_guard_kills_and_waits_on_unwind() {
        let fixture = StartupReaperFixture::new();
        let child = fixture.spawn(Some(Uuid::new_v4()), None, &fixture.domain(), true);
        let pid = child.pid();
        let entry = fixture.proc_root().join(pid.to_string());
        let _ = std::panic::catch_unwind(|| {
            let _child = child;
            panic!("fixture unwind");
        });
        assert!(!entry.exists());
        let mut status = 0;
        // SAFETY: this probes only the reaped child PID captured above.
        assert_eq!(
            unsafe { nix::libc::waitpid(pid, &mut status, nix::libc::WNOHANG) },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(nix::libc::ECHILD)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn startup_reaper_runtime_session_readiness_proves_exact_full_tuple() {
        let fixture = StartupReaperFixture::new();
        let session_id = Uuid::new_v4();
        let mut child = fixture.spawn_runtime_session(session_id);
        let environment = std::fs::read(format!("/proc/{}/environ", child.pid()))
            .expect("read ready runtime fixture environment");
        let stamps = parse_startup_stamp_observation(&environment).expect("parse ready stamps");
        let expected = StartupProcessDomain::default_domain();
        assert_eq!(stamps.namespace.values, vec![expected.namespace]);
        assert_eq!(stamps.socket.values, vec![expected.legacy_socket]);
        assert_eq!(stamps.session.canonical_values, vec![session_id]);
        assert!(!stamps.session.malformed);
        assert_eq!(stamps.invocation.token_count, 0);
        assert!(stamps.invocation.canonical_values.is_empty());
        assert!(child.is_alive("exact-ready fixture child remains live"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn startup_reaper_child_guard_owns_forced_readiness_failure_cleanup() {
        let fixture = StartupReaperFixture::new();
        let session_id = Uuid::new_v4();
        let error = match fixture.spawn_with_options(
            Some(session_id),
            None,
            &StartupProcessDomain::default_domain(),
            true,
            StdInstant::now(),
            None,
        ) {
            Ok(_) => panic!("zero deadline must fail readiness"),
            Err(error) => error,
        };
        assert!(error.message.contains("never became ready"));
        let pid = error.pid.expect("readiness failure child pid");
        assert!(error.entry.is_none());
        assert!(!fixture.proc_root().join(pid.to_string()).exists());
        assert_eq!(
            nix::sys::wait::waitpid(
                nix::unistd::Pid::from_raw(pid),
                Some(nix::sys::wait::WaitPidFlag::WNOHANG),
            ),
            Err(nix::errno::Errno::ECHILD)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn startup_reaper_child_guard_owns_forced_registration_failure_cleanup() {
        let fixture = StartupReaperFixture::new();
        let error = match fixture.spawn_with_options(
            Some(Uuid::new_v4()),
            None,
            &StartupProcessDomain::default_domain(),
            true,
            StdInstant::now() + Duration::from_secs(2),
            Some(StartupReaperFixtureRegistrationFault::AfterStatLink),
        ) {
            Ok(_) => panic!("injected registration failure must fail setup"),
            Err(error) => error,
        };
        assert!(error.message.contains("after stat link"));
        let pid = error.pid.expect("registration failure child pid");
        assert!(
            !error.entry.expect("registration failure entry").exists(),
            "child drop removes partial fixture proc entry"
        );
        assert_eq!(
            nix::sys::wait::waitpid(
                nix::unistd::Pid::from_raw(pid),
                Some(nix::sys::wait::WaitPidFlag::WNOHANG),
            ),
            Err(nix::errno::Errno::ECHILD)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn runtime_reap_proc_root_permits_are_session_keyed_nested_cross_thread_and_unwind_safe() {
        let fixture = StartupReaperFixture::new();
        let foreign_fixture = StartupReaperFixture::new();
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();

        let outer = fixture
            .scoped_runtime_reap_root(first)
            .expect("register outer fixture root");
        let inner = fixture
            .scoped_runtime_reap_root(first)
            .expect("same-fixture nesting is allowed");
        let second_guard = fixture
            .scoped_runtime_reap_root(second)
            .expect("independent session key is allowed");
        assert!(
            foreign_fixture.scoped_runtime_reap_root(first).is_err(),
            "same-session foreign fixture collision is rejected"
        );

        let inner_lease = take_runtime_reap_proc_root(first).expect("newest nested permit leases");
        assert_eq!(inner_lease.proc_root(), fixture.proc_root());
        drop(inner);
        drop(inner_lease);
        let outer_lease =
            take_runtime_reap_proc_root(first).expect("outer permit remains after inner drop");
        assert_eq!(outer_lease.proc_root(), fixture.proc_root());
        drop(outer);
        drop(outer_lease);
        let second_lease =
            take_runtime_reap_proc_root(second).expect("second key stays independent");
        drop(second_guard);
        drop(second_lease);
        assert!(take_runtime_reap_proc_root(first).is_none());
        assert!(take_runtime_reap_proc_root(second).is_none());

        let unwind_id = Uuid::new_v4();
        let _ = std::panic::catch_unwind(|| {
            let _guard = fixture
                .scoped_runtime_reap_root(unwind_id)
                .expect("register unwind fixture root");
            panic!("exercise permit guard unwind");
        });
        assert!(take_runtime_reap_proc_root(unwind_id).is_none());

        let thread_id = Uuid::new_v4();
        let thread_fixture = StartupReaperFixture::new();
        let thread_guard = thread_fixture
            .scoped_runtime_reap_root(thread_id)
            .expect("register cross-thread fixture root");
        let expected_root = thread_fixture.proc_root().to_path_buf();
        drop(thread_fixture);
        std::thread::spawn(move || {
            let lease = take_runtime_reap_proc_root(thread_id).expect("worker consumes permit");
            assert_eq!(lease.proc_root(), expected_root);
            assert!(lease.proc_root().exists(), "lease keeps fixture root alive");
            drop(thread_guard);
            assert!(lease.proc_root().exists(), "consumed guard drop is inert");
            drop(lease);
        })
        .join()
        .expect("cross-thread permit worker");
        assert!(take_runtime_reap_proc_root(thread_id).is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn runtime_reap_proc_root_pending_inner_drop_leases_outer_and_reclaims_every_arc() {
        let fixture = StartupReaperFixture::new();
        let fixture_weak = std::sync::Arc::downgrade(&fixture.inner);
        let session_id = Uuid::new_v4();
        let outer = fixture
            .scoped_runtime_reap_root(session_id)
            .expect("register outer fixture root");
        let outer_token = outer.token;
        let inner = fixture
            .scoped_runtime_reap_root(session_id)
            .expect("register same-fixture inner root");
        drop(inner);

        let lease = take_runtime_reap_proc_root(session_id)
            .expect("pending inner drop exposes the outer permit");
        assert_eq!(lease.token, outer_token, "outer token is selected");
        assert_eq!(
            lease.proc_root(),
            fixture.proc_root(),
            "outer root is selected"
        );
        drop(outer);
        drop(fixture);
        assert!(
            fixture_weak.upgrade().is_some(),
            "leased root remains owned"
        );
        drop(lease);
        assert!(take_runtime_reap_proc_root(session_id).is_none());
        assert!(
            fixture_weak.upgrade().is_none(),
            "final lease drop reclaims every Arc"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn runtime_reap_proc_root_operation_survives_detached_blocking_cancellation() {
        let fixture = StartupReaperFixture::new();
        let fixture_weak = std::sync::Arc::downgrade(&fixture.inner);
        let session_id = Uuid::new_v4();
        let mut child = fixture.spawn_runtime_session(session_id);
        let child_entry = fixture.proc_root().join(child.pid().to_string());
        let guard = fixture
            .scoped_runtime_reap_root(session_id)
            .expect("register exact runtime fixture root");
        let operation = prepare_runtime_reap_proc_root_operation(session_id)
            .expect("prepare runtime reap operation")
            .expect("exact-session permit transfers synchronously");
        drop(guard);
        drop(fixture);
        assert!(
            child_entry.exists(),
            "operation retains the synthetic proc root"
        );
        assert!(
            fixture_weak.upgrade().is_some(),
            "operation owns the fixture Arc"
        );

        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let handle = tokio::task::spawn_blocking(move || {
            started_tx.send(()).expect("report queued blocking closure");
            release_rx
                .recv()
                .expect("release detached blocking closure");
            done_tx
                .send(operation())
                .expect("report detached blocking closure result");
        });
        started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("blocking closure owns the operation before cancellation");
        drop(handle);
        release_tx
            .send(())
            .expect("release detached blocking closure");
        assert_eq!(
            done_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("detached blocking closure completion")
                .expect("synthetic-root reaper succeeds"),
            1,
            "detached closure must reap through its synthetic root"
        );
        child.wait_signalled("detached operation target");
        assert!(
            !child_entry.exists(),
            "reaper removes synthetic target entry"
        );
        assert!(take_runtime_reap_proc_root(session_id).is_none());
        assert!(
            fixture_weak.upgrade().is_none(),
            "completion releases registry and lease Arcs"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn runtime_reap_failure_precedes_proc_root_permit_consumption() {
        let fixture = StartupReaperFixture::new();
        let session_id = Uuid::new_v4();
        let guard = fixture
            .scoped_runtime_reap_root(session_id)
            .expect("register exact runtime fixture root");
        fail_runtime_orphan_reap_for_test(session_id);
        let error = match prepare_runtime_reap_proc_root_operation(session_id) {
            Ok(_) => panic!("injected runtime failure wins before permit consumption"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("injected runtime orphan reap failure")
        );
        let lease = take_runtime_reap_proc_root(session_id)
            .expect("failed preparation leaves the exact permit pending");
        assert_eq!(lease.proc_root(), fixture.proc_root());
        drop(guard);
        drop(lease);
        assert!(take_runtime_reap_proc_root(session_id).is_none());
    }

    #[test]
    fn startup_public_entrypoints_keep_default_domain_and_live_proc() {
        let source = include_str!("reaper.rs");
        let body = source
            .split_once("fn reap_startup_process_ownership_inner")
            .expect("production process-first wrapper")
            .1;
        assert!(body.contains("StartupProcessDomain::default_domain()"));
        assert!(body.contains("Path::new(\"/proc\")"));
        assert_eq!(
            source.matches("STARTUP_PROVIDER_AFTER_SCAN_HOOK").count(),
            1
        );
    }

    #[test]
    fn startup_settlement_fault_permits_are_candidate_keyed_across_threads_and_unwind_safe() {
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let permit = scoped_startup_settlement_reap_failure(first);
        assert!(!take_startup_settlement_fault(
            StartupSettlementFaultKind::Reap,
            &[second]
        ));
        std::thread::spawn(move || {
            assert!(take_startup_settlement_fault(
                StartupSettlementFaultKind::Reap,
                &[first]
            ));
            drop(permit);
        })
        .join()
        .expect("cross-thread permit consumer");
        assert!(!take_startup_settlement_fault(
            StartupSettlementFaultKind::Reap,
            &[first]
        ));

        let candidate = Uuid::new_v4();
        let _ = std::panic::catch_unwind(|| {
            let _permit = scoped_startup_settlement_scan_proof_failure(candidate);
            panic!("exercise permit unwind cleanup");
        });
        assert!(!take_startup_settlement_fault(
            StartupSettlementFaultKind::ScanProof,
            &[candidate]
        ));

        let nested = Uuid::new_v4();
        let outer = scoped_startup_settlement_reap_failure(nested);
        let inner = scoped_startup_settlement_reap_failure(nested);
        drop(inner);
        assert!(take_startup_settlement_fault(
            StartupSettlementFaultKind::Reap,
            &[nested]
        ));
        drop(outer);
        assert!(!take_startup_settlement_fault(
            StartupSettlementFaultKind::Reap,
            &[nested]
        ));

        let kind_isolated = Uuid::new_v4();
        let reap = scoped_startup_settlement_reap_failure(kind_isolated);
        assert!(!take_startup_settlement_fault(
            StartupSettlementFaultKind::ScanProof,
            &[kind_isolated]
        ));
        assert!(take_startup_settlement_fault(
            StartupSettlementFaultKind::Reap,
            &[kind_isolated]
        ));
        drop(reap);
        assert!(!take_startup_settlement_fault(
            StartupSettlementFaultKind::Reap,
            &[kind_isolated]
        ));
    }

    #[test]
    fn startup_after_scan_hook_is_one_shot_nested_thread_isolated_and_unwind_safe() {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hook_calls = std::sync::Arc::clone(&calls);
        let mut hook: StartupAfterScanHook = Some(Box::new(move || {
            hook_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }));
        assert!(run_startup_provider_after_scan_hook(&mut hook));
        assert!(!run_startup_provider_after_scan_hook(&mut hook));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

        let nested_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let inner_calls = std::sync::Arc::clone(&nested_calls);
        let outer_calls = std::sync::Arc::clone(&nested_calls);
        let mut outer: StartupAfterScanHook = Some(Box::new(move || {
            outer_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut inner: StartupAfterScanHook = Some(Box::new(move || {
                inner_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }));
            assert!(run_startup_provider_after_scan_hook(&mut inner));
        }));
        assert!(run_startup_provider_after_scan_hook(&mut outer));
        assert_eq!(nested_calls.load(std::sync::atomic::Ordering::SeqCst), 2);

        let thread_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let thread_hook_calls = std::sync::Arc::clone(&thread_calls);
        std::thread::spawn(move || {
            let mut thread_hook: StartupAfterScanHook = Some(Box::new(move || {
                thread_hook_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }));
            assert!(run_startup_provider_after_scan_hook(&mut thread_hook));
            assert!(!run_startup_provider_after_scan_hook(&mut thread_hook));
        })
        .join()
        .expect("thread-local invocation hook");
        assert_eq!(thread_calls.load(std::sync::atomic::Ordering::SeqCst), 1);

        struct PendingCapture(std::sync::Arc<std::sync::atomic::AtomicUsize>);
        impl Drop for PendingCapture {
            fn drop(&mut self) {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let drops = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let pending_drops = std::sync::Arc::clone(&drops);
        let _ = std::panic::catch_unwind(move || {
            let capture = PendingCapture(pending_drops);
            let _pending: StartupAfterScanHook = Some(Box::new(move || {
                drop(capture);
            }));
            panic!("drop pending after-scan capture during unwind");
        });
        assert_eq!(drops.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn startup_settlement_reaper_reaches_fixed_point_and_preserves_unrelated_processes() {
        let fixture = StartupReaperFixture::new();
        let candidate = Uuid::new_v4();
        let unrelated_id = Uuid::new_v4();
        let mut first = fixture.spawn(Some(candidate), None, &fixture.domain(), true);
        let spawned_after_scan = std::sync::Arc::new(std::sync::Mutex::new(None));
        let hook_child = std::sync::Arc::clone(&spawned_after_scan);
        let mut unrelated = fixture.spawn(Some(unrelated_id), None, &fixture.domain(), true);
        let hook_fixture = fixture.clone();

        let ownership = StartupProcessOwnership::for_sessions_in_domain(
            HashSet::from([candidate]),
            fixture.domain(),
        );
        let reaped = reap_startup_owned_orphans_at(
            &ownership,
            fixture.proc_root(),
            Some(Box::new(move || {
                *hook_child.lock().expect("lock hook child") =
                    Some(hook_fixture.spawn(Some(candidate), None, &hook_fixture.domain(), true));
            })),
        )
        .expect("reap exact candidate cohort to fixed point");
        assert_eq!(reaped, 2);
        first.wait_signalled("first stamped child");
        let mut late = spawned_after_scan
            .lock()
            .expect("lock late child")
            .take()
            .expect("after-scan hook spawned child");
        late.wait_signalled("after-scan stamped child");
        unrelated.assert_alive("different exact session stamp must survive");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn startup_process_first_reaps_pre_session_agent_child_and_scheduled_fresh_invocations() {
        let store = crate::store::Store::open_in_memory().expect("startup ownership store");
        let fixture = StartupReaperFixture::new();
        let agent_invocation = Uuid::new_v4();
        let scheduled_invocation = Uuid::new_v4();
        let reserved_child = Uuid::new_v4();
        insert_running_invocation(
            &store,
            agent_invocation,
            rsi_common::model_control::ModelInvocationPurpose::AgentSpawnChild,
            "orchestration",
        );
        insert_running_invocation(
            &store,
            scheduled_invocation,
            rsi_common::model_control::ModelInvocationPurpose::ScheduledFresh,
            "background",
        );
        let mut agent = fixture.spawn(
            Some(reserved_child),
            Some(agent_invocation),
            &fixture.domain(),
            true,
        );
        let mut scheduled =
            fixture.spawn(None, Some(scheduled_invocation), &fixture.domain(), true);

        let reaped = reap_startup_process_ownership_for_domain_at(
            &store,
            fixture.domain(),
            fixture.proc_root(),
            None,
        )
        .expect("process-first invocation recovery");
        assert_eq!(reaped, 2);
        agent.wait_signalled("pre-Session AgentChild invocation");
        scheduled.wait_signalled("scheduled Fresh invocation");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn startup_process_first_reaps_terminal_sessions_and_foreign_extra_cannot_veto() {
        let store = crate::store::Store::open_in_memory().expect("startup ownership store");
        let fixture = StartupReaperFixture::new();
        let completed_id = Uuid::new_v4();
        let failed_id = Uuid::new_v4();
        for (id, status) in [
            (completed_id, rsi_common::types::SessionStatus::Completed),
            (failed_id, rsi_common::types::SessionStatus::Failed),
        ] {
            let mut session = crate::store::tests::make_test_session();
            session.id = id;
            session.status = status;
            store
                .insert_session(&session)
                .expect("insert terminal Session");
        }
        let mut completed = fixture.spawn(
            Some(completed_id),
            Some(Uuid::new_v4()),
            &fixture.domain(),
            true,
        );
        let mut failed = fixture.spawn(Some(failed_id), None, &fixture.domain(), true);

        let reaped = reap_startup_process_ownership_for_domain_at(
            &store,
            fixture.domain(),
            fixture.proc_root(),
            None,
        )
        .expect("terminal Session ownership recovery");
        assert_eq!(reaped, 2);
        completed.wait_signalled("Completed Session orphan");
        failed.wait_signalled("Failed Session orphan");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn startup_process_first_preserves_cloned_ids_in_another_socket_namespace() {
        let local = crate::store::Store::open_in_memory().expect("local startup Store");
        let foreign = crate::store::Store::open_in_memory().expect("foreign startup Store");
        let fixture = StartupReaperFixture::new();
        let foreign_fixture = StartupReaperFixture::new();
        let foreign_id = Uuid::new_v4();
        let mut foreign_session = crate::store::tests::make_test_session();
        foreign_session.id = foreign_id;
        local
            .insert_session(&foreign_session)
            .expect("insert overlapping local Session owner");
        foreign
            .insert_session(&foreign_session)
            .expect("insert foreign Session owner");
        let ambient_id = Uuid::new_v4();
        let mut foreign_process =
            fixture.spawn(Some(foreign_id), None, &foreign_fixture.domain(), true);
        let mut ambient = fixture.spawn(Some(ambient_id), None, &foreign_fixture.domain(), true);

        let reaped = reap_startup_process_ownership_for_domain_at(
            &local,
            fixture.domain(),
            fixture.proc_root(),
            None,
        )
        .expect("foreign namespace does not fail local startup");
        assert_eq!(reaped, 0);
        foreign_process.assert_alive("cloned UUID in foreign socket namespace");
        ambient.assert_alive("ambient unowned Session stamp");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn startup_process_first_rechecks_store_ownership_after_first_empty_pass() {
        let store = crate::store::Store::open_in_memory().expect("startup ownership store");
        let fixture = StartupReaperFixture::new();
        let invocation_id = Uuid::new_v4();
        insert_running_invocation(
            &store,
            invocation_id,
            rsi_common::model_control::ModelInvocationPurpose::ScheduledFresh,
            "background",
        );
        let late = std::sync::Arc::new(std::sync::Mutex::new(None));
        let hook_child = std::sync::Arc::clone(&late);
        let hook_fixture = fixture.clone();
        let reaped = reap_startup_process_ownership_for_domain_at(
            &store,
            fixture.domain(),
            fixture.proc_root(),
            Some(Box::new(move || {
                *hook_child.lock().expect("lock late invocation") = Some(hook_fixture.spawn(
                    None,
                    Some(invocation_id),
                    &hook_fixture.domain(),
                    true,
                ));
            })),
        )
        .expect("late invocation reaches owned fixed point");
        assert_eq!(reaped, 1);
        let mut late = late
            .lock()
            .expect("lock late invocation")
            .take()
            .expect("first-empty hook spawned invocation");
        late.wait_signalled("late invocation after first empty pass");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn startup_legacy_fallback_requires_exact_socket_and_store_owned_id() {
        let store = crate::store::Store::open_in_memory().expect("startup ownership store");
        let fixture = StartupReaperFixture::new();
        let wrong_fixture = StartupReaperFixture::new();
        let mut session = crate::store::tests::make_test_session();
        session.status = rsi_common::types::SessionStatus::Completed;
        store.insert_session(&session).expect("insert legacy owner");
        let mut exact = fixture.spawn(Some(session.id), None, &fixture.domain(), false);
        let mut wrong_socket =
            fixture.spawn(Some(session.id), None, &wrong_fixture.domain(), false);

        let reaped = reap_startup_process_ownership_for_domain_at(
            &store,
            fixture.domain(),
            fixture.proc_root(),
            None,
        )
        .expect("bounded legacy fallback recovery");
        assert_eq!(reaped, 1);
        exact.wait_signalled("exact legacy socket + Store ID");
        wrong_socket.assert_alive("wrong legacy socket with cloned ID");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn runtime_capacity_reaper_uses_namespace_fixed_point_and_preserves_foreign_clone() {
        let fixture = StartupReaperFixture::new();
        let foreign_fixture = StartupReaperFixture::new();
        let session_id = Uuid::new_v4();
        let mut first = fixture.spawn(Some(session_id), None, &fixture.domain(), true);
        let late = std::sync::Arc::new(std::sync::Mutex::new(None));
        let hook_child = std::sync::Arc::clone(&late);
        let mut foreign = fixture.spawn(Some(session_id), None, &foreign_fixture.domain(), true);
        let hook_fixture = fixture.clone();

        let ownership = StartupProcessOwnership::for_sessions_in_domain(
            HashSet::from([session_id]),
            fixture.domain(),
        );
        let reaped = reap_startup_owned_orphans_at(
            &ownership,
            fixture.proc_root(),
            Some(Box::new(move || {
                *hook_child.lock().expect("lock runtime hook child") =
                    Some(hook_fixture.spawn(Some(session_id), None, &hook_fixture.domain(), true));
            })),
        )
        .expect("runtime exact fixed-point reap");
        assert_eq!(reaped, 2);
        first.wait_signalled("first runtime orphan");
        let mut late = late
            .lock()
            .expect("lock late runtime child")
            .take()
            .expect("runtime hook spawned late child");
        late.wait_signalled("late runtime orphan");
        foreign.assert_alive("foreign namespace with cloned Session ID");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn startup_provider_reaper_catches_child_after_first_empty_inventory() {
        let fixture = StartupReaperFixture::new();
        let candidate = Uuid::new_v4();
        let spawned_after_empty = std::sync::Arc::new(std::sync::Mutex::new(None));
        let hook_child = std::sync::Arc::clone(&spawned_after_empty);
        let ownership = StartupProcessOwnership::for_sessions_in_domain(
            HashSet::from([candidate]),
            fixture.domain(),
        );
        let hook_fixture = fixture.clone();
        let result = reap_startup_owned_orphans_at(
            &ownership,
            fixture.proc_root(),
            Some(Box::new(move || {
                *hook_child.lock().expect("lock hook child") =
                    Some(hook_fixture.spawn(Some(candidate), None, &hook_fixture.domain(), true));
            })),
        );
        let mut late = spawned_after_empty
            .lock()
            .expect("lock late child")
            .take()
            .expect("first-empty hook spawned child");
        assert_eq!(
            result.expect("second empty inventory must observe and reap the late child"),
            1
        );
        late.wait_signalled("child spawned after first empty inventory");
    }

    #[test]
    fn startup_provider_kill_pins_identity_before_final_reproof_and_signal() {
        let source = include_str!("reaper.rs");
        let start = source
            .find("fn kill_startup_provider_orphans(")
            .expect("Linux startup provider kill function");
        let end = source[start..]
            .find("#[cfg(not(target_os = \"linux\"))]")
            .map(|offset| start + offset)
            .expect("non-Linux fallback follows Linux kill function");
        let body = &source[start..end];
        let pidfd_open = body.find("pidfd_open").expect("open kernel process handle");
        let start_reproof = body
            .find("let Some((current_start_time")
            .expect("start-time reproof");
        let environ_reproof = body
            .find("let environ = read_bounded_proc_file")
            .expect("environment reproof");
        let final_reproof = body
            .find("let Some((final_start_time")
            .expect("final start-time reproof");
        let pinned_signal = body
            .find("pidfd_send_signal")
            .expect("signal through pinned kernel process handle");

        assert!(pidfd_open < start_reproof);
        assert!(start_reproof < environ_reproof);
        assert!(environ_reproof < final_reproof);
        assert!(final_reproof < pinned_signal);
        assert!(!body.contains("nix::sys::signal::kill"));
    }

    #[test]
    fn runtime_reapers_delegate_to_the_fixed_point_pidfd_primitive() {
        let source = include_str!("reaper.rs");
        let ordinary_start = source
            .find("pub fn reap_orphans_for_session")
            .expect("ordinary runtime reaper");
        let ordinary_end = source[ordinary_start..]
            .find("static CAPACITY_REAP_FAILURE")
            .map(|offset| ordinary_start + offset)
            .expect("capacity test seam follows ordinary reaper");
        let ordinary = &source[ordinary_start..ordinary_end];
        let capacity_start = source
            .find("pub(super) fn reap_capacity_orphans_checked")
            .expect("capacity runtime reaper");
        let capacity_end = source[capacity_start..]
            .find("static STARTUP_PROVIDER_REAP_FAILURE")
            .map(|offset| capacity_start + offset)
            .expect("startup test seam follows capacity reaper");
        let capacity = &source[capacity_start..capacity_end];

        for body in [ordinary, capacity] {
            assert!(body.contains("reap_startup_owned_orphans_checked"));
            assert!(!body.contains("nix::sys::signal::kill"));
            assert!(!body.contains("PermissionDenied"));
        }
    }

    #[test]
    fn non_linux_startup_and_runtime_entrypoints_are_explicit_no_ops() {
        let source = include_str!("reaper.rs");
        for function in [
            "pub fn reap_orphans_for_session",
            "pub(super) fn reap_capacity_orphans_checked",
            "pub fn reap_startup_process_ownership_checked",
        ] {
            let start = source.find(function).expect("reaper entry point");
            let body = &source[start..source.len().min(start + 1_800)];
            assert!(body.contains("cfg(not(target_os = \"linux\"))"));
            assert!(body.contains("return Ok(0)"));
        }
        let fallback_marker =
            "#[cfg(not(target_os = \"linux\"))]\nfn kill_startup_provider_orphans(";
        let fallback = source
            .find(fallback_marker)
            .expect("non-Linux signal fallback");
        let fallback_end = source[fallback..]
            .find("fn read_bounded_proc_file")
            .map(|offset| fallback + offset)
            .expect("bounded proc reader follows non-Linux fallback");
        let body = &source[fallback..fallback_end];
        assert!(body.contains("Ok(0)"));
        assert!(!body.contains("nix::sys::signal::kill"));
    }

    #[test]
    fn startup_stamp_parser_rejects_duplicate_owned_identity_with_exact_pid() {
        let candidate = Uuid::new_v4();
        let ownership = StartupProcessOwnership::for_sessions(HashSet::from([candidate]));
        let namespace =
            String::from_utf8(ownership.domain.namespace.clone()).expect("test namespace is UTF-8");
        let environment = format!(
            "{}={namespace}\0{}={candidate}\0{}={candidate}\0",
            rsi_common::identity::ENV_PROCESS_OWNERSHIP_NAMESPACE,
            rsi_common::identity::ENV_SESSION_ID,
            rsi_common::identity::ENV_SESSION_ID,
        );
        let observation =
            parse_startup_stamp_observation(environment.as_bytes()).expect("bounded parse");
        let error = observation
            .authorized_exact_stamp(4242, &ownership)
            .expect_err("duplicate owned stamp fails closed");
        assert!(error.to_string().contains("process 4242"));
        assert!(error.to_string().contains("duplicate RSI_SESSION_ID"));
    }

    #[test]
    fn startup_stamp_parser_rejects_owned_plus_foreign_duplicate_but_ignores_wholly_foreign() {
        let candidate = Uuid::new_v4();
        let foreign = Uuid::new_v4();
        let ownership = StartupProcessOwnership::for_sessions(HashSet::from([candidate]));
        let namespace =
            String::from_utf8(ownership.domain.namespace.clone()).expect("test namespace is UTF-8");
        let environment = format!(
            "{}={namespace}\0{}={candidate}\0{}={foreign}\0",
            rsi_common::identity::ENV_PROCESS_OWNERSHIP_NAMESPACE,
            rsi_common::identity::ENV_SESSION_ID,
            rsi_common::identity::ENV_SESSION_ID,
        );
        let observation =
            parse_startup_stamp_observation(environment.as_bytes()).expect("bounded parse");
        assert!(
            observation.authorized_exact_stamp(7, &ownership).is_err(),
            "one current-DB ID makes duplicate structure fail closed"
        );
        let foreign_ownership = StartupProcessOwnership {
            domain: StartupProcessDomain::for_socket(Path::new("/tmp/foreign-daemon.sock")),
            namespaced_session_ids: None,
            session_ids: HashSet::from([candidate]),
            invocation_ids: HashSet::new(),
        };
        assert_eq!(
            observation
                .authorized_exact_stamp(7, &foreign_ownership)
                .expect("foreign namespace remains inert despite overlapping UUID"),
            None
        );
    }

    #[test]
    fn startup_stamp_parser_rejects_malformed_owned_tuple_and_accepts_exact_pair() {
        let session_id = Uuid::new_v4();
        let invocation_id = Uuid::new_v4();
        let ownership = StartupProcessOwnership::for_sessions(HashSet::from([session_id]));
        let namespace =
            String::from_utf8(ownership.domain.namespace.clone()).expect("test namespace is UTF-8");
        let malformed = format!(
            "{}={namespace}\0{}={session_id}\0{}=NOT-CANONICAL\0",
            rsi_common::identity::ENV_PROCESS_OWNERSHIP_NAMESPACE,
            rsi_common::identity::ENV_SESSION_ID,
            rsi_common::identity::ENV_MODEL_INVOCATION_ID,
        );
        assert!(
            parse_startup_stamp_observation(malformed.as_bytes())
                .expect("bounded malformed parse")
                .authorized_exact_stamp(99, &ownership)
                .is_err()
        );

        let exact = format!(
            "{}={namespace}\0{}={session_id}\0{}={invocation_id}\0",
            rsi_common::identity::ENV_PROCESS_OWNERSHIP_NAMESPACE,
            rsi_common::identity::ENV_SESSION_ID,
            rsi_common::identity::ENV_MODEL_INVOCATION_ID,
        );
        assert_eq!(
            parse_startup_stamp_observation(exact.as_bytes())
                .expect("bounded exact parse")
                .authorized_exact_stamp(99, &ownership)
                .expect("exact pair is valid"),
            Some(StartupExactStamp {
                proof: StartupOwnershipProof::Namespaced,
                namespace: Some(namespace.into_bytes()),
                socket: None,
                session_id: Some(session_id),
                invocation_id: Some(invocation_id),
            })
        );
    }

    #[test]
    fn startup_stamp_parser_rejects_duplicate_matching_namespace_but_ignores_foreign_namespace() {
        let ownership = StartupProcessOwnership::for_sessions(HashSet::new());
        let namespace =
            String::from_utf8(ownership.domain.namespace.clone()).expect("test namespace is UTF-8");
        let duplicate = format!(
            "{}={namespace}\0{}={namespace}\0",
            rsi_common::identity::ENV_PROCESS_OWNERSHIP_NAMESPACE,
            rsi_common::identity::ENV_PROCESS_OWNERSHIP_NAMESPACE,
        );
        let error = parse_startup_stamp_observation(duplicate.as_bytes())
            .expect("parse duplicate namespace")
            .authorized_exact_stamp(
                8128,
                &StartupProcessOwnership {
                    namespaced_session_ids: None,
                    ..ownership.clone()
                },
            )
            .expect_err("duplicate matching namespace fails closed");
        assert!(
            error
                .to_string()
                .contains("duplicate RSI_PROCESS_OWNERSHIP_NAMESPACE")
        );

        let foreign_namespace = rsi_common::identity::process_ownership_namespace_for_socket(
            Path::new("/tmp/rsi-parser-foreign.sock"),
        );
        let foreign = format!(
            "{}={foreign_namespace}\0{}={}\0",
            rsi_common::identity::ENV_PROCESS_OWNERSHIP_NAMESPACE,
            rsi_common::identity::ENV_SESSION_ID,
            Uuid::new_v4(),
        );
        assert_eq!(
            parse_startup_stamp_observation(foreign.as_bytes())
                .expect("parse foreign namespace")
                .authorized_exact_stamp(8129, &ownership)
                .expect("foreign namespace stays inert"),
            None
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn startup_inventory_fails_closed_for_unreadable_unknown_same_uid_process() {
        let directory = tempfile::tempdir().expect("create fake proc root");
        let uid = std::fs::metadata(directory.path())
            .expect("stat fake proc root")
            .uid();
        let pid = 4421;
        let process_root = directory.path().join(pid.to_string());
        std::fs::create_dir_all(&process_root).expect("create fake process");
        write_fake_platform_stat(&process_root, pid, "provider", 1, 99);
        let environ = process_root.join("environ");
        std::fs::write(
            &environ,
            format!(
                "{}={}\0",
                rsi_common::identity::ENV_PROCESS_OWNERSHIP_NAMESPACE,
                rsi_common::identity::process_ownership_namespace()
            ),
        )
        .expect("write fake environment");
        std::fs::set_permissions(&environ, std::fs::Permissions::from_mode(0o000))
            .expect("make fake environment unreadable");

        let error = scan_startup_process_inventory(
            directory.path(),
            -1,
            uid,
            StdInstant::now() + Duration::from_secs(1),
        )
        .expect_err("unreadable unknown same-UID process must fail closed");
        std::fs::set_permissions(&environ, std::fs::Permissions::from_mode(0o600))
            .expect("restore cleanup permissions");
        assert!(error.to_string().contains("environ read failed"), "{error}");
        assert!(error.to_string().contains(&pid.to_string()), "{error}");
    }

    #[test]
    fn startup_settlement_permission_exemption_is_exact_user_manager_pair() {
        let directory = tempfile::tempdir().expect("create fake proc root");
        let uid = std::fs::metadata(directory.path())
            .expect("stat fake proc root")
            .uid();
        let scope = format!("0::/user.slice/user-{uid}.slice/user@{uid}.service/init.scope");
        write_fake_proc_identity(directory.path(), 100, "systemd", 1, &scope);
        write_fake_proc_identity(directory.path(), 101, "(sd-pam)", 100, &scope);
        write_fake_proc_identity(directory.path(), 102, "provider", 100, &scope);
        write_fake_proc_identity(
            directory.path(),
            103,
            "systemd",
            1,
            "0::/user.slice/user-1000.slice/session-1.scope",
        );

        assert!(
            process_is_proven_systemd_user_manager_pair(directory.path(), 100, uid)
                .expect("prove exact user manager")
        );
        assert!(
            process_is_proven_systemd_user_manager_pair(directory.path(), 101, uid)
                .expect("prove exact sd-pam child")
        );
        assert!(
            !process_is_proven_systemd_user_manager_pair(directory.path(), 102, uid)
                .expect("unknown init-scope process stays untrusted")
        );
        assert!(
            !process_is_proven_systemd_user_manager_pair(directory.path(), 103, uid)
                .expect("wrong cgroup stays untrusted")
        );
    }
}
