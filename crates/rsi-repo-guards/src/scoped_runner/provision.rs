//! Integrated-base and provisional-renumber provenance. The original admitted
//! repository stays the evidence root: derived bases are synthetic commits in
//! its object store and a renumbered candidate runs in a scratch worktree
//! allocated outside every protected checkout root, removed on every path.
use super::gitx::{self, Git};
use crate::recent_landing;
use crate::scratch_git::Policy;
use serde_json::{Value, json};
use std::io;
use std::path::{Path, PathBuf};

pub const MERGE_CONFLICT_PATH_LIMIT: usize = 32;
const AUTHOR: [(&str, &str); 4] = [
    ("GIT_AUTHOR_NAME", "scoped-test"),
    ("GIT_AUTHOR_EMAIL", "scoped-test@localhost"),
    ("GIT_COMMITTER_NAME", "scoped-test"),
    ("GIT_COMMITTER_EMAIL", "scoped-test@localhost"),
];

/// Base that excludes already-gated rolling work a launch-pin worker merged in
/// (#2135): a synthetic commit whose tree is the pin merged with the rolling
/// commit `head` integrated, so `base..head` is the worker-owned delta. Any
/// failure keeps `base`; landing gates still see the full candidate.
pub fn integrated_base(git: &Git, base: &str, head: &str, rolling: &str) -> String {
    let attempt = || -> io::Result<Option<String>> {
        if git.commit(rolling)?.is_none() {
            return Ok(None);
        }
        let merge_base = git.builtin(&["merge-base", head, rolling], &[])?;
        if !merge_base.ok() || merge_base.first_line().is_empty() {
            return Ok(None);
        }
        let integrated = merge_base.first_line().to_string();
        let Some(base_sha) = git.commit(base)? else {
            return Ok(None);
        };
        if integrated == base_sha {
            return Ok(None);
        }
        for (a, b) in [(&base_sha, &integrated), (&integrated, &base_sha)] {
            if git
                .builtin(&["merge-base", "--is-ancestor", a, b], &[])?
                .ok()
            {
                // Pin already landed (or rolling is behind it): nothing extra to exclude.
                return Ok(None);
            }
        }
        let tree = git.builtin(&["merge-tree", "--write-tree", &base_sha, &integrated], &[])?;
        if !tree.ok() {
            return Ok(None);
        }
        let tree_id = tree
            .stdout
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_string();
        if tree_id.is_empty() {
            return Ok(None);
        }
        let commit = git.builtin(
            &[
                "commit-tree",
                &tree_id,
                "-p",
                &base_sha,
                "-p",
                &integrated,
                "-m",
                "scoped-test integrated base",
            ],
            &AUTHOR,
        )?;
        Ok(commit
            .ok()
            .then(|| commit.first_line().to_string())
            .filter(|s| !s.is_empty()))
    };
    attempt().ok().flatten().unwrap_or_else(|| base.to_string())
}

fn display_argv(git: &Git, args: &[&str]) -> Vec<String> {
    let mut argv = vec![
        "git".to_string(),
        "-C".into(),
        git.repo.to_string_lossy().into_owned(),
    ];
    argv.extend(args.iter().map(|a| (*a).to_string()));
    argv
}

/// The legacy receipt shape for a trial merge that could not be decided: the
/// diagnostic is #362's `git_diagnostic` (byte-for-byte what the recent-landing
/// check reports) and the reason is its `GitStepError` text.
fn unknown(
    argv: &[String],
    kind: &str,
    exit_code: Option<i32>,
    stderr: &str,
    timeout: Option<f64>,
) -> Value {
    let diagnostic =
        recent_landing::git_diagnostic(argv, Some("trial merge"), kind, exit_code, stderr, timeout);
    json!({"merges_cleanly_onto_rolling": null, "conflict_paths": [],
           "merge_check_reason": recent_landing::reason(&diagnostic),
           "merge_check_diagnostic": diagnostic})
}

/// A trial merge's receipt fields plus, when it merged cleanly, the resolved
/// rolling commit and the merged tree the recent-landing advisory blames over.
pub struct TrialMerge {
    pub fields: Value,
    pub tree: Option<(String, String)>,
}

/// Informational fetch-free trial merge of `head` onto the already-resolved
/// `rolling` ref. Returns receipt fields; never raises and never changes the
/// run's verdict.
pub fn trial_merge(git: &Git, head: &str, rolling: &str) -> TrialMerge {
    let refused = |fields: Value| TrialMerge { fields, tree: None };
    let mut resolved = Vec::new();
    for reference in [rolling, head] {
        let spec = format!("{reference}^{{commit}}");
        let argv = display_argv(git, &["rev-parse", "--verify", "--quiet", &spec]);
        if reference.starts_with('-') || reference.contains(['\0', '\n', '\r']) {
            return refused(unknown(&argv, "git_exit", Some(1), "", None));
        }
        match git.commit(reference) {
            Ok(Some(commit)) => resolved.push(commit),
            Ok(None) => return refused(unknown(&argv, "git_exit", Some(1), "", None)),
            Err(error) => {
                return refused(unknown(
                    &argv,
                    "launch_error",
                    None,
                    &error.to_string(),
                    None,
                ));
            }
        }
    }
    let args = [
        "merge-tree",
        "--write-tree",
        "--name-only",
        "--no-messages",
        &resolved[0],
        &resolved[1],
    ];
    let argv = display_argv(git, &args);
    let out = match git.builtin(&args, &[]) {
        Ok(out) => out,
        Err(error) => {
            return refused(unknown(
                &argv,
                "launch_error",
                None,
                &error.to_string(),
                None,
            ));
        }
    };
    if out.timed_out {
        return refused(unknown(
            &argv,
            "timeout_budget",
            None,
            &out.stderr,
            Some(gitx::STEP_SECONDS as f64),
        ));
    }
    match out.code {
        0 => TrialMerge {
            fields: json!({"merges_cleanly_onto_rolling": true, "conflict_paths": []}),
            tree: Some((resolved[0].clone(), out.first_line().to_string())),
        },
        1 => {
            let mut lines = out.stdout.lines().skip(1);
            let mut paths: Vec<String> = Vec::new();
            for line in &mut lines {
                if line.is_empty() {
                    break;
                }
                paths.push(line.to_string());
            }
            paths.sort();
            paths.dedup();
            refused(json!({
                "merges_cleanly_onto_rolling": false,
                "conflict_paths": paths.iter().take(MERGE_CONFLICT_PATH_LIMIT).collect::<Vec<_>>(),
                "conflict_paths_total": paths.len(),
                "merge_check_diagnostic": recent_landing::git_diagnostic(
                    &argv, Some("trial merge"), "merge_conflict", Some(1), &out.stderr, None),
            }))
        }
        code => refused(unknown(&argv, "git_exit", Some(code), &out.stderr, None)),
    }
}

/// The receipt fields alone (the trial merge without its tree).
pub fn merge_onto_rolling(git: &Git, head: &str, rolling: &str) -> Value {
    trial_merge(git, head, rolling).fields
}

/// A detached worktree of `commit` outside every protected root, removed (and
/// pruned from the original repository) on drop.
pub struct ScratchWorktree {
    repo: PathBuf,
    path: PathBuf,
    // Dropped after `Drop::drop` has deregistered the worktree.
    _dir: tempfile::TempDir,
}
impl ScratchWorktree {
    pub fn create(git: &Git, policy: &Policy, prefix: &str, commit: &str) -> io::Result<Self> {
        let dir = policy.tempdir(prefix)?;
        let path = dir.path().join("worktree");
        let out = git.plain(&[
            "worktree",
            "add",
            "--detach",
            "--quiet",
            &path.to_string_lossy(),
            commit,
        ])?;
        if !out.ok() || !path.is_dir() {
            return Err(io::Error::other(format!(
                "git worktree add failed: {}",
                format!("{}{}", out.stdout, out.stderr).trim()
            )));
        }
        Ok(Self {
            repo: git.repo.to_path_buf(),
            path: path.clone(),
            _dir: dir,
        })
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
}
impl Drop for ScratchWorktree {
    fn drop(&mut self) {
        let env: std::collections::BTreeMap<String, String> = ["PATH", "HOME"]
            .iter()
            .filter_map(|k| std::env::var(k).ok().map(|v| ((*k).to_string(), v)))
            .collect();
        let cancel = std::sync::atomic::AtomicUsize::new(0);
        let git = Git {
            repo: &self.repo,
            env: &env,
            cancel: &cancel,
        };
        let _ = git.plain(&[
            "worktree",
            "remove",
            "--force",
            &self.path.to_string_lossy(),
        ]);
        let _ = git.plain(&["worktree", "prune"]);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::testutil;
    use std::collections::BTreeMap;
    use std::sync::atomic::AtomicUsize;
    fn commit(dir: &Path, name: &str, text: &str) -> String {
        testutil::write(dir, name, text);
        testutil::git(dir, &["add", name]);
        testutil::git(
            dir,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-qm",
                name,
            ],
        );
        testutil::git(dir, &["rev-parse", "HEAD"])
            .trim()
            .to_string()
    }
    fn setup() -> (tempfile::TempDir, BTreeMap<String, String>, AtomicUsize) {
        let dir = tempfile::tempdir().unwrap();
        testutil::git(dir.path(), &["init", "-q", "-b", "main"]);
        (
            dir,
            super::super::gitx::test_environment(),
            AtomicUsize::new(0),
        )
    }
    #[test]
    fn merge_check_reports_clean_conflicting_and_unresolved_without_raising() {
        let (dir, env, cancel) = setup();
        let git = Git {
            repo: dir.path(),
            env: &env,
            cancel: &cancel,
        };
        let root = commit(dir.path(), "a.txt", "one\n");
        testutil::git(dir.path(), &["checkout", "-q", "-b", "side"]);
        let side = commit(dir.path(), "a.txt", "side\n");
        testutil::git(dir.path(), &["checkout", "-q", "main"]);
        let clean = commit(dir.path(), "b.txt", "b\n");
        assert_eq!(
            merge_onto_rolling(&git, &root, &clean)["merges_cleanly_onto_rolling"],
            true
        );
        let conflicted = commit(dir.path(), "a.txt", "main\n");
        let report = merge_onto_rolling(&git, &side, &conflicted);
        assert_eq!(report["merges_cleanly_onto_rolling"], false);
        assert_eq!(report["conflict_paths"], json!(["a.txt"]));
        assert_eq!(report["merge_check_diagnostic"]["kind"], "merge_conflict");
        let unresolved = merge_onto_rolling(&git, "no-such-ref", &conflicted);
        assert!(unresolved["merges_cleanly_onto_rolling"].is_null());
        let reason = unresolved["merge_check_reason"].as_str().unwrap();
        assert!(
            reason.starts_with("trial merge: git_exit; exit_code=1; argv="),
            "{reason}"
        );
        assert!(reason.contains("no-such-ref"), "{reason}");
        // The diagnostic is #362's, byte for byte, and the reason is its GitStepError text.
        let diagnostic = &unresolved["merge_check_diagnostic"];
        assert_eq!(diagnostic["step"], "trial merge");
        assert_eq!(diagnostic["argv"][0], "git");
        assert_eq!(diagnostic["argv"][1], "-C");
        assert_eq!(
            *diagnostic,
            recent_landing::git_diagnostic(
                &[
                    "git".to_string(),
                    "-C".into(),
                    dir.path().to_string_lossy().into_owned(),
                    "rev-parse".into(),
                    "--verify".into(),
                    "--quiet".into(),
                    "no-such-ref^{commit}".into()
                ],
                Some("trial merge"),
                "git_exit",
                Some(1),
                "",
                None
            )
        );
        assert_eq!(reason, recent_landing::reason(diagnostic));
        let clean = trial_merge(&git, &root, &conflicted);
        let (rolling, tree) = clean.tree.expect("a clean merge names its tree");
        assert_eq!(rolling, conflicted);
        assert_eq!(tree.len(), 40);
    }
    #[test]
    fn integrated_base_excludes_rolling_work_merged_into_an_unlanded_pin() {
        let (dir, env, cancel) = setup();
        let git = Git {
            repo: dir.path(),
            env: &env,
            cancel: &cancel,
        };
        commit(dir.path(), "a.txt", "one\n");
        testutil::git(dir.path(), &["branch", "rolling"]);
        // The worker's launch pin: unlanded work that rolling does not have.
        testutil::git(dir.path(), &["checkout", "-q", "-b", "pin"]);
        let pin = commit(dir.path(), "pin.txt", "pin\n");
        // Rolling advances; the worker merges it and adds its own commit.
        testutil::git(dir.path(), &["checkout", "-q", "rolling"]);
        let rolling = commit(dir.path(), "rolled.txt", "rolled\n");
        testutil::git(dir.path(), &["checkout", "-q", "pin"]);
        testutil::git(
            dir.path(),
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "merge",
                "-q",
                "--no-edit",
                "rolling",
            ],
        );
        let head = commit(dir.path(), "own.txt", "own\n");
        let integrated = integrated_base(&git, &pin, &head, "rolling");
        assert_ne!(integrated, pin);
        assert_eq!(integrated.len(), 40);
        let changed = testutil::git(dir.path(), &["diff", "--name-only", &integrated, &head]);
        assert_eq!(changed.trim(), "own.txt");
        // Parents: the pin and the rolling commit.
        let parents = testutil::git(dir.path(), &["rev-list", "--parents", "-n1", &integrated]);
        assert!(parents.contains(&pin) && parents.contains(&rolling));
        // An already-landed pin, or an unknown rolling ref, keeps the base.
        assert_eq!(integrated_base(&git, &rolling, &head, "rolling"), rolling);
        assert_eq!(integrated_base(&git, &pin, &head, "no-such-rolling"), pin);
    }
    #[test]
    fn scratch_worktree_is_outside_the_checkout_and_deregistered_on_drop() {
        let (dir, env, cancel) = setup();
        let git = Git {
            repo: dir.path(),
            env: &env,
            cancel: &cancel,
        };
        let head = commit(dir.path(), "a.txt", "one\n");
        let policy = Policy::new(dir.path(), dir.path(), BTreeMap::new(), &[]).unwrap();
        let path = {
            let scratch =
                ScratchWorktree::create(&git, &policy, "rsi-scoped-candidate-", &head).unwrap();
            assert!(scratch.path().join("a.txt").is_file());
            assert!(
                !scratch
                    .path()
                    .canonicalize()
                    .unwrap()
                    .starts_with(dir.path().canonicalize().unwrap())
            );
            assert!(
                testutil::git(dir.path(), &["worktree", "list"])
                    .lines()
                    .count()
                    >= 2
            );
            scratch.path().to_path_buf()
        };
        assert!(!path.exists());
        assert_eq!(
            testutil::git(dir.path(), &["worktree", "list"])
                .lines()
                .count(),
            1
        );
    }
}
