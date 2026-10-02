//! Scoped, resumable shell execution for Harness sessions.
//!
//! A process is returned before completion after `yield_time_ms`; a later
//! `write_stdin` call can either send input or poll the same scoped process.

use super::{
    HarnessTool, ToolContext,
    process_registry::ProcessWakeController,
    shell::{ShellLaunchIdentity, scoped_shell_command},
};
use crate::session::harness::types::{ToolContentBlock, ToolResult};
use serde_json::{Value, json};
use std::process::Stdio;
use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, ChildStdin},
    sync::{Mutex, mpsc},
};
use tokio_util::sync::CancellationToken;

const DEFAULT_EXEC_YIELD_MS: u64 = 10_000;
const MIN_YIELD_MS: u64 = 250;
const MAX_YIELD_MS: u64 = 30_000;
const DEFAULT_WRITE_YIELD_MS: u64 = 1_000;
const MAX_CONCURRENT_PROCESSES: usize = 8;
const POLL_INTERVAL: Duration = Duration::from_millis(20);

fn clamped_yield_ms(value: Option<u64>, default: u64) -> u64 {
    value.unwrap_or(default).clamp(MIN_YIELD_MS, MAX_YIELD_MS)
}

fn output_limit(args: &Value, context: &ToolContext) -> usize {
    let policy_limit = u64::try_from(context.policy.max_output_bytes)
        .unwrap_or(u64::MAX)
        .max(1);
    let requested = args
        .get("max_output_tokens")
        .and_then(Value::as_u64)
        .map(|value| value.clamp(1, policy_limit))
        .unwrap_or(policy_limit);
    usize::try_from(requested)
        .unwrap_or(context.policy.max_output_bytes)
        .max(1)
}

fn typed_json(value: Value, is_error: bool) -> ToolResult {
    ToolResult::from_blocks(
        vec![ToolContentBlock::Text {
            text: value.to_string(),
        }],
        is_error,
    )
}

fn error_json(message: impl Into<String>, code: &str) -> ToolResult {
    typed_json(
        json!({
            "error": message.into(),
            "error_code": code,
        }),
        true,
    )
}

fn result_text(result: &ToolResult) -> String {
    crate::session::harness::types::ChatMessage::tool_result("test-result", result.output.clone())
        .visible_tool_text()
}

#[derive(Debug)]
struct HeadTailBuffer {
    capacity: usize,
    head: Vec<u8>,
    tail: VecDeque<u8>,
    omitted_bytes: usize,
}

impl HeadTailBuffer {
    fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            head: Vec::new(),
            tail: VecDeque::new(),
            omitted_bytes: 0,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        let head_capacity = self.capacity / 2;
        let remaining_head = head_capacity.saturating_sub(self.head.len());
        let (head_chunk, tail_chunk) = chunk
            .split_at_checked(remaining_head)
            .unwrap_or((chunk, &[]));
        self.head.extend_from_slice(head_chunk);

        let tail_capacity = self.capacity.saturating_sub(head_capacity);
        let remaining_tail = tail_capacity.saturating_sub(self.tail.len());
        let excess = tail_chunk.len().saturating_sub(remaining_tail);
        self.omitted_bytes = self.omitted_bytes.saturating_add(excess);
        if excess > self.tail.len() {
            self.tail.clear();
            if excess > self.tail.len() {
                self.tail.extend(&tail_chunk[excess..]);
            }
        } else {
            self.tail.drain(..excess);
            self.tail.extend(tail_chunk);
        }
    }

    fn render(&self) -> String {
        let mut retained = Vec::with_capacity(self.head.len() + self.tail.len());
        retained.extend_from_slice(&self.head);
        retained.extend(self.tail.iter().copied());
        if self.omitted_bytes == 0 {
            return String::from_utf8_lossy(&retained).into_owned();
        }
        let head = String::from_utf8_lossy(&self.head).into_owned();
        let tail = self
            .tail
            .iter()
            .copied()
            .collect::<Vec<u8>>()
            .lossy_string();
        format!(
            "{head}\n... [{} bytes omitted] ...\n{tail}",
            self.omitted_bytes
        )
    }
}

trait LossyString {
    fn lossy_string(self) -> String;
}

impl LossyString for Vec<u8> {
    fn lossy_string(self) -> String {
        String::from_utf8_lossy(&self).into_owned()
    }
}

#[derive(Debug)]
struct ProcessOutput {
    stdout: HeadTailBuffer,
    stderr: HeadTailBuffer,
}

impl ProcessOutput {
    fn render(&self) -> String {
        let stdout = self.stdout.render();
        let stderr = self.stderr.render();
        if stderr.is_empty() {
            stdout
        } else if stdout.is_empty() {
            format!("--- stderr ---\n{stderr}")
        } else {
            format!("{stdout}\n--- stderr ---\n{stderr}")
        }
    }
}

#[derive(Debug)]
struct ProcessState {
    output: Mutex<ProcessOutput>,
    stdin: Mutex<Option<ChildStdin>>,
    exit_code: AtomicU32,
    running: AtomicBool,
    terminated: AtomicBool,
    result_read: AtomicBool,
    pending_notice: AtomicBool,
    completion: Mutex<()>,
    /// The only signal target this process may ever receive. It is `Some`
    /// while the leader is unreaped (alive, or an exited zombie that still
    /// pins the PGID) and is taken *before* the leader is reaped, so a
    /// retained exited-but-unread handle can never signal a recycled PGID.
    group: std::sync::Mutex<Option<i32>>,
}

impl ProcessState {
    fn new(stdin: Option<ChildStdin>, stdout_capacity: usize, stderr_capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            output: Mutex::new(ProcessOutput {
                stdout: HeadTailBuffer::new(stdout_capacity),
                stderr: HeadTailBuffer::new(stderr_capacity),
            }),
            stdin: Mutex::new(stdin),
            exit_code: AtomicU32::new(u32::MAX),
            running: AtomicBool::new(true),
            terminated: AtomicBool::new(false),
            result_read: AtomicBool::new(false),
            pending_notice: AtomicBool::new(false),
            completion: Mutex::new(()),
            group: std::sync::Mutex::new(None),
        })
    }

    fn arm_group(&self, pgid: i32) {
        *self
            .group
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(pgid);
    }

    /// SIGKILL the group while the leader still pins it; a disarmed process
    /// holds no target and is never signalled.
    fn signal_group(&self) {
        let group = self
            .group
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(pgid) = *group else {
            return;
        };
        let _ = crate::process_control::signal_process_group(
            nix::unistd::Pid::from_raw(pgid),
            nix::sys::signal::Signal::SIGKILL,
        );
    }

    /// Drop the signal target without signalling (nothing proves the PGID).
    #[cfg(target_os = "linux")]
    fn disarm_group(&self) {
        self.group
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
    }

    /// Kill the group, then drop the signal target. Must run before the
    /// leader is reaped: the unreaped leader is what proves the PGID is ours.
    fn kill_and_disarm_group(&self) {
        let mut group = self
            .group
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(pgid) = group.take() else {
            return;
        };
        let _ = crate::process_control::signal_process_group(
            nix::unistd::Pid::from_raw(pgid),
            nix::sys::signal::Signal::SIGKILL,
        );
    }

    fn claim_pending_notice(&self) -> bool {
        !self.result_read.load(Ordering::Acquire)
            && self
                .pending_notice
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
    }

    fn mark_read(&self) {
        self.result_read.store(true, Ordering::Release);
    }

    fn retire_pending_notice(&self) -> bool {
        self.pending_notice.swap(false, Ordering::AcqRel)
    }

    async fn mark_suppressed(&self) {
        let completion = self.completion.lock().await;
        self.mark_read();
        drop(completion);
    }

    async fn snapshot(&self) -> ProcessSnapshot {
        let output = self.output.lock().await;
        let exit_code = self.exit_code.load(Ordering::Acquire);
        ProcessSnapshot {
            running: exit_code == u32::MAX,
            terminated: self.terminated.load(Ordering::Acquire),
            output: output.render(),
            stdout_omitted_bytes: output.stdout.omitted_bytes,
            stderr_omitted_bytes: output.stderr.omitted_bytes,
            exit_code: (exit_code != u32::MAX).then_some(exit_code),
        }
    }
}

#[derive(Debug, Clone)]
struct ProcessSnapshot {
    running: bool,
    terminated: bool,
    output: String,
    stdout_omitted_bytes: usize,
    stderr_omitted_bytes: usize,
    exit_code: Option<u32>,
}

#[derive(Debug)]
struct ProcessHandle {
    id: u32,
    terminate: CancellationToken,
    state: Arc<ProcessState>,
    monitor: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl ProcessHandle {
    async fn mark_read_and_remove(self: Arc<Self>) -> Arc<Self> {
        {
            let completion = self.state.completion.lock().await;
            self.state.mark_read();
            drop(completion);
        }
        let monitor = self.monitor.lock().await.take();
        if let Some(monitor) = monitor {
            let _ = monitor.await;
        }
        self
    }

    fn signal_group(&self) {
        self.state.signal_group();
    }
}

#[derive(Debug)]
struct ProcessNotice {
    session_id: u32,
    exit_code: Option<u32>,
    state: Arc<ProcessState>,
}

pub(crate) struct ProcessRegistry {
    capacity: usize,
    next_session_id: AtomicU32,
    processes: Mutex<HashMap<u32, Arc<ProcessHandle>>>,
    notices_tx: mpsc::UnboundedSender<ProcessNotice>,
    notices_rx: Mutex<mpsc::UnboundedReceiver<ProcessNotice>>,
    wake_controller: std::sync::RwLock<Option<Arc<dyn ProcessWakeController>>>,
    turn_active: AtomicBool,
    pending_unread: AtomicUsize,
    wake_armed: AtomicBool,
    closed: AtomicBool,
    wake_lock: Arc<tokio::sync::Mutex<()>>,
}

impl ProcessRegistry {
    pub(super) fn new(capacity: usize) -> Arc<Self> {
        let (notices_tx, notices_rx) = mpsc::unbounded_channel();
        Arc::new(Self {
            capacity,
            next_session_id: AtomicU32::new(1),
            processes: Mutex::new(HashMap::new()),
            notices_tx,
            notices_rx: Mutex::new(notices_rx),
            wake_controller: std::sync::RwLock::new(None),
            turn_active: AtomicBool::new(false),
            pending_unread: AtomicUsize::new(0),
            wake_armed: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            wake_lock: Arc::new(tokio::sync::Mutex::new(())),
        })
    }

    pub(super) fn set_wake_controller(&self, controller: Arc<dyn ProcessWakeController>) {
        *self
            .wake_controller
            .write()
            .expect("Harness process wake controller lock") = Some(controller);
    }

    fn wake_controller(&self) -> Option<Arc<dyn ProcessWakeController>> {
        self.wake_controller
            .read()
            .expect("Harness process wake controller lock")
            .clone()
    }

    pub(super) async fn begin_turn(&self) {
        self.turn_active.store(true, Ordering::Release);
        self.cancel_armed_wake().await;
    }

    pub(super) fn end_turn(self: &Arc<Self>) {
        self.turn_active.store(false, Ordering::Release);
        self.arm_pending_wake();
    }

    fn arm_pending_wake(self: &Arc<Self>) {
        if self.pending_unread.load(Ordering::Acquire) == 0 || self.closed.load(Ordering::Acquire) {
            return;
        }
        let Some(controller) = self.wake_controller() else {
            return;
        };
        if self
            .wake_armed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let wake_lock = self.wake_lock.clone();
        let registry = Arc::clone(self);
        tokio::spawn(async move {
            let _guard = wake_lock.lock().await;
            if !registry.wake_armed.load(Ordering::Acquire) {
                return;
            }
            if let Err(error) = controller.arm().await {
                registry.wake_armed.store(false, Ordering::Release);
                tracing::warn!(%error, "Failed to arm background process wake");
            }
        });
    }

    async fn retire_pending(&self, state: &ProcessState) {
        if state.retire_pending_notice() {
            self.pending_unread.fetch_sub(1, Ordering::AcqRel);
        }
        self.withdraw_idle_wake().await;
    }

    async fn withdraw_idle_wake(&self) {
        if self.pending_unread.load(Ordering::Acquire) != 0 {
            return;
        }
        self.cancel_armed_wake().await;
    }

    async fn cancel_armed_wake(&self) {
        let Some(controller) = self.wake_controller() else {
            return;
        };
        let guard = self.wake_lock.lock().await;
        if self.wake_armed.swap(false, Ordering::AcqRel) {
            controller.withdraw().await;
        }
        drop(guard);
    }

    fn record_completion(self: &Arc<Self>) {
        if !self.turn_active.load(Ordering::Acquire) {
            self.arm_pending_wake();
        }
    }

    async fn start(
        self: &Arc<Self>,
        command: &str,
        working_dir: &Path,
        max_output_bytes: usize,
        external_cancel: &CancellationToken,
        identity: &ShellLaunchIdentity,
    ) -> Result<u32, String> {
        let mut processes = self.processes.lock().await;
        if processes.len() >= self.capacity {
            return Err(format!(
                "concurrent process limit ({}) reached; read or finish an existing session first",
                self.capacity
            ));
        }
        let session_id = self
            .next_session_id
            .fetch_add(1, Ordering::AcqRel)
            .checked_add(1)
            .ok_or_else(|| "process session ids exhausted".to_string())?;
        let mut scoped = scoped_shell_command(command, working_dir, identity)?;
        scoped
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = scoped.spawn().map_err(|error| error.to_string())?;
        let pid = child
            .id()
            .ok_or_else(|| "spawned process did not expose a pid".to_string())?
            as i32;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "process stdout pipe was unavailable".to_string())?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| "process stderr pipe was unavailable".to_string())?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "process stdin pipe was unavailable".to_string())?;

        let stdout_capacity = max_output_bytes.max(1);
        let stderr_capacity = max_output_bytes.max(1);
        let state = ProcessState::new(Some(stdin), stdout_capacity, stderr_capacity);
        let pgid = nix::unistd::getpgid(Some(nix::unistd::Pid::from_raw(pid)))
            .map(|pgid| pgid.as_raw())
            .unwrap_or(pid);
        state.arm_group(pgid);
        let process_registry = Arc::clone(self);
        let stdout_task = tokio::spawn(read_stream(stdout, Arc::clone(&state), true));
        let stderr_task = tokio::spawn(read_stream(stderr, Arc::clone(&state), false));
        let terminate = CancellationToken::new();
        let external_cancel = external_cancel.clone();
        let notices_tx = self.notices_tx.clone();
        let monitor_state = Arc::clone(&state);
        let monitor_terminate = terminate.clone();
        let monitor = tokio::spawn(async move {
            supervise_process(
                child,
                monitor_state,
                monitor_terminate,
                external_cancel,
                notices_tx,
                process_registry,
                session_id,
                vec![stdout_task, stderr_task],
            )
            .await;
        });
        let handle = Arc::new(ProcessHandle {
            id: session_id,
            terminate,
            state,
            monitor: Mutex::new(Some(monitor)),
        });
        processes.insert(session_id, Arc::clone(&handle));
        Ok(session_id)
    }

    async fn terminate(&self, session_id: u32) -> bool {
        let handle = {
            let processes = self.processes.lock().await;
            match processes.get(&session_id) {
                Some(handle) => Arc::clone(handle),
                None => return false,
            }
        };
        handle.state.mark_suppressed().await;
        self.retire_pending(&handle.state).await;
        handle.signal_group();
        handle.terminate.cancel();
        let monitor = handle.monitor.lock().await.take();
        if let Some(monitor) = monitor {
            let _ = monitor.await;
        }
        true
    }

    async fn snapshot(&self, session_id: u32) -> Option<ProcessSnapshot> {
        let handle = {
            let processes = self.processes.lock().await;
            processes.get(&session_id).map(Arc::clone)?
        };
        Some(handle.state.snapshot().await)
    }

    async fn write(&self, session_id: u32, bytes: &[u8]) -> Result<(), String> {
        let handle = {
            let processes = self.processes.lock().await;
            processes.get(&session_id).map(Arc::clone)
        };
        let Some(handle) = handle else {
            return Err(format!("unknown process session id: {session_id}"));
        };
        let mut stdin = handle.state.stdin.lock().await;
        if let Some(stdin) = stdin.as_mut() {
            stdin
                .write_all(bytes)
                .await
                .map_err(|error| format!("failed to write process stdin: {error}"))
        } else {
            Err(format!("stdin is closed for process session {session_id}"))
        }
    }

    async fn take_read_result(&self, session_id: u32) -> Result<ProcessSnapshot, String> {
        let handle = {
            let mut processes = self.processes.lock().await;
            processes
                .remove(&session_id)
                .ok_or_else(|| format!("unknown process session id: {session_id}"))?
        };
        let handle = handle.mark_read_and_remove().await;
        self.retire_pending(&handle.state).await;
        Ok(handle.state.snapshot().await)
    }

    pub(super) async fn take_notices(&self) -> Vec<String> {
        let mut notices = Vec::new();
        let mut receiver = self.notices_rx.lock().await;
        while let Ok(notice) = receiver.try_recv() {
            let _completion = notice.state.completion.lock().await;
            if notice.state.result_read.load(Ordering::Acquire) {
                continue;
            }
            self.retire_pending(&notice.state).await;
            notices.push(format!(
                "Background command finished (session {}, exit code {}). Poll it with write_stdin before it is cleaned up at session end.",
                notice.session_id,
                notice.exit_code.unwrap_or(127)
            ));
        }
        notices
    }

    pub(super) async fn shutdown_processes(&self) {
        self.closed.store(true, Ordering::Release);
        let handles: Vec<Arc<ProcessHandle>> = {
            let mut processes = self.processes.lock().await;
            let handles = processes.values().cloned().collect();
            processes.clear();
            handles
        };
        for handle in handles {
            handle.state.mark_suppressed().await;
            self.retire_pending(&handle.state).await;
            handle.signal_group();
            handle.terminate.cancel();
            let monitor = handle.monitor.lock().await.take();
            if let Some(monitor) = monitor {
                let _ = monitor.await;
            }
        }
    }
}

async fn read_stream<R: tokio::io::AsyncRead + Unpin>(
    mut stream: R,
    state: Arc<ProcessState>,
    stdout: bool,
) {
    let mut chunk = [0u8; 8192];
    loop {
        let read = stream.read(&mut chunk).await;
        match read {
            Ok(0) | Err(_) => break,
            Ok(size) => {
                let mut output = state.output.lock().await;
                if stdout {
                    output.stdout.push(&chunk[..size]);
                } else {
                    output.stderr.push(&chunk[..size]);
                }
            }
        }
    }
}

/// What the supervisor learned about the leader's exit, and therefore what it
/// may still prove about the group's PGID.
#[derive(Debug)]
enum LeaderObservation {
    /// Linux: the leader exited and is an unreaped zombie (`WNOWAIT`), so it
    /// still pins the PGID. ONLY this outcome may SIGKILL the group.
    #[cfg(target_os = "linux")]
    ExitedPinned,
    /// Linux: the exit could not be observed (ECHILD: already reaped
    /// elsewhere, not our child, ...). Nothing pins the PGID, so the handle is
    /// disarmed and the group is never signalled.
    #[cfg(target_os = "linux")]
    ObservationFailed(nix::errno::Errno),
    /// Other platforms cannot observe an exit without reaping it: the leader
    /// was reaped and the handle disarmed in one critical section, with no
    /// group kill.
    #[cfg(not(target_os = "linux"))]
    Reaped(Option<Result<std::process::ExitStatus, std::io::Error>>),
}

#[cfg(target_os = "linux")]
fn waitid_leader_unreaped(pid: i32) -> Result<(), nix::errno::Errno> {
    use nix::sys::wait::{Id, WaitPidFlag, waitid};
    waitid(
        Id::Pid(nix::unistd::Pid::from_raw(pid)),
        WaitPidFlag::WEXITED | WaitPidFlag::WNOWAIT,
    )
    .map(|_| ())
}

/// Classify one blocking observation; `EINTR` is retried, any other error is a
/// failed observation and never an exit.
#[cfg(target_os = "linux")]
fn observe_leader_exit_blocking(
    pid: i32,
    wait: impl Fn(i32) -> Result<(), nix::errno::Errno>,
) -> LeaderObservation {
    loop {
        match wait(pid) {
            Ok(()) => return LeaderObservation::ExitedPinned,
            Err(nix::errno::Errno::EINTR) => continue,
            Err(errno) => return LeaderObservation::ObservationFailed(errno),
        }
    }
}

/// Block until the leader has exited WITHOUT reaping it (`WNOWAIT`): the
/// zombie leader keeps its PID, and so the group's PGID, reserved while the
/// caller signals the group.
#[cfg(target_os = "linux")]
async fn observe_leader_exit(child: &mut Child, _state: &ProcessState) -> LeaderObservation {
    let Some(pid) = child.id().and_then(|pid| i32::try_from(pid).ok()) else {
        return LeaderObservation::ObservationFailed(nix::errno::Errno::ESRCH);
    };
    tokio::task::spawn_blocking(move || observe_leader_exit_blocking(pid, waitid_leader_unreaped))
        .await
        .unwrap_or(LeaderObservation::ObservationFailed(nix::errno::Errno::EIO))
}

#[cfg(not(target_os = "linux"))]
async fn observe_leader_exit(child: &mut Child, state: &ProcessState) -> LeaderObservation {
    loop {
        {
            // Reap and disarm under the group lock so no signal path can see
            // the reaped leader's PGID as still armed.
            let mut group = state
                .group
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match child.try_wait() {
                Ok(Some(status)) => {
                    group.take();
                    return LeaderObservation::Reaped(Some(Ok(status)));
                }
                Ok(None) => {}
                Err(error) => {
                    group.take();
                    return LeaderObservation::Reaped(Some(Err(error)));
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Turn an observation into the leader's exit status, killing the group only
/// when the leader is proven to still pin it, and always disarming before the
/// reap.
async fn settle_observed_exit(
    observation: LeaderObservation,
    state: &ProcessState,
    child: &mut Child,
) -> Option<Result<std::process::ExitStatus, std::io::Error>> {
    match observation {
        #[cfg(target_os = "linux")]
        LeaderObservation::ExitedPinned => {
            state.kill_and_disarm_group();
            Some(child.wait().await)
        }
        #[cfg(target_os = "linux")]
        LeaderObservation::ObservationFailed(errno) => {
            tracing::warn!(%errno, "Background process exit unobservable; disarming without a group signal");
            state.disarm_group();
            tokio::time::timeout(Duration::from_secs(1), child.wait())
                .await
                .ok()
        }
        #[cfg(not(target_os = "linux"))]
        LeaderObservation::Reaped(status) => status,
    }
}

async fn supervise_process(
    mut child: Child,
    state: Arc<ProcessState>,
    terminate: CancellationToken,
    external_cancel: CancellationToken,
    notices_tx: mpsc::UnboundedSender<ProcessNotice>,
    process_registry: Arc<ProcessRegistry>,
    session_id: u32,
    readers: Vec<tokio::task::JoinHandle<()>>,
) {
    // Every branch kills the group and disarms the handle BEFORE the leader
    // is reaped, so no later archive/delete/interrupt/shutdown can signal a
    // PGID the daemon can no longer prove it owns.
    let status: Option<Result<std::process::ExitStatus, std::io::Error>> = tokio::select! {
        observation = observe_leader_exit(&mut child, &state) => {
            settle_observed_exit(observation, &state, &mut child).await
        }
        _ = terminate.cancelled() => {
            state.kill_and_disarm_group();
            state.terminated.store(true, Ordering::Release);
            tokio::time::timeout(Duration::from_secs(1), child.wait())
                .await
                .ok()
        }
        _ = external_cancel.cancelled() => {
            state.kill_and_disarm_group();
            state.terminated.store(true, Ordering::Release);
            terminate.cancel();
            tokio::time::timeout(Duration::from_secs(1), child.wait())
                .await
                .ok()
        }
    };
    let exit_code = status
        .and_then(Result::ok)
        .and_then(|status| status.code())
        .map(|code| u32::try_from(code).unwrap_or(1))
        .unwrap_or(if state.terminated.load(Ordering::Acquire) {
            137
        } else {
            127
        });
    state.exit_code.store(exit_code, Ordering::Release);
    state.running.store(false, Ordering::Release);

    for reader in readers {
        if tokio::time::timeout(Duration::from_millis(250), reader)
            .await
            .is_err()
        {
            // The process group was already killed; abort only a reader that
            // failed to observe EOF within the bounded drain window.
            // The reader variable was moved into timeout above and cannot be
            // aborted here, so rely on pipe closure and task completion.
        }
    }
    let completion = state.completion.lock().await;
    if state.claim_pending_notice() {
        process_registry
            .pending_unread
            .fetch_add(1, Ordering::AcqRel);
        let _ = notices_tx.send(ProcessNotice {
            session_id,
            exit_code: Some(exit_code),
            state: Arc::clone(&state),
        });
        drop(completion);
        process_registry.record_completion();
        return;
    }
    drop(completion);
}

fn snapshot_value(session_id: u32, snapshot: ProcessSnapshot, wall_time_ms: u64) -> Value {
    json!({
        "session_id": snapshot.running.then_some(session_id),
        "exit_code": snapshot.exit_code,
        "output": snapshot.output,
        "stdout_omitted_bytes": snapshot.stdout_omitted_bytes,
        "stderr_omitted_bytes": snapshot.stderr_omitted_bytes,
        "terminated": snapshot.terminated,
        "wall_time_seconds": wall_time_ms as f64 / 1000.0,
    })
}

async fn wait_snapshot(
    registry: &Arc<ProcessRegistry>,
    session_id: u32,
    yield_ms: u64,
    cancel: &CancellationToken,
) -> Result<ProcessSnapshot, String> {
    let started = tokio::time::Instant::now();
    loop {
        if cancel.is_cancelled() {
            registry.terminate(session_id).await;
            return Err("cancelled".into());
        }
        let Some(snapshot) = registry.snapshot(session_id).await else {
            return Err(format!("unknown process session id: {session_id}"));
        };
        if !snapshot.running || started.elapsed() >= Duration::from_millis(yield_ms) {
            return Ok(snapshot);
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

fn requested_workdir(args: &Value, base: &Path) -> PathBuf {
    args.get("workdir")
        .and_then(Value::as_str)
        .map(|workdir| {
            let path = PathBuf::from(workdir);
            if path.is_absolute() {
                path
            } else {
                base.join(path)
            }
        })
        .unwrap_or_else(|| base.to_path_buf())
}

pub(super) struct ExecCommandTool {
    registry: Arc<ProcessRegistry>,
    identity: ShellLaunchIdentity,
}

impl ExecCommandTool {
    pub(super) fn new(
        registry: Arc<ProcessRegistry>,
        session_id: Option<uuid::Uuid>,
        invocation_id: Option<uuid::Uuid>,
        execution_scratch: Option<crate::sandbox::execution_scratch::SandboxExecutionScratch>,
    ) -> Self {
        Self {
            registry,
            identity: ShellLaunchIdentity {
                session_id,
                invocation_id,
                execution_scratch,
                egress: rsi_common::egress_policy::EgressMode::default(),
            },
        }
    }
}

#[async_trait::async_trait]
impl HarnessTool for ExecCommandTool {
    fn name(&self) -> &str {
        "exec_command"
    }

    fn description(&self) -> &str {
        "Run a shell command and return output, or a session id for a still-running command. \
         Uses the same scrubbed, systemd-scoped environment as shell. This implementation uses \
         plain pipes even when tty is requested."
    }

    fn parameters_json(&self) -> &str {
        r#"{"type":"object","additionalProperties":false,"required":["cmd"],"properties":{"cmd":{"type":"string","description":"Shell command to execute"},"workdir":{"type":"string","description":"Working directory; defaults to the turn working directory"},"tty":{"type":"boolean","description":"Accepted for compatibility; scoped execution uses plain pipes"},"yield_time_ms":{"type":"integer","description":"Wait before yielding; defaults to 10000 and is clamped to 250-30000"},"max_output_tokens":{"type":"integer","description":"Output byte budget; capped by session policy"}}}"#
    }

    fn execution_mode(&self) -> super::ToolExecutionMode {
        super::ToolExecutionMode::ParallelSafe
    }

    async fn execute(&self, args: Value, working_dir: &Path) -> ToolResult {
        let context = ToolContext {
            session_id: None,
            working_dir: working_dir.to_path_buf(),
            cancel: CancellationToken::new(),
            event_sink: None,
            policy: super::ToolPolicy::default(),
        };
        self.execute_with_context(args, &context).await
    }

    async fn execute_with_context(&self, args: Value, context: &ToolContext) -> ToolResult {
        let Some(command) = args.get("cmd").and_then(Value::as_str) else {
            return error_json("missing required argument: cmd", "missing_argument");
        };
        if command.trim().is_empty() {
            return error_json("cmd must not be empty", "invalid_argument");
        }
        let yield_ms = clamped_yield_ms(
            args.get("yield_time_ms").and_then(Value::as_u64),
            DEFAULT_EXEC_YIELD_MS,
        );
        let max_output_bytes = output_limit(&args, context);
        let working_dir = requested_workdir(&args, &context.working_dir);
        let identity = ShellLaunchIdentity {
            egress: context.policy.egress.mode,
            ..self.identity.clone()
        };
        let session_id = match self
            .registry
            .start(
                command,
                &working_dir,
                max_output_bytes,
                &context.cancel,
                &identity,
            )
            .await
        {
            Ok(session_id) => session_id,
            Err(error) => return error_json(error, "process_limit"),
        };
        let snapshot =
            match wait_snapshot(&self.registry, session_id, yield_ms, &context.cancel).await {
                Ok(snapshot) => snapshot,
                Err(error) if error == "cancelled" => {
                    return error_json(
                        json!({
                            "message": "process cancelled",
                            "session_id": session_id,
                        })
                        .to_string(),
                        "cancelled",
                    );
                }
                Err(error) => return error_json(error, "process_error"),
            };
        if snapshot.running {
            return typed_json(snapshot_value(session_id, snapshot, yield_ms), false);
        }
        let Ok(final_snapshot) = self.registry.take_read_result(session_id).await else {
            return error_json(
                format!("process disappeared before completion: {session_id}"),
                "process_error",
            );
        };
        let exit_code = final_snapshot.exit_code.unwrap_or(127);
        typed_json(
            snapshot_value(session_id, final_snapshot, yield_ms),
            exit_code != 0,
        )
    }
}

pub(super) struct WriteStdinTool {
    registry: Arc<ProcessRegistry>,
}

impl WriteStdinTool {
    pub(super) fn new(registry: Arc<ProcessRegistry>) -> Self {
        Self { registry }
    }
}

#[async_trait::async_trait]
impl HarnessTool for WriteStdinTool {
    fn name(&self) -> &str {
        "write_stdin"
    }

    fn description(&self) -> &str {
        "Write bytes to an existing exec_command session and return recent output. \
         Empty chars polls the process."
    }

    fn parameters_json(&self) -> &str {
        r#"{"type":"object","additionalProperties":false,"required":["session_id"],"properties":{"session_id":{"type":"integer","description":"Identifier returned by exec_command"},"chars":{"type":"string","description":"Bytes to write; empty polls without writing"},"yield_time_ms":{"type":"integer","description":"Wait before yielding; defaults to 1000 and is clamped to 250-30000"},"max_output_tokens":{"type":"integer","description":"Output byte budget accepted for compatibility"}}}"#
    }

    fn execution_mode(&self) -> super::ToolExecutionMode {
        super::ToolExecutionMode::ParallelSafe
    }

    async fn execute(&self, args: Value, working_dir: &Path) -> ToolResult {
        let _ = working_dir;
        self.execute_with_context(
            args,
            &ToolContext {
                session_id: None,
                working_dir: PathBuf::new(),
                cancel: CancellationToken::new(),
                event_sink: None,
                policy: super::ToolPolicy::default(),
            },
        )
        .await
    }

    async fn execute_with_context(&self, args: Value, context: &ToolContext) -> ToolResult {
        let Some(session_id) = args.get("session_id").and_then(Value::as_u64) else {
            return error_json("missing required argument: session_id", "missing_argument");
        };
        let Ok(session_id) = u32::try_from(session_id) else {
            return error_json("session_id is out of range", "invalid_argument");
        };
        let input = args.get("chars").and_then(Value::as_str).unwrap_or("");
        if !input.is_empty()
            && let Err(error) = self.registry.write(session_id, input.as_bytes()).await
        {
            return error_json(error, "stdin_error");
        }
        let yield_ms = clamped_yield_ms(
            args.get("yield_time_ms").and_then(Value::as_u64),
            DEFAULT_WRITE_YIELD_MS,
        );
        let snapshot =
            match wait_snapshot(&self.registry, session_id, yield_ms, &context.cancel).await {
                Ok(snapshot) => snapshot,
                Err(error) if error == "cancelled" => {
                    self.registry.terminate(session_id).await;
                    return error_json(
                        json!({
                            "message": "process cancelled",
                            "session_id": session_id,
                        })
                        .to_string(),
                        "cancelled",
                    );
                }
                Err(error) => return error_json(error, "unknown_session"),
            };
        if snapshot.running {
            return typed_json(snapshot_value(session_id, snapshot, yield_ms), false);
        }
        let Ok(final_snapshot) = self.registry.take_read_result(session_id).await else {
            return error_json(
                format!("process disappeared before completion: {session_id}"),
                "process_error",
            );
        };
        let exit_code = final_snapshot.exit_code.unwrap_or(127);
        typed_json(
            snapshot_value(session_id, final_snapshot, yield_ms),
            exit_code != 0,
        )
    }
}

pub(crate) fn default_process_registry() -> Arc<ProcessRegistry> {
    ProcessRegistry::new(MAX_CONCURRENT_PROCESSES)
}

/// Production-path drivers for lifecycle-level tests outside this module: they
/// go through the real `exec_command`/`write_stdin` tools and the registry's
/// real turn hooks, never a fake controller.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    fn context() -> ToolContext {
        ToolContext {
            session_id: None,
            working_dir: PathBuf::from("/tmp"),
            cancel: CancellationToken::new(),
            event_sink: None,
            policy: super::super::ToolPolicy {
                max_output_bytes: 4096,
                ..super::super::ToolPolicy::default()
            },
        }
    }

    pub(crate) async fn begin_turn(registry: &Arc<ProcessRegistry>) {
        registry.begin_turn().await;
    }

    pub(crate) fn end_turn(registry: &Arc<ProcessRegistry>) {
        registry.end_turn();
    }

    /// Start `cmd` with a short yield and return its still-running process id.
    pub(crate) async fn start_running(registry: &Arc<ProcessRegistry>, cmd: &str) -> u64 {
        let result = ExecCommandTool::new(Arc::clone(registry), None, None, None)
            .execute_with_context(json!({"cmd": cmd, "yield_time_ms": 250}), &context())
            .await;
        serde_json::from_str::<Value>(&result_text(&result)).expect("typed JSON result")
            ["session_id"]
            .as_u64()
            .expect("command still running after the yield")
    }

    /// Start a command that prints its leader pid (== its PGID, as the shell
    /// is its own group leader) first, then runs `rest`.
    pub(crate) async fn start_running_with_pgid(
        registry: &Arc<ProcessRegistry>,
        rest: &str,
    ) -> (u64, i32) {
        let result = ExecCommandTool::new(Arc::clone(registry), None, None, None)
            .execute_with_context(
                json!({"cmd": format!("echo $$; {rest}"), "yield_time_ms": 250}),
                &context(),
            )
            .await;
        let value = serde_json::from_str::<Value>(&result_text(&result)).expect("typed JSON");
        let process_id = value["session_id"].as_u64().expect("still running");
        let pgid = value["output"]
            .as_str()
            .and_then(|output| output.split_whitespace().next())
            .and_then(|pid| pid.parse::<i32>().ok())
            .expect("leader pid");
        (process_id, pgid)
    }

    /// Poll one process through `write_stdin`, which reads and consumes its result.
    pub(crate) async fn read_result(registry: &Arc<ProcessRegistry>, process_id: u64) -> bool {
        WriteStdinTool::new(Arc::clone(registry))
            .execute_with_context(
                json!({"session_id": process_id, "yield_time_ms": 1500}),
                &context(),
            )
            .await
            .success
    }

    pub(crate) async fn take_notices(registry: &Arc<ProcessRegistry>) -> Vec<String> {
        registry.take_notices().await
    }

    pub(crate) async fn has_process(registry: &Arc<ProcessRegistry>, process_id: u64) -> bool {
        registry
            .snapshot(u32::try_from(process_id).expect("process id"))
            .await
            .is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::harness::tools::ToolPolicy;
    use nix::sys::signal::Signal;

    #[derive(Default)]
    struct CountingWakeController {
        arms: AtomicUsize,
        withdrawals: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl ProcessWakeController for CountingWakeController {
        async fn arm(&self) -> Result<(), String> {
            self.arms.fetch_add(1, Ordering::AcqRel);
            Ok(())
        }

        async fn withdraw(&self) {
            self.withdrawals.fetch_add(1, Ordering::AcqRel);
        }
    }

    #[derive(Default)]
    struct GatedWakeController {
        entered: AtomicBool,
        arms: AtomicUsize,
        withdrawals: AtomicUsize,
        release: Mutex<Option<mpsc::Receiver<()>>>,
    }

    impl GatedWakeController {
        fn new(release: mpsc::Receiver<()>) -> Self {
            Self {
                release: Mutex::new(Some(release)),
                ..Self::default()
            }
        }
    }

    #[async_trait::async_trait]
    impl ProcessWakeController for GatedWakeController {
        async fn arm(&self) -> Result<(), String> {
            self.entered.store(true, Ordering::Release);
            let mut release = self.release.lock().await;
            let Some(mut release) = release.take() else {
                return Err("wake test gate already consumed".to_string());
            };
            if release.recv().await.is_none() {
                return Err("wake test gate closed".to_string());
            }
            self.arms.fetch_add(1, Ordering::AcqRel);
            Ok(())
        }

        async fn withdraw(&self) {
            self.withdrawals.fetch_add(1, Ordering::AcqRel);
        }
    }

    fn complete_process(registry: &Arc<ProcessRegistry>, state: &Arc<ProcessState>) {
        assert!(state.claim_pending_notice());
        registry.pending_unread.fetch_add(1, Ordering::AcqRel);
        registry.record_completion();
    }

    fn send_process_notice(registry: &ProcessRegistry, state: Arc<ProcessState>) {
        registry
            .notices_tx
            .send(ProcessNotice {
                session_id: 1,
                exit_code: Some(0),
                state,
            })
            .expect("process notice channel");
    }

    fn context(cancel: CancellationToken, max_output_bytes: usize) -> ToolContext {
        ToolContext {
            session_id: None,
            working_dir: PathBuf::from("/tmp"),
            cancel,
            event_sink: None,
            policy: ToolPolicy {
                max_output_bytes,
                ..ToolPolicy::default()
            },
        }
    }

    fn json_value(result: &ToolResult) -> Value {
        serde_json::from_str(&result_text(result)).expect("typed JSON result")
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn head_tail_buffer_preserves_head_and_tail() {
        let mut buffer = HeadTailBuffer::new(10);
        let bytes: Vec<u8> = "0123456789".repeat(3).into_bytes();
        buffer.push(&bytes);
        let rendered = buffer.render();
        assert!(rendered.starts_with("01234\n"));
        assert!(rendered.contains("[20 bytes omitted]"));
        assert!(rendered.ends_with("56789"));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[tokio::test]
    async fn exec_yields_then_write_stdin_reads_input_and_exit() {
        let registry = default_process_registry();
        let exec = ExecCommandTool::new(Arc::clone(&registry), None, None, None);
        let write = WriteStdinTool::new(Arc::clone(&registry));
        let command = "printf 'ready\\n'; read line; printf 'got:%s\\n' \"$line\"; exit 7";
        let exec_result = exec
            .execute_with_context(
                json!({"cmd": command, "yield_time_ms": 250, "max_output_tokens": 4096}),
                &context(CancellationToken::new(), 4096),
            )
            .await;
        let exec_value = json_value(&exec_result);
        let session_id = exec_value["session_id"].as_u64().expect("running session");
        assert_eq!(exec_value["output"].as_str().unwrap().trim(), "ready");

        let write_result = write
            .execute_with_context(
                json!({"session_id": session_id, "chars": "world\n", "yield_time_ms": 1000}),
                &context(CancellationToken::new(), 4096),
            )
            .await;
        let write_value = json_value(&write_result);
        assert!(
            write_value["output"]
                .as_str()
                .unwrap()
                .contains("got:world")
        );
        assert!(write_value["session_id"].is_null());
        assert_eq!(write_value["exit_code"].as_u64(), Some(7));
        assert!(registry.take_notices().await.is_empty());

        registry.shutdown_processes().await;
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[tokio::test]
    async fn completion_notice_is_delivered_once_or_suppressed_after_read() {
        let registry = default_process_registry();
        let exec = ExecCommandTool::new(Arc::clone(&registry), None, None, None);
        let write = WriteStdinTool::new(Arc::clone(&registry));
        let exec_result = exec
            .execute_with_context(
                json!({"cmd": "sleep 1; echo done", "yield_time_ms": 250}),
                &context(CancellationToken::new(), 4096),
            )
            .await;
        let session_id = json_value(&exec_result)["session_id"]
            .as_u64()
            .expect("running session");
        assert_eq!(
            registry.take_notices().await,
            Vec::<String>::new(),
            "command has not completed yet"
        );
        tokio::time::sleep(Duration::from_millis(1_100)).await;
        let first = registry.take_notices().await;
        assert_eq!(first.len(), 1);
        assert!(registry.take_notices().await.is_empty());

        let poll_result = write
            .execute_with_context(
                json!({"session_id": session_id, "yield_time_ms": 1000}),
                &context(CancellationToken::new(), 4096),
            )
            .await;
        assert!(poll_result.success, "{:?}", poll_result.error_msg);
        assert!(registry.take_notices().await.is_empty());
        registry.shutdown_processes().await;

        let registry = default_process_registry();
        let exec = ExecCommandTool::new(Arc::clone(&registry), None, None, None);
        let write = WriteStdinTool::new(Arc::clone(&registry));
        let exec_result = exec
            .execute_with_context(
                json!({"cmd": "sleep 1; echo read-before-notice", "yield_time_ms": 250}),
                &context(CancellationToken::new(), 4096),
            )
            .await;
        let session_id = json_value(&exec_result)["session_id"]
            .as_u64()
            .expect("running session");
        let poll_result = write
            .execute_with_context(
                json!({"session_id": session_id, "yield_time_ms": 1000}),
                &context(CancellationToken::new(), 4096),
            )
            .await;
        assert!(poll_result.success, "{:?}", poll_result.error_msg);
        assert!(registry.take_notices().await.is_empty());
        registry.shutdown_processes().await;
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[tokio::test]
    async fn cancel_and_session_end_reap_process_group() {
        let registry = default_process_registry();
        let exec = ExecCommandTool::new(Arc::clone(&registry), None, None, None);
        let result = exec
            .execute_with_context(
                json!({"cmd": "sleep 30 & echo $$ $!; wait", "yield_time_ms": 250}),
                &context(CancellationToken::new(), 4096),
            )
            .await;
        let value = json_value(&result);
        let session_id = value["session_id"].as_u64().expect("running session");
        let mut pids = value["output"]
            .as_str()
            .unwrap()
            .trim()
            .split_whitespace()
            .map(|pid| pid.parse::<i32>().expect("process pid"));
        let pid = pids.next().expect("bash pid");
        let child_pid = pids.next().expect("child pid");
        registry.terminate(u32::try_from(session_id).unwrap()).await;
        registry.shutdown_processes().await;
        assert!(pid_is_gone(pid).await);
        assert!(pid_is_gone(child_pid).await);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[tokio::test]
    async fn exec_command_uses_the_scrubbed_shell_environment() {
        let registry = default_process_registry();
        let exec = ExecCommandTool::new(Arc::clone(&registry), None, None, None);
        // SAFETY: This test uniquely owns the variable name and removes it before returning.
        unsafe { std::env::set_var("RSI_EXEC_TEST_SECRET", "supersecret") };
        let result = exec
            .execute_with_context(
                json!({"cmd": "printf '%s' \"${RSI_EXEC_TEST_SECRET:-EMPTY}\"", "yield_time_ms": 30000}),
                &context(CancellationToken::new(), 4096),
            )
            .await;
        // SAFETY: Test cleanup; this test uniquely owns the variable name.
        unsafe { std::env::remove_var("RSI_EXEC_TEST_SECRET") };
        assert!(result.success, "{:?}", result.error_msg);
        assert_eq!(json_value(&result)["output"].as_str().unwrap(), "EMPTY");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[tokio::test]
    async fn session_end_reaps_running_process_group() {
        let registry = default_process_registry();
        let exec = ExecCommandTool::new(Arc::clone(&registry), None, None, None);
        let result = exec
            .execute_with_context(
                json!({"cmd": "sleep 30 & echo $$ $!; wait", "yield_time_ms": 250}),
                &context(CancellationToken::new(), 4096),
            )
            .await;
        let value = json_value(&result);
        let session_id = value["session_id"].as_u64().expect("running session");
        let mut pids = value["output"]
            .as_str()
            .unwrap()
            .trim()
            .split_whitespace()
            .map(|pid| pid.parse::<i32>().expect("process pid"));
        let pid = pids.next().expect("bash pid");
        let child_pid = pids.next().expect("child pid");
        registry.shutdown_processes().await;
        assert!(
            registry
                .snapshot(u32::try_from(session_id).unwrap())
                .await
                .is_none()
        );
        assert!(pid_is_gone(pid).await);
        assert!(pid_is_gone(child_pid).await);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[tokio::test]
    async fn idle_process_wakes_coalesce_and_rearm_after_drain() {
        let registry = default_process_registry();
        let controller = Arc::new(CountingWakeController::default());
        registry.set_wake_controller(Arc::clone(&controller) as _);
        let first = ProcessState::new(None, 1, 1);
        let second = ProcessState::new(None, 1, 1);
        complete_process(&registry, &first);
        complete_process(&registry, &second);
        for _ in 0..100 {
            if controller.arms.load(Ordering::Acquire) == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(controller.arms.load(Ordering::Acquire), 1);
        assert_eq!(controller.withdrawals.load(Ordering::Acquire), 0);

        send_process_notice(&registry, first);
        send_process_notice(&registry, second);
        let notices = registry.take_notices().await;
        assert_eq!(notices.len(), 2);
        for _ in 0..100 {
            if controller.withdrawals.load(Ordering::Acquire) == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(controller.arms.load(Ordering::Acquire), 1);
        assert_eq!(controller.withdrawals.load(Ordering::Acquire), 1);

        let third = ProcessState::new(None, 1, 1);
        complete_process(&registry, &third);
        for _ in 0..100 {
            if controller.arms.load(Ordering::Acquire) == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        send_process_notice(&registry, third);
        assert_eq!(registry.take_notices().await.len(), 1);
        for _ in 0..100 {
            if controller.withdrawals.load(Ordering::Acquire) == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(controller.arms.load(Ordering::Acquire), 2);
        assert_eq!(controller.withdrawals.load(Ordering::Acquire), 2);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[tokio::test]
    async fn running_process_completion_defers_wake_until_turn_end() {
        let registry = default_process_registry();
        let controller = Arc::new(CountingWakeController::default());
        registry.set_wake_controller(Arc::clone(&controller) as _);
        registry.begin_turn().await;
        let state = ProcessState::new(None, 1, 1);
        complete_process(&registry, &state);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(controller.arms.load(Ordering::Acquire), 0);

        registry.end_turn();
        for _ in 0..100 {
            if controller.arms.load(Ordering::Acquire) == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(controller.arms.load(Ordering::Acquire), 1);
        send_process_notice(&registry, state);
        assert_eq!(registry.take_notices().await.len(), 1);
        for _ in 0..100 {
            if controller.withdrawals.load(Ordering::Acquire) == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(controller.withdrawals.load(Ordering::Acquire), 1);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[tokio::test]
    async fn starting_turn_after_idle_wake_withdraws_until_turn_end() {
        let registry = default_process_registry();
        let controller = Arc::new(CountingWakeController::default());
        registry.set_wake_controller(Arc::clone(&controller) as _);
        let state = ProcessState::new(None, 1, 1);
        complete_process(&registry, &state);
        for _ in 0..100 {
            if controller.arms.load(Ordering::Acquire) == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(controller.arms.load(Ordering::Acquire), 1);
        assert_eq!(registry.pending_unread.load(Ordering::Acquire), 1);
        assert!(registry.wake_armed.load(Ordering::Acquire));

        registry.begin_turn().await;
        assert_eq!(controller.withdrawals.load(Ordering::Acquire), 1);
        registry.end_turn();
        for _ in 0..100 {
            if controller.arms.load(Ordering::Acquire) == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(controller.arms.load(Ordering::Acquire), 2);
        send_process_notice(&registry, state);
        assert_eq!(registry.take_notices().await.len(), 1);
        for _ in 0..100 {
            if controller.withdrawals.load(Ordering::Acquire) == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(controller.withdrawals.load(Ordering::Acquire), 2);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[tokio::test]
    async fn result_read_before_wake_dispatch_withdraws_the_pending_wake() {
        let registry = default_process_registry();
        let (release_tx, release_rx) = mpsc::channel(1);
        let controller = Arc::new(GatedWakeController::new(release_rx));
        registry.set_wake_controller(Arc::clone(&controller) as _);
        let state = ProcessState::new(None, 1, 1);
        complete_process(&registry, &state);
        for _ in 0..100 {
            if controller.entered.load(Ordering::Acquire) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(controller.entered.load(Ordering::Acquire));

        state.mark_read();
        let retire = registry.retire_pending(&state);
        release_tx.send(()).await.expect("wake test gate");
        retire.await;
        assert_eq!(controller.arms.load(Ordering::Acquire), 1);
        assert_eq!(controller.withdrawals.load(Ordering::Acquire), 1);
        assert!(!registry.wake_armed.load(Ordering::Acquire));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[tokio::test]
    async fn exited_leader_handle_is_disarmed_and_never_signalled_again() {
        use crate::process_control::recorded_group_signals::sent_to;
        let registry = default_process_registry();
        let (process_id, pgid) =
            test_support::start_running_with_pgid(&registry, "sleep 0.6; echo finished").await;
        let process_id_u32 = u32::try_from(process_id).unwrap();
        assert_eq!(sent_to(pgid), 0, "a live leader has not been signalled");

        // Natural exit: the group is killed once while the zombie leader pins
        // the PGID, the leader is reaped, and the handle becomes disarmed.
        for _ in 0..200 {
            if registry
                .snapshot(process_id_u32)
                .await
                .is_some_and(|snapshot| !snapshot.running)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let handle = registry
            .processes
            .lock()
            .await
            .get(&process_id_u32)
            .cloned()
            .expect("unread handle is retained");
        assert!(
            handle.state.group.lock().unwrap().is_none(),
            "an exited handle holds no signal target"
        );
        assert_eq!(sent_to(pgid), 1, "exactly the observe-kill before the reap");

        // Archive/delete/interrupt/shutdown all funnel into these calls.
        assert!(registry.terminate(process_id_u32).await);
        assert_eq!(
            sent_to(pgid),
            1,
            "terminate sends nothing to a disarmed handle"
        );
        handle.signal_group();
        registry.shutdown_processes().await;
        assert_eq!(
            sent_to(pgid),
            1,
            "shutdown sends nothing to a disarmed handle"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[tokio::test]
    async fn exited_leader_result_survives_the_disarm() {
        let registry = default_process_registry();
        let exec = ExecCommandTool::new(Arc::clone(&registry), None, None, None);
        let write = WriteStdinTool::new(Arc::clone(&registry));
        let result = exec
            .execute_with_context(
                json!({"cmd": "sleep 0.5; echo kept-output", "yield_time_ms": 250}),
                &context(CancellationToken::new(), 4096),
            )
            .await;
        let process_id = json_value(&result)["session_id"].as_u64().expect("running");
        tokio::time::sleep(Duration::from_millis(900)).await;
        let read = write
            .execute_with_context(
                json!({"session_id": process_id, "yield_time_ms": 500}),
                &context(CancellationToken::new(), 4096),
            )
            .await;
        let value = json_value(&read);
        assert_eq!(value["exit_code"], 0);
        assert!(value["output"].as_str().unwrap().contains("kept-output"));
        registry.shutdown_processes().await;
    }

    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn observation_errors_are_failures_never_an_observed_exit() {
        use nix::errno::Errno;
        let classify = |results: Vec<Result<(), Errno>>| {
            let results = std::sync::Mutex::new(results.into_iter());
            observe_leader_exit_blocking(1, |_| results.lock().unwrap().next().expect("scripted"))
        };
        assert!(matches!(
            classify(vec![Ok(())]),
            LeaderObservation::ExitedPinned
        ));
        assert!(matches!(
            classify(vec![Err(Errno::EINTR), Err(Errno::EINTR), Ok(())]),
            LeaderObservation::ExitedPinned
        ));
        for errno in [Errno::ECHILD, Errno::EINVAL, Errno::ESRCH, Errno::EPERM] {
            assert!(
                matches!(
                    classify(vec![Err(Errno::EINTR), Err(errno)]),
                    LeaderObservation::ObservationFailed(seen) if seen == errno
                ),
                "{errno} must not read as an exit"
            );
        }
        // The real waitid on a pid that is not our child is ECHILD.
        assert!(matches!(
            observe_leader_exit_blocking(1, waitid_leader_unreaped),
            LeaderObservation::ObservationFailed(Errno::ECHILD)
        ));
    }

    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[tokio::test]
    async fn leader_reaped_elsewhere_is_disarmed_without_any_group_signal() {
        use crate::process_control::recorded_group_signals::sent_to;
        let registry = default_process_registry();
        let mut command = tokio::process::Command::new("sh");
        command
            .args(["-c", "echo hi-from-child"])
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().expect("spawn");
        let pid = child.id().expect("pid") as i32;
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let stdin = child.stdin.take().unwrap();
        let state = ProcessState::new(Some(stdin), 4096, 4096);
        state.arm_group(pid);
        let readers = vec![
            tokio::spawn(read_stream(stdout, Arc::clone(&state), true)),
            tokio::spawn(read_stream(stderr, Arc::clone(&state), false)),
        ];
        // Let the leader exit, then reap it behind the supervisor's back: the
        // PGID is no longer pinned and the observation sees ECHILD.
        tokio::time::sleep(Duration::from_millis(400)).await;
        nix::sys::wait::waitpid(nix::unistd::Pid::from_raw(pid), None).expect("external reap");

        supervise_process(
            child,
            Arc::clone(&state),
            CancellationToken::new(),
            CancellationToken::new(),
            registry.notices_tx.clone(),
            Arc::clone(&registry),
            7,
            readers,
        )
        .await;

        assert_eq!(
            sent_to(pid),
            0,
            "a failed observation never signals the group"
        );
        assert!(
            state.group.lock().unwrap().is_none(),
            "the handle is disarmed"
        );
        let snapshot = state.snapshot().await;
        assert!(!snapshot.running);
        assert!(snapshot.exit_code.is_some(), "an exit status is recorded");
        assert!(
            snapshot.output.contains("hi-from-child"),
            "{:?}",
            snapshot.output
        );
    }

    async fn pid_is_gone(pid: i32) -> bool {
        let mut reaped = false;
        for _ in 0..100 {
            let signal = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None::<Signal>);
            if signal.is_err() {
                reaped = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(reaped, "process {pid} remained after cancellation");
        reaped
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[tokio::test]
    async fn concurrent_processes_have_a_clear_cap() {
        let registry = ProcessRegistry::new(1);
        let exec = ExecCommandTool::new(Arc::clone(&registry), None, None, None);
        let first = exec
            .execute_with_context(
                json!({"cmd": "sleep 30", "yield_time_ms": 250}),
                &context(CancellationToken::new(), 4096),
            )
            .await;
        assert!(first.success, "{:?}", first.error_msg);
        let second = exec
            .execute_with_context(
                json!({"cmd": "sleep 30", "yield_time_ms": 250}),
                &context(CancellationToken::new(), 4096),
            )
            .await;
        let error = result_text(&second);
        assert!(
            error.contains("concurrent process limit (1) reached"),
            "{error}"
        );
        registry.shutdown_processes().await;
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[tokio::test]
    async fn exec_output_is_head_tail_capped_with_omitted_accounting() {
        let registry = default_process_registry();
        let exec = ExecCommandTool::new(Arc::clone(&registry), None, None, None);
        let result = exec
            .execute_with_context(
                json!({"cmd": "printf HEAD; printf '0123456789%.0s' {1..30}; printf TAIL", "yield_time_ms": 30000, "max_output_tokens": 20}),
                &context(CancellationToken::new(), 20),
            )
            .await;
        let value = json_value(&result);
        let output = value["output"].as_str().unwrap();
        assert!(output.starts_with("HEAD01"), "{output}");
        assert!(output.contains("bytes omitted"), "{output}");
        assert!(output.ends_with("TAIL"), "{output}");
        assert!(value["stdout_omitted_bytes"].as_u64().unwrap() > 0);
    }
}
