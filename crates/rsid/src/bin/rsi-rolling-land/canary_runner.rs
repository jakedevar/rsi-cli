//! Lander side of the single canary runner (#951).
//!
//! A published landing whose tree was not gated before publication used to run
//! its post-push canary (a second full base-vs-candidate gate) inside the lander
//! process, which kept its `cargo-slot` lander slot for hours. The lander now
//! records the canary in the host-wide registry (`canary.rs`), makes sure one
//! detached runner is draining it, and exits. The runner gates the newest
//! published tip once for every queued landing it covers and falls back to the
//! per-landing canary, with the same forward revert, when that gate is red.

// Items are `pub(super)` for the binary root, like the sibling modules.
#![allow(clippy::redundant_pub_crate)]

use super::canary::{self, CanaryGate, CanaryRequest, GateVerdict, Registry};
use super::{
    AcceptedPair, Options, PublicationState, TARGET, TestGate, copy_committed_script, git_ok,
    git_text, parse_cargo_build_jobs, prepare_descendant_revert, publish_oid, remote_fetch,
    run_guard_pair, run_guard_pair_with_proof, select_gate_scratch, validate_cargo_target_dir,
};
use std::collections::BTreeSet;
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};

const TMPFS_MIN_FREE_GB: u64 = 12;

/// Whether a landing hands its post-push canary to the runner. Production
/// defaults to the handoff; the hermetic binary tests default to the inline
/// canary they were written for and opt in explicitly.
pub(super) fn handoff_enabled() -> bool {
    match std::env::var("RSI_LANDER_CANARY_MODE").as_deref() {
        Ok("inline") => false,
        Ok("handoff") => true,
        _ => !cfg!(test),
    }
}

/// Queue the canary for `published_tip` (merged onto `target`) and make sure
/// a runner drains the queue. Returns the registry root.
pub(super) fn hand_off(
    source_repo: &Path,
    remote_url: &str,
    options: &Options,
    target: &str,
    published_tip: &str,
) -> Result<PathBuf, String> {
    let root = canary::default_root()?;
    let registry = Registry::open(&root)?;
    let request = CanaryRequest::new(
        published_tip.to_string(),
        target.to_string(),
        options
            .accepted
            .iter()
            .map(|pair| (pair.base.clone(), pair.source.clone()))
            .collect(),
        options.test_filters.clone(),
        source_repo.to_path_buf(),
        remote_url.to_string(),
    )?;
    // Enqueue before probing the lock: a runner that is finishing re-reads
    // the queue after it releases the lock, so the request is never stranded.
    registry.enqueue(&request)?;
    let spawn = std::env::var("RSI_LANDER_CANARY_SPAWN").as_deref() != Ok("0");
    if spawn && !registry.runner_lock_held()? {
        spawn_runner(&root)?;
    }
    Ok(root)
}

fn systemd_run_available() -> bool {
    std::process::Command::new("systemd-run")
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Start the runner outside this process's service unit and cgroup, so it
/// outlives the lander (and the lead turn that may be running it).
fn spawn_runner(root: &Path) -> Result<(), String> {
    let exe = std::env::current_exe()
        .map_err(|error| format!("cannot locate the lander executable: {error}"))?;
    // The unit and the fallback process are handed `root/target` as their
    // CARGO_TARGET_DIR; make sure it exists (#1025).
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(root.join("target"))
        .map_err(|error| format!("cannot create canary runner target directory: {error}"))?;
    let wrapper = canary::default_wrapper();
    if systemd_run_available() {
        let unit = format!(
            "rsi-canary-runner-{}-{}",
            chrono::Utc::now().timestamp(),
            std::process::id()
        );
        let status = canary::runner_systemd_command(&exe, root, wrapper.as_deref(), &unit)
            .status()
            .map_err(|error| format!("cannot start canary runner unit: {error}"))?;
        return if status.success() {
            Ok(())
        } else {
            Err(format!("systemd-run for the canary runner exited {status}"))
        };
    }
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(root.join("runner.log"))
        .map_err(|error| format!("cannot open canary runner log: {error}"))?;
    let stderr = log
        .try_clone()
        .map_err(|error| format!("cannot open canary runner log: {error}"))?;
    let mut command = canary::runner_command(&exe, root, wrapper.as_deref());
    command
        .env("CARGO_TARGET_DIR", root.join("target"))
        .stdout(log)
        .stderr(stderr)
        .process_group(0);
    command
        .spawn()
        .map(drop)
        .map_err(|error| format!("cannot spawn canary runner: {error}"))
}

/// Entry point for `rsi-rolling-land --canary-runner`.
pub(super) fn run_main() -> i32 {
    match canary::default_root().and_then(|root| ensure_runner_target_dir(&root)) {
        // SAFETY: still single-threaded; the async runtime starts below.
        Ok(target) => unsafe { std::env::set_var("CARGO_TARGET_DIR", target) },
        Err(error) => {
            let line = format!("canary_runner error={error}");
            eprintln!("rsi-rolling-land {line}");
            append_runner_log(&line);
            return 1;
        }
    }
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("rsi-rolling-land canary runner: cannot start async runtime: {error}");
            return 1;
        }
    };
    match runtime.block_on(run()) {
        Ok(summary) => {
            for line in alert_lines(&summary) {
                append_runner_log(&line);
            }
            let line = format!(
                "canary_runner gates_run={} verified={:?} covered={:?} red={:?} reverted={:?} unverified={:?} base_red={:?} lock_busy={}",
                summary.gates_run,
                summary.verified,
                summary.covered,
                summary.red,
                summary.reverted,
                summary
                    .unverified
                    .iter()
                    .map(|(tip, _)| tip)
                    .collect::<Vec<_>>(),
                summary
                    .base_red
                    .iter()
                    .map(|(tip, _)| tip)
                    .collect::<Vec<_>>(),
                summary.lock_busy
            );
            println!("{line}");
            append_runner_log(&line);
            0
        }
        Err(error) => {
            let line = format!("canary_runner error={error}");
            eprintln!("rsi-rolling-land {line}");
            append_runner_log(&line);
            1
        }
    }
}

/// Entry point for `rsi-rolling-land --canary-status <published tip>`: print
/// the recorded verdict as JSON, or a `pending` / `unknown` state.
pub(super) fn status_main(tip: Option<String>) -> i32 {
    let Some(tip) = tip.filter(|tip| canary::is_40_lower_hex(tip)) else {
        eprintln!("usage: rsi-rolling-land --canary-status <40-hex published tip>");
        return 2;
    };
    let status = || -> Result<String, String> {
        let registry = Registry::open(&canary::default_root()?)?;
        if let Some(record) = registry.verdict(&tip)? {
            return serde_json::to_string(&record).map_err(|error| error.to_string());
        }
        let state = if registry
            .pending()?
            .iter()
            .any(|request| request.published_tip == tip)
        {
            "pending"
        } else {
            "unknown"
        };
        Ok(serde_json::json!({ "tip": tip, "state": state }).to_string())
    };
    match status() {
        Ok(line) => {
            println!("{line}");
            0
        }
        Err(error) => {
            eprintln!("rsi-rolling-land canary status: {error}");
            1
        }
    }
}

fn append_runner_log(line: &str) {
    let Ok(root) = canary::default_root() else {
        return;
    };
    if let Ok(mut log) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(root.join("runner.log"))
    {
        let _ = writeln!(log, "{} {line}", chrono::Utc::now().to_rfc3339());
    }
}

async fn run() -> Result<canary::RunSummary, String> {
    let root = canary::default_root()?;
    let registry = Registry::open(&root)?;
    let pending = registry.pending()?;
    let Some(first) = pending.first() else {
        return Ok(canary::RunSummary::default());
    };
    let mut gate = LanderCanaryGate::new(&root, &first.source_repo, &first.remote_url)?;
    canary::run_queue(&registry, &mut gate).await
}

/// Sort a failed gate into a verdict (#1025). Only a new test failure relative
/// to the base is `Red`, the one verdict that forward-reverts. A gate whose base
/// was already statically red on the same check could not judge the tip
/// (`BaseRed`). Anything else (a lander refusal before any shard, a missing
/// `CARGO_TARGET_DIR`, a guard timeout, a load gate) produced no test result
/// and is `Unverified`.
pub(super) fn classify_gate_error(error: String, base_static_red: bool) -> GateVerdict {
    if super::is_test_red(&error) {
        GateVerdict::Red(error)
    } else if base_static_red {
        GateVerdict::BaseRed(error)
    } else {
        GateVerdict::Unverified(error)
    }
}

/// A passing gate (no new test failure relative to the base) is `Green` only
/// when the base was clean. Tests that were already red on the base were
/// tolerated by the no-new-failures rule but they are not verified: report them
/// as `BaseRed` (never a revert) so the verdict and the alert name them (#1081).
pub(super) fn gate_pass_verdict(base_reds: &BTreeSet<String>) -> GateVerdict {
    if base_reds.is_empty() {
        GateVerdict::Green
    } else {
        GateVerdict::BaseRed(format!(
            "no new test failures, but the base was already red: {}",
            base_reds.iter().cloned().collect::<Vec<_>>().join(", ")
        ))
    }
}

/// The alert lines the runner logs for one run: one per parked landing that
/// could not be judged (`unverified`) or whose base was already red.
pub(super) fn alert_lines(summary: &canary::RunSummary) -> Vec<String> {
    let unverified = summary.unverified.iter().map(|(tip, error)| {
        format!(
            "ALERT canary unverified (gate could not run, nothing reverted) tip={tip} error={error}"
        )
    });
    let base_red = summary.base_red.iter().map(|(tip, error)| {
        format!(
            "ALERT canary base_red (base already red, nothing reverted) tip={tip} error={error}"
        )
    });
    unverified.chain(base_red).collect()
}

/// The runner's own `CARGO_TARGET_DIR` (explicit canary mode for the lander's
/// target-dir contract, #1025): an existing directory under the registry root,
/// created on demand, so the runner never depends on its launcher's
/// environment. A valid configured directory is kept.
pub(super) fn ensure_runner_target_dir(root: &Path) -> Result<PathBuf, String> {
    if let Ok(existing) = validate_cargo_target_dir() {
        return Ok(existing);
    }
    let target = root.join("target");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&target)
        .map_err(|error| format!("cannot create canary runner target directory: {error}"))?;
    Ok(target)
}

/// `CanaryGate` over a private clone, using the lander's own gate, forward
/// revert and fast-forward publication.
pub(super) struct LanderCanaryGate {
    workspace: tempfile::TempDir,
    repo: PathBuf,
    cargo_build_jobs: u8,
}

impl LanderCanaryGate {
    pub(super) fn new(root: &Path, source_repo: &Path, remote_url: &str) -> Result<Self, String> {
        let work = root.join("work");
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&work)
            .map_err(|error| format!("cannot create canary runner workspace: {error}"))?;
        let workspace = tempfile::Builder::new()
            .prefix("runner-")
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir_in(&work)
            .map_err(|error| format!("cannot create canary runner workspace: {error}"))?;
        let repo = workspace.path().join("repo");
        let repo_arg = repo.to_string_lossy().into_owned();
        // Share the source repository's objects when it still exists; a
        // purged sandbox falls back to a clone of the remote itself.
        if source_repo.join(".git").exists() {
            git_ok(
                source_repo,
                &[
                    "clone",
                    "--shared",
                    "--no-checkout",
                    "--quiet",
                    &source_repo.to_string_lossy(),
                    &repo_arg,
                ],
            )?;
        } else {
            git_ok(
                workspace.path(),
                &["clone", "--no-checkout", "--quiet", remote_url, &repo_arg],
            )?;
        }
        copy_committed_script(&repo, "scripts/rolling-landing-guard.py")?;
        copy_committed_script(&repo, "tools/check-released-migrations.py")?;
        for (key, value) in [
            ("maintenance.auto", "false"),
            ("maintenance.autoDetach", "false"),
            ("gc.auto", "0"),
            ("gc.autoDetach", "false"),
            ("fetch.writeCommitGraph", "false"),
        ] {
            git_ok(&repo, &["config", "--local", key, value])?;
        }
        git_ok(&repo, &["remote", "add", "publish", remote_url])?;
        git_ok(
            &repo,
            &["symbolic-ref", "HEAD", "refs/heads/rsi-private-unborn"],
        )?;
        let _ = git_ok(&repo, &["update-ref", "-d", TARGET]);
        let cargo_build_jobs =
            parse_cargo_build_jobs(std::env::var_os("CARGO_BUILD_JOBS").as_deref())?;
        Ok(Self {
            workspace,
            repo,
            cargo_build_jobs,
        })
    }

    fn options(&self, accepted: Vec<AcceptedPair>, test_filters: &[String]) -> Options {
        Options {
            repo: self.repo.clone(),
            remote: "publish".into(),
            accepted,
            test_filters: test_filters.to_vec(),
            cargo_build_jobs: self.cargo_build_jobs,
            remote_gate: None,
            tmpfs_min_free_gb: TMPFS_MIN_FREE_GB,
            disk_scratch_shards: BTreeSet::new(),
            expected_tip: None,
        }
    }

    fn tree(&self, commit: &str) -> Result<String, String> {
        git_text(&self.repo, &["rev-parse", &format!("{commit}^{{tree}}")])
    }
}

impl CanaryGate for LanderCanaryGate {
    fn is_ancestor(&self, older: &str, newer: &str) -> Result<bool, String> {
        super::git_is_ancestor(&self.repo, older, newer)
    }

    async fn refresh(&mut self) -> Result<(), String> {
        remote_fetch(&self.repo).await
    }

    async fn run_gate(
        &mut self,
        base: &str,
        tip: &str,
        test_filters: &[String],
    ) -> Result<GateVerdict, String> {
        // Nothing changed between the two trees, so there is nothing to test.
        if self.tree(base)? == self.tree(tip)? {
            return Ok(GateVerdict::Green);
        }
        let worktree = self
            .workspace
            .path()
            .join(format!("gate-{}", tip.get(..12).unwrap_or(tip)));
        let worktree_arg = worktree.to_string_lossy().into_owned();
        git_ok(
            &self.repo,
            &["worktree", "add", "--detach", "--quiet", &worktree_arg, tip],
        )?;
        // One synthetic accepted pair covers every landing in base..tip: the
        // guard reports the crates changed in that range and gates the tip
        // against base with the lander's no-new-failures rule.
        let pair = AcceptedPair {
            base: base.to_string(),
            source: tip.to_string(),
        };
        let options = self.options(vec![pair.clone()], test_filters);
        let mut base_static_red = false;
        let mut base_reds = BTreeSet::new();
        let result = async {
            let mut test_gate = TestGate::for_landing()?;
            test_gate.scratch = Some(select_gate_scratch(
                &validate_cargo_target_dir()?,
                options.tmpfs_min_free_gb,
            )?);
            let outcome = run_guard_pair_with_proof(
                &self.repo,
                &options,
                &pair,
                base,
                tip,
                Some(&worktree),
                false,
                None,
                &mut test_gate,
            )
            .await;
            base_static_red = !test_gate.base_static_inventory_reds.is_empty();
            base_reds = std::mem::take(&mut test_gate.base_reds);
            outcome
        }
        .await;
        let _ = git_ok(
            &self.repo,
            &["worktree", "remove", "--force", &worktree_arg],
        );
        Ok(match result {
            Ok(()) => gate_pass_verdict(&base_reds),
            Err(error) => classify_gate_error(error, base_static_red),
        })
    }

    async fn forward_revert(&mut self, request: &CanaryRequest) -> Result<String, String> {
        let accepted = request
            .accepted
            .iter()
            .map(|(base, source)| AcceptedPair {
                base: base.clone(),
                source: source.clone(),
            })
            .collect::<Vec<_>>();
        let options = self.options(accepted, &request.test_filters);
        let mut last_error = String::from("forward revert did not run");
        for _attempt in 0..3 {
            remote_fetch(&self.repo).await?;
            let observed = git_text(
                &self.repo,
                &["rev-parse", "--verify", "refs/heads/rolling^{commit}"],
            )?;
            if !super::git_is_ancestor(&self.repo, &request.published_tip, &observed)? {
                return Err(format!(
                    "landing {} is no longer an ancestor of rolling {observed}",
                    request.published_tip
                ));
            }
            let scratch = tempfile::Builder::new()
                .prefix("revert-")
                .permissions(std::fs::Permissions::from_mode(0o700))
                .tempdir_in(self.workspace.path())
                .map_err(|error| format!("cannot create forward-revert scratch: {error}"))?;
            let tree_path = scratch.path().join("tree");
            let tree_arg = tree_path.to_string_lossy().into_owned();
            git_ok(
                &self.repo,
                &[
                    "worktree", "add", "--detach", "--quiet", &tree_arg, &observed,
                ],
            )?;
            // The inverse of this landing's own delta (published tip back to
            // its target), applied to the current tip, as the inline canary's
            // descendant revert does.
            let attempt = async {
                let revert = prepare_descendant_revert(
                    &self.repo,
                    &tree_path,
                    scratch.path(),
                    &request.published_tip,
                    &request.target,
                    &observed,
                )?;
                for pair in &options.accepted {
                    run_guard_pair(
                        &self.repo,
                        &options,
                        pair,
                        &observed,
                        &revert,
                        Some(&tree_path),
                        true,
                    )
                    .await?;
                }
                Ok::<_, String>(revert)
            }
            .await;
            let _ = git_ok(&self.repo, &["worktree", "remove", "--force", &tree_arg]);
            let revert = attempt?;
            match publish_oid(&self.repo, &revert, &observed).await {
                Ok(tip) => return Ok(tip),
                Err(error) if error.state == PublicationState::NotPublished => {
                    last_error = error.message;
                }
                Err(error) => return Err(error.message),
            }
        }
        Err(last_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_RED: &str = "new test failures relative to rolling 0123: demo::red; base reds: ";

    /// #1025: only a new test failure is red; the lander's CARGO_TARGET_DIR
    /// refusal, a guard timeout and a load gate are unverified, and a
    /// statically red base is base_red. Neither of the last two may revert.
    #[test]
    fn only_a_new_test_failure_is_red() {
        assert_eq!(
            classify_gate_error(TEST_RED.into(), false),
            GateVerdict::Red(TEST_RED.into())
        );
        assert!(matches!(
            classify_gate_error(TEST_RED.into(), true),
            GateVerdict::Red(_)
        ));
        for error in [
            "CARGO_TARGET_DIR must point to the current session's existing sandbox target directory",
            "CARGO_TARGET_DIR is unavailable: No such file or directory",
            "guard timed out after 3600s",
            "affected-crate guard failed (TimedOut) running cargo",
            "landing guard script is missing: /x/scripts/rolling-landing-guard.py",
        ] {
            assert_eq!(
                classify_gate_error(error.into(), false),
                GateVerdict::Unverified(error.into()),
                "{error}"
            );
        }
        let static_red = "affected-crate guard failed (Failed) running scripts/run-rsid-test-shards.sh: custody.rs:4508";
        assert_eq!(
            classify_gate_error(static_red.into(), true),
            GateVerdict::BaseRed(static_red.into())
        );
    }

    #[test]
    fn runner_target_dir_is_created_under_the_registry_root() {
        let _guard = crate::tests::env_lock();
        let root = tempfile::TempDir::new().unwrap();
        let saved = std::env::var_os("CARGO_TARGET_DIR");
        // SAFETY: serialised through the lander tests' environment lock.
        unsafe { std::env::remove_var("CARGO_TARGET_DIR") };
        let target = ensure_runner_target_dir(root.path()).unwrap();
        assert_eq!(target, root.path().join("target"));
        assert!(target.is_dir());
        // SAFETY: as above.
        unsafe {
            if let Some(saved) = saved {
                std::env::set_var("CARGO_TARGET_DIR", saved);
            }
        }
    }
}
