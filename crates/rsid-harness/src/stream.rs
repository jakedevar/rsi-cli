//! The raw event the harness tools report progress through.

use serde::Deserialize;
use serde_json::Value;

/// Raw event from a provider's stream-json output.
/// Parsed from NDJSON lines.
#[derive(Debug, Clone, Deserialize)]
pub struct StreamEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(flatten)]
    pub data: Value,
}
