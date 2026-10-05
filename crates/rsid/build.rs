/// Full commit SHA embedded as `RSI_BUILD_SHA` for `AgentGetDaemonInfo`
/// (#1045). `RSI_BUILD_SHA` in the build environment wins (release builds from
/// a source tarball); otherwise `git rev-parse HEAD`; otherwise `unknown`.
fn emit_build_sha() {
    println!("cargo:rerun-if-env-changed=RSI_BUILD_SHA");
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
            .filter(|text| !text.is_empty())
    };
    for path in ["HEAD", "logs/HEAD"] {
        if let Some(found) = git(&["rev-parse", "--git-path", path]) {
            println!("cargo:rerun-if-changed={found}");
        }
    }
    let sha = std::env::var("RSI_BUILD_SHA")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| git(&["rev-parse", "HEAD"]))
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=RSI_BUILD_SHA={sha}");
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

fn main() {
    refuse_test_seam_in_release();
    emit_build_sha();
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
