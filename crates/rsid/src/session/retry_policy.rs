//! Centralized session retry policy.

use crate::config::RuntimeConfig;
use rsi_common::types::SessionKind;
#[cfg(test)]
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// Return the default retry budget for a session kind.
///
/// Automatic retries are fail-closed by default for every kind. A positive
/// retry budget must come from explicit persisted/session policy, not the
/// daemon's worker default.
pub(crate) const fn kind_default_max_retries(_kind: SessionKind, _worker_default: u8) -> u8 {
    // Every kind is fail-closed by default; see the doc comment above. Kept as
    // a function (rather than inlining the literal 0 at call sites) so a
    // future kind-specific default has a single place to land.
    0
}

/// Live retry kill-switch.
pub(crate) fn retries_enabled(runtime_config: &RuntimeConfig) -> bool {
    runtime_config.retry_enabled.load(Ordering::Relaxed)
}

/// Runtime default retry budget for a kind when launch params omit
/// `max_retries`.
pub(crate) fn effective_default(runtime_config: &RuntimeConfig, kind: SessionKind) -> u8 {
    if !retries_enabled(runtime_config) {
        return 0;
    }
    let worker_default = runtime_config.retry_max_default.load(Ordering::Relaxed);
    kind_default_max_retries(kind, worker_default)
}

/// True when the current runtime policy allows retry handling for a persisted
/// session budget.
pub(crate) fn session_retries_allowed(
    runtime_config: &RuntimeConfig,
    kind: SessionKind,
    max_retries: Option<u8>,
) -> bool {
    if !retries_enabled(runtime_config) || max_retries.unwrap_or(0) == 0 {
        return false;
    }

    !matches!(kind, SessionKind::Group | SessionKind::Epic)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, RuntimeConfig};

    fn runtime(default: u8) -> Arc<RuntimeConfig> {
        let mut config = Config::default();
        config.retry_max_default = default;
        RuntimeConfig::from_config(&config)
    }

    #[test]
    fn kind_default_max_retries_is_zero_for_every_kind() {
        // Guards against AGENTS.md / code drift: automatic retries are
        // fail-closed for every SessionKind, unconditionally.
        for kind in [
            SessionKind::Standard,
            SessionKind::TaskRabbit,
            SessionKind::Bug,
            SessionKind::Group,
            SessionKind::Epic,
            SessionKind::Story,
            SessionKind::Task,
            SessionKind::Feature,
            SessionKind::Refactor,
            SessionKind::Research,
        ] {
            assert_eq!(kind_default_max_retries(kind, 3), 0, "{kind:?}");
            assert_eq!(kind_default_max_retries(kind, 0), 0, "{kind:?}");
        }
    }

    #[test]
    fn kind_table_disables_interactive_defaults() {
        let runtime = runtime(3);
        assert_eq!(effective_default(&runtime, SessionKind::Story), 0);
        assert_eq!(effective_default(&runtime, SessionKind::Standard), 0);
    }

    #[test]
    fn kind_table_disables_worker_defaults() {
        let runtime = runtime(3);
        for kind in [
            SessionKind::TaskRabbit,
            SessionKind::Task,
            SessionKind::Bug,
            SessionKind::Feature,
            SessionKind::Refactor,
            SessionKind::Research,
        ] {
            assert_eq!(effective_default(&runtime, kind), 0, "{kind:?}");
            assert!(session_retries_allowed(&runtime, kind, Some(3)), "{kind:?}");
        }
    }

    #[test]
    fn disabled_runtime_forces_zero_default() {
        let runtime = runtime(3);
        runtime.retry_enabled.store(false, Ordering::Relaxed);
        assert_eq!(effective_default(&runtime, SessionKind::Task), 0);
        assert!(!session_retries_allowed(
            &runtime,
            SessionKind::Task,
            Some(3)
        ));
    }

    #[test]
    fn explicit_session_budget_can_enable_interactive_retry_when_runtime_allows_it() {
        let runtime = runtime(0);
        runtime.retry_enabled.store(true, Ordering::Relaxed);

        assert!(session_retries_allowed(
            &runtime,
            SessionKind::Story,
            Some(2)
        ));
        assert!(session_retries_allowed(
            &runtime,
            SessionKind::Standard,
            Some(1)
        ));
        assert!(!session_retries_allowed(&runtime, SessionKind::Story, None));
        assert!(!session_retries_allowed(
            &runtime,
            SessionKind::Epic,
            Some(2)
        ));
    }
}
