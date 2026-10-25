//! Running-TUI daemon recovery (#3433).
//!
//! A fresh TUI starts the daemon at launch, but a TUI that is already running
//! used to retry the missing socket forever and never started one: the hub sat
//! dark for 43 minutes after an unattributed stop. [`DaemonRecovery`] gives the
//! reconnect path the same behaviour as launch. Once the socket has stayed
//! unreachable past [`DAEMON_RECOVERY_GRACE`] it asks the [`DaemonLauncher`]
//! (the binary installs the very function the startup auto-start uses, so unit
//! naming and supervision are identical) to start the daemon, bounded to
//! [`DAEMON_RECOVERY_MAX_ATTEMPTS`] launches [`DAEMON_RECOVERY_COOLDOWN`] apart.
//! When auto-start is off, unsupported, failed or exhausted, it exposes a
//! persistent banner naming the exact command to run, so the outage is never a
//! silent retry loop. It is pure state with injected time; the `App` owns the
//! I/O.

use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long the socket must stay unreachable before the TUI acts.
pub(crate) const DAEMON_RECOVERY_GRACE: Duration = Duration::from_secs(10);
/// Minimum spacing between two launches during one outage.
pub(crate) const DAEMON_RECOVERY_COOLDOWN: Duration = Duration::from_secs(30);
/// Launches attempted during one outage before only the banner remains.
pub(crate) const DAEMON_RECOVERY_MAX_ATTEMPTS: u32 = 3;

/// Starts the daemon the way the TUI does at launch. Blocking: callers run it
/// on a blocking thread.
pub trait DaemonLauncher: Send + Sync {
    /// Start (or confirm) the daemon.
    ///
    /// # Errors
    /// The launcher's own message when the daemon could not be started.
    fn start(&self) -> Result<(), String>;
}

/// Outage state of the daemon socket as seen by the running TUI.
pub struct DaemonRecovery {
    launcher: Option<Arc<dyn DaemonLauncher>>,
    manual_command: String,
    grace: Duration,
    cooldown: Duration,
    down_since: Option<Instant>,
    in_flight: bool,
    attempts: u32,
    last_attempt_at: Option<Instant>,
    last_error: Option<String>,
    /// #3492: set while the supervisor deliberately keeps the daemon down to
    /// wait out an exhausted restart budget; the banner then says so instead of
    /// telling the operator to start a daemon that is about to restart itself.
    supervisor_notice: Option<String>,
}

impl Default for DaemonRecovery {
    fn default() -> Self {
        Self {
            launcher: None,
            manual_command: "rsid".to_string(),
            grace: DAEMON_RECOVERY_GRACE,
            cooldown: DAEMON_RECOVERY_COOLDOWN,
            down_since: None,
            in_flight: false,
            attempts: 0,
            last_attempt_at: None,
            last_error: None,
            supervisor_notice: None,
        }
    }
}

impl DaemonRecovery {
    /// Install the launcher (`None` when auto-start is off) and the exact
    /// command an operator would run by hand.
    pub fn configure(&mut self, launcher: Option<Arc<dyn DaemonLauncher>>, manual_command: String) {
        self.launcher = launcher;
        self.manual_command = manual_command;
    }

    pub(crate) fn launcher(&self) -> Option<Arc<dyn DaemonLauncher>> {
        self.launcher.clone()
    }

    #[cfg(test)]
    pub(crate) fn set_timing(&mut self, grace: Duration, cooldown: Duration) {
        self.grace = grace;
        self.cooldown = cooldown;
    }

    #[cfg(test)]
    pub(crate) fn attempts(&self) -> u32 {
        self.attempts
    }

    /// A handshake or transport failure at `now`. Returns true when the caller
    /// must start the launcher now (and then report [`Self::launch_finished`]).
    pub(crate) fn note_unreachable(&mut self, now: Instant) -> bool {
        let since = *self.down_since.get_or_insert(now);
        if self.launcher.is_none()
            || self.in_flight
            || self.attempts >= DAEMON_RECOVERY_MAX_ATTEMPTS
            || now.saturating_duration_since(since) < self.grace
        {
            return false;
        }
        if self
            .last_attempt_at
            .is_some_and(|at| now.saturating_duration_since(at) < self.cooldown)
        {
            return false;
        }
        self.in_flight = true;
        self.attempts += 1;
        self.last_attempt_at = Some(now);
        true
    }

    /// The launcher returned.
    pub(crate) fn launch_finished(&mut self, result: Result<(), String>) {
        self.in_flight = false;
        self.last_error = result.err();
    }

    /// Record what the supervisor published about its restart budget (#3492).
    pub(crate) fn note_supervisor_backoff(&mut self, notice: Option<String>) {
        self.supervisor_notice = notice;
    }

    /// The daemon answered: the outage is over.
    pub(crate) fn note_connected(&mut self) {
        self.supervisor_notice = None;
        self.down_since = None;
        self.in_flight = false;
        self.attempts = 0;
        self.last_attempt_at = None;
        self.last_error = None;
    }

    /// The persistent banner, once the outage outlasts the grace and the TUI
    /// is not (or no longer) fixing it by itself.
    pub(crate) fn banner(&self, now: Instant) -> Option<String> {
        let since = self.down_since?;
        if now.saturating_duration_since(since) < self.grace || self.in_flight {
            return None;
        }
        if let Some(notice) = &self.supervisor_notice {
            return Some(notice.clone());
        }
        let command = &self.manual_command;
        let reason = if self.launcher.is_none() {
            "auto-start is off".to_string()
        } else if let Some(error) = &self.last_error {
            format!("auto-start failed: {error}")
        } else if self.attempts >= DAEMON_RECOVERY_MAX_ATTEMPTS {
            format!("auto-start tried {} times", self.attempts)
        } else {
            return None;
        };
        Some(format!(
            "rsid is not reachable ({reason}). Start it with: {command}"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Nop;
    impl DaemonLauncher for Nop {
        fn start(&self) -> Result<(), String> {
            Ok(())
        }
    }

    fn recovery(with_launcher: bool) -> DaemonRecovery {
        let mut recovery = DaemonRecovery::default();
        recovery.configure(
            with_launcher.then(|| Arc::new(Nop) as Arc<dyn DaemonLauncher>),
            "systemctl --user start rsid.service".into(),
        );
        recovery
    }

    #[test]
    fn nothing_happens_inside_the_grace() {
        let mut recovery = recovery(true);
        let t0 = Instant::now();
        assert!(!recovery.note_unreachable(t0));
        assert!(!recovery.note_unreachable(t0 + Duration::from_secs(9)));
        assert_eq!(recovery.banner(t0 + Duration::from_secs(9)), None);
    }

    #[test]
    fn launch_is_requested_once_after_the_grace_then_spaced_and_bounded() {
        let mut recovery = recovery(true);
        let t0 = Instant::now();
        assert!(!recovery.note_unreachable(t0));
        assert!(recovery.note_unreachable(t0 + Duration::from_secs(10)));
        // In flight: no second launch, no banner yet.
        assert!(!recovery.note_unreachable(t0 + Duration::from_secs(12)));
        assert_eq!(recovery.banner(t0 + Duration::from_secs(12)), None);
        recovery.launch_finished(Ok(()));
        // Cooldown.
        assert!(!recovery.note_unreachable(t0 + Duration::from_secs(20)));
        assert!(recovery.note_unreachable(t0 + Duration::from_secs(41)));
        recovery.launch_finished(Ok(()));
        assert!(recovery.note_unreachable(t0 + Duration::from_secs(80)));
        recovery.launch_finished(Ok(()));
        // Exhausted: only the banner remains.
        assert!(!recovery.note_unreachable(t0 + Duration::from_secs(200)));
        assert_eq!(recovery.attempts(), DAEMON_RECOVERY_MAX_ATTEMPTS);
        let banner = recovery.banner(t0 + Duration::from_secs(200)).unwrap();
        assert!(
            banner.contains("systemctl --user start rsid.service"),
            "{banner}"
        );
    }

    #[test]
    fn failed_launch_and_auto_start_off_both_show_the_exact_command() {
        let t0 = Instant::now();
        let mut failed = recovery(true);
        assert!(!failed.note_unreachable(t0));
        assert!(failed.note_unreachable(t0 + Duration::from_secs(10)));
        failed.launch_finished(Err("no rsid binary".into()));
        let banner = failed.banner(t0 + Duration::from_secs(11)).unwrap();
        assert!(banner.contains("no rsid binary"), "{banner}");
        assert!(
            banner.contains("systemctl --user start rsid.service"),
            "{banner}"
        );

        let mut off = recovery(false);
        assert!(!off.note_unreachable(t0));
        assert!(!off.note_unreachable(t0 + Duration::from_secs(10)));
        let banner = off.banner(t0 + Duration::from_secs(10)).unwrap();
        assert!(banner.contains("auto-start is off"), "{banner}");
        assert!(
            banner.contains("systemctl --user start rsid.service"),
            "{banner}"
        );
    }

    #[test]
    fn a_supervisor_backoff_replaces_the_start_command_and_clears_on_reconnect() {
        let t0 = Instant::now();
        let mut recovery = recovery(true);
        assert!(!recovery.note_unreachable(t0));
        assert!(recovery.note_unreachable(t0 + Duration::from_secs(10)));
        recovery.launch_finished(Ok(()));
        assert_eq!(recovery.banner(t0 + Duration::from_secs(12)), None);
        recovery.note_supervisor_backoff(Some(
            "rsid is down on purpose: restarts at 01:00:00 UTC".into(),
        ));
        let banner = recovery.banner(t0 + Duration::from_secs(12)).unwrap();
        assert!(banner.contains("restarts at 01:00:00 UTC"), "{banner}");
        recovery.note_connected();
        assert_eq!(recovery.banner(t0 + Duration::from_secs(60)), None);
    }

    #[test]
    fn reconnecting_clears_the_outage() {
        let mut recovery = recovery(false);
        let t0 = Instant::now();
        let _ = recovery.note_unreachable(t0);
        assert!(recovery.banner(t0 + Duration::from_secs(10)).is_some());
        recovery.note_connected();
        assert_eq!(recovery.banner(t0 + Duration::from_secs(60)), None);
        assert!(!recovery.note_unreachable(t0 + Duration::from_secs(61)));
    }
}
