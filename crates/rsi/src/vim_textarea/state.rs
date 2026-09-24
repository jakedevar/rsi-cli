//! VimState struct and supporting types for vim emulation.

use crossterm::event::KeyEvent;

/// Vim editing state for a text area. Shared between input bar and overlay.
#[derive(Debug, Clone)]
pub struct VimState {
    /// Pending operator ('d', 'c', or 'y') awaiting a motion/textobject.
    pub pending_operator: Option<char>,
    /// Accumulated count prefix (e.g., "3" in "3dw"). None = no count = 1.
    pub pending_count: Option<usize>,
    /// Visual mode state. None = not in visual mode.
    pub visual: Option<VisualMode>,
    /// Anchor position for visual selection (logical row, col).
    pub visual_anchor: Option<(usize, usize)>,
    /// Last f/t/F/T target for ; and , repeat.
    pub last_char_search: Option<CharSearch>,
    /// Recorded last change for dot repeat.
    pub last_change: Option<ChangeRecord>,
    /// Keys captured during the most recent change (for dot repeat recording).
    pub recording_change: Option<ChangeRecording>,
    /// Waiting for second key after 'g' prefix.
    pub pending_g: bool,
    /// Waiting for replacement char after 'r'.
    pub pending_replace: bool,
    /// Waiting for text object type after 'i' or 'a' in operator-pending mode.
    pub pending_textobj_prefix: Option<char>,
    /// Waiting for char search target after f/t/F/T.
    pub pending_char_search_dir: Option<CharSearchDir>,
    /// True when replaying a dot-repeat (suppresses new recording).
    pub replaying: bool,
    /// Snapshot of textarea content when insert mode was entered (for dot repeat).
    pub insert_start_snapshot: Option<String>,
    /// Desired column for vertical movement (vim's curswant).
    /// Set on j/k, preserved across empty lines, cleared on any horizontal motion.
    pub desired_col: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisualMode {
    Char,
    Line,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CharSearch {
    pub direction: CharSearchDir,
    pub ch: char,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CharSearchDir {
    ForwardTo,    // f
    ForwardTill,  // t
    BackwardTo,   // F
    BackwardTill, // T
}

impl CharSearchDir {
    pub fn reverse(self) -> Self {
        match self {
            Self::ForwardTo => Self::BackwardTo,
            Self::ForwardTill => Self::BackwardTill,
            Self::BackwardTo => Self::ForwardTo,
            Self::BackwardTill => Self::ForwardTill,
        }
    }
}

/// A recorded change that can be replayed by dot (.).
#[derive(Debug, Clone)]
pub struct ChangeRecord {
    /// Keys that triggered the change (operator + motion/textobject).
    pub keys: Vec<KeyEvent>,
    /// Text inserted after the operator (if any, e.g., for ciw → type → Esc).
    pub inserted_text: Option<String>,
}

/// In-progress change recording.
#[derive(Debug, Clone)]
pub struct ChangeRecording {
    pub keys: Vec<KeyEvent>,
}

impl Default for VimState {
    #[allow(clippy::derivable_impls)]
    fn default() -> Self {
        Self {
            pending_operator: None,
            pending_count: None,
            visual: None,
            visual_anchor: None,
            last_char_search: None,
            last_change: None,
            recording_change: None,
            pending_g: false,
            pending_replace: false,
            pending_textobj_prefix: None,
            pending_char_search_dir: None,
            replaying: false,
            insert_start_snapshot: None,
            desired_col: None,
        }
    }
}

impl VimState {
    /// Reset transient state (called on Esc, overlay close, input bar unfocus).
    pub fn reset_transient(&mut self) {
        self.pending_operator = None;
        self.pending_count = None;
        self.visual = None;
        self.visual_anchor = None;
        self.recording_change = None;
        self.pending_g = false;
        self.pending_replace = false;
        self.pending_textobj_prefix = None;
        self.pending_char_search_dir = None;
        self.desired_col = None;
    }

    /// Start recording a change for dot repeat.
    pub(super) fn start_recording(&mut self, key: KeyEvent) {
        if self.replaying {
            return;
        }
        self.recording_change = Some(ChangeRecording { keys: vec![key] });
    }

    /// Append a key to the current change recording.
    pub(super) fn record_key(&mut self, key: KeyEvent) {
        if self.replaying {
            return;
        }
        if let Some(ref mut rec) = self.recording_change {
            rec.keys.push(key);
        }
    }

    /// Finalize a non-insert change (operator completed without entering insert).
    pub(super) fn finalize_change(&mut self) {
        if self.replaying {
            return;
        }
        if let Some(rec) = self.recording_change.take() {
            self.last_change = Some(ChangeRecord {
                keys: rec.keys,
                inserted_text: None,
            });
        }
    }

    /// Call when entering insert mode. Saves textarea content for diff computation.
    pub fn snapshot_for_insert(&mut self, content: &str) {
        if !self.replaying {
            self.insert_start_snapshot = Some(content.to_string());
        }
    }

    /// Call when exiting insert mode. Computes what text was inserted by diffing
    /// against the snapshot, then finalizes dot-repeat recording.
    pub fn finalize_insert_from_snapshot(&mut self, current_content: &str) {
        let inserted = if let Some(ref snapshot) = self.insert_start_snapshot {
            // Simple diff: find the new text that wasn't in the snapshot
            if current_content.len() > snapshot.len() {
                // Find common prefix and suffix
                let prefix_len = snapshot
                    .chars()
                    .zip(current_content.chars())
                    .take_while(|(a, b)| a == b)
                    .count();
                let s_suffix: Vec<char> = snapshot.chars().skip(prefix_len).collect();
                let c_suffix: Vec<char> = current_content.chars().skip(prefix_len).collect();
                let suffix_len = s_suffix
                    .iter()
                    .rev()
                    .zip(c_suffix.iter().rev())
                    .take_while(|(a, b)| a == b)
                    .count();
                let inserted_chars: String = c_suffix[..c_suffix.len().saturating_sub(suffix_len)]
                    .iter()
                    .collect();
                inserted_chars
            } else {
                String::new()
            }
        } else {
            String::new()
        };
        self.insert_start_snapshot = None;
        self.finalize_insert(inserted);
    }

    /// Call when exiting insert mode. Finalizes dot-repeat recording
    /// with the text that was typed during insert.
    pub fn finalize_insert(&mut self, inserted_text: String) {
        if self.replaying {
            return;
        }
        if let Some(rec) = self.recording_change.take() {
            self.last_change = Some(ChangeRecord {
                keys: rec.keys,
                inserted_text: if inserted_text.is_empty() {
                    None
                } else {
                    Some(inserted_text)
                },
            });
        }
    }
}

/// Result of processing a key in vim normal mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VimAction {
    /// Key was consumed; no mode change.
    Consumed,
    /// Key caused a transition to insert mode (caller should set mode + update suggestions).
    EnteredInsert,
    /// Key was not recognized by the shared handler (caller should handle or ignore).
    Unhandled,
}
