//! macOS process identity for lander ownership (#167).
//!
//! macOS has no `/proc`, so the Linux owner proof (`/proc/<pid>/stat` start
//! ticks, `/proc/self/ns/pid`, `boot_id`) cannot be read. The same guarantees
//! come from the kernel instead:
//!
//! - **Pid reuse:** the process start time in microseconds since the epoch
//!   (`proc_pidinfo(PROC_PIDTBSDINFO)`, or the `sysctl` process table for
//!   another uid's process, which libproc refuses, #252) plays the start-ticks
//!   role. It is set at fork and never adjusted, so a reused pid has a
//!   different value.
//! - **PID namespace:** macOS has none; the fixed [`PID_NAMESPACE`] label
//!   stands in, so an owner file written elsewhere never matches.
//! - **Boot identity:** `kern.boottime`, recorded for diagnosis only. The
//!   kernel adjusts it when the wall clock is stepped, so a mismatch is not
//!   proof of a reboot and liveness never trusts it; a reboot is still
//!   detected because the recorded pid is then absent or has another start
//!   time.
//!
//! The sweep's holder proof (cwd, root, fds, mappings) is still Linux-only, so
//! on macOS a workspace whose owner is gone is retained rather than deleted;
//! this module only makes the owner identity and its liveness reading exact.
//! `register_lander_owner` still needs the workspace birth time, which the
//! macOS directory stat supplies in #166.

use std::io;

/// The label recorded where Linux records its PID namespace link.
#[cfg(target_os = "macos")]
pub(super) const PID_NAMESPACE: &str = "macos";

/// What the kernel knows about a pid.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Probe {
    /// The process exists and started at this many microseconds since the epoch.
    Started(u64),
    /// No such process (`ESRCH`).
    Gone,
}

/// Start time of `pid`, `Gone` when it does not exist.
///
/// # Errors
/// The kernel refused or returned a short record, so the pid cannot be
/// interpreted (the caller must not reclaim).
#[cfg(target_os = "macos")]
pub(super) fn probe(pid: u32) -> io::Result<Probe> {
    use rsid_core::libproc_macos::{process_start, process_table_start_micros};
    let pid = i32::try_from(pid).map_err(|_| io::Error::other("pid out of range"))?;
    probe_from(process_start(pid).map(|start| start.started_micros), || {
        process_table_start_micros(pid)
    })
}

/// Classify the start-time reads of a recorded owner pid: `bsd` is
/// `proc_pidinfo(PROC_PIDTBSDINFO)` (an `errno`, `EIO` for a short record),
/// `process_table` the `sysctl(KERN_PROC_PID)` record, read only when `bsd`
/// is refused with `EPERM`.
///
/// libproc refuses another uid's process with `EPERM` (#252: 189 of 720 host
/// pids on a normal Mac). The recorded owner ran as this uid, but its pid can
/// be reused by another user's process once it exits; read as unknown, that
/// kept the workspace's owner `Ambiguous` for as long as the stranger lived.
/// The process table names every process's start time, so the reading stays
/// exact: a different start is a reused pid (gone), the same start is the
/// owner. Only `ESRCH` is gone; every other refusal stays an error.
#[cfg(any(target_os = "macos", test))]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(super) fn probe_from(
    bsd: Result<u64, i32>,
    process_table: impl FnOnce() -> Result<u64, i32>,
) -> io::Result<Probe> {
    use nix::libc::{EPERM, ESRCH};
    let started = match bsd {
        Err(code) if code == EPERM => process_table(),
        read => read,
    };
    match started {
        Ok(0) => Err(io::Error::other("process start time out of range")),
        Ok(micros) => Ok(Probe::Started(micros)),
        Err(code) if code == ESRCH => Ok(Probe::Gone),
        Err(code) => Err(io::Error::from_raw_os_error(code)),
    }
}

/// `kern.boottime` as `<seconds>.<microseconds>`.
#[cfg(target_os = "macos")]
pub(super) fn boot_time() -> io::Result<String> {
    let mut mib = [nix::libc::CTL_KERN, nix::libc::KERN_BOOTTIME];
    let mut boot = nix::libc::timeval {
        tv_sec: 0,
        tv_usec: 0,
    };
    let mut len = std::mem::size_of::<nix::libc::timeval>();
    // SAFETY: `mib` names two integers, `boot` is a valid `timeval` of `len`
    // bytes, and no new value is passed.
    let status = unsafe {
        nix::libc::sysctl(
            mib.as_mut_ptr(),
            2,
            std::ptr::from_mut(&mut boot).cast(),
            &raw mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if status != 0 || len != std::mem::size_of::<nix::libc::timeval>() || boot.tv_sec == 0 {
        return Err(io::Error::other("cannot read kern.boottime"));
    }
    Ok(format!("{}.{:06}", boot.tv_sec, boot.tv_usec))
}

#[cfg(test)]
mod tests {
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    use super::{Probe, probe_from};
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    use nix::libc::{EINVAL, EIO, EPERM, ESRCH};

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    fn unread() -> Result<u64, i32> {
        panic!("the process table is read only after an EPERM")
    }

    /// #252: another uid's process at the recorded pid is classified by its
    /// start time from the process table, never left unknown.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn another_users_process_at_the_owner_pid_is_read_from_the_process_table() {
        // Its start time, which `lander_owner` compares with the recorded one.
        assert_eq!(probe_from(Err(EPERM), || Ok(9)).unwrap(), Probe::Started(9));
        // The process exited between the two reads.
        assert_eq!(probe_from(Err(EPERM), || Err(ESRCH)).unwrap(), Probe::Gone);
        // A process table that cannot be read stays unknown.
        assert_eq!(
            probe_from(Err(EPERM), || Err(EIO))
                .unwrap_err()
                .raw_os_error(),
            Some(EIO)
        );
        assert!(probe_from(Err(EPERM), || Ok(0)).is_err());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn libproc_reads_and_other_refusals_are_classified_without_the_process_table() {
        assert_eq!(probe_from(Ok(42), unread).unwrap(), Probe::Started(42));
        assert_eq!(probe_from(Err(ESRCH), unread).unwrap(), Probe::Gone);
        for code in [EIO, EINVAL] {
            assert_eq!(
                probe_from(Err(code), unread).unwrap_err().raw_os_error(),
                Some(code)
            );
        }
        assert!(probe_from(Ok(0), unread).is_err());
    }
}
