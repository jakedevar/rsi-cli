//! The OpenRouter engine a launch actually took, and why (#3141).
//!
//! OpenRouter work is meant to run on RSI's Harness. The provider-spawn
//! chokepoint resolves the route (`api_route.openrouter`, a per-model override
//! or the built-in default), runs the Harness preflight, and notes here what it
//! chose for the session. The session monitor takes the note on the run's first
//! event and records it as a `System` row
//! (`metadata.provider_health.kind = "api_route_selected"`,
//! `provider_stream_health::api_route_event`), so a benchmark can assert from
//! the transcript that every session ran Harness-only: `engine` is the process
//! that really started, `configured_route` and `source` say which operator
//! choice (or the default) selected it, and `fallback_cause` is set only when
//! the operator's opt-in `api_route.fallback.openrouter` turned a failed
//! preflight into a Codex CLI launch.
//!
//! The state is in memory (like `provider_launch_overrides::note_applied`); a
//! note nobody takes is dropped once [`MAX_PENDING`] are waiting.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use uuid::Uuid;

const MAX_PENDING: usize = 1024;

/// The `provider_health.kind` of the System row.
pub const ROUTE_SELECTED_KIND: &str = "api_route_selected";

/// One launch's effective OpenRouter route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteSelection {
    /// The process that started: `harness` or `codex_cli`.
    pub engine: &'static str,
    /// The route the settings resolved to: `harness` or `codex_cli`.
    pub configured_route: &'static str,
    /// Where the route came from: `default`, `global_setting` or `model_override`.
    pub source: &'static str,
    /// The preflight cause that the operator's opt-in fallback overrode.
    pub fallback_cause: Option<&'static str>,
}

impl RouteSelection {
    /// The one-line row text.
    #[must_use]
    pub fn describe(&self) -> String {
        match self.fallback_cause {
            Some(cause) => format!(
                "OpenRouter engine: {} (configured {} from {}; Harness preflight failed: {cause}, operator fallback opt-in)",
                self.engine, self.configured_route, self.source
            ),
            None => format!(
                "OpenRouter engine: {} (configured {} from {})",
                self.engine, self.configured_route, self.source
            ),
        }
    }
}

fn pending() -> &'static Mutex<HashMap<Uuid, RouteSelection>> {
    static PENDING: OnceLock<Mutex<HashMap<Uuid, RouteSelection>>> = OnceLock::new();
    PENDING.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record the route a launch of `session_id` took; the latest launch wins.
pub fn note_selected(session_id: Uuid, selection: RouteSelection) {
    let mut pending = pending()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if pending.len() >= MAX_PENDING && !pending.contains_key(&session_id) {
        pending.clear();
    }
    pending.insert(session_id, selection);
}

/// Take the route the latest launch of `session_id` took, if any.
#[must_use]
pub fn take_selected(session_id: Uuid) -> Option<RouteSelection> {
    pending()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&session_id)
}

#[cfg(test)]
mod tests {
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn a_note_is_taken_once_and_describes_the_engine_and_its_source() {
        let session = Uuid::new_v4();
        assert_eq!(take_selected(session), None);
        let harness = RouteSelection {
            engine: "harness",
            configured_route: "harness",
            source: "default",
            fallback_cause: None,
        };
        note_selected(session, harness.clone());
        assert_eq!(take_selected(session), Some(harness.clone()));
        assert_eq!(take_selected(session), None);
        assert_eq!(
            harness.describe(),
            "OpenRouter engine: harness (configured harness from default)"
        );
        let fallback = RouteSelection {
            engine: "codex_cli",
            configured_route: "harness",
            source: "global_setting",
            fallback_cause: Some("missing_credential"),
        };
        assert!(fallback.describe().contains("missing_credential"));
    }
}
