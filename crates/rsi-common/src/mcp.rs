//! Operator-only MCP server configuration and credential wire types (#788).
//!
//! Server definitions are nonsecret. Credentials are passed only in the
//! secret request type below, whose `Debug` is redacted. Every response type
//! is metadata-only by construction.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;

pub const MAX_MCP_SERVERS: usize = 32;
pub const MAX_MCP_ID_BYTES: usize = 64;
pub const MAX_MCP_COMMAND_BYTES: usize = 1024;
pub const MAX_MCP_ARG_COUNT: usize = 32;
pub const MAX_MCP_ARG_BYTES: usize = 4096;
pub const MAX_MCP_SECRET_ENV_NAMES: usize = 16;
pub const MAX_MCP_SECRET_ENV_NAME_BYTES: usize = 64;
pub const MAX_MCP_WORKING_DIR_BYTES: usize = 4096;

pub const METHOD_LIST: &str = "ListMcpServers";
pub const METHOD_UPSERT: &str = "UpsertMcpServer";
pub const METHOD_SET_ENABLED: &str = "SetMcpServerEnabled";
pub const METHOD_SET_SECRET: &str = "SetMcpServerSecret";
pub const METHOD_ROTATE_SECRET: &str = "RotateMcpServerSecret";
pub const METHOD_CLEAR_SECRET: &str = "ClearMcpServerSecret";
pub const OPERATOR_METHODS: [&str; 6] = [
    METHOD_LIST,
    METHOD_UPSERT,
    METHOD_SET_ENABLED,
    METHOD_SET_SECRET,
    METHOD_ROTATE_SECRET,
    METHOD_CLEAR_SECRET,
];

/// Canonical server id: lowercase ASCII letter/digit/hyphen, not starting or
/// ending with a hyphen and without repeated hyphens.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerDefinition {
    pub id: String,
    /// Absolute executable path. Arguments are a closed array, never a shell
    /// string.
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Environment variable names that will receive this server's vault
    /// credential when a later slice spawns it. Names, never values.
    #[serde(default)]
    pub secret_env_names: Vec<String>,
    /// Optional absolute working directory. `None` leaves the later confined
    /// child at the policy-selected default.
    #[serde(default)]
    pub working_dir: Option<String>,
    /// Definitions never become active without an explicit operator action.
    pub enabled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpCredentialState {
    Vault,
    Cleared,
    Absent,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpCredentialMetadata {
    pub id: String,
    pub state: McpCredentialState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotated_from_fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cleared_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub generation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerSummary {
    pub definition: McpServerDefinition,
    pub credential: McpCredentialMetadata,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListMcpServersResult {
    pub servers: Vec<McpServerSummary>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpsertMcpServerParams {
    pub server: McpServerDefinition,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerIdParams {
    pub id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetMcpServerEnabledParams {
    pub id: String,
    pub enabled: bool,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetMcpServerSecretParams {
    pub id: String,
    pub secret: String,
}

impl fmt::Debug for SetMcpServerSecretParams {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SetMcpServerSecretParams")
            .field("id", &self.id)
            .field("secret", &"<redacted>")
            .finish()
    }
}

fn bounded_bytes(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && !value.contains('\0')
}

fn valid_id(value: &str) -> bool {
    if !bounded_bytes(value, MAX_MCP_ID_BYTES) {
        return false;
    }
    let bytes = value.as_bytes();
    if bytes[0] == b'-' || bytes[bytes.len() - 1] == b'-' {
        return false;
    }
    let mut previous_hyphen = false;
    bytes.iter().all(|byte| {
        let valid = byte.is_ascii_lowercase()
            || byte.is_ascii_digit()
            || (*byte == b'-' && !previous_hyphen);
        previous_hyphen = *byte == b'-';
        valid
    })
}

fn valid_secret_env_name(value: &str) -> bool {
    if !bounded_bytes(value, MAX_MCP_SECRET_ENV_NAME_BYTES) {
        return false;
    }
    const DENIED_NAMES: &[&str] = &[
        "PATH",
        "HOME",
        "SHELL",
        "IFS",
        "ENV",
        "BASH_ENV",
        "USER",
        "LOGNAME",
        "PWD",
        "OLDPWD",
        "TMPDIR",
        "TERM",
        "LANG",
        "NODE_OPTIONS",
        "NODE_PATH",
        "PYTHONPATH",
        "PYTHONHOME",
        "PYTHONSTARTUP",
        "RUBYLIB",
        "RUBYOPT",
        "PERL5LIB",
        "PERL5OPT",
        "JAVA_TOOL_OPTIONS",
        "_JAVA_OPTIONS",
        "CLASSPATH",
        "GOFLAGS",
        "CARGO_HOME",
        "RUSTFLAGS",
        "RUSTC_WRAPPER",
    ];
    const DENIED_PREFIXES: &[&str] = &[
        "LD_",
        "DYLD_",
        "RSI_",
        "CLAUDE",
        "CODEX",
        "OPENAI_",
        "ANTHROPIC_",
        "AWS_",
        "GIT_",
        "SSH_",
        "XDG_",
        "LC_",
    ];
    if DENIED_NAMES.contains(&value)
        || DENIED_PREFIXES
            .iter()
            .any(|prefix| value.starts_with(prefix))
    {
        return false;
    }
    value.bytes().enumerate().all(|(index, byte)| {
        byte.is_ascii_uppercase() || byte == b'_' || (index > 0 && byte.is_ascii_digit())
    })
}

/// Validate one nonsecret definition. Errors are static so malformed operator
/// input can never be echoed as an error.
#[must_use]
pub fn validate_mcp_server_definition(
    definition: &McpServerDefinition,
) -> Result<(), &'static str> {
    if !valid_id(&definition.id) {
        return Err("mcp server id must be canonical lowercase ascii");
    }
    if !definition.command.starts_with('/')
        || !bounded_bytes(&definition.command, MAX_MCP_COMMAND_BYTES)
    {
        return Err("mcp command must be an absolute path");
    }
    if definition.args.len() > MAX_MCP_ARG_COUNT
        || definition
            .args
            .iter()
            .any(|argument| argument.contains('\0') || argument.len() > MAX_MCP_ARG_BYTES)
    {
        return Err("mcp args exceed the bounded count or length");
    }
    if definition.secret_env_names.len() > MAX_MCP_SECRET_ENV_NAMES
        || definition
            .secret_env_names
            .iter()
            .any(|name| !valid_secret_env_name(name))
    {
        return Err("mcp secret environment names are invalid or exceed bounds");
    }
    let mut names = definition.secret_env_names.clone();
    names.sort_unstable();
    if names.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err("mcp secret environment names must be unique");
    }
    if let Some(working_dir) = &definition.working_dir
        && (!working_dir.starts_with('/') || !bounded_bytes(working_dir, MAX_MCP_WORKING_DIR_BYTES))
    {
        return Err("mcp working_dir must be an absolute path within bounds");
    }
    Ok(())
}

/// Validate the id used by metadata and credential operations.
#[must_use]
pub fn validate_mcp_server_id(id: &str) -> Result<(), &'static str> {
    valid_id(id).then_some(()).ok_or("invalid mcp server id")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn definition_validation_accepts_a_bounded_absolute_spec() {
        let definition = McpServerDefinition {
            id: "docs-1".into(),
            command: "/usr/local/bin/mcp-docs".into(),
            args: vec!["--stdio".into()],
            secret_env_names: vec!["MCP_DOCS_TOKEN".into()],
            working_dir: Some("/tmp".into()),
            enabled: false,
        };
        assert_eq!(validate_mcp_server_definition(&definition), Ok(()));
    }

    #[test]
    fn definition_validation_rejects_shell_and_noncanonical_input() {
        for id in ["", "Docs", "-docs", "docs-", "docs--1", "docs_1"] {
            assert!(validate_mcp_server_id(id).is_err(), "{id}");
        }
        let mut definition = McpServerDefinition {
            id: "docs".into(),
            command: "mcp-docs --stdio".into(),
            args: Vec::new(),
            secret_env_names: Vec::new(),
            working_dir: None,
            enabled: false,
        };
        assert!(validate_mcp_server_definition(&definition).is_err());
        definition.command = "/usr/local/bin/mcp-docs".into();
        definition.working_dir = Some("relative".into());
        assert!(validate_mcp_server_definition(&definition).is_err());
        definition.working_dir = None;
        definition.secret_env_names = vec!["MCP_TOKEN".into(), "MCP_TOKEN".into()];
        assert!(validate_mcp_server_definition(&definition).is_err());
    }

    #[test]
    fn secret_env_names_reject_dangerous_exact_names_and_prefixes() {
        const DENIED_NAMES: &[&str] = &[
            "PATH",
            "HOME",
            "SHELL",
            "IFS",
            "ENV",
            "BASH_ENV",
            "USER",
            "LOGNAME",
            "PWD",
            "OLDPWD",
            "TMPDIR",
            "TERM",
            "LANG",
            "NODE_OPTIONS",
            "NODE_PATH",
            "PYTHONPATH",
            "PYTHONHOME",
            "PYTHONSTARTUP",
            "RUBYLIB",
            "RUBYOPT",
            "PERL5LIB",
            "PERL5OPT",
            "JAVA_TOOL_OPTIONS",
            "_JAVA_OPTIONS",
            "CLASSPATH",
            "GOFLAGS",
            "CARGO_HOME",
            "RUSTFLAGS",
            "RUSTC_WRAPPER",
        ];
        const DENIED_PREFIX_NAMES: &[&str] = &[
            "LD_PRELOAD",
            "DYLD_INSERT_LIBRARIES",
            "RSI_SESSION_TOKEN",
            "CLAUDE_CODE_TOKEN",
            "CODEX_API_KEY",
            "OPENAI_API_KEY",
            "ANTHROPIC_API_KEY",
            "AWS_SESSION_TOKEN",
            "GIT_ASKPASS",
            "SSH_AUTH_SOCK",
            "XDG_CONFIG_HOME",
            "LC_ALL",
        ];
        for name in DENIED_NAMES
            .iter()
            .copied()
            .chain(DENIED_PREFIX_NAMES.iter().copied())
        {
            assert!(!valid_secret_env_name(name), "{name}");
        }
    }

    #[test]
    fn secret_env_names_accept_mcp_names() {
        assert!(valid_secret_env_name("MCP_DOCS_TOKEN"));
        assert!(valid_secret_env_name("MCP_TOKEN"));
    }

    #[test]
    fn secret_request_debug_is_redacted() {
        let params = SetMcpServerSecretParams {
            id: "docs".into(),
            secret: "mcp-test-secret-canary".into(),
        };
        let debug = format!("{params:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("mcp-test-secret-canary"));
    }
}
