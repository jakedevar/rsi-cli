//! #1166: a manager-action session launch must never stall the daemon runtime.
//! The launch runs on a deliberately small multi-thread runtime while an
//! unrelated Store probe (what an RPC handler or the watchdog does) keeps
//! asking for the Store; the probe's worst wait is bounded.
use super::*;

/// Worst acceptable wait for an unrelated Store acquisition while a launch is
/// in flight. Far below the watchdog's 5 s probe deadline and far above
/// scheduling noise on a loaded runner.
const PROBE_BOUND: std::time::Duration = std::time::Duration::from_millis(1500);

/// A window with fewer completed samples than this proves nothing: a probe that
/// never ran (or never got the Store) must not count as passing.
const MIN_PROBE_SAMPLES: usize = 5;

/// An unrelated Store user on its own OS thread. It is timed with `Instant` on
/// that thread, not on the runtime under test, which the wedge under test can
/// starve. It holds the Store briefly each round, as an RPC handler does, so the
/// launch and the manager-action loop meet it and queue behind one another.
struct StoreProbe {
    samples: std::sync::Arc<std::sync::Mutex<Vec<(std::time::Instant, std::time::Duration)>>>,
    pending_since: std::sync::Arc<std::sync::Mutex<Option<std::time::Instant>>>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    started: std::time::Instant,
}

struct ProbeReport {
    samples: usize,
    worst: std::time::Duration,
}

impl ProbeReport {
    fn assert_live(&self, what: &str) {
        assert!(
            self.samples >= MIN_PROBE_SAMPLES,
            "only {} Store samples completed during {what} (need {MIN_PROBE_SAMPLES}): \
             the Store was unavailable, worst wait {:?}",
            self.samples,
            self.worst
        );
        assert!(
            self.worst < PROBE_BOUND,
            "an unrelated Store user waited {:?} during {what}",
            self.worst
        );
    }
}

impl StoreProbe {
    fn start(store: std::sync::Arc<tokio::sync::Mutex<crate::store::Store>>) -> Self {
        let probe = Self {
            samples: std::sync::Arc::default(),
            pending_since: std::sync::Arc::default(),
            stop: std::sync::Arc::default(),
            started: std::time::Instant::now(),
        };
        let (samples, pending, stop) = (
            probe.samples.clone(),
            probe.pending_since.clone(),
            probe.stop.clone(),
        );
        // Detached on purpose: against a wedged Store the probe thread is blocked
        // in `blocking_lock` and must not be joined.
        std::thread::Builder::new()
            .name("liveness-probe".into())
            .spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    let began = std::time::Instant::now();
                    *pending.lock().unwrap() = Some(began);
                    let guard = store.blocking_lock();
                    let waited = began.elapsed();
                    std::thread::sleep(std::time::Duration::from_micros(300));
                    drop(guard);
                    // Publish the sample before clearing `pending`, so a report
                    // never sees neither (a long wait would be lost in the gap).
                    samples.lock().unwrap().push((began, waited));
                    *pending.lock().unwrap() = None;
                    std::thread::sleep(std::time::Duration::from_micros(500));
                }
            })
            .unwrap();
        probe
    }

    /// Samples whose acquisition ended at or after `since` (a wait that began
    /// earlier but was still starving the Store inside the window counts), plus
    /// any acquisition still pending now (a wedged Store never completes).
    fn report(&self, since: std::time::Instant) -> ProbeReport {
        self.stop.store(true, Ordering::Release);
        let samples = self.samples.lock().unwrap();
        let mut window: Vec<_> = samples
            .iter()
            .filter(|(began, waited)| *began + *waited >= since)
            .collect();
        let mut worst = window
            .iter()
            .map(|(_, waited)| *waited)
            .max()
            .unwrap_or_default();
        if let Some(pending) = *self.pending_since.lock().unwrap() {
            worst = worst.max(pending.elapsed());
        }
        window.shrink_to_fit();
        ProbeReport {
            samples: window.len(),
            worst,
        }
    }
}

/// Wait until the launch of `id` is parked in `phase` (its breadcrumb), the
/// readiness handshake that proves the probe window overlaps the stuck state.
async fn wait_for_launch_phase(id: Uuid, phase: &'static str) {
    let needle = format!("{id} {phase} ");
    for _ in 0..4000 {
        if crate::launch_breadcrumbs::snapshot()
            .iter()
            .any(|line| line.starts_with(&needle))
        {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

/// Drive `work` with a Store probe running. `ready` is the launch phase that
/// must be observed (the stuck state) before samples count; `None` counts from
/// the probe's start. Returns the work's result and the window's report.
async fn run_with_probe<T>(
    p: &Pilot,
    ready: Option<(Uuid, &'static str)>,
    bound: std::time::Duration,
    work: impl std::future::Future<Output = T>,
) -> (T, ProbeReport) {
    let probe = StoreProbe::start(p.manager.store.clone());
    let ready_at = std::sync::Arc::new(std::sync::Mutex::new(Some(probe.started)));
    let watcher = ready.map(|(id, phase)| {
        *ready_at.lock().unwrap() = None;
        let ready_at = ready_at.clone();
        tokio::spawn(async move {
            wait_for_launch_phase(id, phase).await;
            // Settle: let the launch be parked there for a moment before sampling.
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            *ready_at.lock().unwrap() = Some(std::time::Instant::now());
        })
    });
    let result = tokio::time::timeout(bound, work)
        .await
        .expect("the launch finished instead of hanging");
    if let Some(watcher) = watcher {
        watcher.abort();
    }
    let since = ready_at
        .lock()
        .unwrap()
        .expect("the launch reached its stuck phase while the probe was running");
    (result, probe.report(since))
}

/// Admit a `create_session` under a fresh Epic and install its fake provider.
async fn queued_create(p: &Pilot, key: &str) -> Uuid {
    let parent = p
        .admit(
            &format!("{key}-epic"),
            ManagerActionV2::CreateContainer {
                parent_id: Some(p.group),
                kind: SessionKind::Epic,
                name: format!("Liveness {key}"),
                tags: vec!["manager".into()],
            },
        )
        .await
        .target_session_id
        .unwrap();
    p.execute().await.unwrap();
    let receipt = p
        .admit(
            &format!("{key}-spawn"),
            ManagerActionV2::CreateSession {
                parent_id: parent,
                kind: SessionKind::Feature,
                query: "implement scoped work".into(),
                launch: p.policy.allowed_launches[0].clone(),
                sandbox_source: None,
            },
        )
        .await;
    let child = receipt.target_session_id.unwrap();
    let _process = crate::session::launch::install_controller_candidate_test_process(child);
    child
}

/// The provider monitor publishes `Running` asynchronously after the launch
/// returns; poll for it.
async fn wait_for_running(p: &Pilot, id: Uuid) -> bool {
    for _ in 0..500 {
        if session_status_of(p, id).await == SessionStatus::Running {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    false
}

/// Run the claimed launch with a Store probe (see [`run_with_probe`]).
async fn execute_with_probe(
    p: &Pilot,
    ready: Option<(Uuid, &'static str)>,
) -> (Result<()>, ProbeReport) {
    run_with_probe(p, ready, std::time::Duration::from_secs(60), p.execute()).await
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_session_launch_keeps_the_store_free_for_unrelated_users() {
    let p = pilot().await;
    let child = queued_create(&p, "liveness-base").await;
    let (launched, report) = execute_with_probe(&p, None).await;
    launched.unwrap();
    report.assert_live("a plain create_session launch");
    assert!(
        wait_for_running(&p, child).await,
        "the launch established its provider"
    );
    assert_eq!(
        session_status_of(&p, child).await,
        SessionStatus::Running,
        "the launch established its provider"
    );
}

/// Hold the repository mutex of `repo` on a plain thread for `hold`, the way a
/// maintenance proof (purge, archive cleanup, settlement) does.
fn hold_repository_mutex(
    repo: std::path::PathBuf,
    hold: std::time::Duration,
) -> std::thread::JoinHandle<()> {
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        crate::sandbox::git_worktree::with_repository_mutation(&repo, || {
            held_tx.send(()).unwrap();
            std::thread::sleep(hold);
            Ok(())
        })
        .unwrap();
    });
    held_rx.recv().unwrap();
    handle
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_session_behind_a_held_repository_mutex_keeps_the_store_free() {
    let p = pilot().await;
    let _child = queued_create(&p, "liveness-repo").await;
    let holder = hold_repository_mutex(p.repo.clone(), std::time::Duration::from_secs(3));
    let (launched, report) = execute_with_probe(&p, None).await;
    holder.join().unwrap();
    launched.unwrap();
    report.assert_live("a launch behind a held repository mutex");
}

/// Hold this fixture's exact custody root on a plain maintenance thread.
fn hold_root(custody_id: Uuid, hold: std::time::Duration) -> std::thread::JoinHandle<()> {
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        let _guard = crate::store::sandbox_custody::lock_custody_root(custody_id);
        held_tx.send(()).unwrap();
        std::thread::sleep(hold);
    });
    held_rx.recv().unwrap();
    handle
}

/// #1166: the launch binds its session through a Store method that needs the
/// new custody root's stripe. With a maintenance proof holding that stripe the
/// launch used to wait for it on a runtime thread while holding the Store, so
/// every RPC and the watchdog's Store probe stalled for the whole proof. The
/// launch now waits asynchronously and the Store stays free; it still
/// establishes once the stripe frees (the hold is inside the 5 s admission
/// budget).
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_session_behind_held_stripes_keeps_the_store_free_and_establishes() {
    let p = pilot().await;
    let child = queued_create(&p, "liveness-stripes").await;
    let (fresh_root, _reservation) =
        crate::store::stripe_liveness_support::reserve_fresh_root(child);
    let holder = hold_root(fresh_root, std::time::Duration::from_secs(3));
    let (launched, report) = execute_with_probe(&p, Some((child, "custody_bind"))).await;
    holder.join().unwrap();
    report.assert_live("a launch blocked on a custody stripe");
    launched.unwrap();
    assert!(
        wait_for_running(&p, child).await,
        "the launch established its provider"
    );
    assert_eq!(
        session_status_of(&p, child).await,
        SessionStatus::Running,
        "the launch established once the stripe freed"
    );
}

/// The lead-assignment commit authenticates the candidate under the Store and
/// the active map. Park a replacement right before that commit, hold its
/// root, and let it run: the Store must stay free for unrelated users.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replace_lead_assignment_commit_behind_held_stripes_keeps_the_store_free() {
    let p = pilot().await;
    let first = p
        .admit(
            "liveness-lead-1",
            ManagerActionV2::ReplaceLead {
                epic_id: p.epic,
                expected: p.fence().await,
                query: "first replacement".into(),
                launch: p.policy.allowed_launches[0].clone(),
            },
        )
        .await;
    let lead = first.target_session_id.unwrap();
    let _first_process = crate::session::launch::install_controller_candidate_test_process(lead);
    p.execute().await.unwrap();
    wait_launch_event(&p, lead).await;

    let second = p
        .admit(
            "liveness-lead-2",
            ManagerActionV2::ReplaceLead {
                epic_id: p.epic,
                expected: p.fence().await,
                query: "second replacement".into(),
                launch: p.policy.allowed_launches[0].clone(),
            },
        )
        .await;
    let successor = second.target_session_id.unwrap();
    let _second_process =
        crate::session::launch::install_controller_candidate_test_process(successor);
    let (reached, resume) = crate::session::launch::install_controller_candidate_test_pause(
        successor,
        crate::session::launch::ControllerCandidateTestPhase::BeforeAssignment,
    );
    let exec = p.execute();
    tokio::pin!(exec);
    tokio::select! {
        result = &mut exec => panic!("the replacement finished before its assignment commit: {result:?}"),
        reached = reached => reached.unwrap(),
    }
    let custody_id = p
        .manager
        .store
        .lock()
        .await
        .live_custody_for_session(successor)
        .unwrap()
        .custody_id;
    let holder = hold_root(custody_id, std::time::Duration::from_secs(3));
    let probe = StoreProbe::start(p.manager.store.clone());
    resume.send(()).unwrap();
    let launched = tokio::time::timeout(std::time::Duration::from_secs(60), exec)
        .await
        .expect("the replacement finished instead of hanging");
    let report = probe.report(probe.started);
    holder.join().unwrap();
    report.assert_live("an assignment commit blocked on a custody stripe");
    launched.unwrap();
}

/// #1166: a manager `resume_lead` authenticates the sandboxed lead's custody
/// before the provider restarts. Holding the lead's root during the resume used to
/// pin the Store (a blocking stripe wait under the Store guard in the
/// continuation); it now waits asynchronously and the Store stays free.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_lead_behind_held_stripes_keeps_the_store_free_and_resumes() {
    let p = pilot().await;
    let created = p
        .admit(
            "liveness-resume-initial",
            ManagerActionV2::ReplaceLead {
                epic_id: p.epic,
                expected: p.fence().await,
                query: "start".into(),
                launch: p.policy.allowed_launches[0].clone(),
            },
        )
        .await;
    let id = created.target_session_id.unwrap();
    let _initial = crate::session::launch::install_controller_candidate_test_process(id);
    p.execute().await.unwrap();
    wait_launch_event(&p, id).await;
    crate::session::launch::send_controller_candidate_test_event(
        id,
        crate::claude::StreamEvent {
            event_type: "system".into(),
            data: serde_json::json!({"subtype":"init","session_id":"liveness-resumable-provider"}),
        },
    )
    .await;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let captured = p
                .manager
                .store
                .lock()
                .await
                .get_session(id)
                .unwrap()
                .unwrap()
                .claude_session_id
                .as_deref()
                == Some("liveness-resumable-provider");
            if captured && p.manager.persistence.pending.load(Ordering::SeqCst) == 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let pause = p
        .admit(
            "liveness-resume-pause",
            ManagerActionV2::PauseLead {
                epic_id: p.epic,
                expected: p.fence().await,
                reason: "manager pause".into(),
            },
        )
        .await;
    p.execute().await.unwrap();
    assert_eq!(
        p.receipt(pause.operation_id).await.state,
        ManagerActionStateV2::Succeeded
    );
    let resume = p
        .admit(
            "liveness-resume",
            ManagerActionV2::ResumeLead {
                epic_id: p.epic,
                expected: p.fence().await,
                message: "continue the existing work".into(),
            },
        )
        .await;
    let process = crate::session::launch::install_controller_candidate_test_process(id);
    #[cfg(target_os = "linux")]
    let orphan_fixture = crate::session::reaper::StartupReaperFixture::new();
    #[cfg(target_os = "linux")]
    let _orphan_guard = orphan_fixture.scoped_runtime_reap_root(id).unwrap();

    let custody_id = p
        .manager
        .store
        .lock()
        .await
        .live_custody_for_session(id)
        .unwrap()
        .custody_id;
    let holder = hold_root(custody_id, std::time::Duration::from_secs(3));
    let (resumed, report) = execute_with_probe(&p, None).await;
    holder.join().unwrap();
    report.assert_live("a resume blocked on a custody stripe");
    resumed.unwrap();
    assert_eq!(
        p.receipt(resume.operation_id).await.state,
        ManagerActionStateV2::Succeeded
    );
    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 1);
    crate::session::lifecycle::interrupt_active_in_maps(&p.manager.active, id)
        .await
        .unwrap();
    crate::session::launch::drop_controller_candidate_test_stream(id);
}

/// A fake `codex` executable pinned to one manager's runtime config (no
/// process-wide `PATH` mutation): the launch's catalog probe runs
/// `codex --version` and `codex debug models --bundled` against it.
struct FakeCodex {
    _dir: tempfile::TempDir,
    _pin: crate::codex::PinnedCodexBinary,
}

impl FakeCodex {
    fn install(manager: &SessionManager, script_body: &str) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("codex");
        std::fs::write(&path, format!("#!/bin/sh\n{script_body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let pin = crate::codex::pin_codex_binary_for_test(&manager.runtime_config, path);
        Self {
            _dir: dir,
            _pin: pin,
        }
    }
}

/// #1166 (third wedge): a Codex-provider manager create_session probes the
/// installed CLI before model admission. Whatever the CLI does, the launch must
/// finish or refuse within the catalog budget while the Store and the
/// coordinator pass stay live.
async fn codex_create_session_with_cli(script: &str, launch_bound: std::time::Duration) {
    let mut p = pilot().await;
    open_launch_policy(&mut p, "codex-launches").await;
    let parent = second_epic(&p).await;
    // The scripted-provider model passes #1506 model validation (the fake CLI
    // under test is not a real catalog), so the launch reaches the CLI probe.
    let receipt = admit_create_session_with_model(
        &p,
        "codex-create",
        1,
        parent,
        SessionProvider::Codex,
        "manager-scripted-provider",
    )
    .await;
    let child = receipt.target_session_id.unwrap();
    let _process = crate::session::launch::install_controller_candidate_test_process(child);
    let _cli = FakeCodex::install(&p.manager, script);

    // The hub runs the launch inside the manager-action loop of the coordinator
    // pass, which also claims new work while the launch is in flight. The probe
    // counts from the moment the launch is parked in the Codex catalog probe.
    let (launched, report) = run_with_probe(
        &p,
        Some((child, "provider_catalog_refresh")),
        launch_bound,
        p.manager.reconcile_manager_actions_once(),
    )
    .await;
    launched.unwrap();
    report.assert_live("a Codex launch parked in its catalog probe");
    assert_eq!(
        p.receipt(receipt.operation_id).await.state,
        ManagerActionStateV2::Succeeded,
        "the coordinator pass established the Codex worker"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn codex_create_session_with_a_slow_cli_does_not_wedge_the_manager_action_loop() {
    codex_create_session_with_cli("sleep 3600", std::time::Duration::from_secs(60)).await;
}
