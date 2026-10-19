//! The calling package's directory, found at run time (#1741).
//!
//! `env!("CARGO_MANIFEST_DIR")` bakes the absolute checkout path into the
//! compiled code, and Cargo does not treat that variable as a build input, so
//! an artifact built in one checkout keeps reading the other checkout's files.
//! That makes the crate unshareable between worker sandboxes (the warm target
//! pool, `scripts/target-pool`, rebuilds every unit whose rustc dep-info reads
//! the variable). [`manifest_dir!`](crate::manifest_dir) is the drop-in
//! replacement: it returns the same `&'static str`, resolved when it runs.
//!
//! Resolution order:
//! 1. `CARGO_MANIFEST_DIR` from the process environment, when
//!    `CARGO_PKG_NAME` there names the calling package (`cargo test`,
//!    `cargo run` and nextest set both for the binary they run);
//! 2. otherwise the package's workspace-relative directory, taken from the
//!    caller's `file!()` (Cargo compiles workspace members from the workspace
//!    root, so it reads `crates/<pkg>/src/...`), joined to the first ancestor
//!    of the current directory or of the running executable that holds
//!    `<that directory>/Cargo.toml`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// The calling package's directory as a `&'static str`, resolved at run time.
/// Use it instead of `env!("CARGO_MANIFEST_DIR")` everywhere except inside
/// `include!`/`include_str!`/`include_bytes!` (those read at compile time and
/// Cargo tracks the included file).
/// `manifest_dir!("/suffix")` replaces `concat!(env!("CARGO_MANIFEST_DIR"), "/suffix")`.
#[macro_export]
macro_rules! manifest_dir {
    () => {
        $crate::manifest_dir::resolve(::core::env!("CARGO_PKG_NAME"), ::core::file!())
    };
    ($suffix:literal) => {
        $crate::manifest_dir::joined(
            $crate::manifest_dir::resolve(::core::env!("CARGO_PKG_NAME"), ::core::file!()),
            $suffix,
        )
    };
}

/// `dir` followed by `suffix`, cached so each pair is allocated once.
pub fn joined(dir: &'static str, suffix: &'static str) -> &'static str {
    static CACHE: OnceLock<Mutex<HashMap<(&'static str, &'static str), &'static str>>> =
        OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
    cache
        .entry((dir, suffix))
        .or_insert_with(|| Box::leak(format!("{dir}{suffix}").into_boxed_str()))
}

/// Resolve and cache the directory of `package`, whose source file `file` is
/// the caller's `file!()`. Panics with an explanation when it cannot be found.
pub fn resolve(package: &'static str, file: &'static str) -> &'static str {
    try_resolve(package, file).unwrap_or_else(|| {
        panic!(
            "cannot locate package `{package}` ({}/Cargo.toml) from the current \
             directory or executable; run it through cargo or from inside its checkout",
            package_relative_dir(file)
        )
    })
}

/// [`resolve`] without the panic, for production fallbacks: `None` when the
/// package is not reachable from the current directory or executable.
/// `try_manifest_dir!()` calls it with the caller's package and file.
pub fn try_resolve(package: &'static str, file: &'static str) -> Option<&'static str> {
    static CACHE: OnceLock<Mutex<HashMap<(&'static str, &'static str), &'static str>>> =
        OnceLock::new();
    let relative = package_relative_dir(file);
    let key = (package, relative);
    let cache = CACHE.get_or_init(Default::default);
    if let Some(found) = cache.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
        return Some(found);
    }
    let found = find(package, relative)?;
    let leaked: &'static str = Box::leak(found.to_string_lossy().into_owned().into_boxed_str());
    cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key, leaked);
    Some(leaked)
}

/// `Option<&'static str>`: the calling package's directory when it can be
/// found at run time (see [`try_resolve`]).
#[macro_export]
macro_rules! try_manifest_dir {
    () => {
        $crate::manifest_dir::try_resolve(::core::env!("CARGO_PKG_NAME"), ::core::file!())
    };
}

/// `crates/rsid` for `crates/rsid/src/session/x.rs` or `crates/rsid/tests/y.rs`;
/// `""` for a package at the workspace root (`src/lib.rs`).
pub fn package_relative_dir(file: &str) -> &str {
    let file = file.trim_start_matches("./");
    let mut cut = file.len();
    for marker in ["src/", "tests/", "benches/", "examples/", "build.rs"] {
        let at_start = file.starts_with(marker).then_some(0);
        let nested = file.find(&format!("/{marker}")).map(|index| index + 1);
        if let Some(index) = at_start.or(nested) {
            cut = cut.min(index);
        }
    }
    file[..cut].trim_end_matches('/')
}

fn find(package: &str, relative: &str) -> Option<PathBuf> {
    if std::env::var("CARGO_PKG_NAME").ok().as_deref() == Some(package)
        && let Some(dir) = std::env::var_os("CARGO_MANIFEST_DIR")
        && Path::new(&dir).join("Cargo.toml").is_file()
    {
        return Some(PathBuf::from(dir));
    }
    let starts = [
        std::env::current_dir().ok(),
        std::env::current_exe()
            .ok()
            .and_then(|exe| exe.canonicalize().ok()),
    ];
    starts.into_iter().flatten().find_map(|start| {
        start.ancestors().find_map(|ancestor| {
            let candidate = ancestor.join(relative);
            is_package(&candidate, package).then_some(candidate)
        })
    })
}

fn is_package(dir: &Path, package: &str) -> bool {
    let Ok(manifest) = std::fs::read_to_string(dir.join("Cargo.toml")) else {
        return false;
    };
    manifest.lines().any(|line| {
        let line = line.trim();
        line.strip_prefix("name")
            .map(str::trim_start)
            .and_then(|rest| rest.strip_prefix('='))
            .map(|value| value.trim().trim_matches('"') == package)
            .unwrap_or(false)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_relative_dir_strips_the_target_directory_part() {
        assert_eq!(
            package_relative_dir("crates/rsid/src/session/x.rs"),
            "crates/rsid"
        );
        assert_eq!(
            package_relative_dir("crates/rsid/tests/it.rs"),
            "crates/rsid"
        );
        assert_eq!(
            package_relative_dir("crates/rsid/src/bin/tool.rs"),
            "crates/rsid"
        );
        assert_eq!(package_relative_dir("crates/x/benches/b.rs"), "crates/x");
        assert_eq!(package_relative_dir("src/lib.rs"), "");
        assert_eq!(
            package_relative_dir("crates/rsid/tests/fixtures/src/a.rs"),
            "crates/rsid"
        );
    }

    #[test]
    fn resolves_this_package_without_cargo_runtime_variables() {
        let dir = Path::new(crate::manifest_dir!());
        assert!(
            dir.join("src/manifest_dir.rs").is_file(),
            "{}",
            dir.display()
        );
        assert!(is_package(dir, "rsi-common"));
    }

    #[test]
    fn suffix_form_matches_concat_of_the_directory() {
        let joined = crate::manifest_dir!("/src/manifest_dir.rs");
        assert_eq!(
            joined,
            format!("{}/src/manifest_dir.rs", crate::manifest_dir!())
        );
        assert!(Path::new(joined).is_file());
    }

    #[test]
    fn try_form_finds_this_package_and_misses_an_unknown_one() {
        assert_eq!(crate::try_manifest_dir!(), Some(crate::manifest_dir!()));
        assert_eq!(
            try_resolve("no-such-package-1741", "crates/nowhere/src/lib.rs"),
            None
        );
    }

    #[test]
    fn walks_up_from_a_nested_directory_to_the_package() {
        let here = Path::new(crate::manifest_dir!());
        let found = here
            .join("src")
            .ancestors()
            .find_map(|ancestor| {
                let candidate = ancestor.join(package_relative_dir(file!()));
                is_package(&candidate, "rsi-common").then_some(candidate)
            })
            .unwrap();
        assert_eq!(found, here);
    }
}
