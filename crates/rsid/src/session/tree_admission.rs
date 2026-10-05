//! Issue #12: effective-worktree identity and tree-occupancy admission.
//!
//! A `fresh` wake launches a parentless root into the arming agent's working
//! tree. It must never become a second writer in a tree another live session
//! writes. Occupancy is decided on the *effective Git worktree identity*
//! (canonical path of the nearest `.git` ancestor), not on literal strings, so a
//! symlink alias or a subdirectory of the same worktree is the same tree while
//! an independent `git worktree` of the same repository is a different one.
//!
//! The admission mutex per identity serializes the occupancy scan with the
//! launch's durable `Starting` publication, so two fresh launches into one tree
//! cannot both pass the scan.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use uuid::Uuid;

use crate::error::{DaemonError, Result};

/// Marker prefix of the retryable occupancy refusal.
pub(crate) const TREE_OCCUPIED_CODE: &str = "tree_occupied";

/// How long a launch/continuation waits for the per-tree admission before
/// failing retryably (a stuck holder must degrade to an error, never a hang).
const ADMISSION_WAIT: Duration = Duration::from_secs(60);

static TREE_ADMISSION: Mutex<Option<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>> =
    Mutex::new(None);

pub(crate) fn tree_occupied_error(detail: impl AsRef<str>) -> DaemonError {
    DaemonError::PolicyDenied(format!("{TREE_OCCUPIED_CODE}: {}", detail.as_ref()))
}

/// True for the retryable occupancy refusal raised by this module.
pub(crate) fn is_tree_occupied_error(error: &DaemonError) -> bool {
    matches!(error, DaemonError::PolicyDenied(message) if message.starts_with(TREE_OCCUPIED_CODE))
}

/// Canonical path of the nearest `.git` ancestor (the effective Git worktree
/// root), or the canonical directory itself when no repository encloses it.
pub(crate) fn tree_identity(dir: &Path) -> std::io::Result<PathBuf> {
    let canonical = dir.canonicalize()?;
    for ancestor in canonical.ancestors() {
        if ancestor.join(".git").exists() {
            return Ok(ancestor.to_path_buf());
        }
    }
    Ok(canonical)
}

/// Held while a launch or continuation publishes a live writer into a tree.
pub(crate) struct TreeAdmissionGuard {
    _guard: tokio::sync::OwnedMutexGuard<()>,
}

/// Take the exclusive admission for `identity`.
pub(crate) async fn acquire_tree_admission(identity: &Path) -> Result<TreeAdmissionGuard> {
    let lock = {
        let mut map = TREE_ADMISSION
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Arc::clone(
            map.get_or_insert_with(HashMap::new)
                .entry(identity.to_path_buf())
                .or_default(),
        )
    };
    match tokio::time::timeout(ADMISSION_WAIT, lock.lock_owned()).await {
        Ok(guard) => Ok(TreeAdmissionGuard { _guard: guard }),
        Err(_) => Err(tree_occupied_error(format!(
            "admission for {} is busy; retry",
            identity.display()
        ))),
    }
}

/// Live leaf sessions whose *write* tree is `identity`. A session writes its
/// sandbox root when it has one (its stored `working_dir` is then only the
/// source checkout), otherwise its working directory. An unresolvable
/// directory is compared lexically against `lexical` rather than skipped.
pub(crate) async fn live_occupants(
    store: &Arc<tokio::sync::Mutex<crate::store::Store>>,
    identity: &Path,
    lexical: &Path,
) -> Result<Vec<Uuid>> {
    let rows = store.lock().await.live_leaf_session_dirs()?;
    let mut occupants = Vec::new();
    for (id, working_dir, sandbox_root) in rows {
        let dir = Path::new(sandbox_root.as_deref().unwrap_or(&working_dir));
        let same = match tree_identity(dir) {
            Ok(other) => other == identity,
            Err(_) => dir == lexical || dir == identity,
        };
        if same {
            occupants.push(id);
        }
    }
    Ok(occupants)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[test]
    fn identity_unifies_aliases_and_subdirectories_but_not_sibling_worktrees() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        let sub = repo.join("crates/x");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir(repo.join(".git")).unwrap();
        // An independent git worktree has a `.git` *file* and its own root.
        let sibling = root.path().join("wt");
        std::fs::create_dir_all(&sibling).unwrap();
        std::fs::write(sibling.join(".git"), "gitdir: ../repo/.git/worktrees/wt").unwrap();
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(&repo, &alias).unwrap();

        let canonical = tree_identity(&repo).unwrap();
        assert_eq!(tree_identity(&sub).unwrap(), canonical);
        assert_eq!(tree_identity(&alias).unwrap(), canonical);
        assert_eq!(tree_identity(&alias.join("crates/x")).unwrap(), canonical);
        assert_ne!(tree_identity(&sibling).unwrap(), canonical);
        assert!(tree_identity(&root.path().join("missing")).is_err());
    }
}
