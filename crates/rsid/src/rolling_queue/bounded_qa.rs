//! Full-library coverage after a bounded rolling gate (#2092) or a gate on
//! filters the queue derived (#2112): land on compile plus the touched tests,
//! run the full suite asynchronously.
//!
//! Coalesce pending ancestors into one pinned rolling snapshot. There is one
//! full job per repository at a time, not one full suite per landing, and at
//! most one start per operator interval (`rolling_queue_full_qa_interval_mins`).
//! Queue audit events retain obligations across restarts; ordinary durable
//! recipe jobs retain service handles, admission/timeout limits, logs and
//! results. A start waits while a deploy drains or host-load admission holds.
//!
//! #2120: a start goes to a healthy, reachable, compatible-build project
//! satellite over the #1017 link when one qualifies (the hub stays cool); the
//! satellite runs the same recipe job and the hub polls it into the same
//! receipts and QA Issue. The hub runs it itself only when no satellite
//! qualifies, recording `ran_on: hub` and why.
//!
//! #2176: the satellite run is a guest. While a deploy to a peer is staged or
//! restarting nothing is started there; when no other satellite takes the run
//! it waits for that deploy (retried once a minute) instead of loading the
//! hub. A deploy stops a run already in flight there, and that attempt is
//! deferred as `deploy_preempted` (the #2149 path). A satellite qualifies only
//! with free space for its floor plus the run's target, and returns that disk
//! after every run (`satellite_qa`).

use super::*;
use crate::agent_jobs::{self, JobRuntime, JobTools, SubmitContext};
use crate::satellite::qa_link::QaLinkError;
use crate::satellite::registry::RegistryPeer;
use crate::store::rolling_queue::{
    BoundedQaIntent, BoundedQaLanding, BoundedQaPlacement, BoundedQaRun,
};
use chrono::DateTime;
use rsi_common::agent_jobs::{
    AgentJobV1, JOB_ADMISSION_TIMED_OUT, JOB_INTERRUPTED_BY_DEPLOY, JOB_START_TIMED_OUT, JobParams,
    JobState, JobWake, TestJobParams,
};
use rsi_common::rolling_queue::{failing_test_outcomes, failing_test_paths};
use rsi_common::satellite::{SATELLITE_WIRE_VERSION_V1, SatellitePeerV1, SatelliteUuidV1};
use rsi_common::satellite_qa::{
    SATELLITE_QA_DEPLOY_PENDING, SATELLITE_QA_HEALTH_MAX_AGE_SECS, SATELLITE_QA_NO_PEER,
    SATELLITE_QA_NOT_FOUND, SATELLITE_QA_STALE, SatelliteQaRunReceiptV1, SatelliteQaRunRequestV1,
};
use rsi_common::types::NewIssue;

pub(super) const RECIPE: &str = agent_jobs::LIBRARY_QA_RECIPE;
/// `<jobs>/rolling-library-qa/<id>/source`: every snapshot below this
/// directory shares one registered cargo target (`agent_jobs`), on the hub
/// and on a satellite alike.
pub(super) const SNAPSHOTS: &str = "rolling-library-qa";

/// Entries whose enqueuer named its own filters (#2112). The queue derives
/// filters for everything else; only explicit ones are exempt from the
/// obligation. This lives in memory: after a restart an entry still queued is
/// obliged like a derived one, which costs at most a coalesced run and never
/// loses coverage.
static EXPLICIT_FILTERS: std::sync::LazyLock<std::sync::Mutex<HashSet<Uuid>>> =
    std::sync::LazyLock::new(Default::default);

fn explicit_filters() -> std::sync::MutexGuard<'static, HashSet<Uuid>> {
    EXPLICIT_FILTERS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Record that `entry` carries caller-named filters (an enqueue replay
/// records it again).
pub(crate) fn note_explicit_filters(entry: Uuid) {
    explicit_filters().insert(entry);
}

#[cfg(test)]
pub(crate) fn has_explicit_filters(entry: Uuid) -> bool {
    explicit_filters().contains(&entry)
}

/// The settling members whose gate ran filters the queue derived: published,
/// they owe the full run whatever selection the lander reports, because the
/// lander sees derived filters as explicit (`selection=closure
/// reason=explicit_filters`). An unfiltered gate is judged by the lander's own
/// selection evidence. Forgets the members' explicit marks: they are settling.
pub(super) fn derived_filter_members(group: &[RollingQueueEntryV1]) -> Vec<Uuid> {
    let mut explicit = explicit_filters();
    group
        .iter()
        .filter(|entry| !explicit.remove(&entry.id) && !entry.test_filters.is_empty())
        .map(|entry| entry.id)
        .collect()
}

pub(super) fn full_library_params() -> JobParams {
    JobParams::Test(TestJobParams {
        qa_lane: None,
        scoped_test: None,
        recipe: Some(RECIPE.into()),
        shard: None,
        filterset: None,
        package: None,
        filters: Vec::new(),
        exact: false,
        lib_only: false,
        test_target: None,
        candidate_receipt: None,
        timeout_minutes: Some(180),
        cpu_quota_percent: None,
    })
}

/// At most one preceding 180-minute job + one's own 180-minute job, with
/// admission and deploy margins. An overdue attempt files an Issue too.
const COVERAGE_DEADLINE_HOURS: i64 = 8;

/// #2394: host-load admission may postpone a pending start this long after
/// the landing's publication. Past it the start goes ahead (the job's own
/// cargo-slot governor still admits it), so a loaded host cannot starve the
/// coverage obligation into its eight-hour overdue Issue. A deploy drain is
/// never bypassed: a staged deploy would stop the run at once.
const HOST_LOAD_BYPASS_HOURS: i64 = 3;

/// A pending landing this old gets `bounded_qa_held` receipts naming why its
/// start is postponed (#2394); a younger one is just in the normal interval.
const HOLD_AUDIT_AFTER_MINS: i64 = 60;

/// Why a start is postponed right now (#2394). The code is the typed reason
/// in the `bounded_qa_held` receipt and the overdue Issue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Hold {
    /// A deploy is staged or restarting.
    Drain,
    /// Host-load admission holds new heavy work (#1417).
    HostLoad,
}

impl Hold {
    const fn code(self) -> &'static str {
        match self {
            Self::Drain => "deploy_drain",
            Self::HostLoad => "host_load",
        }
    }
}

/// A satellite run that stays unreachable this long after its dispatch (one
/// 180-minute job plus margin) settles incomplete, so a vanished satellite
/// never holds the project's coverage forever.
const SATELLITE_RUN_DEADLINE_MINS: i64 = 240;

/// Longest hub fallback reason a receipt records.
const MAX_REASON_BYTES: usize = 512;

/// #2176: the deferral reason of an attempt a deploy stopped
/// (`interrupted_by_deploy`, on a satellite or the hub).
pub(super) const DEPLOY_PREEMPTED: &str = "deploy_preempted";

/// Hub-side satellite state (#2120): where the forwarded satellite sockets
/// live, and the paced polls of in-flight satellite runs (the loop ticks every
/// five seconds; a satellite is asked about a run at most once a minute).
/// #2176: a peer that reported a pending deploy is offered a new run again
/// only after the same pace.
pub(super) struct SatelliteQa {
    root: PathBuf,
    poll_every: std::time::Duration,
    polled: HashMap<Uuid, std::time::Instant>,
    deploying: HashMap<Uuid, std::time::Instant>,
}

impl SatelliteQa {
    pub(super) fn new(root: PathBuf) -> Self {
        Self {
            root,
            poll_every: std::time::Duration::from_secs(60),
            polled: HashMap::new(),
            deploying: HashMap::new(),
        }
    }

    /// Whether `peer` reported a pending deploy within the pace (#2176).
    fn deploy_pending(&self, peer: Uuid) -> bool {
        self.deploying
            .get(&peer)
            .is_some_and(|seen| seen.elapsed() < self.poll_every)
    }

    /// Whether `run` may be polled now; records the poll when it may.
    fn poll_due(&mut self, run: Uuid) -> bool {
        let now = std::time::Instant::now();
        if self
            .polled
            .get(&run)
            .is_some_and(|last| now.duration_since(*last) < self.poll_every)
        {
            return false;
        }
        self.polled.insert(run, now);
        true
    }
}

pub(super) async fn run_loop(
    store: Arc<tokio::sync::Mutex<Store>>,
    config: Arc<RuntimeConfig>,
    launcher: LanderLauncher,
    drain: Arc<crate::deploy_drain::DeployDrain>,
    host_load: Arc<crate::host_load::HostLoadAdmission>,
) {
    let runtime = agent_jobs::platform_job_runtime();
    let tools = JobTools::discover();
    let mut satellites =
        SatelliteQa::new(rsi_common::identity::data_path("satellites", "satellites"));
    let mut interval = tokio::time::interval(TICK);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        let jobs = match agent_jobs::jobs_dir() {
            Ok(jobs) => jobs,
            Err(error) => {
                tracing::warn!(%error, "bounded library QA job directory unavailable");
                continue;
            }
        };
        // Host-load admission (#1417) holds a new start like a manager launch;
        // the job's own cargo-slot governor admission still applies inside it.
        let hold = if drain.is_draining() {
            Some(Hold::Drain)
        } else if host_load.hold_now().is_some() {
            Some(Hold::HostLoad)
        } else {
            None
        };
        let interval = chrono::Duration::minutes(i64::from(
            config
                .rolling_queue_full_qa_interval_mins
                .load(Ordering::Relaxed),
        ));
        if let Err(error) = drive_once(
            &store,
            &launcher,
            Arc::clone(&runtime),
            &tools,
            &jobs,
            &mut satellites,
            hold,
            interval,
            Utc::now(),
        )
        .await
        {
            tracing::warn!(%error, "bounded library QA deferred");
        }
    }
}

/// How long `landing` has waited since its publication settled.
fn pending_for(landing: &BoundedQaLanding, now: DateTime<Utc>) -> chrono::Duration {
    landing
        .entry
        .finished_at
        .as_deref()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map_or_else(chrono::Duration::zero, |finished| {
            now.signed_duration_since(finished)
        })
}

fn repository(launcher: &LanderLauncher, landing: &BoundedQaLanding) -> Option<PathBuf> {
    let original = PathBuf::from(&landing.repo_path);
    if source_commit_exists(&original, &landing.entry.source_commit) {
        return Some(original);
    }
    // An enqueuer's sandbox can be reclaimed after publication. The queue's
    // persistent checkout still retains its repository and published objects.
    std::fs::read_dir(launcher.workspace.as_ref()?.join("worktrees"))
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .find(|repo| source_commit_exists(repo, &landing.entry.source_commit))
}

fn covers(repo: &Path, sha: &str, landing: &BoundedQaLanding) -> bool {
    git(
        repo,
        &[
            "merge-base",
            "--is-ancestor",
            &landing.entry.source_commit,
            sha,
        ],
    )
    .is_some()
        && landing
            .entry
            .outcome
            .as_ref()
            .and_then(|o| o.landed_sha.as_deref())
            .is_some_and(|landed| {
                git(repo, &["merge-base", "--is-ancestor", landed, sha]).is_some()
            })
}

/// A private, immutable detached checkout. It is never reset between runs;
/// restarting between clone and dispatch preserves exactly the same SHA.
pub(super) fn snapshot(repo: &Path, sha: &str, root: &Path) -> Result<PathBuf> {
    let pinned = root.join("source");
    if head_commit(&pinned).is_some() {
        return Ok(pinned);
    }
    std::fs::create_dir_all(root)?;
    let staged = tempfile::Builder::new()
        .prefix("checkout-")
        .tempdir_in(root)?;
    let checkout = staged.path().join("source");
    let args = vec![
        "clone",
        "--shared",
        "--no-checkout",
        "--",
        repo.to_str()
            .ok_or_else(|| DaemonError::Process("QA repository path is not UTF-8".into()))?,
        checkout
            .to_str()
            .ok_or_else(|| DaemonError::Process("QA checkout path is not UTF-8".into()))?,
    ];
    {
        let output = git_command()
            .args(["-c", "core.hooksPath=/dev/null"])
            .args(args)
            .output()?;
        if !output.status.success() {
            return Err(DaemonError::Process(format!(
                "QA snapshot: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }
    }
    let output = git_command()
        .args(["-c", "core.hooksPath=/dev/null", "-C"])
        .arg(&checkout)
        .args(["checkout", "--detach", sha])
        .output()?;
    if !output.status.success() {
        return Err(DaemonError::Process(format!(
            "QA pin: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    std::fs::rename(checkout, &pinned)?;
    Ok(pinned)
}

#[derive(Clone, Copy)]
enum QaFailureKind {
    TestRed,
    Incomplete,
    /// #2149: the run waited for build capacity and never ran tests (the
    /// refusal code). Nothing is red and nobody waits on the result: the
    /// obligation stays pending and a later attempt covers it, so no Issue.
    Capacity(&'static str),
}

fn failure_kind(state: JobState, refusal: Option<&str>, failing_tests: &[String]) -> QaFailureKind {
    for capacity in [JOB_ADMISSION_TIMED_OUT, JOB_START_TIMED_OUT] {
        if refusal == Some(capacity) {
            return QaFailureKind::Capacity(capacity);
        }
    }
    // #2176: a deploy stopped it before its tests finished. The deploy wins;
    // the obligation waits for the next attempt like a capacity deferral.
    if refusal == Some(JOB_INTERRUPTED_BY_DEPLOY) {
        return QaFailureKind::Capacity(DEPLOY_PREEMPTED);
    }
    // A timed-out/interrupted job can report tests killed by the timeout.
    // Its typed infrastructure cause takes precedence over those test names.
    if state == JobState::Failed && refusal.is_none() && !failing_tests.is_empty() {
        return QaFailureKind::TestRed;
    }
    QaFailureKind::Incomplete
}

fn failure_issue(
    landings: &[BoundedQaLanding],
    sha: &str,
    evidence: &str,
    kind: QaFailureKind,
) -> Option<NewIssue> {
    let project = landings.first()?.project_id?;
    let sources = landings
        .iter()
        .map(|landing| {
            format!(
                "- source {} (queue entry {}, published {})",
                landing.entry.source_commit,
                landing.entry.id,
                landing
                    .entry
                    .outcome
                    .as_ref()
                    .and_then(|o| o.landed_sha.as_deref())
                    .unwrap_or("unknown"),
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let (outcome, label, action) = match kind {
        // A capacity deferral files no Issue (see `settle`).
        QaFailureKind::Capacity(_) => return None,
        QaFailureKind::TestRed => (
            "test red",
            "bug",
            "Inspect the named library test failures and fix the red.",
        ),
        QaFailureKind::Incomplete => (
            "incomplete coverage",
            "coverage-incomplete",
            "Rerun the full-library recipe after capacity or admission recovers.",
        ),
    };
    Some(NewIssue {
        project_id: project,
        title: format!(
            "Full library QA {outcome} after landing {}",
            landings[0].entry.source_commit
        ),
        body: format!(
            "Automatic rsid/rsid-store full library run of rolling {sha} after a bounded or derived-filter gate (#2092, #2112).\n\nLandings covered by this attempt:\n{sources}\n\n{evidence}\n\nOutcome: {outcome}. {action} The attempt has no green coverage receipt."
        ),
        priority: Some(1),
        labels: vec!["qa".into(), label.into()],
        created_by_session_id: None,
        assignee: None,
        idea_id: None,
        source_event_id: None,
        source_finding_ref: None,
    })
}

/// Where a run executed, for the QA Issue's evidence (#2120).
fn ran_on(placement: Option<&BoundedQaPlacement>) -> String {
    match placement {
        Some(BoundedQaPlacement::Satellite {
            peer_id,
            peer_label,
            remote_job_id: Some(job),
        }) => format!("satellite {peer_label} (peer {peer_id}, satellite job {job})"),
        Some(BoundedQaPlacement::Satellite {
            peer_id,
            peer_label,
            remote_job_id: None,
        }) => format!("satellite {peer_label} (peer {peer_id}, dispatch unconfirmed)"),
        Some(BoundedQaPlacement::Hub { reason }) => {
            format!("the hub (no satellite took it: {reason})")
        }
        None => "the hub".into(),
    }
}

/// One settled attempt: what was tested, whether it passed, the Issue
/// evidence and the receipt fields (#2120: the same for hub and satellite).
struct Settlement {
    sha: String,
    passed: bool,
    kind: QaFailureKind,
    evidence: Evidence,
    run: BoundedQaRun,
}

/// The Issue evidence of one attempt. The failing-test lists are kept apart
/// so the Issue can name only the tests no open `qa` Issue already tracks
/// (hub #3054); with every test listed, [`Evidence::render`] is the full text.
struct Evidence {
    /// Where and how the run went (the lines before the test lists).
    head: String,
    /// Test paths (`Failing tests:`), sorted and unique.
    failing: Vec<String>,
    /// `FAIL|TIMEOUT <binary> <path>` entries (`Failing outcomes:`).
    outcomes: Vec<String>,
    omitted: String,
    /// `None` for an attempt that produced no run result (no test lists).
    refusal: Option<String>,
}

impl Evidence {
    fn plain(head: String) -> Self {
        Self {
            head,
            failing: Vec::new(),
            outcomes: Vec::new(),
            omitted: String::new(),
            refusal: None,
        }
    }

    /// The text naming only `only` (every test when `None`).
    fn render(&self, only: Option<&[String]>) -> String {
        let Some(refusal) = &self.refusal else {
            return self.head.clone();
        };
        let wanted = |path: &str| only.is_none_or(|tests| tests.iter().any(|t| t == path));
        let failing: Vec<&str> = self
            .failing
            .iter()
            .map(String::as_str)
            .filter(|path| wanted(path))
            .collect();
        let outcomes: Vec<&str> = self
            .outcomes
            .iter()
            .map(String::as_str)
            .filter(|outcome| outcome.split_whitespace().next_back().is_some_and(wanted))
            .collect();
        let omitted = &self.omitted;
        format!(
            "{}\nFailing tests: {}{omitted}\nFailing outcomes: {}{omitted}\nRefusal: {refusal}",
            self.head,
            failing.join(", "),
            outcomes.join(", "),
        )
    }
}

fn tested_every_landing(cohort: &[BoundedQaLanding], sha: &str) -> bool {
    cohort.iter().all(|w| w.qa_sha.as_deref() == Some(sha))
}

/// The Issue's `Failing tests:` line ends ` (and N more)` when the run had
/// failures its list does not name.
fn omitted_suffix(omitted: u32) -> String {
    match omitted {
        0 => String::new(),
        more => format!(" (and {more} more)"),
    }
}

fn hub_settlement(job: &AgentJobV1, sha: String, cohort: &[BoundedQaLanding]) -> Settlement {
    let result = job.result.clone().unwrap_or_default();
    let placement = cohort.first().and_then(|w| w.placement.clone());
    let failing_tests = failing_test_paths(&result.failing_tests);
    let omitted = omitted_suffix(result.failing_tests_omitted);
    let evidence = Evidence {
        head: format!(
            "Ran on {}.\nJob {}, state {:?}, exit {:?}; log {}",
            ran_on(placement.as_ref()),
            job.id,
            job.state,
            job.exit_code,
            job.log_path,
        ),
        failing: failing_tests.clone(),
        outcomes: failing_test_outcomes(&result.failing_tests),
        omitted,
        refusal: Some(result.refusal.clone().unwrap_or_else(|| "none".into())),
    };
    Settlement {
        passed: job.state == JobState::Succeeded
            && job.exit_code == Some(0)
            && tested_every_landing(cohort, &sha),
        kind: failure_kind(job.state, result.refusal.as_deref(), &failing_tests),
        sha,
        evidence,
        run: BoundedQaRun {
            placement,
            exit_code: job.exit_code,
            failing_tests,
            log_path: Some(job.log_path.clone()),
        },
    }
}

/// `placement` with the satellite job the receipt names (an unconfirmed
/// dispatch learns it here).
fn confirmed(
    placement: &BoundedQaPlacement,
    receipt: &SatelliteQaRunReceiptV1,
) -> BoundedQaPlacement {
    let BoundedQaPlacement::Satellite {
        peer_id,
        peer_label,
        ..
    } = placement
    else {
        return placement.clone();
    };
    BoundedQaPlacement::Satellite {
        peer_id: *peer_id,
        peer_label: peer_label.clone(),
        remote_job_id: Some(receipt.job_id),
    }
}

fn satellite_settlement(
    receipt: SatelliteQaRunReceiptV1,
    placement: &BoundedQaPlacement,
    cohort: &[BoundedQaLanding],
) -> Settlement {
    let placement = &confirmed(placement, &receipt);
    let omitted = omitted_suffix(receipt.failing_tests_omitted);
    let failing_tests = failing_test_paths(&receipt.failing_tests);
    let evidence = Evidence {
        head: format!(
            "Ran on {}.\nSatellite job {}, state {:?}, exit {:?}; log {} (on the satellite)",
            ran_on(Some(placement)),
            receipt.job_id,
            receipt.state,
            receipt.exit_code,
            receipt.log_path,
        ),
        failing: failing_tests.clone(),
        outcomes: failing_test_outcomes(&receipt.failing_tests),
        omitted,
        refusal: Some(receipt.refusal.clone().unwrap_or_else(|| "none".into())),
    };
    Settlement {
        passed: receipt.state == JobState::Succeeded
            && receipt.exit_code == Some(0)
            && tested_every_landing(cohort, &receipt.sha),
        kind: failure_kind(receipt.state, receipt.refusal.as_deref(), &failing_tests),
        evidence,
        run: BoundedQaRun {
            placement: Some(placement.clone()),
            exit_code: receipt.exit_code,
            failing_tests,
            log_path: Some(receipt.log_path),
        },
        sha: receipt.sha,
    }
}

/// A satellite attempt that produced no result (the satellite lost it, or
/// stayed unreachable past the deadline): incomplete coverage.
fn satellite_gap(
    placement: &BoundedQaPlacement,
    cohort: &[BoundedQaLanding],
    why: String,
) -> Settlement {
    Settlement {
        sha: cohort
            .first()
            .and_then(|w| w.qa_sha.clone())
            .unwrap_or_else(|| "unavailable".into()),
        passed: false,
        kind: QaFailureKind::Incomplete,
        evidence: Evidence::plain(format!("Ran on {}.\n{why}", ran_on(Some(placement)))),
        run: BoundedQaRun {
            placement: Some(placement.clone()),
            ..BoundedQaRun::default()
        },
    }
}

/// Record the attempt's receipt on every covered landing and, unless it
/// passed, file its QA Issue in the same transaction. An attempt that ended on
/// capacity (#2149) instead records a `bounded_qa_capacity_deferred` audit
/// event per landing and re-arms the obligation, with no Issue and no
/// finished receipt: only the eight-hour overdue rule can still report it.
async fn settle(
    store: &Arc<tokio::sync::Mutex<Store>>,
    cohort: Vec<BoundedQaLanding>,
    job_id: Uuid,
    settlement: Settlement,
    now: DateTime<Utc>,
) -> Result<()> {
    let Settlement {
        sha,
        passed,
        kind,
        evidence,
        run,
    } = settlement;
    if let (false, QaFailureKind::Capacity(refusal)) = (passed, kind) {
        let landings = cohort.len();
        let deferred_sha = sha.clone();
        with_store(store, move |store| {
            store.defer_bounded_qa(&cohort, job_id, &deferred_sha, refusal, &run, now)
        })
        .await?;
        tracing::info!(%job_id, %sha, refusal, landings, "bounded library QA deferred on capacity; the obligation stays pending");
        return Ok(());
    }
    let has_issue = (!passed)
        .then(|| failure_issue(&cohort, &sha, &evidence.render(None), kind))
        .flatten()
        .is_some();
    if !passed && !has_issue {
        return Err(DaemonError::Process(
            "bounded QA red has no project for its Issue".into(),
        ));
    }
    let recorded_sha = sha.clone();
    // The store names the tests to file when an open `qa` Issue already
    // tracks some of them (hub #3054), so the Issue is built from that list.
    let issue = with_store(store, move |store| {
        let make = |tests: &[String]| {
            failure_issue(&cohort, &recorded_sha, &evidence.render(Some(tests)), kind)
        };
        store.finish_bounded_qa_with(
            &cohort,
            job_id,
            &recorded_sha,
            passed,
            &run,
            has_issue.then_some(&make as &dyn Fn(&[String]) -> Option<NewIssue>),
            now,
        )
    })
    .await?;
    tracing::info!(%job_id, %sha, passed, ?issue, "bounded library QA settled");
    Ok(())
}

/// A hub job's binding: its snapshot while the job runs, `None` once settled.
async fn reconcile_hub_job(
    store: &Arc<tokio::sync::Mutex<Store>>,
    jobs: &Path,
    job_id: Uuid,
    cohort: Vec<BoundedQaLanding>,
    now: DateTime<Utc>,
) -> Result<Option<PathBuf>> {
    let Some(row) = with_store(store, move |store| store.get_agent_job(job_id)).await? else {
        return Err(DaemonError::Process(format!(
            "bounded QA job {job_id} is missing"
        )));
    };
    if matches!(row.job.state, JobState::Queued | JobState::Running) {
        return Ok(Some(PathBuf::from(&row.job.cwd)));
    }
    let cwd = PathBuf::from(&row.job.cwd);
    let sha = tokio::task::spawn_blocking(move || head_commit(&cwd))
        .await
        .map_err(|e| DaemonError::Process(e.to_string()))?
        .unwrap_or_else(|| "unavailable".into());
    let settlement = hub_settlement(&row.job, sha, &cohort);
    settle(store, cohort, job_id, settlement, now).await?;
    let cwd = PathBuf::from(row.job.cwd);
    let owned = jobs.join(SNAPSHOTS);
    if cwd.starts_with(&owned) && cwd.file_name().is_some_and(|name| name == "source") {
        tokio::task::spawn_blocking(move || std::fs::remove_dir_all(cwd))
            .await
            .map_err(|e| DaemonError::Process(e.to_string()))??;
    }
    Ok(None)
}

/// The registered peer `peer_id`, whatever its dispatch switch says now (an
/// in-flight run is still collected after the operator stops new dispatch).
async fn registered_peer(
    store: &Arc<tokio::sync::Mutex<Store>>,
    peer_id: Uuid,
) -> Result<Option<RegistryPeer>> {
    let view = with_store(store, Store::satellite_registry_view).await?;
    Ok(view
        .peers
        .iter()
        .find(|value| value.config.peer_id.0 == peer_id)
        .map(crate::satellite::hub::registry_peer))
}

enum SatellitePoll {
    Pending,
    Settled(Settlement),
}

/// Ask the satellite about one in-flight run (paced). Unreachable is pending
/// until [`SATELLITE_RUN_DEADLINE_MINS`] after dispatch, then a gap.
///
/// #180: an unconfirmed dispatch (its start reply was lost) reconciles by
/// replaying the idempotent start under the same run id on the same peer:
/// the satellite reports the run it already has or starts it now, never a
/// second one. A refusal of that replay leaves it pending until the deadline.
async fn poll_satellite(
    store: &Arc<tokio::sync::Mutex<Store>>,
    satellites: &mut SatelliteQa,
    run_id: Uuid,
    placement: &BoundedQaPlacement,
    cohort: &[BoundedQaLanding],
    now: DateTime<Utc>,
) -> Result<SatellitePoll> {
    let BoundedQaPlacement::Satellite {
        peer_id,
        remote_job_id,
        ..
    } = placement
    else {
        return Ok(SatellitePoll::Pending);
    };
    if !satellites.poll_due(run_id) {
        return Ok(SatellitePoll::Pending);
    }
    let Some(sha) = cohort.first().and_then(|w| w.qa_sha.clone()) else {
        return Ok(SatellitePoll::Pending);
    };
    let hub = with_store(store, Store::satellite_installation_id).await?;
    let wire = SatelliteQaRunRequestV1 {
        wire_version: SATELLITE_WIRE_VERSION_V1,
        hub_installation_id: SatelliteUuidV1(hub),
        run_id,
        sha,
        start: remote_job_id.is_none(),
    };
    let answer = match registered_peer(store, *peer_id).await? {
        Some(peer) => {
            crate::satellite::qa_link::request_qa_run(&satellites.root, &peer, &wire, false).await
        }
        None => Err(QaLinkError::NoEffect("satellite_peer_removed")),
    };
    let error = match answer {
        Ok(receipt) if !receipt.state.is_terminal() => return Ok(SatellitePoll::Pending),
        Ok(receipt) => {
            return Ok(SatellitePoll::Settled(satellite_settlement(
                receipt, placement, cohort,
            )));
        }
        Err(error) => error,
    };
    let code = error.code();
    // A confirmed run the satellite has no record of is lost.
    if error == QaLinkError::NoEffect(SATELLITE_QA_NOT_FOUND) {
        return Ok(SatellitePoll::Settled(satellite_gap(
            placement,
            cohort,
            format!("The satellite has no record of run {run_id} ({code})."),
        )));
    }
    let requested = cohort.first().and_then(|w| w.requested_at);
    if requested
        .is_some_and(|at| now.signed_duration_since(at).num_minutes() < SATELLITE_RUN_DEADLINE_MINS)
    {
        tracing::warn!(%run_id, code, "bounded library QA satellite poll deferred");
        return Ok(SatellitePoll::Pending);
    }
    Ok(SatellitePoll::Settled(satellite_gap(
        placement,
        cohort,
        format!(
            "The satellite gave no result within {SATELLITE_RUN_DEADLINE_MINS} minutes of dispatch ({code})."
        ),
    )))
}

/// Whether a registered peer may take a full-library run (#2120).
enum Candidate {
    /// Not a project satellite for hub work: disabled, unpaired, dispatch off
    /// or no declared scope. Silent: not a reason to record.
    Ineligible,
    /// A project satellite that does not qualify now (stale observation).
    Rejected(&'static str),
    Ready,
}

fn candidate(store: &Store, value: &SatellitePeerV1, now: DateTime<Utc>) -> Result<Candidate> {
    let config = &value.config;
    if !(config.enabled
        && config.read_enabled
        && config.dispatch_enabled
        && config.expected_installation_id.is_some())
        || !store.hub_peer_declares_scope(config.peer_id.0)?
    {
        return Ok(Candidate::Ineligible);
    }
    let fresh = value.observation.as_ref().is_some_and(|observation| {
        !observation.stale
            && observation.state == "healthy"
            && observation.last_observed_at.is_some_and(|at| {
                now.signed_duration_since(at).num_seconds() <= SATELLITE_QA_HEALTH_MAX_AGE_SECS
            })
    });
    if !fresh {
        return Ok(Candidate::Rejected(SATELLITE_QA_STALE));
    }
    Ok(Candidate::Ready)
}

/// The project satellites that may take a run, in registry order, and why the
/// others do not (`label: code`).
async fn satellite_candidates(
    store: &Arc<tokio::sync::Mutex<Store>>,
    now: DateTime<Utc>,
) -> Result<(Vec<RegistryPeer>, Vec<String>)> {
    with_store(store, move |store| {
        let view = store.satellite_registry_view()?;
        let (mut ready, mut rejected) = (Vec::new(), Vec::new());
        for value in &view.peers {
            match candidate(store, value, now)? {
                Candidate::Ineligible => {}
                Candidate::Rejected(code) => {
                    rejected.push(peer_rejection(
                        value.config.peer_id.0,
                        &value.config.label,
                        code,
                    ));
                }
                Candidate::Ready => ready.push(crate::satellite::hub::registry_peer(value)),
            }
        }
        Ok((ready, rejected))
    })
    .await
}

/// Where a new run goes: the first project satellite that accepts it, else
/// the hub with every reason no satellite took it. #180: a satellite whose
/// start reply was lost (`receipt: None`) keeps the run; nothing else is
/// tried, since it may already be running there.
enum Placed {
    Satellite {
        peer: RegistryPeer,
        receipt: Option<SatelliteQaRunReceiptV1>,
    },
    Hub {
        reason: String,
    },
    /// #2176: no satellite took it and a peer's deploy is pending: the run
    /// waits for that deploy (nothing is bound) instead of loading the hub.
    Wait {
        reason: String,
    },
}

fn bounded_reason(reasons: &[String]) -> String {
    let joined = if reasons.is_empty() {
        SATELLITE_QA_NO_PEER.to_owned()
    } else {
        reasons.join("; ")
    };
    let mut reason = String::new();
    for ch in joined.chars().filter(|ch| !ch.is_control()) {
        if reason.len() + ch.len_utf8() > MAX_REASON_BYTES {
            break;
        }
        reason.push(ch);
    }
    reason
}

/// Include the observed platform beside QA rejection codes, without storing
/// platform in the database. A live handshake refreshes it before admission.
fn peer_rejection(peer: Uuid, label: &str, code: &str) -> String {
    let platform = crate::satellite::reported_platform(peer)
        .map(|platform| format!("{}/{}", platform.os, platform.arch))
        .unwrap_or_else(|| "unknown".into());
    format!("{label}: {code} (platform: {platform})")
}

/// Place `cohort`'s run `run_id` of `sha`.
///
/// #2155: the peer, run id and commit are recorded before each start is sent,
/// so a hub that stops after the satellite accepted it finds the intent again
/// ([`recover_dispatches`]) and reconciles that exact peer instead of placing
/// the obligation a second time. The intent is withdrawn only on a refusal
/// that proves the satellite started nothing.
///
/// #2176: a peer whose deploy is pending is skipped (and not asked again
/// within the pace); when no other peer takes the run it waits for it.
async fn place(
    store: &Arc<tokio::sync::Mutex<Store>>,
    satellites: &mut SatelliteQa,
    run_id: Uuid,
    sha: &str,
    cohort: &[BoundedQaLanding],
    now: DateTime<Utc>,
) -> Result<Placed> {
    let (ready, mut reasons) = satellite_candidates(store, now).await?;
    if ready.is_empty() {
        return Ok(Placed::Hub {
            reason: bounded_reason(&reasons),
        });
    }
    let hub = with_store(store, Store::satellite_installation_id).await?;
    let mut deploying = false;
    for peer in ready {
        if satellites.deploy_pending(peer.id) {
            deploying = true;
            reasons.push(peer_rejection(
                peer.id,
                &peer.label,
                SATELLITE_QA_DEPLOY_PENDING,
            ));
            continue;
        }
        let wire = SatelliteQaRunRequestV1 {
            wire_version: SATELLITE_WIRE_VERSION_V1,
            hub_installation_id: SatelliteUuidV1(hub),
            run_id,
            sha: sha.to_owned(),
            start: true,
        };
        let (landings, planned, label, peer_id) =
            (cohort.to_vec(), sha.to_owned(), peer.label.clone(), peer.id);
        with_store(store, move |store| {
            store.begin_bounded_qa_dispatch(&landings, run_id, &planned, peer_id, &label, now)
        })
        .await?;
        match crate::satellite::qa_link::request_qa_run(&satellites.root, &peer, &wire, true).await
        {
            Ok(receipt) => {
                return Ok(Placed::Satellite {
                    peer,
                    receipt: Some(receipt),
                });
            }
            Err(QaLinkError::Uncertain(code)) => {
                tracing::warn!(%run_id, peer = %peer.label, code, "bounded library QA dispatch unconfirmed; the run stays on this satellite");
                return Ok(Placed::Satellite {
                    peer,
                    receipt: None,
                });
            }
            Err(QaLinkError::NoEffect(code)) => {
                let landings = cohort.to_vec();
                with_store(store, move |store| {
                    store.withdraw_bounded_qa_dispatch(&landings, run_id, peer_id, now)
                })
                .await?;
                if code == SATELLITE_QA_DEPLOY_PENDING {
                    deploying = true;
                    satellites
                        .deploying
                        .insert(peer_id, std::time::Instant::now());
                }
                reasons.push(peer_rejection(peer.id, &peer.label, code));
            }
        }
    }
    let reason = bounded_reason(&reasons);
    Ok(if deploying {
        Placed::Wait { reason }
    } else {
        Placed::Hub { reason }
    })
}

/// One planned start: the anchor landing, its repository, the exact commit
/// to test, the project's pending landings and those it covers.
struct Plan {
    anchor: BoundedQaLanding,
    repo: PathBuf,
    sha: String,
    pending: Vec<BoundedQaLanding>,
    cohort: Vec<BoundedQaLanding>,
}

fn full_commit(repo: &Path, rev: &str) -> Option<String> {
    let resolved = git(
        repo,
        &["rev-parse", "--verify", &format!("{rev}^{{commit}}")],
    )?;
    Some(resolved.trim().to_owned())
}

/// Plan `anchor`'s start (git reads only; no snapshot yet).
fn plan_start(
    launcher: &LanderLauncher,
    work: &[BoundedQaLanding],
    anchor: &BoundedQaLanding,
) -> Option<Plan> {
    let repo = repository(launcher, anchor)?;
    let pending: Vec<_> = work
        .iter()
        .filter(|w| w.job_id.is_none() && w.project_id == anchor.project_id)
        .cloned()
        .collect();
    let newest = pending
        .iter()
        .rev()
        .find(|w| source_commit_exists(&repo, &w.entry.source_commit))?;
    let landed = newest.entry.outcome.as_ref()?.landed_sha.as_deref()?;
    // A later rolling tip can cover several pending landings. Use it only
    // when it provably contains the newest recorded publication.
    let tip = full_commit(&repo, "origin/rolling")
        .filter(|tip| git(&repo, &["merge-base", "--is-ancestor", landed, tip]).is_some());
    let sha = match tip {
        Some(tip) => tip,
        None => full_commit(&repo, landed)?,
    };
    let cohort: Vec<_> = pending
        .iter()
        .filter(|w| covers(&repo, &sha, w))
        .cloned()
        .collect();
    if cohort.is_empty() {
        return None;
    }
    Some(Plan {
        anchor: anchor.clone(),
        repo,
        sha,
        pending,
        cohort,
    })
}

/// The job identity of the `attempt`th run for an obligation anchor. The first
/// keeps its original key; a run re-armed after a capacity deferral (#2149)
/// needs its own, or the idempotent submit would replay the failed job.
fn attempt_key(prefix: &str, anchor: Uuid, attempt: u32) -> String {
    match attempt {
        0 => format!("{prefix}:{anchor}"),
        n => format!("{prefix}:{anchor}:{n}"),
    }
}

/// The run id a satellite attempt is replayed under (stable for a retry or a
/// restart of the same attempt).
fn satellite_run_id(anchor: Uuid, attempt: u32) -> Uuid {
    match attempt {
        0 => Uuid::new_v5(&anchor, b"bounded-library-qa-satellite"),
        n => Uuid::new_v5(
            &anchor,
            format!("bounded-library-qa-satellite:{n}").as_bytes(),
        ),
    }
}

/// The hub fallback: pin the snapshot, submit the recipe job and bind the
/// covered landings, recording `ran_on: hub` and why.
#[allow(clippy::too_many_arguments)]
async fn start_on_hub(
    store: &Arc<tokio::sync::Mutex<Store>>,
    runtime: Arc<dyn JobRuntime>,
    tools: &JobTools,
    jobs: &Path,
    plan: Plan,
    reason: String,
    attempt: u32,
    now: DateTime<Utc>,
) -> Result<()> {
    let root = jobs.join(SNAPSHOTS).join(plan.anchor.entry.id.to_string());
    let (repo, sha, pending) = (plan.repo, plan.sha, plan.pending);
    let (cwd, pinned, cohort) = tokio::task::spawn_blocking(move || -> Result<_> {
        let cwd = snapshot(&repo, &sha, &root)?;
        // A snapshot kept from before a restart keeps its original commit.
        let pinned = head_commit(&cwd)
            .ok_or_else(|| DaemonError::Process("QA snapshot has no HEAD".into()))?;
        let cohort: Vec<_> = pending
            .into_iter()
            .filter(|w| covers(&cwd, &pinned, w))
            .collect();
        Ok((cwd, pinned, cohort))
    })
    .await
    .map_err(|e| DaemonError::Process(e.to_string()))??;
    if cohort.is_empty() {
        return Ok(());
    }
    let anchor = &plan.anchor;
    let ctx = SubmitContext {
        owner: anchor.entry.source_session_id,
        project_id: anchor.project_id,
        cwd,
        name: Some(format!(
            "Full library QA for landing {}",
            &anchor.entry.source_commit[..8]
        )),
        params: full_library_params(),
        idempotency_key: Some(attempt_key("bounded-library-qa", anchor.entry.id, attempt)),
        wake: JobWake::None,
    };
    let placement = BoundedQaPlacement::Hub { reason };
    let (tools, jobs) = (tools.clone(), jobs.to_path_buf());
    with_store(store, move |store| {
        let (job, _) = agent_jobs::submit(store, &*runtime, &tools, &jobs, ctx, now)?;
        store.request_bounded_qa(&cohort, job.job.id, &pinned, &placement, now)?;
        tracing::info!(job_id = %job.job.id, sha = %pinned, landings = cohort.len(), ?placement, "bounded library QA dispatched");
        Ok(())
    })
    .await
}

/// The landings a replayed run's commit covers, when the satellite pinned
/// another commit than planned and it covers at least one; else `None`.
async fn replayed_cohort(
    plan: &Plan,
    receipt: Option<&SatelliteQaRunReceiptV1>,
) -> Result<Option<(Vec<BoundedQaLanding>, String)>> {
    let Some(pinned) = receipt
        .map(|receipt| receipt.sha.clone())
        .filter(|pinned| *pinned != plan.sha)
    else {
        return Ok(None);
    };
    let (repo, pending, at) = (plan.repo.clone(), plan.pending.clone(), pinned.clone());
    let covered = tokio::task::spawn_blocking(move || {
        pending
            .into_iter()
            .filter(|w| covers(&repo, &at, w))
            .collect::<Vec<_>>()
    })
    .await
    .map_err(|e| DaemonError::Process(e.to_string()))?;
    Ok((!covered.is_empty()).then_some((covered, pinned)))
}

/// Bind the run a satellite holds. A replayed run may pin another commit than
/// planned: the cohort follows the commit when it covers pending landings;
/// otherwise the planned landings stay bound to the planned commit and the
/// settlement reports the mismatch as incomplete coverage, rather than a
/// second full run starting elsewhere. An unconfirmed dispatch (#180, no
/// receipt) binds the planned landings with the satellite job unknown.
async fn bind_satellite(
    store: &Arc<tokio::sync::Mutex<Store>>,
    satellites: &mut SatelliteQa,
    plan: &Plan,
    run_id: Uuid,
    peer: &RegistryPeer,
    receipt: Option<&SatelliteQaRunReceiptV1>,
    now: DateTime<Utc>,
) -> Result<()> {
    let (cohort, sha) = replayed_cohort(plan, receipt)
        .await?
        .unwrap_or_else(|| (plan.cohort.clone(), plan.sha.clone()));
    let placement = BoundedQaPlacement::Satellite {
        peer_id: peer.id,
        peer_label: peer.label.clone(),
        remote_job_id: receipt.map(|receipt| receipt.job_id),
    };
    satellites.poll_due(run_id);
    with_store(store, move |store| {
        store.request_bounded_qa(&cohort, run_id, &sha, &placement, now)?;
        tracing::info!(%run_id, %sha, landings = cohort.len(), ?placement, "bounded library QA dispatched to a satellite");
        Ok(())
    })
    .await
}

/// #2155: a satellite start whose intent was recorded but whose binding never
/// was (the hub stopped after the satellite accepted the start, or the binding
/// write failed). The satellite may be running it, so the obligation is bound
/// to that exact peer and run id with the satellite job unknown (as for a lost
/// reply, #180): the poll then replays the idempotent start on that peer
/// without a fresh-placement admission check (the original run makes it
/// busy), and nothing is placed on another peer or the hub while the
/// attempt is unresolved. A peer that never answers settles incomplete at
/// the usual deadline, counted from this binding.
async fn recover_dispatches(
    store: &Arc<tokio::sync::Mutex<Store>>,
    work: Vec<BoundedQaLanding>,
    now: DateTime<Utc>,
) -> Result<Vec<BoundedQaLanding>> {
    let mut unresolved: Vec<BoundedQaIntent> = Vec::new();
    for landing in work.iter().filter(|w| w.job_id.is_none()) {
        if let Some(intent) = &landing.intent
            && !unresolved.iter().any(|known| known.run_id == intent.run_id)
        {
            unresolved.push(intent.clone());
        }
    }
    if unresolved.is_empty() {
        return Ok(work);
    }
    for intent in unresolved {
        let cohort: Vec<_> = work
            .iter()
            .filter(|w| {
                w.job_id.is_none()
                    && w.intent
                        .as_ref()
                        .is_some_and(|held| held.run_id == intent.run_id)
            })
            .cloned()
            .collect();
        let placement = BoundedQaPlacement::Satellite {
            peer_id: intent.peer_id,
            peer_label: intent.peer_label.clone(),
            remote_job_id: None,
        };
        tracing::warn!(
            run_id = %intent.run_id,
            peer = %intent.peer_label,
            landings = cohort.len(),
            "bounded library QA start recorded but never bound; reconciling it on that satellite"
        );
        with_store(store, move |store| {
            store.request_bounded_qa(&cohort, intent.run_id, &intent.sha, &placement, now)
        })
        .await?;
    }
    with_store(store, Store::bounded_qa_landings).await
}

/// Start one planned run: on a project satellite when one accepts it (or may
/// have, #180), else on the hub.
#[allow(clippy::too_many_arguments)]
async fn start(
    store: &Arc<tokio::sync::Mutex<Store>>,
    runtime: Arc<dyn JobRuntime>,
    tools: &JobTools,
    jobs: &Path,
    satellites: &mut SatelliteQa,
    plan: Plan,
    now: DateTime<Utc>,
) -> Result<()> {
    // Stable per obligation attempt: a retry or a hub restart reaches the same
    // run; a run re-armed after a capacity deferral (#2149) is a new attempt.
    let anchor = plan.anchor.entry.id;
    let attempt = with_store(store, move |store| {
        store.bounded_qa_capacity_deferrals(anchor)
    })
    .await?;
    let run_id = satellite_run_id(anchor, attempt);
    match place(store, satellites, run_id, &plan.sha, &plan.cohort, now).await? {
        Placed::Satellite { peer, receipt } => {
            bind_satellite(
                store,
                satellites,
                &plan,
                run_id,
                &peer,
                receipt.as_ref(),
                now,
            )
            .await
        }
        Placed::Hub { reason } => {
            start_on_hub(store, runtime, tools, jobs, plan, reason, attempt, now).await
        }
        Placed::Wait { reason } => {
            tracing::debug!(%run_id, reason, "bounded library QA waits for a satellite deploy");
            Ok(())
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn drive_once(
    store: &Arc<tokio::sync::Mutex<Store>>,
    launcher: &LanderLauncher,
    runtime: Arc<dyn JobRuntime>,
    tools: &JobTools,
    jobs: &Path,
    satellites: &mut SatelliteQa,
    hold: Option<Hold>,
    min_interval: chrono::Duration,
    now: DateTime<Utc>,
) -> Result<()> {
    let work = with_store(store, Store::bounded_qa_landings).await?;
    let work = recover_dispatches(store, work, now).await?;
    // #2394: a host-load hold ends once a pending landing has waited this long.
    let starved = work.iter().any(|w| {
        w.job_id.is_none() && pending_for(w, now) >= chrono::Duration::hours(HOST_LOAD_BYPASS_HOURS)
    });
    let hold = match hold {
        Some(Hold::HostLoad) if starved => {
            tracing::warn!(
                "bounded library QA bypasses the host-load hold for a starved obligation"
            );
            None
        }
        other => other,
    };
    // Detect an outage/admission stall without silently losing the obligation.
    // Keep it pending; deterministic Issue creation in the store dedupes wakes.
    // A landing bound to a run is not stalled: that run settles (or defers)
    // through its own deadline, so it is never reported here (#2394).
    for landing in work.iter().filter(|w| w.job_id.is_none()) {
        if pending_for(landing, now) < chrono::Duration::hours(COVERAGE_DEADLINE_HOURS) {
            continue;
        }
        let evidence = format!(
            "The full-library attempt is overdue: eight hours since publication. The obligation remains pending (current hold: {}) and will resume automatically after admission recovers.",
            hold.map_or(
                "none; awaiting placement or the minimum interval",
                Hold::code
            ),
        );
        if let Some(issue) = failure_issue(
            std::slice::from_ref(landing),
            "pending",
            &evidence,
            QaFailureKind::Incomplete,
        ) {
            let landing = landing.clone();
            with_store(store, move |store| {
                store.report_bounded_qa_overdue(&landing, &issue, now)
            })
            .await?;
        }
    }
    // #2394: audit why each project's oldest waiting landing is not starting.
    let mut audited = HashSet::new();
    for landing in work.iter().filter(|w| w.job_id.is_none()) {
        if !audited.insert(landing.project_id)
            || pending_for(landing, now) < chrono::Duration::minutes(HOLD_AUDIT_AFTER_MINS)
        {
            continue;
        }
        if let Some(hold) = hold {
            let landing = landing.clone();
            with_store(store, move |store| {
                store.note_bounded_qa_hold(&landing, hold.code(), now)
            })
            .await?;
        }
    }
    let mut finished = HashSet::new();
    let mut active = Vec::new();
    // #2120: a satellite run in flight holds its project like a hub job.
    let mut busy_projects = HashSet::new();
    for landing in &work {
        let Some(job_id) = landing.job_id else {
            continue;
        };
        if !finished.insert(job_id) {
            continue;
        }
        let cohort: Vec<_> = work
            .iter()
            .filter(|w| w.job_id == Some(job_id))
            .cloned()
            .collect();
        let Some(placement @ BoundedQaPlacement::Satellite { .. }) = &landing.placement else {
            if let Some(cwd) = reconcile_hub_job(store, jobs, job_id, cohort, now).await? {
                active.push(cwd);
            }
            continue;
        };
        match poll_satellite(store, satellites, job_id, placement, &cohort, now).await? {
            SatellitePoll::Pending => {
                busy_projects.insert(landing.project_id);
            }
            SatellitePoll::Settled(settlement) => {
                satellites.polled.remove(&job_id);
                settle(store, cohort, job_id, settlement, now).await?;
            }
        }
    }
    // #2112: a project's next start waits out the minimum interval since its
    // previous dispatch; its pending landings coalesce into that next run.
    let mut waiting = HashSet::new();
    if hold.is_none() {
        let projects: HashSet<_> = work
            .iter()
            .filter(|w| w.job_id.is_none())
            .map(|w| w.project_id)
            .collect();
        for project in projects {
            let last = with_store(store, move |s| s.last_bounded_qa_dispatch(project)).await?;
            if last.is_some_and(|last| now.signed_duration_since(last) < min_interval) {
                waiting.insert(project);
            }
        }
    }
    // Git reads never hold the Store mutex.
    let (planned_work, planned_launcher) = (work.clone(), launcher.clone());
    let plans = tokio::task::spawn_blocking(move || {
        let mut assigned = HashSet::new();
        let mut plans = Vec::new();
        for anchor in planned_work.iter().filter(|w| w.job_id.is_none()) {
            if assigned.contains(&anchor.entry.id) {
                continue;
            }
            // No recurring full run per landing: arrivals while a snapshot is
            // running wait for one coalesced descendant snapshot at its end.
            if hold.is_some()
                || waiting.contains(&anchor.project_id)
                || busy_projects.contains(&anchor.project_id)
                || active
                    .iter()
                    .any(|repo| source_commit_exists(repo, &anchor.entry.source_commit))
            {
                continue;
            }
            let Some(plan) = plan_start(&planned_launcher, &planned_work, anchor) else {
                continue;
            };
            assigned.extend(plan.cohort.iter().map(|w| w.entry.id));
            busy_projects.insert(anchor.project_id);
            active.push(plan.repo.clone());
            plans.push(plan);
        }
        plans
    })
    .await
    .map_err(|e| DaemonError::Process(e.to_string()))?;
    for plan in plans {
        start(
            store,
            Arc::clone(&runtime),
            tools,
            jobs,
            satellites,
            plan,
            now,
        )
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::rolling_queue::RollingQueueBinding;
    use rsi_common::satellite_qa::{
        GIB_BYTES, SATELLITE_QA_BUSY, SATELLITE_QA_DISK_LOW, SATELLITE_QA_MIN_DISK_FREE_BYTES,
        SatelliteQaAdmissionV1,
    };

    fn commit(repo: &Path, value: &str) -> String {
        std::fs::write(repo.join("value"), value).unwrap();
        git(repo, &["add", "value"]).unwrap();
        git(
            repo,
            &[
                "-c",
                "user.name=qa-fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-m",
                value,
            ],
        )
        .unwrap();
        head_commit(repo).unwrap()
    }

    fn landing(source: &str, published: &str) -> BoundedQaLanding {
        BoundedQaLanding {
            entry: RollingQueueEntryV1 {
                id: Uuid::new_v4(),
                sequence: 1,
                source_commit: source.into(),
                source_session_id: Uuid::new_v4(),
                owner_epic_id: None,
                binding: RollingQueueBinding::Unbound,
                work_key: None,
                migration_version: None,
                hot_files: vec![],
                test_filters: vec![],
                fixes_tests: Vec::new(),
                filters_state: None,
                state: RollingQueueEntryState::Published,
                outcome: Some(RollingQueueOutcome {
                    landed_sha: Some(published.into()),
                    ..Default::default()
                }),
                enqueued_at: Utc::now().to_rfc3339(),
                finished_at: Some(Utc::now().to_rfc3339()),
            },
            batch_id: Uuid::new_v4(),
            project_id: Some(Uuid::new_v4()),
            repo_path: String::new(),
            job_id: None,
            qa_sha: None,
            placement: None,
            requested_at: None,
            intent: None,
        }
    }

    fn hub() -> BoundedQaPlacement {
        BoundedQaPlacement::Hub {
            reason: SATELLITE_QA_NO_PEER.into(),
        }
    }

    struct NoLaunch;
    impl JobRuntime for NoLaunch {
        fn launch(&self, _: &agent_jobs::LaunchSpec) -> std::result::Result<(), String> {
            panic!("receipt reconciliation must not relaunch")
        }
        fn unit_active(&self, _: &str) -> bool {
            false
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn bounded_qa_restart_reconciles_a_red_job_and_files_its_landing_issue_once() {
        let issue = reconcile_failed_job(None).await;
        assert!(issue.title.starts_with("Full library QA test red"));
        assert_eq!(issue.labels, ["qa", "bug"]);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn bounded_qa_timeout_files_one_incomplete_coverage_issue_on_restart() {
        let issue = reconcile_failed_job(Some(rsi_common::agent_jobs::JOB_TIMED_OUT)).await;
        assert!(
            issue
                .title
                .starts_with("Full library QA incomplete coverage")
        );
        assert_eq!(issue.labels, ["qa", "coverage-incomplete"]);
        assert!(issue.body.contains(rsi_common::agent_jobs::JOB_TIMED_OUT));
    }

    /// One published landing bound to a hub job that settled `Failed` with
    /// `refusal` (and a failing test), not yet reconciled by the loop.
    struct FailedJob {
        store: Arc<tokio::sync::Mutex<Store>>,
        temp: tempfile::TempDir,
        entry: Uuid,
        job: Uuid,
        sha: String,
        now: DateTime<Utc>,
    }

    impl FailedJob {
        /// One loop tick at `at` (held: reconcile only, never dispatch).
        async fn tick(&self, at: DateTime<Utc>) {
            let launcher = LanderLauncher::new(PathBuf::from("unused"));
            let tools = JobTools {
                cargo_slot: "unused".into(),
                lander: "unused".into(),
            };
            let mut satellites = SatelliteQa::new(self.temp.path().join("satellites"));
            drive_once(
                &self.store,
                &launcher,
                Arc::new(NoLaunch),
                &tools,
                self.temp.path(),
                &mut satellites,
                Some(Hold::Drain),
                chrono::Duration::zero(),
                at,
            )
            .await
            .unwrap();
        }
    }

    async fn reconcile_failed_job(refusal: Option<&str>) -> rsi_common::types::Issue {
        let failed = failed_job(refusal);
        for _ in 0..2 {
            failed.tick(failed.now).await;
        }
        let store = failed.store.lock().await;
        let issue = store
            .get_issue(Uuid::new_v5(&failed.job, b"bounded-library-qa-red"))
            .unwrap()
            .unwrap();
        assert!(issue.body.contains(&failed.sha));
        assert!(issue.body.contains("test::red"));
        assert!(issue.body.contains(&failed.entry.to_string()));
        assert!(store.bounded_qa_landings().unwrap().is_empty());
        issue
    }

    fn failed_job(refusal: Option<&str>) -> FailedJob {
        use crate::store::agent_jobs::NewAgentJob;
        use crate::store::rolling_queue::NewQueueEntry;
        use rsi_common::agent_jobs::AgentJobResultV1;
        use rsi_common::types::Project;
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q"]).unwrap();
        let sha = commit(&repo, "source");
        let store = Store::open_in_memory().unwrap();
        let project = Uuid::new_v4();
        let owner = Uuid::new_v4();
        let now = Utc::now();
        store
            .insert_project(&Project {
                id: project,
                name: "qa-restart".into(),
                path: None,
                description: None,
                color: Project::DEFAULT_COLOR.into(),
                context_files: None,
                created_at: now,
                updated_at: now,
            })
            .unwrap();
        let entry = store
            .enqueue_rolling_queue_source(
                &NewQueueEntry {
                    project_id: Some(project),
                    repo_path: repo.display().to_string(),
                    source_commit: sha.clone(),
                    source_session_id: owner,
                    owner_epic_id: None,
                    binding: RollingQueueBinding::Unbound,
                    work_key: None,
                    migration_version: None,
                    hot_files: vec![],
                    test_filters: vec![],
                    fixes_tests: Vec::new(),
                    idempotency_key: "landing".into(),
                },
                now,
            )
            .unwrap()
            .0;
        store.claim_rolling_queue_batch(now, 1).unwrap().unwrap();
        store
            .settle_rolling_queue_entry(
                entry.id,
                RollingQueueEntryState::Published,
                &RollingQueueOutcome {
                    landed_sha: Some(sha.clone()),
                    detail: Some("selection=bounded selected=400 reached=7613".into()),
                    ..Default::default()
                },
                now,
            )
            .unwrap();
        let job = Uuid::new_v4();
        store
            .insert_agent_job(
                &NewAgentJob {
                    id: job,
                    owner_session_id: owner,
                    project_id: Some(project),
                    name: Some("QA".into()),
                    params: full_library_params(),
                    cwd: repo.display().to_string(),
                    unit_name: format!("qa-{job}"),
                    log_path: temp.path().join("qa.log").display().to_string(),
                    status_path: temp.path().join("qa.status").display().to_string(),
                    idempotency_key: Some("qa".into()),
                    wake: JobWake::None,
                },
                now,
            )
            .unwrap();
        store
            .request_bounded_qa(
                &store.bounded_qa_landings().unwrap(),
                job,
                &sha,
                &hub(),
                now,
            )
            .unwrap();
        store
            .settle_agent_job(
                job,
                JobState::Failed,
                &AgentJobResultV1 {
                    exit_code: Some(1),
                    // A timeout may include killed tests; the typed refusal wins.
                    refusal: refusal.map(str::to_owned),
                    failing_tests: vec!["test::red".into()],
                    ..Default::default()
                },
                false,
                now,
            )
            .unwrap();
        FailedJob {
            store: Arc::new(tokio::sync::Mutex::new(store)),
            temp,
            entry: entry.id,
            job,
            sha,
            now,
        }
    }

    /// #2149: a run that ended waiting for build capacity (`job_admission_timed_out`
    /// or `job_start_timed_out`) never ran tests. It files no Issue and no
    /// finished receipt: one `bounded_qa_capacity_deferred` audit event per
    /// landing, and the obligation is pending again for the next attempt.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_capacity_timeout_rearms_the_obligation_with_an_audit_event_and_no_issue() {
        for refusal in [JOB_ADMISSION_TIMED_OUT, JOB_START_TIMED_OUT] {
            let failed = failed_job(Some(refusal));
            // The restart replay settles it once.
            for _ in 0..2 {
                failed.tick(failed.now).await;
            }
            let store = failed.store.lock().await;
            assert!(
                store
                    .get_issue(Uuid::new_v5(&failed.job, b"bounded-library-qa-red"))
                    .unwrap()
                    .is_none(),
                "{refusal}"
            );
            let pending = store.bounded_qa_landings().unwrap();
            assert_eq!(pending.len(), 1, "{refusal}: still owed");
            assert_eq!(pending[0].entry.id, failed.entry);
            assert!(pending[0].job_id.is_none(), "{refusal}: free for a new run");
            assert_eq!(
                store.bounded_qa_capacity_deferrals(failed.entry).unwrap(),
                1,
                "{refusal}: one audit event, however often it is replayed"
            );
            // The next attempt is a new job and satellite run, not a replay of this one.
            let attempt = store.bounded_qa_capacity_deferrals(failed.entry).unwrap();
            assert_ne!(
                attempt_key("bounded-library-qa", failed.entry, 0),
                attempt_key("bounded-library-qa", failed.entry, attempt)
            );
            assert_ne!(
                satellite_run_id(failed.entry, 0),
                satellite_run_id(failed.entry, attempt)
            );
            // A new run binds the landing again, and a second deferral is its own event.
            let second = Uuid::new_v4();
            store
                .request_bounded_qa(&pending, second, &failed.sha, &hub(), failed.now)
                .unwrap();
            let rebound = store.bounded_qa_landings().unwrap();
            assert_eq!(rebound[0].job_id, Some(second), "{refusal}");
            store
                .defer_bounded_qa(
                    &rebound,
                    second,
                    &failed.sha,
                    refusal,
                    &BoundedQaRun::default(),
                    failed.now,
                )
                .unwrap();
            assert_eq!(
                store.bounded_qa_capacity_deferrals(failed.entry).unwrap(),
                2
            );
            assert!(store.bounded_qa_landings().unwrap()[0].job_id.is_none());
        }
    }

    /// #2149: other incomplete outcomes (a plain timeout) and test failures are
    /// unchanged: they still file their Issue and finish the obligation.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn only_capacity_refusals_defer_and_test_failures_keep_their_issue() {
        let red = vec!["test::red".to_owned()];
        for refusal in [JOB_ADMISSION_TIMED_OUT, JOB_START_TIMED_OUT] {
            // Typed capacity wins even over names a killed run reported.
            assert!(matches!(
                failure_kind(JobState::Failed, Some(refusal), &red),
                QaFailureKind::Capacity(code) if code == refusal
            ));
        }
        assert!(matches!(
            failure_kind(JobState::Failed, None, &red),
            QaFailureKind::TestRed
        ));
        // #2176: a deploy stopped it; the typed cause wins over killed names.
        assert!(matches!(
            failure_kind(JobState::Failed, Some(JOB_INTERRUPTED_BY_DEPLOY), &red),
            QaFailureKind::Capacity(DEPLOY_PREEMPTED)
        ));
        assert!(matches!(
            failure_kind(
                JobState::Failed,
                Some(rsi_common::agent_jobs::JOB_TIMED_OUT),
                &[]
            ),
            QaFailureKind::Incomplete
        ));
        let landing = landing(&"a".repeat(40), &"b".repeat(40));
        assert!(
            failure_issue(
                std::slice::from_ref(&landing),
                "sha",
                "evidence",
                QaFailureKind::Capacity(JOB_ADMISSION_TIMED_OUT)
            )
            .is_none()
        );
    }

    /// Hub #3054: the evidence names every test, or only those the store says
    /// no open `qa` Issue tracks, with the omitted count and refusal kept.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn evidence_renders_only_the_tests_it_is_asked_to_name() {
        let evidence = Evidence {
            head: "Ran on the hub.\nJob 1, state Failed, exit Some(1); log /x".into(),
            failing: vec!["a::new".into(), "a::tracked".into()],
            outcomes: vec![
                "FAIL rsid::bin/rsid a::new".into(),
                "FAIL rsid::bin/rsid a::tracked".into(),
            ],
            omitted: " (and 2 more)".into(),
            refusal: Some("none".into()),
        };
        assert_eq!(
            evidence.render(None),
            "Ran on the hub.\nJob 1, state Failed, exit Some(1); log /x\nFailing tests: a::new, a::tracked (and 2 more)\nFailing outcomes: FAIL rsid::bin/rsid a::new, FAIL rsid::bin/rsid a::tracked (and 2 more)\nRefusal: none"
        );
        let narrowed = evidence.render(Some(&["a::new".to_string()]));
        assert!(
            narrowed.contains("Failing tests: a::new (and 2 more)\n"),
            "{narrowed}"
        );
        assert!(!narrowed.contains("tracked"), "{narrowed}");
        assert_eq!(Evidence::plain("gap".into()).render(Some(&[])), "gap");
    }

    /// #2172: the Issue's `Failing tests:` line is one clean test path per
    /// test, whatever shape the run reported (nextest's progress counter and
    /// binary id, the bare echo of the same failure), so `fixes_issue` (#2167)
    /// reads every name back.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_issue_failing_tests_line_is_one_clean_path_per_test() {
        let reported: Vec<String> = [
            "_5774/8220_ rsid::bin/rsid tests::startup_real_daemon",
            "_5938/8220_ rsid-store model_control::call_control::tests::abandon",
            "model_control::call_control::tests::abandon",
            "tests::startup_real_daemon",
        ]
        .map(String::from)
        .into();
        let placement = BoundedQaPlacement::Satellite {
            peer_id: Uuid::new_v4(),
            peer_label: "laptop".into(),
            remote_job_id: None,
        };
        let receipt = SatelliteQaRunReceiptV1 {
            run_id: Uuid::new_v4(),
            sha: "a".repeat(40),
            job_id: Uuid::new_v4(),
            state: JobState::Failed,
            exit_code: Some(2),
            refusal: None,
            failing_tests: reported,
            failing_tests_omitted: 0,
            log_path: "/laptop/jobs/1.log".into(),
            replayed: false,
        };
        let settled = satellite_settlement(receipt, &placement, &[]);
        let expected = [
            "model_control::call_control::tests::abandon".to_string(),
            "tests::startup_real_daemon".into(),
        ];
        assert_eq!(settled.run.failing_tests, expected);
        assert!(matches!(settled.kind, QaFailureKind::TestRed));
        let evidence = settled.evidence.render(None);
        let line = evidence
            .lines()
            .find(|line| line.starts_with("Failing tests: "))
            .unwrap();
        assert_eq!(line, format!("Failing tests: {}", expected.join(", ")));
        assert_eq!(
            rsi_common::rolling_queue::failing_tests_listed(&settled.evidence.render(None)),
            expected
        );
    }

    /// #185: a timeout reaches the QA Issue as its test path like a failure, the
    /// canonical `Failing tests:` line stays path-only, and a separate
    /// `Failing outcomes:` line keeps each outcome and binary: the same test
    /// path in two binaries stays two entries and the TIMEOUT class survives.
    /// The failures the run had beyond its list are said, not hidden.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_timeout_and_the_omitted_count_reach_the_issue_failing_tests_line() {
        let placement = BoundedQaPlacement::Satellite {
            peer_id: Uuid::new_v4(),
            peer_label: "laptop".into(),
            remote_job_id: None,
        };
        let receipt = SatelliteQaRunReceiptV1 {
            run_id: Uuid::new_v4(),
            sha: "a".repeat(40),
            job_id: Uuid::new_v4(),
            state: JobState::Failed,
            exit_code: Some(2),
            refusal: None,
            failing_tests: [
                "TIMEOUT rsid-store store::tests::slow",
                "TIMEOUT rsid::bin/rsid tests::same_name",
                "rsid-store tests::same_name",
                "rsid session::red",
            ]
            .map(String::from)
            .into(),
            failing_tests_omitted: 3,
            log_path: "/laptop/jobs/1.log".into(),
            replayed: false,
        };
        let settled = satellite_settlement(receipt, &placement, &[]);
        let paths = ["session::red", "store::tests::slow", "tests::same_name"];
        assert_eq!(settled.run.failing_tests, paths);
        assert!(settled.evidence.render(None).contains(
            "Failing tests: session::red, store::tests::slow, tests::same_name (and 3 more)\n"
        ));
        assert!(settled.evidence.render(None).contains(
            "Failing outcomes: FAIL rsid session::red, FAIL rsid-store tests::same_name, TIMEOUT rsid-store store::tests::slow, TIMEOUT rsid::bin/rsid tests::same_name (and 3 more)\n"
        ));
        // The claim parse reads only the canonical line.
        assert_eq!(
            rsi_common::rolling_queue::failing_tests_listed(&settled.evidence.render(None)),
            paths
        );
    }

    /// #2149: deferring is not forgiving. The eight-hour overdue rule still
    /// files its one Issue for an obligation that stays unmet.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_deferred_obligation_still_goes_overdue_after_eight_hours() {
        let failed = failed_job(Some(JOB_ADMISSION_TIMED_OUT));
        failed.tick(failed.now).await;
        let overdue = Uuid::new_v5(&failed.entry, b"bounded-library-qa-overdue");
        // Seven hours after publication nothing is overdue yet.
        failed.tick(failed.now + chrono::Duration::hours(7)).await;
        assert!(
            failed
                .store
                .lock()
                .await
                .get_issue(overdue)
                .unwrap()
                .is_none()
        );
        // Past eight hours it files once, however often the loop runs.
        for _ in 0..2 {
            failed.tick(failed.now + chrono::Duration::hours(9)).await;
        }
        let store = failed.store.lock().await;
        let issue = store
            .get_issue(overdue)
            .unwrap()
            .expect("the overdue Issue");
        assert!(
            issue
                .title
                .starts_with("Full library QA incomplete coverage")
        );
        assert!(issue.body.contains("overdue"));
        // The obligation itself stays pending for the next attempt.
        assert_eq!(store.bounded_qa_landings().unwrap().len(), 1);
    }

    /// One project's published landing of `sha`, obliged by the queue (#2112).
    fn obliged_landing(store: &Store, project: Uuid, repo: &Path, sha: &str, now: DateTime<Utc>) {
        use crate::store::rolling_queue::NewQueueEntry;
        let entry = store
            .enqueue_rolling_queue_source(
                &NewQueueEntry {
                    project_id: Some(project),
                    repo_path: repo.display().to_string(),
                    source_commit: sha.into(),
                    source_session_id: Uuid::new_v4(),
                    owner_epic_id: None,
                    binding: RollingQueueBinding::Unbound,
                    work_key: None,
                    migration_version: None,
                    hot_files: vec![],
                    test_filters: vec!["rsid=rolling_queue".into()],
                    fixes_tests: Vec::new(),
                    idempotency_key: sha.into(),
                },
                now,
            )
            .unwrap()
            .0;
        let batch = store.claim_rolling_queue_batch(now, 1).unwrap().unwrap();
        store
            .oblige_bounded_qa(batch.batch_id, &[entry.id], "derived_filters", now)
            .unwrap();
        store
            .settle_rolling_queue_entry(
                entry.id,
                RollingQueueEntryState::Published,
                &RollingQueueOutcome {
                    landed_sha: Some(sha.into()),
                    detail: Some("gate_timing=selection=closure reason=explicit_filters".into()),
                    ..Default::default()
                },
                now,
            )
            .unwrap();
    }

    /// #2112: a project's second full-library start waits out the minimum
    /// interval since its previous dispatch; the landings published meanwhile
    /// stay pending and coalesce into one start (one pinned snapshot) after it.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn the_interval_holds_a_second_start_and_its_landings_coalesce_into_one() {
        use rsi_common::types::Project;
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q"]).unwrap();
        let shas: Vec<String> = ["zero", "one", "two"]
            .iter()
            .map(|value| commit(&repo, value))
            .collect();
        let store = Store::open_in_memory().unwrap();
        let project = Uuid::new_v4();
        let now = Utc::now();
        store
            .insert_project(&Project {
                id: project,
                name: "qa-interval".into(),
                path: None,
                description: None,
                color: Project::DEFAULT_COLOR.into(),
                context_files: None,
                created_at: now,
                updated_at: now,
            })
            .unwrap();
        // The previous run started 30 minutes ago and has finished.
        let earlier = now - chrono::Duration::minutes(30);
        obliged_landing(&store, project, &repo, &shas[0], earlier);
        let previous = store.bounded_qa_landings().unwrap();
        let job = Uuid::new_v4();
        store
            .request_bounded_qa(&previous, job, &shas[0], &hub(), earlier)
            .unwrap();
        store
            .finish_bounded_qa(
                &previous,
                job,
                &shas[0],
                true,
                &BoundedQaRun::default(),
                None,
                earlier,
            )
            .unwrap();
        for sha in &shas[1..] {
            obliged_landing(&store, project, &repo, sha, now);
        }
        let store = Arc::new(tokio::sync::Mutex::new(store));
        let launcher = LanderLauncher::new(PathBuf::from("unused"));
        let tools = JobTools {
            cargo_slot: "unused".into(),
            lander: "unused".into(),
        };
        let jobs = temp.path().join("jobs");
        let snapshots = || {
            std::fs::read_dir(jobs.join("rolling-library-qa"))
                .map(|dir| dir.count())
                .unwrap_or(0)
        };
        let interval = chrono::Duration::minutes(60);
        let mut satellites = SatelliteQa::new(temp.path().join("satellites"));
        drive_once(
            &store,
            &launcher,
            Arc::new(NoLaunch),
            &tools,
            &jobs,
            &mut satellites,
            None,
            interval,
            now,
        )
        .await
        .unwrap();
        assert_eq!(snapshots(), 0, "held by the interval");
        let pending = store.lock().await.bounded_qa_landings().unwrap();
        assert_eq!(pending.len(), 2);
        assert!(pending.iter().all(|w| w.job_id.is_none()));
        // Past the interval both pending landings plan into ONE start. The
        // fixture declares no recipe, so the submit after the plan is refused.
        let later = now + chrono::Duration::minutes(31);
        let refused = drive_once(
            &store,
            &launcher,
            Arc::new(NoLaunch),
            &tools,
            &jobs,
            &mut satellites,
            None,
            interval,
            later,
        )
        .await;
        assert!(refused.is_err());
        assert_eq!(snapshots(), 1, "one coalesced start for both landings");
        let pinned = jobs
            .join("rolling-library-qa")
            .join(pending[0].entry.id.to_string())
            .join("source");
        assert_eq!(head_commit(&pinned).as_deref(), Some(shas[2].as_str()));
    }

    /// A project with one repository whose single obliged landing was
    /// published `age` ago (#2394 fixtures).
    fn aged_obligation(
        temp: &Path,
        age: chrono::Duration,
    ) -> (Arc<tokio::sync::Mutex<Store>>, PathBuf, DateTime<Utc>) {
        use rsi_common::types::Project;
        let repo = temp.join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q"]).unwrap();
        let sha = commit(&repo, "zero");
        let store = Store::open_in_memory().unwrap();
        let project = Uuid::new_v4();
        let now = Utc::now();
        store
            .insert_project(&Project {
                id: project,
                name: "qa-hold".into(),
                path: None,
                description: None,
                color: Project::DEFAULT_COLOR.into(),
                context_files: None,
                created_at: now,
                updated_at: now,
            })
            .unwrap();
        obliged_landing(&store, project, &repo, &sha, now - age);
        (Arc::new(tokio::sync::Mutex::new(store)), repo, now)
    }

    async fn drive_held(
        store: &Arc<tokio::sync::Mutex<Store>>,
        temp: &Path,
        hold: Option<Hold>,
        now: DateTime<Utc>,
    ) {
        let mut satellites = SatelliteQa::new(temp.join("satellites"));
        // The fixture declares no recipe, so a start that gets past the hold
        // is refused at its submit; the snapshot it pinned first is the proof.
        let _ = drive_once(
            store,
            &LanderLauncher::new(PathBuf::from("unused")),
            Arc::new(NoLaunch),
            &unused_tools(),
            &temp.join("jobs"),
            &mut satellites,
            hold,
            chrono::Duration::zero(),
            now,
        )
        .await;
    }

    /// #2394: a loaded host postpones a pending start with a typed, audited
    /// reason, and cannot starve it: once the landing has waited past the
    /// bypass the start goes ahead. A deploy drain is never bypassed.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_host_load_hold_is_audited_and_cannot_starve_a_pending_obligation() {
        let temp = tempfile::tempdir().unwrap();
        let (store, _repo, now) = aged_obligation(temp.path(), chrono::Duration::hours(2));
        let snapshots = || {
            std::fs::read_dir(temp.path().join("jobs").join("rolling-library-qa"))
                .map(|dir| dir.count())
                .unwrap_or(0)
        };
        for _ in 0..2 {
            drive_held(&store, temp.path(), Some(Hold::HostLoad), now).await;
        }
        assert_eq!(snapshots(), 0, "held while the host is loaded");
        let landing = store.lock().await.bounded_qa_landings().unwrap().remove(0);
        {
            let store = store.lock().await;
            assert!(
                !store
                    .note_bounded_qa_hold(&landing, "host_load", now)
                    .unwrap(),
                "the loop already recorded host_load once, however often it ticked"
            );
            assert!(
                store
                    .note_bounded_qa_hold(&landing, "deploy_drain", now)
                    .unwrap(),
                "a change of reason is a new receipt"
            );
        }
        // Four hours after publication a host-load hold no longer postpones...
        let starved = now + chrono::Duration::hours(2);
        drive_held(&store, temp.path(), Some(Hold::Drain), starved).await;
        assert_eq!(snapshots(), 0, "a deploy drain is never bypassed");
        drive_held(&store, temp.path(), Some(Hold::HostLoad), starved).await;
        assert_eq!(snapshots(), 1, "the starved obligation starts");
    }

    /// #2394: an obligation younger than the audit age records no hold
    /// receipt (that is the normal interval), and one overdue past eight
    /// hours names the typed hold in its Issue while an obligation already
    /// bound to a run files nothing.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn the_overdue_issue_names_the_hold_and_skips_a_bound_run() {
        let temp = tempfile::tempdir().unwrap();
        let (store, _repo, now) = aged_obligation(temp.path(), chrono::Duration::minutes(10));
        drive_held(&store, temp.path(), Some(Hold::Drain), now).await;
        let landing = store.lock().await.bounded_qa_landings().unwrap().remove(0);
        assert!(
            store
                .lock()
                .await
                .note_bounded_qa_hold(&landing, "deploy_drain", now)
                .unwrap(),
            "no receipt for a landing younger than an hour"
        );
        let overdue = Uuid::new_v5(&landing.entry.id, b"bounded-library-qa-overdue");
        // Bound to a run in flight: nothing is overdue however old.
        let day = now + chrono::Duration::hours(24);
        {
            let guard = store.lock().await;
            guard
                .request_bounded_qa(
                    std::slice::from_ref(&landing),
                    Uuid::new_v4(),
                    &landing.entry.source_commit,
                    &hub(),
                    now,
                )
                .unwrap();
        }
        drive_held(&store, temp.path(), Some(Hold::Drain), day).await;
        assert!(store.lock().await.get_issue(overdue).unwrap().is_none());
    }

    /// #2394: a run a deploy stopped ran no tests; it does not make the next
    /// start wait out the minimum interval.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_deploy_preempted_run_does_not_count_toward_the_interval() {
        let temp = tempfile::tempdir().unwrap();
        let (store, _repo, now) = aged_obligation(temp.path(), chrono::Duration::hours(1));
        let store = store.lock().await;
        let project = store.bounded_qa_landings().unwrap()[0].project_id;
        let landings = store.bounded_qa_landings().unwrap();
        let (first, second) = (Uuid::new_v4(), Uuid::new_v4());
        store
            .request_bounded_qa(&landings, first, "sha", &hub(), now)
            .unwrap();
        assert!(store.last_bounded_qa_dispatch(project).unwrap().is_some());
        let bound = store.bounded_qa_landings().unwrap();
        store
            .defer_bounded_qa(
                &bound,
                first,
                "sha",
                DEPLOY_PREEMPTED,
                &BoundedQaRun::default(),
                now,
            )
            .unwrap();
        assert!(
            store.last_bounded_qa_dispatch(project).unwrap().is_none(),
            "the preempted dispatch is not an interval anchor"
        );
        // A capacity timeout (it did not run tests either) keeps its anchor.
        let landings = store.bounded_qa_landings().unwrap();
        store
            .request_bounded_qa(&landings, second, "sha", &hub(), now)
            .unwrap();
        let bound = store.bounded_qa_landings().unwrap();
        store
            .defer_bounded_qa(
                &bound,
                second,
                "sha",
                JOB_ADMISSION_TIMED_OUT,
                &BoundedQaRun::default(),
                now,
            )
            .unwrap();
        assert!(store.last_bounded_qa_dispatch(project).unwrap().is_some());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn bounded_qa_snapshot_is_pinned_and_coalesces_only_verified_published_ancestors() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q"]).unwrap();
        let a = commit(&repo, "first");
        let b = commit(&repo, "second");
        let qa_root = root.path().join("qa");
        let cwd = snapshot(&repo, &b, &qa_root).unwrap();
        assert!(covers(&cwd, &b, &landing(&a, &b)));
        let c = commit(&repo, "third");
        assert!(!covers(&cwd, &b, &landing(&a, &c)));
        // A restart between snapshot construction and job insertion must use
        // its original pinned tree, even if rolling has advanced meanwhile.
        assert_eq!(snapshot(&repo, &c, &qa_root).unwrap(), cwd);
        assert_eq!(head_commit(&cwd).unwrap(), b);
        assert_eq!(
            std::fs::read_to_string(cwd.join("value")).unwrap(),
            "second"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn bounded_qa_failure_issue_names_every_landing_and_the_full_run_sha() {
        let mut work = vec![
            landing(&"a".repeat(40), &"b".repeat(40)),
            landing(&"c".repeat(40), &"d".repeat(40)),
        ];
        work[1].project_id = work[0].project_id;
        let sha = "e".repeat(40);
        let issue = failure_issue(
            &work,
            &sha,
            "Job failed; log /tmp/qa.log; test::red",
            QaFailureKind::TestRed,
        )
        .unwrap();
        for landing in &work {
            assert!(issue.body.contains(&landing.entry.source_commit));
            assert!(issue.body.contains(&landing.entry.id.to_string()));
        }
        assert!(issue.body.contains(&sha));
        assert!(issue.body.contains("test::red"));
        assert_eq!(issue.labels, ["qa", "bug"]);
    }

    // ---- #2120: dispatch to a project satellite over the #1017 link --------

    /// The QA admission a satellite reports (as it computes it): `free` GiB
    /// where its build governor measures disk against a `floor` GiB governor
    /// floor, never below the QA floor, for a run that needs no target above
    /// it.
    fn reported(free: u64, floor: u64) -> SatelliteQaAdmissionV1 {
        reported_with_target(free, floor, 0)
    }

    /// #2176: the same, for a run that needs `target` GiB above the floor.
    fn reported_with_target(free: u64, floor: u64, target: u64) -> SatelliteQaAdmissionV1 {
        let floor_bytes = (floor * GIB_BYTES).max(SATELLITE_QA_MIN_DISK_FREE_BYTES);
        let free_bytes = free * GIB_BYTES;
        let target_bytes = target * GIB_BYTES;
        SatelliteQaAdmissionV1 {
            refusal: (free_bytes < floor_bytes + target_bytes)
                .then(|| SATELLITE_QA_DISK_LOW.to_owned()),
            build_disk_free_bytes: Some(free_bytes),
            build_disk_reclaimable_bytes: 0,
            build_disk_floor_bytes: floor_bytes,
            target_bytes: Some(target_bytes),
        }
    }

    fn health(admission: SatelliteQaAdmissionV1) -> rsi_common::satellite::SatelliteHealthV1 {
        rsi_common::satellite::SatelliteHealthV1 {
            platform: Some(rsi_common::satellite::SatellitePlatformV1 {
                os: std::env::consts::OS.into(),
                arch: std::env::consts::ARCH.into(),
            }),
            daemon_version: "1.0.0".into(),
            binary_sha256: None,
            uptime_seconds: 1,
            schema_version: None,
            sessions_running: 0,
            sessions_waiting_approval: 0,
            disk_free_bytes: admission.build_disk_free_bytes,
            load_avg_1m_milli: None,
            build_sha: None,
            started_at: None,
            supervisor_mode: None,
            last_deploy: None,
            missing_provider_clis: Vec::new(),
            manager_launches: Vec::new(),
            qa_admission: Some(admission),
            provider_logins: Vec::new(),
            provider_faults: Vec::new(),
            store_probe_elapsed_ms: None,
            latest_watchdog_trip: None,
        }
    }

    /// What the fake satellite's job reports to `RequestHubQaRun`.
    #[derive(Clone)]
    struct FakeRun {
        state: JobState,
        exit_code: Option<i32>,
        failing_tests: Vec<String>,
        /// The job's typed infrastructure cause (#2176: `interrupted_by_deploy`).
        refusal: Option<String>,
    }

    struct FakeSatellite {
        peer: Uuid,
        job: Uuid,
        requests: Arc<std::sync::Mutex<Vec<SatelliteQaRunRequestV1>>>,
        run: Arc<std::sync::Mutex<FakeRun>>,
        /// #180: record the next start, then close the link without a reply.
        drop_next_start: Arc<std::sync::atomic::AtomicBool>,
        /// #2155: the satellite's health reports `satellite_qa_busy` (its
        /// original run holds the one QA target), so a fresh placement is
        /// refused by the hub's admission recheck.
        busy: Arc<std::sync::atomic::AtomicBool>,
        /// #2155: `false` closes every link before the handshake (the
        /// satellite is temporarily unreachable).
        reachable: Arc<std::sync::atomic::AtomicBool>,
        /// Answer the next start with this error message instead of a
        /// receipt. The request is still recorded as seen. #2156: the raw
        /// text of a launch error whose job stayed running is not a refusal.
        error_next_start: Arc<std::sync::Mutex<Option<String>>>,
        /// #2176: the health reports a staged deploy (as an older satellite
        /// build does, whatever its QA admission says).
        deploying: Arc<std::sync::atomic::AtomicBool>,
        /// #2176: identity handshakes served (each new placement makes one).
        handshakes: Arc<std::sync::atomic::AtomicUsize>,
        platform: Arc<std::sync::Mutex<Option<rsi_common::satellite::SatellitePlatformV1>>>,
    }

    /// The hub's private satellite socket directory (mode 0700; under /tmp
    /// so socket paths stay short).
    fn socket_root() -> (tempfile::TempDir, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let temp = crate::test_support::socket_tempdir("sat-qa-");
        let root = temp.path().join("satellites");
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        (temp, root)
    }

    /// A paired, dispatch-enabled project satellite peer `label` of the hub
    /// `store`, served under `root` by a fake satellite daemon that reports
    /// `admission` and the `qa_run` capability and answers `RequestHubQaRun`
    /// from its current [`FakeRun`]. `observed` records a fresh healthy poll.
    fn fake_satellite(
        store: &Store,
        root: &Path,
        label: &str,
        admission: SatelliteQaAdmissionV1,
        observed: bool,
    ) -> FakeSatellite {
        use rsi_common::satellite::{
            SatelliteLinkConfigV1, SatelliteLinkDirectionV1, SatellitePeerConfigV1,
            SatellitePutLinkRequestV1, SatellitePutPeerRequestV1,
        };
        use std::os::unix::fs::PermissionsExt;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let socket = root.join(format!("{label}.sock"));
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
        let (installation, incarnation, job) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let run = Arc::new(std::sync::Mutex::new(FakeRun {
            state: JobState::Running,
            exit_code: None,
            failing_tests: Vec::new(),
            refusal: None,
        }));
        let drop_next_start = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let busy = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reachable = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let error_next_start = Arc::new(std::sync::Mutex::new(None::<String>));
        let deploying = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let handshakes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (is_deploying, shaken) = (Arc::clone(&deploying), Arc::clone(&handshakes));
        let (seen, answer, dropping) = (
            Arc::clone(&requests),
            Arc::clone(&run),
            Arc::clone(&drop_next_start),
        );
        let (is_busy, is_reachable, failing) = (
            Arc::clone(&busy),
            Arc::clone(&reachable),
            Arc::clone(&error_next_start),
        );
        let platform = Arc::new(std::sync::Mutex::new(health(admission.clone()).platform));
        let reported_platform = Arc::clone(&platform);
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                if !is_reachable.load(std::sync::atomic::Ordering::SeqCst) {
                    continue;
                }
                let (seen, answer, dropping, admission, is_busy, failing) = (
                    Arc::clone(&seen),
                    Arc::clone(&answer),
                    Arc::clone(&dropping),
                    admission.clone(),
                    Arc::clone(&is_busy),
                    Arc::clone(&failing),
                );
                let reported_platform = Arc::clone(&reported_platform);
                let (is_deploying, shaken) = (Arc::clone(&is_deploying), Arc::clone(&shaken));
                tokio::spawn(async move {
                    let mut stream = tokio::io::BufReader::new(stream);
                    let mut line = String::new();
                    loop {
                        line.clear();
                        if stream.read_line(&mut line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        let request: rsi_common::rpc::RpcRequest =
                            serde_json::from_str(&line).unwrap();
                        let id = Some(request.id.clone().into());
                        let result: std::result::Result<serde_json::Value, String> = match request
                            .method
                            .as_str()
                        {
                            "GetSatelliteIdentity" => {
                                let mut identity =
                                    crate::satellite::identity(installation, incarnation);
                                let mut reported = admission.clone();
                                if is_busy.load(std::sync::atomic::Ordering::SeqCst) {
                                    reported.refusal = Some(SATELLITE_QA_BUSY.into());
                                }
                                let mut health = health(reported);
                                health.platform = reported_platform.lock().unwrap().clone();
                                if is_deploying.load(std::sync::atomic::Ordering::SeqCst) {
                                    health.last_deploy =
                                        Some(rsi_common::satellite::SatelliteDeployStatusV1 {
                                            deploy_id: SatelliteUuidV1(Uuid::new_v4()),
                                            state: rsi_common::agent_deploy::DeployState::Staged,
                                            sha: "0".repeat(40),
                                            blockers: Vec::new(),
                                            waiting_since: None,
                                            finished_at: None,
                                        });
                                }
                                shaken.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                identity.health = Some(health);
                                Ok(serde_json::to_value(identity).unwrap())
                            }
                            "RequestHubQaRun" => {
                                let wire: SatelliteQaRunRequestV1 =
                                    serde_json::from_value(request.params.clone()).unwrap();
                                let lose_reply = wire.start
                                    && dropping.swap(false, std::sync::atomic::Ordering::SeqCst);
                                let error = if wire.start {
                                    failing.lock().unwrap().take()
                                } else {
                                    None
                                };
                                let run = answer.lock().unwrap().clone();
                                let receipt = SatelliteQaRunReceiptV1 {
                                    run_id: wire.run_id,
                                    sha: wire.sha.clone(),
                                    job_id: job,
                                    state: run.state,
                                    exit_code: run.exit_code,
                                    refusal: run.refusal,
                                    failing_tests: run.failing_tests,
                                    failing_tests_omitted: 0,
                                    log_path: format!("/laptop/jobs/{job}.log"),
                                    replayed: !wire.start,
                                };
                                seen.lock().unwrap().push(wire);
                                if lose_reply {
                                    // Accepted, then the link drops before the reply.
                                    return;
                                }
                                match error {
                                    Some(message) => Err(message),
                                    None => Ok(serde_json::to_value(receipt).unwrap()),
                                }
                            }
                            other => panic!("the hub called {other}"),
                        };
                        let response = match result {
                            Ok(result) => rsi_common::rpc::RpcResponse::success(id, result),
                            Err(message) => rsi_common::rpc::RpcResponse::error(
                                id,
                                rsi_common::rpc::RpcError {
                                    code: -32000,
                                    message,
                                    data: None,
                                },
                            ),
                        };
                        let mut bytes = serde_json::to_vec(&response).unwrap();
                        bytes.push(b'\n');
                        stream.get_mut().write_all(&bytes).await.unwrap();
                    }
                });
            }
        });
        let peer = Uuid::new_v4();
        let revision = store.satellite_registry_revision().unwrap();
        store
            .put_satellite_peer(
                &SatellitePutPeerRequestV1 {
                    expected_registry_revision: revision,
                    peer: SatellitePeerConfigV1 {
                        peer_id: SatelliteUuidV1(peer),
                        label: label.into(),
                        expected_installation_id: Some(SatelliteUuidV1(installation)),
                        enabled: true,
                        read_enabled: true,
                        dispatch_enabled: true,
                    },
                    repair_quarantine: false,
                },
                root,
            )
            .unwrap();
        let revision = store.satellite_registry_revision().unwrap();
        store
            .put_satellite_link(
                &SatellitePutLinkRequestV1 {
                    expected_registry_revision: revision,
                    peer_id: SatelliteUuidV1(peer),
                    link: SatelliteLinkConfigV1 {
                        link_id: SatelliteUuidV1(Uuid::new_v4()),
                        direction: SatelliteLinkDirectionV1::DialHomeReverse,
                        socket_path: socket.to_string_lossy().into_owned(),
                        ssh_target: None,
                        trust_reference: format!("ssh-config:{label}"),
                        enabled: true,
                        priority: 0,
                    },
                },
                root,
            )
            .unwrap();
        let revision = store.satellite_registry_revision().unwrap();
        store
            .put_satellite_peer_scope(revision, peer, &[SatelliteUuidV1(Uuid::new_v4())])
            .unwrap();
        if observed {
            store
                .record_satellite_snapshot(
                    peer,
                    &crate::store_support::satellite::PeerSnapshot {
                        installation_id: installation,
                        incarnation_id: incarnation,
                        sessions: Vec::new(),
                        health: None,
                    },
                )
                .unwrap();
        }
        FakeSatellite {
            peer,
            job,
            requests,
            run,
            drop_next_start,
            busy,
            reachable,
            error_next_start,
            deploying,
            handshakes,
            platform,
        }
    }

    /// A hub store with one project and one obliged published landing in a
    /// fresh repository; returns the repository's commit.
    fn hub_fixture(temp: &Path) -> (Store, Uuid, String) {
        use rsi_common::types::Project;
        let repo = temp.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q"]).unwrap();
        let sha = commit(&repo, "landed");
        let store = Store::open_in_memory().unwrap();
        let project = Uuid::new_v4();
        let now = Utc::now();
        store
            .insert_project(&Project {
                id: project,
                name: "qa-satellite".into(),
                path: None,
                description: None,
                color: Project::DEFAULT_COLOR.into(),
                context_files: None,
                created_at: now,
                updated_at: now,
            })
            .unwrap();
        obliged_landing(&store, project, &repo, &sha, now);
        (store, project, sha)
    }

    fn unused_tools() -> JobTools {
        JobTools {
            cargo_slot: "unused".into(),
            lander: "unused".into(),
        }
    }

    /// One loop pass with no hold and a 60-minute minimum interval.
    async fn drive_every_hour(
        store: &Arc<tokio::sync::Mutex<Store>>,
        jobs: &Path,
        satellites: &mut SatelliteQa,
    ) {
        drive_once(
            store,
            &LanderLauncher::new(PathBuf::from("unused")),
            Arc::new(NoLaunch),
            &unused_tools(),
            jobs,
            satellites,
            None,
            chrono::Duration::minutes(60),
            Utc::now(),
        )
        .await
        .unwrap();
    }

    /// One loop pass with no hold and no minimum interval.
    async fn drive(
        store: &Arc<tokio::sync::Mutex<Store>>,
        jobs: &Path,
        satellites: &mut SatelliteQa,
    ) -> Result<()> {
        drive_once(
            store,
            &LanderLauncher::new(PathBuf::from("unused")),
            Arc::new(NoLaunch),
            &unused_tools(),
            jobs,
            satellites,
            None,
            chrono::Duration::zero(),
            Utc::now(),
        )
        .await
    }

    /// #2120 acceptance: a healthy, reachable, compatible-build project
    /// satellite takes the obliged exact SHA. The hub builds no snapshot and
    /// submits no job of its own; the binding names the satellite, an
    /// in-flight run holds the project like a hub job, and the satellite's
    /// red receipt settles every landing and files the existing QA Issue
    /// with the satellite's exit code, failing tests and log.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_healthy_satellite_takes_the_run_and_its_receipt_settles_it() {
        let temp = tempfile::tempdir().unwrap();
        let (store, _project, sha) = hub_fixture(temp.path());
        let (_sockets, root) = socket_root();
        // #2176: 56 GiB free holds the 30 GiB floor plus a 25 GiB target.
        let sat = fake_satellite(
            &store,
            &root,
            "laptop",
            reported_with_target(56, 30, 25),
            true,
        );
        let store = Arc::new(tokio::sync::Mutex::new(store));
        let jobs = temp.path().join("jobs");
        let mut satellites = SatelliteQa::new(root.clone());
        satellites.poll_every = std::time::Duration::ZERO;
        drive(&store, &jobs, &mut satellites).await.unwrap();
        let bound = store.lock().await.bounded_qa_landings().unwrap();
        assert_eq!(bound.len(), 1);
        assert_eq!(
            bound[0].placement,
            Some(BoundedQaPlacement::Satellite {
                peer_id: sat.peer,
                peer_label: "laptop".into(),
                remote_job_id: Some(sat.job),
            })
        );
        assert_eq!(bound[0].qa_sha.as_deref(), Some(sha.as_str()));
        let run_id = bound[0].job_id.unwrap();
        {
            let requests = sat.requests.lock().unwrap();
            assert_eq!(requests.len(), 1);
            assert!(requests[0].start);
            assert_eq!(requests[0].sha, sha);
            assert_eq!(requests[0].run_id, run_id);
        }
        assert!(!jobs.join(SNAPSHOTS).exists(), "no hub snapshot");
        // Still running on the satellite: polled, nothing settles or restarts.
        drive(&store, &jobs, &mut satellites).await.unwrap();
        assert_eq!(store.lock().await.bounded_qa_landings().unwrap().len(), 1);
        assert!(sat.requests.lock().unwrap()[1..].iter().all(|r| !r.start));
        *sat.run.lock().unwrap() = FakeRun {
            state: JobState::Failed,
            exit_code: Some(100),
            failing_tests: vec!["store::tests::red".into()],
            refusal: None,
        };
        drive(&store, &jobs, &mut satellites).await.unwrap();
        let store = store.lock().await;
        assert!(store.bounded_qa_landings().unwrap().is_empty());
        let issue = store
            .get_issue(Uuid::new_v5(&run_id, b"bounded-library-qa-red"))
            .unwrap()
            .unwrap();
        assert_eq!(issue.labels, ["qa", "bug"]);
        for expected in [
            "satellite laptop",
            "store::tests::red",
            "exit Some(100)",
            &format!("/laptop/jobs/{}.log", sat.job),
            &sha,
            &bound[0].entry.id.to_string(),
        ] {
            assert!(issue.body.contains(expected), "{expected}: {}", issue.body);
        }
    }

    /// Place one run on a hub whose only peer is a fake satellite `laptop`
    /// reporting `admission`.
    async fn place_with(
        temp: &Path,
        admission: SatelliteQaAdmissionV1,
        observed: bool,
    ) -> (Placed, FakeSatellite, tempfile::TempDir) {
        let (store, _, sha) = hub_fixture(temp);
        let (sockets, root) = socket_root();
        let sat = fake_satellite(&store, &root, "laptop", admission, observed);
        let store = Arc::new(tokio::sync::Mutex::new(store));
        let cohort = store.lock().await.bounded_qa_landings().unwrap();
        let placed = place(
            &store,
            &mut SatelliteQa::new(root),
            Uuid::new_v4(),
            &sha,
            &cohort,
            Utc::now(),
        )
        .await
        .unwrap();
        (placed, sat, sockets)
    }

    fn hub_reason(placed: Placed) -> String {
        match placed {
            Placed::Hub { reason } => reason,
            Placed::Satellite { peer, .. } => panic!("placed on {}", peer.label),
            Placed::Wait { reason } => panic!("waits: {reason}"),
        }
    }

    /// Platform is checked against the live identity, before any QA request;
    /// matching OS admits even when the satellite has another architecture.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn satellite_platform_match_mismatch_and_unknown_control_placement() {
        use crate::satellite::qa_link::{SATELLITE_PLATFORM_MISMATCH, SATELLITE_PLATFORM_UNKNOWN};
        use rsi_common::satellite::SatellitePlatformV1;
        for (os, expected) in [
            (Some(std::env::consts::OS), None),
            (
                Some(if std::env::consts::OS == "linux" {
                    "macos"
                } else {
                    "linux"
                }),
                Some(SATELLITE_PLATFORM_MISMATCH),
            ),
            (None, Some(SATELLITE_PLATFORM_UNKNOWN)),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let (store, _, sha) = hub_fixture(temp.path());
            let (_sockets, root) = socket_root();
            let sat = fake_satellite(&store, &root, "laptop", reported(80, 30), true);
            *sat.platform.lock().unwrap() = os.map(|os| SatellitePlatformV1 {
                os: os.into(),
                arch: "another_arch".into(),
            });
            let store = Arc::new(tokio::sync::Mutex::new(store));
            let cohort = store.lock().await.bounded_qa_landings().unwrap();
            let placed = place(
                &store,
                &mut SatelliteQa::new(root.clone()),
                Uuid::new_v4(),
                &sha,
                &cohort,
                Utc::now(),
            )
            .await
            .unwrap();
            let info = crate::satellite::deploy_link::read_peer_daemon_info(
                &root,
                &satellite_candidates(&store, Utc::now()).await.unwrap().0[0],
            )
            .await;
            assert!(info.reachable);
            assert_eq!(info.platform, sat.platform.lock().unwrap().clone());
            match expected {
                Some(code) => {
                    let platform = os
                        .map(|os| format!("{os}/another_arch"))
                        .unwrap_or_else(|| "unknown".into());
                    assert_eq!(
                        hub_reason(placed),
                        format!("laptop: {code} (platform: {platform})")
                    );
                    assert_eq!(sat.requests.lock().unwrap().len(), 0);
                    // A definite rejection withdraws the dispatch intent.
                    assert_eq!(
                        store.lock().await.bounded_qa_landings().unwrap()[0].job_id,
                        None
                    );
                }
                None => {
                    assert!(
                        matches!(placed, Placed::Satellite { peer, .. } if peer.id == sat.peer)
                    );
                    let requests = sat.requests.lock().unwrap();
                    assert_eq!(requests.len(), 1);
                    assert!(requests[0].start);
                    assert_eq!(requests[0].sha, sha);
                }
            }
            let registry = store.lock().await.satellite_registry_view().unwrap();
            assert_eq!(
                registry.peers[0].observation.as_ref().unwrap().platform,
                info.platform
            );
        }
    }

    /// #2120 acceptance and #179: with no qualifying satellite the run falls
    /// back to the hub and records why, and the satellite is never sent the
    /// run: none registered, a stale poller observation, free space between
    /// the QA floor (20 GiB) and the build governor's floor (30), a higher
    /// configured governor floor (40), a report whose free space is below its
    /// own floor (the hub rechecks it), or a busy satellite. #2176 acceptance
    /// (c): free space above the floor (what its workers need) but short of
    /// the floor plus the run's measured target, as the satellite reports it,
    /// as the hub rechecks it, and with the 25 GiB first-run default for a
    /// satellite that reports no size.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn no_qualifying_satellite_falls_back_to_the_hub_with_its_reason() {
        let temp = tempfile::tempdir().unwrap();
        let (store, _project, sha) = hub_fixture(temp.path());
        let store = Arc::new(tokio::sync::Mutex::new(store));
        let mut unused = SatelliteQa::new(temp.path().join("satellites"));
        let cohort = store.lock().await.bounded_qa_landings().unwrap();
        assert_eq!(
            hub_reason(
                place(
                    &store,
                    &mut unused,
                    Uuid::new_v4(),
                    &sha,
                    &cohort,
                    Utc::now()
                )
                .await
                .unwrap()
            ),
            SATELLITE_QA_NO_PEER
        );
        let unchecked = SatelliteQaAdmissionV1 {
            refusal: None,
            build_disk_free_bytes: Some(25 * GIB_BYTES),
            build_disk_reclaimable_bytes: 0,
            build_disk_floor_bytes: 30 * GIB_BYTES,
            target_bytes: Some(0),
        };
        let mut busy = reported(50, 30);
        busy.refusal = Some(SATELLITE_QA_BUSY.into());
        let unchecked_target = SatelliteQaAdmissionV1 {
            refusal: None,
            build_disk_free_bytes: Some(50 * GIB_BYTES),
            build_disk_reclaimable_bytes: 0,
            build_disk_floor_bytes: 30 * GIB_BYTES,
            target_bytes: Some(21 * GIB_BYTES),
        };
        let no_size = SatelliteQaAdmissionV1 {
            target_bytes: None,
            ..unchecked_target.clone()
        };
        for (case, admission, observed, expected) in [
            ("stale", reported(31, 30), false, SATELLITE_QA_STALE),
            (
                "between-floors",
                reported(25, 30),
                true,
                SATELLITE_QA_DISK_LOW,
            ),
            (
                "higher-floor",
                reported(35, 40),
                true,
                SATELLITE_QA_DISK_LOW,
            ),
            ("hub-recheck", unchecked, true, SATELLITE_QA_DISK_LOW),
            ("busy", busy, true, SATELLITE_QA_BUSY),
            (
                "floor-plus-target",
                reported_with_target(50, 30, 21),
                true,
                SATELLITE_QA_DISK_LOW,
            ),
            (
                "hub-recheck-target",
                unchecked_target,
                true,
                SATELLITE_QA_DISK_LOW,
            ),
            ("no-size-default", no_size, true, SATELLITE_QA_DISK_LOW),
        ] {
            let (placed, sat, _sockets) =
                place_with(&temp.path().join(case), admission, observed).await;
            let platform = if observed {
                format!("{}/{}", std::env::consts::OS, std::env::consts::ARCH)
            } else {
                "unknown".into()
            };
            assert_eq!(
                hub_reason(placed),
                format!("laptop: {expected} (platform: {platform})"),
                "{case}"
            );
            assert!(sat.requests.lock().unwrap().is_empty(), "{case}");
        }
        // End to end the hub runs it itself: it pins its own snapshot (the
        // fixture declares no recipe, so the submit after it is refused).
        let (full_store, _, full_sha) = hub_fixture(&temp.path().join("full"));
        let (_sockets, root) = socket_root();
        let full = fake_satellite(&full_store, &root, "laptop", reported(25, 30), true);
        let full_store = Arc::new(tokio::sync::Mutex::new(full_store));
        let mut satellites = SatelliteQa::new(root);
        let jobs = temp.path().join("full-jobs");
        let refused = drive(&full_store, &jobs, &mut satellites).await;
        assert!(refused.is_err());
        assert!(full.requests.lock().unwrap().is_empty());
        let pending = full_store.lock().await.bounded_qa_landings().unwrap();
        let pinned = jobs
            .join(SNAPSHOTS)
            .join(pending[0].entry.id.to_string())
            .join("source");
        assert_eq!(head_commit(&pinned).as_deref(), Some(full_sha.as_str()));
    }

    /// #180: a satellite that accepted the start but whose reply was lost
    /// keeps the run. The hub starts nothing itself and offers it to no other
    /// peer; the binding names that peer and run with the satellite job
    /// unknown. A landing published meanwhile waits (one coalesced run per
    /// project). The next poll replays the same run's idempotent start and
    /// reconciles the satellite's receipt, which then settles the run.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_lost_start_reply_keeps_the_run_on_that_satellite_and_reconciles_it() {
        let temp = tempfile::tempdir().unwrap();
        let (store, project, sha) = hub_fixture(temp.path());
        let (_sockets, root) = socket_root();
        // The registry lists peers by label: `a-laptop` is offered the run first.
        let first = fake_satellite(&store, &root, "a-laptop", reported(31, 30), true);
        let second = fake_satellite(&store, &root, "b-laptop", reported(31, 30), true);
        first
            .drop_next_start
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let store = Arc::new(tokio::sync::Mutex::new(store));
        let jobs = temp.path().join("jobs");
        let mut satellites = SatelliteQa::new(root.clone());
        satellites.poll_every = std::time::Duration::ZERO;
        drive(&store, &jobs, &mut satellites).await.unwrap();
        let bound = store.lock().await.bounded_qa_landings().unwrap();
        assert_eq!(bound.len(), 1);
        let run_id = bound[0].job_id.unwrap();
        assert_eq!(
            bound[0].placement,
            Some(BoundedQaPlacement::Satellite {
                peer_id: first.peer,
                peer_label: "a-laptop".into(),
                remote_job_id: None,
            })
        );
        assert_eq!(bound[0].qa_sha.as_deref(), Some(sha.as_str()));
        assert_eq!(first.requests.lock().unwrap().len(), 1);
        assert!(second.requests.lock().unwrap().is_empty(), "no second peer");
        assert!(!jobs.join(SNAPSHOTS).exists(), "no hub run");

        let repo = temp.path().join("repo");
        let later = commit(&repo, "later");
        obliged_landing(&*store.lock().await, project, &repo, &later, Utc::now());
        drive(&store, &jobs, &mut satellites).await.unwrap();
        let pending = store.lock().await.bounded_qa_landings().unwrap();
        assert_eq!(pending.len(), 2);
        assert!(pending.iter().any(|w| w.job_id == Some(run_id)));
        assert!(
            pending
                .iter()
                .any(|w| w.entry.source_commit == later && w.job_id.is_none()),
            "the later landing waits for the in-flight run"
        );
        {
            let requests = first.requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            assert!(
                requests
                    .iter()
                    .all(|r| r.run_id == run_id && r.start && r.sha == sha),
                "the reconcile replays the same run"
            );
        }
        assert!(second.requests.lock().unwrap().is_empty());
        assert!(!jobs.join(SNAPSHOTS).exists());

        *first.run.lock().unwrap() = FakeRun {
            state: JobState::Succeeded,
            exit_code: Some(0),
            failing_tests: Vec::new(),
            refusal: None,
        };
        drive(&store, &jobs, &mut satellites).await.unwrap();
        let pending = store.lock().await.bounded_qa_landings().unwrap();
        // The reconciled run settled; the waiting landing got its own run.
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].entry.source_commit, later);
        assert!(pending[0].job_id.is_some_and(|run| run != run_id));
        assert!(matches!(
            &pending[0].placement,
            Some(BoundedQaPlacement::Satellite { peer_id, .. }) if *peer_id == first.peer
        ));
        assert!(second.requests.lock().unwrap().is_empty());
        assert!(!jobs.join(SNAPSHOTS).exists());
        assert!(
            store
                .lock()
                .await
                .get_issue(Uuid::new_v5(&run_id, b"bounded-library-qa-red"))
                .unwrap()
                .is_none(),
            "a green satellite receipt files no QA Issue"
        );
    }
    /// #2155: the satellite accepted the start and the hub stopped before it
    /// recorded the binding. The intent recorded before the send names the
    /// peer, run and commit; the reconstructed dispatcher binds that exact
    /// peer instead of placing the obligation again, whatever that satellite
    /// then reports. It reports itself busy (its original run holds the one QA
    /// target, which would refuse a fresh placement) and is unreachable for a
    /// pass: no second peer and no hub run start, the replayed starts reuse
    /// the run id and skip the admission check, and the original run's
    /// receipt settles it.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_start_accepted_before_the_hub_bound_it_is_reconciled_not_placed_again() {
        use std::sync::atomic::Ordering::SeqCst;
        let temp = tempfile::tempdir().unwrap();
        let (store, _project, sha) = hub_fixture(temp.path());
        let (_sockets, root) = socket_root();
        let first = fake_satellite(&store, &root, "a-laptop", reported(31, 30), true);
        let second = fake_satellite(&store, &root, "b-laptop", reported(31, 30), true);
        let store = Arc::new(tokio::sync::Mutex::new(store));
        let jobs = temp.path().join("jobs");

        // The start is sent (and accepted); the hub stops before the binding.
        let cohort = store.lock().await.bounded_qa_landings().unwrap();
        let run_id = satellite_run_id(cohort[0].entry.id, 0);
        let placed = place(
            &store,
            &mut SatelliteQa::new(root.clone()),
            run_id,
            &sha,
            &cohort,
            Utc::now(),
        )
        .await
        .unwrap();
        assert!(matches!(
            &placed,
            Placed::Satellite { peer, receipt: Some(_) } if peer.id == first.peer
        ));
        {
            let held = store.lock().await.bounded_qa_landings().unwrap();
            assert_eq!(held.len(), 1);
            assert!(held[0].job_id.is_none(), "the binding never committed");
            let intent = held[0].intent.clone().unwrap();
            assert_eq!(
                (intent.run_id, intent.peer_id, intent.sha),
                (run_id, first.peer, sha.clone())
            );
        }
        assert_eq!(first.requests.lock().unwrap().len(), 1);

        first.busy.store(true, SeqCst);
        // Reconciliation still polls/replays the original run even when a
        // later identity lacks the platform required for a new placement.
        *first.platform.lock().unwrap() = None;
        first.reachable.store(false, SeqCst);
        let mut satellites = SatelliteQa::new(root.clone());
        satellites.poll_every = std::time::Duration::ZERO;
        drive(&store, &jobs, &mut satellites).await.unwrap();
        let bound = store.lock().await.bounded_qa_landings().unwrap();
        assert_eq!(bound.len(), 1);
        assert_eq!(bound[0].job_id, Some(run_id));
        assert_eq!(
            bound[0].placement,
            Some(BoundedQaPlacement::Satellite {
                peer_id: first.peer,
                peer_label: "a-laptop".into(),
                remote_job_id: None,
            })
        );
        assert_eq!(bound[0].qa_sha.as_deref(), Some(sha.as_str()));
        assert!(bound[0].intent.is_none());
        assert!(second.requests.lock().unwrap().is_empty(), "no second peer");
        assert!(!jobs.join(SNAPSHOTS).exists(), "no hub run");
        assert_eq!(first.requests.lock().unwrap().len(), 1, "unreachable");

        // Reachable again, still busy: the replay reconciles the original run.
        first.reachable.store(true, SeqCst);
        drive(&store, &jobs, &mut satellites).await.unwrap();
        {
            let requests = first.requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            assert!(
                requests
                    .iter()
                    .all(|r| r.run_id == run_id && r.start && r.sha == sha),
                "the replay names the same run and commit"
            );
        }
        assert!(second.requests.lock().unwrap().is_empty());
        assert!(!jobs.join(SNAPSHOTS).exists());
        assert_eq!(store.lock().await.bounded_qa_landings().unwrap().len(), 1);

        *first.run.lock().unwrap() = FakeRun {
            state: JobState::Succeeded,
            exit_code: Some(0),
            failing_tests: Vec::new(),
            refusal: None,
        };
        drive(&store, &jobs, &mut satellites).await.unwrap();
        assert!(store.lock().await.bounded_qa_landings().unwrap().is_empty());
        assert!(second.requests.lock().unwrap().is_empty());
        assert!(!jobs.join(SNAPSHOTS).exists());
        assert!(
            store
                .lock()
                .await
                .get_issue(Uuid::new_v5(&run_id, b"bounded-library-qa-red"))
                .unwrap()
                .is_none(),
            "a green satellite receipt files no QA Issue"
        );
    }

    /// #2156: the satellite answers the start with the raw error of a launch
    /// whose job stayed running (its service start failed and the stop could
    /// not be confirmed). That is not a refusal: the run stays bound to that
    /// peer, no second peer and no hub run start while the original may be
    /// alive, and the next poll replays the idempotent start and reconciles
    /// the running job.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn an_uncertain_launch_error_keeps_the_run_on_that_satellite_and_reconciles_it() {
        let temp = tempfile::tempdir().unwrap();
        let (store, _project, sha) = hub_fixture(temp.path());
        let (_sockets, root) = socket_root();
        let first = fake_satellite(&store, &root, "a-laptop", reported(31, 30), true);
        let second = fake_satellite(&store, &root, "b-laptop", reported(31, 30), true);
        *first.error_next_start.lock().unwrap() = Some(
            "Process error: job_launch_failed: controller unavailable after dispatch; job 7c4d2a1e-0000-4000-8000-000000000000 remains running because service stop could not be confirmed"
                .into(),
        );
        let store = Arc::new(tokio::sync::Mutex::new(store));
        let jobs = temp.path().join("jobs");
        let mut satellites = SatelliteQa::new(root.clone());
        satellites.poll_every = std::time::Duration::ZERO;
        drive(&store, &jobs, &mut satellites).await.unwrap();
        let bound = store.lock().await.bounded_qa_landings().unwrap();
        assert_eq!(bound.len(), 1);
        let run_id = bound[0].job_id.unwrap();
        assert_eq!(
            bound[0].placement,
            Some(BoundedQaPlacement::Satellite {
                peer_id: first.peer,
                peer_label: "a-laptop".into(),
                remote_job_id: None,
            })
        );
        assert_eq!(first.requests.lock().unwrap().len(), 1);
        assert!(second.requests.lock().unwrap().is_empty(), "no second peer");
        assert!(!jobs.join(SNAPSHOTS).exists(), "no hub run");

        drive(&store, &jobs, &mut satellites).await.unwrap();
        {
            let requests = first.requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            assert!(
                requests
                    .iter()
                    .all(|r| r.run_id == run_id && r.start && r.sha == sha),
                "the reconcile replays the same run"
            );
        }
        assert!(second.requests.lock().unwrap().is_empty());
        assert!(!jobs.join(SNAPSHOTS).exists());
        assert_eq!(store.lock().await.bounded_qa_landings().unwrap().len(), 1);

        *first.run.lock().unwrap() = FakeRun {
            state: JobState::Failed,
            exit_code: Some(100),
            failing_tests: vec!["store::tests::red".into()],
            refusal: None,
        };
        drive(&store, &jobs, &mut satellites).await.unwrap();
        assert!(store.lock().await.bounded_qa_landings().unwrap().is_empty());
        assert!(second.requests.lock().unwrap().is_empty());
        let issue = store
            .lock()
            .await
            .get_issue(Uuid::new_v5(&run_id, b"bounded-library-qa-red"))
            .unwrap()
            .unwrap();
        assert!(
            issue.body.contains("store::tests::red"),
            "the reconciled run's result reaches the hub: {}",
            issue.body
        );
    }

    /// A listed refusal proves the satellite started nothing: its intent is
    /// withdrawn and the next peer takes the run, with its own intent bound.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_listed_refusal_releases_the_run_to_the_next_peer() {
        let temp = tempfile::tempdir().unwrap();
        let (store, _project, sha) = hub_fixture(temp.path());
        let (_sockets, root) = socket_root();
        let first = fake_satellite(&store, &root, "a-laptop", reported(31, 30), true);
        let second = fake_satellite(&store, &root, "b-laptop", reported(31, 30), true);
        *first.error_next_start.lock().unwrap() = Some("Policy denied: satellite_qa_busy".into());
        let store = Arc::new(tokio::sync::Mutex::new(store));
        let jobs = temp.path().join("jobs");
        let mut satellites = SatelliteQa::new(root.clone());
        satellites.poll_every = std::time::Duration::ZERO;
        drive(&store, &jobs, &mut satellites).await.unwrap();
        let bound = store.lock().await.bounded_qa_landings().unwrap();
        assert_eq!(bound.len(), 1);
        assert_eq!(
            bound[0].placement,
            Some(BoundedQaPlacement::Satellite {
                peer_id: second.peer,
                peer_label: "b-laptop".into(),
                remote_job_id: Some(second.job),
            })
        );
        assert_eq!(bound[0].qa_sha.as_deref(), Some(sha.as_str()));
        assert_eq!(first.requests.lock().unwrap().len(), 1);
        assert_eq!(second.requests.lock().unwrap().len(), 1);
        assert!(!jobs.join(SNAPSHOTS).exists(), "no hub run");
    }
    /// #2176 acceptance (a): a deploy pending on the only satellite (an older
    /// build that reports it only on its deploy status) keeps the run off it:
    /// no start is sent, nothing is bound and the hub does not run it either;
    /// within the minute the peer is not asked again. Once the deploy is over
    /// the next pass places the run there.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_pending_peer_deploy_holds_the_run_off_that_peer_and_off_the_hub() {
        use std::sync::atomic::Ordering::SeqCst;
        let temp = tempfile::tempdir().unwrap();
        let (store, _project, sha) = hub_fixture(temp.path());
        let (_sockets, root) = socket_root();
        let sat = fake_satellite(&store, &root, "laptop", reported(56, 30), true);
        sat.deploying.store(true, SeqCst);
        let store = Arc::new(tokio::sync::Mutex::new(store));
        let jobs = temp.path().join("jobs");
        let mut satellites = SatelliteQa::new(root.clone());
        for _ in 0..2 {
            drive(&store, &jobs, &mut satellites).await.unwrap();
            let pending = store.lock().await.bounded_qa_landings().unwrap();
            assert_eq!(pending.len(), 1);
            assert!(pending[0].job_id.is_none(), "nothing is bound");
            assert!(pending[0].intent.is_none(), "the intent was withdrawn");
            assert!(sat.requests.lock().unwrap().is_empty(), "no start sent");
            assert!(!jobs.join(SNAPSHOTS).exists(), "the hub does not run it");
            assert_eq!(sat.handshakes.load(SeqCst), 1, "asked once a minute");
        }
        // The deploy settled and the minute passed: the satellite takes it.
        sat.deploying.store(false, SeqCst);
        satellites.poll_every = std::time::Duration::ZERO;
        drive(&store, &jobs, &mut satellites).await.unwrap();
        let bound = store.lock().await.bounded_qa_landings().unwrap();
        assert_eq!(
            bound[0].placement,
            Some(BoundedQaPlacement::Satellite {
                peer_id: sat.peer,
                peer_label: "laptop".into(),
                remote_job_id: Some(sat.job),
            })
        );
        let requests = sat.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].start && requests[0].sha == sha);
    }

    /// #2176: the pending deploy skips only that peer. A satellite whose own
    /// admission reports `satellite_qa_deploy_pending` is not sent the run;
    /// the next ready peer takes it.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn another_ready_peer_takes_the_run_while_one_deploys() {
        let temp = tempfile::tempdir().unwrap();
        let (store, _project, _sha) = hub_fixture(temp.path());
        let (_sockets, root) = socket_root();
        let mut deploying = reported(56, 30);
        deploying.refusal = Some(SATELLITE_QA_DEPLOY_PENDING.into());
        let first = fake_satellite(&store, &root, "a-laptop", deploying, true);
        let second = fake_satellite(&store, &root, "b-laptop", reported(56, 30), true);
        let store = Arc::new(tokio::sync::Mutex::new(store));
        let jobs = temp.path().join("jobs");
        let mut satellites = SatelliteQa::new(root.clone());
        drive(&store, &jobs, &mut satellites).await.unwrap();
        let bound = store.lock().await.bounded_qa_landings().unwrap();
        assert_eq!(
            bound[0].placement,
            Some(BoundedQaPlacement::Satellite {
                peer_id: second.peer,
                peer_label: "b-laptop".into(),
                remote_job_id: Some(second.job),
            })
        );
        assert!(first.requests.lock().unwrap().is_empty());
        assert_eq!(second.requests.lock().unwrap().len(), 1);
        assert!(!jobs.join(SNAPSHOTS).exists());
    }

    /// #2176 acceptance (b): a deploy on the satellite stopped the run
    /// (`interrupted_by_deploy`, even with the names of the tests it killed).
    /// That is the #2149 deferral with reason `deploy_preempted`: no QA Issue,
    /// no finished receipt, and the obligation is pending again for a new
    /// attempt (its own run id) after the interval.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_deploy_preempted_satellite_run_is_deferred_without_an_issue() {
        let temp = tempfile::tempdir().unwrap();
        let (store, _project, _sha) = hub_fixture(temp.path());
        let (_sockets, root) = socket_root();
        let sat = fake_satellite(&store, &root, "laptop", reported(56, 30), true);
        let store = Arc::new(tokio::sync::Mutex::new(store));
        let jobs = temp.path().join("jobs");
        let mut satellites = SatelliteQa::new(root.clone());
        satellites.poll_every = std::time::Duration::ZERO;
        drive_every_hour(&store, &jobs, &mut satellites).await;
        let bound = store.lock().await.bounded_qa_landings().unwrap();
        let (entry, run_id) = (bound[0].entry.id, bound[0].job_id.unwrap());
        assert_eq!(run_id, satellite_run_id(entry, 0));
        *sat.run.lock().unwrap() = FakeRun {
            state: JobState::Failed,
            exit_code: None,
            failing_tests: vec!["store::tests::killed".into()],
            refusal: Some(JOB_INTERRUPTED_BY_DEPLOY.into()),
        };
        drive_every_hour(&store, &jobs, &mut satellites).await;
        let store = store.lock().await;
        assert!(
            store
                .get_issue(Uuid::new_v5(&run_id, b"bounded-library-qa-red"))
                .unwrap()
                .is_none()
        );
        let pending = store.bounded_qa_landings().unwrap();
        assert_eq!(pending.len(), 1, "still owed");
        assert!(
            pending[0].job_id.is_none(),
            "re-armed, waiting out the interval"
        );
        assert_eq!(store.bounded_qa_capacity_deferrals(entry).unwrap(), 1);
        let reason: String = store
            .conn
            .query_row(
                "SELECT json_extract(detail_json,'$.refusal') FROM rolling_queue_events
                 WHERE entry_id=?1 AND kind='bounded_qa_capacity_deferred'",
                [entry.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(reason, DEPLOY_PREEMPTED);
        assert_ne!(satellite_run_id(entry, 1), run_id, "a new attempt");
    }
}
