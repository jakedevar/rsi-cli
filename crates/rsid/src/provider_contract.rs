//! Provider CLI contract canary (#2755): do the installed `claude` and `codex`
//! still behave the way RSI's launch code assumes?
//!
//! RSI re-implements pieces of both CLIs' contracts by hand, and a CLI that
//! drifts breaks them silently:
//!
//! * **Codex hook trust.** The identity hash of the session-flag hooks that
//!   carry agent mail and the sandbox custody guard (#2320) is computed by RSI
//!   (`codex.rs`). A Codex build that hashes differently leaves the hooks
//!   untrusted, and an untrusted hook is simply not run.
//! * **Claude flags.** A launch passes ~14 flags. `claude --help` is not
//!   exhaustive (it omits `--max-turns`), so a flag the help text lacks is
//!   checked with an `unknown option` probe.
//! * **Claude non-`--bare` behaviour.** RSI authenticates through the ambient
//!   OAuth login and keeps hooks and CLAUDE.md, all of which `--bare` drops;
//!   the CLI documents `--bare` as the future default for `-p`.
//!
//! The canary runs per CLI at daemon start and whenever the CLI's version
//! changes (a stat of the binary on each manager-coordinator pass; no polling
//! of the CLI itself), is bounded, and never starts a model turn:
//!
//! * Codex: `codex -c hooks.* app-server`, the RSI `initialize` handshake
//!   (`codex_app_server/wire.rs`), then `hooks/list`. Both session-flag hooks
//!   must be listed, `trusted`, with `currentHash` equal to RSI's hash.
//! * Claude: `--help`, an `unknown option` probe for each launch flag the help
//!   text lacks, and the stream-json `initialize` control request, which must
//!   carry `claude_code_version` and `capabilities`, and, when the Claude login
//!   is an OAuth login, the OAuth account fields a non-`--bare` launch sees.
//!
//! The result per provider is four facts (state, reason code, CLI version,
//! check time). `unchecked` fails open: a missing CLI, a timeout or a
//! disabled canary never refuses a launch. Only a definite untrusted Codex
//! custody hook does, and only while the operator setting
//! `codex_refuse_untrusted_custody_hook` is on (default).

mod codex_keys;
pub use codex_keys::{CodexConfigSchema, CodexKeyCheck, ConfigKeyProbe, SchemaVerdict};

use crate::process_control::{
    BoundedLines, CaptureLimits, OverflowBehavior, PROVIDER_MAX_LINE_BYTES, ProcessContainment,
    capture_bounded_with_stdin, configure_tokio_process_group, terminate_process_group,
};
use chrono::{DateTime, Utc};
use rsi_common::provider_contract::{
    MAX_CONTRACT_DETAIL_BYTES, ProviderContractRequestV1, ProviderContractRowV1,
    ProviderContractState, ProviderContractStateResultV1, ProviderContractV1,
    REASON_CANARY_DISABLED, REASON_CANARY_FAILED, REASON_CLAUDE_AMBIENT_AUTH_DROPPED,
    REASON_CLAUDE_CONTRACT_SHAPE, REASON_CLAUDE_FLAG_UNKNOWN_PREFIX,
    REASON_CLAUDE_INITIALIZE_FAILED, REASON_CLI_UNAVAILABLE, REASON_CODEX_HANDSHAKE_FAILED,
    REASON_CODEX_HOOK_HASH_MISMATCH, REASON_CODEX_HOOK_MISSING, REASON_CODEX_HOOK_UNTRUSTED,
    REASON_CODEX_LIFECYCLE_HOOK_UNTRUSTED, bound_contract_detail, custody_refusal_message,
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::process::Command;

/// Wall time of one CLI invocation the canary makes.
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);
/// Most bytes of one stream a CLI invocation may print.
const MAX_PROBE_STREAM_BYTES: usize = 1024 * 1024;
/// An `unchecked` result whose canary could not run (a timeout, a spawn
/// failure) is retried by a later pass no sooner than this; a result that ran
/// is never repeated until the CLI's version changes.
const RETRY_UNCHECKED_AFTER: Duration = Duration::from_secs(300);
/// How long an operator read waits for a running canary.
const READ_WAIT: Duration = Duration::from_secs(10);

fn probe_limits() -> CaptureLimits {
    CaptureLimits::new(
        MAX_PROBE_STREAM_BYTES,
        64 * 1024,
        PROBE_TIMEOUT,
        OverflowBehavior::TruncateAndDrain,
        ProcessContainment::GroupNoEscape,
    )
}

/// A provider CLI whose contract is checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContractProvider {
    Claude,
    Codex,
}

impl ContractProvider {
    pub const ALL: [Self; 2] = [Self::Claude, Self::Codex];

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    const fn index(self) -> usize {
        match self {
            Self::Claude => 0,
            Self::Codex => 1,
        }
    }
}

/// One hook as `hooks/list` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookRow {
    pub key: String,
    pub trust_status: String,
    pub current_hash: Option<String>,
}

/// Why a probe produced no usable observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeFailure {
    /// The CLI could not be run to a verdict (spawn failure, timeout, died).
    /// Not evidence of drift: the result is `unchecked`.
    Inconclusive,
    /// The CLI answered, but not in the shape RSI relies on: a drift.
    Drift,
}

/// What an `unknown option` probe of one Claude flag found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlagProbe {
    Accepted,
    Unknown,
    Inconclusive,
}

/// Runs the CLI invocations the canary needs; behind a trait so a test never
/// starts a real CLI.
#[async_trait::async_trait]
pub trait ContractRunner: Send + Sync {
    /// A cheap identity of the installed binary (path, size, mtime), or `None`
    /// when the CLI is not installed. A change is the cue to read `--version`.
    fn fingerprint(&self, provider: ContractProvider) -> Option<String>;
    /// The first line of `--version`, or `None` when the CLI cannot run.
    async fn cli_version(&self, provider: ContractProvider) -> Option<String>;
    /// `codex -c hooks.* app-server`: handshake and `hooks/list`, with the
    /// hooks RSI's launch installs and the expected `(key, hash)` of each.
    async fn codex_hooks(&self) -> Result<(Vec<HookRow>, Vec<(String, String)>), ProbeFailure>;
    /// The text of `claude --help`.
    async fn claude_help(&self) -> Option<String>;
    /// Whether `claude -p <flag>` is rejected as an unknown option.
    async fn claude_flag_probe(&self, flag: &str) -> FlagProbe;
    /// The `response.response` object of the stream-json `initialize` reply.
    async fn claude_initialize(&self) -> Result<Value, ProbeFailure>;
    /// The installed codex-cli's own config schema
    /// (`app-server generate-json-schema`, `v2/ConfigReadResponse.json`).
    async fn codex_config_schema(&self) -> Result<Value, ProbeFailure> {
        Err(ProbeFailure::Inconclusive)
    }
    /// Whether the installed codex-cli types the config `key`: a nonsense value
    /// for `-c key=...` fails config loading naming the key. A key the CLI does
    /// not know is ignored.
    async fn codex_config_key_probe(&self, _key: &str) -> ConfigKeyProbe {
        ConfigKeyProbe::Inconclusive
    }
}

/// Production runner: the CLIs found through [`crate::provider_cli::resolve`].
pub struct CliContractRunner {
    binaries: Option<HashMap<ContractProvider, PathBuf>>,
    hook_command: Option<Option<String>>,
    timeout: Duration,
}

impl CliContractRunner {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            binaries: None,
            hook_command: None,
            timeout: PROBE_TIMEOUT,
        }
    }

    /// Bound the `app-server` exchange by `timeout` instead of the default.
    #[must_use]
    pub const fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// A runner that starts exactly these binaries with exactly this hook
    /// command (a stub CLI in a test).
    #[must_use]
    pub fn with_binaries(
        binaries: HashMap<ContractProvider, PathBuf>,
        hook_command: Option<String>,
    ) -> Self {
        Self {
            binaries: Some(binaries),
            hook_command: Some(hook_command),
            timeout: PROBE_TIMEOUT,
        }
    }

    fn binary(&self, provider: ContractProvider) -> Option<PathBuf> {
        match &self.binaries {
            Some(binaries) => binaries.get(&provider).cloned(),
            None => crate::provider_cli::resolve(provider.name()),
        }
    }

    /// The command the launch's mail and custody hooks run. The identity hash
    /// depends on the command text only through the hash input, so when no
    /// `rsi-rpc` sibling is installed (no hook would be installed either) the
    /// running executable stands in: the canary then still checks that Codex
    /// computes the hash RSI does.
    fn hook_command(&self) -> Option<String> {
        if let Some(command) = &self.hook_command {
            return command.clone();
        }
        let args = [rsi_common::boundary_mail_hook::MAIL_ONLY_FLAG];
        crate::claude::boundary_mail_hook_command(
            crate::claude::daemon_sibling_rsi_rpc_path().as_deref(),
            &args,
        )
        .or_else(|| {
            let exe = std::env::current_exe().ok()?;
            crate::claude::boundary_mail_hook_command(Some(&exe), &args)
        })
    }

    /// A probe command: the launch credential scrub, a neutral directory.
    fn command(binary: &Path) -> Command {
        let mut command = Command::new(binary);
        command.current_dir(std::env::temp_dir()).env(
            rsi_common::identity::ENV_PROCESS_OWNERSHIP_NAMESPACE,
            rsi_common::identity::process_ownership_namespace(),
        );
        crate::vault::scrub_credential_env(&mut command);
        command
    }

    async fn capture(
        command: Command,
        stdin: Option<Vec<u8>>,
    ) -> Option<crate::process_control::CapturedOutput> {
        capture_bounded_with_stdin(
            command,
            probe_limits(),
            &tokio_util::sync::CancellationToken::new(),
            stdin,
        )
        .await
        .ok()
    }
}

impl Default for CliContractRunner {
    fn default() -> Self {
        Self::new()
    }
}

/// `--version` output reduced to one short printable line.
fn version_line(stdout: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(stdout);
    let line: String = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?
        .chars()
        .filter(|c| !c.is_control())
        .take(120)
        .collect();
    (!line.is_empty()).then_some(line)
}

#[async_trait::async_trait]
impl ContractRunner for CliContractRunner {
    fn fingerprint(&self, provider: ContractProvider) -> Option<String> {
        let binary = self.binary(provider)?;
        let metadata = std::fs::metadata(&binary).ok()?;
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |elapsed| elapsed.as_nanos());
        Some(format!(
            "{}:{}:{modified}",
            binary.display(),
            metadata.len()
        ))
    }

    async fn cli_version(&self, provider: ContractProvider) -> Option<String> {
        let binary = self.binary(provider)?;
        let mut command = Self::command(&binary);
        command.arg("--version");
        let output = Self::capture(command, None).await?;
        output
            .status
            .success()
            .then(|| version_line(&output.stdout))
            .flatten()
    }

    async fn codex_hooks(&self) -> Result<(Vec<HookRow>, Vec<(String, String)>), ProbeFailure> {
        let binary = self
            .binary(ContractProvider::Codex)
            .ok_or(ProbeFailure::Inconclusive)?;
        let hook_command = self.hook_command().ok_or(ProbeFailure::Inconclusive)?;
        // #2766: the lifecycle hooks a launch can add (`PreCompact`, hub
        // #2919 `SessionStart`) are
        // probed too, whatever the operator setting, so their trust drift is
        // seen before an operator enables one.
        let lifecycle = crate::codex::CODEX_PROBED_LIFECYCLE_EVENTS;
        let expected: Vec<(String, String)> =
            crate::codex::codex_session_hook_expectations(&hook_command)
                .into_iter()
                .chain(crate::codex::codex_lifecycle_hook_expectations(
                    &hook_command,
                    &lifecycle,
                ))
                .map(|(key, hash)| (key.to_string(), hash))
                .collect();
        // The real overrides a Codex launch passes, ahead of the subcommand.
        let mut command = crate::codex_live_catalog::probe_command(&binary);
        command.current_dir(std::env::temp_dir());
        for hook_override in
            crate::codex::codex_session_hook_overrides_with_lifecycle(&hook_command, &lifecycle)
        {
            command.arg("-c").arg(hook_override);
        }
        command
            .arg("app-server")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        configure_tokio_process_group(&mut command, ProcessContainment::Group)
            .map_err(|_| ProbeFailure::Inconclusive)?;
        let mut child = command.spawn().map_err(|_| ProbeFailure::Inconclusive)?;
        let pgid = child
            .id()
            .and_then(|pid| i32::try_from(pid).ok())
            .map(nix::unistd::Pid::from_raw);
        let result = match (child.stdin.take(), child.stdout.take()) {
            (Some(stdin), Some(stdout)) => {
                match tokio::time::timeout(self.timeout, drive_hooks_list(stdin, stdout)).await {
                    Ok(result) => result,
                    Err(_) => Err(ProbeFailure::Inconclusive),
                }
            }
            _ => Err(ProbeFailure::Inconclusive),
        };
        // The app-server is a long-lived stdio server: stop its whole group.
        if let Some(pgid) = pgid {
            terminate_process_group(pgid);
        }
        let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
        result.map(|rows| (rows, expected))
    }

    async fn claude_help(&self) -> Option<String> {
        let binary = self.binary(ContractProvider::Claude)?;
        let mut command = Self::command(&binary);
        command.arg("--help");
        let output = Self::capture(command, None).await?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
    }

    async fn claude_flag_probe(&self, flag: &str) -> FlagProbe {
        let Some(binary) = self.binary(ContractProvider::Claude) else {
            return FlagProbe::Inconclusive;
        };
        let mut command = Self::command(&binary);
        // `-p` with no input: the CLI parses its options first, then refuses
        // for want of a prompt, so an accepted flag costs no model turn.
        command.arg("-p").arg(flag);
        let Some(output) = Self::capture(command, None).await else {
            return FlagProbe::Inconclusive;
        };
        if unknown_option_rejected(&output.stderr, flag)
            || unknown_option_rejected(&output.stdout, flag)
        {
            FlagProbe::Unknown
        } else {
            FlagProbe::Accepted
        }
    }

    async fn claude_initialize(&self) -> Result<Value, ProbeFailure> {
        let binary = self
            .binary(ContractProvider::Claude)
            .ok_or(ProbeFailure::Inconclusive)?;
        let mut command = Self::command(&binary);
        command.args(crate::claude::model_discovery::PROBE_ARGS);
        let output = capture_bounded_with_stdin(
            command,
            probe_limits(),
            &tokio_util::sync::CancellationToken::new(),
            Some(crate::claude::model_discovery::probe_request()),
        )
        .await
        .map_err(|_| ProbeFailure::Inconclusive)?;
        parse_claude_initialize(&output.stdout)
    }

    async fn codex_config_schema(&self) -> Result<Value, ProbeFailure> {
        let binary = self
            .binary(ContractProvider::Codex)
            .ok_or(ProbeFailure::Inconclusive)?;
        let out = tempfile::tempdir().map_err(|_| ProbeFailure::Inconclusive)?;
        let mut command = Self::command(&binary);
        command
            .args(["app-server", "generate-json-schema", "--out"])
            .arg(out.path());
        let output = Self::capture(command, None)
            .await
            .ok_or(ProbeFailure::Inconclusive)?;
        if !output.status.success() {
            return Err(ProbeFailure::Inconclusive);
        }
        let path = out.path().join("v2").join("ConfigReadResponse.json");
        let metadata = std::fs::metadata(&path).map_err(|_| ProbeFailure::Inconclusive)?;
        if metadata.len() > MAX_PROBE_STREAM_BYTES as u64 {
            return Err(ProbeFailure::Inconclusive);
        }
        let text = std::fs::read_to_string(&path).map_err(|_| ProbeFailure::Inconclusive)?;
        serde_json::from_str(&text).map_err(|_| ProbeFailure::Inconclusive)
    }

    async fn codex_config_key_probe(&self, key: &str) -> ConfigKeyProbe {
        let Some(binary) = self.binary(ContractProvider::Codex) else {
            return ConfigKeyProbe::Inconclusive;
        };
        let mut command = Self::command(&binary);
        command
            .arg("-c")
            .arg(format!("{key}={{ rsi_probe = true }}"))
            .args(["features", "list"]);
        let Some(output) = Self::capture(command, None).await else {
            return ConfigKeyProbe::Inconclusive;
        };
        if output.status.success() {
            return ConfigKeyProbe::Untyped;
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains(&format!("`{key}`")) {
            ConfigKeyProbe::Typed
        } else {
            ConfigKeyProbe::Inconclusive
        }
    }
}

/// Whether `text` (a CLI's stderr or stdout) says `flag` is an unknown option.
#[must_use]
pub fn unknown_option_rejected(text: &[u8], flag: &str) -> bool {
    let text = String::from_utf8_lossy(text);
    text.contains(&format!("unknown option '{flag}'"))
        || text.contains(&format!("unknown option \"{flag}\""))
}

/// Whether `flag` is listed in the `--help` text: as a whole token, so
/// `--model` is not found inside `--model-foo`, and a short flag is found as
/// `-p,`.
#[must_use]
pub fn help_lists_flag(help: &str, flag: &str) -> bool {
    help.split(|c: char| c.is_whitespace() || matches!(c, ',' | '=' | '<' | '[' | '|'))
        .any(|token| token == flag)
}

/// The `response.response` object of the `initialize` control reply.
fn parse_claude_initialize(stdout: &[u8]) -> Result<Value, ProbeFailure> {
    let text = String::from_utf8_lossy(stdout);
    let reply = text
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .find(|line| {
            line.get("type").and_then(Value::as_str) == Some("control_response")
                && line.pointer("/response/request_id").and_then(Value::as_str)
                    == Some(crate::claude::model_discovery::PROBE_REQUEST_ID)
        })
        .ok_or(ProbeFailure::Drift)?;
    if reply.pointer("/response/subtype").and_then(Value::as_str) != Some("success") {
        return Err(ProbeFailure::Drift);
    }
    reply
        .pointer("/response/response")
        .filter(|inner| inner.is_object())
        .cloned()
        .ok_or(ProbeFailure::Drift)
}

async fn drive_hooks_list(
    mut stdin: tokio::process::ChildStdin,
    stdout: tokio::process::ChildStdout,
) -> Result<Vec<HookRow>, ProbeFailure> {
    use crate::codex_live_catalog::{response_for, send};
    let inconclusive = |_| ProbeFailure::Inconclusive;
    let mut lines = BoundedLines::new(stdout, PROVIDER_MAX_LINE_BYTES);
    // The handshake the CodexAppServer provider performs (#2754).
    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": crate::codex_app_server::wire::initialize_params(),
        }),
    )
    .await
    .map_err(inconclusive)?;
    let init = response_for(&mut lines, 1).await.map_err(inconclusive)?;
    if init.get("error").is_some() || init.get("result").is_none() {
        return Err(ProbeFailure::Drift);
    }
    send(
        &mut stdin,
        crate::codex_app_server::wire::initialized_notification(),
    )
    .await
    .map_err(inconclusive)?;
    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "hooks/list",
            "params": {"cwds": [std::env::temp_dir().to_string_lossy()]},
        }),
    )
    .await
    .map_err(inconclusive)?;
    let response = response_for(&mut lines, 2).await.map_err(inconclusive)?;
    if response.get("error").is_some() {
        return Err(ProbeFailure::Drift);
    }
    let data = response
        .pointer("/result/data")
        .and_then(Value::as_array)
        .ok_or(ProbeFailure::Drift)?;
    let mut rows = Vec::new();
    for entry in data {
        let Some(hooks) = entry.get("hooks").and_then(Value::as_array) else {
            continue;
        };
        for hook in hooks {
            let Some(key) = hook.get("key").and_then(Value::as_str) else {
                continue;
            };
            rows.push(HookRow {
                key: key.to_string(),
                trust_status: hook
                    .get("trustStatus")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                current_hash: hook
                    .get("currentHash")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            });
        }
    }
    Ok(rows)
}

/// The reason a Codex hook list fails the contract, if it does: both expected
/// hooks must be listed, runnable and hashed as RSI hashes them.
#[must_use]
pub fn evaluate_codex_hooks(
    rows: &[HookRow],
    expected: &[(String, String)],
) -> Option<&'static str> {
    for (key, hash) in expected {
        let Some(row) = rows.iter().find(|row| &row.key == key) else {
            return Some(REASON_CODEX_HOOK_MISSING);
        };
        if row
            .current_hash
            .as_deref()
            .is_some_and(|current| current != hash)
        {
            return Some(REASON_CODEX_HOOK_HASH_MISMATCH);
        }
        match row.trust_status.as_str() {
            "trusted" | "managed" => {}
            "modified" => return Some(REASON_CODEX_HOOK_HASH_MISMATCH),
            _ => return Some(REASON_CODEX_HOOK_UNTRUSTED),
        }
    }
    None
}

/// #2766: [`evaluate_codex_hooks`] over the custody and mail hooks first (their
/// reasons may refuse a launch), then over the lifecycle hooks, whose failure
/// is reported as [`REASON_CODEX_LIFECYCLE_HOOK_UNTRUSTED`] and never refuses:
/// an untrusted lifecycle hook is merely not run.
#[must_use]
pub fn evaluate_codex_hooks_with_lifecycle(
    rows: &[HookRow],
    expected: &[(String, String)],
) -> Option<&'static str> {
    let (lifecycle, custody): (Vec<_>, Vec<_>) = expected
        .iter()
        .cloned()
        .partition(|(key, _)| crate::codex::is_codex_lifecycle_hook_key(key));
    if let Some(reason) = evaluate_codex_hooks(rows, &custody) {
        return Some(reason);
    }
    evaluate_codex_hooks(rows, &lifecycle).map(|_| REASON_CODEX_LIFECYCLE_HOOK_UNTRUSTED)
}

/// A contract mismatch the canary found: the stable reason code and the first
/// failing element as one line (hub #2981). The detail names only RSI's own
/// vocabulary (probe step, field or flag name, expected and observed JSON
/// kind), never a value the CLI printed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContractMismatch {
    pub reason: &'static str,
    pub detail: String,
}

/// The JSON kind of `value`, or `missing` when the field is absent.
fn observed_kind(value: Option<&Value>) -> &'static str {
    match value {
        None => "missing",
        Some(Value::Null) => "null",
        Some(Value::Bool(_)) => "bool",
        Some(Value::Number(_)) => "number",
        Some(Value::String(text)) if text.trim().is_empty() => "empty_string",
        Some(Value::String(_)) => "string",
        Some(Value::Array(_)) => "array",
        Some(Value::Object(_)) => "object",
    }
}

fn mismatch_detail(probe: &str, element: &str, expected: &str, observed: &str) -> String {
    format!("probe={probe} element={element} expected={expected} observed={observed}")
}

/// The detail of a Claude launch flag the installed CLI rejects.
fn claude_flag_detail(flag: &str) -> String {
    mismatch_detail("flag", flag, "listed_in_help_or_accepted", "unknown_option")
}

/// The detail of an `initialize` exchange that produced no usable reply.
fn claude_initialize_failed_detail() -> String {
    mismatch_detail(
        "initialize",
        "control_response",
        "success_object",
        "refused_or_malformed",
    )
}

/// The first way a Claude `initialize` reply fails the contract, if it does.
/// `expect_oauth` is true when the host's Claude login is an OAuth login: RSI's
/// non-`--bare` launch then reads the account (e-mail, organisation or
/// subscription), and a reply with none of them says OAuth was not read, as
/// under `--bare`.
#[must_use]
pub fn claude_initialize_mismatch(init: &Value, expect_oauth: bool) -> Option<ContractMismatch> {
    let version = init.get("claude_code_version");
    if !version
        .and_then(Value::as_str)
        .is_some_and(|version| !version.trim().is_empty())
    {
        return Some(ContractMismatch {
            reason: REASON_CLAUDE_CONTRACT_SHAPE,
            detail: mismatch_detail(
                "initialize",
                "claude_code_version",
                "non_empty_string",
                observed_kind(version),
            ),
        });
    }
    let capabilities = init.get("capabilities");
    if !capabilities.is_some_and(Value::is_array) {
        return Some(ContractMismatch {
            reason: REASON_CLAUDE_CONTRACT_SHAPE,
            detail: mismatch_detail(
                "initialize",
                "capabilities",
                "array",
                observed_kind(capabilities),
            ),
        });
    }
    if expect_oauth {
        let account = init.get("account");
        let has_oauth_field = ["email", "organization", "subscriptionType"]
            .iter()
            .any(|field| {
                account
                    .and_then(|account| account.get(field))
                    .is_some_and(|value| !value.is_null() && value.as_str() != Some(""))
            });
        if !has_oauth_field {
            return Some(ContractMismatch {
                reason: REASON_CLAUDE_AMBIENT_AUTH_DROPPED,
                detail: mismatch_detail(
                    "initialize",
                    "account",
                    "email_or_organization_or_subscriptionType",
                    observed_kind(account),
                ),
            });
        }
    }
    None
}

/// The reason a Claude `initialize` reply fails the contract, if it does.
#[must_use]
pub fn evaluate_claude_initialize(init: &Value, expect_oauth: bool) -> Option<&'static str> {
    claude_initialize_mismatch(init, expect_oauth).map(|mismatch| mismatch.reason)
}

/// The one-line detail of a degraded result, with the CLI version appended
/// (control characters stripped, version cut to 64 bytes) and the whole line
/// bounded to [`MAX_CONTRACT_DETAIL_BYTES`].
fn detail_with_version(detail: &str, version: &str) -> String {
    let version: String = version
        .chars()
        .filter(|c| !c.is_control())
        .take(64)
        .collect();
    let version = version.trim();
    // The version is what an operator compares across CLI releases, so it
    // survives a cut of the front of an unusually long detail.
    let suffix = format!(" cli_version={version}");
    let room = MAX_CONTRACT_DETAIL_BYTES.saturating_sub(suffix.len());
    let mut head = bound_contract_detail(detail);
    if head.len() > room {
        let mut end = room;
        while !head.is_char_boundary(end) {
            end -= 1;
        }
        head.truncate(end);
    }
    bound_contract_detail(&format!("{head}{suffix}"))
}

/// What one canary run found: the state, the reason code and the detail line.
type CanaryVerdict = (ProviderContractState, Option<String>, Option<String>);

/// A degraded provider: `(provider, reason, since, detail)`.
pub type DegradedEpisode = (&'static str, String, DateTime<Utc>, Option<String>);

#[derive(Debug, Clone)]
struct Record {
    contract: ProviderContractV1,
    /// The binary fingerprint the result was taken against.
    fingerprint: Option<String>,
    probed: Instant,
}

#[derive(Debug, Default)]
struct Slot {
    record: Option<Record>,
    /// When the provider entered its current degraded `(reason)`; cleared once
    /// it is `ok` again. The manager notice's episode.
    degraded_since: Option<(String, DateTime<Utc>)>,
    /// The detail line of the latest degraded result (hub #2981).
    degraded_detail: Option<String>,
}

pub struct ProviderContractService {
    runner: Arc<dyn ContractRunner>,
    slots: Mutex<[Slot; 2]>,
    /// One canary in flight per provider.
    flights: [tokio::sync::Mutex<()>; 2],
    runs_started: std::sync::atomic::AtomicUsize,
    /// What the installed codex-cli said about config keys (#281), one flight
    /// at a time so a CLI change is asked about once.
    codex_keys: tokio::sync::Mutex<codex_keys::CodexKeyCache>,
}

/// What a caller knows that the service cannot: the settings and the Claude
/// login.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckContext {
    /// The `provider_contract_canary` setting.
    pub enabled: bool,
    /// The Claude login is an OAuth login (`claude.ai`).
    pub claude_oauth_login: bool,
    /// Run even when the CLI's version did not change.
    pub force: bool,
}

impl ProviderContractService {
    #[must_use]
    pub fn new(runner: Arc<dyn ContractRunner>) -> Self {
        Self {
            runner,
            slots: Mutex::new([Slot::default(), Slot::default()]),
            flights: [tokio::sync::Mutex::new(()), tokio::sync::Mutex::new(())],
            runs_started: std::sync::atomic::AtomicUsize::new(0),
            codex_keys: tokio::sync::Mutex::new(codex_keys::CodexKeyCache::default()),
        }
    }

    /// Process-wide production instance (real CLIs).
    pub fn global() -> &'static Arc<Self> {
        static SERVICE: OnceLock<Arc<ProviderContractService>> = OnceLock::new();
        SERVICE.get_or_init(|| {
            #[cfg(test)]
            let runner: Arc<dyn ContractRunner> = Arc::new(NoCliRunner);
            #[cfg(not(test))]
            let runner: Arc<dyn ContractRunner> = Arc::new(CliContractRunner::new());
            Arc::new(Self::new(runner))
        })
    }

    /// The service a session manager launches against: the process-wide one,
    /// or a private one in unit tests so a seeded state never leaks between
    /// tests.
    #[must_use]
    pub fn for_manager() -> Arc<Self> {
        #[cfg(test)]
        {
            Arc::new(Self::new(Arc::new(NoCliRunner)))
        }
        #[cfg(not(test))]
        {
            Arc::clone(Self::global())
        }
    }

    /// How many canary runs started (one per actual check, not per provider
    /// CLI invocation).
    #[must_use]
    pub fn runs_started(&self) -> usize {
        self.runs_started.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn slots(&self) -> std::sync::MutexGuard<'_, [Slot; 2]> {
        self.slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn record(&self, provider: ContractProvider) -> Option<Record> {
        self.slots()[provider.index()].record.clone()
    }

    /// The cached contract of `provider` (`unchecked` before the first run).
    #[must_use]
    pub fn contract(&self, provider: ContractProvider) -> ProviderContractV1 {
        self.record(provider)
            .map_or_else(ProviderContractV1::default, |record| record.contract)
    }

    fn store(
        &self,
        provider: ContractProvider,
        contract: ProviderContractV1,
    ) -> ProviderContractV1 {
        let fingerprint = self.runner.fingerprint(provider);
        let mut slots = self.slots();
        let slot = &mut slots[provider.index()];
        match (&contract.contract_state, &contract.reason) {
            (ProviderContractState::Degraded, Some(reason)) => {
                let changed = slot
                    .degraded_since
                    .as_ref()
                    .is_none_or(|(known, _)| known != reason);
                if changed {
                    slot.degraded_since =
                        Some((reason.clone(), contract.checked_at.unwrap_or_else(Utc::now)));
                }
                slot.degraded_detail.clone_from(&contract.detail);
            }
            (ProviderContractState::Ok, _) => {
                slot.degraded_since = None;
                slot.degraded_detail = None;
            }
            // Unchecked keeps the last definite episode: a timeout between two
            // degraded results is not a recovery.
            _ => {}
        }
        slot.record = Some(Record {
            contract: contract.clone(),
            fingerprint,
            probed: Instant::now(),
        });
        contract
    }

    fn unchecked(reason: &str, version: Option<String>) -> ProviderContractV1 {
        ProviderContractV1 {
            contract_state: ProviderContractState::Unchecked,
            reason: Some(reason.to_string()),
            cli_version: version,
            detail: None,
            checked_at: Some(Utc::now()),
            catalog: None,
        }
    }

    /// Whether the cached record answers for the binary as installed now.
    fn current(&self, provider: ContractProvider, force: bool, enabled: bool) -> bool {
        if force {
            return false;
        }
        let Some(record) = self.record(provider) else {
            return false;
        };
        let reason = record.contract.reason.as_deref();
        if !enabled {
            // Already reported as disabled; nothing to run.
            return record.contract.contract_state == ProviderContractState::Unchecked
                && reason == Some(REASON_CANARY_DISABLED);
        }
        if record.fingerprint != self.runner.fingerprint(provider) {
            return false;
        }
        match (record.contract.contract_state, reason) {
            // Turned back on after being off.
            (ProviderContractState::Unchecked, Some(REASON_CANARY_DISABLED)) => false,
            // The canary could not run to a verdict: try again after a while.
            (ProviderContractState::Unchecked, Some(REASON_CANARY_FAILED)) => {
                record.probed.elapsed() < RETRY_UNCHECKED_AFTER
            }
            _ => true,
        }
    }

    /// Check `provider` unless the cached result already answers for the
    /// installed binary: the canary runs at most once per CLI version (and per
    /// binary replacement), plus on `force`. One canary per provider is in
    /// flight; a caller that waited on it reuses its result.
    pub async fn check(
        &self,
        provider: ContractProvider,
        context: CheckContext,
    ) -> ProviderContractV1 {
        self.check_gated(provider, context, crate::offline::is_offline())
            .await
    }

    /// [`Self::check`] with the offline mode (#130) injected: offline, no CLI
    /// starts (not even its version read) and the contract is `unchecked`
    /// with reason [`crate::offline::REASON_OFFLINE`], which fails open.
    pub(crate) async fn check_gated(
        &self,
        provider: ContractProvider,
        context: CheckContext,
        offline: bool,
    ) -> ProviderContractV1 {
        if offline {
            return self.store(
                provider,
                Self::unchecked(crate::offline::REASON_OFFLINE, None),
            );
        }
        if self.current(provider, context.force, context.enabled) {
            return self.contract(provider);
        }
        let _flight = self.flights[provider.index()].lock().await;
        if self.current(provider, context.force, context.enabled) && !context.force {
            return self.contract(provider);
        }
        if !context.enabled {
            return self.store(provider, Self::unchecked(REASON_CANARY_DISABLED, None));
        }
        let Some(version) = self.runner.cli_version(provider).await else {
            return self.store(provider, Self::unchecked(REASON_CLI_UNAVAILABLE, None));
        };
        // A binary that was replaced without a version change (or that was
        // touched) has nothing new to find: record the fingerprint, keep the
        // verdict, and do not run again.
        if !context.force
            && let Some(record) = self.record(provider)
            && record.contract.cli_version.as_deref() == Some(version.as_str())
            && record.contract.contract_state != ProviderContractState::Unchecked
        {
            let kept = ProviderContractV1 {
                checked_at: record.contract.checked_at,
                ..record.contract
            };
            return self.store(provider, kept);
        }
        self.runs_started
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let (state, reason, detail) = match provider {
            ContractProvider::Codex => self.run_codex().await,
            ContractProvider::Claude => self.run_claude(context.claude_oauth_login).await,
        };
        let detail = detail.map(|detail| detail_with_version(&detail, &version));
        let contract = ProviderContractV1 {
            contract_state: state,
            reason: reason.or_else(|| {
                (state == ProviderContractState::Unchecked)
                    .then(|| REASON_CANARY_FAILED.to_string())
            }),
            cli_version: Some(version),
            detail,
            checked_at: Some(Utc::now()),
            catalog: None,
        };
        let contract = self.store(provider, contract);
        tracing::info!(
            provider = provider.name(),
            state = contract.contract_state.as_str(),
            reason = contract.reason.as_deref().unwrap_or(""),
            cli_version = contract.cli_version.as_deref().unwrap_or(""),
            detail = contract.detail.as_deref().unwrap_or(""),
            "provider contract canary finished"
        );
        contract
    }

    async fn run_codex(&self) -> CanaryVerdict {
        match self.runner.codex_hooks().await {
            Ok((rows, expected)) => match evaluate_codex_hooks_with_lifecycle(&rows, &expected) {
                None => (ProviderContractState::Ok, None, None),
                Some(reason) => (
                    ProviderContractState::Degraded,
                    Some(reason.to_string()),
                    None,
                ),
            },
            Err(ProbeFailure::Drift) => (
                ProviderContractState::Degraded,
                Some(REASON_CODEX_HANDSHAKE_FAILED.to_string()),
                None,
            ),
            Err(ProbeFailure::Inconclusive) => (ProviderContractState::Unchecked, None, None),
        }
    }

    /// Whether the installed Claude CLI definitely rejects `flag`: not in the
    /// `--help` text AND refused by the `unknown option` probe. Hidden flags
    /// (`--max-turns`) are not in the help text, so the probe decides; an
    /// inconclusive probe (no CLI, a timeout) is not a rejection.
    async fn claude_rejects_flag(&self, help: Option<&str>, flag: &str) -> bool {
        if help.is_some_and(|help| help_lists_flag(help, flag)) {
            return false;
        }
        self.runner.claude_flag_probe(flag).await == FlagProbe::Unknown
    }

    /// hub #2765: the operator launch-override flags the installed Claude CLI
    /// rejects as unknown options, checked the way the canary checks RSI's own
    /// flags. Empty when every flag is known or the CLI cannot say (fails open,
    /// like an `unchecked` canary).
    pub async fn claude_flags_unknown_to_installed_cli(&self, flags: &[&str]) -> Vec<String> {
        if flags.is_empty() {
            return Vec::new();
        }
        let help = self.runner.claude_help().await;
        let mut unknown = Vec::new();
        for flag in flags {
            if self.claude_rejects_flag(help.as_deref(), flag).await {
                unknown.push((*flag).to_string());
            }
        }
        unknown
    }

    /// #281 (hub #2765): the operator launch-override Codex config keys the
    /// installed codex-cli does not know. The schema the CLI generates says
    /// which keys it lists; a key the schema does not model is probed with a
    /// nonsense value (`codex_keys` module). Both answers are cached per binary
    /// fingerprint, so the CLI is asked at most once per CLI change. When the
    /// CLI cannot answer, nothing is refused and the result is marked
    /// `degraded`: the static allowlist stands alone.
    pub async fn codex_keys_unknown_to_installed_cli(&self, keys: &[&str]) -> CodexKeyCheck {
        let mut check = CodexKeyCheck::default();
        if keys.is_empty() {
            return check;
        }
        let Some(fingerprint) = self.runner.fingerprint(ContractProvider::Codex) else {
            check.degraded = Some(codex_keys::DEGRADED_CLI_UNAVAILABLE);
            return check;
        };
        let mut cache = self.codex_keys.lock().await;
        if cache.fingerprint.as_deref() != Some(fingerprint.as_str()) {
            *cache = codex_keys::CodexKeyCache {
                fingerprint: Some(fingerprint),
                ..codex_keys::CodexKeyCache::default()
            };
        }
        let schema = self.codex_schema_cached(&mut cache).await;
        let Some(schema) = schema else {
            check.degraded = Some(codex_keys::DEGRADED_SCHEMA_UNAVAILABLE);
            return check;
        };
        for key in keys {
            match schema.classify(key) {
                SchemaVerdict::Known => {}
                SchemaVerdict::Unknown => check.unknown.push((*key).to_string()),
                SchemaVerdict::NotModelled => {
                    match self.codex_key_known_cached(&mut cache, key).await {
                        Some(true) => {}
                        Some(false) => check.unknown.push((*key).to_string()),
                        None => check.degraded = Some(codex_keys::DEGRADED_PROBE_INCONCLUSIVE),
                    }
                }
            }
        }
        check
    }

    /// The cached schema, asking the CLI when none is cached (a failure is
    /// retried no sooner than [`RETRY_UNCHECKED_AFTER`]).
    async fn codex_schema_cached(
        &self,
        cache: &mut codex_keys::CodexKeyCache,
    ) -> Option<Arc<CodexConfigSchema>> {
        match &cache.schema {
            Some(Some(schema)) => return Some(Arc::clone(schema)),
            Some(None)
                if cache
                    .schema_failed_at
                    .is_some_and(|at| at.elapsed() < RETRY_UNCHECKED_AFTER) =>
            {
                return None;
            }
            _ => {}
        }
        let schema = match self.runner.codex_config_schema().await {
            Ok(document) => CodexConfigSchema::parse(document).map(Arc::new),
            Err(_) => None,
        };
        cache.schema = Some(schema.clone());
        cache.schema_failed_at = schema.is_none().then(Instant::now);
        if schema.is_none() {
            tracing::warn!("codex config schema unavailable; launch override keys are unchecked");
        }
        schema
    }

    /// Whether the CLI knows `key`, probing once per key and CLI; `None` when
    /// the probe was inconclusive (not cached).
    async fn codex_key_known_cached(
        &self,
        cache: &mut codex_keys::CodexKeyCache,
        key: &str,
    ) -> Option<bool> {
        if let Some(known) = cache.probes.get(key) {
            return Some(*known);
        }
        let known = match self.runner.codex_config_key_probe(key).await {
            ConfigKeyProbe::Typed => true,
            ConfigKeyProbe::Untyped => false,
            ConfigKeyProbe::Inconclusive => return None,
        };
        cache.probes.insert(key.to_string(), known);
        Some(known)
    }

    async fn run_claude(&self, expect_oauth: bool) -> CanaryVerdict {
        let help = self.runner.claude_help().await;
        for flag in crate::claude::CLAUDE_LAUNCH_FLAGS {
            if self.claude_rejects_flag(help.as_deref(), flag).await {
                return (
                    ProviderContractState::Degraded,
                    Some(format!("{REASON_CLAUDE_FLAG_UNKNOWN_PREFIX}{flag}")),
                    Some(claude_flag_detail(flag)),
                );
            }
        }
        match self.runner.claude_initialize().await {
            Ok(init) => match claude_initialize_mismatch(&init, expect_oauth) {
                None => (ProviderContractState::Ok, None, None),
                Some(mismatch) => (
                    ProviderContractState::Degraded,
                    Some(mismatch.reason.to_string()),
                    Some(mismatch.detail),
                ),
            },
            Err(ProbeFailure::Drift) => (
                ProviderContractState::Degraded,
                Some(REASON_CLAUDE_INITIALIZE_FAILED.to_string()),
                Some(claude_initialize_failed_detail()),
            ),
            Err(ProbeFailure::Inconclusive) => (ProviderContractState::Unchecked, None, None),
        }
    }

    /// Check every provider whose cached result does not answer for the
    /// installed binary, concurrently.
    pub async fn ensure_current_all(&self, context: CheckContext) {
        tokio::join!(
            self.check(ContractProvider::Claude, context),
            self.check(ContractProvider::Codex, context)
        );
    }

    /// Refuse a Codex launch that needs the custody hook while the installed
    /// codex-cli is known not to trust it. The record answers only for the
    /// binary it was taken against: after the CLI was replaced, the next
    /// canary decides, never a stale verdict. `unchecked`, `ok`, a failed
    /// handshake and a setting that is off admit.
    #[must_use]
    pub fn codex_custody_refusal(&self, refuse_untrusted_custody_hook: bool) -> Option<String> {
        if !refuse_untrusted_custody_hook {
            return None;
        }
        let record = self.record(ContractProvider::Codex)?;
        if !record.contract.custody_failed()
            || record.fingerprint != self.runner.fingerprint(ContractProvider::Codex)
        {
            return None;
        }
        record
            .contract
            .reason
            .as_deref()
            .map(custody_refusal_message)
    }

    /// The providers that are degraded and still are, each with the reason, the
    /// time the provider entered that reason (the manager notice's episode) and
    /// the latest detail line (hub #2981).
    #[must_use]
    pub fn degraded_episodes(&self) -> Vec<DegradedEpisode> {
        let slots = self.slots();
        ContractProvider::ALL
            .into_iter()
            .filter_map(|provider| {
                let slot = &slots[provider.index()];
                let record = slot.record.as_ref()?;
                if record.contract.contract_state != ProviderContractState::Degraded {
                    return None;
                }
                let (reason, since) = slot.degraded_since.clone()?;
                Some((
                    provider.name(),
                    reason,
                    since,
                    slot.degraded_detail
                        .clone()
                        .or_else(|| record.contract.detail.clone()),
                ))
            })
            .collect()
    }

    /// The cached contracts for a status report, each `(provider name,
    /// contract)`.
    #[must_use]
    pub fn contracts_for_status(&self) -> Vec<(String, ProviderContractV1)> {
        ContractProvider::ALL
            .into_iter()
            .map(|provider| {
                (
                    provider.name().to_string(),
                    self.contract_with_catalog(provider),
                )
            })
            .collect()
    }

    /// The cached contract of `provider` with the Codex model catalog's trust
    /// (#2761) attached to the `codex` one. The catalog is read at serve time
    /// and never stored with the canary record.
    fn contract_with_catalog(&self, provider: ContractProvider) -> ProviderContractV1 {
        let mut contract = self.contract(provider);
        if provider == ContractProvider::Codex {
            contract.catalog =
                crate::provider_capabilities::provider_capabilities().codex_catalog_trust_view();
        }
        contract
    }

    /// `GetProviderContractState` / `RefreshProviderContractState`.
    pub async fn state_result(
        &self,
        request: &ProviderContractRequestV1,
        context: CheckContext,
        refuse_untrusted_custody_hook: bool,
    ) -> ProviderContractStateResultV1 {
        let wanted: Vec<ContractProvider> = ContractProvider::ALL
            .into_iter()
            .filter(|provider| {
                request
                    .provider
                    .as_deref()
                    .is_none_or(|name| name == provider.name())
            })
            .collect();
        let probe =
            futures::future::join_all(wanted.iter().map(|provider| self.check(*provider, context)));
        // A read waits for a canary that is running (or one that is due), but
        // not past the bound: it then serves the cache.
        let _ = tokio::time::timeout(READ_WAIT + PROBE_TIMEOUT, probe).await;
        ProviderContractStateResultV1 {
            generated_at: Utc::now(),
            canary_enabled: context.enabled,
            codex_refuse_untrusted_custody_hook: refuse_untrusted_custody_hook,
            providers: wanted
                .into_iter()
                .map(|provider| ProviderContractRowV1 {
                    provider: provider.name().to_string(),
                    contract: self.contract_with_catalog(provider),
                })
                .collect(),
        }
    }

    /// Seed a result without a canary (tests of the launch chokepoint). The
    /// fingerprint is the runner's current one, so the verdict applies.
    #[cfg(test)]
    pub(crate) fn seed_for_test(
        &self,
        provider: ContractProvider,
        state: ProviderContractState,
        reason: Option<&str>,
    ) {
        self.store(
            provider,
            ProviderContractV1 {
                contract_state: state,
                reason: reason.map(str::to_string),
                cli_version: Some("seeded".into()),
                detail: None,
                checked_at: Some(Utc::now()),
                catalog: None,
            },
        );
    }
}

/// The runner a process without a real CLI uses (unit tests).
#[cfg(test)]
struct NoCliRunner;

#[cfg(test)]
#[async_trait::async_trait]
impl ContractRunner for NoCliRunner {
    fn fingerprint(&self, _provider: ContractProvider) -> Option<String> {
        None
    }
    async fn cli_version(&self, _provider: ContractProvider) -> Option<String> {
        None
    }
    async fn codex_hooks(&self) -> Result<(Vec<HookRow>, Vec<(String, String)>), ProbeFailure> {
        Err(ProbeFailure::Inconclusive)
    }
    async fn claude_help(&self) -> Option<String> {
        None
    }
    async fn claude_flag_probe(&self, _flag: &str) -> FlagProbe {
        FlagProbe::Inconclusive
    }
    async fn claude_initialize(&self) -> Result<Value, ProbeFailure> {
        Err(ProbeFailure::Inconclusive)
    }
}

/// The operator-only contract methods this module serves.
#[must_use]
pub fn is_operator_method(method: &str) -> bool {
    rsi_common::provider_contract::OPERATOR_METHODS.contains(&method)
}

/// Dispatch `GetProviderContractState` / `RefreshProviderContractState`. A
/// tokened caller is refused again here (the attribution gate already
/// default-denies it); the result carries state, reason, CLI version and check
/// time only.
///
/// # Errors
/// A refusal for a session-attributed caller, malformed params or an unknown
/// provider name.
pub async fn handle_operator_method(
    service: &ProviderContractService,
    login: &crate::provider_login::ProviderLoginService,
    runtime_config: &crate::config::RuntimeConfig,
    method: &str,
    session_token_present: bool,
    params: &Value,
) -> crate::error::Result<Value> {
    use crate::error::DaemonError;
    if session_token_present {
        return Err(DaemonError::PolicyDenied(format!(
            "{method} is operator-only and not available to session-attributed callers"
        )));
    }
    let request: ProviderContractRequestV1 = if params.is_null() {
        ProviderContractRequestV1::default()
    } else {
        serde_json::from_value(params.clone())
            .map_err(|error| DaemonError::InvalidParam(error.to_string()))?
    };
    request
        .validate()
        .map_err(|code| DaemonError::InvalidParam(code.into()))?;
    let context = CheckContext {
        enabled: runtime_config.provider_contract_canary_enabled(),
        claude_oauth_login: claude_oauth_login(login).await,
        force: method == rsi_common::provider_contract::METHOD_REFRESH,
    };
    let result = service
        .state_result(
            &request,
            context,
            runtime_config.codex_refuse_untrusted_custody_hook_enabled(),
        )
        .await;
    serde_json::to_value(result).map_err(|error| DaemonError::Rpc(error.to_string()))
}

/// Whether the host's Claude login is an OAuth (`claude.ai`) login, reading the
/// login probe (probing it first when stale, bounded).
pub async fn claude_oauth_login(login: &crate::provider_login::ProviderLoginService) -> bool {
    use rsi_common::provider_login::{ProviderLoginMethod, ProviderLoginState};
    let state = tokio::time::timeout(
        Duration::from_secs(5),
        login.ensure_fresh(crate::provider_login::LoginProvider::Claude),
    )
    .await
    .unwrap_or_else(|_| login.login(crate::provider_login::LoginProvider::Claude));
    state.state == ProviderLoginState::LoggedIn && state.method == ProviderLoginMethod::ClaudeAi
}

#[cfg(test)]
mod tests;
