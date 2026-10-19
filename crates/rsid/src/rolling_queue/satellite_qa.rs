//! Satellite side of #2120: run the hub's async full-library QA for one exact
//! published rolling SHA as an ordinary durable `rolling-library-qa` recipe
//! job, owned by this satellite's live declared scope seat.
//!
//! The satellite trusts only its own operator-set inbound policy (the hub
//! installation must be allowlisted, as for a hub deploy) and its own guards:
//! one full-library QA job at a time (they share one registered cargo target),
//! its host-load admission, a disk floor, and a deploy drain (which records
//! the job `queued` until the restart, #1566). The snapshot lives beside the
//! queue's own under `<jobs>/rolling-library-qa/`, so `agent_jobs` reuses the
//! one registered target instead of building a target per run; the previous
//! hub snapshot is removed when the next run starts.
//!
//! A start answers with a refusal only when nothing runs for it (#2156): a
//! launch error that left the recorded job alive (the service start failed
//! and the stop could not be confirmed, so `submit_with_hold` returns an error
//! but keeps the job `running`) answers with that job's receipt, so the hub
//! binds and polls the one run instead of placing a second copy elsewhere.
//!
//! Admission (#179) is one function for the health the hub reads and the
//! recheck at start: the run is taken only when its job could enter the build
//! governor now (its `min_free_disk_gb` floor, never below the QA floor, and
//! its load, memory, slice and slot gates), so a run is never accepted only to
//! time out in governor admission.
//!
//! #2176: the run is a guest that must not block deploys or starve this
//! satellite's workers. No run is taken while a deploy is staged or
//! restarting here, and a staged deploy stops a running hub run at once
//! (`agent_jobs::cancel_test_jobs_for_deploy`, `interrupted_by_deploy`, which
//! the hub defers as `deploy_preempted`). The governor floor is what the
//! workers need, so free space must also hold the run's own target: the size
//! measured for the last completed run, else
//! [`SATELLITE_QA_DEFAULT_TARGET_BYTES`]. Once a hub run has settled and no
//! full-library job is active, [`reclaim_settled_runs`] removes the shared
//! QA target's build output (recording its size first) and the run's
//! snapshot, so the disk returns to its pre-run level.

use super::bounded_qa::{RECIPE, SNAPSHOTS, full_library_params, snapshot};
use super::{fetch_rolling, head_commit, source_commit_exists, with_store};
use crate::agent_jobs::SubmitContext;
use crate::error::{DaemonError, Result};
use crate::store::Store;
use crate::store::agent_jobs::AgentJobRow;
use rsi_common::agent_jobs::{AgentJobV1, JobParams, JobState, JobWake};
use rsi_common::satellite_dispatch::SATELLITE_TARGET_NOT_AUTHORIZED;
use rsi_common::satellite_qa::{
    GIB_BYTES, SATELLITE_QA_BUSY, SATELLITE_QA_DEFAULT_TARGET_BYTES, SATELLITE_QA_DEPLOY_PENDING,
    SATELLITE_QA_DISK_LOW, SATELLITE_QA_INVALID, SATELLITE_QA_LAUNCH_FAILED,
    SATELLITE_QA_MIN_DISK_FREE_BYTES, SATELLITE_QA_NOT_FOUND, SATELLITE_QA_OWNER_REQUIRED,
    SATELLITE_QA_SHA_UNAVAILABLE, SatelliteQaAdmissionV1, SatelliteQaRunReceiptV1,
    SatelliteQaRunRequestV1, is_full_sha,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uuid::Uuid;

/// The job name carries the pinned SHA, so a replay reports it durably.
const NAME_PREFIX: &str = "Hub full library QA ";
/// Hub run snapshots are `<jobs>/rolling-library-qa/hub-<run id>/source`.
const HUB_DIR_PREFIX: &str = "hub-";
/// #2176: `<jobs>/rolling-library-qa/<this>` records the bytes the shared QA
/// target held after the last completed hub run (a decimal number). Not
/// `hub-` prefixed, so snapshot pruning never removes it.
const TARGET_BYTES_RECORD: &str = ".hub-run-target-bytes";

/// One start at a time: a retried start of the same run waits for the first
/// snapshot instead of racing it.
static START: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn run_key(run_id: Uuid) -> String {
    format!("hub-library-qa:{run_id}")
}

fn run_sha(job: &AgentJobV1) -> Option<String> {
    job.name
        .as_deref()?
        .strip_prefix(NAME_PREFIX)
        .filter(|sha| is_full_sha(sha))
        .map(str::to_owned)
}

fn refused(code: &str) -> DaemonError {
    DaemonError::PolicyDenied(code.into())
}

/// A failure before the job was recorded (the snapshot or the checkout): no
/// run exists, so it is reported with the listed non-effect code instead of
/// the raw text (which the hub could only read as uncertain, #2156).
fn not_started(error: DaemonError) -> DaemonError {
    match error {
        refusal @ DaemonError::PolicyDenied(_) => refusal,
        other => {
            tracing::warn!(error = %other, "hub QA run: nothing was started");
            refused(SATELLITE_QA_LAUNCH_FAILED)
        }
    }
}

/// The outcome of a failed submit (#2156), decided by the durable job row, not
/// by the error text. A recorded job that is still queued or running keeps a
/// live run (the launch error left its unit alive and the stop could not be
/// confirmed): it is reported by receipt so the hub reconciles that one run.
/// Anything else started nothing, or settled the launch failed with its unit
/// confirmed stopped: the listed non-effect code.
fn after_failed_submit(store: &Store, key: &str, error: &DaemonError) -> Result<AgentJobRow> {
    match store.find_agent_job_by_key(key)? {
        Some(row) if matches!(row.job.state, JobState::Queued | JobState::Running) => {
            tracing::warn!(job_id = %row.job.id, %error, "hub QA run: launch error left the job alive; reporting it");
            Ok(row)
        }
        _ => {
            tracing::warn!(%error, "hub QA run: nothing is running after the launch error");
            Err(refused(SATELLITE_QA_LAUNCH_FAILED))
        }
    }
}

fn receipt(run_id: Uuid, job: &AgentJobV1, replayed: bool) -> Result<SatelliteQaRunReceiptV1> {
    let sha = run_sha(job).ok_or_else(|| refused(SATELLITE_QA_NOT_FOUND))?;
    let result = job.result.clone().unwrap_or_default();
    Ok(SatelliteQaRunReceiptV1 {
        run_id,
        sha,
        job_id: job.id,
        state: job.state,
        exit_code: job.exit_code,
        refusal: result.refusal,
        failing_tests: result.failing_tests,
        failing_tests_omitted: result.failing_tests_omitted,
        log_path: job.log_path.clone(),
        replayed,
    }
    .bounded())
}

/// What the build governor would do with a new build now: its disk gate (the
/// operator's `min_free_disk_gb` on the filesystem it measures) and whether
/// any other gate would hold it (load, memory, workers slice, full build
/// slots or an earlier queued build).
#[derive(Clone, Copy, Debug)]
pub(crate) struct BuildGate {
    /// Free bytes where the governor measures disk; unknown counts as none.
    pub(crate) disk_free_bytes: Option<u64>,
    /// The governor policy's disk floor in GiB.
    pub(crate) disk_floor_gib: u64,
    /// Another governor gate would hold a new build.
    pub(crate) other_closed: bool,
}

impl BuildGate {
    /// Read the live governor's snapshot (the same sample and policy a build
    /// slot request meets).
    pub(crate) fn from_snapshot(snapshot: &crate::governor::GovernorSnapshot) -> Self {
        let free = snapshot.sample.disk_free_gb;
        let disk_free_bytes =
            (free.is_finite() && free >= 0.0).then(|| (free * GIB_BYTES as f64) as u64);
        let other_closed = snapshot
            .margins
            .iter()
            .any(|margin| !margin.open && margin.gate != "disk_free_gb")
            || snapshot.build_in_use >= snapshot.policy.build_slots
            || snapshot
                .queue
                .iter()
                .any(|waiter| waiter.class == crate::governor::AdmissionClass::Build);
        Self {
            disk_free_bytes,
            disk_floor_gib: snapshot.policy.min_free_disk_gb,
            other_closed,
        }
    }

    /// The floor a run needs: the governor's, never below the QA floor.
    pub(crate) fn floor_bytes(&self) -> u64 {
        self.disk_floor_gib
            .saturating_mul(GIB_BYTES)
            .max(SATELLITE_QA_MIN_DISK_FREE_BYTES)
    }
}

/// This satellite's guards at the moment of a start.
pub(crate) struct QaHost {
    /// `<data dir>/jobs`.
    pub(crate) jobs: PathBuf,
    /// Host-load admission holds new work now.
    pub(crate) load_hold: bool,
    /// A deploy drains: the job is recorded `queued` and launched after it.
    pub(crate) draining: bool,
    /// Free bytes on the data filesystem; unknown counts as too little.
    pub(crate) disk_free: Option<u64>,
    /// The build governor's gates for the run's job.
    pub(crate) build: BuildGate,
    /// #2176: the target the run needs above the governor's floor
    /// ([`expected_target_bytes`]).
    pub(crate) target_bytes: u64,
    /// Idle pool bytes on the governor filesystem; rechecked by reclaim at start.
    pub(crate) reclaimable_bytes: u64,
    /// The same pool can cover the data floor only on the same filesystem.
    pub(crate) data_reclaimable_bytes: u64,
}

fn is_library_qa(params: &JobParams) -> bool {
    matches!(params, JobParams::Test(test) if test.recipe.as_deref() == Some(RECIPE))
}

/// #2176: whether `job` is a hub's full-library QA run on this satellite (a
/// guest that any staged deploy stops at once).
pub(crate) fn is_hub_run(job: &AgentJobV1) -> bool {
    is_library_qa(&job.params) && run_sha(job).is_some()
}

/// #2176: the bytes the shared QA target held after the last completed hub
/// run, if one was measured.
fn last_target_bytes(jobs: &Path) -> Option<u64> {
    std::fs::read_to_string(jobs.join(SNAPSHOTS).join(TARGET_BYTES_RECORD))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// #2176: the target a new run needs above the governor's floor: the last
/// measured size, else the first-run default.
pub(crate) fn expected_target_bytes(jobs: &Path) -> u64 {
    last_target_bytes(jobs).unwrap_or(SATELLITE_QA_DEFAULT_TARGET_BYTES)
}

/// Whether a full-library QA job (a hub run or this satellite's own queue
/// run) is queued or running: they share one cargo target.
pub(crate) fn library_qa_active(store: &Store) -> Result<bool> {
    let running = store.list_running_agent_jobs()?;
    let queued = store.list_queued_agent_jobs()?;
    Ok(running
        .iter()
        .chain(queued.iter())
        .any(|row| is_library_qa(&row.job.params)))
}

/// The recorded run for `request`, after the hub allowlist check.
fn recorded(store: &Store, request: &SatelliteQaRunRequestV1) -> Result<Option<AgentJobRow>> {
    if !store
        .inbound_scope()?
        .allows_hub(request.hub_installation_id.0)
    {
        return Err(refused(SATELLITE_TARGET_NOT_AUTHORIZED));
    }
    store.find_agent_job_by_key(&run_key(request.run_id))
}

/// Who owns a new run and where its repository is.
struct Admitted {
    owner: Uuid,
    project_id: Option<Uuid>,
    repo: PathBuf,
}

/// Why a new run would not enter now, or `None`: no live scope seat, a
/// deploy staged or restarting here (#2176), too little disk for the QA floor
/// or for the build governor's floor plus the run's own target (#2176), or
/// any other hold (host-load admission, another governor gate, another
/// full-library job).
fn refusal(store: &Store, host: &QaHost) -> Result<Option<&'static str>> {
    if store.inbound_scope()?.first_live().is_none() {
        return Ok(Some(SATELLITE_QA_OWNER_REQUIRED));
    }
    if store.live_agent_deploy()?.is_some() {
        return Ok(Some(SATELLITE_QA_DEPLOY_PENDING));
    }
    let data_low = host.disk_free.is_none_or(|free| {
        free.saturating_add(host.data_reclaimable_bytes) < SATELLITE_QA_MIN_DISK_FREE_BYTES
    });
    let need = host.build.floor_bytes().saturating_add(host.target_bytes);
    let build_low = host
        .build
        .disk_free_bytes
        .is_none_or(|free| free.saturating_add(host.reclaimable_bytes) < need);
    if data_low || build_low {
        return Ok(Some(SATELLITE_QA_DISK_LOW));
    }
    if host.load_hold || host.build.other_closed || library_qa_active(store)? {
        return Ok(Some(SATELLITE_QA_BUSY));
    }
    Ok(None)
}

/// The admission reported on this satellite's health (#2120): the same
/// check a start rechecks.
pub(crate) fn admission(store: &Store, host: &QaHost) -> Result<SatelliteQaAdmissionV1> {
    Ok(SatelliteQaAdmissionV1 {
        refusal: refusal(store, host)?.map(str::to_owned),
        build_disk_free_bytes: host.build.disk_free_bytes,
        build_disk_reclaimable_bytes: host.reclaimable_bytes,
        build_disk_floor_bytes: host.build.floor_bytes(),
        target_bytes: Some(host.target_bytes),
    })
}

fn admit(store: &Store, host: &QaHost) -> Result<Admitted> {
    if let Some(code) = refusal(store, host)? {
        return Err(refused(code));
    }
    let (_, owner) = store
        .inbound_scope()?
        .first_live()
        .ok_or_else(|| refused(SATELLITE_QA_OWNER_REQUIRED))?;
    let session = store
        .get_session(owner)?
        .ok_or_else(|| refused(SATELLITE_QA_OWNER_REQUIRED))?;
    Ok(Admitted {
        owner,
        project_id: session.project_id,
        repo: session.working_dir,
    })
}

/// Have `sha` in `repo`, fetching the published `origin/rolling` once.
fn ensure_commit(repo: &Path, sha: &str) -> Result<()> {
    if source_commit_exists(repo, sha) {
        return Ok(());
    }
    if let Err(error) = fetch_rolling(repo) {
        tracing::warn!(%error, "hub QA run: fetching origin/rolling failed");
    }
    if source_commit_exists(repo, sha) {
        return Ok(());
    }
    Err(refused(SATELLITE_QA_SHA_UNAVAILABLE))
}

/// Remove earlier hub run snapshots (never this satellite's own queue
/// snapshots). Called only while no full-library QA job is active.
fn prune_hub_snapshots(parent: &Path, keep: &str) {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with(HUB_DIR_PREFIX) || name == keep {
            continue;
        }
        if let Err(error) = std::fs::remove_dir_all(entry.path()) {
            tracing::warn!(%error, dir = %name, "hub QA run: snapshot cleanup failed");
        }
    }
}

/// Pin the run's private snapshot and return it with the commit it pins (an
/// existing snapshot of this run keeps its original commit).
fn prepare(repo: &Path, sha: &str, jobs: &Path, run_id: Uuid) -> Result<(PathBuf, String)> {
    ensure_commit(repo, sha)?;
    let parent = jobs.join(SNAPSHOTS);
    let dir = format!("{HUB_DIR_PREFIX}{run_id}");
    prune_hub_snapshots(&parent, &dir);
    let cwd = snapshot(repo, sha, &parent.join(&dir))?;
    let pinned = head_commit(&cwd)
        .filter(|head| is_full_sha(head))
        .ok_or_else(|| refused(SATELLITE_QA_SHA_UNAVAILABLE))?;
    Ok((cwd, pinned))
}

/// Start (or report) one hub run. `submit` records and launches the job under
/// the store lock (production: `agent_jobs::submit_with_hold`); its `bool` is
/// the deploy-drain hold.
///
/// # Errors
/// `target_not_authorized`, `satellite_qa_not_found` (a poll of an unknown
/// run), `satellite_qa_owner_required`, `satellite_qa_busy`,
/// `satellite_qa_disk_low`, `satellite_qa_sha_unavailable`, or a launch error.
pub(crate) async fn request_hub_qa_run<F>(
    store: &Arc<tokio::sync::Mutex<Store>>,
    host: QaHost,
    request: SatelliteQaRunRequestV1,
    submit: F,
) -> Result<SatelliteQaRunReceiptV1>
where
    F: FnOnce(&Store, SubmitContext, bool) -> Result<AgentJobRow> + Send + 'static,
{
    request_hub_qa_run_with_reclaim(store, host, request, submit, |mut host| {
        crate::target_pool::reclaim_idle_for_qa();
        host.disk_free = crate::satellite::disk_free_bytes(&rsi_common::identity::data_dir());
        host.build.disk_free_bytes = crate::satellite::disk_free_bytes(Path::new("/"));
        host.reclaimable_bytes = 0;
        host.data_reclaimable_bytes = 0;
        host
    })
    .await
}

/// The reclaim seam lets fixtures exercise the real pool script with an isolated
/// pool root and controlled disk readings, without touching the installed pool.
async fn request_hub_qa_run_with_reclaim<F, R>(
    store: &Arc<tokio::sync::Mutex<Store>>,
    mut host: QaHost,
    request: SatelliteQaRunRequestV1,
    submit: F,
    reclaim: R,
) -> Result<SatelliteQaRunReceiptV1>
where
    F: FnOnce(&Store, SubmitContext, bool) -> Result<AgentJobRow> + Send + 'static,
    R: FnOnce(QaHost) -> QaHost + Send + 'static,
{
    request
        .validate()
        .map_err(|code| DaemonError::InvalidParam(code.into()))?;
    let run_id = request.run_id;
    let lookup = request.clone();
    if let Some(row) = with_store(store, move |s| recorded(s, &lookup)).await? {
        return receipt(run_id, &row.job, true);
    }
    if !request.start {
        return Err(refused(SATELLITE_QA_NOT_FOUND));
    }
    let _start = START.lock().await;
    let lookup = request.clone();
    let (existing, admitted) = {
        let store = store.lock().await;
        let existing = recorded(&store, &lookup)?;
        let admitted = existing
            .is_none()
            .then(|| admit(&store, &host))
            .transpose()?;
        (existing, admitted)
    };
    if let Some(row) = existing {
        return receipt(run_id, &row.job, true);
    }
    let admitted = admitted.ok_or_else(|| refused(SATELLITE_QA_INVALID))?;
    let need = host.build.floor_bytes().saturating_add(host.target_bytes);
    if host.build.disk_free_bytes.is_some_and(|free| free < need)
        || host
            .disk_free
            .is_some_and(|free| free < SATELLITE_QA_MIN_DISK_FREE_BYTES)
    {
        host = tokio::task::spawn_blocking(move || reclaim(host))
            .await
            .map_err(|error| not_started(DaemonError::Process(format!("pool reclaim: {error}"))))?;
        // Reclaim is advisory: a newly leased pool or hardlinks may have kept
        // its bytes allocated. Only actual free disk admits the launch.
        host.reclaimable_bytes = 0;
        host.data_reclaimable_bytes = 0;
        admit(&*store.lock().await, &host)?;
    }
    let (repo, sha, jobs) = (
        admitted.repo.clone(),
        request.sha.clone(),
        host.jobs.clone(),
    );
    let (cwd, pinned) = tokio::task::spawn_blocking(move || prepare(&repo, &sha, &jobs, run_id))
        .await
        .map_err(|error| not_started(DaemonError::Process(format!("hub QA run prepare: {error}"))))?
        .map_err(not_started)?;
    let ctx = SubmitContext {
        owner: admitted.owner,
        project_id: admitted.project_id,
        cwd,
        name: Some(format!("{NAME_PREFIX}{pinned}")),
        params: full_library_params(),
        idempotency_key: Some(run_key(run_id)),
        wake: JobWake::None,
    };
    let draining = host.draining;
    let key = run_key(run_id);
    let row = with_store(store, move |s| {
        if library_qa_active(s)? {
            return Err(refused(SATELLITE_QA_BUSY));
        }
        submit(s, ctx, draining).or_else(|error| after_failed_submit(s, &key, &error))
    })
    .await?;
    tracing::info!(%run_id, job_id = %row.job.id, sha = %pinned, "hub full-library QA run started");
    receipt(run_id, &row.job, false)
}

/// What one [`reclaim_settled_runs`] pass returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Reclaimed {
    /// The settled hub runs whose snapshots were removed.
    pub(crate) runs: Vec<Uuid>,
    /// What the shared target held before its build output was removed
    /// (`None`: never built, or too large to measure).
    pub(crate) target_bytes: Option<u64>,
}

/// The hub run snapshot directories under `parent`, with their run ids.
fn hub_run_dirs(parent: &Path) -> Vec<(Uuid, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter_map(|entry| {
            let name = entry.file_name();
            let run = Uuid::parse_str(name.to_str()?.strip_prefix(HUB_DIR_PREFIX)?).ok()?;
            Some((run, entry.path()))
        })
        .collect()
}

/// A settled run that built the whole workspace and ran its tests, so its
/// target size is what the next run will need.
fn completed(job: &AgentJobV1) -> bool {
    let result = job.result.clone().unwrap_or_default();
    match job.state {
        JobState::Succeeded => true,
        JobState::Failed => result.refusal.is_none() && !result.failing_tests.is_empty(),
        _ => false,
    }
}

/// #2176: a settled hub run returns its disk, success or not. While no
/// full-library QA job is queued or running (they share the target) and no
/// start is in progress, remove the shared QA target's build output, record
/// the size it had when a settled run completed (the next admission asks for
/// that much above the floor), then remove the settled runs' snapshots. A
/// busy target is retried on the next pass: the snapshots stay until it
/// succeeds. `target` resolves the shared target (production: the registered
/// allocation the runs build in, `agent_jobs::library_qa_target`).
pub(crate) async fn reclaim_settled_runs<T>(
    store: &Arc<tokio::sync::Mutex<Store>>,
    jobs: &Path,
    target: T,
) -> Result<Option<Reclaimed>>
where
    T: FnOnce() -> std::io::Result<PathBuf> + Send + 'static,
{
    let parent = jobs.join(SNAPSHOTS);
    let dirs = hub_run_dirs(&parent);
    if dirs.is_empty() {
        return Ok(None);
    }
    // A start pins its snapshot before it records its job: never race one. A
    // start that arrives meanwhile waits and then meets the returned disk.
    let Ok(_start) = START.try_lock() else {
        return Ok(None);
    };
    let settled = {
        let store = store.lock().await;
        if library_qa_active(&store)? {
            return Ok(None);
        }
        let mut settled = Vec::new();
        for (run, dir) in dirs {
            // No job: a start that failed before it recorded one. The next
            // start prunes that snapshot.
            if let Some(row) = store.find_agent_job_by_key(&run_key(run))?
                && row.job.state.is_terminal()
            {
                settled.push((run, dir, completed(&row.job)));
            }
        }
        settled
    };
    if settled.is_empty() {
        return Ok(None);
    }
    let record = parent.join(TARGET_BYTES_RECORD);
    tokio::task::spawn_blocking(move || -> Result<Option<Reclaimed>> {
        use crate::release_target::{OutputReclaim, reclaim_build_output};
        let target =
            target().map_err(|error| DaemonError::Process(format!("hub QA target: {error}")))?;
        let target_bytes = match reclaim_build_output(&target)? {
            OutputReclaim::Busy => return Ok(None),
            OutputReclaim::Absent => None,
            OutputReclaim::Reclaimed { bytes } => bytes,
        };
        if let Some(bytes) = target_bytes
            && settled.iter().any(|(_, _, completed)| *completed)
        {
            std::fs::write(&record, format!("{bytes}\n"))?;
        }
        let mut runs = Vec::new();
        for (run, dir, _) in settled {
            match std::fs::remove_dir_all(&dir) {
                Ok(()) => runs.push(run),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => runs.push(run),
                Err(error) => {
                    tracing::warn!(%error, %run, "hub QA run: snapshot cleanup failed");
                }
            }
        }
        Ok(Some(Reclaimed { runs, target_bytes }))
    })
    .await
    .map_err(|error| DaemonError::Process(format!("hub QA target reclaim: {error}")))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::agent_jobs::NewAgentJob;
    use chrono::Utc;
    use rsi_common::satellite::{SATELLITE_WIRE_VERSION_V1, SatelliteUuidV1};
    use rsi_common::satellite_dispatch::SatelliteInboundPolicyV1;

    // Each fixture has its own store, but starts and cleanup share START.
    // Keep test bodies exclusive so cleanup tests exercise settled runs
    // rather than the intentional nonblocking deferral for a concurrent start.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    static TEST_START: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    struct Rig {
        _temp: tempfile::TempDir,
        store: Arc<tokio::sync::Mutex<Store>>,
        hub: Uuid,
        jobs: PathBuf,
        sha: String,
        seat: Uuid,
    }

    /// A satellite whose live scope seat works in a repository with one
    /// commit, and which allowlists `hub`.
    fn rig() -> Rig {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        super::super::git(&repo, &["init", "-q"]).unwrap();
        std::fs::write(repo.join("value"), "qa").unwrap();
        super::super::git(&repo, &["add", "value"]).unwrap();
        super::super::git(
            &repo,
            &[
                "-c",
                "user.name=qa-fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-qm",
                "qa",
            ],
        )
        .unwrap();
        let sha = head_commit(&repo).unwrap();
        let store = Store::open_in_memory().unwrap();
        let mut seat = rsid_store::test_support::make_test_session();
        seat.status = rsi_common::SessionStatus::Completed;
        seat.working_dir = repo;
        store.insert_session(&seat).unwrap();
        let hub = Uuid::new_v4();
        store
            .put_satellite_inbound_policy(&SatelliteInboundPolicyV1 {
                allowed_hub_installations: vec![SatelliteUuidV1(hub)],
                scope_roots: vec![SatelliteUuidV1(seat.id)],
                ..Default::default()
            })
            .unwrap();
        Rig {
            jobs: temp.path().join("jobs"),
            _temp: temp,
            store: Arc::new(tokio::sync::Mutex::new(store)),
            hub,
            sha,
            seat: seat.id,
        }
    }

    /// A build governor with `free` GiB free against a `floor` GiB policy
    /// floor and every other gate open.
    fn gate(free: u64, floor: u64) -> BuildGate {
        BuildGate {
            disk_free_bytes: Some(free * GIB_BYTES),
            disk_floor_gib: floor,
            other_closed: false,
        }
    }

    /// A host with every gate open. Its run needs no target above the floor
    /// (the #2176 floor-plus-target tests set one).
    fn host(rig: &Rig) -> QaHost {
        QaHost {
            jobs: rig.jobs.clone(),
            load_hold: false,
            draining: false,
            disk_free: Some(SATELLITE_QA_MIN_DISK_FREE_BYTES),
            build: gate(31, 30),
            target_bytes: 0,
            reclaimable_bytes: 0,
            data_reclaimable_bytes: 0,
        }
    }

    fn host_with(rig: &Rig, build: BuildGate) -> QaHost {
        QaHost { build, ..host(rig) }
    }

    fn request(rig: &Rig, run_id: Uuid, start: bool) -> SatelliteQaRunRequestV1 {
        SatelliteQaRunRequestV1 {
            wire_version: SATELLITE_WIRE_VERSION_V1,
            hub_installation_id: SatelliteUuidV1(rig.hub),
            run_id,
            sha: rig.sha.clone(),
            start,
        }
    }

    /// Records the job row without launching a unit.
    fn record(store: &Store, ctx: SubmitContext, _hold: bool) -> Result<AgentJobRow> {
        let id = Uuid::new_v4();
        let (row, _) = store.insert_agent_job(
            &NewAgentJob {
                id,
                owner_session_id: ctx.owner,
                project_id: ctx.project_id,
                name: ctx.name,
                params: ctx.params,
                cwd: ctx.cwd.display().to_string(),
                unit_name: format!("qa-{id}"),
                log_path: format!("/jobs/{id}.log"),
                status_path: format!("/jobs/{id}.status"),
                idempotency_key: ctx.idempotency_key,
                wake: ctx.wake,
            },
            Utc::now(),
        )?;
        Ok(row)
    }

    fn code(result: Result<SatelliteQaRunReceiptV1>) -> String {
        match result.unwrap_err() {
            DaemonError::PolicyDenied(code) | DaemonError::InvalidParam(code) => code,
            other => panic!("unexpected error {other}"),
        }
    }

    /// The run is pinned under the shared QA snapshot directory (so the job
    /// reuses the one registered target), owned by the live scope seat, keyed
    /// by the hub's run id; a poll and a retried start report the same job.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_hub_run_is_pinned_in_the_shared_qa_directory_and_replays() {
        let _test_start = TEST_START.lock().await;
        let rig = rig();
        let run = Uuid::new_v4();
        let started = request_hub_qa_run(&rig.store, host(&rig), request(&rig, run, true), record)
            .await
            .unwrap();
        assert_eq!(started.sha, rig.sha);
        assert_eq!(started.state, JobState::Running);
        assert!(!started.replayed);
        let job = rig
            .store
            .lock()
            .await
            .get_agent_job(started.job_id)
            .unwrap()
            .unwrap()
            .job;
        assert_eq!(job.owner_session_id, rig.seat);
        let cwd = PathBuf::from(&job.cwd);
        assert_eq!(
            cwd,
            rig.jobs
                .join("rolling-library-qa")
                .join(format!("hub-{run}"))
                .join("source")
        );
        assert_eq!(head_commit(&cwd).as_deref(), Some(rig.sha.as_str()));
        assert!(is_library_qa(&job.params));
        for start in [false, true] {
            let again =
                request_hub_qa_run(&rig.store, host(&rig), request(&rig, run, start), record)
                    .await
                    .unwrap();
            assert_eq!(again.job_id, started.job_id);
            assert!(again.replayed);
        }
        // One full-library job at a time: a second run is refused busy.
        assert_eq!(
            code(
                request_hub_qa_run(
                    &rig.store,
                    host(&rig),
                    request(&rig, Uuid::new_v4(), true),
                    record
                )
                .await
            ),
            SATELLITE_QA_BUSY
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_hub_run_is_refused_by_the_satellites_own_guards() {
        let _test_start = TEST_START.lock().await;
        let rig = rig();
        let mut stranger = request(&rig, Uuid::new_v4(), true);
        stranger.hub_installation_id = SatelliteUuidV1(Uuid::new_v4());
        assert_eq!(
            code(request_hub_qa_run(&rig.store, host(&rig), stranger, record).await),
            SATELLITE_TARGET_NOT_AUTHORIZED
        );
        assert_eq!(
            code(
                request_hub_qa_run(
                    &rig.store,
                    host(&rig),
                    request(&rig, Uuid::new_v4(), false),
                    record
                )
                .await
            ),
            SATELLITE_QA_NOT_FOUND
        );
        let mut low = host(&rig);
        low.disk_free = Some(SATELLITE_QA_MIN_DISK_FREE_BYTES - 1);
        assert_eq!(
            code(
                request_hub_qa_run(&rig.store, low, request(&rig, Uuid::new_v4(), true), record)
                    .await
            ),
            SATELLITE_QA_DISK_LOW
        );
        let mut loaded = host(&rig);
        loaded.load_hold = true;
        assert_eq!(
            code(
                request_hub_qa_run(
                    &rig.store,
                    loaded,
                    request(&rig, Uuid::new_v4(), true),
                    record
                )
                .await
            ),
            SATELLITE_QA_BUSY
        );
        let mut missing = request(&rig, Uuid::new_v4(), true);
        missing.sha = "f".repeat(40);
        assert_eq!(
            code(request_hub_qa_run(&rig.store, host(&rig), missing, record).await),
            SATELLITE_QA_SHA_UNAVAILABLE
        );
        assert!(
            rig.store
                .lock()
                .await
                .list_running_agent_jobs()
                .unwrap()
                .is_empty()
        );
    }

    /// #179: the run is taken only when its job could enter this satellite's
    /// build governor. Free space between the QA floor (20 GiB) and the
    /// governor's floor (default 30) refuses, a higher configured governor
    /// floor is honoured, and any other closed governor gate is busy. The
    /// reported admission says the same thing the start rechecks.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_run_needs_the_build_governors_floor_not_only_the_qa_floor() {
        let _test_start = TEST_START.lock().await;
        let rig = rig();
        for (free, floor) in [(25, 30), (35, 40), (15, 10)] {
            let host = host_with(&rig, gate(free, floor));
            let reported = admission(&*rig.store.lock().await, &host).unwrap();
            assert_eq!(reported.refusal.as_deref(), Some(SATELLITE_QA_DISK_LOW));
            assert_eq!(
                reported.build_disk_floor_bytes,
                (floor * GIB_BYTES).max(SATELLITE_QA_MIN_DISK_FREE_BYTES)
            );
            assert_eq!(reported.build_disk_free_bytes, Some(free * GIB_BYTES));
            let start = request(&rig, Uuid::new_v4(), true);
            assert_eq!(
                code(request_hub_qa_run(&rig.store, host, start, record).await),
                SATELLITE_QA_DISK_LOW,
                "{free} GiB free against a {floor} GiB governor floor"
            );
        }
        let mut held = gate(50, 30);
        held.other_closed = true;
        let start = request(&rig, Uuid::new_v4(), true);
        assert_eq!(
            code(request_hub_qa_run(&rig.store, host_with(&rig, held), start, record).await),
            SATELLITE_QA_BUSY
        );
        assert!(
            rig.store
                .lock()
                .await
                .list_running_agent_jobs()
                .unwrap()
                .is_empty()
        );
        // Above the configured higher floor the run enters.
        let open = host_with(&rig, gate(41, 40));
        assert_eq!(
            admission(&*rig.store.lock().await, &open).unwrap().refusal,
            None
        );
        let started = request_hub_qa_run(
            &rig.store,
            open,
            request(&rig, Uuid::new_v4(), true),
            record,
        )
        .await
        .unwrap();
        assert_eq!(started.sha, rig.sha);
    }
    /// A runtime whose service start errors after dispatch, whose unit is
    /// alive and whose stop cannot be confirmed: `submit_with_hold` returns an
    /// error but deliberately leaves the job `running` (#2156; the contract
    /// `agent_jobs::tests::an_uncertain_launch_keeps_its_running_row_and_scratch_for_recovery`
    /// pins).
    struct UncertainLaunch;

    impl crate::agent_jobs::JobRuntime for UncertainLaunch {
        fn launch(&self, _: &crate::agent_jobs::LaunchSpec) -> std::result::Result<(), String> {
            Err("controller unavailable after dispatch".into())
        }
        fn unit_active(&self, _: &str) -> bool {
            true
        }
        fn stop_unit(&self, _: &str) -> std::result::Result<(), String> {
            Err("controller unavailable".into())
        }
    }

    /// The production submit with [`UncertainLaunch`]. The run's key, owner,
    /// name and snapshot are kept; the recipe becomes a plain build job so the
    /// test needs neither the repository's recipe declaration nor a registered
    /// recipe build target in the host's data directory.
    fn uncertain_submit(
        jobs: PathBuf,
    ) -> impl FnOnce(&Store, SubmitContext, bool) -> Result<AgentJobRow> + Send + 'static {
        move |store, mut ctx, hold| {
            ctx.params = rsi_common::agent_jobs::AgentSubmitJobRequestV1 {
                project_id: None,
                sandbox_session_id: None,
                kind: rsi_common::agent_jobs::JobKind::Build,
                params: serde_json::json!({"command": "check", "workspace": true}),
                name: None,
                idempotency_key: None,
                worktree: None,
                wake: None,
            }
            .typed_params()
            .expect("valid build params");
            let tools = crate::agent_jobs::JobTools {
                cargo_slot: "unused".into(),
                lander: "unused".into(),
            };
            crate::agent_jobs::submit_with_hold(
                store,
                &UncertainLaunch,
                &tools,
                &jobs,
                ctx,
                hold,
                Utc::now(),
            )
            .map(|(row, _)| row)
        }
    }

    /// #2156: the service start errored after dispatch and stopping the live
    /// unit failed, so the satellite keeps the job running. The start answers
    /// with that job's receipt (never a refusal the hub would place elsewhere);
    /// a retried start or a poll reports the same job and submits nothing.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_launch_error_that_left_the_job_alive_is_reported_by_its_receipt() {
        let _test_start = TEST_START.lock().await;
        let rig = rig();
        let run = Uuid::new_v4();
        let started = request_hub_qa_run(
            &rig.store,
            host(&rig),
            request(&rig, run, true),
            uncertain_submit(rig.jobs.clone()),
        )
        .await
        .unwrap();
        assert_eq!(started.sha, rig.sha);
        assert_eq!(started.state, JobState::Running);
        assert!(!started.replayed);
        let running = rig.store.lock().await.list_running_agent_jobs().unwrap();
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].job.id, started.job_id);
        for start in [false, true] {
            let again = request_hub_qa_run(
                &rig.store,
                host(&rig),
                request(&rig, run, start),
                |_, _, _| panic!("a replay of a recorded run must not submit again"),
            )
            .await
            .unwrap();
            assert_eq!(again.job_id, started.job_id);
            assert_eq!(again.state, JobState::Running);
            assert!(again.replayed);
        }
        assert_eq!(
            rig.store
                .lock()
                .await
                .list_running_agent_jobs()
                .unwrap()
                .len(),
            1
        );
    }

    /// #2156: the satellite refuses with the listed non-effect code only when
    /// nothing runs: the submit failed before any job was recorded, the launch
    /// settled the recorded job failed with its unit stopped, or the snapshot
    /// could not be made.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_start_that_left_nothing_running_is_refused_with_the_listed_code() {
        let _test_start = TEST_START.lock().await;
        let rig = rig();
        let nothing_recorded = request_hub_qa_run(
            &rig.store,
            host(&rig),
            request(&rig, Uuid::new_v4(), true),
            |_, _, _| Err(DaemonError::InvalidParam("job_recipe_not_allowed".into())),
        )
        .await;
        assert_eq!(code(nothing_recorded), SATELLITE_QA_LAUNCH_FAILED);
        let settled_failed = request_hub_qa_run(
            &rig.store,
            host(&rig),
            request(&rig, Uuid::new_v4(), true),
            |store, ctx, hold| {
                let row = record(store, ctx, hold)?;
                store.settle_agent_job(
                    row.job.id,
                    JobState::Failed,
                    &rsi_common::agent_jobs::AgentJobResultV1::default(),
                    false,
                    Utc::now(),
                )?;
                Err(DaemonError::Process(
                    "job_launch_failed: unit start refused".into(),
                ))
            },
        )
        .await;
        assert_eq!(code(settled_failed), SATELLITE_QA_LAUNCH_FAILED);
        assert!(
            rig.store
                .lock()
                .await
                .list_running_agent_jobs()
                .unwrap()
                .is_empty()
        );
        // The snapshot cannot be made (the jobs directory is a file).
        std::fs::remove_dir_all(&rig.jobs).unwrap();
        std::fs::write(&rig.jobs, "not a directory").unwrap();
        assert_eq!(
            code(
                request_hub_qa_run(
                    &rig.store,
                    host(&rig),
                    request(&rig, Uuid::new_v4(), true),
                    record
                )
                .await
            ),
            SATELLITE_QA_LAUNCH_FAILED
        );
    }
    /// #2176: the deploy wins. While a deploy is staged here no run is taken:
    /// the reported admission says so (the hub waits for it) and a start is
    /// refused with the listed non-effect code, recording no job.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_pending_deploy_refuses_a_new_run() {
        let _test_start = TEST_START.lock().await;
        let rig = rig();
        rig.store
            .lock()
            .await
            .insert_agent_deploy(
                &crate::store::agent_deploys::NewDeploy {
                    id: Uuid::new_v4(),
                    owner_session_id: rig.seat,
                    idempotency_key: "hub-deploy",
                    sha: "0123456789abcdef0123456789abcdef01234567",
                    fingerprint: crate::store::agent_deploys::deploy_fingerprint_with(
                        "0123456789abcdef0123456789abcdef01234567",
                        "x",
                        3600,
                        false,
                    ),
                    manifest: &[],
                    max_wait_secs: 3600,
                    interrupt_workers: false,
                },
                Utc::now(),
            )
            .unwrap();
        let reported = admission(&*rig.store.lock().await, &host(&rig)).unwrap();
        assert_eq!(
            reported.refusal.as_deref(),
            Some(SATELLITE_QA_DEPLOY_PENDING)
        );
        assert_eq!(reported.admits(), Err(SATELLITE_QA_DEPLOY_PENDING));
        let start = request(&rig, Uuid::new_v4(), true);
        assert_eq!(
            code(request_hub_qa_run(&rig.store, host(&rig), start, record).await),
            SATELLITE_QA_DEPLOY_PENDING
        );
        assert!(
            rig.store
                .lock()
                .await
                .list_running_agent_jobs()
                .unwrap()
                .is_empty()
        );
    }

    /// #2176: the governor floor is what this satellite's workers need, so a
    /// run also needs room for its own target above it: the last measured
    /// size, else the 25 GiB first-run default. The admission reports it.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_run_needs_the_floor_plus_its_target() {
        let _test_start = TEST_START.lock().await;
        let rig = rig();
        assert_eq!(
            expected_target_bytes(&rig.jobs),
            SATELLITE_QA_DEFAULT_TARGET_BYTES
        );
        let mut tight = host_with(&rig, gate(54, 30));
        tight.target_bytes = expected_target_bytes(&rig.jobs);
        let reported = admission(&*rig.store.lock().await, &tight).unwrap();
        assert_eq!(reported.refusal.as_deref(), Some(SATELLITE_QA_DISK_LOW));
        assert_eq!(
            reported.target_bytes,
            Some(SATELLITE_QA_DEFAULT_TARGET_BYTES)
        );
        assert_eq!(
            code(
                request_hub_qa_run(
                    &rig.store,
                    tight,
                    request(&rig, Uuid::new_v4(), true),
                    record
                )
                .await
            ),
            SATELLITE_QA_DISK_LOW
        );
        let mut roomy = host_with(&rig, gate(55, 30));
        roomy.target_bytes = SATELLITE_QA_DEFAULT_TARGET_BYTES;
        assert_eq!(
            admission(&*rig.store.lock().await, &roomy).unwrap().refusal,
            None
        );
        // A measured size replaces the default.
        std::fs::create_dir_all(rig.jobs.join(SNAPSHOTS)).unwrap();
        std::fs::write(
            rig.jobs.join(SNAPSHOTS).join(TARGET_BYTES_RECORD),
            format!("{}\n", 21 * GIB_BYTES),
        )
        .unwrap();
        assert_eq!(expected_target_bytes(&rig.jobs), 21 * GIB_BYTES);
    }

    /// Isolated pool fixture: the marker estimates 16 GiB without allocating
    /// that much disk. The free-space sampler below models its reclamation.
    #[cfg(all(
        target_os = "linux",
        any(not(feature = "test-shard-mode"), feature = "test-shard-other-01")
    ))]
    fn pool_fixture(rig: &Rig) -> (PathBuf, PathBuf) {
        let root = rig._temp.path().join("pools");
        let directory = root.join("repo");
        let target = directory.join("idle-k/target");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("blob"), "fixture").unwrap();
        std::fs::write(
            directory.join("idle-k.json"),
            serde_json::to_vec(&serde_json::json!({
                "name": "idle-k", "sha": "idle", "key": "k", "state": "ready",
                "target": target, "worktree": directory.join("idle-k"),
                "built_at": 1, "size_bytes": 16 * GIB_BYTES,
            }))
            .unwrap(),
        )
        .unwrap();
        (root, directory)
    }

    #[cfg(all(
        target_os = "linux",
        any(not(feature = "test-shard-mode"), feature = "test-shard-other-01")
    ))]
    fn pool_report(root: &Path, action: &str) -> serde_json::Value {
        let output = std::process::Command::new("python3")
            .arg(Path::new(rsi_common::manifest_dir!()).join("../../scripts/target-pool"))
            .arg(action)
            .arg("--filesystem")
            .arg(root)
            .env("RSI_TARGET_POOL_ROOT", root)
            .env_remove("RSI_SESSION_TOKEN")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    /// #387: use the real reclaim-idle path against a fixture only. Health
    /// counts idle bytes, start reclaims and remeasures, and leases always win.
    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn qa_reclaims_an_idle_pool_only_when_it_needs_and_can_return_the_disk() {
        let _test_start = TEST_START.lock().await;
        for (case, free, leased_at_health, lease_at_start, disk_returned, starts, pools) in [
            ("idle", 40, false, false, true, true, 0),
            ("in-use", 40, true, false, true, false, 1),
            ("free-alone", 55, false, false, true, true, 1),
            ("leased-since-health", 40, false, true, true, false, 1),
            ("hardlinks-retain-disk", 40, false, false, false, false, 0),
        ] {
            let rig = rig();
            let (root, directory) = pool_fixture(&rig);
            let lease = std::fs::File::create(directory.join("idle-k.use")).unwrap();
            if leased_at_health {
                lease.lock_shared().unwrap();
            }
            let mut host = host_with(&rig, gate(free, 30));
            host.target_bytes = SATELLITE_QA_DEFAULT_TARGET_BYTES;
            host.reclaimable_bytes = pool_report(&root, "summary")["reclaimable_bytes"]
                .as_u64()
                .unwrap();
            let health = admission(&*rig.store.lock().await, &host).unwrap();
            assert_eq!(
                health.build_disk_free_bytes,
                Some(free * GIB_BYTES),
                "{case}"
            );
            assert_eq!(
                health.build_disk_reclaimable_bytes,
                if leased_at_health { 0 } else { 16 * GIB_BYTES },
                "{case}"
            );
            assert_eq!(health.admits().is_ok(), !leased_at_health, "{case}");
            if lease_at_start {
                lease.lock_shared().unwrap();
            }
            let reclaim_root = root.clone();
            let result = request_hub_qa_run_with_reclaim(
                &rig.store,
                host,
                request(&rig, Uuid::new_v4(), true),
                record,
                move |mut host| {
                    let report = pool_report(&reclaim_root, "reclaim-idle");
                    assert_eq!(report["reason"], "pool_reclaimed_for_disk_floor");
                    if disk_returned && report["removed"].as_array().unwrap().len() == 1 {
                        host.build.disk_free_bytes = Some(56 * GIB_BYTES);
                    }
                    host
                },
            )
            .await;
            if starts {
                let started = result.unwrap();
                assert_eq!(started.state, JobState::Running, "{case}");
                assert_eq!(started.sha, rig.sha, "{case}");
            } else {
                assert_eq!(code(result), SATELLITE_QA_DISK_LOW, "{case}");
                assert_eq!(
                    rig.store
                        .lock()
                        .await
                        .list_running_agent_jobs()
                        .unwrap()
                        .len(),
                    0,
                    "{case}"
                );
            }
            assert_eq!(pool_report(&root, "summary")["pools"], pools, "{case}");
            if pools == 1 {
                assert!(directory.join("idle-k/target/blob").is_file(), "{case}");
            }
        }
    }

    /// A registered QA target fixture holding `bytes` of build output.
    fn target_fixture(dir: &Path, bytes: usize) {
        std::fs::create_dir_all(dir.join("debug/deps")).unwrap();
        std::fs::write(dir.join("debug/deps/libbig.rlib"), vec![7u8; bytes]).unwrap();
        std::fs::write(dir.join("CACHEDIR.TAG"), b"").unwrap();
    }

    /// #2176: a settled hub run returns its disk, success or not. While the
    /// run is active nothing is touched; once it settled the shared target's
    /// build output and the run's snapshot are removed (the allocation's
    /// files stay), and a completed run's measured size becomes the next
    /// admission's target. A run stopped by a deploy returns its disk too but
    /// leaves the recorded size alone (its target was partial).
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_settled_run_returns_its_target_and_records_its_size() {
        let _test_start = TEST_START.lock().await;
        let rig = rig();
        let target = rig.jobs.parent().unwrap().join("qa-target");
        let resolve = |target: &Path| {
            let target = target.to_path_buf();
            move || Ok::<_, std::io::Error>(target)
        };
        let run = Uuid::new_v4();
        let started = request_hub_qa_run(&rig.store, host(&rig), request(&rig, run, true), record)
            .await
            .unwrap();
        target_fixture(&target, 4096);
        let snapshot = rig.jobs.join(SNAPSHOTS).join(format!("hub-{run}"));
        assert!(snapshot.join("source").is_dir());
        assert_eq!(
            reclaim_settled_runs(&rig.store, &rig.jobs, resolve(&target))
                .await
                .unwrap(),
            None,
            "an active run keeps its target"
        );
        assert!(target.join("debug/deps/libbig.rlib").exists());
        rig.store
            .lock()
            .await
            .settle_agent_job(
                started.job_id,
                JobState::Succeeded,
                &rsi_common::agent_jobs::AgentJobResultV1::default(),
                false,
                Utc::now(),
            )
            .unwrap();
        let reclaimed = reclaim_settled_runs(&rig.store, &rig.jobs, resolve(&target))
            .await
            .unwrap()
            .expect("the settled run is reclaimed");
        assert_eq!(reclaimed.runs, [run]);
        assert_eq!(reclaimed.target_bytes, Some(4096));
        assert!(!target.join("debug").exists(), "build output removed");
        assert!(target.join("CACHEDIR.TAG").is_file(), "allocation kept");
        assert!(!snapshot.exists(), "snapshot removed");
        assert_eq!(expected_target_bytes(&rig.jobs), 4096);
        // The settled run still answers its poll from the job row.
        let polled = request_hub_qa_run(&rig.store, host(&rig), request(&rig, run, false), record)
            .await
            .unwrap();
        assert_eq!(polled.job_id, started.job_id);
        assert_eq!(polled.state, JobState::Succeeded);
        assert_eq!(
            reclaim_settled_runs(&rig.store, &rig.jobs, resolve(&target))
                .await
                .unwrap(),
            None,
            "nothing left to reclaim"
        );

        // A run the deploy stopped: disk returned, recorded size unchanged.
        let preempted = Uuid::new_v4();
        let second = request_hub_qa_run(
            &rig.store,
            host(&rig),
            request(&rig, preempted, true),
            record,
        )
        .await
        .unwrap();
        target_fixture(&target, 100);
        rig.store
            .lock()
            .await
            .settle_agent_job(
                second.job_id,
                JobState::Failed,
                &rsi_common::agent_jobs::AgentJobResultV1 {
                    refusal: Some(rsi_common::agent_jobs::JOB_INTERRUPTED_BY_DEPLOY.into()),
                    ..Default::default()
                },
                false,
                Utc::now(),
            )
            .unwrap();
        let reclaimed = reclaim_settled_runs(&rig.store, &rig.jobs, resolve(&target))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reclaimed.runs, [preempted]);
        assert!(!target.join("debug").exists());
        assert_eq!(expected_target_bytes(&rig.jobs), 4096);
    }
}
