//! Inbound message debouncer.
//!
//! Coalesces rapid-fire messages from the same sender in the same chat
//! into a single dispatch. Uses a time-window approach: messages within
//! the debounce window (default 500ms) are concatenated.

use crate::chatdb::InboundMessage;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Debouncer that coalesces rapid messages by sender+chat key.
pub struct Debouncer {
    /// Pending messages grouped by debounce key.
    pending: HashMap<String, PendingEntry>,
    /// Debounce window duration.
    window: Duration,
}

struct PendingEntry {
    /// Accumulated message texts.
    texts: Vec<String>,
    /// Last message received (used as the representative for metadata).
    last_message: InboundMessage,
    /// When the first message in this batch arrived.
    first_seen: Instant,
}

impl Debouncer {
    pub fn new(window_ms: u64) -> Self {
        Self {
            pending: HashMap::new(),
            window: Duration::from_millis(window_ms),
        }
    }

    /// Push a new message into the debouncer.
    pub fn push(&mut self, msg: InboundMessage) {
        let key = debounce_key(&msg);

        let entry = self.pending.entry(key).or_insert_with(|| PendingEntry {
            texts: Vec::new(),
            last_message: msg.clone(),
            first_seen: Instant::now(),
        });

        entry.texts.push(msg.text.clone());
        entry.last_message = msg;
    }

    /// Flush all entries whose debounce window has elapsed.
    /// Returns coalesced messages ready for dispatch.
    pub fn flush(&mut self) -> Vec<InboundMessage> {
        let now = Instant::now();
        let mut flushed = Vec::new();

        let keys_to_flush: Vec<String> = self
            .pending
            .iter()
            .filter(|(_, entry)| now.duration_since(entry.first_seen) >= self.window)
            .map(|(key, _)| key.clone())
            .collect();

        for key in keys_to_flush {
            if let Some(entry) = self.pending.remove(&key) {
                let coalesced_text = entry.texts.join("\n");
                let mut msg = entry.last_message;
                msg.text = coalesced_text;
                flushed.push(msg);
            }
        }

        flushed
    }

    /// Drain all pending entries regardless of window (for shutdown).
    #[allow(dead_code)]
    pub fn drain(&mut self) -> Vec<InboundMessage> {
        let mut flushed = Vec::new();
        for (_, entry) in self.pending.drain() {
            let coalesced_text = entry.texts.join("\n");
            let mut msg = entry.last_message;
            msg.text = coalesced_text;
            flushed.push(msg);
        }
        flushed
    }

    /// Number of pending debounce entries.
    #[allow(dead_code)]
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }
}

fn debounce_key(msg: &InboundMessage) -> String {
    format!(
        "{}:{}",
        msg.chat_identifier.as_deref().unwrap_or(""),
        msg.sender
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn make_msg(sender: &str, text: &str, chat: &str) -> InboundMessage {
        InboundMessage {
            rowid: 1,
            text: text.to_string(),
            sender: sender.to_string(),
            chat_id: Some(1),
            chat_identifier: Some(chat.to_string()),
            is_group: false,
            group_name: None,
            timestamp: Utc::now(),
        }
    }

    #[test]
    fn test_single_message_flush() {
        let mut debouncer = Debouncer::new(0); // 0ms window = flush immediately
        debouncer.push(make_msg("alice", "hello", "chat1"));

        let flushed = debouncer.flush();
        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].text, "hello");
    }

    #[test]
    fn test_coalesce_multiple_messages() {
        let mut debouncer = Debouncer::new(0);
        debouncer.push(make_msg("alice", "hello", "chat1"));
        debouncer.push(make_msg("alice", "world", "chat1"));

        let flushed = debouncer.flush();
        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].text, "hello\nworld");
    }

    #[test]
    fn test_separate_senders() {
        let mut debouncer = Debouncer::new(0);
        debouncer.push(make_msg("alice", "hello", "chat1"));
        debouncer.push(make_msg("bob", "hi", "chat1"));

        let flushed = debouncer.flush();
        assert_eq!(flushed.len(), 2);
    }

    #[test]
    fn test_separate_chats() {
        let mut debouncer = Debouncer::new(0);
        debouncer.push(make_msg("alice", "hello", "chat1"));
        debouncer.push(make_msg("alice", "hi", "chat2"));

        let flushed = debouncer.flush();
        assert_eq!(flushed.len(), 2);
    }

    #[test]
    fn test_window_not_elapsed() {
        let mut debouncer = Debouncer::new(60_000); // 60s window
        debouncer.push(make_msg("alice", "hello", "chat1"));

        let flushed = debouncer.flush();
        assert_eq!(flushed.len(), 0); // Not yet ready
        assert_eq!(debouncer.pending_count(), 1);
    }

    #[test]
    fn test_drain() {
        let mut debouncer = Debouncer::new(60_000);
        debouncer.push(make_msg("alice", "hello", "chat1"));
        debouncer.push(make_msg("bob", "hi", "chat2"));

        let drained = debouncer.drain();
        assert_eq!(drained.len(), 2);
        assert_eq!(debouncer.pending_count(), 0);
    }
}
