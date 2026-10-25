//! Path arithmetic for the generated `store_migrations.rs` (#373). Shared by
//! `build.rs` (`#[path]` module) and `tests/build_paths.rs`, so the regression
//! test exercises the code the build script runs.

use std::path::{Path, PathBuf};

/// `to` relative to the directory `from`; `None` when they share no root (a
/// different drive on Windows).
pub(crate) fn relative_to(from: &Path, to: &Path) -> Option<PathBuf> {
    let from: Vec<_> = from.components().collect();
    let to: Vec<_> = to.components().collect();
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    if common == 0 {
        return None;
    }
    let mut relative = PathBuf::new();
    for _ in common..from.len() {
        relative.push("..");
    }
    for component in &to[common..] {
        relative.push(component.as_os_str());
    }
    Some(relative)
}

/// The directory `include!` paths are written relative to, from the generated
/// file in `out_directory` to the migration sources in `sources_directory`.
///
/// rustc resolves a relative `include!` from the generated file's *physical*
/// location, so a `..` chain counted from a logical `OUT_DIR` that is reached
/// through a symlink (macOS `/tmp` -> `/private/tmp`, a target dir on a linked
/// volume) lands one level too shallow and the include fails. Both sides are
/// therefore canonicalized first; a side that cannot be canonicalized keeps its
/// logical path. `None` when the two share no root: the caller falls back to
/// a compile-time manifest-path macro.
pub(crate) fn include_directory(out_directory: &Path, sources_directory: &Path) -> Option<PathBuf> {
    let out = out_directory
        .canonicalize()
        .unwrap_or_else(|_| out_directory.to_path_buf());
    let sources = sources_directory
        .canonicalize()
        .unwrap_or_else(|_| sources_directory.to_path_buf());
    relative_to(&out, &sources)
}
