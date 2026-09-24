use crate::config::RuntimeConfig;
use crate::error::{DaemonError, Result};
use crate::model_control::CliExecutionCapability;
use crate::model_control::registry::RuntimeExecutionRoute;
use crate::process_control::{
    BoundedLineError, BoundedLines, PROVIDER_MAX_LINE_BYTES, PROVIDER_MAX_STDERR_BYTES,
    ProcessContainment, configure_tokio_process_group, terminate_process_group,
};
use crate::sandbox::execution_scratch::SandboxExecutionScratch;
use rsi_common::model_control::{InvocationOwner, ModelInvocationPurpose};
use serde::Deserialize;
use serde_json::Value;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

/// Configuration for launching a Claude session.
/// This is daemon-internal and has more fields than LaunchSessionParams.
#[derive(Debug, Clone)]
pub struct LaunchConfig {
    pub query: String,
    pub title: Option<String>,
    /// Durable operator-facing role metadata. Distinct from SessionKind and
    /// the `$CLAUDE_AGENT_ROLE` Git-hook policy environment variable.
    pub agent_role: Option<String>,
    /// Stored lineage ordinal reserved by the owning Epic.
    pub epic_spawn_ordinal: Option<u32>,
    pub working_dir: Option<PathBuf>,
    pub provider: Option<rsi_common::types::SessionProvider>,
    pub model: Option<String>,
    /// Raw context-window configuration for this provider incarnation.
    ///
    /// For Codex this is emitted as `model_context_window` and preserved
    /// separately from the provider's effective runtime denominator.
    pub configured_context_window: Option<u64>,
    pub max_turns: Option<u32>,
    pub system_prompt: Option<String>,
    pub resume_session_id: Option<String>,
    pub session_kind: Option<rsi_common::types::SessionKind>,
    /// Explicit project assignment from TUI. Fallback when path-based resolution
    /// finds no matching project.
    pub project_id: Option<uuid::Uuid>,
    /// Injected by SessionManager before launch — enables flywheel-signal.
    pub rsi_session_id: Option<uuid::Uuid>,
    pub rsi_socket: Option<PathBuf>,
    /// Per-session authority token (P0 attribution gate). Minted by
    /// `SessionManager::launch_session` alongside `rsi_session_id`/
    /// `rsi_socket`; stamped into the provider env (`RSI_SESSION_TOKEN`) for
    /// Claude + agy today. Never persisted into `Session` — only the
    /// in-memory `token -> session_id` map in `SessionManager` and the
    /// spawned process's environment carry it.
    pub rsi_session_token: Option<String>,
    /// Parent session ID for context rotation chains.
    pub continued_from: Option<uuid::Uuid>,
    /// Override base URL for OpenAI-compatible providers (from custom provider config).
    pub openai_base_url: Option<String>,
    /// Override API key for OpenAI-compatible providers (from custom provider config).
    pub openai_api_key: Option<String>,
    /// Pre-seeded conversation history for OpenAI-compatible provider resume/continue.
    /// When set, the agentic loop starts with these messages instead of a fresh conversation.
    pub conversation_history: Option<Vec<serde_json::Value>>,
    /// Workflow this session belongs to (research → plan → implement pipeline).
    /// Propagated from LaunchSessionParams or inherited from parent session.
    pub workflow_id: Option<uuid::Uuid>,
    /// Per-session topology override at spawn time. When `Some`, the spawned
    /// session row carries `workflow_id_override = Some(_)` (supersedes the
    /// Epic-derived topology resolved by `effective_topology_with_override`).
    /// `None` (default) means the session inherits its Epic ancestor's
    /// topology via the derive-on-read walk in `hierarchy_ops::effective_topology`.
    pub workflow_id_override: Option<uuid::Uuid>,
    /// Optional group to assign this session to.
    /// Maximum retry attempts on transient failure. Default 0 (no retry).
    pub max_retries: Option<u8>,
    /// Optional group assignment for organizing related sessions.
    pub group_id: Option<uuid::Uuid>,
    /// Optional hierarchical parent (Group/Epic container) for the new session.
    /// Independent of `continued_from`; validated by the RPC layer before
    /// `launch_session` is called.
    pub parent_id: Option<uuid::Uuid>,
    /// Effort level for Claude sessions ("low", "medium", "high", "max").
    pub effort: Option<String>,
    /// Issue tracker identifier (e.g., "ENG-42") for issue-driven sessions.
    pub issue_identifier: Option<String>,
    /// Issue tracker URL for the source issue.
    pub issue_url: Option<String>,
    /// Issue tracker UUID for reconciliation.
    pub issue_tracker_id: Option<String>,
    /// Scheduled job that spawned this session (for traceability).
    pub scheduled_job_id: Option<uuid::Uuid>,
    /// Explicit control-plane purpose for this launch. Callers must choose
    /// one; there is no implicit interactive fallback.
    pub model_invocation_purpose: ModelInvocationPurpose,
    /// Optional owner metadata beyond the standard session/workflow/job
    /// fields derived from the launch config itself.
    pub model_invocation_owner: Option<InvocationOwner>,
    /// Optional durable dedup key for orchestration launches. When absent, the
    /// launch path falls back to the per-session key.
    pub model_invocation_dedup_key: Option<String>,
    /// Optional caller-supplied fingerprint for stable replay-safe admission.
    pub model_invocation_request_fingerprint: Option<String>,
    /// Keep a cross-provider child with no explicit model on the target
    /// provider's native default instead of applying a project model selected
    /// for a different provider. All ordinary launches retain project defaults.
    pub skip_project_model_default: bool,
    /// Per-launch sandbox request. `None` = canonical working_dir (zero
    /// change vs. pre-sandbox behavior). Consumed by `launch_session`
    /// before provider spawn; ignored by providers themselves.
    pub sandbox: Option<rsi_common::types::SandboxSpec>,
    /// Session-scoped cargo build scratch directory (issue #25). Populated by
    /// `launch_session`/`continue_session` to `<sandbox_root>/target` when the
    /// session runs in a sandbox; stamped into the provider subprocess env as
    /// `CARGO_TARGET_DIR` so worker `cargo` invocations inherit a disk-backed,
    /// sandbox-lifetime scratch dir instead of improvising one on the `/tmp`
    /// tmpfs. `None` = do not stamp (non-sandboxed sessions keep the user's
    /// own cargo config, e.g. the shared target cache).
    pub cargo_target_dir: Option<PathBuf>,
    /// Descriptor-derived execution scratch. This is populated only after an
    /// authenticated sandbox custody context read; public launch parameters
    /// cannot select either path.
    pub execution_scratch: Option<SandboxExecutionScratch>,
    /// Eval-session marker (RSI-006). `false` (default) = production session.
    /// `true` = excluded from production analytics. Set by the RPC layer from
    /// `LaunchSessionParams.is_eval`.
    pub is_eval: bool,
    /// Bypass ContextPipeline assembly when true (RSI-006). When `true`, the
    /// daemon does NOT prepend the kind-specific preamble or run
    /// `ContextPipeline::assemble`; the caller's `system_prompt` is used
    /// verbatim. Required for ±5% reproducibility on eval replays.
    pub skip_context_pipeline: bool,
    /// Declared capability class for routing enforcement (RSI-010).
    /// Stamped onto `Session.capability_class` at launch. Resolved from the
    /// leading `/<command>` in `query` via the `CommandRegistry`, or passed
    /// explicitly by an RPC caller. `None` = free-text launch (no comparison
    /// performed by the validator).
    pub capability_class: Option<rsi_common::types::CapabilityClass>,
    /// Multi-tag set for this session. Validated and normalized by the RPC
    /// handler before LaunchConfig construction. Spawn coordinator inherits
    /// from emitter via `tags_for()`. Persisted via `update_session_tags`
    /// after `launch_session` returns.
    pub tags: Vec<String>,
    /// Topology node id to bind this session to. `None` = unbound (manual
    /// spawn or pre-topology session). Set by SpawnCoordinator binding block
    /// (Phase 7). RPC-direct launches always default to `None`.
    pub topology_node_id: Option<String>,
    /// Topology iteration for this `(epic_id, topology_node_id)` binding.
    /// Default 0 = unbound or first iteration. Hard-capped at
    /// `MAX_ITERATIONS = 32` at spawn time.
    pub topology_iteration: u32,
    /// K1-only immutable source/correlation selector. `None` preserves the
    /// ordinary launch path byte-for-byte.
    pub closure_selector: Option<crate::closure_kernel::ClosureLaunchSelectorV1>,
}

/// Stamp the exact daemon-owned execution environment shared by every CLI
/// provider.  Scratch revalidation happens at the final command-construction
/// boundary so replacement or mount drift cannot reach a child process.
pub(crate) fn stamp_execution_environment(
    cmd: &mut Command,
    config: &LaunchConfig,
    invocation_id: uuid::Uuid,
) -> Result<()> {
    use rsi_common::identity;

    if let Some(session_id) = config.rsi_session_id {
        cmd.env(identity::ENV_SESSION_ID, session_id.to_string());
    }
    cmd.env(identity::ENV_MODEL_INVOCATION_ID, invocation_id.to_string());
    if let Some(socket_path) = &config.rsi_socket {
        cmd.env(identity::ENV_SOCKET, socket_path.as_os_str());
    }
    if let Some(token) = &config.rsi_session_token {
        cmd.env(identity::ENV_SESSION_TOKEN, token);
    }
    if let Some(role) = config.session_kind.and_then(claude_agent_role_for_kind) {
        cmd.env(identity::ENV_CLAUDE_AGENT_ROLE, role);
    }
    if let Some(scratch) = &config.execution_scratch {
        scratch.revalidate()?;
        cmd.env(
            identity::ENV_PROCESS_OWNERSHIP_NAMESPACE,
            identity::process_ownership_namespace(),
        );
        cmd.env(identity::ENV_CARGO_TARGET_DIR, scratch.target());
        cmd.env(identity::ENV_TMPDIR, scratch.temp());
    } else if let Some(dir) = &config.cargo_target_dir {
        // Compatibility for pre-existing internally-constructed sandbox
        // configs; normal Slice 8 paths always carry a descriptor.
        cmd.env(identity::ENV_CARGO_TARGET_DIR, dir);
    }
    Ok(())
}

/// Maps a `SessionKind` to the `$CLAUDE_AGENT_ROLE` value expected by
/// `tools/git-hooks/pre-commit` (RSI-021 branch/path invariant gate).
///
/// Only implementer kinds are stamped. The hook recognizes
/// `pipeline-research` | `pipeline-plan` | `pipeline-implement` (any other
/// non-empty value fails closed), but the research/plan arms encode the
/// standalone RPI slash-command pipeline — they require commits on `main`
/// restricted to `thoughts/shared/{research,plans}/**`. rsi-managed sessions
/// do not work that way: `Story` sessions act as campaign masters (ledger,
/// review, and merge commits on `rolling`/sandbox branches) and `Research`
/// sessions commit research docs on their `rsi/<id>` sandbox branches, so
/// stamping those roles would fail-close every such commit the moment hooks
/// are installed. They return `None` until the hook's research/plan arms are
/// reconciled with rsi session behavior.
///
///   - implementer leaves (`Standard`, `TaskRabbit`, `Bug`, `Task`,
///     `Feature`, `Refactor`) -> `pipeline-implement` (must not commit on
///     main/master — matches rsi sandbox-branch reality)
///   - `Research`, `Story` -> `None` (see above)
///   - container kinds (`Group`, `Epic`) never spawn a provider subprocess
///     (`rsi_common::types::is_leaf_kind`) -> `None`, like any future
///     variant not yet classified here
///
/// Single mapping site shared by every provider's env stamping
/// (`claude.rs`, `codex.rs`, `agy.rs`, `codex_app_server.rs`).
#[must_use]
pub const fn claude_agent_role_for_kind(
    kind: rsi_common::types::SessionKind,
) -> Option<&'static str> {
    use rsi_common::types::SessionKind;
    match kind {
        SessionKind::Standard
        | SessionKind::TaskRabbit
        | SessionKind::Bug
        | SessionKind::Task
        | SessionKind::Feature
        | SessionKind::Refactor => Some("pipeline-implement"),
        _ => None,
    }
}

/// Raw event from Claude's stream-json output.
/// Parsed from NDJSON lines.
#[derive(Debug, Clone, Deserialize)]
pub struct StreamEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(flatten)]
    pub data: Value,
}

/// Wraps a running Claude CLI process (without the event receiver).
/// The receiver is returned separately to avoid lock contention.
pub struct ClaudeProcess {
    child: Child,
}

impl ClaudeProcess {
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

    /// Wait for the process to exit.
    pub async fn wait(&mut self) -> Result<std::process::ExitStatus> {
        Ok(self.child.wait().await?)
    }

    /// Check if the process is still running.
    pub fn try_wait(&mut self) -> Result<Option<std::process::ExitStatus>> {
        Ok(self.child.try_wait()?)
    }
}

/// Operator-selected isolation of an untrusted repository's Claude
/// configuration for `-p` launches (SECURITY).
///
/// Background `[source]` (`claude --help`, CLI 2.1.259; and
/// <https://code.claude.com/docs/en/headless>): a `-p` session shows no
/// workspace-trust dialog and no per-server approval prompt, so by default it
/// runs the hooks in the working directory's `.claude/settings.json` and
/// connects the servers in its `.mcp.json` — even in a folder the operator has
/// never trusted. RSI additionally pins `--permission-mode bypassPermissions`,
/// so the untrusted repo's hooks execute unprompted.
///
/// Deliberately NOT built on two other flags:
/// * `--settings` only layers values on top (`[source]` "load *additional*
///   settings from"); it merges rather than replaces, so it suppresses
///   nothing and provides no isolation.
/// * `--bare` is the mode the docs recommend for scripted/SDK calls and
///   "will become the default for `-p` in a future release" — but it is
///   deliberately NOT adopted here. See [`ClaudeConfigIsolation`]'s
///   `--bare` note below.
///
/// # Why not `--bare`, and what a future migration would require
///
/// `[source]` (`claude --help`, 2.1.259): bare mode skips "hooks, LSP, plugin
/// sync, attribution, auto-memory, background prefetches, keychain reads, and
/// CLAUDE.md auto-discovery", and "Anthropic auth is strictly
/// `ANTHROPIC_API_KEY` or `apiKeyHelper` via `--settings` (OAuth and keychain
/// are never read)."
///
/// RSI authenticates ambiently through the operator's existing OAuth login, so
/// flipping to `--bare` today would fail every session at auth. Adopting it
/// later requires, at minimum:
///
/// 1. A credential path: either an `ANTHROPIC_API_KEY` the daemon can supply
///    per spawn, or an `apiKeyHelper` passed in `--settings` JSON. Both are
///    new secret-handling surfaces (storage, redaction, rotation) that RSI
///    does not have today.
/// 2. Re-supplying what bare mode drops, explicitly: `--add-dir` for the
///    CLAUDE.md directories RSI relies on, plus `--mcp-config`/`--agents`/
///    `--plugin-dir` for anything a session is expected to keep.
/// 3. A migration story for sessions that authenticate as an organization
///    OAuth login, which has no API-key equivalent.
///
/// Until then this enum delivers the hook/MCP isolation `--bare` would give
/// us, using only flags that leave auth resolution untouched.
///
/// `Off` is the default and emits no flags at all, keeping the argv
/// byte-identical to the pre-feature launch path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaudeConfigIsolation {
    /// No isolation. Today's behavior, byte-for-byte: the project's settings
    /// sources and MCP servers are loaded.
    Off,
    /// Load only the operator's own `user` settings source, dropping the
    /// repository-supplied `project` and `local` sources (and therefore their
    /// hooks and permission rules). MCP discovery is untouched, so the
    /// operator keeps their own configured servers — including the project's
    /// `.mcp.json`.
    Settings,
    /// `Settings`, plus `--strict-mcp-config` so no MCP server is connected
    /// except one passed via `--mcp-config` (RSI passes none, so: none).
    ///
    /// Note this also drops the operator's OWN user-scope servers, not just
    /// the repository's `.mcp.json` `[observed]` — hence it is a distinct
    /// level rather than folded into `Settings`.
    Strict,
}

impl ClaudeConfigIsolation {
    pub(crate) fn cli_arg(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Settings => "settings",
            Self::Strict => "strict",
        }
    }

    pub(crate) fn parse(raw: &str) -> Option<Self> {
        let normalized = raw.trim().to_ascii_lowercase().replace('_', "-");
        match normalized.as_str() {
            "off" => Some(Self::Off),
            "settings" => Some(Self::Settings),
            "strict" => Some(Self::Strict),
            _ => None,
        }
    }

    /// Flags this policy appends to the `claude` argv, in order.
    ///
    /// `Off` returns an empty slice, which is what keeps the default launch
    /// argv byte-identical to the pre-feature build.
    pub(crate) fn cli_flags(self) -> &'static [&'static str] {
        match self {
            Self::Off => &[],
            // `[source]` `--setting-sources <sources>`: "Comma-separated list
            // of setting sources to load (user, project, local)." Omitting the
            // flag loads all three; naming only `user` drops the two the
            // untrusted repository controls.
            Self::Settings => &["--setting-sources", "user"],
            // `[source]` `--strict-mcp-config`: "Only use MCP servers from
            // --mcp-config, ignoring all other MCP configurations."
            Self::Strict => &["--setting-sources", "user", "--strict-mcp-config"],
        }
    }
}

/// Client for interacting with the Claude CLI.
pub struct ClaudeClient {
    binary_path: PathBuf,
    /// Daemon-global runtime settings, shared with every other holder of the
    /// same `Arc`. Read live in [`ClaudeClient::launch`] so an
    /// `UpdateDaemonConfig` mutation takes effect on the next spawn without a
    /// daemon restart — the same contract `CodexClient` has for
    /// `codex_sandbox_mode`.
    ///
    /// `None` = discovery-only client (see [`ClaudeClient::for_discovery`]),
    /// which never spawns a session subprocess.
    runtime_config: Option<Arc<RuntimeConfig>>,
}

/// Map an effort string onto the value the Claude CLI accepts, or `None` when
/// the CLI would reject it.
///
/// The accepted set is `low`, `medium`, `high`, `xhigh`, `max` (observed from
/// `claude --help`, CLI 2.1.259). Deliberately exact-match: the CLI is
/// case-sensitive, so a near-miss is a miss.
pub(crate) fn claude_effort_level(effort: Option<&str>) -> Option<&'static str> {
    match effort {
        Some("low") => Some("low"),
        Some("medium") => Some("medium"),
        Some("high") => Some("high"),
        Some("xhigh") => Some("xhigh"),
        Some("max") => Some("max"),
        _ => None,
    }
}

/// Validate `config.effort` before it reaches `--effort`.
///
/// An unknown `--effort` VALUE is not rejected by the CLI: it prints
/// `Warning: Unknown --effort value '<x>' — ignoring it and using the default
/// effort.` on stderr and runs anyway (V-006). That warning only reaches the
/// operator at end-of-stream as a "Provider diagnostic", and nothing corrects
/// the `effort` RSI persisted — so the session record says `ultra` while the
/// model ran at the default. `ultra` is in RSI's own vocabulary
/// (`normalize_orchestration_max_child_effort`) and `AgentSpawnChild.effort`
/// has no enum, so the hardest-classified work was the most likely to silently
/// downgrade.
///
/// Rejecting the launch matches the Codex path, which already fails an
/// unsupported effort with `InvalidParam`
/// (`validated_codex_reasoning_effort`), and keeps the two providers
/// symmetric. Silently dropping the flag was rejected as a fix: it reproduces
/// the same "record and behavior disagree" defect one layer up.
fn validated_claude_effort(config: &LaunchConfig) -> Result<Option<&'static str>> {
    let Some(raw_effort) = config.effort.as_deref() else {
        return Ok(None);
    };
    claude_effort_level(Some(raw_effort))
        .map(Some)
        .ok_or_else(|| {
            DaemonError::InvalidParam(format!(
                "unsupported Claude effort '{raw_effort}' (valid: low, medium, high, xhigh, max)"
            ))
        })
}

impl ClaudeClient {
    /// Create a new client, finding the Claude binary in PATH.
    pub fn new(runtime_config: Arc<RuntimeConfig>) -> Result<Self> {
        let binary_path = which::which("claude").map_err(|_| DaemonError::ClaudeBinaryNotFound)?;

        tracing::info!(path = %binary_path.display(), "Found Claude binary");
        Ok(Self {
            binary_path,
            runtime_config: Some(runtime_config),
        })
    }

    /// Build a client for model discovery only.
    ///
    /// Discovery is catalog-only and never starts a provider process (see
    /// [`ClaudeClient::discover_models`]), so it has no working directory to
    /// isolate and needs no runtime config. Calling [`ClaudeClient::launch`]
    /// on such a client is still safe: a missing runtime config resolves to
    /// [`ClaudeConfigIsolation::Off`], i.e. the pre-feature argv.
    pub fn for_discovery() -> Result<Self> {
        let binary_path = which::which("claude").map_err(|_| DaemonError::ClaudeBinaryNotFound)?;

        Ok(Self {
            binary_path,
            runtime_config: None,
        })
    }

    /// Resolve the operator's configured isolation policy for this spawn.
    ///
    /// Read fresh on every launch so an `UpdateDaemonConfig` mutation applies
    /// to the next session without a daemon restart.
    fn config_isolation(&self) -> ClaudeConfigIsolation {
        let Some(runtime_config) = self.runtime_config.as_ref() else {
            return ClaudeConfigIsolation::Off;
        };
        let raw = runtime_config.claude_config_isolation.read().clone();
        // Defensive fallback — `update_field` validates on write, so an
        // unparseable value should never reach here. Failing to `Off` keeps a
        // corrupted setting from silently changing launch behavior.
        ClaudeConfigIsolation::parse(&raw).unwrap_or(ClaudeConfigIsolation::Off)
    }

    /// Check if the Claude binary is available without creating a client.
    pub fn is_available() -> bool {
        which::which("claude").is_ok()
    }

    /// Discover the catalogued Claude models for the picker.
    ///
    /// Discovery is deliberately catalog-only: resolving aliases by launching
    /// the CLI was a paid, un-attributed model invocation hidden behind a
    /// picker refresh. Keep historical catalog IDs parseable, but never start
    /// a provider process during discovery.
    pub async fn discover_models(&self) -> Result<Vec<(String, String)>> {
        let mut models: Vec<(String, String)> = Vec::new();
        append_catalog_family(&mut models, "fable");

        append_catalog_family(&mut models, "opus");

        append_catalog_family(&mut models, "sonnet");

        append_catalog_family(&mut models, "haiku");

        if models.is_empty() {
            Err(DaemonError::Process(
                "Failed to discover any models".to_string(),
            ))
        } else {
            Ok(models)
        }
    }

    /// Launch a new Claude session.
    /// Returns the process handle, the event receiver, and the stdin handle (for flush injection).
    /// This design prevents lock contention in the session manager.
    pub(crate) fn launch(
        &self,
        config: &LaunchConfig,
        execution: CliExecutionCapability,
    ) -> Result<(ClaudeProcess, mpsc::Receiver<StreamEvent>)> {
        let invocation_id = execution.invocation_id();
        let mut cmd = Command::new(&self.binary_path);
        cmd.args([
            "-p",
            &config.query,
            "--output-format",
            "stream-json",
            "--verbose",
            "--permission-mode",
            "bypassPermissions",
        ]);

        // SECURITY: optionally stop this session from executing the working
        // directory's own Claude configuration. `Off` (the default) appends
        // nothing, so the argv above is exactly what it was before this
        // feature existed.
        //
        // Ordering note: `--permission-mode` is passed unconditionally, above
        // and outside this block, on purpose. `[source]` permission-modes: a
        // `defaultMode` of `auto` or `bypassPermissions` "doesn't take effect"
        // from a settings file, so routing RSI's permission intent through
        // settings instead of the flag would silently stop working. For `-p`
        // the built-in starting mode is Manual, so dropping the flag would
        // break every agent.
        cmd.args(self.config_isolation().cli_flags());

        // Set working directory
        if let Some(dir) = &config.working_dir {
            cmd.current_dir(dir);
        }

        // Optional model override
        if let Some(model) = &config.model {
            cmd.args(["--model", model]);
        }

        // Optional max turns
        if let Some(max) = &config.max_turns {
            cmd.args(["--max-turns", &max.to_string()]);
        }

        // Optional system prompt
        if let Some(prompt) = &config.system_prompt {
            cmd.args(["--system-prompt", prompt]);
        }

        // Resume a previous session
        if let Some(resume_id) = &config.resume_session_id {
            cmd.args(["--resume", resume_id]);
        }

        // Optional effort level (Claude Code 1.0.33+), validated against the
        // values the CLI actually accepts — see `validated_claude_effort`.
        if let Some(effort) = validated_claude_effort(config)? {
            cmd.args(["--effort", effort]);
        }

        stamp_execution_environment(&mut cmd, config, invocation_id)?;

        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        configure_tokio_process_group(&mut cmd, ProcessContainment::Group)?;

        let mut child = execution
            .bind_command(RuntimeExecutionRoute::ClaudeCli, cmd)
            .spawn()?;
        let child_pid = child.id();
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| DaemonError::Process("Failed to capture stdout".to_string()))?;
        let stderr = child.stderr.take();

        // Create channel for stream events
        let (event_tx, event_rx) = mpsc::channel(100);

        // Spawn task to read and parse NDJSON from stdout
        let stderr_tx = event_tx.clone();
        tokio::spawn(async move {
            let mut lines = BoundedLines::new(stdout, PROVIDER_MAX_LINE_BYTES);

            loop {
                let line = match lines.next_line().await {
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
                match serde_json::from_str::<StreamEvent>(&line) {
                    Ok(event) => {
                        if event_tx.send(event).await.is_err() {
                            break; // Receiver dropped
                        }
                    }
                    Err(e) => {
                        tracing::warn!(line = %line, error = %e, "Failed to parse stream event");
                        // Send a synthetic error event so TUI can display it
                        let error_event = StreamEvent {
                            event_type: "parse_error".to_string(),
                            data: serde_json::json!({
                                "error": e.to_string(),
                                "raw_line": line,
                            }),
                        };
                        if event_tx.send(error_event).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });

        // Spawn task to capture stderr and surface errors as synthetic events
        if let Some(stderr) = stderr {
            tokio::spawn(async move {
                let mut lines = BoundedLines::new(stderr, PROVIDER_MAX_LINE_BYTES);
                let mut stderr_lines = Vec::new();
                let mut stderr_bytes = 0_usize;

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
                    if !line.trim().is_empty() {
                        tracing::warn!(line = %line, "Claude CLI stderr");
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
                        stderr_lines.push(line);
                    }
                }

                // Send accumulated stderr as a single synthetic error event
                if !stderr_lines.is_empty() {
                    let combined = stderr_lines.join("\n");
                    let error_event = StreamEvent {
                        event_type: "process_error".to_string(),
                        data: serde_json::json!({
                            "error": combined,
                            "source": "stderr",
                        }),
                    };
                    let _ = stderr_tx.send(error_event).await;
                }
            });
        }

        Ok((ClaudeProcess { child }, event_rx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use tempfile::TempDir;

    fn launch_config_for_provider_test(query: &str) -> LaunchConfig {
        LaunchConfig {
            query: query.to_string(),
            title: None,
            agent_role: None,
            epic_spawn_ordinal: None,
            working_dir: None,
            provider: None,
            model: None,
            configured_context_window: None,
            max_turns: None,
            system_prompt: None,
            resume_session_id: None,
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
            model_invocation_purpose: ModelInvocationPurpose::SessionLaunchFresh,
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

    fn execution_scratch_fixture() -> (TempDir, SandboxExecutionScratch) {
        let base = std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap().join("target"))
            .join("slice8-provider-environment-fixtures");
        fs::create_dir_all(&base).unwrap();
        let root = tempfile::Builder::new()
            .prefix("scratch-")
            .tempdir_in(base)
            .unwrap();
        let scratch = SandboxExecutionScratch::prepare_for_test(root.path()).unwrap();
        (root, scratch)
    }

    fn command_environment(cmd: &Command) -> std::collections::BTreeMap<String, String> {
        cmd.as_std()
            .get_envs()
            .filter_map(|(key, value)| {
                value.map(|value| {
                    (
                        key.to_string_lossy().into_owned(),
                        value.to_string_lossy().into_owned(),
                    )
                })
            })
            .collect()
    }

    #[test]
    fn execution_environment_stamps_authenticated_scratch_and_identity() {
        use rsi_common::identity;

        let (_root, scratch) = execution_scratch_fixture();
        let session_id = uuid::Uuid::new_v4();
        let invocation_id = uuid::Uuid::new_v4();
        let mut config = launch_config_for_provider_test("scratch");
        config.rsi_session_id = Some(session_id);
        config.execution_scratch = Some(scratch.clone());
        let mut cmd = Command::new("true");
        stamp_execution_environment(&mut cmd, &config, invocation_id).unwrap();
        let env = command_environment(&cmd);

        assert_eq!(
            env.get(identity::ENV_CARGO_TARGET_DIR),
            Some(&scratch.target().display().to_string())
        );
        assert_eq!(
            env.get(identity::ENV_TMPDIR),
            Some(&scratch.temp().display().to_string())
        );
        assert_eq!(
            env.get(identity::ENV_SESSION_ID),
            Some(&session_id.to_string())
        );
        assert_eq!(
            env.get(identity::ENV_MODEL_INVOCATION_ID),
            Some(&invocation_id.to_string())
        );
        assert_eq!(
            env.get(identity::ENV_PROCESS_OWNERSHIP_NAMESPACE),
            Some(&identity::process_ownership_namespace())
        );
    }

    #[test]
    fn ordinary_cli_environment_matches_pre_slice8_identity_without_scratch_ownership() {
        use rsi_common::identity;

        let config = launch_config_for_provider_test("ordinary");
        let mut cmd = Command::new("true");
        stamp_execution_environment(&mut cmd, &config, uuid::Uuid::new_v4()).unwrap();
        let env = command_environment(&cmd);
        assert!(!env.contains_key(identity::ENV_CARGO_TARGET_DIR));
        assert!(!env.contains_key(identity::ENV_TMPDIR));
        assert!(!env.contains_key(identity::ENV_PROCESS_OWNERSHIP_NAMESPACE));
    }

    #[test]
    fn test_claude_agent_role_for_kind_maps_implementer_kinds() {
        use rsi_common::types::SessionKind;

        for kind in [
            SessionKind::Standard,
            SessionKind::TaskRabbit,
            SessionKind::Bug,
            SessionKind::Task,
            SessionKind::Feature,
            SessionKind::Refactor,
        ] {
            assert_eq!(
                claude_agent_role_for_kind(kind),
                Some("pipeline-implement"),
                "expected pipeline-implement for {kind:?}"
            );
        }
    }

    #[test]
    fn test_claude_agent_role_for_kind_non_implementers_are_none() {
        use rsi_common::types::SessionKind;

        // Research/Story sessions commit on sandbox branches (research docs,
        // master ledgers/reviews/merges); the hook's pipeline-research and
        // pipeline-plan arms demand main-only commits under
        // thoughts/shared/{research,plans}/**, so stamping those roles would
        // fail-close every such commit. Deliberately unstamped.
        assert_eq!(claude_agent_role_for_kind(SessionKind::Research), None);
        assert_eq!(claude_agent_role_for_kind(SessionKind::Story), None);

        // Container kinds never spawn a provider subprocess, so no role is
        // stamped for them.
        assert_eq!(claude_agent_role_for_kind(SessionKind::Group), None);
        assert_eq!(claude_agent_role_for_kind(SessionKind::Epic), None);
    }

    /// Build a client with no operator isolation configured — the state every
    /// pre-existing argv test was written against.
    fn test_client(binary_path: PathBuf) -> ClaudeClient {
        ClaudeClient {
            binary_path,
            runtime_config: None,
        }
    }

    /// Build a client whose daemon-global setting selects `policy`.
    fn test_client_with_isolation(binary_path: PathBuf, policy: &str) -> ClaudeClient {
        let mut config = Config::default();
        config.claude_config_isolation = policy.to_string();
        ClaudeClient {
            binary_path,
            runtime_config: Some(RuntimeConfig::from_config(&config)),
        }
    }

    #[cfg(unix)]
    fn install_fake_claude(dir: &Path) -> (PathBuf, PathBuf) {
        let binary_path = dir.join("claude");
        let args_path = dir.join("claude_args.bin");
        fs::write(
            &binary_path,
            r#"#!/usr/bin/env bash
set -euo pipefail
out="$(dirname "$0")/claude_args.bin"
: > "$out"
for arg in "$@"; do
  printf '%s\0' "$arg" >> "$out"
done
printf '%s|%s|%s|%s|%s' "${CARGO_TARGET_DIR:-}" "${TMPDIR:-}" "${RSI_PROCESS_OWNERSHIP_NAMESPACE:-}" "${RSI_SESSION_ID:-}" "${RSI_MODEL_INVOCATION_ID:-}" > "$(dirname "$0")/claude_exec_env.txt"
printf '{"type":"result","subtype":"turn_completed","usage":{"input_tokens":0,"output_tokens":0}}\n'
"#,
        )
        .expect("write fake claude");
        let mut perms = fs::metadata(&binary_path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&binary_path, perms).unwrap();
        (binary_path, args_path)
    }

    fn read_nul_args(path: &Path) -> Vec<String> {
        let bytes = fs::read(path).expect("read captured args");
        bytes
            .split(|b| *b == 0)
            .filter(|arg| !arg.is_empty())
            .map(|arg| String::from_utf8(arg.to_vec()).expect("utf8 arg"))
            .collect()
    }

    fn assert_arg_pair(args: &[String], flag: &str, expected_value: &str) {
        let pos = args
            .iter()
            .position(|arg| arg == flag)
            .unwrap_or_else(|| panic!("missing {flag} in args: {args:?}"));
        assert_eq!(
            args.get(pos + 1).map(String::as_str),
            Some(expected_value),
            "wrong value for {flag}; args: {args:?}"
        );
    }

    #[test]
    fn test_claude_client_availability() {
        // This test documents behavior, doesn't assert (Claude may or may not be installed)
        let available = ClaudeClient::is_available();
        println!("Claude CLI available: {}", available);
    }

    #[test]
    fn test_stream_event_parsing() {
        let json = r#"{"type":"system","subtype":"init","session_id":"abc123"}"#;
        let event: StreamEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event_type, "system");
        assert_eq!(event.data.get("subtype").unwrap(), "init");
    }

    #[test]
    fn test_stream_event_with_content() {
        let json = r#"{"type":"assistant","session_id":"abc","message":{"role":"assistant","content":[{"type":"text","text":"Hello"}]}}"#;
        let event: StreamEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event_type, "assistant");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn launch_sends_query_and_system_prompt_as_separate_cli_args() {
        let block = "<docregblock>\n\
/spawn_child kind=Bug tags=e2e\n\
QUERY:\n\
worker prompt body\n\
</docregblock>";
        let directive = crate::session::spawn_directive::SpawnDirective::parse(block)
            .unwrap()
            .unwrap();

        let tmp = TempDir::new().expect("tempdir");
        let (binary_path, args_path) = install_fake_claude(tmp.path());
        let client = test_client(binary_path);

        let mut config = launch_config_for_provider_test(&directive.query);
        config.system_prompt = Some("assembled worker preamble".to_string());

        let (mut process, _rx) = client
            .launch(
                &config,
                CliExecutionCapability::for_test(RuntimeExecutionRoute::ClaudeCli),
            )
            .expect("launch fake claude");
        let status = process.wait().await.expect("wait fake claude");
        assert!(status.success(), "fake claude failed: {status}");

        let args = read_nul_args(&args_path);
        assert_arg_pair(&args, "-p", "worker prompt body");
        assert_arg_pair(&args, "--system-prompt", "assembled worker preamble");
        assert!(
            !args
                .iter()
                .any(|arg| arg.contains("<docregblock>") || arg.contains("/spawn_child")),
            "docregblock wrapper leaked into Claude argv: {args:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn claude_launch_stamps_authenticated_execution_environment() {
        let tmp = TempDir::new().unwrap();
        let (binary_path, _) = install_fake_claude(tmp.path());
        let client = test_client(binary_path);
        let (_root, scratch) = execution_scratch_fixture();
        let session_id = uuid::Uuid::new_v4();
        let mut config = launch_config_for_provider_test("scratch");
        config.execution_scratch = Some(scratch.clone());
        config.rsi_session_id = Some(session_id);
        let (mut process, _) = client
            .launch(
                &config,
                CliExecutionCapability::for_test(RuntimeExecutionRoute::ClaudeCli),
            )
            .unwrap();
        assert!(process.wait().await.unwrap().success());
        let values = fs::read_to_string(tmp.path().join("claude_exec_env.txt")).unwrap();
        let parts: Vec<_> = values.split('|').collect();
        assert_eq!(parts[0], scratch.target().display().to_string());
        assert_eq!(parts[1], scratch.temp().display().to_string());
        assert_eq!(
            parts[2],
            rsi_common::identity::process_ownership_namespace()
        );
        assert_eq!(parts[3], session_id.to_string());
        assert!(!parts[4].is_empty());
    }

    #[test]
    fn test_launch_config() {
        let config = LaunchConfig {
            query: "Hello".to_string(),
            title: None,
            agent_role: None,
            epic_spawn_ordinal: None,
            working_dir: Some(PathBuf::from("/tmp")),
            provider: None,
            model: Some("sonnet".to_string()),
            configured_context_window: None,
            max_turns: Some(5),
            system_prompt: None,
            resume_session_id: None,
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
            model_invocation_purpose: ModelInvocationPurpose::SessionLaunchFresh,
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
        };
        assert_eq!(config.query, "Hello");
        assert_eq!(config.max_turns, Some(5));
    }

    /// F-155: this used to rebuild the argv by hand with a local
    /// `std::process::Command` and assert on its `Debug` string, so it passed
    /// no matter what `launch()` actually emitted. It now runs the real
    /// builder against the fake `claude` binary.
    #[cfg(unix)]
    #[tokio::test]
    async fn launch_includes_permission_and_verbose_flags() {
        let tmp = TempDir::new().expect("tempdir");
        let (binary_path, args_path) = install_fake_claude(tmp.path());
        let client = test_client(binary_path);

        let config = launch_config_for_provider_test("test query");
        let (mut process, _rx) = client
            .launch(
                &config,
                CliExecutionCapability::for_test(RuntimeExecutionRoute::ClaudeCli),
            )
            .expect("launch fake claude");
        let status = process.wait().await.expect("wait fake claude");
        assert!(status.success(), "fake claude failed: {status}");

        let args = read_nul_args(&args_path);
        assert!(
            args.iter().any(|arg| arg == "--verbose"),
            "argv should include --verbose: {args:?}"
        );
        assert_arg_pair(&args, "--permission-mode", "bypassPermissions");
        assert_arg_pair(&args, "--output-format", "stream-json");
        assert_arg_pair(&args, "-p", "test query");
    }

    /// Capture the argv `launch()` actually emits for `policy`.
    #[cfg(unix)]
    async fn argv_for_isolation(policy: Option<&str>) -> Vec<String> {
        let tmp = TempDir::new().expect("tempdir");
        let (binary_path, args_path) = install_fake_claude(tmp.path());
        let client = match policy {
            Some(p) => test_client_with_isolation(binary_path, p),
            None => test_client(binary_path),
        };
        let config = launch_config_for_provider_test("test query");
        let (mut process, _rx) = client
            .launch(
                &config,
                CliExecutionCapability::for_test(RuntimeExecutionRoute::ClaudeCli),
            )
            .expect("launch fake claude");
        let status = process.wait().await.expect("wait fake claude");
        assert!(status.success(), "fake claude failed: {status}");
        read_nul_args(&args_path)
    }

    /// BINDING CONSTRAINT: an operator who sets nothing must get the argv this
    /// launch path emitted before config isolation existed.
    ///
    /// This is a byte-for-byte equality assertion against the historical argv,
    /// not a spot check, so ANY future flag added unconditionally to `launch()`
    /// trips it and forces a deliberate decision about the default.
    #[cfg(unix)]
    #[tokio::test]
    async fn default_launch_argv_is_unchanged_by_config_isolation() {
        // The exact argv `launch()` built before this feature, for a config
        // whose optional fields are all `None`.
        let historical = vec![
            "-p".to_string(),
            "test query".to_string(),
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--verbose".to_string(),
            "--permission-mode".to_string(),
            "bypassPermissions".to_string(),
        ];

        // A daemon with no runtime config at all (e.g. a discovery client).
        assert_eq!(
            argv_for_isolation(None).await,
            historical,
            "absent runtime config must reproduce the historical argv"
        );

        // And the shipped default value of the new setting.
        assert_eq!(
            argv_for_isolation(Some("off")).await,
            historical,
            "the default `off` policy must reproduce the historical argv"
        );
    }

    /// The opt-in policies must actually reach the CLI.
    ///
    /// `[observed]` against claude 2.1.259 in a fixture repo carrying a
    /// `SessionStart` hook and a `.mcp.json`: the default argv ran the hook and
    /// connected the server; adding these flags blocked both while leaving
    /// `permissionMode: bypassPermissions` and ambient OAuth intact.
    #[cfg(unix)]
    #[tokio::test]
    async fn opt_in_config_isolation_passes_isolation_flags() {
        let settings = argv_for_isolation(Some("settings")).await;
        // Drops the repo-controlled `project` and `local` settings sources,
        // which is what carries its hooks.
        assert_arg_pair(&settings, "--setting-sources", "user");
        assert!(
            !settings.iter().any(|a| a == "--strict-mcp-config"),
            "`settings` must leave MCP discovery alone: {settings:?}"
        );

        let strict = argv_for_isolation(Some("strict")).await;
        assert_arg_pair(&strict, "--setting-sources", "user");
        assert!(
            strict.iter().any(|a| a == "--strict-mcp-config"),
            "`strict` must also isolate MCP servers: {strict:?}"
        );

        // Both levels must keep the permission mode on the CLI flag: a
        // `defaultMode` of `bypassPermissions` is ignored when it comes from a
        // settings file, so isolation must never push it into one.
        for args in [&settings, &strict] {
            assert_arg_pair(args, "--permission-mode", "bypassPermissions");
            assert_arg_pair(args, "-p", "test query");
        }
    }

    /// The setting is validated on write, and the default is `off`.
    #[test]
    fn config_isolation_policy_parses_and_defaults_to_off() {
        assert_eq!(
            ClaudeConfigIsolation::parse("off"),
            Some(ClaudeConfigIsolation::Off)
        );
        assert_eq!(
            ClaudeConfigIsolation::parse("settings"),
            Some(ClaudeConfigIsolation::Settings)
        );
        assert_eq!(
            ClaudeConfigIsolation::parse("strict"),
            Some(ClaudeConfigIsolation::Strict)
        );
        // Tolerant of the underscore spelling, like `CodexSandboxMode`.
        assert_eq!(
            ClaudeConfigIsolation::parse("  STRICT  "),
            Some(ClaudeConfigIsolation::Strict)
        );
        assert_eq!(ClaudeConfigIsolation::parse("bare"), None);
        assert_eq!(ClaudeConfigIsolation::parse(""), None);

        assert_eq!(Config::default().claude_config_isolation, "off");
        assert!(ClaudeConfigIsolation::Off.cli_flags().is_empty());
    }

    /// F-155 companion: same rewrite for the `--resume` argv test.
    #[cfg(unix)]
    #[tokio::test]
    async fn launch_with_resume_passes_session_id() {
        let tmp = TempDir::new().expect("tempdir");
        let (binary_path, args_path) = install_fake_claude(tmp.path());
        let client = test_client(binary_path);

        let mut config = launch_config_for_provider_test("follow up");
        config.resume_session_id = Some("abc-123-session".to_string());

        let (mut process, _rx) = client
            .launch(
                &config,
                CliExecutionCapability::for_test(RuntimeExecutionRoute::ClaudeCli),
            )
            .expect("launch fake claude");
        let status = process.wait().await.expect("wait fake claude");
        assert!(status.success(), "fake claude failed: {status}");

        let args = read_nul_args(&args_path);
        assert_arg_pair(&args, "--resume", "abc-123-session");
        assert_arg_pair(&args, "-p", "follow up");
    }

    /// V-006: a supported effort reaches the CLI verbatim.
    #[cfg(unix)]
    #[tokio::test]
    async fn launch_passes_supported_effort_to_cli() {
        let tmp = TempDir::new().expect("tempdir");
        let (binary_path, args_path) = install_fake_claude(tmp.path());
        let client = test_client(binary_path);

        let mut config = launch_config_for_provider_test("effort probe");
        config.effort = Some("xhigh".to_string());
        config.model = Some("claude-opus-5".to_string());
        config.max_turns = Some(7);

        let (mut process, _rx) = client
            .launch(
                &config,
                CliExecutionCapability::for_test(RuntimeExecutionRoute::ClaudeCli),
            )
            .expect("launch fake claude");
        let status = process.wait().await.expect("wait fake claude");
        assert!(status.success(), "fake claude failed: {status}");

        let args = read_nul_args(&args_path);
        assert_arg_pair(&args, "--effort", "xhigh");
        assert_arg_pair(&args, "--model", "claude-opus-5");
        assert_arg_pair(&args, "--max-turns", "7");
    }

    /// V-006: `ultra` is in RSI's vocabulary but not the CLI's. The CLI would
    /// warn and silently run at the DEFAULT effort while RSI kept recording
    /// `ultra`, so the launch is rejected instead — the same treatment the
    /// Codex path already gives an unsupported effort.
    #[cfg(unix)]
    #[tokio::test]
    async fn launch_rejects_effort_the_cli_would_silently_downgrade() {
        let tmp = TempDir::new().expect("tempdir");
        let (binary_path, args_path) = install_fake_claude(tmp.path());
        let client = test_client(binary_path);

        let mut config = launch_config_for_provider_test("ultra probe");
        config.effort = Some("ultra".to_string());

        let Err(error) = client.launch(
            &config,
            CliExecutionCapability::for_test(RuntimeExecutionRoute::ClaudeCli),
        ) else {
            panic!("unsupported effort must not launch");
        };
        assert!(
            matches!(error, DaemonError::InvalidParam(ref message) if message.contains("ultra")
                && message.contains("xhigh")),
            "expected an InvalidParam naming the rejected value and the valid set, got: {error}"
        );
        assert!(
            !args_path.exists(),
            "no provider process may be spawned for a rejected effort"
        );
    }

    #[test]
    fn claude_effort_level_accepts_exactly_the_cli_vocabulary() {
        for accepted in ["low", "medium", "high", "xhigh", "max"] {
            assert_eq!(
                claude_effort_level(Some(accepted)),
                Some(accepted),
                "{accepted} is accepted by the CLI"
            );
        }
        for rejected in ["ultra", "Max", "highest", "", " high"] {
            assert_eq!(
                claude_effort_level(Some(rejected)),
                None,
                "{rejected:?} is not a CLI effort value"
            );
        }
        assert_eq!(claude_effort_level(None), None);
    }
}

/// Push `(id, name)` onto `models` unless an entry with the same ID already
/// exists. Guards the per-family appends below against emitting a model twice.
fn push_unique(models: &mut Vec<(String, String)>, id: &str, name: String) {
    if !models.iter().any(|(existing, _)| existing == id) {
        models.push((id.to_string(), name));
    }
}

/// Append canonical-catalog entries for a single family (matched by substring,
/// e.g. "opus") that aren't already present, in catalog order (newest first).
///
/// The catalog itself lives in `rsi_common::claude_catalog` and is shared with
/// the TUI picker, so the daemon and the client can no longer drift apart.
/// Display names come from the catalog too — they are not re-derived here.
fn append_catalog_family(models: &mut Vec<(String, String)>, family: &str) {
    for spec in rsi_common::claude_catalog::CLAUDE_MODEL_CATALOG {
        if spec.id.contains(family) {
            push_unique(models, spec.id, spec.display_name.to_string());
        }
    }
}

#[cfg(test)]
mod model_discovery_tests {
    use super::*;

    #[test]
    fn test_append_catalog_family_dedupes_and_filters() {
        // A current entry already present is not duplicated, while the
        // still-selectable historical entries remain after it.
        let mut models = vec![("claude-opus-5-5".to_string(), "Opus 5.5 (1M)".to_string())];
        append_catalog_family(&mut models, "opus");
        assert_eq!(
            models,
            vec![
                ("claude-opus-5-5".to_string(), "Opus 5.5 (1M)".to_string()),
                ("claude-opus-4-8".to_string(), "Opus 4.8 (1M)".to_string()),
                ("claude-opus-4-5".to_string(), "Opus 4.5 (1M)".to_string()),
            ]
        );

        let mut catalog_only = Vec::new();
        append_catalog_family(&mut catalog_only, "opus");
        assert_eq!(catalog_only, models);

        // Other families are untouched.
        assert!(!models.iter().any(|(id, _)| id.contains("sonnet")));
    }

    /// The daemon-side picker list is exactly the canonical catalog — same
    /// models, same display names, same order.
    ///
    /// Half of the divergence guard for V-002/F-124/F-158: the daemon
    /// catalog and the TUI's `CLAUDE_MODELS` had drifted apart (the TUI
    /// offered `claude-opus-4-5`, the daemon did not) with no test pinning
    /// them. Both now project from `rsi_common::claude_catalog`; this test
    /// pins the daemon side and
    /// `crates/rsi/src/app/mod.rs::tui_claude_models_are_the_canonical_catalog`
    /// pins the client side, so a model added to one and not the other fails.
    #[tokio::test]
    async fn discover_models_returns_the_canonical_catalog() {
        let client = ClaudeClient {
            binary_path: PathBuf::from("/nonexistent/claude"),
            runtime_config: None,
        };
        let discovered = client
            .discover_models()
            .await
            .expect("catalog discovery never touches the binary");

        let expected: Vec<(String, String)> = rsi_common::claude_catalog::CLAUDE_MODEL_MENU
            .iter()
            .map(|(id, name)| (id.to_string(), name.to_string()))
            .collect();
        assert_eq!(discovered, expected);
    }
}
