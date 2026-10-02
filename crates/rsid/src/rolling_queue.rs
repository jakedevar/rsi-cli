//! Daemon-owned rolling merge queue runner (#1007 slice S1).
//!
//! The runner claims up to N ready sources in FIFO order (the operator's batch
//! size), runs the P0 lander (`rsi-rolling-land`) once with every source as an
//! `--accepted` argument (the lander chain-merges them onto the tip, renumbers
//! provisional migrations in that order, gates the candidate once with the
//! union of the members' test filters and publishes it with one fast-forward)
//! and settles each entry exactly once after verifying its ancestry. A batch
//! that is not green (red gate, merge conflict, policy refusal) is bisected: the
//! first half of the suspect window is gated against the tip it would publish
//! onto and published when green, narrowed when red, so each failure is
//! attributed to exactly one source and the innocent sources still publish.
//!
//! Restart reconcile runs once at boot: a `gating` entry whose source is
//! already an ancestor of `origin/rolling` settles as published; any other is
//! returned to `queued` and re-driven. The lander is idempotent (an integrated
//! source is refused), so re-driving cannot publish twice.

use crate::config::RuntimeConfig;
use crate::error::{DaemonError, Result};
use crate::store::Store;
use chrono::Utc;
use rsi_common::rolling_queue::{
    QUEUE_BATCH_ANCESTRY_UNVERIFIED, QUEUE_BATCH_MERGE_CONFLICT, QUEUE_BATCH_POLICY_REFUSED,
    QUEUE_BISECT_NO_PROGRESS, QUEUE_MIGRATION_OUT_OF_ORDER, QUEUE_REGATE_EXHAUSTED,
    ROLLING_QUEUE_MAX_BATCH_SIZE, RollingQueueEntryState, RollingQueueEntryV1, RollingQueueOutcome,
};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::process::Command;
use uuid::Uuid;

/// Poll interval of the runner loop.
const TICK: Duration = Duration::from_secs(5);
/// Hard ceiling on one lander run (the gate has its own per-command guards).
const RUN_TIMEOUT: Duration = Duration::from_secs(6 * 60 * 60);
/// The out-of-band budget: a moved tip costs at most one regate of the head.
const MAX_REGATES_ENV: &str = "RSI_LANDER_MAX_REGATED_STALE_RETRIES";
const MAX_REGATES: usize = 1;
/// Queue runs never reuse a gate across a moved tip: every stale advance re-gates
/// the remade candidate, still bounded by the regate budget above.
const EXACT_GATE_ENV: &str = "RSI_LANDER_EXACT_GATE";
const OUTCOME_TAIL_BYTES: usize = 2000;
const MAX_FAILING_TESTS: usize = 32;

/// Files that many concurrent sources touch. Informational only: the queue
/// never waits on, claims or seals them (operator directive 2026-09-29).
const HOT_FILES: &[&str] = &[
    "crates/rsid/src/config.rs",
    "crates/rsi-common/src/rpc.rs",
    "crates/rsi-common/src/agent_control_schema.rs",
    "crates/rsi-common/src/bin/rsi-rpc.rs",
    "crates/rsi/src/settings_registry.rs",
    "crates/rsi/src/settings_keys.rs",
];
/// Each schema version is its own file (`vNNN.rs`) here; the head is the
/// highest one, so adding a migration never edits a shared file.
const MIGRATION_DIR: &str = "crates/rsid/src/store/migrations";
/// A source that declares a provisional migration is renumbered by the lander
/// at landing, so its number is assigned in queue order.
const PROVISIONAL_DIR: &str = "tools/provisional-migrations/";
/// Older revisions declared the head as a constant in this file.
const LEGACY_MIGRATION_FILE: &str = "crates/rsid/src/store/mod.rs";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SourceFacts {
    pub migration_version: Option<u32>,
    pub hot_files: Vec<String>,
}

fn git(repo: &Path, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Whether `source` names a commit in `repo`'s object store.
pub(crate) fn source_commit_exists(repo: &Path, source: &str) -> bool {
    git(repo, &["cat-file", "-e", &format!("{source}^{{commit}}")]).is_some()
}

/// The schema head at `revision`: the highest `migrations/vNNN.rs`, else the
/// legacy constant in `store/mod.rs`.
fn schema_version_at(repo: &Path, revision: &str) -> Option<u32> {
    let listing = git(
        repo,
        &[
            "ls-tree",
            "--name-only",
            revision,
            &format!("{MIGRATION_DIR}/"),
        ],
    )?;
    let from_files = listing
        .lines()
        .filter_map(|path| {
            let name = path.rsplit('/').next()?;
            name.strip_prefix('v')?.strip_suffix(".rs")?.parse().ok()
        })
        .max();
    from_files.or_else(|| {
        let text = git(
            repo,
            &["show", &format!("{revision}:{LEGACY_MIGRATION_FILE}")],
        )?;
        let marker = "pub const LATEST_SCHEMA_VERSION: i32 = ";
        text.split(marker)
            .nth(1)?
            .split(';')
            .next()?
            .trim()
            .parse()
            .ok()
    })
}

/// Migration version and hot files the source adds relative to rolling.
/// Best effort: any git failure leaves the facts empty.
pub(crate) fn derive_source_facts(repo: &Path, source: &str) -> SourceFacts {
    let base = ["origin/rolling", "rolling"].iter().find_map(|reference| {
        git(repo, &["merge-base", source, reference]).map(|out| out.trim().to_string())
    });
    let Some(base) = base.filter(|base| !base.is_empty()) else {
        return SourceFacts::default();
    };
    let version_at = |revision: &str| schema_version_at(repo, revision);
    let migration_version = match (version_at(source), version_at(&base)) {
        (Some(source_version), Some(base_version)) if source_version > base_version => {
            Some(source_version)
        }
        _ => None,
    };
    let changed = git(
        repo,
        &["diff", "--name-only", "--no-renames", &base, source],
    )
    .unwrap_or_default();
    let hot_files = changed
        .lines()
        .filter(|path| HOT_FILES.contains(path))
        .map(str::to_string)
        .collect();
    SourceFacts {
        migration_version,
        hot_files,
    }
}

/// After a fetch, the published tip when `source` is an ancestor of
/// `origin/rolling`. `None` when it is not (or git cannot say).
pub(crate) fn landed_tip(repo: &Path, source: &str) -> Option<String> {
    let _ = git(repo, &["fetch", "--quiet", "origin", "rolling"]);
    let tip = git(repo, &["rev-parse", "--verify", "origin/rolling^{commit}"])?
        .trim()
        .to_string();
    git(repo, &["merge-base", "--is-ancestor", source, &tip]).map(|_| tip)
}

/// The migration number the lander actually gave `source`, read from the
/// published history: the provisional commit the lander writes has the queue's
/// source as one parent, and the migration file it adds relative to its other
/// parent carries the final number. `None` when no such commit exists (a plain
/// merge or fast-forward keeps the source's own number) or git cannot say.
pub(crate) fn published_migration_number(repo: &Path, source: &str) -> Option<u32> {
    let history = git(
        repo,
        &[
            "log",
            "--format=%H %P",
            "--ancestry-path",
            &format!("{source}..origin/rolling"),
        ],
    )?;
    let (commit, other_parent) = history.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let commit = fields.next()?;
        let parents: Vec<&str> = fields.collect();
        (parents.len() == 2 && parents.contains(&source)).then(|| {
            let other = parents.iter().find(|parent| **parent != source)?;
            Some((commit.to_string(), (*other).to_string()))
        })?
    })?;
    let added = git(
        repo,
        &[
            "diff",
            "--name-only",
            "--no-renames",
            "--diff-filter=A",
            &other_parent,
            &commit,
            "--",
            &format!("{MIGRATION_DIR}/"),
        ],
    )?;
    added
        .lines()
        .filter_map(|path| {
            let name = path.rsplit('/').next()?;
            name.strip_prefix('v')?
                .strip_suffix(".rs")?
                .parse::<u32>()
                .ok()
        })
        .max()
}

/// Whether `source` adds a provisional-migration declaration relative to
/// rolling: the lander renumbers such a source mechanically at landing.
pub(crate) fn source_declares_provisional(repo: &Path, source: &str) -> bool {
    let base = ["origin/rolling", "rolling"].iter().find_map(|reference| {
        git(repo, &["merge-base", source, reference]).map(|out| out.trim().to_string())
    });
    let Some(base) = base.filter(|base| !base.is_empty()) else {
        return false;
    };
    git(
        repo,
        &["diff", "--name-only", "--no-renames", &base, source],
    )
    .unwrap_or_default()
    .lines()
    .any(|path| path.starts_with(PROVISIONAL_DIR) && path.ends_with(".json"))
}

/// One batch member's migration facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MigrationMember {
    pub id: Uuid,
    /// The schema head the source carries (`None`: no migration).
    pub derived: Option<u32>,
    /// Whether the source declares a provisional migration (renumberable).
    pub declared: bool,
}

/// Migration numbers the queue assigns a batch, in queue order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct MigrationPlan {
    pub assigned: Vec<(Uuid, u32)>,
    /// Members whose fixed number is already taken by a predecessor.
    pub out_of_order: Vec<Uuid>,
}

/// Assign migration numbers tip + 1, + 2, ... in queue order. A declared
/// source is renumbered to the next free number; an undeclared source keeps
/// its fixed number, which must not be one a predecessor already took (the
/// lander's tip + 1 rule then checks every prefix).
pub(crate) fn plan_migration_order(tip_version: u32, members: &[MigrationMember]) -> MigrationPlan {
    let mut plan = MigrationPlan::default();
    let mut next = tip_version + 1;
    for member in members {
        let Some(derived) = member.derived else {
            continue;
        };
        if member.declared {
            plan.assigned.push((member.id, next));
            next += 1;
        } else if derived < next {
            plan.out_of_order.push(member.id);
        } else {
            plan.assigned.push((member.id, derived));
            next = derived + 1;
        }
    }
    plan
}

/// How to launch the lander.
#[derive(Debug, Clone)]
pub struct LanderLauncher {
    binary: PathBuf,
}

impl LanderLauncher {
    #[must_use]
    pub fn new(binary: PathBuf) -> Self {
        Self { binary }
    }

    /// The lander executable this launcher runs.
    #[must_use]
    pub fn binary(&self) -> &Path {
        &self.binary
    }

    /// `RSI_ROLLING_LAND_BIN`, the daemon's sibling binary, the shared debug
    /// build, then `PATH`.
    #[must_use]
    pub fn discover() -> Self {
        if let Some(path) = std::env::var_os("RSI_ROLLING_LAND_BIN") {
            return Self::new(PathBuf::from(path));
        }
        let sibling = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|dir| dir.join("rsi-rolling-land")))
            .filter(|path| path.is_file());
        if let Some(path) = sibling {
            return Self::new(path);
        }
        let shared = std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join(".cargo/shared-target/debug/rsi-rolling-land"))
            .filter(|path| path.is_file());
        Self::new(shared.unwrap_or_else(|| PathBuf::from("rsi-rolling-land")))
    }
}

/// What a lander run resolved to, before settlement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunResult {
    pub state: RollingQueueEntryState,
    pub outcome: RollingQueueOutcome,
}

pub(crate) fn tail(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.len() <= OUTCOME_TAIL_BYTES {
        return trimmed.to_string();
    }
    let mut start = trimmed.len() - OUTCOME_TAIL_BYTES;
    while !trimmed.is_char_boundary(start) {
        start += 1;
    }
    trimmed[start..].to_string()
}

pub(crate) fn line_value<'a>(stdout: &'a str, key: &str) -> Option<&'a str> {
    stdout
        .lines()
        .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
}

/// The receipt key for one failing test: the lander prints one
/// `failing_test=<name>` stdout line per new failure it refuses on.
pub const FAILING_TEST_KEY: &str = "failing_test";

/// One machine-readable receipt line for a failing test.
#[must_use]
pub fn failing_test_line(name: &str) -> String {
    format!("{FAILING_TEST_KEY}={name}")
}

/// The failing tests a run reported: the lander's `failing_test=` receipt
/// lines first, else the raw libtest `test ... FAILED` lines.
#[must_use]
pub fn failing_tests(text: &str) -> Vec<String> {
    let marked: Vec<String> = text
        .lines()
        .filter_map(|line| line_value(line.trim(), FAILING_TEST_KEY))
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .collect();
    let mut names: Vec<String> = if marked.is_empty() {
        text.lines()
            .filter_map(|line| {
                let rest = line.trim().strip_prefix("test ")?;
                rest.strip_suffix(" ... FAILED").map(str::to_string)
            })
            .collect()
    } else {
        marked
    };
    names.sort();
    names.dedup();
    names.truncate(MAX_FAILING_TESTS);
    names
}

/// Map one lander exit (`exit`, stdout evidence lines, stderr message) to a
/// typed result. `landed` is the ancestry probe used when publication is
/// uncertain, so a push that succeeded before the lander could report it is
/// still recorded as published.
pub(crate) fn classify_run(
    exit: Option<i32>,
    stdout: &str,
    stderr: &str,
    landed: impl FnOnce() -> Option<String>,
) -> RunResult {
    let published = |sha: String| RunResult {
        state: RollingQueueEntryState::Published,
        outcome: RollingQueueOutcome {
            landed_sha: Some(sha),
            ..RollingQueueOutcome::default()
        },
    };
    let detail = Some(tail(stderr)).filter(|text| !text.is_empty());
    let refuse = |code: &str, state: RollingQueueEntryState| RunResult {
        state,
        outcome: RollingQueueOutcome {
            landed_sha: None,
            refusal: Some(code.to_string()),
            failing_tests: failing_tests(&format!("{stdout}\n{stderr}")),
            detail: detail.clone(),
        },
    };
    if exit == Some(0) {
        return match line_value(stdout, "published_target_id") {
            Some(tip) => published(tip.to_string()),
            None => refuse("lander_report_missing_tip", RollingQueueEntryState::Failed),
        };
    }
    let status = line_value(stdout, "publication_status");
    if matches!(status, Some("published" | "unknown")) {
        // The push may have landed before the report failed: trust ancestry.
        return match landed() {
            Some(tip) => published(tip),
            None => refuse("publication_unknown", RollingQueueEntryState::Failed),
        };
    }
    if stderr.contains("stale retries exhausted") {
        return refuse(QUEUE_REGATE_EXHAUSTED, RollingQueueEntryState::Refused);
    }
    if stderr.contains("Conflict {") {
        return refuse(QUEUE_BATCH_MERGE_CONFLICT, RollingQueueEntryState::Refused);
    }
    if exit == Some(8) {
        let fence = line_value(stdout, "policy_fence").unwrap_or("unknown");
        return refuse(
            &format!("policy_refused:{fence}"),
            RollingQueueEntryState::Refused,
        );
    }
    refuse("gate_failed", RollingQueueEntryState::Refused)
}

/// Union of the members' filters; one member with none makes the batch run
/// the lander's full affected gate.
pub(crate) fn union_filters(entries: &[RollingQueueEntryV1]) -> Vec<String> {
    if entries.iter().any(|entry| entry.test_filters.is_empty()) {
        return Vec::new();
    }
    let mut union: Vec<String> = Vec::new();
    for filter in entries.iter().flat_map(|entry| &entry.test_filters) {
        if !union.contains(filter) {
            union.push(filter.clone());
        }
    }
    union
}

async fn run_lander(
    launcher: &LanderLauncher,
    repo: &Path,
    entries: &[RollingQueueEntryV1],
    regates_left: usize,
    expected_tip: Option<&str>,
) -> std::result::Result<std::process::Output, String> {
    let mut command = Command::new(&launcher.binary);
    command
        .current_dir(repo)
        .arg("--repo")
        .arg(repo)
        .args(["--remote", "origin"]);
    for entry in entries {
        command.args(["--accepted", &entry.source_commit]);
    }
    for filter in union_filters(entries) {
        command.args(["--test-filter", &filter]);
    }
    if let Some(tip) = expected_tip {
        // The tip the queue probed just before this run: a different initial
        // fetch is an out-of-band advance the lander charges as a re-gate.
        command.args(["--expected-tip", tip]);
    }
    command
        .env(MAX_REGATES_ENV, regates_left.to_string())
        .env(EXACT_GATE_ENV, "1")
        .env_remove("RSI_SESSION_TOKEN")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = command
        .spawn()
        .map_err(|error| format!("cannot start the lander: {error}"))?;
    match tokio::time::timeout(RUN_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(error)) => Err(format!("lander wait failed: {error}")),
        Err(_) => Err("lander run timed out".to_string()),
    }
}

async fn with_store<T: Send + 'static>(
    store: &Arc<tokio::sync::Mutex<Store>>,
    op: impl FnOnce(&Store) -> Result<T> + Send + 'static,
) -> Result<T> {
    let store = Arc::clone(store);
    tokio::task::spawn_blocking(move || op(&store.blocking_lock()))
        .await
        .map_err(|error| DaemonError::Process(format!("rolling queue store join: {error}")))?
}

/// One lander run over `entries`, classified. `stdout` is kept for the
/// evidence lines (`fetched_target_id`, `migration_N_*`).
struct GroupRun {
    result: RunResult,
    stdout: String,
    /// Out-of-band re-gates the lander spent in this run (from its
    /// `stale_retry_N=..:regated` evidence lines).
    regates_used: usize,
    /// The exact commit this run pushed (the lander's `published_target_id`,
    /// or its forward revert): the only tip the queue may call its own. The
    /// observed remote head in `result.outcome.landed_sha` can be a later
    /// external descendant. `None` when the run reported no published commit.
    own_tip: Option<String>,
}

fn own_published_tip(stdout: &str) -> Option<String> {
    let reverted = line_value(stdout, "forward_revert_status") == Some("published");
    let revert = line_value(stdout, "forward_revert_id").filter(|_| reverted);
    revert
        .or_else(|| line_value(stdout, "published_target_id"))
        .map(|tip| tip.trim().to_string())
        .filter(|tip| !tip.is_empty())
}

fn regates_used(stdout: &str) -> usize {
    stdout
        .lines()
        .filter(|line| line.starts_with("stale_retry_") && line.ends_with(":regated"))
        .count()
}

async fn run_group(
    launcher: &LanderLauncher,
    repo: &Path,
    entries: &[RollingQueueEntryV1],
    regates_left: usize,
    expected_tip: Option<&str>,
) -> GroupRun {
    match run_lander(launcher, repo, entries, regates_left, expected_tip).await {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            // Only an uncertain publication needs the ancestry probe.
            let uncertain = output.status.code() != Some(0)
                && matches!(
                    line_value(&stdout, "publication_status"),
                    Some("published" | "unknown")
                );
            let landed = if uncertain {
                let source = entries[0].source_commit.clone();
                let probe_repo = repo.to_path_buf();
                tokio::task::spawn_blocking(move || landed_tip(&probe_repo, &source))
                    .await
                    .ok()
                    .flatten()
            } else {
                None
            };
            let result = classify_run(output.status.code(), &stdout, &stderr, || landed);
            let mut regates_used = regates_used(&stdout);
            if result.outcome.refusal.as_deref() == Some(QUEUE_REGATE_EXHAUSTED) {
                regates_used = regates_used.max(regates_left);
            }
            let own_tip = own_published_tip(&stdout);
            GroupRun {
                result,
                stdout,
                regates_used,
                own_tip,
            }
        }
        Err(message) => GroupRun {
            result: RunResult {
                state: RollingQueueEntryState::Failed,
                outcome: RollingQueueOutcome {
                    refusal: Some("lander_unavailable".into()),
                    detail: Some(message),
                    ..RollingQueueOutcome::default()
                },
            },
            stdout: String::new(),
            regates_used: 0,
            own_tip: None,
        },
    }
}

/// Batch members share one candidate, so a lander policy refusal is reported
/// as the batch code with the fence kept in the detail.
fn as_batch_result(mut result: RunResult) -> RunResult {
    if let Some(fence) = result
        .outcome
        .refusal
        .as_deref()
        .and_then(|code| code.strip_prefix("policy_refused:"))
    {
        let detail = result.outcome.detail.take().unwrap_or_default();
        result.outcome.detail = Some(tail(&format!("policy_fence={fence}\n{detail}")));
        result.outcome.refusal = Some(QUEUE_BATCH_POLICY_REFUSED.to_string());
    }
    result
}

/// Whether a non-green gate is a red gate: only that is attributable to one
/// member by bisecting. A merge conflict or a policy refusal is a property of
/// the batch as a whole and settles every member with its typed refusal.
fn is_bisectable(result: &RunResult) -> bool {
    result.state == RollingQueueEntryState::Refused
        && result.outcome.refusal.as_deref() == Some("gate_failed")
}

async fn settle(
    store: &Arc<tokio::sync::Mutex<Store>>,
    id: Uuid,
    result: &RunResult,
) -> Result<()> {
    let (state, outcome) = (result.state, result.outcome.clone());
    with_store(store, move |s| {
        s.settle_rolling_queue_entry(id, state, &outcome, Utc::now())
    })
    .await?;
    Ok(())
}

/// Ancestry of each source in the published tip: after one fetch, a source is
/// verified only when it is an ancestor of `origin/rolling`. Migration
/// evidence in the lander's report never substitutes for the probe: the
/// provisional commit the lander writes keeps the original source as a parent.
fn verify_ancestry(repo: &Path, sources: &[String]) -> Vec<bool> {
    let _ = git(repo, &["fetch", "--quiet", "origin", "rolling"]);
    sources
        .iter()
        .map(|source| {
            git(
                repo,
                &["merge-base", "--is-ancestor", source, "origin/rolling"],
            )
            .is_some()
        })
        .collect()
}

/// `(source, final_version)` per provisional migration the lander renumbered.
fn migration_evidence(stdout: &str) -> Vec<(String, u32)> {
    let mut found = Vec::new();
    for index in 0.. {
        let (Some(source), Some(version)) = (
            line_value(stdout, &format!("migration_{index}_source_id")),
            line_value(stdout, &format!("migration_{index}_final_version")),
        ) else {
            break;
        };
        let Ok(version) = version.trim().parse() else {
            break;
        };
        found.push((source.to_string(), version));
    }
    found
}

/// Refuse the members whose migration number is out of order, returning the
/// rest (still `gating`) in queue order. Best effort: without a readable tip
/// the lander's own tip + 1 rule decides.
async fn order_migrations(
    store: &Arc<tokio::sync::Mutex<Store>>,
    repo: &Path,
    members: Vec<RollingQueueEntryV1>,
    settled: &mut Vec<(Uuid, RunResult)>,
) -> Result<Vec<RollingQueueEntryV1>> {
    if members
        .iter()
        .all(|entry| entry.migration_version.is_none())
    {
        return Ok(members);
    }
    let probe_repo = repo.to_path_buf();
    let probe_members = members.clone();
    let plan = tokio::task::spawn_blocking(move || {
        let _ = git(&probe_repo, &["fetch", "--quiet", "origin", "rolling"]);
        let tip = schema_version_at(&probe_repo, "origin/rolling")?;
        let facts: Vec<MigrationMember> = probe_members
            .iter()
            .map(|entry| MigrationMember {
                id: entry.id,
                derived: entry.migration_version,
                declared: entry.migration_version.is_some()
                    && source_declares_provisional(&probe_repo, &entry.source_commit),
            })
            .collect();
        Some(plan_migration_order(tip, &facts))
    })
    .await
    .map_err(|error| DaemonError::Process(format!("migration order probe: {error}")))?;
    let Some(plan) = plan else {
        return Ok(members);
    };
    for (id, version) in plan.assigned {
        with_store(store, move |s| {
            s.record_rolling_queue_assigned_migration(id, version)
        })
        .await?;
    }
    let mut kept = Vec::new();
    for entry in members {
        if plan.out_of_order.contains(&entry.id) {
            let result = RunResult {
                state: RollingQueueEntryState::Refused,
                outcome: RollingQueueOutcome {
                    refusal: Some(QUEUE_MIGRATION_OUT_OF_ORDER.to_string()),
                    detail: Some(format!(
                        "migration {} is already taken by an earlier queued source; \
                         re-enqueue after renumbering (declare a provisional migration)",
                        entry.migration_version.unwrap_or_default()
                    )),
                    ..RollingQueueOutcome::default()
                },
            };
            settle(store, entry.id, &result).await?;
            settled.push((entry.id, result));
        } else {
            kept.push(entry);
        }
    }
    Ok(kept)
}

/// Claim the FIFO head, run it and settle it. `Ok(None)` when nothing was
/// runnable (empty queue or another entry is gating).
pub(crate) async fn run_next(
    store: &Arc<tokio::sync::Mutex<Store>>,
    launcher: &LanderLauncher,
) -> Result<Option<(Uuid, RunResult)>> {
    Ok(run_next_batch(store, launcher, 1).await?.into_iter().next())
}

/// Claim up to `batch_size` ready sources in FIFO order, gate them once as one
/// candidate, publish with one fast-forward and settle every member exactly
/// once. Empty when nothing was runnable.
pub(crate) async fn run_next_batch(
    store: &Arc<tokio::sync::Mutex<Store>>,
    launcher: &LanderLauncher,
    batch_size: usize,
) -> Result<Vec<(Uuid, RunResult)>> {
    let size = batch_size.clamp(1, ROLLING_QUEUE_MAX_BATCH_SIZE as usize);
    let Some(claimed) = with_store(store, move |s| {
        s.claim_rolling_queue_batch(Utc::now(), size)
    })
    .await?
    else {
        return Ok(Vec::new());
    };
    let repo = PathBuf::from(&claimed.repo_path);
    let batch_id = claimed.batch_id;
    let mut settled = Vec::new();
    let mut members = claimed.entries;
    if members.len() > 1 {
        members = order_migrations(store, &repo, members, &mut settled).await?;
    }
    if members.is_empty() {
        return Ok(settled);
    }
    let in_batch = members.len() > 1;
    let run = run_group(launcher, &repo, &members, MAX_REGATES, None).await;
    let regates_left = MAX_REGATES.saturating_sub(run.regates_used);
    let result = if in_batch {
        as_batch_result(run.result.clone())
    } else {
        run.result.clone()
    };
    if result.state == RollingQueueEntryState::Published {
        publish_group(
            store,
            &repo,
            batch_id,
            &members,
            &run,
            &result,
            &mut settled,
        )
        .await?;
        return Ok(settled);
    }
    if in_batch && is_bisectable(&result) {
        let reason = result.outcome.refusal.clone().unwrap_or_default();
        with_store(store, move |s| {
            s.begin_rolling_queue_bisect(batch_id, &reason, Utc::now())
        })
        .await?;
        let first_base = match line_value(&run.stdout, "fetched_target_id") {
            Some(tip) => Some(tip.to_string()),
            None => probe_tip(&repo).await,
        };
        bisect_batch(
            store,
            launcher,
            &repo,
            batch_id,
            members,
            result,
            first_base,
            regates_left,
            &mut settled,
        )
        .await?;
        return Ok(settled);
    }
    for entry in &members {
        settle(store, entry.id, &result).await?;
        settled.push((entry.id, result.clone()));
    }
    Ok(settled)
}

/// Settle a group whose lander run published: verify every source against the
/// published tip (a lander report is never trusted alone), record the final
/// migration numbers the lander assigned, and settle every member once with its
/// landed SHA or a typed `queue_batch_ancestry_unverified` failure.
async fn publish_group(
    store: &Arc<tokio::sync::Mutex<Store>>,
    repo: &Path,
    batch_id: Uuid,
    group: &[RollingQueueEntryV1],
    run: &GroupRun,
    result: &RunResult,
    settled: &mut Vec<(Uuid, RunResult)>,
) -> Result<()> {
    let tip = result.outcome.landed_sha.clone();
    let sources: Vec<String> = group.iter().map(|m| m.source_commit.clone()).collect();
    let probe_repo = repo.to_path_buf();
    let probe_sources = sources.clone();
    let verified =
        tokio::task::spawn_blocking(move || verify_ancestry(&probe_repo, &probe_sources))
            .await
            .map_err(|error| DaemonError::Process(format!("ancestry probe: {error}")))?;
    let base = line_value(&run.stdout, "fetched_target_id").map(str::to_string);
    let candidate = line_value(&run.stdout, "candidate_id").map(str::to_string);
    let summary = serde_json::json!({
        "sources": sources,
        "published": tip,
        "verified": verified,
    });
    with_store(store, move |s| {
        s.record_rolling_queue_batch_gate(batch_id, base.as_deref(), candidate.as_deref(), &summary)
    })
    .await?;
    if let Some(published) = tip.clone() {
        // The queue's own push: the next advance beyond it is external.
        bisect_event(
            store,
            batch_id,
            "expected_tip",
            None,
            serde_json::json!({
                "tip": published,
                "candidate": line_value(&run.stdout, "candidate_id"),
                "sources": group.iter().map(|m| m.source_commit.clone()).collect::<Vec<_>>(),
                "migrations": migration_evidence(&run.stdout)
                    .iter()
                    .map(|(source, version)| serde_json::json!({"source": source, "version": version}))
                    .collect::<Vec<_>>(),
            }),
        )
        .await?;
    }
    let versions = migration_evidence(&run.stdout);
    for (entry, ok) in group.iter().zip(verified) {
        let member_result = if ok {
            if let Some((_, version)) = versions
                .iter()
                .find(|(source, _)| *source == entry.source_commit)
            {
                let (id, version) = (entry.id, *version);
                // The actual number is durable before it is applied, so a
                // restart never replaces it with the pre-run plan.
                bisect_event(
                    store,
                    batch_id,
                    "migration_receipt",
                    Some(id),
                    serde_json::json!({ "version": version, "source": entry.source_commit }),
                )
                .await?;
                with_store(store, move |s| {
                    s.record_rolling_queue_assigned_migration(id, version)
                })
                .await?;
            }
            result.clone()
        } else {
            RunResult {
                state: RollingQueueEntryState::Failed,
                outcome: RollingQueueOutcome {
                    refusal: Some(QUEUE_BATCH_ANCESTRY_UNVERIFIED.to_string()),
                    detail: Some(format!(
                        "{} is not contained in the published tip {}",
                        entry.source_commit,
                        tip.clone().unwrap_or_default()
                    )),
                    ..RollingQueueOutcome::default()
                },
            }
        };
        settle(store, entry.id, &member_result).await?;
        settled.push((entry.id, member_result));
    }
    Ok(())
}

/// `origin/rolling` after one fetch (`None` when git cannot say).
fn fetch_tip(repo: &Path) -> Option<String> {
    let _ = git(repo, &["fetch", "--quiet", "origin", "rolling"]);
    git(repo, &["rev-parse", "--verify", "origin/rolling^{commit}"])
        .map(|tip| tip.trim().to_string())
        .filter(|tip| !tip.is_empty())
}

async fn probe_tip(repo: &Path) -> Option<String> {
    let repo = repo.to_path_buf();
    tokio::task::spawn_blocking(move || fetch_tip(&repo))
        .await
        .ok()
        .flatten()
}

/// Durably record, before `group` is run, the migration numbers the lander
/// will give its members (tip + 1, ... in queue order). A crash after the
/// lander pushed but before the report was recorded is then repaired by the
/// restart reconcile from this evidence.
async fn record_migration_plan(
    store: &Arc<tokio::sync::Mutex<Store>>,
    repo: &Path,
    batch_id: Uuid,
    group: &[RollingQueueEntryV1],
) -> Result<()> {
    if group.iter().all(|entry| entry.migration_version.is_none()) {
        return Ok(());
    }
    let probe_repo = repo.to_path_buf();
    let members = group.to_vec();
    let planned = tokio::task::spawn_blocking(move || {
        let _ = git(&probe_repo, &["fetch", "--quiet", "origin", "rolling"]);
        let tip = schema_version_at(&probe_repo, "origin/rolling")?;
        let facts: Vec<MigrationMember> = members
            .iter()
            .map(|entry| MigrationMember {
                id: entry.id,
                derived: entry.migration_version,
                declared: entry.migration_version.is_some()
                    && source_declares_provisional(&probe_repo, &entry.source_commit),
            })
            .collect();
        let plan = plan_migration_order(tip, &facts);
        Some(
            plan.assigned
                .iter()
                .filter_map(|(id, version)| {
                    let member = members.iter().find(|entry| entry.id == *id)?;
                    Some(serde_json::json!({
                        "id": id.to_string(),
                        "source": member.source_commit,
                        "version": version,
                    }))
                })
                .collect::<Vec<_>>(),
        )
    })
    .await
    .map_err(|error| DaemonError::Process(format!("migration plan probe: {error}")))?;
    let Some(assigned) = planned.filter(|list| !list.is_empty()) else {
        return Ok(());
    };
    bisect_event(
        store,
        batch_id,
        "migration_plan",
        None,
        serde_json::json!({ "assigned": assigned }),
    )
    .await
}

async fn bisect_event(
    store: &Arc<tokio::sync::Mutex<Store>>,
    batch_id: Uuid,
    kind: &'static str,
    entry: Option<Uuid>,
    detail: serde_json::Value,
) -> Result<()> {
    with_store(store, move |s| {
        s.record_rolling_queue_batch_event(batch_id, kind, entry, Some(detail), Utc::now())
    })
    .await
}

/// The tip a lander run gated against (its `fetched_target_id`), else the tip
/// probed just before it ran.
fn run_base(stdout: &str, probed: Option<&String>) -> Option<String> {
    line_value(stdout, "fetched_target_id")
        .map(str::to_string)
        .or_else(|| probed.cloned())
}

/// A red batch: attribute the failure to exactly one source per red, and
/// publish every innocent one.
///
/// `pending[..window]` is a prefix known to be red as a candidate on
/// `red_base` (`None`: unknown). Each step gates the first half of the
/// window against the tip it would publish onto: green publishes it (one
/// fast-forward) and the rest of the window stays red; red narrows the window
/// to that half. A window of one is the isolated bad source and settles with
/// the failing tests of the gate that condemned it. The tail after an
/// isolated source is gated whole against the new tip.
///
/// Red evidence is only as fresh as the base it was observed on: when the tip
/// the remaining window would publish onto is not that base (an external push
/// landed, or a prefix was gated on a different tip), the old red is stale and
/// the window is gated again instead of refused. `expected_tip` follows the
/// queue's own pushes; any other advance is external and spends the batch's one
/// out-of-band re-gate (`regates_left`, carried across every run). A second
/// external advance settles the rest with `queue_out_of_band_regate_exhausted`.
/// Every step shrinks the window or the pending list;
/// `queue_bisect_no_progress` settles the rest if that ever fails to hold.
async fn bisect_batch(
    store: &Arc<tokio::sync::Mutex<Store>>,
    launcher: &LanderLauncher,
    repo: &Path,
    batch_id: Uuid,
    members: Vec<RollingQueueEntryV1>,
    first_red: RunResult,
    first_base: Option<String>,
    mut regates_left: usize,
    settled: &mut Vec<(Uuid, RunResult)>,
) -> Result<()> {
    let mut pending = members;
    let mut window: Option<usize> = Some(pending.len());
    let mut evidence = first_red;
    let mut red_base = first_base.clone();
    let mut expected_tip = first_base;
    let mut budget = pending.len() * 4 + 4;
    while !pending.is_empty() {
        let no_progress = budget == 0 || window.is_some_and(|w| w == 0 || w > pending.len());
        if no_progress {
            let result = RunResult {
                state: RollingQueueEntryState::Refused,
                outcome: RollingQueueOutcome {
                    refusal: Some(QUEUE_BISECT_NO_PROGRESS.to_string()),
                    detail: Some(format!(
                        "bisect made no progress with {} source(s) pending",
                        pending.len()
                    )),
                    ..RollingQueueOutcome::default()
                },
            };
            bisect_event(
                store,
                batch_id,
                "bisect_no_progress",
                None,
                serde_json::json!({ "pending": pending.len() }),
            )
            .await?;
            for entry in pending.drain(..) {
                settle(store, entry.id, &result).await?;
                settled.push((entry.id, result.clone()));
            }
            break;
        }
        budget -= 1;
        let current = probe_tip(repo).await;
        if let Some(current) = current.as_ref() {
            if expected_tip.as_ref().is_some_and(|tip| tip != current) {
                // Not the queue's own push: an external advance.
                if regates_left == 0 {
                    let result = RunResult {
                        state: RollingQueueEntryState::Refused,
                        outcome: RollingQueueOutcome {
                            refusal: Some(QUEUE_REGATE_EXHAUSTED.to_string()),
                            detail: Some(format!(
                                "rolling advanced again to {current} outside the queue after the \
                                 batch's one re-gate"
                            )),
                            ..RollingQueueOutcome::default()
                        },
                    };
                    bisect_event(
                        store,
                        batch_id,
                        "regate_exhausted",
                        None,
                        serde_json::json!({ "expected": expected_tip, "observed": current }),
                    )
                    .await?;
                    for entry in pending.drain(..) {
                        settle(store, entry.id, &result).await?;
                        settled.push((entry.id, result.clone()));
                    }
                    break;
                }
                regates_left -= 1;
                bisect_event(
                    store,
                    batch_id,
                    "regate_spent",
                    None,
                    serde_json::json!({ "expected": expected_tip, "observed": current }),
                )
                .await?;
            }
            expected_tip = Some(current.clone());
            if window.is_some() && red_base.as_ref() != Some(current) {
                bisect_event(
                    store,
                    batch_id,
                    "bisect_red_stale",
                    None,
                    serde_json::json!({ "red_base": red_base, "tip": current }),
                )
                .await?;
                window = None;
                red_base = None;
            }
        }
        if window == Some(1) {
            let entry = pending.remove(0);
            bisect_event(
                store,
                batch_id,
                "bisect_isolated",
                Some(entry.id),
                serde_json::json!({
                    "refusal": evidence.outcome.refusal,
                    "failing_tests": evidence.outcome.failing_tests,
                }),
            )
            .await?;
            settle(store, entry.id, &evidence).await?;
            settled.push((entry.id, evidence.clone()));
            window = None;
            red_base = None;
            continue;
        }
        let take = window.map_or(pending.len(), |w| w.div_ceil(2));
        let group: Vec<RollingQueueEntryV1> = pending[..take].to_vec();
        record_migration_plan(store, repo, batch_id, &group).await?;
        let run = run_group(
            launcher,
            repo,
            &group,
            regates_left,
            expected_tip.as_deref(),
        )
        .await;
        regates_left = regates_left.saturating_sub(run.regates_used);
        let result = as_batch_result(run.result.clone());
        let base = run_base(&run.stdout, expected_tip.as_ref());
        let outcome = if result.state == RollingQueueEntryState::Published {
            "green"
        } else if is_bisectable(&result) {
            "red"
        } else {
            "unattributable"
        };
        bisect_event(
            store,
            batch_id,
            "bisect_gate",
            None,
            serde_json::json!({
                "sources": group.iter().map(|e| e.source_commit.clone()).collect::<Vec<_>>(),
                "window": window,
                "outcome": outcome,
                "base": base,
            }),
        )
        .await?;
        match outcome {
            "green" => {
                publish_group(store, repo, batch_id, &group, &run, &result, settled).await?;
                pending.drain(..take);
                // The red evidence carries over only when the prefix was gated
                // on the very base the red was observed on.
                // The queue's own push is the exact candidate the lander
                // reported; the observed remote head can be a later external
                // descendant that may have fixed (or changed) the red.
                let landed = result.outcome.landed_sha.clone();
                let exact = match (run.own_tip.as_ref(), landed.as_ref()) {
                    (Some(own), Some(landed)) => own == landed,
                    _ => false,
                };
                let carried = exact && window.is_some() && red_base.is_some() && red_base == base;
                if !exact {
                    bisect_event(
                        store,
                        batch_id,
                        "bisect_external_descendant",
                        None,
                        serde_json::json!({ "own": run.own_tip, "observed": landed }),
                    )
                    .await?;
                }
                window = if carried {
                    window.map(|w| w - take)
                } else {
                    None
                };
                red_base = if carried {
                    result.outcome.landed_sha.clone()
                } else {
                    None
                };
                // Anything past the exact candidate is external: expecting the
                // candidate makes the next probe charge the re-gate. Without a
                // reported candidate the head is adopted (nothing to charge).
                expected_tip = run.own_tip.clone().or(landed).or(expected_tip);
            }
            "red" => {
                evidence = result;
                window = Some(take);
                red_base = base.clone();
                expected_tip = base.or(expected_tip);
            }
            _ => {
                for entry in &group {
                    settle(store, entry.id, &result).await?;
                    settled.push((entry.id, result.clone()));
                }
                pending.drain(..take);
                if window.is_some() {
                    // The remaining window's red was observed with this group
                    // in it: do not trust it once the group is gone.
                    window = None;
                    red_base = None;
                }
                expected_tip = base.or(expected_tip);
            }
        }
    }
    Ok(())
}

/// One-shot restart reconcile: settle or re-queue every `gating` entry.
/// Returns `(settled, requeued)`.
pub(crate) async fn reconcile_gating(
    store: &Arc<tokio::sync::Mutex<Store>>,
) -> Result<(usize, usize)> {
    let gating = with_store(store, |s| {
        s.list_rolling_queue_entries(Some(RollingQueueEntryState::Gating), 64)
    })
    .await?;
    let (mut settled, mut requeued) = (0, 0);
    for (entry, repo_path) in gating {
        let source = entry.source_commit.clone();
        let repo = PathBuf::from(repo_path);
        let probe_repo = repo.clone();
        let tip = tokio::task::spawn_blocking(move || landed_tip(&repo, &source))
            .await
            .map_err(|error| DaemonError::Process(format!("reconcile probe: {error}")))?;
        let id = entry.id;
        match tip {
            Some(tip) => {
                // The run published before its report was settled. The actual
                // number wins, in order: the lander receipt the queue saved,
                // the number in the published provisional commit, and only
                // then the number the queue planned before the run.
                let receipt =
                    with_store(store, move |s| s.rolling_queue_migration_receipt(id)).await?;
                let recovered = match receipt {
                    Some(version) => Some(version),
                    None => {
                        let (repo, source) = (probe_repo.clone(), entry.source_commit.clone());
                        tokio::task::spawn_blocking(move || {
                            published_migration_number(&repo, &source)
                        })
                        .await
                        .map_err(|error| {
                            DaemonError::Process(format!("reconcile migration probe: {error}"))
                        })?
                    }
                };
                let planned = match recovered {
                    Some(version) => Some(version),
                    None => {
                        with_store(store, move |s| s.planned_rolling_queue_migration(id)).await?
                    }
                };
                if let Some(version) = planned {
                    with_store(store, move |s| {
                        s.record_rolling_queue_assigned_migration(id, version)
                    })
                    .await?;
                }
                let outcome = RollingQueueOutcome {
                    landed_sha: Some(tip),
                    detail: Some("settled by restart reconcile after ancestry check".into()),
                    ..RollingQueueOutcome::default()
                };
                let woke = with_store(store, move |s| {
                    s.settle_rolling_queue_entry(
                        id,
                        RollingQueueEntryState::Published,
                        &outcome,
                        Utc::now(),
                    )
                })
                .await?;
                settled += usize::from(woke.is_some());
            }
            None => {
                let done = with_store(store, move |s| {
                    s.requeue_gating_rolling_queue_entry(id, Utc::now())
                })
                .await?;
                requeued += usize::from(done);
            }
        }
    }
    Ok((settled, requeued))
}

/// The daemon task: reconcile once, then run the FIFO head whenever the
/// operator has the queue enabled.
pub async fn run_rolling_queue_loop(
    store: Arc<tokio::sync::Mutex<Store>>,
    config: Arc<RuntimeConfig>,
    launcher: LanderLauncher,
) {
    match reconcile_gating(&store).await {
        Ok((settled, requeued)) if settled + requeued > 0 => {
            tracing::info!(settled, requeued, "rolling queue restart reconcile");
        }
        Ok(_) => {}
        Err(error) => tracing::warn!(%error, "rolling queue restart reconcile deferred"),
    }
    let mut interval = tokio::time::interval(TICK);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        if !config.rolling_queue_enabled.load(Ordering::Relaxed) {
            continue;
        }
        let size = (config.rolling_queue_batch_size.load(Ordering::Relaxed) as usize)
            .clamp(1, ROLLING_QUEUE_MAX_BATCH_SIZE as usize);
        match run_next_batch(&store, &launcher, size).await {
            Ok(results) => {
                for (id, result) in results {
                    tracing::info!(%id, state = result.state.as_str(), "rolling queue entry settled");
                }
            }
            Err(error) => tracing::warn!(%error, "rolling queue run deferred"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::rolling_queue::RollingQueueBinding;
    use std::os::unix::fs::PermissionsExt;

    fn ok_run(stdout: &str) -> RunResult {
        classify_run(Some(0), stdout, "", || None)
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_green_lander_run_records_the_published_tip() {
        let result = ok_run("publication_status=published\npublished_target_id=abc123\n");
        assert_eq!(result.state, RollingQueueEntryState::Published);
        assert_eq!(result.outcome.landed_sha.as_deref(), Some("abc123"));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn policy_refusal_and_gate_failure_are_typed_with_failing_tests() {
        let policy = classify_run(
            Some(8),
            "policy_fence=migration_number\n",
            "rsi-rolling-land: migration must be tip + 1",
            || None,
        );
        assert_eq!(policy.state, RollingQueueEntryState::Refused);
        assert_eq!(
            policy.outcome.refusal.as_deref(),
            Some("policy_refused:migration_number")
        );

        let gate = classify_run(
            Some(1),
            "publication_status=not_published\n",
            "test rolling_queue::red ... FAILED\ntest rolling_queue::green ... ok\n",
            || None,
        );
        assert_eq!(gate.outcome.refusal.as_deref(), Some("gate_failed"));
        assert_eq!(gate.outcome.failing_tests, vec!["rolling_queue::red"]);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_second_out_of_band_regate_is_refused_not_looped() {
        let result = classify_run(
            Some(1),
            "publication_status=not_published\nobserved_target_id=def\n",
            "rsi-rolling-land: stale retries exhausted: def overlaps the candidate after 1 re-gated retries (1 total)",
            || None,
        );
        assert_eq!(result.state, RollingQueueEntryState::Refused);
        assert_eq!(
            result.outcome.refusal.as_deref(),
            Some(QUEUE_REGATE_EXHAUSTED)
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn an_unknown_publication_is_settled_from_ancestry() {
        let landed = classify_run(Some(3), "publication_status=unknown\n", "", || {
            Some("f00d".into())
        });
        assert_eq!(landed.state, RollingQueueEntryState::Published);
        assert_eq!(landed.outcome.landed_sha.as_deref(), Some("f00d"));
        let unknown = classify_run(Some(3), "publication_status=unknown\n", "", || None);
        assert_eq!(unknown.state, RollingQueueEntryState::Failed);
        assert_eq!(
            unknown.outcome.refusal.as_deref(),
            Some("publication_unknown")
        );
    }

    fn sh(dir: &Path, script: &str) {
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .current_dir(dir)
            .status()
            .unwrap();
        assert!(status.success(), "{script}");
    }

    /// origin (bare) + a work clone with one commit on `rolling`.
    fn repos() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let origin = dir.path().join("origin.git");
        let work = dir.path().join("work");
        sh(dir.path(), "git init -q --bare -b rolling origin.git");
        sh(dir.path(), "git clone -q origin.git work 2>/dev/null");
        sh(
            &work,
            "git config user.email t@t && git config user.name t && git checkout -q -b rolling \
             && mkdir -p crates/rsid/src/store \
             && printf 'pub const LATEST_SCHEMA_VERSION: i32 = 900;\\n' > crates/rsid/src/store/mod.rs \
             && git add -A && git commit -qm base && git push -q origin rolling",
        );
        (dir, origin, work)
    }

    fn head(repo: &Path) -> String {
        git(repo, &["rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_string()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn source_facts_report_the_migration_and_hot_files_a_source_adds() {
        let (_dir, _origin, work) = repos();
        sh(
            &work,
            "git checkout -q -b feature \
             && mkdir -p crates/rsid/src/store/migrations \
             && printf 'impl Store {}\\n' > crates/rsid/src/store/migrations/v901.rs \
             && printf '// hot\\n' > crates/rsid/src/config.rs \
             && printf '// router\\n' > crates/rsid/src/rpc.rs \
             && git add -A && git commit -qm feature",
        );
        let source = head(&work);
        assert!(source_commit_exists(&work, &source));
        assert!(!source_commit_exists(&work, &"1".repeat(40)));
        let facts = derive_source_facts(&work, &source);
        assert_eq!(facts.migration_version, Some(901));
        assert_eq!(
            facts.hot_files,
            vec!["crates/rsid/src/config.rs".to_string()]
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn landed_tip_reports_ancestry_of_origin_rolling() {
        let (_dir, _origin, work) = repos();
        let base = head(&work);
        assert_eq!(landed_tip(&work, &base), Some(base.clone()));
        sh(
            &work,
            "git checkout -q -b feature && echo x > x && git add x && git commit -qm x",
        );
        assert_eq!(landed_tip(&work, &head(&work)), None);
    }

    fn fake_lander(dir: &Path, body: &str) -> LanderLauncher {
        let path = dir.join("fake-lander.sh");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        LanderLauncher::new(path)
    }

    async fn queued(
        store: &Arc<tokio::sync::Mutex<Store>>,
        repo: &Path,
        commit: &str,
        key: &str,
    ) -> Uuid {
        let session = Uuid::new_v4();
        let new = crate::store::rolling_queue::NewQueueEntry {
            project_id: None,
            repo_path: repo.display().to_string(),
            source_commit: commit.into(),
            source_session_id: session,
            owner_epic_id: None,
            binding: RollingQueueBinding::Unbound,
            work_key: None,
            migration_version: None,
            hot_files: vec![],
            test_filters: vec!["rsid=rolling_queue".into()],
            idempotency_key: key.into(),
        };
        store
            .lock()
            .await
            .enqueue_rolling_queue_source(&new, Utc::now())
            .unwrap()
            .0
            .id
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn run_next_settles_a_green_run_and_wakes_the_owner_once() {
        let (dir, _origin, work) = repos();
        let sha = commit_file(&work, "a", "a", "a");
        let log = dir.path().join("lander.log");
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let launcher = merging_lander(dir.path(), &work, &log, "");
        let id = queued(&store, &work, &sha, "k").await;
        let (settled, result) = run_next(&store, &launcher).await.unwrap().unwrap();
        assert_eq!(settled, id);
        assert_eq!(result.state, RollingQueueEntryState::Published);
        assert!(run_next(&store, &launcher).await.unwrap().is_none());
        let landed = entry_of(&store, id).await.outcome.unwrap().landed_sha;
        assert_eq!(landed.as_deref(), Some(head(&work).as_str()));
        assert!(is_ancestor_of_origin(&work, &sha));
        assert_eq!(wake_count(&store).await, 1);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn run_next_passes_the_source_filters_and_the_one_regate_budget() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("args.log");
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let launcher = fake_lander(
            dir.path(),
            &format!(
                "echo \"$@\" > {log}; echo \"$RSI_LANDER_MAX_REGATED_STALE_RETRIES\" >> {log}; \
                 echo \"exact=$RSI_LANDER_EXACT_GATE\" >> {log}; \
                 echo 'test rolling_queue::red ... FAILED'; exit 1",
                log = log.display()
            ),
        );
        let commit = "b".repeat(40);
        let id = queued(&store, dir.path(), &commit, "k").await;
        let (_, result) = run_next(&store, &launcher).await.unwrap().unwrap();
        assert_eq!(result.state, RollingQueueEntryState::Refused);
        assert_eq!(result.outcome.failing_tests, vec!["rolling_queue::red"]);
        let logged = std::fs::read_to_string(&log).unwrap();
        assert!(logged.contains(&format!("--accepted {commit}")), "{logged}");
        assert!(
            logged.contains("--test-filter rsid=rolling_queue"),
            "{logged}"
        );
        assert!(
            logged.lines().any(|line| line == "1") && logged.trim_end().ends_with("exact=1"),
            "the queue runs the lander with one re-gate and the exact gate: {logged}"
        );
        let entry = store
            .lock()
            .await
            .get_rolling_queue_entry(id)
            .unwrap()
            .unwrap();
        assert_eq!(entry.state, RollingQueueEntryState::Refused);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn an_unavailable_lander_fails_the_entry_and_wakes_the_owner() {
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let launcher = LanderLauncher::new(PathBuf::from("/nonexistent/rsi-rolling-land"));
        let dir = tempfile::tempdir().unwrap();
        queued(&store, dir.path(), &"c".repeat(40), "k").await;
        let (_, result) = run_next(&store, &launcher).await.unwrap().unwrap();
        assert_eq!(result.state, RollingQueueEntryState::Failed);
        assert_eq!(
            result.outcome.refusal.as_deref(),
            Some("lander_unavailable")
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test(flavor = "multi_thread")]
    async fn restart_reconcile_settles_a_landed_entry_once_and_requeues_the_rest() {
        let (_dir, _origin, work) = repos();
        let landed_commit = head(&work);
        sh(
            &work,
            "git checkout -q -b feature && echo x > x && git add x && git commit -qm x",
        );
        let pending_commit = head(&work);
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let landed = queued(&store, &work, &landed_commit, "1").await;
        let pending = queued(&store, &work, &pending_commit, "2").await;
        {
            let guard = store.lock().await;
            // Both were mid-run when the daemon stopped.
            for id in [landed, pending] {
                let batch = Uuid::new_v4().to_string();
                guard
                    .conn
                    .execute(
                        "INSERT INTO rolling_queue_batches(id,state,member_ids_json,member_count,created_at,row_version) \
                         VALUES (?1,'gating','[]',1,'2026-09-29T00:00:00.000000000Z',1)",
                        [&batch],
                    )
                    .unwrap();
                guard
                    .conn
                    .execute(
                        "UPDATE rolling_queue_entries SET state='gating', batch_id=?2 WHERE id=?1",
                        [id.to_string(), batch],
                    )
                    .unwrap();
            }
        }
        assert_eq!(reconcile_gating(&store).await.unwrap(), (1, 1));
        // A second boot finds nothing left to settle: the wake is not repeated.
        assert_eq!(reconcile_gating(&store).await.unwrap(), (0, 0));
        let guard = store.lock().await;
        assert_eq!(
            guard
                .get_rolling_queue_entry(landed)
                .unwrap()
                .unwrap()
                .state,
            RollingQueueEntryState::Published
        );
        assert_eq!(
            guard
                .get_rolling_queue_entry(pending)
                .unwrap()
                .unwrap()
                .state,
            RollingQueueEntryState::Queued
        );
        let wakes = guard
            .list_scheduled_jobs()
            .unwrap()
            .into_iter()
            .filter(|job| job.name.starts_with("merge-queue-"))
            .count();
        assert_eq!(wakes, 1);
    }

    fn member(id: u128, derived: Option<u32>, declared: bool) -> MigrationMember {
        MigrationMember {
            id: Uuid::from_u128(id),
            derived,
            declared,
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn declared_migrations_are_numbered_tip_plus_one_in_queue_order() {
        let plan = plan_migration_order(
            148,
            &[
                member(1, Some(149), true),
                member(2, None, false),
                member(3, Some(149), true),
            ],
        );
        assert_eq!(
            plan.assigned,
            vec![(Uuid::from_u128(1), 149), (Uuid::from_u128(3), 150)]
        );
        assert!(plan.out_of_order.is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_fixed_migration_number_taken_by_a_predecessor_is_out_of_order() {
        let plan = plan_migration_order(
            148,
            &[member(1, Some(149), false), member(2, Some(149), false)],
        );
        assert_eq!(plan.assigned, vec![(Uuid::from_u128(1), 149)]);
        assert_eq!(plan.out_of_order, vec![Uuid::from_u128(2)]);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_batch_gates_the_union_of_filters_and_any_unfiltered_member_widens_it() {
        let mut a = queued_entry_view("a", &["rsid=x", "rsi=y"]);
        let b = queued_entry_view("b", &["rsid=x", "rsi-common=z"]);
        assert_eq!(
            union_filters(&[a.clone(), b.clone()]),
            vec!["rsid=x", "rsi=y", "rsi-common=z"]
        );
        a.test_filters.clear();
        assert!(union_filters(&[a, b]).is_empty());
    }

    fn queued_entry_view(commit: &str, filters: &[&str]) -> RollingQueueEntryV1 {
        RollingQueueEntryV1 {
            sequence: 1,
            id: Uuid::new_v4(),
            source_commit: commit.repeat(40),
            source_session_id: Uuid::new_v4(),
            owner_epic_id: None,
            binding: RollingQueueBinding::Unbound,
            work_key: None,
            migration_version: None,
            hot_files: vec![],
            test_filters: filters.iter().map(|f| (*f).to_string()).collect(),
            state: RollingQueueEntryState::Queued,
            outcome: None,
            enqueued_at: String::new(),
            finished_at: None,
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_merge_conflict_is_typed_and_not_a_red_gate() {
        let result = classify_run(
            Some(1),
            "publication_status=not_published\n",
            "rsi-rolling-land: integration refused: Conflict { paths: [\"f\"] }",
            || None,
        );
        assert_eq!(result.state, RollingQueueEntryState::Refused);
        assert_eq!(
            result.outcome.refusal.as_deref(),
            Some(QUEUE_BATCH_MERGE_CONFLICT)
        );
        let policy = as_batch_result(classify_run(
            Some(8),
            "policy_fence=migration_number\n",
            "refused",
            || None,
        ));
        assert_eq!(
            policy.outcome.refusal.as_deref(),
            Some(QUEUE_BATCH_POLICY_REFUSED)
        );
        assert!(
            policy
                .outcome
                .detail
                .unwrap()
                .contains("policy_fence=migration_number")
        );
    }

    /// A fake lander that really merges its `--accepted` sources onto
    /// `origin/rolling` in `work` and pushes once, like the real one on green.
    /// `guard` is shell run first (it sees `"$@"`); `@SKIP@` names a source the
    /// lander reports landed without merging.
    fn merging_lander(dir: &Path, work: &Path, log: &Path, guard: &str) -> LanderLauncher {
        let body = r#"
echo "run $*" >> @LOG@
echo "regates $RSI_LANDER_MAX_REGATED_STALE_RETRIES" >> @LOG@
@GUARD@
cd @WORK@ || exit 9
git fetch -q origin rolling
git checkout -q -B land origin/rolling
fetched=$(git rev-parse HEAD)
while [ $# -gt 0 ]; do
  if [ "$1" = --accepted ]; then
    if [ "$2" != "$SKIP" ]; then
      git merge -q --no-edit "$2" >/dev/null 2>&1 || { git merge --abort; echo 'integration refused: Conflict { paths: ["f"] }' >&2; echo publication_status=not_published; exit 1; }
    fi
    shift
  fi
  shift
done
git push -q origin HEAD:rolling && echo push >> @LOG@
echo publication_status=published
echo fetched_target_id=$fetched
echo candidate_id=$(git rev-parse HEAD)
echo published_target_id=$(git rev-parse HEAD)
"#
        .replace("@LOG@", &log.display().to_string())
        .replace("@WORK@", &work.display().to_string())
        .replace("@GUARD@", guard);
        fake_lander(dir, &body)
    }

    fn commit_file(work: &Path, branch: &str, file: &str, content: &str) -> String {
        sh(
            work,
            &format!(
                "git checkout -q -B {branch} origin/rolling && printf '{content}\\n' > {file} \
                 && git add {file} && git commit -qm {branch}"
            ),
        );
        head(work)
    }

    async fn queue_all(
        store: &Arc<tokio::sync::Mutex<Store>>,
        work: &Path,
        shas: &[String],
        migration: Option<u32>,
    ) -> Vec<Uuid> {
        let mut ids = Vec::new();
        for (index, sha) in shas.iter().enumerate() {
            let id = queued(store, work, sha, &format!("k{index}")).await;
            if let Some(version) = migration {
                store
                    .lock()
                    .await
                    .conn
                    .execute(
                        "UPDATE rolling_queue_entries SET migration_version=?2 WHERE id=?1",
                        rusqlite::params![id.to_string(), version],
                    )
                    .unwrap();
            }
            ids.push(id);
        }
        ids
    }

    async fn entry_of(store: &Arc<tokio::sync::Mutex<Store>>, id: Uuid) -> RollingQueueEntryV1 {
        store
            .lock()
            .await
            .get_rolling_queue_entry(id)
            .unwrap()
            .unwrap()
    }

    fn log_lines(log: &Path) -> Vec<String> {
        std::fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn is_ancestor_of_origin(work: &Path, sha: &str) -> bool {
        let _ = git(work, &["fetch", "--quiet", "origin", "rolling"]);
        git(
            work,
            &["merge-base", "--is-ancestor", sha, "origin/rolling"],
        )
        .is_some()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_green_batch_publishes_every_source_with_one_push_and_verifies_ancestry() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b", "c", "d"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        let launcher = merging_lander(dir.path(), &work, &log, "");
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let ids = queue_all(&store, &work, &shas, None).await;
        let results = run_next_batch(&store, &launcher, 4).await.unwrap();
        assert_eq!(results.len(), 4);
        // FIFO: settled in enqueue order.
        assert_eq!(results.iter().map(|r| r.0).collect::<Vec<_>>(), ids);
        assert!(
            results
                .iter()
                .all(|(_, r)| r.state == RollingQueueEntryState::Published)
        );
        let lines = log_lines(&log);
        assert_eq!(
            lines.iter().filter(|l| l.starts_with("run ")).count(),
            1,
            "one lander run for the whole batch: {lines:?}"
        );
        assert_eq!(lines.iter().filter(|l| *l == "push").count(), 1);
        let run = &lines[0];
        for sha in &shas {
            assert!(run.contains(&format!("--accepted {sha}")), "{run}");
            assert!(is_ancestor_of_origin(&work, sha));
        }
        // The batch is one row; every owner is woken once.
        let guard = store.lock().await;
        let (state, members): (String, i64) = guard
            .conn
            .query_row(
                "SELECT state, member_count FROM rolling_queue_batches",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!((state.as_str(), members), ("published", 4));
        let wakes = guard
            .list_scheduled_jobs()
            .unwrap()
            .into_iter()
            .filter(|job| job.name.starts_with("merge-queue-"))
            .count();
        assert_eq!(wakes, 4);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_reported_source_missing_from_the_published_tip_fails_alone() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b", "c"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        let guard = format!("export SKIP={}", shas[1]);
        let launcher = merging_lander(dir.path(), &work, &log, &guard);
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let ids = queue_all(&store, &work, &shas, None).await;
        run_next_batch(&store, &launcher, 4).await.unwrap();
        assert_eq!(
            entry_of(&store, ids[0]).await.state,
            RollingQueueEntryState::Published
        );
        let lost = entry_of(&store, ids[1]).await;
        assert_eq!(lost.state, RollingQueueEntryState::Failed);
        assert_eq!(
            lost.outcome.unwrap().refusal.as_deref(),
            Some(QUEUE_BATCH_ANCESTRY_UNVERIFIED)
        );
        assert_eq!(
            entry_of(&store, ids[2]).await.state,
            RollingQueueEntryState::Published
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_merge_conflict_refuses_the_whole_batch_and_publishes_nothing() {
        let (dir, _origin, work) = repos();
        let a = commit_file(&work, "a", "shared", "one");
        let b = commit_file(&work, "b", "shared", "two");
        let c = commit_file(&work, "c", "other", "three");
        let shas = vec![a.clone(), b.clone(), c.clone()];
        let log = dir.path().join("lander.log");
        let launcher = merging_lander(dir.path(), &work, &log, "");
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let ids = queue_all(&store, &work, &shas, None).await;
        run_next_batch(&store, &launcher, 4).await.unwrap();
        // A conflict belongs to the batch: one run, no split, no partial publish.
        assert_eq!(lander_runs(&log).len(), 1);
        assert_eq!(log_lines(&log).iter().filter(|l| *l == "push").count(), 0);
        for id in ids {
            let entry = entry_of(&store, id).await;
            assert_eq!(entry.state, RollingQueueEntryState::Refused);
            assert_eq!(
                entry.outcome.unwrap().refusal.as_deref(),
                Some(QUEUE_BATCH_MERGE_CONFLICT)
            );
        }
        for sha in &shas {
            assert!(!is_ancestor_of_origin(&work, sha));
        }
        assert_eq!(wake_count(&store).await, 3);
    }

    /// Guard shell for the merging fake lander: any run whose `--accepted`
    /// list contains a `bad` source prints that source's failing test and
    /// exits red, like a red gate.
    fn red_when_any(bad: &[(&str, &str)]) -> String {
        let mut guard = String::from("red=0\n");
        for (sha, test) in bad {
            guard.push_str(&format!(
                "case \" $* \" in *\" {sha} \"*) echo 'test {test} ... FAILED' >&2; red=1;; esac\n"
            ));
        }
        guard.push_str("if [ $red = 1 ]; then echo publication_status=not_published; exit 1; fi");
        guard
    }

    fn lander_runs(log: &Path) -> Vec<String> {
        log_lines(log)
            .into_iter()
            .filter(|l| l.starts_with("run "))
            .collect()
    }

    async fn wake_count(store: &Arc<tokio::sync::Mutex<Store>>) -> usize {
        store
            .lock()
            .await
            .list_scheduled_jobs()
            .unwrap()
            .into_iter()
            .filter(|job| job.name.starts_with("merge-queue-"))
            .count()
    }

    /// A batch of `count` sources where `bad` (0-based position -> failing
    /// test) are red. Returns the shas, ids, runs and the work tree.
    async fn bisected(
        count: usize,
        bad: &[(usize, &str)],
    ) -> (
        Arc<tokio::sync::Mutex<Store>>,
        Vec<String>,
        Vec<Uuid>,
        Vec<String>,
        PathBuf,
        tempfile::TempDir,
    ) {
        let (dir, _origin, work) = repos();
        let names = ["a", "b", "c", "d", "e", "f", "g", "h"];
        let shas: Vec<String> = names[..count]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        let bad_shas: Vec<(&str, &str)> = bad
            .iter()
            .map(|(index, test)| (shas[*index].as_str(), *test))
            .collect();
        let launcher = merging_lander(dir.path(), &work, &log, &red_when_any(&bad_shas));
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let ids = queue_all(&store, &work, &shas, None).await;
        let results = run_next_batch(&store, &launcher, count).await.unwrap();
        assert_eq!(results.len(), count, "every member settles exactly once");
        let runs = lander_runs(&log);
        (store, shas, ids, runs, work, dir)
    }

    async fn assert_isolated_red(store: &Arc<tokio::sync::Mutex<Store>>, id: Uuid, test: &str) {
        let entry = entry_of(store, id).await;
        assert_eq!(entry.state, RollingQueueEntryState::Refused);
        let outcome = entry.outcome.unwrap();
        assert_eq!(outcome.refusal.as_deref(), Some("gate_failed"));
        assert_eq!(outcome.failing_tests, vec![test.to_string()]);
        assert_eq!(outcome.landed_sha, None);
    }

    async fn assert_landed(
        store: &Arc<tokio::sync::Mutex<Store>>,
        work: &Path,
        id: Uuid,
        sha: &str,
    ) {
        let entry = entry_of(store, id).await;
        assert_eq!(entry.state, RollingQueueEntryState::Published);
        let landed = entry.outcome.unwrap().landed_sha.unwrap();
        assert_eq!(landed.len(), 40);
        assert!(is_ancestor_of_origin(work, sha));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_bad_first_source_is_isolated_in_two_extra_gates_and_the_innocents_publish() {
        let (store, shas, ids, runs, work, _dir) = bisected(4, &[(0, "q::red_a")]).await;
        // batch (red), [a,b] (red), [a] (red: isolated), then [b,c,d] (green).
        assert_eq!(runs.len(), 4, "{runs:?}");
        let counts: Vec<usize> = runs
            .iter()
            .map(|r| r.matches("--accepted").count())
            .collect();
        assert_eq!(counts, vec![4, 2, 1, 3]);
        assert_isolated_red(&store, ids[0], "q::red_a").await;
        for index in 1..4 {
            assert_landed(&store, &work, ids[index], &shas[index]).await;
        }
        assert!(!is_ancestor_of_origin(&work, &shas[0]));
        assert_eq!(wake_count(&store).await, 4);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_bad_last_source_is_isolated_in_two_extra_gates_and_the_innocents_publish() {
        let (store, shas, ids, runs, work, _dir) = bisected(4, &[(3, "q::red_d")]).await;
        // batch (red), [a,b] (green, published), [c] (green, published), d isolated.
        let counts: Vec<usize> = runs
            .iter()
            .map(|r| r.matches("--accepted").count())
            .collect();
        assert_eq!(counts, vec![4, 2, 1], "{runs:?}");
        assert_isolated_red(&store, ids[3], "q::red_d").await;
        for index in 0..3 {
            assert_landed(&store, &work, ids[index], &shas[index]).await;
        }
        assert!(!is_ancestor_of_origin(&work, &shas[3]));
        assert_eq!(wake_count(&store).await, 4);
        // The bisect trail is on the batch, in order.
        let guard = store.lock().await;
        let kinds: Vec<String> = guard
            .conn
            .prepare("SELECT kind FROM rolling_queue_events ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(|kind| kind.unwrap())
            .collect();
        let trail: Vec<&str> = kinds
            .iter()
            .map(String::as_str)
            .filter(|kind| kind.starts_with("bisect"))
            .collect();
        assert_eq!(
            trail,
            vec![
                "bisect_started",
                "bisect_gate",
                "bisect_gate",
                "bisect_isolated"
            ]
        );
        let state: String = guard
            .conn
            .query_row("SELECT state FROM rolling_queue_batches", [], |r| r.get(0))
            .unwrap();
        assert_eq!(state, "published");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn two_bad_sources_in_a_batch_of_four_both_end_red_with_their_own_failing_tests() {
        let (store, shas, ids, _runs, work, _dir) =
            bisected(4, &[(0, "q::red_a"), (2, "q::red_c")]).await;
        assert_isolated_red(&store, ids[0], "q::red_a").await;
        assert_isolated_red(&store, ids[2], "q::red_c").await;
        assert_landed(&store, &work, ids[1], &shas[1]).await;
        assert_landed(&store, &work, ids[3], &shas[3]).await;
        assert!(!is_ancestor_of_origin(&work, &shas[0]));
        assert!(!is_ancestor_of_origin(&work, &shas[2]));
        assert_eq!(wake_count(&store).await, 4);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn an_all_red_batch_settles_every_source_red_and_never_loops() {
        let (store, _shas, ids, runs, _work, _dir) =
            bisected(3, &[(0, "q::r0"), (1, "q::r1"), (2, "q::r2")]).await;
        assert!(runs.len() <= 3 * 4 + 4, "{runs:?}");
        for (index, id) in ids.iter().enumerate() {
            assert_isolated_red(&store, *id, &format!("q::r{index}")).await;
        }
        assert_eq!(wake_count(&store).await, 3);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_second_settle_of_a_bisected_entry_writes_nothing_and_wakes_nobody() {
        let (store, _shas, ids, _runs, _work, _dir) = bisected(4, &[(3, "q::red_d")]).await;
        assert_eq!(wake_count(&store).await, 4);
        let again = RunResult {
            state: RollingQueueEntryState::Refused,
            outcome: RollingQueueOutcome {
                refusal: Some("gate_failed".into()),
                ..RollingQueueOutcome::default()
            },
        };
        for id in &ids {
            settle(&store, *id, &again).await.unwrap();
        }
        assert_eq!(wake_count(&store).await, 4);
        // The first outcome is untouched: the published entries stay published.
        assert_eq!(
            entry_of(&store, ids[0]).await.state,
            RollingQueueEntryState::Published
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_single_source_the_lander_reports_landed_but_the_tip_lacks_fails_typed() {
        let (dir, _origin, work) = repos();
        let sha = commit_file(&work, "a", "a", "a");
        let log = dir.path().join("lander.log");
        let launcher = merging_lander(dir.path(), &work, &log, &format!("export SKIP={sha}"));
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let id = queued(&store, &work, &sha, "k").await;
        run_next(&store, &launcher).await.unwrap().unwrap();
        let entry = entry_of(&store, id).await;
        assert_eq!(entry.state, RollingQueueEntryState::Failed);
        assert_eq!(
            entry.outcome.unwrap().refusal.as_deref(),
            Some(QUEUE_BATCH_ANCESTRY_UNVERIFIED)
        );
        assert_eq!(wake_count(&store).await, 1);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_bisect_segment_is_ancestry_verified_before_it_settles_published() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b", "c", "d"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        // d is red; the lander lies about b in every published segment.
        let guard = format!(
            "export SKIP={}\n{}",
            shas[1],
            red_when_any(&[(shas[3].as_str(), "q::red_d")])
        );
        let launcher = merging_lander(dir.path(), &work, &log, &guard);
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let ids = queue_all(&store, &work, &shas, None).await;
        run_next_batch(&store, &launcher, 4).await.unwrap();
        let lost = entry_of(&store, ids[1]).await;
        assert_eq!(lost.state, RollingQueueEntryState::Failed);
        assert_eq!(
            lost.outcome.unwrap().refusal.as_deref(),
            Some(QUEUE_BATCH_ANCESTRY_UNVERIFIED)
        );
        for index in [0, 2] {
            assert_landed(&store, &work, ids[index], &shas[index]).await;
        }
        assert_isolated_red(&store, ids[3], "q::red_d").await;
        assert_eq!(wake_count(&store).await, 4);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_published_segment_records_the_lander_final_migration_number() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        // a is red; b is renumbered to 903 by the lander once a is out.
        let guard = format!(
            "echo migration_0_source_id={b}\necho migration_0_final_version=903\n{red}",
            b = shas[1],
            red = red_when_any(&[(shas[0].as_str(), "q::red_a")])
        );
        let launcher = merging_lander(dir.path(), &work, &log, &guard);
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let ids = queue_all(&store, &work, &shas, None).await;
        run_next_batch(&store, &launcher, 4).await.unwrap();
        assert_isolated_red(&store, ids[0], "q::red_a").await;
        let published = entry_of(&store, ids[1]).await;
        assert_eq!(published.state, RollingQueueEntryState::Published);
        assert_eq!(published.migration_version, Some(903));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn the_out_of_band_regate_budget_is_spent_once_across_the_whole_bisect() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b", "c", "d"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        // The tip keeps moving: every run reports one re-gated stale retry.
        let guard = format!(
            "echo stale_retry_1=aa..bb:regated\n{}",
            red_when_any(&[(shas[3].as_str(), "q::red_d")])
        );
        let launcher = merging_lander(dir.path(), &work, &log, &guard);
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        queue_all(&store, &work, &shas, None).await;
        run_next_batch(&store, &launcher, 4).await.unwrap();
        let budgets: Vec<String> = log_lines(&log)
            .into_iter()
            .filter_map(|line| line.strip_prefix("regates ").map(str::to_string))
            .collect();
        assert!(budgets.len() >= 3, "{budgets:?}");
        assert_eq!(budgets[0], "1");
        assert!(
            budgets[1..].iter().all(|budget| budget == "0"),
            "the batch has one re-gate in total: {budgets:?}"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test(flavor = "multi_thread")]
    async fn restart_in_the_middle_of_a_bisect_settles_each_entry_once_without_a_second_push() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b", "c", "d"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let ids = queue_all(&store, &work, &shas, None).await;
        let claimed = store
            .lock()
            .await
            .claim_rolling_queue_batch(Utc::now(), 4)
            .unwrap()
            .unwrap();
        store
            .lock()
            .await
            .begin_rolling_queue_bisect(claimed.batch_id, "gate_failed", Utc::now())
            .unwrap();
        // The bisect had published [a,b] and settled a; the daemon died before
        // it could settle b, and c/d were still being bisected.
        sh(
            &work,
            &format!(
                "git checkout -q -B land origin/rolling && git merge -q --no-edit {} {} \
                 && git push -q origin HEAD:rolling",
                shas[0], shas[1]
            ),
        );
        let landed = RunResult {
            state: RollingQueueEntryState::Published,
            outcome: RollingQueueOutcome {
                landed_sha: Some(head(&work)),
                ..RollingQueueOutcome::default()
            },
        };
        settle(&store, ids[0], &landed).await.unwrap();
        assert_eq!(wake_count(&store).await, 1);
        // Boot: b landed (settled once), c and d go back to the queue.
        assert_eq!(reconcile_gating(&store).await.unwrap(), (1, 2));
        assert_eq!(reconcile_gating(&store).await.unwrap(), (0, 0));
        assert_eq!(wake_count(&store).await, 2);
        for id in &ids[..2] {
            assert_eq!(
                entry_of(&store, *id).await.state,
                RollingQueueEntryState::Published
            );
        }
        for id in &ids[2..] {
            assert_eq!(
                entry_of(&store, *id).await.state,
                RollingQueueEntryState::Queued
            );
        }
        // Re-driven: c and d publish once each, nothing republished.
        let log = dir.path().join("lander.log");
        let launcher = merging_lander(dir.path(), &work, &log, "");
        let results = run_next_batch(&store, &launcher, 4).await.unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(lander_runs(&log).len(), 1);
        for sha in &shas {
            assert!(is_ancestor_of_origin(&work, sha));
        }
        assert_eq!(wake_count(&store).await, 4);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_lander_policy_refusal_propagates_as_the_batch_policy_code() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        let launcher = merging_lander(
            dir.path(),
            &work,
            &log,
            "echo policy_fence=migration_number; echo 'migration must be tip + 1' >&2; exit 8",
        );
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let ids = queue_all(&store, &work, &shas, None).await;
        run_next_batch(&store, &launcher, 4).await.unwrap();
        for id in ids {
            let entry = entry_of(&store, id).await;
            assert_eq!(entry.state, RollingQueueEntryState::Refused);
            let outcome = entry.outcome.unwrap();
            assert_eq!(outcome.refusal.as_deref(), Some(QUEUE_BATCH_POLICY_REFUSED));
            assert!(
                outcome
                    .detail
                    .unwrap()
                    .contains("policy_fence=migration_number")
            );
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn the_batch_size_caps_the_claim_and_later_entries_run_next() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b", "c"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        let launcher = merging_lander(dir.path(), &work, &log, "");
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let ids = queue_all(&store, &work, &shas, None).await;
        let first = run_next_batch(&store, &launcher, 2).await.unwrap();
        assert_eq!(first.iter().map(|r| r.0).collect::<Vec<_>>(), ids[..2]);
        assert_eq!(
            entry_of(&store, ids[2]).await.state,
            RollingQueueEntryState::Queued
        );
        let second = run_next_batch(&store, &launcher, 2).await.unwrap();
        assert_eq!(second.iter().map(|r| r.0).collect::<Vec<_>>(), ids[2..]);
        assert!(
            run_next_batch(&store, &launcher, 2)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_second_source_claiming_a_taken_migration_number_is_refused_in_order() {
        let (dir, _origin, work) = repos();
        // Both sources add migration 901 on top of tip 900, neither declares
        // a provisional migration: only the first can take 901.
        let shas: Vec<String> = ["a", "b"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        let launcher = merging_lander(dir.path(), &work, &log, "");
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let ids = queue_all(&store, &work, &shas, Some(901)).await;
        run_next_batch(&store, &launcher, 4).await.unwrap();
        let first = entry_of(&store, ids[0]).await;
        assert_eq!(first.state, RollingQueueEntryState::Published);
        assert_eq!(first.migration_version, Some(901));
        let second = entry_of(&store, ids[1]).await;
        assert_eq!(second.state, RollingQueueEntryState::Refused);
        assert_eq!(
            second.outcome.unwrap().refusal.as_deref(),
            Some(QUEUE_MIGRATION_OUT_OF_ORDER)
        );
        let runs = log_lines(&log)
            .into_iter()
            .filter(|l| l.starts_with("run "))
            .collect::<Vec<_>>();
        assert_eq!(runs.len(), 1);
        assert!(runs[0].contains(&shas[0]) && !runs[0].contains(&shas[1]));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test(flavor = "multi_thread")]
    async fn restart_reconcile_settles_every_member_of_a_landed_batch_once() {
        let (_dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        sh(
            &work,
            &format!(
                "git checkout -q -B land origin/rolling && git merge -q --no-edit {} {} \
                 && git push -q origin HEAD:rolling",
                shas[0], shas[1]
            ),
        );
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let ids = queue_all(&store, &work, &shas, None).await;
        // The daemon claimed the batch, the lander published, then the daemon died.
        store
            .lock()
            .await
            .claim_rolling_queue_batch(Utc::now(), 4)
            .unwrap()
            .unwrap();
        assert_eq!(reconcile_gating(&store).await.unwrap(), (2, 0));
        assert_eq!(reconcile_gating(&store).await.unwrap(), (0, 0));
        for id in ids {
            assert_eq!(
                entry_of(&store, id).await.state,
                RollingQueueEntryState::Published
            );
        }
        let wakes = store
            .lock()
            .await
            .list_scheduled_jobs()
            .unwrap()
            .into_iter()
            .filter(|job| job.name.starts_with("merge-queue-"))
            .count();
        assert_eq!(wakes, 2);
    }

    const EXTERNAL_PUSH: &str = "ext() { ( cd @WORKDIR@ && git fetch -q origin rolling \
        && git checkout -q -B ext origin/rolling && printf x > \"$1\" && git add \"$1\" \
        && git commit -qm \"$1\" && git push -q origin HEAD:rolling ) >/dev/null 2>&1; }\n\
        n=$(grep -c '^run ' @LOG@)\ngit fetch -q origin rolling\nred=0\n";

    fn external_push_guard(work: &Path, log: &Path, rest: &str) -> String {
        format!(
            "{}{rest}",
            EXTERNAL_PUSH
                .replace("@WORKDIR@", &work.display().to_string())
                .replace("@LOG@", &log.display().to_string())
        )
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_lander_receipt_failing_test_lines_beat_the_raw_libtest_lines() {
        let stdout = "publication_status=not_published\nfailing_test=q::a\nfailing_test=q::b\n";
        let stderr = "rsi-rolling-land: new test failures relative to rolling abc: q::a [new], q::b [new]; base reds: ";
        let result = classify_run(Some(1), stdout, stderr, || None);
        assert_eq!(result.state, RollingQueueEntryState::Refused);
        assert_eq!(result.outcome.failing_tests, vec!["q::a", "q::b"]);
        // Raw libtest lines still work when no receipt line exists.
        let raw = classify_run(Some(1), "", "test q::c ... FAILED", || None);
        assert_eq!(raw.outcome.failing_tests, vec!["q::c"]);
        // A receipt line is the authority even when raw lines are present.
        let both = classify_run(
            Some(1),
            "failing_test=q::d\n",
            "test q::e ... FAILED",
            || None,
        );
        assert_eq!(both.outcome.failing_tests, vec!["q::d"]);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_missing_source_with_migration_evidence_never_settles_published() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        // The lander reports b landed with migration evidence but never merged it.
        let guard = format!(
            "export SKIP={b}\necho migration_0_source_id={b}\necho migration_0_final_version=901\n",
            b = shas[1]
        );
        let launcher = merging_lander(dir.path(), &work, &log, &guard);
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let ids = queue_all(&store, &work, &shas, None).await;
        run_next_batch(&store, &launcher, 4).await.unwrap();
        assert_landed(&store, &work, ids[0], &shas[0]).await;
        let lost = entry_of(&store, ids[1]).await;
        assert_eq!(lost.state, RollingQueueEntryState::Failed);
        assert_eq!(
            lost.outcome.unwrap().refusal.as_deref(),
            Some(QUEUE_BATCH_ANCESTRY_UNVERIFIED)
        );
        assert_eq!(wake_count(&store).await, 2);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn an_external_fix_before_a_green_prefix_regates_the_now_green_remainder() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        // b is red until an external commit adds `fix`; the fix lands while the
        // [a] gate runs, so the [a,b] red is stale by the time b is next.
        let guard = external_push_guard(
            &work,
            &log,
            &format!(
                "if ! git cat-file -e origin/rolling:fix 2>/dev/null; then \
                 case \" $* \" in *\" {b} \"*) echo 'test q::b ... FAILED' >&2; red=1;; esac; fi\n\
                 if [ $n = 2 ]; then ext fix; fi\n\
                 if [ $red = 1 ]; then echo publication_status=not_published; exit 1; fi",
                b = shas[1]
            ),
        );
        let launcher = merging_lander(dir.path(), &work, &log, &guard);
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let ids = queue_all(&store, &work, &shas, None).await;
        run_next_batch(&store, &launcher, 4).await.unwrap();
        assert_landed(&store, &work, ids[0], &shas[0]).await;
        assert_landed(&store, &work, ids[1], &shas[1]).await;
        assert_eq!(lander_runs(&log).len(), 3, "batch, [a], then b re-gated");
        assert_eq!(wake_count(&store).await, 2);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn two_external_advances_between_runs_spend_one_regate_then_settle_exhausted() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b", "c", "d"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        // Every run is red and someone else pushes right after it, between runs.
        let guard = external_push_guard(
            &work,
            &log,
            &format!(
                "case \" $* \" in *\" {a} \"*|*\" {d} \"*) echo 'test q::x ... FAILED' >&2; red=1;; esac\n\
                 if [ $red = 1 ]; then echo fetched_target_id=$(git rev-parse origin/rolling); \
                 ext e$n; echo publication_status=not_published; exit 1; fi",
                a = shas[0],
                d = shas[3]
            ),
        );
        let launcher = merging_lander(dir.path(), &work, &log, &guard);
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let ids = queue_all(&store, &work, &shas, None).await;
        let results = run_next_batch(&store, &launcher, 4).await.unwrap();
        assert_eq!(results.len(), 4);
        assert_eq!(lander_runs(&log).len(), 2, "no unbounded loop");
        let budgets: Vec<String> = log_lines(&log)
            .into_iter()
            .filter_map(|line| line.strip_prefix("regates ").map(str::to_string))
            .collect();
        assert_eq!(budgets, vec!["1", "0"]);
        for id in ids {
            let entry = entry_of(&store, id).await;
            assert_eq!(entry.state, RollingQueueEntryState::Refused);
            assert_eq!(
                entry.outcome.unwrap().refusal.as_deref(),
                Some(QUEUE_REGATE_EXHAUSTED)
            );
        }
        assert_eq!(wake_count(&store).await, 4);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn an_interaction_only_red_ends_typed_and_never_publishes_a_red_tip() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        // a and b each pass alone; together they fail.
        let guard = format!(
            "red=0\ncase \" $* \" in *\" {a} \"*) case \" $* \" in *\" {b} \"*) \
             echo 'test q::interact ... FAILED' >&2; red=1;; esac;; esac\n\
             if [ $red = 1 ]; then echo publication_status=not_published; exit 1; fi",
            a = shas[0],
            b = shas[1]
        );
        let launcher = merging_lander(dir.path(), &work, &log, &guard);
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let ids = queue_all(&store, &work, &shas, None).await;
        let results = run_next_batch(&store, &launcher, 4).await.unwrap();
        assert_eq!(results.len(), 2);
        assert_landed(&store, &work, ids[0], &shas[0]).await;
        assert_isolated_red(&store, ids[1], "q::interact").await;
        assert!(!is_ancestor_of_origin(&work, &shas[1]));
        let pushes = log_lines(&log).iter().filter(|l| *l == "push").count();
        assert_eq!(pushes, 1, "only the green prefix was ever pushed");
        assert_eq!(wake_count(&store).await, 2);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn restart_after_a_bisected_member_published_restores_its_planned_migration() {
        let (dir, _origin, work) = repos();
        let a = commit_file(&work, "a", "a", "a");
        // b declares a provisional migration: the lander renumbers it to tip + 1
        // once a is out of its group.
        sh(
            &work,
            "git checkout -q -B b origin/rolling && mkdir -p tools/provisional-migrations \
             && printf '{}\\n' > tools/provisional-migrations/b.json && printf b > b \
             && git add -A && git commit -qm b",
        );
        let b = head(&work);
        let _ = dir;
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let ids = queue_all(&store, &work, &[a.clone(), b.clone()], None).await;
        store
            .lock()
            .await
            .conn
            .execute(
                "UPDATE rolling_queue_entries SET migration_version=905 WHERE id=?1",
                rusqlite::params![ids[1].to_string()],
            )
            .unwrap();
        let claimed = store
            .lock()
            .await
            .claim_rolling_queue_batch(Utc::now(), 4)
            .unwrap()
            .unwrap();
        store
            .lock()
            .await
            .begin_rolling_queue_bisect(claimed.batch_id, "gate_failed", Utc::now())
            .unwrap();
        // The bisect is about to run [b] alone; the plan is durable first.
        let group: Vec<RollingQueueEntryV1> = vec![entry_of(&store, ids[1]).await];
        record_migration_plan(&store, &work, claimed.batch_id, &group)
            .await
            .unwrap();
        // The lander pushed b, then the daemon died before recording its report.
        sh(
            &work,
            &format!(
                "git checkout -q -B land origin/rolling && git merge -q --no-edit {b} \
                 && git push -q origin HEAD:rolling"
            ),
        );
        assert_eq!(entry_of(&store, ids[1]).await.migration_version, Some(905));
        assert_eq!(reconcile_gating(&store).await.unwrap(), (1, 1));
        let restored = entry_of(&store, ids[1]).await;
        assert_eq!(restored.state, RollingQueueEntryState::Published);
        assert_eq!(restored.migration_version, Some(901));
        assert_eq!(
            entry_of(&store, ids[0]).await.state,
            RollingQueueEntryState::Queued
        );
    }

    /// A lander that merges its sources, pushes once, and (`late`) reports an
    /// uncertain publication after an external commit landed on top of its
    /// push. It mirrors the real lander's `--expected-tip` handling.
    fn racing_lander(
        dir: &Path,
        work: &Path,
        log: &Path,
        pre_fetch: &str,
        red_sources: &str,
        late: &str,
    ) -> LanderLauncher {
        let body = r#"
echo "run $*" >> @LOG@
echo "regates $RSI_LANDER_MAX_REGATED_STALE_RETRIES" >> @LOG@
n=$(grep -c '^run ' @LOG@)
ext() { ( cd @WORK@ && git fetch -q origin rolling \
    && git checkout -q -B ext origin/rolling && printf x > "$1" && git add "$1" \
    && git commit -qm "$1" && git push -q origin HEAD:rolling ) >/dev/null 2>&1; }
expected=""; prev=""
for arg in "$@"; do
  if [ "$prev" = --expected-tip ]; then expected=$arg; echo "expected $arg" >> @LOG@; fi
  prev=$arg
done
@PRE_FETCH@
cd @WORK@ || exit 9
git fetch -q origin rolling
fetched=$(git rev-parse origin/rolling)
stale=""
if [ -n "$expected" ] && [ "$expected" != "$fetched" ]; then
  if [ "$RSI_LANDER_MAX_REGATED_STALE_RETRIES" = 0 ]; then
    echo 'lander: stale retries exhausted: advanced before the initial fetch' >&2
    echo publication_status=not_published
    echo fetched_target_id=$fetched
    exit 1
  fi
  stale="stale_retry_1=$expected..$fetched:regated"
fi
red=0
if ! git cat-file -e origin/rolling:fix 2>/dev/null; then
  for red_source in @RED@; do case " $* " in *" $red_source "*) red=1;; esac; done
fi
if [ $red = 1 ]; then
  echo 'test q::red ... FAILED' >&2
  echo publication_status=not_published
  echo fetched_target_id=$fetched
  [ -n "$stale" ] && echo "$stale"
  exit 1
fi
git checkout -q -B land origin/rolling
while [ $# -gt 0 ]; do
  if [ "$1" = --accepted ]; then
    git merge -q --no-edit "$2" >/dev/null 2>&1 || { git merge --abort; echo 'Conflict { paths: ["f"] }' >&2; echo publication_status=not_published; exit 1; }
    shift
  fi
  shift
done
git push -q origin HEAD:rolling && echo push >> @LOG@
cand=$(git rev-parse HEAD)
@LATE@
echo publication_status=published
echo fetched_target_id=$fetched
echo candidate_id=$cand
echo published_target_id=$cand
[ -n "$stale" ] && echo "$stale"
exit 0
"#
        .replace("@LOG@", &log.display().to_string())
        .replace("@WORK@", &work.display().to_string())
        .replace("@PRE_FETCH@", pre_fetch)
        .replace("@RED@", red_sources)
        .replace("@LATE@", late);
        fake_lander(dir, &body)
    }

    fn budgets(log: &Path) -> Vec<String> {
        log_lines(log)
            .into_iter()
            .filter_map(|line| line.strip_prefix("regates ").map(str::to_string))
            .collect()
    }

    /// #1007 S4 races, finding 1: the observed remote head after an uncertain
    /// publication is not the queue's own push when an external descendant of
    /// the published candidate sits on top of it. That external commit fixed
    /// the remaining red, so the red evidence is stale: the remainder is
    /// re-gated (charging the regate) and published, never refused.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn an_external_descendant_of_an_uncertain_publication_is_external_not_own() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        // Run 2 (`[a]`) pushes its candidate, then an external commit adding
        // `fix` lands on top before the lander reports; the report is uncertain.
        let launcher = racing_lander(
            dir.path(),
            &work,
            &log,
            "",
            &shas[1],
            "if [ $n = 2 ]; then ext fix; echo publication_status=unknown; \
             echo fetched_target_id=$fetched; echo candidate_id=$cand; \
             echo published_target_id=$cand; exit 1; fi",
        );
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let ids = queue_all(&store, &work, &shas, None).await;
        run_next_batch(&store, &launcher, 4).await.unwrap();
        assert_landed(&store, &work, ids[0], &shas[0]).await;
        assert_landed(&store, &work, ids[1], &shas[1]).await;
        assert_eq!(lander_runs(&log).len(), 3, "batch, [a], then b re-gated");
        assert_eq!(
            budgets(&log),
            vec!["1", "1", "0"],
            "the external descendant spent the batch's regate"
        );
        assert_eq!(wake_count(&store).await, 2);
    }

    /// Finding 3: a push after the queue's probe but before the lander's
    /// initial fetch is charged through `--expected-tip`. With the budget
    /// already spent the lander refuses, and the queue settles the remainder
    /// as `queue_out_of_band_regate_exhausted` instead of gating it.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_push_between_the_probe_and_the_lander_fetch_spends_the_batch_regate() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b", "c"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        // Run 2 is hit by a push right after the queue probed (charged, so the
        // budget is spent); run 4 (`[b, c]`) is hit by a second one.
        let launcher = racing_lander(
            dir.path(),
            &work,
            &log,
            "if [ $n = 2 ]; then ext e1; fi\n\
             if [ $n = 4 ]; then ext e2; fi",
            &shas[0],
            "",
        );
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let ids = queue_all(&store, &work, &shas, None).await;
        run_next_batch(&store, &launcher, 4).await.unwrap();
        let runs = lander_runs(&log);
        assert_eq!(runs.len(), 4, "{runs:?}");
        assert!(!runs[0].contains("--expected-tip"), "{runs:?}");
        for run in &runs[1..] {
            assert!(run.contains("--expected-tip"), "{runs:?}");
        }
        assert_eq!(
            budgets(&log),
            vec!["1", "1", "0", "0"],
            "the probe-to-fetch push in run 2 spent the regate"
        );
        assert_isolated_red(&store, ids[0], "q::red").await;
        for id in &ids[1..] {
            let entry = entry_of(&store, *id).await;
            assert_eq!(entry.state, RollingQueueEntryState::Refused);
            assert_eq!(
                entry.outcome.unwrap().refusal.as_deref(),
                Some(QUEUE_REGATE_EXHAUSTED)
            );
        }
        assert!(!is_ancestor_of_origin(&work, &shas[1]));
    }

    /// A provisional source published while an external migration landed
    /// between the queue's plan fetch and the lander's fetch: the plan says
    /// 901, the published provisional commit carries 902.
    async fn crashed_provisional_run(
        work: &Path,
    ) -> (Arc<tokio::sync::Mutex<Store>>, Uuid, Uuid, String) {
        let a = commit_file(work, "a", "a", "a");
        sh(
            work,
            "git checkout -q -B b origin/rolling && mkdir -p tools/provisional-migrations \
             && printf '{}\\n' > tools/provisional-migrations/b.json && printf b > b \
             && git add -A && git commit -qm b",
        );
        let b = head(work);
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let ids = queue_all(&store, work, &[a, b.clone()], None).await;
        store
            .lock()
            .await
            .conn
            .execute(
                "UPDATE rolling_queue_entries SET migration_version=905 WHERE id=?1",
                rusqlite::params![ids[1].to_string()],
            )
            .unwrap();
        let claimed = store
            .lock()
            .await
            .claim_rolling_queue_batch(Utc::now(), 4)
            .unwrap()
            .unwrap();
        store
            .lock()
            .await
            .begin_rolling_queue_bisect(claimed.batch_id, "gate_failed", Utc::now())
            .unwrap();
        let group: Vec<RollingQueueEntryV1> = vec![entry_of(&store, ids[1]).await];
        record_migration_plan(&store, work, claimed.batch_id, &group)
            .await
            .unwrap();
        // An external migration lands, then the lander publishes b's
        // provisional commit (parents: the tip and b) renumbered to 902.
        sh(
            work,
            &format!(
                "git checkout -q -B ext origin/rolling && mkdir -p crates/rsid/src/store/migrations \
                 && printf 'impl Store {{}}\\n' > crates/rsid/src/store/migrations/v901.rs \
                 && git add -A && git commit -qm ext901 && git push -q origin HEAD:rolling \
                 && git checkout -q -B land origin/rolling && git merge -q --no-commit --no-ff {b} \
                 && printf 'impl Store {{}}\\n' > crates/rsid/src/store/migrations/v902.rs \
                 && git add -A && git commit -qm provisional && git push -q origin HEAD:rolling"
            ),
        );
        (store, ids[1], claimed.batch_id, b)
    }

    /// Finding 2: the restart recovers the number from the published
    /// provisional commit (902), not the stale plan (901).
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn restart_recovers_the_actual_migration_number_from_the_published_commit() {
        let (_dir, _origin, work) = repos();
        let (store, id, _batch, b) = crashed_provisional_run(&work).await;
        assert_eq!(
            planned_number(&store, id).await,
            Some(901),
            "the plan was made before the external migration"
        );
        assert_eq!(published_migration_number(&work, &b), Some(902));
        assert_eq!(reconcile_gating(&store).await.unwrap(), (1, 1));
        let restored = entry_of(&store, id).await;
        assert_eq!(restored.state, RollingQueueEntryState::Published);
        assert_eq!(restored.migration_version, Some(902));
    }

    /// Finding 2: an actual receipt number saved before the crash is never
    /// overwritten on restart, by the plan or by the commit scan.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn restart_never_overwrites_a_saved_receipt_migration_number() {
        let (_dir, _origin, work) = repos();
        let (store, id, batch, _b) = crashed_provisional_run(&work).await;
        // The queue saved the lander's receipt (903), then died before settling.
        bisect_event(
            &store,
            batch,
            "migration_receipt",
            Some(id),
            serde_json::json!({ "version": 903 }),
        )
        .await
        .unwrap();
        store
            .lock()
            .await
            .record_rolling_queue_assigned_migration(id, 903)
            .unwrap();
        assert_eq!(reconcile_gating(&store).await.unwrap(), (1, 1));
        assert_eq!(entry_of(&store, id).await.migration_version, Some(903));
    }

    async fn planned_number(store: &Arc<tokio::sync::Mutex<Store>>, id: Uuid) -> Option<u32> {
        store
            .lock()
            .await
            .planned_rolling_queue_migration(id)
            .unwrap()
    }
}
