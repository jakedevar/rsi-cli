//! Extra writable roots for a Codex `workspace-write` sandbox (#1202).
//!
//! `workspace-write` makes only the working directory writable. A sandbox
//! worktree keeps its Git administration outside it (`<repo>/.git/worktrees/<id>`
//! for the index, `HEAD`, `FETCH_HEAD`, and the shared `objects`, `refs` and
//! `logs` in the repository's common directory), and the build wrapper keeps
//! its slot state under `RSI_BUILD_SLOT_ROOT`. Without those paths a worker
//! can neither fetch nor commit and `rsi-build-rustc` fails `rustc -vV`.
//!
//! The roots granted are exactly those: never the whole common directory
//! (its `config` and `hooks` stay read-only), never the home directory. The
//! Git pointer files live in the writable workspace, so nothing they say is
//! trusted on its own: the expected common directory comes from the daemon's
//! sandbox custody record (see [`git_write_roots`]).

use std::path::{Path, PathBuf};

/// The environment names the build wrapper state is read from.
const BUILD_SLOT_ROOT_ENV: &str = "RSI_BUILD_SLOT_ROOT";
const SCCACHE_DIR_ENV: &str = "SCCACHE_DIR";
const SCCACHE_SERVER_UDS_ENV: &str = "SCCACHE_SERVER_UDS";

/// Directories of the repository common directory a commit or fetch writes.
const COMMON_WRITE_DIRS: [&str; 3] = ["objects", "refs", "logs"];

/// Writable roots for a linked worktree at `working_dir`: its Git admin
/// directory plus the common `objects`, `refs` and `logs`.
///
/// `trusted_common` is the repository's Git common directory as the daemon's
/// custody row recorded it at allocation (#2746). The workspace's `.git`
/// pointer and the admin directory's own `commondir`/`gitdir` files are
/// writable by the worker, so they only count when they agree with that
/// anchor: the admin directory must sit directly under
/// `<trusted_common>/worktrees`, point back at this workspace's `.git`, and
/// name the trusted common directory. A forged registry elsewhere (inside the
/// workspace or under a granted root) yields nothing. Each common directory is
/// granted only when it is a real directory (not a symlink) that canonicalizes
/// to itself inside the common directory. Empty when the directory is not a
/// linked worktree (a plain `.git` directory is already inside the workspace).
pub(super) fn git_write_roots(working_dir: &Path, trusted_common: &Path) -> Vec<PathBuf> {
    let (Ok(root), Ok(common)) = (
        std::fs::canonicalize(working_dir),
        std::fs::canonicalize(trusted_common),
    ) else {
        return Vec::new();
    };
    let Ok(pointer) = std::fs::read_to_string(root.join(".git")) else {
        return Vec::new();
    };
    let Some(admin) = pointer.trim().strip_prefix("gitdir:").map(str::trim) else {
        return Vec::new();
    };
    let Ok(admin) = std::fs::canonicalize(admin) else {
        return Vec::new();
    };
    let Ok(registry) = std::fs::canonicalize(common.join("worktrees")) else {
        return Vec::new();
    };
    if registry.parent() != Some(common.as_path()) || admin.parent() != Some(registry.as_path()) {
        return Vec::new();
    }
    let common_matches = std::fs::read_to_string(admin.join("commondir"))
        .ok()
        .and_then(|text| std::fs::canonicalize(admin.join(text.trim())).ok())
        .is_some_and(|resolved| resolved == common);
    let reverse_matches = std::fs::read_to_string(admin.join("gitdir"))
        .ok()
        .is_some_and(|text| Path::new(text.trim()) == root.join(".git"));
    if !common_matches || !reverse_matches {
        return Vec::new();
    }
    let mut roots = vec![admin];
    roots.extend(COMMON_WRITE_DIRS.iter().filter_map(|name| {
        let path = common.join(name);
        let is_real_dir = std::fs::symlink_metadata(&path)
            .ok()
            .is_some_and(|meta| meta.is_dir());
        let stays_inside = std::fs::canonicalize(&path).ok().as_deref() == Some(path.as_path());
        (is_real_dir && stays_inside).then_some(path)
    }));
    roots
}

/// Writable roots for the build wrapper state the stamped environment names:
/// the slot root (created when missing, since the wrapper needs it before its
/// first compile) and the shared compiler-cache directory when one is in use.
///
/// The cache directory is created before it is granted (#2745): on a cold
/// start neither it nor the server socket exists, and the sandbox cannot create
/// them under the read-only `~/.rsi`. The stamped `SCCACHE_SERVER_UDS` lives
/// inside that directory, so one root covers the socket and the cache. A socket
/// anywhere else is not granted (its parent could be the whole state directory):
/// the cache is then omitted from the roots and the launch proceeds without it.
pub(super) fn build_write_roots(env_value: impl Fn(&str) -> Option<PathBuf>) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(slot_root) = env_value(BUILD_SLOT_ROOT_ENV).filter(|path| path.is_absolute()) {
        let _ = std::fs::create_dir_all(&slot_root);
        if slot_root.is_dir() {
            roots.push(slot_root);
        }
    }
    if let Some(cache) = env_value(SCCACHE_DIR_ENV).filter(|path| path.is_absolute()) {
        let socket_inside = env_value(SCCACHE_SERVER_UDS_ENV)
            .is_none_or(|socket| socket.is_absolute() && socket.parent() == Some(cache.as_path()));
        if socket_inside {
            let _ = std::fs::create_dir_all(&cache);
            if cache.is_dir() {
                roots.push(cache);
            }
        }
    }
    roots
}

/// The `-c` override that adds `roots` to `workspace-write`, or `None` when
/// there is nothing to add. Roots are rendered as TOML basic strings.
pub(super) fn writable_roots_override(roots: &[PathBuf]) -> Option<String> {
    if roots.is_empty() {
        return None;
    }
    let rendered: Vec<String> = roots
        .iter()
        .map(|path| {
            serde_json::to_string(&path.to_string_lossy()).unwrap_or_else(|_| "\"\"".into())
        })
        .collect();
    Some(format!(
        "sandbox_workspace_write.writable_roots=[{}]",
        rendered.join(",")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A repository with one linked worktree laid out the way `git worktree add` does.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    struct Fixture {
        _dir: tempfile::TempDir,
        common: PathBuf,
        worktree: PathBuf,
        admin: PathBuf,
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let common = base.join("repo/.git");
        let admin = common.join("worktrees/abc");
        let worktree = base.join("sandboxes/abc");
        for name in COMMON_WRITE_DIRS {
            std::fs::create_dir_all(common.join(name)).unwrap();
        }
        std::fs::create_dir_all(common.join("hooks")).unwrap();
        std::fs::create_dir_all(&admin).unwrap();
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(admin.join("commondir"), "../..\n").unwrap();
        std::fs::write(
            admin.join("gitdir"),
            format!("{}\n", worktree.join(".git").display()),
        )
        .unwrap();
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", admin.display()),
        )
        .unwrap();
        Fixture {
            _dir: dir,
            common,
            worktree,
            admin,
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn linked_worktree_gets_its_admin_dir_and_common_write_dirs_only() {
        let fx = fixture();
        let roots = git_write_roots(&fx.worktree, &fx.common);
        assert_eq!(
            roots,
            vec![
                fx.admin.clone(),
                fx.common.join("objects"),
                fx.common.join("refs"),
                fx.common.join("logs"),
            ]
        );
        assert!(!roots.contains(&fx.common));
        assert!(!roots.contains(&fx.common.join("hooks")));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn plain_git_directory_or_missing_pointer_grants_nothing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        assert!(git_write_roots(dir.path(), dir.path()).is_empty());
        let bare = tempfile::tempdir().unwrap();
        assert!(git_write_roots(bare.path(), bare.path()).is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn pointer_to_another_worktrees_admin_dir_grants_nothing() {
        let fx = fixture();
        let other = fx.worktree.parent().unwrap().join("other");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(
            other.join(".git"),
            format!("gitdir: {}\n", fx.admin.display()),
        )
        .unwrap();
        assert!(git_write_roots(&other, &fx.common).is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn forged_commondir_or_admin_outside_a_registry_grants_nothing() {
        let fx = fixture();
        std::fs::write(fx.admin.join("commondir"), "../../../..\n").unwrap();
        assert!(git_write_roots(&fx.worktree, &fx.common).is_empty());

        let elsewhere = fx.worktree.parent().unwrap().join("loose-admin");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("commondir"), "..\n").unwrap();
        std::fs::write(
            elsewhere.join("gitdir"),
            format!("{}\n", fx.worktree.join(".git").display()),
        )
        .unwrap();
        std::fs::write(
            fx.worktree.join(".git"),
            format!("gitdir: {}\n", elsewhere.display()),
        )
        .unwrap();
        assert!(git_write_roots(&fx.worktree, &fx.common).is_empty());
    }

    /// #2746: a registry forged entirely inside the workspace (consistent
    /// `commondir`/`gitdir`, the workspace `.git` pointing at it, and an
    /// `objects` symlink to an external directory) grants nothing, while the
    /// genuine worktree from custody still gets its roots.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn fully_forged_registry_in_the_workspace_grants_nothing() {
        let fx = fixture();
        let external = fx.worktree.parent().unwrap().join("external");
        std::fs::create_dir_all(&external).unwrap();
        let fake_common = fx.worktree.join("fake");
        let fake_admin = fake_common.join("worktrees/id");
        std::fs::create_dir_all(&fake_admin).unwrap();
        std::fs::create_dir_all(fake_common.join("refs")).unwrap();
        std::fs::create_dir_all(fake_common.join("logs")).unwrap();
        std::os::unix::fs::symlink(&external, fake_common.join("objects")).unwrap();
        std::fs::write(fake_admin.join("commondir"), "../..\n").unwrap();
        std::fs::write(
            fake_admin.join("gitdir"),
            format!("{}\n", fx.worktree.join(".git").display()),
        )
        .unwrap();
        std::fs::write(
            fx.worktree.join(".git"),
            format!("gitdir: {}\n", fake_admin.display()),
        )
        .unwrap();
        assert!(git_write_roots(&fx.worktree, &fx.common).is_empty());
        // Without the custody anchor the forged registry would be accepted.
        let accepted = git_write_roots(&fx.worktree, &fake_common);
        assert!(!accepted.contains(&external));
        assert!(!accepted.contains(&fake_common.join("objects")));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn symlinked_common_write_dir_is_not_granted() {
        let fx = fixture();
        let external = fx.worktree.parent().unwrap().join("external");
        std::fs::create_dir_all(&external).unwrap();
        std::fs::remove_dir_all(fx.common.join("objects")).unwrap();
        std::os::unix::fs::symlink(&external, fx.common.join("objects")).unwrap();
        let roots = git_write_roots(&fx.worktree, &fx.common);
        assert_eq!(
            roots,
            vec![
                fx.admin.clone(),
                fx.common.join("refs"),
                fx.common.join("logs"),
            ]
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn a_registry_under_a_granted_root_is_not_the_custody_registry() {
        let fx = fixture();
        // refs is writable once granted; a registry nested there is forged.
        let nested = fx.common.join("refs");
        let admin = nested.join("worktrees/z");
        std::fs::create_dir_all(&admin).unwrap();
        std::fs::write(admin.join("commondir"), "../..\n").unwrap();
        std::fs::write(
            admin.join("gitdir"),
            format!("{}\n", fx.worktree.join(".git").display()),
        )
        .unwrap();
        std::fs::write(
            fx.worktree.join(".git"),
            format!("gitdir: {}\n", admin.display()),
        )
        .unwrap();
        assert!(git_write_roots(&fx.worktree, &fx.common).is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn build_roots_follow_the_stamped_environment() {
        let dir = tempfile::tempdir().unwrap();
        let slots = dir.path().join("build-slots");
        let cache = dir.path().join("sccache");
        std::fs::create_dir(&cache).unwrap();
        let roots = build_write_roots(|name| match name {
            "RSI_BUILD_SLOT_ROOT" => Some(slots.clone()),
            "SCCACHE_DIR" => Some(cache.clone()),
            _ => None,
        });
        assert_eq!(roots, vec![slots.clone(), cache]);
        assert!(slots.is_dir(), "the slot root is created for the wrapper");
        assert!(build_write_roots(|_| None).is_empty());
        assert!(build_write_roots(|_| Some(PathBuf::from("relative"))).is_empty());
    }

    /// #2745: with neither the cache directory nor the server socket present,
    /// the grant creates the cache directory and the socket's parent is that
    /// granted root, so the sccache server can create its socket and cache.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn cold_start_cache_dir_is_created_and_holds_the_server_socket() {
        let dir = tempfile::tempdir().unwrap();
        let slots = dir.path().join("build-slots");
        let cache = dir.path().join("agent-sccache");
        let socket = cache.join("server.sock");
        assert!(!cache.exists() && !socket.exists());
        let roots = build_write_roots(|name| match name {
            "RSI_BUILD_SLOT_ROOT" => Some(slots.clone()),
            "SCCACHE_DIR" => Some(cache.clone()),
            "SCCACHE_SERVER_UDS" => Some(socket.clone()),
            _ => None,
        });
        assert_eq!(roots, vec![slots, cache.clone()]);
        assert!(
            cache.is_dir(),
            "the cache directory is created before it is granted"
        );
        assert!(
            roots
                .iter()
                .any(|root| socket.parent() == Some(root.as_path()))
        );
    }

    /// #2745: a socket outside the cache directory is never granted by widening
    /// to its parent; the cache is left out of the roots instead.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn socket_outside_the_cache_dir_does_not_grant_its_parent() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("agent-sccache");
        let socket = dir.path().join("agent-sccache.sock");
        let roots = build_write_roots(|name| match name {
            "SCCACHE_DIR" => Some(cache.clone()),
            "SCCACHE_SERVER_UDS" => Some(socket.clone()),
            _ => None,
        });
        assert!(roots.is_empty());
        assert!(!cache.exists());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn override_renders_a_toml_array_or_nothing() {
        assert_eq!(writable_roots_override(&[]), None);
        assert_eq!(
            writable_roots_override(&[PathBuf::from("/a"), PathBuf::from("/b c")]).as_deref(),
            Some("sandbox_workspace_write.writable_roots=[\"/a\",\"/b c\"]")
        );
    }
}
