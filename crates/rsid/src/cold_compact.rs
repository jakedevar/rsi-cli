//! The one-time cold-archive compaction at a supervised restart (#2384, slice
//! S4; plan §5, review #2398).
//!
//! `VACUUM` needs a temporary database, and `SQLite` keeps the directory for it
//! in process-global state that a multithreaded daemon cannot change safely. So
//! the live daemon never compacts:
//!
//! 1. **Decision** ([`decide_at_quiet_point`]): the deploy loop, at a supervised
//!    restart's quiet point and under the same store lock as the quiet
//!    judgement, claims a compaction (an operator's request, or a due one while
//!    the archive is on) and records it in-process ([`CompactOnExit`]). A restart
//!    that then does not happen gives it back ([`abandon`]).
//! 2. **Launch** ([`compact_before_exit`]): after the graceful shutdown and the
//!    runtime teardown, `main` first proves the database is quiet
//!    ([`quiesce_then_run`]): the decision closed cold-archive admission and asked
//!    any running pass to stop at its next safe boundary
//!    ([`crate::cold_archive_lifecycle`]), and the launch waits, bounded by
//!    [`QUIESCE_WAIT`], until every pass thread has returned with its SQLite
//!    handles closed (a pass runs on its own OS thread that the runtime teardown
//!    does not stop). Only then does it spawn the executable captured at startup
//!    in the hidden mode `rsid cold-archive-compact ...` with `SQLITE_TMPDIR`,
//!    `TMPDIR`, `TMP` and `TEMP` pointing at `<data_dir>/tmp/compact-<run_id>` (on
//!    the database's own file system), waits at most [`COMPACT_DEADLINE`], kills
//!    and reaps the helper past it, and exits 75 whatever happened. If a pass
//!    outlives the wait the compaction is **deferred**: no helper starts, a result
//!    file records the deferral, the next daemon's reconcile puts the request back,
//!    and the exit is still 75. A deploy is never stranded: its row is already
//!    `restarting` and startup verification settles it.
//! 3. **Helper** ([`run_helper`]): a raw connection, no migration, no daemon; it
//!    writes its result file and exits.
//! 4. **Reconcile**: the next daemon's cold-archive loop, after `request_ready`,
//!    settles the run row from that file (`compact_interrupted` without one).

use crate::cold_archive_lifecycle::{self, ArchiveLifecycle, QUIESCE_WAIT, Quiescence};
use crate::store::Store;
use crate::store::cold_archive::reclaim::{
    COMPACT_DEADLINE, COMPACT_DEFERRED_OUTCOME, COMPACT_HELPER_ARG, CompactResult, compact_paths,
    run_compaction, write_compact_result,
};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// The compaction this process runs after its graceful shutdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactOnExit {
    pub run_id: Uuid,
    pub db_path: PathBuf,
    pub tmp_dir: PathBuf,
    pub result_path: PathBuf,
    pub deadline: Duration,
}

/// How the helper ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HelperOutcome {
    /// It exited by itself (its exit code; `None` when a signal ended it).
    Exited(Option<i32>),
    /// It outlived the deadline: killed and reaped.
    Killed,
    /// It could not be started.
    SpawnFailed(String),
    /// It was not started: a cold-archive pass thread had not settled within the
    /// shutdown wait, so the database was not provably quiet. The request is
    /// put back by the next daemon's reconcile.
    Deferred(String),
}

static COMPACT_ON_EXIT: Mutex<Option<CompactOnExit>> = Mutex::new(None);
static DAEMON_EXE: OnceLock<PathBuf> = OnceLock::new();
static AUTO_COMPACTION: AtomicBool = AtomicBool::new(false);

fn slot() -> std::sync::MutexGuard<'static, Option<CompactOnExit>> {
    COMPACT_ON_EXIT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Records the path of this executable. Called first thing in `main`: after a
/// deploy swaps the binary, the same path names the new build, which is the one
/// the helper should run.
pub fn capture_executable() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = DAEMON_EXE.set(exe);
    }
}

/// Whether a due compaction may be claimed without an operator's request: the
/// deploy loop mirrors `cold_archive_enabled` here on every tick.
pub fn set_auto_compaction(enabled: bool) {
    AUTO_COMPACTION.store(enabled, Ordering::Relaxed);
}

#[must_use]
pub fn auto_compaction() -> bool {
    AUTO_COMPACTION.load(Ordering::Relaxed)
}

/// The run this process scheduled for its own exit, if any (the reconcile
/// leaves it alone).
#[must_use]
pub fn scheduled_run_id() -> Option<Uuid> {
    slot().as_ref().map(|plan| plan.run_id)
}

/// Takes the scheduled compaction (once).
#[must_use]
pub fn take_scheduled() -> Option<CompactOnExit> {
    slot().take()
}

/// The decision at a supervised restart's quiet point (`deploy.rs::poll_step`,
/// before `mark_agent_deploy_restarting`, under the quiet judgement's store
/// lock). Returns the claimed run id. Never on the startup or readiness path.
pub fn decide_at_quiet_point(store: &Store) -> Option<Uuid> {
    let db_path = PathBuf::from(store.database_path()?);
    match store.cold_claim_compaction(auto_compaction()) {
        Ok(Some(run_id)) => {
            let (tmp_dir, result_path) = compact_paths(&db_path, run_id);
            tracing::info!(%run_id, "cold archive: compaction scheduled for this supervised restart");
            *slot() = Some(CompactOnExit {
                run_id,
                db_path,
                tmp_dir,
                result_path,
                deadline: COMPACT_DEADLINE,
            });
            // The restart is draining: no new archive pass starts, and a running
            // one stops at its next safe boundary, so the database is quiet by the
            // time the helper would open it.
            if let Some(lifecycle) = cold_archive_lifecycle::process() {
                lifecycle.close();
            }
            Some(run_id)
        }
        Ok(None) => None,
        Err(error) => {
            tracing::warn!(error = %error, "cold archive: compaction decision failed");
            None
        }
    }
}

/// The restart did not happen after the decision: forget the plan and give the
/// claim back (an operator's request waits for the next restart).
pub fn abandon(store: &Store, run_id: Option<Uuid>) {
    let Some(run_id) = run_id else {
        return;
    };
    {
        let mut slot = slot();
        if slot.as_ref().is_some_and(|plan| plan.run_id == run_id) {
            *slot = None;
        }
    }
    // The restart did not happen: archive passes may start again.
    if let Some(lifecycle) = cold_archive_lifecycle::process() {
        lifecycle.reopen();
    }
    if let Err(error) = store.cold_abandon_compaction(run_id) {
        tracing::warn!(%run_id, error = %error, "cold archive: compaction claim not returned");
    }
}

/// The helper's command line and environment. Only this `Command` names the
/// temporary directory; the daemon never sets it in its own process.
#[must_use]
pub fn helper_command(exe: &Path, plan: &CompactOnExit) -> Command {
    let mut command = Command::new(exe);
    command
        .arg(COMPACT_HELPER_ARG)
        .arg("--db")
        .arg(&plan.db_path)
        .arg("--tmp-dir")
        .arg(&plan.tmp_dir)
        .arg("--result")
        .arg(&plan.result_path)
        .arg("--run-id")
        .arg(plan.run_id.to_string());
    // The Unix VFS reads SQLITE_TMPDIR/TMPDIR once at initialisation; Windows
    // asks GetTempPathW, which reads TMP/TEMP.
    for key in ["SQLITE_TMPDIR", "TMPDIR", "TMP", "TEMP"] {
        command.env(key, &plan.tmp_dir);
    }
    command.stdin(Stdio::null());
    command
}

/// Waits for `child` until `deadline`, polling every `poll`; past it, kills the
/// child and waits for it, so it is always reaped before the caller exits.
pub fn wait_with_deadline(child: &mut Child, deadline: Duration, poll: Duration) -> HelperOutcome {
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return HelperOutcome::Exited(status.code()),
            Ok(None) if started.elapsed() < deadline => std::thread::sleep(poll),
            Ok(None) | Err(_) => break,
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    HelperOutcome::Killed
}

/// Prepares the helper's private temporary directory, spawns it with `spawn`
/// and waits for it within the plan's deadline.
pub fn run_on_exit(
    plan: &CompactOnExit,
    spawn: impl FnOnce(&CompactOnExit) -> std::io::Result<Child>,
    poll: Duration,
) -> HelperOutcome {
    if let Err(error) = rsid_core::private_state::create_private_dir_all(&plan.tmp_dir) {
        return HelperOutcome::SpawnFailed(format!("temporary directory: {error}"));
    }
    match spawn(plan) {
        Ok(mut child) => wait_with_deadline(&mut child, plan.deadline, poll),
        Err(error) => HelperOutcome::SpawnFailed(error.to_string()),
    }
}

/// Writes the result file of a compaction that was not started, so the next
/// daemon's reconcile settles the run row (it puts the request back).
fn record_deferral(plan: &CompactOnExit, detail: &str) {
    if let Some(parent) = plan.result_path.parent() {
        let _ = rsid_core::private_state::create_private_dir_all(parent);
    }
    let result = CompactResult {
        run_id: plan.run_id.to_string(),
        outcome: COMPACT_DEFERRED_OUTCOME.to_string(),
        error_code: None,
        error_detail: Some(detail.to_string()),
        compact_ms: 0,
        bytes_before: 0,
        bytes_after: 0,
        auto_vacuum: 0,
    };
    if let Err(error) = write_compact_result(&plan.result_path, &result) {
        eprintln!("rsid cold-archive-compact: deferral not recorded: {error}");
    }
}

/// The helper launch, behind the quiet-database proof (#2384 review, #205):
/// closes cold-archive admission, asks a running pass to stop, and waits at most
/// `wait` for every pass thread to return with its SQLite handles closed. Only a
/// quiet database reaches [`run_on_exit`]; otherwise the helper is not started,
/// the deferral is recorded for the next daemon, and the caller exits 75 as ever.
/// Without a `lifecycle` (a process that never ran a pass) the launch proceeds.
pub fn quiesce_then_run(
    plan: &CompactOnExit,
    lifecycle: Option<&ArchiveLifecycle>,
    wait: Duration,
    spawn: impl FnOnce(&CompactOnExit) -> std::io::Result<Child>,
    poll: Duration,
) -> HelperOutcome {
    let busy = match lifecycle.map(|lifecycle| lifecycle.quiesce(wait)) {
        Some(Quiescence::Busy { active }) => Some(active),
        Some(Quiescence::Quiet) | None => None,
    };
    let Some(active) = busy else {
        return run_on_exit(plan, spawn, poll);
    };
    let detail = format!(
        "{active} cold-archive pass thread(s) were still running {wait:?} after the shutdown \
         began; the compaction was not started and its request goes back to the queue"
    );
    record_deferral(plan, &detail);
    HelperOutcome::Deferred(detail)
}

/// Runs the compaction this process scheduled, if any, to its deadline. Called
/// by `main` after the graceful shutdown of a supervised restart; `main` exits
/// 75 whatever this returns.
#[must_use]
pub fn compact_before_exit() -> Option<HelperOutcome> {
    let plan = take_scheduled()?;
    let Some(exe) = DAEMON_EXE
        .get()
        .cloned()
        .or_else(|| std::env::current_exe().ok())
    else {
        return Some(HelperOutcome::SpawnFailed("executable path unknown".into()));
    };
    Some(quiesce_then_run(
        &plan,
        cold_archive_lifecycle::process().map(|lifecycle| &**lifecycle),
        QUIESCE_WAIT,
        |plan| helper_command(&exe, plan).spawn(),
        Duration::from_millis(100),
    ))
}

/// Arguments of the hidden helper mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelperArgs {
    pub db: PathBuf,
    pub tmp_dir: PathBuf,
    pub result: PathBuf,
    pub run_id: Uuid,
}

/// Parses `--db P --tmp-dir D --result F --run-id U` (the arguments after the
/// mode word), in any order, each exactly once.
#[must_use]
pub fn parse_helper_args(args: &[OsString]) -> Option<HelperArgs> {
    let mut db = None;
    let mut tmp_dir = None;
    let mut result = None;
    let mut run_id = None;
    let mut iter = args.iter();
    while let Some(flag) = iter.next() {
        let value = iter.next()?;
        let slot = match flag.to_str()? {
            "--db" => &mut db,
            "--tmp-dir" => &mut tmp_dir,
            "--result" => &mut result,
            "--run-id" => {
                if run_id.is_some() {
                    return None;
                }
                run_id = Some(Uuid::parse_str(value.to_str()?).ok()?);
                continue;
            }
            _ => return None,
        };
        if slot.is_some() {
            return None;
        }
        *slot = Some(PathBuf::from(value));
    }
    Some(HelperArgs {
        db: db?,
        tmp_dir: tmp_dir?,
        result: result?,
        run_id: run_id?,
    })
}

/// The helper process: compacts, writes its result file, removes its temporary
/// directory. Exit code 0 when the compaction completed, 1 otherwise.
#[must_use]
pub fn run_helper(args: &HelperArgs) -> i32 {
    let result = run_compaction(&args.db, args.run_id);
    let code = i32::from(result.outcome != "completed");
    if let Err(error) = write_compact_result(&args.result, &result) {
        eprintln!("rsid cold-archive-compact: result not written: {error}");
    }
    let _ = std::fs::remove_dir_all(&args.tmp_dir);
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(dir: &Path, deadline: Duration) -> CompactOnExit {
        CompactOnExit {
            run_id: Uuid::new_v4(),
            db_path: dir.join("rsi.db"),
            tmp_dir: dir.join("tmp").join("compact-x"),
            result_path: dir.join("tmp").join("compact-x.json"),
            deadline,
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_helper_command_names_the_temp_directory_only_in_its_own_environment() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan(dir.path(), COMPACT_DEADLINE);
        let command = helper_command(Path::new("/opt/rsid"), &plan);
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args[0], COMPACT_HELPER_ARG);
        let parsed = parse_helper_args(
            &command
                .get_args()
                .skip(1)
                .map(OsString::from)
                .collect::<Vec<_>>(),
        )
        .unwrap();
        assert_eq!(parsed.db, plan.db_path);
        assert_eq!(parsed.tmp_dir, plan.tmp_dir);
        assert_eq!(parsed.result, plan.result_path);
        assert_eq!(parsed.run_id, plan.run_id);
        let envs: std::collections::BTreeMap<String, String> = command
            .get_envs()
            .filter_map(|(key, value)| {
                Some((
                    key.to_string_lossy().into_owned(),
                    value?.to_string_lossy().into_owned(),
                ))
            })
            .collect();
        for key in ["SQLITE_TMPDIR", "TMPDIR", "TMP", "TEMP"] {
            assert_eq!(
                envs.get(key).map(String::as_str),
                Some(plan.tmp_dir.to_str().unwrap()),
                "{key}"
            );
        }
        // This process's own environment is untouched.
        assert_ne!(
            std::env::var_os("SQLITE_TMPDIR").map(PathBuf::from),
            Some(plan.tmp_dir.clone())
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn helper_arguments_must_be_complete_and_unique() {
        let full: Vec<OsString> = ["--db", "a", "--tmp-dir", "b", "--result", "c", "--run-id"]
            .iter()
            .map(OsString::from)
            .chain([OsString::from(Uuid::nil().to_string())])
            .collect();
        assert!(parse_helper_args(&full).is_some());
        assert!(parse_helper_args(&full[..6]).is_none(), "missing run id");
        let mut twice = full.clone();
        twice.extend(["--db", "z"].iter().map(OsString::from));
        assert!(parse_helper_args(&twice).is_none());
        let unknown: Vec<OsString> = ["--nope", "x"].iter().map(OsString::from).collect();
        assert!(parse_helper_args(&unknown).is_none());
    }

    /// Plan §5 step 4 (review #2398): past the deadline the helper is killed
    /// and reaped before the caller goes on to exit 75.
    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_helper_past_its_deadline_is_killed_and_reaped() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan(dir.path(), Duration::from_millis(300));
        let mut pid = None;
        let started = Instant::now();
        let outcome = run_on_exit(
            &plan,
            |_| {
                let child = Command::new("sleep").arg("30").spawn()?;
                pid = Some(child.id());
                Ok(child)
            },
            Duration::from_millis(20),
        );
        assert_eq!(outcome, HelperOutcome::Killed);
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(
            plan.tmp_dir.is_dir(),
            "the private temp directory was prepared"
        );
        // Reaped: the pid is no longer a child of this process.
        let pid = nix::unistd::Pid::from_raw(i32::try_from(pid.unwrap()).unwrap());
        assert!(nix::sys::wait::waitpid(pid, Some(nix::sys::wait::WaitPidFlag::WNOHANG)).is_err());
    }

    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_helper_that_finishes_reports_its_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan(dir.path(), Duration::from_secs(30));
        let outcome = run_on_exit(
            &plan,
            |_| Command::new("sh").args(["-c", "exit 1"]).spawn(),
            Duration::from_millis(20),
        );
        assert_eq!(outcome, HelperOutcome::Exited(Some(1)));
        let failed = run_on_exit(
            &plan,
            |_| Err(std::io::Error::other("no such binary")),
            Duration::from_millis(20),
        );
        assert!(matches!(failed, HelperOutcome::SpawnFailed(_)));
    }

    /// #2384 review (#205): a pass thread that outlives the shutdown wait keeps
    /// the helper from starting; the deferral is recorded for the next daemon.
    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_pass_thread_that_outlives_the_wait_defers_the_compaction_and_records_it() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan(dir.path(), Duration::from_secs(30));
        let lifecycle = std::sync::Arc::new(ArchiveLifecycle::default());
        // A pass thread that never returns its ticket.
        let _ticket = lifecycle.admit().unwrap();

        let started = Instant::now();
        let outcome = quiesce_then_run(
            &plan,
            Some(lifecycle.as_ref()),
            Duration::from_millis(200),
            |_| panic!("the helper must not start while a pass thread is alive"),
            Duration::from_millis(20),
        );
        assert!(matches!(outcome, HelperOutcome::Deferred(_)), "{outcome:?}");
        assert!(started.elapsed() < Duration::from_secs(10), "bounded wait");
        assert!(lifecycle.cancel_requested(), "the pass was asked to stop");
        let recorded: CompactResult =
            serde_json::from_slice(&std::fs::read(&plan.result_path).unwrap()).unwrap();
        assert_eq!(recorded.run_id, plan.run_id.to_string());
        assert_eq!(recorded.outcome, COMPACT_DEFERRED_OUTCOME);
        assert!(
            !plan.tmp_dir.exists(),
            "no helper temporary directory was prepared"
        );
    }

    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_quiet_database_proceeds_to_the_helper_with_admission_still_closed() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan(dir.path(), Duration::from_secs(30));
        let lifecycle = std::sync::Arc::new(ArchiveLifecycle::default());
        let outcome = quiesce_then_run(
            &plan,
            Some(lifecycle.as_ref()),
            Duration::from_secs(5),
            |_| Command::new("sh").args(["-c", "exit 0"]).spawn(),
            Duration::from_millis(20),
        );
        assert_eq!(outcome, HelperOutcome::Exited(Some(0)));
        assert!(!lifecycle.admitting(), "no pass starts during the exit");
    }

    /// The only launch of the helper in production code goes through the quiet
    /// proof, the quiet-point decision closes archive admission, and an abandoned
    /// restart opens it again.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_helper_is_launched_only_behind_the_quiet_database_proof() {
        let source = include_str!("cold_compact.rs");
        let production = &source[..source.find("#[cfg(test)]").unwrap()];
        let launch = &production[production.find("pub fn compact_before_exit").unwrap()..];
        assert!(launch.contains("quiesce_then_run("));
        assert!(!launch.contains("run_on_exit("), "no unproved launch");
        let decide = &production[production.find("pub fn decide_at_quiet_point").unwrap()
            ..production.find("pub fn abandon").unwrap()];
        assert!(decide.contains("lifecycle.close()"));
        let abandon = &production[production.find("pub fn abandon").unwrap()
            ..production.find("/// The helper's command line").unwrap()];
        assert!(abandon.contains("lifecycle.reopen()"));
    }

    /// The quiet-point decision claims an operator's request, schedules it for
    /// this process's exit, and gives it back when the restart does not happen.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_quiet_point_schedules_a_requested_compaction_and_an_abandoned_restart_returns_it() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("rsi.db");
        let store = Store::open_test_fixture(&db).unwrap();
        assert_eq!(
            decide_at_quiet_point(&store),
            None,
            "nothing asked, nothing due"
        );
        let requested = store.cold_request_compaction().unwrap();
        let claimed = decide_at_quiet_point(&store);
        assert_eq!(claimed, Some(requested));
        assert_eq!(scheduled_run_id(), Some(requested));
        abandon(&store, claimed);
        assert_eq!(scheduled_run_id(), None);
        assert_eq!(store.cold_pending_compaction().unwrap(), Some(requested));

        // Claimed again and taken for the exit: the plan names this database.
        assert_eq!(decide_at_quiet_point(&store), Some(requested));
        let plan = take_scheduled().unwrap();
        assert_eq!(plan.run_id, requested);
        assert_eq!(plan.db_path, db);
        assert_eq!(plan.deadline, COMPACT_DEADLINE);
        assert!(plan.tmp_dir.starts_with(dir.path().join("tmp")));
        assert!(take_scheduled().is_none());
    }
}
