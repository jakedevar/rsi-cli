//! Git safety for scratch reclaim (#1140): a candidate must hold neither a
//! dirty tree nor history that exists nowhere else.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::fsys::{Budget, GitRoot};

const GIT_TIMEOUT: Duration = Duration::from_secs(30);
/// How long to wait for git's output after it exited or was killed.
const READER_GRACE: Duration = Duration::from_secs(5);
/// Repositories checked in one candidate before it is retained as unproven.
const MAX_REPOS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum GitVerdict {
    Clean,
    /// Uncommitted or untracked changes.
    Dirty,
    /// Commits, tags, stashes or a detached HEAD no remote-tracking ref has.
    Unpublished,
    /// Git failed, timed out, or the discovery was incomplete.
    Unproven,
}

/// Run git and collect stdout (capped); `None` on failure or timeout. Output
/// is drained while waiting so a large status cannot stall the child.
fn git_output(dir: &Path, bare: bool, args: &[&str], deadline: Instant) -> Option<Vec<u8>> {
    let mut command = Command::new("git");
    if bare {
        command.arg("--git-dir").arg(dir);
    } else {
        command.arg("-C").arg(dir);
    }
    let mut child = command
        .args(["-c", "core.fsmonitor=false"])
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut out = Vec::new();
        let read = stdout.by_ref().take(1 << 20).read_to_end(&mut out);
        // Drain any remainder so the child never blocks on a full pipe.
        let _ = std::io::copy(&mut stdout, &mut std::io::sink());
        let _ = sender.send(read.map(|_| out));
    });
    let limit = (Instant::now() + GIT_TIMEOUT).min(deadline);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < limit => {
                std::thread::sleep(Duration::from_millis(10));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    // The reader ends when the pipe closes; a stray grandchild holding it must
    // not hold the pass, so the wait for the output is bounded too.
    let out = receiver
        .recv_timeout(READER_GRACE.min(deadline.saturating_duration_since(Instant::now())))
        .ok()?
        .ok()?;
    status?.success().then_some(out)
}

/// Whether `porcelain` (`git status --porcelain`) reports anything but, at
/// most, RSI's own untracked creation record. Only that exact line is excused,
/// and only for the repository at the candidate root (#1147): adopting such a
/// repository writes the record into it, which must not make it dirty forever.
/// A tracked, modified, ignored-then-forced or differently named record still
/// counts.
pub(super) fn dirty_apart_from_record(porcelain: &str, at_candidate_root: bool) -> bool {
    let record_line = format!("?? {}", super::RECORD_FILE);
    porcelain
        .lines()
        .any(|line| !(at_candidate_root && line == record_line))
}

/// Check one repository: clean tree (unless bare) and nothing unpublished.
/// `at_candidate_root` is true for the repository that is the candidate itself.
fn check_repo(dir: &Path, bare: bool, at_candidate_root: bool, deadline: Instant) -> GitVerdict {
    if !bare {
        match git_output(dir, false, &["status", "--porcelain"], deadline) {
            Some(out)
                if !dirty_apart_from_record(&String::from_utf8_lossy(&out), at_candidate_root) => {}
            Some(_) => return GitVerdict::Dirty,
            None => return GitVerdict::Unproven,
        }
    }
    // Every ref (branches, tags, stash, notes) and HEAD, minus whatever a
    // remote-tracking ref already has. Anything left exists only here.
    match git_output(
        dir,
        bare,
        &["rev-list", "-n", "1", "--all", "--not", "--remotes"],
        deadline,
    ) {
        Some(out) if out.is_empty() => GitVerdict::Clean,
        Some(_) => GitVerdict::Unpublished,
        None => GitVerdict::Unproven,
    }
}

/// Prove every repository found under `base` (the candidate's address; a
/// `/proc/<pid>/fd/<n>` anchor keeps it pinned to the opened directory).
///
/// `skip` names repositories whose preservation is proved another way (a
/// lander's registered private clone); every other repository is checked.
pub(super) fn check_all(
    base: &Path,
    gits: &[GitRoot],
    skip: &dyn Fn(&GitRoot) -> bool,
    budget: &mut Budget,
) -> GitVerdict {
    let checked: Vec<&GitRoot> = gits.iter().filter(|git| !skip(git)).collect();
    if checked.len() > MAX_REPOS {
        return GitVerdict::Unproven;
    }
    let mut verdict = GitVerdict::Clean;
    for git in checked {
        if budget.expired() {
            return GitVerdict::Unproven;
        }
        let dir: PathBuf = base.join(&git.rel);
        match check_repo(
            &dir,
            git.bare,
            git.rel.as_os_str().is_empty(),
            budget.deadline(),
        ) {
            GitVerdict::Clean => {}
            // Keep looking only to prefer the more specific reason.
            GitVerdict::Dirty => return GitVerdict::Dirty,
            GitVerdict::Unpublished => verdict = GitVerdict::Unpublished,
            GitVerdict::Unproven => return GitVerdict::Unproven,
        }
    }
    verdict
}
