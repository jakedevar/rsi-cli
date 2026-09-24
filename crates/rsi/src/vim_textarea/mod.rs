//! Vim emulation for tui_textarea::TextArea.
//!
//! Submodules handle distinct vim concerns:
//! - `state`     — VimState struct and mode tracking
//! - `normal`    — Normal mode key dispatch
//! - `visual`    — Visual mode selection and actions
//! - `operators` — Operator-pending mode (d, c, y, etc.)
//! - `motions`   — Cursor movement primitives and char search

mod motions;
mod normal;
mod operators;
mod state;
mod visual;

#[cfg(test)]
mod tests;

// Re-export public API
pub use normal::{FileEditorCtx, handle_vim_normal};
pub use state::{
    ChangeRecord, ChangeRecording, CharSearch, CharSearchDir, VimAction, VimState, VisualMode,
};
// Re-export bracket/cursor helpers for file viewer integration.
pub use motions::{
    find_matching_bracket_pos, is_bracket, move_cursor_to, move_vertical_with_curswant,
};
