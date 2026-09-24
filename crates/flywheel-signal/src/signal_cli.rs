//! signal-cli subprocess integration.
//!
//! Shells out to the `signal-cli` binary for both inbound (`receive`) and
//! outbound (`send`). Replaces the macOS-specific `chatdb.rs` + `applescript.rs`
//! modules from `flywheel-imessage`.
//!
//! Envelope parser is pure and fully unit-tested. The subprocess-invoking
//! `SignalCli` surface is fleshed out in Phase 3.

use serde::Deserialize;
use std::path::PathBuf;

/// A message pulled from one `signal-cli receive` envelope.
#[derive(Debug, Clone)]
pub struct InboundMessage {
    /// Unix epoch millis from `envelope.timestamp`
    /// (or `syncMessage.sentMessage.timestamp` if the outer timestamp is absent).
    pub timestamp_ms: u64,
    /// E.164 of the conversation — for DMs: `envelope.source` (or `sourceNumber`);
    /// for `syncMessage.sentMessage`: `syncMessage.sentMessage.destination`.
    pub sender: String,
    /// Message text (`dataMessage.message` OR `syncMessage.sentMessage.message`).
    pub text: String,
    /// True if this envelope is a sync replay of our own send from another Signal client.
    /// Used to feed the echo cache but still route through the pipeline (desktop replies
    /// are legitimate user input that the cache filters only when they echo our outbound).
    pub is_sync: bool,
}

// ---------- Envelope parsing ----------

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct RawEnvelopeWrapper {
    envelope: RawEnvelope,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct RawEnvelope {
    source: Option<String>,
    #[serde(rename = "sourceNumber")]
    source_number: Option<String>,
    timestamp: Option<u64>,
    #[serde(rename = "dataMessage")]
    data_message: Option<RawDataMessage>,
    #[serde(rename = "syncMessage")]
    sync_message: Option<RawSyncMessage>,
    // Variants we explicitly drop:
    #[serde(rename = "receiptMessage")]
    receipt_message: Option<serde_json::Value>,
    #[serde(rename = "typingMessage")]
    typing_message: Option<serde_json::Value>,
    #[serde(rename = "callMessage")]
    call_message: Option<serde_json::Value>,
    #[serde(rename = "storyMessage")]
    story_message: Option<serde_json::Value>,
    #[serde(rename = "editMessage")]
    edit_message: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct RawDataMessage {
    timestamp: Option<u64>,
    message: Option<String>,
    #[serde(rename = "groupInfo")]
    group_info: Option<serde_json::Value>,
    #[serde(rename = "groupV2")]
    group_v2: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct RawSyncMessage {
    #[serde(rename = "sentMessage")]
    sent_message: Option<RawSentMessage>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct RawSentMessage {
    destination: Option<String>,
    #[serde(rename = "destinationNumber")]
    destination_number: Option<String>,
    timestamp: Option<u64>,
    message: Option<String>,
    #[serde(rename = "groupInfo")]
    group_info: Option<serde_json::Value>,
    #[serde(rename = "groupV2")]
    group_v2: Option<serde_json::Value>,
}

/// Parse one line of `signal-cli --output=json receive` stdout.
///
/// Returns `None` for:
///  - Non-JSON lines (parse error).
///  - Envelopes with no `dataMessage.message` and no `syncMessage.sentMessage.message`.
///  - Envelopes carrying `groupInfo` / `groupV2` (v1 is DM-only).
///  - Envelopes with empty trimmed text.
///  - Envelopes with no sender/destination to route to.
pub fn parse_envelope(line: &str) -> Option<InboundMessage> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }

    let parsed: RawEnvelopeWrapper = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!("Dropping non-JSON signal-cli line: {}", e);
            return None;
        }
    };

    let env = parsed.envelope;

    // Explicit drop variants
    if env.receipt_message.is_some() {
        tracing::debug!("Dropping receiptMessage envelope");
        return None;
    }
    if env.typing_message.is_some() {
        tracing::debug!("Dropping typingMessage envelope");
        return None;
    }
    if env.call_message.is_some() {
        tracing::debug!("Dropping callMessage envelope");
        return None;
    }
    if env.story_message.is_some() {
        tracing::debug!("Dropping storyMessage envelope");
        return None;
    }
    if env.edit_message.is_some() {
        tracing::debug!("Dropping editMessage envelope");
        return None;
    }

    // Prefer dataMessage (real inbound DM) over syncMessage (our-own-send replay)
    if let Some(data) = env.data_message {
        if data.group_info.is_some() || data.group_v2.is_some() {
            tracing::debug!("Dropping group dataMessage envelope (v1 is DM-only)");
            return None;
        }
        let text = data.message.unwrap_or_default();
        if text.trim().is_empty() {
            return None;
        }
        let sender = match env.source.or(env.source_number) {
            Some(s) if !s.trim().is_empty() => s,
            _ => {
                tracing::debug!("Dropping dataMessage envelope with no source");
                return None;
            }
        };
        let timestamp_ms = env.timestamp.or(data.timestamp).unwrap_or(0);
        return Some(InboundMessage {
            timestamp_ms,
            sender,
            text,
            is_sync: false,
        });
    }

    if let Some(sync) = env.sync_message
        && let Some(sent) = sync.sent_message
    {
        if sent.group_info.is_some() || sent.group_v2.is_some() {
            tracing::debug!("Dropping group syncMessage envelope (v1 is DM-only)");
            return None;
        }
        let text = sent.message.unwrap_or_default();
        if text.trim().is_empty() {
            return None;
        }
        // For sync: the "sender" is the destination (the other end of the DM),
        // because the user replied from their desktop Signal to that person.
        let sender = match sent.destination.or(sent.destination_number) {
            Some(s) if !s.trim().is_empty() => s,
            _ => {
                tracing::debug!("Dropping syncMessage with no destination");
                return None;
            }
        };
        let timestamp_ms = env.timestamp.or(sent.timestamp).unwrap_or(0);
        return Some(InboundMessage {
            timestamp_ms,
            sender,
            text,
            is_sync: true,
        });
    }

    None
}

// ---------- Subprocess surface (Phase 3) ----------

/// Errors emitted by the signal-cli subprocess wrapper.
#[derive(Debug, thiserror::Error)]
pub enum SignalCliError {
    #[error("signal-cli binary not found (set signal_cli_path in config or add to PATH)")]
    BinaryNotFound,
    #[error("signal-cli exited non-zero: {0}")]
    Execution(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Wraps the `signal-cli` CLI for spawn-per-tick inbound polling and
/// on-demand outbound sends.
#[derive(Debug, Clone)]
pub struct SignalCli {
    /// Path to the signal-cli binary (absolute).
    binary: PathBuf,
    /// Account E.164.
    account: String,
}

impl SignalCli {
    /// Resolve the signal-cli binary (config override first, then `which`) and
    /// bind it to the given account.
    pub fn new(account: String, override_path: Option<PathBuf>) -> Result<Self, SignalCliError> {
        let binary = match override_path {
            Some(p) => p,
            None => which::which("signal-cli").map_err(|_| SignalCliError::BinaryNotFound)?,
        };
        Ok(Self { binary, account })
    }

    /// Binary path (for diagnostics).
    #[allow(dead_code)]
    pub fn binary(&self) -> &std::path::Path {
        &self.binary
    }

    /// Configured account (for diagnostics).
    #[allow(dead_code)]
    pub fn account(&self) -> &str {
        &self.account
    }

    /// Spawn `signal-cli --output=json -a <account> receive`, parse each stdout
    /// line into an envelope, and return the resulting messages. Exits when
    /// signal-cli exits (it does so after draining the server queue).
    pub async fn poll(&self) -> Result<Vec<InboundMessage>, SignalCliError> {
        let output = tokio::process::Command::new(&self.binary)
            .args(["--output=json", "-a", &self.account, "receive"])
            .output()
            .await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(SignalCliError::Execution(stderr.trim().to_string()));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut messages = Vec::new();
        for line in stdout.lines() {
            if let Some(msg) = parse_envelope(line) {
                messages.push(msg);
            }
        }
        Ok(messages)
    }

    /// Shell out to `signal-cli -a <account> send -m <text> <recipient>`.
    /// Returns `Ok(())` on exit status 0.
    pub async fn send(&self, recipient: &str, text: &str) -> Result<(), SignalCliError> {
        let output = tokio::process::Command::new(&self.binary)
            .args(["-a", &self.account, "send", "-m", text, recipient])
            .output()
            .await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            tracing::warn!("signal-cli send to {} failed: {}", recipient, stderr.trim());
            return Err(SignalCliError::Execution(stderr.trim().to_string()));
        }
        tracing::debug!(
            "Sent signal message to {} ({} chars)",
            recipient,
            text.len()
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_data_message() {
        let line = r#"{
            "envelope": {
                "source": "+15551234567",
                "sourceNumber": "+15551234567",
                "timestamp": 1713542400000,
                "dataMessage": {
                    "timestamp": 1713542400000,
                    "message": "!new fix the login bug"
                }
            }
        }"#;
        let msg = parse_envelope(line).expect("should parse");
        assert_eq!(msg.sender, "+15551234567");
        assert_eq!(msg.text, "!new fix the login bug");
        assert_eq!(msg.timestamp_ms, 1713542400000);
        assert!(!msg.is_sync);
    }

    #[test]
    fn test_parse_sync_sent_message() {
        let line = r#"{
            "envelope": {
                "source": "+15557777777",
                "timestamp": 1713542460000,
                "syncMessage": {
                    "sentMessage": {
                        "destination": "+15551234567",
                        "destinationNumber": "+15551234567",
                        "timestamp": 1713542460000,
                        "message": "looks good ship it"
                    }
                }
            }
        }"#;
        let msg = parse_envelope(line).expect("should parse sync");
        // For sync, sender is the *destination* (the other side of the chat).
        assert_eq!(msg.sender, "+15551234567");
        assert_eq!(msg.text, "looks good ship it");
        assert_eq!(msg.timestamp_ms, 1713542460000);
        assert!(msg.is_sync);
    }

    #[test]
    fn test_parse_group_info_dropped() {
        let line = r#"{
            "envelope": {
                "source": "+15551234567",
                "timestamp": 1713542400000,
                "dataMessage": {
                    "message": "hey team",
                    "groupInfo": {"groupId": "abc"}
                }
            }
        }"#;
        assert!(parse_envelope(line).is_none());
    }

    #[test]
    fn test_parse_group_v2_dropped() {
        let line = r#"{
            "envelope": {
                "source": "+15551234567",
                "timestamp": 1713542400000,
                "dataMessage": {
                    "message": "hey team",
                    "groupV2": {"id": "abc"}
                }
            }
        }"#;
        assert!(parse_envelope(line).is_none());
    }

    #[test]
    fn test_parse_receipt_dropped() {
        let line = r#"{
            "envelope": {
                "source": "+15551234567",
                "timestamp": 1713542400000,
                "receiptMessage": {"when": 1713542400000, "isDelivery": true}
            }
        }"#;
        assert!(parse_envelope(line).is_none());
    }

    #[test]
    fn test_parse_typing_dropped() {
        let line = r#"{
            "envelope": {
                "source": "+15551234567",
                "timestamp": 1713542400000,
                "typingMessage": {"action": "STARTED"}
            }
        }"#;
        assert!(parse_envelope(line).is_none());
    }

    #[test]
    fn test_parse_call_dropped() {
        let line = r#"{
            "envelope": {
                "source": "+15551234567",
                "timestamp": 1713542400000,
                "callMessage": {"offerMessage": {}}
            }
        }"#;
        assert!(parse_envelope(line).is_none());
    }

    #[test]
    fn test_parse_story_dropped() {
        let line = r#"{
            "envelope": {
                "source": "+15551234567",
                "timestamp": 1713542400000,
                "storyMessage": {}
            }
        }"#;
        assert!(parse_envelope(line).is_none());
    }

    #[test]
    fn test_parse_edit_dropped() {
        let line = r#"{
            "envelope": {
                "source": "+15551234567",
                "timestamp": 1713542400000,
                "editMessage": {}
            }
        }"#;
        assert!(parse_envelope(line).is_none());
    }

    #[test]
    fn test_parse_malformed_json() {
        assert!(parse_envelope("not json").is_none());
        assert!(parse_envelope("").is_none());
        assert!(parse_envelope("   ").is_none());
        assert!(parse_envelope("{\"broken").is_none());
    }

    #[test]
    fn test_parse_empty_text_dropped() {
        let line = r#"{
            "envelope": {
                "source": "+15551234567",
                "timestamp": 1713542400000,
                "dataMessage": { "message": "" }
            }
        }"#;
        assert!(parse_envelope(line).is_none());

        let whitespace_only = r#"{
            "envelope": {
                "source": "+15551234567",
                "timestamp": 1713542400000,
                "dataMessage": { "message": "   \n\t   " }
            }
        }"#;
        assert!(parse_envelope(whitespace_only).is_none());
    }

    #[test]
    fn test_parse_source_number_fallback() {
        let line = r#"{
            "envelope": {
                "sourceNumber": "+15551234567",
                "timestamp": 1713542400000,
                "dataMessage": { "message": "hi" }
            }
        }"#;
        let msg = parse_envelope(line).expect("should parse via sourceNumber");
        assert_eq!(msg.sender, "+15551234567");
    }

    #[test]
    fn test_parse_data_message_inner_timestamp_fallback() {
        // Outer timestamp absent but dataMessage.timestamp present — use the inner.
        let line = r#"{
            "envelope": {
                "source": "+15551234567",
                "dataMessage": { "timestamp": 1713542400000, "message": "hi" }
            }
        }"#;
        let msg = parse_envelope(line).expect("should parse");
        assert_eq!(msg.timestamp_ms, 1713542400000);
    }

    #[test]
    fn test_parse_no_source_dropped() {
        // dataMessage present but no source at all — we cannot route.
        let line = r#"{
            "envelope": {
                "timestamp": 1713542400000,
                "dataMessage": { "message": "hi" }
            }
        }"#;
        assert!(parse_envelope(line).is_none());
    }

    #[test]
    fn test_parse_sync_no_destination_dropped() {
        let line = r#"{
            "envelope": {
                "source": "+15557777777",
                "timestamp": 1713542400000,
                "syncMessage": { "sentMessage": { "message": "hi" } }
            }
        }"#;
        assert!(parse_envelope(line).is_none());
    }

    #[test]
    fn test_parse_empty_envelope_dropped() {
        let line = r#"{ "envelope": {} }"#;
        assert!(parse_envelope(line).is_none());
    }
}
