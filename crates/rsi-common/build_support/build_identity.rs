/// Full commit SHA embedded as `RSI_BUILD_SHA` for `AgentGetDaemonInfo`
/// (#1045). `RSI_BUILD_SHA` in the build environment wins (release builds from
/// a source tarball, merge-queue gate builds); otherwise `git rev-parse HEAD`;
/// otherwise `unknown`.
///
/// A pinned id never consults git, so nothing in the repository can change it
/// and the script must not rerun when `HEAD` moves (#2508): a warm lander slot
/// re-clones the candidate at every gate, which rewrites `.git/HEAD`, and a
/// rerun makes Cargo recompile this crate and everything above it.
pub fn emit_build_sha() -> (String, bool) {
    println!("cargo:rerun-if-env-changed=RSI_BUILD_SHA");
    if let Some(sha) = std::env::var("RSI_BUILD_SHA")
        .ok()
        .filter(|value| !value.trim().is_empty())
    {
        println!("cargo:rustc-env=RSI_BUILD_SHA={sha}");
        return (sha, true);
    }
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
    let sha = git(&["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=RSI_BUILD_SHA={sha}");
    (sha, false)
}
