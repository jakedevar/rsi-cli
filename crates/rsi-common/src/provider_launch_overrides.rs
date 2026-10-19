//! Operator provider launch-option pass-through (hub #2765, #2748 gap G14).
//!
//! The operator keeps one list of extra launch options per provider CLI in the
//! daemon field [`FIELD`] (`UpdateDaemonConfig`, TUI Settings > Provider
//! Isolation). Defaults are empty, and an empty list changes no argv.
//!
//! Every write is checked against a static, per-provider table before it is
//! stored:
//!
//! 1. A key on the deny list is refused with [`CODE_DENIED`]: anything that
//!    widens privilege (Claude `--dangerously-skip-permissions`,
//!    `--permission-mode`, `--add-dir`, `permissions.*`, `sandbox.*`,
//!    `hooks.*`; Codex `approval_policy`, `sandbox_mode`, `hooks.*`,
//!    `permissions.*`, `mcp_servers.*`, ...) and every option RSI sets itself
//!    (`--model`, `--resume`, `model_reasoning_effort`, ...). The deny list is
//!    matched case-insensitively and by prefix, so it errs on denying.
//! 2. A key that is not on the allowlist is refused with [`CODE_UNKNOWN_KEY`].
//! 3. A value that does not fit the key's type is refused with
//!    [`CODE_INVALID_VALUE`].
//!
//! The daemon also asks the installed Claude CLI about each allowed Claude flag
//! at write time, through the provider-contract canary's probe (#2755): a flag
//! the installed CLI rejects as an unknown option is refused with
//! [`CODE_UNKNOWN_KEY`]. There is no installed-CLI source for Codex config keys
//! yet (the canary checks hooks, not the config schema), so Codex keys are
//! checked against this table only.
//!
//! Accepted entries are applied at each CLI launch: Claude as `--flag=value`
//! arguments, Codex as `-c key=value` overrides with the value rendered as TOML.
//! Each run records what it applied on the session as a `System` row
//! (`metadata.provider_health.kind = "launch_overrides"`).

use serde_json::Value;

/// `UpdateDaemonConfig` / `GetDaemonConfig` field holding the overrides.
pub const FIELD: &str = "provider_launch_overrides";

/// The key is on the deny list (widens privilege or is set by RSI itself).
pub const CODE_DENIED: &str = "provider_launch_override_denied";
/// The key is not on the provider's allowlist, or the installed CLI rejects it.
pub const CODE_UNKNOWN_KEY: &str = "provider_launch_override_unknown_key";
/// The value does not fit the key's type.
pub const CODE_INVALID_VALUE: &str = "provider_launch_override_invalid_value";
/// The provider is not one whose launch options can be overridden.
pub const CODE_UNKNOWN_PROVIDER: &str = "provider_launch_override_unknown_provider";
/// The write is not an object of string lists (or a JSON string of one).
pub const CODE_MALFORMED: &str = "provider_launch_override_malformed";

/// The providers whose launch options can be overridden.
pub const PROVIDERS: [&str; 2] = ["claude", "codex"];

/// Most entries per provider.
pub const MAX_ENTRIES_PER_PROVIDER: usize = 16;

/// Longest free-text value (`--append-system-prompt`).
pub const MAX_TEXT_CHARS: usize = 2000;

/// Longest key or token value.
const MAX_TOKEN_CHARS: usize = 200;

/// A launch-option provider family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OverrideProvider {
    /// Every Claude Code CLI launch.
    Claude,
    /// Every Codex CLI (`codex exec`) launch: Codex, Pioneer and the
    /// Codex-routed OpenRouter and Bedrock launches.
    Codex,
}

impl OverrideProvider {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            _ => None,
        }
    }

    /// The allowlist of this provider.
    #[must_use]
    pub const fn known_keys(self) -> &'static [KnownKey] {
        match self {
            Self::Claude => CLAUDE_KNOWN_KEYS,
            Self::Codex => CODEX_KNOWN_KEYS,
        }
    }

    const fn deny_rules(self) -> &'static [DenyRule] {
        match self {
            Self::Claude => CLAUDE_DENY,
            Self::Codex => CODEX_DENY,
        }
    }
}

/// The type of an allowed key's value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueKind {
    /// One of a fixed set of words.
    Enum(&'static [&'static str]),
    /// `true` or `false`.
    Bool,
    /// An integer in `min..=max`.
    UInt { min: u64, max: u64 },
    /// One identifier (a model id): ASCII letters, digits and `._:/-[]`.
    Token,
    /// Up to `max` comma-separated tokens.
    TokenList { max: usize },
    /// Free text up to [`MAX_TEXT_CHARS`] (no NUL).
    Text,
}

impl ValueKind {
    fn describe(self) -> String {
        match self {
            Self::Enum(words) => format!("one of {}", words.join("|")),
            Self::Bool => "true or false".to_string(),
            Self::UInt { min, max } => format!("an integer {min}-{max}"),
            Self::Token => "a model id".to_string(),
            Self::TokenList { max } => format!("up to {max} comma-separated ids"),
            Self::Text => format!("text of at most {MAX_TEXT_CHARS} characters"),
        }
    }
}

/// One allowlisted key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KnownKey {
    /// Claude: the CLI flag (`--betas`). Codex: the config key
    /// (`model_reasoning_summary`).
    pub key: &'static str,
    pub kind: ValueKind,
}

/// Why a key is denied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DenyClass {
    /// Widens what the session may do (permissions, sandbox, hooks, tools,
    /// directories, MCP servers, credentials, environment).
    WidensPrivilege,
    /// RSI sets this option itself on every launch.
    RsiManaged,
}

impl DenyClass {
    const fn reason(self) -> &'static str {
        match self {
            Self::WidensPrivilege => "it widens the session's privilege",
            Self::RsiManaged => "RSI sets it itself on every launch",
        }
    }
}

/// A deny pattern: an exact key, `prefix.*` (the key and everything below it)
/// or `prefix*` (every key starting with `prefix`). Matched lower-cased.
#[derive(Debug, Clone, Copy)]
struct DenyRule {
    pattern: &'static str,
    class: DenyClass,
}

const fn widens(pattern: &'static str) -> DenyRule {
    DenyRule {
        pattern,
        class: DenyClass::WidensPrivilege,
    }
}

const fn managed(pattern: &'static str) -> DenyRule {
    DenyRule {
        pattern,
        class: DenyClass::RsiManaged,
    }
}

impl DenyRule {
    fn matches(&self, key_lower: &str) -> bool {
        if let Some(prefix) = self.pattern.strip_suffix(".*") {
            return key_lower == prefix
                || key_lower
                    .strip_prefix(prefix)
                    .is_some_and(|rest| rest.starts_with('.'));
        }
        if let Some(prefix) = self.pattern.strip_suffix('*') {
            return key_lower.starts_with(prefix);
        }
        key_lower == self.pattern
    }
}

/// Claude Code flags an operator may pass (each as `--flag=value`).
pub const CLAUDE_KNOWN_KEYS: &[KnownKey] = &[
    KnownKey {
        key: "--append-system-prompt",
        kind: ValueKind::Text,
    },
    KnownKey {
        key: "--betas",
        kind: ValueKind::TokenList { max: 8 },
    },
];

/// Claude deny list. `--settings` keys are listed too: they are not accepted
/// at all (only flags are), and naming the privileged ones gives the operator
/// the typed denial instead of an unknown-key refusal.
const CLAUDE_DENY: &[DenyRule] = &[
    widens("--dangerously*"),
    widens("--allow-dangerously*"),
    widens("--permission-mode"),
    widens("--permission-prompt-tool"),
    widens("--permission-prompts"),
    widens("--allowedtools"),
    widens("--allowed-tools"),
    widens("--disallowedtools"),
    widens("--disallowed-tools"),
    widens("--tools"),
    widens("--add-dir"),
    widens("--settings"),
    widens("--setting-sources"),
    widens("--mcp-config"),
    widens("--strict-mcp-config"),
    widens("--plugin-dir"),
    widens("--plugin-url"),
    widens("--agents"),
    widens("--agent"),
    widens("--bare"),
    widens("--safe-mode"),
    widens("--restricted"),
    widens("--ide"),
    widens("--remote-control"),
    widens("--cloud"),
    widens("--teleport"),
    widens("--worktree"),
    widens("--debug-file"),
    // `--settings` keys.
    widens("permissions.*"),
    widens("sandbox.*"),
    widens("hooks.*"),
    widens("disableallhooks"),
    widens("env.*"),
    widens("apikeyhelper"),
    widens("awscredentialexport"),
    widens("awsauthrefresh"),
    widens("otelheadershelper"),
    widens("defaultmode"),
    widens("additionaldirectories"),
    widens("enableallprojectmcpservers"),
    widens("enabledmcpjsonservers"),
    widens("allowedmcpservers"),
    widens("statusline.*"),
    widens("skipdangerousmodepermissionprompt"),
    widens("forceloginmethod"),
    // Set by RSI on every launch.
    managed("-p"),
    managed("--print"),
    managed("--output-format"),
    managed("--input-format"),
    managed("--verbose"),
    managed("--model"),
    managed("--max-turns"),
    managed("--system-prompt"),
    managed("--resume"),
    managed("-r"),
    managed("--continue"),
    managed("-c"),
    managed("--fork-session"),
    managed("--session-id"),
    managed("--effort"),
    managed("--no-session-persistence"),
    managed("--disable-slash-commands"),
    // #2762: the operator's own settings `claude_fallback_models` and
    // `claude_max_budget_usd` set these.
    managed("--fallback-model"),
    managed("--max-budget-usd"),
];

/// Codex config keys an operator may pass (each as `-c key=value`).
pub const CODEX_KNOWN_KEYS: &[KnownKey] = &[
    KnownKey {
        key: "model_reasoning_summary",
        kind: ValueKind::Enum(&["auto", "concise", "detailed", "none"]),
    },
    KnownKey {
        key: "model_verbosity",
        kind: ValueKind::Enum(&["low", "medium", "high"]),
    },
    KnownKey {
        key: "service_tier",
        kind: ValueKind::Enum(&["fast", "flex"]),
    },
    KnownKey {
        key: "web_search",
        kind: ValueKind::Enum(&["disabled", "cached", "live"]),
    },
    KnownKey {
        key: "hide_agent_reasoning",
        kind: ValueKind::Bool,
    },
    KnownKey {
        key: "show_raw_agent_reasoning",
        kind: ValueKind::Bool,
    },
    KnownKey {
        key: "agents.enabled",
        kind: ValueKind::Bool,
    },
];

/// Codex deny list. Codex overrides are `-c` config keys only, so every flag
/// is refused; the privileged ones are named for the typed denial.
const CODEX_DENY: &[DenyRule] = &[
    widens("--dangerously*"),
    widens("--yolo"),
    widens("--full-auto"),
    widens("--sandbox"),
    widens("-s"),
    widens("--ask-for-approval"),
    widens("-a"),
    widens("--add-dir"),
    widens("--ignore-user-config"),
    widens("--ignore-rules"),
    widens("--profile"),
    widens("-p"),
    widens("--config"),
    widens("-c"),
    widens("approval_policy.*"),
    widens("approvals_reviewer.*"),
    widens("sandbox_mode"),
    widens("sandbox_workspace_write.*"),
    widens("sandbox_permissions.*"),
    widens("sandbox.*"),
    widens("default_permissions"),
    widens("permissions.*"),
    widens("hooks.*"),
    widens("mcp_servers.*"),
    widens("shell_environment_policy.*"),
    widens("projects.*"),
    widens("notify"),
    widens("model_provider"),
    widens("model_providers.*"),
    widens("chatgpt_base_url"),
    widens("experimental_*"),
    widens("tools.*"),
    widens("features.*"),
    widens("profile"),
    widens("profiles.*"),
    widens("cli_auth_credentials_store"),
    widens("forced_login_method"),
    widens("zdr.*"),
    // Set by RSI on every launch.
    managed("model"),
    managed("model_reasoning_effort"),
    managed("model_context_window"),
    managed("model_auto_compact_token_limit"),
    managed("model_catalog_json"),
    // #2917: the operator's own settings `codex_max_concurrent_subagents` and
    // `codex_subagent_model` set these on every launch; a later `-c` of the
    // same key would silently replace that governance setting.
    managed("agents.max_concurrent_threads_per_session"),
    managed("agents.default_subagent_model"),
];

/// A typed refusal of a write to [`FIELD`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchOverrideError {
    /// One of the `CODE_*` constants.
    pub code: &'static str,
    pub provider: Option<&'static str>,
    pub key: Option<String>,
    pub detail: String,
}

impl LaunchOverrideError {
    fn new(
        code: &'static str,
        provider: Option<OverrideProvider>,
        key: Option<&str>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            code,
            provider: provider.map(OverrideProvider::name),
            key: key.map(str::to_string),
            detail: detail.into(),
        }
    }

    /// The refusal of a Claude flag the installed CLI does not know.
    #[must_use]
    pub fn unknown_to_installed_claude(flag: &str) -> Self {
        Self::new(
            CODE_UNKNOWN_KEY,
            Some(OverrideProvider::Claude),
            Some(flag),
            "the installed claude CLI rejects it as an unknown option",
        )
    }

    /// The refusal of a Codex config key the installed codex-cli does not know.
    #[must_use]
    pub fn unknown_to_installed_codex(key: &str) -> Self {
        Self::new(
            CODE_UNKNOWN_KEY,
            Some(OverrideProvider::Codex),
            Some(key),
            "the installed codex CLI does not know this config key",
        )
    }
}

impl std::fmt::Display for LaunchOverrideError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:", self.code)?;
        if let Some(provider) = self.provider {
            write!(f, " {provider}")?;
        }
        if let Some(key) = &self.key {
            write!(f, " {key}")?;
        }
        write!(f, ": {}", self.detail)
    }
}

impl std::error::Error for LaunchOverrideError {}

/// One accepted override.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchOverride {
    pub key: String,
    pub value: String,
}

impl LaunchOverride {
    /// The canonical `key=value` text (what is stored and shown).
    #[must_use]
    pub fn entry_text(&self) -> String {
        format!("{}={}", self.key, self.value)
    }
}

/// The validated overrides of every provider. `Default` is empty.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderLaunchOverrides {
    pub claude: Vec<LaunchOverride>,
    pub codex: Vec<LaunchOverride>,
}

impl ProviderLaunchOverrides {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.claude.is_empty() && self.codex.is_empty()
    }

    #[must_use]
    pub fn entries(&self, provider: OverrideProvider) -> &[LaunchOverride] {
        match provider {
            OverrideProvider::Claude => &self.claude,
            OverrideProvider::Codex => &self.codex,
        }
    }

    /// The canonical `key=value` texts of `provider`.
    #[must_use]
    pub fn entry_texts(&self, provider: OverrideProvider) -> Vec<String> {
        self.entries(provider)
            .iter()
            .map(LaunchOverride::entry_text)
            .collect()
    }

    /// `GetDaemonConfig` form: `{"claude": ["--betas=x"], "codex": [...]}`,
    /// providers without entries omitted (`{}` when empty).
    #[must_use]
    pub fn to_json(&self) -> Value {
        let mut map = serde_json::Map::new();
        for provider in [OverrideProvider::Claude, OverrideProvider::Codex] {
            let texts = self.entry_texts(provider);
            if !texts.is_empty() {
                map.insert(provider.name().to_string(), texts.into());
            }
        }
        Value::Object(map)
    }

    /// The Claude CLI arguments: one `--flag=value` per entry, in order.
    #[must_use]
    pub fn claude_args(&self) -> Vec<String> {
        self.claude.iter().map(LaunchOverride::entry_text).collect()
    }

    /// The Codex CLI arguments: `-c key=<TOML value>` per entry, in order.
    #[must_use]
    pub fn codex_args(&self) -> Vec<String> {
        let mut args = Vec::with_capacity(self.codex.len() * 2);
        for entry in &self.codex {
            let kind = lookup(OverrideProvider::Codex, &entry.key).map(|known| known.kind);
            args.push("-c".to_string());
            args.push(format!("{}={}", entry.key, toml_value(kind, &entry.value)));
        }
        args
    }

    /// The Claude flags configured (for the installed-CLI check).
    #[must_use]
    pub fn claude_flags(&self) -> Vec<&str> {
        self.claude.iter().map(|entry| entry.key.as_str()).collect()
    }

    /// The Codex config keys configured (for the installed-CLI check).
    #[must_use]
    pub fn codex_keys(&self) -> Vec<&str> {
        self.codex.iter().map(|entry| entry.key.as_str()).collect()
    }
}

/// The TOML form of a validated Codex value: booleans and integers bare,
/// everything else a basic string (the validated characters need no escape
/// beyond `"` and `\`, which are escaped anyway).
fn toml_value(kind: Option<ValueKind>, value: &str) -> String {
    match kind {
        Some(ValueKind::Bool | ValueKind::UInt { .. }) => value.to_string(),
        _ => {
            let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
            format!("\"{escaped}\"")
        }
    }
}

fn lookup(provider: OverrideProvider, key: &str) -> Option<&'static KnownKey> {
    provider.known_keys().iter().find(|known| known.key == key)
}

/// Normalise a write to [`FIELD`].
///
/// Accepts `null`, `""`, `"clear"`/`"none"`/`"off"` or `{}` (clear), an object
/// `{"claude": [...], "codex": [...]}` whose entries are `"key=value"` strings
/// or `{"key": ..., "value": ...}` objects, or a JSON string holding such an
/// object (the TUI editor's form). The write replaces every provider's list.
///
/// # Errors
/// The first refusal: [`CODE_MALFORMED`], [`CODE_UNKNOWN_PROVIDER`],
/// [`CODE_DENIED`], [`CODE_UNKNOWN_KEY`] or [`CODE_INVALID_VALUE`].
pub fn normalize(value: &Value) -> Result<ProviderLaunchOverrides, LaunchOverrideError> {
    let object = match value {
        Value::Null => return Ok(ProviderLaunchOverrides::default()),
        Value::String(text) => {
            let trimmed = text.trim();
            if matches!(
                trimmed.to_ascii_lowercase().as_str(),
                "" | "clear" | "none" | "off"
            ) {
                return Ok(ProviderLaunchOverrides::default());
            }
            let parsed: Value = serde_json::from_str(trimmed).map_err(|error| {
                LaunchOverrideError::new(
                    CODE_MALFORMED,
                    None,
                    None,
                    format!(
                        "expected a JSON object such as {{\"codex\":[\"model_reasoning_summary=concise\"]}}: {error}"
                    ),
                )
            })?;
            if !parsed.is_object() {
                return Err(malformed("expected a JSON object"));
            }
            return normalize(&parsed);
        }
        Value::Object(object) => object,
        _ => return Err(malformed("expected an object, a JSON string or null")),
    };
    let mut out = ProviderLaunchOverrides::default();
    for (name, entries) in object {
        let provider = OverrideProvider::parse(name).ok_or_else(|| {
            LaunchOverrideError::new(
                CODE_UNKNOWN_PROVIDER,
                None,
                Some(name),
                format!(
                    "launch options can be set for {} only",
                    PROVIDERS.join(" and ")
                ),
            )
        })?;
        let list = normalize_provider(provider, entries)?;
        match provider {
            OverrideProvider::Claude => out.claude = list,
            OverrideProvider::Codex => out.codex = list,
        }
    }
    Ok(out)
}

fn malformed(detail: &str) -> LaunchOverrideError {
    LaunchOverrideError::new(CODE_MALFORMED, None, None, detail)
}

fn normalize_provider(
    provider: OverrideProvider,
    entries: &Value,
) -> Result<Vec<LaunchOverride>, LaunchOverrideError> {
    let items: &[Value] = match entries {
        Value::Null => &[],
        Value::Array(items) => items,
        _ => {
            return Err(LaunchOverrideError::new(
                CODE_MALFORMED,
                Some(provider),
                None,
                "expected a list of \"key=value\" entries",
            ));
        }
    };
    if items.len() > MAX_ENTRIES_PER_PROVIDER {
        return Err(LaunchOverrideError::new(
            CODE_MALFORMED,
            Some(provider),
            None,
            format!("at most {MAX_ENTRIES_PER_PROVIDER} entries per provider"),
        ));
    }
    let mut out: Vec<LaunchOverride> = Vec::with_capacity(items.len());
    for item in items {
        let (key, value) = split_entry(provider, item)?;
        let entry = validate_entry(provider, &key, value.as_deref())?;
        if out.iter().any(|seen| seen.key == entry.key) {
            return Err(LaunchOverrideError::new(
                CODE_INVALID_VALUE,
                Some(provider),
                Some(&entry.key),
                "named more than once",
            ));
        }
        out.push(entry);
    }
    Ok(out)
}

/// `"key=value"`, `"key"` or `{"key": .., "value": ..}` into its parts.
fn split_entry(
    provider: OverrideProvider,
    item: &Value,
) -> Result<(String, Option<String>), LaunchOverrideError> {
    match item {
        Value::String(text) => {
            let text = text.trim();
            Ok(match text.split_once('=') {
                Some((key, value)) => (key.trim().to_string(), Some(value.to_string())),
                None => (text.to_string(), None),
            })
        }
        Value::Object(object) => {
            let key = object
                .get("key")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    LaunchOverrideError::new(
                        CODE_MALFORMED,
                        Some(provider),
                        None,
                        "an entry object needs a string \"key\"",
                    )
                })?
                .trim()
                .to_string();
            let value = match object.get("value") {
                None | Some(Value::Null) => None,
                Some(Value::String(text)) => Some(text.clone()),
                Some(Value::Bool(flag)) => Some(flag.to_string()),
                Some(Value::Number(number)) => Some(number.to_string()),
                Some(_) => {
                    return Err(LaunchOverrideError::new(
                        CODE_INVALID_VALUE,
                        Some(provider),
                        Some(&key),
                        "the value must be a string, number or bool",
                    ));
                }
            };
            Ok((key, value))
        }
        _ => Err(LaunchOverrideError::new(
            CODE_MALFORMED,
            Some(provider),
            None,
            "an entry is a \"key=value\" string",
        )),
    }
}

/// The deny class of `key` for `provider`, if any.
fn denied(provider: OverrideProvider, key: &str) -> Option<DenyClass> {
    let lower = key.to_ascii_lowercase();
    // Codex dotted keys may be quoted segments; strip quotes so `"hooks".x`
    // is still `hooks.x`.
    let lower: String = lower.chars().filter(|c| !matches!(c, '"' | '\'')).collect();
    provider
        .deny_rules()
        .iter()
        .find(|rule| rule.matches(&lower))
        .map(|rule| rule.class)
}

fn validate_entry(
    provider: OverrideProvider,
    key: &str,
    value: Option<&str>,
) -> Result<LaunchOverride, LaunchOverrideError> {
    if key.is_empty() {
        return Err(LaunchOverrideError::new(
            CODE_MALFORMED,
            Some(provider),
            None,
            "an entry has an empty key",
        ));
    }
    if let Some(class) = denied(provider, key) {
        return Err(LaunchOverrideError::new(
            CODE_DENIED,
            Some(provider),
            Some(key),
            format!("denied because {}", class.reason()),
        ));
    }
    let Some(known) = lookup(provider, key) else {
        return Err(unknown_key(provider, key));
    };
    let Some(value) = value else {
        return Err(LaunchOverrideError::new(
            CODE_INVALID_VALUE,
            Some(provider),
            Some(key),
            format!("needs a value: {}", known.kind.describe()),
        ));
    };
    let value = validate_value(known.kind, value).ok_or_else(|| {
        LaunchOverrideError::new(
            CODE_INVALID_VALUE,
            Some(provider),
            Some(key),
            format!("expected {}", known.kind.describe()),
        )
    })?;
    Ok(LaunchOverride {
        key: known.key.to_string(),
        value,
    })
}

fn unknown_key(provider: OverrideProvider, key: &str) -> LaunchOverrideError {
    let allowed: Vec<&str> = provider
        .known_keys()
        .iter()
        .map(|known| known.key)
        .collect();
    LaunchOverrideError::new(
        CODE_UNKNOWN_KEY,
        Some(provider),
        Some(key),
        format!(
            "not an allowed {} option; allowed: {}",
            provider.name(),
            allowed.join(", ")
        ),
    )
}

fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '/' | '-' | '[' | ']')
}

fn valid_token(token: &str) -> bool {
    !token.is_empty() && token.len() <= MAX_TOKEN_CHARS && token.chars().all(is_token_char)
}

/// The canonical value, or `None` when it does not fit `kind`.
fn validate_value(kind: ValueKind, value: &str) -> Option<String> {
    match kind {
        ValueKind::Text => {
            let ok = !value.trim().is_empty()
                && value.chars().count() <= MAX_TEXT_CHARS
                && !value.contains('\0');
            ok.then(|| value.to_string())
        }
        ValueKind::Enum(words) => {
            let lower = value.trim().to_ascii_lowercase();
            words
                .iter()
                .find(|word| **word == lower)
                .map(|word| (*word).to_string())
        }
        ValueKind::Bool => match value.trim().to_ascii_lowercase().as_str() {
            "true" => Some("true".to_string()),
            "false" => Some("false".to_string()),
            _ => None,
        },
        ValueKind::UInt { min, max } => value
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|number| (min..=max).contains(number))
            .map(|number| number.to_string()),
        ValueKind::Token => {
            let token = value.trim();
            valid_token(token).then(|| token.to_string())
        }
        ValueKind::TokenList { max } => {
            let tokens: Vec<&str> = value.split(',').map(str::trim).collect();
            let ok = tokens.len() <= max && tokens.iter().all(|token| valid_token(token));
            ok.then(|| tokens.join(","))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn refusal(value: Value) -> LaunchOverrideError {
        normalize(&value).expect_err("must be refused")
    }

    #[test]
    fn defaults_and_clears_are_empty_and_render_no_args() {
        for clear in [
            Value::Null,
            json!(""),
            json!("clear"),
            json!("off"),
            json!({}),
            json!("{}"),
            json!({"claude": [], "codex": null}),
        ] {
            let overrides = normalize(&clear).unwrap();
            assert!(overrides.is_empty(), "{clear}");
            assert!(overrides.claude_args().is_empty());
            assert!(overrides.codex_args().is_empty());
            assert_eq!(overrides.to_json(), json!({}));
        }
        assert!(ProviderLaunchOverrides::default().is_empty());
    }

    /// Golden argv: every allowed Claude flag becomes one `--flag=value`
    /// argument, in the operator's order.
    #[test]
    fn allowed_claude_flags_render_golden_argv() {
        let overrides = normalize(&json!({"claude": [
            "--append-system-prompt=Prefer small diffs.",
            {"key": "--betas", "value": "context-1m-2025-08-07, other-beta"},
        ]}))
        .unwrap();
        assert_eq!(
            overrides.claude_args(),
            vec![
                "--append-system-prompt=Prefer small diffs.",
                "--betas=context-1m-2025-08-07,other-beta",
            ]
        );
        assert!(overrides.codex_args().is_empty());
        assert_eq!(
            overrides.claude_flags(),
            vec!["--append-system-prompt", "--betas"]
        );
    }

    /// Golden `-c`: every allowed Codex key becomes `-c key=<TOML>`; strings
    /// quoted, booleans and integers bare, enum words canonical lower case.
    #[test]
    fn allowed_codex_keys_render_golden_config_overrides() {
        let overrides = normalize(&json!({"codex": [
            "model_reasoning_summary=Concise",
            "model_verbosity=low",
            "service_tier=fast",
            "web_search=cached",
            "hide_agent_reasoning=TRUE",
            {"key": "show_raw_agent_reasoning", "value": false},
            "agents.enabled=true",
        ]}))
        .unwrap();
        assert_eq!(
            overrides.codex_args(),
            vec![
                "-c",
                "model_reasoning_summary=\"concise\"",
                "-c",
                "model_verbosity=\"low\"",
                "-c",
                "service_tier=\"fast\"",
                "-c",
                "web_search=\"cached\"",
                "-c",
                "hide_agent_reasoning=true",
                "-c",
                "show_raw_agent_reasoning=false",
                "-c",
                "agents.enabled=true",
            ]
        );
        assert!(overrides.claude_args().is_empty());
    }

    #[test]
    fn the_stored_form_round_trips_through_json_and_the_editor_string() {
        let overrides = normalize(&json!({
            "codex": ["model_reasoning_summary=concise"],
            "claude": ["--betas=a,b"],
        }))
        .unwrap();
        let stored = overrides.to_json();
        assert_eq!(
            stored,
            json!({"claude": ["--betas=a,b"], "codex": ["model_reasoning_summary=concise"]})
        );
        assert_eq!(normalize(&stored).unwrap(), overrides);
        assert_eq!(normalize(&json!(stored.to_string())).unwrap(), overrides);
    }

    /// Everything that widens privilege is denied with the typed code, for
    /// either provider, whatever its case or nesting.
    #[test]
    fn privilege_widening_keys_are_denied_with_the_typed_code() {
        let claude = [
            "--dangerously-skip-permissions",
            "--allow-dangerously-skip-permissions",
            "--permission-mode=bypassPermissions",
            "--allowedTools=Bash",
            "--disallowedTools=",
            "--add-dir=/",
            "--settings={}",
            "--mcp-config=x.json",
            "--plugin-dir=/tmp",
            "--bare",
            "permissions.allow=Bash(*)",
            "permissions=x",
            "sandbox.enabled=false",
            "sandbox.network.allowUnixSockets=true",
            "hooks.PreToolUse=[]",
            "env.ANTHROPIC_API_KEY=x",
            "apiKeyHelper=/bin/sh",
        ];
        for entry in claude {
            let error = refusal(json!({"claude": [entry]}));
            assert_eq!(error.code, CODE_DENIED, "{entry}: {error}");
            assert_eq!(error.provider, Some("claude"));
            assert!(error.to_string().starts_with(CODE_DENIED), "{error}");
        }
        let codex = [
            "approval_policy=never",
            "APPROVAL_POLICY=never",
            "sandbox_mode=danger-full-access",
            "sandbox_workspace_write.network_access=true",
            "sandbox_workspace_write.writable_roots=[\"/\"]",
            "hooks.PreToolUse=[]",
            "\"hooks\".state=x",
            "permissions.default.filesystem=x",
            "default_permissions=full",
            "mcp_servers.evil.command=x",
            "shell_environment_policy.inherit=all",
            "projects./.trust_level=trusted",
            "notify=[\"sh\"]",
            "model_provider=x",
            "--dangerously-bypass-approvals-and-sandbox",
            "--dangerously-bypass-hook-trust",
            "--full-auto",
            "--sandbox=danger-full-access",
            "features.anything=true",
        ];
        for entry in codex {
            let error = refusal(json!({"codex": [entry]}));
            assert_eq!(error.code, CODE_DENIED, "{entry}: {error}");
            assert_eq!(error.provider, Some("codex"));
        }
    }

    #[test]
    fn options_rsi_sets_itself_are_denied() {
        for entry in [
            "--model=x",
            "--resume=x",
            "--effort=max",
            "--system-prompt=x",
            "--fallback-model=claude-sonnet-5-5",
            "--max-budget-usd=5",
        ] {
            let error = refusal(json!({"claude": [entry]}));
            assert_eq!(error.code, CODE_DENIED, "{entry}");
            assert!(error.detail.contains("RSI sets it itself"), "{error}");
        }
        for entry in [
            "model=x",
            "model_reasoning_effort=high",
            "model_context_window=1",
        ] {
            assert_eq!(
                refusal(json!({"codex": [entry]})).code,
                CODE_DENIED,
                "{entry}"
            );
        }
    }

    /// #2917: the subagent model and concurrency cap have dedicated RSI
    /// settings (`codex_subagent_model`, `codex_max_concurrent_subagents`) that
    /// the launch passes first; an override of the same key would replace
    /// them, so both are refused, in any spelling and in either entry form.
    #[test]
    fn codex_subagent_governance_keys_are_denied_as_rsi_managed() {
        for entry in [
            json!("agents.max_concurrent_threads_per_session=64"),
            json!("AGENTS.Max_Concurrent_Threads_Per_Session=64"),
            json!("agents.\"max_concurrent_threads_per_session\"=64"),
            json!({"key": "agents.max_concurrent_threads_per_session", "value": 2}),
            json!("agents.max_concurrent_threads_per_session=0"),
            json!("agents.default_subagent_model=gpt-6-astra"),
            json!("agents.default_subagent_model=\"x\""),
            json!({"key": "agents.default_subagent_model", "value": "gpt-6-astra"}),
        ] {
            let error = refusal(json!({"codex": [entry.clone()]}));
            assert_eq!(error.code, CODE_DENIED, "{entry}: {error}");
            assert_eq!(error.provider, Some("codex"));
            assert!(error.detail.contains("RSI sets it itself"), "{error}");
        }
        // The other `agents.*` key stays operator-settable.
        assert!(normalize(&json!({"codex": ["agents.enabled=false"]})).is_ok());
    }

    #[test]
    fn unknown_keys_and_providers_are_refused() {
        let error = refusal(json!({"claude": ["--no-such-flag=1"]}));
        assert_eq!(error.code, CODE_UNKNOWN_KEY);
        assert_eq!(error.key.as_deref(), Some("--no-such-flag"));
        assert!(error.detail.contains("--append-system-prompt"), "{error}");
        // A Claude `--settings` key that is not privileged is still unknown:
        // only flags are accepted.
        assert_eq!(
            refusal(json!({"claude": ["fallbackModel=x"]})).code,
            CODE_UNKNOWN_KEY
        );
        assert_eq!(
            refusal(json!({"codex": ["no_such_key=1"]})).code,
            CODE_UNKNOWN_KEY
        );
        // The allowlist is exact: case variants are not the allowed key.
        assert_eq!(
            refusal(json!({"codex": ["Model_Verbosity=low"]})).code,
            CODE_UNKNOWN_KEY
        );
        assert_eq!(refusal(json!({"gemini": []})).code, CODE_UNKNOWN_PROVIDER);
    }

    #[test]
    fn values_are_type_checked() {
        for (provider, entry) in [
            ("codex", "model_reasoning_summary=verbose"),
            ("codex", "hide_agent_reasoning=yes"),
            ("codex", "agents.enabled=maybe"),
            ("codex", "service_tier"),
            ("claude", "--betas=a,b,c,d,e,f,g,h,i"),
            ("claude", "--append-system-prompt="),
            ("claude", "--betas=a;b"),
        ] {
            let error = refusal(json!({provider: [entry]}));
            assert_eq!(error.code, CODE_INVALID_VALUE, "{entry}: {error}");
        }
        let long = format!("--append-system-prompt={}", "x".repeat(MAX_TEXT_CHARS + 1));
        assert_eq!(refusal(json!({"claude": [long]})).code, CODE_INVALID_VALUE);
        assert_eq!(
            refusal(json!({"codex": ["model_verbosity=low", "model_verbosity=high"]})).code,
            CODE_INVALID_VALUE
        );
    }

    #[test]
    fn malformed_writes_are_refused() {
        for bad in [
            json!(7),
            json!([]),
            json!("not json"),
            json!("[1]"),
            json!({"codex": "model_verbosity=low"}),
            json!({"codex": [7]}),
            json!({"codex": [{"value": "x"}]}),
        ] {
            assert_eq!(refusal(bad.clone()).code, CODE_MALFORMED, "{bad}");
        }
        let many: Vec<String> = (0..=MAX_ENTRIES_PER_PROVIDER)
            .map(|_| "model_verbosity=low".to_string())
            .collect();
        assert_eq!(refusal(json!({"codex": many})).code, CODE_MALFORMED);
    }

    #[test]
    fn every_allowed_key_is_off_the_deny_list() {
        for provider in [OverrideProvider::Claude, OverrideProvider::Codex] {
            for known in provider.known_keys() {
                assert!(denied(provider, known.key).is_none(), "{}", known.key);
            }
        }
    }
}
