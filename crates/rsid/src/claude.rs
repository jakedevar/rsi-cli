use crate::config::RuntimeConfig;
use crate::error::{DaemonError, Result};
use crate::model_control::CliExecutionCapability;
use crate::model_control::registry::RuntimeExecutionRoute;
use crate::process_control::{
    BoundedLineError, BoundedLines, CaptureLimits, PROVIDER_MAX_LINE_BYTES,
    PROVIDER_MAX_STDERR_BYTES, ProcessContainment, capture_bounded_with_stdin,
    configure_tokio_process_group,
};
use crate::sandbox::execution_scratch::SandboxExecutionScratch;
pub(crate) use crate::store_support::provider_defaults::claude_effort_level;
pub use crate::store_support::provider_settings::ClaudeConfigIsolation;
use rsi_common::model_control::{InvocationOwner, ModelInvocationPurpose};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

pub(crate) mod model_discovery;
pub(crate) mod turn_spool;

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
    /// redacting in-memory registry in `SessionManager` and the
    /// spawned process's environment carry it.
    pub rsi_session_token: Option<rsi_common::rpc::AgentTransportBearer>,
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
    /// #794: operator-set completion gates, persisted at fresh launch and
    /// resolved through `continued_from` on continue/rotation.
    pub completion_gates: Option<rsi_common::completion_gates::CompletionGates>,
    /// Keep a cross-provider child with no explicit model on the target
    /// provider's native default instead of applying a project model selected
    /// for a different provider. All ordinary launches retain project defaults.
    pub skip_project_model_default: bool,
    /// Operator-set (`LaunchSessionParams.tool_policy`) or inherited (spawned
    /// child) Harness tool policy. Persisted per session at launch; continues
    /// and rotations resolve it from the store, never from this field.
    pub tool_policy: Option<rsi_common::harness_tool_policy::HarnessToolPolicy>,
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
    /// #2759: this launch is the turn the daemon opened to obtain a node's
    /// handoff, so the harness validates the handoff's shape (Claude
    /// `--json-schema`; Codex `--output-schema` + `-o`). Set at the provider
    /// spawn chokepoint from `crate::structured_handoff`, never by a caller;
    /// `false` leaves every argv exactly as it was.
    pub structured_handoff: bool,
}

/// MCP server name for the agent-control gateway. Claude lists its tools as
/// `mcp__rsi-agent__rsi_control_*`; Codex uses the same server name.
/// #1549: Claude Code's native wake/cron tools never fire under headless
/// `claude -p`; denying them leaves RSI's `schedule_wake` / `AgentSubmitJob`
/// as the only way to wait.
const CLAUDE_DISALLOWED_TOOLS_ARG: &str =
    "--disallowedTools=ScheduleWakeup,CronCreate,CronDelete,CronList";

pub(crate) const AGENT_MCP_SERVER_NAME: &str = "rsi-agent";

/// #2758: the tool arguments of a read-only Claude session, in `=` form because
/// both flags are variadic. `--tools` names the built-in tools that exist at all
/// (no shell, no edit tool); it does not filter MCP tools, so the `rsi-agent`
/// server stays. `dontAsk` denies a tool that is not pre-approved, so
/// `--allowedTools` approves the same built-ins and the whole `rsi-agent`
/// server (`mcp__rsi-agent` is the server-wide rule). The `--settings` hooks
/// (custody `PreToolUse`, mail `PostToolUse`) are added separately and stay.
/// `[observed]` claude 2.1.301 `system/init`: `tools` lists the built-ins named
/// here plus `mcp__<server>__*`, and an MCP call without the allow rule is
/// denied in don't-ask mode.
fn claude_read_only_tool_args() -> [String; 2] {
    let tools = rsi_common::harness_tool_policy::CLAUDE_READ_ONLY_TOOLS.join(",");
    [
        format!("--tools={tools}"),
        format!("--allowedTools={tools},mcp__{AGENT_MCP_SERVER_NAME}"),
    ]
}

/// Every flag a Claude launch ([`ClaudeClient::launch_inner`]) and the model
/// discovery probe ([`model_discovery::PROBE_ARGS`]) pass, as passed. The
/// provider contract canary (#2755) asks the installed CLI whether it still
/// knows each: `claude --help` is not exhaustive (it omits `--max-turns`), so a
/// flag absent from the help text is checked with an `unknown option` probe.
/// Add a flag here in the same change that passes it; the test
/// `launch_flag_list_covers_the_derived_flags` pins the flags derived from
/// other constants. `--bare` is deliberately absent and pinned absent: RSI
/// authenticates through the ambient OAuth login, which `--bare` never reads.
pub(crate) const CLAUDE_LAUNCH_FLAGS: &[&str] = &[
    "-p",
    "--output-format",
    "--input-format",
    "--verbose",
    "--permission-mode",
    "--disallowedTools",
    "--tools",
    "--allowedTools",
    "--setting-sources",
    "--strict-mcp-config",
    "--mcp-config",
    "--settings",
    "--model",
    "--max-turns",
    "--system-prompt",
    "--resume",
    "--effort",
    "--fallback-model",
    "--max-budget-usd",
    "--no-session-persistence",
    "--disable-slash-commands",
    "--plugin-dir",
    // #2759: only on a topology node's handoff turn.
    "--json-schema",
];

/// The `--json-schema=<schema>` argument of a handoff turn (#2759).
pub(crate) fn structured_handoff_flag() -> String {
    format!(
        "--json-schema={}",
        rsi_common::structured_handoff::structured_handoff_schema_text()
    )
}

/// Claude Code environment variable that pins every subagent's model.
pub(crate) const CLAUDE_SUBAGENT_MODEL_ENV: &str = "CLAUDE_CODE_SUBAGENT_MODEL";
/// Claude Code environment variable that caps concurrent subagents.
pub(crate) const CLAUDE_MAX_CONCURRENT_SUBAGENTS_ENV: &str = "CLAUDE_CODE_MAX_CONCURRENT_SUBAGENTS";

/// The environment a Claude launch adds for the operator's subagent caps
/// (#2763, `[source]` Claude Code sub-agents docs). Empty for the defaults.
pub(crate) fn claude_subagent_env(
    caps: &crate::store_support::provider_settings::SubagentCaps,
) -> Vec<(&'static str, String)> {
    let mut env = Vec::new();
    if let Some(model) = &caps.model {
        env.push((CLAUDE_SUBAGENT_MODEL_ENV, model.clone()));
    }
    if let Some(cap) = caps.max_concurrent {
        env.push((CLAUDE_MAX_CONCURRENT_SUBAGENTS_ENV, cap.to_string()));
    }
    env
}

/// Whether two Claude model names are the same model: case-insensitive, and a
/// context-variant tag (`claude-opus-5[1m]`) does not make a different model.
pub(crate) fn same_claude_model(a: &str, b: &str) -> bool {
    use rsi_common::claude_catalog::strip_context_variant_suffix;
    strip_context_variant_suffix(a).eq_ignore_ascii_case(strip_context_variant_suffix(b))
}

/// The `--fallback-model` chain a Claude CLI launch of `model` passes (#2762):
/// the operator's `claude_fallback_models` without the session's own model
/// (the CLI refuses a fallback equal to the main model and would not start),
/// and empty for a Bedrock-routed model, whose names live in another
/// namespace. Also the set of models whose appearance next to the session's
/// own in a result's `modelUsage` means a fallback ran.
pub(crate) fn claude_fallback_chain_for(model: Option<&str>, chain: &[String]) -> Vec<String> {
    if model.and_then(crate::bedrock::bedrock_vendor).is_some() {
        return Vec::new();
    }
    chain
        .iter()
        .filter(|candidate| model.is_none_or(|own| !same_claude_model(own, candidate)))
        .cloned()
        .collect()
}

/// The `rsi-agent-mcp` binary installed next to the running daemon.
fn daemon_sibling_agent_mcp_path() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()?
        .parent()
        .map(|directory| directory.join("rsi-agent-mcp"))
}

/// Stamp the exact daemon-owned execution environment shared by every CLI
/// provider.  Scratch revalidation happens at the final command-construction
/// boundary so replacement or mount drift cannot reach a child process.
///
/// This is also the shared credential-scrub chokepoint (Claude, Codex, AGY
/// and the `CodexAppServer` `build_app_server_command`): see
/// [`crate::vault::scrub_credential_env`].
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
        cmd.env(identity::ENV_SESSION_TOKEN, token.expose_for_transport());
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
        scratch.stamp_build_env(cmd);
    } else if let Some(dir) = &config.cargo_target_dir {
        // Compatibility for pre-existing internally-constructed sandbox
        // configs; normal Slice 8 paths always carry a descriptor.
        cmd.env(identity::ENV_CARGO_TARGET_DIR, dir);
    }
    // #694 K1: no provider API key reaches an agent-facing CLI by
    // inheritance. A route credential the launch explicitly injected (Codex
    // fallback only) survives; every other slot/generic var is removed.
    crate::vault::scrub_credential_env(cmd);
    Ok(())
}

/// Point Claude Code at Amazon Bedrock when `config.model` is a Bedrock Claude
/// model ID (`[<geo>.]anthropic.claude-*`); otherwise leave `cmd` untouched.
///
/// Claude Code reads `CLAUDE_CODE_USE_BEDROCK`, `AWS_REGION` and the Bedrock
/// API key from its environment. The key is injected through the vault so the
/// exposure is recorded; unlike Codex, Claude Code's tool shells inherit it
/// (its subprocess scrub forces the `default` permission mode).
pub(crate) fn apply_bedrock_launch_env(
    cmd: &mut Command,
    config: &LaunchConfig,
    vault: &crate::vault::VaultHandle,
) -> Result<()> {
    let is_bedrock_claude = config
        .model
        .as_deref()
        .and_then(crate::bedrock::bedrock_vendor)
        == Some(crate::bedrock::BedrockVendor::Anthropic);
    if !is_bedrock_claude {
        return Ok(());
    }
    let region = crate::bedrock::region().map_err(DaemonError::Process)?;
    let resolved = crate::bedrock::credential_from(vault).map_err(DaemonError::Process)?;
    cmd.env("CLAUDE_CODE_USE_BEDROCK", "1");
    cmd.env("AWS_REGION", region);
    vault.inject_cli_credential(
        cmd,
        crate::vault::Slot::Bedrock,
        crate::bedrock::BEDROCK_ENV,
        &resolved.secret,
        rsi_common::provider_credentials::CliExposureConsumer::SessionClaudeCli,
        rsi_common::provider_credentials::CliExposureReason::Route,
    );
    Ok(())
}

/// hub #2919: the `--system-prompt` value of one Claude launch.
///
/// Without a `SessionStart` hook (`session_start_hook` false, the default
/// `provider_lifecycle_hooks`) this is exactly `config.system_prompt`. With one,
/// a prompt that starts with the session's startup preamble
/// ([`crate::session::preamble::startup_parts`] for its kind and working
/// directory, the head a fresh launch composes) passes the rest only and
/// defers the preamble to the hook, when it fits the hook's cap. A prompt that
/// carries the preamble but is too long keeps it. Any other launch (a resumed
/// turn) puts back a preamble the hook never delivered, in front of its prompt,
/// unless the prompt already starts with it.
pub(crate) fn claude_launch_system_prompt(
    config: &LaunchConfig,
    session_start_hook: bool,
) -> Option<String> {
    let Some(session_id) = config.rsi_session_id else {
        return config.system_prompt.clone();
    };
    let prompt = config.system_prompt.as_deref();
    if session_start_hook && let Some(split) = split_startup_preamble(config, prompt) {
        return deferred_or_kept(session_id, prompt, split);
    }
    let Some(preamble) = crate::session_start_preamble::take_undelivered(session_id) else {
        return config.system_prompt.clone();
    };
    if prompt.is_some_and(|prompt| prompt.starts_with(&preamble)) {
        return config.system_prompt.clone();
    }
    tracing::warn!(%session_id, "SessionStart hook did not deliver the preamble; it goes back in the prompt");
    Some(match prompt {
        Some(prompt) => format!("{preamble}\n\n{prompt}"),
        None => preamble,
    })
}

/// `(preamble, rest)` when `prompt` starts with the session's startup preamble.
fn split_startup_preamble(
    config: &LaunchConfig,
    prompt: Option<&str>,
) -> Option<(String, Option<String>)> {
    let prompt = prompt?;
    let parts = crate::session::preamble::startup_parts(
        config.session_kind.unwrap_or_default(),
        config.working_dir.as_deref()?,
    );
    if parts.is_empty() {
        return None;
    }
    let preamble = parts.join("\n\n");
    if prompt == preamble {
        return Some((preamble, None));
    }
    let rest = prompt.strip_prefix(&format!("{preamble}\n\n"))?.to_string();
    Some((preamble, Some(rest)))
}

fn deferred_or_kept(
    session_id: uuid::Uuid,
    prompt: Option<&str>,
    (preamble, rest): (String, Option<String>),
) -> Option<String> {
    if !crate::session_start_preamble::fits(&preamble) {
        tracing::info!(%session_id, chars = preamble.chars().count(), "preamble over the SessionStart context cap; it stays in the prompt");
        crate::session_start_preamble::forget(session_id);
        return prompt.map(str::to_string);
    }
    crate::session_start_preamble::defer(session_id, preamble);
    rest
}

/// The `rsi-rpc` binary installed next to the running daemon: the only
/// executable a boundary-mail hook may run (#1183 review). Never resolved from
/// `PATH`, so a workspace-controlled binary cannot receive the session token.
pub(crate) fn daemon_sibling_rsi_rpc_path() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()?
        .parent()
        .map(|directory| directory.join("rsi-rpc"))
}

/// The shell command a boundary-mail hook runs: `'<abs rsi-rpc>'
/// boundary-mail-hook [args]`, or `None` (no hook, so mail waits for turn end)
/// unless `rsi_rpc` is an existing absolute file whose UTF-8 path can be
/// single-quoted safely. The Codex CLI launch installs the same command.
pub(crate) fn boundary_mail_hook_command(rsi_rpc: Option<&Path>, args: &[&str]) -> Option<String> {
    let Some(path) = rsi_rpc
        .filter(|path| path.is_absolute() && path.is_file())
        .and_then(Path::to_str)
        .filter(|path| !path.contains('\'') && !path.contains('\n'))
    else {
        tracing::warn!(
            path = ?rsi_rpc,
            "trusted rsi-rpc sibling missing; no boundary-mail hook, mail waits for turn end"
        );
        return None;
    };
    let mut command = format!(
        "'{path}' {}",
        rsi_common::boundary_mail_hook::HOOK_SUBCOMMAND
    );
    for arg in args {
        command.push(' ');
        command.push_str(arg);
    }
    Some(command)
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

impl crate::vault::LaunchCredentialInputs for LaunchConfig {
    fn provider(&self) -> Option<rsi_common::types::SessionProvider> {
        self.provider
    }

    fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    fn has_openai_base_url(&self) -> bool {
        self.openai_base_url.is_some()
    }

    fn openai_api_key(&self) -> Option<&str> {
        self.openai_api_key.as_deref()
    }
}

/// Raw event from Claude's stream-json output (defined in `rsid-harness`, #2616,
/// because the harness tool context carries it).
pub use rsid_harness::stream::StreamEvent;

/// Wraps a running Claude CLI process (without the event receiver).
/// The receiver is returned separately to avoid lock contention.
pub struct CliTurnProcess {
    child: Option<Child>,
    stop: crate::process_control::WorkerStopHandle,
    pub(crate) detached: Option<turn_spool::DetachedTurn>,
}

pub type ClaudeProcess = CliTurnProcess;

impl CliTurnProcess {
    pub(crate) fn direct(child: Child, stop: crate::process_control::WorkerStopHandle) -> Self {
        Self {
            child: Some(child),
            stop,
            detached: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn from_child_for_route_test(child: Child) -> Self {
        let stop = crate::process_control::WorkerStopHandle::new(child.id(), None);
        Self::direct(child, stop)
    }

    #[cfg(test)]
    pub(crate) fn detached_for_test(child: Child, turn: turn_spool::DetachedTurn) -> Self {
        Self::detached_child(child, turn)
    }

    pub(crate) fn detached_child(child: Child, turn: turn_spool::DetachedTurn) -> Self {
        let stop = crate::process_control::WorkerStopHandle::new(child.id(), None);
        Self {
            stop,
            child: Some(child),
            detached: Some(turn),
        }
    }

    /// Send SIGINT to gracefully interrupt the session.
    pub fn interrupt(&self) -> Result<()> {
        if let Some(turn) = &self.detached {
            return turn.signal(nix::sys::signal::Signal::SIGINT);
        }
        if let Some(pid) = self.child.as_ref().and_then(Child::id) {
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
        if let Some(turn) = self.detached.clone() {
            if turn.is_handed_off() {
                return Ok(());
            }
            let stop = async {
                turn.custody(uuid::Uuid::nil(), uuid::Uuid::nil()).await?;
                turn.signal(nix::sys::signal::Signal::SIGQUIT)?;
                tokio::time::timeout(std::time::Duration::from_secs(5), self.wait())
                    .await
                    .map_err(|_| DaemonError::Process("turn shim force stop timed out".into()))??;
                Ok::<_, DaemonError>(())
            }
            .await;
            match stop {
                Ok(()) => {
                    // The shim only signals its own process group; escaped
                    // descendants die with the recorded scope (#2007).
                    turn.kill_recorded_scope();
                    return Ok(());
                }
                Err(error) => {
                    // Shutdown may hand custody off while this bounded stop
                    // awaits identity or exit. Its new owner must not be killed.
                    if turn.is_handed_off() {
                        return Ok(());
                    }
                    tracing::warn!(%error, "Turn shim stop failed; killing the owned child process group");
                    turn.kill_recorded_scope();
                }
            }
        }
        if let Some(child) = self.child.as_mut() {
            crate::process_scope::kill_worker_child(child, &self.stop).await
        } else {
            Err(DaemonError::Process(
                "adopted turn could not be stopped safely".into(),
            ))
        }
    }

    /// Wait for the process to exit.
    pub async fn wait(&mut self) -> Result<std::process::ExitStatus> {
        if let Some(child) = self.child.as_mut() {
            return Ok(self.stop.wait(child).await?);
        }
        loop {
            if let Some(status) = self.try_wait()? {
                return Ok(status);
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    /// OS pid of the provider child, `None` once it was reaped.
    pub fn pid(&self) -> Option<u32> {
        self.child
            .as_ref()
            .and_then(Child::id)
            .or_else(|| self.detached.as_ref().and_then(|t| t.pid()))
    }

    /// Check if the process is still running.
    pub fn try_wait(&mut self) -> Result<Option<std::process::ExitStatus>> {
        if let Some(child) = self.child.as_mut() {
            return Ok(self.stop.try_wait(child)?);
        }
        self.detached
            .as_ref()
            .expect("adopted handle has custody")
            .exit_status()
    }

    pub(crate) fn adopt(turn: turn_spool::DetachedTurn) -> Self {
        Self {
            child: None,
            stop: crate::process_control::WorkerStopHandle::new(None, None),
            detached: Some(turn),
        }
    }
}

impl Drop for CliTurnProcess {
    fn drop(&mut self) {
        self.stop.release();
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
    /// Daemon-installed `rsi-agent-mcp` sibling that exposes the typed
    /// `rsi_control_*` tools to a tokened session (see
    /// [`ClaudeClient::agent_mcp_config`]). `None` for discovery/test clients.
    agent_mcp_path: Option<PathBuf>,
    /// Daemon-installed `rsi-rpc` sibling the #1049 boundary-mail hook runs.
    /// `None` (discovery/test clients, or not installed) installs no hook.
    boundary_mail_hook_path: Option<PathBuf>,
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
///
/// #2768: `ladder` is the model's own ladder from the CLI's `initialize`
/// reply ([`model_discovery::known_effort_ladder`]). When it is known it is
/// the whole truth: a level it omits (`max` on a smaller model) is refused and
/// a level it lists is passed, `ultracode` included. When it is unknown (no
/// probe yet, or the reply did not describe the model) the compiled
/// `low..max` list applies, and `ultracode` is refused because the CLI has not
/// reported it for this model.
fn validated_claude_effort(
    config: &LaunchConfig,
    ladder: Option<&[String]>,
) -> Result<Option<String>> {
    let Some(raw_effort) = config.effort.as_deref() else {
        return Ok(None);
    };
    let model = config.model.as_deref().unwrap_or("the default model");
    if let Some(ladder) = ladder {
        return ladder_effort(raw_effort, model, ladder).map(Some);
    }
    claude_effort_level(Some(raw_effort))
        .map(|effort| Some(effort.to_string()))
        .ok_or_else(|| {
            DaemonError::InvalidParam(format!(
                "unsupported Claude effort '{raw_effort}' for {model} (valid: low, medium, high, \
                 xhigh, max; {CLAUDE_ULTRACODE_EFFORT} only when the Claude CLI reports it for the \
                 model)"
            ))
        })
}

/// The `ultracode` effort level: valid only for a model whose live ladder
/// lists it (#2768).
pub(crate) const CLAUDE_ULTRACODE_EFFORT: &str = "ultracode";

/// #289: the live ladder the store's successor-seat check reads: the last
/// good probe of the Claude binary a launch would use, or `None`.
fn live_effort_ladder_for_store(model: &str) -> Option<Vec<String>> {
    let binary_path = crate::provider_cli::resolve("claude")?;
    model_discovery::known_effort_ladder(&binary_path, Some(model))
}

/// #289: let rsid-store judge a successor seat's Claude effort by the live
/// ladder, as [`validated_claude_effort`] does. Called once at daemon start.
pub fn install_live_effort_ladder_source() {
    crate::store_support::provider_defaults::install_claude_effort_ladder_source(
        live_effort_ladder_for_store,
    );
}

/// `raw_effort` checked against a model's live ladder.
fn ladder_effort(raw_effort: &str, model: &str, ladder: &[String]) -> Result<String> {
    if ladder.iter().any(|level| level == raw_effort) {
        return Ok(raw_effort.to_string());
    }
    if ladder.is_empty() {
        return Err(DaemonError::InvalidParam(format!(
            "Claude model {model} supports no effort selection; omit effort '{raw_effort}'"
        )));
    }
    Err(DaemonError::InvalidParam(format!(
        "unsupported Claude effort '{raw_effort}' for {model} (valid for this model: {})",
        ladder.join(", ")
    )))
}

impl ClaudeClient {
    #[cfg(test)]
    pub(crate) fn with_binary_path_for_test(binary_path: PathBuf) -> Self {
        Self {
            binary_path,
            runtime_config: None,
            agent_mcp_path: None,
            boundary_mail_hook_path: None,
        }
    }

    /// Create a new client, finding the Claude binary in PATH.
    pub fn new(runtime_config: Arc<RuntimeConfig>) -> Result<Self> {
        let binary_path =
            crate::provider_cli::resolve("claude").ok_or(DaemonError::ClaudeBinaryNotFound)?;

        tracing::info!(path = %binary_path.display(), "Found Claude binary");
        Ok(Self {
            binary_path,
            runtime_config: Some(runtime_config),
            agent_mcp_path: daemon_sibling_agent_mcp_path(),
            boundary_mail_hook_path: daemon_sibling_rsi_rpc_path(),
        })
    }

    /// Build a client for model discovery only.
    ///
    /// Discovery starts no model turn (see [`ClaudeClient::discover_models`]):
    /// its one `initialize` probe runs in a neutral directory, so it has no
    /// working directory to isolate and needs no runtime config. Calling [`ClaudeClient::launch`]
    /// on such a client is still safe: a missing runtime config resolves to
    /// [`ClaudeConfigIsolation::Off`], i.e. the pre-feature argv.
    pub fn for_discovery() -> Result<Self> {
        let binary_path =
            crate::provider_cli::resolve("claude").ok_or(DaemonError::ClaudeBinaryNotFound)?;

        Ok(Self {
            binary_path,
            runtime_config: None,
            agent_mcp_path: None,
            boundary_mail_hook_path: None,
        })
    }

    /// The `--mcp-config` payload that gives a tokened session the typed
    /// `rsi_control_*` tools, or `None` when the session carries no authority
    /// token or the gateway binary is not installed.
    ///
    /// Mirrors the Codex path (`CodexClient::build_cmd`): the server is the
    /// daemon-installed sibling (never resolved from PATH, so a mutable
    /// workspace binary cannot become an authority bridge), it is passed as a
    /// session-ephemeral CLI argument (never written to a settings file), and
    /// the token is NOT in the payload. `rsi-agent-mcp` reads
    /// `RSI_SESSION_TOKEN`/`RSI_SOCKET` from the environment it inherits from
    /// the Claude CLI, which [`stamp_execution_environment`] stamped
    /// `[observed]` (claude 2.1.x: a `--mcp-config` stdio server resolved
    /// `rsi_control_status` to the caller's session with no `env` block).
    ///
    /// A missing gateway degrades to `rsi-rpc` over Bash instead of failing
    /// the launch.
    fn agent_mcp_config(&self, config: &LaunchConfig) -> Option<String> {
        config.rsi_session_id?;
        config.rsi_session_token.as_ref()?;
        let Some(path) = self
            .agent_mcp_path
            .as_deref()
            .filter(|path| path.is_absolute() && path.is_file())
            .and_then(Path::to_str)
        else {
            tracing::warn!(
                path = ?self.agent_mcp_path,
                "rsi-agent-mcp sibling missing; Claude session gets no native rsi_control tools"
            );
            return None;
        };
        Some(
            serde_json::json!({
                "mcpServers": {
                    (AGENT_MCP_SERVER_NAME): {
                        "type": "stdio",
                        "command": path,
                        "args": [],
                    }
                }
            })
            .to_string(),
        )
    }

    /// #2766: the lifecycle hooks the operator enabled, read at each launch.
    /// No runtime config (a test client) installs none.
    fn lifecycle_hooks(&self) -> rsi_common::provider_lifecycle_hooks::LifecycleHookSet {
        self.runtime_config
            .as_ref()
            .map(|runtime_config| runtime_config.provider_lifecycle_hooks())
            .unwrap_or_default()
    }

    /// #2767: whether this launch delivers its repository skills explicitly
    /// (`provider_skill_delivery = explicit`). `false` for the default `ambient`
    /// and for no runtime config (a test client): argv unchanged.
    fn skill_delivery_explicit(&self) -> bool {
        self.runtime_config
            .as_ref()
            .is_some_and(|runtime_config| runtime_config.provider_skill_delivery().is_explicit())
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

    /// The operator's subagent governance for this spawn, read fresh so an
    /// `UpdateDaemonConfig` mutation applies to the next launch (#2763).
    fn subagent_caps(&self) -> crate::store_support::provider_settings::SubagentCaps {
        self.runtime_config
            .as_ref()
            .map(|runtime_config| runtime_config.claude_subagent_caps())
            .unwrap_or_default()
    }

    /// Check if the Claude binary is available without creating a client.
    pub fn is_available() -> bool {
        crate::provider_cli::resolve("claude").is_some()
    }

    /// Discover the Claude models the installed CLI offers this account.
    ///
    /// Asks the CLI itself through its stream-json `initialize` control
    /// request (see [`model_discovery`]). That request sends no prompt and
    /// starts no model turn, so discovery stays unbilled; an earlier approach
    /// that resolved aliases by running a real turn was a paid,
    /// un-attributed invocation and must not come back. A successful probe is
    /// cached for [`model_discovery::CACHE_TTL`]; a failed one falls back to
    /// the last good probe and then to the compiled catalog.
    ///
    /// #2768: the catalog also carries each described model's effort ladder,
    /// and a probe refreshes the ladders the launch validates `--effort`
    /// against. This item stays the registered catalog-only probe owner
    /// (`claude_catalog` in the model-control registry).
    pub async fn discover_models(&self) -> Result<model_discovery::ClaudeCatalog> {
        Ok(
            model_discovery::cached_or_probe(&self.binary_path, || self.probe_cli_model_catalog())
                .await,
        )
    }

    /// #2768: an effort outside the compiled list (`ultracode`) is valid only
    /// for a model whose live ladder lists it. A daemon that has not probed
    /// this binary since it started knows no ladder and would refuse such an
    /// effort, so read the ladders once first (one unbilled `initialize`
    /// probe, cached like the picker's). A compiled-list effort, no effort or
    /// an already probed binary costs nothing; a failed probe leaves the
    /// compiled fallback in force.
    pub(crate) async fn ensure_effort_ladders_for(&self, effort: Option<&str>) {
        let Some(effort) = effort else {
            return;
        };
        if claude_effort_level(Some(effort)).is_some() {
            return;
        }
        if model_discovery::has_probed(&self.binary_path) {
            return;
        }
        let _ = self.discover_models().await;
    }

    /// Run one bounded `initialize` probe against the Claude CLI.
    async fn probe_cli_model_catalog(&self) -> Result<model_discovery::ClaudeCatalog> {
        let mut command = Command::new(&self.binary_path);
        command
            .args(model_discovery::PROBE_ARGS)
            // A neutral directory: no project `.claude/` settings, `.mcp.json`
            // or CLAUDE.md from the daemon's cwd shapes the answer.
            .current_dir(std::env::temp_dir())
            .env(
                rsi_common::identity::ENV_PROCESS_OWNERSHIP_NAMESPACE,
                rsi_common::identity::process_ownership_namespace(),
            );
        // Same credential scrub as a session launch (#694 K1): the probe sees
        // the login a session would, and no inherited provider key.
        crate::vault::scrub_credential_env(&mut command);
        let output = capture_bounded_with_stdin(
            command,
            CaptureLimits::catalog(),
            &tokio_util::sync::CancellationToken::new(),
            Some(model_discovery::probe_request()),
        )
        .await
        .map_err(|error| DaemonError::Process(format!("Claude model probe failed: {error}")))?;
        if !output.status.success() {
            return Err(DaemonError::Process(format!(
                "Claude model probe exited with {}",
                output.status
            )));
        }
        model_discovery::parse_initialize_models(&output.stdout)
    }

    /// Launch a new Claude session.
    /// Returns the process handle and event receiver.
    /// This design prevents lock contention in the session manager.
    pub(crate) fn launch(
        &self,
        config: &LaunchConfig,
        execution: CliExecutionCapability,
    ) -> Result<(ClaudeProcess, mpsc::Receiver<StreamEvent>)> {
        self.launch_inner(config, execution, None)
    }

    fn launch_inner(
        &self,
        config: &LaunchConfig,
        execution: CliExecutionCapability,
        shim_override: Option<&Path>,
    ) -> Result<(ClaudeProcess, mpsc::Receiver<StreamEvent>)> {
        let invocation_id = execution.invocation_id();
        let mut cmd = Command::new(&self.binary_path);
        let read_only = config
            .tool_policy
            .as_ref()
            .and_then(|policy| policy.cli_access)
            == Some(rsi_common::harness_tool_policy::CliAccessMode::ReadOnly);
        cmd.args([
            "-p",
            "--output-format",
            "stream-json",
            "--verbose",
            "--permission-mode",
            // #2758: a read-only policy runs `dontAsk` (the tools below are the
            // only ones allowed; the rest are denied, never prompted) instead
            // of today's `bypassPermissions`.
            if read_only {
                rsi_common::harness_tool_policy::CLAUDE_READ_ONLY_PERMISSION_MODE
            } else {
                "bypassPermissions"
            },
            // #1549: print-mode sessions are headless, so Claude Code's own
            // wake/cron tools never fire and a worker that arms one ends its
            // turn and is never resumed. Only RSI's `schedule_wake` and
            // `AgentSubmitJob` are real. `=` form: the flag is variadic.
            CLAUDE_DISALLOWED_TOOLS_ARG,
        ]);
        if read_only {
            cmd.args(claude_read_only_tool_args());
        }

        // RSI settles the entire CLI process group at each print-mode turn.
        // Claude background tasks cannot survive that boundary; restoring their
        // stopped-task notifications can abort the next --resume before a reply.
        cmd.env("CLAUDE_CODE_DISABLE_BACKGROUND_TASKS", "1");

        // #2763: subagent governance. Each variable is set only when the
        // operator chose a value, so the defaults leave the environment exactly
        // as it was (the CLI keeps its own model inheritance and its default
        // of 20 concurrent subagents).
        for (name, value) in claude_subagent_env(&self.subagent_caps()) {
            cmd.env(name, value);
        }

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

        // Native agent-control tools. Under `strict` isolation this is the
        // only MCP server the session connects. The next argument is always an
        // option or the `--` separator, so the CLI's variadic `--mcp-config`
        // takes exactly this payload.
        if let Some(mcp_config) = self.agent_mcp_config(config) {
            cmd.arg("--mcp-config").arg(mcp_config);
        }

        // #1049: deliver mail at tool boundaries. Only sessions rsid launches
        // with its own attribution token get the per-session `--settings`
        // PostToolUse hook; the operator's global settings are never touched.
        let lifecycle = self.lifecycle_hooks();
        let hook_command = config
            .rsi_session_token
            .as_ref()
            .and_then(|_| boundary_mail_hook_command(self.boundary_mail_hook_path.as_deref(), &[]));
        if let Some(command) = hook_command.as_deref() {
            // #2766: plus the operator's lifecycle hooks; an empty set (the
            // default) is exactly the custody and mail hooks.
            cmd.arg("--settings")
                .arg(rsi_common::provider_lifecycle_hooks::claude_settings_json(
                    command, lifecycle,
                ));
        }
        // hub #2919: the session's `SessionStart` hook carries the preamble.
        let session_start_hook = hook_command.is_some()
            && lifecycle
                .contains(rsi_common::provider_lifecycle_hooks::LifecycleHookEvent::SessionStart);

        // #2767: explicit skills delivery. The repository's `.claude/skills`
        // become one content-addressed plugin (`rsi-skills`) named by the launch,
        // so the skill set does not depend on the cwd's own settings, the
        // operator's home or a `--bare` default. Absent under `ambient`.
        //
        // #2923: naming the plugin is not enough, the CLI still loads the user's
        // `~/.claude/skills`, the working directory's `.claude/skills` and every
        // settings-enabled plugin. Explicit delivery therefore also passes an
        // empty `--setting-sources` (after any isolation value, so it wins),
        // which leaves only the CLI's bundled skills and the delivered plugin.
        // A plugin that cannot be built delivers nothing; it never falls back
        // to ambient skills.
        if self.skill_delivery_explicit() {
            cmd.args(rsi_common::provider_skill_delivery::CLAUDE_AMBIENT_SUPPRESS_ARGS);
            if let Some(plugin_dir) =
                crate::skill_delivery::claude_plugin_dir_for_launch(config.working_dir.as_deref())
            {
                cmd.arg("--plugin-dir").arg(plugin_dir);
            }
        }

        // Set working directory
        if let Some(dir) = &config.working_dir {
            cmd.current_dir(dir);
        }

        // Optional model override
        if let Some(model) = &config.model {
            // Some models are only sized correctly (1M) when launched with the
            // `[1m]` variant tag; see `claude_launch_model` (#1038).
            let launch_model = rsi_common::claude_catalog::claude_launch_model(model);
            cmd.args([format!("--model={launch_model}")]);
        }

        // Optional max turns
        if let Some(max) = &config.max_turns {
            cmd.args([format!("--max-turns={max}")]);
        }

        // Optional system prompt. hub #2919: without a `SessionStart` hook
        // this is exactly `config.system_prompt`.
        if let Some(prompt) = claude_launch_system_prompt(config, session_start_hook) {
            cmd.args([format!("--system-prompt={prompt}")]);
        }

        // Resume a previous session
        if let Some(resume_id) = &config.resume_session_id {
            cmd.args([format!("--resume={resume_id}")]);
        }

        // Optional effort level (Claude Code 1.0.33+), validated against the
        // values the CLI actually accepts — see `validated_claude_effort`.
        let ladder =
            model_discovery::known_effort_ladder(&self.binary_path, config.model.as_deref());
        if let Some(effort) = validated_claude_effort(config, ladder.as_deref())? {
            cmd.args([format!("--effort={effort}")]);
        }

        // #2759: a topology node's handoff turn has the harness validate the
        // handoff's shape (`structured_output` on the result event). `=` form,
        // like the other value flags, so the JSON never reads as an option.
        if config.structured_handoff {
            cmd.arg(structured_handoff_flag());
        }

        // #2762: operator-set fallback chain and per-session spend cap. Both
        // default off (no flag, argv unchanged); read per spawn so an
        // `UpdateDaemonConfig` applies to the next session.
        if let Some(runtime_config) = self.runtime_config.as_ref() {
            let chain = claude_fallback_chain_for(
                config.model.as_deref(),
                &runtime_config.claude_fallback_models(),
            );
            if !chain.is_empty() {
                cmd.args([format!("--fallback-model={}", chain.join(","))]);
            }
            if let Some(usd) = runtime_config.claude_max_budget_usd() {
                cmd.args([format!("--max-budget-usd={usd}")]);
            }
        }

        // hub #2765: the operator's allowlisted extra launch options, after
        // every flag RSI sets (validated at write time, so none of them
        // overlaps an RSI flag or widens privilege). Empty adds nothing.
        if let Some(applied) = self.runtime_config.as_deref().and_then(|runtime| {
            crate::provider_launch_overrides::resolve(
                &runtime.provider_launch_overrides(),
                rsi_common::provider_launch_overrides::OverrideProvider::Claude,
            )
        }) {
            cmd.args(&applied.args);
            if let Some(session_id) = config.rsi_session_id {
                crate::provider_launch_overrides::note_applied(session_id, applied);
            }
        }

        apply_bedrock_launch_env(&mut cmd, config, &crate::vault::global())?;
        stamp_execution_environment(&mut cmd, config, invocation_id)?;

        cmd.arg("--").arg(&config.query);

        let detached = if let Some(shim) =
            turn_spool::available_shim(config, self.runtime_config.as_deref(), shim_override)
        {
            let (wrapped, turn) = turn_spool::prepare_command(
                &cmd,
                config,
                invocation_id,
                &shim,
                turn_spool::TurnOutput::Claude,
                None,
            )?;
            cmd = wrapped;
            Some(turn)
        } else {
            cmd = crate::process_scope::ScopedWorkerCommand::wrap_unspawned(&cmd, invocation_id)?;

            cmd.stdout(Stdio::piped());
            cmd.stderr(Stdio::piped());
            configure_tokio_process_group(&mut cmd, ProcessContainment::Group)?;
            None
        };

        let scope = crate::process_scope::command_scope(&cmd);
        let mut child = execution
            .bind_command(RuntimeExecutionRoute::ClaudeCli, cmd)
            .spawn()?;
        let stop = crate::process_control::WorkerStopHandle::new(child.id(), scope);
        if let Some(turn) = detached {
            return Ok(turn.into_process(child));
        }
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| DaemonError::Process("Failed to capture stdout".to_string()))?;
        let stderr = child.stderr.take();

        // Create channel for stream events
        let (event_tx, event_rx) = mpsc::channel(100);

        // Spawn task to read and parse NDJSON from stdout
        let stderr_tx = event_tx.clone();
        let reader_stop = stop.clone();
        tokio::spawn(async move {
            let mut lines = BoundedLines::new(stdout, PROVIDER_MAX_LINE_BYTES);

            loop {
                let next_line = match lines.next_line().await {
                    Err(BoundedLineError::Exceeded { limit }) => {
                        match lines.drain_oversized_line().await {
                            Ok(()) => {
                                if event_tx
                                    .send(crate::provider::stdout_line_truncated_event(limit))
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                                continue;
                            }
                            Err(error) => Err(error),
                        }
                    }
                    other => other,
                };
                let line = match next_line {
                    Ok(Some(line)) => line,
                    Ok(None) => break,
                    Err(error) => {
                        let _ = reader_stop.terminate("provider pipe read failed");
                        let _ = event_tx.try_send(StreamEvent {
                            event_type: "process_error".to_string(),
                            data: serde_json::json!({
                                "error": error.to_string(),
                                "source": "stdout",
                                "terminal": true,
                                "error_class": "provider_output_read_error",
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
            let reader_stop = stop.clone();
            tokio::spawn(async move {
                let mut lines = BoundedLines::new(stderr, PROVIDER_MAX_LINE_BYTES);
                let mut stderr_lines = Vec::new();
                let mut stderr_bytes = 0_usize;

                loop {
                    let line = match lines.next_line().await {
                        Ok(Some(line)) => line,
                        Ok(None) => break,
                        Err(error) => {
                            let _ = reader_stop.terminate("provider pipe read failed");
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
                            let _ = reader_stop.terminate("provider pipe read failed");
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

        Ok((ClaudeProcess::direct(child, stop), event_rx))
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
            completion_gates: None,
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
            tool_policy: None,
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
            structured_handoff: false,
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn bedrock_claude_model_routes_claude_code_to_bedrock_with_vault_key() {
        temp_env::with_vars([("AWS_REGION", Some("us-east-2"))], || {
            let root = TempDir::new().unwrap();
            let vault = crate::vault::VaultHandleBuilder::new(Arc::new(
                crate::vault::VaultSettings::default(),
            ))
            .dir(root.path().join("vault"))
            .env(|_| None)
            .open()
            .unwrap();
            vault
                .set(
                    crate::vault::Slot::Bedrock,
                    "bedrock-api-key-test-claude-0001",
                )
                .unwrap();
            let mut config = launch_config_for_provider_test("bedrock");
            config.model = Some("us.anthropic.claude-sonnet-5-v1:0".to_string());
            let mut cmd = Command::new("true");
            apply_bedrock_launch_env(&mut cmd, &config, &vault).unwrap();
            stamp_execution_environment(&mut cmd, &config, uuid::Uuid::new_v4()).unwrap();
            let env = command_environment(&cmd);
            assert_eq!(
                env.get("CLAUDE_CODE_USE_BEDROCK").map(String::as_str),
                Some("1")
            );
            assert_eq!(env.get("AWS_REGION").map(String::as_str), Some("us-east-2"));
            assert_eq!(
                env.get(crate::bedrock::BEDROCK_ENV).map(String::as_str),
                Some("bedrock-api-key-test-claude-0001")
            );
            crate::vault::env_scrub::tests::assert_only_injected(
                &cmd,
                Some(crate::bedrock::BEDROCK_ENV),
            );
            // Claude Code takes no Codex `-c` shell-exclude argument.
            assert_eq!(cmd.as_std().get_args().count(), 0);

            // A first-party Claude model keeps the ordinary environment.
            let mut direct = launch_config_for_provider_test("direct");
            direct.model = Some("claude-sonnet-5".to_string());
            let mut cmd = Command::new("true");
            apply_bedrock_launch_env(&mut cmd, &direct, &vault).unwrap();
            let env = command_environment(&cmd);
            assert_eq!(env.get("CLAUDE_CODE_USE_BEDROCK"), None::<&String>);
        });
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
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

    #[allow(clippy::unwrap_used, clippy::expect_used)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn claude_cli_environment_scrubs_every_provider_credential() {
        // Claude, AGY and Codex all build their child env through this
        // chokepoint; no slot or generic credential var may be inherited.
        let config = launch_config_for_provider_test("scrub");
        let mut cmd = Command::new("claude");
        stamp_execution_environment(&mut cmd, &config, uuid::Uuid::new_v4()).unwrap();
        crate::vault::env_scrub::tests::assert_only_injected(&cmd, None);
        // Identity stamping is unaffected by the scrub.
        assert!(
            command_environment(&cmd).contains_key(rsi_common::identity::ENV_MODEL_INVOCATION_ID)
        );
    }

    #[allow(clippy::unwrap_used, clippy::expect_used)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn stamped_claude_child_suppresses_daemon_keys_and_keeps_ordinary_env() {
        let mut config = launch_config_for_provider_test("scrub-daemon-keys");
        config.rsi_session_id = Some(uuid::Uuid::new_v4());
        let mut cmd = Command::new("claude");
        stamp_execution_environment(&mut cmd, &config, uuid::Uuid::new_v4()).unwrap();
        crate::vault::env_scrub::tests::assert_child_env_suppresses_keys(&cmd, None);
        assert!(command_environment(&cmd).contains_key(rsi_common::identity::ENV_SESSION_ID));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn agent_transport_bearer_launch_debug_redacts_and_env_stays_compatible() {
        let marker = format!("synthetic-bearer-{}", uuid::Uuid::new_v4());
        let mut config = launch_config_for_provider_test("transport privacy");
        config.rsi_session_token = Some(marker.clone().into());
        let diagnostic = format!("{:?}", Err::<(), _>(Some(config.clone())));
        assert!(
            !diagnostic.contains(&marker),
            "launch error diagnostics exposed a credential"
        );
        assert!(diagnostic.contains("AgentTransportBearer(<redacted>)"));
        let mut cmd = Command::new("provider");
        stamp_execution_environment(&mut cmd, &config, uuid::Uuid::new_v4()).unwrap();
        let env = command_environment(&cmd);
        assert!(
            env.get(rsi_common::identity::ENV_SESSION_TOKEN) == Some(&marker),
            "provider environment changed the transport credential"
        );
        assert!(
            cmd.as_std()
                .get_args()
                .all(|arg| !arg.to_string_lossy().contains(&marker)),
            "provider arguments exposed a credential"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
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
            agent_mcp_path: None,
            boundary_mail_hook_path: None,
        }
    }

    /// Build a client whose daemon-global setting selects `policy`.
    fn test_client_with_isolation(binary_path: PathBuf, policy: &str) -> ClaudeClient {
        let mut config = Config::default();
        config.claude_config_isolation = policy.to_string();
        let runtime = RuntimeConfig::from_config(&config);
        runtime
            .turn_detach_enabled
            .store(false, std::sync::atomic::Ordering::Relaxed);
        ClaudeClient {
            binary_path,
            runtime_config: Some(runtime),
            agent_mcp_path: None,
            boundary_mail_hook_path: None,
        }
    }

    /// Build a client whose daemon-global settings pin the subagent model and
    /// concurrency cap (#2763); `None` leaves the setting at its default.
    fn test_client_with_subagents(
        binary_path: PathBuf,
        model: Option<&str>,
        cap: Option<u64>,
    ) -> ClaudeClient {
        let runtime = RuntimeConfig::from_config(&Config::default());
        runtime
            .turn_detach_enabled
            .store(false, std::sync::atomic::Ordering::Relaxed);
        if let Some(model) = model {
            runtime
                .update_field("claude_subagent_model", &serde_json::json!(model))
                .expect("valid model");
        }
        if let Some(cap) = cap {
            runtime
                .update_field("claude_max_concurrent_subagents", &serde_json::json!(cap))
                .expect("valid cap");
        }
        ClaudeClient {
            binary_path,
            runtime_config: Some(runtime),
            agent_mcp_path: None,
            boundary_mail_hook_path: None,
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
printf '%s' "${CLAUDE_CODE_DISABLE_BACKGROUND_TASKS:-}" > "$(dirname "$0")/claude_background_tasks_env.txt"
printf '%s|%s' "${CLAUDE_CODE_SUBAGENT_MODEL-UNSET}" "${CLAUDE_CODE_MAX_CONCURRENT_SUBAGENTS-UNSET}" > "$(dirname "$0")/claude_subagent_env.txt"
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

    /// [`read_nul_args`] that keeps an empty argument (`--setting-sources ""`).
    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    fn read_nul_args_keeping_empty(path: &Path) -> Vec<String> {
        let bytes = fs::read(path).expect("read captured args");
        let mut args: Vec<String> = bytes
            .split(|b| *b == 0)
            .map(|arg| String::from_utf8(arg.to_vec()).expect("utf8 arg"))
            .collect();
        // Every argument ends in a NUL, so the split leaves one empty tail.
        args.pop();
        args
    }

    #[cfg(unix)]
    fn install_prompt_parsing_claude(dir: &std::path::Path) -> (PathBuf, PathBuf) {
        let binary_path = dir.join("claude-prompt-parser");
        let output_path = dir.join("claude_prompt_output.txt");
        fs::write(
            &binary_path,
            r#"#!/usr/bin/env bash
set -euo pipefail
while [[ "$#" -gt 0 ]]; do
  case "$1" in
    --) shift; break ;;
    -p|--verbose) ;;
    --output-format|--permission-mode)
      [[ "$#" -ge 2 ]] || { echo "missing option value" >&2; exit 2; }
      shift
      ;;
    --*=*) ;;
    -*) echo "unknown option: $1" >&2; exit 2 ;;
  esac
  shift
done
[[ "$#" -eq 1 ]] || { echo "expected exactly one prompt after --" >&2; exit 2; }
# Serialize the stream event rather than hand-escaping prompt text. Passing the
# prompt as argv preserves leading dashes and trailing newlines on BSD and GNU.
python3 - "$1" "$(dirname "$0")/claude_prompt_output.txt" <<'PYTHON'
import json
import pathlib
import sys

prompt = sys.argv[1]
pathlib.Path(sys.argv[2]).write_bytes(prompt.encode("utf-8"))
print(json.dumps({
    "type": "assistant",
    "session_id": "prompt-parser",
    "message": {"role": "assistant", "content": [{"type": "text", "text": prompt}]},
}))
PYTHON
"#,
        )
        .expect("write prompt-parsing fake claude");
        let mut perms = fs::metadata(&binary_path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&binary_path, perms).unwrap();
        (binary_path, output_path)
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

    fn assert_prompt_is_final_verbatim(args: &[String], prompt: &str) {
        assert!(
            args.iter().any(|arg| arg == "-p"),
            "print mode must remain a flag: {args:?}"
        );
        assert!(
            args.iter().any(|arg| arg == "--"),
            "end-of-options separator must be present: {args:?}"
        );
        assert_eq!(
            args.last().map(String::as_str),
            Some(prompt),
            "prompt must be final argv, byte-for-byte: {args:?}"
        );
        assert_eq!(
            args.iter().filter(|arg| arg.as_str() == prompt).count(),
            1,
            "prompt must not appear before its final position: {args:?}"
        );
        assert_eq!(
            args.get(args.len() - 2).map(String::as_str),
            Some("--"),
            "end-of-options separator must immediately precede prompt: {args:?}"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn test_claude_client_availability() {
        // This test documents behavior, doesn't assert (Claude may or may not be installed)
        let available = ClaudeClient::is_available();
        println!("Claude CLI available: {}", available);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn test_stream_event_parsing() {
        let json = r#"{"type":"system","subtype":"init","session_id":"abc123"}"#;
        let event: StreamEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event_type, "system");
        assert_eq!(event.data.get("subtype").unwrap(), "init");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn test_stream_event_with_content() {
        let json = r#"{"type":"assistant","session_id":"abc","message":{"role":"assistant","content":[{"type":"text","text":"Hello"}]}}"#;
        let event: StreamEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event_type, "assistant");
    }

    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
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
        assert_prompt_is_final_verbatim(&args, "worker prompt body");
        assert!(args.contains(&"--system-prompt=assembled worker preamble".to_string()));
        assert!(
            !args
                .iter()
                .any(|arg| arg.contains("<docregblock>") || arg.contains("/spawn_child")),
            "docregblock wrapper leaked into Claude argv: {args:?}"
        );
    }

    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[tokio::test]
    async fn claude_launch_passes_leading_dash_prompts_after_options() {
        let tmp = TempDir::new().expect("tempdir");
        let (binary_path, output_path) = install_prompt_parsing_claude(tmp.path());
        let client = test_client(binary_path);

        for prompt in [
            "---\ndate: test\n---\n# Handoff",
            "-x",
            "--help",
            "--",
            "--quoted \"text\" and \\path\\file\nnext line\n\n",
            "-controls\twith CR\r\nand backspace\u{0008}\u{000c}",
            "-Unicode: café, 日本語, 🦀\n",
            "",
        ] {
            let mut config = launch_config_for_provider_test(prompt);
            config.system_prompt = Some("-leading-dash-free-text".to_string());
            let (mut process, mut rx) = client
                .launch(
                    &config,
                    CliExecutionCapability::for_test(RuntimeExecutionRoute::ClaudeCli),
                )
                .expect("launch prompt-parsing fake claude");
            let status = process
                .wait()
                .await
                .expect("wait prompt-parsing fake claude");
            assert!(status.success(), "prompt {prompt:?} failed: {status}");
            let event = rx.recv().await.expect("assistant event");
            assert_eq!(
                event.data["message"]["content"][0]["text"],
                serde_json::Value::String(prompt.to_string())
            );
            assert_eq!(
                fs::read(&output_path).expect("read captured prompt"),
                prompt.as_bytes(),
                "prompt argv must survive byte-for-byte: {prompt:?}"
            );
        }
    }

    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn test_launch_config() {
        let config = LaunchConfig {
            completion_gates: None,
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
            tool_policy: None,
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
            structured_handoff: false,
        };
        assert_eq!(config.query, "Hello");
        assert_eq!(config.max_turns, Some(5));
    }

    /// F-155: this used to rebuild the argv by hand with a local
    /// `std::process::Command` and assert on its `Debug` string, so it passed
    /// no matter what `launch()` actually emitted. It now runs the real
    /// builder against the fake `claude` binary.
    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
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
        assert_prompt_is_final_verbatim(&args, "test query");
    }

    /// #1549: headless sessions must not be offered Claude Code's native
    /// wake/cron tools, which never fire under `claude -p`.
    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[tokio::test]
    async fn launch_denies_native_wake_and_cron_tools() {
        let tmp = TempDir::new().expect("tempdir");
        let (binary_path, args_path) = install_fake_claude(tmp.path());
        let client = test_client(binary_path);
        let mut config = launch_config_for_provider_test("test query");
        config.resume_session_id = Some("previous-turn".to_string());
        let (mut process, _rx) = client
            .launch(
                &config,
                CliExecutionCapability::for_test(RuntimeExecutionRoute::ClaudeCli),
            )
            .expect("launch fake claude");
        process.wait().await.expect("wait fake claude");

        let args = read_nul_args(&args_path);
        let denied = args
            .iter()
            .find_map(|arg| arg.strip_prefix("--disallowedTools="))
            .unwrap_or_else(|| panic!("no --disallowedTools in {args:?}"));
        for tool in ["ScheduleWakeup", "CronCreate", "CronDelete", "CronList"] {
            assert!(
                denied.split(',').any(|t| t == tool),
                "{tool} not denied: {denied}"
            );
        }
        assert_prompt_is_final_verbatim(&args, "test query");
    }

    /// #1206: fresh and resumed print-mode turns must keep Claude tasks in the
    /// foreground so a stopped background task cannot abort the next prompt.
    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[tokio::test]
    async fn claude_launch_disables_background_tasks_for_fresh_and_resumed_turns() {
        let tmp = TempDir::new().expect("tempdir");
        let (binary_path, args_path) = install_fake_claude(tmp.path());
        let client = test_client(binary_path);

        for resume_session_id in [None, Some("previous-turn".to_string())] {
            let mut config = launch_config_for_provider_test("How is it now?");
            config.resume_session_id = resume_session_id;
            let (mut process, mut rx) = client
                .launch(
                    &config,
                    CliExecutionCapability::for_test(RuntimeExecutionRoute::ClaudeCli),
                )
                .expect("launch fake claude");
            assert!(process.wait().await.expect("wait fake claude").success());
            assert_eq!(
                fs::read_to_string(tmp.path().join("claude_background_tasks_env.txt"))
                    .expect("read background task environment"),
                "1",
                "background tasks must be disabled for resume {:?}",
                config.resume_session_id
            );
            assert_prompt_is_final_verbatim(&read_nul_args(&args_path), "How is it now?");
            let event = rx.recv().await.expect("terminal result");
            assert_eq!(event.event_type, "result");
            assert_eq!(event.data["subtype"], "turn_completed");
        }
    }

    /// #2763: subagent governance reaches the Claude process as environment.
    /// The defaults add nothing (the child sees exactly the environment the
    /// daemon had, and the argv is the historical one); a chosen model and cap
    /// become `CLAUDE_CODE_SUBAGENT_MODEL` and
    /// `CLAUDE_CODE_MAX_CONCURRENT_SUBAGENTS` on fresh and resumed turns.
    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[tokio::test]
    async fn claude_launch_passes_subagent_governance_only_when_set() {
        let ambient = |name: &str| std::env::var(name).unwrap_or_else(|_| "UNSET".to_string());
        let default_env = format!(
            "{}|{}",
            ambient("CLAUDE_CODE_SUBAGENT_MODEL"),
            ambient("CLAUDE_CODE_MAX_CONCURRENT_SUBAGENTS")
        );
        let cases: [(Option<&str>, Option<u64>, String); 4] = [
            (None, None, default_env.clone()),
            (
                Some("haiku"),
                None,
                format!("haiku|{}", ambient("CLAUDE_CODE_MAX_CONCURRENT_SUBAGENTS")),
            ),
            (
                None,
                Some(4),
                format!("{}|4", ambient("CLAUDE_CODE_SUBAGENT_MODEL")),
            ),
            (
                Some("claude-haiku-4-5"),
                Some(2),
                "claude-haiku-4-5|2".to_string(),
            ),
        ];
        let mut baseline_args = None;
        for (model, cap, expected) in cases {
            for resume_session_id in [None, Some("previous-turn".to_string())] {
                let tmp = TempDir::new().expect("tempdir");
                let (binary_path, args_path) = install_fake_claude(tmp.path());
                let client = test_client_with_subagents(binary_path, model, cap);
                let mut config = launch_config_for_provider_test("test query");
                config.resume_session_id = resume_session_id.clone();
                let (mut process, _rx) = client
                    .launch(
                        &config,
                        CliExecutionCapability::for_test(RuntimeExecutionRoute::ClaudeCli),
                    )
                    .expect("launch fake claude");
                assert!(process.wait().await.expect("wait fake claude").success());
                assert_eq!(
                    fs::read_to_string(tmp.path().join("claude_subagent_env.txt"))
                        .expect("read subagent environment"),
                    expected,
                    "model {model:?} cap {cap:?} resume {resume_session_id:?}"
                );
                // Environment only: the argv never changes.
                if resume_session_id.is_none() {
                    let args = read_nul_args(&args_path);
                    assert_eq!(
                        baseline_args.get_or_insert_with(|| args.clone()),
                        &args,
                        "subagent governance must not touch argv"
                    );
                }
            }
        }
    }

    /// #2763: the pure env mapping. Defaults map to nothing.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn claude_subagent_env_maps_only_chosen_values() {
        use crate::store_support::provider_settings::SubagentCaps;
        assert!(claude_subagent_env(&SubagentCaps::default()).is_empty());
        assert!(claude_subagent_env(&SubagentCaps::from_settings("", 0)).is_empty());
        assert_eq!(
            claude_subagent_env(&SubagentCaps::from_settings("haiku", 4)),
            vec![
                ("CLAUDE_CODE_SUBAGENT_MODEL", "haiku".to_string()),
                ("CLAUDE_CODE_MAX_CONCURRENT_SUBAGENTS", "4".to_string()),
            ]
        );
    }

    /// #1049: an rsid-launched session (it carries its own attribution token)
    /// gets the per-session PostToolUse boundary-mail hook; a launch without
    /// one keeps the argv it always had, and the token itself is never in argv.
    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[tokio::test]
    async fn launch_installs_the_boundary_mail_hook_only_for_rsid_launched_sessions() {
        async fn argv(token: Option<&str>) -> Vec<String> {
            let tmp = TempDir::new().expect("tempdir");
            let (binary_path, args_path) = install_fake_claude(tmp.path());
            let mut client = test_client(binary_path);
            let rsi_rpc = tmp.path().join("rsi-rpc");
            std::fs::write(&rsi_rpc, b"").expect("fake rsi-rpc");
            client.boundary_mail_hook_path = Some(rsi_rpc);
            let mut config = launch_config_for_provider_test("q");
            config.rsi_session_token = token.map(Into::into);
            let (mut process, _rx) = client
                .launch(
                    &config,
                    CliExecutionCapability::for_test(RuntimeExecutionRoute::ClaudeCli),
                )
                .expect("launch fake claude");
            process.wait().await.expect("wait fake claude");
            read_nul_args(&args_path)
        }

        let without = argv(None).await;
        assert!(
            !without.iter().any(|arg| arg == "--settings"),
            "{without:?}"
        );

        let with = argv(Some("s3cr3t-token-value")).await;
        let position = with
            .iter()
            .position(|arg| arg == "--settings")
            .expect("--settings present");
        let settings: serde_json::Value =
            serde_json::from_str(&with[position + 1]).expect("settings json");
        let hook = &settings["hooks"]["PostToolUse"][0]["hooks"][0];
        assert_eq!(hook["type"], "command");
        let command = hook["command"].as_str().unwrap();
        // #1183 review: the trusted absolute sibling, single-quoted.
        assert!(command.starts_with("'/"), "{hook}");
        assert!(command.ends_with("/rsi-rpc' boundary-mail-hook"), "{hook}");
        // #1097: the same hook also routes heavy Bash commands through the
        // spill wrapper at PreToolUse.
        let pre = &settings["hooks"]["PreToolUse"][0];
        assert_eq!(pre["matcher"], "Bash");
        assert!(
            pre["hooks"][0]["command"]
                .as_str()
                .unwrap()
                .ends_with("boundary-mail-hook"),
            "{pre}"
        );
        // Hub #1161: and answers a headless AskUserQuestion with the typed
        // undeliverable notice at PreToolUse.
        let ask = &settings["hooks"]["PreToolUse"][1];
        assert_eq!(ask["matcher"], "AskUserQuestion");
        assert_eq!(ask["hooks"][0]["command"], hook["command"]);
        assert!(
            !with.iter().any(|arg| arg.contains("s3cr3t-token-value")),
            "the session token must never be in argv"
        );
        assert_prompt_is_final_verbatim(&with, "q");
    }

    /// #2766: the default (empty) lifecycle set leaves the `--settings`
    /// byte-identical to the custody and mail hooks; enabled events add their
    /// groups running the same command.
    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[tokio::test]
    async fn launch_installs_exactly_the_enabled_lifecycle_hooks() {
        async fn settings(lifecycle: Option<&str>) -> (String, String) {
            let tmp = TempDir::new().expect("tempdir");
            let (binary_path, args_path) = install_fake_claude(tmp.path());
            let mut client = test_client_with_isolation(binary_path, "off");
            if let Some(value) = lifecycle {
                client
                    .runtime_config
                    .as_ref()
                    .expect("runtime config")
                    .update_field("provider_lifecycle_hooks", &serde_json::json!(value))
                    .expect("valid lifecycle set");
            }
            let rsi_rpc = tmp.path().join("rsi-rpc");
            std::fs::write(&rsi_rpc, b"").expect("fake rsi-rpc");
            client.boundary_mail_hook_path = Some(rsi_rpc.clone());
            let mut config = launch_config_for_provider_test("q");
            config.rsi_session_token = Some("token".into());
            let (mut process, _rx) = client
                .launch(
                    &config,
                    CliExecutionCapability::for_test(RuntimeExecutionRoute::ClaudeCli),
                )
                .expect("launch fake claude");
            process.wait().await.expect("wait fake claude");
            let args = read_nul_args(&args_path);
            let position = args
                .iter()
                .position(|arg| arg == "--settings")
                .expect("--settings present");
            let command = boundary_mail_hook_command(Some(&rsi_rpc), &[]).expect("hook command");
            (args[position + 1].clone(), command)
        }

        let (default, command) = settings(None).await;
        assert_eq!(
            default,
            rsi_common::boundary_mail_hook::claude_settings_json(&command),
            "empty set: byte-identical to the custody and mail hooks"
        );
        let (none, command) = settings(Some("none")).await;
        assert_eq!(
            none,
            rsi_common::boundary_mail_hook::claude_settings_json(&command)
        );

        let (both, command) = settings(Some("StopFailure,PreCompact")).await;
        let value: serde_json::Value = serde_json::from_str(&both).expect("settings json");
        for event in ["StopFailure", "PreCompact"] {
            let handler = &value["hooks"][event][0]["hooks"][0];
            assert_eq!(handler["command"], command.as_str(), "{event}");
            assert!(handler.get("onFailure").is_none(), "{event}");
        }
        assert_eq!(value["hooks"]["PreToolUse"][0]["matcher"], "Bash");
        assert_eq!(value["hooks"]["PostToolUse"][0]["matcher"], "*");
    }

    /// hub #2919: one fake-Claude launch of a tokened session in a project
    /// that ships a router and a worker preamble; returns its argv and the
    /// startup preamble a fresh launch composes for that project.
    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    async fn session_start_launch(
        lifecycle: Option<&str>,
        sibling: bool,
        session_id: uuid::Uuid,
        system_prompt: impl FnOnce(&str) -> Option<String>,
    ) -> (Vec<String>, String) {
        let tmp = TempDir::new().expect("tempdir");
        let (binary_path, args_path) = install_fake_claude(tmp.path());
        let mut client = test_client_with_isolation(binary_path, "off");
        if let Some(value) = lifecycle {
            client
                .runtime_config
                .as_ref()
                .expect("runtime config")
                .update_field("provider_lifecycle_hooks", &serde_json::json!(value))
                .expect("valid lifecycle set");
        }
        let rsi_rpc = tmp.path().join("rsi-rpc");
        std::fs::write(&rsi_rpc, b"").expect("fake rsi-rpc");
        client.boundary_mail_hook_path = sibling.then_some(rsi_rpc);
        let project = tmp.path().join("project");
        let shared = project.join(".claude/commands/_shared");
        std::fs::create_dir_all(&shared).expect("commands dir");
        std::fs::create_dir_all(project.join(".git")).expect("git marker");
        std::fs::write(
            project.join(".claude/commands/orchestration_router.md"),
            "ROUTER",
        )
        .expect("router");
        std::fs::write(shared.join("worker_preamble.md"), "WORKER PREAMBLE").expect("preamble");
        let preamble = crate::session::preamble::startup_parts(
            rsi_common::types::SessionKind::Standard,
            &project,
        )
        .join("\n\n");
        let mut config = launch_config_for_provider_test("q");
        config.rsi_session_token = Some("token".into());
        config.rsi_session_id = Some(session_id);
        config.working_dir = Some(project);
        config.system_prompt = system_prompt(&preamble);
        let (mut process, _rx) = client
            .launch(
                &config,
                CliExecutionCapability::for_test(RuntimeExecutionRoute::ClaudeCli),
            )
            .expect("launch fake claude");
        process.wait().await.expect("wait fake claude");
        (read_nul_args(&args_path), preamble)
    }

    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    fn system_prompt_arg(args: &[String]) -> Option<String> {
        args.iter()
            .find_map(|arg| arg.strip_prefix("--system-prompt="))
            .map(str::to_string)
    }

    /// hub #2919: with `SessionStart` on, the fresh launch's system prompt no
    /// longer starts with the preamble, the `SessionStart` hook is installed,
    /// and the hook's daemon claim returns exactly the preamble left out.
    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[tokio::test]
    async fn a_session_start_launch_moves_the_preamble_from_the_system_prompt_to_the_hook() {
        let session = uuid::Uuid::new_v4();
        let rest = "## RSI session identity\nYou are running as provider `Claude`.";
        let (args, preamble) = session_start_launch(Some("SessionStart"), true, session, |p| {
            Some(format!("{p}\n\n{rest}"))
        })
        .await;
        assert!(
            preamble.starts_with("ROUTER\n\nWORKER PREAMBLE"),
            "{preamble}"
        );
        assert_eq!(system_prompt_arg(&args).as_deref(), Some(rest));
        let position = args.iter().position(|arg| arg == "--settings").unwrap();
        let settings: serde_json::Value = serde_json::from_str(&args[position + 1]).unwrap();
        let handler = &settings["hooks"]["SessionStart"][0]["hooks"][0];
        assert!(
            handler["command"]
                .as_str()
                .unwrap()
                .ends_with("boundary-mail-hook"),
            "{settings}"
        );
        assert_eq!(
            crate::session_start_preamble::claim_for_hook(session, "startup"),
            Some(preamble)
        );
        assert_prompt_is_final_verbatim(&args, "q");
        crate::session_start_preamble::forget(session);
    }

    /// hub #2919: a preamble the hook never claimed goes back in front of the
    /// next turn's system prompt, once.
    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[tokio::test]
    async fn a_missed_claude_session_start_hook_falls_back_to_the_system_prompt() {
        let session = uuid::Uuid::new_v4();
        let (first, preamble) =
            session_start_launch(Some("SessionStart"), true, session, |p| Some(p.to_string()))
                .await;
        assert_eq!(
            system_prompt_arg(&first),
            None,
            "the whole prompt was the preamble"
        );
        let (resumed, _) = session_start_launch(Some("SessionStart"), true, session, |_| {
            Some("CONTEXT".to_string())
        })
        .await;
        assert_eq!(
            system_prompt_arg(&resumed),
            Some(format!("{preamble}\n\nCONTEXT"))
        );
        assert_eq!(
            crate::session_start_preamble::claim_for_hook(session, "resume"),
            None
        );
        let (again, _) = session_start_launch(Some("SessionStart"), true, session, |_| {
            Some("CONTEXT".to_string())
        })
        .await;
        assert_eq!(system_prompt_arg(&again).as_deref(), Some("CONTEXT"));
    }

    /// hub #2919: the default `none` (and `SessionStart` with no hook able to
    /// run) passes the composed system prompt byte-for-byte and installs no
    /// `SessionStart` hook.
    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[tokio::test]
    async fn without_a_session_start_hook_the_system_prompt_is_unchanged() {
        for (lifecycle, sibling) in [
            (None, true),
            (Some("none"), true),
            (Some("StopFailure,PreCompact"), true),
            (Some("SessionStart"), false),
        ] {
            let session = uuid::Uuid::new_v4();
            let mut composed = String::new();
            let (args, _) = session_start_launch(lifecycle, sibling, session, |p| {
                composed = format!("{p}\n\nREST");
                Some(composed.clone())
            })
            .await;
            assert_eq!(system_prompt_arg(&args), Some(composed), "{lifecycle:?}");
            assert!(
                args.iter().all(|arg| !arg.contains("\"SessionStart\"")),
                "{lifecycle:?}: {args:?}"
            );
            assert_eq!(
                crate::session_start_preamble::claim_for_hook(session, "startup"),
                None
            );
        }
    }

    /// #1183 review: a tokened launch with no trusted absolute `rsi-rpc`
    /// (not installed, relative, or missing) gets NO hook rather than a
    /// `PATH`-resolved one; mail then waits for turn end.
    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[tokio::test]
    async fn launch_without_a_trusted_rsi_rpc_installs_no_boundary_mail_hook() {
        for path in [
            None,
            Some(PathBuf::from("rsi-rpc")),
            Some(PathBuf::from("/nonexistent/rsi-rpc")),
        ] {
            let tmp = TempDir::new().expect("tempdir");
            let (binary_path, args_path) = install_fake_claude(tmp.path());
            let mut client = test_client(binary_path);
            client.boundary_mail_hook_path = path.clone();
            let mut config = launch_config_for_provider_test("q");
            config.rsi_session_token = Some("tok".into());
            let (mut process, _rx) = client
                .launch(
                    &config,
                    CliExecutionCapability::for_test(RuntimeExecutionRoute::ClaudeCli),
                )
                .expect("launch fake claude");
            process.wait().await.expect("wait fake claude");
            let argv = read_nul_args(&args_path);
            assert!(
                !argv.iter().any(|arg| arg.contains("boundary-mail-hook")),
                "{path:?}: {argv:?}"
            );
        }
        assert_eq!(boundary_mail_hook_command(None, &[]), None);
    }

    /// #2758: a read-only policy swaps `bypassPermissions` for `dontAsk` with an
    /// explicit read-only tool allowlist, keeps the `rsi-agent` MCP server and
    /// its allow rule and the custody/mail `--settings` hooks, and a launch with
    /// no policy keeps the historical permission flags.
    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[tokio::test]
    async fn a_read_only_policy_maps_to_dont_ask_with_a_read_only_tool_allowlist() {
        use rsi_common::harness_tool_policy::{CliAccessMode, HarnessToolPolicy};
        async fn argv(dir: &Path, policy: Option<HarnessToolPolicy>) -> Vec<String> {
            let (binary_path, args_path) = install_fake_claude(dir);
            let mut client = test_client(binary_path);
            let rsi_rpc = dir.join("rsi-rpc");
            std::fs::write(&rsi_rpc, b"").expect("fake rsi-rpc");
            let mcp = dir.join("rsi-agent-mcp");
            std::fs::write(&mcp, b"").expect("fake rsi-agent-mcp");
            client.boundary_mail_hook_path = Some(rsi_rpc);
            client.agent_mcp_path = Some(mcp);
            let mut config = launch_config_for_provider_test("q");
            config.rsi_session_id = Some(uuid::Uuid::new_v4());
            config.rsi_session_token = Some("tok".into());
            config.tool_policy = policy;
            let (mut process, _rx) = client
                .launch(
                    &config,
                    CliExecutionCapability::for_test(RuntimeExecutionRoute::ClaudeCli),
                )
                .expect("launch fake claude");
            process.wait().await.expect("wait fake claude");
            read_nul_args(&args_path)
        }

        let tmp = TempDir::new().expect("tempdir");
        let plain = argv(tmp.path(), None).await;
        assert_arg_pair(&plain, "--permission-mode", "bypassPermissions");
        assert!(
            !plain
                .iter()
                .any(|arg| arg.starts_with("--tools") || arg.starts_with("--allowedTools")),
            "{plain:?}"
        );
        // A policy that sets no CLI access is also today's argv.
        assert_eq!(
            argv(tmp.path(), Some(HarnessToolPolicy::default())).await,
            plain
        );

        let read_only = argv(
            tmp.path(),
            Some(HarnessToolPolicy {
                cli_access: Some(CliAccessMode::ReadOnly),
                ..HarnessToolPolicy::default()
            }),
        )
        .await;
        assert_arg_pair(&read_only, "--permission-mode", "dontAsk");
        assert!(
            !read_only.iter().any(|arg| arg == "bypassPermissions"),
            "{read_only:?}"
        );
        assert!(
            read_only.iter().any(|arg| arg == "--tools=Read,Grep,Glob"),
            "{read_only:?}"
        );
        assert!(
            read_only
                .iter()
                .any(|arg| arg == "--allowedTools=Read,Grep,Glob,mcp__rsi-agent"),
            "{read_only:?}"
        );
        assert!(
            !read_only.iter().any(|arg| arg.contains("Bash")
                && (arg.starts_with("--tools") || arg.starts_with("--allowedTools"))),
            "a read-only session has no shell: {read_only:?}"
        );
        // The wake/cron denial, the rsi-agent MCP server and both hooks stay.
        assert!(
            read_only
                .iter()
                .any(|arg| arg == CLAUDE_DISALLOWED_TOOLS_ARG)
        );
        let mcp_at = read_only
            .iter()
            .position(|arg| arg == "--mcp-config")
            .expect("rsi-agent MCP config kept");
        assert!(read_only[mcp_at + 1].contains(AGENT_MCP_SERVER_NAME));
        let settings_at = read_only
            .iter()
            .position(|arg| arg == "--settings")
            .expect("hook settings kept");
        let settings: serde_json::Value =
            serde_json::from_str(&read_only[settings_at + 1]).expect("settings json");
        assert_eq!(settings["hooks"]["PreToolUse"][0]["matcher"], "Bash");
        assert!(settings["hooks"]["PostToolUse"][0]["hooks"][0]["command"].is_string());
        assert_prompt_is_final_verbatim(&read_only, "q");
        // Everything outside the permission flags is unchanged.
        let strip = |args: &[String]| -> Vec<String> {
            args.iter()
                .filter(|arg| {
                    !arg.starts_with("--tools")
                        && !arg.starts_with("--allowedTools")
                        && *arg != "dontAsk"
                        && *arg != "bypassPermissions"
                })
                .cloned()
                .collect()
        };
        assert_eq!(strip(&read_only), strip(&plain));
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
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[tokio::test]
    async fn default_launch_argv_is_unchanged_by_config_isolation() {
        // The exact argv `launch()` built before this feature, for a config
        // whose optional fields are all `None`.
        let historical = vec![
            "-p".to_string(),
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--verbose".to_string(),
            "--permission-mode".to_string(),
            "bypassPermissions".to_string(),
            "--disallowedTools=ScheduleWakeup,CronCreate,CronDelete,CronList".to_string(),
            "--".to_string(),
            "test query".to_string(),
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
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
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
            assert_prompt_is_final_verbatim(args, "test query");
        }
    }

    /// A tokened session gets the daemon's `rsi-agent-mcp` gateway as its
    /// typed `rsi_control_*` tool surface; the token itself stays in the
    /// environment and never reaches argv.
    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[tokio::test]
    async fn tokened_launch_connects_agent_mcp_gateway_without_token_in_argv() {
        let tmp = TempDir::new().expect("tempdir");
        let (binary_path, args_path) = install_fake_claude(tmp.path());
        let gateway = tmp.path().join("rsi-agent-mcp");
        fs::write(&gateway, "#!/bin/sh\n").expect("write fake gateway");
        let mut client = test_client_with_isolation(binary_path, "strict");
        client.agent_mcp_path = Some(gateway.clone());

        let mut config = launch_config_for_provider_test("test query");
        config.rsi_session_id = Some(uuid::Uuid::new_v4());
        config.rsi_session_token = Some("tok-secret-sentinel".into());
        let (mut process, _rx) = client
            .launch(
                &config,
                CliExecutionCapability::for_test(RuntimeExecutionRoute::ClaudeCli),
            )
            .expect("launch fake claude");
        assert!(process.wait().await.expect("wait").success());
        let args = read_nul_args(&args_path);

        let pos = args
            .iter()
            .position(|arg| arg == "--mcp-config")
            .unwrap_or_else(|| panic!("missing --mcp-config: {args:?}"));
        let payload: serde_json::Value =
            serde_json::from_str(&args[pos + 1]).expect("mcp config is JSON");
        let server = &payload["mcpServers"][AGENT_MCP_SERVER_NAME];
        assert_eq!(server["type"], "stdio");
        assert_eq!(server["command"], gateway.to_str().expect("utf8 path"));
        assert!(args.iter().any(|arg| arg == "--strict-mcp-config"));
        assert!(
            args.iter().all(|arg| !arg.contains("tok-secret-sentinel")),
            "token must stay transport-only: {args:?}"
        );
        assert_prompt_is_final_verbatim(&args, "test query");

        // Without a token the gateway could authorize nothing: the argv keeps
        // its historical shape.
        let untokened_dir = tmp.path().join("untokened");
        fs::create_dir(&untokened_dir).expect("create untokened dir");
        let (untokened_binary, untokened_args) = install_fake_claude(&untokened_dir);
        let mut client = test_client(untokened_binary);
        client.agent_mcp_path = Some(gateway);
        let config = launch_config_for_provider_test("test query");
        let (mut process, _rx) = client
            .launch(
                &config,
                CliExecutionCapability::for_test(RuntimeExecutionRoute::ClaudeCli),
            )
            .expect("launch fake claude");
        assert!(process.wait().await.expect("wait").success());
        assert_eq!(
            read_nul_args(&untokened_args).first().map(String::as_str),
            Some("-p")
        );
        assert_eq!(
            read_nul_args(&untokened_args).len(),
            9,
            "untokened argv must be the historical eight arguments plus the #1549 tool denial"
        );
    }

    /// The setting is validated on write, and the default is `off`.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
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
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
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
        assert!(args.contains(&"--resume=abc-123-session".to_string()));
        assert_prompt_is_final_verbatim(&args, "follow up");
    }

    /// #1038: a model the CLI does not recognize under its bare id is launched
    /// with the `[1m]` variant tag so the session really has a 1M window.
    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[tokio::test]
    async fn launch_tags_unrecognized_one_million_model_with_variant() {
        let tmp = TempDir::new().expect("tempdir");
        let (binary_path, args_path) = install_fake_claude(tmp.path());
        let client = test_client(binary_path);

        let mut config = launch_config_for_provider_test("window probe");
        config.model = Some("claude-sonnet-5-5".to_string());

        let (mut process, _rx) = client
            .launch(
                &config,
                CliExecutionCapability::for_test(RuntimeExecutionRoute::ClaudeCli),
            )
            .expect("launch fake claude");
        let status = process.wait().await.expect("wait fake claude");
        assert!(status.success(), "fake claude failed: {status}");

        let args = read_nul_args(&args_path);
        assert!(args.contains(&"--model=claude-sonnet-5-5[1m]".to_string()));
    }

    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[tokio::test]
    async fn managed_turn_detach_toggle_keeps_direct_pipe_argv_and_events() {
        let dir = TempDir::new().unwrap();
        let (binary, args) = install_fake_claude(dir.path());
        let mut client = test_client(binary);
        let runtime = RuntimeConfig::from_config(&Config::default());
        runtime
            .update_field("turn_detach_enabled", &serde_json::json!(false))
            .unwrap();
        client.runtime_config = Some(runtime);
        let mut config = launch_config_for_provider_test("literal prompt");
        config.rsi_session_id = Some(uuid::Uuid::new_v4());
        let (mut process, mut rx) = client
            .launch(
                &config,
                CliExecutionCapability::for_test(RuntimeExecutionRoute::ClaudeCli),
            )
            .unwrap();
        assert!(process.detached.is_none());
        assert_eq!(rx.recv().await.unwrap().event_type, "result");
        assert!(process.wait().await.unwrap().success());
        let expected = read_nul_args(&args);
        client.runtime_config = None;
        let (mut process, mut rx) = client
            .launch(
                &config,
                CliExecutionCapability::for_test(RuntimeExecutionRoute::ClaudeCli),
            )
            .unwrap();
        assert_eq!(rx.recv().await.unwrap().event_type, "result");
        assert!(process.wait().await.unwrap().success());
        assert_eq!(read_nul_args(&args), expected);
    }

    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[tokio::test]
    async fn managed_turn_missing_or_non_executable_shim_falls_back_to_direct_pipes() {
        #[derive(Clone, Default)]
        struct LogBuffer(Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for LogBuffer {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let dir = TempDir::new().unwrap();
        let (binary, args) = install_fake_claude(dir.path());
        let mut client = test_client(binary);
        let mut config = launch_config_for_provider_test("literal $HOME and `prompt`");
        config.rsi_session_id = Some(uuid::Uuid::new_v4());
        config.rsi_socket = Some(dir.path().join("daemon.sock"));
        let (mut direct, mut direct_rx) = client
            .launch(
                &config,
                CliExecutionCapability::for_test(RuntimeExecutionRoute::ClaudeCli),
            )
            .unwrap();
        let expected_event = direct_rx.recv().await.unwrap();
        assert!(direct.wait().await.unwrap().success());
        let expected_args = read_nul_args(&args);

        let runtime = RuntimeConfig::from_config(&Config::default());
        runtime
            .update_field("turn_detach_enabled", &serde_json::json!(true))
            .unwrap();
        client.runtime_config = Some(runtime);
        let missing = dir.path().join("missing-shim");
        let non_executable = dir.path().join("non-executable-shim");
        fs::write(&non_executable, "#!/bin/sh\nexit 99\n").unwrap();
        fs::set_permissions(&non_executable, fs::Permissions::from_mode(0o644)).unwrap();

        for shim in [&missing, &non_executable] {
            let logs = LogBuffer::default();
            let writer = logs.clone();
            let subscriber = tracing_subscriber::fmt()
                .without_time()
                .with_ansi(false)
                .with_writer(move || writer.clone())
                .finish();
            let (mut process, mut rx) = tracing::subscriber::with_default(subscriber, || {
                client.launch_inner(
                    &config,
                    CliExecutionCapability::for_test(RuntimeExecutionRoute::ClaudeCli),
                    Some(shim),
                )
            })
            .unwrap();
            assert!(process.detached.is_none());
            let event = rx.recv().await.unwrap();
            assert_eq!(event.event_type, expected_event.event_type);
            assert_eq!(event.data, expected_event.data);
            assert!(process.wait().await.unwrap().success());
            assert_eq!(read_nul_args(&args), expected_args);
            let captured = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
            assert!(captured.contains("rsi-turn-shim is missing or not executable; falling back to direct Claude pipes"), "{captured}");
        }
    }

    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[tokio::test]
    async fn managed_turn_detach_launches_shim_with_literal_provider_argv_and_spools() {
        let dir = TempDir::new().unwrap();
        let (binary, args) = install_fake_claude(dir.path());
        let shim = dir.path().join("shim-fixture");
        fs::write(&shim, r#"#!/usr/bin/env python3
import fcntl, json, os, subprocess, sys
folder = sys.argv[2]
with open(folder + '/alive.lock', 'w') as lock:
    fcntl.flock(lock, fcntl.LOCK_EX)
    with open(folder + '/.shim.json.tmp', 'w') as identity:
        json.dump({'pid': os.getpid()}, identity)
    os.replace(folder + '/.shim.json.tmp', folder + '/shim.json')
    with open(folder + '/stdout', 'ab') as out, open(folder + '/stderr', 'ab') as err:
        status = subprocess.run(sys.argv[4:], stdin=subprocess.DEVNULL, stdout=out, stderr=err).returncode
    with open(folder + '/.exit.json.tmp', 'w') as record:
        json.dump({'exit_code': status, 'error': None, 'signal': None}, record)
    os.replace(folder + '/.exit.json.tmp', folder + '/exit.json')
"#).unwrap();
        fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).unwrap();
        let mut client = test_client(binary);
        let runtime = RuntimeConfig::from_config(&Config::default());
        runtime
            .update_field("turn_detach_enabled", &serde_json::json!(true))
            .unwrap();
        client.runtime_config = Some(runtime);
        let mut config = launch_config_for_provider_test("literal $HOME and `prompt`");
        config.rsi_session_id = Some(uuid::Uuid::new_v4());
        config.rsi_socket = Some(dir.path().join("daemon.sock"));
        let (mut process, mut rx) = client
            .launch_inner(
                &config,
                CliExecutionCapability::for_test(RuntimeExecutionRoute::ClaudeCli),
                Some(&shim),
            )
            .unwrap();
        let turn = process.detached.clone().unwrap();
        let boot = uuid::Uuid::new_v4();
        let row = turn
            .custody(config.rsi_session_id.unwrap(), boot)
            .await
            .unwrap();
        assert_eq!(
            row.spool_dir,
            dir.path().join("turns").join(uuid::Uuid::nil().to_string())
        );
        turn.start(Ok(boot));
        let event = tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.event_type, "result");
        let receipt = turn.next_receipt().unwrap();
        assert_eq!(receipt.expected_offset, 0);
        assert_eq!(
            receipt.next_offset,
            fs::metadata(row.spool_dir.join("stdout")).unwrap().len()
        );
        assert!(process.wait().await.unwrap().success());
        assert_eq!(
            read_nul_args(&args).last().map(String::as_str),
            Some(config.query.as_str())
        );
        assert!(rx.recv().await.is_none());
        assert!(turn.complete.load(std::sync::atomic::Ordering::Acquire));
    }

    /// V-006: a supported effort reaches the CLI verbatim.
    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
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
        assert!(args.contains(&"--effort=xhigh".to_string()));
        assert!(args.contains(&"--model=claude-opus-5".to_string()));
        assert!(args.contains(&"--max-turns=7".to_string()));
    }

    /// #2755: every flag a full Claude launch passes is in
    /// [`CLAUDE_LAUNCH_FLAGS`] (so the contract canary asks the installed CLI
    /// about it), and `--bare` is never passed: RSI authenticates through the
    /// ambient OAuth login and keeps its hooks and CLAUDE.md, which `--bare`
    /// drops.
    /// hub #2765: golden argv. An empty override list leaves the launch argv
    /// exactly as it was; allowlisted Claude overrides are appended as
    /// `--flag=value`, in order, after every RSI flag and before `--`, and the
    /// launch notes what it applied for the session record.
    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[tokio::test]
    async fn operator_launch_overrides_render_golden_argv_and_empty_changes_nothing() {
        let tmp = TempDir::new().expect("tempdir");
        let (binary_path, args_path) = install_fake_claude(tmp.path());
        let client = test_client_with_isolation(binary_path, "off");
        let session_id = uuid::Uuid::new_v4();
        let mut config = launch_config_for_provider_test("q");
        config.rsi_session_id = Some(session_id);
        config.model = Some("claude-opus-5".into());
        config.effort = Some("high".into());
        async fn launch_args(
            client: &ClaudeClient,
            config: &LaunchConfig,
            args_path: &Path,
        ) -> Vec<String> {
            let (mut process, _rx) = client
                .launch(
                    config,
                    CliExecutionCapability::for_test(RuntimeExecutionRoute::ClaudeCli),
                )
                .expect("launch fake claude");
            process.wait().await.expect("wait fake claude");
            read_nul_args(args_path)
        }

        let baseline = launch_args(&client, &config, &args_path).await;
        assert_eq!(
            crate::provider_launch_overrides::take_applied(session_id),
            None,
            "an empty list records nothing"
        );
        let runtime = client.runtime_config.as_deref().expect("runtime config");
        runtime
            .update_field(
                rsi_common::provider_launch_overrides::FIELD,
                &serde_json::json!({
                    "claude": ["--append-system-prompt=Be terse.", "--betas=b1,b2"],
                    "codex": ["model_verbosity=low"],
                }),
            )
            .expect("allowed overrides");
        let with = launch_args(&client, &config, &args_path).await;
        let separator = with.iter().position(|arg| arg == "--").expect("--");
        assert_eq!(
            with[separator - 2..separator],
            ["--append-system-prompt=Be terse.", "--betas=b1,b2"]
        );
        let mut without: Vec<String> = with.clone();
        without.drain(separator - 2..separator);
        assert_eq!(without, baseline, "only the overrides were added");
        let applied =
            crate::provider_launch_overrides::take_applied(session_id).expect("applied note");
        assert_eq!(
            applied.entries,
            vec!["--append-system-prompt=Be terse.", "--betas=b1,b2"]
        );

        runtime
            .update_field(
                rsi_common::provider_launch_overrides::FIELD,
                &serde_json::json!("clear"),
            )
            .expect("clear");
        assert_eq!(launch_args(&client, &config, &args_path).await, baseline);
    }

    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[tokio::test]
    async fn launch_flag_list_covers_every_flag_a_full_launch_passes_and_never_bare() {
        for policy in ["off", "settings", "strict"] {
            let tmp = TempDir::new().expect("tempdir");
            let (binary_path, args_path) = install_fake_claude(tmp.path());
            let mut client = test_client_with_isolation(binary_path, policy);
            let rsi_rpc = tmp.path().join("rsi-rpc");
            std::fs::write(&rsi_rpc, b"").expect("fake rsi-rpc");
            client.boundary_mail_hook_path = Some(rsi_rpc);
            let mcp = tmp.path().join("rsi-agent-mcp");
            std::fs::write(&mcp, b"").expect("fake mcp");
            client.agent_mcp_path = Some(mcp);
            let mut config = launch_config_for_provider_test("q");
            config.rsi_session_token = Some("token-value".into());
            config.rsi_session_id = Some(uuid::Uuid::new_v4());
            config.model = Some("claude-opus-5".into());
            config.max_turns = Some(3);
            config.system_prompt = Some("system".into());
            config.resume_session_id = Some("resume-id".into());
            config.effort = Some("high".into());
            client
                .runtime_config
                .as_ref()
                .expect("runtime config")
                .update_field(
                    "claude_fallback_models",
                    &serde_json::json!("claude-sonnet-5-5"),
                )
                .expect("fallback chain");
            client
                .runtime_config
                .as_ref()
                .expect("runtime config")
                .update_field("claude_max_budget_usd", &serde_json::json!(5))
                .expect("budget cap");
            let (mut process, _rx) = client
                .launch(
                    &config,
                    CliExecutionCapability::for_test(RuntimeExecutionRoute::ClaudeCli),
                )
                .expect("launch fake claude");
            process.wait().await.expect("wait fake claude");
            let args = read_nul_args(&args_path);
            let flags: Vec<&str> = args
                .iter()
                .take_while(|arg| arg.as_str() != "--")
                .filter(|arg| arg.starts_with('-'))
                .map(|arg| arg.split('=').next().unwrap_or(arg))
                .collect();
            for flag in &flags {
                assert!(
                    CLAUDE_LAUNCH_FLAGS.contains(flag),
                    "{policy}: launch passes {flag}, which CLAUDE_LAUNCH_FLAGS does not list: {args:?}"
                );
            }
            for expected in [
                "--max-turns",
                "--mcp-config",
                "--settings",
                "--effort",
                "--fallback-model",
                "--max-budget-usd",
            ] {
                assert!(
                    flags.contains(&expected),
                    "{policy}: {expected} in {args:?}"
                );
            }
            assert!(!flags.contains(&"--bare"), "{policy}: {args:?}");
        }
        // The probe's own argv and the isolation flags are covered too.
        for flag in crate::claude::model_discovery::PROBE_ARGS
            .iter()
            .filter(|arg| arg.starts_with('-'))
        {
            assert!(CLAUDE_LAUNCH_FLAGS.contains(flag), "probe flag {flag}");
            assert_ne!(*flag, "--bare");
        }
        assert!(
            CLAUDE_LAUNCH_FLAGS.contains(
                &CLAUDE_DISALLOWED_TOOLS_ARG
                    .split('=')
                    .next()
                    .expect("flag name")
            )
        );
        for policy in [
            ClaudeConfigIsolation::Off,
            ClaudeConfigIsolation::Settings,
            ClaudeConfigIsolation::Strict,
        ] {
            for flag in policy.cli_flags().iter().filter(|f| f.starts_with('-')) {
                assert!(CLAUDE_LAUNCH_FLAGS.contains(flag), "isolation flag {flag}");
            }
        }
        assert!(!CLAUDE_LAUNCH_FLAGS.contains(&"--bare"));
    }

    /// #2759: the handoff turn of a `structured_handoff` node passes
    /// `--json-schema=<the handoff schema>` before the `--` separator, once;
    /// every other launch passes no such flag.
    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[tokio::test]
    async fn structured_handoff_turn_passes_json_schema_and_no_other_turn_does() {
        for structured in [false, true] {
            let tmp = TempDir::new().expect("tempdir");
            let (binary_path, args_path) = install_fake_claude(tmp.path());
            let client = test_client(binary_path);
            let mut config = launch_config_for_provider_test("repeat your handoff");
            config.resume_session_id = Some("resume-id".into());
            config.structured_handoff = structured;
            let (mut process, _rx) = client
                .launch(
                    &config,
                    CliExecutionCapability::for_test(RuntimeExecutionRoute::ClaudeCli),
                )
                .expect("launch fake claude");
            process.wait().await.expect("wait fake claude");
            let args = read_nul_args(&args_path);
            let separator = args.iter().position(|arg| arg == "--").expect("separator");
            let schema_args: Vec<&String> = args[..separator]
                .iter()
                .filter(|arg| arg.starts_with("--json-schema"))
                .collect();
            if structured {
                assert_eq!(schema_args.len(), 1, "{args:?}");
                let schema: serde_json::Value =
                    serde_json::from_str(&schema_args[0]["--json-schema=".len()..])
                        .expect("schema argument is JSON");
                assert_eq!(
                    schema,
                    rsi_common::structured_handoff::structured_handoff_schema()
                );
                assert_eq!(args[separator + 1], "repeat your handoff");
            } else {
                assert!(schema_args.is_empty(), "{args:?}");
            }
        }
    }

    /// #2762: a non-empty `claude_fallback_models` adds `--fallback-model=a,b`
    /// and a non-zero `claude_max_budget_usd` adds `--max-budget-usd=<n>`; with
    /// both at their defaults the argv carries neither; a fallback equal to the
    /// session's own model is dropped (the CLI refuses it); a Bedrock-routed
    /// model gets no chain.
    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[tokio::test]
    async fn launch_passes_fallback_chain_and_budget_cap_only_when_configured() {
        async fn argv(model: &str, chain: &str, budget: serde_json::Value) -> Vec<String> {
            let tmp = TempDir::new().expect("tempdir");
            let (binary_path, args_path) = install_fake_claude(tmp.path());
            let client = test_client_with_isolation(binary_path, "off");
            let runtime = client.runtime_config.as_ref().expect("runtime config");
            runtime
                .update_field("claude_fallback_models", &serde_json::json!(chain))
                .expect("chain");
            runtime
                .update_field("claude_max_budget_usd", &budget)
                .expect("budget");
            let mut config = launch_config_for_provider_test("q");
            config.model = Some(model.to_string());
            let (mut process, _rx) = client
                .launch(
                    &config,
                    CliExecutionCapability::for_test(RuntimeExecutionRoute::ClaudeCli),
                )
                .expect("launch fake claude");
            process.wait().await.expect("wait fake claude");
            read_nul_args(&args_path)
        }
        let flag = |args: &[String], prefix: &str| {
            args.iter().find(|arg| arg.starts_with(prefix)).cloned()
        };

        let off = argv("claude-opus-5", "", serde_json::json!(0)).await;
        assert_eq!(flag(&off, "--fallback-model"), None, "{off:?}");
        assert_eq!(flag(&off, "--max-budget-usd"), None, "{off:?}");

        let on = argv(
            "claude-opus-5",
            "claude-sonnet-5-5, claude-haiku-4-5",
            serde_json::json!(2.5),
        )
        .await;
        assert_eq!(
            flag(&on, "--fallback-model").as_deref(),
            Some("--fallback-model=claude-sonnet-5-5,claude-haiku-4-5")
        );
        assert_eq!(
            flag(&on, "--max-budget-usd").as_deref(),
            Some("--max-budget-usd=2.5")
        );

        // The session's own model is not a fallback for itself.
        let own = argv(
            "claude-opus-5",
            "claude-opus-5[1m],claude-haiku-4-5",
            serde_json::json!(5),
        )
        .await;
        assert_eq!(
            flag(&own, "--fallback-model").as_deref(),
            Some("--fallback-model=claude-haiku-4-5")
        );
        assert_eq!(
            flag(&own, "--max-budget-usd").as_deref(),
            Some("--max-budget-usd=5")
        );

        // A Bedrock-routed model takes no chain.
        let bedrock = claude_fallback_chain_for(
            Some("us.anthropic.claude-opus-5-5-v1:0"),
            &["claude-haiku-4-5".to_string()],
        );
        assert!(bedrock.is_empty());
    }

    /// #2767 golden argv: `ambient` (the default) passes no `--plugin-dir`;
    /// `explicit` passes exactly one, naming the content-addressed `rsi-skills`
    /// plugin built from the repository's `.claude/skills`; two sandboxes with
    /// the same skills get the same plugin directory; a repository with no
    /// skills passes none.
    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[tokio::test]
    async fn launch_passes_the_skill_plugin_only_with_explicit_delivery() {
        async fn argv(mode: &str, working_dir: &Path) -> Vec<String> {
            let tmp = TempDir::new().expect("tempdir");
            let (binary_path, args_path) = install_fake_claude(tmp.path());
            let client = test_client_with_isolation(binary_path, "off");
            client
                .runtime_config
                .as_ref()
                .expect("runtime config")
                .update_field("provider_skill_delivery", &serde_json::json!(mode))
                .expect("mode");
            let mut config = launch_config_for_provider_test("q");
            config.working_dir = Some(working_dir.to_path_buf());
            let (mut process, _rx) = client
                .launch(
                    &config,
                    CliExecutionCapability::for_test(RuntimeExecutionRoute::ClaudeCli),
                )
                .expect("launch fake claude");
            process.wait().await.expect("wait fake claude");
            read_nul_args_keeping_empty(&args_path)
        }
        let plugin_dirs = |args: &[String]| -> Vec<String> {
            args.windows(2)
                .filter(|pair| pair[0] == "--plugin-dir")
                .map(|pair| pair[1].clone())
                .collect()
        };
        let one = crate::skill_delivery::tests::repo_with_skills(
            rsi_common::provider_skill_delivery::CLAUDE_SKILLS_DIR,
            &["alpha", "beta"],
        );
        let two = crate::skill_delivery::tests::repo_with_skills(
            rsi_common::provider_skill_delivery::CLAUDE_SKILLS_DIR,
            &["beta", "alpha"],
        );

        let ambient = argv("ambient", one.path()).await;
        assert!(plugin_dirs(&ambient).is_empty(), "{ambient:?}");

        let explicit = argv("explicit", one.path()).await;
        let delivered = plugin_dirs(&explicit);
        assert_eq!(delivered.len(), 1, "{explicit:?}");
        let plugin = Path::new(&delivered[0]);
        assert!(plugin.starts_with(crate::skill_delivery::plugin_store_dir()));
        assert!(plugin.join(".claude-plugin/plugin.json").is_file());
        assert!(plugin.join("skills/alpha/SKILL.md").is_file());
        // #2923: explicit delivery also suppresses every ambient source with an
        // empty `--setting-sources`; the plugin flag and that pair are the only
        // differences from the ambient argv, and the ambient argv has neither.
        let has_suppress = |args: &[String]| {
            args.windows(2)
                .any(|pair| pair[0] == "--setting-sources" && pair[1].is_empty())
        };
        assert!(!has_suppress(&ambient), "{ambient:?}");
        assert!(has_suppress(&explicit), "{explicit:?}");
        let mut without = Vec::new();
        let mut index = 0;
        while index < explicit.len() {
            let pair_is_ours = explicit[index] == "--plugin-dir"
                || (explicit[index] == "--setting-sources"
                    && explicit.get(index + 1).is_some_and(String::is_empty));
            if pair_is_ours {
                index += 2;
            } else {
                without.push(explicit[index].clone());
                index += 1;
            }
        }
        assert_eq!(without, ambient);

        // The same skills in another sandbox: the same delivered plugin.
        assert_eq!(plugin_dirs(&argv("explicit", two.path()).await), delivered);

        let bare = crate::skill_delivery::tests::repo_with_skills(
            rsi_common::provider_skill_delivery::CLAUDE_SKILLS_DIR,
            &[],
        );
        let bare_argv = argv("explicit", bare.path()).await;
        assert!(plugin_dirs(&bare_argv).is_empty());
        // Nothing to deliver still means no ambient skills.
        assert!(has_suppress(&bare_argv), "{bare_argv:?}");
    }

    /// V-006: `ultra` is in RSI's vocabulary but not the CLI's. The CLI would
    /// warn and silently run at the DEFAULT effort while RSI kept recording
    /// `ultra`, so the launch is rejected instead — the same treatment the
    /// Codex path already gives an unsupported effort.
    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
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

    /// Seed the ladder index for `binary_path` from a stub `initialize`
    /// reply listing `models`, through the real probe cache.
    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    async fn seed_stub_ladders(binary_path: &Path, models: serde_json::Value) {
        let stdout = serde_json::json!({
            "type": "control_response",
            "response": {"subtype": "success", "request_id": model_discovery::PROBE_REQUEST_ID,
                         "response": {"models": models}}
        })
        .to_string();
        let catalog =
            model_discovery::parse_initialize_models(stdout.as_bytes()).expect("stub parses");
        let seeded = model_discovery::cached_or_probe(binary_path, || async { Ok(catalog) }).await;
        assert!(!seeded.efforts.is_empty(), "stub ladders are cached");
    }

    /// Launch `model` at `effort` against the fake CLI; the argv on success.
    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    async fn launch_with_effort(
        client: &ClaudeClient,
        args_path: &Path,
        model: Option<&str>,
        effort: &str,
    ) -> Result<Vec<String>> {
        let _ = std::fs::remove_file(args_path);
        let mut config = launch_config_for_provider_test("ladder probe");
        config.model = model.map(str::to_string);
        config.effort = Some(effort.to_string());
        let (mut process, _rx) = client.launch(
            &config,
            CliExecutionCapability::for_test(RuntimeExecutionRoute::ClaudeCli),
        )?;
        let status = process.wait().await.expect("wait fake claude");
        assert!(status.success(), "fake claude failed: {status}");
        Ok(read_nul_args(args_path))
    }

    /// #2768: a model whose live ladder stops at `xhigh` refuses `max` (which
    /// the compiled list would pass) and still passes the levels it lists; a
    /// model whose ladder includes `max` passes it.
    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[tokio::test]
    async fn live_ladder_decides_max_per_model() {
        let tmp = TempDir::new().expect("tempdir");
        let (binary_path, args_path) = install_fake_claude(tmp.path());
        seed_stub_ladders(
            &binary_path,
            serde_json::json!([
                {"value":"sonnet","resolvedModel":"claude-sonnet-9","displayName":"Sonnet 9",
                 "supportedEffortLevels":["low","medium","high","xhigh"]},
                {"value":"opus","resolvedModel":"claude-opus-9","displayName":"Opus 9",
                 "supportedEffortLevels":["low","medium","high","xhigh","max"]}
            ]),
        )
        .await;
        let client = test_client(binary_path);

        let refused = launch_with_effort(&client, &args_path, Some("claude-sonnet-9"), "max")
            .await
            .expect_err("max is not on this model's ladder");
        assert!(
            matches!(refused, DaemonError::InvalidParam(ref message)
                if message.contains("'max'") && message.contains("low, medium, high, xhigh")),
            "expected the model's own ladder in the refusal, got: {refused}"
        );
        let args = launch_with_effort(&client, &args_path, Some("claude-sonnet-9"), "xhigh")
            .await
            .expect("xhigh is on the ladder");
        assert!(args.contains(&"--effort=xhigh".to_string()), "{args:?}");
        let args = launch_with_effort(&client, &args_path, Some("claude-opus-9[1m]"), "max")
            .await
            .expect("max is on opus's ladder");
        assert!(args.contains(&"--effort=max".to_string()), "{args:?}");
        let args = launch_with_effort(&client, &args_path, Some("opus"), "max")
            .await
            .expect("the alias finds the ladder of the model it resolves to");
        assert!(args.contains(&"--effort=max".to_string()), "{args:?}");
    }

    /// #2768: `ultracode` reaches the CLI only for a model whose live ladder
    /// lists it, including the default model when no model is named.
    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[tokio::test]
    async fn ultracode_is_accepted_only_when_the_cli_reports_it() {
        let tmp = TempDir::new().expect("tempdir");
        let (binary_path, args_path) = install_fake_claude(tmp.path());
        seed_stub_ladders(
            &binary_path,
            serde_json::json!([
                {"value":"default","resolvedModel":"claude-opus-9","displayName":"Default (recommended)",
                 "supportedEffortLevels":["low","medium","high","xhigh","max","ultracode"]},
                {"value":"sonnet","resolvedModel":"claude-sonnet-9","displayName":"Sonnet 9",
                 "supportedEffortLevels":["low","medium","high","xhigh","max"]}
            ]),
        )
        .await;
        let client = test_client(binary_path);

        let args = launch_with_effort(&client, &args_path, Some("claude-opus-9"), "ultracode")
            .await
            .expect("the CLI reports ultracode for opus");
        assert!(args.contains(&"--effort=ultracode".to_string()), "{args:?}");
        let args = launch_with_effort(&client, &args_path, None, "ultracode")
            .await
            .expect("the default model's ladder lists ultracode");
        assert!(args.contains(&"--effort=ultracode".to_string()), "{args:?}");

        let refused = launch_with_effort(&client, &args_path, Some("claude-sonnet-9"), "ultracode")
            .await
            .expect_err("sonnet's ladder does not list ultracode");
        assert!(
            matches!(refused, DaemonError::InvalidParam(ref message)
                if message.contains("'ultracode'") && message.contains("xhigh, max")),
            "got: {refused}"
        );
    }

    /// #2768: after a restart no probe has run, so the launch path reads the
    /// ladders once before an `ultracode` launch and the CLI's report decides.
    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[tokio::test]
    async fn an_unprobed_binary_is_probed_once_before_an_ultracode_launch() {
        let tmp = TempDir::new().expect("tempdir");
        let binary_path = tmp.path().join("claude");
        let args_path = tmp.path().join("claude_args.bin");
        let reply = serde_json::json!({
            "type": "control_response",
            "response": {"subtype": "success", "request_id": model_discovery::PROBE_REQUEST_ID,
                         "response": {"models": [
                {"value":"default","resolvedModel":"claude-opus-9","displayName":"Default",
                 "supportedEffortLevels":["low","high","ultracode"]}
            ]}}
        });
        fs::write(
            &binary_path,
            format!(
                r#"#!/usr/bin/env bash
d="$(dirname "$0")"
case " $* " in
  *" --disable-slash-commands "*) echo probe >> "$d/probes"; cat > /dev/null; printf '%s\n' '{reply}'; exit 0 ;;
esac
: > "$d/claude_args.bin"
for arg in "$@"; do printf '%s\0' "$arg" >> "$d/claude_args.bin"; done
printf '{{"type":"result","subtype":"turn_completed","usage":{{"input_tokens":0,"output_tokens":0}}}}\n'
"#
            ),
        )
        .expect("write fake claude");
        fs::set_permissions(&binary_path, fs::Permissions::from_mode(0o755)).unwrap();
        let client = test_client(binary_path.clone());
        let probes = || {
            fs::read_to_string(tmp.path().join("probes"))
                .map(|text| text.lines().count())
                .unwrap_or(0)
        };

        client.ensure_effort_ladders_for(Some("high")).await;
        assert_eq!(probes(), 0, "a compiled-list effort needs no probe");
        client.ensure_effort_ladders_for(Some("ultracode")).await;
        assert_eq!(probes(), 1);
        client.ensure_effort_ladders_for(Some("ultracode")).await;
        assert_eq!(probes(), 1, "a probed binary is not probed again");

        let args = launch_with_effort(&client, &args_path, None, "ultracode")
            .await
            .expect("the default model reports ultracode");
        assert!(args.contains(&"--effort=ultracode".to_string()), "{args:?}");
    }

    /// #2768: a model no probe described falls back to the compiled
    /// `low..max` list, which passes `max` and refuses `ultracode`.
    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[tokio::test]
    async fn unknown_model_falls_back_to_the_compiled_effort_list() {
        let tmp = TempDir::new().expect("tempdir");
        let (binary_path, args_path) = install_fake_claude(tmp.path());
        seed_stub_ladders(
            &binary_path,
            serde_json::json!([
                {"value":"opus","resolvedModel":"claude-opus-9","displayName":"Opus 9",
                 "supportedEffortLevels":["low","ultracode"]}
            ]),
        )
        .await;
        let client = test_client(binary_path);

        let args = launch_with_effort(&client, &args_path, Some("claude-fable-9"), "max")
            .await
            .expect("the compiled list passes max for an undescribed model");
        assert!(args.contains(&"--effort=max".to_string()), "{args:?}");
        let refused = launch_with_effort(&client, &args_path, Some("claude-fable-9"), "ultracode")
            .await
            .expect_err("ultracode is not reported for this model");
        assert!(
            matches!(refused, DaemonError::InvalidParam(ref message)
                if message.contains("'ultracode'") && message.contains("low, medium, high, xhigh, max")),
            "got: {refused}"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
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

#[cfg(test)]
mod model_discovery_tests {
    use super::*;

    /// When the CLI cannot be probed, the daemon-side picker list is exactly
    /// the canonical catalog — same models, same display names, same order.
    ///
    /// Half of the divergence guard for V-002/F-124/F-158: the daemon's
    /// fallback and the TUI's `CLAUDE_MODELS` both project from
    /// `rsi_common::claude_catalog`; this test pins the daemon side and
    /// `crates/rsi/src/app/mod.rs::tui_claude_models_are_the_canonical_catalog`
    /// pins the client side. The binary path does not exist, so the probe
    /// fails and the fallback path is what is exercised.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[tokio::test]
    async fn discover_models_returns_the_canonical_catalog() {
        let client = ClaudeClient {
            binary_path: PathBuf::from("/nonexistent/claude"),
            runtime_config: None,
            agent_mcp_path: None,
            boundary_mail_hook_path: None,
        };
        let discovered = client
            .discover_models()
            .await
            .expect("a failed probe falls back instead of erroring")
            .models;

        let expected: Vec<(String, String)> = rsi_common::claude_catalog::CLAUDE_MODEL_MENU
            .iter()
            .map(|(id, name)| (id.to_string(), name.to_string()))
            .collect();
        assert_eq!(discovered, expected);
    }

    /// Live probe against the installed CLI. Ignored by default: it needs a
    /// logged-in `claude` on PATH. Run with
    /// `cargo test -p rsid --lib live_claude_cli_probe -- --ignored --nocapture`.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[tokio::test]
    #[ignore = "needs a logged-in claude CLI on PATH"]
    async fn live_claude_cli_probe_lists_resolved_claude_ids() {
        let client = ClaudeClient::for_discovery().expect("claude on PATH");
        let models = client
            .probe_cli_model_catalog()
            .await
            .expect("live initialize probe succeeds");
        println!("{models:#?}");
        assert!(!models.models.is_empty());
        assert!(
            models
                .models
                .iter()
                .all(|(id, _)| id.starts_with("claude-"))
        );
    }
}
