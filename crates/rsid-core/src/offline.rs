//! Offline daemon mode (#130): one explicit opt-in for tests and isolated
//! harness runs (the desktop e2e `--isolated-write` run) under which `rsid`
//! opens no outbound connection on its own from its first boot.
//!
//! `RSI_OFFLINE=1` (also `true`, `yes`, `on`) in the daemon's environment is
//! read once at the top of `main` ([`init_from_env`]), before any background
//! task runs, and holds for the life of the process. While it is set every
//! provider probe, poll, catalog fetch and credential check short-circuits and
//! reports a typed offline state instead of reaching the network:
//!
//! * the provider-key check classifies `unknown` with detail
//!   [`REASON_OFFLINE_UNCHECKED`] (unchecked, offline) and never refuses;
//! * the Claude/Codex login probe starts no CLI and reads `unknown`;
//! * the provider CLI contract canary is `unchecked` with reason
//!   [`REASON_OFFLINE`];
//! * the Codex live model catalog is the bundled list, degraded
//!   [`REASON_OFFLINE`];
//! * the Codex plan-window poll does not run (its status reports
//!   `poll_secs: 0` and `last_error: "offline"`);
//! * the `aws configure get region` lookup and Bedrock model discovery do not
//!   run.
//!
//! It never changes a default: unset (or any other value) is the normal
//! daemon. Production install scripts never set it.

use std::ffi::OsStr;
use std::sync::OnceLock;

/// The environment variable that switches the daemon offline.
pub const ENV_OFFLINE: &str = "RSI_OFFLINE";
/// Reason code of a probe, poll or catalog fetch skipped because the daemon
/// is offline.
pub const REASON_OFFLINE: &str = "offline";
/// Detail code of a provider-key check skipped offline: the key is unchecked.
pub const REASON_OFFLINE_UNCHECKED: &str = "offline_unchecked";

static OFFLINE: OnceLock<bool> = OnceLock::new();

/// Whether `value` of [`ENV_OFFLINE`] turns offline mode on.
#[must_use]
pub fn flag_enabled(value: Option<&OsStr>) -> bool {
    value
        .and_then(OsStr::to_str)
        .map(str::trim)
        .is_some_and(|value| {
            ["1", "true", "yes", "on"]
                .iter()
                .any(|on| value.eq_ignore_ascii_case(on))
        })
}

/// Read [`ENV_OFFLINE`] once, at daemon start, and return the mode. Later
/// calls (and later changes of the environment) do not change it.
pub fn init_from_env() -> bool {
    *OFFLINE.get_or_init(|| flag_enabled(std::env::var_os(ENV_OFFLINE).as_deref()))
}

/// The daemon is offline: no probe, poll, catalog fetch or credential check
/// may open a network connection. `false` until [`init_from_env`] ran, so a
/// unit test (which never calls it) is never offline by accident.
#[must_use]
pub fn is_offline() -> bool {
    OFFLINE.get().copied().unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_explicit_on_value_enables_offline_mode() {
        for on in ["1", "true", "TRUE", "yes", "On", " 1 "] {
            assert!(flag_enabled(Some(OsStr::new(on))), "{on:?}");
        }
        for off in ["", "0", "false", "no", "off", "offline"] {
            assert!(!flag_enabled(Some(OsStr::new(off))), "{off:?}");
        }
        assert!(!flag_enabled(None));
    }
}
