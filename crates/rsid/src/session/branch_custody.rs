//! #2320: the turn-end check that a sandbox is still on its assigned branch.
//!
//! AGENTS.md hard rule 2 forbids changing a sandbox's branch. The Harness
//! shell and the CLI provider hooks refuse the commands that do it
//! (`rsi_common::custody_scan`), but both are guard rails read from command
//! text: a quoting accident (a backticked example evaluated by the shell,
//! 2026-10-13) or a script they do not follow still gets through. A sandbox
//! left on `main` also blocks `git checkout main` for everyone else, so the
//! daemon verifies the outcome when a turn ends. If HEAD is not the branch the
//! daemon assigned it puts the branch back when that cannot lose anything (no
//! tracked changes, no merge, rebase or similar in progress); otherwise it
//! leaves the tree alone and says so loudly (fail visible, #2307).

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use uuid::Uuid;

use crate::sandbox::git_worktree::{
    BranchRestoreOutcome, TreeAfterKill, observe_head_branch_bounded, observe_tracked_clean_idle,
    restore_branch_bounded,
};

/// How long the restoring `git switch` may run before it is killed and the
/// drift is reported instead (#2541). Turn-end finalization waits for it.
const RESTORE_TIMEOUT: Duration = Duration::from_secs(15);

/// What the check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BranchCheck {
    /// HEAD is the assigned branch.
    OnBranch,
    /// HEAD was elsewhere and the daemon switched back.
    Restored { was: String },
    /// HEAD is elsewhere and was left alone, for `reason`.
    Drifted { was: String, reason: String },
}

fn describe(head: Option<&str>) -> String {
    match head {
        Some(reference) => reference
            .strip_prefix("refs/heads/")
            .unwrap_or(reference)
            .to_owned(),
        None => "a detached HEAD".to_owned(),
    }
}

/// Compare the sandbox at `root` with `assigned` (a branch name without
/// `refs/heads/`) and restore it when that is safe. `None` when git cannot
/// tell (the root is gone or not a repository): nothing to report.
pub(crate) fn reconcile(root: &Path, assigned: &str) -> Option<BranchCheck> {
    reconcile_within(root, assigned, RESTORE_TIMEOUT)
}

fn reconcile_within(root: &Path, assigned: &str, timeout: Duration) -> Option<BranchCheck> {
    let head = observe_head_branch_bounded(root).ok()?;
    if head.as_deref() == Some(&format!("refs/heads/{assigned}")) {
        return Some(BranchCheck::OnBranch);
    }
    let was = describe(head.as_deref());
    // Tracked changes would be carried onto the wrong branch or refused, and
    // an unfinished merge/rebase/cherry-pick/bisect owns HEAD until it ends.
    match observe_tracked_clean_idle(root) {
        Ok(true) => {}
        Ok(false) => {
            return Some(BranchCheck::Drifted {
                was,
                reason: "the worktree has uncommitted tracked changes or an unfinished \
                         merge, rebase, cherry-pick or bisect"
                    .to_owned(),
            });
        }
        Err(error) => {
            return Some(BranchCheck::Drifted {
                was,
                reason: format!("the worktree state could not be read: {error}"),
            });
        }
    }
    // Ignored and untracked paths are protected by the switch itself
    // (`--no-overwrite-ignore`; #2539): a collision is a refusal reported as a
    // drift, never an overwrite. The bounded runner kills a stalled Git and its
    // helpers at `timeout` (#2541).
    match restore_branch_bounded(root, assigned, timeout) {
        Ok(BranchRestoreOutcome::Switched) => Some(BranchCheck::Restored { was }),
        Ok(BranchRestoreOutcome::Refused(stderr)) => Some(BranchCheck::Drifted {
            was,
            reason: format!("`git switch {assigned}` was refused: {stderr}"),
        }),
        Ok(BranchRestoreOutcome::Interrupted { error, tree }) => {
            let state = match tree {
                TreeAfterKill::Untouched => {
                    "Git left no lock and the worktree status is what it was before".to_owned()
                }
                TreeAfterKill::Recovered => "Git was killed while it may have been partly through \
                     switching, so the worktree may have been partially switched; the daemon \
                     removed the index.lock Git left and put the touched tracked files back, and \
                     the worktree status is what it was before"
                    .to_owned(),
                TreeAfterKill::MaybePartial(why) => format!(
                    "the worktree may be partially switched and an index.lock may remain \
                     ({why}); inspect `git status` and `.git/index.lock` before committing"
                ),
            };
            Some(BranchCheck::Drifted {
                was,
                reason: format!("`git switch {assigned}` did not complete: {error}; {state}"),
            })
        }
        Err(error) => Some(BranchCheck::Drifted {
            was,
            reason: format!("`git switch {assigned}` could not run: {error}"),
        }),
    }
}

/// The operator-visible notice for a restore or a drift; `None` for
/// [`BranchCheck::OnBranch`].
pub(crate) fn notice(session_id: Uuid, assigned: &str, check: &BranchCheck) -> Option<String> {
    match check {
        BranchCheck::OnBranch => None,
        BranchCheck::Restored { was } => Some(format!(
            "sandbox custody (#2320): session {session_id} ended a turn on `{was}`; its assigned \
             branch is `{assigned}` (AGENTS.md hard rule 2). The worktree was clean, so the \
             daemon switched it back."
        )),
        BranchCheck::Drifted { was, reason } => Some(format!(
            "sandbox custody (#2320): session {session_id} ended a turn on `{was}`; its assigned \
             branch is `{assigned}` (AGENTS.md hard rule 2). It was NOT switched back because \
             {reason}. Until it is, the branch it is on cannot be checked out anywhere else. \
             Commit or discard the work you made and run `git switch {assigned}` in the sandbox."
        )),
    }
}

type ActiveMap =
    Arc<tokio::sync::RwLock<std::collections::HashMap<Uuid, super::types::TrackedSession>>>;
type BoxFut<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// At a turn's end: verify (and when safe restore) the session's sandbox
/// branch, and publish the notice. Boxed so the monitor future stays small.
#[inline(never)]
pub(crate) fn check_turn_end_boxed<'a>(
    active: &'a ActiveMap,
    session_id: Uuid,
    event_bus: &'a crate::bus::EventBus,
) -> BoxFut<'a, ()> {
    Box::pin(async move {
        let (root, assigned) = {
            let guard = active.read().await;
            let Some(tracked) = guard.get(&session_id) else {
                return;
            };
            let (Some(root), Some(branch)) = (
                tracked.session.sandbox_root.clone(),
                tracked.session.sandbox_branch.clone(),
            ) else {
                return;
            };
            (root, branch)
        };
        let check = {
            let root = root.clone();
            let assigned = assigned.clone();
            tokio::task::spawn_blocking(move || reconcile(Path::new(&root), &assigned))
                .await
                .ok()
                .flatten()
        };
        let Some(check) = check else {
            return;
        };
        let Some(message) = notice(session_id, &assigned, &check) else {
            return;
        };
        match check {
            BranchCheck::Drifted { .. } => tracing::error!(%session_id, "{message}"),
            _ => tracing::warn!(%session_id, "{message}"),
        }
        event_bus.publish(crate::bus::DaemonEvent::SystemMessage {
            level: if matches!(check, BranchCheck::Drifted { .. }) {
                "error"
            } else {
                "warn"
            }
            .to_owned(),
            message,
        });
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .expect("git runs");
        assert!(status.success(), "git {args:?}");
    }

    fn rev(dir: &Path, name: &str) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["rev-parse", name])
            .output()
            .expect("git rev-parse");
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    fn repo_on(branch: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q", "-b", "main"]);
        std::fs::write(dir.path().join("a.txt"), "one").unwrap();
        git(dir.path(), &["add", "a.txt"]);
        git(dir.path(), &["commit", "-q", "-m", "init"]);
        git(dir.path(), &["branch", branch]);
        git(dir.path(), &["switch", "-q", branch]);
        dir
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[test]
    fn a_sandbox_on_its_assigned_branch_is_left_alone() {
        let dir = repo_on("rsi/assigned");
        assert_eq!(
            reconcile(dir.path(), "rsi/assigned"),
            Some(BranchCheck::OnBranch)
        );
        assert_eq!(
            notice(Uuid::nil(), "rsi/assigned", &BranchCheck::OnBranch),
            None
        );
    }

    /// The incident: a clean sandbox ends its turn on `main`; the daemon puts
    /// it back on the assigned branch.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[test]
    fn a_clean_sandbox_on_another_branch_is_switched_back() {
        let dir = repo_on("rsi/assigned");
        git(dir.path(), &["switch", "-q", "main"]);
        let check = reconcile(dir.path(), "rsi/assigned").expect("checked");
        assert_eq!(check, BranchCheck::Restored { was: "main".into() });
        assert_eq!(
            observe_head_branch_bounded(dir.path()).unwrap().as_deref(),
            Some("refs/heads/rsi/assigned")
        );
        let text = notice(Uuid::nil(), "rsi/assigned", &check).expect("notice");
        assert!(
            text.contains("`main`") && text.contains("switched it back"),
            "{text}"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[test]
    fn a_detached_head_is_switched_back_too() {
        let dir = repo_on("rsi/assigned");
        git(dir.path(), &["switch", "-q", "--detach"]);
        let check = reconcile(dir.path(), "rsi/assigned").expect("checked");
        assert_eq!(
            check,
            BranchCheck::Restored {
                was: "a detached HEAD".into()
            }
        );
    }

    /// With tracked changes the daemon does not touch the tree: the drift is
    /// reported with the reason and the work stays where it is.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[test]
    fn a_dirty_sandbox_on_another_branch_is_reported_not_moved() {
        let dir = repo_on("rsi/assigned");
        git(dir.path(), &["switch", "-q", "main"]);
        std::fs::write(dir.path().join("a.txt"), "edited").unwrap();
        let check = reconcile(dir.path(), "rsi/assigned").expect("checked");
        let BranchCheck::Drifted { was, reason } = &check else {
            panic!("expected a drift report, got {check:?}");
        };
        assert_eq!(was, "main");
        assert!(reason.contains("uncommitted"), "{reason}");
        assert_eq!(
            observe_head_branch_bounded(dir.path()).unwrap().as_deref(),
            Some("refs/heads/main")
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "edited"
        );
        let text = notice(Uuid::nil(), "rsi/assigned", &check).expect("notice");
        assert!(text.contains("NOT switched back"), "{text}");
    }

    /// `main` is held by another worktree (the incident's side effect): the
    /// restore of a branch that is checked out elsewhere is refused by git
    /// and reported, never forced.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[test]
    fn a_branch_checked_out_elsewhere_is_reported_not_forced() {
        let dir = repo_on("rsi/assigned");
        git(dir.path(), &["switch", "-q", "main"]);
        let other = tempfile::tempdir().unwrap();
        let other_path = other.path().join("wt");
        git(
            dir.path(),
            &[
                "worktree",
                "add",
                "-q",
                other_path.to_str().unwrap(),
                "rsi/assigned",
            ],
        );
        let check = reconcile(dir.path(), "rsi/assigned").expect("checked");
        let BranchCheck::Drifted { reason, .. } = &check else {
            panic!("expected a drift report, got {check:?}");
        };
        assert!(reason.contains("git switch rsi/assigned"), "{reason}");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[test]
    fn a_missing_root_reports_nothing() {
        assert_eq!(
            reconcile(Path::new("/nonexistent/rsi-sandbox-2320"), "rsi/assigned"),
            None
        );
    }

    /// #2539: the drifted branch ignores `local.txt` (with local work in it)
    /// while the assigned branch tracks a different `local.txt`. The switch
    /// would overwrite the ignored file; it must be refused and reported, with
    /// the file's bytes and HEAD untouched.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[test]
    fn an_ignored_file_the_restore_would_overwrite_blocks_it() {
        let dir = repo_on("rsi/assigned");
        std::fs::write(dir.path().join("local.txt"), "tracked on assigned").unwrap();
        git(dir.path(), &["add", "local.txt"]);
        git(dir.path(), &["commit", "-q", "-m", "track local"]);
        git(dir.path(), &["switch", "-q", "main"]);
        std::fs::write(dir.path().join(".gitignore"), "local.txt\n").unwrap();
        git(dir.path(), &["add", ".gitignore"]);
        git(dir.path(), &["commit", "-q", "-m", "ignore local"]);
        std::fs::write(dir.path().join("local.txt"), "precious ignored work").unwrap();
        let head_before = observe_head_branch_bounded(dir.path()).unwrap();
        let check = reconcile(dir.path(), "rsi/assigned").expect("checked");
        let BranchCheck::Drifted { was, reason } = &check else {
            panic!("expected a drift report, got {check:?}");
        };
        assert_eq!(was, "main");
        assert!(reason.contains("local.txt"), "{reason}");
        assert_eq!(
            std::fs::read(dir.path().join("local.txt")).unwrap(),
            b"precious ignored work"
        );
        assert_eq!(
            observe_head_branch_bounded(dir.path()).unwrap(),
            head_before
        );
    }

    /// An untracked file the restore would overwrite is protected too.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[test]
    fn an_untracked_file_the_restore_would_overwrite_blocks_it() {
        let dir = repo_on("rsi/assigned");
        std::fs::write(dir.path().join("local.txt"), "tracked on assigned").unwrap();
        git(dir.path(), &["add", "local.txt"]);
        git(dir.path(), &["commit", "-q", "-m", "track local"]);
        git(dir.path(), &["switch", "-q", "main"]);
        std::fs::write(dir.path().join("local.txt"), "precious untracked work").unwrap();
        let check = reconcile(dir.path(), "rsi/assigned").expect("checked");
        assert!(matches!(check, BranchCheck::Drifted { .. }), "{check:?}");
        assert_eq!(
            std::fs::read(dir.path().join("local.txt")).unwrap(),
            b"precious untracked work"
        );
        assert_eq!(
            observe_head_branch_bounded(dir.path()).unwrap().as_deref(),
            Some("refs/heads/main")
        );
    }

    /// #2541: a smudge filter that never returns must not hold the turn-end
    /// check: the restore is killed at the deadline, reported as a drift, and
    /// no process of the filter outlives the report.
    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[test]
    fn a_blocked_git_helper_yields_a_bounded_drift_and_leaves_no_child() {
        let dir = repo_on("rsi/assigned");
        std::fs::write(dir.path().join("a.txt"), "assigned edit of a").unwrap();
        std::fs::write(dir.path().join("big.dat"), "assigned").unwrap();
        std::fs::write(dir.path().join(".gitattributes"), "*.dat filter=slow\n").unwrap();
        git(dir.path(), &["add", "a.txt", "big.dat", ".gitattributes"]);
        git(dir.path(), &["commit", "-q", "-m", "dat"]);
        git(dir.path(), &["switch", "-q", "main"]);
        let pid_file = dir.path().join(".git").join("smudge.pid");
        let smudge = format!("sh -c 'echo $$ > {}; exec sleep 600'", pid_file.display());
        git(
            dir.path(),
            &["config", "filter.slow.smudge", smudge.as_str()],
        );
        let status_of = |dir: &Path| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(["status", "--porcelain=v1", "--untracked-files=all"])
                .output()
                .expect("git status")
                .stdout
        };
        let status_before = status_of(dir.path());
        let started = std::time::Instant::now();
        let check = reconcile_within(dir.path(), "rsi/assigned", Duration::from_millis(1500))
            .expect("checked");
        assert!(started.elapsed() < Duration::from_secs(20), "unbounded");
        let BranchCheck::Drifted { reason, .. } = &check else {
            panic!("expected a drift report, got {check:?}");
        };
        assert!(reason.contains("timed out"), "{reason}");
        // #2557: the killed switch had written files and taken index.lock.
        assert!(reason.contains("partially switched"), "{reason}");
        assert!(
            !dir.path().join(".git/index.lock").exists(),
            "a stale index.lock was left behind"
        );
        assert_eq!(status_of(dir.path()), status_before, "{reason}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "one"
        );
        assert!(!dir.path().join("big.dat").exists());
        assert!(!dir.path().join(".gitattributes").exists());
        assert_eq!(
            observe_head_branch_bounded(dir.path()).unwrap().as_deref(),
            Some("refs/heads/main")
        );
        // The worktree is usable: git can take the index again.
        git(dir.path(), &["add", "a.txt"]);
        let pid: i32 = std::fs::read_to_string(&pid_file)
            .expect("the smudge filter ran")
            .trim()
            .parse()
            .unwrap();
        let alive = || {
            std::path::Path::new(&format!("/proc/{pid}")).exists()
                && std::fs::read_to_string(format!("/proc/{pid}/stat"))
                    .map(|stat| !stat.contains(") Z "))
                    .unwrap_or(false)
        };
        for _ in 0..50 {
            if !alive() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(!alive(), "the smudge filter outlived the reported failure");
    }

    /// #2556: with `submodule.recurse=true`, a plain `git switch` recurses into
    /// the submodule through `read-tree`, where `--no-overwrite-ignore` does not
    /// apply, and overwrites an ignored file holding local work there. The
    /// restore must neither recurse nor update the submodule.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[test]
    fn submodule_recurse_config_cannot_overwrite_an_ignored_file_inside_a_submodule() {
        let sub_origin = tempfile::tempdir().unwrap();
        git(sub_origin.path(), &["init", "-q", "-b", "main"]);
        std::fs::write(sub_origin.path().join(".gitignore"), "local.txt\n").unwrap();
        git(sub_origin.path(), &["add", ".gitignore"]);
        git(sub_origin.path(), &["commit", "-q", "-m", "ignores local"]);
        git(sub_origin.path(), &["switch", "-q", "-c", "tracked"]);
        std::fs::remove_file(sub_origin.path().join(".gitignore")).unwrap();
        std::fs::write(sub_origin.path().join("local.txt"), "tracked").unwrap();
        git(sub_origin.path(), &["add", "-A"]);
        git(sub_origin.path(), &["commit", "-q", "-m", "tracks local"]);
        let ignored_commit = rev(sub_origin.path(), "main");
        let tracked_commit = rev(sub_origin.path(), "tracked");

        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q", "-b", "main"]);
        std::fs::write(dir.path().join("a.txt"), "one").unwrap();
        git(dir.path(), &["add", "a.txt"]);
        git(dir.path(), &["commit", "-q", "-m", "init"]);
        let origin = sub_origin.path().to_str().unwrap();
        git(
            dir.path(),
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                origin,
                "sub",
            ],
        );
        git(
            &dir.path().join("sub"),
            &["checkout", "-q", &ignored_commit],
        );
        git(dir.path(), &["add", "sub"]);
        git(dir.path(), &["commit", "-q", "-m", "sub ignores local"]);
        git(dir.path(), &["switch", "-q", "-c", "rsi/assigned"]);
        git(
            &dir.path().join("sub"),
            &["checkout", "-q", &tracked_commit],
        );
        git(dir.path(), &["add", "sub"]);
        git(dir.path(), &["commit", "-q", "-m", "sub tracks local"]);
        git(dir.path(), &["switch", "-q", "main"]);
        git(
            &dir.path().join("sub"),
            &["checkout", "-q", &ignored_commit],
        );
        git(dir.path(), &["config", "submodule.recurse", "true"]);
        git(dir.path(), &["config", "protocol.file.allow", "always"]);
        let local = dir.path().join("sub/local.txt");
        std::fs::write(&local, "precious").unwrap();
        let check = reconcile(dir.path(), "rsi/assigned").expect("checked");
        assert!(matches!(check, BranchCheck::Drifted { .. }), "{check:?}");
        assert_eq!(std::fs::read(&local).unwrap(), b"precious");
        assert_eq!(
            observe_head_branch_bounded(dir.path()).unwrap().as_deref(),
            Some("refs/heads/main")
        );
        assert_eq!(rev(&dir.path().join("sub"), "HEAD"), ignored_commit);
    }

    /// #2556: `git switch <name>` creates the branch from `origin/<name>` when
    /// the local one is missing; the restore is for an existing local branch.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[test]
    fn a_missing_local_branch_is_not_guessed_from_a_remote_one() {
        let dir = repo_on("rsi/assigned");
        let remote = tempfile::tempdir().unwrap();
        git(remote.path(), &["init", "-q", "--bare", "-b", "main"]);
        git(
            dir.path(),
            &["remote", "add", "origin", remote.path().to_str().unwrap()],
        );
        git(dir.path(), &["push", "-q", "origin", "rsi/assigned"]);
        git(dir.path(), &["fetch", "-q", "origin"]);
        git(dir.path(), &["switch", "-q", "main"]);
        git(dir.path(), &["branch", "-q", "-D", "rsi/assigned"]);
        let check = reconcile(dir.path(), "rsi/assigned").expect("checked");
        assert!(matches!(check, BranchCheck::Drifted { .. }), "{check:?}");
        assert_eq!(
            observe_head_branch_bounded(dir.path()).unwrap().as_deref(),
            Some("refs/heads/main")
        );
    }
}
