//! Reaping init for the PID namespace (#423, hub #3667).
//!
//! Inside `unshare --pid --fork` the first process is PID 1. A command that is PID 1 never reaps
//! an orphaned grandchild, so the dead orphan stays a zombie and `kill(pid, 0)` still succeeds.
//! `pid-isolate` therefore runs `rsi-repo-guard pid-isolate --init -- COMMAND` as that PID 1: it
//! spawns the command as its child, forwards termination signals, and loops on `waitpid(-1)` so
//! every orphan reparented to it is reaped. It exits with the command's status (128+signal when
//! the command was killed by a signal), which ends the namespace as PID 1 exit always did.
//!
//! On Linux the init also marks itself `PR_SET_CHILD_SUBREAPER`, so the same reaping holds when
//! it is used outside a PID namespace (the portable test of the contract). Other Unix hosts have
//! no subreaper; their orphans go to the host init, which reaps them. Non-Unix has no init.
use std::io::Write;
use std::path::Path;

/// Exit status when the init cannot run the command (matches the shell convention).
pub const SPAWN_FAILED: i32 = 127;

#[cfg(unix)]
mod imp {
    use super::SPAWN_FAILED;
    use std::io::Write;
    use std::path::Path;
    use std::sync::atomic::{AtomicI32, Ordering};

    static CHILD: AtomicI32 = AtomicI32::new(0);

    /// Forwarded to the command; PID 1 of a namespace ignores unhandled signals otherwise.
    const FORWARDED: [i32; 6] = [
        libc::SIGTERM,
        libc::SIGINT,
        libc::SIGHUP,
        libc::SIGQUIT,
        libc::SIGUSR1,
        libc::SIGUSR2,
    ];

    pub fn decode(status: i32) -> i32 {
        if libc::WIFEXITED(status) {
            libc::WEXITSTATUS(status)
        } else if libc::WIFSIGNALED(status) {
            128 + libc::WTERMSIG(status)
        } else {
            1
        }
    }

    /// Reap every child that exits until `main` (the command) does; return its decoded status.
    pub fn reap_until(main: libc::pid_t) -> i32 {
        loop {
            let mut status = 0;
            // SAFETY: `status` is a valid out-pointer for the duration of the call.
            let pid = unsafe { libc::waitpid(-1, &raw mut status, 0) };
            if pid < 0 {
                if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                // ECHILD: nothing left to wait for, but the command's status was never seen.
                return 1;
            }
            if pid == main {
                return decode(status);
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn become_subreaper() {
        // SAFETY: PR_SET_CHILD_SUBREAPER takes one integer flag and no pointers.
        unsafe {
            libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0);
        }
    }
    #[cfg(not(target_os = "linux"))]
    fn become_subreaper() {}

    fn forward_signals() {
        for signal in FORWARDED {
            // SAFETY: the handler only loads an atomic and calls kill(2), both
            // async-signal-safe. Registration failure leaves the default disposition.
            let _ = unsafe {
                signal_hook::low_level::register(signal, move || {
                    let child = CHILD.load(Ordering::SeqCst);
                    if child > 0 {
                        libc::kill(child, signal);
                    }
                })
            };
        }
    }

    pub fn run(command: &[String], cwd: &Path, err: &mut dyn Write) -> i32 {
        become_subreaper();
        forward_signals();
        let mut child = std::process::Command::new(&command[0]);
        child.args(&command[1..]).current_dir(cwd);
        let child = match child.spawn() {
            Ok(child) => child,
            Err(error) => {
                let _ = writeln!(err, "pid-isolate: cannot run {}: {error}", command[0]);
                return SPAWN_FAILED;
            }
        };
        #[allow(clippy::cast_possible_wrap)]
        let pid = child.id() as libc::pid_t;
        CHILD.store(pid, Ordering::SeqCst);
        // `child` is never `wait`ed on through std; `reap_until` owns every waitpid.
        reap_until(pid)
    }
}

/// `args` is everything after `--init`: `[--] COMMAND [ARG...]`.
pub fn run(args: &[String], cwd: &Path, err: &mut dyn Write) -> i32 {
    let command = if args.first().is_some_and(|s| s == "--") {
        &args[1..]
    } else {
        args
    };
    if command.is_empty() {
        let _ = writeln!(err, "usage: pid-isolate --init -- COMMAND [ARG...]");
        return 2;
    }
    #[cfg(unix)]
    {
        imp::run(command, cwd, err)
    }
    #[cfg(not(unix))]
    {
        let _ = cwd;
        let _ = writeln!(err, "pid-isolate: --init is unsupported on this platform");
        crate::pid_isolation::UNAVAILABLE_EXIT
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    // `run` itself calls waitpid(-1) and installs process-wide handlers, so it is only
    // exercised through the real binary (tests/pid_isolation.rs), never inside the shared
    // test harness where it would steal other tests' children.
    #[test]
    fn decode_handles_exit_signal_and_unknown_wait_status() {
        assert_eq!(imp::decode(3 << 8), 3);
        assert_eq!(imp::decode(9), 128 + 9);
        assert_eq!(imp::decode(0x7f), 1);
    }

    #[test]
    fn empty_command_is_a_usage_error() {
        let mut err = vec![];
        assert_eq!(run(&[], Path::new("."), &mut err), 2);
        assert_eq!(run(&["--".into()], Path::new("."), &mut err), 2);
    }
}
