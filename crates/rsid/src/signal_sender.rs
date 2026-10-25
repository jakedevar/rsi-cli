//! Who sent the daemon its shutdown signal (#3433).
//!
//! A SIGINT or SIGTERM that reaches `rsid` from another process used to leave
//! no trace of its sender: tokio's signal streams carry no `siginfo`, so the
//! 2026-10-21 hub stop could only be logged as "Received shutdown signal". This
//! module installs, after tokio's own handlers exist, a thin `SA_SIGINFO`
//! handler for SIGINT and SIGTERM that records the sender's pid and uid (and,
//! on Linux, its `comm` and cgroup read in the handler, while the sender is most
//! likely still alive) and then chains to the previous handler, so tokio keeps
//! delivering the signal exactly as before. The handler touches only atomics,
//! `open`/`read`/`close` and the chained handler, all async-signal-safe.
//!
//! [`take_sender`] reads the record; [`describe`] is the one formatter used for
//! the `daemon.log` line, so the next stop names its sender.

#![cfg_attr(not(unix), allow(dead_code, unused_imports))]

use std::sync::atomic::{AtomicI32, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};

/// Longest `comm` kept (the kernel's is 15 bytes plus NUL).
const COMM_CAP: usize = 32;
/// Longest cgroup path kept.
const CGROUP_CAP: usize = 192;

/// The process that sent a signal, as far as it could be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignalSender {
    pub pid: i32,
    pub uid: u32,
    /// `/proc/<pid>/comm`, when the sender was still readable (Linux).
    pub comm: Option<String>,
    /// The sender's cgroup path (its systemd unit), when readable (Linux).
    pub cgroup: Option<String>,
}

/// One log/friction line for a received shutdown signal. `sender` is `None`
/// when the platform cannot say (or the signal came from the kernel).
#[must_use]
pub fn describe(signal: &str, sender: Option<&SignalSender>) -> String {
    let Some(sender) = sender else {
        return format!("{signal} sender=unknown");
    };
    let comm = sender.comm.as_deref().unwrap_or("exited-or-unreadable");
    let cgroup = sender.cgroup.as_deref().unwrap_or("unknown");
    format!(
        "{signal} sender_pid={} sender_uid={} sender_comm={comm} sender_cgroup={cgroup}",
        sender.pid, sender.uid
    )
}

/// Keep printable ASCII only and at most `cap` bytes, so a hostile `comm` or
/// cgroup name cannot forge log lines.
fn sanitize(raw: &[u8], cap: usize) -> Option<String> {
    let text: String = raw
        .iter()
        .take(cap)
        .map(|byte| {
            if byte.is_ascii_graphic() {
                char::from(*byte)
            } else {
                '_'
            }
        })
        .collect();
    (!text.is_empty()).then_some(text)
}

/// The cgroup line of a `/proc/<pid>/cgroup` read: the unified (`0::`) entry's
/// path, else the first line.
fn cgroup_path(raw: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(raw);
    let line = text
        .lines()
        .find(|line| line.starts_with("0::"))
        .or_else(|| text.lines().next())?;
    let path = line.rsplit(':').next().unwrap_or(line);
    sanitize(path.as_bytes(), CGROUP_CAP)
}

struct Slot {
    /// Odd while the handler is writing; a reader retries on a change.
    seq: AtomicU64,
    pid: AtomicI32,
    uid: AtomicU32,
    comm_len: AtomicUsize,
    comm: [AtomicU8; COMM_CAP],
    cgroup_len: AtomicUsize,
    cgroup: [AtomicU8; CGROUP_CAP],
}

#[allow(clippy::declare_interior_mutable_const)]
const BYTE: AtomicU8 = AtomicU8::new(0);

impl Slot {
    const fn new() -> Self {
        Self {
            seq: AtomicU64::new(0),
            pid: AtomicI32::new(0),
            uid: AtomicU32::new(0),
            comm_len: AtomicUsize::new(0),
            comm: [BYTE; COMM_CAP],
            cgroup_len: AtomicUsize::new(0),
            cgroup: [BYTE; CGROUP_CAP],
        }
    }

    fn read(&self) -> Option<SignalSender> {
        let before = self.seq.load(Ordering::Acquire);
        let pid = self.pid.load(Ordering::Relaxed);
        if pid <= 0 {
            return None;
        }
        let uid = self.uid.load(Ordering::Relaxed);
        let comm_len = self.comm_len.load(Ordering::Relaxed).min(COMM_CAP);
        let cgroup_len = self.cgroup_len.load(Ordering::Relaxed).min(CGROUP_CAP);
        let comm: Vec<u8> = self.comm[..comm_len]
            .iter()
            .map(|byte| byte.load(Ordering::Relaxed))
            .collect();
        let cgroup: Vec<u8> = self.cgroup[..cgroup_len]
            .iter()
            .map(|byte| byte.load(Ordering::Relaxed))
            .collect();
        let torn = before % 2 == 1 || self.seq.load(Ordering::Acquire) != before;
        Some(SignalSender {
            pid,
            uid,
            comm: if torn {
                None
            } else {
                sanitize(&comm, COMM_CAP)
            },
            cgroup: if torn { None } else { cgroup_path(&cgroup) },
        })
    }
}

#[cfg(unix)]
mod capture {
    use super::Slot;
    #[cfg(target_os = "linux")]
    use super::{CGROUP_CAP, COMM_CAP};
    use nix::libc;
    use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};

    const SLOT_COUNT: usize = 3;
    #[allow(clippy::declare_interior_mutable_const)]
    const EMPTY: Slot = Slot::new();
    pub(super) static SLOTS: [Slot; SLOT_COUNT] = [EMPTY; SLOT_COUNT];
    #[allow(clippy::declare_interior_mutable_const)]
    const ZERO: AtomicUsize = AtomicUsize::new(0);
    #[allow(clippy::declare_interior_mutable_const)]
    const ZERO_FLAGS: AtomicI32 = AtomicI32::new(0);
    /// The handler each signal had before ours (`sa_sigaction`) and its flags.
    static OLD_HANDLER: [AtomicUsize; SLOT_COUNT] = [ZERO; SLOT_COUNT];
    static OLD_FLAGS: [AtomicI32; SLOT_COUNT] = [ZERO_FLAGS; SLOT_COUNT];

    pub(super) fn slot_index(signal: libc::c_int) -> Option<usize> {
        match signal {
            libc::SIGINT => Some(0),
            libc::SIGTERM => Some(1),
            // Test-only: lets one signal test run without touching SIGINT.
            #[cfg(test)]
            libc::SIGUSR2 => Some(2),
            _ => None,
        }
    }

    /// Format `/proc/<pid>/<leaf>\0` into `buffer` without allocating.
    #[cfg(target_os = "linux")]
    fn proc_path(buffer: &mut [u8; 48], pid: i32, leaf: &[u8]) -> bool {
        const PREFIX: &[u8] = b"/proc/";
        let mut at = PREFIX.len();
        buffer[..at].copy_from_slice(PREFIX);
        let mut digits = [0u8; 12];
        let mut count = 0;
        let mut rest = u32::try_from(pid).unwrap_or(0);
        if rest == 0 {
            return false;
        }
        while rest > 0 {
            digits[count] = b'0' + u8::try_from(rest % 10).unwrap_or(0);
            rest /= 10;
            count += 1;
        }
        if at + count + 1 + leaf.len() + 1 > buffer.len() {
            return false;
        }
        while count > 0 {
            count -= 1;
            buffer[at] = digits[count];
            at += 1;
        }
        buffer[at] = b'/';
        at += 1;
        buffer[at..at + leaf.len()].copy_from_slice(leaf);
        at += leaf.len();
        buffer[at] = 0;
        true
    }

    /// Read up to `out.len()` bytes of a `/proc` file, async-signal-safely.
    #[cfg(target_os = "linux")]
    fn read_proc(pid: i32, leaf: &[u8], out: &mut [u8]) -> usize {
        let mut path = [0u8; 48];
        if !proc_path(&mut path, pid, leaf) {
            return 0;
        }
        // SAFETY: `path` is NUL-terminated; open/read/close are async-signal-safe.
        unsafe {
            let fd = libc::open(
                path.as_ptr().cast::<libc::c_char>(),
                libc::O_RDONLY | libc::O_CLOEXEC,
            );
            if fd < 0 {
                return 0;
            }
            let read = libc::read(fd, out.as_mut_ptr().cast::<libc::c_void>(), out.len());
            libc::close(fd);
            usize::try_from(read).unwrap_or(0)
        }
    }

    #[cfg(target_os = "linux")]
    fn sender_ids(info: *mut libc::siginfo_t) -> (i32, u32) {
        // SAFETY: the kernel hands the handler a valid siginfo for SA_SIGINFO.
        unsafe { ((*info).si_pid(), (*info).si_uid()) }
    }

    #[cfg(not(target_os = "linux"))]
    fn sender_ids(info: *mut libc::siginfo_t) -> (i32, u32) {
        // SAFETY: as above; these are plain fields off Linux.
        unsafe { ((*info).si_pid, (*info).si_uid) }
    }

    extern "C" fn on_signal(
        signal: libc::c_int,
        info: *mut libc::siginfo_t,
        context: *mut libc::c_void,
    ) {
        let Some(index) = slot_index(signal) else {
            return;
        };
        if !info.is_null() {
            let slot = &SLOTS[index];
            let (pid, uid) = sender_ids(info);
            slot.seq.fetch_add(1, Ordering::AcqRel);
            slot.pid.store(pid, Ordering::Relaxed);
            slot.uid.store(uid, Ordering::Relaxed);
            slot.comm_len.store(0, Ordering::Relaxed);
            slot.cgroup_len.store(0, Ordering::Relaxed);
            #[cfg(target_os = "linux")]
            if pid > 0 {
                let mut comm = [0u8; COMM_CAP];
                let mut length = read_proc(pid, b"comm", &mut comm);
                while length > 0 && comm[length - 1] == b'\n' {
                    length -= 1;
                }
                for (byte, value) in slot.comm.iter().zip(&comm[..length]) {
                    byte.store(*value, Ordering::Relaxed);
                }
                slot.comm_len.store(length, Ordering::Relaxed);
                // A cgroup file is mostly short; a long first line is cut.
                let mut cgroup = [0u8; CGROUP_CAP];
                let length = read_proc(pid, b"cgroup", &mut cgroup);
                for (byte, value) in slot.cgroup.iter().zip(&cgroup[..length]) {
                    byte.store(*value, Ordering::Relaxed);
                }
                slot.cgroup_len.store(length, Ordering::Relaxed);
            }
            slot.seq.fetch_add(1, Ordering::AcqRel);
        }
        chain(index, signal, info, context);
    }

    fn chain(
        index: usize,
        signal: libc::c_int,
        info: *mut libc::siginfo_t,
        context: *mut libc::c_void,
    ) {
        let handler = OLD_HANDLER[index].load(Ordering::Acquire);
        let flags = OLD_FLAGS[index].load(Ordering::Acquire);
        if handler == libc::SIG_DFL || handler == libc::SIG_IGN {
            return;
        }
        // SAFETY: `handler` is the `sa_sigaction` the previous owner registered
        // for this signal, called with the signature its flags promise.
        unsafe {
            if flags & libc::SA_SIGINFO != 0 {
                let function: extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void) =
                    std::mem::transmute(handler);
                function(signal, info, context);
            } else {
                let function: extern "C" fn(libc::c_int) = std::mem::transmute(handler);
                function(signal);
            }
        }
    }

    /// Install the recording handler for `signal` once, keeping the previous
    /// handler to chain to.
    pub(super) fn install_one(signal: libc::c_int) -> std::io::Result<()> {
        let Some(index) = slot_index(signal) else {
            return Ok(());
        };
        // SAFETY: plain sigaction calls with zeroed, initialised structs.
        unsafe {
            let mut old: libc::sigaction = std::mem::zeroed();
            if libc::sigaction(signal, std::ptr::null(), &mut old) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            let handler: extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void) =
                on_signal;
            let ours = handler as usize;
            if old.sa_sigaction == ours {
                return Ok(());
            }
            OLD_HANDLER[index].store(old.sa_sigaction, Ordering::Release);
            OLD_FLAGS[index].store(old.sa_flags, Ordering::Release);
            let mut new: libc::sigaction = std::mem::zeroed();
            new.sa_sigaction = ours;
            new.sa_flags = libc::SA_SIGINFO | libc::SA_RESTART;
            libc::sigemptyset(&mut new.sa_mask);
            if libc::sigaction(signal, &new, std::ptr::null_mut()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(())
    }
}

/// Start recording who sends SIGINT and SIGTERM. Call after the runtime's own
/// handlers for those signals exist (they are chained to, never replaced).
/// Failure is logged and leaves the signal path exactly as it was.
pub fn install_shutdown_signal_capture() {
    #[cfg(unix)]
    for signal in [nix::libc::SIGINT, nix::libc::SIGTERM] {
        if let Err(error) = capture::install_one(signal) {
            tracing::warn!(signal, %error, "signal sender capture not installed");
        }
    }
}

/// The sender recorded for the last SIGINT (`sigint == true`) or SIGTERM.
/// `None` off Unix, when no signal with a sender arrived, or when the signal
/// came from the kernel.
#[must_use]
pub fn take_sender(sigint: bool) -> Option<SignalSender> {
    #[cfg(unix)]
    {
        let signal = if sigint {
            nix::libc::SIGINT
        } else {
            nix::libc::SIGTERM
        };
        capture::slot_index(signal).and_then(|index| capture::SLOTS[index].read())
    }
    #[cfg(not(unix))]
    {
        let _ = sigint;
        None
    }
}

#[cfg(test)]
mod tests {
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn the_formatter_names_the_sender_pid_comm_and_cgroup() {
        use super::{SignalSender, describe};
        let sender = SignalSender {
            pid: 4242,
            uid: 1000,
            comm: Some("install-release".into()),
            cgroup: Some("/user.slice/user-1000.slice/app.slice/rsi-job.service".into()),
        };
        let line = describe("SIGINT", Some(&sender));
        assert_eq!(
            line,
            "SIGINT sender_pid=4242 sender_uid=1000 sender_comm=install-release \
             sender_cgroup=/user.slice/user-1000.slice/app.slice/rsi-job.service"
        );
        let gone = SignalSender {
            comm: None,
            cgroup: None,
            ..sender
        };
        assert!(describe("SIGTERM", Some(&gone)).contains("sender_comm=exited-or-unreadable"));
        assert_eq!(describe("SIGTERM", None), "SIGTERM sender=unknown");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn hostile_names_cannot_forge_log_lines() {
        use super::{COMM_CAP, cgroup_path, sanitize};
        assert_eq!(
            sanitize(b"bad name\nsender_pid=1", COMM_CAP).as_deref(),
            Some("bad_name_sender_pid=1")
        );
        assert_eq!(
            cgroup_path(b"0::/user.slice/a.service\n").as_deref(),
            Some("/user.slice/a.service")
        );
        assert_eq!(
            cgroup_path(b"12:cpu:/legacy\n0::/unified\n").as_deref(),
            Some("/unified")
        );
        assert_eq!(cgroup_path(b""), None);
    }

    /// The handler records the sending process: here this very process, which
    /// `kill`s itself with a test-only signal whose previous disposition was
    /// the default (nothing to chain to).
    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn a_signal_records_its_sender_pid() {
        use super::capture;
        capture::install_one(nix::libc::SIGUSR2).expect("install");
        // To this very thread: a process-directed signal may be handled later
        // on another harness thread.
        // SAFETY: signals the calling thread with a signal whose handler is ours.
        let sent =
            unsafe { nix::libc::pthread_kill(nix::libc::pthread_self(), nix::libc::SIGUSR2) };
        assert_eq!(sent, 0, "pthread_kill failed");
        let index = capture::slot_index(nix::libc::SIGUSR2).expect("slot");
        let sender = capture::SLOTS[index].read().expect("sender recorded");
        let own = i32::try_from(std::process::id()).unwrap();
        assert_eq!(sender.pid, own);
        #[cfg(target_os = "linux")]
        {
            assert!(sender.comm.is_some(), "{sender:?}");
            assert!(sender.cgroup.is_some(), "{sender:?}");
        }
    }
}
