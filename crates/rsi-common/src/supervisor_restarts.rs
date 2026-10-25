//! The supervisor's published restart history (#3492).
//!
//! `scripts/rsid-supervisor.sh` allows `deploy_restarts_per_hour + 1` restarts
//! per window for EVERY restart it performs: a deploy's exit 75, a watchdog
//! exit 75 and an unannounced SIGINT/SIGTERM stop. The daemon's deploy
//! admission used to count only deploys, so it could admit an upgrade the
//! supervisor was about to refuse. The supervisor now writes the restart times
//! it holds to `supervisor-restarts` beside the `deploy-restart-budget`
//! snapshot, one atomic file, and the daemon (deploy admission, the health
//! wake) and the TUI read it, so all of them judge ONE budget.
//!
//! Format, one `key=value` per line:
//!
//! ```text
//! limit=3             restarts the supervisor allows per window
//! window=3600         the window in seconds
//! restart=<epoch>     one per restart still in the window (oldest first)
//! backoff_until=<epoch>  present while the supervisor waits out an exhausted budget
//! ```

use std::path::{Path, PathBuf};

/// File name beside the `deploy-restart-budget` snapshot.
pub const FILE_NAME: &str = "supervisor-restarts";
/// The file is a few hundred bytes; refuse to read a runaway one.
const MAX_BYTES: u64 = 16 * 1024;
/// Most restart lines kept (the newest).
const MAX_RESTARTS: usize = 256;

/// What the supervisor published.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupervisorRestarts {
    /// Restarts the supervisor allows per window (deploys, crashes, watchdog).
    pub limit: usize,
    /// The window, in seconds.
    pub window_secs: i64,
    /// Unix seconds of each restart the supervisor recorded, oldest first.
    pub restarts: Vec<i64>,
    /// While set and in the future the supervisor has no daemon running on
    /// purpose: it is backing off an exhausted budget and restarts at this time.
    pub backoff_until: Option<i64>,
}

impl SupervisorRestarts {
    /// Parse the published text; `None` when it carries no usable limit and
    /// window. Unknown and malformed lines are skipped.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let mut limit = None;
        let mut window_secs = None;
        let mut restarts = Vec::new();
        let mut backoff_until = None;
        for line in text.lines() {
            let Some((key, value)) = line.trim().split_once('=') else {
                continue;
            };
            match key {
                "limit" => limit = value.parse::<usize>().ok().filter(|v| (1..=64).contains(v)),
                "window" => {
                    window_secs = value
                        .parse::<i64>()
                        .ok()
                        .filter(|v| (60..=86_400).contains(v));
                }
                "restart" => {
                    if let Ok(at) = value.parse::<i64>() {
                        restarts.push(at);
                    }
                }
                "backoff_until" => backoff_until = value.parse::<i64>().ok(),
                _ => {}
            }
        }
        if restarts.len() > MAX_RESTARTS {
            restarts.drain(..restarts.len() - MAX_RESTARTS);
        }
        restarts.sort_unstable();
        Some(Self {
            limit: limit?,
            window_secs: window_secs?,
            restarts,
            backoff_until,
        })
    }

    /// Read `path`; `None` when it is missing, oversized or unusable (a host
    /// with no supervisor, or one that predates the file).
    #[must_use]
    pub fn read(path: &Path) -> Option<Self> {
        let metadata = std::fs::metadata(path).ok()?;
        if !metadata.is_file() || metadata.len() > MAX_BYTES {
            return None;
        }
        Self::parse(&std::fs::read_to_string(path).ok()?)
    }

    /// The restarts still inside the window at `now` (unix seconds).
    #[must_use]
    pub fn in_window(&self, now: i64) -> Vec<i64> {
        self.restarts
            .iter()
            .copied()
            .filter(|at| now - at < self.window_secs)
            .collect()
    }

    /// True when the supervisor would not restart the daemon at once: the
    /// budget is spent, or it is backing off now.
    #[must_use]
    pub fn exhausted(&self, now: i64) -> bool {
        self.in_window(now).len() >= self.limit || self.backoff_until.is_some_and(|at| at > now)
    }

    /// Restarts a DEPLOY may rely on: the supervisor's limit minus the one
    /// slot it keeps for rollback and watchdog recovery.
    #[must_use]
    pub fn deploy_allowance(&self) -> usize {
        self.limit.saturating_sub(1).max(1)
    }
}

impl SupervisorRestarts {
    /// The operator-facing line for a daemon that is down because the
    /// supervisor is backing off an exhausted budget (#3492); `None` when it
    /// is not backing off at `now`. Names the restart time, so it never goes
    /// stale on screen.
    #[must_use]
    pub fn backoff_notice(&self, now: i64) -> Option<String> {
        let until = self.backoff_until.filter(|until| *until > now)?;
        let at = chrono::DateTime::from_timestamp(until, 0)?.format("%H:%M:%S UTC");
        Some(format!(
            "rsid is down on purpose: its supervisor spent its restart budget ({} restarts in the last hour, limit {}) and restarts the daemon at {at}. No action needed; deploys are refused until the budget frees.",
            self.in_window(now).len(),
            self.limit,
        ))
    }
}

/// The published file beside the `deploy-restart-budget` snapshot.
#[must_use]
pub fn default_path() -> PathBuf {
    crate::identity::data_path(FILE_NAME, FILE_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT: &str =
        "limit=3\nwindow=3600\nrestart=1000\nrestart=1500\nbackoff_until=9000\nnoise\n";

    #[test]
    fn parses_limit_window_restarts_and_backoff() {
        let parsed = SupervisorRestarts::parse(TEXT).unwrap();
        assert_eq!(parsed.limit, 3);
        assert_eq!(parsed.window_secs, 3600);
        assert_eq!(parsed.restarts, vec![1000, 1500]);
        assert_eq!(parsed.backoff_until, Some(9000));
        assert_eq!(parsed.deploy_allowance(), 2);
    }

    #[test]
    fn a_file_without_a_limit_or_window_is_unusable() {
        assert_eq!(SupervisorRestarts::parse("restart=1\n"), None);
        assert_eq!(SupervisorRestarts::parse("limit=3\nrestart=1\n"), None);
        assert_eq!(SupervisorRestarts::parse("limit=0\nwindow=3600\n"), None);
    }

    #[test]
    fn exhaustion_counts_only_the_window_and_the_backoff() {
        let parsed = SupervisorRestarts::parse(
            "limit=3\nwindow=3600\nrestart=100\nrestart=200\nrestart=300\n",
        )
        .unwrap();
        assert!(parsed.exhausted(400));
        // The oldest leaves the window at 3700.
        assert!(!parsed.exhausted(3701));
        let backing_off =
            SupervisorRestarts::parse("limit=3\nwindow=3600\nbackoff_until=500\n").unwrap();
        assert!(backing_off.exhausted(400));
        assert!(!backing_off.exhausted(500));
    }

    #[test]
    fn the_backoff_notice_names_the_restart_time_only_while_backing_off() {
        let parsed =
            SupervisorRestarts::parse("limit=3\nwindow=3600\nrestart=100\nbackoff_until=3600\n")
                .unwrap();
        let notice = parsed.backoff_notice(200).unwrap();
        assert!(
            notice.contains("restarts the daemon at 01:00:00 UTC"),
            "{notice}"
        );
        assert!(
            notice.contains("1 restarts in the last hour, limit 3"),
            "{notice}"
        );
        assert_eq!(parsed.backoff_notice(3600), None);
    }

    #[test]
    fn a_missing_or_oversized_file_reads_as_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(SupervisorRestarts::read(&dir.path().join("absent")), None);
        let big = dir.path().join("big");
        std::fs::write(&big, "x".repeat(20_000)).unwrap();
        assert_eq!(SupervisorRestarts::read(&big), None);
        let ok = dir.path().join("ok");
        std::fs::write(&ok, TEXT).unwrap();
        assert!(SupervisorRestarts::read(&ok).is_some());
    }
}
