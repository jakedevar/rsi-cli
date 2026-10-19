//! #2823: a cargo job runs the toolchain the repository pins, or it is refused.
//!
//! A daemon started with a PATH that lacks `~/.cargo/bin` (a bare
//! `systemd-run --user` restart) resolves the system `rustc` for every job
//! unit. Every build then runs a different compiler than `rust-toolchain.toml`
//! pins, and the warning gate fails on untouched code. The refusal names the
//! expected and found compiler and the PATH searched, before a row exists.

use rsi_common::agent_jobs::JOB_TOOLCHAIN_MISMATCH;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const PROBE_DEADLINE: Duration = Duration::from_secs(20);
const MAX_PATH_DETAIL: usize = 512;

/// The `channel` a `rust-toolchain.toml` pins when it is an exact version
/// (`1.94.1`). A channel name such as `stable` or `nightly-2026-01-01` names no
/// version to compare, so it pins nothing checkable here.
fn pinned_version(toml: &str) -> Option<String> {
    for line in toml.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("channel") else {
            continue;
        };
        let value = rest.trim_start().strip_prefix('=')?.trim();
        let value = value.split('#').next()?.trim().trim_matches(['"', '\'']);
        let numeric = !value.is_empty()
            && value.split('.').count() >= 2
            && value
                .split('.')
                .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()));
        return numeric.then(|| value.to_string());
    }
    None
}

/// `rustc 1.94.1 (e71f9a2eb 2026-03-25)` -> `1.94.1`.
fn parse_rustc_version(output: &str) -> Option<String> {
    output
        .split_whitespace()
        .nth(1)
        .filter(|v| v.bytes().next().is_some_and(|b| b.is_ascii_digit()))
        .map(|v| v.split('-').next().unwrap_or(v).to_string())
}

/// The first executable `rustc` along `path`, as the unit's shell would find it.
fn find_on_path(path: &str) -> Option<PathBuf> {
    std::env::split_paths(path)
        .map(|dir| dir.join("rustc"))
        .find(|candidate| candidate.is_file())
}

/// Run `<rustc> --version` in `cwd` so a rustup proxy resolves the directory's
/// toolchain override exactly as the job's cargo would. Bounded: a proxy
/// downloading a missing toolchain must not hold a submit.
fn probe(rustc: &Path, path: &str, cwd: &Path) -> Option<String> {
    let mut child = Command::new(rustc)
        .arg("--version")
        .current_dir(cwd)
        .env("PATH", path)
        .env_remove("RUSTUP_TOOLCHAIN")
        .env_remove("RSI_SESSION_TOKEN")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + PROBE_DEADLINE;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(_)) | Err(_) => return None,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    let mut out = String::new();
    std::io::Read::read_to_string(&mut child.stdout.take()?, &mut out).ok()?;
    parse_rustc_version(&out)
}

fn bounded(path: &str) -> String {
    if path.len() <= MAX_PATH_DETAIL {
        return path.to_string();
    }
    let cut = (0..=MAX_PATH_DETAIL)
        .rev()
        .find(|i| path.is_char_boundary(*i))
        .unwrap_or(0);
    format!("{}...", &path[..cut])
}

/// Refuse a cargo job whose `PATH` resolves a `rustc` other than the version
/// `cwd`'s `rust-toolchain.toml` pins.
///
/// An unpinned or channel-named repository, or a probe that could not run to
/// completion, is not refused: only a proven mismatch (or no `rustc` at all)
/// is.
///
/// # Errors
/// `toolchain_mismatch: ...` naming the expected and found compiler and the
/// PATH searched.
pub(super) fn check(path: Option<&str>, cwd: &Path) -> Result<(), String> {
    let Ok(toml) = std::fs::read_to_string(cwd.join("rust-toolchain.toml")) else {
        return Ok(());
    };
    let Some(expected) = pinned_version(&toml) else {
        return Ok(());
    };
    let path = path.unwrap_or_default();
    let Some(rustc) = find_on_path(path) else {
        return Err(format!(
            "{JOB_TOOLCHAIN_MISMATCH}: rust-toolchain.toml pins rustc {expected}, but no rustc is on the daemon PATH ({}); start the daemon with ~/.cargo/bin on PATH",
            bounded(path)
        ));
    };
    match probe(&rustc, path, cwd) {
        Some(found) if found != expected => Err(format!(
            "{JOB_TOOLCHAIN_MISMATCH}: rust-toolchain.toml pins rustc {expected}, but the daemon PATH resolves {} to rustc {found} (PATH searched: {}); start the daemon with ~/.cargo/bin on PATH",
            rustc.display(),
            bounded(path)
        )),
        _ => Ok(()),
    }
}

#[cfg(test)]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
mod tests {
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn pinned_version_reads_only_an_exact_version_channel() {
        assert_eq!(
            pinned_version("[toolchain]\nchannel = \"1.94.1\"\ncomponents = []\n").as_deref(),
            Some("1.94.1")
        );
        assert_eq!(
            pinned_version("channel = '1.94.1' # pin\n").as_deref(),
            Some("1.94.1")
        );
        assert_eq!(pinned_version("channel = \"stable\"\n"), None);
        assert_eq!(pinned_version("channel = \"nightly-2026-01-01\"\n"), None);
        assert_eq!(pinned_version("[toolchain]\n"), None);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn rustc_version_is_the_second_word() {
        assert_eq!(
            parse_rustc_version("rustc 1.99.0 (abcdef012 2026-05-01)\n").as_deref(),
            Some("1.99.0")
        );
        assert_eq!(
            parse_rustc_version("rustc 1.95.0-beta.2 (x y)").as_deref(),
            Some("1.95.0")
        );
        assert_eq!(parse_rustc_version("error: no toolchain"), None);
    }

    #[cfg(unix)]
    mod unix {
        use super::*;
        use std::os::unix::fs::PermissionsExt;

        fn repo(channel: &str) -> tempfile::TempDir {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(
                dir.path().join("rust-toolchain.toml"),
                format!("[toolchain]\nchannel = \"{channel}\"\n"),
            )
            .unwrap();
            dir
        }

        fn bin_with_rustc(version: &str) -> tempfile::TempDir {
            let dir = tempfile::tempdir().unwrap();
            let rustc = dir.path().join("rustc");
            std::fs::write(
                &rustc,
                format!("#!/bin/sh\necho 'rustc {version} (0123abcde 2026-01-01)'\n"),
            )
            .unwrap();
            std::fs::set_permissions(&rustc, std::fs::Permissions::from_mode(0o755)).unwrap();
            dir
        }

        #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
        #[test]
        fn a_path_resolving_a_different_rustc_is_refused_with_both_versions_and_the_path() {
            let repo = repo("1.94.1");
            let bin = bin_with_rustc("1.99.0");
            let path = bin.path().display().to_string();
            let error = check(Some(&path), repo.path()).unwrap_err();
            assert!(error.starts_with("toolchain_mismatch:"), "{error}");
            assert!(error.contains("pins rustc 1.94.1"), "{error}");
            assert!(error.contains("rustc 1.99.0"), "{error}");
            assert!(error.contains(&path), "{error}");
        }

        #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
        #[test]
        fn the_pinned_rustc_is_admitted() {
            let repo = repo("1.94.1");
            let bin = bin_with_rustc("1.94.1");
            assert_eq!(
                check(Some(&bin.path().display().to_string()), repo.path()),
                Ok(())
            );
        }

        #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
        #[test]
        fn a_path_with_no_rustc_is_refused_and_an_unpinned_repo_is_not_checked() {
            let repo = repo("1.94.1");
            let empty = tempfile::tempdir().unwrap();
            let error = check(Some(&empty.path().display().to_string()), repo.path()).unwrap_err();
            assert!(error.starts_with("toolchain_mismatch:"), "{error}");
            assert!(error.contains("no rustc is on the daemon PATH"), "{error}");
            let named = self::repo("stable");
            assert_eq!(check(Some("/nonexistent"), named.path()), Ok(()));
            let none = tempfile::tempdir().unwrap();
            assert_eq!(check(Some("/nonexistent"), none.path()), Ok(()));
        }
    }
}
