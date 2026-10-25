//! Run a child process inside a deadline and capture its stdout (#465). Shared by the
//! helpers that replaced inline interpreter heredocs in the product shell scripts.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Why a bounded run produced no output.
#[derive(Debug)]
pub enum RunError {
    /// The program could not be started.
    Spawn(std::io::Error),
    /// It outlived the deadline and its process group was killed.
    Timeout,
    /// Waiting for it or reading its output failed.
    Other,
}

/// Run `cmd` (stdin closed, stderr discarded) in its own process group until it
/// exits or `deadline` passes. Returns whether it exited successfully and its stdout.
/// A timeout kills the whole group, so a client that started children leaves none.
pub fn run_bounded(mut cmd: Command, deadline: Instant) -> Result<(bool, Vec<u8>), RunError> {
    use std::os::unix::process::CommandExt;
    let mut child = cmd
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(RunError::Spawn)?;
    let mut stdout = child.stdout.take().ok_or(RunError::Other)?;
    let reader = std::thread::spawn(move || {
        let mut data = Vec::new();
        let _ = stdout.read_to_end(&mut data);
        data
    });
    let limit = deadline.max(Instant::now() + Duration::from_millis(1));
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let data = reader.join().map_err(|_| RunError::Other)?;
                return Ok((status.success(), data));
            }
            Ok(None) if Instant::now() >= limit => {
                // SAFETY: killpg only signals the process group this function created.
                unsafe { libc::killpg(child.id() as libc::pid_t, libc::SIGKILL) };
                let _ = child.kill();
                let _ = child.wait();
                // The reader ends once the group is gone; never wait for it here.
                return Err(RunError::Timeout);
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(2)),
            Err(_) => return Err(RunError::Other),
        }
    }
}
