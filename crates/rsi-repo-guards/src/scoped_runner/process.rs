//! Bounded foreground process ownership. Only groups created here are stopped.
use crate::Ctx;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{self, Read};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

pub const BUILD_ENV_NAMES: &[&str] = &[
    "PATH",
    "HOME",
    "USERPROFILE",
    "SYSTEMROOT",
    "PATHEXT",
    "CARGO_HOME",
    "RUSTUP_HOME",
    "CARGO_TARGET_DIR",
    "CARGO_BUILD_JOBS",
    "RUSTUP_TOOLCHAIN",
    "RUSTFLAGS",
    "CARGO_ENCODED_RUSTFLAGS",
    "RSI_CARGO_SLOT_HELD",
    "TMPDIR",
    "TMP",
    "TEMP",
    "RUST_BACKTRACE",
    "CHECK_TOUCHED_SHARDS_TMPDIR",
    "CARGO_INCREMENTAL",
    "CARGO_PROFILE_DEV_DEBUG",
    "RUSTC_WRAPPER",
    "RSI_BUILD_SLOTS",
    "RSI_BUILD_SLOT_ROOT",
    "RSI_BUILD_SCCACHE",
    "SCCACHE_CACHE_SIZE",
    "SCCACHE_DIR",
    "SCCACHE_SERVER_UDS",
    "SCCACHE_CLIENT_SIDE",
    "RUST_TEST_THREADS",
    "NEXTEST_TEST_THREADS",
];

pub fn build_env(ctx: &Ctx) -> BTreeMap<String, String> {
    BUILD_ENV_NAMES
        .iter()
        .filter_map(|key| ctx.var(key).map(|value| ((*key).into(), value.into())))
        .collect()
}

pub fn python_free(command: &[String]) -> io::Result<()> {
    if command.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty command"));
    }
    for arg in command {
        let name = Path::new(arg)
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_ascii_lowercase();
        if name.starts_with("python")
            || name == "pip"
            || name.starts_with("pip3")
            || name.ends_with(".py")
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "operator directive #358: Python execution is forbidden",
            ));
        }
    }
    Ok(())
}

pub struct OwnedChild {
    pub(super) child: Child,
    pub stdin: Option<std::process::ChildStdin>,
    stopped: bool,
}
impl OwnedChild {
    pub fn spawn(
        argv: &[String],
        cwd: &Path,
        env: &BTreeMap<String, String>,
        piped_stdin: bool,
    ) -> io::Result<Self> {
        python_free(argv)?;
        let mut command = Command::new(&argv[0]);
        command
            .args(&argv[1..])
            .current_dir(cwd)
            .env_clear()
            .envs(env)
            .stdin(if piped_stdin {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        Self::spawn_command(command)
    }
    fn spawn_command(mut command: Command) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // process_group(0) creates a group whose id equals our child's pid.
            command.process_group(0);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x00000200); // CREATE_NEW_PROCESS_GROUP
        }
        let mut child = command.spawn()?;
        let stdin = child.stdin.take();
        Ok(Self {
            child,
            stdin,
            stopped: false,
        })
    }
    pub fn id(&self) -> u32 {
        self.child.id()
    }
    #[cfg(unix)]
    fn signal(&mut self, signal: i32) {
        // SAFETY: the negative pid addresses only the group this child created.
        let rc = unsafe { libc::kill(-(self.child.id() as i32), signal) };
        if rc < 0 && self.child.try_wait().ok().flatten().is_none() {
            // macOS may refuse a group containing only exiting processes.
            // SAFETY: fallback is limited to our owned live child.
            unsafe {
                libc::kill(self.child.id() as i32, signal);
            }
        }
    }
    pub fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        self.stdin.take();
        #[cfg(unix)]
        {
            self.signal(libc::SIGTERM);
            let end = Instant::now() + Duration::from_millis(200);
            while Instant::now() < end {
                if self.child.try_wait().ok().flatten().is_some() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            // Even when the leader exited, descendants may still own pipes.
            self.signal(libc::SIGKILL);
        }
        #[cfg(windows)]
        {
            let mut taskkill = Command::new("taskkill");
            taskkill.args(["/PID", &self.child.id().to_string(), "/T", "/F"]);
            stop_windows_child(&mut self.child, &mut taskkill, Duration::from_secs(2));
        }
        #[cfg(not(windows))]
        {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[cfg(any(windows, test))]
fn wait_bounded(child: &mut Child, grace: Duration) -> io::Result<bool> {
    let deadline = Instant::now() + grace;
    loop {
        if child.try_wait()?.is_some() {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// taskkill may be absent, fail, or wedge. Bound its own execution/reap, then
/// always stop/reap the direct owned child with a separate finite grace.
#[cfg(any(windows, test))]
fn stop_windows_child(child: &mut Child, taskkill: &mut Command, grace: Duration) {
    if let Ok(mut helper) = taskkill
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        if !wait_bounded(&mut helper, grace).unwrap_or(false) {
            let _ = helper.kill();
            let _ = wait_bounded(&mut helper, grace);
        }
    }
    let _ = child.kill();
    let _ = wait_bounded(child, grace);
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        self.stop();
    }
}

#[derive(Debug)]
pub struct Outcome {
    /// Negative POSIX signal, otherwise the child's exact exit code.
    pub code: i32,
    pub timed_out: bool,
    pub cancelled: Option<usize>,
    pub output_complete: bool,
}

enum Event {
    Data(usize, Vec<u8>),
    End(usize),
    Error,
}
fn reader(
    mut input: impl Read + Send + 'static,
    index: usize,
    send: mpsc::SyncSender<Event>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut buffer = [0; 8192];
        loop {
            match input.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    if send
                        .send(Event::Data(index, buffer[..count].to_vec()))
                        .is_err()
                    {
                        return;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    let _ = send.send(Event::Error);
                    break;
                }
            }
        }
        let _ = send.send(Event::End(index));
    })
}

pub fn install_signals() -> io::Result<Arc<AtomicUsize>> {
    let signal = Arc::new(AtomicUsize::new(0));
    #[cfg(unix)]
    for number in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
        signal_hook::flag::register_usize(number, signal.clone(), number as usize)?;
    }
    Ok(signal)
}

/// Stream complete lines and bounded unterminated tails. Deadline/cancellation
/// checks never depend on output; a silent child cannot bypass either clock.
pub fn stream(
    argv: &[String],
    cwd: &Path,
    env: &BTreeMap<String, String>,
    deadline: Instant,
    cancel: &AtomicUsize,
    mut emit: impl FnMut(&str),
    mut partial: impl FnMut(&str),
) -> io::Result<Outcome> {
    stream_channels(
        argv,
        cwd,
        env,
        deadline,
        cancel,
        |_, line| emit(line),
        |_, tail| partial(tail),
    )
}

/// Like `stream`, retaining the pipe identity (0 stdout, 1 stderr).
pub fn stream_channels(
    argv: &[String],
    cwd: &Path,
    env: &BTreeMap<String, String>,
    deadline: Instant,
    cancel: &AtomicUsize,
    mut emit: impl FnMut(usize, &str),
    mut partial: impl FnMut(usize, &str),
) -> io::Result<Outcome> {
    pump(
        OwnedChild::spawn(argv, cwd, env, false)?,
        deadline,
        cancel,
        false,
        |index, line| emit(index, &String::from_utf8_lossy(line)),
        |index, tail| partial(index, &String::from_utf8_lossy(tail)),
    )
}

/// The same owned supervisor without text conversion. Pipe identity and every
/// byte (including non-UTF8 Git paths and NUL separators) survive. Chunks are
/// emitted immediately without line buffering; callbacks must not block.
pub fn stream_bytes(
    argv: &[String],
    cwd: &Path,
    env: &BTreeMap<String, String>,
    deadline: Instant,
    cancel: &AtomicUsize,
    emit: impl FnMut(usize, &[u8]),
) -> io::Result<Outcome> {
    pump(
        OwnedChild::spawn(argv, cwd, env, false)?,
        deadline,
        cancel,
        true,
        emit,
        |_, _| {},
    )
}

/// Bounded Git object/history operations with native filenames. These builtins
/// read source objects (including .py paths) as data; they do not execute them.
/// Keep this on the same owned pump as every runner process. No Git alias or
/// arbitrary external subcommand is admitted by this metadata adapter.
pub fn stream_git_bytes(
    repo: &Path,
    args: &[OsString],
    env: &BTreeMap<OsString, OsString>,
    deadline: Instant,
    cancel: &AtomicUsize,
    emit: impl FnMut(usize, &[u8]),
) -> io::Result<Outcome> {
    if !args
        .first()
        .and_then(|arg| arg.to_str())
        .is_some_and(|arg| {
            matches!(
                arg,
                "rev-parse"
                    | "cat-file"
                    | "diff"
                    | "show"
                    | "merge-base"
                    | "merge-tree"
                    | "commit-tree"
                    | "ls-files"
                    | "archive"
                    | "ls-tree"
            )
        })
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected Git object/history builtin",
        ));
    }
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(repo)
        .arg("--no-optional-locks")
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .current_dir(repo)
        .env_clear()
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    pump(
        OwnedChild::spawn_command(command)?,
        deadline,
        cancel,
        true,
        emit,
        |_, _| {},
    )
}

fn pump(
    mut child: OwnedChild,
    deadline: Instant,
    cancel: &AtomicUsize,
    raw: bool,
    mut emit: impl FnMut(usize, &[u8]),
    mut partial: impl FnMut(usize, &[u8]),
) -> io::Result<Outcome> {
    let (send, receive) = mpsc::sync_channel(64);
    let stdout = child
        .child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("missing stdout pipe"))?;
    let stderr = child
        .child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("missing stderr pipe"))?;
    let readers = [reader(stdout, 0, send.clone()), reader(stderr, 1, send)];
    let mut buffers = [Vec::new(), Vec::new()];
    let mut closed = [false; 2];
    let mut outcome = None;
    let mut exited_at = None;
    let mut output_damaged = false;
    loop {
        let signal = cancel.load(Ordering::Relaxed);
        if outcome.is_none() && (signal != 0 || Instant::now() >= deadline) {
            child.stop();
            outcome = Some(Outcome {
                code: if signal == 0 {
                    124
                } else {
                    128 + signal as i32
                },
                timed_out: signal == 0,
                cancelled: (signal != 0).then_some(signal),
                output_complete: false,
            });
            exited_at = Some(Instant::now());
        }
        if outcome.is_none() {
            if let Some(status) = child.child.try_wait()? {
                #[cfg(unix)]
                let code = {
                    use std::os::unix::process::ExitStatusExt;
                    status
                        .code()
                        .unwrap_or_else(|| -status.signal().unwrap_or(1))
                };
                #[cfg(not(unix))]
                let code = status.code().unwrap_or(1);
                outcome = Some(Outcome {
                    code,
                    timed_out: false,
                    cancelled: None,
                    output_complete: false,
                });
                exited_at = Some(Instant::now());
                child.stop();
            }
        }
        match receive.recv_timeout(Duration::from_millis(10)) {
            Ok(Event::Data(index, data)) => {
                if raw {
                    emit(index, &data);
                } else {
                    buffers[index].extend(data);
                    while let Some(end) = buffers[index].iter().position(|c| *c == b'\n') {
                        let line: Vec<_> = buffers[index].drain(..=end).collect();
                        emit(index, &line);
                    }
                    if !buffers[index].is_empty() {
                        partial(index, &buffers[index]);
                    }
                    if buffers[index].len() >= 4 * 1024 * 1024 {
                        output_damaged = true;
                        emit(index, &buffers[index]);
                        buffers[index].clear();
                    }
                }
            }
            Ok(Event::End(index)) => {
                closed[index] = true;
            }
            Ok(Event::Error) => output_damaged = true,
            Err(mpsc::RecvTimeoutError::Disconnected) => closed = [true; 2],
            Err(mpsc::RecvTimeoutError::Timeout) => (),
        }
        if outcome.is_some()
            && (closed.iter().all(|v| *v)
                || exited_at.is_some_and(|t| t.elapsed() > Duration::from_secs(2)))
        {
            break;
        }
    }
    for (index, buffer) in buffers.into_iter().enumerate() {
        if !buffer.is_empty() {
            emit(index, &buffer);
        }
    }
    drop(receive);
    for thread in readers {
        if thread.is_finished() {
            let _ = thread.join();
        }
    }
    let mut outcome = outcome.unwrap_or(Outcome {
        code: 1,
        timed_out: false,
        cancelled: None,
        output_complete: false,
    });
    outcome.output_complete = closed.iter().all(|v| *v) && !output_damaged;
    Ok(outcome)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    fn args(command: &str) -> Vec<String> {
        vec!["/bin/sh".into(), "-c".into(), command.into()]
    }
    #[test]
    fn output_and_exact_exit_are_preserved() {
        let mut output = String::new();
        let done = stream(
            &args("printf 'one\\ntail'; printf 'error\\n' >&2; exit 7"),
            Path::new("/tmp"),
            &BTreeMap::new(),
            Instant::now() + Duration::from_secs(3),
            &AtomicUsize::new(0),
            |line| output.push_str(line),
            |_| {},
        )
        .unwrap();
        assert_eq!(done.code, 7);
        assert!(output.contains("one\n"));
        assert!(output.contains("tail"));
        assert!(output.contains("error\n"));
    }
    #[test]
    fn silent_descendants_are_stopped_at_deadline() {
        let start = Instant::now();
        let done = stream(
            &args("sleep 30 & wait"),
            Path::new("/tmp"),
            &BTreeMap::new(),
            start + Duration::from_millis(80),
            &AtomicUsize::new(0),
            |_| {},
            |_| {},
        )
        .unwrap();
        assert!(done.timed_out);
        assert_eq!(done.code, 124);
        assert!(start.elapsed() < Duration::from_secs(2));
    }
    #[test]
    fn cancellation_is_distinct_from_timeout_and_python_is_refused() {
        let done = stream(
            &args("sleep 30"),
            Path::new("/tmp"),
            &BTreeMap::new(),
            Instant::now() + Duration::from_secs(2),
            &AtomicUsize::new(15),
            |_| {},
            |_| {},
        )
        .unwrap();
        assert_eq!(done.cancelled, Some(15));
        assert_eq!(done.code, 143);
        assert!(!done.timed_out);
        assert!(python_free(&["/usr/bin/python3".into()]).is_err());
        assert!(python_free(&["env".into(), "legacy.py".into()]).is_err());
    }
    #[test]
    fn build_environment_is_an_explicit_allowlist() {
        let ctx = Ctx::new(
            "/tmp",
            &[
                ("PATH", "/bin"),
                ("CARGO_TARGET_DIR", "target"),
                ("UNRELATED_AUTHORITY", "secret"),
            ],
        );
        assert_eq!(
            build_env(&ctx),
            BTreeMap::from([
                ("PATH".into(), "/bin".into()),
                ("CARGO_TARGET_DIR".into(), "target".into())
            ])
        );
    }

    #[test]
    fn byte_stream_preserves_nul_non_utf8_and_pipe_identity() {
        let mut output = [Vec::new(), Vec::new()];
        let done = stream_bytes(
            &args("printf '\\377\\000name\\n'; printf '\\376error' >&2"),
            Path::new("/tmp"),
            &BTreeMap::new(),
            Instant::now() + Duration::from_secs(3),
            &AtomicUsize::new(0),
            |pipe, bytes| output[pipe].extend_from_slice(bytes),
        )
        .unwrap();
        assert_eq!(done.code, 0);
        assert!(done.output_complete);
        assert_eq!(output[0], b"\xff\0name\n");
        assert_eq!(output[1], b"\xfeerror");
    }

    #[test]
    fn windows_cleanup_helper_and_direct_fallback_are_bounded() {
        for command in ["exit 0", "exit 7", "exec sleep 30"] {
            let mut child = Command::new("/bin/sh")
                .args(["-c", "exec sleep 30"])
                .spawn()
                .unwrap();
            let mut helper = Command::new("/bin/sh");
            helper.args(["-c", command]);
            let start = Instant::now();
            stop_windows_child(&mut child, &mut helper, Duration::from_millis(30));
            assert!(start.elapsed() < Duration::from_secs(2));
            assert!(child.try_wait().unwrap().is_some());
        }
        let mut child = Command::new("/bin/sh")
            .args(["-c", "exec sleep 30"])
            .spawn()
            .unwrap();
        stop_windows_child(
            &mut child,
            &mut Command::new("/definitely/missing/taskkill"),
            Duration::from_millis(30),
        );
        assert!(child.try_wait().unwrap().is_some());
    }
}

/// Foreground shell-client child supervision. Inherited stdin/stdout/stderr
/// preserve interactive command behavior; the group belongs only to this run.
pub fn supervise(argv: &[String]) -> io::Result<i32> {
    python_free(argv)?;
    let cancel = install_signals()?;
    let mut command = Command::new(&argv[0]);
    command.args(&argv[1..]);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x00000200);
    }
    let child = command.spawn()?;
    let mut child = OwnedChild {
        child,
        stdin: None,
        stopped: false,
    };
    loop {
        let signal = cancel.load(Ordering::Relaxed);
        if signal != 0 {
            child.stop();
            return Ok(128 + signal as i32);
        }
        if let Some(status) = child.child.try_wait()? {
            #[cfg(unix)]
            let code = {
                use std::os::unix::process::ExitStatusExt;
                status
                    .code()
                    .unwrap_or_else(|| 128 + status.signal().unwrap_or(1))
            };
            #[cfg(not(unix))]
            let code = status.code().unwrap_or(1);
            child.stop();
            return Ok(code);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
