use super::*;
use similar::{Algorithm, DiffOp, capture_diff_slices_deadline};
use std::collections::{HashMap, HashSet};
use std::process::Stdio;
use tokio::{io::AsyncReadExt, process::Command};
const MAX_BYTES: u64 = 2 * 1024 * 1024;

// #978: evidence bounds. Production refuses a hung git command, a
// pathological diff or an overlong proof with these wall-clock limits. Test
// builds use generous limits so a busy landing host cannot turn a correctness
// test into a timing race; dedicated tests drive each refusal explicitly.
#[cfg(not(test))]
const GIT_COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
#[cfg(test)]
const GIT_COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
#[cfg(not(test))]
const DIFF_DEADLINE: std::time::Duration = std::time::Duration::from_secs(2);
#[cfg(test)]
const DIFF_DEADLINE: std::time::Duration = std::time::Duration::from_secs(60);
#[cfg(not(test))]
const ACCEPTED_CONTENT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
#[cfg(test)]
const ACCEPTED_CONTENT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

/// Longest stderr kept to classify a failure; the rest is drained and dropped.
const STDERR_CLASSIFY_BYTES: u64 = 4096;
/// Longest wait for a killed evidence command to be reaped.
const REAP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Whether a failed evidence command failed because a required object is not
/// in the local object store. Local-only reads (`GIT_NO_LAZY_FETCH`) refuse a
/// promised object instead of fetching it.
fn missing_local_object(stderr: &[u8]) -> bool {
    let text = String::from_utf8_lossy(stderr).to_ascii_lowercase();
    [
        "lazy fetching disabled",
        "unable to read",
        "bad object",
        "bad file",
        "missing blob",
        "missing tree",
        "missing commit",
        "promisor",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

/// One evidence git command, confined to local objects: no lazy promisor
/// fetch, no transport, no credential helper or prompt, and no replacement
/// or graft history. The child runs in its own process group so a timeout
/// kills every descendant, and the group is reaped before this returns.
async fn run_bounded(
    root: &Path,
    args: &[&str],
    timeout: std::time::Duration,
) -> Result<(std::process::ExitStatus, Vec<u8>, Vec<u8>)> {
    run_reach(root, args, timeout, Reach::Local).await
}

/// Whether an evidence command may talk to the named `origin`. Only the two
/// freshness probes (`ls-remote` and the private-ref `fetch`) are `Origin`;
/// every object read is `Local`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Reach {
    Local,
    Origin,
}

async fn run_reach(
    root: &Path,
    args: &[&str],
    timeout: std::time::Duration,
    reach: Reach,
) -> Result<(std::process::ExitStatus, Vec<u8>, Vec<u8>)> {
    let mut command = Command::new("git");
    command.args([
        "--no-optional-locks",
        "-c",
        "core.fsmonitor=false",
        "-c",
        "core.hooksPath=/dev/null",
    ]);
    if reach == Reach::Local {
        // No transport and no credential helper or prompt for object reads.
        command.args([
            "-c",
            "protocol.allow=never",
            "-c",
            "credential.helper=",
            "-c",
            "core.askPass=",
        ]);
    }
    command
        .arg("-C")
        .arg(root)
        .args(args)
        // Evidence names canonical objects, never locally substituted history.
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_GRAFT_FILE", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        // Local objects only: a missing promised object is an error, never a
        // fetch through a remote, transport or credential helper (#389).
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_ASKPASS",
        "SSH_ASKPASS",
        "GIT_SSH",
        "GIT_SSH_COMMAND",
        "GIT_PROXY_COMMAND",
        "GIT_EXTERNAL_DIFF",
    ] {
        command.env_remove(key);
    }
    crate::process_control::configure_tokio_process_group(
        &mut command,
        crate::process_control::ProcessContainment::Group,
    )
    .map_err(|_| refused("manager_v2_git_unavailable"))?;
    let mut child = command
        .spawn()
        .map_err(|_| refused("manager_v2_git_unavailable"))?;
    let pgid = child
        .id()
        .and_then(|pid| i32::try_from(pid).ok())
        .map(nix::unistd::Pid::from_raw);
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| refused("manager_v2_git_unavailable"))?
        .take(MAX_BYTES + 1);
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| refused("manager_v2_git_unavailable"))?;
    let outcome = tokio::time::timeout(timeout, async {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let (out_read, err_read) = tokio::join!(stdout.read_to_end(&mut out), async {
            let mut capped = (&mut stderr).take(STDERR_CLASSIFY_BYTES);
            let read = capped.read_to_end(&mut err).await;
            // Keep draining so a chatty command never blocks on a full pipe.
            let _ = tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await;
            read
        });
        out_read.map_err(|_| refused("manager_v2_git_read"))?;
        err_read.map_err(|_| refused("manager_v2_git_read"))?;
        if out.len() as u64 > MAX_BYTES {
            return Err(refused("manager_v2_git_output_limit"));
        }
        let status = child
            .wait()
            .await
            .map_err(|_| refused("manager_v2_git_wait"))?;
        Ok((status, out, err))
    })
    .await;
    match outcome {
        Ok(Ok(done)) => Ok(done),
        failed => {
            // Timeout or an over-limit read: kill the whole group, then reap.
            if let Some(pgid) = pgid {
                crate::process_control::terminate_process_group(pgid);
            }
            let _ = tokio::time::timeout(REAP_TIMEOUT, child.wait()).await;
            match failed {
                Ok(Err(error)) => Err(error),
                _ => Err(refused("manager_v2_git_timeout")),
            }
        }
    }
}

async fn run(root: &Path, args: &[&str]) -> Result<(std::process::ExitStatus, Vec<u8>)> {
    let (status, bytes, _) = run_bounded(root, args, GIT_COMMAND_TIMEOUT).await?;
    Ok((status, bytes))
}

pub(super) async fn read(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    read_within(root, args, GIT_COMMAND_TIMEOUT).await
}

/// A freshness probe that talks to `origin` (still no lazy promisor fetch,
/// same bounds and process-group cleanup).
async fn read_origin(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let (status, bytes, _) = run_reach(root, args, GIT_COMMAND_TIMEOUT, Reach::Origin).await?;
    if !status.success() {
        return Err(refused("manager_v2_git_failed"));
    }
    Ok(bytes)
}

pub(super) async fn read_within(
    root: &Path,
    args: &[&str],
    timeout: std::time::Duration,
) -> Result<Vec<u8>> {
    let (status, bytes, stderr) = run_bounded(root, args, timeout).await?;
    if !status.success() {
        return Err(refused(if missing_local_object(&stderr) {
            "manager_v2_git_missing_local_object"
        } else {
            "manager_v2_git_failed"
        }));
    }
    Ok(bytes)
}
pub(crate) async fn head(root: &Path, reference: &str) -> Result<String> {
    let bytes = read(
        root,
        &["rev-parse", "--verify", &format!("{reference}^{{commit}}")],
    )
    .await?;
    let head = String::from_utf8_lossy(&bytes).trim().to_string();
    if !canonical_sha(&head) {
        return Err(refused("manager_v2_invalid_commit"));
    }
    Ok(head)
}
pub(crate) async fn clean(root: &Path) -> Result<()> {
    if !read(root, &["status", "--porcelain=v1", "-z"])
        .await?
        .is_empty()
    {
        return Err(refused("manager_v2_dirty_custody"));
    }
    Ok(())
}
pub(super) async fn check_custody(root: &Path, branch: &str) -> Result<()> {
    let actual = read(root, &["symbolic-ref", "--short", "HEAD"]).await?;
    if String::from_utf8_lossy(&actual).trim() != branch {
        return Err(refused("manager_v2_custody_branch_changed"));
    }
    Ok(())
}
pub(crate) async fn custody(c: &crate::store::sandbox_custody::PersistedCustody) -> Result<()> {
    let root = Path::new(&c.sandbox_root);
    if tokio::fs::canonicalize(root)
        .await
        .map_err(|_| refused("manager_v2_custody_unavailable"))?
        != root
    {
        return Err(refused("manager_v2_custody_path_changed"));
    }
    check_custody(root, &c.sandbox_branch).await?;
    let common = read(
        root,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .await?;
    let common =
        String::from_utf8(common).map_err(|_| refused("manager_v2_custody_repository_changed"))?;
    let actual = tokio::fs::canonicalize(common.trim())
        .await
        .map_err(|_| refused("manager_v2_custody_repository_changed"))?;
    let expected = c
        .repository_identity
        .strip_prefix("git-common-dir:")
        .unwrap_or(&c.repository_identity);
    if actual != Path::new(expected) {
        return Err(refused("manager_v2_custody_repository_changed"));
    }
    Ok(())
}
/// Run the same commit-bound seal rule used by infrastructure relaunch.
/// Git work is synchronous and bounded, so keep it off the async executor.
pub(super) async fn sealed_source_holds(root: &Path, commit: &str, head: &str) -> Result<()> {
    let root = root.to_path_buf();
    let commit = commit.to_owned();
    let head = head.to_owned();
    tokio::task::spawn_blocking(move || {
        crate::sandbox::git_worktree::review_sealed_source_holds_bounded(&root, &commit, &head)
    })
    .await
    .map_err(|_| refused("manager_v2_git_unavailable"))?
}
pub(super) async fn repository(root: &Path) -> Result<()> {
    if tokio::fs::canonicalize(root)
        .await
        .map_err(|_| refused("manager_v2_custody_repository_changed"))?
        != root
    {
        return Err(refused("manager_v2_custody_repository_changed"));
    }
    let common = read(
        root,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .await?;
    let common =
        String::from_utf8(common).map_err(|_| refused("manager_v2_custody_repository_changed"))?;
    if tokio::fs::canonicalize(common.trim())
        .await
        .map_err(|_| refused("manager_v2_custody_repository_changed"))?
        != root
    {
        return Err(refused("manager_v2_custody_repository_changed"));
    }
    Ok(())
}
pub(super) async fn ancestor(root: &Path, source: &str, target: &str) -> Result<bool> {
    let (status, _, stderr) = run_bounded(
        root,
        &["merge-base", "--is-ancestor", source, target],
        GIT_COMMAND_TIMEOUT,
    )
    .await?;
    match status.code() {
        Some(0) => Ok(true),
        // Exit 1 is the only "not an ancestor" answer.
        Some(1) => Ok(false),
        // Both operands are commit ids here, so a commit git cannot resolve
        // locally is a missing promised object, not a generic git failure.
        _ if missing_local_object(&stderr)
            || String::from_utf8_lossy(&stderr)
                .to_ascii_lowercase()
                .contains("not a valid commit name") =>
        {
            Err(refused("manager_v2_git_missing_local_object"))
        }
        _ => Err(refused("manager_v2_git_failed")),
    }
}

pub(super) async fn content_blob(root: &Path, commit: &str, path: &str) -> Result<Option<Vec<u8>>> {
    let object = format!("{commit}:{path}");
    // Resolve through trees only: a path that is absent is `None`, while a
    // promised object that is not local stays a typed missing-object error.
    let (status, _, stderr) = run_bounded(
        root,
        &["rev-parse", "--verify", "--quiet", &object],
        GIT_COMMAND_TIMEOUT,
    )
    .await?;
    if !status.success() {
        if missing_local_object(&stderr) {
            return Err(refused("manager_v2_git_missing_local_object"));
        }
        return Ok(None);
    }
    read(root, &["show", &object]).await.map(Some)
}

fn lines(blob: &[u8]) -> Vec<&[u8]> {
    blob.split_inclusive(|byte| *byte == b'\n').collect()
}

fn equal_covering(ops: &[DiffOp], old: std::ops::Range<usize>) -> bool {
    // Target-only insertions split equal spans without removing source lines.
    let mut covered_until = old.start;
    for op in ops {
        if !matches!(op, DiffOp::Equal { .. }) {
            continue;
        }
        let equal = op.old_range();
        if equal.end <= covered_until {
            continue;
        }
        if equal.start > covered_until {
            return false;
        }
        covered_until = equal.end;
        if covered_until >= old.end {
            return true;
        }
    }
    false
}

fn restored_base_line(ops: &[DiffOp], changed: std::ops::Range<usize>) -> bool {
    ops.iter().any(|op| {
        matches!(op, DiffOp::Equal { .. })
            && op.old_range().start < changed.end
            && changed.start < op.old_range().end
    })
}

fn text_content_survives(base: &[u8], source: &[u8], target: &[u8]) -> Result<bool> {
    text_content_survives_within(base, source, target, DIFF_DEADLINE)
}

pub(super) fn text_content_survives_within(
    base: &[u8],
    source: &[u8],
    target: &[u8],
    limit: std::time::Duration,
) -> Result<bool> {
    let old = lines(base);
    let accepted = lines(source);
    let published = lines(target);
    let deadline = std::time::Instant::now() + limit;
    let changes = capture_diff_slices_deadline(Algorithm::Myers, &old, &accepted, Some(deadline));
    let old_to_target =
        capture_diff_slices_deadline(Algorithm::Myers, &old, &published, Some(deadline));
    let source_to_target =
        capture_diff_slices_deadline(Algorithm::Myers, &accepted, &published, Some(deadline));
    if std::time::Instant::now() >= deadline {
        return Err(refused("manager_v2_accepted_content_ambiguous"));
    }
    let survives = changes.into_iter().all(|change| {
        if matches!(change, DiffOp::Equal { .. }) {
            return true;
        }
        let old_range = change.old_range();
        let new_range = change.new_range();
        // A restored base span proves that an accepted edit or deletion was lost.
        if !old_range.is_empty() && restored_base_line(&old_to_target, old_range) {
            return false;
        }
        // An insertion or replacement must remain positively identifiable.
        // Ambiguity refuses admission rather than treating ancestry as content.
        new_range.is_empty() || equal_covering(&source_to_target, new_range)
    });
    if std::time::Instant::now() >= deadline {
        return Err(refused("manager_v2_accepted_content_ambiguous"));
    }
    Ok(survives)
}

/// For a source delivered by a landing merge, its first parent is the rolling
/// state before publication. Comparing the source to their merge base excludes
/// earlier work on rolling and source-side rolling merges. The caller must
/// separately check source content shared with that parent: a prior merge may
/// have dropped it even though its commit remains reachable.
///
/// Direct fast-forward publication and later content checks have no such
/// landing boundary. Retain the custody-base/merged-parent rule for them.
async fn content_base_selection(
    root: &Path,
    base: &str,
    source: &str,
    target: &str,
) -> Result<(String, Option<String>)> {
    if source != target {
        let raw = read(root, &["rev-list", "--parents", "-n", "1", target]).await?;
        let history = std::str::from_utf8(&raw)
            .map_err(|_| refused("manager_v2_accepted_content_unsupported"))?;
        let parents = history.split_ascii_whitespace().collect::<Vec<_>>();
        if parents.len() == 3
            && parents[0] == target
            && canonical_sha(parents[1])
            && canonical_sha(parents[2])
            && ancestor(root, source, parents[2]).await?
            && !ancestor(root, source, parents[1]).await?
        {
            let raw = read(root, &["merge-base", "--all", source, parents[1]]).await?;
            let bases = std::str::from_utf8(&raw)
                .map_err(|_| refused("manager_v2_accepted_content_unsupported"))?
                .split_ascii_whitespace()
                .collect::<Vec<_>>();
            if bases.is_empty() || bases.len() > 2 || !bases.iter().all(|base| canonical_sha(base))
            {
                return Err(refused("manager_v2_accepted_content_ambiguous"));
            }
            if bases.len() == 2 {
                // Criss-cross history has two incomparable commit bases. Git's
                // recursive merge builds their shared virtual tree; refuse a
                // conflicted merge rather than selecting the convenient base.
                // Only the base on the source's first-parent path can carry
                // earlier source work. The virtual tree also imports rolling
                // changes, which must not be attributed to this source.
                let mut source_side_base = None;
                for candidate in &bases {
                    if on_first_parent_path(root, candidate, source).await? {
                        if source_side_base.replace(*candidate).is_some() {
                            return Err(refused("manager_v2_accepted_content_ambiguous"));
                        }
                    }
                }
                let source_side_base = source_side_base
                    .ok_or_else(|| refused("manager_v2_accepted_content_ambiguous"))?;
                let rolling_side_base = bases
                    .iter()
                    .copied()
                    .find(|candidate| *candidate != source_side_base)
                    .ok_or_else(|| refused("manager_v2_accepted_content_ambiguous"))?;
                if !ancestor(root, base, source_side_base).await? {
                    return Err(refused("manager_v2_accepted_content_ambiguous"));
                }
                let first_parent = read(
                    root,
                    &[
                        "rev-list",
                        "--first-parent",
                        "--parents",
                        &format!("{base}..{source_side_base}"),
                    ],
                )
                .await?;
                let mut inspected_commits = 0;
                for line in std::str::from_utf8(&first_parent)
                    .map_err(|_| refused("manager_v2_accepted_content_ambiguous"))?
                    .lines()
                {
                    let parents = line.split_ascii_whitespace().collect::<Vec<_>>();
                    if parents.is_empty() {
                        continue;
                    }
                    inspected_commits += 1;
                    if inspected_commits > 256
                        || ancestor(root, parents[0], rolling_side_base).await?
                    {
                        // A source fast-forward through already published
                        // rolling history has no merge commit to mark imported
                        // prefix paths. That history can include rolling's
                        // second-parent side branches.
                        return Err(refused("manager_v2_accepted_content_ambiguous"));
                    }
                    match parents.as_slice() {
                        [_, _] => {}
                        [_, _, second] if canonical_sha(second) => {
                            // A source-side worker merge remains attributable
                            // to the source. A merge that imports the rolling
                            // side can carry content that rolling later evolves.
                            let shared =
                                read(root, &["merge-base", "--all", second, rolling_side_base])
                                    .await?;
                            let shared = std::str::from_utf8(&shared)
                                .map_err(|_| refused("manager_v2_accepted_content_ambiguous"))?;
                            if shared
                                .split_ascii_whitespace()
                                .any(|shared| !canonical_sha(shared))
                                || shared.trim().is_empty()
                            {
                                return Err(refused("manager_v2_accepted_content_ambiguous"));
                            }
                            for shared in shared.split_ascii_whitespace() {
                                if !ancestor(root, shared, base).await? {
                                    return Err(refused("manager_v2_accepted_content_ambiguous"));
                                }
                            }
                        }
                        _ => return Err(refused("manager_v2_accepted_content_ambiguous")),
                    }
                }
                let (status, raw) =
                    run(root, &["merge-tree", "--write-tree", bases[0], bases[1]]).await?;
                if !status.success() {
                    return Err(refused("manager_v2_accepted_content_ambiguous"));
                }
                let tree = std::str::from_utf8(&raw)
                    .map_err(|_| refused("manager_v2_accepted_content_ambiguous"))?
                    .trim();
                if !canonical_sha(tree) || read(root, &["cat-file", "-t", tree]).await? != b"tree\n"
                {
                    return Err(refused("manager_v2_accepted_content_ambiguous"));
                }
                return Ok((tree.to_owned(), Some(source_side_base.to_owned())));
            }
            return Ok((bases[0].to_owned(), Some(bases[0].to_owned())));
        }
    }
    let source_history = read(
        root,
        &[
            "rev-list",
            "--first-parent",
            "--parents",
            &format!("{base}..{source}"),
        ],
    )
    .await?;
    for line in source_history.split(|byte| *byte == b'\n') {
        let parents: Vec<&[u8]> = line.split(|byte| *byte == b' ').collect();
        if parents.len() != 3 {
            continue;
        }
        let parent = std::str::from_utf8(parents[2])
            .map_err(|_| refused("manager_v2_accepted_content_unsupported"))?;
        if canonical_sha(parent)
            && ancestor(root, base, parent).await?
            && ancestor(root, parent, target).await?
        {
            return Ok((parent.to_owned(), None));
        }
    }
    Ok((base.to_owned(), None))
}

pub(super) async fn content_base(
    root: &Path,
    base: &str,
    source: &str,
    target: &str,
) -> Result<String> {
    Ok(content_base_selection(root, base, source, target).await?.0)
}

async fn on_first_parent_path(root: &Path, commit: &str, source: &str) -> Result<bool> {
    let history = read(root, &["rev-list", "--first-parent", source]).await?;
    Ok(history
        .split(|byte| *byte == b'\n')
        .any(|ancestor| ancestor == commit.as_bytes()))
}

/// Find when the common source ancestor first entered the target's rolling
/// line. Content shared with the target before that crossing is earlier
/// published work; novel content must survive at the crossing itself.
async fn shared_prefix_entry(
    root: &Path,
    base: &str,
    content_base: &str,
    target: &str,
) -> Result<Option<(String, String)>> {
    let parent = head(root, &format!("{target}^1")).await?;
    if on_first_parent_path(root, content_base, &parent).await? {
        return Ok(None);
    }
    let raw = read(
        root,
        &[
            "rev-list",
            "--first-parent",
            "--reverse",
            "--ancestry-path",
            &format!("{content_base}..{parent}"),
        ],
    )
    .await?;
    let entry = std::str::from_utf8(&raw)
        .map_err(|_| refused("manager_v2_accepted_content_unsupported"))?
        .split_ascii_whitespace()
        .next()
        .ok_or_else(|| refused("manager_v2_accepted_content_ambiguous"))?;
    if !canonical_sha(entry) || !ancestor(root, content_base, entry).await? {
        return Err(refused("manager_v2_accepted_content_ambiguous"));
    }
    let before_entry = head(root, &format!("{entry}^1")).await?;
    if ancestor(root, content_base, &before_entry).await? {
        return Err(refused("manager_v2_accepted_content_ambiguous"));
    }
    let raw = read(root, &["merge-base", "--all", content_base, &before_entry]).await?;
    let bases = std::str::from_utf8(&raw)
        .map_err(|_| refused("manager_v2_accepted_content_unsupported"))?
        .split_ascii_whitespace()
        .collect::<Vec<_>>();
    if bases.len() != 1 || !canonical_sha(bases[0]) {
        return Err(refused("manager_v2_accepted_content_ambiguous"));
    }
    let crossing_base = if ancestor(root, base, bases[0]).await? {
        bases[0]
    } else {
        base
    };
    Ok(Some((crossing_base.to_owned(), entry.to_owned())))
}

/// A source can remain an ancestor after a forward revert. Compare only its
/// attributable changes with the exact target before recording integration.
pub(super) async fn accepted_content(
    root: &Path,
    base: &str,
    source: &str,
    target: &str,
) -> Result<()> {
    accepted_content_within(root, base, source, target, ACCEPTED_CONTENT_TIMEOUT).await
}

pub(super) async fn accepted_content_within(
    root: &Path,
    base: &str,
    source: &str,
    target: &str,
    limit: std::time::Duration,
) -> Result<()> {
    tokio::time::timeout(limit, accepted_content_inner(root, base, source, target))
        .await
        .map_err(|_| refused("manager_v2_accepted_content_ambiguous"))?
}

async fn accepted_content_inner(root: &Path, base: &str, source: &str, target: &str) -> Result<()> {
    if !canonical_sha(base) || !canonical_sha(source) || !canonical_sha(target) {
        return Err(refused("manager_v2_invalid_commit"));
    }
    if !ancestor(root, base, source).await? {
        return Err(refused("manager_v2_accepted_base_mismatch"));
    }
    let (content_base, prefix_base) = content_base_selection(root, base, source, target).await?;
    if let Some(prefix_base) = prefix_base.filter(|prefix_base| base != prefix_base) {
        if ancestor(root, base, &prefix_base).await?
            && on_first_parent_path(root, &prefix_base, source).await?
        {
            // The shared prefix may contain work that crossed into the target's
            // first-parent history before this landing. Check only the content
            // novel at that crossing, at the commit where it was published.
            if let Some((crossing_base, entry)) =
                shared_prefix_entry(root, base, &prefix_base, target).await?
            {
                let prefix = changed_paths(root, &crossing_base, &prefix_base).await?;
                let paths: Vec<&[u8]> = prefix
                    .split(|byte| *byte == 0)
                    .filter(|path| !path.is_empty())
                    .collect();
                verify_content_paths(root, &crossing_base, &prefix_base, &entry, &paths).await?;

                // Recheck prefix paths changed by this landing. The accepted
                // source may itself evolve an earlier published path; in that
                // case its version must survive. Otherwise the landing must
                // preserve the version already on rolling's first parent.
                let parent = head(root, &format!("{target}^1")).await?;
                let landing_changes = changed_paths(root, &parent, target).await?;
                let landing_paths: HashSet<&[u8]> = landing_changes
                    .split(|byte| *byte == 0)
                    .filter(|path| !path.is_empty())
                    .collect();
                let changed_prefix_paths: Vec<&[u8]> = paths
                    .into_iter()
                    .filter(|path| landing_paths.contains(path))
                    .collect();
                let source_changes = changed_paths(root, &prefix_base, source).await?;
                let source_paths: HashSet<&[u8]> = source_changes
                    .split(|byte| *byte == 0)
                    .filter(|path| !path.is_empty())
                    .collect();
                let (source_modified, rolling_only): (Vec<_>, Vec<_>) = changed_prefix_paths
                    .into_iter()
                    .partition(|path| source_paths.contains(path));
                verify_content_paths(root, &crossing_base, source, target, &source_modified)
                    .await?;
                verify_content_paths(root, &crossing_base, &parent, target, &rolling_only).await?;
            }
        }
    }
    let changed = changed_paths(root, &content_base, source).await?;
    if changed.is_empty() {
        return Err(refused("manager_v2_accepted_content_empty"));
    }
    let paths: Vec<&[u8]> = changed
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .collect();
    match verify_content_paths(root, &content_base, source, target, &paths).await {
        Err(error)
            if error
                .to_string()
                .contains("manager_v2_accepted_content_lost") =>
        {
            match provisional_mapping(root, &content_base, source, target).await? {
                None => Err(error),
                Some(mapping) => {
                    verify_mapped_content_paths(
                        root,
                        &content_base,
                        source,
                        target,
                        &paths,
                        &mapping,
                    )
                    .await
                }
            }
        }
        result => result,
    }
}

/// #984: the set of paths changed between two commits (or refs). A missing
/// ref or any Git failure is an error, so a caller proving disjointness
/// fails closed.
pub(super) async fn changed_path_set(
    root: &Path,
    from: &str,
    to: &str,
) -> Result<std::collections::BTreeSet<Vec<u8>>> {
    Ok(changed_paths(root, from, to)
        .await?
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(<[u8]>::to_vec)
        .collect())
}

async fn changed_paths(root: &Path, base: &str, source: &str) -> Result<Vec<u8>> {
    let changed = read(
        root,
        &[
            "diff",
            "--no-ext-diff",
            "--no-renames",
            "--name-only",
            "-z",
            base,
            source,
        ],
    )
    .await?;
    Ok(changed)
}

pub(super) async fn verify_content_paths(
    root: &Path,
    base: &str,
    source: &str,
    target: &str,
    paths: &[&[u8]],
) -> Result<()> {
    for raw in paths {
        std::str::from_utf8(raw).map_err(|_| refused("manager_v2_accepted_content_unsupported"))?;
    }
    if paths.is_empty() {
        return Ok(());
    }

    // Tree equality proves unchanged paths without per-blob Git subprocesses.
    // Git output and the whole proof still have bounded, fail-closed deadlines.
    let changed = changed_paths(root, source, target).await?;
    let divergent: HashSet<&[u8]> = changed
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .collect();
    for raw in paths {
        if !divergent.contains(raw) {
            continue;
        }
        let path = std::str::from_utf8(raw).expect("candidate paths validated above");
        let after = content_blob(root, source, path).await?;
        let now = content_blob(root, target, path).await?;
        if after == now {
            continue;
        }
        let Some(after) = after else {
            return Err(refused("manager_v2_accepted_content_lost"));
        };
        let Some(now) = now else {
            return Err(refused("manager_v2_accepted_content_lost"));
        };
        let before = content_blob(root, base, path).await?;
        let before = before.unwrap_or_default();
        if before.contains(&0) || after.contains(&0) || now.contains(&0) {
            return Err(refused("manager_v2_accepted_content_unsupported"));
        }
        if !text_content_survives(&before, &after, &now)? {
            return Err(refused("manager_v2_accepted_content_lost"));
        }
    }
    Ok(())
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ProvisionalDeclaration {
    schema_version: i64,
    version: i64,
    files: Vec<ProvisionalFile>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ProvisionalFile {
    path: String,
    path_template: String,
    source_blob: String,
    sites: Vec<ProvisionalSite>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ProvisionalSite {
    anchor: String,
    replacement: String,
    scope: ProvisionalScope,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "lowercase")]
enum ProvisionalScope {
    Unit,
    Head,
}

struct ProvisionalMapping {
    source: String,
    declaration: ProvisionalDeclaration,
    unit_version: i64,
    source_inventory: serde_json::Value,
    target_inventory: serde_json::Value,
}

fn provisional_proof_failed() -> crate::error::DaemonError {
    refused("manager_v2_provisional_proof_failed")
}

fn provisional_proof_ambiguous() -> crate::error::DaemonError {
    refused("manager_v2_provisional_proof_ambiguous")
}

fn safe_declaration_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\\')
        && !path.contains('\0')
        && path.split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && part.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'/' | b'-')
                })
        })
        && path.split('/').filter(|part| *part != ".git").count() == path.split('/').count()
}

async fn json_blob(root: &Path, commit: &str, path: &str) -> Result<serde_json::Value> {
    let bytes = content_blob(root, commit, path)
        .await?
        .ok_or_else(provisional_proof_failed)?;
    serde_json::from_slice(&bytes).map_err(|_| provisional_proof_failed())
}

fn object(value: &serde_json::Value) -> Result<&serde_json::Map<String, serde_json::Value>> {
    value.as_object().ok_or_else(provisional_proof_failed)
}

fn inventory_latest(value: &serde_json::Value) -> Result<i64> {
    object(value)?
        .get("latest_schema_version")
        .and_then(serde_json::Value::as_i64)
        .ok_or_else(provisional_proof_failed)
}

fn inventory_map<'a>(
    value: &'a serde_json::Value,
    key: &str,
) -> Result<&'a serde_json::Map<String, serde_json::Value>> {
    object(value)?
        .get(key)
        .and_then(serde_json::Value::as_object)
        .ok_or_else(provisional_proof_failed)
}

fn sha256_prefix(bytes: &[u8]) -> String {
    use sha2::Digest;
    format!("sha256:{}", hex::encode(sha2::Sha256::digest(bytes)))
}

async fn provisional_mapping(
    root: &Path,
    content_base: &str,
    source: &str,
    target: &str,
) -> Result<Option<ProvisionalMapping>> {
    let changed = changed_paths(root, content_base, source).await?;
    let mut declarations = Vec::new();
    for raw in changed
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        let path = std::str::from_utf8(raw)
            .map_err(|_| refused("manager_v2_accepted_content_unsupported"))?;
        if path.starts_with("tools/provisional-migrations/")
            && path.ends_with(".json")
            && content_blob(root, content_base, path).await?.is_none()
        {
            declarations.push(path.to_owned());
        }
    }
    match declarations.len() {
        0 => return Ok(None),
        1 => {}
        _ => return Err(provisional_proof_ambiguous()),
    }
    let declaration_path = declarations.pop().expect("declaration count checked");
    let declaration_bytes = content_blob(root, source, &declaration_path)
        .await?
        .ok_or_else(provisional_proof_failed)?;
    if declaration_bytes.len() > 512 * 1024 {
        return Err(provisional_proof_failed());
    }
    let declaration: ProvisionalDeclaration =
        serde_json::from_slice(&declaration_bytes).map_err(|_| provisional_proof_failed())?;
    if declaration.schema_version != 1 {
        return Err(provisional_proof_failed());
    }

    let raw = read(
        root,
        &[
            "rev-list",
            "--parents",
            "--ancestry-path",
            &format!("{source}..{target}"),
        ],
    )
    .await?;
    let history =
        String::from_utf8(raw).map_err(|_| refused("manager_v2_accepted_content_unsupported"))?;
    let mut landings = Vec::new();
    for line in history.lines() {
        let fields = line.split_ascii_whitespace().collect::<Vec<_>>();
        if fields.len() == 3 && fields[2] == source {
            let landing = fields[0];
            let parent = fields[1];
            if canonical_sha(landing)
                && canonical_sha(parent)
                && !ancestor(root, source, parent).await?
            {
                landings.push((landing.to_owned(), parent.to_owned()));
            }
        }
    }
    let (landing, parent) = match landings.as_slice() {
        [] => return Ok(None),
        [landing] => (landing.0.clone(), landing.1.clone()),
        _ => return Err(provisional_proof_ambiguous()),
    };

    let source_inventory = json_blob(root, source, "tools/released-migrations.json").await?;
    let parent_inventory = json_blob(root, &parent, "tools/released-migrations.json").await?;
    let landing_inventory = json_blob(root, &landing, "tools/released-migrations.json").await?;
    let target_inventory = json_blob(root, target, "tools/released-migrations.json").await?;
    let old = declaration.version;
    if inventory_latest(&source_inventory)? != old
        || !inventory_map(&source_inventory, "blocks")?.contains_key(&old.to_string())
    {
        return Err(provisional_proof_failed());
    }
    let unit_version = inventory_latest(&parent_inventory)? + 1;
    if unit_version < old || inventory_latest(&landing_inventory)? != unit_version {
        return Err(provisional_proof_failed());
    }

    if declaration.files.is_empty() || declaration.files.len() > 128 {
        return Err(provisional_proof_failed());
    }
    let total_sites: usize = declaration.files.iter().map(|file| file.sites.len()).sum();
    if total_sites > 4096 {
        return Err(provisional_proof_failed());
    }
    let mut destinations = HashSet::new();
    let mut store_declared = false;
    for file in &declaration.files {
        let destination = file
            .path_template
            .replace("${VERSION}", &unit_version.to_string());
        let old_path = file.path_template.replace("${VERSION}", &old.to_string());
        if !safe_declaration_path(&file.path)
            || !safe_declaration_path(&old_path)
            || !safe_declaration_path(&destination)
            || old_path != file.path
        {
            return Err(provisional_proof_failed());
        }
        if !destinations.insert(destination) {
            return Err(provisional_proof_failed());
        }
        store_declared |= holds_migration_block(&file.path, old);
        let blob = content_blob(root, source, &file.path)
            .await?
            .ok_or_else(provisional_proof_failed)?;
        if blob.contains(&0) || sha256_prefix(&blob) != file.source_blob {
            return Err(provisional_proof_failed());
        }
        let mut covered = vec![false; blob.len()];
        for site in &file.sites {
            if site.anchor.is_empty()
                || site.replacement.contains('\0')
                || !site.replacement.contains("${VERSION}")
                || site.replacement.replace("${VERSION}", &old.to_string()) != site.anchor
            {
                return Err(provisional_proof_failed());
            }
            let anchor = site.anchor.as_bytes();
            let Some(offset) = blob
                .windows(anchor.len())
                .position(|window| window == anchor)
            else {
                return Err(provisional_proof_failed());
            };
            if blob
                .windows(anchor.len())
                .filter(|window| *window == anchor)
                .count()
                != 1
                || covered[offset..offset + anchor.len()]
                    .iter()
                    .any(|covered| *covered)
            {
                return Err(provisional_proof_failed());
            }
            covered[offset..offset + anchor.len()].fill(true);
        }
    }
    if !store_declared {
        return Err(provisional_proof_failed());
    }
    Ok(Some(ProvisionalMapping {
        source: source.to_owned(),
        declaration,
        unit_version,
        source_inventory,
        target_inventory,
    }))
}

fn render_mapped(
    file: &ProvisionalFile,
    unit_version: i64,
    head_version: i64,
    source: &[u8],
) -> Result<Vec<u8>> {
    let mut replacements = Vec::new();
    for site in &file.sites {
        let anchor = site.anchor.as_bytes();
        if source
            .windows(anchor.len())
            .filter(|window| *window == anchor)
            .count()
            != 1
        {
            return Err(provisional_proof_failed());
        }
        let offset = source
            .windows(anchor.len())
            .position(|window| window == anchor)
            .expect("anchor count checked");
        let version = match site.scope {
            ProvisionalScope::Unit => unit_version,
            ProvisionalScope::Head => head_version,
        };
        replacements.push((
            offset,
            anchor.len(),
            site.replacement
                .replace("${VERSION}", &version.to_string())
                .into_bytes(),
        ));
    }
    replacements.sort_by_key(|(offset, _, _)| std::cmp::Reverse(*offset));
    let mut mapped = source.to_vec();
    for (offset, anchor_len, replacement) in replacements {
        mapped.splice(offset..offset + anchor_len, replacement);
    }
    Ok(mapped)
}

/// Split like the Python inventory guard's `splitlines(keepends=True)`.
/// A lone carriage return would split differently there, so refuse it.
pub(super) fn normalized_lines(blob: &[u8]) -> Result<Vec<&[u8]>> {
    let carriage_returns = blob.iter().filter(|byte| **byte == b'\r').count();
    if carriage_returns != blob.windows(2).filter(|window| window == b"\r\n").count() {
        return Err(provisional_proof_failed());
    }
    Ok(lines(blob))
}

/// The guard's `BLOCK_RE` is `^(?P<indent>\s*)if version < (?P<version>\d+) \{\s*$`;
/// the block ends at the first later line equal to `<indent>}`.
pub(super) fn mapped_block<'a>(mapped: &'a [&'a [u8]], version: i64) -> Result<&'a [&'a [u8]]> {
    let opener = format!("if version < {version} {{");
    let start = mapped
        .iter()
        .position(|line| String::from_utf8_lossy(line).trim() == opener)
        .ok_or_else(provisional_proof_failed)?;
    let indent = String::from_utf8_lossy(mapped[start])
        .chars()
        .take_while(|character| character.is_whitespace())
        .collect::<String>();
    let end = mapped[start + 1..]
        .iter()
        .position(|line| {
            let text = String::from_utf8_lossy(line);
            text.trim_end_matches(['\r', '\n']) == format!("{indent}}}")
        })
        .map(|offset| start + offset + 1)
        .ok_or_else(provisional_proof_failed)?;
    Ok(&mapped[start..=end])
}

const SECTION_BEGIN: &str = "// RSI-RELEASED-MIGRATION-BEGIN: ";
const SECTION_END: &str = "// RSI-RELEASED-MIGRATION-END: ";

/// Locate one protected section exactly as the guard's `BEGIN_RE`/`END_RE`
/// (`^\s*// RSI-RELEASED-MIGRATION-BEGIN: <name>\s*$`) would.
pub(super) fn section_location(lines: &[&[u8]], name: &str) -> Result<(usize, usize)> {
    let begin = format!("{SECTION_BEGIN}{name}");
    let end = format!("{SECTION_END}{name}");
    let marked = |wanted: &str| {
        lines
            .iter()
            .enumerate()
            .filter(|(_, line)| String::from_utf8_lossy(line).trim() == wanted)
            .map(|(index, _)| index)
            .collect::<Vec<_>>()
    };
    match (marked(&begin).as_slice(), marked(&end).as_slice()) {
        ([start], [stop]) if start < stop => Ok((*start, *stop)),
        _ => Err(provisional_proof_failed()),
    }
}

async fn verify_mapped_inventory(
    mapping: &ProvisionalMapping,
    root: &Path,
    content_base: &str,
) -> Result<()> {
    let base = json_blob(root, content_base, "tools/released-migrations.json").await?;
    let source = &mapping.source_inventory;
    let published = &mapping.target_inventory;
    let base_object = object(&base)?;
    let source_object = object(source)?;
    let target_object = object(published)?;
    if inventory_latest(published)? < mapping.unit_version {
        return Err(refused("manager_v2_accepted_content_lost"));
    }
    let base_blocks = inventory_map(&base, "blocks")?;
    let source_blocks = inventory_map(source, "blocks")?;
    let target_blocks = inventory_map(published, "blocks")?;
    for (version, fingerprint) in base_blocks {
        if source_blocks.get(version) != Some(fingerprint) {
            return Err(provisional_proof_failed());
        }
    }
    let base_sections = inventory_map(&base, "protected_sections")?;
    let source_sections = inventory_map(source, "protected_sections")?;
    for (name, section) in base_sections {
        if source_sections.get(name) != Some(section) {
            return Err(provisional_proof_failed());
        }
    }
    for (key, value) in source_object {
        if !matches!(
            key.as_str(),
            "latest_schema_version" | "blocks" | "protected_sections"
        ) && base_object.get(key) != Some(value)
            && target_object.get(key) != Some(value)
        {
            return Err(refused("manager_v2_accepted_content_lost"));
        }
    }

    let mut file_map: HashMap<String, &ProvisionalFile> = HashMap::new();
    let mut source_blobs = HashMap::new();
    for file in &mapping.declaration.files {
        file_map.insert(file.path.clone(), file);
        let blob = content_blob(root, &mapping.source, &file.path).await?;
        let blob = blob.ok_or_else(provisional_proof_failed)?;
        source_blobs.insert(&file.path, blob);
    }
    let store = mapping
        .declaration
        .files
        .iter()
        .find(|file| holds_migration_block(&file.path, mapping.declaration.version))
        .ok_or_else(provisional_proof_failed)?;
    let store_source = &source_blobs[&store.path];
    let mapped_store = render_mapped(
        store,
        mapping.unit_version,
        inventory_latest(published)?,
        store_source,
    )?;
    let mapped_store_lines = normalized_lines(&mapped_store)?;
    let block = mapped_block(mapped_store_lines.as_slice(), mapping.unit_version)?;
    let target_block = target_blocks
        .get(&mapping.unit_version.to_string())
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| refused("manager_v2_accepted_content_lost"))?;
    if target_block != sha256_prefix(&block.concat()) {
        return Err(refused("manager_v2_accepted_content_lost"));
    }
    for (version, fingerprint) in source_blocks {
        if version != &mapping.declaration.version.to_string()
            && !base_blocks.contains_key(version)
            && target_blocks.get(version) != Some(fingerprint)
        {
            return Err(refused("manager_v2_accepted_content_lost"));
        }
    }

    for (name, fingerprint) in source_sections {
        if base_sections.contains_key(name) {
            continue;
        }
        let fingerprint_object = object(fingerprint)?;
        let path = fingerprint_object
            .get("path")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(provisional_proof_failed)?;
        // A section in an undeclared file carries no version sites: its
        // accepted text and path must survive unchanged.
        let (source_blob, mapped, destination) = match file_map.get(path) {
            Some(file) => {
                let source_blob = source_blobs[&file.path].clone();
                let mapped = render_mapped(
                    file,
                    mapping.unit_version,
                    inventory_latest(published)?,
                    &source_blob,
                )?;
                let destination = file
                    .path_template
                    .replace("${VERSION}", &mapping.unit_version.to_string());
                (source_blob, mapped, destination)
            }
            None => {
                let source_blob = content_blob(root, &mapping.source, path)
                    .await?
                    .ok_or_else(provisional_proof_failed)?;
                (source_blob.clone(), source_blob, path.to_owned())
            }
        };
        let source_lines = normalized_lines(&source_blob)?;
        let (start, stop) = section_location(source_lines.as_slice(), name)?;
        let mapped_lines = normalized_lines(&mapped)?;
        if mapped_lines.len() != source_lines.len() {
            return Err(provisional_proof_failed());
        }
        let begin_text = String::from_utf8_lossy(mapped_lines[start]);
        let mapped_name = begin_text
            .trim()
            .strip_prefix(SECTION_BEGIN)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .ok_or_else(provisional_proof_failed)?;
        let expected = serde_json::json!({
            "path": destination,
            "sha256": sha256_prefix(&mapped_lines[start..=stop].concat()),
        });
        if target_object
            .get("protected_sections")
            .and_then(serde_json::Value::as_object)
            .and_then(|sections| sections.get(mapped_name))
            != Some(&expected)
        {
            return Err(refused("manager_v2_accepted_content_lost"));
        }
    }
    Ok(())
}

async fn verify_mapped_content_paths(
    root: &Path,
    content_base: &str,
    source: &str,
    target: &str,
    paths: &[&[u8]],
    mapping: &ProvisionalMapping,
) -> Result<()> {
    for raw in paths {
        std::str::from_utf8(raw).map_err(|_| refused("manager_v2_accepted_content_unsupported"))?;
    }
    let mut file_map: HashMap<String, &ProvisionalFile> = HashMap::new();
    for file in &mapping.declaration.files {
        file_map.insert(file.path.clone(), file);
    }
    let head_version = inventory_latest(&mapping.target_inventory)?;
    // Undeclared paths, including the declaration itself, keep the ordinary
    // line check; one call shares its source/target tree comparison.
    let mut ordinary = Vec::new();
    for raw in paths {
        let path = std::str::from_utf8(raw).expect("candidate paths validated above");
        if path == "tools/released-migrations.json" {
            verify_mapped_inventory(mapping, root, content_base).await?;
            continue;
        }
        let Some(file) = file_map.get(path) else {
            ordinary.push(*raw);
            continue;
        };
        let source_blob = content_blob(root, source, path)
            .await?
            .ok_or_else(provisional_proof_failed)?;
        let mapped = render_mapped(file, mapping.unit_version, head_version, &source_blob)?;
        let destination = file
            .path_template
            .replace("${VERSION}", &mapping.unit_version.to_string());
        let now = content_blob(root, target, &destination).await?;
        if now.as_deref() == Some(mapped.as_slice()) {
            continue;
        }
        let Some(now) = now else {
            return Err(refused("manager_v2_accepted_content_lost"));
        };
        let before = content_blob(root, content_base, path)
            .await?
            .unwrap_or_default();
        if before.contains(&0) || mapped.contains(&0) || now.contains(&0) {
            return Err(refused("manager_v2_accepted_content_unsupported"));
        }
        if !text_content_survives(&before, &mapped, &now)? {
            return Err(refused("manager_v2_accepted_content_lost"));
        }
    }
    verify_content_paths(root, content_base, source, target, &ordinary).await
}

const ROLLING_REF: &str = "refs/heads/rolling";

pub(super) async fn configured_origin(root: &Path) -> Result<Option<String>> {
    let (status, raw) = run(
        root,
        &["config", "--local", "--get-all", "remote.origin.url"],
    )
    .await?;
    if status.code() == Some(1) {
        return Ok(None);
    }
    if !status.success() {
        return Err(refused("manager_v2_remote_unknown"));
    }
    let url = String::from_utf8(raw).map_err(|_| refused("manager_v2_remote_ambiguous"))?;
    let urls: Vec<_> = url.lines().collect();
    if urls.len() != 1 || urls[0].trim().is_empty() {
        return Err(refused("manager_v2_remote_ambiguous"));
    }
    Ok(Some(urls[0].to_owned()))
}

async fn origin_url(root: &Path) -> Result<String> {
    configured_origin(root)
        .await?
        .ok_or_else(|| refused("manager_v2_remote_missing"))
}

/// A repository without origin must explicitly opt into the historical local
/// branch contract. Merely lacking a configured remote is not an opt-in.
pub(super) async fn local_only_policy(root: &Path) -> Result<bool> {
    let (status, raw) = run(
        root,
        &[
            "config",
            "--local",
            "--get-all",
            "rsi.managerIntegrationTarget",
        ],
    )
    .await?;
    if status.code() == Some(1) {
        return Ok(false);
    }
    if !status.success() {
        return Err(refused("manager_v2_local_only_policy_unknown"));
    }
    let value =
        String::from_utf8(raw).map_err(|_| refused("manager_v2_local_only_policy_invalid"))?;
    if value.lines().collect::<Vec<_>>() != ["local-only"] {
        return Err(refused("manager_v2_local_only_policy_invalid"));
    }
    Ok(true)
}

async fn remote_integration_head(root: &Path) -> Result<(String, String)> {
    let raw = read_origin(
        root,
        &["ls-remote", "--symref", "origin", "HEAD", ROLLING_REF],
    )
    .await
    .map_err(|_| refused("manager_v2_remote_unknown"))?;
    let output = String::from_utf8(raw).map_err(|_| refused("manager_v2_remote_ambiguous"))?;
    let mut default_ref = None;
    let mut default_head = None;
    let mut rolling_head = None;
    for line in output.lines() {
        let Some((value, name)) = line.split_once('\t') else {
            return Err(refused("manager_v2_remote_ambiguous"));
        };
        match name {
            "HEAD" if value.starts_with("ref: ") => {
                if default_ref.replace(value[5..].to_owned()).is_some() {
                    return Err(refused("manager_v2_remote_ambiguous"));
                }
            }
            "HEAD" if canonical_sha(value) => {
                if default_head.replace(value.to_owned()).is_some() {
                    return Err(refused("manager_v2_remote_ambiguous"));
                }
            }
            ROLLING_REF if canonical_sha(value) => {
                if rolling_head.replace(value.to_owned()).is_some() {
                    return Err(refused("manager_v2_remote_ambiguous"));
                }
            }
            _ => return Err(refused("manager_v2_remote_ambiguous")),
        }
    }
    if let Some(head) = rolling_head {
        return Ok((ROLLING_REF.to_owned(), head));
    }
    let (Some(reference), Some(head)) = (default_ref, default_head) else {
        return Err(refused("manager_v2_remote_unknown"));
    };
    if !reference.starts_with("refs/heads/")
        || !run(root, &["check-ref-format", &reference])
            .await?
            .0
            .success()
    {
        return Err(refused("manager_v2_remote_ambiguous"));
    }
    Ok((reference, head))
}

pub(super) async fn remote_head(root: &Path) -> Result<String> {
    Ok(remote_integration_head(root).await?.1)
}

/// Check remote reachability and landing shape without moving a checkout or
/// Git's normal refs. Exact publication identity needs a durable landing record.
/// The private ref is removed on every path.
async fn remote_contains_target(root: &Path, source: &str, target: &str) -> Result<()> {
    if !canonical_sha(source) || !canonical_sha(target) {
        return Err(refused("manager_v2_invalid_commit"));
    }
    let observed = remote_integration_head(root).await?;
    let private_ref = format!("refs/rsi/observe/{}", Uuid::new_v4());
    let refspec = format!("{}:{private_ref}", observed.0);
    let fetched = read_origin(
        root,
        &[
            "-c",
            "fetch.writeCommitGraph=false",
            "fetch",
            "--no-auto-maintenance",
            "--no-tags",
            "--no-write-fetch-head",
            "--refmap=",
            "origin",
            &refspec,
        ],
    )
    .await;
    let result = async {
        fetched.map_err(|_| refused("manager_v2_remote_unknown"))?;
        if head(root, &private_ref).await? != observed.1 {
            return Err(refused("manager_v2_remote_target_changed"));
        }
        if head(root, target).await.is_err() || !ancestor(root, target, &observed.1).await? {
            return Err(refused("manager_v2_remote_target_mismatch"));
        }
        // A later fast-forward to a sandbox may put a landing merge off the
        // first-parent line. Its second parent must contain the accepted source,
        // while its first parent must not: a source-side merge is not a landing.
        let first_parent = read(
            root,
            &[
                "rev-list",
                "--first-parent",
                "--max-count=40000",
                &observed.1,
            ],
        )
        .await?;
        if !first_parent
            .split(|byte| *byte == b'\n')
            .any(|commit| commit == target.as_bytes())
        {
            let raw = read(root, &["rev-list", "--parents", "-n", "1", target]).await?;
            let history = std::str::from_utf8(&raw)
                .map_err(|_| refused("manager_v2_remote_target_mismatch"))?;
            let parents = history.split_ascii_whitespace().collect::<Vec<_>>();
            if parents.len() != 3
                || parents[0] != target
                || !canonical_sha(parents[1])
                || !canonical_sha(parents[2])
                || !ancestor(root, source, parents[2]).await?
                || ancestor(root, source, parents[1]).await?
            {
                return Err(refused("manager_v2_remote_target_mismatch"));
            }
        }
        if remote_integration_head(root).await? != observed {
            return Err(refused("manager_v2_remote_target_changed"));
        }
        Ok(())
    }
    .await;
    let cleanup = read(root, &["update-ref", "-d", &private_ref]).await;
    if result.is_ok() && cleanup.is_err() {
        return Err(refused("manager_v2_remote_observation_cleanup"));
    }
    result
}

/// The target is the actual landing commit. Later fast-forward landings may
/// advance the selected integration branch before the manager records it, but
/// the source must already be contained in that exact target.
pub(super) async fn remote_target(root: &Path, source: &str, target: &str) -> Result<String> {
    if !canonical_sha(source) || !canonical_sha(target) {
        return Err(refused("manager_v2_invalid_commit"));
    }
    let url = origin_url(root).await?;
    if head(root, source).await.is_err() {
        return Err(refused("manager_v2_accepted_source_missing"));
    }
    remote_contains_target(root, source, target).await?;
    if !ancestor(root, source, target).await? {
        return Err(refused("manager_v2_remote_ancestry_mismatch"));
    }
    if origin_url(root).await? != url {
        return Err(refused("manager_v2_remote_changed"));
    }
    Ok(url)
}

pub(super) async fn remote_target_unchanged(
    root: &Path,
    url: &str,
    source: &str,
    target: &str,
) -> Result<()> {
    if origin_url(root).await? != url {
        return Err(refused("manager_v2_remote_changed"));
    }
    remote_contains_target(root, source, target).await
}
pub(super) fn artifact_path(path: &str) -> Result<()> {
    if path.len() > 1024
        || !path.starts_with("thoughts/")
        || Path::new(path)
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
        || path.contains('\0')
    {
        return Err(refused("manager_v2_invalid_artifact_path"));
    }
    Ok(())
}
pub(super) async fn blob(root: &Path, commit: &str, path: &str) -> Result<Vec<u8>> {
    artifact_path(path)?;
    let entry = format!("{commit}:{path}");
    let mode = read(root, &["ls-tree", commit, "--", path]).await?;
    if !String::from_utf8_lossy(&mode).starts_with("100644 blob ") {
        return Err(refused("manager_v2_regular_artifact_required"));
    }
    read(root, &["cat-file", "blob", &entry]).await
}
const STORE_MOD_PATH: &str = "crates/rsid/src/store/mod.rs";
const MIGRATION_DIR: &str = "crates/rsid/src/store/migrations";

fn migration_file_path(version: u32) -> String {
    format!("{MIGRATION_DIR}/v{version:03}.rs")
}

/// The store layout of one revision. The per-file layout keeps each released
/// migration in `migrations/vNNN.rs` and derives the schema head from the
/// highest file; the legacy layout keeps every block and the
/// `LATEST_SCHEMA_VERSION` declaration in `store/mod.rs`.
pub(super) enum MigrationLayout {
    PerFile { head: u32 },
    Legacy { source: String },
}

pub(super) async fn migration_layout(root: &Path, rev: &str) -> Result<MigrationLayout> {
    let listing = read(
        root,
        &[
            "ls-tree",
            "--name-only",
            rev,
            "--",
            &format!("{MIGRATION_DIR}/"),
        ],
    )
    .await?;
    let listing =
        String::from_utf8(listing).map_err(|_| refused("manager_v2_invalid_inventory"))?;
    let mut head: Option<u32> = None;
    for path in listing.lines() {
        let Some(name) = path.strip_prefix(&format!("{MIGRATION_DIR}/")) else {
            continue;
        };
        let Some(digits) = name
            .strip_prefix('v')
            .and_then(|rest| rest.strip_suffix(".rs"))
        else {
            continue;
        };
        if digits.len() < 3 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        let version: u32 = digits
            .parse()
            .map_err(|_| refused("manager_v2_invalid_inventory"))?;
        if name != format!("v{version:03}.rs") {
            return Err(refused("manager_v2_invalid_inventory"));
        }
        head = Some(head.map_or(version, |current| current.max(version)));
    }
    if let Some(head) = head {
        return Ok(MigrationLayout::PerFile { head });
    }
    let source = read(
        root,
        &["cat-file", "blob", &format!("{rev}:{STORE_MOD_PATH}")],
    )
    .await?;
    let source = String::from_utf8(source).map_err(|_| refused("manager_v2_invalid_inventory"))?;
    Ok(MigrationLayout::Legacy { source })
}

/// True when `path` is the file that holds migration `version`'s block:
/// `store/mod.rs` (legacy layout) or `migrations/vNNN.rs` (per-file layout).
fn holds_migration_block(path: &str, version: i64) -> bool {
    path == STORE_MOD_PATH
        || u32::try_from(version).is_ok_and(|number| path == migration_file_path(number))
}

pub(super) async fn migration(root: &Path, baseline: &str, digest: &str) -> Result<()> {
    if !canonical_sha(baseline) || remote_head(root).await? != baseline {
        return Err(refused("manager_v2_stale_migration_baseline"));
    }
    let raw = read(
        root,
        &[
            "cat-file",
            "blob",
            &format!("{baseline}:tools/released-migrations.json"),
        ],
    )
    .await?;
    if format!("sha256:{:x}", Sha256::digest(&raw)) != digest {
        return Err(refused("manager_v2_inventory_digest"));
    }
    let inventory: Value = serde_json::from_slice(&raw)?;
    let canonical: Value = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tools/released-migrations.json"
    )))?;
    // A digest supplied by an agent is only a correlation value. The complete
    // daemon-pinned catalog, including every old pin, is the admission authority.
    if inventory != canonical {
        return Err(refused("manager_v2_released_inventory_changed"));
    }
    let blocks = inventory["blocks"]
        .as_object()
        .ok_or_else(|| refused("manager_v2_invalid_inventory"))?;
    let layout = migration_layout(root, baseline).await?;
    let expected_head = crate::store::LATEST_SCHEMA_VERSION;
    match &layout {
        MigrationLayout::PerFile { head } => {
            if i64::from(*head) != i64::from(expected_head) {
                return Err(refused("manager_v2_schema_head_changed"));
            }
        }
        MigrationLayout::Legacy { source } => {
            let expected = format!("pub const LATEST_SCHEMA_VERSION: i32 = {expected_head};");
            if !source.lines().any(|line| line == expected) {
                return Err(refused("manager_v2_schema_head_changed"));
            }
        }
    }
    for (version, pin) in blocks {
        if canonical["blocks"][version] != *pin {
            return Err(refused("manager_v2_released_pin_changed"));
        }
        let number: u32 = version
            .parse()
            .map_err(|_| refused("manager_v2_invalid_inventory"))?;
        let source = match &layout {
            MigrationLayout::Legacy { source } => source.clone(),
            MigrationLayout::PerFile { .. } => {
                let bytes = read(
                    root,
                    &[
                        "cat-file",
                        "blob",
                        &format!("{baseline}:{}", migration_file_path(number)),
                    ],
                )
                .await
                .map_err(|_| refused("manager_v2_invalid_inventory"))?;
                String::from_utf8(bytes).map_err(|_| refused("manager_v2_invalid_inventory"))?
            }
        };
        let body = if number == 0 {
            let lines: Vec<_> = source.split_inclusive('\n').collect();
            let start = lines
                .iter()
                .position(|l| l.contains("// V0: Original schema"))
                .ok_or_else(|| refused("manager_v2_invalid_inventory"))?;
            // Legacy: the V0 region ends at the V1 comment; per-file: at the
            // step function's closing `Ok(())` (as the Python guard).
            let end = (start + 1..lines.len())
                .find(|i| {
                    lines[*i].contains("// V1: Session metadata columns")
                        || lines[*i].trim_end_matches(['\r', '\n']) == "        Ok(())"
                })
                .ok_or_else(|| refused("manager_v2_invalid_inventory"))?;
            lines[start..end].concat()
        } else {
            let lines: Vec<_> = source.split_inclusive('\n').collect();
            let marker = format!("if version < {version} {{");
            let at = lines
                .iter()
                .position(|l| l.trim() == marker)
                .ok_or_else(|| refused("manager_v2_invalid_inventory"))?;
            let indent = lines[at].len() - lines[at].trim_start().len();
            let close = format!("{}}}", " ".repeat(indent));
            let end = (at + 1..lines.len())
                .find(|i| lines[*i].trim_end() == close)
                .ok_or_else(|| refused("manager_v2_invalid_inventory"))?;
            lines[at..=end].concat()
        };
        if json!(format!("sha256:{:x}", Sha256::digest(body.as_bytes()))) != *pin {
            return Err(refused("manager_v2_released_source_changed"));
        }
    }
    for (name, pin) in inventory["protected_sections"]
        .as_object()
        .ok_or_else(|| refused("manager_v2_invalid_inventory"))?
    {
        if canonical["protected_sections"][name] != *pin {
            return Err(refused("manager_v2_released_pin_changed"));
        }
        let path = pin["path"]
            .as_str()
            .ok_or_else(|| refused("manager_v2_invalid_inventory"))?;
        if !path.starts_with("crates/") || path.contains("..") {
            return Err(refused("manager_v2_invalid_inventory"));
        }
        let bytes = read(root, &["cat-file", "blob", &format!("{baseline}:{path}")]).await?;
        let text = String::from_utf8(bytes).map_err(|_| refused("manager_v2_invalid_inventory"))?;
        let lines: Vec<_> = text.split_inclusive('\n').collect();
        let begin = format!("// RSI-RELEASED-MIGRATION-BEGIN: {name}");
        let end = format!("// RSI-RELEASED-MIGRATION-END: {name}");
        let start = lines
            .iter()
            .position(|l| l.trim() == begin)
            .ok_or_else(|| refused("manager_v2_invalid_inventory"))?;
        let end = lines
            .iter()
            .position(|l| l.trim() == end)
            .ok_or_else(|| refused("manager_v2_invalid_inventory"))?;
        if start >= end {
            return Err(refused("manager_v2_invalid_inventory"));
        }
        if json!(format!(
            "sha256:{:x}",
            Sha256::digest(lines[start..=end].concat().as_bytes())
        )) != pin["sha256"]
        {
            return Err(refused("manager_v2_released_source_changed"));
        }
    }
    if remote_head(root).await? != baseline {
        return Err(refused("manager_v2_stale_migration_baseline"));
    }
    Ok(())
}

/// Observe the local rolling tracking ref and its declared schema head.
/// A seal records this exact tip; publication rechecks it after fetching.
pub(super) async fn migration_seal_head(root: &Path) -> Result<(String, u32)> {
    let tip = head(root, "refs/remotes/origin/rolling").await?;
    let raw = read(
        root,
        &[
            "cat-file",
            "blob",
            &format!("{tip}:tools/released-migrations.json"),
        ],
    )
    .await?;
    let inventory: Value =
        serde_json::from_slice(&raw).map_err(|_| refused("manager_v2_invalid_inventory"))?;
    let version = inventory["latest_schema_version"]
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .filter(|n| *n < i32::MAX as u32)
        .ok_or_else(|| refused("manager_v2_invalid_inventory"))?;
    match migration_layout(root, &tip).await? {
        MigrationLayout::PerFile { head } => {
            if head != version {
                return Err(refused("manager_v2_schema_head_changed"));
            }
        }
        MigrationLayout::Legacy { source } => {
            let declaration = format!("pub const LATEST_SCHEMA_VERSION: i32 = {version};");
            if !source.lines().any(|line| line == declaration) {
                return Err(refused("manager_v2_schema_head_changed"));
            }
        }
    }
    Ok((tip, version))
}
