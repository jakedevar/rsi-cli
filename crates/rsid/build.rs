/// The digest code is shared with `rsi-common`; it depends on `std` and `sha2` only.
#[allow(dead_code)]
#[path = "../rsi-common/src/harness_source_digest.rs"]
mod harness_source_digest;

#[path = "../rsi-common/build_support/build_identity.rs"]
mod build_identity;

/// The harness source digest of the build commit, embedded as `RSI_HARNESS_SOURCE_DIGEST`
/// (#3192): a sha256 over the git tree or blob ids, at `sha`, of the paths in
/// `crates/rsi-common/data/harness_source_paths.txt`. A redeploy that leaves those paths alone
/// keeps the digest, so a benchmark baseline measured on an earlier build stays comparable. When
/// git cannot say (no repository, an unknown commit, uncommitted changes under a listed path) the
/// value is `unavailable:<sha>`, which the paired compare never accepts.
fn emit_harness_source_digest(sha: &str, pinned: bool) {
    let manifest = std::path::PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string()),
    );
    let repo = manifest.join("../..");
    println!("cargo:rerun-if-changed=../rsi-common/data/harness_source_paths.txt");
    if !pinned {
        // An unpinned build describes the working tree, so an edit under a listed path must
        // re-evaluate the dirty check. A pinned build is a fixed commit and must not rerun when
        // files move (see `emit_build_sha`).
        if let Ok(paths) = harness_source_digest::parse_path_list(harness_source_digest::PATH_LIST)
        {
            for entry in paths {
                println!(
                    "cargo:rerun-if-changed={}",
                    repo.join(entry.git_path()).display()
                );
            }
        }
    }
    let digest = harness_source_digest::embedded_source_digest(&repo, sha);
    println!("cargo:rustc-env=RSI_HARNESS_SOURCE_DIGEST={digest}");
}

/// Main worktree of the repo this binary is built from, embedded as
/// `RSI_BUILD_MAIN_WORKTREE` (#1613). A daemon built in a manager sandbox
/// (a git linked worktree) would otherwise embed a path that matches no
/// project and that disappears when the sandbox is reclaimed; the parent of
/// the git common dir is the stable checkout. Empty when git is missing, the
/// source is not a repo or the common dir is not a `.git` directory (bare
/// repo): the runtime then falls back to resolving the build dir itself.
fn emit_build_main_worktree() {
    let main = std::process::Command::new("git")
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .map(std::path::PathBuf::from)
        .filter(|common| common.file_name().is_some_and(|name| name == ".git"))
        .and_then(|common| common.parent().map(std::path::Path::to_path_buf))
        .map(|main| main.display().to_string())
        .unwrap_or_default();
    println!("cargo:rustc-env=RSI_BUILD_MAIN_WORKTREE={main}");
}

// SEAM-GUARD-BEGIN
/// `test-seam` carries fake capabilities and route-validation bypasses; it must
/// never be compiled into a release build. A release-mode test run that needs
/// it opts in explicitly with `RSID_ALLOW_TEST_SEAM_RELEASE=1`.
fn refuse_test_seam_in_release() {
    println!("cargo:rerun-if-env-changed=RSID_ALLOW_TEST_SEAM_RELEASE");
    let seam = std::env::var_os("CARGO_FEATURE_TEST_SEAM").is_some();
    let release = std::env::var("PROFILE").is_ok_and(|profile| profile == "release");
    let allowed = std::env::var("RSID_ALLOW_TEST_SEAM_RELEASE").is_ok_and(|value| value == "1");
    assert!(
        !(seam && release && !allowed),
        "the `test-seam` feature is enabled in a release build; it must never ship \
         (set RSID_ALLOW_TEST_SEAM_RELEASE=1 only for a release-mode test run)"
    );
}
// SEAM-GUARD-END

// E2E-SEED-GUARD-BEGIN
/// `e2e-seed` compiles the `rsi-e2e-seed` fixture writer (local #122); it writes a database, so it must
/// never be compiled into a release build and, unlike `test-seam`, has no release opt-in (hub #3244).
fn refuse_e2e_seed_in_release() {
    let seed = std::env::var_os("CARGO_FEATURE_E2E_SEED").is_some();
    let release = std::env::var("PROFILE").is_ok_and(|profile| profile == "release");
    assert!(
        !(seed && release),
        "the `e2e-seed` feature is enabled in a release build; it must never ship \
         (build the seeder in the debug profile only; there is no release override)"
    );
}
// E2E-SEED-GUARD-END

fn main() {
    refuse_test_seam_in_release();
    refuse_e2e_seed_in_release();
    println!("cargo:rerun-if-changed=../rsi-common/build_support/build_identity.rs");
    let (build_sha, pinned) = build_identity::emit_build_sha();
    emit_harness_source_digest(&build_sha, pinned);
    emit_build_main_worktree();
    // Get the SQLite include directory from the bundled libsqlite3-sys crate.
    // This provides sqlite3.h / sqlite3ext.h needed by the sqlite-vec extension.
    let sqlite_include = std::env::var("DEP_SQLITE3_INCLUDE").unwrap_or_default();

    let mut build = cc::Build::new();
    build
        .file("vendor/sqlite-vec/sqlite-vec.c")
        .include("vendor/sqlite-vec");

    if !sqlite_include.is_empty() {
        build.include(&sqlite_include);
        // Compile as core extension when we have the SQLite headers
        build.define("SQLITE_CORE", None);
    }

    build.warnings(false).compile("sqlite_vec");

    println!("cargo:rerun-if-changed=vendor/sqlite-vec/sqlite-vec.c");
    println!("cargo:rerun-if-changed=vendor/sqlite-vec/sqlite-vec.h");
}
