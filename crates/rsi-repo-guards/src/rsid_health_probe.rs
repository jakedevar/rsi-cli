//! `rsid-health-probe`: the GetHealthStatus readiness probe of `scripts/rsid-health.sh`,
//! once an inline interpreter heredoc (#465, umbrella #358).
//!
//! ```text
//! rsi-repo-guard rsid-health-probe <rsi-rpc> <socket> [launcher-pid]
//! ```
//!
//! With a launcher PID the probe connects to the Unix socket itself, sends one
//! `GetHealthStatus` request and binds the reply to its sender: the Linux
//! `SCM_CREDENTIALS` of every received chunk, or the Darwin `LOCAL_PEERPID` for a
//! daemon that predates `daemon_pid`. The responder must be the launcher or one
//! of its descendants. Without a launcher it runs `<rsi-rpc> --socket <socket>
//! GetHealthStatus` and judges the reply alone. The whole probe has a 2 s budget.
//!
//! Exit status (the contract `wait_for_rsid_health` reads): 0 ready; 1 not ready
//! (including a foreign daemon, an invalid identity or a timeout); 2 the launcher
//! or the daemon process is gone; 3 the process that answered exited right after
//! replying. Nothing is printed: the caller's progress line is the report.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::Ctx;
use crate::bounded_run::run_bounded;

const REQUEST: &[u8] = br#"{"jsonrpc":"2.0","id":1,"method":"GetHealthStatus","params":null}"#;
const BUDGET: Duration = Duration::from_secs(2);
const MAX_REPLY: usize = 1024 * 1024;

/// What a probe concluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Ready,
    NotReady,
    /// The launcher (or the daemon) process no longer exists.
    ProcessGone,
    /// The process that answered no longer exists.
    ResponderGone,
}

impl Outcome {
    const fn code(self) -> i32 {
        match self {
            Self::Ready => 0,
            Self::NotReady => 1,
            Self::ProcessGone => 2,
            Self::ResponderGone => 3,
        }
    }
}

/// Why a step stopped (the Python probe's exception classes).
enum Stop {
    /// `ProcessLookupError`: a tracked process is gone.
    Gone,
    /// `ResponderExited`: the replying process is gone.
    Responder,
    /// `OSError` / `TimeoutExpired` / `ValueError`: not ready.
    Bad,
}

/// `rsid-health-probe <rpc> <socket> [launcher]`.
pub fn main(args: &[String], _ctx: &Ctx, _out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let (Some(rpc), Some(socket)) = (args.first(), args.get(1)) else {
        let _ = writeln!(
            err,
            "usage: rsi-repo-guard rsid-health-probe <rsi-rpc> <socket> [launcher-pid]"
        );
        return 64;
    };
    let launcher = match args.get(2).map(String::as_str) {
        None | Some("") => None,
        Some(raw) => match raw.trim().parse::<i64>() {
            Ok(pid) => Some(pid),
            // The Python probe died on `int(...)` with a traceback; not ready is the closed answer.
            Err(_) => return Outcome::NotReady.code(),
        },
    };
    probe(rpc, socket, launcher, Instant::now() + BUDGET).code()
}

/// Run one probe against `deadline`.
#[must_use]
pub fn probe(rpc: &str, socket: &str, launcher: Option<i64>, deadline: Instant) -> Outcome {
    let result = match launcher {
        Some(launcher) => probe_launcher(socket, launcher, deadline),
        None => probe_rpc(rpc, socket, deadline),
    };
    match result {
        Ok(reply) => {
            if reply_ready(&reply) {
                Outcome::Ready
            } else {
                Outcome::NotReady
            }
        }
        Err(Stop::Responder) => Outcome::ResponderGone,
        Err(Stop::Gone) => Outcome::ProcessGone,
        Err(Stop::Bad) => Outcome::NotReady,
    }
}

fn remaining(deadline: Instant) -> Duration {
    deadline
        .saturating_duration_since(Instant::now())
        .max(Duration::from_millis(1))
}

/// `ps -o ppid=,stat= -p PID` -> the parent PID; a missing or zombie process is gone.
fn process_parent(pid: i64, deadline: Instant) -> Result<i64, Stop> {
    let mut cmd = Command::new("ps");
    cmd.args(["-o", "ppid=,stat=", "-p", &pid.to_string()]);
    let (ok, stdout) = run_bounded(cmd, deadline).map_err(|_| Stop::Bad)?;
    let text = String::from_utf8_lossy(&stdout);
    let fields: Vec<&str> = text.split_whitespace().collect();
    if !ok || fields.len() != 2 || fields[1].starts_with('Z') {
        return Err(Stop::Gone);
    }
    fields[0].parse::<i64>().map_err(|_| Stop::Bad)
}

/// Is `pid` the launcher or one of its descendants? Only the first process (the one
/// that answered) maps its disappearance to [`Stop::Responder`].
fn in_launcher_tree(mut pid: i64, launcher: i64, deadline: Instant) -> Result<bool, Stop> {
    let mut seen = Vec::new();
    let mut first = true;
    while pid > 1 && !seen.contains(&pid) && Instant::now() < deadline {
        seen.push(pid);
        let parent = match process_parent(pid, deadline) {
            Ok(parent) => parent,
            Err(Stop::Gone) if first => return Err(Stop::Responder),
            Err(stop) => return Err(stop),
        };
        first = false;
        if pid == launcher {
            return Ok(true);
        }
        pid = parent;
    }
    Ok(false)
}

fn probe_rpc(rpc: &str, socket: &str, deadline: Instant) -> Result<Value, Stop> {
    let mut cmd = Command::new(rpc);
    cmd.args(["--socket", socket, "GetHealthStatus"]);
    let (ok, stdout) = run_bounded(cmd, deadline).map_err(|_| Stop::Bad)?;
    if !ok {
        return Err(Stop::Bad);
    }
    serde_json::from_slice(&stdout).map_err(|_| Stop::Bad)
}

fn probe_launcher(socket: &str, launcher: i64, deadline: Instant) -> Result<Value, Stop> {
    if launcher <= 1 {
        return Err(Stop::Gone);
    }
    process_parent(launcher, deadline)?;
    let reply = {
        let mut conn = connect(socket, deadline)?;
        conn.write_all(REQUEST).map_err(|_| Stop::Bad)?;
        conn.write_all(b"\n").map_err(|_| Stop::Bad)?;
        let (line, mut senders) = read_reply(&mut conn, deadline)?;
        let reply: Value = serde_json::from_slice(&line).map_err(|_| Stop::Bad)?;
        let result = reply.get("result").filter(|result| result.is_object());
        let pid = result.and_then(|result| result.get("daemon_pid"));
        let started = result.and_then(|result| result.get("daemon_started_at"));
        if pid.is_some_and(|v| !v.is_null()) || started.is_some_and(|v| !v.is_null()) {
            let pid = pid
                .and_then(Value::as_i64)
                .filter(|pid| *pid > 1)
                .ok_or(Stop::Bad)?;
            let started = started.and_then(Value::as_str).ok_or(Stop::Bad)?;
            // A startup time without a timezone is an invalid identity.
            chrono::DateTime::parse_from_rfc3339(started).map_err(|_| Stop::Bad)?;
            if cfg!(target_os = "linux") && senders != [pid] {
                return Err(Stop::Bad);
            }
            senders = vec![pid];
        } else if cfg!(target_os = "macos") {
            // Legacy Darwin SOL_LOCAL=0 / LOCAL_PEERPID=2 fallback.
            senders.push(peer_pid(&conn)?);
        }
        senders.sort_unstable();
        senders.dedup();
        let [sender] = senders[..] else {
            return Err(Stop::Bad);
        };
        if !in_launcher_tree(sender, launcher, deadline)? {
            return Err(Stop::Bad);
        }
        reply
    };
    process_parent(launcher, deadline)?;
    Ok(reply)
}

fn connect(path: &str, deadline: Instant) -> Result<UnixStream, Stop> {
    if !cfg!(any(target_os = "linux", target_os = "macos")) {
        return Err(Stop::Bad); // daemon identity probe unsupported on this platform
    }
    let conn = UnixStream::connect(path).map_err(|_| Stop::Bad)?;
    conn.set_write_timeout(Some(remaining(deadline)))
        .map_err(|_| Stop::Bad)?;
    enable_credentials(&conn)?;
    Ok(conn)
}

#[cfg(target_os = "linux")]
fn enable_credentials(conn: &UnixStream) -> Result<(), Stop> {
    use std::os::fd::AsRawFd;
    let on: libc::c_int = 1;
    // SAFETY: the fd is a live socket owned by `conn`; `on` outlives the call and the
    // length is its size.
    let rc = unsafe {
        libc::setsockopt(
            conn.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PASSCRED,
            (&raw const on).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if rc == 0 { Ok(()) } else { Err(Stop::Bad) }
}

#[cfg(not(target_os = "linux"))]
#[allow(clippy::unnecessary_wraps)]
fn enable_credentials(_conn: &UnixStream) -> Result<(), Stop> {
    Ok(())
}

#[cfg(target_os = "macos")]
fn peer_pid(conn: &UnixStream) -> Result<i64, Stop> {
    use std::os::fd::AsRawFd;
    let mut pid: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: the fd is a live socket; `pid` and `len` are valid out-pointers sized for an int.
    let rc = unsafe {
        libc::getsockopt(
            conn.as_raw_fd(),
            0, // SOL_LOCAL
            2, // LOCAL_PEERPID
            (&raw mut pid).cast(),
            &raw mut len,
        )
    };
    if rc == 0 {
        Ok(i64::from(pid))
    } else {
        Err(Stop::Bad)
    }
}

#[cfg(not(target_os = "macos"))]
const fn peer_pid(_conn: &UnixStream) -> Result<i64, Stop> {
    Err(Stop::Bad)
}

/// Read one newline-terminated reply (at most 1 MiB); the senders are the PIDs the
/// kernel attached to each chunk on Linux (every chunk must carry credentials).
fn read_reply(conn: &mut UnixStream, deadline: Instant) -> Result<(Vec<u8>, Vec<i64>), Stop> {
    let mut data: Vec<u8> = Vec::new();
    let mut senders: Vec<i64> = Vec::new();
    while !data.contains(&b'\n') && data.len() < MAX_REPLY {
        conn.set_read_timeout(Some(remaining(deadline)))
            .map_err(|_| Stop::Bad)?;
        let mut buf = vec![0u8; 65536];
        let (n, creds) = recv_chunk(conn, &mut buf)?;
        if cfg!(target_os = "linux") {
            if creds.is_empty() {
                return Err(Stop::Bad); // health reply has no sender credentials
            }
            senders.extend(creds);
        }
        if n == 0 {
            return Err(Stop::Bad); // incomplete health reply
        }
        data.extend_from_slice(&buf[..n]);
    }
    let Some(end) = data.iter().position(|b| *b == b'\n') else {
        return Err(Stop::Bad); // oversized health reply
    };
    data.truncate(end);
    Ok((data, senders))
}

#[cfg(target_os = "linux")]
fn recv_chunk(conn: &UnixStream, buf: &mut [u8]) -> Result<(usize, Vec<i64>), Stop> {
    use std::os::fd::AsRawFd;
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    // 8-byte aligned and larger than CMSG_SPACE(sizeof(ucred)) = 32 on Linux.
    let mut control = [0u64; 8];
    // SAFETY: an all-zero msghdr is a valid empty message header.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &raw mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    // SAFETY: CMSG_SPACE is a pure size computation.
    msg.msg_controllen =
        unsafe { libc::CMSG_SPACE(std::mem::size_of::<libc::ucred>() as u32) } as _;
    let received = loop {
        // SAFETY: `msg` points at `iov` (into `buf`) and `control`, both live and writable.
        let n = unsafe { libc::recvmsg(conn.as_raw_fd(), &raw mut msg, 0) };
        if n >= 0 {
            if let Ok(n) = usize::try_from(n) {
                break n;
            }
        }
        if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return Err(Stop::Bad);
        }
    };
    if msg.msg_flags & libc::MSG_CTRUNC != 0 {
        return Err(Stop::Bad); // truncated credentials
    }
    let mut pids = Vec::new();
    // SAFETY: the kernel filled `msg` for `recvmsg`; the CMSG_* walk stays inside
    // `control` as bounded by `msg_controllen`.
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(&raw const msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_CREDENTIALS
            {
                let cred = std::ptr::read_unaligned(libc::CMSG_DATA(cmsg).cast::<libc::ucred>());
                pids.push(i64::from(cred.pid));
            }
            cmsg = libc::CMSG_NXTHDR(&raw const msg, cmsg);
        }
    }
    Ok((received, pids))
}

#[cfg(not(target_os = "linux"))]
fn recv_chunk(conn: &mut UnixStream, buf: &mut [u8]) -> Result<(usize, Vec<i64>), Stop> {
    std::io::Read::read(conn, buf)
        .map(|n| (n, Vec::new()))
        .map_err(|_| Stop::Bad)
}

/// The Python predicate: JSON-RPC 2.0, id 1, no error, and the two integer fields.
fn reply_ready(reply: &Value) -> bool {
    let Some(object) = reply.as_object() else {
        return false;
    };
    object.get("jsonrpc").and_then(Value::as_str) == Some("2.0")
        && object
            .get("id")
            .is_some_and(|id| id.as_i64() == Some(1) || id.as_f64() == Some(1.0))
        && object.get("error").is_none_or(Value::is_null)
        && object
            .get("result")
            .and_then(Value::as_object)
            .is_some_and(|result| {
                result
                    .get("persistence_queue_capacity")
                    .is_some_and(Value::is_i64)
                    && result.get("project_cache_size").is_some_and(Value::is_i64)
            })
}

#[cfg(test)]
#[path = "rsid_health_probe_tests.rs"]
mod tests;
