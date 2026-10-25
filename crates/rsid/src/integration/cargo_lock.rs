//! Mechanical resolution of a merge that conflicts only in the root `Cargo.lock`.
//!
//! Queued sources that each add a workspace member rewrite neighbouring lines
//! of `Cargo.lock` and conflict there while `Cargo.toml` merges cleanly (#3717).
//! The lock is derived data, so the engine takes the target's side, lets Cargo
//! re-derive it offline for the merged workspace (`cargo update --workspace
//! --offline`: workspace members only, no version bumps) and accepts the result
//! only when every registry or git package in it already appears in one of the
//! two locks being merged. Any other outcome (Cargo missing, needing the
//! network, picking a version neither side had) leaves the conflict to be
//! refused as before.

use super::{IntegrationConfig, git};
use crate::process_control::{
    CaptureLimits, OverflowBehavior, ProcessContainment, capture_bounded,
};
use std::collections::BTreeSet;
use std::path::Path;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

const LOCK: &str = "Cargo.lock";
const MAX_CARGO_STDOUT_BYTES: usize = 256 * 1024;
const MAX_CARGO_STDERR_BYTES: usize = 64 * 1024;

/// A package a lock takes from outside the workspace: `(name, version, source)`.
type External = (String, String, String);

/// Whether the unmerged paths of a failed merge are exactly the root lock.
pub(super) fn is_lock_only(paths: &[String]) -> bool {
    matches!(paths, [only] if only == LOCK)
}

/// Resolve a merge left conflicted only in `Cargo.lock` and stage the result.
/// `false` means the conflict stands (the caller aborts the merge and refuses);
/// the worktree may then hold a partial resolution, which the abort discards.
pub(super) async fn resolve(config: &IntegrationConfig, worktree: &Path) -> bool {
    try_resolve(config, worktree).await.is_ok()
}

async fn try_resolve(config: &IntegrationConfig, worktree: &Path) -> Result<(), String> {
    let ours = stage(config, worktree, 2).await?;
    let theirs = stage(config, worktree, 3).await?;
    let ours_text = String::from_utf8(ours).map_err(|_| "target lock is not UTF-8")?;
    let theirs_text = String::from_utf8(theirs).map_err(|_| "source lock is not UTF-8")?;

    // The target's lock is the starting point: Cargo keeps every version it
    // pins and adds only what the merged manifests newly need.
    let checkout = git::run(config, worktree, &["checkout", "--ours", "--", LOCK])
        .await
        .map_err(|error| error.to_string())?;
    if !checkout.status.success() {
        return Err(format!(
            "cannot take the target lock: {}",
            checkout.stderr_tail()
        ));
    }
    run_cargo_update(config, worktree).await?;

    let merged = std::fs::read_to_string(worktree.join(LOCK))
        .map_err(|error| format!("cannot read the regenerated lock: {error}"))?;
    check_no_new_versions(&merged, &ours_text, &theirs_text)?;

    let add = git::run(config, worktree, &["add", "--", LOCK])
        .await
        .map_err(|error| error.to_string())?;
    if !add.status.success() {
        return Err(format!("cannot stage the lock: {}", add.stderr_tail()));
    }
    Ok(())
}

async fn stage(config: &IntegrationConfig, worktree: &Path, number: u8) -> Result<Vec<u8>, String> {
    let spec = format!(":{number}:{LOCK}");
    git::stdout_raw(config, worktree, &["show", &spec])
        .await
        .map_err(|error| error.to_string())
}

async fn run_cargo_update(config: &IntegrationConfig, worktree: &Path) -> Result<(), String> {
    let mut command = Command::new("cargo");
    // The candidate's sources are agent-written: give Cargo only the variables
    // the lander lets its gate children inherit.
    crate::lander_env_policy::apply_tokio(&mut command);
    command
        .args(["update", "--workspace", "--offline"])
        .current_dir(worktree)
        .env("CARGO_NET_OFFLINE", "true")
        .env("CARGO_TERM_COLOR", "never")
        .env("RUSTC_WRAPPER", "")
        .env("RUSTC_WORKSPACE_WRAPPER", "");
    let limits = CaptureLimits::new(
        MAX_CARGO_STDOUT_BYTES,
        MAX_CARGO_STDERR_BYTES,
        config.git_timeout,
        OverflowBehavior::TruncateAndDrain,
        ProcessContainment::Group,
    );
    let captured = capture_bounded(command, limits, &CancellationToken::new())
        .await
        .map_err(|error| format!("cargo update: {error}"))?;
    if captured.status.success() {
        return Ok(());
    }
    let tail = String::from_utf8_lossy(&captured.stderr);
    Err(format!("cargo update failed: {}", tail.trim()))
}

/// Registry and git packages of a lock (workspace and path packages carry no
/// `source`).
fn externals(text: &str) -> Result<BTreeSet<External>, String> {
    let value: toml::Value = text
        .parse()
        .map_err(|error| format!("unparseable lock: {error}"))?;
    let packages = value
        .get("package")
        .and_then(toml::Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    let mut found = BTreeSet::new();
    for package in packages {
        let field = |key: &str| package.get(key).and_then(toml::Value::as_str);
        if let (Some(name), Some(version), Some(source)) =
            (field("name"), field("version"), field("source"))
        {
            found.insert((name.to_string(), version.to_string(), source.to_string()));
        }
    }
    Ok(found)
}

/// Every external package of `merged` is in `ours` or `theirs`.
fn check_no_new_versions(merged: &str, ours: &str, theirs: &str) -> Result<(), String> {
    let mut known = externals(ours)?;
    known.extend(externals(theirs)?);
    let stray: Vec<String> = externals(merged)?
        .into_iter()
        .filter(|package| !known.contains(package))
        .map(|(name, version, _)| format!("{name} {version}"))
        .collect();
    if stray.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "regeneration chose package versions neither side locked: {}",
            stray.join(", ")
        ))
    }
}

#[cfg(test)]
mod tests {
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    fn lock(packages: &[(&str, &str, Option<&str>)]) -> String {
        let mut text = String::from("version = 4\n");
        for (name, version, source) in packages {
            text.push_str(&format!(
                "\n[[package]]\nname = \"{name}\"\nversion = \"{version}\"\n"
            ));
            if let Some(source) = source {
                text.push_str(&format!("source = \"{source}\"\n"));
            }
        }
        text
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    const REGISTRY: &str = "registry+https://github.com/rust-lang/crates.io-index";

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn only_the_root_lock_alone_is_lock_only() {
        assert!(super::is_lock_only(&["Cargo.lock".to_string()]));
        assert!(!super::is_lock_only(&[]));
        assert!(!super::is_lock_only(&["crates/x/Cargo.lock".to_string()]));
        assert!(!super::is_lock_only(&[
            "Cargo.lock".to_string(),
            "Cargo.toml".to_string()
        ]));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn new_workspace_members_are_not_version_changes() {
        let ours = lock(&[("a", "0.1.0", None), ("serde", "1.0.1", Some(REGISTRY))]);
        let theirs = lock(&[("c", "0.1.0", None), ("serde", "1.0.1", Some(REGISTRY))]);
        let merged = lock(&[
            ("a", "0.1.0", None),
            ("c", "0.1.0", None),
            ("serde", "1.0.1", Some(REGISTRY)),
        ]);
        assert_eq!(
            super::check_no_new_versions(&merged, &ours, &theirs),
            Ok(())
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_dependency_one_side_locked_is_accepted() {
        let ours = lock(&[("a", "0.1.0", None)]);
        let theirs = lock(&[("c", "0.1.0", None), ("tiny", "0.2.0", Some(REGISTRY))]);
        let merged = lock(&[
            ("a", "0.1.0", None),
            ("c", "0.1.0", None),
            ("tiny", "0.2.0", Some(REGISTRY)),
        ]);
        assert_eq!(
            super::check_no_new_versions(&merged, &ours, &theirs),
            Ok(())
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_version_neither_side_locked_is_refused() {
        let ours = lock(&[("serde", "1.0.1", Some(REGISTRY))]);
        let theirs = lock(&[("serde", "1.0.1", Some(REGISTRY))]);
        let merged = lock(&[("serde", "1.0.9", Some(REGISTRY))]);
        let error = super::check_no_new_versions(&merged, &ours, &theirs).unwrap_err();
        assert!(error.contains("serde 1.0.9"), "{error}");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn an_unparseable_lock_is_refused() {
        assert!(super::check_no_new_versions("not = [toml", "", "").is_err());
    }
}
