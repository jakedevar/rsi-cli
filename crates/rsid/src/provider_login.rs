//! Provider CLI login probes (#2753): is `claude` / `codex` logged in on this
//! host, before a session is spent on a launch that would fail.
//!
//! Each probe runs the CLI's own status command (`claude auth status --json`,
//! `codex login status`) and keeps three facts: the [`ProviderLoginState`], the
//! closed [`ProviderLoginMethod`] and the check time. The CLI's output (account
//! e-mail, organisation, directory) is reduced to an opaque account fingerprint,
//! used to invalidate a previous account’s plan windows; raw output is never stored,
//! logged or served; no auth file is ever opened.
//!
//! A probe runs in the environment a launch gives the same CLI (the launch
//! credential scrub, #2810) and owns its whole process tree: the group is
//! killed at the deadline, on cancellation and once the command exits (#2812).
//!
//! `unknown` fails open: a missing CLI, a timeout or unreadable output never
//! refuses a launch. Only a definite `logged_out` does, with the typed
//! `provider_login_required` code and the host-side fix.
//!
//! Results are cached for [`PROVIDER_LOGIN_CACHE_TTL_SECS`]; one probe per
//! provider is in flight at a time and concurrent callers share its result.
//! Probes run at daemon start, from the manager coordinator's backstop pass,
//! after a provider auth stop reason, and on operator refresh.

use chrono::{DateTime, Utc};
use rsi_common::provider_login::{
    PROVIDER_LOGIN_CACHE_TTL_SECS, ProviderLoginHealthV1, ProviderLoginMethod,
    ProviderLoginRequestV1, ProviderLoginRowV1, ProviderLoginState, ProviderLoginStateResultV1,
    ProviderLoginV1, login_remedy, login_required_message,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;

/// How long one probe may run before it reads as `unknown`.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// Most bytes of one stream a probe reads.
const MAX_PROBE_OUTPUT_BYTES: u64 = 64 * 1024;
/// How long a status read waits for a stale probe before serving the cache.
const STATUS_READ_WAIT: Duration = Duration::from_secs(5);

/// A provider CLI whose login is probed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LoginProvider {
    Claude,
    Codex,
}

impl LoginProvider {
    pub const ALL: [Self; 2] = [Self::Claude, Self::Codex];

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            _ => None,
        }
    }

    const fn index(self) -> usize {
        match self {
            Self::Claude => 0,
            Self::Codex => 1,
        }
    }

    /// The probed login behind a launch, or `None` for a provider that does
    /// not run on a CLI login. A Claude launch on a Bedrock Claude model runs
    /// on the injected Bedrock credential, not on the `claude` login.
    #[must_use]
    pub fn for_launch(
        provider: rsi_common::types::SessionProvider,
        model: Option<&str>,
    ) -> Option<Self> {
        use rsi_common::types::SessionProvider;
        match provider {
            SessionProvider::Claude => {
                let bedrock = model.and_then(crate::bedrock::bedrock_vendor)
                    == Some(crate::bedrock::BedrockVendor::Anthropic);
                (!bedrock).then_some(Self::Claude)
            }
            SessionProvider::Codex | SessionProvider::CodexAppServer => Some(Self::Codex),
            _ => None,
        }
    }
}

/// What one run of a status command produced. The output is held only to be
/// interpreted; its `Debug` never prints it.
#[derive(Clone)]
pub enum ProbeRun {
    Exited {
        code: Option<i32>,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    },
    TimedOut,
    /// The CLI could not be found or started.
    Unavailable,
    /// The daemon is offline (#130): no CLI was started; the login reads
    /// `unknown`, which never refuses a launch.
    Offline,
}

impl std::fmt::Debug for ProbeRun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exited {
                code,
                stdout,
                stderr,
            } => f
                .debug_struct("Exited")
                .field("code", code)
                .field("stdout_bytes", &stdout.len())
                .field("stderr_bytes", &stderr.len())
                .finish(),
            Self::TimedOut => f.write_str("TimedOut"),
            Self::Unavailable => f.write_str("Unavailable"),
            Self::Offline => f.write_str("Offline"),
        }
    }
}

/// Runs a provider CLI's status command; behind a trait so a test never
/// starts a real CLI.
#[async_trait::async_trait]
pub trait LoginProbeRunner: Send + Sync {
    async fn run(&self, provider: LoginProvider) -> ProbeRun;
}

/// Production runner: the CLI found through [`crate::provider_cli::resolve`].
pub struct CliLoginRunner {
    timeout: Duration,
    binaries: Option<HashMap<LoginProvider, PathBuf>>,
    /// Offline daemon (#130): no status command starts (`claude auth status`
    /// reaches the provider's API).
    offline: bool,
}

impl CliLoginRunner {
    #[must_use]
    pub fn new() -> Self {
        Self {
            timeout: PROBE_TIMEOUT,
            binaries: None,
            offline: crate::offline::is_offline(),
        }
    }

    /// A runner that starts exactly these binaries (a stub CLI in a test).
    #[must_use]
    pub fn with_binaries(binaries: HashMap<LoginProvider, PathBuf>, timeout: Duration) -> Self {
        Self {
            timeout,
            binaries: Some(binaries),
            offline: false,
        }
    }

    /// This runner with the offline mode (#130) set explicitly.
    #[must_use]
    pub const fn with_offline(mut self, offline: bool) -> Self {
        self.offline = offline;
        self
    }

    fn binary(&self, provider: LoginProvider) -> Option<PathBuf> {
        match &self.binaries {
            Some(binaries) => binaries.get(&provider).cloned(),
            None => crate::provider_cli::resolve(provider.name()),
        }
    }
}

impl Default for CliLoginRunner {
    fn default() -> Self {
        Self::new()
    }
}

async fn read_bounded<R: tokio::io::AsyncRead + Unpin>(reader: Option<R>) -> Vec<u8> {
    let mut out = Vec::new();
    if let Some(reader) = reader {
        let _ = reader
            .take(MAX_PROBE_OUTPUT_BYTES)
            .read_to_end(&mut out)
            .await;
    }
    out
}

/// The status command for `provider`, in the environment a launch gives the
/// same CLI (#2810): every inherited provider credential variable is removed
/// with the launch scrub ([`crate::vault::scrub_credential_env`], as
/// `claude.rs` and the Codex launch apply it), while the login and config
/// directories (`HOME`, `CLAUDE_CONFIG_DIR`, `CODEX_HOME`) stay inherited. An
/// `ANTHROPIC_API_KEY` the daemon happens to hold therefore cannot make the
/// probe read `logged_in` for a launch that will not see that key.
fn probe_command(binary: &std::path::Path, provider: LoginProvider) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(binary);
    match provider {
        LoginProvider::Claude => command.args(["auth", "status", "--json"]),
        LoginProvider::Codex => command.args(["login", "status"]),
    };
    command.env(
        rsi_common::identity::ENV_PROCESS_OWNERSHIP_NAMESPACE,
        rsi_common::identity::process_ownership_namespace(),
    );
    crate::vault::scrub_credential_env(&mut command);
    command
}

/// How long the output pipes may stay open after the status command exits
/// before its group is stopped (a helper it left behind holding a pipe).
const PROBE_POST_EXIT_DRAIN: Duration = Duration::from_millis(250);
/// How long the pipes and the reap get once the group was stopped.
const PROBE_CLEANUP: Duration = Duration::from_secs(1);

/// Run one status command as the leader of its own process group and own the
/// whole group on every path (#2812):
///
/// * past `timeout` the group is killed and the run is `TimedOut`;
/// * once the leader exited (and its pipes reached EOF or had
///   [`PROBE_POST_EXIT_DRAIN`] to), the group is killed too: a status command
///   owns no process past its own exit, so a helper it forked never outlives
///   the probe;
/// * a dropped future (a caller's deadline, cancellation) kills the group
///   through [`crate::process_control::OwnedGroupLeader`]'s drop.
///
/// Every group signal is sent while the leader is unreaped, so its group id
/// cannot name another process group: Linux and macOS observe the exit with a
/// non-reaping `waitid` and reap only after the group was stopped. A platform
/// with no non-reaping wait reaps the leader at exit, so its success-path stop
/// is refused and logged by `OwnedGroupLeader::terminate_group` (a reaped
/// leader's id may already name another group) instead of signalled; only the
/// timeout and drop paths stop the group there.
async fn run_contained(mut command: tokio::process::Command, timeout: Duration) -> ProbeRun {
    use crate::process_control::{
        OwnedGroupLeader, ProcessContainment, configure_tokio_process_group,
    };
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    // No-escape where the platform has it, a plain new group elsewhere.
    if configure_tokio_process_group(&mut command, ProcessContainment::GroupNoEscape).is_err()
        && configure_tokio_process_group(&mut command, ProcessContainment::Group).is_err()
    {
        return ProbeRun::Unavailable;
    }
    let Ok(mut child) = command.spawn() else {
        return ProbeRun::Unavailable;
    };
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let Ok(mut leader) = OwnedGroupLeader::new(child) else {
        return ProbeRun::Unavailable;
    };
    let deadline = tokio::time::Instant::now() + timeout;
    let mut stdout_read = std::pin::pin!(read_bounded(stdout));
    let mut stderr_read = std::pin::pin!(read_bounded(stderr));
    let mut stdout_bytes: Option<Vec<u8>> = None;
    let mut stderr_bytes: Option<Vec<u8>> = None;
    let mut exit: Option<std::io::Result<()>> = None;
    let mut exited_at: Option<tokio::time::Instant> = None;
    let mut timed_out = false;
    loop {
        if exit.is_some() && stdout_bytes.is_some() && stderr_bytes.is_some() {
            break;
        }
        let wake = exited_at.map_or(deadline, |exited| {
            deadline.min(exited + PROBE_POST_EXIT_DRAIN)
        });
        if tokio::time::Instant::now() >= wake {
            timed_out = exit.is_none();
            break;
        }
        tokio::select! {
            biased;
            observed = leader.exited(), if exit.is_none() => {
                exit = Some(observed);
                exited_at = Some(tokio::time::Instant::now());
            }
            bytes = &mut stdout_read, if stdout_bytes.is_none() => stdout_bytes = Some(bytes),
            bytes = &mut stderr_read, if stderr_bytes.is_none() => stderr_bytes = Some(bytes),
            () = tokio::time::sleep_until(wake) => {}
        }
    }
    // Stop everything left in the group while the leader still pins its id.
    leader.terminate_group("provider login probe settled");
    if timed_out {
        let _ = tokio::time::timeout(PROBE_CLEANUP, leader.reap()).await;
        return ProbeRun::TimedOut;
    }
    // The group is gone, so any pipe a member held reaches EOF now; a process
    // that escaped the group cannot hold the probe past the cleanup bound.
    let rest = tokio::time::timeout(PROBE_CLEANUP, async {
        let stdout = match stdout_bytes {
            Some(bytes) => bytes,
            None => stdout_read.await,
        };
        let stderr = match stderr_bytes {
            Some(bytes) => bytes,
            None => stderr_read.await,
        };
        (stdout, stderr)
    })
    .await;
    let status = tokio::time::timeout(PROBE_CLEANUP, leader.reap()).await;
    match (exit, rest, status) {
        (Some(Ok(())), Ok((stdout, stderr)), Ok(Ok(status))) => ProbeRun::Exited {
            code: status.code(),
            stdout,
            stderr,
        },
        (Some(Ok(())), Err(_), _) => ProbeRun::TimedOut,
        _ => ProbeRun::Unavailable,
    }
}

#[async_trait::async_trait]
impl LoginProbeRunner for CliLoginRunner {
    async fn run(&self, provider: LoginProvider) -> ProbeRun {
        if self.offline {
            return ProbeRun::Offline;
        }
        let Some(binary) = self.binary(provider) else {
            return ProbeRun::Unavailable;
        };
        run_contained(probe_command(&binary, provider), self.timeout).await
    }
}

/// The runner a process without a real CLI uses (unit tests).
#[cfg(test)]
struct NoCliRunner;

#[cfg(test)]
#[async_trait::async_trait]
impl LoginProbeRunner for NoCliRunner {
    async fn run(&self, _provider: LoginProvider) -> ProbeRun {
        ProbeRun::Unavailable
    }
}

fn claude_json(stdout: &[u8]) -> Option<serde_json::Value> {
    let text = std::str::from_utf8(stdout).ok()?;
    serde_json::from_str(text.trim()).ok().or_else(|| {
        // Tolerate a banner line around the JSON object.
        let start = text.find('{')?;
        let end = text.rfind('}')?;
        serde_json::from_str(text.get(start..=end)?).ok()
    })
}

/// Stable identity only: plan type and login method are not account identity.
/// Raw identity is neither logged nor returned on any RPC surface.
pub(crate) fn account_fingerprint(account: &serde_json::Value) -> Option<String> {
    let identity = ["accountId", "accountUuid", "id", "email"]
        .into_iter()
        .find_map(|field| {
            account
                .get(field)
                .and_then(serde_json::Value::as_str)
                .filter(|s| !s.is_empty())
        })?;
    let organization = account
        .get("orgId")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    Some(
        blake3::hash(&serde_json::to_vec(&(identity, organization)).ok()?)
            .to_hex()
            .to_string(),
    )
}

fn lowercase_text(run_stdout: &[u8], run_stderr: &[u8]) -> String {
    let mut text = String::from_utf8_lossy(run_stdout).into_owned();
    text.push('\n');
    text.push_str(&String::from_utf8_lossy(run_stderr));
    text.to_ascii_lowercase()
}

/// Reduce one run to the three-valued state and the closed method. Everything
/// the CLI printed besides those two facts is dropped here.
#[must_use]
pub fn interpret(
    provider: LoginProvider,
    run: &ProbeRun,
) -> (ProviderLoginState, ProviderLoginMethod) {
    use ProviderLoginMethod as Method;
    use ProviderLoginState as State;
    let ProbeRun::Exited {
        code,
        stdout,
        stderr,
    } = run
    else {
        return (State::Unknown, Method::Unknown);
    };
    match provider {
        LoginProvider::Claude => {
            if let Some(json) = claude_json(stdout)
                && let Some(logged_in) = json.get("loggedIn").and_then(serde_json::Value::as_bool)
            {
                let method = json
                    .get("authMethod")
                    .and_then(serde_json::Value::as_str)
                    .map_or(Method::Unknown, Method::from_claude_auth_method);
                return if logged_in {
                    (State::LoggedIn, method)
                } else {
                    (State::LoggedOut, method)
                };
            }
            if *code == Some(1) && lowercase_text(stdout, stderr).contains("not logged in") {
                return (State::LoggedOut, Method::None);
            }
            (State::Unknown, Method::Unknown)
        }
        LoginProvider::Codex => {
            let text = lowercase_text(stdout, stderr);
            match code {
                Some(0) => {
                    let method = if text.contains("chatgpt") {
                        Method::Chatgpt
                    } else if text.contains("api key") {
                        Method::ApiKey
                    } else {
                        Method::Unknown
                    };
                    (State::LoggedIn, method)
                }
                // Exit 1 is "not logged in"; an exit 1 that printed something
                // else is some other failure and stays unknown.
                Some(1) if text.trim().is_empty() || text.contains("not logged in") => {
                    (State::LoggedOut, Method::None)
                }
                _ => (State::Unknown, Method::Unknown),
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Record {
    state: ProviderLoginState,
    method: ProviderLoginMethod,
    checked_at: DateTime<Utc>,
    probed: Instant,
}

#[derive(Debug, Default)]
struct Slot {
    record: Option<Record>,
    /// The latest definite state, kept across `unknown` probes so a timeout
    /// between two definite answers does not hide a `logged_in` -> `logged_out`
    /// transition.
    last_known: Option<ProviderLoginState>,
    /// When the state went from `logged_in` to `logged_out`; cleared once the
    /// provider is logged in again.
    logout_transition: Option<DateTime<Utc>>,
}

pub struct ProviderLoginService {
    runner: Arc<dyn LoginProbeRunner>,
    ttl: Duration,
    slots: Mutex<[Slot; 2]>,
    /// One probe in flight per provider.
    flights: [tokio::sync::Mutex<()>; 2],
    probes_started: std::sync::atomic::AtomicUsize,
    billing_store:
        Mutex<Option<std::sync::Weak<rsid_core::store_lock::Mutex<crate::store::Store>>>>,
    rate_limit_store:
        Mutex<Option<std::sync::Weak<rsid_core::store_lock::Mutex<crate::store::Store>>>>,
    rate_limit_bus: Mutex<Option<std::sync::Weak<crate::bus::EventBus>>>,
}

impl ProviderLoginService {
    #[must_use]
    pub fn new(runner: Arc<dyn LoginProbeRunner>, ttl: Duration) -> Self {
        Self {
            runner,
            ttl,
            billing_store: Mutex::new(None),
            slots: Mutex::new([Slot::default(), Slot::default()]),
            flights: [tokio::sync::Mutex::new(()), tokio::sync::Mutex::new(())],
            probes_started: std::sync::atomic::AtomicUsize::new(0),
            rate_limit_store: Mutex::new(None),
            rate_limit_bus: Mutex::new(None),
        }
    }

    /// Process-wide production instance (real CLIs, 60 s TTL).
    pub fn global() -> &'static Arc<Self> {
        static SERVICE: OnceLock<Arc<ProviderLoginService>> = OnceLock::new();
        SERVICE.get_or_init(|| {
            #[cfg(test)]
            let runner: Arc<dyn LoginProbeRunner> = Arc::new(NoCliRunner);
            #[cfg(not(test))]
            let runner: Arc<dyn LoginProbeRunner> = Arc::new(CliLoginRunner::new());
            Arc::new(Self::new(
                runner,
                Duration::from_secs(PROVIDER_LOGIN_CACHE_TTL_SECS),
            ))
        })
    }

    /// Bind account-switch observations to the manager's telemetry store.
    #[must_use]
    pub(crate) fn for_manager_with_rate_limits(
        store: &Arc<rsid_core::store_lock::Mutex<crate::store::Store>>,
        bus: &Arc<crate::bus::EventBus>,
    ) -> Arc<Self> {
        let service = Self::for_manager();
        service.attach_rate_limit_store(store);
        *service
            .rate_limit_bus
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::downgrade(bus));
        service
    }

    /// The process-wide service, or a private service in unit tests so a
    /// seeded state never leaks between tests.
    #[must_use]
    pub fn for_manager() -> Arc<Self> {
        #[cfg(test)]
        {
            Arc::new(Self::new(
                Arc::new(NoCliRunner),
                Duration::from_secs(PROVIDER_LOGIN_CACHE_TTL_SECS),
            ))
        }
        #[cfg(not(test))]
        {
            Arc::clone(Self::global())
        }
    }

    /// Publish only the closed login method for billing classification.
    pub fn set_billing_store(
        &self,
        store: &Arc<rsid_core::store_lock::Mutex<crate::store::Store>>,
    ) {
        *self
            .billing_store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::downgrade(store));
    }

    pub(crate) fn attach_rate_limit_store(
        &self,
        store: &Arc<rsid_core::store_lock::Mutex<crate::store::Store>>,
    ) {
        *self
            .rate_limit_store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::downgrade(store));
    }

    async fn observe_account(&self, provider: LoginProvider, run: &ProbeRun) {
        if provider != LoginProvider::Claude
            || interpret(provider, run).0 != ProviderLoginState::LoggedIn
        {
            return;
        }
        let ProbeRun::Exited { stdout, .. } = run else {
            return;
        };
        let Some(account) = claude_json(stdout) else {
            return;
        };
        let Some(fingerprint) = account_fingerprint(&account) else {
            return;
        };
        let store = self
            .rate_limit_store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .and_then(std::sync::Weak::upgrade);
        if let Some(store) = store {
            let store = store.lock().await;
            let now = Utc::now();
            let provider = rsi_common::types::SessionProvider::Claude;
            match store.observe_provider_rate_limit_account(provider, &fingerprint, now) {
                Ok(true) => {
                    let bus = self
                        .rate_limit_bus
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .as_ref()
                        .and_then(std::sync::Weak::upgrade);
                    if let Some(bus) = bus {
                        bus.publish(crate::bus::DaemonEvent::ProviderRateLimitUpdated {
                            snapshot: rsi_common::rpc::ProviderRateLimitSnapshot::invalidated(
                                provider, now,
                            ),
                        });
                    }
                }
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(%error, "could not reconcile provider plan-window account")
                }
            }
        }
    }

    /// How many probes ran (one per actual CLI start).
    #[must_use]
    pub fn probes_started(&self) -> usize {
        self.probes_started
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    #[must_use]
    pub const fn ttl(&self) -> Duration {
        self.ttl
    }

    fn slots(&self) -> std::sync::MutexGuard<'_, [Slot; 2]> {
        self.slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn record(&self, provider: LoginProvider) -> Option<Record> {
        self.slots()[provider.index()].record
    }

    /// The cached login of `provider` (`unknown` before the first probe).
    #[must_use]
    pub fn login(&self, provider: LoginProvider) -> ProviderLoginV1 {
        self.record(provider)
            .map_or_else(ProviderLoginV1::default, |record| ProviderLoginV1 {
                state: record.state,
                method: record.method,
                checked_at: Some(record.checked_at),
                hosts: Vec::new(),
            })
    }

    /// Run a probe unless a result no older than `max_age` exists. One probe
    /// per provider is in flight; a caller that waited on it reuses its result.
    async fn probe_if_older_than(&self, provider: LoginProvider, max_age: Duration) -> Record {
        let threshold = Instant::now().checked_sub(max_age);
        let usable = |record: Option<Record>| {
            record.filter(|record| threshold.is_none_or(|threshold| record.probed >= threshold))
        };
        if let Some(record) = usable(self.record(provider)) {
            return record;
        }
        let _flight = self.flights[provider.index()].lock().await;
        if let Some(record) = usable(self.record(provider)) {
            return record;
        }
        self.probes_started
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let run = self.runner.run(provider).await;
        self.observe_account(provider, &run).await;
        let (state, method) = interpret(provider, &run);
        let billing_store = self
            .billing_store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .and_then(std::sync::Weak::upgrade);
        if state == ProviderLoginState::LoggedIn
            && method != ProviderLoginMethod::Unknown
            && let Some(store) = billing_store
        {
            if let Err(error) = store
                .lock()
                .await
                .record_provider_billing_login(provider.name(), method)
            {
                tracing::warn!(%error, "could not publish provider billing login method");
            }
        }
        let record = Record {
            state,
            method,
            checked_at: Utc::now(),
            probed: Instant::now(),
        };
        {
            let mut slots = self.slots();
            let slot = &mut slots[provider.index()];
            if state != ProviderLoginState::Unknown {
                if slot.last_known == Some(ProviderLoginState::LoggedIn)
                    && state == ProviderLoginState::LoggedOut
                {
                    slot.logout_transition = Some(record.checked_at);
                }
                if state == ProviderLoginState::LoggedIn {
                    slot.logout_transition = None;
                }
                slot.last_known = Some(state);
            }
            slot.record = Some(record);
        }
        // State and method only: the CLI's output stays out of the log.
        tracing::info!(
            provider = provider.name(),
            state = state.as_str(),
            method = method.as_str(),
            "provider login probe finished"
        );
        record
    }

    /// The login of `provider`, probing first when the cache is older than the
    /// TTL.
    pub async fn ensure_fresh(&self, provider: LoginProvider) -> ProviderLoginV1 {
        self.probe_if_older_than(provider, self.ttl).await;
        self.login(provider)
    }

    /// Probe now, bypassing the cache (operator refresh, after an auth stop).
    pub async fn refresh(&self, provider: LoginProvider) -> ProviderLoginV1 {
        self.probe_if_older_than(provider, Duration::ZERO).await;
        self.login(provider)
    }

    /// Probe every provider whose cache is stale, concurrently.
    pub async fn ensure_fresh_all(&self) {
        tokio::join!(
            self.ensure_fresh(LoginProvider::Claude),
            self.ensure_fresh(LoginProvider::Codex)
        );
    }

    /// Refresh every provider in the background (daemon start).
    pub fn spawn_refresh_all(self: &Arc<Self>) {
        for provider in LoginProvider::ALL {
            let service = Arc::clone(self);
            tokio::spawn(async move {
                service.refresh(provider).await;
            });
        }
    }

    /// Refresh stale providers in the background (satellite identity polls).
    pub fn spawn_refresh_stale(self: &Arc<Self>) {
        for provider in LoginProvider::ALL {
            let service = Arc::clone(self);
            tokio::spawn(async move {
                service.ensure_fresh(provider).await;
            });
        }
    }

    /// A provider session just stopped on an authentication failure: probe
    /// again now, in the background, so the state follows the failure.
    pub fn note_auth_stop(self: &Arc<Self>, provider: LoginProvider) {
        let service = Arc::clone(self);
        tokio::spawn(async move {
            service.refresh(provider).await;
        });
    }

    /// Refuse a launch that needs `provider`'s login while it is logged out.
    /// `unknown` and a missing probe admit. A `logged_out` record older than
    /// the TTL is probed again first, so a login the operator just did lifts
    /// the refusal at once.
    pub async fn launch_refusal(&self, provider: LoginProvider) -> Option<String> {
        let login = match self.record(provider) {
            Some(record) if record.state == ProviderLoginState::LoggedOut => {
                if record.probed.elapsed() >= self.ttl {
                    self.ensure_fresh(provider).await
                } else {
                    self.login(provider)
                }
            }
            _ => return None,
        };
        (login.state == ProviderLoginState::LoggedOut)
            .then(|| login_required_message(provider.name()))
    }

    /// The providers that went from `logged_in` to `logged_out` and still are,
    /// each with the transition time (the manager notice's episode).
    #[must_use]
    pub fn logout_transitions(&self) -> Vec<(&'static str, DateTime<Utc>)> {
        let slots = self.slots();
        LoginProvider::ALL
            .into_iter()
            .filter_map(|provider| {
                let slot = &slots[provider.index()];
                let at = slot.logout_transition?;
                (slot.record?.state == ProviderLoginState::LoggedOut)
                    .then_some((provider.name(), at))
            })
            .collect()
    }

    /// The probes that finished, as a satellite reports them in its health.
    #[must_use]
    pub fn health_reports(&self) -> Vec<ProviderLoginHealthV1> {
        LoginProvider::ALL
            .into_iter()
            .filter_map(|provider| {
                let record = self.record(provider)?;
                Some(ProviderLoginHealthV1 {
                    provider: provider.name().to_string(),
                    state: record.state,
                    method: record.method,
                    checked_at: record.checked_at,
                })
            })
            .collect()
    }

    /// Cached logins for a status report, probing stale ones first (bounded:
    /// a slow CLI serves the previous cache instead of stalling the read).
    pub async fn logins_for_status(&self) -> Vec<(String, ProviderLoginV1)> {
        let _ = tokio::time::timeout(STATUS_READ_WAIT, self.ensure_fresh_all()).await;
        LoginProvider::ALL
            .into_iter()
            .map(|provider| (provider.name().to_string(), self.login(provider)))
            .collect()
    }

    /// `GetProviderLoginState` / `RefreshProviderLoginState`.
    pub async fn state_result(
        &self,
        request: &ProviderLoginRequestV1,
        refresh: bool,
    ) -> ProviderLoginStateResultV1 {
        let wanted: Vec<LoginProvider> = LoginProvider::ALL
            .into_iter()
            .filter(|provider| {
                request
                    .provider
                    .as_deref()
                    .is_none_or(|name| name == provider.name())
            })
            .collect();
        let probe = async {
            let mut probes = Vec::new();
            for provider in &wanted {
                probes.push(async move {
                    if refresh {
                        self.refresh(*provider).await
                    } else {
                        self.ensure_fresh(*provider).await
                    }
                });
            }
            futures::future::join_all(probes).await;
        };
        // An operator refresh waits for the CLI's own timeout; a plain read
        // serves the cache after the shorter status wait.
        let _ = tokio::time::timeout(
            if refresh {
                PROBE_TIMEOUT * 2
            } else {
                STATUS_READ_WAIT
            },
            probe,
        )
        .await;
        ProviderLoginStateResultV1 {
            generated_at: Utc::now(),
            cache_ttl_secs: self.ttl.as_secs(),
            providers: wanted
                .into_iter()
                .map(|provider| ProviderLoginRowV1 {
                    provider: provider.name().to_string(),
                    login: self.login(provider),
                    remedy: login_remedy(provider.name()).to_string(),
                })
                .collect(),
        }
    }

    /// Seed a state without a probe (tests of the launch chokepoint).
    #[cfg(test)]
    pub(crate) fn seed_for_test(&self, provider: LoginProvider, state: ProviderLoginState) {
        let mut slots = self.slots();
        let slot = &mut slots[provider.index()];
        slot.last_known = Some(state);
        slot.record = Some(Record {
            state,
            method: ProviderLoginMethod::None,
            checked_at: Utc::now(),
            probed: Instant::now(),
        });
    }
}

/// The operator-only login methods this module serves.
#[must_use]
pub fn is_operator_method(method: &str) -> bool {
    rsi_common::provider_login::OPERATOR_METHODS.contains(&method)
}

/// Dispatch `GetProviderLoginState` / `RefreshProviderLoginState`. A tokened
/// caller is refused again here (the attribution gate already default-denies
/// it); the result carries state, method, check time and the remedy only.
///
/// # Errors
/// A refusal for a session-attributed caller, malformed params or an unknown
/// provider name.
pub async fn handle_operator_method(
    service: &ProviderLoginService,
    method: &str,
    session_token_present: bool,
    params: &serde_json::Value,
) -> crate::error::Result<serde_json::Value> {
    use crate::error::DaemonError;
    if session_token_present {
        return Err(DaemonError::PolicyDenied(format!(
            "{method} is operator-only and not available to session-attributed callers"
        )));
    }
    let request: ProviderLoginRequestV1 = if params.is_null() {
        ProviderLoginRequestV1::default()
    } else {
        serde_json::from_value(params.clone())
            .map_err(|error| DaemonError::InvalidParam(error.to_string()))?
    };
    request
        .validate()
        .map_err(|code| DaemonError::InvalidParam(code.into()))?;
    let refresh = method == rsi_common::provider_login::METHOD_REFRESH;
    let result = service.state_result(&request, refresh).await;
    serde_json::to_value(result).map_err(|error| DaemonError::Rpc(error.to_string()))
}

#[cfg(test)]
mod tests;
