//! Rolling publication record for issue #1018 phase 2.
//!
//! The record is the host-local reflog of `refs/remotes/origin/rolling`: every
//! fetch (or lander push) that moves the tip appends one entry with the observed
//! time. The reader pairs each entry with its predecessor and counts the
//! first-parent steps between the two tips, so a fast-forward publication of N
//! commits is N landings at the moment it was observed, whatever the commits'
//! committer times are. No schema is involved.

use chrono::{DateTime, Utc};
use std::path::Path;
use std::process::Command;

const ROLLING_REF: &str = "refs/remotes/origin/rolling";

/// One first-parent step a publication added to `origin/rolling`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PublishedStep {
    pub sha: String,
    pub committer_time: DateTime<Utc>,
}

/// One observed movement of the rolling tip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Publication {
    pub observed_at: DateTime<Utc>,
    pub new_tip: String,
    pub steps: Vec<PublishedStep>,
}

fn git_output(repo: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .current_dir(repo)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

/// Publications observed in `[from, to)`, oldest first. `None` when the
/// repository or its rolling reflog cannot be read (the count is unknown, not
/// zero).
pub(crate) fn read_publications(
    repo: &Path,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Option<Vec<Publication>> {
    let reflog = git_output(
        repo,
        &[
            "reflog",
            "show",
            "--date=unix",
            "--format=%gd %H",
            ROLLING_REF,
        ],
    )?;
    // Newest first: the entry after a line is that line's old tip.
    let entries: Vec<(DateTime<Utc>, &str)> = reflog
        .lines()
        .filter_map(|line| {
            let (selector, sha) = line.split_once(' ')?;
            let seconds = selector.split_once("@{")?.1.strip_suffix('}')?;
            let observed = DateTime::from_timestamp(seconds.parse().ok()?, 0)?;
            Some((observed, sha))
        })
        .collect();
    let mut publications = Vec::new();
    for (index, (observed_at, new_tip)) in entries.iter().enumerate() {
        if *observed_at < from || *observed_at >= to {
            continue;
        }
        let Some((_, old_tip)) = entries.get(index + 1) else {
            continue;
        };
        if old_tip == new_tip {
            continue;
        }
        let range = format!("{old_tip}..{new_tip}");
        let log = git_output(
            repo,
            &["log", "--first-parent", "--format=%H %ct", range.as_str()],
        )?;
        let steps = log
            .lines()
            .filter_map(|line| {
                let (sha, seconds) = line.split_once(' ')?;
                Some(PublishedStep {
                    sha: sha.to_string(),
                    committer_time: DateTime::from_timestamp(seconds.parse().ok()?, 0)?,
                })
            })
            .collect();
        publications.push(Publication {
            observed_at: *observed_at,
            new_tip: (*new_tip).to_string(),
            steps,
        });
    }
    publications.reverse();
    Some(publications)
}

/// Nearest-rank percentile (`p` in `0.0..=1.0`) of `samples`; `None` when empty.
pub(crate) fn percentile(samples: &[f64], p: f64) -> Option<f64> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(|left, right| left.total_cmp(right));
    let rank = (p * sorted.len() as f64).ceil() as usize;
    sorted.get(rank.saturating_sub(1)).copied()
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub(crate) fn git(repo: &Path, date: &str, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(repo)
            .env("GIT_COMMITTER_DATE", date)
            .env("GIT_AUTHOR_DATE", date)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?} failed");
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    pub(crate) fn commit(repo: &Path, date: &str, name: &str) -> String {
        std::fs::write(repo.join(name), name).unwrap();
        git(repo, date, &["add", name]);
        git(repo, date, &["commit", "-q", "-m", name]);
        git(repo, date, &["rev-parse", "HEAD"])
    }

    /// Commits authored in 2020, published (fetched) on 2026-09-28: the
    /// first-parent committer-time count of 2026-09-28 is zero while the
    /// reflog records two publications of 2 and 1 steps.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
    #[test]
    fn fast_forward_publications_count_steps_by_observation_time() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        git(repo, "1577836800 +0000", &["init", "-q", "-b", "rolling"]);
        let base = commit(repo, "1577836800 +0000", "base");
        git(
            repo,
            "1790586000 +0000", // 2026-09-28T09:00:00Z
            &["update-ref", "--create-reflog", ROLLING_REF, &base],
        );
        commit(repo, "1577836900 +0000", "one");
        let two = commit(repo, "1577837000 +0000", "two");
        git(
            repo,
            "1790589600 +0000", // 2026-09-28T10:00:00Z
            &["update-ref", ROLLING_REF, &two],
        );
        let three = commit(repo, "1577837100 +0000", "three");
        git(
            repo,
            "1790683200 +0000", // 2026-09-29T12:00:00Z
            &["update-ref", ROLLING_REF, &three],
        );

        let day_start = DateTime::parse_from_rfc3339("2026-09-28T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let day_end = day_start + chrono::Duration::days(1);
        let day = read_publications(repo, day_start, day_end).unwrap();
        assert_eq!(day.len(), 1);
        assert_eq!(day[0].new_tip, two);
        assert_eq!(day[0].steps.len(), 2);
        assert!(
            day[0]
                .steps
                .iter()
                .all(|step| step.committer_time < day_start),
            "committer-time counting would see none of these steps"
        );

        let both = read_publications(repo, day_start, day_end + chrono::Duration::days(1)).unwrap();
        assert_eq!(
            both.iter().map(|p| p.steps.len()).collect::<Vec<_>>(),
            vec![2, 1]
        );
        // The window is exclusive at `to`.
        let exact = DateTime::from_timestamp(1790589600, 0).unwrap();
        assert!(
            read_publications(repo, day_start, exact)
                .unwrap()
                .is_empty()
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
    #[test]
    fn unreadable_repository_is_unknown_not_zero() {
        let dir = tempfile::tempdir().unwrap();
        // A repository that never fetched `origin/rolling` has no record.
        git(
            dir.path(),
            "1577836800 +0000",
            &["init", "-q", "-b", "rolling"],
        );
        let now = Utc::now();
        assert_eq!(
            read_publications(dir.path(), now - chrono::Duration::days(1), now),
            None
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
    #[test]
    fn percentile_uses_nearest_rank() {
        assert_eq!(percentile(&[], 0.5), None);
        let samples = [40.0, 10.0, 30.0, 20.0];
        assert_eq!(percentile(&samples, 0.5), Some(20.0));
        assert_eq!(percentile(&samples, 0.9), Some(40.0));
    }
}
