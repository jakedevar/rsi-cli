//! macOS settlement orphan scan (#106, hub #3092): the libproc counterpart of
//! the Linux `/proc` walk behind `reap_startup_owned_orphans_at` and
//! `prove_startup_settlement_orphan_scan_readable`. Only the settlement entry
//! points use it; the runtime, capacity and daemon-startup reapers stay the
//! documented no-ops off Linux.
//!
//! A process belongs to a settlement candidate only through the exact RSI
//! stamps in its own environment (`KERN_PROCARGS2`), parsed and authorized by
//! the same code as on Linux, with the same bounds, deadline and two-empty-pass
//! fixed point. The kernel withholds the environment of a platform or hardened
//! binary from other processes; such a process is unproven and never
//! signalled, exactly like a Linux process whose `environ` is unreadable, and
//! the quarantine holder inventory still retains any root it holds.
//!
//! Darwin has no pidfd, so the signal names a numeric pid. It is sent only to
//! a process of this effective uid, after its start time, the exact stamp
//! tuple and its start time again are re-proved immediately before `kill(2)`.
//! Residual [inferred]: the pid could be reused between that last re-proof and
//! the signal; XNU hands out pids in increasing order up to 99 999 before it
//! wraps, so a reuse inside that window needs the whole pid space cycled.

use super::{
    STARTUP_ORPHAN_ENV_MAX_BYTES, STARTUP_ORPHAN_FIXED_POINT_MAX_PASSES,
    STARTUP_ORPHAN_POST_KILL_GRACE, STARTUP_ORPHAN_POST_KILL_POLL, STARTUP_ORPHAN_PROC_ENTRY_MAX,
    STARTUP_ORPHAN_STAMP_TUPLE_MAX, STARTUP_ORPHAN_TOTAL_DEADLINE, StartupAfterScanHook,
    StartupOrphanIdentity, StartupProcessOwnership, StartupStampObservation,
    parse_startup_stamp_observation, run_startup_provider_after_scan_hook,
};
use nix::libc;
use rsid_core::libproc_macos::{
    credentials, errno, exited, list_pids, procargs_environment, process_arguments, process_start,
};
use std::time::Instant as StdInstant;

struct ObservedProcess {
    pid: libc::pid_t,
    started_micros: u64,
    stamps: StartupStampObservation,
}

/// Attempts at an environment read that fails without explanation.
const ENVIRONMENT_READ_ATTEMPTS: usize = 4;
const ENVIRONMENT_READ_BACKOFF: std::time::Duration = std::time::Duration::from_millis(10);

/// The environment of `pid` in Linux `environ` layout. `Ok(None)` when the
/// process exited or withholds it (`EPERM`, or `EINVAL` for a protected or
/// exiting process): an unproven process, never one to signal.
pub(super) fn read_environment(pid: libc::pid_t) -> crate::error::Result<Option<Vec<u8>>> {
    read_environment_with(
        pid,
        || process_arguments(pid, STARTUP_ORPHAN_ENV_MAX_BYTES),
        || process_start(pid).map(|start| !start.zombie),
    )
}

/// [`read_environment`] over injectable reads: `read` is `KERN_PROCARGS2`,
/// `live` whether the process still runs. Any other errno (observed: `EIO`
/// while other processes start and stop) is retried a bounded number of times;
/// a process that exited or turned zombie meanwhile holds nothing, and one that
/// stays unreadable fails the inventory closed.
fn read_environment_with(
    pid: libc::pid_t,
    mut read: impl FnMut() -> Result<Vec<u8>, i32>,
    mut live: impl FnMut() -> Result<bool, i32>,
) -> crate::error::Result<Option<Vec<u8>>> {
    let mut attempt = 1;
    loop {
        let code = match read() {
            Ok(record) => {
                return procargs_environment(&record).map(Some).ok_or_else(|| {
                    crate::error::DaemonError::Process(format!(
                        "startup provider argument record is malformed for pid {pid}"
                    ))
                });
            }
            Err(code) if exited(code) => return Ok(None),
            Err(code) if code == libc::EPERM || code == libc::EINVAL => {
                tracing::debug!(
                    pid,
                    code,
                    "startup provider environment is withheld; skipping unproven process"
                );
                return Ok(None);
            }
            Err(code) => code,
        };
        match live() {
            Ok(false) => return Ok(None),
            Err(gone) if exited(gone) => return Ok(None),
            Ok(true) | Err(_) => {}
        }
        if attempt >= ENVIRONMENT_READ_ATTEMPTS {
            return Err(crate::error::DaemonError::Process(format!(
                "startup provider environment read failed for pid {pid}: errno {code}"
            )));
        }
        attempt += 1;
        std::thread::sleep(ENVIRONMENT_READ_BACKOFF);
    }
}

/// One bounded pass over every process of this effective uid that carries an
/// RSI stamp.
fn scan_inventory(
    self_pid: libc::pid_t,
    self_uid: u32,
    deadline: StdInstant,
) -> crate::error::Result<Vec<ObservedProcess>> {
    let pids = list_pids().ok_or_else(|| {
        crate::error::DaemonError::Process(format!(
            "startup provider process list failed: errno {}",
            errno()
        ))
    })?;
    if pids.len() > STARTUP_ORPHAN_PROC_ENTRY_MAX {
        return Err(crate::error::DaemonError::Process(
            "startup provider process entry bound exceeded".into(),
        ));
    }
    let mut processes = Vec::new();
    let mut stamp_token_count = 0_usize;
    for pid in pids {
        if StdInstant::now() >= deadline {
            return Err(crate::error::DaemonError::Process(
                "startup provider inventory exceeded its deadline".into(),
            ));
        }
        // pid 0 is the kernel.
        if pid <= 0 || pid == self_pid {
            continue;
        }
        let owner = match credentials(pid) {
            Ok(owner) => owner,
            Err(code) if exited(code) => continue,
            Err(code) => {
                return Err(crate::error::DaemonError::Process(format!(
                    "startup provider process identity failed for pid {pid}: errno {code}"
                )));
            }
        };
        // A zombie cannot act on a settlement root.
        if owner.uid != self_uid || owner.zombie {
            continue;
        }
        let started_micros = match process_start(pid) {
            Ok(start) if start.zombie => continue,
            Ok(start) => start.started_micros,
            Err(code) if exited(code) => continue,
            // Its own user may not read it: a protected process, whose
            // environment is withheld too. Unproven, never signalled.
            Err(code) if code == libc::EPERM => continue,
            Err(code) => {
                return Err(crate::error::DaemonError::Process(format!(
                    "startup provider process start failed for pid {pid}: errno {code}"
                )));
            }
        };
        let Some(environment) = read_environment(pid)? else {
            continue;
        };
        let stamps = parse_startup_stamp_observation(&environment)?;
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
        if stamp_token_count > STARTUP_ORPHAN_STAMP_TUPLE_MAX
            || processes.len() >= STARTUP_ORPHAN_STAMP_TUPLE_MAX
        {
            return Err(crate::error::DaemonError::Process(format!(
                "startup provider stamp tuple bound exceeded at pid {pid} ({STARTUP_ORPHAN_STAMP_TUPLE_MAX})"
            )));
        }
        processes.push(ObservedProcess {
            pid,
            started_micros,
            stamps,
        });
    }
    Ok(processes)
}

fn self_identity() -> (libc::pid_t, u32) {
    (
        std::process::id() as libc::pid_t,
        nix::unistd::geteuid().as_raw(),
    )
}

/// The read-only proof fresh settlement takes before durable intent: the
/// inventory is complete and every stamp it carries is well formed.
pub(super) fn prove_scan_readable(ownership: &StartupProcessOwnership) -> crate::error::Result<()> {
    let (self_pid, self_uid) = self_identity();
    let deadline = StdInstant::now() + STARTUP_ORPHAN_TOTAL_DEADLINE;
    for process in scan_inventory(self_pid, self_uid, deadline)? {
        process
            .stamps
            .authorized_exact_stamp(process.pid, ownership)?;
    }
    Ok(())
}

/// Reap the exact-stamped processes of `ownership` to two consecutive complete
/// empty inventories, as the Linux fixed point does.
pub(super) fn reap_owned_orphans(
    ownership: &StartupProcessOwnership,
    mut after_scan_hook: StartupAfterScanHook,
) -> crate::error::Result<usize> {
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
    // Detached-turn adoption reads Linux `/proc`; only the runtime reaper (a
    // no-op here) carries such turns. Refuse rather than signal a turn that
    // may still be adoptable.
    if !ownership.detached_turns.is_empty() {
        return Err(crate::error::DaemonError::Process(
            "detached provider turn protection is unsupported on this platform".into(),
        ));
    }
    let (self_pid, self_uid) = self_identity();
    let deadline = StdInstant::now() + STARTUP_ORPHAN_TOTAL_DEADLINE;
    let mut reaped = 0_usize;
    let mut consecutive_empty_passes = 0_u8;
    for _ in 0..STARTUP_ORPHAN_FIXED_POINT_MAX_PASSES {
        if StdInstant::now() >= deadline {
            return Err(crate::error::DaemonError::Process(
                "startup provider inventory exceeded its deadline".into(),
            ));
        }
        let mut observed = Vec::new();
        for process in scan_inventory(self_pid, self_uid, deadline)? {
            if let Some(stamps) = process
                .stamps
                .authorized_exact_stamp(process.pid, ownership)?
            {
                observed.push(StartupOrphanIdentity {
                    pid: process.pid,
                    start_time: process.started_micros,
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
            .checked_add(kill_orphans(observed, ownership, deadline)?)
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

/// The same incarnation is still running: `Ok(true)`. Exited, replaced or a
/// zombie: `Ok(false)`.
fn same_live_incarnation(identity: &StartupOrphanIdentity) -> crate::error::Result<bool> {
    match process_start(identity.pid) {
        Ok(start) => Ok(start.started_micros == identity.start_time && !start.zombie),
        Err(code) if exited(code) => Ok(false),
        Err(code) => Err(crate::error::DaemonError::Process(format!(
            "startup provider start reproof failed for pid {}: errno {code}",
            identity.pid
        ))),
    }
}

fn kill_orphans(
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
        if !same_live_incarnation(&identity)? {
            continue;
        }
        let Some(environment) = read_environment(identity.pid)? else {
            continue;
        };
        let stamps = parse_startup_stamp_observation(&environment)?
            .authorized_exact_stamp(identity.pid, ownership)?;
        if stamps.as_ref() != Some(&identity.stamps) {
            return Err(crate::error::DaemonError::Process(format!(
                "startup provider process {} changed exact ownership tuple before kill",
                identity.pid
            )));
        }
        // The last reproof, immediately before the signal (no pidfd here).
        if !same_live_incarnation(&identity)? {
            continue;
        }
        // #1227: name every process this scan kills, and why.
        let target = crate::process_control::SignalTarget::describe(identity.pid);
        let reason = format!(
            "orphan stamped with session {} (invocation {})",
            identity
                .stamps
                .session_id
                .map_or_else(|| "-".to_string(), |id| id.to_string()),
            identity
                .stamps
                .invocation_id
                .map_or_else(|| "-".to_string(), |id| id.to_string()),
        );
        let record = crate::process_control::SignalRecord {
            pid: identity.pid,
            pgid: None,
            signal: "SIGKILL",
            reason: &reason,
            target: &target,
        };
        let sent = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(identity.pid),
            nix::sys::signal::Signal::SIGKILL,
        );
        crate::process_control::log_signal(
            &record,
            std::panic::Location::caller(),
            &match &sent {
                Ok(()) => "sent".to_string(),
                Err(error) => format!("failed: {error}"),
            },
        );
        match sent {
            Ok(()) => signaled.push(identity),
            Err(nix::errno::Errno::ESRCH) => {}
            Err(error) => {
                return Err(crate::error::DaemonError::Process(format!(
                    "startup provider SIGKILL failed for pid {}: {error}",
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
            if same_live_incarnation(&identity)? {
                still_live.push(identity);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use uuid::Uuid;

    /// Re-executed by the tests below as a stamped child: this ad-hoc-signed
    /// test binary, unlike a platform binary such as `/bin/sleep`, shows its
    /// environment to `KERN_PROCARGS2`.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[test]
    fn stamped_orphan_child_helper() {
        if std::env::var_os("RSI_TEST_STAMPED_ORPHAN_CHILD").is_none() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_secs(120));
    }

    fn stamped_child(session_id: Uuid) -> super::super::StartupReaperChild {
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "session::reaper::orphans_macos::tests::stamped_orphan_child_helper",
                "--test-threads=1",
            ])
            .env_remove(rsi_common::identity::ENV_SESSION_ID)
            .env_remove(rsi_common::identity::ENV_MODEL_INVOCATION_ID)
            .env_remove(rsi_common::identity::ENV_PROCESS_OWNERSHIP_NAMESPACE)
            .env_remove(rsi_common::identity::ENV_SOCKET)
            .env("RSI_TEST_STAMPED_ORPHAN_CHILD", "1")
            .env(rsi_common::identity::ENV_SESSION_ID, session_id.to_string())
            .env(
                rsi_common::identity::ENV_SOCKET,
                rsi_common::identity::default_socket_path(),
            )
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("spawn stamped child");
        let child = super::super::StartupReaperChild::unregistered(child);
        child.wait_for_environment(&format!(
            "{}={session_id}",
            rsi_common::identity::ENV_SESSION_ID
        ));
        child
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[test]
    fn an_unexplained_environment_failure_is_retried_then_fails_closed() {
        let mut record = 1i32.to_ne_bytes().to_vec();
        record.extend_from_slice(b"/opt/tool\0\0tool\0A=b\0\0");
        // Recovers after transient EIO.
        let mut failures = 2;
        let read = || {
            if failures > 0 {
                failures -= 1;
                Err(libc::EIO)
            } else {
                Ok(record.clone())
            }
        };
        assert_eq!(
            read_environment_with(7, read, || Ok(true)).expect("recovered"),
            Some(b"A=b\0".to_vec())
        );
        // A process that exits or turns zombie meanwhile holds nothing.
        assert_eq!(
            read_environment_with(7, || Err(libc::EIO), || Ok(false)).expect("zombie"),
            None
        );
        assert_eq!(
            read_environment_with(7, || Err(libc::EIO), || Err(libc::ESRCH)).expect("gone"),
            None
        );
        // Withheld or exited: unproven, no retry.
        for code in [libc::EPERM, libc::EINVAL, libc::ESRCH] {
            let mut reads = 0;
            let result = read_environment_with(
                7,
                || {
                    reads += 1;
                    Err(code)
                },
                || Ok(true),
            );
            assert_eq!(result.expect("unproven"), None);
            assert_eq!(reads, 1);
        }
        // A live process that stays unreadable fails closed after the bound.
        let mut reads = 0;
        let error = read_environment_with(
            7,
            || {
                reads += 1;
                Err(libc::EIO)
            },
            || Ok(true),
        )
        .expect_err("fails closed");
        assert_eq!(reads, ENVIRONMENT_READ_ATTEMPTS);
        assert!(error.to_string().contains("errno 5"), "{error}");
        // A malformed record is never skipped.
        assert!(read_environment_with(7, || Ok(vec![1]), || Ok(true)).is_err());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[test]
    fn macos_settlement_reaper_kills_only_the_exact_owner() {
        let owner = Uuid::new_v4();
        let other = Uuid::new_v4();
        let mut owned = stamped_child(owner);
        let mut foreign = stamped_child(other);
        let ownership = StartupProcessOwnership::for_sessions(HashSet::from([owner]));
        prove_scan_readable(&ownership).expect("the inventory is readable");
        owned.assert_alive("the read-only proof signals nothing");
        assert_eq!(
            reap_owned_orphans(&ownership, None).expect("reap the exact owner"),
            1
        );
        owned.wait_signalled("exact owner");
        foreign.assert_alive("another Session's process is never signalled");
        assert_eq!(
            reap_owned_orphans(&ownership, None).expect("the fixed point is empty"),
            0
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[test]
    fn macos_settlement_reaper_refuses_detached_turns_and_empty_ownership_is_inert() {
        let empty = StartupProcessOwnership::for_sessions(HashSet::new());
        assert_eq!(reap_owned_orphans(&empty, None).expect("inert"), 0);
        let owner = Uuid::new_v4();
        let mut ownership = StartupProcessOwnership::for_sessions(HashSet::from([owner]));
        let now = chrono::Utc::now();
        ownership
            .detached_turns
            .push(crate::store::provider_turn_custody::ProviderTurnCustody {
                invocation_id: Uuid::new_v4(),
                session_id: owner,
                spool_dir: std::path::PathBuf::from("/nonexistent-spool"),
                worker_scope: None,
                pid: 1,
                start_time: None,
                boot_id: Uuid::new_v4(),
                stdout_offset: 0,
                state: crate::store::provider_turn_custody::ProviderTurnCustodyState::Live,
                created_at: now,
                updated_at: now,
            });
        let error = reap_owned_orphans(&ownership, None).expect_err("refused");
        assert!(
            error.to_string().contains("unsupported on this platform"),
            "{error}"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[test]
    fn a_platform_binary_environment_is_withheld_and_never_proven() {
        let session_id = Uuid::new_v4();
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .env(rsi_common::identity::ENV_SESSION_ID, session_id.to_string())
            .spawn()
            .expect("spawn platform child");
        let pid = child.id() as libc::pid_t;
        std::thread::sleep(std::time::Duration::from_millis(100));
        let environment = read_environment(pid).expect("read or withheld");
        let stamp = format!("{}={session_id}", rsi_common::identity::ENV_SESSION_ID);
        let shown = environment.is_some_and(|environment| {
            environment
                .split(|byte| *byte == 0)
                .any(|variable| variable == stamp.as_bytes())
        });
        let ownership = StartupProcessOwnership::for_sessions(HashSet::from([session_id]));
        let reaped = reap_owned_orphans(&ownership, None).expect("reap");
        let alive = child.try_wait().expect("probe child").is_none();
        child.kill().ok();
        child.wait().ok();
        // Whatever the kernel shows decides: a shown stamp is reaped, a
        // withheld one leaves the process unproven and untouched.
        assert_eq!(reaped, usize::from(shown));
        assert_eq!(alive, !shown);
    }
}
