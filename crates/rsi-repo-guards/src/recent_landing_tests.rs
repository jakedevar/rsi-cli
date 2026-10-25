use super::*;
use crate::testutil::{self, git, write};
struct Graph {
    temp: tempfile::TempDir,
    first: String,
    introducing: String,
}
const PATH: &str = "notes [v1].txt";
const OLD: &str = "base\nold line\nend\n";
impl Graph {
    fn new() -> Self {
        let temp = testutil::scratch_repository();
        let root = temp.path();
        git(root, &["init", "-q", "-b", "rolling"]);
        write(root, PATH, OLD);
        std::fs::write(root.join("image.bin"), b"\0old").unwrap();
        git(root, &["add", PATH, "image.bin"]);
        git(root, &["commit", "-qm", "root"]);
        git(root, &["checkout", "-qb", "worker"]);
        write(root, "worker.txt", "work\n");
        git(root, &["add", "worker.txt"]);
        git(root, &["commit", "-qm", "first own change"]);
        let first = git(root, &["rev-parse", "HEAD"]).trim().to_string();
        git(root, &["checkout", "-q", "rolling"]);
        write(root, PATH, "base\nold line\nrecent one\nrecent two\nend\n");
        std::fs::write(root.join("image.bin"), b"\0new").unwrap();
        git(root, &["add", PATH, "image.bin"]);
        git(root, &["commit", "-qm", "recent landing"]);
        let introducing = git(root, &["rev-parse", "HEAD"]).trim().to_string();
        git(root, &["update-ref", "refs/remotes/origin/rolling", "HEAD"]);
        git(root, &["checkout", "-q", "worker"]);
        git(root, &["merge", "-q", "--no-edit", "rolling"]);
        Self {
            temp,
            first,
            introducing,
        }
    }
    fn root(&self) -> &Path {
        self.temp.path()
    }
    fn commit(&self, message: &str) -> String {
        let files = [PATH, "image.bin", "worker.txt"]
            .into_iter()
            .filter(|p| self.root().join(p).exists())
            .collect::<Vec<_>>();
        let mut args = vec!["add"];
        args.extend(files);
        git(self.root(), &args);
        git(self.root(), &["commit", "-qm", message]);
        git(self.root(), &["rev-parse", "HEAD"]).trim().into()
    }
    fn stale(&self, message: &str) -> String {
        write(self.root(), PATH, OLD);
        std::fs::write(self.root().join("image.bin"), b"\0old").unwrap();
        self.commit(message)
    }
    fn refs(&self) -> (String, String, String) {
        let source = git(self.root(), &["rev-parse", "HEAD"]).trim().to_string();
        let rolling = git(self.root(), &["rev-parse", "origin/rolling"])
            .trim()
            .to_string();
        let tree = git(
            self.root(),
            &["merge-tree", "--write-tree", &rolling, &source],
        )
        .trim()
        .to_string();
        (source, rolling, tree)
    }
    fn check(&self, limits: Limits) -> Value {
        let (s, r, t) = self.refs();
        recent_landing_deletions(self.root(), &s, &r, &t, limits)
    }
}
#[test]
fn ported_stale_copy_old_edit_whole_removal_and_net_delta() {
    let g = Graph::new();
    g.stale("old copy");
    let r = g.check(Limits::default());
    assert_eq!(r["status"], "complete");
    assert_eq!(r["first_own_commit"], g.first);
    assert_eq!(r["lines_blamed"], 2);
    assert_eq!(
        r["warnings"],
        json!([{"path":PATH,"line_count":2,"introducing_commit":g.introducing,"line_ranges":[[3,4]]}])
    );
    let g = Graph::new();
    let current = std::fs::read_to_string(g.root().join(PATH)).unwrap();
    write(
        g.root(),
        PATH,
        &current.replace("old line", "edited old line"),
    );
    g.commit("ordinary edit");
    let r = g.check(Limits::default());
    assert_eq!(r["status"], "complete");
    assert_eq!(r["lines_blamed"], 1);
    assert_eq!(r["warnings"], json!([]));
    let g = Graph::new();
    git(g.root(), &["rm", PATH]);
    git(g.root(), &["commit", "-qm", "remove whole file"]);
    let r = g.check(Limits::default());
    assert_eq!(r["lines_blamed"], 5);
    assert_eq!(r["warnings"][0]["line_ranges"], json!([[3, 4]]));
    let g = Graph::new();
    let rolling = git(g.root(), &["rev-parse", "origin/rolling"])
        .trim()
        .to_string();
    let tree = git(
        g.root(),
        &["merge-tree", "--write-tree", &rolling, &g.first],
    )
    .trim()
    .to_string();
    let r = recent_landing_deletions(g.root(), &g.first, &rolling, &tree, Limits::default());
    assert_eq!(r["status"], "complete");
    assert_eq!(r["lines_blamed"], 0);
    assert_eq!(r["warnings"], json!([]));
}
#[test]
fn ported_trailers_are_scoped_to_source_only_commits() {
    let g = Graph::new();
    let marker = g.stale("intentional removal\n\nAllow-Recent-Landing-Removals: true");
    let r = g.check(Limits::default());
    assert_eq!(r["status"], "opted_out");
    assert_eq!(r["opt_out_commit"], marker);
    assert_eq!(r["warnings"], json!([]));
    assert_eq!(r["lines_blamed"], 0);
    write(g.root(), "worker.txt", "more work\n");
    g.commit("later own change");
    assert_eq!(g.check(Limits::default())["opt_out_commit"], marker);
    let g = Graph::new();
    git(g.root(), &["checkout", "-q", "rolling"]);
    git(
        g.root(),
        &[
            "commit",
            "--allow-empty",
            "-qm",
            "unrelated\n\nAllow-Recent-Landing-Removals: true",
        ],
    );
    git(
        g.root(),
        &["update-ref", "refs/remotes/origin/rolling", "HEAD"],
    );
    git(g.root(), &["checkout", "-q", "worker"]);
    git(g.root(), &["merge", "-q", "--no-edit", "rolling"]);
    g.stale("old copy");
    assert_eq!(
        g.check(Limits::default())["warnings"][0]["introducing_commit"],
        g.introducing
    );
    let g = Graph::new();
    g.stale("removal\n\nAllow-Recent-Landing-Removals: true\nthis is prose, not trailers");
    assert_eq!(g.check(Limits::default())["warnings"][0]["line_count"], 2);
}
#[test]
fn ported_line_file_time_budgets_are_visible() {
    let g = Graph::new();
    g.stale("old copy");
    let r = g.check(Limits {
        lines: 1,
        ..Limits::default()
    });
    assert_eq!(r["status"], "partial");
    assert_eq!(r["lines_blamed"], 1);
    assert_eq!(r["warnings"][0]["line_ranges"], json!([[3, 3]]));
    let r = g.check(Limits {
        files: 0,
        ..Limits::default()
    });
    assert_eq!(r["status"], "partial");
    assert_eq!(r["diagnostic"]["step"], "file budget");
    let r = g.check(Limits {
        time: Duration::ZERO,
        ..Limits::default()
    });
    assert_eq!(r["status"], "unavailable");
    assert_eq!(r["warnings"], json!([]));
    assert_eq!(r["diagnostic"]["kind"], "timeout_budget");
    assert_eq!(r["diagnostic"]["step"], "rev-list");
    assert!(r["diagnostic"]["exit_code"].is_null());
    assert_eq!(r["diagnostic"]["timeout_seconds"], 0);
}
struct FakeGit {
    fail: String,
    timeout: bool,
    attempted: Vec<Vec<String>>,
}
impl GitRunner for FakeGit {
    fn run(&mut self, argv: &[String], remaining: Duration) -> Result<GitOutput, GitError> {
        self.attempted.push(argv.to_vec());
        if argv[3] == self.fail {
            if self.timeout {
                return Err(GitError {
                    kind: "timeout_budget",
                    stderr: b"history scan stalled\n".to_vec(),
                });
            }
            return Ok(GitOutput {
                code: 128,
                stdout: Vec::new(),
                stderr: format!("{}fatal: missing history\n", "noise\n".repeat(1000)).into_bytes(),
            });
        }
        ProcessGit.run(argv, remaining)
    }
}
#[test]
fn ported_each_git_failure_and_timeout_retains_full_diagnostic() {
    let g = Graph::new();
    g.stale("old copy");
    let (s, r, t) = g.refs();
    for step in ["rev-list", "log", "diff", "blame", "merge-base"] {
        let mut fake = FakeGit {
            fail: step.into(),
            timeout: false,
            attempted: Vec::new(),
        };
        let result = check_with(g.root(), &s, &r, &t, Limits::default(), &mut fake);
        assert_eq!(
            result["status"],
            if step == "merge-base" {
                "partial"
            } else {
                "unavailable"
            }
        );
        let d = &result["diagnostic"];
        assert_eq!(d["step"], step);
        assert_eq!(d["exit_code"], 128);
        assert_eq!(
            d["argv"],
            git_diagnostic(
                fake.attempted.last().unwrap(),
                None,
                "git_exit",
                None,
                "",
                None
            )["argv"]
        );
        assert!(
            d["stderr_tail"]
                .as_str()
                .unwrap()
                .ends_with("fatal: missing history")
        );
        assert!(d["stderr_tail"].as_str().unwrap().chars().count() <= STDERR_LIMIT);
        assert_eq!(d["stderr_shortened"], true);
        assert!(result["reason"].as_str().unwrap().contains("exit_code=128"));
    }
    let mut fake = FakeGit {
        fail: "blame".into(),
        timeout: true,
        attempted: Vec::new(),
    };
    let result = check_with(g.root(), &s, &r, &t, Limits::default(), &mut fake);
    let d = &result["diagnostic"];
    assert_eq!(d["step"], "blame");
    assert_eq!(d["kind"], "timeout_budget");
    assert!(d["exit_code"].is_null());
    assert_eq!(d["stderr_tail"], "history scan stalled");
    assert_eq!(d["timeout_seconds"], 10);
    let args = d["argv"].as_array().unwrap();
    assert_eq!(&args[args.len() - 2..], &[json!("--"), json!(PATH)]);
}
#[test]
fn ported_long_argv_and_batched_hunks() {
    let argv = vec![
        "git".into(),
        "-C".into(),
        format!("/long/{}", "repo/".repeat(100)),
        "blame".into(),
        "-L".into(),
        "29,29".into(),
        "-L".into(),
        "610,610".into(),
        "a".repeat(40),
        "--".into(),
        format!("{}file.py", "nested/".repeat(100)),
    ];
    let d = git_diagnostic(&argv, None, "git_exit", None, "", None);
    let args = d["argv"].as_array().unwrap();
    assert_eq!(args.len(), argv.len());
    assert_eq!(
        &args[4..8],
        &[json!("-L"), json!("29,29"), json!("-L"), json!("610,610")]
    );
    assert_eq!(args[8], format!("{}...", "a".repeat(12)));
    assert!(args.last().unwrap().as_str().unwrap().ends_with("file.py"));
    assert_eq!(d["argv_shortened"], true);
    assert!(
        args.iter()
            .all(|s| s.as_str().unwrap().chars().count() <= ARG_LIMIT)
    );
    let g = Graph::new();
    write(g.root(), PATH, "base\nrecent one\nrecent two\n");
    g.commit("separate deletion hunks");
    let (s, r, t) = g.refs();
    let mut fake = FakeGit {
        fail: String::new(),
        timeout: false,
        attempted: Vec::new(),
    };
    check_with(g.root(), &s, &r, &t, Limits::default(), &mut fake);
    let blame = fake
        .attempted
        .iter()
        .filter(|a| a[3] == "blame")
        .collect::<Vec<_>>();
    assert_eq!(blame.len(), 1);
    assert_eq!(blame[0].iter().filter(|a| *a == "-L").count(), 2);
}
#[test]
fn ported_multiple_merge_bases_and_ignore_config() {
    let g = Graph::new();
    git(g.root(), &["checkout", "-q", "rolling"]);
    git(g.root(), &["merge", "-q", "--no-edit", &g.first]);
    git(
        g.root(),
        &["update-ref", "refs/remotes/origin/rolling", "HEAD"],
    );
    git(g.root(), &["checkout", "-q", "worker"]);
    g.stale("old copy");
    assert_eq!(
        git(g.root(), &["merge-base", "--all", "origin/rolling", "HEAD"])
            .lines()
            .count(),
        2
    );
    let r = g.check(Limits::default());
    assert_eq!(r["status"], "complete");
    assert_eq!(r["lines_blamed"], 2);
    assert_eq!(r["warnings"], json!([]));
    let g = Graph::new();
    let ignore = write(g.root(), "ignore-revs", &format!("{}\n", g.introducing));
    git(
        g.root(),
        &["config", "blame.ignoreRevsFile", ignore.to_str().unwrap()],
    );
    g.stale("old copy");
    assert_eq!(
        g.check(Limits::default())["warnings"][0]["introducing_commit"],
        g.introducing
    );
}

#[test]
fn large_history_cutoff_does_not_open_irrelevant_missing_old_blob() {
    let g = Graph::new();
    git(g.root(), &["checkout", "-q", "rolling"]);
    let old = (1..=50000)
        .map(|n| format!("historical line {n}\n"))
        .collect::<String>();
    let mut obsolete = String::new();
    for version in 0..24 {
        write(g.root(), PATH, &format!("{old}history version {version}\n"));
        g.commit(&format!("historical edit {version}"));
        if version == 0 {
            obsolete = git(g.root(), &["rev-parse", &format!("HEAD:{PATH}")])
                .trim()
                .into();
        }
    }
    git(g.root(), &["checkout", "-qb", "large-worker"]);
    write(g.root(), "worker.txt", "new source\n");
    g.commit("first large-file source change");
    git(g.root(), &["checkout", "-q", "rolling"]);
    let current = std::fs::read_to_string(g.root().join(PATH)).unwrap();
    write(g.root(), PATH, &format!("{current}recent addition\n"));
    let introducing = g.commit("recent large-file landing");
    git(
        g.root(),
        &["update-ref", "refs/remotes/origin/rolling", "HEAD"],
    );
    git(g.root(), &["checkout", "-q", "large-worker"]);
    git(g.root(), &["merge", "-q", "--no-edit", "rolling"]);
    write(
        g.root(),
        PATH,
        &current.replace("historical line 3951\n", ""),
    );
    g.commit("delete an old line and a recent line");
    // Only a disposable fixture object is removed. The source-boundary blob
    // remains: a bounded blame must never need the irrelevant oldest blob.
    let object = g
        .root()
        .join(".git/objects")
        .join(&obsolete[..2])
        .join(&obsolete[2..]);
    std::fs::remove_file(object).unwrap();
    let start = Instant::now();
    let r = g.check(Limits::default());
    assert!(start.elapsed() < Limits::default().time);
    assert_eq!(r["status"], "complete");
    assert_eq!(r["lines_blamed"], 2);
    assert_eq!(
        r["warnings"],
        json!([{"path":PATH,"line_count":1,"introducing_commit":introducing,"line_ranges":[[50002,50002]]}])
    );
}

#[cfg(unix)]
mod raw_paths {
    use super::*;
    use crate::testutil::raw::{commit, tree};

    #[test]
    fn non_utf8_git_paths_survive_argv_and_print_as_legacy_surrogate_escapes() {
        let temp = testutil::scratch_repository();
        let root = temp.path();
        git(root, &["init", "-q", "-b", "rolling"]);
        let path: &[u8] = b"dir/old\xff\xfe.txt";
        let new = "base\nold line\nrecent one\nrecent two\nend\n";
        let a = commit(root, &tree(root, &[(path, OLD)]), &[]);
        let w_tree = tree(root, &[(path, OLD), (b"worker.txt", "work\n")]);
        let w = commit(root, &w_tree, &[&a]);
        let r = commit(root, &tree(root, &[(path, new)]), &[&a]);
        let s = commit(root, &w_tree, &[&w, &r]);
        let result = recent_landing_deletions(root, &s, &r, &w_tree, Limits::default());
        assert_eq!(result["status"], "complete", "{result}");
        let warnings = result["warnings"].as_array().unwrap();
        assert_eq!(warnings.len(), 1, "{result}");
        assert_eq!(warnings[0]["introducing_commit"], r.as_str());
        assert_eq!(warnings[0]["line_ranges"], json!([[3, 4]]));
        let printed = crate::os_escape::legacy_compact_json(&result);
        assert!(
            printed.contains(r#""path":"dir/old\udcff\udcfe.txt""#),
            "{printed}"
        );
    }
}

#[test]
fn advisory_wraps_the_trial_merge_for_the_receipt() {
    let g = Graph::new();
    g.stale("old copy");
    let (source, rolling, tree) = g.refs();
    let kept = advisory(
        g.root(),
        &source,
        Some((&rolling, &tree)),
        "",
        &Value::Null,
        true,
    );
    assert_eq!(kept["status"], "complete");
    assert!(
        kept["note"]
            .as_str()
            .unwrap()
            .contains("renumbered candidate")
    );
    assert_eq!(
        kept["warnings"],
        recent_landing_deletions(g.root(), &source, &rolling, &tree, Limits::default())["warnings"]
    );
    let skipped = advisory(
        g.root(),
        &source,
        None,
        "trial merge conflicted; see conflict_paths",
        &json!({"step": "trial merge"}),
        true,
    );
    assert_eq!(
        skipped,
        json!({"status": "skipped", "warnings": [], "reason": "trial merge conflicted; see conflict_paths", "diagnostic": {"step": "trial merge"}})
    );
    assert_eq!(
        advisory_line(&skipped),
        r#"scoped-test: reverts_recent_landing {"status":"skipped","warnings":[],"reason":"trial merge conflicted; see conflict_paths","diagnostic":{"step":"trial merge"}}"#
    );
}
