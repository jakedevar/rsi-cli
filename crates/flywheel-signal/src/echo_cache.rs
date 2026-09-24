//! Echo detection for sent messages.
//!
//! When we send a Signal message via `signal-cli send`, our own `syncMessage`
//! envelope comes back from the server on the next `receive` tick. The echo
//! cache remembers recently sent messages (keyed by `"{recipient E.164}:{text}"`)
//! and filters them out.
//!
//! Keeping the user's legitimate desktop-Signal replies (also `syncMessage`
//! envelopes) flowing through the pipeline is load-bearing: the parser emits
//! both kinds, and only this cache distinguishes them.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Cache of recently sent messages for echo detection.
#[derive(Debug, Clone)]
pub struct SentMessageCache {
    /// Map of `"{chat_id_or_sender}:{text_trimmed}"` → expiry instant.
    entries: HashMap<String, Instant>,
    /// TTL for cache entries.
    ttl: Duration,
}

impl SentMessageCache {
    pub fn new(ttl_ms: u64) -> Self {
        Self {
            entries: HashMap::new(),
            ttl: Duration::from_millis(ttl_ms),
        }
    }

    /// Remember that we sent a message to a specific chat.
    /// Also performs lazy cleanup of expired entries.
    pub fn remember(&mut self, chat_id: &str, text: &str) {
        let key = make_key(chat_id, text);
        let expiry = Instant::now() + self.ttl;
        self.entries.insert(key, expiry);

        // Lazy cleanup: remove expired entries
        let now = Instant::now();
        self.entries.retain(|_, exp| *exp > now);
    }

    /// Check if a message is likely an echo of something we sent.
    pub fn is_echo(&self, chat_id: &str, text: &str) -> bool {
        let key = make_key(chat_id, text);
        match self.entries.get(&key) {
            Some(expiry) => Instant::now() < *expiry,
            None => false,
        }
    }

    /// Number of entries in the cache (for diagnostics).
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the cache is empty.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

fn make_key(chat_id: &str, text: &str) -> String {
    // Use trimmed text to handle whitespace variations
    format!("{}:{}", chat_id, text.trim())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn test_remember_and_detect() {
        let mut cache = SentMessageCache::new(5000);
        cache.remember("+15551234567", "Hello world");
        assert!(cache.is_echo("+15551234567", "Hello world"));
        assert!(!cache.is_echo("+15551234567", "Different message"));
        assert!(!cache.is_echo("+15550000000", "Hello world")); // different chat
    }

    #[test]
    fn test_trimmed_matching() {
        let mut cache = SentMessageCache::new(5000);
        cache.remember("+15551234567", "  Hello  ");
        assert!(cache.is_echo("+15551234567", "  Hello  "));
        assert!(cache.is_echo("+15551234567", "Hello")); // trimmed matches
    }

    #[test]
    fn test_expiry() {
        let mut cache = SentMessageCache::new(50); // 50ms TTL
        cache.remember("+15551234567", "Hello");
        assert!(cache.is_echo("+15551234567", "Hello"));

        thread::sleep(Duration::from_millis(100));
        assert!(!cache.is_echo("+15551234567", "Hello"));
    }

    #[test]
    fn test_lazy_cleanup() {
        let mut cache = SentMessageCache::new(50);
        cache.remember("+15551234567", "msg1");
        cache.remember("+15551234567", "msg2");
        assert_eq!(cache.len(), 2);

        thread::sleep(Duration::from_millis(100));

        // Cleanup happens on next remember
        cache.remember("+15551234567", "msg3");
        assert_eq!(cache.len(), 1); // only msg3 remains
    }

    #[test]
    fn test_empty_cache() {
        let cache = SentMessageCache::new(5000);
        assert!(cache.is_empty());
        assert!(!cache.is_echo("+15551234567", "Hello"));
    }
}
