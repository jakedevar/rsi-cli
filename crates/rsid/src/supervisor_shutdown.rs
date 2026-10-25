//! Daemon side of the supervisor's shutdown backstop (#2561).
//!
//! `scripts/rsid-supervisor.sh` bounds a daemon shutdown. The daemon tells it
//! that a shutdown began by writing `pid=<pid> reason=<reason>` to the file
//! named in [`SHUTDOWN_MARKER_ENV`] (read once at startup and removed from the
//! environment so no child inherits it, #2594; every other supervisor-internal
//! variable is scrubbed alongside it, #2601); the supervisor then allows the operator setting
//! `supervisor_shutdown_grace_secs`, and past it sends SIGTERM again, then
//! SIGKILL to the daemon's process group, and appends one line to the incident
//! file ([`incident_path`]). The next daemon start reads that file here and
//! records a `forced_shutdown` friction event and a manager notice, so a forced
//! end is never silent.
//!
//! The process-group kill is Unix-only; the daemon has no supervisor on
//! Windows, where nothing writes either file.

use crate::store::Store;
use rsi_common::friction::{FrictionKind, NewFrictionEventV1, is_friction_code};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

/// Environment variable the supervisor sets to the shutdown marker path.
pub const SHUTDOWN_MARKER_ENV: &str = "RSID_SHUTDOWN_MARKER";
/// Prefix of every variable `scripts/rsid-supervisor.sh` reads or exports for
/// its own use (`RSID_SUPERVISOR_PATH`, `_SHUTDOWN_MARKER`, `_SNAPSHOT`,
/// `_RESTART_TIMES`, `_AFTER_RESTART`, `_LAST_FALLBACK` and the tuning knobs).
/// The daemon reads none of them, so all are removed at startup (#2601).
pub const SUPERVISOR_INTERNAL_ENV_PREFIX: &str = "RSID_SUPERVISOR_";
/// Supervisor-internal variables outside [`SUPERVISOR_INTERNAL_ENV_PREFIX`]:
/// the one list of names scrubbed from the daemon's environment so no child
/// inherits them (#2594, #2601). `RSI_LISTEN_FD` and the systemd `LISTEN_*`
/// set are consumed by `inherited_daemon_listener`, which runs later in startup
/// and needs them; they are scrubbed there (#1801), not here.
pub const SUPERVISOR_INTERNAL_ENV: &[&str] = &[SHUTDOWN_MARKER_ENV];
/// File name of the incident log, beside the supervisor's restart budget.
pub const INCIDENT_FILE_NAME: &str = "forced-shutdown-incident";
/// More incident lines than this in one file are a runaway; the rest are
/// dropped (the most recent are kept).
const MAX_INCIDENTS: usize = 16;

static MARKED: AtomicBool = AtomicBool::new(false);
/// The marker path the supervisor named for this process, captured once by
/// [`capture_marker_path`]. Never re-read from the environment: the variable is
/// removed so no child inherits it (#2594).
static MARKER_PATH: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Where the supervisor appends forced-shutdown incidents.
#[must_use]
pub fn incident_path() -> PathBuf {
    rsi_common::identity::data_path(INCIDENT_FILE_NAME, INCIDENT_FILE_NAME)
}

/// Read [`SHUTDOWN_MARKER_ENV`] and remove it from the process environment, so
/// no provider, job unit, lander or test the daemon spawns inherits a path to
/// the real marker (#2594). Returns the path the variable named.
#[must_use]
pub fn take_marker_env() -> Option<PathBuf> {
    let path = std::env::var_os(SHUTDOWN_MARKER_ENV).map(PathBuf::from);
    if path.is_some() {
        // SAFETY: called from `main` before any thread exists (see
        // `capture_marker_path`); the unit test that calls it holds the
        // process-wide `temp_env` lock (#190).
        unsafe { std::env::remove_var(SHUTDOWN_MARKER_ENV) };
    }
    path
}

/// Capture the supervisor's marker path once and scrub it from the
/// environment. Call first in `main`, while the process is single-threaded.
pub fn capture_marker_path() {
    let _ = MARKER_PATH.set(take_marker_env());
    scrub_supervisor_env();
}

/// Remove every supervisor-internal variable ([`SUPERVISOR_INTERNAL_ENV`] and
/// anything under [`SUPERVISOR_INTERNAL_ENV_PREFIX`]) from the process
/// environment, so no worker, job unit, lander, codegraph or test the daemon
/// spawns inherits it (#2601). Call while single-threaded. Does not touch the
/// marker path: read it with [`take_marker_env`] first.
pub fn scrub_supervisor_env() {
    let doomed: Vec<std::ffi::OsString> = std::env::vars_os()
        .map(|(key, _)| key)
        .filter(|key| {
            key.to_str().is_some_and(|name| {
                name.starts_with(SUPERVISOR_INTERNAL_ENV_PREFIX)
                    || SUPERVISOR_INTERNAL_ENV.contains(&name)
            })
        })
        .collect();
    for key in doomed {
        // SAFETY: single-threaded startup, see `capture_marker_path`.
        unsafe { std::env::remove_var(key) };
    }
}

/// Tell the supervisor this daemon began to shut down. Idempotent per process
/// and a no-op without a supervisor (or when [`capture_marker_path`] never saw
/// a path). Never fails the shutdown it reports.
pub fn mark_shutdown_begun(reason: &str) {
    mark_shutdown_begun_from(reason, None);
}

/// [`mark_shutdown_begun`] for a shutdown a signal started: the marker also
/// names the signal's sender (`sender_pid=<pid>`, #3433), which the supervisor
/// echoes when it restarts the daemon after an unannounced stop.
pub fn mark_shutdown_begun_from(reason: &str, sender_pid: Option<i32>) {
    let Some(path) = MARKER_PATH.get().and_then(Option::as_deref) else {
        return;
    };
    if MARKED.swap(true, Ordering::AcqRel) {
        return;
    }
    if let Err(error) = write_marker(path, reason, std::process::id(), sender_pid) {
        tracing::warn!(%error, path = %path.display(), "shutdown marker not written");
    }
}

/// #3433: the friction event for a shutdown signal. Signature
/// `forced_shutdown:signal:<sigint|sigterm>` (the existing shutdown kind, so no
/// new variant of a type half the daemon matches on), evidence `sender:<pid>` (`sender:unknown`
/// when the platform or the kernel gave no sender).
#[must_use]
pub fn shutdown_signal_event(
    signal: &str,
    sender: Option<&crate::signal_sender::SignalSender>,
) -> NewFrictionEventV1 {
    let evidence = sender.map_or_else(|| "unknown".to_string(), |sender| sender.pid.to_string());
    NewFrictionEventV1::new(
        FrictionKind::ForcedShutdown,
        &["signal", &signal.to_ascii_lowercase()],
    )
    .evidence("sender", evidence)
}

/// #3433: the `daemon.log` line naming who sent the shutdown signal.
pub fn log_shutdown_signal(signal: &str, sender: Option<&crate::signal_sender::SignalSender>) {
    tracing::warn!(
        "Shutdown signal received: {}",
        crate::signal_sender::describe(signal, sender)
    );
}

/// #3433: record the shutdown signal as a friction event, so the next
/// unexplained stop names its sender in the friction rollup (the `daemon.log`
/// line comes from [`log_shutdown_signal`]). Bounded: a busy store never delays
/// the shutdown by more than two seconds.
pub async fn record_shutdown_signal(
    store: &rsid_core::store_lock::Mutex<Store>,
    signal: &str,
    sender: Option<&crate::signal_sender::SignalSender>,
) {
    let event = shutdown_signal_event(signal, sender);
    match tokio::time::timeout(std::time::Duration::from_secs(2), store.lock()).await {
        Ok(store) => crate::friction::note_locked(&store, &event),
        Err(_) => tracing::warn!("shutdown signal friction event skipped: store busy"),
    }
}

/// The marker names its writer, `pid=<pid> reason=<reason>`: the supervisor
/// starts its grace clock only for its own child's pid and removes any other
/// (#2594).
fn write_marker(
    path: &Path,
    reason: &str,
    pid: u32,
    sender_pid: Option<i32>,
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let reason = if is_friction_code(reason) {
        reason
    } else {
        "unclassified"
    };
    // Atomic, so the supervisor never reads a half-written marker.
    let staged = path.with_extension("tmp");
    let sender = sender_pid
        .filter(|sender| *sender > 0)
        .map_or_else(String::new, |sender| format!(" sender_pid={sender}"));
    std::fs::write(&staged, format!("pid={pid} reason={reason}{sender}\n"))?;
    std::fs::rename(&staged, path)
}

/// One shutdown the supervisor had to force-end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForcedShutdown {
    /// RFC3339 time of the forced end.
    pub forced_at: String,
    /// `daemon_shutdown` (the daemon began it) or `supervisor_stop` (the
    /// supervisor was told to stop); #3433: `unannounced_signal` (a SIGINT or
    /// SIGTERM no intended-stop path announced ended the daemon, which the
    /// supervisor restarted) or `restart_budget_exhausted` (#3492: the shared
    /// restart budget was spent, so it waited before restarting).
    pub trigger: String,
    /// The grace period that elapsed.
    pub grace_seconds: u64,
    /// The daemon process the supervisor ended.
    pub pid: u64,
}

impl ForcedShutdown {
    /// Stable per-incident key (a replay of the same file is idempotent).
    #[must_use]
    pub fn key(&self) -> String {
        let stamp: String = self
            .forced_at
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .collect();
        format!("{stamp}-{}", self.pid)
    }

    /// What the supervisor did about this incident.
    fn action(&self) -> &'static str {
        match self.trigger.as_str() {
            // #3433: a SIGINT/SIGTERM nobody announced ended the daemon.
            "unannounced_signal" => {
                "daemon stopped by a signal no intended-stop path announced; restarted (sender in daemon.log)"
            }
            "restart_budget_exhausted" => {
                "the supervisor's restart budget (deploys, watchdog exits and signal stops share it) was spent; it waited the budget out (capped) and restarted the daemon instead of exiting; deploys are refused until the window frees"
            }
            _ => "SIGTERM then SIGKILL to the daemon process group, then restart",
        }
    }

    fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "forced_at": self.forced_at,
            "trigger": self.trigger,
            "grace_seconds": self.grace_seconds,
            "pid": self.pid,
            "action": self.action(),
        })
    }
}

/// Parse incident lines (`key=value` pairs); malformed lines are skipped.
#[must_use]
pub fn parse_incidents(text: &str) -> Vec<ForcedShutdown> {
    let mut parsed: Vec<ForcedShutdown> = text
        .lines()
        .filter_map(|line| {
            let mut forced_at = None;
            let mut trigger = None;
            let mut grace = None;
            let mut pid = None;
            for pair in line.split_whitespace() {
                match pair.split_once('=')? {
                    ("forced_at", value) => forced_at = Some(value.to_string()),
                    ("trigger", value) if is_friction_code(value) => {
                        trigger = Some(value.to_string());
                    }
                    ("grace_seconds", value) => grace = value.parse().ok(),
                    ("pid", value) => pid = value.parse().ok(),
                    _ => {}
                }
            }
            Some(ForcedShutdown {
                forced_at: forced_at.filter(|value| value.len() <= 40)?,
                trigger: trigger?,
                grace_seconds: grace?,
                pid: pid?,
            })
        })
        .collect();
    if parsed.len() > MAX_INCIDENTS {
        parsed.drain(..parsed.len() - MAX_INCIDENTS);
    }
    parsed
}

/// Take the incidents out of `path`: the file is renamed first so an incident
/// appended meanwhile is kept for the next start, then removed once read.
///
/// # Errors
/// Any I/O error other than the file not existing.
pub fn take_incidents(path: &Path) -> std::io::Result<Vec<ForcedShutdown>> {
    let taken = path.with_extension("taken");
    match std::fs::rename(path, &taken) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Vec::new());
        }
        Err(error) => return Err(error),
    }
    let text = std::fs::read_to_string(&taken);
    let _ = std::fs::remove_file(&taken);
    Ok(parse_incidents(&text?))
}

/// Record every incident in `path` as a `forced_shutdown` friction event and a
/// manager notice. Returns how many were recorded. Telemetry never fails the
/// start: a store error is logged and the incident is dropped with the file.
pub fn surface_incidents(store: &Store, path: &Path) -> usize {
    let incidents = match take_incidents(path) {
        Ok(incidents) => incidents,
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "forced-shutdown incident file unreadable");
            return 0;
        }
    };
    for incident in &incidents {
        tracing::warn!(
            forced_at = %incident.forced_at,
            trigger = %incident.trigger,
            grace_seconds = incident.grace_seconds,
            pid = incident.pid,
            "The supervisor force-ended the previous daemon's shutdown (SIGKILL after the grace period)"
        );
        let event = NewFrictionEventV1::new(FrictionKind::ForcedShutdown, &[&incident.trigger])
            .evidence("incident", incident.key());
        crate::friction::note_locked(store, &event);
        if let Err(error) =
            store.record_daemon_forced_shutdown_notices(&incident.key(), &incident.json())
        {
            tracing::warn!(%error, "forced-shutdown manager notice not recorded");
        }
    }
    incidents.len()
}

/// macOS (#2936): end a daemon whose `rsid-supervisor.sh` is gone. Under the
/// launchd agent `dev.rsi.rsid` a SIGKILL of the holder (an operator `kill -9`,
/// or launchd past `ExitTimeOut`) makes launchd kill the job's process group,
/// which holds the supervisor but not rsid (the supervisor starts it as its own
/// group leader). The orphaned rsid kept serving the inherited listener, so
/// every holder launchd started next refused the live socket and the agent
/// crashlooped with no supervisor. Linux needs no watch: a systemd unit ends
/// every process in its cgroup. A daemon started under the supervisor checks
/// every 2 s whether its parent changed and then sends itself SIGTERM, the
/// ordinary graceful shutdown; launchd's next holder then takes the socket.
#[cfg(target_os = "macos")]
pub fn spawn_supervisor_parent_watch() {
    let supervisor = std::os::unix::process::parent_id();
    if crate::daemon_info::supervisor_mode().as_deref()
        != Some(rsi_common::agent_daemon_info::DAEMON_SUPERVISOR_SCRIPT)
    {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("rsid-supervisor-watch".into())
        .spawn(move || {
            while std::os::unix::process::parent_id() == supervisor {
                std::thread::sleep(std::time::Duration::from_secs(2));
            }
            tracing::warn!(
                supervisor_pid = supervisor,
                "rsid-supervisor.sh exited while rsid runs; shutting down so the launchd agent restarts the holder chain"
            );
            let _ = nix::sys::signal::kill(nix::unistd::getpid(), nix::sys::signal::Signal::SIGTERM);
        });
    if let Err(error) = spawned {
        tracing::warn!(%error, "supervisor parent watch not started");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn the_marker_names_its_writer_and_is_written_where_told() {
        let dir = std::env::temp_dir().join(format!("rsid-marker-{}", std::process::id()));
        let marker = dir.join("nested").join("shutdown-begun");
        write_marker(&marker, "sigterm", 4242, None).unwrap();
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap(),
            "pid=4242 reason=sigterm\n"
        );
        write_marker(&marker, "not a friction code!", 7, None).unwrap();
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap(),
            "pid=7 reason=unclassified\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #3433: a signal-driven shutdown names its sender in the marker, and the
    /// supervisor's `pid=` match still reads only the daemon's own pid.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn a_signal_shutdown_marker_names_the_sender() {
        let dir = std::env::temp_dir().join(format!("rsid-marker-sender-{}", std::process::id()));
        let marker = dir.join("shutdown-begun");
        write_marker(&marker, "ctrl_c", 4242, Some(777)).unwrap();
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap(),
            "pid=4242 reason=ctrl_c sender_pid=777\n"
        );
        write_marker(&marker, "sigterm", 4242, Some(0)).unwrap();
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap(),
            "pid=4242 reason=sigterm\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn an_unannounced_signal_incident_says_it_was_restarted_not_force_killed() {
        let incidents = parse_incidents(
            "forced_at=2026-10-21T01:54:59Z trigger=unannounced_signal grace_seconds=0 pid=2799079 signal=INT\n\
             forced_at=2026-10-21T02:00:00Z trigger=restart_budget_exhausted grace_seconds=0 pid=5 signal=TERM\n\
             forced_at=2026-10-21T02:10:00Z trigger=daemon_shutdown grace_seconds=120 pid=6 signal=KILL\n",
        );
        assert_eq!(incidents.len(), 3);
        assert!(
            incidents[0].json()["action"]
                .as_str()
                .unwrap()
                .contains("restarted")
        );
        assert!(
            incidents[1].json()["action"]
                .as_str()
                .unwrap()
                .contains("waited the budget out")
        );
        assert!(
            incidents[2].json()["action"]
                .as_str()
                .unwrap()
                .contains("SIGKILL")
        );
    }

    /// #2594: the daemon reads the supervisor's marker path once and removes it
    /// from its environment, so a worker, job unit, lander or test it spawns
    /// can never write the real marker.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn the_marker_variable_is_removed_so_no_child_inherits_it() {
        // `temp_env` holds the one process-wide environment lock and restores
        // the variable afterwards (#190).
        temp_env::with_var(
            SHUTDOWN_MARKER_ENV,
            Some("/nonexistent/shutdown-begun"),
            || {
                assert_eq!(
                    take_marker_env(),
                    Some(PathBuf::from("/nonexistent/shutdown-begun"))
                );
                assert!(std::env::var_os(SHUTDOWN_MARKER_ENV).is_none());
                // The child probe needs the Unix `env` executable; the in-process
                // assertion above holds on every OS (a child inherits the process
                // environment, so its absence there is what a child would see).
                #[cfg(unix)]
                {
                    let output = std::process::Command::new("env").output().unwrap();
                    assert!(
                        !String::from_utf8_lossy(&output.stdout).contains(SHUTDOWN_MARKER_ENV),
                        "a spawned child must not see the marker variable"
                    );
                }
                assert_eq!(take_marker_env(), None);
            },
        );
    }

    /// #2601: every supervisor-internal variable is gone from a child spawned
    /// by each spawn path (direct and through a shell), while `RSI_LISTEN_FD` is left for
    /// `inherited_daemon_listener` to consume.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn supervisor_internal_variables_are_scrubbed_from_every_spawn_path() {
        let names = [
            "RSID_SUPERVISOR_PATH",
            "RSID_SUPERVISOR_SHUTDOWN_MARKER",
            "RSID_SUPERVISOR_SNAPSHOT",
            "RSID_SUPERVISOR_RESTART_TIMES",
            "RSID_SUPERVISOR_AFTER_RESTART",
            "RSID_SUPERVISOR_LAST_FALLBACK",
            "RSID_SUPERVISOR_POLL_SECONDS",
            "RSID_SUPERVISOR_FUTURE_KNOB",
            SHUTDOWN_MARKER_ENV,
        ];
        // The scrub reads the whole process environment, so it runs under the
        // one process-wide environment lock (`temp_env`), which also restores
        // every name afterwards (#190).
        let vars: Vec<(&str, Option<&str>)> = names
            .iter()
            .map(|name| (*name, Some("/nonexistent/supervisor")))
            .chain([("RSI_SCRUB_TEST_KEEP", Some("1"))])
            .collect();
        temp_env::with_vars(vars, || {
            scrub_supervisor_env();
            // Portable, in-process: nothing supervisor-internal is left in the
            // environment any spawn path would inherit, and an unrelated variable
            // is untouched.
            for name in names {
                assert!(
                    std::env::var_os(name).is_none(),
                    "{name} survived the scrub"
                );
            }
            assert!(
                std::env::vars_os().all(|(key, _)| {
                    let key = key.to_string_lossy();
                    let name: &str = &key;
                    !name.starts_with(SUPERVISOR_INTERNAL_ENV_PREFIX)
                        && !SUPERVISOR_INTERNAL_ENV.contains(&name)
                }),
                "a supervisor-internal variable survived the scrub"
            );
            assert_eq!(
                std::env::var_os("RSI_SCRUB_TEST_KEEP"),
                Some(std::ffi::OsString::from("1")),
                "unrelated env kept"
            );
            // Child probes need the Unix `env` and `sh` executables: the direct
            // and shell spawn paths both see the scrubbed environment.
            #[cfg(unix)]
            {
                let std_out = std::process::Command::new("env").output().unwrap();
                let shell_out = std::process::Command::new("sh")
                    .args(["-c", "env"])
                    .output()
                    .unwrap();
                for out in [std_out, shell_out] {
                    let text = String::from_utf8_lossy(&out.stdout);
                    for name in names {
                        assert!(!text.contains(name), "a child inherited {name}");
                    }
                    assert!(text.contains("RSI_SCRUB_TEST_KEEP=1"), "unrelated env kept");
                }
            }
        });
    }

    /// Without a captured path (every unit test, any non-supervised start) a
    /// shutdown writes nothing (the environment is never consulted again).
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn mark_shutdown_begun_without_a_captured_path_writes_nothing() {
        mark_shutdown_begun("sigterm");
        assert!(MARKER_PATH.get().is_none());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn parses_supervisor_incident_lines_and_skips_malformed_ones() {
        let text = "forced_at=2026-10-14T20:27:00Z trigger=daemon_shutdown grace_seconds=120 pid=4242\n\
                    garbage line\n\
                    forced_at=2026-10-14T21:00:00Z trigger=supervisor_stop grace_seconds=60 pid=7\n\
                    forced_at=2026-10-14T21:30:00Z trigger=bad;code grace_seconds=60 pid=8\n";
        let incidents = parse_incidents(text);
        assert_eq!(
            incidents,
            vec![
                ForcedShutdown {
                    forced_at: "2026-10-14T20:27:00Z".into(),
                    trigger: "daemon_shutdown".into(),
                    grace_seconds: 120,
                    pid: 4242,
                },
                ForcedShutdown {
                    forced_at: "2026-10-14T21:00:00Z".into(),
                    trigger: "supervisor_stop".into(),
                    grace_seconds: 60,
                    pid: 7,
                },
            ]
        );
        assert_eq!(incidents[0].key(), "20261014T202700Z-4242");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn take_incidents_consumes_the_file_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(INCIDENT_FILE_NAME);
        assert!(take_incidents(&path).unwrap().is_empty());
        std::fs::write(
            &path,
            "forced_at=2026-10-14T20:27:00Z trigger=daemon_shutdown grace_seconds=120 pid=1\n",
        )
        .unwrap();
        assert_eq!(take_incidents(&path).unwrap().len(), 1);
        assert!(!path.exists());
        assert!(take_incidents(&path).unwrap().is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[tokio::test]
    async fn a_shutdown_signal_is_recorded_with_its_sender() {
        let store = rsid_core::store_lock::Mutex::new(Store::open_in_memory().unwrap());
        let sender = crate::signal_sender::SignalSender {
            pid: 31337,
            uid: 1000,
            comm: Some("kill".into()),
            cgroup: None,
        };
        record_shutdown_signal(&store, "SIGINT", Some(&sender)).await;
        let rollup = store
            .lock()
            .await
            .friction_rollup(
                &rsi_common::friction::ListFrictionRollupRequestV1::default(),
                chrono::Utc::now(),
            )
            .unwrap();
        let row = rollup
            .rows
            .iter()
            .find(|row| row.signature == "forced_shutdown:signal:sigint")
            .unwrap_or_else(|| panic!("{rollup:?}"));
        assert!(
            row.evidence_refs
                .iter()
                .any(|reference| reference == "sender:31337")
        );
        assert_eq!(
            shutdown_signal_event("SIGTERM", None)
                .evidence_ref
                .as_deref(),
            Some("sender:unknown")
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn surfacing_records_a_friction_event_and_consumes_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(INCIDENT_FILE_NAME);
        std::fs::write(
            &path,
            "forced_at=2026-10-14T20:27:00Z trigger=daemon_shutdown grace_seconds=120 pid=4242\n",
        )
        .unwrap();
        let store = Store::open_in_memory().unwrap();
        assert_eq!(surface_incidents(&store, &path), 1);
        assert!(!path.exists());
        let rollup = store
            .friction_rollup(
                &rsi_common::friction::ListFrictionRollupRequestV1::default(),
                chrono::Utc::now(),
            )
            .unwrap();
        assert!(
            rollup
                .rows
                .iter()
                .any(|row| row.signature == "forced_shutdown:daemon_shutdown"),
            "{rollup:?}"
        );
        assert_eq!(surface_incidents(&store, &path), 0);
    }
}
