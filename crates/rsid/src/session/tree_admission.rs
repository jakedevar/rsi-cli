//! Issue #12: effective-worktree identity and tree-occupancy admission.
//!
//! A `fresh` wake launches a parentless root into the arming agent's working
//! tree. It must never become a second writer in a tree another live session
//! writes. Occupancy is decided on the *effective Git worktree identity*
//! (canonical path of the nearest `.git` ancestor), not on literal strings, so a
//! symlink alias or a subdirectory of the same worktree is the same tree while
//! an independent `git worktree` of the same repository is a different one.
//!
//! The admission lock per identity fences the occupancy scan through the
//! launch's durable `Starting` publication. Continuations, retries and rotation
//! use the same fence. Ordinary sessions may share a tree, but a fresh-wake
//! lineage requires exclusive occupancy for every new provider run.
//!
//! A `tree_occupied` refusal is always retryable and never consumes work
//! (#1185): it is raised before any effect, a scheduled Resume is retained with
//! bounded backoff (`scheduler::TRANSIENT_RESUME_REFUSAL_CODES`), an automatic
//! retry is re-armed by its caller, and the occupying writer is a live session
//! the operator can interrupt, so no separate override is needed.

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

static TREE_ADMISSION: Mutex<Option<HashMap<PathBuf, Arc<tokio::sync::RwLock<()>>>>> =
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
pub(crate) enum TreeAdmissionGuard {
    Shared {
        _guard: tokio::sync::OwnedRwLockReadGuard<()>,
    },
    Exclusive {
        _guard: tokio::sync::OwnedRwLockWriteGuard<()>,
    },
}

/// Take the exclusive admission for `identity`.
pub(crate) async fn acquire_tree_admission(identity: &Path) -> Result<TreeAdmissionGuard> {
    acquire_writer_admission(identity, true).await
}

async fn acquire_writer_admission(identity: &Path, exclusive: bool) -> Result<TreeAdmissionGuard> {
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
    let acquire = async move {
        if exclusive {
            TreeAdmissionGuard::Exclusive {
                _guard: lock.write_owned().await,
            }
        } else {
            TreeAdmissionGuard::Shared {
                _guard: lock.read_owned().await,
            }
        }
    };
    match tokio::time::timeout(ADMISSION_WAIT, acquire).await {
        Ok(guard) => Ok(guard),
        Err(_) => Err(tree_occupied_error(format!(
            "admission for {} is busy; retry",
            identity.display()
        ))),
    }
}

/// Admit a provider writer under the same fence as a fresh wake. `replaced`
/// names only the same incarnation being restarted, whose provider is stopped
/// by the caller's existing lifecycle/custody boundary. A raw `continued_from`
/// link never exempts a different live incarnation from the occupancy check.
/// Keep the returned guard through provider dispatch and durable publication.
pub(crate) async fn writer_admission(
    store: &Arc<tokio::sync::Mutex<crate::store::Store>>,
    active: &Arc<tokio::sync::RwLock<HashMap<Uuid, super::TrackedSession>>>,
    dir: &Path,
    replaced: &[Uuid],
    fresh: bool,
) -> Result<Option<TreeAdmissionGuard>> {
    let identity = match tree_identity(dir) {
        Ok(identity) => identity,
        // A missing cwd cannot start a provider; the launch boundary reports it.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(tree_occupied_error(format!(
                "cannot resolve the worktree identity of {}: {error}",
                dir.display()
            )));
        }
    };
    let mut exclusive = fresh;
    {
        let store = store.lock().await;
        for id in replaced {
            exclusive |= store.session_has_wake_origin(*id)?;
        }
    }
    // Ordinary launches keep their existing concurrent admission (including
    // capacity reservations). Fresh lineages exclude both them and each other.
    let guard = acquire_writer_admission(&identity, exclusive).await?;
    let occupants = live_occupants(store, active, &identity, dir).await?;
    let store = store.lock().await;
    for id in occupants.into_iter().filter(|id| !replaced.contains(id)) {
        if exclusive || store.session_has_wake_origin(id)? {
            return Err(tree_occupied_error(format!(
                "live session {id} already writes the worktree {}; a fresh-wake \
                 lineage cannot share its write tree",
                identity.display()
            )));
        }
    }
    Ok(Some(guard))
}

/// Resolve a continuation/rotation's durable effective cwd before it changes
/// maps, grants, custody or providers. Raw source checkouts do not name a
/// sandboxed session's write tree.
pub(crate) async fn session_writer_admission(
    store: &Arc<tokio::sync::Mutex<crate::store::Store>>,
    active: &Arc<tokio::sync::RwLock<HashMap<Uuid, super::TrackedSession>>>,
    session_id: Uuid,
) -> Result<Option<TreeAdmissionGuard>> {
    let session = store
        .lock()
        .await
        .get_session(session_id)?
        .ok_or(DaemonError::SessionNotFound(session_id))?;
    writer_admission(
        store,
        active,
        session
            .sandbox_root
            .as_deref()
            .unwrap_or(&session.working_dir),
        &[session_id],
        false,
    )
    .await
}

/// Live leaf sessions whose *write* tree is `identity`. A session writes its
/// sandbox root when it has one (its stored `working_dir` is then only the
/// source checkout), otherwise its working directory. An unresolvable
/// missing directory is compared lexically against `lexical`. Other identity
/// failures count as occupants only of trees their resolvable prefixes could
/// name. The caller then applies the ordinary/fresh-lineage sharing policy.
pub(crate) async fn live_occupants(
    store: &Arc<tokio::sync::Mutex<crate::store::Store>>,
    active: &Arc<tokio::sync::RwLock<HashMap<Uuid, super::TrackedSession>>>,
    identity: &Path,
    lexical: &Path,
) -> Result<Vec<Uuid>> {
    let mut rows = store.lock().await.live_leaf_session_dirs()?;
    // An active entry still reserves its cwd if status persistence fails or
    // finalization stamps terminal status before removing the tracked process.
    // Read runtime and Store independently: no nested map/Store lock order.
    {
        let active = active.read().await;
        rows.extend(active.values().map(|tracked| {
            (
                tracked.session.id,
                tracked.session.working_dir.display().to_string(),
                tracked
                    .session
                    .sandbox_root
                    .as_ref()
                    .map(|path| path.display().to_string()),
            )
        }));
    }
    let mut occupants = Vec::new();
    for (id, working_dir, sandbox_root) in rows {
        let dir = Path::new(sandbox_root.as_deref().unwrap_or(&working_dir));
        let same = match tree_identity(dir) {
            Ok(other) => other == identity,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                dir == lexical || dir == identity
            }
            Err(_) => unresolved_tree_may_match(dir, identity, lexical),
        };
        if same && !occupants.contains(&id) {
            occupants.push(id);
        }
    }
    Ok(occupants)
}

/// A failed lookup must not turn one broken cwd into a daemon-wide fence.
/// Anchor its unresolved suffix at the nearest canonical ancestor: either
/// that ancestor belongs to this Git tree, or the suffix encloses this tree.
/// Follow readable symlinks too, so an alias outside the tree still fences a
/// broken path inside it. Bound traversal like canonicalization does.
fn unresolved_tree_may_match(dir: &Path, identity: &Path, lexical: &Path) -> bool {
    let mut path = dir.to_path_buf();
    let mut visited: Vec<PathBuf> = Vec::new();
    loop {
        if path == identity || path == lexical {
            return true;
        }
        if let Some(cycle) = visited.iter().position(|seen| seen == &path) {
            return visited[cycle..]
                .iter()
                .any(|path| unresolved_prefix_may_match(path, identity));
        }
        if visited.len() >= 40 {
            return unresolved_prefix_may_match(&path, identity)
                || visited
                    .iter()
                    .any(|path| unresolved_prefix_may_match(path, identity));
        }
        visited.push(path.clone());
        let mut next = None;
        for ancestor in path.ancestors() {
            let suffix = path.strip_prefix(ancestor).expect("path ancestor");
            if ancestor.canonicalize().is_ok() {
                break;
            }
            if let Ok(target) = std::fs::read_link(ancestor) {
                let target = if target.is_absolute() {
                    target
                } else {
                    ancestor.parent().unwrap_or(Path::new("")).join(target)
                };
                next = Some(target.join(suffix));
                break;
            }
        }
        match next {
            Some(target) => path = target,
            None => return unresolved_prefix_may_match(&path, identity),
        }
    }
}

fn unresolved_prefix_may_match(path: &Path, identity: &Path) -> bool {
    for ancestor in path.ancestors() {
        if let Ok(canonical) = ancestor.canonicalize() {
            let suffix = path.strip_prefix(ancestor).expect("path ancestor");
            return identity.starts_with(canonical.join(suffix))
                || tree_identity(&canonical).is_ok_and(|root| root == identity);
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    async fn retained_wake_lineage_excludes_other_writers_until_it_settles() {
        use rsi_common::types::{Recurrence, ScheduleSpec, ScheduledJob, SessionStatus, WakeMode};

        let tree = tempfile::tempdir().unwrap();
        std::fs::create_dir(tree.path().join(".git")).unwrap();
        let sub = tree.path().join("src");
        std::fs::create_dir(&sub).unwrap();
        let sibling = tempfile::tempdir().unwrap();
        std::fs::write(sibling.path().join(".git"), "gitdir: other-worktree").unwrap();
        let store = Arc::new(tokio::sync::Mutex::new(
            crate::store::Store::open_in_memory().unwrap(),
        ));
        let active = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
        let origin = Uuid::new_v4();
        let now = chrono::Utc::now();
        let job = ScheduledJob {
            id: Uuid::new_v4(),
            name: "retained-origin".into(),
            message: "m".into(),
            schedule: ScheduleSpec {
                recurrence: Recurrence::Once,
                anchor: now,
            },
            last_fired_at: Some(now),
            next_fire_at: now,
            enabled: false,
            working_dir: Some(tree.path().to_path_buf()),
            provider: None,
            model: None,
            project_id: None,
            created_at: now,
            updated_at: now,
            wake_mode: WakeMode::AgentFresh,
            wake_session_id: Some(origin),
        };
        let mut root =
            rsid_store::test_support::test_session(Uuid::new_v4(), tree.path().to_path_buf());
        root.status = SessionStatus::Archived;
        root.scheduled_job_id = Some(job.id);
        let mut successor = rsid_store::test_support::test_session(Uuid::new_v4(), sub.clone());
        successor.status = SessionStatus::Running;
        successor.continued_from = Some(root.id);
        let mut ordinary =
            rsid_store::test_support::test_session(Uuid::new_v4(), tree.path().to_path_buf());
        ordinary.status = SessionStatus::Completed;
        {
            let store = store.lock().await;
            store.insert_scheduled_job(&job).unwrap();
            for session in [&root, &successor, &ordinary] {
                store.insert_session(session).unwrap();
            }
            store.delete_scheduled_job(&job.id).unwrap();
        }
        let error = session_writer_admission(&store, &active, ordinary.id)
            .await
            .err()
            .expect("live wake successor owns the tree");
        assert!(is_tree_occupied_error(&error));
        // Continuing the same live incarnation is permitted; its own row is
        // excluded, while the retained origin still requires exclusive use.
        drop(
            session_writer_admission(&store, &active, successor.id)
                .await
                .unwrap(),
        );
        drop(
            writer_admission(&store, &active, sibling.path(), &[], false)
                .await
                .unwrap(),
        );
        {
            let store = store.lock().await;
            store
                .update_session_status(successor.id, SessionStatus::Completed)
                .unwrap();
            store
                .update_session_status(ordinary.id, SessionStatus::Running)
                .unwrap();
        }
        let error = session_writer_admission(&store, &active, successor.id)
            .await
            .err()
            .expect("resuming the wake lineage cannot join an ordinary writer");
        assert!(is_tree_occupied_error(&error));
        // Ordinary launches retain their existing ability to share a tree.
        drop(
            writer_admission(&store, &active, &sub, &[], false)
                .await
                .unwrap(),
        );
        store
            .lock()
            .await
            .update_session_status(ordinary.id, SessionStatus::Completed)
            .unwrap();
        drop(
            session_writer_admission(&store, &active, successor.id)
                .await
                .unwrap(),
        );
    }

    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    async fn ambiguous_live_worktree_identity_refuses_admission() {
        let tree = tempfile::tempdir().unwrap();
        std::fs::create_dir(tree.path().join(".git")).unwrap();
        let alias = tree.path().join("loop");
        std::os::unix::fs::symlink(&alias, &alias).unwrap();
        let other = tempfile::tempdir().unwrap();
        std::fs::create_dir(other.path().join(".git")).unwrap();
        let outside_alias = other.path().join("broken-alias");
        std::os::unix::fs::symlink(&alias, &outside_alias).unwrap();
        assert!(tree_identity(&outside_alias).is_err());
        let store = Arc::new(tokio::sync::Mutex::new(
            crate::store::Store::open_in_memory().unwrap(),
        ));
        let active = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
        let mut live =
            rsid_store::test_support::test_session(Uuid::new_v4(), other.path().to_path_buf());
        live.sandbox_root = Some(outside_alias);
        live.status = rsi_common::types::SessionStatus::Running;
        store.lock().await.insert_session(&live).unwrap();
        // Ordinary writers may share even the ambiguous tree with a non-wake
        // occupant. Exclusive writers still fail closed in that tree.
        drop(
            writer_admission(&store, &active, tree.path(), &[], false)
                .await
                .unwrap(),
        );
        let error = writer_admission(&store, &active, tree.path(), &[], true)
            .await
            .err()
            .expect("ambiguous live identity fails closed");
        assert!(is_tree_occupied_error(&error));
        for fresh in [false, true] {
            drop(
                writer_admission(&store, &active, other.path(), &[], fresh)
                    .await
                    .expect("an unresolvable sandbox does not occupy an unrelated tree"),
            );
        }
    }

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
        let canonical = tree_identity(&repo).unwrap();
        assert_eq!(tree_identity(&sub).unwrap(), canonical);
        #[cfg(unix)]
        {
            let alias = root.path().join("alias");
            std::os::unix::fs::symlink(&repo, &alias).unwrap();
            assert_eq!(tree_identity(&alias).unwrap(), canonical);
            assert_eq!(tree_identity(&alias.join("crates/x")).unwrap(), canonical);
        }
        assert_ne!(tree_identity(&sibling).unwrap(), canonical);
        assert!(tree_identity(&root.path().join("missing")).is_err());
    }
}
