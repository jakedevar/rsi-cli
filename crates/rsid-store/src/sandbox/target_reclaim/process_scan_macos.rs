//! macOS process scan for the reclaim idle check (#89, hub #3055): the
//! libproc counterpart of the Linux `/proc` walk in `ensure_no_target_process`.
//!
//! A process of this uid uses the sandbox when its cwd or executable lies
//! beneath the sandbox root, or when its `CARGO_TARGET_DIR` or one of its open
//! vnode descriptors lies beneath the target. Another owner's process, and one
//! that exits or turns zombie during the scan, is unrelated. A process of
//! this uid whose basic information cannot be read is re-read a few times and
//! then fails the candidate closed, as on Linux. macOS has no systemd unit to
//! tell RSI's processes from the operator's, so an unreadable environment or
//! descriptor table counts as unrelated only when the kernel denies it (EPERM,
//! a protected process); any other failure fails closed.

use super::{
    PROCESS_SCAN_RETRY_DELAY, ProcessProbe, ProcessProbeFailure, SandboxReclaimCheck, SkipReason,
    check, current_pass_state, normalize_lexically, settle_process_probe, unreadable_process,
};
use nix::libc;
use rsid_core::libproc_macos::{
    Prefixes, credentials, executable, exited, fd_vnode, is_vnode, list_fds, list_pids,
    process_arguments, working_directory,
};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

pub(super) fn ensure_no_target_process(
    root: &Path,
    target: &Path,
) -> Result<(), (SkipReason, SandboxReclaimCheck)> {
    let by_reason = |reason: SkipReason| (reason, check(reason.as_str()));
    let pids = list_pids().ok_or_else(|| unreadable_process(0))?;
    let uid = nix::unistd::getuid().as_raw();
    let own = std::process::id();
    let roots = Prefixes::new(root);
    let targets = Prefixes::new(target);
    for pid in pids {
        current_pass_state().checkpoint(0).map_err(by_reason)?;
        // The reclaimer itself owns pinned target descriptors; pid 0 is the kernel.
        if pid <= 0 || pid as u32 == own {
            continue;
        }
        let probed = settle_process_probe(
            || probe_process(pid, uid, &roots, &targets),
            || {
                std::thread::sleep(PROCESS_SCAN_RETRY_DELAY);
                current_pass_state().checkpoint(0)
            },
        );
        match probed {
            Ok(ProcessProbe::Unrelated) => {}
            Ok(ProcessProbe::InUse) => {
                return Err((
                    SkipReason::ActiveOwner,
                    SandboxReclaimCheck::with_subject("process_uses_sandbox", pid.to_string()),
                ));
            }
            Err(ProcessProbeFailure::Unreadable) => return Err(unreadable_process(pid as u32)),
            Err(ProcessProbeFailure::Budget(reason)) => return Err(by_reason(reason)),
        }
    }
    Ok(())
}

fn probe_process(
    pid: libc::pid_t,
    uid: u32,
    roots: &Prefixes,
    targets: &Prefixes,
) -> Result<ProcessProbe, ProcessProbeFailure> {
    let unreadable = ProcessProbeFailure::Unreadable;
    let info = match credentials(pid) {
        Ok(info) => info,
        Err(code) if exited(code) || code == libc::EPERM => return Ok(ProcessProbe::Unrelated),
        Err(_) => return Err(unreadable),
    };
    if info.uid != uid || info.zombie {
        return Ok(ProcessProbe::Unrelated);
    }
    let cwd = match working_directory(pid) {
        Ok(cwd) => Some(cwd.path),
        Err(code) if exited(code) => return Ok(ProcessProbe::Unrelated),
        Err(code) if code == libc::EPERM => None,
        Err(_) => return Err(unreadable),
    };
    if cwd.as_deref().is_some_and(|cwd| roots.contains(cwd)) {
        return Ok(ProcessProbe::InUse);
    }
    // An exited process (Ok(None)) names no executable and a protected one
    // (EPERM) withholds it, as it withholds its other probes; any other
    // failure leaves the evidence unread, so the process fails closed.
    match executable(pid) {
        Ok(Some(exe)) if roots.contains(&exe) => return Ok(ProcessProbe::InUse),
        Ok(_) => {}
        Err(code) if code == libc::EPERM => {}
        Err(_) => return Err(unreadable),
    }
    match cargo_target_dir(pid) {
        Ok(Some(value)) => {
            let configured = Path::new(OsStr::from_bytes(&value));
            let configured = if configured.is_absolute() {
                configured.to_path_buf()
            } else {
                let Some(cwd) = cwd.as_deref() else {
                    return Err(unreadable);
                };
                cwd.join(configured)
            };
            if targets.contains(&normalize_lexically(&configured)) {
                return Ok(ProcessProbe::InUse);
            }
        }
        Ok(None) => {}
        Err(code) if exited(code) => return Ok(ProcessProbe::Unrelated),
        // A protected or exiting process withholds its arguments (EPERM/EINVAL).
        Err(code) if code == libc::EPERM || code == libc::EINVAL => {}
        Err(_) => return Err(unreadable),
    }
    open_files_use_target(pid, targets)
}

/// The process's `CARGO_TARGET_DIR` value from `KERN_PROCARGS2` (argc, the
/// exec path, argv, then the environment, all NUL-separated). The value is
/// compared and dropped; it is never logged or stored.
fn cargo_target_dir(pid: libc::pid_t) -> Result<Option<Vec<u8>>, i32> {
    // No size bound of its own: the kernel's `kern.argmax` bounds the record.
    let record = process_arguments(pid, usize::MAX)?;
    Ok(parse_procargs_cargo_target_dir(&record))
}

fn parse_procargs_cargo_target_dir(buffer: &[u8]) -> Option<Vec<u8>> {
    let argc_bytes: [u8; 4] = buffer.get(..4)?.try_into().ok()?;
    let argc = usize::try_from(i32::from_ne_bytes(argc_bytes)).ok()?;
    let mut strings = buffer[4..]
        .split(|byte| *byte == 0)
        .filter(|item| !item.is_empty());
    // The exec path, then argc arguments.
    for _ in 0..=argc {
        strings.next()?;
    }
    strings.find_map(|variable| {
        variable
            .strip_prefix(b"CARGO_TARGET_DIR=")
            .map(<[u8]>::to_vec)
    })
}

fn open_files_use_target(
    pid: libc::pid_t,
    targets: &Prefixes,
) -> Result<ProcessProbe, ProcessProbeFailure> {
    let fds = match list_fds(pid) {
        Ok(fds) => fds,
        Err(code) if exited(code) || code == libc::EPERM => return Ok(ProcessProbe::Unrelated),
        Err(_) => return Err(ProcessProbeFailure::Unreadable),
    };
    for fd in fds {
        current_pass_state()
            .checkpoint(0)
            .map_err(ProcessProbeFailure::Budget)?;
        if !is_vnode(&fd) {
            continue;
        }
        match fd_vnode(pid, fd.proc_fd) {
            Ok(Some(vnode)) if targets.contains(&vnode.path) => return Ok(ProcessProbe::InUse),
            // A closed or pathless (unlinked) descriptor cannot name the target.
            Ok(_) => {}
            Err(code) if exited(code) => return Ok(ProcessProbe::Unrelated),
            // A protected process withholds a descriptor (EPERM), as it withholds its table.
            Err(code) if code == libc::EPERM => {}
            Err(_) => return Err(ProcessProbeFailure::Unreadable),
        }
    }
    Ok(ProcessProbe::Unrelated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn procargs_parser_reads_only_the_environment() {
        let mut buffer = 2i32.to_ne_bytes().to_vec();
        buffer.extend_from_slice(b"/bin/cargo\0\0\0cargo\0CARGO_TARGET_DIR=/argv\0");
        buffer.extend_from_slice(b"HOME=/h\0CARGO_TARGET_DIR=/env/target\0");
        assert_eq!(
            parse_procargs_cargo_target_dir(&buffer),
            Some(b"/env/target".to_vec())
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn macos_scan_sees_an_open_descriptor_and_a_child_cwd_beneath_the_target() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let target = root.join("target");
        std::fs::create_dir(&target).unwrap();

        // A child process sitting in the target holds it by cwd.
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .current_dir(&target)
            .spawn()
            .unwrap();
        let verdict = ensure_no_target_process(&root, &target);
        child.kill().ok();
        child.wait().ok();
        let (reason, _) = verdict.expect_err("a child in the target uses the sandbox");
        assert_eq!(reason, SkipReason::ActiveOwner);

        // With no process inside, the scan admits the target.
        ensure_no_target_process(&root, &target).unwrap();
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn macos_scan_sees_a_child_holding_a_target_file_with_its_cwd_outside() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let target = root.join("target");
        std::fs::create_dir(&target).unwrap();
        let held = target.join("held.rlib");
        std::fs::write(&held, b"x").unwrap();
        // The child's cwd and executable are outside the sandbox root.
        let elsewhere = tempfile::tempdir().unwrap();
        ensure_no_target_process(&root, &target).unwrap();

        // `exec` keeps the descriptor 3 open on the target file in `sleep`.
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exec /bin/sleep 30 3<\"$1\"", "sh"])
            .arg(&held)
            .current_dir(elsewhere.path())
            .spawn()
            .unwrap();
        // The shell opens the file just after spawn: poll until the scan sees it.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let verdict = loop {
            let verdict = ensure_no_target_process(&root, &target);
            if verdict.is_err() || std::time::Instant::now() >= deadline {
                break verdict;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        child.kill().ok();
        child.wait().ok();
        let (reason, check) = verdict.expect_err("a child holding a target file uses the sandbox");
        assert_eq!(reason, SkipReason::ActiveOwner);
        assert_eq!(
            check.token(),
            format!("process_uses_sandbox:{}", child.id())
        );

        // Once it is gone the scan admits the target again.
        ensure_no_target_process(&root, &target).unwrap();
    }
}
