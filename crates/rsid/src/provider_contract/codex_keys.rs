//! Codex `-c` config keys against the installed codex-cli (#281, hub #2765).
//!
//! `provider_launch_overrides` keeps a static allowlist (#278). This module
//! asks the installed CLI whether it still knows an allowed key, in two steps:
//!
//! 1. `codex app-server generate-json-schema` writes the CLI's own config
//!    schema (`v2/ConfigReadResponse.json`, local, no network, no model turn).
//!    A key it lists is known. The schema is a partial view (`Config` allows
//!    additional properties), so a key it does not list is only *not
//!    modelled*, unless the schema walk reaches an object that is closed.
//! 2. A not-modelled key is probed: `codex -c 'key={ rsi_probe = true }'
//!    features list` fails config loading naming `key` when the CLI types the
//!    key, and succeeds when the CLI ignores an unknown key.
//!
//! Both are bounded by the canary's probe timeout and cached per binary
//! fingerprint (path, size, mtime), so the CLI is asked at most once per CLI
//! change. When the CLI cannot answer the check is *degraded*: nothing is
//! refused and the static allowlist stands alone.

use serde_json::Value;
use std::collections::HashMap;

/// Degraded: no codex-cli is installed.
pub const DEGRADED_CLI_UNAVAILABLE: &str = "codex_cli_unavailable";
/// Degraded: the CLI could not produce its config schema.
pub const DEGRADED_SCHEMA_UNAVAILABLE: &str = "codex_schema_unavailable";
/// Degraded: the schema does not model the key and the probe was inconclusive.
pub const DEGRADED_PROBE_INCONCLUSIVE: &str = "codex_key_probe_inconclusive";

/// What one probe of a key not in the schema found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigKeyProbe {
    /// The CLI typed the key (rejected a nonsense value): it knows the key.
    Typed,
    /// The CLI ignored the nonsense value: it does not know the key.
    Untyped,
    /// The CLI could not run to a verdict.
    Inconclusive,
}

/// What the schema says about one key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaVerdict {
    Known,
    /// Absent from a closed object: the CLI does not have the key.
    Unknown,
    /// The schema does not describe this part of the config.
    NotModelled,
}

/// The CLI's config schema (`v2/ConfigReadResponse.json`).
#[derive(Debug)]
pub struct CodexConfigSchema {
    document: Value,
}

impl CodexConfigSchema {
    /// The schema, or `None` when the document has no `Config` object.
    #[must_use]
    pub fn parse(document: Value) -> Option<Self> {
        let config = document.pointer("/definitions/Config")?;
        config.get("properties")?.as_object()?;
        Some(Self { document })
    }

    /// Follow `$ref`, then the first `anyOf`/`oneOf`/`allOf` alternative that
    /// describes an object.
    fn resolve<'a>(&'a self, node: &'a Value, depth: usize) -> &'a Value {
        if depth > 8 {
            return node;
        }
        if let Some(target) = node
            .get("$ref")
            .and_then(Value::as_str)
            .and_then(|reference| reference.strip_prefix("#/definitions/"))
            .and_then(|name| self.document.pointer(&format!("/definitions/{name}")))
        {
            return self.resolve(target, depth + 1);
        }
        for combinator in ["anyOf", "oneOf", "allOf"] {
            let Some(alternatives) = node.get(combinator).and_then(Value::as_array) else {
                continue;
            };
            for alternative in alternatives {
                let resolved = self.resolve(alternative, depth + 1);
                if resolved.get("properties").is_some() {
                    return resolved;
                }
            }
        }
        node
    }

    /// Whether the schema lists the dotted config `key`.
    #[must_use]
    pub fn classify(&self, key: &str) -> SchemaVerdict {
        let Some(config) = self.document.pointer("/definitions/Config") else {
            return SchemaVerdict::NotModelled;
        };
        let mut node = config;
        let mut segments = key.split('.').peekable();
        while let Some(segment) = segments.next() {
            let current = self.resolve(node, 0);
            let Some(properties) = current.get("properties").and_then(Value::as_object) else {
                return SchemaVerdict::NotModelled;
            };
            let Some(next) = properties.get(segment) else {
                let open = current
                    .get("additionalProperties")
                    .is_none_or(|extra| extra != &Value::Bool(false));
                return if open {
                    SchemaVerdict::NotModelled
                } else {
                    SchemaVerdict::Unknown
                };
            };
            if segments.peek().is_none() {
                return SchemaVerdict::Known;
            }
            node = next;
        }
        SchemaVerdict::NotModelled
    }
}

/// Per-binary cache of what the installed codex-cli said.
#[derive(Debug, Default)]
pub(super) struct CodexKeyCache {
    /// The binary fingerprint every entry below answers for.
    pub fingerprint: Option<String>,
    /// `Some(None)`: the schema was asked for and the CLI could not give it.
    pub schema: Option<Option<std::sync::Arc<CodexConfigSchema>>>,
    /// Probe verdicts by key (`true`: the CLI knows the key).
    pub probes: HashMap<String, bool>,
    /// When the schema last failed, for the retry back-off.
    pub schema_failed_at: Option<std::time::Instant>,
}

/// The outcome of checking Codex override keys against the installed CLI.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CodexKeyCheck {
    /// Keys the installed CLI does not know, in the order asked.
    pub unknown: Vec<String>,
    /// Why the check could not be complete (a `DEGRADED_*` code): the static
    /// allowlist alone answered for the keys it names.
    pub degraded: Option<&'static str>,
}

impl CodexKeyCheck {
    /// The `launch_override_check` object an operator response carries.
    #[must_use]
    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "codex": if self.degraded.is_some() { "degraded" } else { "checked" },
            "degraded_reason": self.degraded,
        })
    }
}
