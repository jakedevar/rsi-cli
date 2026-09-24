//! Core types for the agent harness LLM conversation protocol.
//!
//! These map to the intersection of Anthropic's Messages API and OpenAI's
//! Chat Completions API. Provider implementations translate to/from these.

// Provider layer is built ahead of resolve_provider() wiring (Phase 6+).
#![allow(dead_code)]

use serde::{Deserialize, Serialize};

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
#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub role: MessageRole,
    pub content: String,
    /// For tool result messages: the ID of the tool call this responds to.
    pub tool_call_id: Option<String>,
    /// Tool calls requested by the assistant (populated when role == Assistant).
    pub tool_calls: Vec<ToolCall>,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::System,
            content: content.into(),
            tool_call_id: None,
            tool_calls: Vec::new(),
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::User,
            content: content.into(),
            tool_call_id: None,
            tool_calls: Vec::new(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Assistant,
            content: content.into(),
            tool_call_id: None,
            tool_calls: Vec::new(),
        }
    }

    pub fn tool_result(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Tool,
            content: content.into(),
            tool_call_id: Some(call_id.into()),
            tool_calls: Vec::new(),
        }
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
    }

    #[test]
    fn test_estimated_tokens() {
        let msg = ChatMessage::user("hello world"); // 11 chars -> (11+3)/4 = 3
        assert_eq!(msg.estimated_tokens(), 3);

        let empty = ChatMessage::user("");
        assert_eq!(empty.estimated_tokens(), 0); // (0+3)/4 = 0 (integer division)
    }

    #[test]
    fn test_message_role_serde() {
        let json = serde_json::to_string(&MessageRole::System).unwrap();
        assert_eq!(json, "\"system\"");

        let parsed: MessageRole = serde_json::from_str("\"assistant\"").unwrap();
        assert_eq!(parsed, MessageRole::Assistant);
    }

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
