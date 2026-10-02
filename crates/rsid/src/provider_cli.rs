//! Provider CLI lookup that does not depend on the PATH the daemon inherited
//! (#1087).
//!
//! A daemon started by a supervisor or a service manager often has a bare PATH
//! (`/usr/local/bin:/usr/bin`), while the provider CLIs live in the user's
//! `~/.local/bin` or `~/.cargo/bin`. Every provider lookup goes through
//! [`resolve`]: the inherited PATH first, then a fixed list of well-known
//! install directories.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Provider CLIs whose absence the daemon reports by name, in stable order.
pub const PROVIDER_CLI_NAMES: [&str; 3] = ["claude", "codex", "agy"];

/// The home-relative install directories searched after the inherited PATH.
const HOME_FALLBACK_DIRS: [&str; 3] = [".local/bin", ".cargo/bin", ".claude/local"];
/// Absolute install directories searched last.
const SYSTEM_FALLBACK_DIRS: [&str; 4] = [
    "/usr/local/bin",
    "/usr/bin",
    "/opt/homebrew/bin",
    "/snap/bin",
];

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

/// The fixed fallback search list for `home`.
#[must_use]
pub fn fallback_dirs(home: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = home {
        dirs.extend(HOME_FALLBACK_DIRS.iter().map(|dir| home.join(dir)));
    }
    dirs.extend(SYSTEM_FALLBACK_DIRS.iter().map(PathBuf::from));
    dirs
}

/// Resolve `name` against `path_env` first, then the fixed fallback list.
#[must_use]
pub fn resolve_with(
    name: &str,
    path_env: Option<OsString>,
    home: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(path_env) = path_env
        && let Ok(found) = which::which_in(name, Some(path_env), ".")
    {
        return Some(found);
    }
    let fallback = std::env::join_paths(fallback_dirs(home)).ok()?;
    which::which_in(name, Some(fallback), ".").ok()
}

/// Resolve a provider CLI from the process PATH, then the fixed fallback list.
#[must_use]
pub fn resolve(name: &str) -> Option<PathBuf> {
    resolve_with(name, std::env::var_os("PATH"), home_dir().as_deref())
}

/// The first of `names` that resolves.
#[must_use]
pub fn resolve_any(names: &[&str]) -> Option<PathBuf> {
    names.iter().find_map(|name| resolve(name))
}

/// Names of provider CLIs that cannot be found by [`resolve`]. The Antigravity
/// CLI is reported as `agy` whichever of its binary names is missing.
#[must_use]
pub fn missing_provider_clis() -> Vec<String> {
    PROVIDER_CLI_NAMES
        .iter()
        .filter(|name| {
            if **name == "agy" {
                resolve_any(&["agy", "antigravity", "antigravity-cli"]).is_none()
            } else {
                resolve(name).is_none()
            }
        })
        .map(|name| (*name).to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn fake_cli(dir: &Path, name: &str) -> PathBuf {
        std::fs::create_dir_all(dir).expect("dir");
        let path = dir.join(name);
        std::fs::write(&path, "#!/bin/sh\nexit 0\n").expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        path
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn a_bare_path_still_finds_a_cli_in_home_local_bin() {
        let home = tempfile::tempdir().expect("home");
        let expected = fake_cli(&home.path().join(".local/bin"), "claude-1087-fake");
        let found = resolve_with(
            "claude-1087-fake",
            Some(OsString::from("/usr/bin:/usr/local/bin")),
            Some(home.path()),
        );
        assert_eq!(found, Some(expected));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn the_inherited_path_wins_over_the_fallback_list() {
        let home = tempfile::tempdir().expect("home");
        let on_path = tempfile::tempdir().expect("path dir");
        fake_cli(&home.path().join(".local/bin"), "codex-1087-fake");
        let expected = fake_cli(on_path.path(), "codex-1087-fake");
        let found = resolve_with(
            "codex-1087-fake",
            Some(on_path.path().as_os_str().to_owned()),
            Some(home.path()),
        );
        assert_eq!(found, Some(expected));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn a_cli_in_no_searched_directory_is_not_found() {
        let home = tempfile::tempdir().expect("home");
        let found = resolve_with(
            "no-such-cli-1087",
            Some(OsString::from("/usr/bin")),
            Some(home.path()),
        );
        assert_eq!(found, None);
    }
}
