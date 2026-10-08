//! CLI turn spools shared by fresh launches and startup adoption.
use super::*;
use crate::store::provider_turn_custody::{NewProviderTurnCustody, ProviderTurnCursor};
use std::collections::VecDeque;
use std::sync::Mutex;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use uuid::Uuid;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) enum TurnOutput {
    Claude,
    Codex {
        boundary: crate::codex::CodexTranscriptBoundary,
    },
    Antigravity {
        conversation_cache_path: PathBuf,
        working_dir: Option<PathBuf>,
        pre_snapshot: Option<String>,
        capture: bool,
    },
}

/// Only non-secret decoding state is durable. Credentials and transport
/// authority remain in the inherited environment, never in spool metadata.
pub(crate) fn available_shim(
    config: &LaunchConfig,
    runtime: Option<&crate::config::RuntimeConfig>,
    override_path: Option<&Path>,
) -> Option<PathBuf> {
    if config.rsi_session_id.is_none()
        || !runtime.is_some_and(|runtime| {
            runtime
                .turn_detach_enabled
                .load(std::sync::atomic::Ordering::Relaxed)
        })
    {
        return None;
    }
    let available = (|| -> Result<PathBuf> {
        let shim = match override_path {
            Some(path) => path.to_path_buf(),
            None => std::env::current_exe()?
                .parent()
                .ok_or_else(|| DaemonError::Process("daemon binary has no parent".into()))?
                .join("rsi-turn-shim"),
        };
        if !std::fs::metadata(&shim)?.is_file() {
            return Err(DaemonError::Process(
                "turn shim is not a regular file".into(),
            ));
        }
        #[cfg(unix)]
        nix::unistd::access(&shim, nix::unistd::AccessFlags::X_OK)
            .map_err(|error| std::io::Error::from_raw_os_error(error as i32))?;
        Ok(shim)
    })();
    match available {
        Ok(shim) => Some(shim),
        Err(error) => {
            // Keep the existing Claude diagnostic stable for operator logs.
            tracing::warn!(%error, "rsi-turn-shim is missing or not executable; falling back to direct Claude pipes");
            None
        }
    }
}

pub(crate) fn launch(
    command: &Command,
    config: &LaunchConfig,
    execution: CliExecutionCapability,
    route: RuntimeExecutionRoute,
    shim: &Path,
    output: TurnOutput,
    stdin: Option<&[u8]>,
) -> Result<(CliTurnProcess, mpsc::Receiver<StreamEvent>)> {
    #[cfg(not(unix))]
    return Err(DaemonError::Process(
        "detached CLI turns are unsupported on this platform".into(),
    ));
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        let invocation = execution.invocation_id();
        let state = config
            .rsi_socket
            .as_ref()
            .and_then(|path| path.parent())
            .ok_or_else(|| {
                DaemonError::Process("detached turn requires the daemon socket directory".into())
            })?;
        let turns = state.join("turns");
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&turns)?;
        let spool = turns.join(invocation.to_string());
        std::fs::DirBuilder::new().mode(0o700).create(&spool)?;
        let write_private = |name: &str, bytes: &[u8]| -> Result<()> {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(spool.join(name))?;
            file.write_all(bytes)?;
            Ok(())
        };
        write_private("output.json", &serde_json::to_vec(&output)?)?;
        if let Some(stdin) = stdin {
            write_private("stdin", stdin)?;
        }
        let mut wrapped = wrap_command(command, shim, &spool);
        if stdin.is_some() {
            // wrap_command preserves literal argv; put shim options before --.
            let original = wrapped;
            wrapped = Command::new(shim);
            wrapped.arg("--stdin-file").arg(spool.join("stdin"));
            wrapped.args(original.as_std().get_args());
            if let Some(dir) = original.as_std().get_current_dir() {
                wrapped.current_dir(dir);
            }
            for (key, value) in original.as_std().get_envs() {
                match value {
                    Some(v) => {
                        wrapped.env(key, v);
                    }
                    None => {
                        wrapped.env_remove(key);
                    }
                }
            }
        }
        let mut wrapped =
            crate::process_scope::ScopedWorkerCommand::wrap_unspawned(&wrapped, invocation)?;
        wrapped
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        configure_tokio_process_group(&mut wrapped, ProcessContainment::Group)?;
        let child = execution.bind_command(route, wrapped).spawn()?;
        let mut turn = DetachedTurn::new(invocation, spool);
        turn.output = output;
        let (tx, rx) = mpsc::channel(100);
        turn.spawn_reader(tx);
        Ok((CliTurnProcess::detached_child(child, turn), rx))
    }
}

#[derive(Clone)]
pub(crate) struct DetachedTurn {
    output: TurnOutput,
    auxiliary: Arc<Mutex<VecDeque<Vec<StreamEvent>>>>,
    pub invocation_id: Uuid,
    pub spool_dir: PathBuf,
    receipts: Arc<Mutex<VecDeque<Option<ProviderTurnCursor>>>>,
    start: Arc<Mutex<Option<std::result::Result<Uuid, String>>>>,
    ready: Arc<tokio::sync::Notify>,
    identity: Arc<Mutex<Option<(u32, Option<u64>)>>>,
    pub complete: Arc<std::sync::atomic::AtomicBool>,
    handed_off: Arc<std::sync::atomic::AtomicBool>,
    handoff: Arc<tokio::sync::Notify>,
    pub(crate) adopted: Option<crate::store::provider_turn_custody::ProviderTurnCustody>,
}

impl DetachedTurn {
    pub fn new(invocation_id: Uuid, spool_dir: PathBuf) -> Self {
        Self {
            output: TurnOutput::Claude,
            auxiliary: Arc::default(),
            invocation_id,
            spool_dir,
            receipts: Arc::default(),
            start: Arc::default(),
            ready: Arc::default(),
            identity: Arc::default(),
            complete: Arc::default(),
            handed_off: Arc::default(),
            handoff: Arc::default(),
            adopted: None,
        }
    }

    pub(crate) fn adopt(row: crate::store::provider_turn_custody::ProviderTurnCustody) -> Self {
        let mut turn = Self::new(row.invocation_id, row.spool_dir.clone());
        *turn.identity.lock().unwrap() = Some((row.pid, row.start_time));
        turn.adopted = Some(row);
        turn
    }

    pub(crate) fn adopt_with_output(
        row: crate::store::provider_turn_custody::ProviderTurnCustody,
        provider: rsi_common::SessionProvider,
    ) -> Result<Self> {
        let mut turn = Self::adopt(row);
        turn.output = match std::fs::File::open(turn.spool_dir.join("output.json")) {
            Ok(file) => serde_json::from_reader(std::io::Read::take(file, 65537))?,
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    && provider == rsi_common::SessionProvider::Claude =>
            {
                TurnOutput::Claude
            }
            Err(error) => return Err(error.into()),
        };
        if !matches!(
            (&turn.output, provider),
            (TurnOutput::Claude, rsi_common::SessionProvider::Claude)
                | (TurnOutput::Codex { .. }, rsi_common::SessionProvider::Codex)
                | (
                    TurnOutput::Antigravity { .. },
                    rsi_common::SessionProvider::Antigravity
                )
        ) {
            return Err(DaemonError::Process(
                "detached turn decoder does not match provider".into(),
            ));
        }
        Ok(turn)
    }

    pub(crate) fn next_auxiliary(&self) -> Vec<StreamEvent> {
        self.auxiliary
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_default()
    }

    pub(crate) fn pid(&self) -> Option<u32> {
        self.identity.lock().unwrap().map(|(pid, _)| pid)
    }

    #[cfg(not(unix))]
    pub(crate) fn exit_status(&self) -> Result<Option<std::process::ExitStatus>> {
        Err(DaemonError::Process(
            "detached turn adoption is unsupported on this platform".into(),
        ))
    }

    #[cfg(unix)]
    pub(crate) fn exit_status(&self) -> Result<Option<std::process::ExitStatus>> {
        use std::os::unix::process::ExitStatusExt;
        if lock_held(&self.spool_dir)? {
            return Ok(None);
        }
        let file = match std::fs::File::open(self.spool_dir.join("exit.json")) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Some(std::process::ExitStatus::from_raw(1 << 8)));
            }
            other => other?,
        };
        // The reader reports corrupt exit records as terminal output errors.
        // Once the lock is released, a malformed record must still settle the
        // process rather than masquerading as a live shim.
        let record: Value = match serde_json::from_reader(std::io::Read::take(file, 4097)) {
            Ok(record) => record,
            Err(_) => return Ok(Some(std::process::ExitStatus::from_raw(1 << 8))),
        };
        let raw = if let Some(code) = record.get("exit_code").and_then(Value::as_i64) {
            i32::try_from(code)
                .ok()
                .filter(|code| (0..=255).contains(code))
                .unwrap_or(1)
                << 8
        } else if let Some(signal) = record.get("signal").and_then(Value::as_i64) {
            i32::try_from(signal)
                .ok()
                .filter(|signal| (1..=127).contains(signal))
                .unwrap_or(1 << 8)
        } else {
            1 << 8
        };
        Ok(Some(std::process::ExitStatus::from_raw(raw)))
    }

    /// Read terminal evidence already committed before the handoff. Do not
    /// replay events or accounting; use the same bounded framing as the tail.
    pub(crate) async fn consumed_terminal_event(&self) -> Result<Option<StreamEvent>> {
        let Some(row) = &self.adopted else {
            return Ok(None);
        };
        if row.stdout_offset == 0 {
            return Ok(None);
        }
        if matches!(self.output, TurnOutput::Antigravity { .. }) {
            return Ok(Some(agy_result()));
        }
        let mut file = tokio::fs::File::open(self.spool_dir.join("stdout"))
            .await?
            .take(row.stdout_offset);
        let mut tail = LineTail::default();
        let mut last = None;
        let mut thread_id = None;
        let mut bytes = [0u8; 65536];
        loop {
            let count = file.read(&mut bytes).await?;
            if count == 0 {
                break;
            }
            for mut frame in tail.push(&bytes[..count]) {
                if matches!(self.output, TurnOutput::Codex { .. }) {
                    frame.event = decode_codex(frame.event, &mut thread_id);
                }
                if frame.event.event_type == "result"
                    || (frame.event.event_type == "process_error"
                        && frame.event.data.get("terminal").and_then(Value::as_bool) == Some(true))
                {
                    last = Some(frame.event);
                }
            }
        }
        if tail.offset != row.stdout_offset || tail.has_partial() {
            return Err(DaemonError::Process(
                "turn custody cursor is not a complete stdout boundary".into(),
            ));
        }
        Ok(last)
    }

    pub fn leave_running(&self) {
        self.handed_off
            .store(true, std::sync::atomic::Ordering::Release);
        self.handoff.notify_waiters();
    }

    pub fn is_handed_off(&self) -> bool {
        self.handed_off.load(std::sync::atomic::Ordering::Acquire)
    }

    pub async fn wait_for_handoff(&self) {
        loop {
            let notified = self.handoff.notified();
            // Register before checking, so notify_waiters cannot be lost.
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_handed_off() {
                return;
            }
            notified.await;
        }
    }

    pub fn start(&self, result: std::result::Result<Uuid, String>) {
        *self.start.lock().unwrap() = Some(result);
        self.ready.notify_one();
    }

    pub fn next_receipt(&self) -> Option<ProviderTurnCursor> {
        self.receipts.lock().unwrap().pop_front().flatten()
    }

    pub async fn custody(&self, session_id: Uuid, boot_id: Uuid) -> Result<NewProviderTurnCustody> {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if let Some(pid) = self.shim_pid()? {
                let mut identity = self.identity.lock().unwrap();
                let start_time = match *identity {
                    Some((known_pid, start_time)) if known_pid == pid => start_time,
                    Some(_) => {
                        return Err(DaemonError::Process("turn shim identity changed".into()));
                    }
                    None => {
                        let start_time = pid_start_time(pid);
                        *identity = Some((pid, start_time));
                        start_time
                    }
                };
                return Ok(NewProviderTurnCustody {
                    invocation_id: self.invocation_id,
                    session_id,
                    spool_dir: self.spool_dir.clone(),
                    pid,
                    start_time,
                    boot_id,
                });
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(DaemonError::Process(
                    "turn shim did not publish its identity".into(),
                ));
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    fn shim_pid(&self) -> Result<Option<u32>> {
        let path = self.spool_dir.join("shim.json");
        let mut file = match std::fs::File::open(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            other => other?,
        };
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut std::io::Read::take(&mut file, 1025), &mut bytes)?;
        if bytes.len() > 1024 {
            return Err(DaemonError::Process(
                "turn shim identity is oversized".into(),
            ));
        }
        let data: Value = serde_json::from_slice(&bytes)?;
        let pid = data
            .get("pid")
            .and_then(Value::as_u64)
            .filter(|pid| *pid > 0 && *pid <= i32::MAX as u64)
            .ok_or_else(|| DaemonError::Process("invalid turn shim PID".into()))?;
        Ok(Some(pid as u32))
    }

    pub fn signal(&self, signal: nix::sys::signal::Signal) -> Result<()> {
        if self.is_handed_off() {
            return Ok(());
        }
        if self.identity.lock().unwrap().is_none() {
            return Err(DaemonError::Process(
                "turn_shim_identity_pending: cannot signal before the shim publishes its identity"
                    .into(),
            ));
        }
        if let Some(row) = &self.adopted {
            if !row.is_adoptable()? {
                if !lock_held(&self.spool_dir)? {
                    return Ok(());
                }
                return Err(DaemonError::Process(
                    "adopted turn identity cannot be proven".into(),
                ));
            }
        }
        // Fresh-launch custody only: a unique private spool cannot be reused.
        // The held lock and PID start time fence signaling after the shim exits.
        if !lock_held(&self.spool_dir)? {
            return Ok(());
        }
        let identity = *self.identity.lock().unwrap();
        if let Some((pid, start_time)) = identity {
            if self.shim_pid()? != Some(pid) {
                return Err(DaemonError::Process("turn shim identity changed".into()));
            }
            #[cfg(target_os = "linux")]
            if start_time.is_none() || pid_start_time(pid) != start_time {
                return Err(DaemonError::Process(
                    "turn shim PID identity cannot be proven".into(),
                ));
            }
            #[cfg(not(target_os = "linux"))]
            let _ = start_time;
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), signal)
                .map_err(|e| DaemonError::Process(format!("turn shim signal failed: {e}")))?;
        }
        Ok(())
    }

    async fn send(
        &self,
        tx: &mpsc::Sender<StreamEvent>,
        event: StreamEvent,
        receipt: Option<ProviderTurnCursor>,
    ) -> bool {
        self.send_with_auxiliary(tx, event, receipt, Vec::new())
            .await
    }

    async fn send_with_auxiliary(
        &self,
        tx: &mpsc::Sender<StreamEvent>,
        event: StreamEvent,
        receipt: Option<ProviderTurnCursor>,
        auxiliary: Vec<StreamEvent>,
    ) -> bool {
        self.receipts.lock().unwrap().push_back(receipt);
        self.auxiliary.lock().unwrap().push_back(auxiliary);
        tx.send(event).await.is_ok()
    }

    pub fn spawn_reader(&self, tx: mpsc::Sender<StreamEvent>) {
        let turn = self.clone();
        tokio::spawn(async move {
            let boot = loop {
                let notified = turn.ready.notified();
                let result = turn.start.lock().unwrap().clone();
                if let Some(result) = result {
                    break result;
                }
                tokio::select! { _ = notified => {}, _ = tx.closed() => return }
            };
            let result = tokio::select! {
                biased;
                _ = turn.wait_for_handoff() => return,
                result = async { match boot {
                Ok(boot) => turn.read(&tx, boot).await,
                Err(error) => Err(DaemonError::Process(error)),
            } } => result,
            };
            if let Err(error) = result {
                if turn.is_handed_off() {
                    return;
                }
                let _ = turn.signal(nix::sys::signal::Signal::SIGTERM);
                turn.send(&tx, terminal_error(error.to_string()), None)
                    .await;
            }
        });
    }

    async fn read(&self, tx: &mpsc::Sender<StreamEvent>, boot_id: Uuid) -> Result<()> {
        if matches!(self.output, TurnOutput::Antigravity { .. }) {
            return self.read_agy(tx, boot_id).await;
        }
        let offset = self.adopted.as_ref().map_or(0, |row| row.stdout_offset);
        let mut codex = if let TurnOutput::Codex { boundary } = &self.output {
            Some(crate::codex::DetachedCodexDecoder::new(boundary.clone()))
        } else {
            None
        };
        // Rebuild only decoding state (thread id), without sending consumed stdout.
        if offset > 0
            && let Some(decoder) = &mut codex
        {
            let mut file = tokio::fs::File::open(self.spool_dir.join("stdout"))
                .await?
                .take(offset);
            let mut replay = LineTail::default();
            let mut bytes = [0u8; 65536];
            loop {
                let count = file.read(&mut bytes).await?;
                if count == 0 {
                    break;
                }
                for frame in replay.push(&bytes[..count]) {
                    decoder.rehydrate(&frame.event);
                }
            }
            if replay.offset != offset || replay.has_partial() {
                return Err(DaemonError::Process(
                    "turn custody cursor is not a complete stdout boundary".into(),
                ));
            }
        }
        let mut tail = LineTail {
            offset,
            start: offset,
            ..LineTail::default()
        };
        let mut position = offset;
        loop {
            let done = self.spool_dir.join("exit.json").exists();
            match tokio::fs::File::open(self.spool_dir.join("stdout")).await {
                Ok(mut file) => {
                    if file.metadata().await?.len() < position {
                        return Err(DaemonError::Process("turn stdout spool shrank".into()));
                    }
                    file.seek(std::io::SeekFrom::Start(position)).await?;
                    let mut bytes = [0u8; 65536];
                    loop {
                        let count = file.read(&mut bytes).await?;
                        if count == 0 {
                            break;
                        }
                        position += count as u64;
                        for mut frame in tail.push(&bytes[..count]) {
                            let auxiliary = if let Some(decoder) = &mut codex {
                                let (event, auxiliary, context) = decoder.decode(frame.event).await;
                                frame.event = event;
                                if let Some(context) = context {
                                    self.send(tx, context, None).await;
                                }
                                auxiliary
                            } else {
                                Vec::new()
                            };
                            let receipt = ProviderTurnCursor {
                                invocation_id: self.invocation_id,
                                boot_id,
                                expected_offset: frame.start,
                                next_offset: frame.end,
                            };
                            let terminal = frame.event.event_type == "process_error"
                                && frame.event.data.get("terminal").and_then(Value::as_bool)
                                    == Some(true);
                            if !self
                                .send_with_auxiliary(tx, frame.event, Some(receipt), auxiliary)
                                .await
                            {
                                return Ok(());
                            }
                            if terminal {
                                let _ = self.signal(nix::sys::signal::Signal::SIGTERM);
                                return Ok(());
                            }
                        }
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound && !done => {}
                Err(error) => return Err(error.into()),
            }
            if done {
                if tail.has_partial() {
                    return Err(DaemonError::Process(
                        "turn ended with an incomplete stdout line".into(),
                    ));
                }
                let file = tokio::fs::File::open(self.spool_dir.join("exit.json")).await?;
                let mut record = Vec::new();
                file.take(4097).read_to_end(&mut record).await?;
                if record.len() > 4096 {
                    return Err(DaemonError::Process("turn exit record is oversized".into()));
                }
                let record: Value = serde_json::from_slice(&record)?;
                let completion_error = record.get("error").and_then(Value::as_str);
                if completion_error.is_none()
                    && record.get("exit_code").and_then(Value::as_i64).is_none()
                    && record.get("signal").and_then(Value::as_i64).is_none()
                {
                    return Err(DaemonError::Process(
                        "turn shim exit record has no outcome".into(),
                    ));
                }
                if let Ok(file) = tokio::fs::File::open(self.spool_dir.join("stderr")).await {
                    let mut bytes = Vec::new();
                    file.take(PROVIDER_MAX_STDERR_BYTES as u64 + 1)
                        .read_to_end(&mut bytes)
                        .await?;
                    if bytes.len() > PROVIDER_MAX_STDERR_BYTES {
                        return Err(DaemonError::Process(
                            "provider stderr exceeded its bound".into(),
                        ));
                    }
                    let text = String::from_utf8(bytes)
                        .map_err(|e| DaemonError::Process(e.to_string()))?;
                    let text = text
                        .lines()
                        .filter(|s| !s.trim().is_empty())
                        .collect::<Vec<_>>()
                        .join("\n");
                    if let Some(decoder) = &mut codex {
                        for event in decoder.stderr(&text) {
                            self.send(tx, event, None).await;
                        }
                    } else if !text.is_empty() {
                        self.send(
                            tx,
                            StreamEvent {
                                event_type: "process_error".into(),
                                data: serde_json::json!({"error": text, "source": "stderr"}),
                            },
                            None,
                        )
                        .await;
                    }
                }
                if let Some(error) = completion_error {
                    if !self.send(tx, terminal_error(error.to_string()), None).await {
                        return Ok(());
                    }
                }
                self.complete
                    .store(true, std::sync::atomic::Ordering::Release);
                return Ok(());
            }
            if let Some(decoder) = &mut codex {
                if let Some(event) = decoder.poll_context().await {
                    self.send(tx, event, None).await;
                }
            }
            // Missing lock before identity creation is a launch in progress.
            // After identity exists, unlocked-without-exit means abandonment.
            if self.spool_dir.join("shim.json").exists() && !lock_held(&self.spool_dir)? {
                if self.spool_dir.join("exit.json").exists() {
                    continue;
                }
                return Err(DaemonError::Process(
                    "turn shim released its lock without an exit record".into(),
                ));
            }
            if tx.is_closed() {
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }
    async fn read_agy(&self, tx: &mpsc::Sender<StreamEvent>, boot: Uuid) -> Result<()> {
        use crate::process_control::AGY_MAX_TURN_BYTES;
        loop {
            let size = std::fs::metadata(self.spool_dir.join("stdout"))
                .map(|m| m.len())
                .unwrap_or(0);
            if size > AGY_MAX_TURN_BYTES as u64 {
                return Err(DaemonError::Process(
                    "provider turn output exceeded its bound".into(),
                ));
            }
            if !lock_held(&self.spool_dir)? && self.spool_dir.join("exit.json").exists() {
                break;
            }
            if self.spool_dir.join("shim.json").exists() && !lock_held(&self.spool_dir)? {
                if self.spool_dir.join("exit.json").exists() {
                    continue;
                }
                return Err(DaemonError::Process(
                    "turn shim released its lock without an exit record".into(),
                ));
            }
            if tx.is_closed() {
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        // Validate durable completion before publishing provider output.
        let status = self
            .exit_status()?
            .ok_or_else(|| DaemonError::Process("turn completion is still live".into()))?;
        let offset = self.adopted.as_ref().map_or(0, |r| r.stdout_offset);
        let mut file = tokio::fs::File::open(self.spool_dir.join("stdout")).await?;
        let mut bytes = Vec::new();
        (&mut file)
            .take(AGY_MAX_TURN_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .await?;
        if bytes.len() > AGY_MAX_TURN_BYTES {
            return Err(DaemonError::Process(
                "provider turn output exceeded its bound".into(),
            ));
        }
        let len = bytes.len() as u64;
        let output = String::from_utf8(bytes).map_err(|e| DaemonError::Process(e.to_string()))?;
        if offset != 0 && offset != len {
            return Err(DaemonError::Process("invalid coalesced turn cursor".into()));
        }
        let mut result = agy_result();
        if let TurnOutput::Antigravity {
            conversation_cache_path,
            working_dir: Some(dir),
            pre_snapshot,
            capture: true,
        } = &self.output
        {
            for _ in 0..15 {
                if let Some(id) =
                    crate::agy::read_last_conversation_for_dir(conversation_cache_path, dir)
                    && pre_snapshot.as_deref() != Some(&id)
                {
                    result.data["session_id"] = Value::String(id);
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
        if offset == 0 && len > 0 {
            // Match the direct reader's line bound and one coalesced message,
            // including a final plain-text line without a newline.
            let text = output
                .lines()
                .map(|line| {
                    if line.len() > PROVIDER_MAX_LINE_BYTES {
                        format!(
                            "[provider stdout line truncated at {} bytes]",
                            PROVIDER_MAX_LINE_BYTES
                        )
                    } else {
                        line.to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            let mut event =
                crate::agy::assistant_event_from_output(&text).unwrap_or_else(checkpoint);
            if let Some(id) = result.data.get("session_id") {
                event.data["session_id"] = id.clone();
            }
            if !self
                .send(
                    tx,
                    event,
                    Some(ProviderTurnCursor {
                        invocation_id: self.invocation_id,
                        boot_id: boot,
                        expected_offset: 0,
                        next_offset: len,
                    }),
                )
                .await
            {
                return Ok(());
            }
        }
        if !status.success() {
            self.send(
                tx,
                terminal_error(format!("Antigravity exited with {status}")),
                None,
            )
            .await;
        } else {
            self.send(tx, result, None).await;
        }
        self.complete
            .store(true, std::sync::atomic::Ordering::Release);
        Ok(())
    }
}

fn checkpoint() -> StreamEvent {
    StreamEvent {
        event_type: "turn_spool_checkpoint".into(),
        data: serde_json::json!({}),
    }
}

pub(crate) fn decode_codex(event: StreamEvent, thread: &mut Option<String>) -> StreamEvent {
    if matches!(
        event.event_type.as_str(),
        "parse_error" | "process_error" | "turn_spool_checkpoint"
    ) {
        return event;
    }
    let mut raw = event.data;
    raw["type"] = Value::String(event.event_type);
    crate::codex::map_codex_json_to_stream_event(&raw, thread).unwrap_or_else(checkpoint)
}

fn agy_result() -> StreamEvent {
    StreamEvent {
        event_type: "result".into(),
        data: serde_json::json!({
            "result": "success", "subtype": "turn_completed", "duration_ms": 0,
            "usage": {"input_tokens":0,"output_tokens":0,"cache_read_input_tokens":0}
        }),
    }
}

fn terminal_error(error: String) -> StreamEvent {
    StreamEvent {
        event_type: "process_error".into(),
        data: serde_json::json!({"error": error, "source": "stdout", "terminal": true, "error_class": "provider_output_read_error"}),
    }
}

pub(crate) fn lock_held(dir: &Path) -> Result<bool> {
    let file = match std::fs::OpenOptions::new()
        .write(true)
        .open(dir.join("alive.lock"))
    {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        other => other?,
    };
    match file.try_lock() {
        Ok(()) => Ok(false),
        Err(std::fs::TryLockError::WouldBlock) => Ok(true),
        Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
    }
}

#[cfg(target_os = "linux")]
fn pid_start_time(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(") ")?
        .1
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()
}

#[cfg(not(target_os = "linux"))]
fn pid_start_time(_pid: u32) -> Option<u64> {
    None
}

struct Frame {
    start: u64,
    end: u64,
    event: StreamEvent,
}

#[derive(Default)]
struct LineTail {
    offset: u64,
    start: u64,
    buffer: Vec<u8>,
    oversized: bool,
}

impl LineTail {
    fn has_partial(&self) -> bool {
        self.offset != self.start
    }
    fn push(&mut self, bytes: &[u8]) -> Vec<Frame> {
        let mut frames = Vec::new();
        for byte in bytes {
            self.offset += 1;
            if *byte != b'\n' {
                if self.buffer.len() < PROVIDER_MAX_LINE_BYTES {
                    self.buffer.push(*byte);
                } else {
                    self.oversized = true;
                }
                continue;
            }
            let event = if self.oversized {
                crate::provider::stdout_line_truncated_event(PROVIDER_MAX_LINE_BYTES)
            } else {
                let line = std::str::from_utf8(&self.buffer);
                if let Err(error) = line {
                    terminal_error(error.to_string())
                } else if line.unwrap().trim().is_empty() {
                    StreamEvent {
                        event_type: "turn_spool_checkpoint".into(),
                        data: serde_json::json!({}),
                    }
                } else {
                    let line = line.unwrap().trim_end_matches('\r');
                    serde_json::from_str::<StreamEvent>(line).unwrap_or_else(|error| StreamEvent {
                        event_type: "parse_error".into(),
                        data: serde_json::json!({"error": error.to_string(), "raw_line": line}),
                    })
                }
            };
            frames.push(Frame {
                start: self.start,
                end: self.offset,
                event,
            });
            self.start = self.offset;
            self.buffer.clear();
            self.oversized = false;
        }
        frames
    }
}

pub(super) fn wrap_command(inner: &Command, shim: &Path, spool_dir: &Path) -> Command {
    let source = inner.as_std();
    let mut command = Command::new(shim);
    command
        .arg("--spool-dir")
        .arg(spool_dir)
        .arg("--")
        .arg(source.get_program())
        .args(source.get_args());
    if let Some(dir) = source.get_current_dir() {
        command.current_dir(dir);
    }
    for (key, value) in source.get_envs() {
        match value {
            Some(value) => {
                command.env(key, value);
            }
            None => {
                command.env_remove(key);
            }
        }
    }
    command
}

#[cfg(test)]
mod tests;
