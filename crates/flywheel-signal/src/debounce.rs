//! Inbound message debouncer.
//!
//! Coalesces rapid-fire messages from the same E.164 into a single dispatch.
//! Messages arriving within `window_ms` (default 500) from the same sender are
//! concatenated with `\n` before dispatch. Prevents RPC amplification on fast
//! typing from the phone.
//!
//! Signal has no chat_identifier distinct from the sender (v1 is DM-only), so
//! the debounce key is simply `msg.sender`.

use crate::signal_cli::InboundMessage;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Debouncer that coalesces rapid messages by sender.
pub struct Debouncer {
    /// Pending messages grouped by debounce key (E.164 sender).
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

        let keys_to_flush: Vec<String> = self
            .pending
            .iter()
            .filter(|(_, entry)| now.duration_since(entry.first_seen) >= self.window)
            .map(|(key, _)| key.clone())
            .collect();

        let mut flushed = Vec::new();
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
    msg.sender.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_msg(sender: &str, text: &str) -> InboundMessage {
        InboundMessage {
            timestamp_ms: 1713542400000,
            sender: sender.to_string(),
            text: text.to_string(),
            is_sync: false,
        }
    }

    #[test]
    fn test_single_message_flush() {
        let mut debouncer = Debouncer::new(0); // 0ms window = flush immediately
        debouncer.push(make_msg("+15551234567", "hello"));

        let flushed = debouncer.flush();
        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].text, "hello");
    }

    #[test]
    fn test_coalesce_multiple_messages() {
        let mut debouncer = Debouncer::new(0);
        debouncer.push(make_msg("+15551234567", "hello"));
        debouncer.push(make_msg("+15551234567", "world"));

        let flushed = debouncer.flush();
        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].text, "hello\nworld");
    }

    #[test]
    fn test_separate_senders() {
        let mut debouncer = Debouncer::new(0);
        debouncer.push(make_msg("+15551234567", "hello"));
        debouncer.push(make_msg("+15559998888", "hi"));

        let flushed = debouncer.flush();
        assert_eq!(flushed.len(), 2);
    }

    #[test]
    fn test_window_not_elapsed() {
        let mut debouncer = Debouncer::new(60_000); // 60s window
        debouncer.push(make_msg("+15551234567", "hello"));

        let flushed = debouncer.flush();
        assert_eq!(flushed.len(), 0);
        assert_eq!(debouncer.pending_count(), 1);
    }

    #[test]
    fn test_drain() {
        let mut debouncer = Debouncer::new(60_000);
        debouncer.push(make_msg("+15551234567", "hello"));
        debouncer.push(make_msg("+15559998888", "hi"));

        let drained = debouncer.drain();
        assert_eq!(drained.len(), 2);
        assert_eq!(debouncer.pending_count(), 0);
    }
}
