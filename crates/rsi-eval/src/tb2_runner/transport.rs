//! Command execution and the SSH side of the remote transport (#3679 R2, design #3628
//! "Architecture and transport" items 2, 4 and 5).
//!
//! Everything the runner asks of the hub's operating system goes through [`CommandExecutor`]:
//! `docker -H ssh://<host> ...` ([`super::docker`]) and bounded `ssh -o BatchMode=yes ...`
//! commands ([`Remote`]). The real executor is [`SystemExecutor`]; tests inject a fake one and
//! assert argv, quoting, environment and timeouts without a Docker daemon or a laptop. No remote
//! RSI program, venv, provider key or reverse socket forward exists on this path.

use std::{
    io::{Read, Write},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde::Serialize;

use super::trial::EnvError;

/// SSH client options (the historical `scripts/bench-transport` set): never prompt, fail fast
/// on an unreachable host, notice a dead connection.
pub const SSH_OPTS: [&str; 8] = [
    "-o",
    "BatchMode=yes",
    "-o",
    "ConnectTimeout=15",
    "-o",
    "ServerAliveInterval=30",
    "-o",
    "ServerAliveCountMax=6",
];

/// The only environment variables a spawned `docker` or `ssh` inherits. Everything else, in
/// particular `DOCKER_HOST`, `DOCKER_CONTEXT`, `COMPOSE_*` and provider keys, is cleared: the
/// Docker host is always the explicit `-H` argument.
pub const INHERITED_ENV: [&str; 9] = [
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "LANG",
    "TMPDIR",
    "XDG_RUNTIME_DIR",
    "SSH_AUTH_SOCK",
    "SSH_AGENT_PID",
];

/// Default cap on captured output per stream. A reward file or a probe line is far below it.
pub const DEFAULT_MAX_OUTPUT: usize = 1 << 20;

/// Hub executables approved by the R7 retirement contract (#3685). Shell commands and
/// benchmark scripts execute inside the remote environment through these two transports.
pub const APPROVED_EXECUTABLES: [&str; 2] = ["docker", "ssh"];

/// What the runner asks the operating system to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    /// Bytes written to stdin; `None` closes stdin.
    pub stdin: Option<Vec<u8>>,
    /// The command is killed and reported as [`ExecError::Timeout`] after this long.
    pub timeout: Duration,
    /// Captured bytes kept per stream; the rest is drained and dropped.
    pub max_output: usize,
}

impl CommandSpec {
    #[must_use]
    pub fn new(program: &str, args: Vec<String>, timeout: Duration) -> Self {
        Self {
            program: program.to_string(),
            args,
            stdin: None,
            timeout,
            max_output: DEFAULT_MAX_OUTPUT,
        }
    }
}

/// A finished command. `code` is `None` when a signal ended it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CommandOutput {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// A stream was longer than `max_output`.
    pub truncated: bool,
}

impl CommandOutput {
    #[must_use]
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }

    #[must_use]
    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    /// The last lines of stderr, joined, for an error message.
    #[must_use]
    pub fn stderr_tail(&self) -> String {
        let text = String::from_utf8_lossy(&self.stderr);
        let lines: Vec<&str> = text.trim().lines().collect();
        let start = lines.len().saturating_sub(3);
        lines[start..].join(" | ")
    }
}

/// Why a command did not finish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecError {
    /// The program could not be started.
    Spawn(String),
    /// It ran past its timeout and was killed.
    Timeout,
}

/// Runs commands. Implementations must be usable from the scoped trial threads.
pub trait CommandExecutor: Send + Sync {
    /// Run one command to completion.
    ///
    /// # Errors
    /// [`ExecError`] when it cannot start or exceeds its timeout.
    fn run(&self, spec: &CommandSpec) -> Result<CommandOutput, ExecError>;
}

/// The real executor: `std::process` with a cleared environment, bounded output and a timeout.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemExecutor;

fn drain(mut pipe: impl Read + Send + 'static, cap: usize) -> thread::JoinHandle<(Vec<u8>, bool)> {
    thread::spawn(move || {
        let mut kept = Vec::new();
        let mut truncated = false;
        let mut chunk = [0u8; 8192];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let room = cap.saturating_sub(kept.len());
                    if n > room {
                        truncated = true;
                    }
                    kept.extend_from_slice(&chunk[..n.min(room)]);
                }
            }
        }
        (kept, truncated)
    })
}

impl CommandExecutor for SystemExecutor {
    fn run(&self, spec: &CommandSpec) -> Result<CommandOutput, ExecError> {
        if !APPROVED_EXECUTABLES.contains(&spec.program.as_str()) {
            return Err(ExecError::Spawn(format!(
                "runner executable {:?} is not approved (expected docker or ssh)",
                spec.program
            )));
        }
        let mut command = Command::new(&spec.program);
        command.args(&spec.args).env_clear();
        for key in INHERITED_ENV {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        command
            .stdin(if spec.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|error| ExecError::Spawn(format!("{}: {error}", spec.program)))?;
        let writer = spec.stdin.clone().and_then(|bytes| {
            child.stdin.take().map(|mut stdin| {
                thread::spawn(move || {
                    // A child that exits early closes the pipe; that is its own result.
                    let _ = stdin.write_all(&bytes);
                })
            })
        });
        let stdout = child.stdout.take().map(|p| drain(p, spec.max_output));
        let stderr = child.stderr.take().map(|p| drain(p, spec.max_output));
        let started = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) if started.elapsed() < spec.timeout => {
                    thread::sleep(Duration::from_millis(5));
                }
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break Err(ExecError::Timeout);
                }
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break Err(ExecError::Spawn(format!("{}: {error}", spec.program)));
                }
            }
        };
        if let Some(writer) = writer {
            let _ = writer.join();
        }
        let join = |handle: Option<thread::JoinHandle<(Vec<u8>, bool)>>| {
            handle.and_then(|h| h.join().ok()).unwrap_or_default()
        };
        let (stdout, out_cut) = join(stdout);
        let (stderr, err_cut) = join(stderr);
        status.map(|status| CommandOutput {
            code: status.code(),
            stdout,
            stderr,
            truncated: out_cut || err_cut,
        })
    }
}

/// What kind of operation failed. Typed so callers and the CLI never parse prose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OpKind {
    /// The Docker host is not allowlisted, or does not match `bench.laptop_host`.
    HostRefused,
    /// ssh did not answer, or the connection died.
    SshFailed,
    /// The Docker daemon is unreachable (usually the SSH tunnel under `-H ssh://`).
    DockerUnreachable,
    /// A Docker or Compose command failed for another reason.
    DockerFailed,
    /// Docker Compose v2 is missing.
    ComposeUnavailable,
    /// The remote is not linux/amd64.
    UnsupportedPlatform,
    DiskInsufficient,
    MemoryInsufficient,
    /// Free disk or available memory could not be measured; the runner fails closed.
    CapacityUnmeasured,
    /// The checkout or the plan differs from the pinned dataset.
    PinDrift,
    ImagePullFailed,
    /// The pulled or running image is not the pinned digest.
    DigestMismatch,
    /// The running container's labels are not the trial's identity.
    LabelMismatch,
    /// CPU, memory or log bind of the running container differ from the plan.
    LimitMismatch,
    /// The main container did not become ready.
    NotReady,
    /// An operation ran past its deadline.
    Timeout,
    /// The plan is malformed or unusable.
    InvalidPlan,
    /// A task layout or mode this slice does not implement.
    Unsupported,
    /// A path is not safe to hand to a remote command.
    UnsafePath,
    /// A hub filesystem operation failed.
    Io,
    /// The run journal is owned by another runner, belongs to another plan, binary or dataset, or
    /// cannot be written (R5).
    Journal,
    /// An internal runner or trial worker panicked; the run stopped without a summary.
    RunnerPanicked,
}

impl OpKind {
    /// Stable snake_case spelling, the same as the serialized form.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HostRefused => "host_refused",
            Self::SshFailed => "ssh_failed",
            Self::DockerUnreachable => "docker_unreachable",
            Self::DockerFailed => "docker_failed",
            Self::ComposeUnavailable => "compose_unavailable",
            Self::UnsupportedPlatform => "unsupported_platform",
            Self::DiskInsufficient => "disk_insufficient",
            Self::MemoryInsufficient => "memory_insufficient",
            Self::CapacityUnmeasured => "capacity_unmeasured",
            Self::PinDrift => "pin_drift",
            Self::ImagePullFailed => "image_pull_failed",
            Self::DigestMismatch => "digest_mismatch",
            Self::LabelMismatch => "label_mismatch",
            Self::LimitMismatch => "limit_mismatch",
            Self::NotReady => "not_ready",
            Self::Timeout => "timeout",
            Self::InvalidPlan => "invalid_plan",
            Self::Unsupported => "unsupported",
            Self::UnsafePath => "unsafe_path",
            Self::Io => "io",
            Self::Journal => "journal",
            Self::RunnerPanicked => "runner_panicked",
        }
    }

    /// The process exit status, keeping the historical `scripts/bench-transport` family:
    /// 20 preflight, 21 pin check, 22 shard/trial operation, 23 ssh (24 is new: the run journal).
    #[must_use]
    pub const fn exit_code(self) -> u8 {
        match self {
            Self::HostRefused
            | Self::DockerUnreachable
            | Self::ComposeUnavailable
            | Self::UnsupportedPlatform
            | Self::DiskInsufficient
            | Self::MemoryInsufficient
            | Self::CapacityUnmeasured
            | Self::InvalidPlan
            | Self::Unsupported
            | Self::UnsafePath => 20,
            Self::PinDrift | Self::DigestMismatch => 21,
            Self::ImagePullFailed
            | Self::DockerFailed
            | Self::LabelMismatch
            | Self::LimitMismatch
            | Self::NotReady
            | Self::Timeout
            | Self::Io
            | Self::RunnerPanicked => 22,
            Self::SshFailed => 23,
            Self::Journal => 24,
        }
    }
}

/// A typed operation failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpError {
    pub kind: OpKind,
    pub detail: String,
}

impl OpError {
    #[must_use]
    pub fn new(kind: OpKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
        }
    }
}

impl std::fmt::Display for OpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.kind.as_str(), self.detail)
    }
}

impl std::error::Error for OpError {}

/// Trial-environment view of an operation failure: a deadline overrun is a timeout, a lost
/// transport is a (retryable) infrastructure interruption, everything else is an ordinary
/// failure.
impl From<OpError> for EnvError {
    fn from(error: OpError) -> Self {
        match error.kind {
            OpKind::Timeout => Self::Timeout,
            OpKind::SshFailed | OpKind::DockerUnreachable => Self::Infra(error.to_string()),
            _ => Self::Failed(error.to_string()),
        }
    }
}

// ---- quoting and validation ----------------------------------------------------------------

/// POSIX single-quote `value` (a bare word when it is already safe).
#[must_use]
pub fn posix_quote(value: &str) -> String {
    let safe = !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-./=:@+,%".contains(&b));
    if safe {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', r"'\''"))
    }
}

/// `command` as one ssh remote-command string that runs it under `sh -c`.
///
/// sshd hands that string to the remote user's login shell, which may be fish (#3472), so the
/// outer quoting must parse identically in a POSIX shell and in fish. Fish reads `\\` and `\'`
/// as escapes inside single quotes, so runs of ordinary characters are single-quoted while `'`
/// and `\` are emitted as `"'"` and `"\\"`, which both shells read the same way.
#[must_use]
pub fn remote_sh(command: &str) -> String {
    let mut parts = String::new();
    let mut run = String::new();
    let flush = |parts: &mut String, run: &mut String| {
        if !run.is_empty() {
            parts.push('\'');
            parts.push_str(run);
            parts.push('\'');
            run.clear();
        }
    };
    for ch in command.chars() {
        match ch {
            '\'' => {
                flush(&mut parts, &mut run);
                parts.push_str("\"'\"");
            }
            '\\' => {
                flush(&mut parts, &mut run);
                parts.push_str("\"\\\\\"");
            }
            other => run.push(other),
        }
    }
    flush(&mut parts, &mut run);
    if parts.is_empty() {
        parts.push_str("''");
    }
    format!("sh -c {parts}")
}

/// An ssh destination: `[user@]host`, never starting with `-` (option injection).
///
/// # Errors
/// [`OpKind::HostRefused`] for anything else.
pub fn validate_ssh_host(host: &str) -> Result<(), OpError> {
    let (user, name) = host.split_once('@').map_or(("a", host), |(u, h)| (u, h));
    let word = |s: &str| {
        !s.is_empty()
            && !s.starts_with(['-', '.'])
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    };
    if word(user) && word(name) {
        Ok(())
    } else {
        Err(OpError::new(
            OpKind::HostRefused,
            format!("{host:?} is not a plain [user@]host"),
        ))
    }
}

/// Check the exact allowlisted Docker host and its correspondence to `bench.laptop_host`
/// before any remote action: `docker_host` must be `ssh://<laptop_host>` and an allowlist entry.
///
/// # Errors
/// [`OpKind::HostRefused`] naming the failed rule.
pub fn validate_hosts(
    docker_host: &str,
    laptop_host: &str,
    allowed: &[String],
) -> Result<(), OpError> {
    validate_ssh_host(laptop_host)?;
    if !allowed.iter().any(|a| a == docker_host) {
        return Err(OpError::new(
            OpKind::HostRefused,
            format!("{docker_host} is not in bench.docker_hosts"),
        ));
    }
    if docker_host.strip_prefix("ssh://") != Some(laptop_host) {
        return Err(OpError::new(
            OpKind::HostRefused,
            format!("{docker_host} is not ssh://{laptop_host} (bench.laptop_host)"),
        ));
    }
    Ok(())
}

/// A path safe to name in a remote command and a bind source: absolute, no `..`, and only
/// conservative characters.
///
/// # Errors
/// [`OpKind::UnsafePath`].
pub fn validate_remote_path(path: &str) -> Result<(), OpError> {
    let ok = path.starts_with('/')
        && path.len() > 1
        && path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-/@+=:,".contains(&b))
        && !path.split('/').any(|c| c == "..");
    if ok {
        Ok(())
    } else {
        Err(OpError::new(
            OpKind::UnsafePath,
            format!("{path:?} is not a safe absolute remote path"),
        ))
    }
}

// ---- ssh -----------------------------------------------------------------------------------

/// Bounded `ssh` commands against one host.
pub struct Remote<'a> {
    pub exec: &'a dyn CommandExecutor,
    pub host: &'a str,
}

impl Remote<'_> {
    /// The argv of one remote command (exposed so tests can assert it).
    #[must_use]
    pub fn spec(&self, command: &str, timeout: Duration) -> CommandSpec {
        let mut args: Vec<String> = SSH_OPTS.iter().map(ToString::to_string).collect();
        args.push("--".to_string());
        args.push(self.host.to_string());
        args.push(remote_sh(command));
        CommandSpec::new("ssh", args, timeout)
    }

    /// Run `command` on the host; a nonzero exit is returned as output (the caller decides),
    /// while an ssh-level failure (exit 255, no start, signal) is [`OpKind::SshFailed`].
    ///
    /// # Errors
    /// [`OpKind::SshFailed`] or [`OpKind::Timeout`].
    pub fn sh(&self, command: &str, timeout: Duration) -> Result<CommandOutput, OpError> {
        let output = self
            .exec
            .run(&self.spec(command, timeout))
            .map_err(|error| match error {
                ExecError::Timeout => OpError::new(
                    OpKind::Timeout,
                    format!("ssh {} ran past {}s", self.host, timeout.as_secs()),
                ),
                ExecError::Spawn(why) => OpError::new(OpKind::SshFailed, why),
            })?;
        match output.code {
            Some(255) | None => Err(OpError::new(
                OpKind::SshFailed,
                format!("ssh {}: {}", self.host, output.stderr_tail()),
            )),
            Some(_) => Ok(output),
        }
    }

    /// Like [`Remote::sh`] but a nonzero exit is an error of `kind`.
    ///
    /// # Errors
    /// An ssh failure, or `kind` for a nonzero exit.
    pub fn must(
        &self,
        command: &str,
        timeout: Duration,
        kind: OpKind,
    ) -> Result<CommandOutput, OpError> {
        let output = self.sh(command, timeout)?;
        if output.success() {
            Ok(output)
        } else {
            Err(OpError::new(
                kind,
                format!(
                    "`{}` exited {}: {}",
                    command.chars().take(80).collect::<String>(),
                    output.code.unwrap_or(-1),
                    output.stderr_tail()
                ),
            ))
        }
    }
}

/// Facts measured on the laptop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RemoteFacts {
    pub home: String,
    pub os: String,
    pub arch: String,
    pub disk_free_bytes: Option<u64>,
    pub mem_available_bytes: Option<u64>,
}

/// The probe script: one `key=value` per line, never fails. `{ROOT}` is replaced by the quoted
/// Docker root directory; free space falls back to `$HOME` when it cannot be stat'ed.
const PROBE: &str = r#"echo "home=$HOME"
echo "os=$(uname -s)"
echo "arch=$(uname -m)"
d={ROOT}
[ -d "$d" ] && df -Pk -- "$d" >/dev/null 2>&1 || d=$HOME
df -Pk -- "$d" 2>/dev/null | tail -n 1 | { read -r _ _ _ avail _; echo "disk_kb=$avail"; }
while read -r k v _; do [ "$k" = MemAvailable: ] && echo "mem_kb=$v"; done < /proc/meminfo 2>/dev/null
exit 0"#;

/// Parse the probe output. Missing or non-numeric capacity stays `None` (fail closed later).
#[must_use]
pub fn parse_probe(text: &str) -> RemoteFacts {
    let mut facts = RemoteFacts {
        home: String::new(),
        os: String::new(),
        arch: String::new(),
        disk_free_bytes: None,
        mem_available_bytes: None,
    };
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        let kib = || value.parse::<u64>().ok().and_then(|v| v.checked_mul(1024));
        match key {
            "home" => facts.home = value.to_string(),
            "os" => facts.os = value.to_string(),
            "arch" => facts.arch = value.to_string(),
            "disk_kb" => facts.disk_free_bytes = kib(),
            "mem_kb" => facts.mem_available_bytes = kib(),
            _ => {}
        }
    }
    facts
}

/// Probe the laptop. `docker_root` is where the Docker graph lives (free space is measured
/// there when it can be stat'ed).
///
/// # Errors
/// An ssh failure, a non-absolute `$HOME`, or an unsafe `docker_root`.
pub fn probe(
    remote: &Remote<'_>,
    docker_root: &str,
    timeout: Duration,
) -> Result<RemoteFacts, OpError> {
    validate_remote_path(docker_root)?;
    let script = PROBE.replace("{ROOT}", &posix_quote(docker_root));
    let output = remote.must(&script, timeout, OpKind::DockerFailed)?;
    let facts = parse_probe(&output.stdout_text());
    validate_remote_path(&facts.home).map_err(|_| {
        OpError::new(
            OpKind::UnsafePath,
            format!("the laptop reported an unusable HOME {:?}", facts.home),
        )
    })?;
    Ok(facts)
}
