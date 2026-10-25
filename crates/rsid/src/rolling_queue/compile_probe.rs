//! #3394: find a SEMANTIC (compile) conflict between a wide queued entry, the
//! rolling tip and the entries queued ahead of it before a batch gates them.
//!
//! The tip sweep ([`super::tip_conflict`]) and the enqueue warnings only see
//! TEXTUAL conflicts. A 181-file type refactor that merges cleanly can still
//! break the entry queued ahead of it, or a file that landed after it was cut,
//! and the lander learns that only when the batch's release build fails: the
//! batch's whole wall plus a bisect round for every other member.
//!
//! What this module does, for a queued entry that is WIDE (many Rust files, or
//! a removed/changed `pub` item of a shared crate, [`classify`]):
//!
//! 1. **Hold.** [`held_head`] reports the first wide entry that has no verdict
//!    yet; the claim ([`crate::store::Store::claim_rolling_queue_batch_lanes_held`])
//!    does not claim it or anything behind it, so it cannot join a batch before
//!    its first probe. The hold is bounded ([`HOLD_MAX`]) and fails open: a
//!    probe that could not run is a verdict too ([`Verdict::Unknown`]).
//! 2. **Probe.** [`spawn_sweep`] merges the rolling tip, the live entries ahead
//!    of the entry (gating ones included) and the entry in a persistent private
//!    clone and runs `cargo check --workspace --all-targets` there, in the hub
//!    build slice, with its own target directory so it never contends with the
//!    gate's lock. One pass runs at a time.
//! 3. **Attribute.** A red check is blamed only on what the entry changes: the
//!    entry alone red on the tip is the entry's own break against rolling; the
//!    entry alone green and the entries ahead green is an interaction, and the
//!    entry is refused `queue_compile_conflict` naming the entries ahead whose
//!    changed files appear in the diagnostics (all of them when none does);
//!    entries ahead red on their own are not the entry's fault and it is kept.
//!
//! A green entry costs one bounded check and nothing else; a narrow entry
//! costs two bounded git reads at claim time (cached per source). Never
//! refused: a probe that could not decide (no cargo, a timeout, a merge that
//! failed, a diagnostic-free failure).

use super::{
    BOUNDED_GIT_MAX_OUTPUT, DeltaRead, git, isolate_probe_git, repository_key_cached,
    run_bounded_optional, source_changed_paths, stable_key, with_git_deadline, with_store,
};
use crate::error::{DaemonError, Result};
use crate::store::Store;
use crate::store::rolling_queue::ROLLING_QUEUE_CLAIM_WINDOW;
use chrono::Utc;
use rsi_common::rolling_queue::{QUEUE_COMPILE_CONFLICT, RollingQueueEntryState};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// Rust files (`*.rs`, `Cargo.toml`, `Cargo.lock`) at which an entry is wide.
pub(super) const WIDE_FILE_THRESHOLD: usize = 40;
/// Source directories whose removed or changed `pub` items make an entry wide:
/// every other crate depends on them.
const SHARED_CRATE_DIRS: &[&str] = &[
    "crates/rsid-core/src/",
    "crates/rsid-store/src/",
    "crates/rsi-common/src/",
];
/// How long a wide entry without a verdict holds the claim.
const HOLD_MAX: Duration = Duration::from_secs(40 * 60);
/// Wall budget of one `cargo check`.
const CHECK_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Wall budget of the git steps that materialize one candidate tree.
const TREE_STEP_TIMEOUT: Duration = Duration::from_secs(120);
/// No probe starts after a pass has run this long.
const PASS_BUDGET: Duration = Duration::from_secs(90 * 60);
/// A settled verdict is recomputed (tip or entries ahead moved) at most this often.
const REPROBE_AFTER: Duration = Duration::from_secs(10 * 60);
/// A probe that could not decide is retried this many times, then left alone.
const MAX_UNKNOWN_ATTEMPTS: u8 = 2;
/// Claim-time wall budget to classify entries not yet seen.
const CLASSIFY_BUDGET: Duration = Duration::from_secs(5);
/// Most diagnostic text kept for attribution.
const MAX_DIAGNOSTIC_BYTES: usize = 256 * 1024;
/// Most characters of the first diagnostics shown in a refusal.
const MAX_SUMMARY_CHARS: usize = 700;
/// Most entries a refusal names.
const MAX_NAMED_ENTRIES: usize = 8;
/// The probe's cargo target is emptied when it grows past this.
const TARGET_MAX_BYTES: u64 = 40 << 30;

/// Whether an entry needs a compile probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Width {
    Narrow,
    /// Wide, with the reason shown in logs.
    Wide(String),
}

/// An entry is wide when it changes many Rust files, or removes or changes a
/// `pub` item (`public_surface_changed`) in a shared crate. An addition cannot
/// break a caller, so an additions-only diff stays narrow.
pub(super) fn classify(paths: &[String], public_surface_changed: bool) -> Width {
    let rust = paths.iter().filter(|path| is_rust_path(path)).count();
    if rust >= WIDE_FILE_THRESHOLD {
        return Width::Wide(format!("{rust} Rust files"));
    }
    if public_surface_changed {
        return Width::Wide("a public item of a shared crate was removed or changed".to_owned());
    }
    Width::Narrow
}

fn is_rust_path(path: &str) -> bool {
    path.ends_with(".rs")
        || path.ends_with("/Cargo.toml")
        || path.ends_with("/Cargo.lock")
        || path == "Cargo.toml"
        || path == "Cargo.lock"
}

/// Whether a `git diff -U0` text removes or changes a `pub` item: a removed
/// line (a change is a removal plus an addition) that declares one.
pub(super) fn removes_public_item(diff: &str) -> bool {
    diff.lines().any(|line| {
        line.strip_prefix('-')
            .filter(|_| !line.starts_with("---"))
            .map(str::trim_start)
            .is_some_and(|text| text.starts_with("pub ") || text.starts_with("pub("))
    })
}

/// What the probe learned about one `(checkout, source commit)` pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Verdict {
    /// The tip, the entries ahead and the source compile together.
    Clear,
    /// They do not, and the break is the source's: `blame` says against what.
    Breaks { blame: Blame, error: String },
    /// Red, but the entries ahead are red on their own: not this source's fault.
    NotBlamed,
    /// The probe could not decide; nothing is refused.
    Unknown(String),
}

/// What a source breaks against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Blame {
    /// The rolling tip on its own.
    Tip,
    /// These entries queued ahead of it (id and source commit).
    Entries(Vec<(Uuid, String)>),
}

/// An entry ahead of the probed one.
#[derive(Debug, Clone)]
pub(super) struct Ahead {
    pub id: Uuid,
    pub source: String,
    /// Paths the entry changes: attribution matches them against diagnostics.
    pub paths: Vec<String>,
}

/// The diagnostics of a red check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Diagnostics {
    /// The first diagnostics, for the refusal.
    pub summary: String,
    /// The bounded diagnostic text, for attribution.
    pub text: String,
}

/// One `cargo check` outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum CheckOutcome {
    Green,
    Red(Diagnostics),
    Unknown(String),
}

/// Compiles a materialized tree. Production is [`CargoChecker`]; tests inject a
/// stand-in that reads the tree.
pub(super) trait Checker: Send + Sync {
    fn check(&self, tree: &Path, deadline: Instant) -> CheckOutcome;
}

/// `cargo check --workspace --all-targets` in the hub build slice.
pub(super) struct CargoChecker {
    /// The probe's own cargo target.
    pub target_dir: PathBuf,
    /// Run in a scope of `rsi-build.slice` (the slice could be configured).
    pub scoped: bool,
}

impl Checker for CargoChecker {
    fn check(&self, tree: &Path, deadline: Instant) -> CheckOutcome {
        let remaining = deadline
            .saturating_duration_since(Instant::now())
            .min(CHECK_TIMEOUT);
        if remaining.is_zero() {
            return CheckOutcome::Unknown("the probe budget ran out".to_owned());
        }
        let log = tree.with_file_name("check.stderr");
        let Ok(stderr) = std::fs::File::create(&log) else {
            return CheckOutcome::Unknown("cannot create the check log".to_owned());
        };
        let mut inner = Command::new("cargo");
        inner
            .args([
                "check",
                "--workspace",
                "--all-targets",
                "--keep-going",
                "--message-format",
                "short",
            ])
            .current_dir(tree)
            .env("CARGO_TARGET_DIR", &self.target_dir)
            .env("CARGO_TERM_COLOR", "never")
            .env_remove("RSI_SESSION_TOKEN");
        let (command, unit) = match self.scoped {
            true => {
                let (command, unit) = crate::build_budget::scope_std_command(&inner);
                (command, Some(unit))
            }
            false => (inner, None),
        };
        let outcome = run_bounded_optional(command, remaining, 1 << 20, Stdio::from(stderr));
        if outcome.is_none()
            && let Some(unit) = unit
        {
            crate::build_budget::kill_scope_blocking(&unit);
        }
        let text = read_capped(&log);
        interpret_check(outcome.map(|(code, _)| code), &text)
    }
}

fn read_capped(path: &Path) -> String {
    use std::io::Read;
    let mut bytes = Vec::new();
    if let Ok(file) = std::fs::File::open(path) {
        let _ = file
            .take(MAX_DIAGNOSTIC_BYTES as u64)
            .read_to_end(&mut bytes);
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Read a finished `cargo check` (`None`: it timed out and was killed).
/// Red needs an actual compiler diagnostic: cargo exits 101 for every failure
/// (an offline registry, a manifest it cannot parse), and none of those is a
/// semantic conflict.
pub(super) fn interpret_check(exit: Option<Option<i32>>, stderr: &str) -> CheckOutcome {
    match exit {
        None => CheckOutcome::Unknown("cargo check ran past its wall budget".to_owned()),
        Some(Some(0)) => CheckOutcome::Green,
        Some(_) if has_compiler_diagnostic(stderr) => CheckOutcome::Red(Diagnostics {
            summary: first_diagnostics(stderr),
            text: stderr.to_owned(),
        }),
        Some(code) => CheckOutcome::Unknown(format!(
            "cargo check exited {code:?} without a compiler diagnostic: {}",
            stderr
                .lines()
                .rev()
                .find(|line| !line.trim().is_empty())
                .unwrap_or("no output")
                .chars()
                .take(200)
                .collect::<String>()
        )),
    }
}

fn is_diagnostic_line(line: &str) -> bool {
    // `--message-format short`: `path:line:col: error[E0425]: message`.
    line.contains(": error[E") || line.contains(": error:") || line.starts_with("error[E")
}

fn has_compiler_diagnostic(stderr: &str) -> bool {
    stderr.lines().any(|line| {
        (is_diagnostic_line(line) || line.contains("could not compile"))
            && !line.contains("failed to")
    })
}

/// The first diagnostics of a red check, bounded.
pub(super) fn first_diagnostics(stderr: &str) -> String {
    let mut shown = String::new();
    for line in stderr
        .lines()
        .filter(|line| is_diagnostic_line(line))
        .take(3)
    {
        if !shown.is_empty() {
            shown.push_str(" | ");
        }
        shown.push_str(line.trim());
    }
    if shown.is_empty() {
        shown.push_str("the check failed to compile");
    }
    shown.chars().take(MAX_SUMMARY_CHARS).collect()
}

/// The entries ahead whose changed paths appear in the diagnostics; every
/// entry ahead when none does (the break is real but not located).
pub(super) fn narrow_blame(diagnostics: &str, ahead: &[Ahead]) -> Vec<(Uuid, String)> {
    let named: Vec<(Uuid, String)> = ahead
        .iter()
        .filter(|entry| {
            entry
                .paths
                .iter()
                .any(|path| diagnostics.contains(path.as_str()))
        })
        .map(|entry| (entry.id, entry.source.clone()))
        .collect();
    match named.is_empty() {
        true => ahead
            .iter()
            .map(|entry| (entry.id, entry.source.clone()))
            .collect(),
        false => named,
    }
}

/// Builds and checks candidate trees for one repository.
pub(super) struct Prober<'a> {
    /// The persistent private clone the candidates are merged in.
    pub tree: PathBuf,
    pub checker: &'a dyn Checker,
}

/// Why a candidate tree could not be built.
enum Built {
    Ready,
    /// The source itself does not merge (the tip sweep and the enqueue
    /// warnings own textual conflicts).
    SourceConflicts,
    Failed(String),
}

impl Prober<'_> {
    fn git(&self, args: &[&str], deadline: Instant) -> Option<(Option<i32>, Vec<u8>)> {
        let mut command = Command::new(super::git_program());
        isolate_probe_git(&mut command);
        command
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "core.fsmonitor=false",
                "-c",
                "gc.auto=0",
                "-c",
                "maintenance.auto=false",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "user.name=rsi-queue-probe",
                "-c",
                "user.email=rsi-queue-probe@localhost",
            ])
            .arg("-C")
            .arg(&self.tree)
            .args(args);
        let remaining = deadline
            .saturating_duration_since(Instant::now())
            .min(TREE_STEP_TIMEOUT);
        if remaining.is_zero() {
            return None;
        }
        run_bounded_optional(command, remaining, BOUNDED_GIT_MAX_OUTPUT, Stdio::null())
    }

    fn git_ok(&self, args: &[&str], deadline: Instant) -> bool {
        matches!(self.git(args, deadline), Some((Some(0), _)))
    }

    /// Reset the tree to `tip`, then merge each of `ahead` that merges cleanly
    /// (one that conflicts is skipped: the tip sweep owns it) and `source`.
    fn build(&self, tip: &str, ahead: &[&str], source: Option<&str>, deadline: Instant) -> Built {
        let _ = self.git(&["merge", "--abort"], deadline);
        if !self.git_ok(
            &["checkout", "--detach", "--force", "--quiet", tip],
            deadline,
        ) || !self.git_ok(&["reset", "--hard", "--quiet", tip], deadline)
            || !self.git_ok(&["clean", "-fdq"], deadline)
        {
            // A tree whose alternate object store vanished never heals: drop
            // it so the next probe clones afresh.
            let _ = std::fs::remove_dir_all(&self.tree);
            return Built::Failed("cannot check out the rolling tip in the probe tree".to_owned());
        }
        let merge = |commit: &str| {
            self.git(
                &[
                    "merge",
                    "--no-edit",
                    "--no-ff",
                    "--no-verify",
                    "--quiet",
                    commit,
                ],
                deadline,
            )
        };
        for commit in ahead {
            match merge(commit) {
                Some((Some(0), _)) => {}
                Some(_) => {
                    let _ = self.git(&["merge", "--abort"], deadline);
                    let _ = self.git(&["reset", "--hard", "--quiet", "HEAD"], deadline);
                }
                None => return Built::Failed("a probe merge did not finish".to_owned()),
            }
        }
        if let Some(commit) = source {
            match merge(commit) {
                Some((Some(0), _)) => {}
                Some(_) => {
                    let _ = self.git(&["merge", "--abort"], deadline);
                    return Built::SourceConflicts;
                }
                None => return Built::Failed("the source merge did not finish".to_owned()),
            }
        }
        Built::Ready
    }

    fn check_tree(
        &self,
        tip: &str,
        ahead: &[&str],
        source: Option<&str>,
        deadline: Instant,
    ) -> Tried {
        match self.build(tip, ahead, source, deadline) {
            Built::Ready => Tried::Checked(self.checker.check(&self.tree, deadline)),
            Built::SourceConflicts => Tried::Undecided(
                "the source does not merge onto the tip and the entries ahead".to_owned(),
            ),
            Built::Failed(reason) => Tried::Undecided(reason),
        }
    }

    /// Judge `source` against `tip` and the entries `ahead` of it.
    pub(super) fn probe(
        &self,
        tip: &str,
        ahead: &[Ahead],
        source: &str,
        deadline: Instant,
    ) -> Verdict {
        let ahead_sources: Vec<&str> = ahead.iter().map(|entry| entry.source.as_str()).collect();
        let diagnostics = match self.check_tree(tip, &ahead_sources, Some(source), deadline) {
            Tried::Undecided(reason) => return Verdict::Unknown(reason),
            Tried::Checked(CheckOutcome::Green) => return Verdict::Clear,
            Tried::Checked(CheckOutcome::Unknown(reason)) => return Verdict::Unknown(reason),
            Tried::Checked(CheckOutcome::Red(diagnostics)) => diagnostics,
        };
        // Red. Is it the source against the tip alone?
        match self.check_tree(tip, &[], Some(source), deadline) {
            Tried::Undecided(reason) => return Verdict::Unknown(reason),
            Tried::Checked(CheckOutcome::Unknown(reason)) => return Verdict::Unknown(reason),
            Tried::Checked(CheckOutcome::Red(alone)) => {
                return Verdict::Breaks {
                    blame: Blame::Tip,
                    error: alone.summary,
                };
            }
            Tried::Checked(CheckOutcome::Green) => {}
        }
        if ahead.is_empty() {
            // Nothing to blame but the source, yet it is green alone: the
            // first check saw something the second did not. Undecided.
            return Verdict::Unknown(
                "the tip plus the source was red, the source alone green".to_owned(),
            );
        }
        // Green on the tip alone: is the red already in the entries ahead?
        match self.check_tree(tip, &ahead_sources, None, deadline) {
            Tried::Undecided(reason) => Verdict::Unknown(reason),
            Tried::Checked(CheckOutcome::Unknown(reason)) => Verdict::Unknown(reason),
            Tried::Checked(CheckOutcome::Red(_)) => Verdict::NotBlamed,
            Tried::Checked(CheckOutcome::Green) => Verdict::Breaks {
                blame: Blame::Entries(narrow_blame(&diagnostics.text, ahead)),
                error: diagnostics.summary,
            },
        }
    }
}

enum Tried {
    Checked(CheckOutcome),
    Undecided(String),
}

/// The refusal detail for a source that breaks the build.
pub(super) fn refusal_detail(tip: &str, blame: &Blame, error: &str) -> String {
    let against = match blame {
        Blame::Tip => format!("the rolling tip {tip}"),
        Blame::Entries(entries) => {
            let shown: Vec<String> = entries
                .iter()
                .take(MAX_NAMED_ENTRIES)
                .map(|(id, source)| format!("{id} ({})", &source[..source.len().min(12)]))
                .collect();
            let more = entries.len().saturating_sub(shown.len());
            format!(
                "the rolling tip {tip} plus the queued entries ahead of it: {}{}",
                shown.join(", "),
                if more > 0 {
                    format!(" (+{more} more)")
                } else {
                    String::new()
                }
            )
        }
    };
    let next = match blame {
        Blame::Tip => {
            "merge origin/rolling into the worker's branch, fix the build, re-verify and enqueue that commit instead"
        }
        Blame::Entries(_) => {
            "wait for those entries to land, merge origin/rolling into the worker's branch, fix the build, re-verify and enqueue that commit instead"
        }
    };
    format!(
        "compile_conflict: `cargo check --workspace --all-targets` of this source merged with {against} fails: {error}; {next}"
    )
}

// ---------------------------------------------------------------- state

/// What the last probe of one `(checkout, source)` settled.
#[derive(Debug, Clone)]
struct Judged {
    tip: String,
    ahead: Vec<String>,
    at: Instant,
    unknowns: u8,
}

#[derive(Default)]
struct ProbeState {
    widths: HashMap<(PathBuf, String), Width>,
    /// Wide entries seen without a verdict, and since when.
    pending: HashMap<(PathBuf, String), Instant>,
    judged: HashMap<(PathBuf, String), Judged>,
}

static STATE: std::sync::LazyLock<Mutex<ProbeState>> = std::sync::LazyLock::new(Default::default);
static RUNNING: AtomicBool = AtomicBool::new(false);

fn state() -> MutexGuard<'static, ProbeState> {
    STATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Blocking: whether `source` is wide, reading git once and remembering the
/// answer (a commit never changes). `None` when git could not say.
fn width_of(repo: &Path, source: &str) -> Option<Width> {
    let key = (repo.to_path_buf(), source.to_owned());
    if let Some(known) = state().widths.get(&key) {
        return Some(known.clone());
    }
    let paths = match source_changed_paths(repo, source) {
        DeltaRead::Paths(paths) => paths,
        // Too large to list is as wide as it gets; unreadable is unknown.
        DeltaRead::TooLarge(_) => {
            return remember(key, Width::Wide("too many changed files".to_owned()));
        }
        DeltaRead::Unreadable => return None,
    };
    let surface = match classify(&paths, false) {
        Width::Wide(_) => false,
        Width::Narrow => shared_surface_removed(repo, source)?,
    };
    remember(key, classify(&paths, surface))
}

fn remember(key: (PathBuf, String), width: Width) -> Option<Width> {
    let mut state = state();
    if state.widths.len() >= 4096 {
        state.widths.clear();
    }
    state.widths.insert(key, width.clone());
    Some(width)
}

/// Blocking: whether `source` removes or changes a `pub` item under a shared
/// crate, against its merge base with rolling.
fn shared_surface_removed(repo: &Path, source: &str) -> Option<bool> {
    let base = ["origin/rolling", "rolling"].iter().find_map(|reference| {
        git(repo, &["merge-base", source, reference])
            .map(|out| out.trim().to_owned())
            .filter(|base| !base.is_empty())
    })?;
    let mut args = vec![
        "diff",
        "-U0",
        "--no-color",
        "--no-ext-diff",
        "--no-renames",
        base.as_str(),
        source,
        "--",
    ];
    args.extend(SHARED_CRATE_DIRS);
    let diff = git(repo, &args)?;
    Some(removes_public_item(&diff))
}

/// Blocking, claim time: the source of the first queued entry (FIFO) that is
/// wide and has no verdict yet. Classification is cached per source and the
/// read is bounded by [`CLASSIFY_BUDGET`]; what it did not reach is not held.
pub(super) fn held_head(candidates: &[(PathBuf, String)]) -> Option<String> {
    let deadline = Instant::now() + CLASSIFY_BUDGET;
    for (repo, source) in candidates {
        if Instant::now() >= deadline {
            return None;
        }
        let key = (repo.clone(), source.clone());
        if state().judged.contains_key(&key) {
            continue;
        }
        let (width, _) = with_git_deadline(deadline, || width_of(repo, source));
        match width {
            Some(Width::Wide(reason)) => {
                let mut state = state();
                let since = *state.pending.entry(key).or_insert_with(|| {
                    tracing::info!(%source, reason, "rolling queue entry is wide: held for its compile probe");
                    Instant::now()
                });
                if since.elapsed() < HOLD_MAX {
                    return Some(source.clone());
                }
            }
            Some(Width::Narrow) | None => {}
        }
    }
    None
}

/// The held head for the queue's oldest entries, for the claim.
pub(super) async fn held_head_for_claim(
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    limit: usize,
) -> Option<String> {
    let queued = with_store(store, move |s| {
        s.list_rolling_queue_entries(Some(RollingQueueEntryState::Queued), limit)
    })
    .await
    .ok()?;
    let candidates: Vec<(PathBuf, String)> = queued
        .into_iter()
        .map(|(entry, repo)| (PathBuf::from(repo), entry.source_commit))
        .collect();
    tokio::task::spawn_blocking(move || held_head(&candidates))
        .await
        .ok()
        .flatten()
}

// ---------------------------------------------------------------- sweep

/// Run one pass in the background unless one is still running.
pub(super) fn spawn_sweep(store: &Arc<rsid_core::store_lock::Mutex<Store>>, root: PathBuf) {
    if RUNNING.swap(true, Ordering::AcqRel) {
        return;
    }
    let store = Arc::clone(store);
    tokio::spawn(async move {
        if let Err(error) = sweep(&store, &root).await {
            tracing::warn!(%error, "rolling queue compile probe deferred");
        }
        RUNNING.store(false, Ordering::Release);
    });
}

/// One queued or gating entry as the pass sees it.
struct Candidate {
    id: Uuid,
    sequence: i64,
    repo: PathBuf,
    source: String,
    queued: bool,
}

async fn sweep(store: &Arc<rsid_core::store_lock::Mutex<Store>>, root: &Path) -> Result<Vec<Uuid>> {
    let scoped = !crate::build_budget::apply("compile-probe")
        .await
        .is_unavailable();
    let checker = Arc::new(CargoChecker {
        target_dir: root.join("target"),
        scoped,
    });
    sweep_with(store, root, checker).await
}

/// One pass with `checker` compiling the candidate trees. Returns the entries
/// it refused.
pub(super) async fn sweep_with(
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    root: &Path,
    checker: Arc<dyn Checker>,
) -> Result<Vec<Uuid>> {
    let (gating, queued) = with_store(store, |s| {
        Ok((
            s.list_rolling_queue_entries(
                Some(RollingQueueEntryState::Gating),
                ROLLING_QUEUE_CLAIM_WINDOW,
            )?,
            s.list_rolling_queue_entries(
                Some(RollingQueueEntryState::Queued),
                ROLLING_QUEUE_CLAIM_WINDOW,
            )?,
        ))
    })
    .await?;
    let mut candidates: Vec<Candidate> = gating
        .into_iter()
        .map(|entry| (entry, false))
        .chain(queued.into_iter().map(|entry| (entry, true)))
        .map(|((entry, repo), queued)| Candidate {
            id: entry.id,
            sequence: entry.sequence,
            repo: PathBuf::from(repo),
            source: entry.source_commit,
            queued,
        })
        .collect();
    candidates.sort_by_key(|candidate| candidate.sequence);
    if candidates.iter().all(|candidate| !candidate.queued) {
        state().pending.clear();
        return Ok(Vec::new());
    }
    let root = root.to_path_buf();
    let refusals = tokio::task::spawn_blocking(move || judge_all(&candidates, &root, &*checker))
        .await
        .map_err(|error| DaemonError::Process(format!("compile probe join: {error}")))?;
    let mut refused = Vec::new();
    for (id, source, detail) in refusals {
        let target = source.clone();
        let wake = with_store(store, move |s| {
            s.refuse_queued_rolling_queue_entry_for_source(
                id,
                &target,
                QUEUE_COMPILE_CONFLICT,
                &detail,
                Utc::now(),
            )
        })
        .await?;
        if wake.is_some() {
            tracing::info!(%id, %source, "rolling queue entry refused by the compile probe");
            refused.push(id);
        }
    }
    Ok(refused)
}

/// A refusal the pass settles: entry, source commit, detail.
type Refusal = (Uuid, String, String);

/// Blocking: probe every wide queued entry that needs it, in FIFO order, those
/// holding the claim first. Returns the refusals to settle.
fn judge_all(candidates: &[Candidate], root: &Path, checker: &dyn Checker) -> Vec<Refusal> {
    let started = Instant::now();
    let deadline = started + PASS_BUDGET;
    // Entries gone from the queue forget their state.
    {
        let live: std::collections::HashSet<(&Path, &str)> = candidates
            .iter()
            .map(|candidate| (candidate.repo.as_path(), candidate.source.as_str()))
            .collect();
        let mut state = state();
        state
            .judged
            .retain(|(repo, source), _| live.contains(&(repo.as_path(), source.as_str())));
        state
            .pending
            .retain(|(repo, source), _| live.contains(&(repo.as_path(), source.as_str())));
    }
    let target_dir = root.join("target");
    let mut refusals = Vec::new();
    let mut tips: HashMap<PathBuf, Option<String>> = HashMap::new();
    // Held entries first: they are what the queue waits for.
    let mut order: Vec<usize> = (0..candidates.len())
        .filter(|&index| candidates[index].queued)
        .collect();
    order.sort_by_key(|&index| {
        let candidate = &candidates[index];
        !state()
            .pending
            .contains_key(&(candidate.repo.clone(), candidate.source.clone()))
    });
    let mut trimmed = false;
    for index in order {
        if Instant::now() >= deadline {
            break;
        }
        let candidate = &candidates[index];
        let key = (candidate.repo.clone(), candidate.source.clone());
        let Some(tip) = tips
            .entry(candidate.repo.clone())
            .or_insert_with(|| super::tip_conflict::fresh_tip_for_probe(&candidate.repo, deadline))
            .clone()
        else {
            release_hold(&key, "the rolling tip could not be read");
            continue;
        };
        let (width, _) =
            with_git_deadline(deadline, || width_of(&candidate.repo, &candidate.source));
        match width {
            Some(Width::Wide(_)) => {}
            Some(Width::Narrow) => continue,
            None => {
                release_hold(&key, "the entry could not be classified");
                continue;
            }
        }
        let repo_key = repository_key_cached(&candidate.repo.display().to_string());
        let ahead_entries: Vec<&Candidate> = candidates
            .iter()
            .filter(|other| {
                other.sequence < candidate.sequence
                    && repository_key_cached(&other.repo.display().to_string()) == repo_key
            })
            .collect();
        let ahead_sources: Vec<String> = ahead_entries
            .iter()
            .map(|other| other.source.clone())
            .collect();
        if !needs_probe(&key, &tip, &ahead_sources) {
            continue;
        }
        if !trimmed {
            trimmed = true;
            trim_target(&target_dir);
        }
        let Some(tree) = ensure_tree(root, &candidate.repo, deadline) else {
            record(&key, &tip, ahead_sources, true);
            continue;
        };
        let ahead: Vec<Ahead> = ahead_entries
            .iter()
            .map(|other| Ahead {
                id: other.id,
                source: other.source.clone(),
                paths: match source_changed_paths(&other.repo, &other.source) {
                    DeltaRead::Paths(paths) => paths,
                    _ => Vec::new(),
                },
            })
            .collect();
        let prober = Prober { tree, checker };
        let verdict = prober.probe(&tip, &ahead, &candidate.source, deadline);
        tracing::info!(id = %candidate.id, source = %candidate.source, ?verdict, ahead = ahead.len(), "rolling queue compile probe");
        match verdict {
            Verdict::Clear | Verdict::NotBlamed => record(&key, &tip, ahead_sources, false),
            Verdict::Unknown(reason) => {
                tracing::debug!(reason, "compile probe could not decide");
                record(&key, &tip, ahead_sources, true);
            }
            Verdict::Breaks { blame, error } => {
                record(&key, &tip, ahead_sources, false);
                refusals.push((
                    candidate.id,
                    candidate.source.clone(),
                    refusal_detail(&tip, &blame, &error),
                ));
            }
        }
    }
    refusals
}

/// Whether the pair needs a (re)probe: never judged, or the tip or the
/// entries ahead moved and the last verdict is old enough.
fn needs_probe(key: &(PathBuf, String), tip: &str, ahead: &[String]) -> bool {
    let state = state();
    let Some(judged) = state.judged.get(key) else {
        return true;
    };
    if judged.at.elapsed() < REPROBE_AFTER {
        return false;
    }
    match judged.unknowns {
        0 => judged.tip != tip || judged.ahead != ahead,
        attempts => attempts < MAX_UNKNOWN_ATTEMPTS,
    }
}

fn record(key: &(PathBuf, String), tip: &str, ahead: Vec<String>, unknown: bool) {
    let mut state = state();
    state.pending.remove(key);
    let unknowns = match unknown {
        true => state
            .judged
            .get(key)
            .map_or(1, |judged| judged.unknowns.saturating_add(1)),
        false => 0,
    };
    state.judged.insert(
        key.clone(),
        Judged {
            tip: tip.to_owned(),
            ahead,
            at: Instant::now(),
            unknowns,
        },
    );
}

/// Release the claim hold of an entry whose probe cannot run.
fn release_hold(key: &(PathBuf, String), reason: &str) {
    let mut state = state();
    if state.pending.remove(key).is_some() {
        tracing::debug!(reason, "compile probe hold released");
        state.judged.insert(
            key.clone(),
            Judged {
                tip: String::new(),
                ahead: Vec::new(),
                at: Instant::now(),
                unknowns: MAX_UNKNOWN_ATTEMPTS,
            },
        );
    }
}

/// Blocking: the persistent private clone for `repo`'s object store, created
/// on first use. It borrows the repository's objects through an alternate and
/// has no remote, hooks or config of its own, so nothing reaches the source.
fn ensure_tree(root: &Path, repo: &Path, deadline: Instant) -> Option<PathBuf> {
    let key = stable_key(&repository_key_cached(&repo.display().to_string()));
    let tree = root.join("trees").join(&key).join("tree");
    if tree.join(".git").is_dir() {
        return Some(tree);
    }
    std::fs::create_dir_all(tree.parent()?).ok()?;
    let mut command = Command::new(super::git_program());
    isolate_probe_git(&mut command);
    command
        .args([
            "clone",
            "--shared",
            "--no-checkout",
            "--quiet",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "gc.auto=0",
            "-c",
            "maintenance.auto=false",
        ])
        .arg(repo)
        .arg(&tree);
    let remaining = deadline
        .saturating_duration_since(Instant::now())
        .min(TREE_STEP_TIMEOUT);
    match run_bounded_optional(command, remaining, BOUNDED_GIT_MAX_OUTPUT, Stdio::null()) {
        Some((Some(0), _)) => Some(tree),
        _ => {
            let _ = std::fs::remove_dir_all(&tree);
            None
        }
    }
}

/// Empty the probe's cargo target when it grew past [`TARGET_MAX_BYTES`].
fn trim_target(target: &Path) {
    if dir_exceeds(target, TARGET_MAX_BYTES) {
        tracing::info!(target = %target.display(), "compile probe target over its cap: emptied");
        let _ = std::fs::remove_dir_all(target);
    }
}

/// Whether the files under `dir` total more than `cap` bytes (stops early).
fn dir_exceeds(dir: &Path, cap: u64) -> bool {
    let mut total = 0_u64;
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&next) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                pending.push(entry.path());
            } else if let Ok(meta) = entry.metadata() {
                total = total.saturating_add(meta.len());
                if total > cap {
                    return true;
                }
            }
        }
    }
    false
}

#[cfg(test)]
pub(super) mod tests;
