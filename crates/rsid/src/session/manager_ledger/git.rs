use super::*;
use similar::{Algorithm, DiffOp, capture_diff_slices_deadline};
use std::collections::HashSet;
use std::process::Stdio;
use tokio::{io::AsyncReadExt, process::Command};
const MAX_BYTES: u64 = 2 * 1024 * 1024;

async fn run(root: &Path, args: &[&str]) -> Result<(std::process::ExitStatus, Vec<u8>)> {
    let mut command = Command::new("git");
    command
        .args([
            "--no-optional-locks",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
            "-C",
        ])
        .arg(root)
        .args(args)
        // Evidence names canonical objects, never locally substituted history.
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_GRAFT_FILE", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    ] {
        command.env_remove(key);
    }
    let mut child = command
        .spawn()
        .map_err(|_| refused("manager_v2_git_unavailable"))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| refused("manager_v2_git_unavailable"))?
        .take(MAX_BYTES + 1);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut bytes = Vec::new();
        stdout
            .read_to_end(&mut bytes)
            .await
            .map_err(|_| refused("manager_v2_git_read"))?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(refused("manager_v2_git_output_limit"));
        }
        let status = child
            .wait()
            .await
            .map_err(|_| refused("manager_v2_git_wait"))?;
        Ok((status, bytes))
    })
    .await
    .map_err(|_| refused("manager_v2_git_timeout"))?
}

pub(super) async fn read(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let (status, bytes) = run(root, args).await?;
    if !status.success() {
        return Err(refused("manager_v2_git_failed"));
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
    let (status, _) = run(root, &["merge-base", "--is-ancestor", source, target]).await?;
    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(refused("manager_v2_git_failed")),
    }
}

async fn content_blob(root: &Path, commit: &str, path: &str) -> Result<Option<Vec<u8>>> {
    let object = format!("{commit}:{path}");
    let (status, _) = run(root, &["cat-file", "-e", &object]).await?;
    if !status.success() {
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
    let old = lines(base);
    let accepted = lines(source);
    let published = lines(target);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
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
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        accepted_content_inner(root, base, source, target),
    )
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
    verify_content_paths(root, &content_base, source, target, &paths).await
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

async fn verify_content_paths(
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
    let raw = read(
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
    let fetched = read(
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
    let source = read(
        root,
        &[
            "cat-file",
            "blob",
            &format!("{baseline}:crates/rsid/src/store/mod.rs"),
        ],
    )
    .await?;
    let source = String::from_utf8(source).map_err(|_| refused("manager_v2_invalid_inventory"))?;
    let expected_head = format!(
        "pub const LATEST_SCHEMA_VERSION: i32 = {};",
        crate::store::LATEST_SCHEMA_VERSION
    );
    if !source.lines().any(|line| line == expected_head) {
        return Err(refused("manager_v2_schema_head_changed"));
    }
    for (version, pin) in blocks {
        if canonical["blocks"][version] != *pin {
            return Err(refused("manager_v2_released_pin_changed"));
        }
        let body = if version == "0" {
            let lines: Vec<_> = source.split_inclusive('\n').collect();
            let start = lines
                .iter()
                .position(|l| l.contains("// V0: Original schema"))
                .ok_or_else(|| refused("manager_v2_invalid_inventory"))?;
            let end = lines
                .iter()
                .position(|l| l.contains("// V1: Session metadata columns"))
                .ok_or_else(|| refused("manager_v2_invalid_inventory"))?;
            if start >= end {
                return Err(refused("manager_v2_invalid_inventory"));
            }
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
    let source = read(
        root,
        &[
            "cat-file",
            "blob",
            &format!("{tip}:crates/rsid/src/store/mod.rs"),
        ],
    )
    .await?;
    let declaration = format!("pub const LATEST_SCHEMA_VERSION: i32 = {version};");
    if !String::from_utf8(source)
        .map_err(|_| refused("manager_v2_invalid_inventory"))?
        .lines()
        .any(|line| line == declaration)
    {
        return Err(refused("manager_v2_schema_head_changed"));
    }
    Ok((tip, version))
}
