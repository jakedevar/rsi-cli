//! Which copy of `scripts/scoped-test` a `scoped_test` job runs (#1938).
//!
//! The job used to run the worker sandbox's own `scripts/scoped-test`, so a
//! fix to the verification tool (#1898, #1910, #1913, #1934) reached a
//! long-lived worker only after it merged `rolling` and the worker integrated
//! it. The job now runs the tool from the current `origin/rolling` tip,
//! materialized read-only under `<jobs_dir>/scoped-test-tools/<rev>-<set digest>/`, unless
//! the candidate diff itself changes the tool or one of its helpers: then the
//! candidate's own copy runs, so a change to the tool is verified by that
//! change. The daemon records the selected path/revision/file blobs before
//! launch in the job's result JSON. The script's `tool` receipt is diagnostic
//! only. `rolling_tool:true` forces trusted rolling verification (#579).
//!
//! #279: a candidate whose tool change sits in its base (an unlanded tool
//! change the worker built on) leaves `base...head` untouched, yet its copy is
//! the one under test. When the tool files changed on the candidate's side of
//! `origin/rolling` (`rolling...head`) and still differ from rolling's bytes,
//! the candidate's copy runs too, with `source: candidate_differs_from_rolling`.
//! A sandbox merely behind rolling has no such change, so it still runs the
//! rolling copy.
//!
//! Any failure to resolve the rolling copy (no `origin/rolling`, not a git
//! checkout, no disk) falls back to the sandbox copy: verification still runs,
//! and the receipt says `worktree`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use rsi_common::agent_jobs::JOB_SCOPED_TEST_TOOL_HELPER_MISSING;
use sha2::{Digest, Sha256};
use std::process::Command;

/// Files `scripts/scoped-test` is known to load from its own directory. This is
/// the floor of the tool set, not all of it: [`referenced_helpers`] adds every
/// sibling the tool scripts name, so a helper a change adds needs no edit here
/// and a daemon not yet redeployed cannot run a tool copy without it (hub #3024).
pub(super) const TOOL_FILES: &[&str] = &[
    "scripts/scoped-test",
    "scripts/check-touched-shards",
    "scripts/python-test-gate.py",
    "scripts/target-pool",
    "scripts/e2e-prebuild-bins.txt",
];

/// The scripts whose text names the helpers they load beside themselves.
const TOOL_SCRIPTS: &[&str] = &["scripts/scoped-test", "scripts/check-touched-shards"];

/// Environment names the script reads into its receipt's `tool` object.
pub(super) const TOOL_SOURCE_ENV: &str = "RSI_SCOPED_TEST_TOOL_SOURCE";
pub(super) const TOOL_REV_ENV: &str = "RSI_SCOPED_TEST_TOOL_REV";
/// The comma-separated file names (under `scripts/`) of the tool set, echoed by
/// the script into the receipt with each file's blob.
pub(super) const TOOL_FILES_ENV: &str = "RSI_SCOPED_TEST_TOOL_FILES";

/// The newest materialized revs kept; older ones are pruned on materialize.
const KEEP_REVS: usize = 8;

/// The copy of the tool a job runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ScopedTool {
    /// The tool from the `origin/rolling` tip, materialized at `script`, with
    /// every file of `files` (names under `scripts/`) beside it.
    Rolling {
        script: PathBuf,
        rev: String,
        files: Vec<String>,
    },
    /// The candidate changes the tool or a helper it loads: its own
    /// `scripts/scoped-test`, with the helpers it references in `files`, and
    /// `why` names which comparison chose it.
    Candidate {
        files: Vec<String>,
        why: CandidateWhy,
    },
    /// The rolling copy could not be resolved: the sandbox's own copy.
    Worktree,
}

/// Why a job runs the candidate's own copy of the tool; the receipt's
/// `tool.source` carries [`CandidateWhy::source`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CandidateWhy {
    /// The candidate diff (`base...head`) changes a tool file (#1938).
    DiffChangesTool,
    /// The diff does not, but the head changed a tool file on its side of
    /// `origin/rolling` and its bytes differ from rolling's: the tool change
    /// is in the base, not yet on rolling (#279).
    DiffersFromRolling,
}

impl CandidateWhy {
    /// The receipt's `tool.source` for this choice.
    pub(super) fn source(self) -> &'static str {
        match self {
            Self::DiffChangesTool => "candidate",
            Self::DiffersFromRolling => "candidate_differs_from_rolling",
        }
    }
}

impl ScopedTool {
    /// Observe selected files directly, never from the script's output/env.
    pub(super) fn provenance(
        &self,
        cwd: &Path,
    ) -> Result<rsi_common::agent_jobs::ScopedToolProvenanceV1, String> {
        use rsi_common::agent_jobs::{ScopedToolFileV1, ScopedToolProvenanceV1, ScopedToolSource};
        let (source, script, revision, files) = match self {
            Self::Rolling { script, rev, files } => (
                ScopedToolSource::Rolling,
                script.clone(),
                rev.as_str(),
                files.clone(),
            ),
            Self::Candidate { files, .. } => (
                ScopedToolSource::Candidate,
                cwd.join("scripts/scoped-test"),
                "HEAD",
                files.clone(),
            ),
            Self::Worktree => (
                ScopedToolSource::Sandbox,
                cwd.join("scripts/scoped-test"),
                "HEAD",
                tool_names(&tool_paths(&helpers_at(cwd, "HEAD"))),
            ),
        };
        let text = |args: &[&str]| {
            git(cwd, args)
                .and_then(|bytes| String::from_utf8(bytes).ok())
                .map(|value| value.trim().to_string())
        };
        let rev = text(&["rev-parse", "--verify", &format!("{revision}^{{commit}}")]);
        let mut observed = Vec::new();
        for name in files {
            let path = script.with_file_name(&name);
            let blob = text(&["hash-object", "--no-filters", &path.to_string_lossy()]);
            if source == ScopedToolSource::Rolling {
                let expected = text(&[
                    "rev-parse",
                    "--verify",
                    &format!("{revision}:scripts/{name}"),
                ]);
                if blob.is_none() || blob != expected {
                    return Err(format!(
                        "scoped_test_rolling_tool_unavailable: rolling tool file {} differs from {revision}; rerun scoped_test with the rolling tool copy (rolling_tool:true)",
                        path.display()
                    ));
                }
            }
            observed.push(ScopedToolFileV1 {
                path: path.display().to_string(),
                blob,
            });
        }
        Ok(ScopedToolProvenanceV1 {
            source,
            path: script.display().to_string(),
            rev,
            files: observed,
        })
    }

    /// The script path and the `NAME=value` entries to put after `env`.
    pub(super) fn launch(&self) -> (String, Vec<String>) {
        match self {
            Self::Rolling { script, rev, files } => (
                script.display().to_string(),
                vec![
                    format!("{TOOL_SOURCE_ENV}=rolling"),
                    format!("{TOOL_REV_ENV}={rev}"),
                    format!("{TOOL_FILES_ENV}={}", files.join(",")),
                ],
            ),
            Self::Candidate { files, why } => (
                "scripts/scoped-test".to_string(),
                vec![
                    format!("{TOOL_SOURCE_ENV}={}", why.source()),
                    format!("{TOOL_FILES_ENV}={}", files.join(",")),
                ],
            ),
            Self::Worktree => ("scripts/scoped-test".to_string(), Vec::new()),
        }
    }
}

fn git(cwd: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .ok()?;
    output.status.success().then_some(output.stdout)
}

/// Refuse unresolved scoped-test inputs before toolchain probes or job effects.
pub(crate) fn validate_refs(
    cwd: &Path,
    params: &rsi_common::agent_jobs::JobParams,
) -> crate::error::Result<()> {
    let rsi_common::agent_jobs::JobParams::Test(test) = params else {
        return Ok(());
    };
    let Some(scoped) = &test.scoped_test else {
        return Ok(());
    };
    for (field, reference) in [
        ("base", scoped.base.as_str()),
        ("head", scoped.head.as_deref().unwrap_or("HEAD")),
    ] {
        if git(
            cwd,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{reference}^{{commit}}"),
            ],
        )
        .is_none()
        {
            return Err(crate::error::DaemonError::InvalidParam(format!(
                "{}: params.scoped_test.{field} {reference:?} does not resolve to a commit in {}; pass a full 40-hex SHA or a fetched ref",
                rsi_common::agent_jobs::JOB_SCOPED_TEST_REF_UNRESOLVED,
                cwd.display()
            )));
        }
    }
    Ok(())
}

/// The sibling file names a tool script's text loads next to itself: the
/// string argument of `.with_name("x")` and the right side of `.parent / "x"`.
/// A name with a path separator or a dot-dot is not a sibling and is skipped.
pub(super) fn referenced_helpers(text: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for marker in [".with_name(\"", ".parent / \""] {
        for (at, _) in text.match_indices(marker) {
            let rest = &text[at + marker.len()..];
            let Some(end) = rest.find('"') else {
                continue;
            };
            let name = &rest[..end];
            if name.is_empty() || name.contains('/') || name.contains("..") {
                continue;
            }
            names.insert(name.to_string());
        }
    }
    names
}

/// `scripts/<name>` for every file of the tool set: [`TOOL_FILES`] plus `extra`.
fn tool_paths(extra: &BTreeSet<String>) -> Vec<String> {
    let mut paths: Vec<String> = TOOL_FILES.iter().map(|file| (*file).to_string()).collect();
    for name in extra {
        let path = format!("scripts/{name}");
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    paths
}

/// The names (under `scripts/`) of a tool set, as the receipt lists them.
fn tool_names(paths: &[String]) -> Vec<String> {
    paths
        .iter()
        .map(|path| path.strip_prefix("scripts/").unwrap_or(path).to_string())
        .collect()
}

/// The helpers the tool scripts at `rev` reference, or none when the scripts
/// cannot be read (the floor [`TOOL_FILES`] still applies).
fn helpers_at(cwd: &Path, rev: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for script in TOOL_SCRIPTS {
        let object = format!("{rev}:{script}");
        let Some(text) = git(cwd, &["show", object.as_str()]) else {
            continue;
        };
        names.extend(referenced_helpers(&String::from_utf8_lossy(&text)));
    }
    names
}

/// The typed refusal for a tool script that loads a helper its own tree lacks.
fn helper_missing(file: &str, at: &str, who: &str) -> String {
    format!(
        "{JOB_SCOPED_TEST_TOOL_HELPER_MISSING}: {who} scripts/scoped-test or scripts/check-touched-shards loads {file} beside itself, but {at} has no such file; commit {file} with the change that loads it (the job will not silently run another copy of the tool)"
    )
}

/// A helper in `extra` that `rev` lacks, as a refusal naming the file.
fn first_missing(cwd: &Path, rev: &str, extra: &BTreeSet<String>, who: &str) -> Option<String> {
    extra.iter().find_map(|name| {
        let object = format!("{rev}:scripts/{name}");
        let present = git(cwd, &["cat-file", "-e", object.as_str()]).is_some();
        (!present).then(|| helper_missing(&format!("scripts/{name}"), rev, who))
    })
}

/// True when the candidate diff (`base...head`, the merge-base form
/// `scripts/check-touched-shards` uses) changes a file of `paths`.
fn candidate_changes_tool(cwd: &Path, base: &str, head: &str, paths: &[String]) -> Option<bool> {
    let range = format!("{base}...{head}");
    let mut args = vec!["diff", "--name-only", "-z", range.as_str(), "--"];
    args.extend(paths.iter().map(String::as_str));
    git(cwd, &args).map(|out| !out.is_empty())
}

/// True when `head` changed a file of `paths` on its own side of `rolling`
/// (`rolling...head`, from their merge base) and that file still differs from
/// rolling's (`rolling head`). A head merely behind rolling, or one whose tool
/// change has landed, is false; so is a pair Git cannot compare.
fn head_tool_differs_from_rolling(cwd: &Path, rolling: &str, head: &str, paths: &[String]) -> bool {
    let changed = |range: &[&str]| {
        let mut args = vec!["diff", "--name-only", "-z"];
        args.extend(range);
        args.push("--");
        args.extend(paths.iter().map(String::as_str));
        git(cwd, &args).is_some_and(|out| !out.is_empty())
    };
    let ahead = format!("{rolling}...{head}");
    changed(&[ahead.as_str()]) && changed(&[rolling, head])
}

/// The candidate's own copy, unless it loads a helper its head lacks.
fn candidate_tool(
    cwd: &Path,
    head: &str,
    extra: &BTreeSet<String>,
    why: CandidateWhy,
) -> Result<ScopedTool, String> {
    if let Some(refusal) = first_missing(cwd, head, extra, "the candidate's") {
        return Err(refusal);
    }
    Ok(ScopedTool::Candidate {
        files: tool_names(&tool_paths(extra)),
        why,
    })
}

/// Choose and, for the rolling copy, materialize the tool a job runs.
/// `cache_root` is the daemon-owned directory the revs live under.
///
/// The tool set is [`TOOL_FILES`] plus every helper the tool scripts name (the
/// rolling tip's and the candidate's), so a helper cannot be missing from the
/// copy a job runs just because this daemon's list predates it (hub #3024):
/// a candidate that changes any file of the set runs its own copy, the rolling
/// copy materializes the whole rolling set, and the receipt lists the set.
/// A candidate whose diff leaves the set alone but whose head carries its own
/// unlanded change to it (the change is in the base) runs its own copy too
/// (#279, [`head_tool_differs_from_rolling`]); the same helper check applies.
///
/// # Errors
/// `scoped_test_tool_helper_missing` (naming the file) when the copy that would
/// run loads a helper its own tree does not contain.
pub(super) fn resolve(
    cwd: &Path,
    base: &str,
    head: Option<&str>,
    cache_root: &Path,
) -> Result<ScopedTool, String> {
    resolve_with_policy(cwd, base, head, cache_root, false)
}

/// A forced rolling run refuses rather than silently using candidate code.
pub(super) fn resolve_rolling(
    cwd: &Path,
    base: &str,
    head: Option<&str>,
    cache_root: &Path,
) -> Result<ScopedTool, String> {
    resolve_with_policy(cwd, base, head, cache_root, true)
}

fn resolve_with_policy(
    cwd: &Path,
    base: &str,
    head: Option<&str>,
    cache_root: &Path,
    rolling_tool: bool,
) -> Result<ScopedTool, String> {
    let Some(rev) = git(
        cwd,
        &[
            "rev-parse",
            "--verify",
            "refs/remotes/origin/rolling^{commit}",
        ],
    )
    .and_then(|out| String::from_utf8(out).ok())
    .map(|rev| rev.trim().to_string())
    .filter(|rev| rev.len() >= 40 && rev.bytes().all(|b| b.is_ascii_hexdigit())) else {
        return Ok(ScopedTool::Worktree);
    };
    let head = head.unwrap_or("HEAD");
    let rolling_extra = helpers_at(cwd, &rev);
    let candidate_extra = helpers_at(cwd, head);
    let both: BTreeSet<String> = rolling_extra.union(&candidate_extra).cloned().collect();
    let watched = tool_paths(&both);
    // An unreadable diff (bad ref) leaves the choice to the candidate's own
    // script, which refuses the same ref with its own message.
    if !rolling_tool && candidate_changes_tool(cwd, base, head, &watched).unwrap_or(true) {
        return candidate_tool(cwd, head, &candidate_extra, CandidateWhy::DiffChangesTool);
    }
    if !rolling_tool && head_tool_differs_from_rolling(cwd, &rev, head, &watched) {
        return candidate_tool(
            cwd,
            head,
            &candidate_extra,
            CandidateWhy::DiffersFromRolling,
        );
    }
    if let Some(refusal) = first_missing(cwd, &rev, &rolling_extra, "the rolling tip's") {
        return Err(refusal);
    }
    let paths = tool_paths(&rolling_extra);
    match materialize(cwd, &rev, cache_root, &paths) {
        Ok(script) => Ok(ScopedTool::Rolling {
            script,
            rev,
            files: tool_names(&paths),
        }),
        Err(error) => {
            tracing::warn!(%error, "scoped-test tool materialization failed; using the sandbox copy");
            Ok(ScopedTool::Worktree)
        }
    }
}

/// The revision pins the bytes; the digest pins the full helper set, including
/// the daemon's [`TOOL_FILES`] floor. Sorting makes the key independent of list
/// order, and NUL delimiters keep distinct path lists unambiguous.
fn cache_key(rev: &str, paths: &[String]) -> String {
    let mut paths: Vec<&str> = paths.iter().map(String::as_str).collect();
    paths.sort_unstable();
    let mut digest = Sha256::new();
    for path in paths {
        digest.update(path.as_bytes());
        digest.update([0]);
    }
    format!("{rev}-{:x}", digest.finalize())
}

/// `<cache_root>/<rev>-<set digest>/scripts/scoped-test`, written read-only
/// from `rev`'s blobs for every file of `paths`. Each tool set has its own
/// immutable tree, so daemons with different helper sets never share one.
/// A partial tree is built in a private temp directory and renamed into place.
fn materialize(
    cwd: &Path,
    rev: &str,
    cache_root: &Path,
    paths: &[String],
) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let key = cache_key(rev, paths);
    let root = cache_root.join(&key);
    let script = root.join("scripts/scoped-test");
    // Even a matching key must hold the complete set before it can be reused.
    let holds_set = |dir: &Path| {
        dir.join(".complete").is_file() && paths.iter().all(|path| dir.join(path).is_file())
    };
    if holds_set(&root) {
        return Ok(script);
    }
    std::fs::create_dir_all(cache_root)?;
    let tmp = cache_root.join(format!(
        ".tmp-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let result = (|| {
        std::fs::create_dir_all(tmp.join("scripts"))?;
        for file in paths {
            let object = format!("{rev}:{file}");
            let bytes = git(cwd, &["show", object.as_str()])
                .ok_or_else(|| std::io::Error::other(format!("git show {object} failed")))?;
            let path = tmp.join(file);
            std::fs::write(&path, bytes)?;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o555))?;
        }
        std::fs::write(tmp.join(".complete"), rev)?;
        move_short_tree_aside(cache_root, &root, &holds_set);
        match std::fs::rename(&tmp, &root) {
            Ok(()) => Ok(()),
            // A concurrent job won the race; its tree is the same bytes.
            Err(_) if holds_set(&root) => Ok(()),
            Err(error) => Err(error),
        }
    })();
    if result.is_err() || tmp.exists() {
        let _ = std::fs::remove_dir_all(&tmp);
    }
    result?;
    prune(cache_root, &key);
    Ok(script)
}

/// Move a tree that does not hold the whole set aside (a job that already runs
/// from it keeps its open files) so the full set takes its place.
fn move_short_tree_aside(cache_root: &Path, root: &Path, holds_set: &dyn Fn(&Path) -> bool) {
    if !root.exists() || holds_set(root) {
        return;
    }
    let stale = cache_root.join(format!(".stale-{}", uuid::Uuid::new_v4()));
    if std::fs::rename(root, &stale).is_err() {
        return;
    }
    let _ = std::fs::remove_dir_all(&stale);
}

/// Remove all but the newest [`KEEP_REVS`] materialized revs (never `keep`).
fn prune(cache_root: &Path, keep: &str) {
    let Ok(entries) = std::fs::read_dir(cache_root) else {
        return;
    };
    let mut revs: Vec<(std::time::SystemTime, PathBuf)> = entries
        .filter_map(std::result::Result::ok)
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let (rev, digest) = name.split_once('-').unwrap_or((&name, ""));
            let is_hex = |value: &str| value.bytes().all(|byte| byte.is_ascii_hexdigit());
            name != keep
                && rev.len() == 40
                && is_hex(rev)
                && (digest.is_empty() || (digest.len() == 64 && is_hex(digest)))
        })
        .filter_map(|entry| Some((entry.metadata().ok()?.modified().ok()?, entry.path())))
        .collect();
    revs.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, path) in revs.into_iter().skip(KEEP_REVS - 1) {
        let _ = std::fs::remove_dir_all(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    /// A repo whose `origin/rolling` holds a tool at `rolling_text`, checked
    /// out on a branch whose HEAD holds `candidate_text`; returns (dir, rolling rev).
    fn repo(rolling_text: &str, candidate_text: &str) -> (tempfile::TempDir, String) {
        let dir = crate::test_support::disk_backed_tempdir("scoped-tool");
        let path = dir.path();
        run(path, &["init", "-q", "-b", "main"]);
        std::fs::create_dir_all(path.join("scripts")).unwrap();
        for file in TOOL_FILES {
            std::fs::write(path.join(file), format!("{file} {rolling_text}\n")).unwrap();
        }
        std::fs::write(path.join("other.txt"), "one\n").unwrap();
        run(path, &["add", "."]);
        run(path, &["commit", "-q", "-m", "tool"]);
        let rolling = run(path, &["rev-parse", "HEAD"]);
        run(
            path,
            &["update-ref", "refs/remotes/origin/rolling", &rolling],
        );
        if candidate_text != rolling_text {
            std::fs::write(path.join("scripts/scoped-test"), candidate_text).unwrap();
        } else {
            std::fs::write(path.join("other.txt"), "two\n").unwrap();
        }
        run(path, &["commit", "-q", "-a", "-m", "candidate"]);
        (dir, rolling)
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn an_old_sandbox_copy_is_not_used_the_rolling_copy_is_materialized_read_only() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, rolling) = repo("rolling", "rolling");
        let cache = crate::test_support::disk_backed_tempdir("scoped-tool-cache");
        let tool = resolve(dir.path(), "origin/rolling", Some("HEAD"), cache.path()).unwrap();
        let ScopedTool::Rolling { script, rev, files } = &tool else {
            panic!("expected the rolling copy, got {tool:?}");
        };
        assert_eq!(rev, &rolling);
        // The fixture's scripts name no helper, so the set is the TOOL_FILES floor.
        let floor: Vec<String> = TOOL_FILES
            .iter()
            .map(|file| file.strip_prefix("scripts/").unwrap().to_string())
            .collect();
        assert_eq!(files, &floor);
        let root = cache
            .path()
            .join(cache_key(&rolling, &tool_paths(&BTreeSet::new())));
        assert_eq!(script, &root.join("scripts/scoped-test"));
        // Every helper sits next to the script, byte-identical, read-only.
        for file in TOOL_FILES {
            let copy = root.join(file);
            assert_eq!(
                std::fs::read_to_string(&copy).unwrap(),
                format!("{file} rolling\n")
            );
            assert_eq!(
                std::fs::metadata(&copy).unwrap().permissions().mode() & 0o222,
                0
            );
        }
        // The launch names the copy for the receipt and runs it by absolute path.
        let (argv0, env) = tool.launch();
        assert_eq!(argv0, script.display().to_string());
        assert_eq!(
            env,
            vec![
                "RSI_SCOPED_TEST_TOOL_SOURCE=rolling".to_string(),
                format!("RSI_SCOPED_TEST_TOOL_REV={rolling}"),
                format!("RSI_SCOPED_TEST_TOOL_FILES={}", floor.join(","))
            ]
        );
        // A second resolve reuses the same tree.
        assert_eq!(
            resolve(dir.path(), "origin/rolling", None, cache.path()).unwrap(),
            tool
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_candidate_that_edits_the_tool_runs_its_own_copy() {
        let (dir, _) = repo("rolling", "candidate edit");
        let cache = crate::test_support::disk_backed_tempdir("scoped-tool-cache");
        let tool = resolve(dir.path(), "origin/rolling", Some("HEAD"), cache.path()).unwrap();
        assert_eq!(
            tool,
            ScopedTool::Candidate {
                files: tool_names(&tool_paths(&BTreeSet::new())),
                why: CandidateWhy::DiffChangesTool,
            }
        );
        let (argv0, env) = tool.launch();
        assert_eq!(argv0, "scripts/scoped-test");
        assert_eq!(env[0], "RSI_SCOPED_TEST_TOOL_SOURCE=candidate");
        assert_eq!(env.len(), 2);
        // Nothing was materialized.
        assert!(std::fs::read_dir(cache.path()).unwrap().next().is_none());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn synthetic_green_direct_and_inherited_tool_edits_require_a_rolling_rerun() {
        use crate::store::agent_jobs::NewAgentJob;
        use rsi_common::agent_jobs::{JobParams, JobState, JobWake, ScopedToolSource};
        use rsi_common::agent_landing_candidate::{
            CANDIDATE_TOOL_UNTRUSTED, assess_candidate, assess_candidate_with_alternatives,
        };
        for inherited in [false, true] {
            for forged in [false, true] {
                let (dir, _) = repo("rolling", "rolling");
                let path = dir.path();
                let script = |claim: &str| {
                    format!(
                        "import json, subprocess\nr = {{'ok': True, 'head': subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip(), 'new_failures': []}}\n{claim}\nprint('SCOPED_TEST_RECEIPT ' + json.dumps(r))\n"
                    )
                };
                let rolling = commit(
                    path,
                    &[("scripts/scoped-test", &script("# trusted rolling fixture"))],
                    "rolling tool",
                );
                run(
                    path,
                    &["update-ref", "refs/remotes/origin/rolling", &rolling],
                );
                let candidate = commit(
                    path,
                    &[(
                        "scripts/scoped-test",
                        &script(if forged {
                            "r['tool'] = {'source': 'rolling', 'rev': 'forged'}"
                        } else {
                            "# omit tool fields"
                        }),
                    )],
                    "synthetic green tool",
                );
                let base = if inherited {
                    commit(
                        path,
                        &[("other.txt", "worker edit\n")],
                        "worker on unlanded tool",
                    );
                    candidate.as_str()
                } else {
                    rolling.as_str()
                };
                let head = run(path, &["rev-parse", "HEAD"]);
                let cache = crate::test_support::disk_backed_tempdir("scoped-tool-provenance");
                let tool = resolve(path, base, None, cache.path()).unwrap();
                assert!(
                    matches!(tool, ScopedTool::Candidate { why, .. } if why == if inherited { CandidateWhy::DiffersFromRolling } else { CandidateWhy::DiffChangesTool })
                );
                let store = crate::store::Store::open_in_memory().unwrap();
                let owner = uuid::Uuid::new_v4();
                let execute = |tool: &ScopedTool| {
                    let provenance = tool.provenance(path).unwrap();
                    let id = uuid::Uuid::new_v4();
                    let (row, _) = store
                        .insert_agent_job(
                            &NewAgentJob {
                                id,
                                owner_session_id: owner,
                                project_id: None,
                                name: None,
                                params: serde_json::from_value::<JobParams>(
                                    serde_json::json!({"kind":"test", "scoped_test":{"base":base}}),
                                )
                                .unwrap(),
                                cwd: path.display().to_string(),
                                unit_name: format!("rsi-job-{id}"),
                                log_path: path.join(format!("{id}.log")).display().to_string(),
                                status_path: path
                                    .join(format!("{id}.status"))
                                    .display()
                                    .to_string(),
                                idempotency_key: None,
                                wake: JobWake::None,
                            },
                            chrono::Utc::now(),
                        )
                        .unwrap();
                    store.record_agent_job_scoped_tool(id, &provenance).unwrap();
                    let running = store.get_agent_job(id).unwrap().unwrap().job;
                    assert_eq!(
                        running.result.unwrap().scoped_tool,
                        Some(provenance.clone())
                    );
                    let output = Command::new("python3")
                        .arg(&provenance.path)
                        .current_dir(path)
                        .output()
                        .unwrap();
                    assert!(output.status.success(), "{output:?}");
                    let (state, result) = super::super::classify(
                        &row.job,
                        Some(0),
                        &String::from_utf8(output.stdout).unwrap(),
                    );
                    assert_eq!(state, JobState::Succeeded);
                    store
                        .settle_agent_job(id, state, &result, false, chrono::Utc::now())
                        .unwrap();
                    let job = store.get_agent_job(id).unwrap().unwrap().job;
                    assert_eq!(job.result.as_ref().unwrap().scoped_tool, Some(provenance));
                    job
                };
                let candidate_job = execute(&tool);
                let refused = assess_candidate(&candidate_job, &head);
                assert_eq!(refused.ok, false);
                assert_eq!(refused.refusal.as_deref(), Some(CANDIDATE_TOOL_UNTRUSTED));
                assert_eq!(
                    refused.scoped_tool.unwrap().source,
                    ScopedToolSource::Candidate
                );
                let rolling_tool = resolve_rolling(path, base, None, cache.path()).unwrap();
                let rolling_job = execute(&rolling_tool);
                let accepted =
                    assess_candidate_with_alternatives(&candidate_job, [&rolling_job], &head);
                assert!(accepted.ok, "{accepted:?}");
                assert_eq!(accepted.job_id, Some(rolling_job.id));
                assert_eq!(
                    accepted.scoped_tool.unwrap().source,
                    ScopedToolSource::Rolling
                );
                assert_eq!(accepted.tool_under_test.unwrap().job_id, candidate_job.id);
            }
        }
    }

    /// Commit every `(file, text)` write in `dir` and return the new HEAD.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    fn commit(dir: &Path, writes: &[(&str, &str)], message: &str) -> String {
        for (file, text) in writes {
            std::fs::write(dir.join(file), text).unwrap();
        }
        run(dir, &["add", "."]);
        run(dir, &["commit", "-q", "-m", message]);
        run(dir, &["rev-parse", "HEAD"])
    }

    /// #279: the worker built on an unlanded tool change (it is in the base, so
    /// `base...head` does not touch the tool). Its head's tool differs from
    /// rolling's on its own side, so its copy runs and the receipt says why.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_tool_change_in_the_base_not_on_rolling_runs_the_candidate_copy() {
        let (dir, rolling) = repo("rolling", "rolling");
        let path = dir.path();
        // HEAD of the fixture is one non-tool commit on rolling; restart from
        // rolling: the base adds the tool change, the head one more commit.
        run(path, &["reset", "-q", "--hard", &rolling]);
        let base = commit(
            path,
            &[("scripts/scoped-test", "linux_check lane\n")],
            "tool change (unlanded)",
        );
        commit(path, &[("other.txt", "worker\n")], "worker change");
        let cache = crate::test_support::disk_backed_tempdir("scoped-tool-cache");
        let tool = resolve(path, &base, Some("HEAD"), cache.path()).unwrap();
        assert_eq!(
            tool,
            ScopedTool::Candidate {
                files: tool_names(&tool_paths(&BTreeSet::new())),
                why: CandidateWhy::DiffersFromRolling,
            }
        );
        let (argv0, env) = tool.launch();
        assert_eq!(argv0, "scripts/scoped-test");
        assert_eq!(
            env[0],
            "RSI_SCOPED_TEST_TOOL_SOURCE=candidate_differs_from_rolling"
        );
        assert!(std::fs::read_dir(cache.path()).unwrap().next().is_none());
    }

    /// A sandbox merely behind rolling (rolling's tool advanced, the worker
    /// never touched it) still runs the rolling copy, the point of #1938.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_sandbox_behind_a_rolling_tool_change_runs_the_rolling_copy() {
        let (dir, old) = repo("old", "old");
        let path = dir.path();
        let worker_head = run(path, &["rev-parse", "HEAD"]);
        // Rolling advances from the old tip with a tool fix the worker lacks.
        run(path, &["reset", "-q", "--hard", &old]);
        let rolling = commit(path, &[("scripts/scoped-test", "rolling fix\n")], "fix");
        run(
            path,
            &["update-ref", "refs/remotes/origin/rolling", &rolling],
        );
        run(path, &["reset", "-q", "--hard", &worker_head]);
        let cache = crate::test_support::disk_backed_tempdir("scoped-tool-cache");
        // Both the usual base and the worker's own old base pick rolling.
        for base in ["origin/rolling", old.as_str()] {
            let tool = resolve(path, base, Some("HEAD"), cache.path()).unwrap();
            let ScopedTool::Rolling { script, rev, .. } = &tool else {
                panic!("expected the rolling copy for base {base}, got {tool:?}");
            };
            assert_eq!(rev, &rolling);
            assert_eq!(std::fs::read_to_string(script).unwrap(), "rolling fix\n");
        }
    }

    /// Once the tool change has landed (the head's tool equals rolling's), the
    /// rolling copy runs again.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_landed_tool_change_in_the_base_runs_the_rolling_copy() {
        let (dir, rolling) = repo("rolling", "rolling");
        let path = dir.path();
        run(path, &["reset", "-q", "--hard", &rolling]);
        let base = commit(path, &[("scripts/scoped-test", "landed lane\n")], "tool");
        run(path, &["update-ref", "refs/remotes/origin/rolling", &base]);
        commit(path, &[("other.txt", "worker\n")], "worker change");
        let cache = crate::test_support::disk_backed_tempdir("scoped-tool-cache");
        let tool = resolve(path, &base, Some("HEAD"), cache.path()).unwrap();
        assert!(
            matches!(&tool, ScopedTool::Rolling { rev, .. } if rev == &base),
            "{tool:?}"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn without_a_rolling_ref_the_sandbox_copy_runs_unchanged() {
        // The system temp dir lies outside any checkout; the fixture root of
        // `disk_backed_tempdir` sits inside this sandbox's own repository.
        let dir = tempfile::tempdir().unwrap();
        let cache = crate::test_support::disk_backed_tempdir("scoped-tool-cache");
        // Not a git checkout at all, and a checkout without origin/rolling.
        assert_eq!(
            resolve(dir.path(), "origin/rolling", None, cache.path()),
            Ok(ScopedTool::Worktree)
        );
        run(dir.path(), &["init", "-q", "-b", "main"]);
        assert_eq!(
            resolve(dir.path(), "origin/rolling", None, cache.path()),
            Ok(ScopedTool::Worktree)
        );
        let (argv0, env) = ScopedTool::Worktree.launch();
        assert_eq!(argv0, "scripts/scoped-test");
        assert!(env.is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn old_revs_are_pruned_and_the_newest_survives() {
        let cache = crate::test_support::disk_backed_tempdir("scoped-tool-prune");
        // Prune both legacy SHA-only directories and the current keyed sets.
        let names: Vec<String> = (0..KEEP_REVS + 3)
            .map(|n| {
                let rev = format!("{n:040x}");
                if n % 2 == 0 {
                    rev
                } else {
                    cache_key(&rev, &tool_paths(&BTreeSet::new()))
                }
            })
            .collect();
        for name in &names {
            std::fs::create_dir_all(cache.path().join(name)).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
        let keep = names.last().unwrap();
        prune(cache.path(), keep);
        let left = std::fs::read_dir(cache.path()).unwrap().count();
        assert_eq!(left, KEEP_REVS);
        assert!(cache.path().join(keep).is_dir());
        assert!(!cache.path().join(&names[0]).exists());
    }

    /// The scanner finds every helper the real scripts load beside themselves and
    /// each one exists, so a new helper needs no edit to [`TOOL_FILES`] (hub #3024).
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_scanner_finds_every_helper_the_real_scripts_load_beside_themselves() {
        let root = Path::new(rsi_common::manifest_dir!()).join("../..");
        let mut found = BTreeSet::new();
        for script in TOOL_SCRIPTS {
            let text = std::fs::read_to_string(root.join(script)).unwrap();
            found.extend(referenced_helpers(&text));
        }
        for file in TOOL_FILES {
            assert!(root.join(file).is_file(), "{file} is missing");
        }
        for name in &found {
            assert!(
                root.join("scripts").join(name).is_file(),
                "the tool scripts load scripts/{name}, which does not exist"
            );
        }
        // The floor is what the scanner finds today: a loader it stops
        // recognizing would silently shrink the derived set.
        for loaded in [
            "check-touched-shards",
            "python-test-gate.py",
            "target-pool",
            "e2e-prebuild-bins.txt",
        ] {
            assert!(
                found.contains(loaded),
                "the scanner no longer finds {loaded}"
            );
            assert!(
                TOOL_FILES.iter().any(|file| file.ends_with(loaded)),
                "{loaded} is not in TOOL_FILES"
            );
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_scanner_reads_with_name_and_parent_siblings_only() {
        let text = r#"
spec = spec_from_file_location("x", SCRIPT.with_name("one.py"))
POOL = Path(__file__).resolve().parent / "two.txt"
scratch = target.with_name(target.name + ".partial")
guard = Path(__file__).resolve().parents[1] / "tools/guard.py"
nested = SCRIPT.with_name("sub/three.py")
up = SCRIPT.with_name("../four.py")
"#;
        let names: Vec<String> = referenced_helpers(text).into_iter().collect();
        assert_eq!(names, ["one.py", "two.txt"]);
    }

    /// A repo whose `origin/rolling` holds the default tool files plus
    /// `rolling` overrides, checked out on a branch whose HEAD adds `candidate`
    /// writes. Returns (dir, rolling rev).
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    fn repo_files(
        rolling: &[(&str, &str)],
        candidate: &[(&str, &str)],
    ) -> (tempfile::TempDir, String) {
        let dir = crate::test_support::disk_backed_tempdir("scoped-tool");
        let path = dir.path();
        run(path, &["init", "-q", "-b", "main"]);
        std::fs::create_dir_all(path.join("scripts")).unwrap();
        for file in TOOL_FILES {
            std::fs::write(path.join(file), format!("{file} base\n")).unwrap();
        }
        std::fs::write(path.join("other.txt"), "one\n").unwrap();
        for (file, text) in rolling {
            std::fs::write(path.join(file), text).unwrap();
        }
        run(path, &["add", "."]);
        run(path, &["commit", "-q", "-m", "tool"]);
        let rev = run(path, &["rev-parse", "HEAD"]);
        run(path, &["update-ref", "refs/remotes/origin/rolling", &rev]);
        std::fs::write(path.join("other.txt"), "two\n").unwrap();
        for (file, text) in candidate {
            std::fs::write(path.join(file), text).unwrap();
        }
        run(path, &["add", "."]);
        run(path, &["commit", "-q", "-m", "candidate"]);
        (dir, rev)
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    const LOADS_NEW_HELPER: &str = "gate = load(SCRIPT.with_name(\"new-helper.py\"))\n";

    /// The #337 shape: a candidate adds a new helper and a scoped-test that loads
    /// it. It runs its own copy, and the receipt list names the helper.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_candidate_that_adds_a_helper_its_scoped_test_loads_runs_it_and_names_it() {
        let (dir, _) = repo_files(
            &[],
            &[
                ("scripts/scoped-test", LOADS_NEW_HELPER),
                ("scripts/new-helper.py", "print('lane')\n"),
            ],
        );
        let cache = crate::test_support::disk_backed_tempdir("scoped-tool-cache");
        let tool = resolve(dir.path(), "origin/rolling", Some("HEAD"), cache.path()).unwrap();
        let ScopedTool::Candidate { files, .. } = &tool else {
            panic!("expected the candidate copy, got {tool:?}");
        };
        assert!(files.contains(&"new-helper.py".to_string()), "{files:?}");
        let (_, env) = tool.launch();
        assert!(
            env.iter()
                .any(|entry| entry.starts_with(TOOL_FILES_ENV) && entry.contains("new-helper.py")),
            "{env:?}"
        );
        // Nothing was taken from rolling.
        assert!(std::fs::read_dir(cache.path()).unwrap().next().is_none());
    }

    /// A candidate whose scoped-test loads a helper it never committed is refused
    /// with the file named; the rolling copy is not silently run instead.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_candidate_that_loads_an_uncommitted_helper_is_refused_naming_the_file() {
        let (dir, _) = repo_files(&[], &[("scripts/scoped-test", LOADS_NEW_HELPER)]);
        let cache = crate::test_support::disk_backed_tempdir("scoped-tool-cache");
        let error = resolve(dir.path(), "origin/rolling", Some("HEAD"), cache.path()).unwrap_err();
        assert!(
            error.starts_with(JOB_SCOPED_TEST_TOOL_HELPER_MISSING),
            "{error}"
        );
        assert!(error.contains("scripts/new-helper.py"), "{error}");
        assert!(std::fs::read_dir(cache.path()).unwrap().next().is_none());
    }

    /// The deployed daemon's list predates the helper: rolling's scoped-test loads
    /// it, so the rolling copy materializes it anyway and the receipt names it.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_helper_rollings_script_loads_is_materialized_though_the_daemon_list_lacks_it() {
        let (dir, rev) = repo_files(
            &[
                ("scripts/scoped-test", LOADS_NEW_HELPER),
                ("scripts/new-helper.py", "print('lane')\n"),
            ],
            &[],
        );
        let cache = crate::test_support::disk_backed_tempdir("scoped-tool-cache");
        let tool = resolve(dir.path(), "origin/rolling", Some("HEAD"), cache.path()).unwrap();
        let ScopedTool::Rolling { script, files, .. } = &tool else {
            panic!("expected the rolling copy, got {tool:?}");
        };
        assert!(files.contains(&"new-helper.py".to_string()), "{files:?}");
        assert_eq!(tool.launch().1[1], format!("{TOOL_REV_ENV}={rev}"));
        let copy = script.with_file_name("new-helper.py");
        assert_eq!(std::fs::read_to_string(copy).unwrap(), "print('lane')\n");
    }

    /// A candidate that edits only a helper rolling's scoped-test loads (one this
    /// daemon's list lacks) runs its own copy, not the rolling bytes.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_candidate_that_edits_a_helper_rollings_script_loads_runs_its_own_copy() {
        let (dir, _) = repo_files(
            &[
                ("scripts/scoped-test", LOADS_NEW_HELPER),
                ("scripts/new-helper.py", "print('lane')\n"),
            ],
            &[("scripts/new-helper.py", "print('changed lane')\n")],
        );
        let cache = crate::test_support::disk_backed_tempdir("scoped-tool-cache");
        let tool = resolve(dir.path(), "origin/rolling", Some("HEAD"), cache.path()).unwrap();
        let ScopedTool::Candidate { files, .. } = &tool else {
            panic!("expected the candidate copy, got {tool:?}");
        };
        assert!(files.contains(&"new-helper.py".to_string()), "{files:?}");
    }

    /// Rolling's own script loads a helper rolling lacks: refused with the file named.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_helper_missing_from_the_rolling_tree_is_refused_naming_the_file() {
        let (dir, _) = repo_files(&[("scripts/scoped-test", LOADS_NEW_HELPER)], &[]);
        let cache = crate::test_support::disk_backed_tempdir("scoped-tool-cache");
        let error = resolve(dir.path(), "origin/rolling", Some("HEAD"), cache.path()).unwrap_err();
        assert!(
            error.starts_with(JOB_SCOPED_TEST_TOOL_HELPER_MISSING),
            "{error}"
        );
        assert!(error.contains("scripts/new-helper.py"), "{error}");
    }

    /// A legacy SHA-only tree remains separate from the complete keyed tree.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_legacy_cached_tree_gets_a_separate_complete_tool_set() {
        let (dir, rev) = repo_files(
            &[
                ("scripts/scoped-test", LOADS_NEW_HELPER),
                ("scripts/new-helper.py", "print('lane')\n"),
            ],
            &[],
        );
        let cache = crate::test_support::disk_backed_tempdir("scoped-tool-cache");
        let stale = cache.path().join(&rev);
        std::fs::create_dir_all(stale.join("scripts")).unwrap();
        for file in TOOL_FILES {
            std::fs::write(stale.join(file), "old").unwrap();
        }
        std::fs::write(stale.join(".complete"), &rev).unwrap();
        let tool = resolve(dir.path(), "origin/rolling", Some("HEAD"), cache.path()).unwrap();
        let ScopedTool::Rolling { script, .. } = tool else {
            panic!("expected the rolling copy, got {tool:?}");
        };
        assert!(script.with_file_name("new-helper.py").is_file());
        assert_eq!(std::fs::read_to_string(script).unwrap(), LOADS_NEW_HELPER);
        assert_eq!(
            std::fs::read_to_string(stale.join("scripts/scoped-test")).unwrap(),
            "old"
        );
    }

    /// A daemon's expanded TOOL_FILES floor at the same rolling SHA gets a
    /// complete new tree, while the earlier set stays immutable and reusable.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn different_helper_sets_at_one_revision_materialize_separate_complete_trees() {
        let (dir, rev) = repo_files(&[("scripts/new-helper.py", "print('lane')\n")], &[]);
        let cache = crate::test_support::disk_backed_tempdir("scoped-tool-cache");
        let floor = tool_paths(&BTreeSet::new());
        let old = materialize(dir.path(), &rev, cache.path(), &floor).unwrap();
        let mut expanded = floor.clone();
        expanded.push("scripts/new-helper.py".to_string());
        let new = materialize(dir.path(), &rev, cache.path(), &expanded).unwrap();
        assert_ne!(old, new);
        for file in &expanded {
            let copy = new
                .parent()
                .unwrap()
                .join(file.strip_prefix("scripts/").unwrap());
            let expected = git(dir.path(), &["show", &format!("{rev}:{file}")]).unwrap();
            assert_eq!(std::fs::read(copy).unwrap(), expected);
        }
        assert_eq!(
            materialize(dir.path(), &rev, cache.path(), &floor).unwrap(),
            old
        );
        expanded.reverse();
        assert_eq!(
            materialize(dir.path(), &rev, cache.path(), &expanded).unwrap(),
            new
        );
        assert_eq!(std::fs::read_dir(cache.path()).unwrap().count(), 2);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn an_incomplete_keyed_cache_is_rebuilt_with_every_helper() {
        let (dir, rev) = repo("rolling", "rolling");
        let cache = crate::test_support::disk_backed_tempdir("scoped-tool-cache");
        let paths = tool_paths(&BTreeSet::new());
        let root = cache.path().join(cache_key(&rev, &paths));
        std::fs::create_dir_all(root.join("scripts")).unwrap();
        std::fs::write(root.join("scripts/scoped-test"), "incomplete").unwrap();
        std::fs::write(root.join(".complete"), &rev).unwrap();
        let script = materialize(dir.path(), &rev, cache.path(), &paths).unwrap();
        assert_eq!(script, root.join("scripts/scoped-test"));
        for file in paths {
            assert_eq!(
                std::fs::read_to_string(root.join(&file)).unwrap(),
                format!("{file} rolling\n")
            );
        }
    }
}
