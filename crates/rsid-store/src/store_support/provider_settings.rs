//! Provider option types the store and config read (moved down from `codex`,
//! `claude` and `provider`, which re-export them so callers keep their paths).

/// Sandbox policy for each `codex exec` turn, including resumed turns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexSandboxMode {
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

impl CodexSandboxMode {
    pub fn cli_arg(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::DangerFullAccess => "danger-full-access",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        let normalized = raw.trim().to_ascii_lowercase().replace('_', "-");
        match normalized.as_str() {
            "read-only" => Some(Self::ReadOnly),
            "workspace-write" => Some(Self::WorkspaceWrite),
            "danger-full-access" => Some(Self::DangerFullAccess),
            _ => None,
        }
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
    /// except one passed via `--mcp-config` (RSI passes only its own
    /// `rsi-agent` gateway, and only to a tokened session).
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

    pub fn parse(raw: &str) -> Option<Self> {
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
    pub fn cli_flags(self) -> &'static [&'static str] {
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

/// Decision type for structured approval responses.
#[derive(Debug, Clone)]
pub enum ApprovalDecision {
    /// Approve for this single request.
    Approve,
    /// Approve for the remainder of the session.
    ApproveForSession,
    /// Deny the request.
    Deny,
}
