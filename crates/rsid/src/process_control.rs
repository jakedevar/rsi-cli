//! Bounded subprocess capture and process-group containment.
//!
//! Short-lived daemon children must not use `Command::output`: it buffers both
//! streams without a limit, kills only the direct child when its future is
//! dropped, and can wait forever when a descendant inherits either pipe.  This
//! module keeps those invariants in one place for provider helpers, tools, and
//! catalog probes.  The synchronous settlement runner uses the same process
//! group and Linux no-escape primitives without changing its polling policy.

use crate::error::{DaemonError, Result as DaemonResult};
use std::fmt;
use std::os::unix::process::CommandExt;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdout, Command};
use tokio::time::{Instant, sleep_until};
use tokio_util::sync::CancellationToken;

pub(crate) const PROCESS_READ_CHUNK_BYTES: usize = 8 * 1024;
pub(crate) const PROCESS_POST_EXIT_DRAIN_TIMEOUT: Duration = Duration::from_millis(250);
pub(crate) const PROCESS_CLEANUP_TIMEOUT: Duration = Duration::from_secs(1);

pub(crate) const SESSION_TOOL_MAX_STREAM_BYTES: usize = 1024 * 1024;
pub(crate) const SESSION_TOOL_TIMEOUT: Duration = Duration::from_secs(120);
pub(crate) const LOCAL_TOOL_MAX_STREAM_BYTES: usize = 50 * 1024;
pub(crate) const LOCAL_TOOL_TIMEOUT: Duration = Duration::from_secs(120);
pub(crate) const MEMORY_CLI_MAX_STDOUT_BYTES: usize = 4 * 1024 * 1024;
pub(crate) const MEMORY_CLI_MAX_STDERR_BYTES: usize = 256 * 1024;
pub(crate) const MEMORY_CLI_TIMEOUT: Duration = Duration::from_secs(300);
pub(crate) const CATALOG_MAX_STDOUT_BYTES: usize = 1024 * 1024;
pub(crate) const CATALOG_MAX_STDERR_BYTES: usize = 64 * 1024;
pub(crate) const CATALOG_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const PROVIDER_MAX_LINE_BYTES: usize = 2 * 1024 * 1024;
pub(crate) const PROVIDER_MAX_STDERR_BYTES: usize = 256 * 1024;
pub(crate) const AGY_MAX_TURN_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OverflowBehavior {
    /// Crossing either stream's bound invalidates the result and terminates the
    /// owned process group.
    Error,
    /// Retain at most the configured prefix while continuing to drain.  This is
    /// appropriate for tools whose already-started side effects must not change
    /// merely because their diagnostic output is large.
    TruncateAndDrain,
    /// Retain the last `limit` bytes of each stream (the true tail) while
    /// continuing to drain. On timeout, partial output captured before the
    /// deadline is returned as `Ok(CapturedOutput { timed_out: true })`
    /// instead of `Err(ExecutionTimedOut)`, so callers can report output
    /// emitted before the timeout. Use this when the diagnostic cause of a
    /// failure lives at the end of the output.
    RetainTail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProcessContainment {
    /// Put the child in a new process group. Descendants that deliberately call
    /// `setsid`/`setpgid` may escape, so callers must still use bounded drains.
    Group,
    /// On Linux, additionally deny `setsid` and `setpgid` in the child and all
    /// descendants with an inherited seccomp filter.
    GroupNoEscape,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct CaptureLimits {
    pub(crate) max_stdout_bytes: usize,
    pub(crate) max_stderr_bytes: usize,
    pub(crate) execution_timeout: Duration,
    pub(crate) post_exit_drain_timeout: Duration,
    pub(crate) cleanup_timeout: Duration,
    pub(crate) overflow: OverflowBehavior,
    pub(crate) containment: ProcessContainment,
}

impl CaptureLimits {
    pub(crate) const fn new(
        max_stdout_bytes: usize,
        max_stderr_bytes: usize,
        execution_timeout: Duration,
        overflow: OverflowBehavior,
        containment: ProcessContainment,
    ) -> Self {
        Self {
            max_stdout_bytes,
            max_stderr_bytes,
            execution_timeout,
            post_exit_drain_timeout: PROCESS_POST_EXIT_DRAIN_TIMEOUT,
            cleanup_timeout: PROCESS_CLEANUP_TIMEOUT,
            overflow,
            containment,
        }
    }

    pub(crate) const fn session_tool() -> Self {
        Self::new(
            SESSION_TOOL_MAX_STREAM_BYTES,
            SESSION_TOOL_MAX_STREAM_BYTES,
            SESSION_TOOL_TIMEOUT,
            OverflowBehavior::TruncateAndDrain,
            ProcessContainment::GroupNoEscape,
        )
    }

    pub(crate) const fn local_tool() -> Self {
        Self::new(
            LOCAL_TOOL_MAX_STREAM_BYTES,
            LOCAL_TOOL_MAX_STREAM_BYTES,
            LOCAL_TOOL_TIMEOUT,
            OverflowBehavior::TruncateAndDrain,
            ProcessContainment::GroupNoEscape,
        )
    }

    pub(crate) const fn memory_cli() -> Self {
        Self::new(
            MEMORY_CLI_MAX_STDOUT_BYTES,
            MEMORY_CLI_MAX_STDERR_BYTES,
            MEMORY_CLI_TIMEOUT,
            OverflowBehavior::Error,
            ProcessContainment::Group,
        )
    }

    pub(crate) const fn catalog() -> Self {
        Self::new(
            CATALOG_MAX_STDOUT_BYTES,
            CATALOG_MAX_STDERR_BYTES,
            CATALOG_TIMEOUT,
            OverflowBehavior::Error,
            ProcessContainment::GroupNoEscape,
        )
    }
}

#[derive(Debug)]
pub(crate) struct CapturedOutput {
    pub(crate) status: ExitStatus,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
    pub(crate) stdout_truncated: bool,
    pub(crate) stderr_truncated: bool,
    /// True when the execution deadline expired and the process was killed.
    /// Only set for [`OverflowBehavior::RetainTail`]; other overflow modes
    /// return `Err(CaptureError::ExecutionTimedOut)` on timeout.
    pub(crate) timed_out: bool,
}

/// Concatenate byte segments into one lossy UTF-8 result without ever retaining
/// more than `max_bytes` of source data. The optional marker is included inside
/// the same final UTF-8 byte bound whenever this function or an upstream stream
/// reader truncated data.
pub(crate) fn bounded_lossy_concat(
    parts: &[&[u8]],
    max_bytes: usize,
    upstream_truncated: bool,
    truncation_marker: &str,
) -> String {
    let mut bytes = Vec::with_capacity(max_bytes.min(PROCESS_READ_CHUNK_BYTES));
    let mut truncated = upstream_truncated;
    for part in parts {
        let retained = part.len().min(max_bytes.saturating_sub(bytes.len()));
        bytes.extend_from_slice(&part[..retained]);
        truncated |= retained != part.len();
    }

    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    truncated |= text.len() > max_bytes;
    truncate_utf8(&mut text, max_bytes);
    if truncated && !truncation_marker.is_empty() {
        let marker_end = floor_char_boundary(truncation_marker, max_bytes);
        let marker = &truncation_marker[..marker_end];
        truncate_utf8(&mut text, max_bytes.saturating_sub(marker.len()));
        text.push_str(marker);
    }
    text
}

fn truncate_utf8(text: &mut String, max_bytes: usize) {
    if text.len() > max_bytes {
        text.truncate(floor_char_boundary(text, max_bytes));
    }
}

fn floor_char_boundary(text: &str, max_bytes: usize) -> usize {
    let mut end = max_bytes.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CaptureError {
    Cancelled,
    ExecutionTimedOut,
    StdoutExceeded {
        limit: usize,
    },
    StderrExceeded {
        limit: usize,
    },
    MissingPipe(&'static str),
    Spawn(String),
    Read {
        stream: &'static str,
        detail: String,
    },
    Wait(String),
    CleanupTimedOut,
    OutputDrainTimedOut,
    Supervisor(String),
}

impl fmt::Display for CaptureError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("process capture cancelled"),
            Self::ExecutionTimedOut => formatter.write_str("process execution timed out"),
            Self::StdoutExceeded { limit } => {
                write!(formatter, "process stdout exceeded {limit}-byte bound")
            }
            Self::StderrExceeded { limit } => {
                write!(formatter, "process stderr exceeded {limit}-byte bound")
            }
            Self::MissingPipe(stream) => write!(formatter, "process {stream} pipe was unavailable"),
            Self::Spawn(detail) => write!(formatter, "process spawn failed: {detail}"),
            Self::Read { stream, detail } => {
                write!(formatter, "process {stream} read failed: {detail}")
            }
            Self::Wait(detail) => write!(formatter, "process wait failed: {detail}"),
            Self::CleanupTimedOut => {
                formatter.write_str("process cleanup timed out before child reap")
            }
            Self::OutputDrainTimedOut => formatter.write_str("process output drain timed out"),
            Self::Supervisor(detail) => write!(formatter, "process supervisor failed: {detail}"),
        }
    }
}

impl std::error::Error for CaptureError {}

impl From<std::io::Error> for CaptureError {
    fn from(error: std::io::Error) -> Self {
        Self::Spawn(error.to_string())
    }
}

/// Configure, spawn, supervise, and capture one short-lived command.
pub(crate) async fn capture_bounded(
    command: Command,
    limits: CaptureLimits,
    cancel: &CancellationToken,
) -> std::result::Result<CapturedOutput, CaptureError> {
    capture_bounded_with_spawn(command, limits, cancel, |mut command| {
        command
            .spawn()
            .map_err(|error| CaptureError::Spawn(error.to_string()))
    })
    .await
}

/// [`capture_bounded`] with a caller-owned final spawn seam.
///
/// Memory/model callers use this closure to consume their one-use execution
/// capability at the exact spawn boundary after this module has installed all
/// pipes and containment settings.
pub(crate) async fn capture_bounded_with_spawn<F>(
    mut command: Command,
    limits: CaptureLimits,
    cancel: &CancellationToken,
    spawn: F,
) -> std::result::Result<CapturedOutput, CaptureError>
where
    F: FnOnce(Command) -> std::result::Result<Child, CaptureError>,
{
    if cancel.is_cancelled() {
        return Err(CaptureError::Cancelled);
    }
    configure_tokio_process_group(&mut command, limits.containment)
        .map_err(|error| CaptureError::Spawn(error.to_string()))?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if cancel.is_cancelled() {
        return Err(CaptureError::Cancelled);
    }

    let mut child = spawn(command)?;
    let pgid = nix::unistd::Pid::from_raw(child.id().ok_or_else(|| {
        CaptureError::Spawn("spawned child did not expose a process id".to_string())
    })? as i32);
    let Some(stdout) = child.stdout.take() else {
        terminate_process_group(pgid);
        let _ = tokio::time::timeout(limits.cleanup_timeout, child.wait()).await;
        return Err(CaptureError::MissingPipe("stdout"));
    };
    let Some(stderr) = child.stderr.take() else {
        terminate_process_group(pgid);
        drop(stdout);
        let _ = tokio::time::timeout(limits.cleanup_timeout, child.wait()).await;
        return Err(CaptureError::MissingPipe("stderr"));
    };

    // The supervisor owns every resource. If its caller is aborted, the drop
    // guard cancels this detached task, which still kills and reaps the child.
    let abandoned = CancellationToken::new();
    let mut abandon_guard = CancelOnDrop::new(abandoned.clone());
    let external_cancel = cancel.clone();
    let supervisor = tokio::spawn(supervise_capture(
        child,
        stdout,
        stderr,
        pgid,
        limits,
        external_cancel,
        abandoned,
    ));
    let joined = supervisor.await;
    abandon_guard.disarm();
    joined.map_err(|error| CaptureError::Supervisor(error.to_string()))?
}

struct CancelOnDrop {
    token: CancellationToken,
    armed: bool,
}

impl CancelOnDrop {
    fn new(token: CancellationToken) -> Self {
        Self { token, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.token.cancel();
        }
    }
}

#[derive(Debug)]
struct PipeCapture {
    bytes: Vec<u8>,
    truncated: bool,
}

async fn supervise_capture(
    mut child: Child,
    stdout: ChildStdout,
    stderr: ChildStderr,
    pgid: nix::unistd::Pid,
    limits: CaptureLimits,
    external_cancel: CancellationToken,
    abandoned: CancellationToken,
) -> std::result::Result<CapturedOutput, CaptureError> {
    let mut wait = Box::pin(child.wait());
    let mut stdout_read = Box::pin(read_pipe(
        stdout,
        "stdout",
        limits.max_stdout_bytes,
        limits.overflow,
    ));
    let mut stderr_read = Box::pin(read_pipe(
        stderr,
        "stderr",
        limits.max_stderr_bytes,
        limits.overflow,
    ));
    let execution_deadline = Instant::now() + limits.execution_timeout;
    let mut status: Option<std::result::Result<ExitStatus, std::io::Error>> = None;
    let mut stdout_result: Option<std::result::Result<PipeCapture, CaptureError>> = None;
    let mut stderr_result: Option<std::result::Result<PipeCapture, CaptureError>> = None;
    let mut failure = None;
    let mut leader_exit_at = None;
    let mut cleanup_deadline = None;
    let mut group_terminated = false;

    loop {
        if let Some(result) = stdout_result
            .as_ref()
            .and_then(|result| result.as_ref().err())
            && failure.is_none()
        {
            failure = Some(result.clone());
        }
        if let Some(result) = stderr_result
            .as_ref()
            .and_then(|result| result.as_ref().err())
            && failure.is_none()
        {
            failure = Some(result.clone());
        }
        if let Some(Err(error)) = status.as_ref()
            && failure.is_none()
        {
            failure = Some(CaptureError::Wait(error.to_string()));
        }

        if status.is_some() && stdout_result.is_some() && stderr_result.is_some() {
            // Tail-retaining capture returns partial output on timeout so the
            // caller can report output emitted before the deadline.
            if matches!(failure, Some(CaptureError::ExecutionTimedOut))
                && limits.overflow == OverflowBehavior::RetainTail
                && let Some(Ok(status)) = status.as_ref()
            {
                let stdout = stdout_result
                    .as_ref()
                    .and_then(|result| result.as_ref().ok())
                    .map_or_else(Vec::new, |capture| capture.bytes.clone());
                let stderr = stderr_result
                    .as_ref()
                    .and_then(|result| result.as_ref().ok())
                    .map_or_else(Vec::new, |capture| capture.bytes.clone());
                let stdout_truncated = stdout_result
                    .as_ref()
                    .and_then(|result| result.as_ref().ok())
                    .is_some_and(|capture| capture.truncated);
                let stderr_truncated = stderr_result
                    .as_ref()
                    .and_then(|result| result.as_ref().ok())
                    .is_some_and(|capture| capture.truncated);
                return Ok(CapturedOutput {
                    status: *status,
                    stdout,
                    stderr,
                    stdout_truncated,
                    stderr_truncated,
                    timed_out: true,
                });
            }
            if let Some(error) = failure {
                return Err(error);
            }
            let status = status
                .expect("status checked above")
                .map_err(|error| CaptureError::Wait(error.to_string()))?;
            let stdout = stdout_result.expect("stdout checked above")?;
            let stderr = stderr_result.expect("stderr checked above")?;
            return Ok(CapturedOutput {
                status,
                stdout: stdout.bytes,
                stderr: stderr.bytes,
                stdout_truncated: stdout.truncated,
                stderr_truncated: stderr.truncated,
                timed_out: false,
            });
        }

        let now = Instant::now();
        if failure.is_none() && status.is_none() && now >= execution_deadline {
            failure = Some(CaptureError::ExecutionTimedOut);
        }
        if failure.is_some() && !group_terminated {
            terminate_process_group(pgid);
            group_terminated = true;
            cleanup_deadline = Some(now + limits.cleanup_timeout);
        }
        if failure.is_none()
            && !group_terminated
            && leader_exit_at.is_some_and(|exited| {
                now >= exited + limits.post_exit_drain_timeout
                    && (stdout_result.is_none() || stderr_result.is_none())
            })
        {
            terminate_process_group(pgid);
            group_terminated = true;
            cleanup_deadline = Some(now + limits.cleanup_timeout);
        }
        if let Some(deadline) = cleanup_deadline
            && now >= deadline
        {
            terminate_process_group(pgid);
            if status.is_none() {
                return Err(CaptureError::CleanupTimedOut);
            }
            return Err(failure.unwrap_or(CaptureError::OutputDrainTimedOut));
        }

        let next_deadline = if let Some(deadline) = cleanup_deadline {
            deadline
        } else if let Some(exited) = leader_exit_at {
            exited + limits.post_exit_drain_timeout
        } else {
            execution_deadline
        };

        tokio::select! {
            biased;
            _ = external_cancel.cancelled(), if failure.is_none() => {
                failure = Some(CaptureError::Cancelled);
            }
            _ = abandoned.cancelled(), if failure.is_none() => {
                failure = Some(CaptureError::Cancelled);
            }
            observed = &mut wait, if status.is_none() => {
                status = Some(observed);
                leader_exit_at.get_or_insert_with(Instant::now);
            }
            observed = &mut stdout_read, if stdout_result.is_none() => {
                stdout_result = Some(observed);
            }
            observed = &mut stderr_read, if stderr_result.is_none() => {
                stderr_result = Some(observed);
            }
            _ = sleep_until(next_deadline) => {}
        }
    }
}

async fn read_pipe<R>(
    mut reader: R,
    stream: &'static str,
    limit: usize,
    overflow: OverflowBehavior,
) -> std::result::Result<PipeCapture, CaptureError>
where
    R: AsyncRead + Unpin,
{
    let mut bytes = Vec::with_capacity(limit.min(PROCESS_READ_CHUNK_BYTES));
    let mut buffer = [0_u8; PROCESS_READ_CHUNK_BYTES];
    let mut truncated = false;
    loop {
        let read = reader
            .read(&mut buffer)
            .await
            .map_err(|error| CaptureError::Read {
                stream,
                detail: error.to_string(),
            })?;
        if read == 0 {
            return Ok(PipeCapture { bytes, truncated });
        }
        let remaining = limit.saturating_sub(bytes.len());
        if read > remaining {
            match overflow {
                OverflowBehavior::Error => {
                    return Err(if stream == "stdout" {
                        CaptureError::StdoutExceeded { limit }
                    } else {
                        CaptureError::StderrExceeded { limit }
                    });
                }
                OverflowBehavior::TruncateAndDrain => {
                    bytes.extend_from_slice(&buffer[..remaining]);
                    truncated = true;
                }
                OverflowBehavior::RetainTail => {
                    // Keep the last `limit` bytes: append all new data, then
                    // trim from the front so the true end of the stream
                    // survives.
                    bytes.extend_from_slice(&buffer[..read]);
                    if bytes.len() > limit {
                        bytes.drain(0..(bytes.len() - limit));
                    }
                    truncated = true;
                }
            }
        } else {
            bytes.extend_from_slice(&buffer[..read]);
        }
        // A continuously writable pipe must not keep this future ready forever
        // and starve cancellation or deadline checks in the supervisor.
        tokio::task::yield_now().await;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BoundedLineError {
    Io(String),
    Exceeded { limit: usize },
    InvalidUtf8,
}

impl fmt::Display for BoundedLineError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(detail) => write!(formatter, "bounded line read failed: {detail}"),
            Self::Exceeded { limit } => write!(formatter, "line exceeded {limit}-byte bound"),
            Self::InvalidUtf8 => formatter.write_str("line was not valid UTF-8"),
        }
    }
}

impl std::error::Error for BoundedLineError {}

/// Newline reader whose allocation is bounded before every extension.
pub(crate) struct BoundedLines<R> {
    reader: BufReader<R>,
    current: Vec<u8>,
    pending_cr: bool,
    max_line_bytes: usize,
}

impl<R> BoundedLines<R>
where
    R: AsyncRead + Unpin,
{
    pub(crate) fn new(reader: R, max_line_bytes: usize) -> Self {
        Self {
            reader: BufReader::with_capacity(PROCESS_READ_CHUNK_BYTES, reader),
            current: Vec::with_capacity(max_line_bytes.min(PROCESS_READ_CHUNK_BYTES)),
            pending_cr: false,
            max_line_bytes,
        }
    }

    pub(crate) async fn next_line(
        &mut self,
    ) -> std::result::Result<Option<String>, BoundedLineError> {
        loop {
            let available = self
                .reader
                .fill_buf()
                .await
                .map_err(|error| BoundedLineError::Io(error.to_string()))?;
            if available.is_empty() {
                if self.pending_cr {
                    if self.current.len() == self.max_line_bytes {
                        return Err(BoundedLineError::Exceeded {
                            limit: self.max_line_bytes,
                        });
                    }
                    self.current.push(b'\r');
                    self.pending_cr = false;
                }
                if self.current.is_empty() {
                    return Ok(None);
                }
                return self.finish_line().map(Some);
            }

            let newline = available.iter().position(|byte| *byte == b'\n');
            let consumed = newline.map_or(available.len(), |position| position + 1);
            let content = newline.map_or(consumed, |position| position);

            if self.pending_cr {
                if newline != Some(0) {
                    if self.current.len() == self.max_line_bytes {
                        return Err(BoundedLineError::Exceeded {
                            limit: self.max_line_bytes,
                        });
                    }
                    self.current.push(b'\r');
                }
                self.pending_cr = false;
            }

            let trailing_cr = content != 0 && available[content - 1] == b'\r';
            let appended = content.saturating_sub(usize::from(trailing_cr));
            if self.current.len().saturating_add(appended) > self.max_line_bytes {
                return Err(BoundedLineError::Exceeded {
                    limit: self.max_line_bytes,
                });
            }
            self.current.extend_from_slice(&available[..appended]);
            self.pending_cr = trailing_cr && newline.is_none();
            self.reader.consume(consumed);
            if newline.is_some() {
                return self.finish_line().map(Some);
            }
        }
    }

    fn finish_line(&mut self) -> std::result::Result<String, BoundedLineError> {
        let bytes = std::mem::take(&mut self.current);
        String::from_utf8(bytes).map_err(|_| BoundedLineError::InvalidUtf8)
    }
}

pub(crate) fn configure_tokio_process_group(
    command: &mut Command,
    containment: ProcessContainment,
) -> DaemonResult<()> {
    configure_std_process_group(command.as_std_mut(), containment)
}

/// Put one command in a new process group and optionally prevent descendants
/// from escaping it. This is shared by async capture and settlement Git.
pub(crate) fn configure_std_process_group(
    command: &mut std::process::Command,
    containment: ProcessContainment,
) -> DaemonResult<()> {
    command.process_group(0);
    if containment == ProcessContainment::GroupNoEscape {
        configure_no_escape(command)?;
    }
    Ok(())
}

/// Terminate the complete group, falling back to the direct child only when
/// group signalling itself fails. ESRCH is already a successful postcondition.
pub(crate) fn terminate_process_group(pgid: nix::unistd::Pid) {
    use nix::errno::Errno;
    use nix::sys::signal::{Signal, kill, killpg};
    match killpg(pgid, Signal::SIGKILL) {
        Ok(()) | Err(Errno::ESRCH) => {}
        Err(_) => {
            let _ = kill(pgid, Signal::SIGKILL);
        }
    }
}

fn configure_no_escape(command: &mut std::process::Command) -> DaemonResult<()> {
    #[cfg(target_os = "linux")]
    {
        if SECCOMP_AUDIT_ARCH == 0 {
            return Err(DaemonError::Process(
                "bounded process containment is unavailable on this Linux architecture".into(),
            ));
        }
        // SAFETY: the callback calls only async-signal-safe syscalls and uses a
        // stack-local BPF program after fork.
        unsafe {
            command.pre_exec(install_no_escape_filter);
        }
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = command;
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn install_no_escape_filter() -> std::io::Result<()> {
    use nix::libc;

    const SECCOMP_NR_OFFSET: u32 = std::mem::offset_of!(libc::seccomp_data, nr) as u32;
    const SECCOMP_ARCH_OFFSET: u32 = std::mem::offset_of!(libc::seccomp_data, arch) as u32;
    const BPF_LOAD_WORD_ABSOLUTE: u16 = (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16;
    const BPF_JUMP_EQUAL: u16 = (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16;
    const BPF_JUMP_BITS_SET: u16 = (libc::BPF_JMP | libc::BPF_JSET | libc::BPF_K) as u16;
    const BPF_RETURN: u16 = (libc::BPF_RET | libc::BPF_K) as u16;
    const DENY_EPERM: u32 = libc::SECCOMP_RET_ERRNO | libc::EPERM as u32;

    let mut filter = [
        bpf_statement(BPF_LOAD_WORD_ABSOLUTE, SECCOMP_ARCH_OFFSET),
        bpf_jump(BPF_JUMP_EQUAL, SECCOMP_AUDIT_ARCH, 1, 0),
        bpf_statement(BPF_RETURN, libc::SECCOMP_RET_KILL_PROCESS),
        bpf_statement(BPF_LOAD_WORD_ABSOLUTE, SECCOMP_NR_OFFSET),
        bpf_jump(BPF_JUMP_BITS_SET, SECCOMP_COMPAT_SYSCALL_BIT, 0, 1),
        bpf_statement(BPF_RETURN, libc::SECCOMP_RET_KILL_PROCESS),
        bpf_jump(BPF_JUMP_EQUAL, libc::SYS_setsid as u32, 2, 0),
        bpf_jump(BPF_JUMP_EQUAL, libc::SYS_setpgid as u32, 1, 0),
        bpf_statement(BPF_RETURN, libc::SECCOMP_RET_ALLOW),
        bpf_statement(BPF_RETURN, DENY_EPERM),
    ];
    let mut program = libc::sock_fprog {
        len: filter.len() as libc::c_ushort,
        filter: filter.as_mut_ptr(),
    };

    // SAFETY: direct async-signal-safe Linux syscalls; the kernel copies the
    // stack-local filter before `seccomp` returns.
    unsafe {
        if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) == -1 {
            return Err(std::io::Error::last_os_error());
        }
        if libc::syscall(
            libc::SYS_seccomp,
            libc::SECCOMP_SET_MODE_FILTER,
            0,
            &mut program as *mut libc::sock_fprog,
        ) == -1
        {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
const fn bpf_statement(code: u16, value: u32) -> nix::libc::sock_filter {
    nix::libc::sock_filter {
        code,
        jt: 0,
        jf: 0,
        k: value,
    }
}

#[cfg(target_os = "linux")]
const fn bpf_jump(code: u16, value: u32, jump_true: u8, jump_false: u8) -> nix::libc::sock_filter {
    nix::libc::sock_filter {
        code,
        jt: jump_true,
        jf: jump_false,
        k: value,
    }
}

#[cfg(all(
    target_os = "linux",
    target_arch = "x86_64",
    target_pointer_width = "64"
))]
const SECCOMP_AUDIT_ARCH: u32 = 0xc000_003e;
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
const SECCOMP_AUDIT_ARCH: u32 = 0xc000_00b7;
#[cfg(all(target_os = "linux", target_arch = "riscv64"))]
const SECCOMP_AUDIT_ARCH: u32 = 0xc000_00f3;
#[cfg(all(target_os = "linux", target_arch = "s390x"))]
const SECCOMP_AUDIT_ARCH: u32 = 0x8000_0016;
#[cfg(all(
    target_os = "linux",
    target_arch = "powerpc64",
    target_endian = "little"
))]
const SECCOMP_AUDIT_ARCH: u32 = 0xc000_0015;
#[cfg(all(target_os = "linux", target_arch = "powerpc64", target_endian = "big"))]
const SECCOMP_AUDIT_ARCH: u32 = 0x8000_0015;
#[cfg(all(target_os = "linux", target_arch = "loongarch64"))]
const SECCOMP_AUDIT_ARCH: u32 = 0xc000_0102;
#[cfg(all(target_os = "linux", target_arch = "sparc64"))]
const SECCOMP_AUDIT_ARCH: u32 = 0x8000_002b;
#[cfg(all(
    target_os = "linux",
    not(any(
        all(target_arch = "x86_64", target_pointer_width = "64"),
        target_arch = "aarch64",
        target_arch = "riscv64",
        target_arch = "s390x",
        target_arch = "powerpc64",
        target_arch = "loongarch64",
        target_arch = "sparc64"
    ))
))]
const SECCOMP_AUDIT_ARCH: u32 = 0;

#[cfg(all(
    target_os = "linux",
    target_arch = "x86_64",
    target_pointer_width = "64"
))]
const SECCOMP_COMPAT_SYSCALL_BIT: u32 = 0x4000_0000;
#[cfg(all(
    target_os = "linux",
    not(all(target_arch = "x86_64", target_pointer_width = "64"))
))]
const SECCOMP_COMPAT_SYSCALL_BIT: u32 = 0;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use tempfile::TempDir;

    fn short_limits(overflow: OverflowBehavior) -> CaptureLimits {
        CaptureLimits {
            max_stdout_bytes: 64,
            max_stderr_bytes: 64,
            execution_timeout: Duration::from_millis(500),
            post_exit_drain_timeout: Duration::from_millis(50),
            cleanup_timeout: Duration::from_millis(250),
            overflow,
            containment: ProcessContainment::GroupNoEscape,
        }
    }

    fn shell(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.args(["-c", script]);
        command
    }

    #[tokio::test]
    async fn bounded_capture_accepts_exact_caps_on_both_streams() {
        let output = capture_bounded(
            shell("head -c 64 /dev/zero; head -c 64 /dev/zero >&2"),
            short_limits(OverflowBehavior::Error),
            &CancellationToken::new(),
        )
        .await
        .expect("exact bounds accepted");
        assert!(output.status.success());
        assert_eq!(output.stdout.len(), 64);
        assert_eq!(output.stderr.len(), 64);
        assert!(!output.stdout_truncated);
        assert!(!output.stderr_truncated);
    }

    #[tokio::test]
    async fn bounded_capture_rejects_one_byte_over_either_cap() {
        for (script, expected) in [
            (
                "head -c 65 /dev/zero",
                CaptureError::StdoutExceeded { limit: 64 },
            ),
            (
                "head -c 65 /dev/zero >&2",
                CaptureError::StderrExceeded { limit: 64 },
            ),
        ] {
            let error = capture_bounded(
                shell(script),
                short_limits(OverflowBehavior::Error),
                &CancellationToken::new(),
            )
            .await
            .expect_err("one extra byte rejected");
            assert_eq!(error, expected);
        }
    }

    #[tokio::test]
    async fn truncate_mode_stays_bounded_while_draining_to_exit() {
        let output = capture_bounded(
            shell("head -c 1048576 /dev/zero; head -c 1048576 /dev/zero >&2"),
            short_limits(OverflowBehavior::TruncateAndDrain),
            &CancellationToken::new(),
        )
        .await
        .expect("large output drained");
        assert!(output.status.success());
        assert_eq!(output.stdout.len(), 64);
        assert_eq!(output.stderr.len(), 64);
        assert!(output.stdout_truncated);
        assert!(output.stderr_truncated);
    }

    #[tokio::test]
    async fn continuously_writable_truncated_stream_still_honors_deadline() {
        let mut limits = short_limits(OverflowBehavior::TruncateAndDrain);
        limits.execution_timeout = Duration::from_millis(75);
        let error = capture_bounded(
            shell("while :; do printf 1234567890; done"),
            limits,
            &CancellationToken::new(),
        )
        .await
        .expect_err("infinite output reaches execution deadline");
        assert_eq!(error, CaptureError::ExecutionTimedOut);
    }

    #[allow(clippy::expect_used)]
    #[tokio::test]
    async fn retain_tail_preserves_output_emitted_before_timeout() {
        let mut limits = short_limits(OverflowBehavior::RetainTail);
        limits.execution_timeout = Duration::from_millis(75);
        let output = capture_bounded(
            shell("printf 'early-output'; sleep 1"),
            limits,
            &CancellationToken::new(),
        )
        .await
        .expect("timeout returns partial output");
        assert!(output.timed_out);
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(
            text.contains("early-output"),
            "expected pre-timeout output retained: {text:?}"
        );
    }

    #[allow(clippy::expect_used)]
    #[tokio::test]
    async fn retain_tail_retains_final_bytes_past_bound() {
        let output = capture_bounded(
            shell("printf 'START'; head -c 200 /dev/zero; printf 'END'"),
            short_limits(OverflowBehavior::RetainTail),
            &CancellationToken::new(),
        )
        .await
        .expect("over-bound output drained");
        assert!(output.status.success());
        assert!(!output.timed_out);
        assert!(output.stdout_truncated);
        assert!(
            output.stdout.ends_with(b"END"),
            "tail must end with END: {:?}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(
            !output.stdout.starts_with(b"START"),
            "prefix must not survive in tail"
        );
    }

    #[tokio::test]
    async fn timeout_kills_and_reaps_the_direct_child() {
        let temp = TempDir::new().expect("tempdir");
        let marker = temp.path().join("pid");
        let mut command = shell("printf '%s' \"$$\" > \"$PID_MARKER\"; while :; do sleep 1; done");
        command.env("PID_MARKER", &marker);
        let mut limits = short_limits(OverflowBehavior::Error);
        limits.execution_timeout = Duration::from_millis(75);
        let error = capture_bounded(command, limits, &CancellationToken::new())
            .await
            .expect_err("hung child times out");
        assert_eq!(error, CaptureError::ExecutionTimedOut);
        let pid = std::fs::read_to_string(marker)
            .expect("pid marker")
            .parse::<i32>()
            .expect("numeric pid");
        assert_eq!(
            nix::sys::wait::waitpid(
                nix::unistd::Pid::from_raw(pid),
                Some(nix::sys::wait::WaitPidFlag::WNOHANG),
            ),
            Err(nix::errno::Errno::ECHILD)
        );
    }

    #[tokio::test]
    async fn cancellation_kills_and_reaps_the_direct_child() {
        let temp = TempDir::new().expect("tempdir");
        let marker = temp.path().join("pid");
        let mut command = shell("printf '%s' \"$$\" > \"$PID_MARKER\"; while :; do sleep 1; done");
        command.env("PID_MARKER", &marker);
        let cancel = CancellationToken::new();
        let run = capture_bounded(command, short_limits(OverflowBehavior::Error), &cancel);
        tokio::pin!(run);
        let error = loop {
            tokio::select! {
                result = &mut run => break result.expect_err("cancelled child"),
                _ = tokio::time::sleep(Duration::from_millis(5)) => {
                    if marker.exists() {
                        cancel.cancel();
                    }
                }
            }
        };
        assert_eq!(error, CaptureError::Cancelled);
        let pid = std::fs::read_to_string(marker)
            .expect("pid marker")
            .parse::<i32>()
            .expect("numeric pid");
        assert_eq!(
            nix::sys::wait::waitpid(
                nix::unistd::Pid::from_raw(pid),
                Some(nix::sys::wait::WaitPidFlag::WNOHANG),
            ),
            Err(nix::errno::Errno::ECHILD)
        );
    }

    #[tokio::test]
    async fn parent_exit_cannot_leave_a_quiet_descendant_holding_pipes() {
        let temp = TempDir::new().expect("tempdir");
        let marker = temp.path().join("descendant-finished");
        let mut command = shell("(sleep 2; printf done > \"$MARKER\") & exit 0");
        command.env("MARKER", &marker);
        let started = Instant::now();
        let output = capture_bounded(
            command,
            short_limits(OverflowBehavior::Error),
            &CancellationToken::new(),
        )
        .await
        .expect("quiet pipe holder terminated");
        assert!(output.status.success());
        assert!(started.elapsed() < Duration::from_secs(1));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!marker.exists());
    }

    #[tokio::test]
    async fn bounded_lines_accepts_exact_limit_and_crlf() {
        let mut lines = BoundedLines::new(Cursor::new(b"12345678\r\nnext".to_vec()), 8);
        assert_eq!(
            lines.next_line().await.unwrap().as_deref(),
            Some("12345678")
        );
        assert_eq!(lines.next_line().await.unwrap().as_deref(), Some("next"));
        assert_eq!(lines.next_line().await.unwrap(), None);
    }

    #[tokio::test]
    async fn bounded_lines_rejects_before_extending_past_limit() {
        let mut lines = BoundedLines::new(Cursor::new(b"123456789\n".to_vec()), 8);
        assert_eq!(
            lines.next_line().await,
            Err(BoundedLineError::Exceeded { limit: 8 })
        );
        assert!(lines.current.len() <= 8);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn no_escape_containment_denies_setsid_and_setpgid() {
        let output = capture_bounded(
            shell(
                "setsid sh -c 'exit 97' >/dev/null 2>&1 && exit 91; \
                 python3 -c 'import os; os.setpgid(0,0)' >/dev/null 2>&1 && exit 92; exit 0",
            ),
            short_limits(OverflowBehavior::Error),
            &CancellationToken::new(),
        )
        .await
        .expect("escape attempts remain contained");
        assert!(output.status.success());
    }

    #[test]
    fn configured_limits_pin_the_route_contracts() {
        assert_eq!(CaptureLimits::session_tool().max_stdout_bytes, 1024 * 1024);
        assert_eq!(
            CaptureLimits::session_tool().execution_timeout,
            Duration::from_secs(120)
        );
        assert_eq!(CaptureLimits::local_tool().max_stdout_bytes, 50 * 1024);
        assert_eq!(
            CaptureLimits::memory_cli().max_stdout_bytes,
            4 * 1024 * 1024
        );
        assert_eq!(CaptureLimits::memory_cli().max_stderr_bytes, 256 * 1024);
        assert_eq!(
            CaptureLimits::memory_cli().execution_timeout,
            Duration::from_secs(300)
        );
        assert_eq!(CaptureLimits::catalog().max_stdout_bytes, 1024 * 1024);
        assert_eq!(CaptureLimits::catalog().max_stderr_bytes, 64 * 1024);
        assert_eq!(
            CaptureLimits::catalog().execution_timeout,
            Duration::from_secs(10)
        );
        assert_eq!(PROVIDER_MAX_LINE_BYTES, 2 * 1024 * 1024);
        assert_eq!(PROVIDER_MAX_STDERR_BYTES, 256 * 1024);
        assert_eq!(AGY_MAX_TURN_BYTES, 4 * 1024 * 1024);
    }

    #[test]
    fn bounded_concat_accepts_n_and_marks_n_plus_one_inside_the_cap() {
        assert_eq!(
            bounded_lossy_concat(&[b"12345678"], 8, false, "~"),
            "12345678"
        );
        assert_eq!(
            bounded_lossy_concat(&[b"123456789"], 8, false, "~"),
            "1234567~"
        );
        assert_eq!(
            bounded_lossy_concat(&[b"1234", b"56789"], 8, false, "~"),
            "1234567~"
        );
        assert_eq!(
            bounded_lossy_concat(&[b"12345678"], 8, true, "~"),
            "1234567~"
        );
    }

    #[test]
    fn bounded_concat_never_splits_utf8_or_expands_invalid_input_past_cap() {
        assert_eq!(
            bounded_lossy_concat(&["ééé".as_bytes()], 5, false, "~"),
            "éé~"
        );
        let output = bounded_lossy_concat(&[&[0xff; 8]], 8, false, "~");
        assert!(output.len() <= 8);
        assert!(output.ends_with('~'));
    }
}
