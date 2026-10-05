//! #1122: operator-requested quiet-point restart.
//!
//! The operator's rebuild asks the daemon to restart onto already-built
//! binaries without cutting off running turns. This is a thin front for the
//! `AgentRequestDeploy` runner (`deploy.rs`, `deploy_drain.rs`): it stages and
//! verifies the build, records an owner-less `staged` row and lets the runner
//! wait for the quiet point, swap, restart under the supervisor and verify the
//! running build. Cancel settles the staged row; force skips the quiet gate.

use crate::deploy::DeployService;
use crate::error::{DaemonError, Result};
use crate::store::Store;
use crate::store::agent_deploys::{DeployRow, NewOperatorDeploy};
use chrono::{DateTime, SecondsFormat, Utc};
use rsi_common::agent_deploy::{
    DEPLOY_IN_PROGRESS, DEPLOY_NEEDS_SUPERVISOR, DeployBinaryV1, DeployState,
};
use rsi_common::operator_restart::{
    OperatorRestartStatusV1, RESTART_NOT_CANCELLABLE, RESTART_NOTHING_PENDING,
    RequestOperatorRestartRequestV1,
};
use tokio::sync::Mutex;
use uuid::Uuid;

fn status(
    store: &Store,
    row: Option<&DeployRow>,
    supervised: bool,
) -> Result<OperatorRestartStatusV1> {
    let Some(row) = row else {
        return Ok(OperatorRestartStatusV1 {
            pending: false,
            deploy_id: None,
            state: None,
            sha: None,
            release_by: None,
            forced: false,
            blockers: Vec::new(),
            turns_in_flight: 0,
            reason: None,
            supervised,
        });
    };
    let pending = !row.state.is_terminal();
    let (blockers, turns) = if row.state == DeployState::Staged {
        let (blockers, turns) = store.operator_quiet_blockers()?;
        (blockers.into_iter().map(str::to_string).collect(), turns)
    } else {
        (Vec::new(), 0)
    };
    Ok(OperatorRestartStatusV1 {
        pending,
        deploy_id: Some(row.id),
        state: Some(row.state),
        sha: Some(row.sha.clone()),
        release_by: Some(row.deadline_at.to_rfc3339_opts(SecondsFormat::Nanos, true)),
        forced: row.forced,
        blockers,
        turns_in_flight: u32::try_from(turns).unwrap_or(u32::MAX),
        reason: row.reason.clone(),
        supervised,
    })
}

/// `GetOperatorRestart`: the live operator restart, else the latest one.
///
/// # Errors
/// A persistence error.
pub async fn get(store: &Mutex<Store>, service: &DeployService) -> Result<OperatorRestartStatusV1> {
    let store = store.lock().await;
    let live = store.live_agent_deploy()?.filter(|row| row.operator);
    let row = match live {
        Some(row) => Some(row),
        None => store.latest_operator_deploy()?,
    };
    status(&store, row.as_ref(), service.is_supervised())
}

/// Whether a live operator deploy already carries exactly this build: same
/// commit sha and the same sha256 for every staged binary.
fn same_payload(row: &DeployRow, sha: &str, manifest: &[DeployBinaryV1]) -> bool {
    let key = |manifest: &[DeployBinaryV1]| {
        let mut key: Vec<(String, String)> = manifest
            .iter()
            .map(|entry| (entry.name.clone(), entry.sha256.clone()))
            .collect();
        key.sort();
        key
    };
    row.sha == sha && key(&row.manifest) == key(manifest)
}

/// `RequestOperatorRestart`: stage and verify the built binaries and record a
/// `staged` operator deploy.
///
/// A request for the build already pending is idempotent (it only applies
/// `now`). A request for a *different* build (a newer rebuild while the first
/// still waits) replaces the pending one atomically under the store lock: the
/// old staged copies are removed, the old row settles `failed`
/// (`superseded_by_new_build`) and the new row keeps a forced flag the old one
/// had, so the operator never ends up installing a stale build (#1122 review).
///
/// # Errors
/// `deploy_needs_supervisor`, `deploy_already_in_progress` (an agent deploy is
/// live), a `deploy_*` staging refusal, or a persistence error.
pub async fn request(
    store: &Mutex<Store>,
    service: &DeployService,
    request: RequestOperatorRestartRequestV1,
    now: DateTime<Utc>,
) -> Result<OperatorRestartStatusV1> {
    request
        .validate()
        .map_err(|code| DaemonError::InvalidParam(code.into()))?;
    let live_schema = {
        let guard = store.lock().await;
        match guard.live_agent_deploy()? {
            Some(live) if !live.operator => {
                return Err(DaemonError::InvalidParam(DEPLOY_IN_PROGRESS.into()));
            }
            Some(live) if live.state != DeployState::Staged => {
                // The swap has begun: nothing to replace or force.
                return status(&guard, Some(&live), service.is_supervised());
            }
            Some(_) => {}
            None => {
                if !service.is_supervised() {
                    return Err(DaemonError::PolicyDenied(DEPLOY_NEEDS_SUPERVISOR.into()));
                }
                service.check_target()?;
                service.check_budget(&guard, now)?;
            }
        }
        guard.schema_user_version()?
    };
    let id = Uuid::new_v4();
    let (manifest, sha) = {
        let plan = service.stage_plan();
        let dir = request.binaries_dir.clone();
        tokio::task::spawn_blocking(move || {
            plan.stage_binaries_discovering_sha(id, &dir, live_schema)
        })
        .await
        .map_err(|error| DaemonError::Store(error.to_string()))??
    };
    let guard = store.lock().await;
    let mut forced = request.now;
    match guard.live_agent_deploy() {
        Err(error) => {
            crate::deploy::remove_staged(&manifest, id);
            return Err(error);
        }
        Ok(Some(live)) if !live.operator => {
            crate::deploy::remove_staged(&manifest, id);
            return Err(DaemonError::InvalidParam(DEPLOY_IN_PROGRESS.into()));
        }
        Ok(Some(live)) if live.state != DeployState::Staged => {
            crate::deploy::remove_staged(&manifest, id);
            return status(&guard, Some(&live), service.is_supervised());
        }
        Ok(Some(live)) if same_payload(&live, &sha, &manifest) => {
            crate::deploy::remove_staged(&manifest, id);
            if request.now {
                guard.force_operator_deploy()?;
            }
            let live = guard.live_agent_deploy()?;
            return status(&guard, live.as_ref(), service.is_supervised());
        }
        Ok(Some(live)) => {
            forced |= live.forced;
            crate::deploy::remove_staged(&live.manifest, live.id);
            if let Err(error) = guard.settle_agent_deploy(
                live.id,
                DeployState::Failed,
                Some("superseded_by_new_build"),
                now,
            ) {
                crate::deploy::remove_staged(&manifest, id);
                return Err(error);
            }
        }
        Ok(None) => {}
    }
    let inserted = guard.insert_operator_deploy(
        &NewOperatorDeploy {
            id,
            sha: &sha,
            manifest: &manifest,
            max_wait_secs: request.wait_secs(),
            forced,
        },
        now,
    );
    match inserted {
        Ok(row) => status(&guard, Some(&row), service.is_supervised()),
        Err(error) => {
            crate::deploy::remove_staged(&manifest, id);
            Err(error)
        }
    }
}

/// `ForceOperatorRestart`: skip the quiet gate of the staged operator restart.
///
/// # Errors
/// `operator_restart_nothing_pending` or a persistence error.
pub async fn force(
    store: &Mutex<Store>,
    service: &DeployService,
) -> Result<OperatorRestartStatusV1> {
    {
        let guard = store.lock().await;
        if !guard.force_operator_deploy()? {
            return Err(DaemonError::InvalidParam(RESTART_NOTHING_PENDING.into()));
        }
    }
    get(store, service).await
}

/// `CancelOperatorRestart`: settle the staged operator restart as failed
/// (`cancelled_by_operator`) and drop its staged copies.
///
/// # Errors
/// `operator_restart_nothing_pending`, `operator_restart_not_cancellable` once
/// the swap has begun, or a persistence error.
pub async fn cancel(
    store: &Mutex<Store>,
    service: &DeployService,
    now: DateTime<Utc>,
) -> Result<OperatorRestartStatusV1> {
    {
        let guard = store.lock().await;
        let Some(row) = guard.live_agent_deploy()?.filter(|row| row.operator) else {
            return Err(DaemonError::InvalidParam(RESTART_NOTHING_PENDING.into()));
        };
        if row.state != DeployState::Staged {
            return Err(DaemonError::InvalidParam(RESTART_NOT_CANCELLABLE.into()));
        }
        crate::deploy::remove_staged(&row.manifest, row.id);
        guard.settle_agent_deploy(
            row.id,
            DeployState::Failed,
            Some("cancelled_by_operator"),
            now,
        )?;
    }
    get(store, service).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deploy::{GateState, PollOutcome, poll_once};
    use crate::deploy_drain::DeployDrain;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    struct Fixture {
        _dir: tempfile::TempDir,
        build: PathBuf,
        install: PathBuf,
        service: DeployService,
        restarts: Arc<AtomicUsize>,
        store: Mutex<Store>,
        drain: DeployDrain,
    }

    fn insert_session(store: &Store, id: Uuid, status: &str, parent: Option<Uuid>) {
        store
            .conn
            .execute(
                "INSERT INTO sessions (id, provider, query, working_dir, status, created_at, \
                 updated_at, session_kind, parent_id) VALUES (?1,'Claude','q','/tmp',?2,?3,?3,'Task',?4)",
                rusqlite::params![
                    id.to_string(),
                    status,
                    Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true),
                    parent.map(|p| p.to_string()),
                ],
            )
            .unwrap();
    }

    fn set_status(store: &Store, id: Uuid, status: &str) {
        store
            .conn
            .execute(
                "UPDATE sessions SET status=?2 WHERE id=?1",
                rusqlite::params![id.to_string(), status],
            )
            .unwrap();
    }

    fn fixture(supervised: bool) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let build = dir.path().join("build");
        let install = dir.path().join("install");
        std::fs::create_dir_all(&build).unwrap();
        std::fs::create_dir_all(&install).unwrap();
        std::fs::write(build.join("rsid"), b"new-rsid").unwrap();
        std::fs::write(install.join("rsid"), b"old-rsid").unwrap();
        let service = DeployService::new(
            install.clone(),
            vec![dir.path().to_path_buf()],
            Box::new(move || supervised),
            Arc::new(|_: &Path| Ok((SHA.to_string(), 999))),
        );
        let restarts = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&restarts);
        service.set_restart_trigger(Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        }));
        Fixture {
            _dir: dir,
            build,
            install,
            service,
            restarts,
            store: Mutex::new(Store::open_in_memory().unwrap()),
            drain: DeployDrain::new(),
        }
    }

    fn ask(f: &Fixture, now: bool) -> RequestOperatorRestartRequestV1 {
        RequestOperatorRestartRequestV1 {
            binaries_dir: f.build.to_string_lossy().into_owned(),
            max_wait_secs: Some(60),
            now,
        }
    }

    async fn poll(f: &Fixture, gate: &mut GateState, now: DateTime<Utc>) -> PollOutcome {
        poll_once(&f.store, &f.service, now, gate, &f.drain, true)
            .await
            .unwrap()
    }

    fn installed(f: &Fixture) -> Vec<u8> {
        std::fs::read(f.install.join("rsid")).unwrap()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[tokio::test]
    async fn a_restart_waits_for_a_manager_and_a_worker_mid_turn_then_restarts_when_they_end() {
        let f = fixture(true);
        let (manager, worker) = (Uuid::new_v4(), Uuid::new_v4());
        {
            let store = f.store.lock().await;
            insert_session(&store, manager, "Running", None);
            insert_session(&store, worker, "Running", Some(manager));
        }
        let now = Utc::now();
        let status = request(&f.store, &f.service, ask(&f, false), now)
            .await
            .unwrap();
        assert!(status.pending);
        assert_eq!(status.state, Some(DeployState::Staged));
        assert_eq!(status.sha.as_deref(), Some(SHA));
        assert_eq!(status.turns_in_flight, 2);
        assert!(
            status
                .summary()
                .unwrap()
                .starts_with("restart pending: waiting for 2 turns")
        );
        let mut gate = GateState::default();
        for _ in 0..3 {
            let outcome = poll(&f, &mut gate, now).await;
            assert_eq!(
                outcome,
                PollOutcome::Waiting(vec!["worker_mid_turn", "manager_mid_turn"])
            );
        }
        assert_eq!(f.restarts.load(Ordering::SeqCst), 0);
        assert_eq!(installed(&f), b"old-rsid");
        set_status(&*f.store.lock().await, worker, "Completed");
        assert_eq!(
            poll(&f, &mut gate, now).await,
            PollOutcome::Waiting(vec!["manager_mid_turn"])
        );
        set_status(&*f.store.lock().await, manager, "Completed");
        assert_eq!(poll(&f, &mut gate, now).await, PollOutcome::Waiting(vec![]));
        let restarting = poll(&f, &mut gate, now).await;
        assert!(matches!(restarting, PollOutcome::Restarting(_)));
        assert_eq!(f.restarts.load(Ordering::SeqCst), 1);
        assert_eq!(installed(&f), b"new-rsid");
        let status = get(&f.store, &f.service).await.unwrap();
        assert_eq!(status.state, Some(DeployState::Restarting));
        assert_eq!(status.summary().as_deref(), Some("restart in progress"));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[tokio::test]
    async fn now_restarts_at_the_next_poll_even_with_a_manager_mid_turn() {
        let f = fixture(true);
        insert_session(&*f.store.lock().await, Uuid::new_v4(), "Running", None);
        let now = Utc::now();
        let status = request(&f.store, &f.service, ask(&f, true), now)
            .await
            .unwrap();
        assert!(status.forced);
        let mut gate = GateState::default();
        let outcome = poll(&f, &mut gate, now).await;
        assert!(matches!(outcome, PollOutcome::Restarting(_)), "{outcome:?}");
        assert_eq!(f.restarts.load(Ordering::SeqCst), 1);
        assert_eq!(installed(&f), b"new-rsid");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[tokio::test]
    async fn force_after_a_waiting_request_skips_the_quiet_gate() {
        let f = fixture(true);
        insert_session(&*f.store.lock().await, Uuid::new_v4(), "Running", None);
        let now = Utc::now();
        request(&f.store, &f.service, ask(&f, false), now)
            .await
            .unwrap();
        let mut gate = GateState::default();
        assert!(matches!(
            poll(&f, &mut gate, now).await,
            PollOutcome::Waiting(_)
        ));
        let status = force(&f.store, &f.service).await.unwrap();
        assert!(status.forced);
        assert!(matches!(
            poll(&f, &mut gate, now).await,
            PollOutcome::Restarting(_)
        ));
        let again = force(&f.store, &f.service).await.unwrap_err();
        assert!(again.to_string().contains(RESTART_NOTHING_PENDING));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[tokio::test]
    async fn cancel_settles_the_staged_restart_and_leaves_the_install_alone() {
        let f = fixture(true);
        insert_session(&*f.store.lock().await, Uuid::new_v4(), "Running", None);
        let now = Utc::now();
        request(&f.store, &f.service, ask(&f, false), now)
            .await
            .unwrap();
        let status = cancel(&f.store, &f.service, now).await.unwrap();
        assert!(!status.pending);
        assert_eq!(status.state, Some(DeployState::Failed));
        assert_eq!(status.reason.as_deref(), Some("cancelled_by_operator"));
        assert_eq!(installed(&f), b"old-rsid");
        let mut gate = GateState::default();
        assert_eq!(poll(&f, &mut gate, now).await, PollOutcome::Idle);
        let nothing = cancel(&f.store, &f.service, now).await.unwrap_err();
        assert!(nothing.to_string().contains(RESTART_NOTHING_PENDING));
        // A new request is accepted once the cancelled one has settled.
        let again = request(&f.store, &f.service, ask(&f, false), now)
            .await
            .unwrap();
        assert!(again.pending);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[tokio::test]
    async fn an_unsupervised_daemon_refuses_so_the_installer_can_restart_directly() {
        let f = fixture(false);
        let error = request(&f.store, &f.service, ask(&f, false), Utc::now())
            .await
            .unwrap_err();
        assert!(error.to_string().contains(DEPLOY_NEEDS_SUPERVISOR));
        let status = get(&f.store, &f.service).await.unwrap();
        assert!(!status.pending);
        assert!(!status.supervised);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[tokio::test]
    async fn a_repeated_request_is_idempotent_and_an_agent_deploy_is_never_displaced() {
        let f = fixture(true);
        let now = Utc::now();
        let first = request(&f.store, &f.service, ask(&f, false), now)
            .await
            .unwrap();
        let second = request(&f.store, &f.service, ask(&f, false), now)
            .await
            .unwrap();
        assert_eq!(first.deploy_id, second.deploy_id);
        cancel(&f.store, &f.service, now).await.unwrap();

        let owner = Uuid::new_v4();
        insert_session(&*f.store.lock().await, owner, "Running", None);
        let id = Uuid::new_v4();
        let manifest = f
            .service
            .stage_plan()
            .stage_binaries(id, f.build.to_str().unwrap(), SHA, 1)
            .unwrap();
        f.store
            .lock()
            .await
            .insert_agent_deploy(
                &crate::store::agent_deploys::NewDeploy {
                    id,
                    owner_session_id: owner,
                    idempotency_key: "k",
                    sha: SHA,
                    fingerprint: crate::store::agent_deploys::deploy_fingerprint(SHA, "x", 60),
                    manifest: &manifest,
                    max_wait_secs: 60,
                },
                now,
            )
            .unwrap();
        let refused = request(&f.store, &f.service, ask(&f, false), now)
            .await
            .unwrap_err();
        assert!(refused.to_string().contains(DEPLOY_IN_PROGRESS));
        assert!(
            cancel(&f.store, &f.service, now)
                .await
                .unwrap_err()
                .to_string()
                .contains(RESTART_NOTHING_PENDING)
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[tokio::test]
    async fn a_rebuilt_request_replaces_the_pending_build_instead_of_reusing_it() {
        let f = fixture(true);
        let (manager, now) = (Uuid::new_v4(), Utc::now());
        insert_session(&*f.store.lock().await, manager, "Running", None);
        let first = request(&f.store, &f.service, ask(&f, true), now)
            .await
            .unwrap();
        let first_staged = |f: &Fixture| {
            std::fs::read_dir(&f.install)
                .unwrap()
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.file_name().to_string_lossy().contains(".deploy-"))
                .count()
        };
        assert_eq!(first_staged(&f), 1);
        // The operator rebuilds while the first request still waits.
        std::fs::write(f.build.join("rsid"), b"newer-rsid").unwrap();
        let second = request(&f.store, &f.service, ask(&f, false), now)
            .await
            .unwrap();
        assert_ne!(first.deploy_id, second.deploy_id);
        assert!(second.pending);
        assert!(second.forced, "a forced pending request stays forced");
        assert_eq!(first_staged(&f), 1, "the old staged copy is removed");
        let old = f
            .store
            .lock()
            .await
            .get_agent_deploy(first.deploy_id.unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(old.state, DeployState::Failed);
        assert_eq!(old.reason.as_deref(), Some("superseded_by_new_build"));
        let mut gate = GateState::default();
        let outcome = poll(&f, &mut gate, now).await;
        assert!(matches!(outcome, PollOutcome::Restarting(_)), "{outcome:?}");
        assert_eq!(installed(&f), b"newer-rsid");
        // The same build requested again is a no-op, not a second row.
        let again = get(&f.store, &f.service).await.unwrap();
        assert_eq!(again.deploy_id, second.deploy_id);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[tokio::test]
    async fn a_restart_past_its_release_by_time_settles_timed_out_without_waking_anyone() {
        let f = fixture(true);
        insert_session(&*f.store.lock().await, Uuid::new_v4(), "Running", None);
        let now = Utc::now();
        request(&f.store, &f.service, ask(&f, false), now)
            .await
            .unwrap();
        let mut gate = GateState::default();
        let late = now + chrono::Duration::seconds(61);
        let settled = poll(&f, &mut gate, late).await;
        assert!(matches!(
            settled,
            PollOutcome::Settled(_, DeployState::TimedOut)
        ));
        let status = get(&f.store, &f.service).await.unwrap();
        assert!(!status.pending);
        assert!(status.reason.unwrap().contains("manager_mid_turn"));
        assert!(
            f.store
                .lock()
                .await
                .list_scheduled_jobs()
                .unwrap()
                .is_empty()
        );
    }
}
