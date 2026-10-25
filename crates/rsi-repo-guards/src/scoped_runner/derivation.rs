//! The runner's calls into #426's derivation API, in process: no `check-touched-shards`
//! executable, no `RSI_TOUCHED_SHARDS_STAGED` opt-in (that gate guards only the
//! standalone CLI entry), no Python. Each function fails closed with a named
//! `Incomplete` and never turns an unreadable input into an empty selection.
//!
//! The derivation is pure computation over Git output, but a Git call can still
//! wedge, so `bounded` runs it on a worker thread and gives up at a hard deadline
//! or on cancellation; the run then fails closed and the process exits.
use super::contracts::{Derivation, Incomplete, SlowTier, gap};
use crate::touched_shards::{
    derive::{self, Failure, Loaded},
    downstream_tests, git as shard_git, modes, slow, source,
};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

/// Filters-only derivation is seconds of work; the lander gives the whole
/// derivation step far more than this, so a miss means a wedged Git call.
pub const DERIVE_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const POLL: Duration = Duration::from_millis(100);

pub(super) fn bounded<T: Send + 'static>(
    cancel: &Arc<AtomicUsize>,
    timeout: Duration,
    what: &str,
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, String> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(work());
    });
    let deadline = Instant::now() + timeout;
    loop {
        match receiver.recv_timeout(POLL) {
            Ok(value) => return Ok(value),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(format!("{what} stopped without a result"));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        let signal = cancel.load(Ordering::SeqCst);
        if signal != 0 {
            return Err(format!("{what} was cancelled by signal {signal}"));
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "{what} did not finish within {} s",
                timeout.as_secs()
            ));
        }
    }
}

fn owned(gap_of: Incomplete, detail: String) -> Incomplete {
    Incomplete {
        detail: Some(detail),
        ..gap_of
    }
}

/// `check-touched-shards --filters-only --json`, folded to exactly what the
/// queue's consumer reads: the exit code, the receipt JSON and the printed text.
pub(super) fn filters_only(
    repo: &Path,
    base: &str,
    head: &str,
    environment: Vec<(String, String)>,
    cancel: &Arc<AtomicUsize>,
) -> Result<Derivation, Incomplete> {
    let (repo, base, head): (PathBuf, String, String) =
        (repo.to_path_buf(), base.to_string(), head.to_string());
    let outcome = bounded(
        cancel,
        DERIVE_TIMEOUT,
        "the filters-only derivation",
        move || {
            let options = derive::Options {
                filters_only: true,
                no_compile: true,
                env: Some(environment),
                ..derive::Options::default()
            };
            Loaded::load(&repo, &base, &head).and_then(|loaded| derive::derive(&loaded, &options))
        },
    )
    .map_err(|detail| owned(gap("#426", "check-touched-shards derivation"), detail))?;
    let refused = |code: i32, message: String| Derivation {
        exit_code: code,
        data: json!({}),
        stdout: format!("check-touched-shards: {message}\n"),
    };
    Ok(match outcome {
        Ok(derived) => Derivation {
            exit_code: i32::from(derived.failed),
            stdout: format!("{}\n", modes::python_json(&derived.receipt, true)),
            data: derived.receipt,
        },
        Err(Failure::Refusal(message)) => refused(2, message),
        Err(Failure::Fatal(message)) => refused(1, message),
        Err(Failure::Interrupted {
            code,
            partial,
            message,
        }) => Derivation {
            exit_code: code,
            data: partial,
            stdout: format!("{message}\n"),
        },
    })
}

/// The slow-tier config at `head`; `None` when that revision has none (no tier).
/// An unreadable or malformed config is an error whose detail the runner reports
/// as `config_unreadable` and answers with the full tier (never a silent skip).
pub(super) fn slow_tier(repo: &Path, head: &str) -> Result<Option<SlowTier>, Incomplete> {
    let unreadable = |detail: String| owned(gap("#426", "slow-tier configuration"), detail);
    let repo = shard_git::Repository::new(repo).map_err(|e| unreadable(e.to_string()))?;
    let spec = format!("{head}:{}", slow::CONFIG);
    let present = repo
        .run(&["cat-file", "-e", &spec])
        .map_err(|e| unreadable(e.to_string()))?
        .status
        .success();
    if !present {
        return Ok(None);
    }
    let text = repo
        .blobs(head, &[slow::CONFIG.to_string()])
        .map_err(|e| unreadable(e.to_string()))?
        .remove(slow::CONFIG)
        .ok_or_else(|| unreadable("the slow-tier config is not a file".into()))?;
    let config = slow::parse_slow_tier(&text).map_err(unreadable)?;
    Ok(Some(SlowTier {
        package: config.package,
        tests: config.tests.into_iter().collect::<BTreeSet<_>>(),
    }))
}

/// Tests that need an opt-in feature to be compiled in, per package at `head`.
pub(super) fn feature_inventory(
    repo: &Path,
    head: &str,
    package: &str,
) -> Result<Vec<(String, Vec<String>)>, Incomplete> {
    let failed = |detail: String| owned(gap("#426", "required-feature test inventory"), detail);
    if package != "rsid" {
        return Ok(Vec::new());
    }
    let repo = shard_git::Repository::new(repo).map_err(|e| failed(e.to_string()))?;
    let path = "crates/rsid/src/recursive_dag/smoke_fixture.rs";
    let present = repo
        .run(&["cat-file", "-e", &format!("{head}:{path}")])
        .map_err(|e| failed(e.to_string()))?
        .status
        .success();
    if !present {
        return Ok(Vec::new());
    }
    let texts = repo
        .blobs(head, &[path.to_string()])
        .map_err(|e| failed(e.to_string()))?;
    source::SourceTree::new(texts, derive::host_os())
        .feature_test_inventory(package)
        .map_err(failed)
}

/// The public-item consumer and downstream compile plan used when no derivation
/// ran (explicit filters): the same derivation the queue admits with.
pub(super) fn downstream_plan(
    repo: &Path,
    base: &str,
    head: &str,
    cancel: &Arc<AtomicUsize>,
) -> Result<Value, Incomplete> {
    let (repo, base, head) = (repo.to_path_buf(), base.to_string(), head.to_string());
    bounded(
        cancel,
        DERIVE_TIMEOUT,
        "the downstream test plan",
        move || {
            let loaded = Loaded::load(&repo, &base, &head).map_err(|failure| match failure {
                Failure::Refusal(m) | Failure::Fatal(m) => m,
                Failure::Interrupted { message, .. } => message,
            })?;
            downstream_tests::downstream_test_plan(
                &loaded.repo,
                &loaded.ctx.merge_base,
                &loaded.ctx.head,
                &loaded.sources,
            )
        },
    )
    .and_then(|plan| plan)
    .map_err(|detail| owned(gap("#426", "downstream compile/test plan"), detail))
}

/// The residual (#366) lane for the diff: selection from the committed trees,
/// one red `incomplete_contract` entry per selected, still-unported suite.
pub(super) fn residual(
    repo: &Path,
    base: &str,
    head: &str,
    cancel: &Arc<AtomicUsize>,
) -> Result<Vec<Value>, Incomplete> {
    let (repo, base, head) = (repo.to_path_buf(), base.to_string(), head.to_string());
    bounded(
        cancel,
        DERIVE_TIMEOUT,
        "the residual suite selection",
        move || super::residual::lane(&repo, &base, &head).map_err(|e| e.to_string()),
    )
    .and_then(|lane| lane)
    .map_err(|detail| owned(gap("#366", "residual mandatory suites"), detail))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil;

    const RSID: &str = "[features]\ndefault = []\ntest-shard-mode = []\ntest-shard-store-02 = [\"test-shard-mode\"]\n";
    const STORE: &str = "[features]\ndefault = []\ntest-shard-mode = []\ntest-shard-store-01 = [\"test-shard-mode\"]\n";

    fn gate(shard: &str, name: &str) -> String {
        format!(
            "#[cfg(any(not(feature = \"test-shard-mode\"), feature = \"test-shard-{shard}\"))]\n#[test]\nfn {name}() {{}}\n"
        )
    }
    fn commit(dir: &Path, message: &str) -> String {
        testutil::git(dir, &["add", "-A"]);
        testutil::git(
            dir,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-qm",
                message,
            ],
        );
        testutil::git(dir, &["rev-parse", "HEAD"])
            .trim()
            .to_string()
    }
    /// (repo, base, head): the head changes one sharded test file.
    fn repo(with_manifests: bool) -> (tempfile::TempDir, String, String) {
        let dir = tempfile::tempdir().unwrap();
        testutil::git(dir.path(), &["init", "-q", "-b", "main"]);
        if with_manifests {
            testutil::write(dir.path(), "crates/rsid/Cargo.toml", RSID);
            testutil::write(dir.path(), "crates/rsid-store/Cargo.toml", STORE);
        }
        testutil::write(dir.path(), "crates/rsid/src/lib.rs", "mod foo;\n");
        testutil::write(
            dir.path(),
            "crates/rsid-store/src/lib.rs",
            &gate("store-01", "s"),
        );
        testutil::write(dir.path(), "crates/rsid/src/foo.rs", &gate("store-02", "a"));
        let base = commit(dir.path(), "base");
        testutil::write(
            dir.path(),
            "crates/rsid/src/foo.rs",
            &(gate("store-02", "a") + "pub fn changed() {}\n"),
        );
        let head = commit(dir.path(), "head");
        (dir, base, head)
    }
    fn idle() -> Arc<AtomicUsize> {
        Arc::new(AtomicUsize::new(0))
    }

    #[test]
    fn a_filters_only_derivation_runs_in_process_without_the_staged_opt_in() {
        let (dir, base, head) = repo(true);
        let derived = filters_only(dir.path(), &base, &head, vec![], &idle()).unwrap();
        assert_eq!(derived.exit_code, 0, "{}", derived.stdout);
        assert_eq!(derived.data["head"], head);
        assert_eq!(derived.data["filters_only"], true);
        assert!(derived.data["suggested_filters"].is_array());
        assert!(
            !derived.data["changed_modules"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        let printed: Value = serde_json::from_str(derived.stdout.trim()).unwrap();
        assert_eq!(printed, derived.data);
    }

    #[test]
    fn a_refused_derivation_is_a_red_with_its_message_and_no_selection() {
        let (dir, base, head) = repo(false);
        let derived = filters_only(dir.path(), &base, &head, vec![], &idle()).unwrap();
        assert_eq!(derived.exit_code, 2);
        assert_eq!(derived.data, json!({}));
        assert!(
            derived
                .stdout
                .contains("shard package inventory is unavailable"),
            "{}",
            derived.stdout
        );
    }

    #[test]
    fn the_slow_tier_is_absent_parsed_or_unreadable() {
        let (dir, _, _) = repo(true);
        testutil::write(dir.path(), "x.txt", "x\n");
        let none = commit(dir.path(), "none");
        assert!(slow_tier(dir.path(), &none).unwrap().is_none());
        testutil::write(
            dir.path(),
            ".config/slow-tier.toml",
            "package = \"rsid-store\"\ntrigger_paths = [\"a\"]\ntests = [\"m::one\", \"m::two\"]\n",
        );
        let good = commit(dir.path(), "good");
        let config = slow_tier(dir.path(), &good).unwrap().unwrap();
        assert_eq!(config.package, "rsid-store");
        assert_eq!(config.tests.len(), 2);
        testutil::write(dir.path(), ".config/slow-tier.toml", "package = 3\n");
        let bad = commit(dir.path(), "bad");
        let error = slow_tier(dir.path(), &bad).unwrap_err();
        assert!(error.detail.is_some_and(|d| d.contains("package name")));
        // The earlier revision is still readable.
        assert!(slow_tier(dir.path(), &good).unwrap().is_some());
    }

    #[test]
    fn the_feature_inventory_names_only_the_opt_in_fixture_tests() {
        let (dir, _, head) = repo(true);
        assert!(
            feature_inventory(dir.path(), &head, "rsid-store")
                .unwrap()
                .is_empty()
        );
        assert!(
            feature_inventory(dir.path(), &head, "rsid")
                .unwrap()
                .is_empty()
        );
        testutil::write(
            dir.path(),
            "crates/rsid/src/recursive_dag/smoke_fixture.rs",
            "#[test]\nfn smoke() {}\nfn helper() {}\n",
        );
        let with = commit(dir.path(), "fixture");
        let inventory = feature_inventory(dir.path(), &with, "rsid").unwrap();
        assert_eq!(inventory.len(), 1, "{inventory:?}");
        assert!(inventory[0].0.ends_with("::smoke"), "{inventory:?}");
        assert_eq!(inventory[0].1, vec!["dev-fixtures".to_string()]);
    }

    #[test]
    fn the_downstream_plan_comes_from_the_same_derivation() {
        let (dir, base, head) = repo(true);
        let plan = downstream_plan(dir.path(), &base, &head, &idle()).unwrap();
        assert!(plan["packages"].is_array(), "{plan}");
        assert!(plan["filters"].is_array(), "{plan}");
    }

    #[test]
    fn a_wedged_derivation_is_stopped_at_its_deadline_and_on_cancellation() {
        let stuck = || std::thread::sleep(Duration::from_secs(30));
        let error =
            bounded(&idle(), Duration::from_millis(250), "the derivation", stuck).unwrap_err();
        assert!(error.contains("did not finish within"), "{error}");
        let cancelled = Arc::new(AtomicUsize::new(15));
        let error =
            bounded(&cancelled, Duration::from_secs(20), "the derivation", stuck).unwrap_err();
        assert!(error.contains("cancelled by signal 15"), "{error}");
        assert_eq!(
            bounded(&idle(), Duration::from_secs(5), "x", || 7).unwrap(),
            7
        );
    }
}
