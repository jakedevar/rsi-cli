//! Per-session Harness tool policy (#792).
//!
//! Operator-only: the policy is set on `LaunchSessionParams.tool_policy` and
//! through the daemon-wide default settings. No agent verb carries or widens
//! it; a spawned child inherits its emitter's policy unchanged.
//!
//! The policy is enforced twice by rsid: when the tool catalog (including
//! provider-hosted web specs) is built, and again when a call executes.

use crate::egress_policy::EgressMode;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const WEB_SEARCH_TOOL: &str = "web_search";
pub const WEB_FETCH_TOOL: &str = "web_fetch";
/// #3177: the closed tool surface of a benchmark session: the three tools that
/// run in the task container, `read_output`, and the plan, context and
/// compaction tools. Nothing else is advertised or runnable.
pub const BENCH_ENABLED_TOOLS: [&str; 7] = [
    "shell",
    "exec_command",
    "write_stdin",
    "read_output",
    "update_plan",
    "context_status",
    "compact",
];
/// #3173: the command tools whose output spill threshold a policy may set. The
/// others either read content the model asked for verbatim or bound themselves.
pub const SPILL_COMMAND_TOOLS: [&str; 4] = ["shell", "exec_command", "write_stdin", "git"];
/// Range of an `output_spill` `max_bytes` (the stub itself is under 2 KiB, and
/// the tool output hard cap is 1 MiB). Out of range is refused, never clamped.
pub const OUTPUT_SPILL_MIN_BYTES: u64 = 2 * 1024;
pub const OUTPUT_SPILL_MAX_BYTES: u64 = 1024 * 1024;
/// Range of an `output_spill` `max_lines`.
pub const OUTPUT_SPILL_MIN_LINES: u32 = 10;
pub const OUTPUT_SPILL_MAX_LINES: u32 = 100_000;
pub const TOOL_POLICY_MAX_NAMES: usize = 128;
pub const TOOL_POLICY_MAX_NAME_BYTES: usize = 64;

/// Substrings (uppercase) that make an environment variable name look like a
/// credential (#3169). Such a name never reaches a `shell` or `exec_command`
/// launch, whatever the policy allowlists.
pub const ENV_CREDENTIAL_MARKERS: [&str; 6] =
    ["KEY", "TOKEN", "SECRET", "PASSWORD", "PASSWD", "CREDENTIAL"];
/// Prefix of the daemon's own variables (`RSI_SESSION_TOKEN`, ...); never passed.
pub const ENV_DAEMON_PREFIX: &str = "RSI_";

/// Whether `name` is an environment variable the Harness must never hand to a
/// command: it looks like a credential or is one of the daemon's own.
#[must_use]
pub fn env_name_is_withheld(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    upper.starts_with(ENV_DAEMON_PREFIX)
        || ENV_CREDENTIAL_MARKERS
            .iter()
            .any(|marker| upper.contains(marker))
}
/// Estimated cost of one provider-hosted web search: providers bill searches
/// at about 10 USD per 1000 calls. Fetches add only token cost, which the
/// model-call ledger already meters.
pub const HOSTED_SEARCH_COST_USD_MICROS: u64 = 10_000;

/// Stable refusal codes, used as the whole error text (launch validation) or
/// the `Error: <code>: ...` prefix of a settled tool-error row.
pub const TOOL_POLICY_INVALID: &str = "tool_policy_invalid";
/// The whole error text is `tool_policy_unsupported_for_provider:<what>`, where
/// `<what>` names the provider and, for a CLI provider, the policy field its CLI
/// cannot express (`claude:web_access`, `antigravity`). A policy is never
/// silently widened to fit a provider (#2758).
pub const TOOL_POLICY_UNSUPPORTED_FOR_PROVIDER: &str = "tool_policy_unsupported_for_provider";
pub const TOOL_POLICY_DENIED: &str = "tool_policy_denied";
pub const TOOL_BUDGET_EXHAUSTED: &str = "tool_budget_exhausted";

/// Providers whose sessions run the Harness loop, the only place a tool policy
/// (stored or daemon default) is enforced.
#[must_use]
pub const fn provider_runs_harness_loop(provider: crate::types::SessionProvider) -> bool {
    matches!(
        provider,
        crate::types::SessionProvider::Harness
            | crate::types::SessionProvider::OpenRouter
            | crate::types::SessionProvider::Bedrock
    )
}

/// Providers whose CLI can enforce the CLI-expressible part of a policy
/// ([`CliAccessMode`]): the Claude Code CLI and the Codex CLI family. The
/// Codex-routed OpenRouter and Bedrock sessions are Harness-loop providers and
/// are not listed.
#[must_use]
pub const fn provider_runs_policy_cli(provider: crate::types::SessionProvider) -> bool {
    matches!(
        provider,
        crate::types::SessionProvider::Claude
            | crate::types::SessionProvider::Codex
            | crate::types::SessionProvider::Pioneer
    )
}

/// Lowercase label of a provider inside a `tool_policy_unsupported_for_provider`
/// `<what>`.
#[must_use]
pub const fn provider_label(provider: crate::types::SessionProvider) -> &'static str {
    use crate::types::SessionProvider as P;
    match provider {
        P::Claude => "claude",
        P::Codex => "codex",
        P::Pioneer => "pioneer",
        P::OpenRouter => "openrouter",
        P::Bedrock => "bedrock",
        P::Local => "local",
        P::Antigravity => "antigravity",
        P::CodexAppServer => "codex_app_server",
        P::Harness => "harness",
    }
}

/// The typed refusal for starting `policy` on `provider`, or `None` when the
/// provider can enforce it. `policy` is `None` for a session that is under the
/// daemon-wide Harness defaults only; no CLI enforces those.
///
/// Harness-loop providers enforce everything except [`CliAccessMode`]; the
/// Claude and Codex CLIs enforce only [`CliAccessMode`]; every other provider
/// enforces nothing.
#[must_use]
pub fn provider_policy_refusal(
    provider: crate::types::SessionProvider,
    policy: Option<&HarnessToolPolicy>,
) -> Option<String> {
    let label = provider_label(provider);
    let what = if provider_runs_harness_loop(provider) {
        policy
            .is_some_and(|policy| policy.cli_access.is_some())
            .then_some("cli_access")
    } else if provider_runs_policy_cli(provider) {
        match policy {
            Some(policy) => policy.cli_unsupported_field(),
            None => Some("daemon_defaults"),
        }
    } else {
        return Some(format!("{TOOL_POLICY_UNSUPPORTED_FOR_PROVIDER}:{label}"));
    }?;
    Some(format!(
        "{TOOL_POLICY_UNSUPPORTED_FOR_PROVIDER}:{label}:{what}"
    ))
}

/// What a CLI provider is allowed to do (#2758). Unset is today's behavior:
/// Claude `bypassPermissions`, Codex the daemon-wide `codex_sandbox_mode`.
/// Operator-only, like the rest of the policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CliAccessMode {
    /// A read-only session: the CLI can read the workspace and call the
    /// `rsi-agent` MCP tools, and nothing else. Codex runs `--sandbox
    /// read-only`; Claude runs `--permission-mode dontAsk` with an explicit
    /// read-only `--tools` allowlist (no shell, no edit tools).
    ReadOnly,
}

/// Claude's `--permission-mode` for a read-only session. `dontAsk` denies every
/// tool the allowlist does not name instead of prompting, which a headless
/// `-p` turn cannot answer.
pub const CLAUDE_READ_ONLY_PERMISSION_MODE: &str = "dontAsk";
/// The built-in Claude tools a read-only session keeps. MCP tools are not part
/// of `--tools`; `mcp__rsi-agent` is added to `--allowedTools` separately.
pub const CLAUDE_READ_ONLY_TOOLS: &[&str] = &["Read", "Grep", "Glob"];

impl CliAccessMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "read_only" => Some(Self::ReadOnly),
            _ => None,
        }
    }

    /// One line for the session transcript: the effective sandbox or
    /// permission mode `provider`'s CLI runs under.
    #[must_use]
    pub fn describe(self, provider: crate::types::SessionProvider) -> String {
        match (self, provider) {
            (Self::ReadOnly, crate::types::SessionProvider::Claude) => format!(
                "Access policy: read-only. Claude runs with --permission-mode \
                 {CLAUDE_READ_ONLY_PERMISSION_MODE} and the built-in tools {}, plus the \
                 rsi-agent MCP tools; there is no shell and no edit tool.",
                CLAUDE_READ_ONLY_TOOLS.join(", ")
            ),
            (Self::ReadOnly, _) => "Access policy: read-only. The Codex sandbox is read-only: \
                 shell commands cannot write the workspace; the rsi-agent MCP tools stay available."
                .to_string(),
        }
    }
}

/// Where the Harness file tools (`read_file`, `write_file`, `file_edit`) may
/// reach (#3172). Operator-only, like the rest of the policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilePathScope {
    /// Contained to the session working directory: an absolute path works only
    /// when it lies inside it. The default, and what every RSI sandbox gets.
    #[default]
    WorkingDir,
    /// Any absolute path of the task environment. For a disposable task
    /// container (a benchmark session) whose filesystem is the whole sandbox;
    /// never set it for a session that runs on the host.
    TaskEnvironment,
}

impl FilePathScope {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::WorkingDir => "working_dir",
            Self::TaskEnvironment => "task_environment",
        }
    }
}

/// Ordered from most to least restrictive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebAccessMode {
    Disabled,
    HostedOnly,
    Enabled,
}

impl WebAccessMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Enabled => "enabled",
            Self::HostedOnly => "hosted_only",
            Self::Disabled => "disabled",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "enabled" => Self::Enabled,
            "hosted_only" => Self::HostedOnly,
            "disabled" => Self::Disabled,
            _ => return None,
        })
    }
}

/// Which vendor backs the native Harness `web_search` tool (#2251). The
/// daemon-wide `harness_web_search_backend` setting; it only picks a backend.
/// Whether the tool is advertised also needs that backend's vault key, or for
/// `searxng` the `harness_searxng_url` setting (#2315).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebSearchBackendChoice {
    /// The first keyed backend whose vault key resolves (Serper, then Exa),
    /// else the keyless SearXNG instance when `harness_searxng_url` is set.
    #[default]
    Auto,
    Serper,
    Exa,
    /// A self-hosted SearXNG instance at `harness_searxng_url`; needs no key
    /// (#2315).
    Searxng,
    /// The native tool is never advertised.
    Disabled,
}

impl WebSearchBackendChoice {
    pub const ALL: [Self; 5] = [
        Self::Auto,
        Self::Serper,
        Self::Exa,
        Self::Searxng,
        Self::Disabled,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Serper => "serper",
            Self::Exa => "exa",
            Self::Searxng => "searxng",
            Self::Disabled => "disabled",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|choice| choice.as_str() == value)
    }
}

/// Per-session caps. `None` is unlimited; `Some(0)` allows none.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolBudgets {
    #[serde(default)]
    pub max_search_calls: Option<u32>,
    #[serde(default)]
    pub max_fetch_calls: Option<u32>,
    /// Total bytes of tool output the session may consume.
    #[serde(default)]
    pub max_result_bytes: Option<u64>,
    /// Estimated web cost in micro-USD (1_000_000 = 1 USD).
    #[serde(default)]
    pub max_web_cost_usd_micros: Option<u64>,
}

impl ToolBudgets {
    #[must_use]
    pub const fn is_unlimited(&self) -> bool {
        self.max_search_calls.is_none()
            && self.max_fetch_calls.is_none()
            && self.max_result_bytes.is_none()
            && self.max_web_cost_usd_micros.is_none()
        // A budget is a ceiling: every field only ever narrows.
    }

    fn or(self, defaults: Self) -> Self {
        Self {
            max_search_calls: self.max_search_calls.or(defaults.max_search_calls),
            max_fetch_calls: self.max_fetch_calls.or(defaults.max_fetch_calls),
            max_result_bytes: self.max_result_bytes.or(defaults.max_result_bytes),
            max_web_cost_usd_micros: self
                .max_web_cost_usd_micros
                .or(defaults.max_web_cost_usd_micros),
        }
    }
}

/// #3173: when a command tool's output is replaced by a spill stub. A `None`
/// field keeps the default (`rsi_common::spill::DEFAULT_MAX_BYTES` /
/// `DEFAULT_MAX_LINES`, or the daemon's `RSI_SPILL_*` environment).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputSpill {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_lines: Option<u32>,
}

impl OutputSpill {
    /// Sets nothing, so the default threshold stays.
    #[must_use]
    pub const fn is_unset(&self) -> bool {
        self.max_bytes.is_none() && self.max_lines.is_none()
    }

    /// # Errors
    /// A message naming the field that is outside its range.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(bytes) = self.max_bytes
            && !(OUTPUT_SPILL_MIN_BYTES..=OUTPUT_SPILL_MAX_BYTES).contains(&bytes)
        {
            return Err(format!(
                "max_bytes {bytes} is outside {OUTPUT_SPILL_MIN_BYTES}..={OUTPUT_SPILL_MAX_BYTES}"
            ));
        }
        if let Some(lines) = self.max_lines
            && !(OUTPUT_SPILL_MIN_LINES..=OUTPUT_SPILL_MAX_LINES).contains(&lines)
        {
            return Err(format!(
                "max_lines {lines} is outside {OUTPUT_SPILL_MIN_LINES}..={OUTPUT_SPILL_MAX_LINES}"
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessToolPolicy {
    /// When set, only these tools (native and hosted) are advertised and run.
    #[serde(default)]
    pub enabled_tools: Option<Vec<String>>,
    /// Tools that are never advertised or run; wins over `enabled_tools`.
    #[serde(default)]
    pub denied_tools: Vec<String>,
    /// `None` inherits the daemon default (enabled when that is unset too).
    #[serde(default)]
    pub web_access: Option<WebAccessMode>,
    /// Network egress mode for network-capable tools and the shell (#774).
    /// `None` inherits the daemon default (`deny_private` when unset too).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub egress: Option<EgressMode>,
    /// Server-side context editing (#1097 slice 4): the Anthropic API clears
    /// old tool results in bulk (`clear_tool_uses_20250919`). `None` inherits
    /// the default, which is on for Claude Harness sessions; it is never
    /// applied to other providers. Nothing is edited client-side.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_editing: Option<bool>,
    #[serde(default)]
    pub budgets: ToolBudgets,
    /// Sandbox / permission mode of a Claude or Codex CLI session (#2758).
    /// The only field a CLI provider can enforce; a Harness-loop provider
    /// refuses it. `None` keeps today's argv.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cli_access: Option<CliAccessMode>,
    /// Reach of the file tools (#3172). `None` is [`FilePathScope::WorkingDir`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_paths: Option<FilePathScope>,
    /// Extra environment variable names a host `shell` / `exec_command` launch
    /// passes through from the daemon's environment, on top of the fixed
    /// allowlist (#3169). Names that look like credentials (`KEY`, `TOKEN`,
    /// `SECRET`, `PASSWORD`) and the daemon's `RSI_*` variables are refused at
    /// validation and again at launch. A container target runs with the
    /// container's own environment, so this does not apply there.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env_passthrough: Vec<String>,
    /// Spill thresholds for the command tools, by tool name (#3173): one of
    /// [`SPILL_COMMAND_TOOLS`]. A tool with no entry keeps the default.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub output_spill: BTreeMap<String, OutputSpill>,
}

impl HarnessToolPolicy {
    /// The effective reach of the file tools.
    #[must_use]
    pub fn file_path_scope(&self) -> FilePathScope {
        self.file_paths.unwrap_or_default()
    }

    /// The first Harness-only field this policy sets, or `None` when a Claude
    /// or Codex CLI can enforce all of it (only `cli_access` is set).
    #[must_use]
    pub fn cli_unsupported_field(&self) -> Option<&'static str> {
        if self.enabled_tools.is_some() {
            Some("enabled_tools")
        } else if !self.denied_tools.is_empty() {
            Some("denied_tools")
        } else if self.web_access.is_some() {
            Some("web_access")
        } else if self.egress.is_some() {
            Some("egress")
        } else if self.context_editing.is_some() {
            Some("context_editing")
        } else if !self.budgets.is_unlimited() {
            Some("budgets")
        } else if self.file_paths.is_some() {
            Some("file_paths")
        } else if !self.env_passthrough.is_empty() {
            Some("env_passthrough")
        } else if !self.output_spill.is_empty() {
            Some("output_spill")
        } else {
            None
        }
    }

    /// # Errors
    /// The stable `tool_policy_invalid` code for an unbounded or malformed set.
    pub fn validate(&self) -> Result<(), &'static str> {
        let valid_set = |names: &[String]| {
            names.len() <= TOOL_POLICY_MAX_NAMES
                && names.iter().all(|name| {
                    !name.is_empty()
                        && name.len() <= TOOL_POLICY_MAX_NAME_BYTES
                        && name
                            .bytes()
                            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
                })
        };
        if !valid_set(&self.denied_tools)
            || !valid_set(self.enabled_tools.as_deref().unwrap_or(&[]))
        {
            return Err(TOOL_POLICY_INVALID);
        }
        let valid_env_name = |name: &String| {
            !name.is_empty()
                && name.len() <= TOOL_POLICY_MAX_NAME_BYTES
                && !name.bytes().next().is_some_and(|b| b.is_ascii_digit())
                && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                && !env_name_is_withheld(name)
        };
        if self.env_passthrough.len() > TOOL_POLICY_MAX_NAMES
            || !self.env_passthrough.iter().all(valid_env_name)
        {
            return Err(TOOL_POLICY_INVALID);
        }
        if self.output_spill.iter().any(|(tool, spill)| {
            !SPILL_COMMAND_TOOLS.contains(&tool.as_str()) || spill.validate().is_err()
        }) {
            return Err(TOOL_POLICY_INVALID);
        }
        Ok(())
    }

    /// The spill threshold this policy sets for `tool`, if any (#3173).
    #[must_use]
    pub fn output_spill_for(&self, tool: &str) -> Option<OutputSpill> {
        self.output_spill
            .get(tool)
            .copied()
            .filter(|spill| !spill.is_unset())
    }

    /// Fill every field this policy leaves unspecified from `defaults`; deny
    /// sets union. Used to layer the daemon defaults under a session policy.
    #[must_use]
    pub fn or_defaults(&self, defaults: &Self) -> Self {
        let mut denied = self.denied_tools.clone();
        for name in &defaults.denied_tools {
            if !denied.contains(name) {
                denied.push(name.clone());
            }
        }
        Self {
            enabled_tools: self
                .enabled_tools
                .clone()
                .or_else(|| defaults.enabled_tools.clone()),
            denied_tools: denied,
            web_access: self.web_access.or(defaults.web_access),
            egress: self.egress.or(defaults.egress),
            context_editing: self.context_editing.or(defaults.context_editing),
            budgets: self.budgets.or(defaults.budgets),
            cli_access: self.cli_access.or(defaults.cli_access),
            file_paths: self.file_paths.or(defaults.file_paths),
            env_passthrough: {
                let mut names = self.env_passthrough.clone();
                for name in &defaults.env_passthrough {
                    if !names.contains(name) {
                        names.push(name.clone());
                    }
                }
                names
            },
            output_spill: {
                let mut spill = defaults.output_spill.clone();
                spill.extend(self.output_spill.iter().map(|(k, v)| (k.clone(), *v)));
                spill
            },
        }
    }

    /// #3177: the bench preset: [`BENCH_ENABLED_TOOLS`] only and no web access.
    /// An allowlist is enforced both when the catalog is built and again at
    /// execution, so an MCP bridge tool, `web_search`, `web_fetch`,
    /// `memory_search`, an `rsi_control_*` tool, `schedule_wake` or a codegraph
    /// tool is neither advertised nor runnable.
    #[must_use]
    pub fn bench() -> Self {
        Self {
            enabled_tools: Some(
                BENCH_ENABLED_TOOLS
                    .iter()
                    .map(|n| (*n).to_string())
                    .collect(),
            ),
            web_access: Some(WebAccessMode::Disabled),
            ..Self::default()
        }
    }

    /// The most restrictive policy: no tools and no web. Used when a stored
    /// policy cannot be read, so a launch never silently drops a restriction.
    #[must_use]
    pub fn fail_closed() -> Self {
        Self {
            enabled_tools: Some(Vec::new()),
            denied_tools: Vec::new(),
            web_access: Some(WebAccessMode::Disabled),
            egress: Some(EgressMode::Offline),
            context_editing: None,
            budgets: ToolBudgets::default(),
            cli_access: None,
            file_paths: None,
            env_passthrough: Vec::new(),
            output_spill: BTreeMap::new(),
        }
    }

    #[must_use]
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// Effective egress mode (`deny_private` when unspecified everywhere).
    #[must_use]
    pub fn egress_mode(&self) -> EgressMode {
        self.egress.unwrap_or_default()
    }

    /// Whether server-side context editing is requested (default on).
    #[must_use]
    pub fn context_editing_enabled(&self) -> bool {
        self.context_editing.unwrap_or(true)
    }

    /// Effective web mode (`enabled` when unspecified everywhere).
    #[must_use]
    pub fn web_mode(&self) -> WebAccessMode {
        self.web_access.unwrap_or(WebAccessMode::Enabled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bench_preset_is_the_closed_container_tool_set_with_web_off() {
        let policy = HarnessToolPolicy::bench();
        assert_eq!(policy.validate(), Ok(()));
        assert_eq!(policy.web_mode(), WebAccessMode::Disabled);
        let enabled = policy.enabled_tools.expect("an allowlist");
        assert_eq!(
            enabled,
            [
                "shell",
                "exec_command",
                "write_stdin",
                "read_output",
                "update_plan",
                "context_status",
                "compact"
            ]
        );
        assert!(policy_denies_nothing_extra(&HarnessToolPolicy::bench()));
    }

    fn policy_denies_nothing_extra(policy: &HarnessToolPolicy) -> bool {
        policy.denied_tools.is_empty() && policy.cli_access.is_none()
    }

    #[test]
    fn modes_round_trip_and_order_by_restriction() {
        for mode in [
            WebAccessMode::Enabled,
            WebAccessMode::HostedOnly,
            WebAccessMode::Disabled,
        ] {
            assert_eq!(WebAccessMode::parse(mode.as_str()), Some(mode));
            assert_eq!(serde_json::to_value(mode).unwrap(), mode.as_str());
        }
        assert!(WebAccessMode::Disabled < WebAccessMode::HostedOnly);
        assert!(WebAccessMode::HostedOnly < WebAccessMode::Enabled);
    }

    #[test]
    fn file_path_scope_defaults_contained_and_layers_under_the_session() {
        assert_eq!(
            HarnessToolPolicy::default().file_path_scope(),
            FilePathScope::WorkingDir
        );
        assert!(
            HarnessToolPolicy::default()
                .cli_unsupported_field()
                .is_none()
        );
        let defaults = HarnessToolPolicy {
            file_paths: Some(FilePathScope::TaskEnvironment),
            ..HarnessToolPolicy::default()
        };
        assert_eq!(
            HarnessToolPolicy::default()
                .or_defaults(&defaults)
                .file_path_scope(),
            FilePathScope::TaskEnvironment
        );
        let explicit = HarnessToolPolicy {
            file_paths: Some(FilePathScope::WorkingDir),
            ..HarnessToolPolicy::default()
        };
        assert_eq!(
            explicit.or_defaults(&defaults).file_path_scope(),
            FilePathScope::WorkingDir
        );
        assert_eq!(explicit.cli_unsupported_field(), Some("file_paths"));
        let json = serde_json::json!({"file_paths": "task_environment"});
        let parsed: HarnessToolPolicy = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.file_path_scope(), FilePathScope::TaskEnvironment);
    }

    #[test]
    fn env_passthrough_refuses_credential_names_and_layers_as_a_union() {
        assert!(env_name_is_withheld("GITHUB_TOKEN"));
        assert!(env_name_is_withheld("anthropic_api_key"));
        assert!(env_name_is_withheld("DB_PASSWORD"));
        assert!(env_name_is_withheld("CLIENT_SECRET"));
        assert!(env_name_is_withheld("RSI_SESSION_TOKEN"));
        assert!(!env_name_is_withheld("JAVA_HOME"));
        assert!(!env_name_is_withheld("HTTPS_PROXY"));

        let allowed = HarnessToolPolicy {
            env_passthrough: vec!["JAVA_HOME".into(), "PYTHONPATH".into()],
            ..HarnessToolPolicy::default()
        };
        assert_eq!(allowed.validate(), Ok(()));
        assert_eq!(allowed.cli_unsupported_field(), Some("env_passthrough"));
        for bad in ["MY_TOKEN", "9LIVES", "has space", "", "RSI_ANYTHING"] {
            let policy = HarnessToolPolicy {
                env_passthrough: vec![bad.into()],
                ..HarnessToolPolicy::default()
            };
            assert_eq!(policy.validate(), Err(TOOL_POLICY_INVALID), "{bad}");
        }
        let defaults = HarnessToolPolicy {
            env_passthrough: vec!["PYTHONPATH".into(), "CC".into()],
            ..HarnessToolPolicy::default()
        };
        assert_eq!(
            allowed.or_defaults(&defaults).env_passthrough,
            ["JAVA_HOME", "PYTHONPATH", "CC"]
        );
        // An unset list is not serialised, so stored policy rows are unchanged.
        assert!(
            !serde_json::to_string(&HarnessToolPolicy::default())
                .unwrap()
                .contains("env_passthrough")
        );
    }

    #[test]
    fn unknown_fields_and_malformed_names_are_refused() {
        assert!(
            serde_json::from_value::<HarnessToolPolicy>(serde_json::json!({"web": "off"})).is_err()
        );
        let mut policy = HarnessToolPolicy::default();
        assert_eq!(policy.validate(), Ok(()));
        policy.denied_tools = vec!["Web Search".into()];
        assert_eq!(policy.validate(), Err(TOOL_POLICY_INVALID));
        policy.denied_tools = vec![String::new()];
        assert_eq!(policy.validate(), Err(TOOL_POLICY_INVALID));
        policy.denied_tools = vec!["shell".into()];
        policy.enabled_tools = Some(vec!["x".repeat(TOOL_POLICY_MAX_NAME_BYTES + 1)]);
        assert_eq!(policy.validate(), Err(TOOL_POLICY_INVALID));
    }

    #[test]
    fn defaults_fill_only_what_the_session_leaves_unset() {
        let defaults = HarnessToolPolicy {
            denied_tools: vec!["shell".into()],
            web_access: Some(WebAccessMode::HostedOnly),
            budgets: ToolBudgets {
                max_search_calls: Some(5),
                max_fetch_calls: Some(5),
                ..ToolBudgets::default()
            },
            ..HarnessToolPolicy::default()
        };
        let session = HarnessToolPolicy {
            denied_tools: vec!["git".into()],
            web_access: Some(WebAccessMode::Disabled),
            budgets: ToolBudgets {
                max_search_calls: Some(1),
                ..ToolBudgets::default()
            },
            ..HarnessToolPolicy::default()
        };
        let merged = session.or_defaults(&defaults);
        assert_eq!(merged.web_access, Some(WebAccessMode::Disabled));
        assert_eq!(merged.budgets.max_search_calls, Some(1));
        assert_eq!(merged.budgets.max_fetch_calls, Some(5));
        assert_eq!(
            merged.denied_tools,
            vec!["git".to_string(), "shell".to_string()]
        );
        assert_eq!(
            HarnessToolPolicy::default().web_mode(),
            WebAccessMode::Enabled
        );
    }

    #[test]
    fn egress_mode_layers_under_the_session_and_fails_closed() {
        let defaults = HarnessToolPolicy {
            egress: Some(EgressMode::Offline),
            ..HarnessToolPolicy::default()
        };
        let unset = HarnessToolPolicy::default();
        assert_eq!(
            unset.or_defaults(&defaults).egress_mode(),
            EgressMode::Offline
        );
        let explicit = HarnessToolPolicy {
            egress: Some(EgressMode::DenyPrivate),
            ..HarnessToolPolicy::default()
        };
        assert_eq!(
            explicit.or_defaults(&defaults).egress_mode(),
            EgressMode::DenyPrivate
        );
        assert_eq!(unset.egress_mode(), EgressMode::DenyPrivate);
        assert_eq!(
            HarnessToolPolicy::fail_closed().egress_mode(),
            EgressMode::Offline
        );
        // An unset field is not serialised, so stored policy rows are unchanged.
        assert!(!serde_json::to_string(&unset).unwrap().contains("egress"));
        let parsed: HarnessToolPolicy =
            serde_json::from_value(serde_json::json!({"egress": "offline"})).unwrap();
        assert_eq!(parsed.egress, Some(EgressMode::Offline));
    }

    #[test]
    fn context_editing_defaults_on_layers_and_stays_out_of_a_default_wire_form() {
        let unset = HarnessToolPolicy::default();
        assert!(unset.context_editing_enabled());
        assert!(
            !serde_json::to_string(&unset)
                .unwrap()
                .contains("context_editing")
        );
        assert!(unset.is_default());

        let off: HarnessToolPolicy =
            serde_json::from_value(serde_json::json!({"context_editing": false})).unwrap();
        assert!(!off.context_editing_enabled());
        assert!(!off.is_default());
        let layered = HarnessToolPolicy::default().or_defaults(&off);
        assert_eq!(layered.context_editing, Some(false));
        let session_on = HarnessToolPolicy {
            context_editing: Some(true),
            ..Default::default()
        };
        assert!(session_on.or_defaults(&off).context_editing_enabled());
    }

    #[test]
    fn cli_access_round_trips_layers_and_stays_out_of_a_default_wire_form() {
        let unset = HarnessToolPolicy::default();
        assert!(
            !serde_json::to_string(&unset)
                .unwrap()
                .contains("cli_access")
        );
        let read_only: HarnessToolPolicy =
            serde_json::from_value(serde_json::json!({"cli_access": "read_only"})).unwrap();
        assert_eq!(read_only.cli_access, Some(CliAccessMode::ReadOnly));
        assert!(!read_only.is_default());
        assert_eq!(read_only.cli_unsupported_field(), None);
        assert_eq!(
            serde_json::to_value(&read_only).unwrap()["cli_access"],
            CliAccessMode::ReadOnly.as_str()
        );
        assert_eq!(
            CliAccessMode::parse("read_only"),
            Some(CliAccessMode::ReadOnly)
        );
        assert!(
            serde_json::from_value::<HarnessToolPolicy>(serde_json::json!({"cli_access": "rw"}))
                .is_err()
        );
        // A session that leaves it unset follows the defaults; a set value wins.
        assert_eq!(
            unset.or_defaults(&read_only).cli_access,
            Some(CliAccessMode::ReadOnly)
        );
        assert_eq!(
            read_only.or_defaults(&unset).cli_access,
            Some(CliAccessMode::ReadOnly)
        );
    }

    #[test]
    fn a_provider_refuses_a_policy_field_it_cannot_enforce() {
        use crate::types::SessionProvider as P;
        let read_only = HarnessToolPolicy {
            cli_access: Some(CliAccessMode::ReadOnly),
            ..HarnessToolPolicy::default()
        };
        let web = HarnessToolPolicy {
            web_access: Some(WebAccessMode::Disabled),
            ..HarnessToolPolicy::default()
        };
        let mixed = HarnessToolPolicy {
            web_access: Some(WebAccessMode::Disabled),
            ..read_only.clone()
        };
        let refusal =
            |provider, policy: &HarnessToolPolicy| provider_policy_refusal(provider, Some(policy));
        // The CLIs enforce exactly the CLI access mode.
        for provider in [P::Claude, P::Codex, P::Pioneer] {
            assert_eq!(refusal(provider, &read_only), None, "{provider:?}");
            assert_eq!(refusal(provider, &HarnessToolPolicy::default()), None);
        }
        assert_eq!(
            refusal(P::Claude, &web).as_deref(),
            Some("tool_policy_unsupported_for_provider:claude:web_access")
        );
        assert_eq!(
            refusal(P::Codex, &mixed).as_deref(),
            Some("tool_policy_unsupported_for_provider:codex:web_access")
        );
        let budgets = HarnessToolPolicy {
            budgets: ToolBudgets {
                max_fetch_calls: Some(1),
                ..ToolBudgets::default()
            },
            ..HarnessToolPolicy::default()
        };
        assert_eq!(
            refusal(P::Claude, &budgets).as_deref(),
            Some("tool_policy_unsupported_for_provider:claude:budgets")
        );
        // The Harness loop enforces the Harness fields and refuses the CLI one.
        for provider in [P::Harness, P::OpenRouter, P::Bedrock] {
            assert_eq!(refusal(provider, &web), None, "{provider:?}");
        }
        assert_eq!(
            refusal(P::Harness, &read_only).as_deref(),
            Some("tool_policy_unsupported_for_provider:harness:cli_access")
        );
        // Providers with no policy support, and daemon defaults on a CLI.
        for (provider, label) in [
            (P::Local, "local"),
            (P::Antigravity, "antigravity"),
            (P::CodexAppServer, "codex_app_server"),
        ] {
            assert_eq!(
                refusal(provider, &read_only),
                Some(format!("tool_policy_unsupported_for_provider:{label}"))
            );
        }
        assert_eq!(
            provider_policy_refusal(P::Claude, None).as_deref(),
            Some("tool_policy_unsupported_for_provider:claude:daemon_defaults")
        );
        assert_eq!(provider_policy_refusal(P::Harness, None), None);
    }

    #[test]
    fn output_spill_is_per_command_tool_bounded_and_layered() {
        let spill = |bytes| OutputSpill {
            max_bytes: Some(bytes),
            max_lines: None,
        };
        let mut policy = HarnessToolPolicy::default();
        assert_eq!(policy.output_spill_for("shell"), None);
        policy.output_spill.insert("shell".into(), spill(32 * 1024));
        assert_eq!(policy.validate(), Ok(()));
        assert_eq!(policy.output_spill_for("shell"), Some(spill(32 * 1024)));
        assert_eq!(policy.cli_unsupported_field(), Some("output_spill"));
        // The default policy serialises without the field.
        let json = serde_json::to_string(&HarnessToolPolicy::default()).unwrap();
        assert!(!json.contains("output_spill"));
        // A file tool is not a command tool, and a value outside its range is refused.
        let mut bad = HarnessToolPolicy::default();
        bad.output_spill.insert("read_file".into(), spill(4096));
        assert_eq!(bad.validate(), Err(TOOL_POLICY_INVALID));
        for out in [spill(1024), spill(OUTPUT_SPILL_MAX_BYTES + 1)] {
            let mut bad = HarnessToolPolicy::default();
            bad.output_spill.insert("git".into(), out);
            assert_eq!(bad.validate(), Err(TOOL_POLICY_INVALID));
        }
        // A session entry wins over the defaults; the rest fill in.
        let mut defaults = HarnessToolPolicy::default();
        defaults.output_spill.insert("shell".into(), spill(4096));
        defaults.output_spill.insert("git".into(), spill(8192));
        let layered = policy.or_defaults(&defaults);
        assert_eq!(layered.output_spill_for("shell"), Some(spill(32 * 1024)));
        assert_eq!(layered.output_spill_for("git"), Some(spill(8192)));
    }
}
