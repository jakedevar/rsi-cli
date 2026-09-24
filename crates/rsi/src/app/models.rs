//! Model discovery operations for App.

use super::App;

impl App {
    /// Get available models (dynamic if discovered, static as fallback).
    pub fn get_available_models(&self) -> &[(String, String)] {
        &self.available_models
    }
}
