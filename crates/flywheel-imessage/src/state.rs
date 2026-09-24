//! Persistent bridge state.
//!
//! Stores `last_seen_rowid` and session-to-chat mappings across restarts.
//! Persisted to `~/.rsi/imessage-state.json`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use uuid::Uuid;

/// Bridge state persisted across restarts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeState {
    /// Last processed message ROWID from chat.db.
    pub last_seen_rowid: i64,

    /// Maps chat_identifier -> session_id for routing plain-text messages.
    #[serde(default)]
    pub session_chat_map: HashMap<String, Uuid>,

    /// Maps chat_identifier -> sender handle (for reverse lookups on push events).
    #[serde(default)]
    pub chat_sender_map: HashMap<String, String>,

    /// File path for persistence. Not serialized.
    #[serde(skip)]
    state_path: PathBuf,
}

impl BridgeState {
    /// Load state from disk. Returns default state if file doesn't exist.
    pub fn load() -> Self {
        let state_path = Self::default_path();
        if state_path.exists() {
            match std::fs::read_to_string(&state_path) {
                Ok(contents) => match serde_json::from_str::<BridgeState>(&contents) {
                    Ok(mut state) => {
                        state.state_path = state_path;
                        tracing::info!(
                            "Loaded bridge state: last_seen_rowid={}, {} chat mappings",
                            state.last_seen_rowid,
                            state.session_chat_map.len()
                        );
                        return state;
                    }
                    Err(e) => {
                        tracing::warn!("Failed to parse bridge state: {}. Starting fresh.", e);
                    }
                },
                Err(e) => {
                    tracing::warn!("Failed to read bridge state: {}. Starting fresh.", e);
                }
            }
        }

        Self {
            last_seen_rowid: 0,
            session_chat_map: HashMap::new(),
            chat_sender_map: HashMap::new(),
            state_path,
        }
    }

    /// Save state to disk.
    pub fn save(&self) {
        match serde_json::to_string_pretty(self) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&self.state_path, json) {
                    tracing::warn!("Failed to save bridge state: {}", e);
                }
            }
            Err(e) => {
                tracing::warn!("Failed to serialize bridge state: {}", e);
            }
        }
    }

    /// Map a chat identifier to a session ID.
    pub fn map_chat_to_session(&mut self, chat_identifier: &str, session_id: Uuid) {
        self.session_chat_map
            .insert(chat_identifier.to_string(), session_id);
    }

    /// Record the sender handle for a chat identifier.
    #[allow(dead_code)]
    pub fn record_sender(&mut self, chat_identifier: &str, sender: &str) {
        self.chat_sender_map
            .insert(chat_identifier.to_string(), sender.to_string());
    }

    /// Get the session ID mapped to a chat identifier.
    pub fn get_session_for_chat(&self, chat_identifier: &str) -> Option<Uuid> {
        self.session_chat_map.get(chat_identifier).copied()
    }

    /// Get the chat identifier mapped to a session ID (reverse lookup).
    pub fn get_chat_for_session(&self, session_id: Uuid) -> Option<String> {
        self.session_chat_map
            .iter()
            .find(|(_, sid)| **sid == session_id)
            .map(|(chat_id, _)| chat_id.clone())
    }

    /// Get the sender handle for a chat identifier.
    pub fn get_sender_for_chat(&self, chat_identifier: &str) -> Option<String> {
        self.chat_sender_map.get(chat_identifier).cloned()
    }

    /// Remove stale mappings for sessions in terminal states.
    #[allow(dead_code)]
    pub fn cleanup_terminal_sessions(&mut self, terminal_session_ids: &[Uuid]) {
        self.session_chat_map
            .retain(|_, sid| !terminal_session_ids.contains(sid));
    }

    /// Create a test instance with a given state path. Not persisted.
    #[cfg(test)]
    pub fn new_test(state_path: PathBuf) -> Self {
        Self {
            last_seen_rowid: 0,
            session_chat_map: HashMap::new(),
            chat_sender_map: HashMap::new(),
            state_path,
        }
    }

    fn default_path() -> PathBuf {
        rsi_common::identity::data_path("imessage-state.json", "imessage-state.json")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_map_chat_to_session() {
        let mut state = BridgeState {
            last_seen_rowid: 0,
            session_chat_map: HashMap::new(),
            chat_sender_map: HashMap::new(),
            state_path: PathBuf::from("/tmp/test-state.json"),
        };

        let session_id = Uuid::new_v4();
        state.map_chat_to_session("iMessage;-;+15551234567", session_id);

        assert_eq!(
            state.get_session_for_chat("iMessage;-;+15551234567"),
            Some(session_id)
        );
        assert_eq!(
            state.get_chat_for_session(session_id),
            Some("iMessage;-;+15551234567".to_string())
        );
    }

    #[test]
    fn test_reverse_lookup_missing() {
        let state = BridgeState {
            last_seen_rowid: 0,
            session_chat_map: HashMap::new(),
            chat_sender_map: HashMap::new(),
            state_path: PathBuf::from("/tmp/test-state.json"),
        };

        assert!(state.get_chat_for_session(Uuid::new_v4()).is_none());
    }

    #[test]
    fn test_sender_tracking() {
        let mut state = BridgeState {
            last_seen_rowid: 0,
            session_chat_map: HashMap::new(),
            chat_sender_map: HashMap::new(),
            state_path: PathBuf::from("/tmp/test-state.json"),
        };

        state.record_sender("iMessage;-;+15551234567", "+15551234567");
        assert_eq!(
            state.get_sender_for_chat("iMessage;-;+15551234567"),
            Some("+15551234567".to_string())
        );
    }

    #[test]
    fn test_cleanup_terminal_sessions() {
        let mut state = BridgeState {
            last_seen_rowid: 0,
            session_chat_map: HashMap::new(),
            chat_sender_map: HashMap::new(),
            state_path: PathBuf::from("/tmp/test-state.json"),
        };

        let active = Uuid::new_v4();
        let terminal = Uuid::new_v4();
        state.map_chat_to_session("chat1", active);
        state.map_chat_to_session("chat2", terminal);

        state.cleanup_terminal_sessions(&[terminal]);

        assert_eq!(state.session_chat_map.len(), 1);
        assert!(state.get_session_for_chat("chat1").is_some());
        assert!(state.get_session_for_chat("chat2").is_none());
    }

    #[test]
    fn test_serde_roundtrip() {
        let mut state = BridgeState {
            last_seen_rowid: 42,
            session_chat_map: HashMap::new(),
            chat_sender_map: HashMap::new(),
            state_path: PathBuf::from("/tmp/test-state.json"),
        };
        state.map_chat_to_session("chat1", Uuid::new_v4());
        state.record_sender("chat1", "+15551234567");

        let json = serde_json::to_string(&state).unwrap();
        let deser: BridgeState = serde_json::from_str(&json).unwrap();

        assert_eq!(deser.last_seen_rowid, 42);
        assert_eq!(deser.session_chat_map.len(), 1);
        assert_eq!(deser.chat_sender_map.len(), 1);
    }
}
