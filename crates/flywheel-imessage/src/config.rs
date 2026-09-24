//! Configuration for the iMessage bridge.
//!
//! Loaded from `~/.rsi/imessage.toml` (or `--config` override).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

/// DM access policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum DmPolicy {
    /// Accept messages from anyone.
    #[serde(alias = "Open")]
    #[default]
    Open,
    /// Accept only from senders in `allow_from`.
    #[serde(alias = "Allowlist")]
    Allowlist,
    /// Drop all inbound messages.
    #[serde(alias = "Disabled")]
    Disabled,
}

/// Bridge configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImessageConfig {
    /// Whether the bridge is enabled (default true).
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Polling interval for chat.db in milliseconds (default 2000).
    #[serde(default = "default_poll_interval")]
    pub poll_interval_ms: u64,

    /// Allowed sender handles (phone numbers, iCloud emails).
    #[serde(default, alias = "allowlist")]
    pub allow_from: Vec<String>,

    /// DM access policy (default Open).
    #[serde(default)]
    pub dm_policy: DmPolicy,

    /// Maximum outbound message length before chunking (default 4000).
    #[serde(default = "default_max_message_length")]
    pub max_message_length: usize,

    /// TTL for echo cache entries in milliseconds (default 5000).
    #[serde(default = "default_echo_cache_ttl")]
    pub echo_cache_ttl_ms: u64,

    /// Debounce window in milliseconds (default 500).
    #[serde(default = "default_debounce", alias = "debounce_window_ms")]
    pub debounce_ms: u64,

    /// Default project ID for new sessions launched via iMessage.
    #[serde(default)]
    pub default_project_id: Option<Uuid>,

    /// Working directory for new sessions (REQUIRED for LaunchSession).
    #[serde(default = "default_working_dir", alias = "working_dir")]
    pub default_working_dir: PathBuf,

    /// Default provider for new sessions (default "Claude").
    #[serde(default)]
    pub default_provider: Option<String>,

    /// Default model for new sessions.
    #[serde(default)]
    pub default_model: Option<String>,

    /// Override chat.db path (default ~/Library/Messages/chat.db).
    #[serde(default)]
    pub chat_db_path: Option<PathBuf>,
}

impl Default for ImessageConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            poll_interval_ms: 2000,
            allow_from: Vec::new(),
            dm_policy: DmPolicy::Open,
            max_message_length: 4000,
            echo_cache_ttl_ms: 5000,
            debounce_ms: 500,
            default_project_id: None,
            default_working_dir: default_working_dir(),
            default_provider: None,
            default_model: None,
            chat_db_path: None,
        }
    }
}

impl ImessageConfig {
    /// Resolve the chat.db path (override or default macOS location).
    pub fn chat_db_path(&self) -> PathBuf {
        self.chat_db_path.clone().unwrap_or_else(|| {
            dirs::home_dir()
                .map(|h| h.join("Library/Messages/chat.db"))
                .unwrap_or_else(|| PathBuf::from("/Users/Shared/chat.db"))
        })
    }

    /// Load config from a TOML file. Returns default config if file doesn't exist.
    pub fn load(path: &std::path::Path) -> Result<Self, ConfigError> {
        if !path.exists() {
            tracing::info!(
                "Config file not found at {}, using defaults",
                path.display()
            );
            return Ok(Self::default());
        }

        let contents = std::fs::read_to_string(path).map_err(ConfigError::Io)?;
        let config: Self = toml::from_str(&contents).map_err(ConfigError::Parse)?;
        config.validate()?;
        Ok(config)
    }

    /// Default config file path.
    pub fn default_path() -> PathBuf {
        rsi_common::identity::data_path("imessage.toml", "imessage.toml")
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.poll_interval_ms < 500 {
            return Err(ConfigError::Validation(
                "poll_interval_ms must be >= 500".to_string(),
            ));
        }
        if self.max_message_length < 100 {
            return Err(ConfigError::Validation(
                "max_message_length must be >= 100".to_string(),
            ));
        }
        if self.dm_policy == DmPolicy::Allowlist && self.allow_from.is_empty() {
            return Err(ConfigError::Validation(
                "dm_policy is 'allowlist' but allow_from is empty".to_string(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("IO error reading config: {0}")]
    Io(std::io::Error),
    #[error("TOML parse error: {0}")]
    Parse(toml::de::Error),
    #[error("Config validation error: {0}")]
    Validation(String),
}

fn default_true() -> bool {
    true
}

fn default_poll_interval() -> u64 {
    2000
}

fn default_max_message_length() -> usize {
    4000
}

fn default_echo_cache_ttl() -> u64 {
    5000
}

fn default_debounce() -> u64 {
    500
}

fn default_working_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = ImessageConfig::default();
        assert!(config.enabled);
        assert_eq!(config.poll_interval_ms, 2000);
        assert_eq!(config.max_message_length, 4000);
        assert_eq!(config.echo_cache_ttl_ms, 5000);
        assert_eq!(config.debounce_ms, 500);
        assert_eq!(config.dm_policy, DmPolicy::Open);
        assert!(config.allow_from.is_empty());
        assert!(config.default_project_id.is_none());
        assert!(config.default_provider.is_none());
        assert!(config.default_model.is_none());
        assert!(config.chat_db_path.is_none());
    }

    #[test]
    fn test_parse_minimal_toml() {
        let toml_str = r#"
enabled = true
default_working_dir = "/home/user/projects"
"#;
        let config: ImessageConfig = toml::from_str(toml_str).unwrap();
        assert!(config.enabled);
        assert_eq!(
            config.default_working_dir,
            PathBuf::from("/home/user/projects")
        );
        assert_eq!(config.poll_interval_ms, 2000);
    }

    #[test]
    fn test_parse_full_toml() {
        let toml_str = r#"
enabled = true
poll_interval_ms = 3000
allow_from = ["+15551234567", "user@icloud.com"]
dm_policy = "allowlist"
max_message_length = 3000
echo_cache_ttl_ms = 8000
debounce_ms = 750
default_working_dir = "/home/user/projects"
default_provider = "Claude"
default_model = "claude-sonnet-5"
"#;
        let config: ImessageConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.poll_interval_ms, 3000);
        assert_eq!(config.allow_from.len(), 2);
        assert_eq!(config.dm_policy, DmPolicy::Allowlist);
        assert_eq!(config.max_message_length, 3000);
        assert_eq!(config.echo_cache_ttl_ms, 8000);
        assert_eq!(config.debounce_ms, 750);
    }

    #[test]
    fn test_validate_low_poll_interval() {
        let config = ImessageConfig {
            poll_interval_ms: 100,
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_empty_allowlist() {
        let config = ImessageConfig {
            dm_policy: DmPolicy::Allowlist,
            allow_from: vec![],
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_allowlist_with_entries() {
        let config = ImessageConfig {
            dm_policy: DmPolicy::Allowlist,
            allow_from: vec!["+15551234567".to_string()],
            ..Default::default()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_chat_db_path_override() {
        let config = ImessageConfig {
            chat_db_path: Some(PathBuf::from("/custom/chat.db")),
            ..Default::default()
        };
        assert_eq!(config.chat_db_path(), PathBuf::from("/custom/chat.db"));
    }

    #[test]
    fn test_chat_db_path_default() {
        let config = ImessageConfig::default();
        let path = config.chat_db_path();
        // Should contain Messages/chat.db somewhere in the path
        assert!(path.to_string_lossy().contains("chat.db"));
    }

    #[test]
    fn test_dm_policy_serde_roundtrip() {
        for policy in [DmPolicy::Open, DmPolicy::Allowlist, DmPolicy::Disabled] {
            let json = serde_json::to_string(&policy).unwrap();
            let deser: DmPolicy = serde_json::from_str(&json).unwrap();
            assert_eq!(deser, policy);
        }
    }
}
