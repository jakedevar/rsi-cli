//! Reusable list-overlay navigation state.
//!
//! Provides `ListOverlayState`, a small utility used by list-based overlays
//! (model selector, theme picker, sort picker, session picker) to centralise
//! j/k/g/G navigation and bounds-checking. New list overlays should compose
//! this type rather than duplicating the pattern.
//!
//! # Usage
//!
//! ```rust,ignore
//! let mut nav = ListOverlayState::new(items.len());
//!
//! // In key handler:
//! match key.code {
//!     KeyCode::Char('j') | KeyCode::Down  => nav.nav_down(),
//!     KeyCode::Char('k') | KeyCode::Up    => nav.nav_up(),
//!     KeyCode::Char('g')                  => nav.jump_top(),
//!     KeyCode::Char('G')                  => nav.jump_bottom(),
//!     _ => {}
//! }
//! let selected = nav.selected;
//! ```

/// Navigation state for a fixed-length list overlay.
///
/// Tracks the selected index with inclusive bounds `[0, len)`.
/// Clamping is always applied so callers never see an out-of-range index.
#[derive(Debug, Clone, Copy, Default)]
pub struct ListOverlayState {
    /// Currently highlighted row (0-indexed).
    pub selected: usize,
    /// Total number of items in the list.
    len: usize,
}

impl ListOverlayState {
    /// Create a new state for a list of `len` items.
    pub fn new(len: usize) -> Self {
        Self { selected: 0, len }
    }

    /// Move selection down by one row. No-op at the last item.
    pub fn nav_down(&mut self) {
        if self.selected + 1 < self.len {
            self.selected += 1;
        }
    }

    /// Move selection up by one row. No-op at the first item.
    pub fn nav_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    /// Jump to the first item (vim `g` / `gg`).
    pub fn jump_top(&mut self) {
        self.selected = 0;
    }

    /// Jump to the last item (vim `G`).
    pub fn jump_bottom(&mut self) {
        if self.len > 0 {
            self.selected = self.len - 1;
        }
    }

    /// Update the item count and clamp selection if needed.
    ///
    /// Call this when the underlying item list grows or shrinks.
    pub fn set_len(&mut self, len: usize) {
        self.len = len;
        if len == 0 {
            self.selected = 0;
        } else if self.selected >= len {
            self.selected = len - 1;
        }
    }

    /// Current list length.
    pub fn len(&self) -> usize {
        self.len
    }

    /// True when the list is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// Try to handle a common list-overlay navigation key (j/k/g/G/Down/Up).
///
/// Operates directly on a `&mut usize` selected index and the list length,
/// so callers don't need to construct a `ListOverlayState`. Returns `true`
/// if the key was consumed.
pub fn handle_list_nav_key(
    selected: &mut usize,
    len: usize,
    key: &crossterm::event::KeyEvent,
) -> bool {
    use crossterm::event::KeyCode;
    match key.code {
        KeyCode::Char('j') | KeyCode::Down => {
            if *selected + 1 < len {
                *selected += 1;
            }
            true
        }
        KeyCode::Char('k') | KeyCode::Up => {
            if *selected > 0 {
                *selected -= 1;
            }
            true
        }
        KeyCode::Char('g') => {
            *selected = 0;
            true
        }
        KeyCode::Char('G') => {
            *selected = len.saturating_sub(1);
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_nav_down_clamps_at_end() {
        let mut s = ListOverlayState::new(3);
        s.nav_down();
        s.nav_down();
        s.nav_down(); // already at 2
        assert_eq!(s.selected, 2);
    }

    #[test]
    fn test_nav_up_clamps_at_zero() {
        let mut s = ListOverlayState::new(3);
        s.nav_up(); // already at 0
        assert_eq!(s.selected, 0);
    }

    #[test]
    fn test_jump_top_and_bottom() {
        let mut s = ListOverlayState::new(5);
        s.jump_bottom();
        assert_eq!(s.selected, 4);
        s.jump_top();
        assert_eq!(s.selected, 0);
    }

    #[test]
    fn test_set_len_clamps_selection() {
        let mut s = ListOverlayState::new(5);
        s.jump_bottom();
        assert_eq!(s.selected, 4);
        s.set_len(3); // shrink — should clamp
        assert_eq!(s.selected, 2);
    }

    #[test]
    fn test_empty_list() {
        let mut s = ListOverlayState::new(0);
        s.nav_down();
        s.nav_up();
        s.jump_bottom();
        assert_eq!(s.selected, 0);
        assert!(s.is_empty());
    }

    #[test]
    fn test_handle_list_nav_key() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let make_key = |code: KeyCode| KeyEvent::new(code, KeyModifiers::NONE);

        let mut sel = 0usize;
        let len = 5;

        // j moves down
        assert!(handle_list_nav_key(
            &mut sel,
            len,
            &make_key(KeyCode::Char('j'))
        ));
        assert_eq!(sel, 1);

        // Down arrow also moves down
        assert!(handle_list_nav_key(&mut sel, len, &make_key(KeyCode::Down)));
        assert_eq!(sel, 2);

        // k moves up
        assert!(handle_list_nav_key(
            &mut sel,
            len,
            &make_key(KeyCode::Char('k'))
        ));
        assert_eq!(sel, 1);

        // G jumps to bottom
        assert!(handle_list_nav_key(
            &mut sel,
            len,
            &make_key(KeyCode::Char('G'))
        ));
        assert_eq!(sel, 4);

        // g jumps to top
        assert!(handle_list_nav_key(
            &mut sel,
            len,
            &make_key(KeyCode::Char('g'))
        ));
        assert_eq!(sel, 0);

        // Unhandled key returns false
        assert!(!handle_list_nav_key(
            &mut sel,
            len,
            &make_key(KeyCode::Enter)
        ));
    }
}
