//! Where an archive directory may live (#2435).
//!
//! The archive must never sit inside a sandbox root (a sandbox is reclaimed or
//! committed as a unit) or inside a `.git` directory. A lexical prefix test is
//! not enough: `..` segments and symlinks reach the same place by another
//! spelling. Every check here therefore resolves the path first, the way the
//! operating system would on open (existing prefix canonicalized, so symlinks and
//! `..` after a symlink are followed), and compares the result.
//!
//! The same check runs when the setting is written and again when a pass starts,
//! so a link planted or retargeted afterwards is still refused before any file is
//! created.

use std::io;
use std::path::{Component, Path, PathBuf};

/// The directories an archive may never resolve into: the daemon's sandbox base
/// (`RSI_SANDBOX_BASE`, default `~/.rsi/sandboxes`) and the default sandbox
/// location under the data directory.
#[must_use]
pub fn default_forbidden_roots() -> Vec<PathBuf> {
    forbidden_roots_for(&crate::config::sandbox_base_from_env())
}

/// [`default_forbidden_roots`] for a daemon whose sandbox base is `sandbox_base`.
#[must_use]
pub fn forbidden_roots_for(sandbox_base: &Path) -> Vec<PathBuf> {
    let mut roots = vec![sandbox_base.to_path_buf()];
    let default = rsi_common::identity::data_dir().join("sandboxes");
    if !roots.contains(&default) {
        roots.push(default);
    }
    roots
}

/// The real location of an absolute `path`: the longest existing prefix is
/// canonicalized (symlinks followed, `..` applied after them) and the part that
/// does not exist yet is appended. Refuses a relative path, a dangling symlink
/// and a `.` or `..` segment that follows a missing directory, because none of
/// those has a place the operating system could be asked about.
pub fn resolve_real_path(path: &Path) -> io::Result<PathBuf> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected an absolute path",
        ));
    }
    let components: Vec<Component<'_>> = path.components().collect();
    for keep in (1..=components.len()).rev() {
        let prefix: PathBuf = components[..keep].iter().collect();
        match std::fs::canonicalize(&prefix) {
            Ok(mut real) => {
                for component in &components[keep..] {
                    match component {
                        Component::Normal(name) => real.push(name),
                        _ => {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidInput,
                                "a . or .. segment follows a directory that does not exist",
                            ));
                        }
                    }
                }
                return Ok(real);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                // `canonicalize` also reports NotFound for a symlink whose target
                // is missing; creating a directory there would follow the link.
                if std::fs::symlink_metadata(&prefix).is_ok() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("{} is a dangling symlink", prefix.display()),
                    ));
                }
            }
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "no part of the path exists",
    ))
}

fn has_git_component(path: &Path) -> bool {
    path.components()
        .any(|component| matches!(component, Component::Normal(name) if name == ".git"))
}

/// Checks that `dir` resolves outside every forbidden root and outside any `.git`
/// directory, and returns the resolved path (no symlinks, no `..`). A pass uses
/// the returned path, not the configured spelling.
pub fn check_archive_dir(dir: &Path, forbidden_roots: &[PathBuf]) -> Result<PathBuf, String> {
    let real = resolve_real_path(dir).map_err(|error| {
        format!(
            "cannot resolve the archive directory {}: {error}",
            dir.display()
        )
    })?;
    if has_git_component(&real) {
        return Err(format!(
            "the archive directory resolves to {}, inside a .git directory",
            real.display()
        ));
    }
    for root in forbidden_roots {
        // A root that cannot be resolved still forbids its own spelling.
        let root_real = resolve_real_path(root).unwrap_or_else(|_| root.clone());
        if real.starts_with(&root_real) || real.starts_with(root) {
            return Err(format!(
                "the archive directory resolves to {}, inside the sandbox root {}",
                real.display(),
                root.display()
            ));
        }
    }
    Ok(real)
}

/// Validates a `cold_archive_dir` value against `forbidden_roots` and returns it
/// as the operator should store it. `""` selects the default
/// (`<data dir>/archive`, which a pass checks too). A path must be absolute, free
/// of NUL and line breaks and of `.` / `..` segments, and must resolve (symlinks
/// followed) outside every forbidden root and any `.git` directory. Whether the
/// directory is creatable and writable is checked when a pass uses it.
pub fn validate_archive_dir_setting_with(
    raw: &str,
    forbidden_roots: &[PathBuf],
) -> Result<String, String> {
    let value = raw.trim();
    if value.is_empty() {
        return Ok(String::new());
    }
    if value.len() > 4096 || value.contains(['\0', '\n', '\r']) {
        return Err("expected a path of at most 4096 characters without line breaks".into());
    }
    let path = Path::new(value);
    if !path.is_absolute() {
        return Err("expected an absolute path (or \"\" for the default)".into());
    }
    if value
        .split(['/', '\\'])
        .any(|segment| segment == "." || segment == "..")
    {
        return Err("expected a normalized path without . or .. segments".into());
    }
    if has_git_component(path) {
        return Err("the archive directory must not be inside a .git directory".into());
    }
    check_archive_dir(path, forbidden_roots)?;
    Ok(value.to_string())
}

/// [`validate_archive_dir_setting_with`] against [`default_forbidden_roots`].
pub fn validate_archive_dir_setting(raw: &str) -> Result<String, String> {
    validate_archive_dir_setting_with(raw, &default_forbidden_roots())
}
