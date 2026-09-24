//! Persistent bridge state.
//!
//! Stores `last_seen_timestamp` (millis since Unix epoch) and the active
//! E.164 → session_id map across restarts. Persisted to
//! `~/.rsi/signal-state.json`.
//!
//! Signal DMs collapse the iMessage chat_identifier / sender distinction: the
//! sender's E.164 IS the conversation key. Consequently the state carries a
//! single `active_sessions: HashMap<String /* E.164 */, Uuid>` instead of the
//! two-map split used by `flywheel-imessage`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use uuid::Uuid;

/// Bridge state persisted across restarts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeState {
    /// Last processed `envelope.timestamp` (millis since Unix epoch).
    #[serde(default)]
    pub last_seen_timestamp: u64,

    /// Map E.164 → active session_id for that chat.
    #[serde(default)]
    pub active_sessions: HashMap<String, Uuid>,

    /// File path for persistence. Not serialized.
    #[serde(skip)]
    state_path: PathBuf,
}

impl BridgeState {
    /// Load state from disk. Returns default state if file doesn't exist.
    pub fn load() -> Self {
        let state_path = Self::default_path();
        Self::load_from(state_path)
    }

    /// Load state from an explicit path (for tests).
    pub fn load_from(state_path: PathBuf) -> Self {
        if state_path.exists() {
            match std::fs::read_to_string(&state_path) {
                Ok(contents) => match serde_json::from_str::<BridgeState>(&contents) {
                    Ok(mut state) => {
                        state.state_path = state_path;
                        tracing::info!(
                            "Loaded bridge state: last_seen_timestamp={}, {} active sessions",
                            state.last_seen_timestamp,
                            state.active_sessions.len()
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
            last_seen_timestamp: 0,
            active_sessions: HashMap::new(),
            state_path,
        }
    }

    /// Save state to disk. Creates the parent directory (`~/.rsi/`) if it
    /// doesn't exist — this is the crate-controlled write path, so it's the
    /// right place to centralize the guarantee.
    pub fn save(&self) {
        if let Some(parent) = self.state_path.parent()
            && !parent.as_os_str().is_empty()
            && !parent.exists()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            tracing::warn!(
                "Failed to create state parent dir {}: {}",
                parent.display(),
                e
            );
            return;
        }
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

    /// Record an E.164 → session_id binding.
    pub fn map_sender_to_session(&mut self, sender: &str, session_id: Uuid) {
        self.active_sessions.insert(sender.to_string(), session_id);
    }

    /// Get the session ID bound to an E.164, if any.
    pub fn get_session_for_sender(&self, sender: &str) -> Option<Uuid> {
        self.active_sessions.get(sender).copied()
    }

    /// Reverse lookup: find the E.164 whose active session is the given ID.
    pub fn get_sender_for_session(&self, session_id: Uuid) -> Option<String> {
        self.active_sessions
            .iter()
            .find(|(_, sid)| **sid == session_id)
            .map(|(sender, _)| sender.clone())
    }

    /// Clear the active session binding for an E.164 (e.g. after interrupt).
    #[allow(dead_code)]
    pub fn clear_session_for_sender(&mut self, sender: &str) {
        self.active_sessions.remove(sender);
    }

    /// Remove bindings whose session is in a terminal state.
    #[allow(dead_code)]
    pub fn cleanup_terminal_sessions(&mut self, terminal_session_ids: &[Uuid]) {
        self.active_sessions
            .retain(|_, sid| !terminal_session_ids.contains(sid));
    }

    /// Create a test instance with a given state path. Not persisted until `save()`.
    #[cfg(test)]
    pub fn new_test(state_path: PathBuf) -> Self {
        Self {
            last_seen_timestamp: 0,
            active_sessions: HashMap::new(),
            state_path,
        }
    }

    fn default_path() -> PathBuf {
        rsi_common::identity::data_path("signal-state.json", "signal-state.json")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with(path: PathBuf) -> BridgeState {
        BridgeState {
            last_seen_timestamp: 0,
            active_sessions: HashMap::new(),
            state_path: path,
        }
    }

    #[test]
    fn test_map_and_get_session() {
        let mut state = state_with(PathBuf::from("/tmp/test-sig-state.json"));
        let session_id = Uuid::new_v4();
        state.map_sender_to_session("+15551234567", session_id);
        assert_eq!(
            state.get_session_for_sender("+15551234567"),
            Some(session_id)
        );
    }

    #[test]
    fn test_reverse_lookup() {
        let mut state = state_with(PathBuf::from("/tmp/test-sig-state.json"));
        let session_id = Uuid::new_v4();
        state.map_sender_to_session("+15551234567", session_id);
        assert_eq!(
            state.get_sender_for_session(session_id),
            Some("+15551234567".to_string())
        );
    }

    #[test]
    fn test_reverse_lookup_missing() {
        let state = state_with(PathBuf::from("/tmp/test-sig-state.json"));
        assert!(state.get_sender_for_session(Uuid::new_v4()).is_none());
    }

    #[test]
    fn test_clear_session_for_sender() {
        let mut state = state_with(PathBuf::from("/tmp/test-sig-state.json"));
        state.map_sender_to_session("+15551234567", Uuid::new_v4());
        state.clear_session_for_sender("+15551234567");
        assert!(state.get_session_for_sender("+15551234567").is_none());
    }

    #[test]
    fn test_cleanup_terminal_sessions() {
        let mut state = state_with(PathBuf::from("/tmp/test-sig-state.json"));
        let active = Uuid::new_v4();
        let terminal = Uuid::new_v4();
        state.map_sender_to_session("+15551234567", active);
        state.map_sender_to_session("+15559998888", terminal);

        state.cleanup_terminal_sessions(&[terminal]);

        assert_eq!(state.active_sessions.len(), 1);
        assert!(state.get_session_for_sender("+15551234567").is_some());
        assert!(state.get_session_for_sender("+15559998888").is_none());
    }

    #[test]
    fn test_serde_roundtrip() {
        let mut state = state_with(PathBuf::from("/tmp/test-sig-state.json"));
        state.last_seen_timestamp = 1713542400000;
        state.map_sender_to_session("+15551234567", Uuid::new_v4());

        let json = serde_json::to_string(&state).unwrap();
        let deser: BridgeState = serde_json::from_str(&json).unwrap();

        assert_eq!(deser.last_seen_timestamp, 1713542400000);
        assert_eq!(deser.active_sessions.len(), 1);
        // state_path skipped, deserializes to default
        assert_eq!(deser.state_path, PathBuf::new());
    }

    #[test]
    fn test_save_creates_parent_directory() {
        // Use a unique nested dir under /tmp so we don't collide with concurrent tests
        let uniq = format!("flywheel-signal-test-{}", Uuid::new_v4());
        let dir = std::env::temp_dir().join(uniq).join("nested");
        let file = dir.join("signal-state.json");
        assert!(!dir.exists(), "precondition: dir should not exist");

        let mut state = state_with(file.clone());
        state.last_seen_timestamp = 42;
        state.save();

        assert!(dir.exists(), "save() should have created parent dir");
        assert!(file.exists(), "save() should have written the file");

        // Round-trip the saved file
        let loaded = BridgeState::load_from(file.clone());
        assert_eq!(loaded.last_seen_timestamp, 42);

        // Cleanup
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }
}
