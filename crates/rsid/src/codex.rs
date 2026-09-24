mod live_context;

use crate::bedrock;
use crate::claude::{LaunchConfig, StreamEvent};
use crate::config::RuntimeConfig;
use crate::error::{DaemonError, Result};
use crate::model_control::CliExecutionCapability;
use crate::model_control::registry::RuntimeExecutionRoute;
use crate::openrouter::{OpenRouterCodexConfigOverrides, openrouter_credential_from_env};
use crate::pioneer::{
    PioneerCodexConfigOverrides, PioneerCredentialSource, existing_pioneer_codex_catalog_path,
    pioneer_credential_from_env, pioneer_launch_model,
};
use crate::process_control::{
    BoundedLineError, BoundedLines, CaptureLimits, PROVIDER_MAX_LINE_BYTES,
    PROVIDER_MAX_STDERR_BYTES, ProcessContainment, capture_bounded, configure_tokio_process_group,
    terminate_process_group,
};
#[cfg(test)]
use crate::provider_capabilities::parse_codex_catalog_snapshot;
use crate::provider_capabilities::{
    CatalogRefreshReason, CodexCatalogRefresh, MAX_VALIDATED_CONTEXT_TOKENS,
    ProviderCapabilityRegistry, provider_capabilities,
};
use serde_json::Value;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader as StdBufReader, Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};
use walkdir::WalkDir;

const CODEX_TRANSCRIPT_FIELD_MAX_BYTES: usize = 128 * 1024;
const CODEX_TRANSCRIPT_EVIDENCE_MAX_BYTES: usize = 1024 * 1024;
const CODEX_CONFIG_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);
const CODEX_CONFIG_PROBE_MAX_OUTPUT_BYTES: usize = 64 * 1024;
const CODEX_TRANSCRIPT_MAX_CUSTOM_ITEMS: usize = 512;
const CODEX_TRANSCRIPT_WATERMARK_TAIL_BYTES: u64 = 4 * 1024;
const CODEX_FATAL_STDERR_MAX_BYTES: usize = 4 * 1024;
pub(crate) const CODEX_STORAGE_FULL_ERROR_CLASS: &str = "codex_storage_full";
pub(crate) const CODEX_STORAGE_FULL_PROVIDER_EVENT_TYPE: &str = "stderr.codex_storage_full";
pub(crate) const CODEX_STORAGE_FULL_STOP_REASON: &str = "provider_error:codex_storage_full";
const CODEX_USAGE_LIMIT_MESSAGE_PREFIX: &str = "You've hit your usage limit.";
pub(crate) const CODEX_USAGE_LIMIT_ERROR_CLASS: &str = "codex_usage_limit";
pub(crate) const CODEX_USAGE_LIMIT_STOP_REASON: &str = "provider_error:codex_usage_limit";

/// Wraps a running Codex CLI process.
pub struct CodexProcess {
    child: Child,
}

/// Sandbox policy for fresh `codex exec` launches.
///
/// `exec resume` inherits the sandbox policy from the original session, so this
/// only affects new Codex sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexSandboxMode {
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

impl CodexSandboxMode {
    pub(crate) fn cli_arg(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::DangerFullAccess => "danger-full-access",
        }
    }

    pub(crate) fn parse(raw: &str) -> Option<Self> {
        let normalized = raw.trim().to_ascii_lowercase().replace('_', "-");
        match normalized.as_str() {
            "read-only" => Some(Self::ReadOnly),
            "workspace-write" => Some(Self::WorkspaceWrite),
            "danger-full-access" => Some(Self::DangerFullAccess),
            _ => None,
        }
    }
}

impl CodexProcess {
    /// Send SIGINT to gracefully interrupt the session.
    pub fn interrupt(&self) -> Result<()> {
        if let Some(pid) = self.child.id() {
            nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid as i32),
                nix::sys::signal::Signal::SIGINT,
            )
            .map_err(|e| DaemonError::Process(format!("Failed to send SIGINT: {}", e)))?;
        }
        Ok(())
    }

    /// Force kill the process.
    pub async fn kill(&mut self) -> Result<()> {
        self.child.kill().await?;
        Ok(())
    }

    /// Non-blocking check if the process has exited.
    pub fn try_wait(&mut self) -> Result<Option<std::process::ExitStatus>> {
        Ok(self.child.try_wait()?)
    }
}

/// Client for interacting with Codex CLI.
///
/// Holds an `Arc<RuntimeConfig>` so each `launch()` reads the current
/// `--sandbox` policy at spawn time. This means an `UpdateDaemonConfig` RPC
/// flipping `codex_sandbox_mode` takes effect on the next fresh Codex spawn
/// — no daemon restart required. `exec resume` continues to skip `--sandbox`
/// (it inherits the original session's policy from the Codex CLI side).
pub struct CodexClient {
    binary_path: PathBuf,
    agent_mcp_path: PathBuf,
    runtime_config: Arc<RuntimeConfig>,
}

impl CodexClient {
    pub fn new(runtime_config: Arc<RuntimeConfig>) -> Result<Self> {
        let binary_path = which::which("codex").map_err(|_| DaemonError::CodexBinaryNotFound)?;
        tracing::info!(
            path = %binary_path.display(),
            "Found Codex binary"
        );
        Ok(Self {
            binary_path,
            agent_mcp_path: std::env::current_exe()
                .map_err(|error| DaemonError::Process(format!("resolve rsid executable: {error}")))?
                .parent()
                .map(|directory| directory.join("rsi-agent-mcp"))
                .ok_or_else(|| {
                    DaemonError::Process("rsid executable has no parent directory".to_string())
                })?,
            runtime_config,
        })
    }

    pub fn is_available() -> bool {
        which::which("codex").is_ok()
    }

    /// Return model menu entries for the Codex provider.
    ///
    /// Prefer the installed Codex CLI's bundled model catalog so RSI tracks
    /// headless CLI model updates without a repo change. Fall back to the
    /// current static catalog for older binaries that do not expose
    /// `codex debug models`.
    pub async fn discover_models(&self) -> Vec<(String, String)> {
        self.discover_models_with_reason(CatalogRefreshReason::ExplicitDiscovery)
            .await
    }

    pub(crate) async fn discover_models_with_reason(
        &self,
        reason: CatalogRefreshReason,
    ) -> Vec<(String, String)> {
        let registry = provider_capabilities();
        match self.refresh_catalog(registry, reason).await {
            Ok(refresh) => {
                let models = refresh.snapshot().legacy_model_tuples();
                if models.is_empty() {
                    codex_fallback_models()
                } else {
                    prioritize_codex_models(models)
                }
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "Codex model catalog discovery failed; using last validated projection or static fallback",
                );
                registry
                    .cached_codex_catalog()
                    .map(|snapshot| snapshot.legacy_model_tuples())
                    .filter(|models| !models.is_empty())
                    .map(prioritize_codex_models)
                    .unwrap_or_else(codex_fallback_models)
            }
        }
    }

    pub(crate) async fn refresh_catalog(
        &self,
        registry: &ProviderCapabilityRegistry,
        reason: CatalogRefreshReason,
    ) -> Result<CodexCatalogRefresh> {
        let cli_version = match self.installed_version().await {
            Ok(version) => version,
            Err(error) => {
                registry.record_codex_probe_failure(None, None);
                return Err(error);
            }
        };
        if reason == CatalogRefreshReason::VersionChange
            && let Some(snapshot) = registry.current_codex_catalog_for_version(&cli_version)
        {
            return Ok(CodexCatalogRefresh::Reused(snapshot));
        }

        let raw = match self.discover_model_catalog_bytes().await {
            Ok(raw) => raw,
            Err(error) => {
                registry.record_codex_probe_failure(Some(cli_version), None);
                return Err(error);
            }
        };
        let confirmed_version = match self.installed_version().await {
            Ok(version) => version,
            Err(error) => {
                registry.record_codex_probe_failure(Some(cli_version), Some(&raw));
                return Err(error);
            }
        };
        if confirmed_version != cli_version {
            registry.record_codex_probe_failure(Some(confirmed_version.clone()), Some(&raw));
            return Err(DaemonError::Process(format!(
                "Codex CLI version changed during catalog discovery ({cli_version} -> {confirmed_version})"
            )));
        }
        registry.refresh_codex_catalog(&cli_version, &raw, chrono::Utc::now())
    }

    async fn installed_version(&self) -> Result<String> {
        let mut command = Command::new(&self.binary_path);
        command.arg("--version").env(
            rsi_common::identity::ENV_PROCESS_OWNERSHIP_NAMESPACE,
            rsi_common::identity::process_ownership_namespace(),
        );
        let output = capture_bounded(
            command,
            CaptureLimits::catalog(),
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .map_err(|error| DaemonError::Process(format!("Codex version probe failed: {error}")))?;
        if !output.status.success() {
            return Err(DaemonError::Process(format!(
                "codex --version exited with {}",
                output.status
            )));
        }
        let version = String::from_utf8(output.stdout).map_err(|error| {
            DaemonError::Process(format!("codex --version emitted non-UTF8 output: {error}"))
        })?;
        let version = version.trim();
        if version.is_empty() {
            return Err(DaemonError::Process(
                "codex --version emitted an empty version".to_string(),
            ));
        }
        Ok(version.to_string())
    }

    async fn discover_model_catalog_bytes(&self) -> Result<Vec<u8>> {
        let mut command = Command::new(&self.binary_path);
        command.args(["debug", "models", "--bundled"]).env(
            rsi_common::identity::ENV_PROCESS_OWNERSHIP_NAMESPACE,
            rsi_common::identity::process_ownership_namespace(),
        );
        let output = capture_bounded(
            command,
            CaptureLimits::catalog(),
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .map_err(|error| {
            DaemonError::Process(format!("codex model catalog probe failed: {error}"))
        })?;

        if !output.status.success() {
            return Err(DaemonError::Process(format!(
                "codex debug models --bundled exited with {}",
                output.status
            )));
        }

        Ok(output.stdout)
    }

    /// Build the `codex exec [...]` Command — separated from `launch()` so
    /// tests can inspect the argv without spawning a real codex binary.
    /// The sandbox arg is read from `self.runtime_config` at call time, so
    /// runtime mutations are reflected on the next spawn.
    pub(crate) fn build_cmd(&self, config: &LaunchConfig) -> Result<Command> {
        let pioneer_launch = if config.provider == Some(rsi_common::types::SessionProvider::Pioneer)
        {
            let credential = pioneer_credential_from_env()
                .map_err(|error| DaemonError::Process(error.to_string()))?;
            Some((credential.source(), existing_pioneer_codex_catalog_path()))
        } else {
            None
        };
        let openrouter_launch =
            if config.provider == Some(rsi_common::types::SessionProvider::OpenRouter) {
                Some(
                    openrouter_credential_from_env()
                        .map_err(|error| DaemonError::Process(error.to_string()))?,
                )
            } else {
                None
            };
        let bedrock_launch = if config.provider == Some(rsi_common::types::SessionProvider::Bedrock)
        {
            Some((
                bedrock::region().map_err(DaemonError::Process)?,
                bedrock::credential().map_err(DaemonError::Process)?,
            ))
        } else {
            None
        };
        let mut command = self.build_cmd_with_custom_provider(
            config,
            pioneer_launch
                .as_ref()
                .map(|(source, path)| (*source, path.as_deref())),
            openrouter_launch.as_deref(),
            bedrock_launch.as_ref().map(|(region, _)| region.as_str()),
        )?;
        if let Some((_, credential)) = bedrock_launch {
            command.env(bedrock::BEDROCK_ENV, credential);
        }
        Ok(command)
    }

    fn build_cmd_with_custom_provider(
        &self,
        config: &LaunchConfig,
        pioneer_launch: Option<(PioneerCredentialSource, Option<&Path>)>,
        openrouter_credential: Option<&str>,
        bedrock_region: Option<&str>,
    ) -> Result<Command> {
        let mut cmd = Command::new(&self.binary_path);
        cmd.arg("exec");

        let is_resume = config.resume_session_id.is_some();

        if let Some(resume_id) = &config.resume_session_id {
            // `codex exec resume <SESSION_ID> [PROMPT]` only reads stdin
            // when PROMPT is the literal "-".  Without it, the daemon's
            // piped stdin is silently ignored and the resumed session
            // never sees the follow-up message.
            cmd.args(["resume", resume_id, "-"]);
        }

        cmd.args(["--json", "--skip-git-repo-check"]);

        // `--sandbox` is only valid on the top-level `exec` subcommand.
        // `exec resume` rejects it ("unexpected argument '--sandbox'") and
        // inherits the original session's sandbox configuration.
        if !is_resume {
            let mode_str = self.runtime_config.codex_sandbox_mode.read().clone();
            // Defensive fallback — `update_field` validates inputs, so this
            // should never trip in practice.
            let mode =
                CodexSandboxMode::parse(&mode_str).unwrap_or(CodexSandboxMode::WorkspaceWrite);
            cmd.args(["--sandbox", mode.cli_arg()]);
        }

        let model = if config.provider == Some(rsi_common::types::SessionProvider::Pioneer) {
            Some(pioneer_launch_model(config.model.as_deref()))
        } else {
            config.model.as_deref()
        };
        if let Some(model) = model {
            cmd.args(["-m", model]);
        }

        if let Some(effort) = validated_codex_reasoning_effort(config)? {
            cmd.args(["-c", &format!("model_reasoning_effort=\"{}\"", effort)]);
        }

        if let Some(tokens) = config.configured_context_window {
            validate_codex_configured_context_window(tokens)?;
            cmd.args(["-c", &format!("model_context_window={tokens}")]);
        }

        // Keep the MCP configuration session-ephemeral: Codex receives it as
        // ordered command-line overrides, never through `codex mcp add` or a
        // user configuration file. Resolve only the daemon-installed sibling,
        // not PATH, so a mutable workspace binary cannot become an authority
        // bridge for a tokened provider process.
        if config.rsi_session_id.is_some() {
            if !self.agent_mcp_path.is_absolute() || !self.agent_mcp_path.is_file() {
                return Err(DaemonError::Process(format!(
                    "trusted rsi-agent-mcp sibling is missing: {}",
                    self.agent_mcp_path.display()
                )));
            }
            let mcp_path = self.agent_mcp_path.to_string_lossy();
            cmd.arg("-c")
                .arg(format!("mcp_servers.rsi-agent.command={mcp_path:?}"))
                .arg("-c")
                .arg("mcp_servers.rsi-agent.args=[]");
            cmd.arg("-c").arg(format!(
                "mcp_servers.rsi-agent.env_vars=[\"{}\",\"{}\"]",
                rsi_common::identity::ENV_SESSION_TOKEN,
                rsi_common::identity::ENV_SOCKET
            ));
        }

        if config.provider == Some(rsi_common::types::SessionProvider::Pioneer) {
            let (credential_source, catalog_path) = pioneer_launch.ok_or_else(|| {
                DaemonError::Process(
                    "Pioneer credential source was not resolved before Codex launch".to_string(),
                )
            })?;
            PioneerCodexConfigOverrides::new(credential_source, catalog_path)
                .map_err(|error| DaemonError::Process(error.to_string()))?
                .append_to(&mut cmd);
        }
        if config.provider == Some(rsi_common::types::SessionProvider::Bedrock) {
            let region = bedrock_region.ok_or_else(|| {
                DaemonError::Process("Bedrock region was not resolved before Codex launch".into())
            })?;
            bedrock::CodexOverrides::for_launch(region, &self.binary_path, config.model.as_deref())
                .append_to(&mut cmd);
        }
        if config.provider == Some(rsi_common::types::SessionProvider::OpenRouter) {
            let credential = openrouter_credential.ok_or_else(|| {
                DaemonError::Process(
                    "OpenRouter credential was not resolved before Codex launch".to_string(),
                )
            })?;
            OpenRouterCodexConfigOverrides::new(credential).append_to(&mut cmd);
        }

        // Apply working directory at the process level so both `exec` and
        // `exec resume` work (resume subcommand does not accept `--cd`).
        if let Some(dir) = &config.working_dir {
            cmd.current_dir(dir);
        }

        crate::claude::stamp_execution_environment(&mut cmd, config, uuid::Uuid::nil())?;

        // The Codex CLI reads the prompt from stdin when stdin is not a TTY.
        // We pipe stdin and write the query in, then close it (EOF) so the
        // process doesn't block waiting for more input.
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        Ok(cmd)
    }

    #[cfg(test)]
    fn build_cmd_with_pioneer(
        &self,
        config: &LaunchConfig,
        pioneer_launch: Option<(PioneerCredentialSource, Option<&Path>)>,
    ) -> Result<Command> {
        self.build_cmd_with_custom_provider(config, pioneer_launch, None, None)
    }

    pub(crate) fn launch(
        &self,
        config: &LaunchConfig,
        execution: CliExecutionCapability,
    ) -> Result<(CodexProcess, mpsc::Receiver<StreamEvent>)> {
        let invocation_id = execution.invocation_id();
        let mut cmd = self.build_cmd(config)?;
        crate::claude::stamp_execution_environment(&mut cmd, config, invocation_id)?;
        configure_tokio_process_group(&mut cmd, ProcessContainment::Group)?;
        let is_resume = config.resume_session_id.is_some();
        let transcript_boundary =
            CodexTranscriptBoundary::for_launch(config.resume_session_id.as_deref());

        let mut child = execution
            .bind_command(RuntimeExecutionRoute::CodexCli, cmd)
            .spawn()?;
        let child_pid = child.id();

        // Write the query to stdin and close the pipe. First-turn only, the
        // compact agent-discovery nudge is PREPENDED here (never in
        // `config.query`, which becomes the stored user event).
        if let Some(mut stdin) = child.stdin.take() {
            let query = codex_stdin_payload(config, is_resume);
            tokio::spawn(async move {
                use tokio::io::AsyncWriteExt;
                let _ = stdin.write_all(query.as_bytes()).await;
                let _ = stdin.write_all(b"\n").await;
                // `stdin` drops here → EOF sent to the Codex process.
            });
        }

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| DaemonError::Process("Failed to capture stdout".to_string()))?;
        let stderr = child.stderr.take();

        let (event_tx, event_rx) = mpsc::channel(100);
        let stderr_correlation = Arc::new(Mutex::new(CodexStderrCorrelation::default()));
        let stdout_correlation = stderr_correlation.clone();
        let (stdout_done_tx, stdout_done_rx) = oneshot::channel();

        let stderr_tx = event_tx.clone();
        tokio::spawn(async move {
            let mut lines = BoundedLines::new(stdout, PROVIDER_MAX_LINE_BYTES);
            let mut thread_id: Option<String> = None;
            let mut context_reader =
                Some(live_context::LiveContextReader::new(&transcript_boundary));
            let mut context_tick = tokio::time::interval(std::time::Duration::from_secs(1));
            context_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                let next_line = live_context::next_line_with_context(
                    &mut lines,
                    &mut context_reader,
                    thread_id.as_deref(),
                    &mut context_tick,
                    &event_tx,
                )
                .await;
                let line = match next_line {
                    Ok(Some(line)) => line,
                    Ok(None) => break,
                    Err(error) => {
                        if let Some(pid) = child_pid {
                            terminate_process_group(nix::unistd::Pid::from_raw(pid as i32));
                        }
                        let error_class = if matches!(error, BoundedLineError::Exceeded { .. }) {
                            "provider_output_overflow"
                        } else {
                            "provider_output_read_error"
                        };
                        let _ = event_tx.try_send(StreamEvent {
                            event_type: "process_error".to_string(),
                            data: serde_json::json!({
                                "error": error.to_string(),
                                "source": "stdout",
                                "terminal": true,
                                "error_class": error_class,
                            }),
                        });
                        break;
                    }
                };
                if line.trim().is_empty() {
                    continue;
                }

                match serde_json::from_str::<Value>(&line) {
                    Ok(value) => {
                        // `codex exec --json` is an adapter over the app-server
                        // protocol. It records the core `event_msg/token_count`
                        // notification in the thread transcript, but emits only
                        // cumulative `turn.completed.usage` on stdout. Read the
                        // matching durable transcript on live polls and before each
                        // terminal event so the monitor receives the same
                        // current-window `last_token_usage` that Codex's own TUI
                        // uses for its context indicator.
                        if value.get("type").and_then(|v| v.as_str()) == Some("turn.completed")
                            && let Some(id) = thread_id.as_deref()
                        {
                            if let Some(snapshot) =
                                read_latest_codex_turn_snapshot_with_retry(id, &transcript_boundary)
                                    .await
                            {
                                let CodexTranscriptTurnSnapshot {
                                    context_event,
                                    custom_tool_events,
                                    failure_evidence,
                                    ..
                                } = snapshot;
                                if let Ok(mut correlation) = stdout_correlation.lock() {
                                    correlation.failure_evidence.extend(failure_evidence);
                                }

                                let mut send_failed = false;
                                for event in custom_tool_events {
                                    if event_tx.send(event).await.is_err() {
                                        send_failed = true;
                                        break;
                                    }
                                }
                                if send_failed {
                                    break;
                                }
                                let context_event =
                                    context_event.and_then(|event| match context_reader.as_mut() {
                                        Some(reader) => reader.accept(event),
                                        None => Some(event),
                                    });
                                if let Some(context_event) = context_event
                                    && event_tx.send(context_event).await.is_err()
                                {
                                    break;
                                }
                            }
                        }
                        if let Some(event) = map_codex_json_to_stream_event(&value, &mut thread_id)
                            && event_tx.send(event).await.is_err()
                        {
                            break;
                        }
                    }
                    Err(e) => {
                        let raw_line = bounded_codex_transcript_field(&line, 8 * 1024);
                        tracing::warn!(line = %raw_line, error = %e, "Failed to parse Codex JSONL event");
                        let error_event = StreamEvent {
                            event_type: "process_error".to_string(),
                            data: serde_json::json!({
                                "error": format!("Failed to parse Codex JSONL line: {}\nRaw line: {}", e, raw_line),
                                "source": "codex_parse",
                            }),
                        };
                        if event_tx.send(error_event).await.is_err() {
                            break;
                        }
                    }
                }
            }
            let _ = stdout_done_tx.send(());
        });

        // Capture stderr and surface errors as synthetic events
        if let Some(stderr) = stderr {
            tokio::spawn(async move {
                let mut lines = BoundedLines::new(stderr, PROVIDER_MAX_LINE_BYTES);
                let mut stderr_lines: Vec<String> = Vec::new();
                let mut stderr_bytes = 0_usize;
                let mut fatal_state = CodexFatalStderrState::default();

                loop {
                    let line = match lines.next_line().await {
                        Ok(Some(line)) => line,
                        Ok(None) => break,
                        Err(error) => {
                            if let Some(pid) = child_pid {
                                terminate_process_group(nix::unistd::Pid::from_raw(pid as i32));
                            }
                            let error_class = if matches!(error, BoundedLineError::Exceeded { .. })
                            {
                                "provider_output_overflow"
                            } else {
                                "provider_output_read_error"
                            };
                            let _ = stderr_tx.try_send(StreamEvent {
                                event_type: "process_error".to_string(),
                                data: serde_json::json!({
                                    "error": error.to_string(),
                                    "source": "stderr",
                                    "terminal": true,
                                    "error_class": error_class,
                                }),
                            });
                            return;
                        }
                    };
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    if let Some(reason) = codex_stderr_suppression_reason(trimmed) {
                        tracing::debug!(
                            line = %trimmed,
                            reason,
                            "Codex CLI: suppressed non-fatal stderr message"
                        );
                        continue;
                    }
                    let fatal_record = is_codex_storage_full_fatal_record(trimmed);
                    let companion_record = is_codex_storage_full_companion_record(trimmed);
                    let will_buffer =
                        !fatal_record && !(fatal_state.storage_full_emitted && companion_record);
                    if will_buffer {
                        let separator = usize::from(!stderr_lines.is_empty());
                        let next_bytes = stderr_bytes
                            .saturating_add(separator)
                            .saturating_add(line.len());
                        if next_bytes > PROVIDER_MAX_STDERR_BYTES {
                            if let Some(pid) = child_pid {
                                terminate_process_group(nix::unistd::Pid::from_raw(pid as i32));
                            }
                            let _ = stderr_tx.try_send(StreamEvent {
                                event_type: "process_error".to_string(),
                                data: serde_json::json!({
                                    "error": format!(
                                        "provider stderr exceeded {}-byte bound",
                                        PROVIDER_MAX_STDERR_BYTES
                                    ),
                                    "source": "stderr",
                                    "terminal": true,
                                    "error_class": "provider_output_overflow",
                                }),
                            });
                            return;
                        }
                        stderr_bytes = next_bytes;
                    }
                    if let Some(error) = fatal_state.consume(line, &mut stderr_lines) {
                        stderr_bytes = stderr_lines
                            .iter()
                            .enumerate()
                            .map(|(index, line)| line.len() + usize::from(index > 0))
                            .sum();
                        tracing::error!(
                            line = %error,
                            "Codex CLI rollout recorder exhausted storage"
                        );
                        if stderr_tx
                            .send(StreamEvent {
                                event_type: "process_error".to_string(),
                                data: serde_json::json!({
                                    "error": error,
                                    "source": "stderr",
                                    "terminal": true,
                                    "error_class": CODEX_STORAGE_FULL_ERROR_CLASS,
                                    "provider_event_type": CODEX_STORAGE_FULL_PROVIDER_EVENT_TYPE,
                                }),
                            })
                            .await
                            .is_err()
                        {
                            return;
                        }
                        continue;
                    }
                    if fatal_record {
                        tracing::debug!(
                            "Codex CLI: suppressed repeated terminal storage-full stderr"
                        );
                        continue;
                    }
                    if fatal_state.storage_full_emitted && companion_record {
                        tracing::debug!("Codex CLI: suppressed storage-full companion stderr");
                        continue;
                    }
                }

                // Stdout owns transcript reconciliation. Wait until it has
                // either completed or dropped so recoverable router stderr is
                // filtered only with evidence from this exact turn. A dropped
                // sender leaves the correlation empty and therefore fails open.
                let _ = stdout_done_rx.await;

                let records = group_codex_stderr_records(stderr_lines);
                let remaining = match stderr_correlation.lock() {
                    Ok(mut correlation) => {
                        filter_correlated_codex_stderr(records, &mut correlation)
                    }
                    Err(_) => records,
                };

                let diagnostic = summarize_codex_stderr_records(&remaining);
                for record in &remaining {
                    tracing::warn!(line = %record, "Codex CLI stderr");
                }

                if !diagnostic.records.is_empty() {
                    let _ = stderr_tx
                        .send(StreamEvent {
                            event_type: "process_error".to_string(),
                            data: serde_json::json!({
                                "error": diagnostic.rendered,
                                "source": "stderr",
                                "stderr_record_count": diagnostic.record_count,
                            }),
                        })
                        .await;
                }
            });
        }

        Ok((CodexProcess { child }, event_rx))
    }
}

/// Return the Codex home directory used for persisted rollout transcripts.
///
/// `codex exec` does not expose `ThreadTokenUsageUpdated` on its JSONL stdout,
/// but it persists the unmodified core event to `$CODEX_HOME/sessions`. Keeping
/// this lookup centralized also makes a custom `CODEX_HOME` work without a
/// daemon configuration knob.
fn codex_home_dir() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".codex")))
}

fn codex_sessions_dir() -> Option<PathBuf> {
    codex_home_dir().map(|home| home.join("sessions"))
}

pub(crate) fn validate_codex_configured_context_window(tokens: u64) -> Result<()> {
    if !(1..=MAX_VALIDATED_CONTEXT_TOKENS).contains(&tokens) {
        return Err(DaemonError::InvalidParam(format!(
            "configured Codex context window must be in 1..={MAX_VALIDATED_CONTEXT_TOKENS}"
        )));
    }
    Ok(())
}

/// Resolve the raw Codex context-window configuration that will govern a new
/// provider incarnation. A per-launch override wins. Otherwise, ask the
/// installed Codex app-server for its effective configuration at the exact
/// launch working directory rather than attempting to reproduce its managed,
/// cloud, profile, trust, and project-layer resolution in RSI.
///
/// A probe failure deliberately returns no override. This fails closed: RSI
/// does not promote a partially resolved lower-precedence value into `-c`, and
/// the subsequently launched Codex process retains its native configuration
/// semantics.
pub(crate) async fn load_codex_configured_context_window(
    explicit: Option<u64>,
    working_dir: &Path,
) -> Result<Option<u64>> {
    if let Some(tokens) = explicit {
        validate_codex_configured_context_window(tokens)?;
        return Ok(Some(tokens));
    }

    let Some(cwd) = codex_config_probe_cwd(working_dir) else {
        return Ok(None);
    };
    #[cfg(test)]
    if let Some(result) = take_forced_codex_config_probe_result_for_test(&cwd) {
        observe_codex_config_probe_cwd_for_test(&cwd);
        return Ok(result);
    }

    let Some(binary_path) = which::which("codex").ok() else {
        return Ok(None);
    };
    Ok(
        probe_codex_configured_context_window(
            &binary_path,
            &cwd,
            CodexConfigProbeLimits::default(),
        )
        .await,
    )
}

#[derive(Clone, Copy)]
struct CodexConfigProbeLimits {
    timeout: std::time::Duration,
    max_output_bytes: usize,
}

impl Default for CodexConfigProbeLimits {
    fn default() -> Self {
        Self {
            timeout: CODEX_CONFIG_PROBE_TIMEOUT,
            max_output_bytes: CODEX_CONFIG_PROBE_MAX_OUTPUT_BYTES,
        }
    }
}

/// Read one scalar from `codex app-server` without ever starting a thread or
/// submitting model input. The app-server itself owns all configuration-layer
/// semantics; RSI only trusts its merged `config/read` response.
async fn probe_codex_configured_context_window(
    binary_path: &Path,
    working_dir: &Path,
    limits: CodexConfigProbeLimits,
) -> Option<u64> {
    let cwd = codex_config_probe_cwd(working_dir)?;
    #[cfg(test)]
    observe_codex_config_probe_cwd_for_test(&cwd);
    let mut command = Command::new(binary_path);
    command
        .arg("app-server")
        .current_dir(&cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    configure_tokio_process_group(&mut command, ProcessContainment::Group).ok()?;

    let mut child = command.spawn().ok()?;
    let pgid = nix::unistd::Pid::from_raw(child.id()? as i32);
    let result = tokio::time::timeout(limits.timeout, async {
        let mut stdin = child.stdin.take()?;
        let stdout = child.stdout.take()?;
        let mut lines = BoundedLines::new(stdout, limits.max_output_bytes);
        let mut output_bytes = 0_usize;

        write_codex_config_probe_message(
            &mut stdin,
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {
                        "name": "rsi-config-probe",
                        "version": env!("CARGO_PKG_VERSION"),
                    }
                }
            }),
        )
        .await?;
        read_codex_config_probe_response(&mut lines, 1, &mut output_bytes, limits.max_output_bytes)
            .await?;

        write_codex_config_probe_message(
            &mut stdin,
            serde_json::json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized",
            }),
        )
        .await?;
        write_codex_config_probe_message(
            &mut stdin,
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "config/read",
                "params": {
                    "cwd": cwd,
                    "includeLayers": false,
                }
            }),
        )
        .await?;
        let response = read_codex_config_probe_response(
            &mut lines,
            2,
            &mut output_bytes,
            limits.max_output_bytes,
        )
        .await?;
        parse_codex_configured_context_window(&response)
    })
    .await
    .ok()
    .flatten();

    // The config/read probe is intentionally transient. Kill and reap its full
    // process group even after a valid response, so no helper process survives
    // to race the actual provider launch.
    terminate_process_group(pgid);
    let _ = child.start_kill();
    let _ = tokio::time::timeout(std::time::Duration::from_millis(250), child.wait()).await;
    result
}

fn codex_config_probe_cwd(working_dir: &Path) -> Option<PathBuf> {
    if working_dir.is_absolute() {
        Some(working_dir.to_path_buf())
    } else {
        std::env::current_dir()
            .ok()
            .map(|cwd| cwd.join(working_dir))
    }
}

async fn write_codex_config_probe_message(
    stdin: &mut (impl AsyncWrite + Unpin),
    message: Value,
) -> Option<()> {
    let mut serialized = serde_json::to_vec(&message).ok()?;
    serialized.push(b'\n');
    stdin.write_all(&serialized).await.ok()?;
    stdin.flush().await.ok()?;
    Some(())
}

async fn read_codex_config_probe_response<R>(
    lines: &mut BoundedLines<R>,
    expected_id: i64,
    output_bytes: &mut usize,
    max_output_bytes: usize,
) -> Option<Value>
where
    R: tokio::io::AsyncRead + Unpin,
{
    loop {
        let line = lines.next_line().await.ok()??;
        *output_bytes = output_bytes.checked_add(line.len().saturating_add(1))?;
        if *output_bytes > max_output_bytes {
            return None;
        }
        let response: Value = serde_json::from_str(&line).ok()?;
        if response.get("id").and_then(Value::as_i64) == Some(expected_id) {
            return response.get("result").cloned();
        }
    }
}

fn parse_codex_configured_context_window(response: &Value) -> Option<u64> {
    let tokens = response
        .get("config")?
        .get("model_context_window")?
        .as_u64()?;
    validate_codex_configured_context_window(tokens).ok()?;
    Some(tokens)
}

#[cfg(test)]
fn codex_config_probe_cwds_for_test() -> &'static Mutex<Vec<PathBuf>> {
    static OBSERVATIONS: std::sync::OnceLock<Mutex<Vec<PathBuf>>> = std::sync::OnceLock::new();
    OBSERVATIONS.get_or_init(|| Mutex::new(Vec::new()))
}

#[cfg(test)]
fn forced_codex_config_probe_results_for_test() -> &'static Mutex<HashMap<PathBuf, Option<u64>>> {
    static RESULTS: std::sync::OnceLock<Mutex<HashMap<PathBuf, Option<u64>>>> =
        std::sync::OnceLock::new();
    RESULTS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(test)]
fn observe_codex_config_probe_cwd_for_test(cwd: &Path) {
    codex_config_probe_cwds_for_test()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(cwd.to_path_buf());
}

/// Install one path-scoped effective-config response without depending on an
/// installed Codex binary or the developer's real config. The launch tests use
/// this after their ContextRead pause so the hook also proves the probe cannot
/// begin before custody authorization.
#[cfg(test)]
pub(crate) fn force_codex_config_probe_result_for_test(cwd: &Path, result: Option<u64>) {
    let cwd = codex_config_probe_cwd(cwd).expect("test Codex probe cwd resolves");
    forced_codex_config_probe_results_for_test()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(cwd, result);
}

#[cfg(test)]
pub(crate) fn clear_forced_codex_config_probe_result_for_test(cwd: &Path) -> bool {
    let cwd = codex_config_probe_cwd(cwd).expect("test Codex probe cwd resolves");
    forced_codex_config_probe_results_for_test()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&cwd)
        .is_some()
}

#[cfg(test)]
fn take_forced_codex_config_probe_result_for_test(cwd: &Path) -> Option<Option<u64>> {
    forced_codex_config_probe_results_for_test()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(cwd)
}

/// Removes and returns the matching probe observation. The path key keeps
/// parallel launch tests independent without making production probe behavior
/// configurable by tests.
#[cfg(test)]
pub(crate) fn take_codex_config_probe_cwd_for_test(cwd: &Path) -> bool {
    let mut observations = codex_config_probe_cwds_for_test()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(position) = observations.iter().position(|observed| observed == cwd) else {
        return false;
    };
    observations.remove(position);
    true
}

/// Locate the persisted rollout file for one Codex thread.
fn find_codex_session_transcript(thread_id: &str) -> Option<PathBuf> {
    let sessions_dir = codex_sessions_dir()?;
    if !sessions_dir.is_dir() {
        return None;
    }

    let suffix = format!("-{thread_id}.jsonl");
    WalkDir::new(sessions_dir)
        .max_depth(4)
        .into_iter()
        .filter_map(std::result::Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(&suffix))
        .filter_map(|entry| {
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((modified, entry.into_path()))
        })
        .max_by_key(|(modified, _)| *modified)
        .map(|(_, path)| path)
}

#[derive(Clone, Debug)]
struct CodexTranscriptWatermark {
    path: PathBuf,
    byte_offset: u64,
    device: u64,
    inode: u64,
    prefix_tail: Vec<u8>,
}

#[derive(Clone, Debug)]
enum CodexTranscriptBoundary {
    Fresh,
    Resume(CodexTranscriptWatermark),
    ResumeUnavailable,
}

impl CodexTranscriptBoundary {
    fn for_launch(resume_session_id: Option<&str>) -> Self {
        match resume_session_id {
            None => Self::Fresh,
            Some(thread_id) => codex_transcript_watermark(thread_id)
                .map(Self::Resume)
                .unwrap_or(Self::ResumeUnavailable),
        }
    }

    fn watermark(&self) -> Option<&CodexTranscriptWatermark> {
        match self {
            Self::Resume(watermark) => Some(watermark),
            Self::Fresh | Self::ResumeUnavailable => None,
        }
    }
}

fn codex_transcript_watermark(thread_id: &str) -> Option<CodexTranscriptWatermark> {
    let path = find_codex_session_transcript(thread_id)?;
    let mut file = File::open(&path).ok()?;
    let metadata = file.metadata().ok()?;
    let byte_offset = metadata.len();
    let prefix_tail_len = byte_offset.min(CODEX_TRANSCRIPT_WATERMARK_TAIL_BYTES);
    file.seek(SeekFrom::Start(byte_offset - prefix_tail_len))
        .ok()?;
    let mut prefix_tail = vec![0; usize::try_from(prefix_tail_len).ok()?];
    file.read_exact(&mut prefix_tail).ok()?;
    Some(CodexTranscriptWatermark {
        path,
        byte_offset,
        device: metadata.dev(),
        inode: metadata.ino(),
        prefix_tail,
    })
}

#[derive(Debug, Default)]
struct CodexTranscriptTurnSnapshot {
    turn_id: Option<String>,
    context_event: Option<StreamEvent>,
    custom_tool_events: Vec<StreamEvent>,
    failure_evidence: Vec<CodexToolFailureEvidence>,
    turn_complete: bool,
}

#[derive(Debug)]
struct CodexCustomToolCall {
    call_id: String,
    raw_name: String,
    input: String,
    nested_tools: Vec<String>,
}

#[derive(Debug)]
enum CodexCustomToolItem {
    Call(CodexCustomToolCall),
    Output { call_id: String, output: String },
}

#[derive(Debug)]
struct CodexToolFailureEvidence {
    nested_tools: Vec<String>,
    output: String,
}

#[derive(Debug, Default)]
struct CodexStderrCorrelation {
    failure_evidence: Vec<CodexToolFailureEvidence>,
}

impl CodexStderrCorrelation {
    fn consume_exact_failure(&mut self, tool: Option<&str>, payload: &str) -> bool {
        if payload.is_empty() {
            return false;
        }
        for evidence in &mut self.failure_evidence {
            if tool.is_some_and(|tool| !evidence.nested_tools.iter().any(|name| name == tool)) {
                continue;
            }
            let Some(start) = evidence.output.find(payload) else {
                continue;
            };
            let end = start + payload.len();
            evidence
                .output
                .replace_range(start..end, &" ".repeat(payload.len()));
            return true;
        }
        false
    }
}

fn bounded_codex_transcript_field(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… [truncated by RSI]", &value[..end])
}

fn codex_nested_tool_names(input: &str) -> Vec<String> {
    let mut names = Vec::new();
    for (offset, _) in input.match_indices("tools.") {
        let rest = &input[offset + "tools.".len()..];
        let name_len = rest
            .bytes()
            .take_while(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
            .count();
        if name_len == 0 {
            continue;
        }
        let name = &rest[..name_len];
        if !rest[name_len..].trim_start().starts_with('(') {
            continue;
        }
        if !names.iter().any(|existing| existing == name) {
            names.push(name.to_string());
        }
    }
    names
}

fn codex_custom_tool_output_text(output: &Value) -> String {
    match output {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| {
                block
                    .get("text")
                    .and_then(Value::as_str)
                    .or_else(|| block.as_str())
            })
            .collect::<Vec<_>>()
            .join(""),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn codex_failed_tool_evidence(payload: &Value) -> Option<CodexToolFailureEvidence> {
    let item = payload.get("item")?;
    if item.get("status").and_then(Value::as_str) != Some("failed") {
        return None;
    }
    let message = item.get("error")?.get("message")?.as_str()?.trim();
    if message.is_empty() {
        return None;
    }

    let nested_tools = item
        .get("tool")
        .and_then(Value::as_str)
        .into_iter()
        .map(str::to_string)
        .collect();
    Some(CodexToolFailureEvidence {
        nested_tools,
        output: bounded_codex_transcript_field(message, CODEX_TRANSCRIPT_EVIDENCE_MAX_BYTES),
    })
}

fn codex_turn_id(value: &Value) -> Option<&str> {
    value
        .get("payload")?
        .get("internal_chat_message_metadata_passthrough")?
        .get("turn_id")?
        .as_str()
}

fn codex_context_event(value: &Value, thread_id: &str) -> Option<StreamEvent> {
    if value.get("type").and_then(Value::as_str) != Some("event_msg") {
        return None;
    }
    let payload = value.get("payload")?;
    if payload.get("type").and_then(Value::as_str) != Some("token_count") {
        return None;
    }
    let info = payload.get("info")?.clone();
    info.get("last_token_usage")?
        .get("total_tokens")?
        .as_u64()?;
    Some(StreamEvent {
        event_type: "codex_token_count".to_string(),
        data: serde_json::json!({
            "session_id": thread_id,
            "info": info,
            "provider_event_type": "session_transcript.event_msg.token_count",
            "observed_at": value.get("timestamp"),
        }),
    })
}

fn codex_custom_tool_events(
    items: Vec<CodexCustomToolItem>,
    thread_id: &str,
) -> (Vec<StreamEvent>, Vec<CodexToolFailureEvidence>) {
    let mut calls: HashMap<String, (String, Vec<String>)> = HashMap::new();
    for item in &items {
        if let CodexCustomToolItem::Call(call) = item {
            let display_name = if call.nested_tools.len() == 1 {
                call.nested_tools[0].clone()
            } else {
                call.raw_name.clone()
            };
            calls.insert(
                call.call_id.clone(),
                (display_name, call.nested_tools.clone()),
            );
        }
    }

    // CommandExecution and other supported items already arrive on exec JSONL
    // stdout. Project only the recoverable custom-tool failures that the
    // official adapter omits, otherwise every successful code-mode command
    // would appear twice in RSI.
    let mut qualifying_calls = HashMap::new();
    for item in &items {
        let CodexCustomToolItem::Output { call_id, output } = item else {
            continue;
        };
        let Some((display_name, nested_tools)) = calls.get(call_id) else {
            continue;
        };
        let is_recoverable = (nested_tools.iter().any(|name| name == "apply_patch")
            && output.contains("apply_patch verification failed:"))
            || (nested_tools.iter().any(|name| name == "write_stdin")
                && output.contains("write_stdin failed:"));
        if is_recoverable {
            qualifying_calls.insert(
                call_id.clone(),
                (display_name.clone(), nested_tools.clone()),
            );
        }
    }

    let mut events = Vec::new();
    let mut evidence = Vec::new();

    for item in items {
        match item {
            CodexCustomToolItem::Call(call) => {
                let Some((display_name, _)) = qualifying_calls.get(&call.call_id) else {
                    continue;
                };
                events.push(StreamEvent {
                    event_type: "tool_use".to_string(),
                    data: serde_json::json!({
                        "session_id": thread_id,
                        "name": display_name,
                        "input": {
                            "code": bounded_codex_transcript_field(
                                &call.input,
                                CODEX_TRANSCRIPT_FIELD_MAX_BYTES,
                            ),
                        },
                        "call_id": call.call_id.clone(),
                        "provider_event_type": "session_transcript.response_item.custom_tool_call",
                    }),
                });
            }
            CodexCustomToolItem::Output { call_id, output } => {
                let Some((display_name, nested_tools)) = qualifying_calls.get(&call_id) else {
                    continue;
                };
                events.push(StreamEvent {
                    event_type: "tool_result".to_string(),
                    data: serde_json::json!({
                        "session_id": thread_id,
                        "name": display_name,
                        "content": bounded_codex_transcript_field(
                            &output,
                            CODEX_TRANSCRIPT_FIELD_MAX_BYTES,
                        ),
                        "call_id": call_id,
                        "provider_event_type": "session_transcript.response_item.custom_tool_call_output",
                    }),
                });
                evidence.push(CodexToolFailureEvidence {
                    nested_tools: nested_tools.clone(),
                    output: bounded_codex_transcript_field(
                        &output,
                        CODEX_TRANSCRIPT_EVIDENCE_MAX_BYTES,
                    ),
                });
            }
        }
    }

    (events, evidence)
}

fn latest_codex_turn_snapshot_from_values(
    values: impl IntoIterator<Item = Value>,
    thread_id: &str,
) -> CodexTranscriptTurnSnapshot {
    let mut context_event = None;
    let mut latest_turn_id: Option<String> = None;
    let mut custom_items = Vec::new();
    let mut typed_failure_evidence = Vec::new();
    let mut turn_complete = false;

    for value in values {
        if let Some(event) = codex_context_event(&value, thread_id) {
            context_event = Some(event);
        }

        let outer_type = value.get("type").and_then(Value::as_str);
        let payload = value.get("payload");
        let payload_type = payload
            .and_then(|payload| payload.get("type"))
            .and_then(Value::as_str);

        if outer_type == Some("event_msg") && payload_type == Some("task_started") {
            latest_turn_id = payload
                .and_then(|payload| payload.get("turn_id"))
                .and_then(Value::as_str)
                .map(str::to_string);
            context_event = None;
            custom_items.clear();
            typed_failure_evidence.clear();
            turn_complete = false;
            continue;
        }

        let Some(active_turn_id) = latest_turn_id.as_deref() else {
            continue;
        };
        if outer_type == Some("event_msg") && payload_type == Some("task_complete") {
            if payload
                .and_then(|payload| payload.get("turn_id"))
                .and_then(Value::as_str)
                == Some(active_turn_id)
            {
                turn_complete = true;
            }
            continue;
        }
        if outer_type == Some("event_msg") && payload_type == Some("item_completed") {
            if payload
                .and_then(|payload| payload.get("turn_id"))
                .and_then(Value::as_str)
                == Some(active_turn_id)
                && typed_failure_evidence.len() < CODEX_TRANSCRIPT_MAX_CUSTOM_ITEMS
                && let Some(evidence) = payload.and_then(codex_failed_tool_evidence)
            {
                typed_failure_evidence.push(evidence);
            }
            continue;
        }
        if outer_type != Some("response_item")
            || codex_turn_id(&value) != Some(active_turn_id)
            || custom_items.len() >= CODEX_TRANSCRIPT_MAX_CUSTOM_ITEMS
        {
            continue;
        }

        let Some(payload) = payload else {
            continue;
        };
        match payload_type {
            Some("custom_tool_call") => {
                let Some(call_id) = payload.get("call_id").and_then(Value::as_str) else {
                    continue;
                };
                let raw_name = payload
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("custom_tool")
                    .to_string();
                let input = payload
                    .get("input")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let mut nested_tools = codex_nested_tool_names(&input);
                if nested_tools.is_empty()
                    && matches!(raw_name.as_str(), "apply_patch" | "write_stdin")
                {
                    nested_tools.push(raw_name.clone());
                }
                custom_items.push(CodexCustomToolItem::Call(CodexCustomToolCall {
                    call_id: call_id.to_string(),
                    raw_name,
                    input,
                    nested_tools,
                }));
            }
            Some("custom_tool_call_output") => {
                let Some(call_id) = payload.get("call_id").and_then(Value::as_str) else {
                    continue;
                };
                custom_items.push(CodexCustomToolItem::Output {
                    call_id: call_id.to_string(),
                    output: payload
                        .get("output")
                        .map(codex_custom_tool_output_text)
                        .unwrap_or_default(),
                });
            }
            _ => {}
        }
    }

    let (custom_tool_events, mut failure_evidence) =
        codex_custom_tool_events(custom_items, thread_id);
    failure_evidence.extend(typed_failure_evidence);
    CodexTranscriptTurnSnapshot {
        turn_id: latest_turn_id,
        context_event,
        custom_tool_events,
        failure_evidence,
        turn_complete,
    }
}

#[cfg(test)]
fn latest_codex_turn_snapshot_from_transcript(
    transcript: &std::path::Path,
    thread_id: &str,
) -> Option<CodexTranscriptTurnSnapshot> {
    latest_codex_turn_snapshot_from_transcript_after(transcript, thread_id, None)
}

fn latest_codex_turn_snapshot_from_transcript_after(
    transcript: &std::path::Path,
    thread_id: &str,
    watermark: Option<&CodexTranscriptWatermark>,
) -> Option<CodexTranscriptTurnSnapshot> {
    let mut file = File::open(transcript).ok()?;
    if let Some(watermark) = watermark {
        let metadata = file.metadata().ok()?;
        if watermark.path != transcript
            || metadata.len() < watermark.byte_offset
            || metadata.dev() != watermark.device
            || metadata.ino() != watermark.inode
        {
            return None;
        }
        let prefix_tail_len = u64::try_from(watermark.prefix_tail.len()).ok()?;
        file.seek(SeekFrom::Start(
            watermark.byte_offset.checked_sub(prefix_tail_len)?,
        ))
        .ok()?;
        let mut prefix_tail = vec![0; watermark.prefix_tail.len()];
        file.read_exact(&mut prefix_tail).ok()?;
        if prefix_tail != watermark.prefix_tail {
            return None;
        }
        file.seek(SeekFrom::Start(watermark.byte_offset)).ok()?;
    }
    let reader = StdBufReader::new(file);
    let values = reader
        .lines()
        .map_while(std::result::Result::ok)
        .filter_map(|line| serde_json::from_str::<Value>(&line).ok());
    Some(latest_codex_turn_snapshot_from_values(values, thread_id))
}

/// Extract the newest raw current-window token event from a rollout transcript.
#[cfg(test)]
fn latest_codex_context_event_from_transcript(
    transcript: &std::path::Path,
    thread_id: &str,
) -> Option<StreamEvent> {
    latest_codex_turn_snapshot_from_transcript(transcript, thread_id)?.context_event
}

/// The transcript is flushed independently of the JSONL renderer. A tiny,
/// bounded retry closes that race without ever using cumulative turn usage as a
/// substitute for the live context measurement.
async fn read_latest_codex_turn_snapshot_with_retry(
    thread_id: &str,
    boundary: &CodexTranscriptBoundary,
) -> Option<CodexTranscriptTurnSnapshot> {
    const TRANSCRIPT_READ_ATTEMPTS: usize = 3;
    if matches!(boundary, CodexTranscriptBoundary::ResumeUnavailable) {
        return None;
    }
    let watermark = boundary.watermark();
    let mut latest = None;

    for attempt in 0..TRANSCRIPT_READ_ATTEMPTS {
        let id = thread_id.to_string();
        let watermark = watermark.cloned();
        let snapshot = tokio::task::spawn_blocking(move || {
            let transcript = find_codex_session_transcript(&id)?;
            latest_codex_turn_snapshot_from_transcript_after(&transcript, &id, watermark.as_ref())
        })
        .await
        .ok()
        .flatten();
        if snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.turn_id.is_some() && snapshot.turn_complete)
        {
            return snapshot;
        }
        if snapshot.is_some() {
            latest = snapshot;
        }
        if attempt + 1 < TRANSCRIPT_READ_ATTEMPTS {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    latest
}

fn is_codex_timestamped_stderr_record_start(line: &str) -> bool {
    let Some((timestamp, remainder)) = line.split_once(char::is_whitespace) else {
        return false;
    };
    if chrono::DateTime::parse_from_rfc3339(timestamp).is_err() {
        return false;
    }
    matches!(
        remainder.split_whitespace().next(),
        Some("TRACE" | "DEBUG" | "INFO" | "WARN" | "ERROR")
    )
}

fn codex_timestamped_error_payload(line: &str) -> Option<&str> {
    if !is_codex_timestamped_stderr_record_start(line) {
        return None;
    }
    let (_, remainder) = line.split_once(char::is_whitespace)?;
    Some(remainder.strip_prefix("ERROR")?.trim_start())
}

fn is_codex_storage_full_fatal_record(record: &str) -> bool {
    let Some(payload) = codex_timestamped_error_payload(record) else {
        return false;
    };
    payload.starts_with("codex_rollout::recorder:")
        && payload.contains("No space left on device (os error 28)")
        && payload.contains("error_kind=StorageFull")
        && payload.contains("raw_os_error=Some(28)")
}

fn is_codex_storage_full_companion_record(record: &str) -> bool {
    let Some(payload) = codex_timestamped_error_payload(record) else {
        return false;
    };
    payload.starts_with(
        "codex_core::session: failed to record rollout items: thread-store internal error: \
         No space left on device (os error 28)",
    )
}

#[derive(Debug, Default)]
struct CodexFatalStderrState {
    storage_full_emitted: bool,
}

impl CodexFatalStderrState {
    fn consume(&mut self, line: String, buffered: &mut Vec<String>) -> Option<String> {
        let trimmed = line.trim();
        if is_codex_storage_full_fatal_record(trimmed) {
            if self.storage_full_emitted {
                return None;
            }
            self.storage_full_emitted = true;
            buffered.retain(|prior| !is_codex_storage_full_companion_record(prior.trim()));
            return Some(bounded_codex_transcript_field(
                trimmed,
                CODEX_FATAL_STDERR_MAX_BYTES,
            ));
        }
        if self.storage_full_emitted && is_codex_storage_full_companion_record(trimmed) {
            return None;
        }
        buffered.push(line);
        None
    }
}

fn group_codex_stderr_records(lines: Vec<String>) -> Vec<String> {
    let mut records: Vec<(String, bool)> = Vec::new();
    for line in lines {
        let timestamped = is_codex_timestamped_stderr_record_start(&line);
        if !timestamped && let Some((record, true)) = records.last_mut() {
            record.push('\n');
            record.push_str(&line);
            continue;
        }
        records.push((line, timestamped));
    }
    records.into_iter().map(|(record, _)| record).collect()
}

/// Compact consecutive timestamped records that have an identical payload.
///
/// Codex can retry a recoverable background operation many times in one
/// provider turn. Rendering each timestamped retry in the session transcript
/// turns one useful diagnostic into a wall of identical text. Keep the first
/// complete record as evidence, retain the final timestamp, and expose the
/// original record count separately to the session-detail heading.
struct CodexStderrDiagnostic {
    records: Vec<String>,
    rendered: String,
    record_count: usize,
}

fn summarize_codex_stderr_records(records: &[String]) -> CodexStderrDiagnostic {
    struct Run {
        first: String,
        payload: String,
        count: usize,
        last_timestamp: Option<String>,
    }

    let record_count = records.len();
    let mut runs: Vec<Run> = Vec::new();
    for record in records {
        let payload = codex_timestamped_error_payload(&record)
            .unwrap_or(record.as_str())
            .to_string();
        let timestamp = codex_stderr_timestamp(&record);
        if let Some(run) = runs.last_mut()
            && run.payload == payload
        {
            run.count += 1;
            run.last_timestamp = timestamp;
            continue;
        }
        runs.push(Run {
            first: record.clone(),
            payload,
            count: 1,
            last_timestamp: timestamp,
        });
    }

    let records = runs
        .into_iter()
        .map(|run| match (run.count, run.last_timestamp) {
            (1, _) => run.first,
            (count, Some(last_timestamp)) => format!(
                "{}\n[repeated {} additional times; last at {}]",
                run.first,
                count - 1,
                last_timestamp
            ),
            (count, None) => format!("{}\n[repeated {} additional times]", run.first, count - 1),
        })
        .collect::<Vec<_>>();
    let rendered = records.join("\n");

    CodexStderrDiagnostic {
        records,
        rendered,
        record_count,
    }
}

fn codex_stderr_timestamp(line: &str) -> Option<String> {
    is_codex_timestamped_stderr_record_start(line)
        .then(|| {
            line.split_once(char::is_whitespace)
                .map(|(timestamp, _)| timestamp.to_string())
        })
        .flatten()
}

fn recoverable_codex_router_failure(record: &str) -> Option<(Option<&'static str>, &str)> {
    const ROUTER_MARKER: &str = "codex_core::tools::router: error=";
    let marker = record.find(ROUTER_MARKER)?;
    if !record[..marker]
        .split_whitespace()
        .any(|part| part == "ERROR")
    {
        return None;
    }
    let payload = record[marker + ROUTER_MARKER.len()..].trim_end();
    let tool = if payload.starts_with("apply_patch verification failed:") {
        Some("apply_patch")
    } else if payload.starts_with("write_stdin failed:") {
        Some("write_stdin")
    } else {
        None
    };
    Some((tool, payload))
}

fn filter_correlated_codex_stderr(
    records: Vec<String>,
    correlation: &mut CodexStderrCorrelation,
) -> Vec<String> {
    records
        .into_iter()
        .filter(|record| {
            let Some((tool, payload)) = recoverable_codex_router_failure(record) else {
                return true;
            };
            if correlation.consume_exact_failure(tool, payload) {
                tracing::debug!(
                    tool = tool.unwrap_or("typed_tool_failure"),
                    payload,
                    "Codex CLI: suppressed transcript-correlated recoverable tool stderr"
                );
                false
            } else {
                true
            }
        })
        .collect()
}

pub(crate) fn codex_fallback_models() -> Vec<(String, String)> {
    vec![
        ("gpt-6-sol".to_string(), "GPT-6-Sol".to_string()),
        ("gpt-6-luna".to_string(), "GPT-6-Luna".to_string()),
        ("gpt-6-astra".to_string(), "GPT-6-Astra".to_string()),
        ("gpt-5.5".to_string(), "GPT-5.5".to_string()),
        ("gpt-5.2".to_string(), "GPT-5.2".to_string()),
    ]
}

/// Put released GPT-6 entries before the installed catalog without removing
/// any catalog-provided model. This keeps a lagging CLI usable while exposing
/// the newest models immediately.
fn prioritize_codex_models(models: Vec<(String, String)>) -> Vec<(String, String)> {
    let preferred = [
        ("gpt-6-sol", "GPT-6-Sol"),
        ("gpt-6-luna", "GPT-6-Luna"),
        ("gpt-6-astra", "GPT-6-Astra"),
    ];
    let mut seen = std::collections::HashSet::new();
    preferred
        .into_iter()
        .map(|(slug, label)| (slug.to_string(), label.to_string()))
        .chain(models)
        .filter(|(slug, _)| seen.insert(slug.to_ascii_lowercase()))
        .collect()
}

#[cfg(test)]
pub(crate) fn parse_codex_model_catalog(raw: &str) -> Result<Vec<(String, String)>> {
    Ok(
        parse_codex_catalog_snapshot("legacy-projection", raw.as_bytes(), chrono::Utc::now())?
            .legacy_model_tuples(),
    )
}

/// Compose the exact byte payload written to the Codex CLI's piped stdin.
///
/// First-turn only (`!is_resume`), the binary-embedded agent-discovery nudge,
/// thoughts-artifact commit policy, and daemon-message convention are PREPENDED
/// so a fresh Codex session on any project receives the same durable-artifact
/// and daemon-attribution contracts as providers with a
/// developer-instructions channel. A session-specific custody instruction, when
/// present, is sent on both initial and resumed turns because Codex has no
/// system-prompt channel. `config.query` is never mutated; this is a pure
/// function over it, so the stored user event stays clean.
pub(crate) fn codex_stdin_payload(config: &LaunchConfig, is_resume: bool) -> String {
    // `system_prompt` has historically not been a Codex CLI channel. Only
    // forward the daemon's dedicated sandbox instruction; passing arbitrary
    // caller system prompts here would silently change ordinary Codex launches.
    let sandbox_custody_instruction = config
        .system_prompt
        .as_deref()
        .filter(|prompt| prompt.starts_with("## RSI sandbox custody (HARD)"));
    if is_resume {
        match sandbox_custody_instruction {
            Some(instruction) => format!("{instruction}\n\n{}", config.query),
            None => config.query.clone(),
        }
    } else {
        let mut parts = vec![
            crate::session::preamble::AGENT_DISCOVERY_NUDGE,
            crate::session::preamble::THOUGHTS_COMMIT_POLICY,
            crate::session::preamble::DAEMON_MESSAGE_CONVENTION,
        ];
        if let Some(instruction) = sandbox_custody_instruction {
            parts.push(instruction);
        }
        parts.push(&config.query);
        parts.join("\n\n")
    }
}

pub(crate) fn codex_reasoning_effort(effort: Option<&str>) -> Option<&'static str> {
    match effort {
        Some("low") => Some("low"),
        Some("medium") => Some("medium"),
        Some("high") => Some("high"),
        Some("xhigh") => Some("xhigh"),
        Some("max") => Some("max"),
        Some("ultra") => Some("ultra"),
        _ => None,
    }
}

fn validated_codex_reasoning_effort(config: &LaunchConfig) -> Result<Option<&'static str>> {
    let Some(raw_effort) = config.effort.as_deref() else {
        return Ok(None);
    };
    let effort = codex_reasoning_effort(Some(raw_effort)).ok_or_else(|| {
        DaemonError::InvalidParam(format!("unsupported Codex reasoning effort '{raw_effort}'"))
    })?;

    // A missing model deliberately remains compatible with the configured
    // effective model, which is not available at this adapter boundary. An
    // explicit custom/future model also passes through when RSI has no bundled
    // capability data for it; the Codex CLI remains the authority in that case.
    if let Some(model) = config.model.as_deref()
        && let Some(ladder) = rsi_common::model_utils::known_codex_effort_ladder(model)
        && !ladder.contains(&effort)
    {
        return Err(DaemonError::InvalidParam(format!(
            "reasoning effort '{effort}' is unsupported for Codex model '{model}'"
        )));
    }

    Ok(Some(effort))
}

/// Non-fatal Codex rollout-recorder diagnostic.
///
/// The recorder can be handed a thread id that no longer exists and then
/// reports `failed to record rollout items: thread <id> not found`. The turn
/// itself is healthy and the tool result the operator needs is already in the
/// transcript, so rendering this internal bookkeeping line as an assistant
/// "Provider diagnostic" is pure noise (#67).
///
/// This is deliberately narrower than the fatal storage-full recorder failure
/// (#68): `thread-store internal error: No space left on device (os error 28)`
/// keeps its own signature and must stay visible and terminal. Requiring the
/// exact `thread <id> not found` shape cannot match that payload, because
/// `thread-store` carries no space after `thread`.
fn is_codex_nonfatal_rollout_recorder_diagnostic(line: &str) -> bool {
    let payload = codex_timestamped_error_payload(line).unwrap_or_else(|| line.trim());
    let Some(thread) =
        payload.strip_prefix("codex_core::session: failed to record rollout items: thread ")
    else {
        return false;
    };
    let Some(thread_id) = thread.strip_suffix(" not found") else {
        return false;
    };
    !thread_id.is_empty() && !thread_id.contains(' ')
}

pub(crate) fn codex_stderr_suppression_reason(line: &str) -> Option<&'static str> {
    let trimmed = line.trim();
    if trimmed.starts_with("Reading ") && trimmed.ends_with("from stdin...") {
        return Some("stdin prompt read message");
    }
    if trimmed.starts_with("Ignored unsupported project-local config keys ") {
        return Some("unsupported project-local config keys warning");
    }
    if trimmed.contains("[features].collab") && trimmed.contains("deprecated") {
        return Some("deprecated collab feature flag");
    }
    if is_codex_nonfatal_rollout_recorder_diagnostic(trimmed) {
        return Some("non-fatal rollout recorder thread-not-found diagnostic");
    }
    None
}

fn is_codex_usage_limit_error(message: &str) -> bool {
    // Compaction failures are wrapped by the CLI before they are emitted as
    // `turn.failed`; classify that known envelope using the same exact provider
    // message prefix as top-level `error` events.
    let message = message
        .trim()
        .strip_prefix("Error running remote compact task: ")
        .unwrap_or(message.trim());
    message == CODEX_USAGE_LIMIT_MESSAGE_PREFIX
        || message
            .strip_prefix(CODEX_USAGE_LIMIT_MESSAGE_PREFIX)
            .and_then(|suffix| suffix.chars().next())
            .is_some_and(char::is_whitespace)
}

pub(crate) fn map_codex_json_to_stream_event(
    value: &Value,
    thread_id: &mut Option<String>,
) -> Option<StreamEvent> {
    map_codex_json_to_stream_event_inner(value, thread_id, true)
}

fn map_codex_json_to_stream_event_inner(
    value: &Value,
    thread_id: &mut Option<String>,
    classify_cli_usage_limit: bool,
) -> Option<StreamEvent> {
    let event_type = value.get("type")?.as_str()?;

    match event_type {
        "thread.started" => {
            let tid = value.get("thread_id")?.as_str()?.to_string();
            *thread_id = Some(tid.clone());
            Some(StreamEvent {
                event_type: "system".to_string(),
                data: serde_json::json!({
                    "subtype": "init",
                    "session_id": tid,
                    "provider_event_type": "thread.started",
                }),
            })
        }
        "item.started" => {
            let item = value.get("item")?;
            if item.get("type").and_then(|v| v.as_str()) == Some("command_execution") {
                Some(StreamEvent {
                    event_type: "tool_use".to_string(),
                    data: serde_json::json!({
                        "session_id": thread_id.clone(),
                        "name": "shell",
                        "input": {
                            "command": item.get("command").and_then(|v| v.as_str()).unwrap_or(""),
                        },
                        "provider_event_type": "item.started",
                    }),
                })
            } else {
                None
            }
        }
        "item.completed" => {
            let item = value.get("item")?;
            match item.get("type").and_then(|v| v.as_str()) {
                Some("agent_message") => {
                    let text = item
                        .get("text")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    Some(StreamEvent {
                        event_type: "assistant".to_string(),
                        data: serde_json::json!({
                            "session_id": thread_id.clone(),
                            "message": {
                                "role": "assistant",
                                "content": [{"type":"text", "text": text}],
                            },
                            "provider_event_type": "item.completed",
                        }),
                    })
                }
                Some("command_execution") => {
                    let output = item
                        .get("aggregated_output")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let mut data = serde_json::json!({
                        "session_id": thread_id.clone(),
                        "content": output,
                        "provider_event_type": "item.completed",
                        "exit_code": item.get("exit_code"),
                    });
                    if let Some(exit_code) = item.get("exit_code").and_then(Value::as_i64) {
                        data["is_error"] = Value::Bool(exit_code != 0);
                    }
                    Some(StreamEvent {
                        event_type: "tool_result".to_string(),
                        data,
                    })
                }
                Some("error") => {
                    let message = item
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("Codex emitted item.completed error with no message field")
                        .to_string();
                    let mut data = serde_json::json!({
                        "error": message,
                        "source": "codex_event",
                        "provider_event_type": "item.completed.error",
                    });
                    if classify_cli_usage_limit {
                        // Codex CLI emits item-level `error` items for
                        // warnings and diagnostics (ignored config settings,
                        // missing model metadata for non-OpenAI models, ...),
                        // often before `turn.started`. They never fail the
                        // turn: terminal evidence is `turn.failed` or process
                        // exit (issue #662, sibling of #603's `error` path).
                        data.as_object_mut()
                            .expect("Codex CLI item error data is an object")
                            .insert("terminal".to_string(), Value::Bool(false));
                    }
                    Some(StreamEvent {
                        event_type: "process_error".to_string(),
                        data,
                    })
                }
                _ => None,
            }
        }
        "event_msg" => {
            let payload = value.get("payload")?;
            if payload.get("type").and_then(|v| v.as_str()) != Some("token_count") {
                return None;
            }
            Some(StreamEvent {
                event_type: "codex_token_count".to_string(),
                data: serde_json::json!({
                    "session_id": thread_id.clone(),
                    "info": payload.get("info").cloned().unwrap_or_else(|| serde_json::json!({})),
                    "provider_event_type": "event_msg.token_count",
                }),
            })
        }
        "turn.completed" => {
            let usage = value
                .get("usage")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}));
            // Codex CLI reports `cached_input_tokens` as cumulative provider
            // telemetry across internal model calls. It is not a Claude-style
            // additive cache-read field for the current live context. Feeding it
            // into `extract_token_usage` as `cache_read_input_tokens` makes the
            // context numerator jump into the millions and clamp to 100%.
            //
            // Keep only the non-cached remainder for turn metrics and omit cache
            // fields so downstream confidence is Partial rather than Full. Live
            // context fill comes from Codex `event_msg/token_count` telemetry,
            // which reports current-window usage instead of cumulative usage.
            let reported_input = usage
                .get("input_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let cached = usage
                .get("cached_input_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let context_input = if cached > 0 && cached <= reported_input {
                reported_input - cached
            } else {
                reported_input
            };
            Some(StreamEvent {
                event_type: "result".to_string(),
                data: serde_json::json!({
                    "session_id": thread_id.clone(),
                    "usage": {
                        "input_tokens": context_input,
                        // Codex CLI 0.133 includes `reasoning_output_tokens` in
                        // the JSONL stream. It is completion accounting, not
                        // context-window input, so context % continues to use
                        // the normalized input token estimate above.
                        "output_tokens": usage.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                    },
                    "subtype": "turn_completed",
                    "provider_event_type": "turn.completed",
                }),
            })
        }
        "error" => {
            let message = match value.get("message").and_then(|v| v.as_str()) {
                Some(msg) => msg.to_string(),
                None => {
                    tracing::warn!(value = %value, "Codex 'error' event missing 'message' field");
                    "Codex emitted error event with no message field".to_string()
                }
            };
            let usage_limited = classify_cli_usage_limit && is_codex_usage_limit_error(&message);
            let mut data = serde_json::json!({
                "error": message,
                "source": "codex_event",
                "provider_event_type": "error",
            });
            if classify_cli_usage_limit {
                data.as_object_mut()
                    .expect("Codex CLI error data is an object")
                    .insert("terminal".to_string(), Value::Bool(false));
            }
            if usage_limited {
                data.as_object_mut()
                    .expect("Codex error data is an object")
                    .insert(
                        "error_class".to_string(),
                        Value::String(CODEX_USAGE_LIMIT_ERROR_CLASS.to_string()),
                    );
            }
            Some(StreamEvent {
                event_type: "process_error".to_string(),
                data,
            })
        }
        "turn.failed" => {
            let error_obj = value.get("error");
            let message = error_obj
                .and_then(|v| v.get("message"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("{:?}", error_obj));
            let usage_limited = classify_cli_usage_limit && is_codex_usage_limit_error(&message);
            let mut data = serde_json::json!({
                "error": message,
                "source": "codex_event",
                "provider_event_type": "turn.failed",
            });
            if usage_limited {
                data.as_object_mut()
                    .expect("Codex turn.failed data is an object")
                    .insert(
                        "error_class".to_string(),
                        Value::String(CODEX_USAGE_LIMIT_ERROR_CLASS.to_string()),
                    );
            }
            Some(StreamEvent {
                event_type: "process_error".to_string(),
                data,
            })
        }
        // Known no-op signal — emitted between item.completed and the next event.
        // No diagnostic value, intentionally silent (no warn log).
        "turn.started" => None,
        _ => {
            tracing::warn!(
                event_type = %event_type,
                value = ?value,
                "Codex CLI: unrecognized event type — please update map_codex_json_to_stream_event"
            );
            None
        }
    }
}

/// Alias for use by the app-server module to normalize compatible events.
pub(crate) fn map_app_server_compatible_event(
    value: &Value,
    thread_id: &mut Option<String>,
) -> Option<StreamEvent> {
    map_codex_json_to_stream_event_inner(value, thread_id, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pioneer::PIONEER_DEFAULT_MODEL;

    #[test]
    fn sandbox_mode_parse_accepts_known_values() {
        assert_eq!(
            CodexSandboxMode::parse("read-only"),
            Some(CodexSandboxMode::ReadOnly)
        );
        assert_eq!(
            CodexSandboxMode::parse("workspace-write"),
            Some(CodexSandboxMode::WorkspaceWrite)
        );
        assert_eq!(
            CodexSandboxMode::parse("danger-full-access"),
            Some(CodexSandboxMode::DangerFullAccess)
        );
    }

    #[test]
    fn sandbox_mode_parse_accepts_underscore_aliases() {
        assert_eq!(
            CodexSandboxMode::parse("workspace_write"),
            Some(CodexSandboxMode::WorkspaceWrite)
        );
        assert_eq!(
            CodexSandboxMode::parse("danger_full_access"),
            Some(CodexSandboxMode::DangerFullAccess)
        );
    }

    /// Build a fresh `RuntimeConfig` for tests. Seeds `codex_sandbox_mode` to the
    /// supplied value; everything else defaults via `Config::from_env()`.
    fn runtime_config_with_sandbox_mode(mode: &str) -> Arc<RuntimeConfig> {
        let mut config = crate::config::Config::from_env();
        config.codex_sandbox_mode = mode.to_string();
        RuntimeConfig::from_config(&config)
    }

    /// Build a minimal `LaunchConfig` for cmd-inspection tests. All other
    /// fields default to `None`/`false` — only `query` is mandatory.
    fn launch_config_minimal(resume: Option<String>) -> LaunchConfig {
        LaunchConfig {
            query: "noop".to_string(),
            title: None,
            agent_role: None,
            epic_spawn_ordinal: None,
            working_dir: None,
            provider: None,
            model: None,
            configured_context_window: None,
            max_turns: None,
            system_prompt: None,
            resume_session_id: resume,
            session_kind: None,
            project_id: None,
            rsi_session_id: None,
            rsi_socket: None,
            rsi_session_token: None,
            continued_from: None,
            openai_base_url: None,
            openai_api_key: None,
            conversation_history: None,
            workflow_id: None,
            workflow_id_override: None,
            max_retries: None,
            group_id: None,
            parent_id: None,
            effort: None,
            issue_identifier: None,
            issue_url: None,
            issue_tracker_id: None,
            scheduled_job_id: None,
            model_invocation_owner: None,
            model_invocation_dedup_key: None,
            model_invocation_request_fingerprint: None,
            skip_project_model_default: false,
            model_invocation_purpose:
                rsi_common::model_control::ModelInvocationPurpose::SessionLaunchFresh,
            sandbox: None,
            cargo_target_dir: None,
            execution_scratch: None,
            is_eval: false,
            skip_context_pipeline: false,
            capability_class: None,
            tags: vec![],
            topology_node_id: None,
            topology_iteration: 0,
            closure_selector: None,
        }
    }

    /// Build a CodexClient with a stub binary path — `build_cmd` never spawns,
    /// so the path's existence is irrelevant; only argv is inspected.
    fn codex_client_for_test(rc: Arc<RuntimeConfig>) -> CodexClient {
        CodexClient {
            binary_path: PathBuf::from("/usr/bin/true"),
            agent_mcp_path: PathBuf::from("/usr/bin/true"),
            runtime_config: rc,
        }
    }

    /// Pull args out of a `tokio::process::Command` as owned `String`s.
    fn cmd_args(cmd: &Command) -> Vec<String> {
        cmd.as_std()
            .get_args()
            .map(|s| s.to_string_lossy().into_owned())
            .collect()
    }

    /// Look up an env var stamped onto a `tokio::process::Command`.
    fn cmd_env<'a>(cmd: &'a Command, key: &str) -> Option<String> {
        cmd.as_std().get_envs().find_map(|(k, v)| {
            if k.to_string_lossy() == key {
                Some(
                    v.map(|v| v.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                )
            } else {
                None
            }
        })
    }

    #[test]
    fn session_launch_injects_only_ephemeral_trusted_agent_mcp_config() {
        let rc = runtime_config_with_sandbox_mode("workspace-write");
        let client = codex_client_for_test(rc);
        let mut config = launch_config_minimal(None);
        config.rsi_session_id = Some(uuid::Uuid::new_v4());
        config.rsi_session_token = Some("must-not-appear-in-argv".to_string());
        let command = client.build_cmd(&config).unwrap();
        let args = cmd_args(&command);
        assert!(
            args.iter()
                .any(|arg| arg.contains("mcp_servers.rsi-agent.command"))
        );
        assert!(
            args.iter()
                .any(|arg| arg == "mcp_servers.rsi-agent.args=[]")
        );
        assert!(args.iter().any(|arg| *arg
            == format!(
                "mcp_servers.rsi-agent.env_vars=[\"{}\",\"{}\"]",
                rsi_common::identity::ENV_SESSION_TOKEN,
                rsi_common::identity::ENV_SOCKET
            )));
        assert!(
            args.iter()
                .all(|arg| !arg.contains("must-not-appear-in-argv"))
        );
        assert_eq!(
            cmd_env(&command, rsi_common::identity::ENV_SESSION_TOKEN).as_deref(),
            Some("must-not-appear-in-argv")
        );
    }

    #[test]
    fn bedrock_fresh_and_resume_commands_route_to_runtime() {
        let client = codex_client_for_test(runtime_config_with_sandbox_mode("workspace-write"));
        for resume in [None, Some("bedrock-thread-id".to_string())] {
            let mut config = launch_config_minimal(resume.clone());
            config.provider = Some(rsi_common::types::SessionProvider::Bedrock);
            config.model = Some("global.openai.gpt-5.6-sol".to_string());
            let command = client
                .build_cmd_with_custom_provider(&config, None, None, Some("us-west-1"))
                .unwrap();
            let args = cmd_args(&command);
            assert!(
                args.windows(2)
                    .any(|pair| pair == ["-m", "global.openai.gpt-5.6-sol"])
            );
            for value in bedrock::CodexOverrides::new("us-west-1").values() {
                assert!(
                    args.windows(2)
                        .any(|pair| pair[0] == "-c" && pair[1] == *value)
                );
            }
            assert_eq!(args.iter().any(|arg| arg == "resume"), resume.is_some());
            assert!(!args.join(" ").contains("bedrock-api-key-"));
        }
    }

    #[test]
    fn bedrock_launch_passes_key_only_in_child_environment() {
        temp_env::with_vars(
            [
                (bedrock::BEDROCK_ENV, Some("bedrock-api-key-test")),
                ("AWS_REGION", Some("us-west-1")),
            ],
            || {
                let client =
                    codex_client_for_test(runtime_config_with_sandbox_mode("workspace-write"));
                let mut config = launch_config_minimal(None);
                config.provider = Some(rsi_common::types::SessionProvider::Bedrock);
                let command = client.build_cmd(&config).unwrap();
                assert_eq!(
                    cmd_env(&command, bedrock::BEDROCK_ENV).as_deref(),
                    Some("bedrock-api-key-test")
                );
                assert!(
                    !cmd_args(&command)
                        .join(" ")
                        .contains("bedrock-api-key-test")
                );
            },
        );
    }

    #[test]
    fn pioneer_fresh_and_resume_commands_apply_secret_safe_codex_overrides() {
        let rc = runtime_config_with_sandbox_mode("workspace-write");
        let client = codex_client_for_test(rc);
        let directory = tempfile::tempdir().unwrap();
        let catalog_path = directory.path().join("pioneer.json");

        for resume in [None, Some("pioneer-thread-id".to_string())] {
            let mut config = launch_config_minimal(resume.clone());
            config.provider = Some(rsi_common::types::SessionProvider::Pioneer);
            let command = client
                .build_cmd_with_pioneer(
                    &config,
                    Some((PioneerCredentialSource::AiInference, Some(&catalog_path))),
                )
                .unwrap();
            let args = cmd_args(&command);

            assert!(
                args.windows(2)
                    .any(|pair| pair == ["-m", PIONEER_DEFAULT_MODEL])
            );
            for value in PioneerCodexConfigOverrides::new(
                PioneerCredentialSource::AiInference,
                Some(&catalog_path),
            )
            .unwrap()
            .values()
            {
                assert!(
                    args.windows(2)
                        .any(|pair| pair[0] == "-c" && pair[1] == *value)
                );
            }
            assert!(!args.join(" ").contains("fixture-credential-never-log"));
            assert_eq!(args.iter().any(|arg| arg == "resume"), resume.is_some());
        }
    }

    #[test]
    fn pioneer_command_preserves_explicit_model_and_requires_resolved_source() {
        let rc = runtime_config_with_sandbox_mode("workspace-write");
        let client = codex_client_for_test(rc);
        let mut config = launch_config_minimal(None);
        config.provider = Some(rsi_common::types::SessionProvider::Pioneer);
        config.model = Some("vendor/direct-model".to_string());

        let command = client
            .build_cmd_with_pioneer(&config, Some((PioneerCredentialSource::ApiKey, None)))
            .unwrap();
        let args = cmd_args(&command);
        assert!(
            args.windows(2)
                .any(|pair| pair == ["-m", "vendor/direct-model"])
        );
        assert!(!args.iter().any(|arg| arg == PIONEER_DEFAULT_MODEL));
        assert!(
            args.iter()
                .any(|arg| arg == "model_providers.pioneer.env_key=\"PIONEER_API_KEY\"")
        );

        let error = client.build_cmd_with_pioneer(&config, None).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("credential source was not resolved")
        );
    }

    #[test]
    fn build_cmd_injects_rsi_identity_env() {
        use rsi_common::identity;
        let rc = runtime_config_with_sandbox_mode("workspace-write");
        let client = codex_client_for_test(rc);
        let mut config = launch_config_minimal(None);
        let sid = uuid::Uuid::new_v4();
        config.rsi_session_id = Some(sid);
        config.rsi_socket = Some(PathBuf::from("/tmp/rsi-test.sock"));
        config.rsi_session_token = Some("tok-abc123".to_string());

        let cmd = client.build_cmd(&config).unwrap();
        assert_eq!(
            cmd_env(&cmd, identity::ENV_SESSION_ID).as_deref(),
            Some(sid.to_string().as_str())
        );
        assert_eq!(
            cmd_env(&cmd, identity::ENV_SOCKET).as_deref(),
            Some("/tmp/rsi-test.sock")
        );
        assert_eq!(
            cmd_env(&cmd, identity::ENV_SESSION_TOKEN).as_deref(),
            Some("tok-abc123")
        );
    }

    /// Issue #25: a designated build scratch is stamped into the subprocess
    /// env as `CARGO_TARGET_DIR`; absent designation leaves the env alone
    /// (non-sandboxed sessions keep the user's own cargo config).
    #[test]
    fn build_cmd_stamps_cargo_target_dir_only_when_designated() {
        use rsi_common::identity;
        let rc = runtime_config_with_sandbox_mode("workspace-write");
        let client = codex_client_for_test(rc);

        let mut config = launch_config_minimal(None);
        config.cargo_target_dir = Some(PathBuf::from("/sandboxes/abc/target"));
        let cmd = client.build_cmd(&config).unwrap();
        assert_eq!(
            cmd_env(&cmd, identity::ENV_CARGO_TARGET_DIR).as_deref(),
            Some("/sandboxes/abc/target")
        );

        let bare = launch_config_minimal(None);
        let cmd = client.build_cmd(&bare).unwrap();
        assert_eq!(cmd_env(&cmd, identity::ENV_CARGO_TARGET_DIR), None);
    }

    #[test]
    fn codex_and_pioneer_commands_stamp_authenticated_execution_scratch() {
        use crate::sandbox::execution_scratch::SandboxExecutionScratch;
        use rsi_common::identity;

        let base = std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap().join("target"))
            .join("slice8-codex-provider-fixtures");
        std::fs::create_dir_all(&base).unwrap();
        let root = tempfile::Builder::new()
            .prefix("scratch-")
            .tempdir_in(base)
            .unwrap();
        let scratch = SandboxExecutionScratch::prepare_for_test(root.path()).unwrap();
        let client = codex_client_for_test(runtime_config_with_sandbox_mode("workspace-write"));
        let session_id = uuid::Uuid::new_v4();
        let mut config = launch_config_minimal(None);
        config.provider = Some(rsi_common::types::SessionProvider::Pioneer);
        config.rsi_session_id = Some(session_id);
        config.execution_scratch = Some(scratch.clone());
        let command = client
            .build_cmd_with_pioneer(&config, Some((PioneerCredentialSource::ApiKey, None)))
            .unwrap();
        assert_eq!(
            cmd_env(&command, identity::ENV_CARGO_TARGET_DIR),
            Some(scratch.target().display().to_string())
        );
        assert_eq!(
            cmd_env(&command, identity::ENV_TMPDIR),
            Some(scratch.temp().display().to_string())
        );
        assert_eq!(
            cmd_env(&command, identity::ENV_SESSION_ID),
            Some(session_id.to_string())
        );
        assert!(cmd_env(&command, identity::ENV_MODEL_INVOCATION_ID).is_some());
        assert_eq!(
            cmd_env(&command, identity::ENV_PROCESS_OWNERSHIP_NAMESPACE),
            Some(identity::process_ownership_namespace())
        );
    }

    #[test]
    fn ordinary_codex_and_pioneer_environments_match_pre_slice8_ownership_absence() {
        use rsi_common::identity;

        let client = codex_client_for_test(runtime_config_with_sandbox_mode("workspace-write"));
        let config = launch_config_minimal(None);
        let codex = client.build_cmd(&config).unwrap();
        let pioneer = client
            .build_cmd_with_pioneer(&config, Some((PioneerCredentialSource::ApiKey, None)))
            .unwrap();
        for command in [&codex, &pioneer] {
            assert_eq!(cmd_env(command, identity::ENV_CARGO_TARGET_DIR), None);
            assert_eq!(cmd_env(command, identity::ENV_TMPDIR), None);
            assert_eq!(
                cmd_env(command, identity::ENV_PROCESS_OWNERSHIP_NAMESPACE),
                None
            );
        }
    }

    #[test]
    fn stdin_payload_prepends_nudge_first_turn_only() {
        use crate::session::preamble::{
            AGENT_DISCOVERY_NUDGE, DAEMON_MESSAGE_CONVENTION, THOUGHTS_COMMIT_POLICY,
        };
        let mut config = launch_config_minimal(None);
        config.query = "do the thing".to_string();

        // First turn (!is_resume): nudge prepended, query preserved verbatim.
        let first = codex_stdin_payload(&config, false);
        assert!(
            first.contains(AGENT_DISCOVERY_NUDGE),
            "first-turn stdin payload must carry the discovery nudge"
        );
        assert!(
            first.contains(THOUGHTS_COMMIT_POLICY),
            "first-turn stdin payload must carry the thoughts commit policy"
        );
        assert!(
            first.contains(DAEMON_MESSAGE_CONVENTION),
            "first-turn stdin payload must carry the daemon message convention"
        );
        assert!(first.contains("do the thing"));
        assert!(first.starts_with(AGENT_DISCOVERY_NUDGE));

        // Resume turn: verbatim query, no nudge re-injection.
        let resumed = codex_stdin_payload(&config, true);
        assert_eq!(resumed, "do the thing");
        assert!(!resumed.contains(AGENT_DISCOVERY_NUDGE));
        assert!(!resumed.contains(THOUGHTS_COMMIT_POLICY));

        // `config.query` (the stored user event) is never mutated.
        assert_eq!(config.query, "do the thing");
    }

    #[test]
    fn stdin_payload_carries_sandbox_custody_instruction_on_initial_and_resume() {
        let mut config = launch_config_minimal(None);
        config.query = "implement the assigned task".to_string();
        config.system_prompt = Some(crate::session::preamble::sandbox_custody_instruction(
            std::path::Path::new("/tmp/rsi-sandboxes/43ae018b"),
            "rsi/43ae018b",
        ));

        let initial = codex_stdin_payload(&config, false);
        assert!(initial.contains(crate::session::preamble::AGENT_DISCOVERY_NUDGE));
        assert!(initial.contains("/tmp/rsi-sandboxes/43ae018b"));
        assert!(initial.contains("rsi/43ae018b"));
        assert!(initial.ends_with("implement the assigned task"));

        let resumed = codex_stdin_payload(&config, true);
        assert!(resumed.starts_with("## RSI sandbox custody (HARD)"));
        assert!(resumed.contains("/tmp/rsi-sandboxes/43ae018b"));
        assert!(resumed.ends_with("implement the assigned task"));
    }

    #[test]
    fn parse_codex_model_catalog_keeps_visible_models_in_priority_order() {
        let raw = serde_json::json!({
            "models": [
                {
                    "slug": "hidden-model",
                    "display_name": "Hidden",
                    "visibility": "hide",
                    "priority": 1
                },
                {
                    "slug": "gpt-5.6-luna",
                    "display_name": "GPT-5.6-Luna",
                    "visibility": "list",
                    "priority": 1
                },
                {
                    "slug": "gpt-6-astra",
                    "display_name": "GPT-6-Astra",
                    "visibility": "list",
                    "priority": 2
                },
                {
                    "slug": "gpt-5.5",
                    "display_name": "GPT-5.5",
                    "visibility": "list",
                    "priority": 3
                }
            ]
        });

        let models = parse_codex_model_catalog(&raw.to_string()).unwrap();
        assert_eq!(
            models,
            vec![
                ("gpt-5.6-luna".to_string(), "GPT-5.6-Luna".to_string()),
                ("gpt-6-astra".to_string(), "GPT-6-Astra".to_string()),
                ("gpt-5.5".to_string(), "GPT-5.5".to_string()),
            ]
        );
    }

    #[test]
    fn codex_fallback_models_match_installed_visible_catalog() {
        assert_eq!(
            codex_fallback_models(),
            vec![
                ("gpt-6-sol".to_string(), "GPT-6-Sol".to_string()),
                ("gpt-6-luna".to_string(), "GPT-6-Luna".to_string()),
                ("gpt-6-astra".to_string(), "GPT-6-Astra".to_string()),
                ("gpt-5.5".to_string(), "GPT-5.5".to_string()),
                ("gpt-5.2".to_string(), "GPT-5.2".to_string()),
            ]
        );
    }

    #[test]
    fn prioritize_codex_models_keeps_every_discovered_entry_after_gpt_6() {
        let models = prioritize_codex_models(vec![
            ("gpt-5.6-sol".to_string(), "GPT-5.6-Sol".to_string()),
            ("gpt-6-astra".to_string(), "Catalog Astra".to_string()),
            ("gpt-5.5".to_string(), "GPT-5.5".to_string()),
        ]);
        assert_eq!(
            models,
            vec![
                ("gpt-6-sol".to_string(), "GPT-6-Sol".to_string()),
                ("gpt-6-luna".to_string(), "GPT-6-Luna".to_string()),
                ("gpt-6-astra".to_string(), "GPT-6-Astra".to_string()),
                ("gpt-5.6-sol".to_string(), "GPT-5.6-Sol".to_string()),
                ("gpt-5.5".to_string(), "GPT-5.5".to_string()),
            ]
        );
    }

    #[test]
    fn parse_codex_model_catalog_uses_abbreviated_display_fallback() {
        let raw = serde_json::json!({
            "models": [
                {
                    "slug": "gpt-6-astra",
                    "display_name": "",
                    "visibility": "list",
                    "priority": 1
                }
            ]
        });

        let models = parse_codex_model_catalog(&raw.to_string()).unwrap();
        assert_eq!(
            models,
            vec![("gpt-6-astra".to_string(), "GPT-6-Astra".to_string())]
        );
    }

    #[test]
    fn launch_cmd_emits_sandbox_arg_from_runtime_config() {
        let rc = runtime_config_with_sandbox_mode("danger-full-access");
        let client = codex_client_for_test(rc);
        let cmd = client.build_cmd(&launch_config_minimal(None)).unwrap();
        let args = cmd_args(&cmd);
        let pair = args
            .windows(2)
            .find(|w| w[0] == "--sandbox")
            .expect("--sandbox arg must be present on fresh launch");
        assert_eq!(pair[1], "danger-full-access");
    }

    #[test]
    fn launch_cmd_emits_workspace_write_default() {
        let rc = runtime_config_with_sandbox_mode("workspace-write");
        let client = codex_client_for_test(rc);
        let cmd = client.build_cmd(&launch_config_minimal(None)).unwrap();
        let args = cmd_args(&cmd);
        let pair = args.windows(2).find(|w| w[0] == "--sandbox").unwrap();
        assert_eq!(pair[1], "workspace-write");
    }

    #[test]
    fn launch_cmd_suppresses_sandbox_arg_on_resume() {
        let rc = runtime_config_with_sandbox_mode("danger-full-access");
        let client = codex_client_for_test(rc);
        let cmd = client
            .build_cmd(&launch_config_minimal(Some("abc-123".to_string())))
            .unwrap();
        let args = cmd_args(&cmd);
        assert!(
            !args.iter().any(|a| a == "--sandbox"),
            "resume must NOT pass --sandbox (codex inherits parent session policy)"
        );
        // Resume positional args still present.
        assert!(args.iter().any(|a| a == "resume"));
        assert!(args.iter().any(|a| a == "abc-123"));
    }

    #[test]
    fn launch_cmd_reflects_runtime_mutation() {
        // Seed workspace-write, mutate at runtime to read-only, expect build_cmd
        // to reflect the new value on the next call.
        let rc = runtime_config_with_sandbox_mode("workspace-write");
        rc.update_field("codex_sandbox_mode", &serde_json::json!("read-only"))
            .expect("update_field should accept canonical value");
        let client = codex_client_for_test(rc);
        let cmd = client.build_cmd(&launch_config_minimal(None)).unwrap();
        let args = cmd_args(&cmd);
        let pair = args.windows(2).find(|w| w[0] == "--sandbox").unwrap();
        assert_eq!(pair[1], "read-only");
    }

    #[test]
    fn launch_cmd_falls_back_to_workspace_write_on_corrupt_runtime_value() {
        // Directly corrupt the lock to simulate a stale unvalidated value.
        // The launch path must NOT crash; it falls back to the safe default.
        let rc = runtime_config_with_sandbox_mode("workspace-write");
        *rc.codex_sandbox_mode.write() = "this-is-not-a-mode".to_string();
        let client = codex_client_for_test(rc);
        let cmd = client.build_cmd(&launch_config_minimal(None)).unwrap();
        let args = cmd_args(&cmd);
        let pair = args.windows(2).find(|w| w[0] == "--sandbox").unwrap();
        assert_eq!(pair[1], "workspace-write");
    }

    #[test]
    fn launch_cmd_emits_codex_reasoning_effort_config() {
        let rc = runtime_config_with_sandbox_mode("workspace-write");
        let client = codex_client_for_test(rc);
        let mut config = launch_config_minimal(None);
        config.effort = Some("high".to_string());

        let cmd = client.build_cmd(&config).unwrap();
        let args = cmd_args(&cmd);
        let pair = args
            .windows(2)
            .find(|w| w[0] == "-c")
            .expect("-c config override must be present when effort is set");

        assert_eq!(pair[1], "model_reasoning_effort=\"high\"");
    }

    #[test]
    fn launch_cmd_emits_raw_configured_context_window() {
        let rc = runtime_config_with_sandbox_mode("workspace-write");
        let client = codex_client_for_test(rc);
        let mut config = launch_config_minimal(None);
        config.configured_context_window = Some(400_000);

        let cmd = client.build_cmd(&config).unwrap();
        let args = cmd_args(&cmd);
        assert!(
            args.windows(2)
                .any(|pair| pair[0] == "-c" && pair[1] == "model_context_window=400000"),
            "the resolved raw window must be frozen into the provider argv: {args:?}"
        );
    }

    fn write_codex_config_probe_server(
        directory: &Path,
        expected_cwd: &Path,
        config_response: &str,
    ) -> (PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;

        let path = directory.join("fake-codex-config-probe");
        let request_path = directory.join("config-read-request.json");
        std::fs::write(
            &path,
            format!(
                r#"#!/bin/sh
while IFS= read -r request; do
    case "$request" in
        *'"method":"initialize"'*)
            printf '%s\n' '{{"jsonrpc":"2.0","id":1,"result":{{}}}}'
            ;;
        *'"method":"config/read"'*)
            printf '%s' "$request" > '{}'
            case "$request" in
                *'"cwd":"{}"'*) printf '%s\n' '{}' ;;
            esac
            ;;
    esac
done
"#,
                request_path.display(),
                expected_cwd.display(),
                config_response,
            ),
        )
        .expect("write fake config probe server");
        let mut permissions = std::fs::metadata(&path)
            .expect("config probe server metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).expect("make config probe server executable");
        (path, request_path)
    }

    fn config_read_response(tokens: u64) -> String {
        format!(
            r#"{{"jsonrpc":"2.0","id":2,"result":{{"config":{{"model_context_window":{tokens}}}}}}}"#
        )
    }

    fn short_config_probe_limits() -> CodexConfigProbeLimits {
        CodexConfigProbeLimits {
            timeout: std::time::Duration::from_millis(250),
            max_output_bytes: 4 * 1024,
        }
    }

    #[tokio::test]
    async fn configured_context_window_uses_codex_effective_project_over_base_layer() {
        let directory = tempfile::tempdir().unwrap();
        let project = directory.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let (server, request) = write_codex_config_probe_server(
            directory.path(),
            &project,
            &config_read_response(200_000),
        );

        // The fake app-server reports Codex's merged project value (200k),
        // not the lower-precedence base value (400k). RSI does no file merge.
        assert_eq!(
            probe_codex_configured_context_window(&server, &project, short_config_probe_limits())
                .await,
            Some(200_000)
        );
        assert!(
            std::fs::read_to_string(request)
                .unwrap()
                .contains(&format!(r#""cwd":"{}""#, project.display()))
        );
    }

    #[tokio::test]
    async fn configured_context_window_uses_codex_effective_project_only_layer() {
        let directory = tempfile::tempdir().unwrap();
        let project = directory.path().join("project-only");
        std::fs::create_dir(&project).unwrap();
        let (server, _) = write_codex_config_probe_server(
            directory.path(),
            &project,
            &config_read_response(200_000),
        );

        assert_eq!(
            probe_codex_configured_context_window(&server, &project, short_config_probe_limits())
                .await,
            Some(200_000)
        );
    }

    #[tokio::test]
    async fn configured_context_window_uses_codex_effective_selected_profile_layer() {
        let directory = tempfile::tempdir().unwrap();
        let project = directory.path().join("profile-project");
        std::fs::create_dir(&project).unwrap();
        let (server, _) = write_codex_config_probe_server(
            directory.path(),
            &project,
            &config_read_response(300_000),
        );

        // Selected-profile semantics remain Codex-owned; RSI persists exactly
        // the effective scalar returned by config/read.
        assert_eq!(
            probe_codex_configured_context_window(&server, &project, short_config_probe_limits())
                .await,
            Some(300_000)
        );
    }

    #[tokio::test]
    async fn configured_context_window_uses_codex_effective_cwd_layer() {
        let directory = tempfile::tempdir().unwrap();
        let cwd = directory.path().join("nested-cwd");
        std::fs::create_dir(&cwd).unwrap();
        let (server, request) =
            write_codex_config_probe_server(directory.path(), &cwd, &config_read_response(150_000));

        assert_eq!(
            probe_codex_configured_context_window(&server, &cwd, short_config_probe_limits()).await,
            Some(150_000)
        );
        assert!(
            std::fs::read_to_string(request)
                .unwrap()
                .contains(&format!(r#""cwd":"{}""#, cwd.display()))
        );
    }

    #[tokio::test]
    async fn configured_context_window_validates_explicit_override_before_probe() {
        let probe_irrelevant_working_dir = Path::new("/definitely/not/a/codex-binary");
        assert_eq!(
            load_codex_configured_context_window(Some(400_000), probe_irrelevant_working_dir)
                .await
                .unwrap(),
            Some(400_000)
        );
        assert!(
            load_codex_configured_context_window(Some(0), probe_irrelevant_working_dir)
                .await
                .is_err()
        );
        assert!(
            load_codex_configured_context_window(
                Some(MAX_VALIDATED_CONTEXT_TOKENS + 1),
                probe_irrelevant_working_dir
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn configured_context_window_rejects_malformed_and_zero_probe_values() {
        let directory = tempfile::tempdir().unwrap();
        let malformed =
            r#"{"jsonrpc":"2.0","id":2,"result":{"config":{"model_context_window":"400000"}}}"#;
        let (server, _) =
            write_codex_config_probe_server(directory.path(), directory.path(), malformed);
        assert_eq!(
            probe_codex_configured_context_window(
                &server,
                directory.path(),
                short_config_probe_limits()
            )
            .await,
            None
        );

        let (server, _) = write_codex_config_probe_server(
            directory.path(),
            directory.path(),
            &config_read_response(0),
        );
        assert_eq!(
            probe_codex_configured_context_window(
                &server,
                directory.path(),
                short_config_probe_limits()
            )
            .await,
            None
        );
    }

    #[tokio::test]
    async fn configured_context_window_probe_failure_is_bounded_and_fails_closed() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let server = directory.path().join("hanging-codex-config-probe");
        std::fs::write(&server, "#!/bin/sh\nsleep 5\n").unwrap();
        let mut permissions = std::fs::metadata(&server).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&server, permissions).unwrap();

        let started = std::time::Instant::now();
        assert_eq!(
            probe_codex_configured_context_window(
                &server,
                directory.path(),
                short_config_probe_limits()
            )
            .await,
            None
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    #[test]
    fn launch_cmd_preserves_max_effort_for_codex() {
        let rc = runtime_config_with_sandbox_mode("workspace-write");
        let client = codex_client_for_test(rc);
        let mut config = launch_config_minimal(None);
        config.effort = Some("max".to_string());

        let cmd = client.build_cmd(&config).unwrap();
        let args = cmd_args(&cmd);
        let pair = args
            .windows(2)
            .find(|w| w[0] == "-c")
            .expect("-c config override must be present when effort is set");

        assert_eq!(pair[1], "model_reasoning_effort=\"max\"");
    }

    #[test]
    fn launch_cmd_validates_known_codex_model_effort_pairs_and_preserves_unknown_models() {
        let rc = runtime_config_with_sandbox_mode("workspace-write");
        let client = codex_client_for_test(rc);
        for (model, effort) in [
            ("gpt-6-astra", "low"),
            ("gpt-6-astra", "high"),
            ("gpt-6-astra", "ultra"),
        ] {
            let mut config = launch_config_minimal(None);
            config.model = Some(model.to_string());
            config.effort = Some(effort.to_string());
            let cmd = client.build_cmd(&config).unwrap();
            let args = cmd_args(&cmd);
            assert!(
                args.windows(2).any(|pair| {
                    pair[0] == "-c" && pair[1] == format!("model_reasoning_effort=\"{effort}\"")
                }),
                "{model} should accept {effort}"
            );
        }

        for (model, effort) in [
            ("gpt-6-astra", "turbo"),
            ("gpt-5.5", "max"),
            ("gpt-5.5", "ultra"),
        ] {
            let mut config = launch_config_minimal(None);
            config.model = Some(model.to_string());
            config.effort = Some(effort.to_string());
            assert!(matches!(
                client.build_cmd(&config),
                Err(DaemonError::InvalidParam(message)) if message.contains(model) && message.contains(effort)
            ));
        }

        // Without an explicit model, this adapter cannot know the configured
        // effective model, so retain the historical pass-through behavior.
        let mut config = launch_config_minimal(None);
        config.effort = Some("ultra".to_string());
        assert!(client.build_cmd(&config).is_ok());

        // Custom and future model IDs have no trustworthy bundled capability
        // data, so preserve their explicit effort for the CLI to validate.
        for (model, effort) in [
            ("custom-codex-model", "ultra"),
            ("gpt-5.3-codex", "max"),
            ("gpt-5.3-codex", "ultra"),
        ] {
            let mut config = launch_config_minimal(None);
            config.model = Some(model.to_string());
            config.effort = Some(effort.to_string());
            let cmd = client.build_cmd(&config).unwrap();
            let args = cmd_args(&cmd);
            assert!(args.windows(2).any(|pair| {
                pair[0] == "-c" && pair[1] == format!("model_reasoning_effort=\"{effort}\"")
            }));
        }
    }

    #[test]
    fn launch_cmd_rejects_invalid_codex_reasoning_effort() {
        let rc = runtime_config_with_sandbox_mode("workspace-write");
        let client = codex_client_for_test(rc);
        let mut config = launch_config_minimal(None);
        config.effort = Some("turbo".to_string());

        assert!(matches!(
            client.build_cmd(&config),
            Err(DaemonError::InvalidParam(message)) if message.contains("turbo")
        ));
    }

    #[test]
    fn test_turn_completed_ignores_cached_for_turn_metrics() {
        // Codex cached_input_tokens is cumulative provider telemetry, not an
        // additive live-context cache field. Keep only the non-cached remainder
        // and intentionally omit cache fields so extract_token_usage marks the
        // reading Partial.
        let value = serde_json::json!({
            "type": "turn.completed",
            "usage": {
                "input_tokens": 50000,
                "cached_input_tokens": 40000,
                "output_tokens": 5000,
                "reasoning_output_tokens": 1200,
            }
        });
        let mut thread_id = Some("thread-123".to_string());
        let event = map_codex_json_to_stream_event(&value, &mut thread_id).unwrap();

        assert_eq!(event.event_type, "result");
        assert_eq!(event.data.get("subtype").unwrap(), "turn_completed");

        let usage = event.data.get("usage").unwrap();
        // Non-cached portion: 50000 - 40000 = 10000
        assert_eq!(usage.get("input_tokens").unwrap(), 10000);
        assert!(usage.get("cache_creation_input_tokens").is_none());
        assert!(usage.get("cache_read_input_tokens").is_none());
        assert_eq!(usage.get("output_tokens").unwrap(), 5000);

        let extracted = crate::monitor::extract_token_usage(&event).unwrap();
        assert_eq!(extracted.total_input, 10000);
        assert_eq!(
            extracted.confidence,
            rsi_common::types::ContextUsageConfidence::Partial
        );
    }

    #[test]
    fn test_event_msg_token_count_maps_current_window_usage() {
        let value = serde_json::json!({
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {
                    "total_token_usage": {
                        "input_tokens": 422282u64,
                        "cached_input_tokens": 331904u64,
                        "output_tokens": 9366u64,
                        "total_tokens": 431648u64,
                    },
                    "last_token_usage": {
                        "input_tokens": 92833u64,
                        "cached_input_tokens": 84864u64,
                        "output_tokens": 261u64,
                        "total_tokens": 93094u64,
                    },
                    "model_context_window": 258400u64,
                }
            }
        });
        let mut thread_id = Some("thread-token-count".to_string());
        let event = map_codex_json_to_stream_event(&value, &mut thread_id).unwrap();

        assert_eq!(event.event_type, "codex_token_count");
        assert_eq!(
            event
                .data
                .get("provider_event_type")
                .and_then(|v| v.as_str()),
            Some("event_msg.token_count")
        );
        let usage = crate::monitor::extract_codex_context_usage(&event).unwrap();
        assert_eq!(usage.context_tokens, 93_094);
        assert_eq!(usage.output_tokens, 261);
        assert_eq!(usage.context_window, Some(258_400));
        assert_eq!(usage.cache_read_tokens, Some(84_864));
    }

    #[test]
    fn transcript_token_count_uses_latest_current_window_usage() {
        let tempdir = tempfile::tempdir().unwrap();
        let transcript = tempdir.path().join("rollout-thread-123.jsonl");
        std::fs::write(
            &transcript,
            concat!(
                "{\"type\":\"thread.started\",\"thread_id\":\"thread-123\"}\n",
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"total_tokens\":93094,\"output_tokens\":261},\"model_context_window\":258400}}}\n",
                "{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\"}}\n",
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"total_tokens\":130492,\"output_tokens\":640},\"model_context_window\":258400}}}\n"
            ),
        )
        .unwrap();

        let event = latest_codex_context_event_from_transcript(&transcript, "thread-123")
            .expect("latest token count should be forwarded");
        assert_eq!(event.event_type, "codex_token_count");
        assert_eq!(
            event
                .data
                .get("provider_event_type")
                .and_then(|value| value.as_str()),
            Some("session_transcript.event_msg.token_count")
        );

        let usage = crate::monitor::extract_codex_context_usage(&event).unwrap();
        assert_eq!(usage.context_tokens, 130_492);
        assert_eq!(usage.output_tokens, 640);
        assert_eq!(usage.context_window, Some(258_400));
    }

    #[test]
    fn test_turn_completed_cumulative_cache_does_not_become_context() {
        // Regression fixture from real Codex telemetry: adding cached_input_tokens
        // produced a 16M-token numerator and clamped the UI to 100%.
        let value = serde_json::json!({
            "type": "turn.completed",
            "usage": {
                "input_tokens": 16_776_424u64,
                "cached_input_tokens": 16_574_592u64,
                "output_tokens": 37_298u64,
            }
        });
        let mut thread_id = Some("thread-cumulative-cache".to_string());
        let event = map_codex_json_to_stream_event(&value, &mut thread_id).unwrap();

        let extracted = crate::monitor::extract_token_usage(&event).unwrap();
        assert_eq!(extracted.input, 201_832);
        assert_eq!(extracted.cache_read, 0);
        assert_eq!(extracted.total_input, 201_832);
    }

    #[test]
    fn test_turn_completed_no_cached_tokens() {
        // When no cached tokens are reported, input_tokens passes through unchanged.
        let value = serde_json::json!({
            "type": "turn.completed",
            "usage": {
                "input_tokens": 30000,
                "output_tokens": 2000,
            }
        });
        let mut thread_id = Some("thread-456".to_string());
        let event = map_codex_json_to_stream_event(&value, &mut thread_id).unwrap();

        let usage = event.data.get("usage").unwrap();
        assert_eq!(usage.get("input_tokens").unwrap(), 30000);
        assert!(usage.get("cache_read_input_tokens").is_none());
        assert_eq!(usage.get("output_tokens").unwrap(), 2000);
    }

    #[test]
    fn test_turn_completed_cached_equals_total() {
        // Edge case: all tokens are cached → non-cached = 0.
        let value = serde_json::json!({
            "type": "turn.completed",
            "usage": {
                "input_tokens": 20000,
                "cached_input_tokens": 20000,
                "output_tokens": 1000,
            }
        });
        let mut thread_id = None;
        let event = map_codex_json_to_stream_event(&value, &mut thread_id).unwrap();

        let usage = event.data.get("usage").unwrap();
        assert_eq!(usage.get("input_tokens").unwrap(), 0);
        assert!(usage.get("cache_read_input_tokens").is_none());
    }

    #[test]
    fn test_error_event_becomes_process_error() {
        let value = serde_json::json!({
            "type": "error",
            "message": "Quota exceeded.",
        });
        let mut thread_id = Some("thread-err".to_string());
        let event = map_codex_json_to_stream_event(&value, &mut thread_id).unwrap();

        assert_eq!(event.event_type, "process_error");
        assert_eq!(event.data.get("error").unwrap(), "Quota exceeded.");
        assert!(event.data.get("error_class").is_none());
        assert_eq!(event.data.get("terminal"), Some(&Value::Bool(false)));
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn test_codex_retry_notices_are_nonterminal_at_every_attempt_count() {
        for message in [
            "Reconnecting... 1/5 (SSE idle timeout)",
            "Reconnecting... 5/5 (SSE idle timeout)",
            "Reconnecting... 1/2 (HTTPS fallback reset the counter)",
        ] {
            let value = serde_json::json!({"type": "error", "message": message});
            let mut thread_id = None;
            let event = map_codex_json_to_stream_event(&value, &mut thread_id).unwrap();
            assert_eq!(
                event.data.get("terminal").and_then(Value::as_bool),
                Some(false),
                "unexpected terminal classification for {message:?}"
            );
            assert!(event.data.get("error_class").is_none());
        }
    }

    #[test]
    fn test_usage_limit_error_receives_closed_classification() {
        let message = "You've hit your usage limit. Visit https://chatgpt.com/codex/settings/usage to purchase more credits or try again at Aug 15th, 2026 1:29 PM.";
        let value = serde_json::json!({
            "type": "error",
            "message": message,
        });
        let mut thread_id = Some("thread-usage-limit".to_string());
        let event = map_codex_json_to_stream_event(&value, &mut thread_id).unwrap();

        assert_eq!(event.event_type, "process_error");
        assert_eq!(
            event.data.get("error").and_then(Value::as_str),
            Some(message)
        );
        assert_eq!(
            event.data.get("error_class").and_then(Value::as_str),
            Some(CODEX_USAGE_LIMIT_ERROR_CLASS)
        );
        assert_eq!(event.data.get("terminal"), Some(&Value::Bool(false)));
    }

    #[test]
    fn test_usage_limit_near_misses_and_other_error_shapes_remain_unclassified() {
        for message in [
            "You've hit your usage limit",
            "You've hit your usage limit.unrelated",
            "You've hit a usage limit.",
            "Quota exceeded.",
            "Visit https://chatgpt.com/codex/settings/usage",
        ] {
            let value = serde_json::json!({"type": "error", "message": message});
            let mut thread_id = None;
            let event = map_codex_json_to_stream_event(&value, &mut thread_id).unwrap();
            assert!(
                event.data.get("error_class").is_none(),
                "near miss must remain unclassified: {message}"
            );
        }

        let value = serde_json::json!({
            "type": "turn.failed",
            "error": {"message": "Error running remote compact task: You've hit your usage limit. Retry later."},
        });
        let mut thread_id = None;
        let event = map_codex_json_to_stream_event(&value, &mut thread_id).unwrap();
        assert_eq!(
            event.data.get("error_class").and_then(Value::as_str),
            Some(CODEX_USAGE_LIMIT_ERROR_CLASS),
            "the known remote compact wrapper retains the Codex usage-limit class"
        );

        let value = serde_json::json!({
            "type": "error",
            "message": "You've hit your usage limit. Retry later.",
        });
        let mut thread_id = None;
        let event = map_app_server_compatible_event(&value, &mut thread_id).unwrap();
        assert!(
            event.data.get("error_class").is_none(),
            "Codex App Server compatibility mapping remains outside the CLI classifier"
        );
        assert!(
            event.data.get("terminal").is_none(),
            "Codex App Server compatibility mapping preserves its terminal contract"
        );
    }

    #[test]
    fn test_turn_failed_extracts_nested_message() {
        let value = serde_json::json!({
            "type": "turn.failed",
            "error": { "message": "Quota exceeded." },
        });
        let mut thread_id = Some("thread-tf".to_string());
        let event = map_codex_json_to_stream_event(&value, &mut thread_id).unwrap();

        assert_eq!(event.event_type, "process_error");
        assert_eq!(event.data.get("error").unwrap(), "Quota exceeded.");
    }

    #[test]
    fn test_item_completed_error_subtype_becomes_process_error() {
        let value = serde_json::json!({
            "type": "item.completed",
            "item": {
                "id": "item_0",
                "type": "error",
                "message": "`[features].collab` is deprecated.",
            },
        });
        let mut thread_id = Some("thread-ic".to_string());
        let event = map_codex_json_to_stream_event(&value, &mut thread_id).unwrap();

        assert_eq!(event.event_type, "process_error");
        let err_str = event.data.get("error").unwrap().as_str().unwrap();
        assert!(err_str.contains("[features].collab"));
        assert!(err_str.contains("deprecated"));
        assert_eq!(
            event.data.get("terminal").and_then(Value::as_bool),
            Some(false),
            "Codex CLI item-level error items are warnings, never turn settlement (#662)"
        );

        let mut thread_id = None;
        let compat = map_app_server_compatible_event(&value, &mut thread_id).unwrap();
        assert_eq!(compat.event_type, "process_error");
        assert!(
            compat.data.get("terminal").is_none(),
            "App Server compatibility mapping retains its existing terminal contract"
        );
    }

    #[test]
    fn test_command_execution_result_sets_error_flag_from_integer_exit_code() {
        for (exit_code, expected_error) in
            [(Some(0), Some(false)), (Some(7), Some(true)), (None, None)]
        {
            let mut item = serde_json::json!({
                "type": "command_execution",
                "aggregated_output": "command output",
            });
            if let Some(exit_code) = exit_code {
                item["exit_code"] = serde_json::json!(exit_code);
            }
            let value = serde_json::json!({"type": "item.completed", "item": item});
            let mut thread_id = Some("thread-command".to_string());
            let event = map_codex_json_to_stream_event(&value, &mut thread_id).unwrap();

            assert_eq!(event.event_type, "tool_result");
            assert_eq!(event.data["content"], "command output");
            assert_eq!(
                event.data.get("is_error").and_then(Value::as_bool),
                expected_error
            );
            assert_eq!(event.data["exit_code"], serde_json::json!(exit_code));
        }
    }

    #[test]
    fn test_unknown_event_type_returns_none() {
        let value = serde_json::json!({
            "type": "some_future_event",
            "foo": "bar",
        });
        let mut thread_id = Some("thread-unk".to_string());
        assert!(map_codex_json_to_stream_event(&value, &mut thread_id).is_none());
    }

    #[test]
    fn test_turn_started_returns_none() {
        let value = serde_json::json!({ "type": "turn.started" });
        let mut thread_id = Some("thread-ts".to_string());
        assert!(map_codex_json_to_stream_event(&value, &mut thread_id).is_none());
    }

    #[test]
    fn test_latest_codex_turn_snapshot_maps_only_current_custom_tool_pair() {
        let values = vec![
            serde_json::json!({
                "type": "event_msg",
                "payload": {"type": "task_started", "turn_id": "turn-old"},
            }),
            serde_json::json!({
                "type": "response_item",
                "payload": {
                    "type": "custom_tool_call",
                    "call_id": "old-call",
                    "name": "exec",
                    "input": "const r = await tools.write_stdin({session_id: 1});",
                    "internal_chat_message_metadata_passthrough": {"turn_id": "turn-old"},
                },
            }),
            serde_json::json!({
                "type": "event_msg",
                "payload": {"type": "task_started", "turn_id": "turn-current"},
            }),
            serde_json::json!({
                "type": "response_item",
                "payload": {
                    "type": "custom_tool_call",
                    "call_id": "patch-call",
                    "name": "exec",
                    "input": "const r = await tools.apply_patch(\"*** Begin Patch\"); text(r);",
                    "internal_chat_message_metadata_passthrough": {"turn_id": "turn-current"},
                },
            }),
            serde_json::json!({
                "type": "response_item",
                "payload": {
                    "type": "custom_tool_call_output",
                    "call_id": "patch-call",
                    "output": [
                        {"type": "input_text", "text": "Script failed\n"},
                        {"type": "input_text", "text": "Script error:\napply_patch verification failed: changed context"}
                    ],
                    "internal_chat_message_metadata_passthrough": {"turn_id": "turn-current"},
                },
            }),
            serde_json::json!({
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {"last_token_usage": {"total_tokens": 42}}
                },
            }),
            serde_json::json!({
                "type": "event_msg",
                "payload": {"type": "task_complete", "turn_id": "turn-current"},
            }),
        ];

        let snapshot = latest_codex_turn_snapshot_from_values(values, "thread-123");

        assert!(snapshot.turn_complete);
        assert!(snapshot.context_event.is_some());
        assert_eq!(snapshot.custom_tool_events.len(), 2);
        assert_eq!(snapshot.custom_tool_events[0].event_type, "tool_use");
        assert_eq!(snapshot.custom_tool_events[0].data["name"], "apply_patch");
        assert_eq!(snapshot.custom_tool_events[1].event_type, "tool_result");
        assert_eq!(snapshot.custom_tool_events[1].data["call_id"], "patch-call");
        assert!(
            snapshot.custom_tool_events[1].data["content"]
                .as_str()
                .unwrap()
                .contains("changed context")
        );
        assert_eq!(snapshot.failure_evidence.len(), 1);
        assert_eq!(
            snapshot.failure_evidence[0].nested_tools,
            vec!["apply_patch"]
        );
    }

    #[test]
    fn test_codex_transcript_does_not_duplicate_successful_exec_json_tool() {
        let values = vec![
            serde_json::json!({
                "type": "event_msg",
                "payload": {"type": "task_started", "turn_id": "turn-current"},
            }),
            serde_json::json!({
                "type": "response_item",
                "payload": {
                    "type": "custom_tool_call",
                    "call_id": "shell-call",
                    "name": "exec",
                    "input": "const r = await tools.exec_command({cmd: \"true\"}); text(r.output);",
                    "internal_chat_message_metadata_passthrough": {"turn_id": "turn-current"},
                },
            }),
            serde_json::json!({
                "type": "response_item",
                "payload": {
                    "type": "custom_tool_call_output",
                    "call_id": "shell-call",
                    "output": "Script completed\nWall time 0.0 seconds\nOutput:\n",
                    "internal_chat_message_metadata_passthrough": {"turn_id": "turn-current"},
                },
            }),
            serde_json::json!({
                "type": "event_msg",
                "payload": {"type": "task_complete", "turn_id": "turn-current"},
            }),
        ];

        let snapshot = latest_codex_turn_snapshot_from_values(values, "thread-123");

        assert!(snapshot.custom_tool_events.is_empty());
        assert!(snapshot.failure_evidence.is_empty());
    }

    #[test]
    fn test_codex_failed_typed_tool_stderr_is_suppressed_one_for_one() {
        let failure = "resources/read failed: unknown MCP server 'filesystem'";
        let values = vec![
            serde_json::json!({
                "type": "event_msg",
                "payload": {"type": "task_started", "turn_id": "turn-current"},
            }),
            serde_json::json!({
                "type": "event_msg",
                "payload": {
                    "type": "item_completed",
                    "turn_id": "turn-current",
                    "item": {
                        "type": "McpToolCall",
                        "tool": "read_mcp_resource",
                        "status": "failed",
                        "error": {"message": failure},
                    },
                },
            }),
            serde_json::json!({
                "type": "event_msg",
                "payload": {"type": "task_complete", "turn_id": "turn-current"},
            }),
        ];
        let mut snapshot = latest_codex_turn_snapshot_from_values(values, "thread-123");
        assert_eq!(snapshot.failure_evidence.len(), 1);
        assert_eq!(
            snapshot.failure_evidence[0].nested_tools,
            vec!["read_mcp_resource"]
        );

        let record =
            format!("2026-08-26T19:38:29.060758Z ERROR codex_core::tools::router: error={failure}");
        let remaining = filter_correlated_codex_stderr(
            vec![record.clone(), record.clone()],
            &mut CodexStderrCorrelation {
                failure_evidence: std::mem::take(&mut snapshot.failure_evidence),
            },
        );

        assert_eq!(remaining, vec![record]);
    }

    #[test]
    fn test_codex_typed_failure_evidence_is_scoped_to_latest_turn() {
        let failure = "resources/read failed: unknown MCP server 'filesystem'";
        let values = vec![
            serde_json::json!({
                "type": "event_msg",
                "payload": {"type": "task_started", "turn_id": "turn-old"},
            }),
            serde_json::json!({
                "type": "event_msg",
                "payload": {
                    "type": "item_completed",
                    "turn_id": "turn-old",
                    "item": {
                        "type": "McpToolCall",
                        "tool": "read_mcp_resource",
                        "status": "failed",
                        "error": {"message": failure},
                    },
                },
            }),
            serde_json::json!({
                "type": "event_msg",
                "payload": {"type": "task_complete", "turn_id": "turn-old"},
            }),
            serde_json::json!({
                "type": "event_msg",
                "payload": {"type": "task_started", "turn_id": "turn-current"},
            }),
            serde_json::json!({
                "type": "event_msg",
                "payload": {"type": "task_complete", "turn_id": "turn-current"},
            }),
        ];

        let snapshot = latest_codex_turn_snapshot_from_values(values, "thread-123");

        assert_eq!(snapshot.turn_id.as_deref(), Some("turn-current"));
        assert!(snapshot.turn_complete);
        assert!(snapshot.failure_evidence.is_empty());
    }

    #[test]
    fn test_codex_transcript_watermark_excludes_stale_completed_turn() {
        use std::io::Write;

        let directory = tempfile::tempdir().unwrap();
        let transcript = directory.path().join("rollout-thread-123.jsonl");
        let old_turn = [
            serde_json::json!({
                "type": "event_msg",
                "payload": {"type": "task_started", "turn_id": "turn-old"},
            }),
            serde_json::json!({
                "type": "event_msg",
                "payload": {"type": "task_complete", "turn_id": "turn-old"},
            }),
        ];
        {
            let mut file = File::create(&transcript).unwrap();
            for value in old_turn {
                writeln!(file, "{value}").unwrap();
            }
        }
        let metadata = transcript.metadata().unwrap();
        let watermark = CodexTranscriptWatermark {
            path: transcript.clone(),
            byte_offset: metadata.len(),
            device: metadata.dev(),
            inode: metadata.ino(),
            prefix_tail: std::fs::read(&transcript).unwrap(),
        };

        let stale = latest_codex_turn_snapshot_from_transcript_after(
            &transcript,
            "thread-123",
            Some(&watermark),
        )
        .unwrap();
        assert!(stale.turn_id.is_none());
        assert!(!stale.turn_complete);

        {
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&transcript)
                .unwrap();
            for value in [
                serde_json::json!({
                    "type": "event_msg",
                    "payload": {"type": "task_started", "turn_id": "turn-current"},
                }),
                serde_json::json!({
                    "type": "event_msg",
                    "payload": {"type": "task_complete", "turn_id": "turn-current"},
                }),
            ] {
                writeln!(file, "{value}").unwrap();
            }
        }

        let current = latest_codex_turn_snapshot_from_transcript_after(
            &transcript,
            "thread-123",
            Some(&watermark),
        )
        .unwrap();
        assert_eq!(current.turn_id.as_deref(), Some("turn-current"));
        assert!(current.turn_complete);
    }

    #[test]
    fn test_codex_transcript_watermark_mismatch_refuses_correlation() {
        use std::io::Write;

        let directory = tempfile::tempdir().unwrap();
        let transcript = directory.path().join("rollout-thread-123.jsonl");
        let stale_turn = format!(
            "{}\n{}\n",
            serde_json::json!({
                "type": "event_msg",
                "payload": {"type": "task_started", "turn_id": "turn-old"},
            }),
            serde_json::json!({
                "type": "event_msg",
                "payload": {"type": "task_complete", "turn_id": "turn-old"},
            }),
        );
        File::create(&transcript)
            .unwrap()
            .write_all(stale_turn.as_bytes())
            .unwrap();
        let metadata = transcript.metadata().unwrap();
        let watermark = CodexTranscriptWatermark {
            path: transcript.clone(),
            byte_offset: metadata.len(),
            device: metadata.dev(),
            inode: metadata.ino(),
            prefix_tail: std::fs::read(&transcript).unwrap(),
        };

        let different_transcript = directory.path().join("rollout-thread-123-replaced.jsonl");
        File::create(&different_transcript)
            .unwrap()
            .write_all(stale_turn.as_bytes())
            .unwrap();
        assert!(
            latest_codex_turn_snapshot_from_transcript_after(
                &different_transcript,
                "thread-123",
                Some(&watermark),
            )
            .is_none()
        );

        File::options()
            .write(true)
            .open(&transcript)
            .unwrap()
            .set_len(watermark.byte_offset - 1)
            .unwrap();
        assert!(
            latest_codex_turn_snapshot_from_transcript_after(
                &transcript,
                "thread-123",
                Some(&watermark),
            )
            .is_none()
        );

        {
            let mut file = File::options()
                .write(true)
                .truncate(true)
                .open(&transcript)
                .unwrap();
            file.write_all(&vec![b'x'; watermark.byte_offset as usize])
                .unwrap();
        }
        assert!(
            latest_codex_turn_snapshot_from_transcript_after(
                &transcript,
                "thread-123",
                Some(&watermark),
            )
            .is_none()
        );

        let original_transcript = directory.path().join("original-rollout.jsonl");
        std::fs::rename(&transcript, original_transcript).unwrap();
        File::create(&transcript)
            .unwrap()
            .write_all(stale_turn.as_bytes())
            .unwrap();
        assert!(
            latest_codex_turn_snapshot_from_transcript_after(
                &transcript,
                "thread-123",
                Some(&watermark),
            )
            .is_none()
        );
    }

    #[tokio::test]
    async fn test_codex_resume_without_watermark_refuses_transcript_correlation() {
        let snapshot = read_latest_codex_turn_snapshot_with_retry(
            "thread-with-unavailable-transcript",
            &CodexTranscriptBoundary::ResumeUnavailable,
        )
        .await;

        assert!(snapshot.is_none());
    }

    #[test]
    fn test_codex_unpaired_typed_router_stderr_stays_visible() {
        let record = "2026-08-26T19:38:29.060758Z ERROR codex_core::tools::router: error=resources/read failed: unknown MCP server 'filesystem'".to_string();
        let mut correlation = CodexStderrCorrelation::default();

        let remaining = filter_correlated_codex_stderr(vec![record.clone()], &mut correlation);

        assert_eq!(remaining, vec![record]);
    }

    #[test]
    fn test_codex_multiline_apply_patch_stderr_requires_exact_paired_output() {
        let payload = "apply_patch verification failed: Failed to find expected lines in /tmp/a.rs:\n    old line";
        let mut correlation = CodexStderrCorrelation {
            failure_evidence: vec![CodexToolFailureEvidence {
                nested_tools: vec!["apply_patch".to_string()],
                output: format!("Script failed\nScript error:\n{payload}"),
            }],
        };
        let records = group_codex_stderr_records(vec![
            "2026-08-20T01:02:03.000Z ERROR codex_core::tools::router: error=apply_patch verification failed: Failed to find expected lines in /tmp/a.rs:".to_string(),
            "    old line".to_string(),
            "2026-08-20T01:02:04.000Z ERROR quota exceeded".to_string(),
        ]);

        let remaining = filter_correlated_codex_stderr(records, &mut correlation);

        assert_eq!(
            remaining,
            vec!["2026-08-20T01:02:04.000Z ERROR quota exceeded"]
        );
    }

    #[test]
    fn summarize_codex_stderr_records_compacts_consecutive_timestamped_retries() {
        let records = vec![
            "2026-09-14T19:02:23.024519Z ERROR codex_models_manager::manager: failed to refresh available models: timeout waiting for child process to exit".to_string(),
            "2026-09-14T19:02:32.901713Z ERROR codex_models_manager::manager: failed to refresh available models: timeout waiting for child process to exit".to_string(),
            "2026-09-14T19:02:48.192594Z ERROR codex_models_manager::manager: failed to refresh available models: timeout waiting for child process to exit".to_string(),
        ];
        let summary = summarize_codex_stderr_records(&records);

        assert_eq!(summary.record_count, 3);
        assert_eq!(summary.records.len(), 1);
        assert!(
            summary
                .rendered
                .contains("failed to refresh available models")
        );
        assert!(summary.rendered.contains("repeated 2 additional times"));
        assert!(
            summary
                .rendered
                .contains("last at 2026-09-14T19:02:48.192594Z")
        );
    }

    #[test]
    fn test_codex_apply_patch_stderr_with_different_context_stays_visible() {
        let mut correlation = CodexStderrCorrelation {
            failure_evidence: vec![CodexToolFailureEvidence {
                nested_tools: vec!["apply_patch".to_string()],
                output: "apply_patch verification failed: expected alpha".to_string(),
            }],
        };
        let record = "2026-08-20T01:02:03.000Z ERROR codex_core::tools::router: error=apply_patch verification failed: expected beta".to_string();

        let remaining = filter_correlated_codex_stderr(vec![record.clone()], &mut correlation);

        assert_eq!(remaining, vec![record]);
    }

    #[test]
    fn test_codex_batched_write_stdin_stderr_is_consumed_one_for_one() {
        let first = "write_stdin failed: Unknown process id 7";
        let second = "write_stdin failed: Unknown process id 8";
        let mut correlation = CodexStderrCorrelation {
            failure_evidence: vec![CodexToolFailureEvidence {
                nested_tools: vec!["write_stdin".to_string()],
                output: format!("{{\"e\":\"{first}\"}}{{\"e\":\"{second}\"}}"),
            }],
        };
        let first_record =
            format!("2026-08-20T01:02:03.000Z ERROR codex_core::tools::router: error={first}");
        let second_record =
            format!("2026-08-20T01:02:04.000Z ERROR codex_core::tools::router: error={second}");
        let excess_record = second_record.clone();

        let remaining = filter_correlated_codex_stderr(
            vec![first_record, second_record, excess_record.clone()],
            &mut correlation,
        );

        assert_eq!(remaining, vec![excess_record]);
    }

    #[test]
    fn test_codex_router_stderr_without_matching_tool_family_stays_visible() {
        let record = "2026-08-20T01:02:03.000Z ERROR codex_core::tools::router: error=write_stdin failed: Unknown process id 7".to_string();
        let mut correlation = CodexStderrCorrelation {
            failure_evidence: vec![CodexToolFailureEvidence {
                nested_tools: vec!["apply_patch".to_string()],
                output: "write_stdin failed: Unknown process id 7".to_string(),
            }],
        };

        let remaining = filter_correlated_codex_stderr(vec![record.clone()], &mut correlation);

        assert_eq!(remaining, vec![record]);
    }

    #[test]
    fn test_codex_storage_full_classifier_requires_complete_recorder_signature() {
        let exact = "2026-08-19T21:48:22.123456Z ERROR codex_rollout::recorder: failed to persist rollout: No space left on device (os error 28); error_kind=StorageFull; raw_os_error=Some(28)";
        assert!(is_codex_storage_full_fatal_record(exact));

        for near_miss in [
            "2026-08-19T21:48:22.123456Z WARN codex_rollout::recorder: failed to persist rollout: No space left on device (os error 28); error_kind=StorageFull; raw_os_error=Some(28)",
            "2026-08-19T21:48:22.123456Z ERROR codex_core::tools::router: error=write_stdin failed: No space left on device (os error 28); error_kind=StorageFull; raw_os_error=Some(28)",
            "2026-08-19T21:48:22.123456Z ERROR codex_rollout::recorder: failed to persist rollout: No space left on device (os error 28); raw_os_error=Some(28)",
            "2026-08-19T21:48:22.123456Z ERROR codex_rollout::recorder: failed to persist rollout: No space left on device (os error 28); error_kind=StorageFull",
            "2026-08-19T21:48:22.123456Z ERROR codex_rollout::recorder: failed to persist rollout: No space left on device (os error 30); error_kind=StorageFull; raw_os_error=Some(30)",
            "ERROR codex_rollout::recorder: failed to persist rollout: No space left on device (os error 28); error_kind=StorageFull; raw_os_error=Some(28)",
        ] {
            assert!(
                !is_codex_storage_full_fatal_record(near_miss),
                "near miss stays nonterminal: {near_miss}"
            );
        }
    }

    #[test]
    fn test_codex_storage_full_companion_requires_exact_error_component() {
        let exact = "2026-08-19T21:48:22.123456Z ERROR codex_core::session: failed to record rollout items: thread-store internal error: No space left on device (os error 28)";
        assert!(is_codex_storage_full_companion_record(exact));
        assert!(!is_codex_storage_full_fatal_record(exact));

        for near_miss in [
            "2026-08-19T21:48:22.123456Z WARN codex_core::session: failed to record rollout items: thread-store internal error: No space left on device (os error 28)",
            "2026-08-19T21:48:22.123456Z ERROR codex_core::session: failed to record rollout items: No space left on device (os error 28)",
            "2026-08-19T21:48:22.123456Z ERROR codex_core::session: failed to record rollout items: thread-store internal error: No space left on device (os error 30)",
        ] {
            assert!(
                !is_codex_storage_full_companion_record(near_miss),
                "near miss remains visible: {near_miss}"
            );
        }
    }

    #[test]
    fn test_codex_storage_full_state_emits_once_and_removes_companions() {
        let companion = "2026-08-19T21:48:22.123456Z ERROR codex_core::session: failed to record rollout items: thread-store internal error: No space left on device (os error 28)";
        let fatal = "2026-08-19T21:48:22.123457Z ERROR codex_rollout::recorder: rollout writer failed; buffered rollout items will be retried: No space left on device (os error 28); error_kind=StorageFull; raw_os_error=Some(28)";
        let ordinary = "2026-08-19T21:48:22.123458Z WARN codex_core::other: diagnostic";
        let mut state = CodexFatalStderrState::default();
        let mut buffered = Vec::new();

        assert_eq!(state.consume(companion.to_string(), &mut buffered), None);
        assert_eq!(buffered, vec![companion]);

        let terminal = state
            .consume(fatal.to_string(), &mut buffered)
            .expect("first exact recorder failure emits");
        assert_eq!(terminal, fatal);
        assert!(buffered.is_empty(), "prior exact companion is removed");

        assert_eq!(state.consume(fatal.to_string(), &mut buffered), None);
        assert_eq!(state.consume(companion.to_string(), &mut buffered), None);
        assert_eq!(state.consume(ordinary.to_string(), &mut buffered), None);
        assert_eq!(
            buffered,
            vec![ordinary],
            "repeats and companions stay out of the EOF aggregate"
        );
    }

    #[test]
    fn test_codex_transcript_fields_are_utf8_safe_and_bounded() {
        let input = "é".repeat(CODEX_TRANSCRIPT_FIELD_MAX_BYTES);
        let bounded = bounded_codex_transcript_field(&input, CODEX_TRANSCRIPT_FIELD_MAX_BYTES);

        assert!(bounded.is_char_boundary(bounded.len()));
        assert!(bounded.contains("[truncated by RSI]"));
        assert!(bounded.len() <= CODEX_TRANSCRIPT_FIELD_MAX_BYTES + 32);
    }

    #[test]
    fn test_codex_stderr_suppression_reason_suppresses_stdin_message() {
        let line = "Reading prompt from stdin...";
        assert_eq!(
            codex_stderr_suppression_reason(line),
            Some("stdin prompt read message")
        );
    }

    #[test]
    fn test_codex_stderr_suppression_reason_suppresses_project_config_warning() {
        let line = "Ignored unsupported project-local config keys in /home/jakedevar/rsi/.codex/config.toml: notify.";
        assert_eq!(
            codex_stderr_suppression_reason(line),
            Some("unsupported project-local config keys warning")
        );
    }

    #[test]
    fn test_codex_stderr_suppression_reason_suppresses_collab_deprecation_warning() {
        let line = "`[features].collab` is deprecated. Use `[features].multi_agent` instead.";
        assert_eq!(
            codex_stderr_suppression_reason(line),
            Some("deprecated collab feature flag")
        );
    }

    #[test]
    fn test_codex_stderr_suppression_reason_keeps_real_errors_visible() {
        let line = "Quota exceeded. Check your plan and billing details.";
        assert_eq!(codex_stderr_suppression_reason(line), None);
    }

    #[test]
    fn test_codex_stderr_suppression_reason_suppresses_nonfatal_rollout_recorder_diagnostic() {
        // Issue #67 benchmark session 6c3ebb3a emitted this exact shape as an
        // assistant Provider diagnostic after otherwise useful tool activity.
        let exact = "2026-09-22T00:51:22.536783Z ERROR codex_core::session: failed to record rollout items: thread 01a0c68d-b608-75c1-a584-12effa6789aa not found";
        assert_eq!(
            codex_stderr_suppression_reason(exact),
            Some("non-fatal rollout recorder thread-not-found diagnostic")
        );

        // The payload shape is also recognized without a leading timestamp.
        let untimestamped =
            "codex_core::session: failed to record rollout items: thread abc-123 not found";
        assert_eq!(
            codex_stderr_suppression_reason(untimestamped),
            Some("non-fatal rollout recorder thread-not-found diagnostic")
        );
    }

    #[test]
    fn test_codex_stderr_suppression_reason_keeps_fatal_and_unrelated_rollout_records_visible() {
        // Issue #68 storage exhaustion must stay visible and terminal even
        // though it shares the "failed to record rollout items" prefix.
        for fatal in [
            "2026-08-19T21:48:22.123456Z ERROR codex_core::session: failed to record rollout items: thread-store internal error: No space left on device (os error 28)",
            "2026-08-19T21:48:22.123456Z ERROR codex_rollout::recorder: failed to persist rollout: No space left on device (os error 28); error_kind=StorageFull; raw_os_error=Some(28)",
        ] {
            assert_eq!(
                codex_stderr_suppression_reason(fatal),
                None,
                "fatal recorder stderr must remain visible: {fatal}"
            );
        }

        // A non-error level line is not a non-fatal ERROR diagnostic.
        let warn = "2026-09-22T00:51:22.536783Z WARN codex_core::session: failed to record rollout items: thread abc not found";
        assert_eq!(codex_stderr_suppression_reason(warn), None);

        // Unrelated recorder failures keep their existing visibility.
        for unrelated in [
            "2026-09-22T00:51:22.536783Z ERROR codex_core::session: failed to record rollout items: thread-store internal error: connection reset",
            "2026-09-22T00:51:22.536783Z ERROR codex_core::session: failed to record rollout items: disk quota exceeded",
            "2026-09-22T00:51:22.536783Z ERROR codex_core::session: failed to record rollout items: thread  not found",
        ] {
            assert_eq!(
                codex_stderr_suppression_reason(unrelated),
                None,
                "unrelated recorder stderr must remain visible: {unrelated}"
            );
        }
    }
}
