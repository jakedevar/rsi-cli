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

mod owned_group;
pub use owned_group::{OwnedGroupError, OwnedGroupLeader, OwnedGroupLimits, capture_owned_group};

pub const PROCESS_READ_CHUNK_BYTES: usize = 8 * 1024;
pub const PROCESS_POST_EXIT_DRAIN_TIMEOUT: Duration = Duration::from_millis(250);
pub const PROCESS_CLEANUP_TIMEOUT: Duration = Duration::from_secs(1);

pub const SESSION_TOOL_MAX_STREAM_BYTES: usize = 1024 * 1024;
pub const SESSION_TOOL_TIMEOUT: Duration = Duration::from_secs(120);
pub const LOCAL_TOOL_MAX_STREAM_BYTES: usize = 50 * 1024;
pub const LOCAL_TOOL_TIMEOUT: Duration = Duration::from_secs(120);
pub const MEMORY_CLI_MAX_STDOUT_BYTES: usize = 4 * 1024 * 1024;
pub const MEMORY_CLI_MAX_STDERR_BYTES: usize = 256 * 1024;
pub const MEMORY_CLI_TIMEOUT: Duration = Duration::from_secs(300);
pub const CATALOG_MAX_STDOUT_BYTES: usize = 1024 * 1024;
pub const CATALOG_MAX_STDERR_BYTES: usize = 64 * 1024;
pub const CATALOG_TIMEOUT: Duration = Duration::from_secs(10);
pub const PROVIDER_MAX_LINE_BYTES: usize = 2 * 1024 * 1024;
pub const PROVIDER_MAX_STDERR_BYTES: usize = 256 * 1024;
pub const AGY_MAX_TURN_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverflowBehavior {
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
pub enum ProcessContainment {
    /// Put the child in a new process group. Descendants that deliberately call
    /// `setsid`/`setpgid` may escape, so callers must still use bounded drains.
    Group,
    /// On Linux, additionally deny `setsid` and `setpgid` in the child and all
    /// descendants with an inherited seccomp filter.
    GroupNoEscape,
}

#[derive(Debug, Clone, Copy)]
pub struct CaptureLimits {
    pub max_stdout_bytes: usize,
    pub max_stderr_bytes: usize,
    pub execution_timeout: Duration,
    pub post_exit_drain_timeout: Duration,
    pub cleanup_timeout: Duration,
    pub overflow: OverflowBehavior,
    pub containment: ProcessContainment,
}

impl CaptureLimits {
    pub const fn new(
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

    pub const fn session_tool() -> Self {
        Self::new(
            SESSION_TOOL_MAX_STREAM_BYTES,
            SESSION_TOOL_MAX_STREAM_BYTES,
            SESSION_TOOL_TIMEOUT,
            OverflowBehavior::TruncateAndDrain,
            ProcessContainment::GroupNoEscape,
        )
    }

    pub const fn local_tool() -> Self {
        Self::new(
            LOCAL_TOOL_MAX_STREAM_BYTES,
            LOCAL_TOOL_MAX_STREAM_BYTES,
            LOCAL_TOOL_TIMEOUT,
            OverflowBehavior::TruncateAndDrain,
            ProcessContainment::GroupNoEscape,
        )
    }

    pub const fn memory_cli() -> Self {
        Self::new(
            MEMORY_CLI_MAX_STDOUT_BYTES,
            MEMORY_CLI_MAX_STDERR_BYTES,
            MEMORY_CLI_TIMEOUT,
            OverflowBehavior::Error,
            ProcessContainment::Group,
        )
    }

    pub const fn catalog() -> Self {
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
pub struct CapturedOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    /// True when the execution deadline expired and the process was killed.
    /// Only set for [`OverflowBehavior::RetainTail`]; other overflow modes
    /// return `Err(CaptureError::ExecutionTimedOut)` on timeout.
    pub timed_out: bool,
}

/// Concatenate byte segments into one lossy UTF-8 result without ever retaining
/// more than `max_bytes` of source data. The optional marker is included inside
/// the same final UTF-8 byte bound whenever this function or an upstream stream
/// reader truncated data.
pub fn bounded_lossy_concat(
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
pub enum CaptureError {
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
pub async fn capture_bounded(
    command: Command,
    limits: CaptureLimits,
    cancel: &CancellationToken,
) -> std::result::Result<CapturedOutput, CaptureError> {
    capture_bounded_with_stdin(command, limits, cancel, None).await
}

/// [`capture_bounded`] that optionally feeds `stdin` to the child.
///
/// With `Some(bytes)` the child's stdin is a pipe: the bytes are written and
/// the pipe is then closed, so a reader sees EOF after the payload. With
/// `None` stdin is `/dev/null`, exactly as [`capture_bounded`]. Catalog probes
/// that speak a request/response protocol on stdin (the Claude CLI's
/// stream-json `initialize` control request) use this form.
pub async fn capture_bounded_with_stdin(
    command: Command,
    limits: CaptureLimits,
    cancel: &CancellationToken,
    stdin: Option<Vec<u8>>,
) -> std::result::Result<CapturedOutput, CaptureError> {
    capture_bounded_with_spawn_and_stdin(command, limits, cancel, stdin, |mut command| {
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
pub async fn capture_bounded_with_spawn<F>(
    command: Command,
    limits: CaptureLimits,
    cancel: &CancellationToken,
    spawn: F,
) -> std::result::Result<CapturedOutput, CaptureError>
where
    F: FnOnce(Command) -> std::result::Result<Child, CaptureError>,
{
    capture_bounded_with_spawn_and_stdin(command, limits, cancel, None, spawn).await
}

/// Shared body of every bounded capture: optional stdin payload plus the
/// caller-owned spawn seam.
async fn capture_bounded_with_spawn_and_stdin<F>(
    mut command: Command,
    limits: CaptureLimits,
    cancel: &CancellationToken,
    stdin: Option<Vec<u8>>,
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
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
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
    if let Some(payload) = stdin {
        let Some(mut child_stdin) = child.stdin.take() else {
            terminate_process_group(pgid);
            let _ = tokio::time::timeout(limits.cleanup_timeout, child.wait()).await;
            return Err(CaptureError::MissingPipe("stdin"));
        };
        // Written off the supervisor so a child that never reads cannot stall
        // stdout/stderr draining. The writer ends when the payload is flushed
        // or when the supervisor kills the group and the pipe closes; dropping
        // the handle delivers EOF.
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            if child_stdin.write_all(&payload).await.is_ok() {
                let _ = child_stdin.shutdown().await;
            }
        });
    }
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
    child: Child,
    stdout: ChildStdout,
    stderr: ChildStderr,
    limits: CaptureLimits,
    external_cancel: CancellationToken,
    abandoned: CancellationToken,
) -> std::result::Result<CapturedOutput, CaptureError> {
    // The leader stays unreaped (a zombie) until every group signal is sent, so
    // its group id cannot be reused while the supervisor still signals it
    // (#1259, #1251). Only `leader.reap()` ends that, on the way out.
    let mut leader =
        OwnedGroupLeader::new(child).map_err(|error| CaptureError::Spawn(error.to_string()))?;
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
    // Whether the leader has exited (observed without reaping where possible).
    let mut exit: Option<std::result::Result<(), std::io::Error>> = None;
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
        if let Some(Err(error)) = exit.as_ref()
            && failure.is_none()
        {
            failure = Some(CaptureError::Wait(error.to_string()));
        }

        if exit.is_some() && stdout_result.is_some() && stderr_result.is_some() {
            // Every signal is sent; only now may the leader be reaped.
            let reaped = match exit.take().expect("exit checked above") {
                Ok(()) => tokio::time::timeout(limits.cleanup_timeout, leader.reap())
                    .await
                    .unwrap_or_else(|_| Err(std::io::Error::other("leader reap timed out"))),
                Err(error) => Err(error),
            };
            // Tail-retaining capture returns partial output on timeout so the
            // caller can report output emitted before the deadline.
            if matches!(failure, Some(CaptureError::ExecutionTimedOut))
                && limits.overflow == OverflowBehavior::RetainTail
                && let Ok(status) = reaped.as_ref()
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
            let status = reaped.map_err(|error| CaptureError::Wait(error.to_string()))?;
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
        if failure.is_none() && exit.is_none() && now >= execution_deadline {
            failure = Some(CaptureError::ExecutionTimedOut);
        }
        if failure.is_some() && !group_terminated {
            leader.terminate_group("bounded capture failed or was cancelled");
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
            leader.terminate_group("bounded capture output stayed open after the leader exited");
            group_terminated = true;
            cleanup_deadline = Some(now + limits.cleanup_timeout);
        }
        if let Some(deadline) = cleanup_deadline
            && now >= deadline
        {
            leader.terminate_group("bounded capture cleanup deadline passed");
            if exit.is_none() {
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
            observed = leader.exited(), if exit.is_none() => {
                exit = Some(observed);
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
pub enum BoundedLineError {
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
pub struct BoundedLines<R> {
    reader: BufReader<R>,
    current: Vec<u8>,
    pending_cr: bool,
    max_line_bytes: usize,
}

impl<R> BoundedLines<R>
where
    R: AsyncRead + Unpin,
{
    pub fn new(reader: R, max_line_bytes: usize) -> Self {
        Self {
            reader: BufReader::with_capacity(PROCESS_READ_CHUNK_BYTES, reader),
            current: Vec::with_capacity(max_line_bytes.min(PROCESS_READ_CHUNK_BYTES)),
            pending_cr: false,
            max_line_bytes,
        }
    }

    pub async fn next_line(&mut self) -> std::result::Result<Option<String>, BoundedLineError> {
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

    /// Drop the remainder of one oversized frame so the next call can read the
    /// following event. Keep draining without retaining any more provider data.
    pub async fn drain_oversized_line(&mut self) -> std::result::Result<(), BoundedLineError> {
        self.current.clear();
        self.pending_cr = false;
        loop {
            let available = self
                .reader
                .fill_buf()
                .await
                .map_err(|error| BoundedLineError::Io(error.to_string()))?;
            if available.is_empty() {
                break;
            }
            let newline = available.iter().position(|byte| *byte == b'\n');
            let consumed = newline.map_or(available.len(), |position| position + 1);
            self.reader.consume(consumed);
            if newline.is_some() {
                break;
            }
        }
        Ok(())
    }
}

pub fn configure_tokio_process_group(
    command: &mut Command,
    containment: ProcessContainment,
) -> DaemonResult<()> {
    configure_std_process_group(command.as_std_mut(), containment)
}

/// Put one command in a new process group and optionally prevent descendants
/// from escaping it. This is shared by async capture and settlement Git.
pub fn configure_std_process_group(
    command: &mut std::process::Command,
    containment: ProcessContainment,
) -> DaemonResult<()> {
    command.process_group(0);
    if containment == ProcessContainment::GroupNoEscape {
        configure_no_escape(command)?;
    }
    Ok(())
}

/// Signal one owned process group. A caller must supply the direct child PID
/// of a command configured with `process_group(0)`, never an untrusted PID.
/// Fall back to that exact child only if group signalling fails. ESRCH means
/// the entire group has already exited.
///
/// Every signal is logged with its target's unit and the caller (#1227):
/// prefer [`signal_process_group_because`] to also name the reason.
#[track_caller]
pub fn signal_process_group(
    pgid: nix::unistd::Pid,
    signal: nix::sys::signal::Signal,
) -> std::result::Result<(), nix::errno::Errno> {
    signal_process_group_because(pgid, signal, UNSPECIFIED_SIGNAL_REASON)
}

/// The reason logged when a caller names none; the caller's source location
/// still identifies the kill path.
pub const UNSPECIFIED_SIGNAL_REASON: &str = "unspecified (see caller)";

/// [`signal_process_group`] with the reason the daemon is sending it, logged
/// together with the target group's systemd unit and command name (#1227).
/// A numeric process group can outlive its leader, so a log line per signal
/// is the only way to attribute a kill that hit an innocent process.
#[track_caller]
pub fn signal_process_group_because(
    pgid: nix::unistd::Pid,
    signal: nix::sys::signal::Signal,
    reason: &str,
) -> std::result::Result<(), nix::errno::Errno> {
    use nix::errno::Errno;
    use nix::sys::signal::{kill, killpg};
    if pgid.as_raw() <= 0 {
        return Err(Errno::EINVAL);
    }
    // Read the target before signalling: a killed process loses its /proc entry.
    let target = SignalTarget::describe(pgid.as_raw());
    let caller = std::panic::Location::caller();
    #[cfg(any(test, feature = "test-seam"))]
    recorded_group_signals::record(pgid.as_raw(), signal, reason, &target);
    let result = match killpg(pgid, signal) {
        Ok(()) | Err(Errno::ESRCH) => Ok(()),
        Err(_) => match kill(pgid, signal) {
            Ok(()) | Err(Errno::ESRCH) => Ok(()),
            Err(error) => Err(error),
        },
    };
    let outcome = match result {
        Ok(()) => "sent".to_string(),
        Err(error) => format!("failed: {error}"),
    };
    log_signal(
        &SignalRecord {
            pid: pgid.as_raw(),
            pgid: Some(pgid.as_raw()),
            signal: signal.as_str(),
            reason,
            target: &target,
        },
        caller,
        &outcome,
    );
    result
}

/// Who a daemon signal is about to hit, read from `/proc` before it is sent
/// (#1227). Fields are `None` when the process is already gone or the
/// platform has no `/proc`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SignalTarget {
    /// The systemd unit (`*.service` or `*.scope`) whose cgroup holds the
    /// process: the attribution an operator needs when an unrelated
    /// `systemd-run` unit dies.
    pub unit: Option<String>,
    /// The process's command name (`/proc/<pid>/comm`).
    pub comm: Option<String>,
}

impl SignalTarget {
    /// Describe process `pid` (for a group: its leader's id).
    #[must_use]
    pub fn describe(pid: i32) -> Self {
        #[cfg(target_os = "linux")]
        {
            if pid <= 0 {
                return Self::default();
            }
            let read = |name: &str| std::fs::read_to_string(format!("/proc/{pid}/{name}")).ok();
            Self {
                unit: read("cgroup").and_then(|cgroup| systemd_unit_from_cgroup(&cgroup)),
                comm: read("comm")
                    .map(|comm| comm.trim_end().to_string())
                    .filter(|comm| !comm.is_empty()),
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = pid;
            Self::default()
        }
    }
}

/// The innermost systemd unit named in a `/proc/<pid>/cgroup` listing: the
/// last `*.service` or `*.scope` path component of the unified (`0::`) line,
/// or of any line when there is no unified hierarchy.
#[must_use]
pub fn systemd_unit_from_cgroup(cgroup: &str) -> Option<String> {
    let unit_of = |path: &str| {
        path.rsplit('/')
            .find(|part| part.ends_with(".service") || part.ends_with(".scope"))
            .map(str::to_string)
    };
    let paths = || cgroup.lines().filter_map(|line| line.splitn(3, ':').nth(2));
    cgroup
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .and_then(unit_of)
        .or_else(|| paths().find_map(unit_of))
}

/// One signal the daemon sent to a process it did not reap itself.
#[derive(Debug)]
pub struct SignalRecord<'a> {
    /// The signalled process, or the group leader's id for a group.
    pub pid: i32,
    /// `Some` when the whole process group was signalled.
    pub pgid: Option<i32>,
    pub signal: &'a str,
    pub reason: &'a str,
    pub target: &'a SignalTarget,
}

/// Log one daemon signal on the `rsid::signal` target with the sender's
/// source location (#1227).
pub fn log_signal(record: &SignalRecord<'_>, caller: &std::panic::Location<'_>, outcome: &str) {
    tracing::info!(
        target: "rsid::signal",
        pid = record.pid,
        pgid = record.pgid,
        signal = record.signal,
        unit = record.target.unit.as_deref().unwrap_or("-"),
        comm = record.target.comm.as_deref().unwrap_or("-"),
        reason = record.reason,
        caller = %caller,
        outcome,
        "daemon signal",
    );
}

/// Test seam: every process-group signal the daemon attempts, so a test can
/// prove a disarmed handle sends none without relying on real PID reuse.
#[cfg(any(test, feature = "test-seam"))]
pub mod recorded_group_signals {
    use super::SignalTarget;
    use std::sync::Mutex;

    /// One attempted group signal with what the log line names.
    #[derive(Debug, Clone)]
    pub struct RecordedGroupSignal {
        pub pgid: i32,
        pub signal: nix::sys::signal::Signal,
        pub reason: String,
        pub target: SignalTarget,
    }

    static RECORDED: Mutex<Vec<RecordedGroupSignal>> = Mutex::new(Vec::new());

    pub(super) fn record(
        pgid: i32,
        signal: nix::sys::signal::Signal,
        reason: &str,
        target: &SignalTarget,
    ) {
        RECORDED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(RecordedGroupSignal {
                pgid,
                signal,
                reason: reason.to_string(),
                target: target.clone(),
            });
    }

    pub fn sent_to(pgid: i32) -> usize {
        RECORDED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|recorded| recorded.pgid == pgid)
            .count()
    }

    /// Every signal attempted on `pgid`, oldest first.
    pub fn all_to(pgid: i32) -> Vec<RecordedGroupSignal> {
        RECORDED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|recorded| recorded.pgid == pgid)
            .cloned()
            .collect()
    }

    /// The most recent signal attempted on `pgid`.
    pub fn last_to(pgid: i32) -> Option<RecordedGroupSignal> {
        RECORDED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .rev()
            .find(|recorded| recorded.pgid == pgid)
            .cloned()
    }
}

/// Terminate the complete group, retaining the existing best-effort cleanup
/// policy for bounded capture and settlement callers.
#[track_caller]
pub fn terminate_process_group(pgid: nix::unistd::Pid) {
    let _ = signal_process_group(pgid, nix::sys::signal::Signal::SIGKILL);
}

/// [`terminate_process_group`] naming why, for kill paths that stop work
/// another session or the operator may be waiting on (#1227).
#[track_caller]
pub fn terminate_process_group_because(pgid: nix::unistd::Pid, reason: &str) {
    let _ = signal_process_group_because(pgid, nix::sys::signal::Signal::SIGKILL, reason);
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

    /// #1259: after the leader exits, a pipe held open by a descendant that
    /// left the group makes the supervisor stop the group, first at the
    /// post-exit drain and again at the cleanup deadline. Both signals must be
    /// sent while the leader is still an unreaped zombie (its `/proc` entry is
    /// readable); a signal after the reap would target a reusable group id.
    #[tokio::test]
    #[cfg(target_os = "linux")]
    async fn bounded_capture_signals_the_group_only_while_the_leader_is_unreaped() {
        let dir = TempDir::new().unwrap();
        let holder_pid = dir.path().join("holder.pid");
        let script = format!(
            "setsid sh -c 'echo $$ > {pid}; sleep 3' &\n\
             while [ ! -s {pid} ]; do sleep 0.02; done\n\
             exit 0",
            pid = holder_pid.display()
        );
        let pgid = std::sync::atomic::AtomicI32::new(0);
        let started = std::time::Instant::now();
        let result = capture_bounded_with_spawn(
            shell(&script),
            CaptureLimits {
                execution_timeout: Duration::from_secs(30),
                containment: ProcessContainment::Group,
                ..short_limits(OverflowBehavior::Error)
            },
            &CancellationToken::new(),
            |mut command| {
                let child = command
                    .spawn()
                    .map_err(|error| CaptureError::Spawn(error.to_string()))?;
                pgid.store(
                    i32::try_from(child.id().unwrap()).unwrap(),
                    std::sync::atomic::Ordering::SeqCst,
                );
                Ok(child)
            },
        )
        .await;
        assert!(
            matches!(result, Err(CaptureError::OutputDrainTimedOut)),
            "the escaped holder must bound the capture: {result:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        let pgid = pgid.load(std::sync::atomic::Ordering::SeqCst);
        let signals = recorded_group_signals::all_to(pgid);
        assert!(
            signals.len() >= 2,
            "expected the drain and cleanup signals, saw {signals:?}"
        );
        for signal in &signals {
            assert!(
                signal.target.comm.is_some(),
                "a group signal was sent after the leader was reaped: {signal:?}"
            );
        }
        let holder: i32 = std::fs::read_to_string(&holder_pid)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(recorded_group_signals::sent_to(holder), 0);
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(holder),
            nix::sys::signal::Signal::SIGKILL,
        );
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
    async fn bounded_capture_feeds_stdin_payload_then_eof() {
        // `cat` exits only at EOF, so a clean exit proves the pipe was closed.
        let output = capture_bounded_with_stdin(
            shell("cat"),
            short_limits(OverflowBehavior::Error),
            &CancellationToken::new(),
            Some(b"initialize\n".to_vec()),
        )
        .await
        .expect("payload echoed");
        assert!(output.status.success());
        assert_eq!(output.stdout, b"initialize\n");

        // `None` keeps stdin on /dev/null: `cat` reads EOF immediately.
        let output = capture_bounded_with_stdin(
            shell("cat"),
            short_limits(OverflowBehavior::Error),
            &CancellationToken::new(),
            None,
        )
        .await
        .expect("null stdin");
        assert!(output.status.success());
        assert!(output.stdout.is_empty());
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
        let mut lines = BoundedLines::new(Cursor::new(b"123456789\nnext\n".to_vec()), 8);
        assert_eq!(
            lines.next_line().await,
            Err(BoundedLineError::Exceeded { limit: 8 })
        );
        assert!(lines.current.len() <= 8);
        lines.drain_oversized_line().await.unwrap();
        assert_eq!(lines.next_line().await.unwrap().as_deref(), Some("next"));
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

    #[test]
    fn systemd_unit_is_the_innermost_unit_of_the_unified_cgroup() {
        let unit = |text: &str| systemd_unit_from_cgroup(text);
        assert_eq!(
            unit("0::/user.slice/user-1000.slice/user@1000.service/app.slice/af33-1208-tests.service\n")
                .as_deref(),
            Some("af33-1208-tests.service")
        );
        assert_eq!(
            unit(
                "0::/user.slice/user-1000.slice/user@1000.service/app.slice/rsi-worker-x.scope/sub"
            )
            .as_deref(),
            Some("rsi-worker-x.scope")
        );
        assert_eq!(
            unit("12:pids:/system.slice/sshd.service\n1:name=systemd:/system.slice/sshd.service\n")
                .as_deref(),
            Some("sshd.service")
        );
        assert_eq!(unit("0::/\n"), None);
        assert_eq!(unit(""), None);
    }

    /// #1227: a group signal names its reason and the target's command and
    /// unit, read before the signal lands.
    #[cfg(target_os = "linux")]
    #[test]
    fn group_signal_records_reason_and_target_identity() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .expect("spawn sleep");
        let pgid = i32::try_from(child.id()).expect("pid fits");
        // Wait until the child has exec'd `sleep`, so its comm is final.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while SignalTarget::describe(pgid).comm.as_deref() != Some("sleep")
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        signal_process_group_because(
            nix::unistd::Pid::from_raw(pgid),
            nix::sys::signal::Signal::SIGKILL,
            "test: stop the stand-in",
        )
        .expect("signal sent");
        let status = child.wait().expect("reap");
        assert!(!status.success());
        let recorded = recorded_group_signals::last_to(pgid).expect("recorded");
        assert_eq!(recorded.reason, "test: stop the stand-in");
        assert_eq!(recorded.signal, nix::sys::signal::Signal::SIGKILL);
        assert_eq!(recorded.target.comm.as_deref(), Some("sleep"));
        // A child inherits its parent's cgroup, so it names the same unit.
        let own = i32::try_from(std::process::id()).expect("pid fits");
        assert_eq!(recorded.target.unit, SignalTarget::describe(own).unit);
    }

    #[test]
    fn unnamed_group_signals_record_the_unspecified_reason() {
        // A pid far above pid_max: killpg reports ESRCH, nothing is signalled.
        let pgid = i32::MAX - 7;
        terminate_process_group(nix::unistd::Pid::from_raw(pgid));
        let recorded = recorded_group_signals::last_to(pgid).expect("recorded");
        assert_eq!(recorded.reason, UNSPECIFIED_SIGNAL_REASON);
        assert_eq!(recorded.target, SignalTarget::default());
    }
}
