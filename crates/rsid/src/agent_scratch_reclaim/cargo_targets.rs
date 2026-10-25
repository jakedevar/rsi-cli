//! Manager build allocations and bounded discovery of legacy targets (#1864).
//! Legacy names grant visibility only; the operator's adoption path supplies
//! provenance, and the existing scratch proof owns every deletion.

use super::{Budget, Pinned, RootKind, fsys, record, registry};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const CACHE_SIGNATURE: &str = "Signature: 8a477f597d28d172789f06886806bc55";

pub(super) fn allows(name: &str) -> bool {
    // #1218 owns this singleton's size cap and preserves its lock inodes.
    // Whole-allocation scratch deletion must never remove those locks.
    if name == "release-target" || (name.starts_with("rsi-") && name.ends_with("-tmp")) {
        return false;
    }
    matches!(
        name,
        "target"
            | "artifact-target"
            | "target-integ"
            | "target-integ2"
            | "sat-target-deploy"
            | "shared-target"
    ) || name.starts_with("rsi-build-")
        || name.starts_with("review-target-")
        || name.starts_with("rsi-review-")
        || name.starts_with("koplik-review-")
        || name.ends_with("-target")
}

/// The worktree a registered build target was allocated for and what is known
/// of its use (#646).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct TargetOwner {
    pub worktree: String,
    /// An open job whose working directory is inside the worktree.
    pub job: Option<String>,
    /// The worktree is gone, the open-job list is known and names no job in it.
    pub orphaned: bool,
}

/// The owner a build target's registry entry records. `None` for a legacy
/// allocation that recorded no source worktree. A worktree is gone only when
/// the filesystem says it does not exist; any other error keeps it present.
pub(super) fn owner_of(
    entry: &super::Entry,
    open_jobs: Option<&[super::OpenJob]>,
) -> Option<TargetOwner> {
    let worktree = entry
        .source
        .as_deref()
        .filter(|source| !source.is_empty())?;
    let source = Path::new(worktree);
    let job = open_jobs
        .into_iter()
        .flatten()
        .find(|job| job.cwd.starts_with(source))
        .map(|job| job.id.clone());
    let gone = matches!(
        fs::symlink_metadata(source),
        Err(error) if error.kind() == io::ErrorKind::NotFound
    );
    Some(TargetOwner {
        worktree: worktree.to_string(),
        orphaned: gone && open_jobs.is_some() && job.is_none(),
        job,
    })
}

/// Directory under the data dir that holds the manager build allocations.
const BUILD_TARGETS_DIR: &str = "build-targets";

/// Whether `root` is the manager build-targets root (#3261).
pub(super) fn is_build_targets_root(root: &Path) -> bool {
    root.file_name()
        .is_some_and(|name| name == BUILD_TARGETS_DIR)
}

/// The manager build-targets root of the running daemon.
pub(super) fn build_targets_root() -> PathBuf {
    rsi_common::identity::data_dir().join(BUILD_TARGETS_DIR)
}

/// A repository a test created under a registered Cargo target's fixture
/// directory (`rsid-test-fixtures`, `slice8-*-fixtures`, ...): throwaway
/// scratch of the test run, never operator work, so it cannot pin the whole
/// allocation as unpublished (#3261). Only a top-level `*-fixtures` child
/// qualifies; a repository anywhere else in a target still has to be clean
/// and published.
pub(super) fn is_fixture_repo(git: &fsys::GitRoot) -> bool {
    git.rel
        .components()
        .next()
        .and_then(|first| first.as_os_str().to_str())
        .is_some_and(|first| first.ends_with("-fixtures"))
}

pub(super) fn roots() -> Vec<(PathBuf, RootKind)> {
    known_roots(
        &rsi_common::identity::data_dir(),
        dirs::home_dir().as_deref(),
    )
}

fn known_roots(data: &Path, home: Option<&Path>) -> Vec<(PathBuf, RootKind)> {
    let mut roots = vec![
        data.join(BUILD_TARGETS_DIR),
        data.to_path_buf(),
        data.join("staging"),
        data.join("cloud"),
        data.join("koplik-mgr"),
    ];
    // A seat may contain source and state; only its target child is a candidate.
    if let Ok(entries) = fs::read_dir(data.join("mgr")) {
        roots.extend(
            entries
                .filter_map(Result::ok)
                .take(256)
                .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
                .map(|entry| entry.path()),
        );
    }
    if let Some(home) = home {
        roots.extend([home.join(".cargo"), home.join(".cache")]);
    }
    let mut roots: Vec<_> = roots
        .into_iter()
        .map(|root| (root, RootKind::CargoTarget))
        .collect();
    roots.push((data.join("review-targets"), RootKind::CargoTargetPool));
    roots
}

/// Whole-target retain rule, descriptor-relative. Unknown content and symlinks
/// are retained, including during operator adoption. Deep Git safety, mounts,
/// live consumers and mutation races remain the scratch proof's responsibility.
pub(super) fn cache_only(target: &Pinned, budget: &mut Budget) -> bool {
    let Ok(names) = fsys::entry_names(target.raw(), true, budget) else {
        return false;
    };
    // A name alone proves nothing. Cargo writes either a cache signature or
    // compiler metadata; a freshly registered manager allocation gets a tag.
    let tag = fsys::read_small_file(target, "CACHEDIR.TAG", 4096)
        .ok()
        .flatten()
        .is_some_and(|text| text.starts_with(CACHE_SIGNATURE));
    let rustc = fsys::read_small_file(target, ".rustc_info.json", 1024 * 1024)
        .ok()
        .flatten()
        .is_some_and(|text| {
            serde_json::from_str::<serde_json::Value>(&text)
                .is_ok_and(|value| value.get("rustc_fingerprint").is_some())
        });
    (tag || rustc)
        && names.iter().all(|name| {
            let Ok(stat) = fsys::stat_at(target.raw(), name) else {
                return false;
            };
            // Cargo profiles, target triples and tool-owned subtargets are all
            // directories. The deep mount/Git/holder/manifest proofs still apply.
            if fsys::is_dir(&stat) {
                true
            } else if fsys::is_regular(&stat) {
                matches!(
                    name.as_str(),
                    super::RECORD_FILE
                        | "CACHEDIR.TAG"
                        | ".rustc_info.json"
                        | ".cargo-lock"
                        | ".future-incompat-report.json"
                        | ".rsid-test-shards.lock"
                        | ".rsi-job-admission.lock"
                        | ".rsi-build-env.json"
                        | ".rsi-target-pool.json"
                        | crate::qa_target_trim::TRIM_STAMP
                )
            } else {
                false
            }
        })
}

/// Installed links remain consumers between invocations. Check the original
/// allocation path too: rename-aside can make an installed link temporarily
/// dangling, and interrupted removals must still retain that allocation.
pub(super) fn has_consumer(
    path: &Path,
    original: &str,
    config: &super::ScratchConfig,
    budget: &mut Budget,
) -> io::Result<bool> {
    let candidates = [
        resolve_path(path, budget, 0)?,
        resolve_path(Path::new(original), budget, 0)?,
    ];
    for root in &config.deploy_roots {
        let root = resolve_path(root, budget, 0)?;
        if candidates
            .iter()
            .any(|candidate| root.starts_with(candidate))
        {
            return Ok(true);
        }
    }
    for install in &config.install_dirs {
        let entries = match fs::read_dir(install) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        for entry in entries {
            if !budget.spend(1) {
                return Err(io::Error::other("install consumer scan exhausted"));
            }
            let entry = entry?;
            if entry.file_type()?.is_symlink() {
                let destination = resolve_path(&entry.path(), budget, 0)?;
                if candidates
                    .iter()
                    .any(|candidate| destination.starts_with(candidate))
                {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}

/// Resolve chains and relative links, including a dangling final component
/// after rename-aside. Resolve parents before appending `..` components.
fn resolve_path(path: &Path, budget: &mut Budget, depth: usize) -> io::Result<PathBuf> {
    if depth > 40 || !budget.spend(1) {
        return Err(io::Error::other("install link resolution exhausted"));
    }
    match path.canonicalize() {
        Ok(path) => return Ok(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("unresolved install path"))?;
    let parent = resolve_path(parent, budget, depth + 1)?;
    let leaf = path
        .file_name()
        .ok_or_else(|| io::Error::other("invalid install path"))?;
    let resolved = parent.join(leaf);
    match fs::read_link(&resolved) {
        Ok(target) => resolve_path(&parent.join(target), budget, depth + 1),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::InvalidInput
            ) =>
        {
            Ok(resolved)
        }
        Err(error) => Err(error),
    }
}

/// Stable per-worktree allocation: recurring manager release builds retain
/// their release artifacts, while different source worktrees never share them.
pub(crate) fn manager_target(cwd: &Path) -> io::Result<PathBuf> {
    manager_target_in(
        &rsi_common::identity::data_dir().join("build-targets"),
        &registry::default_registry_path(),
        cwd,
    )
}

pub(super) fn manager_target_in(
    parent: &Path,
    registry_path: &Path,
    cwd: &Path,
) -> io::Result<PathBuf> {
    registered_target_in(parent, registry_path, "rsi-build-", cwd)
}

/// Name prefix of a seat QA target (still `rsi-build-`, which [`allows`] lists).
const SEAT_QA_PREFIX: &str = "rsi-build-qa-seat-";

/// The one warm build target of a manager seat's QA worktrees (hub #3870 b).
/// `seat_root` is the seat's own sandbox root, the same for every QA worktree
/// the seat builds in, so a cold build is paid once per seat and not once per
/// head. Reclaim sees the seat root as the registered source: while the seat
/// lives the age floor keeps the target, and once the root is gone the target
/// is orphaned and reclaimable at once (#646). The prefix differs from the
/// per-worktree one, so the two kinds never collide.
pub(crate) fn seat_qa_target(seat_root: &Path) -> io::Result<PathBuf> {
    seat_qa_target_in(
        &rsi_common::identity::data_dir().join("build-targets"),
        &registry::default_registry_path(),
        seat_root,
    )
}

pub(super) fn seat_qa_target_in(
    parent: &Path,
    registry_path: &Path,
    seat_root: &Path,
) -> io::Result<PathBuf> {
    registered_target_in(parent, registry_path, SEAT_QA_PREFIX, seat_root)
}

/// A registered Cargo target named for the canonical `source` path, which the
/// allocation registry records as its owner.
fn registered_target_in(
    parent: &Path,
    registry_path: &Path,
    prefix: &str,
    source: &Path,
) -> io::Result<PathBuf> {
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::DirBuilderExt;
    let cwd = source.canonicalize()?;
    let digest = Sha256::digest(cwd.as_os_str().as_encoded_bytes());
    let name = format!("{prefix}{digest:x}");
    match fs::DirBuilder::new().mode(0o700).create(parent) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let uid = super::current_uid();
    let root = fsys::open_root(parent, uid)?;
    // Lock the stable parent inode before mkdir: no second allocator can
    // observe the interval between creation and publication of the record.
    let allocation_lock = fsys::open_dir_lock(&root)?;
    use std::os::fd::AsRawFd;
    loop {
        // SAFETY: allocation_lock owns this open descriptor until return.
        if unsafe { nix::libc::flock(allocation_lock.as_raw_fd(), nix::libc::LOCK_EX) } == 0 {
            break;
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
    let path = parent.join(&name);
    // The registered source worktree lets reclaim tell that it is gone (#646).
    match record::create_scratch_dir_for_in(
        registry_path,
        parent,
        &name,
        RootKind::CargoTarget,
        Some(&cwd),
    ) {
        Ok(_) => {
            let fresh = fsys::pin_child(&root, &name)?;
            fsys::create_file(&fresh, "CACHEDIR.TAG", CACHE_SIGNATURE.as_bytes())?;
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let pinned = fsys::pin_child(&root, &name)?;
    let rec = record::read_record(&pinned)
        .ok_or_else(|| io::Error::other("manager target is unrecorded"))?;
    let entry = registry::Registry::open(registry_path, uid)?
        .get(&rec.nonce)
        .ok_or_else(|| io::Error::other("manager target allocation is missing"))?;
    if rec.kind != RootKind::CargoTarget.tag()
        || !entry.binds(&pinned, &root, RootKind::CargoTarget.tag(), uid)
        || !cache_only(
            &pinned,
            &mut Budget::new(Instant::now() + Duration::from_secs(5), 1024),
        )
    {
        return Err(io::Error::other(
            "manager target identity or Cargo layout changed",
        ));
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn legacy_roots_cover_manager_review_staging_and_satellite_targets() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join(".rsi");
        fs::create_dir_all(data.join("mgr/seat")).unwrap();
        let roots = known_roots(&data, Some(temp.path()));
        for path in [
            "build-targets",
            "review-targets",
            "mgr/seat",
            "staging",
            "cloud",
            "koplik-mgr",
        ] {
            assert!(
                roots.iter().any(|(root, _)| root == &data.join(path)),
                "{path}"
            );
        }
        for path in [".cargo", ".cache"] {
            assert!(
                roots
                    .iter()
                    .any(|(root, _)| root == &temp.path().join(path)),
                "{path}"
            );
        }
        for name in [
            "target",
            "review-target-worker",
            "target-integ",
            "target-integ2",
            "artifact-target",
            "integration-target",
            "sat-target-deploy",
            "shared-target",
            "rsi-review-worker",
            "koplik-review-worker",
        ] {
            assert!(allows(name), "{name}");
        }
        // `~/.cache` is also the worker-cache root: a worker tmp name belongs
        // to exactly one kind, so it is never listed or reclaimed twice.
        for name in ["rsi-review-x-tmp", "rsi-w1-tmp", "release-target"] {
            assert!(!allows(name), "{name}");
        }
    }
}
