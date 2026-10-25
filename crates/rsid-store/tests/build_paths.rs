//! Regression for #373: the generated `include!` paths of `store_migrations.rs`
//! must resolve when the target directory sits outside the checkout and is
//! reached through a symlink (macOS `/tmp` -> `/private/tmp`).

#[allow(dead_code)]
#[path = "../build_paths.rs"]
mod build_paths;

use std::fs;
use std::path::Path;

/// Resolve `relative` from the generated file's directory the way rustc does:
/// the OS follows `..` from the physical location of `out`.
fn resolve_include(out: &Path, relative: &Path) -> std::path::PathBuf {
    fs::canonicalize(out.join(relative)).expect("include path names an existing file")
}

fn seed_checkout(root: &Path) -> std::path::PathBuf {
    let sources = root.join("checkout/crates/rsid-store/src/store/migrations");
    fs::create_dir_all(&sources).unwrap();
    fs::write(sources.join("v000.rs"), "// v000\n").unwrap();
    sources
}

#[test]
fn include_directory_resolves_for_target_outside_checkout() {
    let temp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let sources = seed_checkout(&root);
    let out = root.join("target/debug/build/rsid-store-abc/out");
    fs::create_dir_all(&out).unwrap();

    let relative = build_paths::include_directory(&out, &sources).expect("shared root");
    assert_eq!(
        resolve_include(&out, &relative.join("v000.rs")),
        sources.join("v000.rs")
    );
}

#[cfg(unix)]
#[test]
fn include_directory_resolves_when_out_dir_is_reached_through_a_symlink() {
    let temp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let sources = seed_checkout(&root);

    // The physical target is two levels deeper than the symlink that names it,
    // like `/tmp` -> `/private/tmp`.
    let physical = root.join("volume/private/tmp");
    fs::create_dir_all(physical.join("target/debug/build/rsid-store-abc/out")).unwrap();
    std::os::unix::fs::symlink(&physical, root.join("tmp")).unwrap();
    let logical_out = root.join("tmp/target/debug/build/rsid-store-abc/out");

    let relative = build_paths::include_directory(&logical_out, &sources).expect("shared root");
    assert_eq!(
        resolve_include(&logical_out, &relative.join("v000.rs")),
        sources.join("v000.rs")
    );
}

#[test]
fn include_directory_is_none_without_a_shared_root() {
    // Windows drives share no root; elsewhere a relative pair of different
    // first components models the same "no common ancestor" fallback.
    assert!(build_paths::relative_to(Path::new("a/out"), Path::new("b/src")).is_none());
}
