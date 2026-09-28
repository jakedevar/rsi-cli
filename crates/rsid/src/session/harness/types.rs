//! Core types for the agent harness LLM conversation protocol.
//!
//! These map to the intersection of Anthropic's Messages API and OpenAI's
//! Chat Completions API. Provider implementations translate to/from these.

// Provider layer is built ahead of resolve_provider() wiring (Phase 6+).
#![allow(dead_code)]

use serde::{Deserialize, Serialize};

const TOOL_BLOCKS_PREFIX: &str = "\u{001e}rsi-tool-blocks-v1:";

/// Tool content independent of any provider's request format. Image data is
/// base64 encoded; callers must apply their own size and media-type limits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolContentBlock {
    Text { text: String },
    Image { media_type: String, data: String },
}

fn encode_tool_blocks(blocks: &[ToolContentBlock]) -> String {
    format!(
        "{TOOL_BLOCKS_PREFIX}{}",
        serde_json::to_string(blocks).expect("tool content blocks serialize")
    )
}

fn decode_tool_blocks(content: &str) -> Option<Vec<ToolContentBlock>> {
    content
        .strip_prefix(TOOL_BLOCKS_PREFIX)
        .and_then(|json| serde_json::from_str(json).ok())
}

fn visible_tool_text(blocks: &[ToolContentBlock]) -> String {
    blocks
        .iter()
        .map(|block| match block {
            ToolContentBlock::Text { text } => text.clone(),
            ToolContentBlock::Image { media_type, .. } => format!("[image: {media_type}]"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Role in a conversation message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}

/// A single message in the conversation history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: MessageRole,
    pub content: String,
    /// For tool result messages: the ID of the tool call this responds to.
    pub tool_call_id: Option<String>,
    /// Whether this tool result represents a failed execution.
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_error: bool,
    /// Tool calls requested by the assistant (populated when role == Assistant).
    pub tool_calls: Vec<ToolCall>,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::System,
            content: content.into(),
            tool_call_id: None,
            is_error: false,
            tool_calls: Vec::new(),
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::User,
            content: content.into(),
            tool_call_id: None,
            is_error: false,
            tool_calls: Vec::new(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Assistant,
            content: content.into(),
            tool_call_id: None,
            is_error: false,
            tool_calls: Vec::new(),
        }
    }

    pub fn tool_result(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Tool,
            content: content.into(),
            tool_call_id: Some(call_id.into()),
            is_error: false,
            tool_calls: Vec::new(),
        }
    }

    pub fn tool_error_result(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Tool,
            content: content.into(),
            tool_call_id: Some(call_id.into()),
            is_error: true,
            tool_calls: Vec::new(),
        }
    }

    /// Preserve the persisted string history shape while carrying typed blocks.
    /// Legacy tool messages remain ordinary strings and decode as one text block.
    pub fn tool_result_blocks(
        call_id: impl Into<String>,
        blocks: Vec<ToolContentBlock>,
        is_error: bool,
    ) -> Self {
        let mut message = Self::tool_result(call_id, encode_tool_blocks(&blocks));
        message.is_error = is_error;
        message
    }

    pub fn tool_blocks(&self) -> Vec<ToolContentBlock> {
        decode_tool_blocks(&self.content).unwrap_or_else(|| {
            vec![ToolContentBlock::Text {
                text: self.content.clone(),
            }]
        })
    }

    pub fn has_typed_tool_blocks(&self) -> bool {
        decode_tool_blocks(&self.content).is_some()
    }

    pub fn visible_tool_text(&self) -> String {
        visible_tool_text(&self.tool_blocks())
    }

    /// Approximate token count using the 4-chars-per-token heuristic.
    pub fn estimated_tokens(&self) -> u64 {
        self.content.len().div_ceil(4) as u64
    }
}

/// A tool call from the assistant.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Raw JSON string of the arguments.
    pub arguments: String,
}

/// Specification for a tool the LLM can call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema string for the tool's input parameters.
    pub parameters_json: String,
}

/// Request to send to an LLM provider.
#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub messages: Vec<ChatMessage>,
    pub model: String,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub tools: Vec<HarnessToolSpec>,
    pub stream: bool,
    pub reasoning_effort: Option<String>,
}

/// Response from an LLM provider (blocking path).
#[derive(Debug, Clone)]
pub struct ChatResponse {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: TokenUsage,
    pub reasoning_content: Option<String>,
    pub stop_reason: Option<String>,
}

/// Token usage from a provider response.
#[derive(Debug, Clone, Default)]
pub struct TokenUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
}

/// A single chunk from a streaming response.
#[derive(Debug, Clone)]
pub struct StreamChunk {
    pub delta_text: String,
    pub tool_call_deltas: Vec<ToolCallDelta>,
    pub is_final: bool,
    pub usage: Option<TokenUsage>,
    pub stop_reason: Option<String>,
}

/// Incremental tool call data from a streaming chunk.
#[derive(Debug, Clone)]
pub struct ToolCallDelta {
    pub index: usize,
    pub id: Option<String>,
    pub name: Option<String>,
    pub arguments_delta: String,
}

/// Result of executing a tool.
#[derive(Debug, Clone)]
pub struct ToolResult {
    pub success: bool,
    pub output: String,
    pub error_msg: Option<String>,
}

impl ToolResult {
    /// Whether tool execution failed. `success` remains the source of truth for
    /// existing tool implementations while exposing the protocol error flag.
    pub fn is_error(&self) -> bool {
        !self.success
    }

    /// Produce a block result without changing existing text-tool constructors.
    pub fn from_blocks(blocks: Vec<ToolContentBlock>, is_error: bool) -> Self {
        Self {
            success: !is_error,
            output: encode_tool_blocks(&blocks),
            error_msg: None,
        }
    }

    pub fn has_typed_blocks(&self) -> bool {
        decode_tool_blocks(&self.output).is_some()
    }
}

fn is_false(value: &bool) -> bool {
    !value
}

/// Per-provider quirk flags for compatible providers.
#[derive(Debug, Clone)]
pub struct ProviderQuirks {
    /// Merge system messages into the first user message as "[System: ...]".
    pub merge_system_into_user: bool,
    /// Disable streaming (broken tool_calls in streaming mode).
    pub disable_streaming: bool,
    /// Cap max_tokens for non-streaming requests.
    pub max_tokens_non_streaming: Option<u32>,
    /// Strip `<think>...</think>` blocks from streaming deltas.
    pub strip_think_tags: bool,
    /// Auth style override (default: Bearer token).
    pub auth_style: AuthStyle,
    /// Whether this provider supports native function-calling tool_calls.
    pub native_tools: bool,
}

impl Default for ProviderQuirks {
    fn default() -> Self {
        Self {
            merge_system_into_user: false,
            disable_streaming: false,
            max_tokens_non_streaming: None,
            strip_think_tags: false,
            auth_style: AuthStyle::Bearer,
            native_tools: false,
        }
    }
}

/// Authentication style for API requests.
#[derive(Debug, Clone, Default)]
pub enum AuthStyle {
    /// `Authorization: Bearer <key>` (default for most providers).
    #[default]
    Bearer,
    /// `x-api-key: <key>` (Anthropic standard keys).
    ApiKeyHeader,
    /// No authentication required (local models).
    None,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[test]
    fn test_chat_message_constructors() {
        let sys = ChatMessage::system("you are helpful");
        assert_eq!(sys.role, MessageRole::System);
        assert_eq!(sys.content, "you are helpful");
        assert!(sys.tool_calls.is_empty());
        assert!(sys.tool_call_id.is_none());

        let user = ChatMessage::user("hello");
        assert_eq!(user.role, MessageRole::User);

        let asst = ChatMessage::assistant("hi there");
        assert_eq!(asst.role, MessageRole::Assistant);

        let tool = ChatMessage::tool_result("call-123", "result data");
        assert_eq!(tool.role, MessageRole::Tool);
        assert_eq!(tool.tool_call_id.as_deref(), Some("call-123"));
        assert_eq!(tool.content, "result data");
        assert!(!tool.is_error);

        let tool_error = ChatMessage::tool_error_result("call-error", "failed");
        assert!(tool_error.is_error);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[test]
    fn test_estimated_tokens() {
        let msg = ChatMessage::user("hello world"); // 11 chars -> (11+3)/4 = 3
        assert_eq!(msg.estimated_tokens(), 3);

        let empty = ChatMessage::user("");
        assert_eq!(empty.estimated_tokens(), 0); // (0+3)/4 = 0 (integer division)
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[test]
    fn test_message_role_serde() {
        let json = serde_json::to_string(&MessageRole::System).unwrap();
        assert_eq!(json, "\"system\"");

        let parsed: MessageRole = serde_json::from_str("\"assistant\"").unwrap();
        assert_eq!(parsed, MessageRole::Assistant);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[test]
    fn test_tool_call_serde() {
        let tc = ToolCall {
            id: "tc-1".into(),
            name: "read_file".into(),
            arguments: r#"{"path":"src/main.rs"}"#.into(),
        };
        let json = serde_json::to_string(&tc).unwrap();
        let parsed: ToolCall = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, "tc-1");
        assert_eq!(parsed.name, "read_file");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[test]
    #[allow(clippy::unwrap_used)]
    fn tool_error_history_round_trips_and_success_flag_stays_omitted() {
        let history = vec![
            ChatMessage::tool_result("call-ok", "ok"),
            ChatMessage::tool_error_result("call-error", "failed"),
        ];
        let json = serde_json::to_value(&history).unwrap();
        assert_eq!(json[0].get("is_error"), None);
        assert_eq!(json[1]["is_error"], true);

        let parsed: Vec<ChatMessage> = serde_json::from_value(json).unwrap();
        assert!(!parsed[0].is_error);
        assert!(parsed[1].is_error);
        assert_eq!(parsed[1].tool_call_id.as_deref(), Some("call-error"));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[test]
    fn typed_tool_blocks_round_trip_in_string_history() {
        let blocks = vec![
            ToolContentBlock::Text {
                text: "preview".into(),
            },
            ToolContentBlock::Image {
                media_type: "image/png".into(),
                data: "aGVsbG8=".into(),
            },
        ];
        let result = ToolResult::from_blocks(blocks.clone(), true);
        assert!(result.is_error());
        assert!(result.has_typed_blocks());
        let message = ChatMessage::tool_result_blocks("call-image", blocks.clone(), true);
        let stored = serde_json::to_value(&message).unwrap();
        assert!(stored["content"].is_string());
        let restored: ChatMessage = serde_json::from_value(stored).unwrap();
        assert!(restored.is_error);
        assert_eq!(restored.tool_blocks(), blocks);
        assert_eq!(restored.visible_tool_text(), "preview\n[image: image/png]");

        let legacy: ChatMessage = serde_json::from_value(serde_json::json!({
            "role": "tool",
            "content": "plain text",
            "tool_call_id": "old",
            "tool_calls": []
        }))
        .unwrap();
        assert_eq!(
            legacy.tool_blocks(),
            vec![ToolContentBlock::Text {
                text: "plain text".into()
            }]
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[test]
    fn test_provider_quirks_default() {
        let q = ProviderQuirks::default();
        assert!(!q.merge_system_into_user);
        assert!(!q.disable_streaming);
        assert!(q.max_tokens_non_streaming.is_none());
        assert!(!q.strip_think_tags);
        assert!(!q.native_tools);
        assert!(matches!(q.auth_style, AuthStyle::Bearer));
    }
}
