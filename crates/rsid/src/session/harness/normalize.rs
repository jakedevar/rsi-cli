//! Repair incomplete tool exchanges before sending conversation history to a provider.

use super::types::{ChatMessage, MessageRole};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct RepairCount {
    pub synthesized: usize,
    pub dropped: usize,
}

/// Algorithm adapted from OpenAI Codex CLI (Apache-2.0), codex-rs/core/src/context_manager/normalize.rs @ f5f08c54.
#[allow(clippy::doc_markdown)] // Keep the required upstream attribution verbatim.
pub(super) fn normalize_history(history: &mut Vec<ChatMessage>) -> RepairCount {
    let call_ids: HashSet<String> = history
        .iter()
        .flat_map(|message| message.tool_calls.iter().map(|call| call.id.clone()))
        .collect();
    let mut seen_results = HashSet::new();
    let mut counts = RepairCount::default();
    let mut filtered = Vec::with_capacity(history.len());

    for message in history.drain(..) {
        if message.role == MessageRole::Tool {
            let call_id = message.tool_call_id.as_deref().unwrap_or_default();
            if !call_ids.contains(call_id) {
                tracing::warn!(call_id, "dropping orphan tool result");
                counts.dropped += 1;
                continue;
            }
            if !seen_results.insert(call_id.to_owned()) {
                counts.dropped += 1;
                continue;
            }
        }
        filtered.push(message);
    }

    let mut results: HashMap<String, ChatMessage> = filtered
        .iter()
        .filter(|message| message.role == MessageRole::Tool)
        .filter_map(|message| {
            message
                .tool_call_id
                .as_ref()
                .map(|call_id| (call_id.clone(), message.clone()))
        })
        .collect();
    let mut repaired = Vec::with_capacity(filtered.len());
    for message in filtered {
        if message.role == MessageRole::Tool {
            continue;
        }
        if message.role == MessageRole::Assistant {
            let calls = message.tool_calls.clone();
            repaired.push(message);
            for call in calls {
                if let Some(result) = results.remove(&call.id) {
                    repaired.push(result);
                } else {
                    repaired.push(ChatMessage::tool_error_result(call.id, "aborted"));
                    counts.synthesized += 1;
                }
            }
        } else {
            repaired.push(message);
        }
    }
    *history = repaired;
    counts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::harness::types::ToolCall;

    fn assistant(ids: &[&str]) -> ChatMessage {
        let mut message = ChatMessage::assistant("calling");
        message.tool_calls = ids
            .iter()
            .map(|id| ToolCall {
                id: (*id).into(),
                name: "test".into(),
                arguments: "{}".into(),
            })
            .collect();
        message
    }

    fn signature(history: &[ChatMessage]) -> Vec<(MessageRole, String, Option<String>)> {
        history
            .iter()
            .map(|m| (m.role, m.content.clone(), m.tool_call_id.clone()))
            .collect()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[test]
    fn orphan_call_gets_aborted_result_after_assistant() {
        let mut history = vec![assistant(&["a"]), ChatMessage::user("next")];
        assert_eq!(normalize_history(&mut history).synthesized, 1);
        assert_eq!(history[1].tool_call_id.as_deref(), Some("a"));
        assert_eq!(history[1].content, "aborted");
        assert!(history[1].is_error);
        assert_eq!(history[2].content, "next");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[test]
    fn orphan_result_is_dropped() {
        let mut history = vec![ChatMessage::tool_result("lost", "output")];
        assert_eq!(normalize_history(&mut history).dropped, 1);
        assert!(history.is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[test]
    fn parallel_calls_keep_call_order_with_missing_result() {
        let mut history = vec![
            assistant(&["a", "b"]),
            ChatMessage::tool_result("b", "real"),
        ];
        assert_eq!(normalize_history(&mut history).synthesized, 1);
        assert_eq!(history[1].tool_call_id.as_deref(), Some("a"));
        assert_eq!(history[1].content, "aborted");
        assert_eq!(history[2].tool_call_id.as_deref(), Some("b"));
        assert_eq!(history[2].content, "real");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[test]
    fn duplicate_result_keeps_first() {
        let mut history = vec![
            assistant(&["a"]),
            ChatMessage::tool_result("a", "first"),
            ChatMessage::tool_result("a", "second"),
        ];
        assert_eq!(normalize_history(&mut history).dropped, 1);
        assert_eq!(history.len(), 2);
        assert_eq!(history[1].content, "first");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[test]
    fn normalization_is_idempotent() {
        let mut history = vec![
            assistant(&["a", "b"]),
            ChatMessage::tool_result("b", "real"),
            ChatMessage::tool_result("lost", "orphan"),
        ];
        normalize_history(&mut history);
        let once = signature(&history);
        assert_eq!(normalize_history(&mut history), RepairCount::default());
        assert_eq!(signature(&history), once);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[test]
    fn late_error_result_is_relocated_with_error_flag() {
        let mut result = ChatMessage::tool_error_result("a", "failed");
        result.is_error = true;
        let mut history = vec![assistant(&["a"]), ChatMessage::user("middle"), result];

        normalize_history(&mut history);

        assert_eq!(history[1].tool_call_id.as_deref(), Some("a"));
        assert!(history[1].is_error);
        assert_eq!(history[2].content, "middle");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[test]
    fn mixed_interleaving_moves_late_result_after_call_block() {
        let mut history = vec![
            ChatMessage::system("system"),
            assistant(&["a", "b"]),
            ChatMessage::tool_result("b", "b real"),
            ChatMessage::user("middle"),
            ChatMessage::tool_result("a", "a real"),
            assistant(&["c"]),
            ChatMessage::user("end"),
        ];
        assert_eq!(normalize_history(&mut history).synthesized, 1);
        assert_eq!(history[0].content, "system");
        assert_eq!(history[1].role, MessageRole::Assistant);
        assert_eq!(history[2].tool_call_id.as_deref(), Some("a"));
        assert_eq!(history[2].content, "a real");
        assert_eq!(history[3].tool_call_id.as_deref(), Some("b"));
        assert_eq!(history[3].content, "b real");
        assert_eq!(history[4].content, "middle");
        assert_eq!(history[6].tool_call_id.as_deref(), Some("c"));
        assert_eq!(history[7].content, "end");
    }
}
