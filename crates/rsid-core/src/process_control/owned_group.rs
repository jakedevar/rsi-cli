//! A process group the daemon signals only while it provably owns the group's
//! id (#1251, #1248).
//!
//! A process group's id is its leader's PID. While the leader is unreaped
//! (running, or exited and still a zombie) the kernel cannot give that number
//! to another process or group, so a group signal reaches only the leader's own
//! cohort. Once the leader is reaped the number is free, and a `killpg` on a
//! saved id can hit an unrelated same-UID group (another agent's test unit or
//! lander). Tokio's `Child::wait` and `wait_with_output` reap the leader the
//! moment it exits, independently of pipe EOF, and a timed-out future drops its
//! child before any caller code runs. So this handle never lets tokio reap until
//! the caller has finished signalling: it observes exit with
//! `waitid(WEXITED | WNOWAIT)`, which leaves the zombie in place, and refuses
//! every group signal once reaping has begun.
//!
//! Where `waitid(WNOWAIT)` is unavailable (macOS) the leader is reaped when its
//! exit is observed; from then on a group signal is logged and skipped.
//!
//! Descendants that call `setsid`/`setpgid` leave the group and are never
//! signalled through it; [`capture_owned_group`] bounds how long they can hold
//! the output pipes, then releases them.

use super::terminate_process_group_because;
use std::process::{ExitStatus, Output};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Child;
use tokio::time::{Instant, sleep_until};

/// The direct child of a command spawned with `process_group(0)`, held
/// unreaped so its group id stays pinned while the daemon may still signal it.
pub struct OwnedGroupLeader {
    child: Child,
    pgid: nix::unistd::Pid,
    /// Set once this handle starts reaping the leader (or learns it is not
    /// its child to reap): from then on the group id is not provably owned and
    /// no group signal is sent, this handle's drop included.
    released: bool,
}

impl OwnedGroupLeader {
    /// Take a just-spawned child that was configured with `process_group(0)`
    /// and has never been waited on. Take its pipes first.
    pub fn new(child: Child) -> std::io::Result<Self> {
        let pid = child
            .id()
            .ok_or_else(|| std::io::Error::other("spawned child was already reaped"))?;
        let pid = i32::try_from(pid)
            .map_err(|_| std::io::Error::other("spawned child pid is out of range"))?;
        Ok(Self {
            child,
            pgid: nix::unistd::Pid::from_raw(pid),
            released: false,
        })
    }

    /// The group id (the leader's PID).
    #[must_use]
    pub fn pgid(&self) -> nix::unistd::Pid {
        self.pgid
    }

    /// Whether a group signal is still provably delivered to this group: the
    /// leader has not been reaped, so its id cannot have been reused.
    #[must_use]
    pub fn owns_group(&self) -> bool {
        !self.released && self.child.id().is_some()
    }

    /// Resolve once the leader has exited. On Linux the leader is left a
    /// zombie, so the group stays signallable; elsewhere it is reaped here and
    /// later group signals are skipped. Cancel-safe.
    pub async fn exited(&mut self) -> std::io::Result<()> {
        #[cfg(any(
            target_os = "android",
            target_os = "freebsd",
            target_os = "haiku",
            all(target_os = "linux", not(target_env = "uclibc"))
        ))]
        {
            use nix::errno::Errno;
            use nix::sys::wait::{Id, WaitPidFlag, WaitStatus, waitid};
            /// The longest wait between polls of an unreaped leader; a poll
            /// starts at 1ms so a short-lived child is not delayed by it.
            const EXIT_POLL_INTERVAL: Duration = Duration::from_millis(50);
            if self.released {
                return Ok(());
            }
            let mut poll = Duration::from_millis(1);
            loop {
                match waitid(
                    Id::Pid(self.pgid),
                    WaitPidFlag::WEXITED | WaitPidFlag::WNOWAIT | WaitPidFlag::WNOHANG,
                ) {
                    Ok(WaitStatus::StillAlive) => {
                        tokio::time::sleep(poll).await;
                        poll = (poll * 2).min(EXIT_POLL_INTERVAL);
                    }
                    Ok(_) => return Ok(()),
                    Err(Errno::EINTR) => {}
                    Err(error) => {
                        // ECHILD: something else reaped it. Ownership of the id
                        // is no longer provable.
                        self.released = true;
                        return Err(std::io::Error::from(error));
                    }
                }
            }
        }
        #[cfg(not(any(
            target_os = "android",
            target_os = "freebsd",
            target_os = "haiku",
            all(target_os = "linux", not(target_env = "uclibc"))
        )))]
        {
            // No non-reaping wait: observing exit reaps the leader, which ends
            // provable ownership of its group id.
            let result = self.child.wait().await;
            self.released = true;
            result.map(|_| ())
        }
    }

    /// SIGKILL the leader's whole group if, and only if, the leader is still
    /// unreaped. Otherwise log and skip: the saved id may name another group.
    /// Returns whether the signal was sent.
    #[track_caller]
    pub fn terminate_group(&self, reason: &str) -> bool {
        if !self.owns_group() {
            tracing::warn!(
                target: "rsid::signal",
                pgid = self.pgid.as_raw(),
                reason,
                caller = %std::panic::Location::caller(),
                "group signal skipped: the leader was already reaped, so its group id is no longer provably owned",
            );
            return false;
        }
        terminate_process_group_because(self.pgid, reason);
        true
    }

    /// Reap the leader. From the first poll on, this handle sends no group
    /// signal (its drop included): the id may be reused once reaped.
    pub async fn reap(mut self) -> std::io::Result<ExitStatus> {
        self.released = true;
        self.child.wait().await
    }
}

impl Drop for OwnedGroupLeader {
    fn drop(&mut self) {
        // Dropped before the caller settled the group (its future was
        // cancelled): stop the group while the leader still pins its id. The
        // child itself is then killed and reaped by tokio.
        if self.owns_group() {
            terminate_process_group_because(
                self.pgid,
                "owned process group dropped before it was settled",
            );
        }
    }
}

/// Time bounds for [`capture_owned_group`] besides its deadline.
#[derive(Debug, Clone, Copy)]
pub struct OwnedGroupLimits {
    /// How long the output pipes may stay open after the leader exits before
    /// the group is stopped and the pipes are released.
    pub post_exit_drain: Duration,
    /// How long to wait for the leader's exit and the pipes' EOF after the
    /// group was stopped.
    pub cleanup: Duration,
}

/// Why [`capture_owned_group`] produced no output.
#[derive(Debug)]
pub enum OwnedGroupError {
    /// The deadline passed; the group was stopped while its leader was
    /// unreaped.
    TimedOut,
    /// Reading a pipe or waiting for the leader failed.
    Io(std::io::Error),
}

/// Capture the stdout and stderr of `child` (spawned with `process_group(0)`,
/// both pipes piped) until its leader exits, stopping its group only while the
/// leader is unreaped (#1251).
///
/// - Past `deadline` the group is SIGKILLed and the run is `TimedOut`.
/// - After the leader exits, the pipes get `limits.post_exit_drain` to reach
///   EOF; then the group is SIGKILLed (the zombie leader still pins its id) and
///   any descendant that left the group is not signalled: its pipe ends are
///   released after `limits.cleanup` and the output read so far is returned.
/// - A normal exit with both pipes at EOF sends no signal.
pub async fn capture_owned_group(
    mut child: Child,
    deadline: Instant,
    limits: OwnedGroupLimits,
    reason: &str,
) -> Result<Output, OwnedGroupError> {
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let mut leader = OwnedGroupLeader::new(child).map_err(OwnedGroupError::Io)?;
    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    let mut read_failure = None;
    let mut exit_failure = None;
    let mut timed_out = false;
    {
        let mut stdout_read = std::pin::pin!(drain(stdout, &mut stdout_bytes));
        let mut stderr_read = std::pin::pin!(drain(stderr, &mut stderr_bytes));
        let mut stdout_done = false;
        let mut stderr_done = false;
        let mut exited_at: Option<Instant> = None;
        let mut cleanup_deadline: Option<Instant> = None;
        loop {
            if (exited_at.is_some() || exit_failure.is_some()) && stdout_done && stderr_done {
                break;
            }
            let now = Instant::now();
            if cleanup_deadline.is_none() {
                let stop = if now >= deadline {
                    timed_out = true;
                    Some(reason.to_string())
                } else if exited_at.is_some_and(|exited| now >= exited + limits.post_exit_drain) {
                    Some(format!(
                        "{reason}: the output stayed open after the leader exited"
                    ))
                } else if exit_failure.is_some() {
                    Some(format!("{reason}: the leader could not be waited for"))
                } else {
                    None
                };
                if let Some(stop) = stop {
                    leader.terminate_group(&stop);
                    cleanup_deadline = Some(now + limits.cleanup);
                }
            }
            if let Some(cleanup) = cleanup_deadline
                && now >= cleanup
            {
                if !(stdout_done && stderr_done) {
                    tracing::warn!(
                        target: "rsid::signal",
                        pgid = leader.pgid().as_raw(),
                        reason,
                        "a process outside the owned group still holds its output; \
                         releasing the pipes without signalling it",
                    );
                }
                break;
            }
            let wake = cleanup_deadline.unwrap_or_else(|| {
                exited_at.map_or(deadline, |exited| {
                    deadline.min(exited + limits.post_exit_drain)
                })
            });
            tokio::select! {
                biased;
                observed = leader.exited(), if exited_at.is_none() && exit_failure.is_none() => {
                    match observed {
                        Ok(()) => exited_at = Some(Instant::now()),
                        Err(error) => exit_failure = Some(error),
                    }
                }
                observed = &mut stdout_read, if !stdout_done => {
                    stdout_done = true;
                    if let Err(error) = observed {
                        read_failure.get_or_insert(error);
                    }
                }
                observed = &mut stderr_read, if !stderr_done => {
                    stderr_done = true;
                    if let Err(error) = observed {
                        read_failure.get_or_insert(error);
                    }
                }
                () = sleep_until(wake) => {}
            }
        }
    }
    // Every signal is sent; only now may the leader be reaped.
    let reaped = tokio::time::timeout(limits.cleanup, leader.reap()).await;
    if timed_out {
        return Err(OwnedGroupError::TimedOut);
    }
    if let Some(error) = exit_failure.or(read_failure) {
        return Err(OwnedGroupError::Io(error));
    }
    let status = reaped
        .map_err(|_| OwnedGroupError::Io(std::io::Error::other("leader reap timed out")))?
        .map_err(OwnedGroupError::Io)?;
    Ok(Output {
        status,
        stdout: stdout_bytes,
        stderr: stderr_bytes,
    })
}

/// Append everything `reader` yields to `bytes` in chunks, so a cancelled
/// drain keeps what it already read.
async fn drain<R>(reader: Option<R>, bytes: &mut Vec<u8>) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
{
    let Some(mut reader) = reader else {
        return Ok(());
    };
    let mut buffer = [0_u8; super::PROCESS_READ_CHUNK_BYTES];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            return Ok(());
        }
        bytes.extend_from_slice(&buffer[..read]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process_control::recorded_group_signals::{last_to, sent_to};
    use std::process::Stdio;
    use tokio::process::Command;

    fn limits() -> OwnedGroupLimits {
        OwnedGroupLimits {
            post_exit_drain: Duration::from_millis(300),
            cleanup: Duration::from_secs(2),
        }
    }

    fn script(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("owned-leader.sh");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn spawn(path: &std::path::Path) -> Child {
        let mut command = Command::new(path);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .process_group(0);
        // A parallel test's fork can briefly hold the fresh script open for
        // writing (ETXTBSY).
        for _ in 0..100 {
            match command.spawn() {
                Err(error) if error.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                spawned => return spawned.unwrap(),
            }
        }
        panic!("the test script stayed busy");
    }

    /// The process `pid` still exists (signal 0 delivers nothing).
    fn exists(pid: i32) -> bool {
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
    }

    #[tokio::test]
    async fn a_normal_exit_sends_no_group_signal() {
        let dir = tempfile::tempdir().unwrap();
        let child = spawn(&script(dir.path(), "echo out; echo err >&2; exit 3"));
        let pgid = i32::try_from(child.id().unwrap()).unwrap();
        let output = capture_owned_group(
            child,
            Instant::now() + Duration::from_secs(30),
            limits(),
            "test",
        )
        .await
        .unwrap();
        assert_eq!(output.status.code(), Some(3));
        assert_eq!(output.stdout, b"out\n");
        assert_eq!(output.stderr, b"err\n");
        assert_eq!(sent_to(pgid), 0, "a settled group is never signalled");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn an_exited_leader_stays_unreaped_until_reap_and_then_refuses_signals() {
        let dir = tempfile::tempdir().unwrap();
        let mut leader = OwnedGroupLeader::new(spawn(&script(dir.path(), "exit 0"))).unwrap();
        let pgid = leader.pgid().as_raw();
        tokio::time::timeout(Duration::from_secs(10), leader.exited())
            .await
            .unwrap()
            .unwrap();
        // The leader is a zombie: its id is still ours.
        assert!(leader.owns_group());
        assert!(exists(pgid), "the exited leader was reaped by observing it");
        let status = leader.reap().await.unwrap();
        assert!(status.success());
        assert_eq!(sent_to(pgid), 0, "a reaped leader's drop sends no signal");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_released_leader_skips_its_group_signal() {
        let dir = tempfile::tempdir().unwrap();
        let mut leader = OwnedGroupLeader::new(spawn(&script(dir.path(), "exit 0"))).unwrap();
        let pgid = leader.pgid().as_raw();
        leader.exited().await.unwrap();
        // Model the reap having begun without letting the number go: the
        // handle must already refuse the saved id.
        leader.released = true;
        assert!(!leader.terminate_group("test"));
        assert_eq!(sent_to(pgid), 0);
        leader.released = false;
        let status = leader.reap().await.unwrap();
        assert!(status.success());
        assert_eq!(sent_to(pgid), 0);
    }

    /// #1251: the leader exits while a descendant that left its group keeps
    /// stdout open. The gate is not held for the deadline; the group signal
    /// lands while the zombie leader still pins the id (the recorder reads the
    /// leader's own name from /proc), and the escaped descendant, which ends on
    /// its own, is never signalled.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn an_exited_leader_with_an_escaped_pipe_holder_is_bounded_and_signalled_while_unreaped()
    {
        let dir = tempfile::tempdir().unwrap();
        let holder_pid = dir.path().join("holder.pid");
        let body = format!(
            "setsid sh -c 'echo $$ > {pid}; sleep 4' &\n\
             while [ ! -s {pid} ]; do sleep 0.05; done\n\
             echo published\n\
             exit 0",
            pid = holder_pid.display()
        );
        let child = spawn(&script(dir.path(), &body));
        let pgid = i32::try_from(child.id().unwrap()).unwrap();
        let started = std::time::Instant::now();
        let output = capture_owned_group(
            child,
            Instant::now() + Duration::from_secs(60),
            OwnedGroupLimits {
                post_exit_drain: Duration::from_millis(300),
                cleanup: Duration::from_millis(300),
            },
            "test",
        )
        .await
        .unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "the escaped holder kept the capture open: {:?}",
            started.elapsed()
        );
        assert!(output.status.success());
        assert_eq!(output.stdout, b"published\n");
        let signal = last_to(pgid).expect("the leader's group was stopped");
        assert_eq!(signal.signal, nix::sys::signal::Signal::SIGKILL);
        assert_eq!(
            signal.target.comm.as_deref(),
            Some("owned-leader.sh"),
            "the group was signalled after its leader was reaped"
        );
        let holder: i32 = std::fs::read_to_string(&holder_pid)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_ne!(holder, pgid);
        assert_eq!(sent_to(holder), 0, "the escaped holder was signalled");
    }

    /// #1248: a timed-out run stops the group before anything reaps the
    /// leader, so the signal reaches the leader's own group and its member.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_timeout_signals_the_group_before_the_leader_is_reaped() {
        let dir = tempfile::tempdir().unwrap();
        let member_pid = dir.path().join("member.pid");
        let body = format!(
            "sleep 30 & echo $! > {pid}\nwait",
            pid = member_pid.display()
        );
        let child = spawn(&script(dir.path(), &body));
        let pgid = i32::try_from(child.id().unwrap()).unwrap();
        let result = capture_owned_group(
            child,
            Instant::now() + Duration::from_millis(500),
            limits(),
            "test timeout",
        )
        .await;
        assert!(matches!(result, Err(OwnedGroupError::TimedOut)));
        let signal = last_to(pgid).expect("the timed-out group was stopped");
        assert_eq!(signal.reason, "test timeout");
        assert_eq!(signal.target.comm.as_deref(), Some("owned-leader.sh"));
        let member: i32 = std::fs::read_to_string(&member_pid)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let mut alive = true;
        for _ in 0..50 {
            alive = exists(member);
            if !alive {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(!alive, "the group member outlived the timeout");
    }

    /// #1248: dropping a running capture (its caller was cancelled) stops the
    /// group from the handle's drop while the leader is still unreaped.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_dropped_capture_signals_the_group_while_the_leader_is_unreaped() {
        let dir = tempfile::tempdir().unwrap();
        let child = spawn(&script(dir.path(), "exec sleep 30"));
        let pgid = i32::try_from(child.id().unwrap()).unwrap();
        let capture = capture_owned_group(
            child,
            Instant::now() + Duration::from_secs(60),
            limits(),
            "test",
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(300), capture)
                .await
                .is_err()
        );
        let signal = last_to(pgid).expect("the dropped capture stopped its group");
        assert_eq!(
            signal.reason,
            "owned process group dropped before it was settled"
        );
        // `exec sleep` keeps the leader's pid; it was alive when signalled.
        assert_eq!(signal.target.comm.as_deref(), Some("sleep"));
    }
}
