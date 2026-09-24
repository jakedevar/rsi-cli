//! Repair incomplete tool exchanges before sending conversation history to a provider.

use super::types::{ChatMessage, MessageRole};
use std::collections::HashSet;

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

    let mut repaired = Vec::with_capacity(filtered.len());
    let mut messages = filtered.into_iter().peekable();
    while let Some(message) = messages.next() {
        let missing: Vec<String> = if message.role == MessageRole::Assistant {
            message
                .tool_calls
                .iter()
                .filter(|call| !seen_results.contains(&call.id))
                .map(|call| call.id.clone())
                .collect()
        } else {
            Vec::new()
        };
        repaired.push(message);
        if !missing.is_empty() {
            // Keep all real sibling results in their original order, then append repairs.
            while matches!(messages.peek(), Some(next) if next.role == MessageRole::Tool) {
                if let Some(result) = messages.next() {
                    repaired.push(result);
                }
            }
            for call_id in missing {
                if seen_results.insert(call_id.clone()) {
                    repaired.push(ChatMessage::tool_result(call_id, "aborted"));
                    counts.synthesized += 1;
                }
            }
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

    #[test]
    fn orphan_call_gets_aborted_result_after_assistant() {
        let mut history = vec![assistant(&["a"]), ChatMessage::user("next")];
        assert_eq!(normalize_history(&mut history).synthesized, 1);
        assert_eq!(history[1].tool_call_id.as_deref(), Some("a"));
        assert_eq!(history[1].content, "aborted");
        assert_eq!(history[2].content, "next");
    }

    #[test]
    fn orphan_result_is_dropped() {
        let mut history = vec![ChatMessage::tool_result("lost", "output")];
        assert_eq!(normalize_history(&mut history).dropped, 1);
        assert!(history.is_empty());
    }

    #[test]
    fn parallel_calls_keep_real_result_before_missing_result() {
        let mut history = vec![
            assistant(&["a", "b"]),
            ChatMessage::tool_result("b", "real"),
        ];
        assert_eq!(normalize_history(&mut history).synthesized, 1);
        assert_eq!(history[1].content, "real");
        assert_eq!(history[2].tool_call_id.as_deref(), Some("a"));
        assert_eq!(history[2].content, "aborted");
    }

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

    #[test]
    fn mixed_interleaving_preserves_real_message_order() {
        let mut history = vec![
            ChatMessage::system("system"),
            assistant(&["a", "b"]),
            ChatMessage::tool_result("b", "b real"),
            ChatMessage::user("middle"),
            ChatMessage::tool_result("a", "a real"),
            assistant(&["c"]),
            ChatMessage::user("end"),
        ];
        let original = signature(&history);
        assert_eq!(normalize_history(&mut history).synthesized, 1);
        let real_after: Vec<_> = signature(&history)
            .into_iter()
            .filter(|(_, content, _)| content != "aborted")
            .collect();
        assert_eq!(real_after, original);
        assert_eq!(history[6].tool_call_id.as_deref(), Some("c"));
    }
}
