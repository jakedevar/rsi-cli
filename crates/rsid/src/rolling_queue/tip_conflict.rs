//! #3293: refuse a queued source that already conflicts with the rolling tip.
//!
//! The lander discovers such a conflict only when the entry reaches the front
//! of the queue, so an enqueuer (a topology land node, a manager) waits the
//! whole queue for a refusal it could have had at once. Each tick the queue
//! loop runs [`spawn_sweep`]: every `queued` entry (never one a batch already
//! claimed) is trial-merged onto the rolling tip, and one whose source conflicts
//! with the TIP ITSELF (not with an entry ahead of it) settles `refused` with
//! `queue_conflicts_with_rolling` and its conflict paths, waking its enqueuer
//! like any other refusal.
//!
//! Bounds: one trial merge per entry per tip (the verdict is remembered until
//! the tip moves), at most [`SWEEP_MAX_PROBES`] merges and [`SWEEP_BUDGET`] of
//! wall time per pass, one fetch per repository per [`FETCH_INTERVAL`], and
//! one pass in flight at a time (a stalled fetch never holds the queue loop).
//!
//! Never refused: a merge probe that could not run or finish (missing object,
//! timeout, unsupported git), a tip that could not be read, and a source whose
//! conflicts are all version clashes of the provisional-migration declaration
//! it carries, which the lander renumbers mechanically
//! (`rolling-migration-renumber` in `rsi-repo-guards`).
//!
//! #3346, #3336, #3362: that renumber is proved, never assumed. A source that
//! adds a provisional declaration is dry-run through the tool's transform,
//! resolve and prove steps against the tip ([`super::provisional_probe`]), the
//! very validation the lander applies. A conflict is renumberable only when the
//! tool proves it, so a path merely named in a declaration, or an invalid
//! declaration, exempts nothing: such a conflict settles `refused` with
//! `queue_conflicts_with_rolling` and its paths. A conflict-free source the tool
//! refuses settles `refused` with `provisional_unprovable` and the tool's
//! reason, so it never joins (and splits) a batch.

use super::provisional_probe::{Proof, Renumber, classify_conflict, prove_against_tip};
use super::{
    MERGE_PROBE_TIMEOUT, MergeConflictProbe, fetch_rolling, git, merge_conflict_paths_with,
    with_git_deadline, with_store,
};
use crate::error::{DaemonError, Result};
use crate::store::Store;
use crate::store::rolling_queue::ROLLING_QUEUE_CLAIM_WINDOW;
use chrono::Utc;
use rsi_common::rolling_queue::{
    PROVISIONAL_UNPROVABLE, QUEUE_CONFLICT_WARNING_MAX_FILES, QUEUE_CONFLICTS_WITH_ROLLING,
    RollingQueueEntryState,
};
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// Most trial merges one pass starts; the rest wait for the next tick.
const SWEEP_MAX_PROBES: usize = 16;
/// Wall budget of one pass: no merge starts after it.
const SWEEP_BUDGET: Duration = Duration::from_secs(60);
/// A repository's `origin/rolling` is fetched at most this often by the sweep.
const FETCH_INTERVAL: Duration = Duration::from_secs(30);
/// A probe that could not finish is retried on the same tip this many times.
const MAX_UNKNOWN_ATTEMPTS: u8 = 3;

/// What the last pass learned about one `(repository, source commit)`.
#[derive(Debug, Clone)]
struct Judged {
    tip: String,
    /// Probes of this tip that could not finish.
    unknowns: u8,
}

#[derive(Default)]
struct SweepState {
    judged: HashMap<(PathBuf, String), Judged>,
    fetched: HashMap<PathBuf, Instant>,
}

static STATE: std::sync::LazyLock<Mutex<SweepState>> = std::sync::LazyLock::new(Default::default);
static RUNNING: AtomicBool = AtomicBool::new(false);

fn state() -> MutexGuard<'static, SweepState> {
    STATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The verdict on one source against one tip.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// Merges cleanly, or conflicts only where the renumber tool proves it
    /// resolves them.
    Keep,
    /// Conflicts with the tip on these paths (possibly none named); the tool's
    /// reason when the source's declaration cannot cover them.
    Refuse(Vec<String>, Option<String>),
    /// The source's provisional migration cannot be renumbered onto the tip
    /// (#3346); the tool's reason.
    Unprovable(String),
    /// The probe could not decide; nothing is refused.
    Unknown,
}

/// Run one sweep in the background unless one is still running.
pub(crate) fn spawn_sweep(store: &Arc<rsid_core::store_lock::Mutex<Store>>) {
    if RUNNING.swap(true, Ordering::AcqRel) {
        return;
    }
    let store = Arc::clone(store);
    tokio::spawn(async move {
        if let Err(error) = sweep(&store, FETCH_INTERVAL).await {
            tracing::warn!(%error, "rolling queue conflict sweep deferred");
        }
        RUNNING.store(false, Ordering::Release);
    });
}

/// One pass over the queued entries. Returns the entries it refused.
pub(super) async fn sweep(
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    fetch_interval: Duration,
) -> Result<Vec<Uuid>> {
    let queued = with_store(store, |s| {
        s.list_rolling_queue_entries(
            Some(RollingQueueEntryState::Queued),
            ROLLING_QUEUE_CLAIM_WINDOW,
        )
    })
    .await?;
    let candidates: Vec<(Uuid, PathBuf, String)> = queued
        .into_iter()
        .map(|(entry, repo)| (entry.id, PathBuf::from(repo), entry.source_commit))
        .collect();
    let refusals = tokio::task::spawn_blocking(move || judge_all(candidates, fetch_interval))
        .await
        .map_err(|error| DaemonError::Process(format!("conflict sweep join: {error}")))?;
    let mut refused = Vec::new();
    for (id, commit, code, detail) in refusals {
        let target = commit.clone();
        let wake = with_store(store, move |s| {
            s.refuse_queued_rolling_queue_entry_for_source(id, &target, code, &detail, Utc::now())
        })
        .await?;
        if wake.is_some() {
            tracing::info!(%id, %commit, code, "rolling queue entry refused by the tip sweep");
            refused.push(id);
        }
    }
    Ok(refused)
}

/// A refusal the sweep settles: the entry, its source commit, the refusal code
/// and the detail.
type SweepRefusal = (Uuid, String, &'static str, String);

/// Blocking: every candidate to refuse.
fn judge_all(
    candidates: Vec<(Uuid, PathBuf, String)>,
    fetch_interval: Duration,
) -> Vec<SweepRefusal> {
    let started = Instant::now();
    let deadline = started + SWEEP_BUDGET;
    // Entries gone from the queue forget their verdict.
    {
        let live: BTreeSet<(&Path, &str)> = candidates
            .iter()
            .map(|(_, repo, commit)| (repo.as_path(), commit.as_str()))
            .collect();
        state()
            .judged
            .retain(|(repo, commit), _| live.contains(&(repo.as_path(), commit.as_str())));
        let repos: BTreeSet<&Path> = candidates
            .iter()
            .map(|(_, repo, _)| repo.as_path())
            .collect();
        state()
            .fetched
            .retain(|repo, _| repos.contains(repo.as_path()));
    }
    let mut tips: HashMap<PathBuf, Option<String>> = HashMap::new();
    let mut verdicts: HashMap<(PathBuf, String), Verdict> = HashMap::new();
    let mut probes = 0_usize;
    let mut refusals = Vec::new();
    for (id, repo, commit) in candidates {
        let Some(tip) = tips
            .entry(repo.clone())
            .or_insert_with(|| fresh_tip(&repo, fetch_interval, deadline))
            .clone()
        else {
            continue;
        };
        let key = (repo.clone(), commit.clone());
        if !verdicts.contains_key(&key) {
            let skip = state().judged.get(&key).is_some_and(|judged| {
                judged.tip == tip
                    && (judged.unknowns == 0 || judged.unknowns >= MAX_UNKNOWN_ATTEMPTS)
            });
            if skip || probes >= SWEEP_MAX_PROBES || Instant::now() >= deadline {
                continue;
            }
            probes += 1;
            let (verdict, _) =
                with_git_deadline(deadline, || judge(&repo, &tip, &commit, deadline));
            let mut state = state();
            match verdict {
                // Settled by the refusal below; a failed write is retried.
                Verdict::Refuse(..) | Verdict::Unprovable(_) => {
                    state.judged.remove(&key);
                }
                Verdict::Keep => {
                    state.judged.insert(
                        key.clone(),
                        Judged {
                            tip: tip.clone(),
                            unknowns: 0,
                        },
                    );
                }
                Verdict::Unknown => {
                    let unknowns = state
                        .judged
                        .get(&key)
                        .filter(|judged| judged.tip == tip)
                        .map_or(1, |judged| judged.unknowns.saturating_add(1));
                    state.judged.insert(
                        key.clone(),
                        Judged {
                            tip: tip.clone(),
                            unknowns,
                        },
                    );
                }
            }
            drop(state);
            verdicts.insert(key.clone(), verdict);
        }
        match verdicts.get(&key) {
            Some(Verdict::Refuse(paths, reason)) => refusals.push((
                id,
                commit,
                QUEUE_CONFLICTS_WITH_ROLLING,
                refusal_detail(&tip, paths, reason.as_deref()),
            )),
            Some(Verdict::Unprovable(reason)) => refusals.push((
                id,
                commit,
                PROVISIONAL_UNPROVABLE,
                unprovable_detail(&tip, reason),
            )),
            _ => {}
        }
    }
    tracing::debug!(
        elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        probes,
        refused = refusals.len(),
        "rolling queue conflict sweep"
    );
    refusals
}

/// `repo`'s `origin/rolling` commit after at most one fetch per
/// `fetch_interval`. `None` when the fetch failed or the ref cannot be read, so
/// a stale cached ref never refuses anything.
fn fresh_tip(repo: &Path, fetch_interval: Duration, deadline: Instant) -> Option<String> {
    let due = state()
        .fetched
        .get(repo)
        .is_none_or(|at| at.elapsed() >= fetch_interval);
    if due {
        if Instant::now() >= deadline {
            return None;
        }
        state().fetched.insert(repo.to_path_buf(), Instant::now());
        if fetch_rolling(repo).is_err() {
            // Retry on the next pass, not the next interval.
            state().fetched.remove(repo);
            return None;
        }
    }
    with_git_deadline(deadline, || {
        git(repo, &["rev-parse", "--verify", "origin/rolling^{commit}"])
            .map(|tip| tip.trim().to_owned())
            .filter(|tip| !tip.is_empty())
    })
    .0
}

/// [`fresh_tip`] with the sweep's own fetch interval, for the compile probe
/// (#3394): both read the same remembered fetch, so a pass of either one does
/// not add a fetch to the other.
pub(super) fn fresh_tip_for_probe(repo: &Path, deadline: Instant) -> Option<String> {
    fresh_tip(repo, FETCH_INTERVAL, deadline)
}

fn refusal_detail(tip: &str, paths: &[String], reason: Option<&str>) -> String {
    let shown: Vec<&str> = paths
        .iter()
        .take(QUEUE_CONFLICT_WARNING_MAX_FILES)
        .map(String::as_str)
        .collect();
    let more = paths.len().saturating_sub(shown.len());
    format!(
        "conflict_paths={}{}; the source conflicts with the rolling tip {tip}: merge origin/rolling into the worker's branch, re-verify and enqueue that commit instead{}",
        shown.join(","),
        if more > 0 {
            format!(" (+{more} more)")
        } else {
            String::new()
        },
        reason.map_or_else(String::new, |reason| format!(
            " (its declared provisional migration does not cover them: {reason})"
        )),
    )
}

fn unprovable_detail(tip: &str, reason: &str) -> String {
    format!(
        "{reason}; the source's provisional migration cannot be renumbered onto the rolling tip {tip}: merge origin/rolling into the worker's branch, re-verify and enqueue that commit instead"
    )
}

/// Blocking: the verdict once the trial merge found nothing to refuse: the
/// provisional-migration dry run of the source, if it adds a declaration.
fn judge_provisional(repo: &Path, tip: &str, source: &str, deadline: Instant) -> Verdict {
    match prove_against_tip(repo, tip, source, deadline) {
        Proof::Clear | Proof::Renumbered { .. } => Verdict::Keep,
        Proof::Unprovable(reason) => Verdict::Unprovable(reason),
        Proof::Inconclusive { .. } => Verdict::Unknown,
    }
}

/// Blocking: trial-merge `source` onto `tip`, then dry-run its provisional
/// migration. A conflict stands unless the renumber tool proves the source's
/// declared migration resolves it (#3336, #3362).
fn judge(repo: &Path, tip: &str, source: &str, deadline: Instant) -> Verdict {
    match merge_conflict_paths_with(repo, tip, source, &["git"], MERGE_PROBE_TIMEOUT) {
        MergeConflictProbe::Clean => judge_provisional(repo, tip, source, deadline),
        MergeConflictProbe::Unknown { reason } => {
            tracing::debug!(reason, "queued source-vs-rolling conflict probe incomplete");
            Verdict::Unknown
        }
        MergeConflictProbe::Conflicts(paths) => {
            // A conflict git names no path for is still a conflict.
            if paths.is_empty() {
                return Verdict::Refuse(paths, None);
            }
            match classify_conflict(repo, tip, source, deadline) {
                Renumber::Renumberable(_) => Verdict::Keep,
                Renumber::Conflict(reason) => Verdict::Refuse(paths, reason),
                // Nothing proves the conflict is a real one.
                Renumber::Unknown => Verdict::Unknown,
            }
        }
    }
}
