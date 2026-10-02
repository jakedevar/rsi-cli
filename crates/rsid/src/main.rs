use rsi_common::recursive_dag::{
    RecursiveRecoveryBudget, RecursiveRecoveryPassStatus, RecursiveRecoverySource,
};
use rsid::agy::AgyClient;
use rsid::bus::EventBus;
use rsid::claude::ClaudeClient;
use rsid::codegraph::IndexRuntime;
use rsid::codex::CodexClient;
use rsid::config::{Config, RuntimeConfig};
use rsid::dotenv::{DaemonDotenvStatus, load_daemon_dotenv};
use rsid::error::Result;
use rsid::instance_guard::{DaemonInstanceGuard, refuse_live_daemon_socket};
use rsid::memory::manager::MemoryManager;
use rsid::openai::OpenAiClient;
use rsid::rpc::RpcServer;
use rsid::session::SessionManager;
use rsid::store::Store;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UnixListener;
use tokio::signal;
use tracing::{error, info, warn};

const MANAGER_ACTION_BACKSTOP_INTERVAL: Duration = Duration::from_secs(10);
const MANAGER_COORDINATOR_STALL_AFTER: Duration = Duration::from_secs(30);
const CONNECTION_DRAIN_LIMIT: Duration = Duration::from_secs(2);
const SHUTDOWN_LIMIT: Duration = Duration::from_secs(10);
const OPERATOR_DRAIN_LIMIT: Duration = Duration::from_secs(30);
/// How long the process waits for tokio blocking tasks after `Shutdown complete`
/// before it stops waiting and exits (#1057). Dropping a runtime otherwise
/// blocks until every `spawn_blocking` task returns, and a blocking read of a
/// provider child's pipe can outlive the daemon by minutes.
const RUNTIME_DROP_GRACE: Duration = Duration::from_secs(2);

async fn shutdown_stage<F: std::future::Future>(
    subsystem: &'static str,
    deadline: tokio::time::Instant,
    future: F,
) -> F::Output {
    info!(subsystem, "Shutdown stage started");
    tokio::pin!(future);
    let result = if let Ok(result) = tokio::time::timeout_at(deadline, &mut future).await {
        result
    } else {
        // Settlement owners must finish before the runtime exits. Keep
        // waiting, but identify the stage that exceeded the deploy bound.
        warn!(subsystem, "Shutdown stage exceeded ten-second deadline");
        future.await
    };
    info!(subsystem, "Shutdown stage complete");
    result
}

#[derive(Clone, Copy)]
struct ManagerCoordinatorProgress {
    last_completed: tokio::time::Instant,
    pass_started: Option<tokio::time::Instant>,
}

async fn run_manager_action_backstop<F, Fut>(mut reconcile: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<usize>>,
{
    let mut interval = tokio::time::interval(MANAGER_ACTION_BACKSTOP_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    interval.tick().await;
    loop {
        interval.tick().await;
        if let Err(error) = reconcile().await {
            warn!(error = %error, "Manager action backstop deferred");
        }
    }
}

async fn supervise_manager_coordinator<F>(mut spawn: F)
where
    F: FnMut() -> tokio::task::JoinHandle<()>,
{
    let mut backoff = Duration::from_secs(1);
    loop {
        let result = spawn().await;
        match result {
            Ok(()) => error!("Manager coordinator exited; restarting"),
            Err(error) => error!(error = %error, "Manager coordinator task failed; restarting"),
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

async fn program_run_startup_sequence<R, RFut, C, CFut, D, T, E>(
    restore_sessions: R,
    reconcile_program_runs: C,
    start_dispatcher: D,
) -> Option<std::result::Result<T, E>>
where
    R: FnOnce() -> RFut,
    RFut: std::future::Future<Output = bool>,
    C: FnOnce() -> CFut,
    CFut: std::future::Future<Output = bool>,
    D: FnOnce() -> std::result::Result<T, E>,
{
    if !restore_sessions().await || !reconcile_program_runs().await {
        return None;
    }
    Some(start_dispatcher())
}

fn main() -> Result<()> {
    // Informational flags exit here, before any daemon state exists. The
    // daemon starts after the match so no arm owns daemon startup: the
    // provider-capability scan reads a match arm's callees as one mapping.
    match cli_action(&std::env::args_os().skip(1).collect::<Vec<_>>()) {
        CliAction::Run => {}
        CliAction::Help => {
            println!(
                "rsid {}\nUsage: rsid [--help | --version]\nRun the RSI daemon when no flag is given.",
                env!("CARGO_PKG_VERSION")
            );
            return Ok(());
        }
        CliAction::Version => {
            println!("rsid {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        CliAction::BuildInfo => {
            // `<commit sha> <schema version>`: what `AgentRequestDeploy` probes.
            println!("{}", rsid::deploy::build_info_line());
            return Ok(());
        }
        CliAction::Invalid => {
            eprintln!("Usage: rsid [--help | --version]");
            std::process::exit(2);
        }
    }
    // Allocator tuning must precede the runtime: glibc caps only arenas
    // created after the call, and the runtime starts the worker threads
    // that would otherwise take their own arenas first (#997).
    rsid::process_memory::configure_allocator();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    run_to_bounded_exit(runtime, Box::pin(run_daemon()))
}

/// Runs the daemon future, then tears the runtime down within
/// `RUNTIME_DROP_GRACE`. A plain runtime drop waits for every blocking task, so
/// a stuck provider-child read kept rsid alive long after `Shutdown complete`
/// (#1057). Blocking threads still running after the grace are abandoned; the
/// process exits when `main` returns, so the result keeps its exit code.
fn run_to_bounded_exit<F: std::future::Future<Output = Result<()>>>(
    runtime: tokio::runtime::Runtime,
    daemon: F,
) -> Result<()> {
    let result = runtime.block_on(daemon);
    runtime.shutdown_timeout(RUNTIME_DROP_GRACE);
    // #1045: a deploy restart exits 75 so `rsid-supervisor.sh` relaunches the
    // new binary with its own environment.
    if result.is_ok() && rsid::deploy::exit_75_requested() {
        std::process::exit(rsid::watchdog::WATCHDOG_EXIT_CODE);
    }
    result
}

#[derive(Debug, PartialEq, Eq)]
enum CliAction {
    Run,
    Help,
    Version,
    BuildInfo,
    Invalid,
}

fn cli_action(args: &[std::ffi::OsString]) -> CliAction {
    match args {
        [] => CliAction::Run,
        [flag] if flag == "--help" || flag == "-h" => CliAction::Help,
        [flag] if flag == "--version" || flag == "-V" => CliAction::Version,
        [flag] if flag == "--build-info" => CliAction::BuildInfo,
        _ => CliAction::Invalid,
    }
}

async fn run_daemon() -> Result<()> {
    // tokio-console subscriber — compile-time + runtime double opt-in; zero overhead when off.
    #[cfg(feature = "tokio-console")]
    {
        if std::env::var("RSI_TOKIO_CONSOLE").as_deref() == Ok("1") {
            console_subscriber::init();
        }
    }

    // Migrate legacy ~/.flywheel/ data directory to ~/.rsi/
    if let Some(home) = dirs::home_dir() {
        let legacy_dir = home.join(".flywheel");
        let new_dir = home.join(rsi_common::identity::PROJECT_DIR_NAME);
        if legacy_dir.exists() && !new_dir.exists() {
            tracing::info!("Migrating data directory: ~/.flywheel/ → ~/.rsi/");
            if let Err(e) = std::fs::rename(&legacy_dir, &new_dir) {
                tracing::warn!("Failed to rename data directory: {e}. Falling back to symlink.");
                #[cfg(unix)]
                if let Err(e2) = std::os::unix::fs::symlink(&legacy_dir, &new_dir) {
                    tracing::error!("Failed to create symlink: {e2}. Using legacy path.");
                }
            }
        }
    }

    // Initialize tracing
    tracing_subscriber::fmt::init();
    // Keep a redirected daemon.log bounded (#960); a no-op under journald.
    rsid::log_rotation::spawn_bounded_log();

    // Route panics through tracing so they land in the same sink as everything
    // else instead of writing an unstructured line to raw stderr and vanishing
    // from whatever collects daemon logs.
    install_panic_hook();

    info!("rsid starting...");

    match load_daemon_dotenv() {
        DaemonDotenvStatus::Loaded(_) | DaemonDotenvStatus::Missing(_) => {}
        DaemonDotenvStatus::Failed { .. } => {
            warn!("Continuing without daemon dotenv values");
        }
    }

    // Load configuration
    let mut config = Config::default();
    rsi_common::identity::initialize_process_ownership_namespace(&config.socket_path)
        .map_err(rsid::error::DaemonError::Process)?;
    info!(
        socket = %config.socket_path.display(),
        context_rotation_enabled = config.context_rotation_enabled,
        memory_enabled = config.memory_enabled,
        sandbox_base = %config.sandbox_base.display(),
        "Configuration loaded"
    );

    // Create data directory if needed
    if let Some(parent) = config.socket_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    // The database is the singleton identity. Take its kernel lease before
    // opening/migrating the Store or touching the socket: startup reconciliation
    // is correct only when no other daemon still owns live provider processes.
    let data_dir = config
        .socket_path
        .parent()
        .unwrap_or(std::path::Path::new("/tmp"))
        .to_path_buf();
    let db_path = data_dir.join(rsi_common::identity::DB_FILENAME);
    let instance_guard = DaemonInstanceGuard::acquire(&db_path, &config.socket_path)?;
    info!(
        lock = %instance_guard.path().display(),
        db = %db_path.display(),
        "Acquired daemon single-instance lease"
    );

    // Bridge the first guarded rollout: an already-running legacy binary does
    // not hold the new file lease. A connectable socket is positive incumbent
    // evidence and must never be unlinked. Missing/refused is the stale case.
    refuse_live_daemon_socket(&config.socket_path).await?;

    // Open/migrate the singleton Store before any provider probe or worker can
    // create a subprocess. The process-first ownership fence inventories
    // exact stamps, accepts the exact configured daemon namespace, and uses a
    // single deadline-bounded Store snapshot only for namespace-less legacy
    // children carrying this exact socket. It reaches a two-empty-pass fixed
    // point before startup advances. A copied database on another socket
    // remains outside this daemon's authority even when UUIDs overlap.
    let store = Store::open(&db_path).inspect_err(|e| {
        error!(db = %db_path.display(), error = %e, "Database open/migration failed; daemon cannot start");
    })?;
    info!(db = %db_path.display(), "Database opened");
    match rsid::watchdog::import_pending_restart_records(&data_dir, |record| {
        store
            .persist_daemon_restart_record(record)
            .map_err(std::io::Error::other)
    }) {
        Ok(records) if !records.is_empty() => {
            warn!(
                count = records.len(),
                "Imported durable watchdog restart evidence"
            );
        }
        Ok(_) => {}
        Err(error) => {
            error!(%error, "Watchdog restart evidence import failed; sidecar retained");
        }
    }
    let reaped_startup_processes = rsid::session::reap_startup_process_ownership_checked(&store)?;
    if reaped_startup_processes > 0 {
        warn!(
            reaped = reaped_startup_processes,
            "Reaped Store-owned provider/tool orphan(s) before provider probes"
        );
    }

    // Validate providers only after proving this process owns daemon custody.
    // The local probe can wait on network I/O; a duplicate must fail before
    // spending that time or initializing anything beyond singleton evidence.
    let claude_available = ClaudeClient::is_available();
    let codex_available = CodexClient::is_available();
    let local_available = OpenAiClient::is_local_available().await;
    let antigravity_available = AgyClient::is_available();
    if !claude_available && !codex_available && !local_available && !antigravity_available {
        error!(
            "No supported provider boundary found (Claude, Codex/Pioneer, Local, or Antigravity)."
        );
        return Err(rsid::error::DaemonError::Process(
            "No supported provider found".to_string(),
        ));
    }
    if claude_available {
        info!("Claude binary found");
    } else {
        warn!("Claude binary not found; Claude provider unavailable");
    }
    if codex_available {
        info!("Codex binary found");
    } else {
        warn!("Codex binary not found; Codex provider unavailable");
    }
    if local_available {
        info!("Local model server reachable");
    } else {
        warn!("Local model server not found; Local provider unavailable");
    }
    if antigravity_available {
        info!("Antigravity binary found");
    } else {
        warn!("Antigravity binary not found; Antigravity provider unavailable");
    }

    // Remove stale socket
    if config.socket_path.exists() {
        warn!(socket = %config.socket_path.display(), "Removing stale socket file");
        tokio::fs::remove_file(&config.socket_path).await?;
    }

    // Create Unix socket listener
    let listener = UnixListener::bind(&config.socket_path)?;

    // Set socket permissions (owner only)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&config.socket_path, std::fs::Permissions::from_mode(0o600))?;
    }

    info!(
        startup_milestone = "socket_bound",
        socket = %config.socket_path.display(),
        "Unix socket bound; RPC acceptance pending initialization"
    );

    // C5 activation gates the durable pending journal before any manager can
    // restore or reconcile sessions. Historical Failed rows are never scanned.
    store.ensure_c5_autofile_activation()?;

    // Ensure tag autocomplete index (idempotent — V43 already creates it).
    store
        .ensure_tag_indexes()
        .unwrap_or_else(|e| warn!("ensure_tag_indexes: {}", e));

    let event_bus = Arc::new(EventBus::new(config.event_buffer_size));

    // RSI-026: seed `daemon_settings.system_prompt_preset` from state.json
    // on first boot after V48 migration. Idempotent thereafter — subsequent
    // boots return the existing row without re-reading state.json.
    let state_json_path = rsi_common::identity::data_path("state.json", "state.json");
    let system_prompt_preset_seed =
        rsid::store::daemon_settings::maybe_import_legacy_system_prompt_preset(
            &store,
            &state_json_path,
        )?;
    rsid::store::daemon_settings::maybe_import_legacy_memory_settings(&store, &state_json_path)?;
    let runtime_config =
        RuntimeConfig::from_config_with_system_prompt_preset(&config, system_prompt_preset_seed);
    let persisted_daemon_settings =
        rsid::store::daemon_settings::apply_persisted_runtime_config(&store, &runtime_config)?;
    // #1036: publish the operator's spend caps for scripts/cloud-spend.py.
    if let Err(error) = rsid::cloud_spend::write_caps_file(
        &rsid::cloud_spend::default_cloud_dir(),
        &runtime_config.cloud_spend_caps(),
    ) {
        warn!(%error, "could not write the remote-gate spend caps file");
    }
    if persisted_daemon_settings > 0 {
        config.apply_runtime_config_snapshot(&runtime_config);
        info!(
            count = persisted_daemon_settings,
            "Applied persisted daemon settings"
        );
    }

    // #694 K1: open the provider key vault after the persisted
    // `vault.*` settings are applied. A vault whose directory or file mode is
    // looser than 0700/0600 (or is malformed) refuses daemon start and names
    // the path; nothing falls back silently past a tombstone.
    let vault = rsid::vault::open_default(Arc::clone(&runtime_config.vault_settings))
        .map_err(|error| rsid::error::DaemonError::Process(format!("key vault: {error}")))?;
    if rsid::vault::install_global(vault).is_err() {
        warn!("key vault was already installed; keeping the first handle");
    }
    rsid::vault::log_provider_credential_health(&rsid::vault::global());

    // Codegraph uses its own project-bound databases. An unavailable project
    // or index subsystem must not prevent the session daemon from starting.
    let index_root = db_path.with_file_name("codegraph");
    let mut _codegraph_runtime = match IndexRuntime::start_from_store(
        index_root.clone(),
        &store,
        &config.workspace_roots,
        Some(Arc::clone(&event_bus)),
        Arc::clone(&runtime_config.codegraph_indexing_enabled),
    ) {
        Ok(runtime) => Some(runtime),
        Err(error) => {
            warn!(%error, "Codegraph startup snapshot rejected; registry refresh will retry");
            IndexRuntime::start_with_registrations_and_bus_and_gate(
                index_root,
                Vec::new(),
                Vec::new(),
                &config.workspace_roots,
                Some(Arc::clone(&event_bus)),
                Arc::clone(&runtime_config.codegraph_indexing_enabled),
            )
            .inspect_err(|error| warn!(%error, "Codegraph registry fallback failed"))
            .ok()
        }
    };

    // Initialize memory system (enabled by default; disable with RSI_MEMORY_ENABLED=false)
    let memory_manager = if let Some(memory_config) = config.memory_config() {
        // Ensure memory directory exists
        if let Err(e) = tokio::fs::create_dir_all(&memory_config.memory_dir).await {
            warn!(error = %e, "Failed to create memory directory, memory system disabled");
            None
        } else {
            match init_memory_system(&memory_config, &db_path, &event_bus, &runtime_config).await {
                Ok(mm) => {
                    info!(
                        memory_dir = %memory_config.memory_dir.display(),
                        db_path = %memory_config.db_path.display(),
                        "Memory system initialized"
                    );
                    Some(Arc::new(mm))
                }
                Err(e) => {
                    warn!(error = %e, "Memory system initialization failed, continuing without memory");
                    None
                }
            }
        }
    } else {
        info!("Memory system disabled (RSI_MEMORY_ENABLED=false)");
        None
    };

    let memory_handle = memory_manager.as_ref().map(|m| m.handle().clone());
    // #1045: pin the start time and hash the running binary in the background.
    rsid::daemon_info::DaemonInfoService::init_global(config.sandbox_base.clone());
    let mut session_manager = SessionManager::new(
        Arc::clone(&event_bus),
        store,
        config.context_rotation_enabled,
        config.socket_path.clone(),
        memory_handle,
        config.workspace_roots.clone(),
        Arc::clone(&runtime_config),
        config.sandbox_base.clone(),
    )?;
    if let Some(runtime) = &mut _codegraph_runtime {
        runtime.attach_registry(
            session_manager.store().clone(),
            config.workspace_roots.clone(),
        );
        session_manager.set_codegraph_handle(runtime.handle().clone());
    }

    if codex_available && let Err(error) = session_manager.refresh_codex_catalog_at_startup().await
    {
        warn!(%error, "Codex catalog startup refresh failed; using repository fallbacks");
    }

    // Initialize background task queue
    let queue_handle = if let Some(queue_config) = config.queue_config() {
        use rsid::queue::processor::NoOpProcessor;
        use rsid::queue::worker::spawn_queue_worker;

        let handle = spawn_queue_worker(
            session_manager.store().clone(),
            Arc::clone(&event_bus),
            queue_config,
            Box::new(NoOpProcessor),
        );
        info!("Background task queue initialized");
        Some(handle)
    } else {
        info!("Background task queue disabled");
        None
    };

    if let Some(qh) = queue_handle {
        session_manager.set_queue_handle(qh);
    }

    // Take retry receiver before wrapping in Arc
    let retry_rx = session_manager
        .take_retry_rx()
        .expect("retry_rx already taken");

    // Take spawn-request receiver. The SpawnCoordinator inside `session_manager`
    // enqueues `SpawnRequest`s here when an Epic-lead session emits a
    // `<docregblock>/spawn_child …</docregblock>` directive.
    let mut spawn_rx = session_manager
        .take_spawn_rx()
        .expect("spawn_rx already taken");
    let mut successor_rx = session_manager
        .take_successor_rx()
        .expect("successor_rx already taken");

    let session_manager = Arc::new(session_manager);

    // Settle the operator queue's crash boundary before restore can start a
    // monitor or the main loop can dispatch another provider turn.
    let (retryable, uncertain) = session_manager
        .reconcile_operator_messages_at_startup()
        .await?;
    if retryable > 0 || uncertain > 0 {
        info!(
            retryable,
            uncertain, "Reconciled operator message claims after restart"
        );
    }

    // Restore sessions from previous daemon runs. A provider-inventory failure
    // is a daemon-wide startup fence: later best-effort reconciliation and
    // background services must not mutate away the durable candidate cohort.
    // Other restore failures retain the historical degraded-start behavior.
    let session_restore_ready = match session_manager.restore_sessions().await {
        Ok(()) => {
            info!("Previous sessions restored from database");
            if let Err(error) = session_manager.reconcile_automatic_child_watches().await {
                warn!(error = %error, "Automatic agent child watch reconciliation deferred");
            }
            true
        }
        Err(error @ rsid::error::DaemonError::StartupProviderInventory(_)) => {
            return Err(error);
        }
        Err(error) => {
            warn!(error = %error, "Failed to restore sessions; ProgramRun recovery remains disabled");
            false
        }
    };
    if session_restore_ready {
        // The exact journal owns graceful interruptions. Claim it after
        // invocation and custody restore, before schedulers or watch dispatch.
        session_manager
            .reconcile_restart_intents_at_startup()
            .await?;
    }

    let program_run_dispatcher = match program_run_startup_sequence(
        || async { session_restore_ready },
        || async {
            match session_manager.reconcile_program_runs_at_startup().await {
                Ok(()) => true,
                Err(error) => {
                    warn!(error = %error, "ProgramRun startup reconciliation failed; dispatcher remains disabled");
                    false
                }
            }
        },
        || session_manager.start_program_run_dispatcher(),
    )
    .await
    {
        None => None,
        Some(Ok(runtime)) => {
            info!("ProgramRun dispatcher started after bounded startup reconciliation");
            Some(runtime)
        }
        Some(Err(error)) => {
            warn!(error = %error, "ProgramRun dispatcher unavailable; operator status remains available");
            None
        }
    };

    // C-P2-15: the bounded AppServer control worker. Startup keyset
    // reconciliation runs inside this call and gates writer admission, so a
    // failure here must leave the worker unstarted rather than admitting a
    // possibly duplicate turn.
    let app_server_seal_worker = match session_manager.start_app_server_seal_worker().await {
        Ok(runtime) => {
            info!("AppServer control worker started after startup keyset reconciliation");
            Some(runtime)
        }
        Err(error) => {
            warn!(
                error = %error,
                "AppServer control worker unavailable; writer admission stays disabled"
            );
            None
        }
    };

    // Issue 21 P2-06c: the periodic agent-message reconciliation worker. This
    // is what finally WIRES crash recovery and expiry — the P2-06a/b writers
    // had no production caller until now.
    //
    // DETACHED, not an awaited startup gate, and the difference is deliberate.
    // `start_app_server_seal_worker` above is awaited because its startup
    // reconciliation GATES writer admission, so failing closed withholds a
    // capability that could otherwise duplicate a paid turn. Reconciliation
    // gates nothing — it repairs custody a dead incarnation left behind — so an
    // awaited gate here would buy no safety while making one bad row able to
    // stop the daemon from starting. Running detached beside a live daemon is
    // sound because the crash classifier excludes every row carrying THIS
    // incarnation's `delivery_boot_id`, so the reconciler provably cannot touch
    // an attempt the running dispatcher owns.
    //
    // There is no `?` here and none is possible: the pass returns a report, not
    // a `Result`. Errors are counted and logged inside the loop.
    //
    // A PANIC is contained too, and deliberately not here. This task is detached
    // and its `JoinHandle` is dropped, so an escaping panic would end
    // reconciliation silently for the whole process lifetime. Containment lives
    // where the pass is synchronous and therefore trivially wrappable —
    // `agent_message_reconciler::reconcile_agent_messages_pass_catching_panics`,
    // called from `SessionManager::reconcile_agent_messages_once` inside the
    // store-guard scope. Nothing panic-related belongs at this spawn site.
    {
        let manager = Arc::clone(&session_manager);
        tokio::spawn(manager.run_agent_message_reconciliation_loop());
    }
    {
        let manager = Arc::clone(&session_manager);
        tokio::spawn(manager.run_archived_sandbox_purge_loop());
    }
    // Issue #1007: the daemon-owned rolling merge queue. It reconciles
    // interrupted runs once, then claims work only while the operator setting
    // is on.
    tokio::spawn(rsid::rolling_queue::run_rolling_queue_loop(
        Arc::clone(session_manager.store()),
        Arc::clone(&runtime_config),
        rsid::rolling_queue::LanderLauncher::discover(),
    ));

    // Issue #1045: daemon-owned deploys. Verifies a `restarting` deploy once,
    // then waits for a quiet point and restarts through the graceful drain.
    {
        let service = rsid::deploy::DeployService::init_global(config.sandbox_base.clone());
        let manager = Arc::clone(&session_manager);
        service.set_restart_trigger(Arc::new(move || {
            rsid::deploy::request_exit_75();
            manager.request_drain_restart();
        }));
        tokio::spawn(rsid::deploy::run_deploy_loop(
            Arc::clone(session_manager.store()),
            service,
            session_manager.deploy_drain(),
            Arc::clone(&runtime_config),
        ));
    }

    // Issue #1002: daemon-owned durable agent jobs. The first poll reconciles
    // units that finished while rsid was down; each job settles and wakes its
    // owner exactly once.
    tokio::spawn(rsid::agent_jobs::run_agent_jobs_loop(
        Arc::clone(session_manager.store()),
        Arc::new(rsid::agent_jobs::SystemdJobRuntime::default()),
    ));

    // Restore has armed all retry candidates; now perform the one bounded C5
    // pending-journal replay without delaying startup or scanning sessions.
    {
        let control = session_manager.agent_control();
        tokio::spawn(async move {
            control.replay_c5_autofile_pending().await;
        });
    }

    let recursive_recovery_budget = recursive_startup_recovery_budget(&config);
    {
        let store = session_manager.store().lock().await;
        match store.recover_recursive_live_attempts_with_budget(recursive_recovery_budget.clone()) {
            Ok(report) if report.deferred > 0 || report.errors > 0 => warn!(
                checked = report.checked,
                recovered = report.recovered,
                lost = report.lost,
                failed = report.failed,
                interrupted = report.interrupted,
                terminal_session_observed = report.terminal_session_observed,
                deferred = report.deferred,
                skipped = report.skipped,
                malformed = report.malformed,
                errors = report.errors,
                stop_reason = ?report.stop_reason,
                "Recursive DAG live restart recovery deferred or incomplete"
            ),
            Ok(report)
                if report.checked > 0
                    || report.recovered > 0
                    || report.lost > 0
                    || report.failed > 0
                    || report.interrupted > 0
                    || report.malformed > 0 =>
            {
                info!(
                    checked = report.checked,
                    recovered = report.recovered,
                    unchanged = report.unchanged,
                    lost = report.lost,
                    failed = report.failed,
                    interrupted = report.interrupted,
                    terminal_session_observed = report.terminal_session_observed,
                    deferred = report.deferred,
                    skipped = report.skipped,
                    malformed = report.malformed,
                    errors = report.errors,
                    stop_reason = ?report.stop_reason,
                    "Recursive DAG live restart recovery pass complete"
                );
            }
            Ok(_) => {}
            Err(e) => warn!("recursive DAG live restart recovery failed: {}", e),
        }
    }

    match session_manager
        .commit_recoverable_recursive_live_outputs_after_restart(recursive_recovery_budget.clone())
        .await
    {
        Ok((checked, committed, deferred)) if checked > 0 || committed > 0 || deferred > 0 => {
            info!(
                checked,
                committed,
                deferred,
                "Recursive DAG live completed-session output recovery pass complete"
            );
        }
        Ok(_) => {}
        Err(e) => warn!(
            error = %e,
            "Recursive DAG live completed-session output recovery failed"
        ),
    }

    match session_manager
        .reconcile_closure_outputs_after_restart()
        .await
    {
        Ok(report)
            if report.examined > 0
                || report.committed > 0
                || report.replayed > 0
                || report.deferred > 0 =>
        {
            info!(
                examined = report.examined,
                committed = report.committed,
                replayed = report.replayed,
                deferred = report.deferred,
                deadline_reached = report.deadline_reached,
                "Closure terminal-output startup reconciliation pass complete"
            );
        }
        Ok(_) => {}
        Err(error) => {
            warn!(error = %error, "Closure terminal-output startup reconciliation deferred")
        }
    }
    Arc::clone(&session_manager).run_closure_output_reconciliation_loop();

    {
        let store = session_manager.store().lock().await;
        match store
            .recover_recursive_task_graphs_after_restart_with_budget(recursive_recovery_budget)
        {
            Ok(report)
                if report.status == RecursiveRecoveryPassStatus::Deferred || report.errors > 0 =>
            {
                warn!(
                    pass_id = %report.id,
                    checked = report.checked,
                    recovered = report.recovered,
                    quarantined = report.quarantined,
                    deferred = report.deferred,
                    skipped = report.skipped,
                    errors = report.errors,
                    stop_reason = ?report.stop_reason,
                    "Recursive DAG restart recovery deferred or incomplete"
                );
            }
            Ok(report) if report.checked > 0 || report.quarantined > 0 => info!(
                pass_id = %report.id,
                checked = report.checked,
                recovered = report.recovered,
                quarantined = report.quarantined,
                deferred = report.deferred,
                skipped = report.skipped,
                errors = report.errors,
                stop_reason = ?report.stop_reason,
                "Recursive DAG restart recovery pass complete"
            ),
            Ok(_) => {}
            Err(e) => warn!("recursive DAG restart recovery failed: {}", e),
        }
    }

    // #634: durable topology executions left in flight by a previous daemon
    // incarnation resume beside the recursive passes, gated by the kill
    // switch and bounded by the same startup recovery budget.
    Arc::clone(&session_manager)
        .start_topology_executor(
            config.recursive_dag_startup_recovery_max_graphs as usize,
            std::time::Duration::from_millis(config.recursive_dag_startup_recovery_time_budget_ms),
        )
        .await;

    match session_manager.reconcile_topology_workflows().await {
        Ok(count) => info!(
            topology_count = count,
            "Topology workflow bridge reconciled"
        ),
        Err(e) => warn!(error = %e, "Failed to reconcile topology workflow bridge"),
    }

    // Stall classifier signal channel — moved BEFORE stall_detector spawn
    // so the detector can receive a clone of the Sender. Channel is created
    // unconditionally; the classifier task and the detector's classifier
    // branch are both gated on `stall_classifier_enabled`.
    let (stall_classifier_tx, stall_classifier_rx) = tokio::sync::mpsc::channel::<uuid::Uuid>(64);
    let (nudge_tx, nudge_rx) =
        tokio::sync::mpsc::channel::<(uuid::Uuid, rsid::stall_classifier::NudgeAction)>(64);

    // Spawn stall detection background task
    let _stall_detector_handle = if config.stall_detection_enabled {
        use rsid::reconciliation::StallAction;
        // Only hand the detector the classifier sender when the classifier
        // is also enabled, so the detector's branch is a true no-op when
        // off (avoids per-tick gating + spurious `try_send` calls).
        let detector_classifier_tx = if config.stall_classifier_enabled {
            Some(stall_classifier_tx.clone())
        } else {
            None
        };
        Some(rsid::stall_detector::spawn_stall_detector(
            session_manager.active(),
            Arc::clone(&event_bus),
            rsid::stall_detector::StallConfig {
                running_secs: config.stall_timeout_running_secs,
                waiting_secs: config.stall_timeout_waiting_secs,
                standard_action: StallAction::from_str(
                    &config.reconciliation_stall_action_standard,
                ),
                unattended_action: StallAction::from_str(
                    &config.reconciliation_stall_action_unattended,
                ),
                classifier_idle_secs: config.stall_classifier_idle_secs,
                classifier_idle_secs_codex: config.stall_classifier_idle_secs_codex,
                classifier_cooldown_secs: config.stall_classifier_cooldown_secs,
                classifier_max_per_session: config.stall_classifier_max_per_session,
            },
            session_manager.retry_sender(),
            detector_classifier_tx,
            Arc::clone(&runtime_config),
        ))
    } else {
        info!("Stall detection disabled");
        None
    };

    // Spawn chain_driver background task — closes the master_improve convergence loop.
    let cancel_token = tokio_util::sync::CancellationToken::new();
    let _chain_driver_handle = rsid::session::chain_driver::spawn_chain_driver(
        Arc::clone(&session_manager),
        session_manager.store().clone(),
        Arc::clone(&event_bus),
        cancel_token.clone(),
    );
    tracing::info!("chain_driver background task spawned");

    // Spawn reconciliation loop background task
    let reconciliation_heartbeat = config
        .reconciliation_enabled
        .then(rsid::watchdog::LoopHeartbeat::new);
    let _reconciliation_handle = if config.reconciliation_enabled {
        use rsid::reconciliation::{ReconciliationConfig, StallAction};
        info!(
            liveness_interval_secs = config.reconciliation_liveness_interval_secs,
            consistency_interval_secs = config.reconciliation_consistency_interval_secs,
            "Reconciliation loop enabled"
        );
        Some(
            rsid::reconciliation::spawn_reconciliation_loop_with_heartbeat(
                session_manager.active(),
                Arc::clone(session_manager.store()),
                Arc::clone(&event_bus),
                ReconciliationConfig {
                    liveness_interval_secs: config.reconciliation_liveness_interval_secs,
                    consistency_interval_secs: config.reconciliation_consistency_interval_secs,
                    standard_stall_action: StallAction::from_str(
                        &config.reconciliation_stall_action_standard,
                    ),
                    unattended_stall_action: StallAction::from_str(
                        &config.reconciliation_stall_action_unattended,
                    ),
                },
                Some(session_manager.agent_control()),
                reconciliation_heartbeat.clone(),
            ),
        )
    } else {
        info!("Reconciliation loop disabled");
        None
    };

    // Create exactly one runtime from durable policy before any model-capable
    // service is started.  It is shared by Dream and the RPC control surface.
    let model_control_runtime = {
        let store = session_manager.store().lock().await;
        rsid::model_control::ModelControlRuntime::from_store_fail_closed(&store)
    };
    let llm = rsid::dreamer::llm_client::DreamerLlmClient::new(
        Arc::clone(session_manager.store()),
        Arc::clone(&event_bus),
        *runtime_config.dream_model_provider.read(),
        config
            .dream_api_url
            .clone()
            .unwrap_or_else(|| "https://api.anthropic.com/v1/messages".to_string()),
        config.dream_api_key.clone(),
        config
            .dream_model
            .clone()
            .unwrap_or_else(|| "claude-sonnet-5".to_string()),
    );
    let dream_config = rsid::dreamer::scheduler::DreamConfig {
        observation_threshold: config.dream_observation_threshold,
        idle_secs: config.dream_idle_secs,
        cooldown_secs: config.dream_cooldown_secs,
        batch_size: config.dream_batch_size,
        poll_interval_secs: 1,
        max_model_calls: (config.dream_batch_size.saturating_mul(3)).max(1) as u32,
        max_estimated_input_tokens: 96_000,
        max_estimated_output_tokens: 24_000,
        max_estimated_total_tokens: 120_000,
        max_wall_time_ms: 1_800_000,
    };

    let dreamer_handle = rsid::dreamer::scheduler::spawn_dreamer_with_model_control_runtime(
        Arc::clone(session_manager.store()),
        Arc::clone(&event_bus),
        llm,
        dream_config,
        session_manager.active(),
        Arc::clone(&runtime_config),
        model_control_runtime.clone(),
    );
    info!(
        dream_enabled = config.dream_enabled,
        observation_threshold = config.dream_observation_threshold,
        idle_secs = config.dream_idle_secs,
        cooldown_secs = config.dream_cooldown_secs,
        batch_size = config.dream_batch_size,
        "Dream consolidation system initialized"
    );
    if !config.dream_enabled {
        info!("Dream consolidation starts disabled; runtime enable applies live");
    }

    // Stall classifier (RSI-0XX). Opt-in via RSI_STALL_CLASSIFIER_ENABLED.
    // Channels were created above (before the stall_detector spawn) so the
    // detector could receive a clone of the Sender. The classifier task
    // and the detector's classifier branch are both gated on the same
    // enabled flag.
    let _stall_classifier_handle = if config.stall_classifier_enabled {
        let llm = rsid::stall_classifier::StallClassifierLlmClient::new(
            config.stall_classifier_api_url.clone(),
            config.stall_classifier_api_key.clone(),
            config.stall_classifier_model.clone(),
            std::time::Duration::from_secs(config.stall_classifier_timeout_secs),
        );
        let classifier_config = rsid::stall_classifier::ClassifierConfig::from_config(&config);
        let handle = rsid::stall_classifier::spawn_classifier(
            Arc::clone(session_manager.store()),
            session_manager.active(),
            Arc::clone(&event_bus),
            llm,
            classifier_config,
            Arc::clone(&runtime_config),
            stall_classifier_rx,
            nudge_tx.clone(),
        );
        info!(
            model = %config.stall_classifier_model,
            idle_secs = config.stall_classifier_idle_secs,
            idle_secs_codex = config.stall_classifier_idle_secs_codex,
            cooldown_secs = config.stall_classifier_cooldown_secs,
            max_per_session = config.stall_classifier_max_per_session,
            confidence_floor = config.stall_classifier_confidence_floor,
            "Stall classifier enabled"
        );
        Some(handle)
    } else {
        // Drain the rx so Phase 2 sends never block on a full buffer when
        // classifier is disabled and Sender is still held.
        let mut rx = stall_classifier_rx;
        tokio::spawn(async move { while rx.recv().await.is_some() {} });
        info!("Stall classifier disabled (RSI_STALL_CLASSIFIER_ENABLED=false)");
        None
    };
    // Phase 4: nudge consumer loop. Receives `(session_id, NudgeAction)`
    // from the classifier and dispatches `Continue` verdicts through
    // `SessionManager::continue_session`. `NotifyOnly` should never reach
    // this loop (the classifier filters before send) but is handled
    // defensively as a logged no-op so an over-broad send never corrupts
    // session state.
    {
        let mut rx = nudge_rx;
        let session_manager_for_nudge = Arc::clone(&session_manager);
        tokio::spawn(async move {
            while let Some((session_id, action)) = rx.recv().await {
                match action {
                    rsid::stall_classifier::NudgeAction::Continue { verdict, prompt } => {
                        tracing::info!(
                            session_id = %session_id,
                            verdict = ?verdict,
                            "Stall classifier nudge: invoking continue_session"
                        );
                        // K2: fenced; a refusal is a typed log and a drop.
                        if let Err(e) = session_manager_for_nudge
                            .continue_stall_nudge(
                                session_id,
                                rsi_common::daemon_message::wrap("stall-nudge", &prompt),
                            )
                            .await
                        {
                            tracing::warn!(
                                session_id = %session_id,
                                error = %e,
                                "Stall classifier nudge: continue_session failed"
                            );
                        }
                    }
                    rsid::stall_classifier::NudgeAction::NotifyOnly { verdict } => {
                        tracing::debug!(
                            session_id = %session_id,
                            verdict = ?verdict,
                            "Stall classifier nudge: NotifyOnly reached consumer; ignoring"
                        );
                    }
                }
            }
        });
    }
    // `stall_classifier_tx` is held by the detector (when classifier is
    // enabled) and the keepalive below ensures the Sender survives when
    // the detector itself is disabled. `nudge_tx` is held by the
    // classifier task; the Phase 1 drain on `nudge_rx` keeps the channel
    // healthy until Phase 4 wires the real consumer.
    let _stall_classifier_tx_keepalive = stall_classifier_tx;
    let _nudge_tx_keepalive = nudge_tx;

    // Spawn issue tracker poller if configured
    let issue_tracker_manager = if let Some(it_config) = config.issue_tracker_config() {
        use rsid::issue_tracker::linear::LinearClient;
        use rsid::issue_tracker::local::LocalTracker;
        use rsid::issue_tracker::manager::IssueTrackerManager;
        use rsid::issue_tracker::tracker::Tracker;

        let local_project_id = it_config.project_id;
        let local_project_exists = if it_config.kind == "local" {
            match local_project_id {
                Some(project_id) => session_manager
                    .store()
                    .lock()
                    .await
                    .get_project(project_id)?
                    .is_some(),
                None => {
                    warn!("Local issue tracker disabled: missing project binding");
                    false
                }
            }
        } else {
            true
        };
        if !local_project_exists {
            warn!(
                project_id = ?local_project_id,
                "Local issue tracker disabled: configured project does not exist"
            );
            None
        } else {
            let tracker: Box<dyn Tracker> = match it_config.kind.as_str() {
                "local" => Box::new(LocalTracker::new_for_project(
                    session_manager.store().clone(),
                    local_project_id.ok_or_else(|| {
                        rsid::error::DaemonError::InvalidParam(
                            "local issue tracker project binding is required".to_string(),
                        )
                    })?,
                )),
                _ => Box::new(LinearClient::new(reqwest::Client::new())),
            };

            let manager = Arc::new(IssueTrackerManager::new(
                it_config,
                tracker,
                Arc::clone(&event_bus),
                Arc::clone(&session_manager)
                    as Arc<dyn rsid::issue_tracker::poller::SessionLauncher>,
                session_manager.store().clone(),
            ));
            // Restore dispatch state from DB
            if let Err(e) = manager.restore_from_db().await {
                warn!(
                    error = %e,
                    "Failed to restore issue tracker state (starting fresh)"
                );
            }
            // Spawn the polling loop
            let _poll_handle = Arc::clone(&manager).spawn();
            // Spawn the completion listener
            let _completion_handle = manager.spawn_completion_listener();
            info!("Issue tracker polling enabled");
            Some(manager)
        }
    } else {
        info!(
            "Issue tracker not configured (set RSI_LINEAR_API_KEY + RSI_LINEAR_TEAM_ID + RSI_ISSUE_TRACKER_WORKING_DIR, or RSI_ISSUE_TRACKER_KIND=local + RSI_ISSUE_TRACKER_WORKING_DIR)"
        );
        None
    };

    // Spawn scheduled jobs scheduler
    let scheduler_heartbeat = config
        .scheduler_enabled
        .then(rsid::watchdog::LoopHeartbeat::new);
    session_manager.clear_stale_harness_process_wakes().await;
    let scheduler_handle = if config.scheduler_enabled {
        let launcher =
            Arc::clone(&session_manager) as Arc<dyn rsid::issue_tracker::poller::SessionLauncher>;
        let handle = rsid::scheduler::spawn_scheduler_with_heartbeat(
            session_manager.store().clone(),
            Arc::clone(&event_bus),
            launcher,
            config.scheduler_poll_interval_secs,
            scheduler_heartbeat.clone(),
        );
        info!(
            "Scheduled jobs scheduler enabled (poll interval: {}s)",
            config.scheduler_poll_interval_secs
        );
        Some(handle)
    } else {
        info!("Scheduled jobs scheduler disabled (RSI_SCHEDULER_ENABLED=false)");
        None
    };
    if let Some(ref scheduler_handle) = scheduler_handle {
        session_manager.install_harness_process_scheduler(scheduler_handle.clone());
    }

    // A8: terminal-watch acceleration service — bridges bus events
    // (terminal flips, reconciled bypass flips, raised questions) to the
    // scheduler's due-poll over persisted watch rows. Correctness lives in
    // the poll (DB truth); this only cuts delivery latency. Only meaningful
    // when the scheduler runs.
    if let Some(ref sh) = scheduler_handle {
        rsid::watch_service::spawn_terminal_watch_service(
            Arc::clone(&event_bus),
            session_manager.store().clone(),
            sh.clone(),
        );
        info!("Terminal watch service enabled");
    }

    // Shared HTTP client for Ollama — one pool, reused across compile + warmup
    // + ad-hoc GenerateText calls.
    let ollama_http = reqwest::Client::new();

    // Prompt-compile engine (streaming, LRU-cached, supersede-by-caller).
    let compile_engine = rsid::prompt_compile::CompileEngine::new(
        ollama_http.clone(),
        session_manager.store().clone(),
        Arc::clone(&runtime_config),
        Arc::clone(&event_bus),
    );

    // Background warmup — non-blocking, retries with exponential backoff.
    {
        let warmup_model = runtime_config.prompt_compile_model_local.read().clone();
        let warmup_http = ollama_http.clone();
        tokio::spawn(async move {
            let mut delay = std::time::Duration::from_secs(1);
            loop {
                match rsid::ollama_client::warmup(&warmup_http, &warmup_model).await {
                    Ok(()) => {
                        info!(model = %warmup_model, "ollama warmup succeeded");
                        return;
                    }
                    Err(e) => {
                        tracing::debug!(error = %e, "ollama warmup failed; retrying");
                        tokio::time::sleep(delay).await;
                        delay = (delay * 2).min(std::time::Duration::from_secs(60));
                    }
                }
            }
        });
    }

    // Registry controls and this poller ship together: disabled/unpaired peers
    // never cause a socket connection or a remote daemon launch.
    rsid::start_satellite_hub_poller(
        Arc::clone(session_manager.store()),
        Arc::clone(&runtime_config),
    );

    let mut rpc_server = RpcServer::new(
        Arc::clone(&session_manager),
        memory_manager.clone(),
        issue_tracker_manager,
        scheduler_handle.clone(),
        Arc::clone(&runtime_config),
        compile_engine,
        ollama_http,
        model_control_runtime.clone(),
    );
    if let Some(runtime) = &_codegraph_runtime {
        rpc_server.set_codegraph_handle(runtime.handle().clone());
    }
    rpc_server.set_dreamer_handle(dreamer_handle.clone());
    rpc_server.init_dialectic(&config);
    let rpc_server = Arc::new(rpc_server);

    // Spawn retry handler loop.
    //
    // A9.1 (D7): dispatch each retry with BOUNDED CONCURRENCY rather than
    // awaiting `launch_retry` inline. The pre-A9.1 loop was strictly serial, so
    // a single slow or hung `launch_retry` stalled every other queued retry
    // (F-004 liveness gap). A9's single-flight guard on the original session id
    // makes concurrent fires safe, so `run_bounded_retry_dispatch` caps in-flight
    // launches at `RETRY_HANDLER_CONCURRENCY` and runs each on its own task — a
    // hung launch now ties up at most one permit, never the whole queue.
    const RETRY_HANDLER_CONCURRENCY: usize = 4;
    let session_manager_for_retry = Arc::clone(&session_manager);
    tokio::spawn(rsid::session::run_bounded_retry_dispatch(
        retry_rx,
        RETRY_HANDLER_CONCURRENCY,
        move |session_id| {
            let session_manager = Arc::clone(&session_manager_for_retry);
            async move {
                if let Err(e) = session_manager.launch_retry(session_id).await {
                    error!(
                        error = %e,
                        session_id = %session_id,
                        "Retry handler: launch_retry failed"
                    );
                }
            }
        },
    ));

    // Spawn-request handler loop. Consumes `SpawnRequest`s emitted by the
    // SpawnCoordinator (Phase 3, Epic lead-agent graph entry-point) and
    // turns each into a real `launch_session` call. Mirrors the retry-loop
    // pattern — keeps the launch path centralized while letting monitor
    // tasks (which have no `&self` access) trigger child spawns.
    let session_manager_for_spawn = Arc::clone(&session_manager);
    let event_bus_for_spawn = Arc::clone(&event_bus);
    tokio::spawn(async move {
        while let Some(req) = spawn_rx.recv().await {
            // #1073: while a deploy drains, each spawn request waits in its own
            // task so one held launch never stalls the consumer.
            let manager = Arc::clone(&session_manager_for_spawn);
            let bus = Arc::clone(&event_bus_for_spawn);
            if manager.deploy_drain().is_draining() {
                tokio::spawn(process_spawn_request(manager, bus, req));
            } else {
                process_spawn_request(manager, bus, req).await;
            }
        }
    });
    let session_manager_for_successor = Arc::clone(&session_manager);
    tokio::spawn(async move {
        while let Some(request) = successor_rx.recv().await {
            let result = Box::pin(
                session_manager_for_successor.reconcile_agent_successor(request.reservation_id),
            )
            .await;
            session_manager_for_successor
                .spawn_coordinator()
                .complete_successor_dispatch(request.reservation_id)
                .await;
            if let Err(error) = result {
                error!(
                    reservation_id = %request.reservation_id,
                    error = %error,
                    "master successor reconciliation deferred"
                );
            }
        }
    });
    let mut successor_events = event_bus.subscribe();
    let session_manager_for_terminal_successor = Arc::clone(&session_manager);
    tokio::spawn(async move {
        while let Ok(event) = successor_events.recv().await {
            let rsid::bus::DaemonEvent::SessionStatusChanged {
                session_id,
                new_status,
                ..
            } = event.as_ref()
            else {
                continue;
            };
            if !matches!(
                new_status,
                rsi_common::types::SessionStatus::Completed
                    | rsi_common::types::SessionStatus::Failed
                    | rsi_common::types::SessionStatus::Interrupted
                    | rsi_common::types::SessionStatus::Archived
                    | rsi_common::types::SessionStatus::Deleted
            ) {
                continue;
            }
            let _ = session_manager_for_terminal_successor
                .reconcile_agent_successors_for_predecessor(*session_id)
                .await;
        }
    });
    if let Err(error) = session_manager.reconcile_incomplete_agent_spawns().await {
        warn!(error = %error, "Incomplete durable agent spawn reconciliation deferred");
    }
    if let Err(error) = session_manager
        .reconcile_incomplete_agent_successors_at_startup()
        .await
    {
        warn!(error = %error, "Incomplete master successor reconciliation deferred");
    }
    let session_manager_for_successor_backstop = Arc::clone(&session_manager);
    tokio::spawn(rsid::session::retention::run_retention_loop(Arc::clone(
        &session_manager,
    )));
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await;
        loop {
            interval.tick().await;
            if let Err(error) = session_manager_for_successor_backstop
                .reconcile_incomplete_agent_successors()
                .await
            {
                warn!(error = %error, "Master successor bounded backstop deferred");
            }
        }
    });

    // Manager operating intent is daemon-owned durable work. Event hints give
    // prompt feedback; a bounded keyset backstop covers missed events/restart.
    let manager_progress = Arc::new(std::sync::Mutex::new(ManagerCoordinatorProgress {
        last_completed: tokio::time::Instant::now(),
        pass_started: None,
    }));
    let action_runtime = Arc::clone(&session_manager);
    tokio::spawn(run_manager_action_backstop(move || {
        let runtime = Arc::clone(&action_runtime);
        async move { runtime.reconcile_manager_actions_once().await }
    }));
    let progress_for_watch = Arc::clone(&manager_progress);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(MANAGER_ACTION_BACKSTOP_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await;
        let mut stalled = false;
        let mut last_stall_log: Option<tokio::time::Instant> = None;
        loop {
            interval.tick().await;
            let progress = *progress_for_watch.lock().unwrap();
            let age = progress.last_completed.elapsed();
            if age >= MANAGER_COORDINATOR_STALL_AFTER {
                if last_stall_log
                    .is_none_or(|last| last.elapsed() >= MANAGER_COORDINATOR_STALL_AFTER)
                {
                    warn!(
                        last_completed_age_secs = age.as_secs(),
                        active_pass_age_secs =
                            progress.pass_started.map(|start| start.elapsed().as_secs()),
                        "Manager coordinator pass stalled"
                    );
                    last_stall_log = Some(tokio::time::Instant::now());
                }
                stalled = true;
            } else if stalled {
                info!(
                    last_completed_age_secs = age.as_secs(),
                    "Manager coordinator pass recovered"
                );
                stalled = false;
                last_stall_log = None;
            }
        }
    });
    let manager_runtime = Arc::clone(&session_manager);
    let manager_bus = event_bus.clone();
    tokio::spawn(supervise_manager_coordinator(move || {
        let manager_runtime = Arc::clone(&manager_runtime);
        let mut manager_events = manager_bus.subscribe();
        let progress = Arc::clone(&manager_progress);
        tokio::spawn(async move {
            progress.lock().unwrap().pass_started = Some(tokio::time::Instant::now());
            if let Err(error) = manager_runtime.reconcile_harness_managers_startup().await {
                warn!(error=%error,"Manager startup reconciliation deferred");
            }
            *progress.lock().unwrap() = ManagerCoordinatorProgress {
                last_completed: tokio::time::Instant::now(),
                pass_started: None,
            };
            let mut interval = tokio::time::interval(Duration::from_secs(10));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut last_scan = tokio::time::Instant::now();
            loop {
                let due = tokio::select! {
                    _=interval.tick()=>true,
                    event=manager_events.recv()=>match event {
                        Ok(event)=>matches!(event.as_ref(),
                            rsid::bus::DaemonEvent::SessionStatusChanged{..}
                            |rsid::bus::DaemonEvent::SessionQuestionRaised{..}
                            |rsid::bus::DaemonEvent::SessionCreated{..}
                            |rsid::bus::DaemonEvent::SessionMetadataChanged{..}
                            |rsid::bus::DaemonEvent::ProviderRateLimitUpdated{..})
                            ||matches!(event.as_ref(),rsid::bus::DaemonEvent::SystemMessage{message,..} if message.starts_with("Harness manager decision")),
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_))=>true,
                        Err(tokio::sync::broadcast::error::RecvError::Closed)=>break,
                    }
                };
                if !due || last_scan.elapsed() < Duration::from_secs(1) {
                    continue;
                }
                last_scan = tokio::time::Instant::now();
                progress.lock().unwrap().pass_started = Some(tokio::time::Instant::now());
                let result = manager_runtime.reconcile_harness_managers_once().await;
                let pass_age = progress.lock().unwrap().pass_started.unwrap().elapsed();
                *progress.lock().unwrap() = ManagerCoordinatorProgress {
                    last_completed: tokio::time::Instant::now(),
                    pass_started: None,
                };
                if pass_age >= MANAGER_COORDINATOR_STALL_AFTER {
                    warn!(
                        pass_age_secs = pass_age.as_secs(),
                        "Manager coordinator slow pass completed"
                    );
                }
                if let Err(error) = result {
                    warn!(error=%error,"Manager bounded reconciliation deferred");
                }
            }
        })
    }));

    // #959: deliver due context-succession requests to idle managers and
    // Epic leads (never mid-turn; a busy seat is retried on a later pass).
    {
        let succession_runtime = Arc::clone(&session_manager);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(15));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                if let Err(error) = succession_runtime.deliver_due_context_successions().await {
                    warn!(error = %error, "Context succession pass deferred");
                }
            }
        });
    }

    // Spawn stall-retry handler if RSI_RETRY_ON_STALL=true
    if config.retry_on_stall {
        let session_manager_for_stall = Arc::clone(&session_manager);
        let mut stall_rx = event_bus.subscribe();
        tokio::spawn(async move {
            loop {
                match stall_rx.recv().await {
                    Ok(event) => {
                        if let rsid::bus::DaemonEvent::SessionStalled { session_id, .. } =
                            event.as_ref()
                        {
                            let session_id = *session_id;
                            // interrupt_if_stall_retryable handles the flag + max_retries check
                            if session_manager_for_stall
                                .interrupt_if_stall_retryable(session_id)
                                .await
                            {
                                info!(session_id = %session_id, "Stall-triggered retry interrupt sent");
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!(lagged = n, "Stall-retry handler lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        info!("Stall-triggered retry handler started (RSI_RETRY_ON_STALL=true)");
    }

    let authority_initialized_at = std::time::Instant::now();
    info!(
        startup_milestone = "authority_initialized",
        "Daemon authority initialization complete"
    );

    // Issue #25/#69: startup maintenance is accepted only after every
    // authority-bearing restore and reconciliation above. Its daemon-owned
    // receiver monitor keeps the lifecycle cancellation signal truthful while
    // request readiness advances independently of the pass result.
    let sandbox_cache_maintenance = session_manager.start_sandbox_build_cache_maintenance();

    // Journaled Prepared intents hold the custody gate until their rename
    // seam is resolved. Retry them independently of the ordinary policy
    // interval, using the reclaim coordinator's single-flight worker.
    {
        let manager = Arc::clone(&session_manager);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(1));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            ticker.tick().await;
            loop {
                ticker.tick().await;
                if let Err(error) = manager.recover_prepared_target_reclaims().await {
                    warn!(%error, "Prepared target reclaim recovery will retry");
                }
            }
        });
    }

    info!(
        startup_milestone = "request_ready",
        post_authority_elapsed_ms = authority_initialized_at
            .elapsed()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64,
        "Daemon initialized, ready to accept connections"
    );
    rsid::process_memory::on_request_ready();
    // Sandbox diagnostics classify every retained sandbox; they run after
    // readiness so they never delay restarts (#961).
    tokio::spawn({
        let session_manager = Arc::clone(&session_manager);
        async move { session_manager.run_startup_sandbox_diagnostics().await }
    });

    let mut watchdog = Some(rsid::watchdog::start_watchdog(
        config.socket_path.clone(),
        Arc::clone(session_manager.store()),
        scheduler_heartbeat,
        reconciliation_heartbeat,
        rsid::watchdog::WatchdogPolicy::from_intervals(
            config.scheduler_poll_interval_secs,
            config.reconciliation_liveness_interval_secs,
        ),
        data_dir,
    )?);

    // Main loop
    let ctrl_c = signal::ctrl_c();
    tokio::pin!(ctrl_c);

    // SIGTERM is what process supervisors actually send (`systemctl stop`,
    // bare `kill`, `docker stop`). Without an explicit handler its default
    // disposition is immediate process termination, which bypasses the
    // graceful shutdown below entirely — orphaning live provider subprocesses
    // instead of interrupting them cleanly, and dropping in-flight persistence
    // writes. Handling it here is what makes that shutdown path reachable in
    // the standard "stop the daemon" case. `Signal::recv` is cancel-safe, so
    // losing this branch to another `select!` arm cannot drop a signal.
    let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate())?;

    let mut drain_deadline: Option<tokio::time::Instant> = None;
    let mut operator_drain_started: Option<tokio::time::Instant> = None;
    let mut hard_shutdown_requested = false;
    let mut shutdown_deadline = tokio::time::Instant::now();
    let mut connections = tokio::task::JoinSet::new();
    let mut operator_message_tick = tokio::time::interval(Duration::from_secs(1));
    operator_message_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut drain_request_tick = tokio::time::interval(Duration::from_millis(250));
    drain_request_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        if drain_deadline.is_some() && connections.is_empty() {
            info!("Connection drain complete");
            break;
        }
        let deadline_snapshot = drain_deadline;
        tokio::select! {
            _ = drain_request_tick.tick(), if drain_deadline.is_none() => {
                if session_manager.drain_restart_requested() {
                    let started = *operator_drain_started.get_or_insert_with(|| {
                        info!("Operator DRAIN restart requested; status reads remain available during the wait");
                        tokio::time::Instant::now()
                    });
                    shutdown_deadline = started + OPERATOR_DRAIN_LIMIT + SHUTDOWN_LIMIT;
                    let snapshot = session_manager.drain_restart_status().await;
                    let active_count = snapshot["active_sessions"].as_array().map_or(0, Vec::len);
                    if active_count == 0 || tokio::time::Instant::now() >= started + OPERATOR_DRAIN_LIMIT {
                        info!(active_count, "Operator DRAIN wait complete");
                        if let Some(watchdog) = watchdog.take() { watchdog.stop(); }
                        if let Some(handle) = &scheduler_handle {
                            let _ = shutdown_stage("scheduler", shutdown_deadline, handle.shutdown()).await;
                        }
                        drain_deadline = Some(tokio::time::Instant::now() + CONNECTION_DRAIN_LIMIT);
                    }
                }
            }
            _ = operator_message_tick.tick(), if drain_deadline.is_none() => {
                if let Err(error) = session_manager.dispatch_operator_messages_once().await {
                    warn!(%error, "Operator message dispatch deferred");
                }
            }
            _ = &mut ctrl_c, if drain_deadline.is_none() => {
                info!("Received shutdown signal (Ctrl+C)");
                hard_shutdown_requested = true;
                shutdown_deadline = tokio::time::Instant::now() + SHUTDOWN_LIMIT;
                session_manager.begin_restart_drain();
                if let Some(watchdog) = watchdog.take() { watchdog.stop(); }
                if let Some(handle) = &scheduler_handle {
                    let _ = shutdown_stage("scheduler", shutdown_deadline, handle.shutdown()).await;
                }
                drain_deadline = Some(tokio::time::Instant::now() + CONNECTION_DRAIN_LIMIT);
                info!(connections = connections.len(), "Connection drain started");
            }
            _ = sigterm.recv(), if drain_deadline.is_none() => {
                info!("Received shutdown signal (SIGTERM)");
                hard_shutdown_requested = true;
                shutdown_deadline = tokio::time::Instant::now() + SHUTDOWN_LIMIT;
                session_manager.begin_restart_drain();
                if let Some(watchdog) = watchdog.take() { watchdog.stop(); }
                if let Some(handle) = &scheduler_handle {
                    let _ = shutdown_stage("scheduler", shutdown_deadline, handle.shutdown()).await;
                }
                drain_deadline = Some(tokio::time::Instant::now() + CONNECTION_DRAIN_LIMIT);
                info!(connections = connections.len(), "Connection drain started");
            }
            () = async {
                if let Some(deadline) = deadline_snapshot {
                    tokio::time::sleep_until(deadline).await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => {
                warn!(connections = connections.len(), "Connection drain deadline reached");
                break;
            }
            result = connections.join_next(), if !connections.is_empty() => {
                if let Some(Err(error)) = result {
                    warn!(%error, "Connection handler task failed during drain");
                }
            }
            result = listener.accept(), if drain_deadline.is_none() => {
                match result {
                    Ok((stream, _)) => {
                        let server = Arc::clone(&rpc_server);
                        connections.spawn(async move {
                            if let Err(e) = server.handle_connection(stream).await {
                                let msg = e.to_string();
                                if msg.contains("Broken pipe") || msg.contains("Connection reset") {
                                    warn!(error = %e, "Client disconnected");
                                } else {
                                    error!(error = %e, "Connection handler error");
                                }
                            }
                        });
                    }
                    Err(e) => {
                        error!(error = %e, "Failed to accept connection");
                    }
                }
            }
        }
    }
    // Timed-out handlers retain their ordinary task lifetime while the other
    // subsystems settle. Dropping a JoinSet would abort them immediately.
    connections.detach_all();

    // Stop pressure continuations before shutdown awaits other services.
    // Already accepted worker jobs retain their cancellation-safe settlement.
    sandbox_cache_maintenance.abort();

    // Graceful shutdown
    if let Some(watchdog) = watchdog.take() {
        watchdog.stop();
    }
    info!("Shutting down subsystems...");

    if let Some((cancellation, handle)) = program_run_dispatcher {
        info!(
            subsystem = "program_run_dispatcher",
            "Shutdown stage started"
        );
        cancellation.cancel();
        if tokio::time::timeout_at(shutdown_deadline - Duration::from_secs(5), handle)
            .await
            .is_err()
        {
            warn!(
                subsystem = "program_run_dispatcher",
                "Shutdown stage exceeded worker deadline"
            );
        } else {
            info!(
                subsystem = "program_run_dispatcher",
                "Shutdown stage complete"
            );
        }
    }

    if let Some((cancellation, handle)) = app_server_seal_worker {
        info!(
            subsystem = "app_server_seal_worker",
            "Shutdown stage started"
        );
        cancellation.cancel();
        if tokio::time::timeout_at(shutdown_deadline - Duration::from_secs(5), handle)
            .await
            .is_err()
        {
            warn!(
                subsystem = "app_server_seal_worker",
                "Shutdown stage exceeded worker deadline"
            );
        } else {
            info!(
                subsystem = "app_server_seal_worker",
                "Shutdown stage complete"
            );
        }
    }

    // Shut down background queue before session manager
    if let Some(qh) = session_manager.queue_handle() {
        let _ = shutdown_stage("background_queue", shutdown_deadline, qh.shutdown()).await;
    }

    // Shut down memory system before session manager
    if let Some(ref mm) = memory_manager {
        shutdown_stage("memory", shutdown_deadline, mm.shutdown()).await;
    }

    if session_manager.drain_restart_requested() && !hard_shutdown_requested {
        let remaining = operator_drain_started.map_or(Duration::ZERO, |started| {
            (started + OPERATOR_DRAIN_LIMIT).saturating_duration_since(tokio::time::Instant::now())
        });
        shutdown_stage(
            "session_manager",
            shutdown_deadline,
            session_manager.shutdown_drain(remaining),
        )
        .await?;
    } else {
        shutdown_stage(
            "session_manager",
            shutdown_deadline,
            session_manager.shutdown(),
        )
        .await?;
    }

    // Clean up socket
    if let Err(e) = tokio::fs::remove_file(&config.socket_path).await {
        warn!(error = %e, "Failed to remove socket file");
    }

    drop(instance_guard);
    info!("Shutdown complete");
    Ok(())
}

/// Install a panic hook that routes panic payload + location through `tracing::error!`
/// before chaining to the previously-installed (default) hook, so stderr output and
/// backtrace behavior are preserved. Without this, panics bypass `tracing` entirely
/// and write an unstructured line to raw stderr, invisible to whatever collects
/// daemon logs.
fn install_panic_hook() {
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let location = panic_info.location().map_or_else(
            || "unknown location".to_string(),
            |loc| format!("{}:{}:{}", loc.file(), loc.line(), loc.column()),
        );
        let message = panic_info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| panic_info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "non-string panic payload".to_string());
        tracing::error!(location = %location, message = %message, "panic in daemon");
        previous_hook(panic_info);
    }));
}

fn recursive_startup_recovery_budget(config: &Config) -> RecursiveRecoveryBudget {
    RecursiveRecoveryBudget {
        max_graphs: config.recursive_dag_startup_recovery_max_graphs,
        time_budget_ms: Some(config.recursive_dag_startup_recovery_time_budget_ms),
        source: RecursiveRecoverySource::Startup,
    }
}

async fn process_spawn_request(
    session_manager: Arc<rsid::session::SessionManager>,
    event_bus: Arc<rsid::bus::EventBus>,
    req: rsid::session::spawn_coordinator::SpawnRequest,
) {
    let parent_epic_id = req.epic_id;
    let kind = req.kind;
    let spawn_request_id = req.spawn_request_id;
    match session_manager.launch_agent_child(req).await {
        Ok(child_id) => {
            info!(
                spawn_request_id = %spawn_request_id,
                parent_epic_id = %parent_epic_id,
                child_id = %child_id,
                ?kind,
                "spawn_child: child session launched"
            );
            event_bus.publish(rsid::bus::DaemonEvent::ChildSpawned {
                parent_epic_id,
                child_id,
                kind,
            });
        }
        Err(e) => {
            error!(
                spawn_request_id = %spawn_request_id,
                error = %e,
                parent_epic_id = %parent_epic_id,
                ?kind,
                "spawn_child: launch_session failed"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn manager_action_backstop_reaches_one_due_claim_while_coordinator_is_parked() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let coordinator = tokio::spawn(std::future::pending::<()>());
        let claims = Arc::new(AtomicUsize::new(0));
        let attempts = Arc::new(AtomicUsize::new(0));
        let backstop = tokio::spawn(run_manager_action_backstop({
            let claims = Arc::clone(&claims);
            let attempts = Arc::clone(&attempts);
            move || {
                let claims = Arc::clone(&claims);
                let attempts = Arc::clone(&attempts);
                async move {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    // Model the durable claim transition: later passes see no
                    // queued work, even while the coordinator stays parked.
                    if claims
                        .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
                        .is_ok()
                    {
                        Ok(1)
                    } else {
                        Ok(0)
                    }
                }
            }
        }));
        tokio::task::yield_now().await;
        for expected in 1..=3 {
            tokio::time::advance(MANAGER_ACTION_BACKSTOP_INTERVAL).await;
            tokio::task::yield_now().await;
            assert_eq!(attempts.load(Ordering::SeqCst), expected);
        }
        assert!(!coordinator.is_finished());
        assert_eq!(claims.load(Ordering::SeqCst), 1);
        coordinator.abort();
        backstop.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn manager_coordinator_supervisor_restarts_after_panic() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let launches = Arc::new(AtomicUsize::new(0));
        let supervisor = tokio::spawn(supervise_manager_coordinator({
            let launches = Arc::clone(&launches);
            move || {
                let launch = launches.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    if launch == 0 {
                        panic!("simulated coordinator failure");
                    }
                    std::future::pending::<()>().await;
                })
            }
        }));
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(launches.load(Ordering::SeqCst), 2);
        assert!(!supervisor.is_finished());
        supervisor.abort();
    }

    struct IsolatedDaemonChild(Option<std::process::Child>);

    impl IsolatedDaemonChild {
        fn stop(&mut self) {
            if let Some(mut child) = self.0.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    impl Drop for IsolatedDaemonChild {
        fn drop(&mut self) {
            self.stop();
        }
    }

    fn wait_for_log(path: &std::path::Path, needle: &str, timeout: std::time::Duration) -> String {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let contents = std::fs::read_to_string(path).unwrap_or_default();
            if contents.contains(needle) {
                return contents;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for {needle:?}; log:\n{contents}"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    fn health_rpc(socket_path: &std::path::Path) -> std::io::Result<String> {
        use std::io::{BufRead, Write};

        let mut stream = std::os::unix::net::UnixStream::connect(socket_path)?;
        stream.set_read_timeout(Some(std::time::Duration::from_secs(1)))?;
        stream.write_all(
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"GetHealthStatus\",\"params\":null}\n",
        )?;
        stream.flush()?;
        let mut response = String::new();
        std::io::BufReader::new(stream).read_line(&mut response)?;
        Ok(response)
    }

    fn fixture_rpc(
        socket_path: &std::path::Path,
        method: &str,
        params: serde_json::Value,
    ) -> serde_json::Value {
        use std::io::{BufRead, Write};

        let mut stream =
            std::os::unix::net::UnixStream::connect(socket_path).expect("connect isolated daemon");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("RPC timeout");
        serde_json::to_writer(
            &mut stream,
            &serde_json::json!({
                "jsonrpc": "2.0", "id": 1, "method": method, "params": params
            }),
        )
        .expect("encode RPC");
        stream.write_all(b"\n").expect("send RPC");
        let mut response = String::new();
        std::io::BufReader::new(stream)
            .read_line(&mut response)
            .expect("read RPC");
        serde_json::from_str(&response).expect("parse RPC")
    }

    fn parse_u64_log_field(line: &str, field: &str) -> Option<u64> {
        let prefix = format!("{field}=");
        line.split_whitespace()
            .find_map(|part| part.strip_prefix(&prefix))
            .and_then(|value| {
                value
                    .trim_matches(|character: char| !character.is_ascii_digit())
                    .parse()
                    .ok()
            })
    }

    #[test]
    #[ignore = "fixture entry point for the isolated real-daemon startup test"]
    fn sr2_real_daemon_fixture_process() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("fixture runtime");
        if std::env::var_os("RSI_TEST_STUCK_BLOCKING_TASK").is_some() {
            // Stands in for a blocking read of a provider child's pipe.
            runtime.spawn_blocking(|| std::thread::sleep(Duration::from_secs(600)));
        }
        run_to_bounded_exit(runtime, run_daemon()).expect("isolated daemon fixture");
    }

    fn assert_isolated_sigint_exit(
        hold_client_open: bool,
        live_provider: bool,
        stuck_blocking_task: bool,
        bound: Duration,
    ) {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        use std::process::{Command, Stdio};

        let fixture = tempfile::Builder::new()
            .prefix("rsid-sigint-")
            .tempdir_in("/tmp")
            .expect("fixture root");
        let home = fixture.path().join("home");
        let data_dir = home.join(".rsi");
        let bin_dir = fixture.path().join("bin");
        let sandbox_base = fixture.path().join("sandboxes");
        for dir in [&data_dir, &bin_dir, &sandbox_base] {
            std::fs::create_dir_all(dir).expect("fixture directory");
        }
        let provider_stub = bin_dir.join("codex");
        std::fs::write(&provider_stub, b"#!/bin/sh\nexit 0\n").expect("provider stub");
        std::fs::set_permissions(&provider_stub, std::fs::Permissions::from_mode(0o755))
            .expect("provider stub permissions");
        let provider_marker = fixture.path().join("provider-running");
        let (provider_url, provider_release, provider_thread) = if live_provider {
            let listener =
                std::net::TcpListener::bind("127.0.0.1:0").expect("bind isolated provider");
            let url = format!(
                "http://{}/v1",
                listener.local_addr().expect("provider address")
            );
            let marker = provider_marker.clone();
            let (release, released) = std::sync::mpsc::channel::<()>();
            let thread = std::thread::spawn(move || {
                let (stream, _) = listener.accept().expect("accept provider request");
                std::fs::write(marker, b"accepted\n").expect("provider dispatch marker");
                let _ = released.recv_timeout(Duration::from_secs(15));
                drop(stream);
            });
            (Some(url), Some(release), Some(thread))
        } else {
            (None, None, None)
        };
        let socket = data_dir.join("daemon.sock");
        let log_path = fixture.path().join("rsid.log");
        let log = std::fs::File::create(&log_path).expect("daemon log");
        let inherited_path = std::env::var("PATH").unwrap_or_default();
        let child = Command::new(std::env::current_exe().expect("current rsid test binary"))
            .args([
                "--ignored",
                "--exact",
                "tests::sr2_real_daemon_fixture_process",
                "--nocapture",
            ])
            .env("HOME", &home)
            .env("RSI_DAEMON_SOCKET_PATH", &socket)
            .env("RSI_SANDBOX_BASE", &sandbox_base)
            // Plain-text daemon log regardless of the ambient color setting.
            .env("NO_COLOR", "1")
            .env("RSI_MEMORY_ENABLED", "false")
            .env("RSI_DREAM_ENABLED", "false")
            .env("RSI_QUEUE_ENABLED", "false")
            .env("RSI_RECONCILIATION_ENABLED", "false")
            .env("RSI_STALL_DETECTION_ENABLED", "false")
            .env("RSI_SCHEDULER_ENABLED", "false")
            .env("RSI_SMOKE_SUPPRESS_RETRY_RESTORE", "true")
            .env("PATH", format!("{}:{inherited_path}", bin_dir.display()))
            .envs(
                stuck_blocking_task
                    .then_some(("RSI_TEST_STUCK_BLOCKING_TASK", "1"))
                    .into_iter(),
            )
            .env_remove("RSI_PROCESS_OWNERSHIP_NAMESPACE")
            .env_remove("RSI_SESSION_ID")
            .env_remove("RSI_SESSION_TOKEN")
            .env_remove("RSI_SOCKET")
            .stdout(Stdio::from(log.try_clone().expect("clone daemon stdout")))
            .stderr(Stdio::from(log))
            .spawn()
            .expect("spawn isolated daemon fixture");
        let mut child = IsolatedDaemonChild(Some(child));
        wait_for_log(
            &log_path,
            "Daemon initialized, ready to accept connections",
            Duration::from_secs(15),
        );
        // Readiness only: the SIGINT exit bound below is the assertion. A
        // single 1 s health read timed out (WouldBlock) under shard load, so
        // retry until the daemon answers, as the startup fixture does.
        let health_deadline = std::time::Instant::now() + Duration::from_secs(15);
        let response = loop {
            match health_rpc(&socket) {
                Ok(response) if response.contains("\"result\"") => break response,
                _ if std::time::Instant::now() < health_deadline => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                other => panic!("isolated daemon health RPC did not answer: {other:?}"),
            }
        };
        assert!(response.contains("\"jsonrpc\":\"2.0\""));
        let session_id = if live_provider {
            let response = fixture_rpc(
                &socket,
                "LaunchSession",
                serde_json::json!({
                    "query": "wait for restart interrupt",
                    "working_dir": sandbox_base,
                "provider": "Local",
                "model": "qwen3:14b",
                "openai_base_url": provider_url.expect("isolated provider URL"),
                    "tags": ["restart-test"],
                    "skip_context_pipeline": true
                }),
            );
            let session_id = response["result"]["session_id"]
                .as_str()
                .unwrap_or_else(|| {
                    panic!(
                        "launch failed: {response}; log:\n{}",
                        std::fs::read_to_string(&log_path).unwrap_or_default()
                    )
                })
                .to_owned();
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while !provider_marker.exists() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "provider did not start; log:\n{}",
                    std::fs::read_to_string(&log_path).unwrap_or_default()
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            Some(session_id)
        } else {
            None
        };
        let held_client = if hold_client_open {
            let mut stream =
                std::os::unix::net::UnixStream::connect(&socket).expect("connect held client");
            stream.write_all(b"{").expect("start held request");
            std::thread::sleep(Duration::from_millis(50));
            Some(stream)
        } else {
            None
        };

        let started = std::time::Instant::now();
        let pid = nix::unistd::Pid::from_raw(child.0.as_ref().expect("child").id() as i32);
        nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGINT).expect("send fixture SIGINT");
        let status = loop {
            if let Some(status) = child
                .0
                .as_mut()
                .expect("child")
                .try_wait()
                .expect("poll child")
            {
                break status;
            }
            assert!(
                started.elapsed() < bound,
                "daemon did not exit within {bound:?}; log:\n{}",
                std::fs::read_to_string(&log_path).unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(started.elapsed() < bound, "daemon exit exceeded {bound:?}");
        assert!(
            status.success(),
            "daemon exit: {status}; log:\n{}",
            std::fs::read_to_string(&log_path).unwrap_or_default()
        );
        let log = std::fs::read_to_string(&log_path).expect("daemon log");
        assert!(log.contains("Shutdown complete"), "{log}");
        if hold_client_open {
            assert!(log.contains("Connection drain deadline reached"), "{log}");
        }
        if let Some(session_id) = session_id {
            let db = rusqlite::Connection::open(data_dir.join(rsi_common::identity::DB_FILENAME))
                .expect("open isolated database");
            let (state, outcome): (String, Option<String>) = db
                .query_row(
                    "SELECT state,outcome FROM daemon_restart_intents WHERE session_id=?1",
                    [&session_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .expect("durable restart intent");
            assert_eq!(
                state, "pending",
                "restart intent must remain available for startup recovery; log:\n{log}"
            );
            assert_eq!(
                outcome.as_deref(),
                Some("shutdown_cancelled"),
                "intent state: {state}; log:\n{log}"
            );
            let session_status: String = db
                .query_row(
                    "SELECT status FROM sessions WHERE id=?1",
                    [&session_id],
                    |row| row.get(0),
                )
                .expect("persisted session status");
            assert_eq!(session_status, "Interrupted", "log:\n{log}");
        }
        if let Some(release) = provider_release {
            release.send(()).expect("release isolated provider");
        }
        if let Some(thread) = provider_thread {
            thread.join().expect("isolated provider thread");
        }
        drop(held_client);
        child.0.take();
    }

    #[test]
    fn idle_daemon_exits_cleanly_within_three_seconds_of_sigint() {
        assert_isolated_sigint_exit(false, false, false, Duration::from_secs(3));
    }

    #[test]
    fn open_client_does_not_prevent_bounded_sigint_shutdown() {
        assert_isolated_sigint_exit(true, false, false, Duration::from_secs(10));
    }

    #[test]
    fn live_provider_settles_restart_intent_within_ten_seconds_of_sigint() {
        assert_isolated_sigint_exit(false, true, false, Duration::from_secs(10));
    }

    #[test]
    fn stuck_blocking_task_does_not_hold_process_after_shutdown_complete() {
        // Exit must follow shutdown within RUNTIME_DROP_GRACE plus slack, even
        // though a blocking task would otherwise run for ten minutes (#1057).
        assert_isolated_sigint_exit(false, false, true, Duration::from_secs(6));
    }

    #[test]
    fn startup_real_daemon_accepts_health_while_reclaim_is_held_for_two_seconds() {
        use std::os::unix::fs::PermissionsExt;
        use std::process::{Command, Stdio};

        let fixture = tempfile::Builder::new()
            .prefix("rsid-sr2-startup-")
            .tempdir_in("/tmp")
            .expect("fixture root");
        let home = fixture.path().join("home");
        let data_dir = home.join(".rsi");
        let sandbox_base = fixture.path().join("sandboxes");
        let bin_dir = fixture.path().join("bin");
        std::fs::create_dir_all(&data_dir).expect("fixture data dir");
        std::fs::create_dir_all(&sandbox_base).expect("fixture sandbox base");
        std::fs::create_dir_all(&bin_dir).expect("fixture bin dir");
        let provider_stub = bin_dir.join("codex");
        std::fs::write(&provider_stub, b"#!/bin/sh\nexit 0\n").expect("provider stub");
        std::fs::set_permissions(&provider_stub, std::fs::Permissions::from_mode(0o755))
            .expect("provider stub permissions");

        let socket = data_dir.join("daemon.sock");
        let log_path = fixture.path().join("rsid.log");
        let reclaim_entered = sandbox_base.join("startup-reclaim-entered");
        let reclaim_release = sandbox_base.join("startup-reclaim-release");
        let log = std::fs::File::create(&log_path).expect("daemon log");
        let inherited_path = std::env::var("PATH").unwrap_or_default();
        let child = Command::new(std::env::current_exe().expect("current rsid test binary"))
            .args([
                "--ignored",
                "--exact",
                "tests::sr2_real_daemon_fixture_process",
                "--nocapture",
            ])
            .env("HOME", &home)
            .env("RSI_DAEMON_SOCKET_PATH", &socket)
            .env("RSI_SANDBOX_BASE", &sandbox_base)
            .env("RSI_SR2_TEST_RECLAIM_ENTERED", &reclaim_entered)
            .env("RSI_SR2_TEST_RECLAIM_RELEASE", &reclaim_release)
            // The assertions match plain `field="value"` log text; without
            // this the default tracing formatter styles fields with ANSI
            // escapes unless the ambient environment happens to disable color.
            .env("NO_COLOR", "1")
            .env("RSI_MEMORY_ENABLED", "false")
            .env("RSI_DREAM_ENABLED", "false")
            .env("RSI_QUEUE_ENABLED", "false")
            .env("RSI_RECONCILIATION_ENABLED", "false")
            .env("RSI_STALL_DETECTION_ENABLED", "false")
            .env("RSI_SCHEDULER_ENABLED", "false")
            .env("RSI_SMOKE_SUPPRESS_RETRY_RESTORE", "true")
            .env("PATH", format!("{}:{inherited_path}", bin_dir.display()))
            .env_remove("RSI_PROCESS_OWNERSHIP_NAMESPACE")
            .env_remove("RSI_SESSION_ID")
            .env_remove("RSI_SESSION_TOKEN")
            .env_remove("RSI_SOCKET")
            .stdout(Stdio::from(log.try_clone().expect("clone daemon stdout")))
            .stderr(Stdio::from(log))
            .spawn()
            .expect("spawn isolated daemon fixture");
        let mut child = IsolatedDaemonChild(Some(child));

        let entered_deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !reclaim_entered.exists() && std::time::Instant::now() < entered_deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        if !reclaim_entered.exists() {
            panic!(
                "startup reclaim did not enter isolated hold; log:\n{}",
                std::fs::read_to_string(&log_path).unwrap_or_default()
            );
        }
        let held_at = std::time::Instant::now();
        let ready_log = wait_for_log(
            &log_path,
            "startup_milestone=\"request_ready\"",
            std::time::Duration::from_secs(5),
        );
        let request_ready_line = ready_log
            .lines()
            .find(|line| line.contains("startup_milestone=\"request_ready\""))
            .expect("request-ready line");
        let post_authority_elapsed_ms =
            parse_u64_log_field(request_ready_line, "post_authority_elapsed_ms")
                .expect("post-authority duration field");
        assert!(
            post_authority_elapsed_ms <= 250,
            "request readiness took {post_authority_elapsed_ms} ms after authority initialization"
        );

        let health_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let health_response = loop {
            match health_rpc(&socket) {
                Ok(response) if response.contains("\"result\"") => break response,
                _ if std::time::Instant::now() < health_deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                other => panic!("health RPC did not complete during reclaim hold: {other:?}"),
            }
        };
        assert!(health_response.contains("\"jsonrpc\":\"2.0\""));
        assert!(
            !std::fs::read_to_string(&log_path)
                .unwrap_or_default()
                .lines()
                .any(|line| {
                    line.contains("trigger=\"startup\"")
                        && line.contains("lifecycle_phase=\"completed\"")
                }),
            "startup reclaim must still be held when health succeeds"
        );

        let remaining_hold = std::time::Duration::from_secs(2).saturating_sub(held_at.elapsed());
        std::thread::sleep(remaining_hold);
        let held_reclaim_ms = held_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
        assert!(
            held_reclaim_ms >= 2_000,
            "startup reclaim hold lasted only {held_reclaim_ms} ms"
        );
        std::fs::write(&reclaim_release, b"release\n").expect("release startup reclaim");
        let terminal_log = wait_for_log(
            &log_path,
            "Sandbox build-cache reclaim pass completed",
            std::time::Duration::from_secs(5),
        );

        let accepted = terminal_log
            .lines()
            .position(|line| {
                line.contains("trigger=\"startup\"")
                    && line.contains("lifecycle_phase=\"accepted\"")
            })
            .expect("startup accepted record");
        let ready = terminal_log
            .lines()
            .position(|line| line.contains("startup_milestone=\"request_ready\""))
            .expect("request-ready record");
        let terminal_records: Vec<_> = terminal_log
            .lines()
            .enumerate()
            .filter(|(_, line)| {
                line.contains("trigger=\"startup\"")
                    && (line.contains("lifecycle_phase=\"completed\"")
                        || line.contains("lifecycle_phase=\"error\""))
            })
            .collect();
        assert!(
            accepted < ready,
            "startup enqueue must precede request readiness"
        );
        assert_eq!(terminal_records.len(), 1, "{terminal_log}");
        assert!(ready < terminal_records[0].0, "{terminal_log}");
        assert_eq!(
            terminal_log
                .lines()
                .filter(|line| {
                    line.contains("trigger=\"startup\"")
                        && line.contains("lifecycle_phase=\"started\"")
                })
                .count(),
            1,
            "{terminal_log}"
        );
        assert!(terminal_records[0].1.contains("request_cancelled=false"));
        assert!(terminal_records[0].1.contains("run_duration_ms="));
        assert!(terminal_records[0].1.contains("total_elapsed_ms="));
        println!(
            "SR-2 real-daemon proof: post_authority_elapsed_ms={post_authority_elapsed_ms} \
             held_reclaim_ms={held_reclaim_ms} health_rpc=ok terminal_records={}",
            terminal_records.len()
        );

        child.stop();
    }

    #[test]
    fn recursive_dag_startup_recovery_budget_uses_config() {
        let mut config = Config::default();
        config.recursive_dag_startup_recovery_max_graphs = 7;
        config.recursive_dag_startup_recovery_time_budget_ms = 23;

        let budget = recursive_startup_recovery_budget(&config);

        assert_eq!(budget.max_graphs, 7);
        assert_eq!(budget.time_budget_ms, Some(23));
        assert_eq!(budget.source, RecursiveRecoverySource::Startup);
    }

    #[tokio::test]
    async fn d05_failed_session_restore_skips_program_run_reconcile_and_dispatch() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let restore_calls = AtomicUsize::new(0);
        let reconcile_calls = AtomicUsize::new(0);
        let dispatch_calls = AtomicUsize::new(0);
        let result = program_run_startup_sequence(
            || async {
                restore_calls.fetch_add(1, Ordering::SeqCst);
                false
            },
            || async {
                reconcile_calls.fetch_add(1, Ordering::SeqCst);
                true
            },
            || {
                dispatch_calls.fetch_add(1, Ordering::SeqCst);
                Ok::<_, ()>(())
            },
        )
        .await;

        assert!(result.is_none());
        assert_eq!(restore_calls.load(Ordering::SeqCst), 1);
        assert_eq!(reconcile_calls.load(Ordering::SeqCst), 0);
        assert_eq!(dispatch_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn d05_failed_program_reconcile_remains_nonfatal_and_skips_dispatch() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let reconcile_calls = AtomicUsize::new(0);
        let dispatch_calls = AtomicUsize::new(0);
        let result = program_run_startup_sequence(
            || async { true },
            || async {
                reconcile_calls.fetch_add(1, Ordering::SeqCst);
                false
            },
            || {
                dispatch_calls.fetch_add(1, Ordering::SeqCst);
                Ok::<_, ()>(())
            },
        )
        .await;

        assert!(result.is_none());
        assert_eq!(reconcile_calls.load(Ordering::SeqCst), 1);
        assert_eq!(dispatch_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn provider_inventory_fence_precedes_probes_workers_and_restore() {
        let source = include_str!("main.rs");
        let main_start = source.find("async fn run_daemon").expect("main function");
        let helper_start = source[main_start..]
            .find("fn recursive_startup_recovery_budget")
            .map(|offset| main_start + offset)
            .expect("startup recovery helper follows main");
        let main_body = &source[main_start..helper_start];
        let namespace_bind = main_body
            .find("initialize_process_ownership_namespace")
            .expect("bind configured process ownership namespace");
        let store_open = main_body.find("Store::open").expect("Store open");
        let ownership_fence = main_body
            .find("reap_startup_process_ownership_checked")
            .expect("process-first Store ownership fence");
        let provider_probe = main_body
            .find("ClaudeClient::is_available")
            .expect("provider availability probe");
        let no_provider_return = main_body
            .find("No supported provider found")
            .expect("no-provider return");
        let socket_bind = main_body
            .find("UnixListener::bind")
            .expect("daemon socket bind");
        let first_status_mutation = main_body
            .find("ensure_c5_autofile_activation")
            .expect("first post-fence Store activation");
        let memory_worker = main_body
            .find("init_memory_system")
            .expect("memory worker initialization");
        let queue_worker = main_body
            .find("spawn_queue_worker")
            .expect("queue worker initialization");
        let restore = main_body
            .find("session_manager.restore_sessions().await")
            .expect("session restore call");

        assert!(namespace_bind < store_open);
        assert!(store_open < ownership_fence);
        assert!(ownership_fence < provider_probe);
        assert!(provider_probe < no_provider_return);
        assert!(ownership_fence < socket_bind);
        assert!(ownership_fence < first_status_mutation);
        assert!(ownership_fence < memory_worker);
        assert!(ownership_fence < queue_worker);
        assert!(ownership_fence < restore);
    }

    #[test]
    fn recursive_dag_live_startup_recovery_order_is_after_session_restore_before_graph_recovery() {
        let source = include_str!("main.rs");
        let main_start = source.find("async fn run_daemon").expect("main function");
        let helper_start = source[main_start..]
            .find("fn recursive_startup_recovery_budget")
            .map(|offset| main_start + offset)
            .expect("startup recovery helper follows main");
        let main_body = &source[main_start..helper_start];

        let restore = main_body
            .find("session_manager.restore_sessions().await")
            .expect("session restore call");
        let live_recovery = main_body
            .find("store.recover_recursive_live_attempts_with_budget")
            .expect("live recovery call");
        let live_output_recovery = main_body
            .find("commit_recoverable_recursive_live_outputs_after_restart")
            .expect("live output recovery call");
        let graph_recovery = main_body
            .find("recover_recursive_task_graphs_after_restart_with_budget")
            .expect("graph recovery call");

        assert!(restore < live_recovery);
        assert!(live_recovery < live_output_recovery);
        assert!(live_output_recovery < graph_recovery);
    }

    #[test]
    fn startup_reclaim_is_after_authority_and_before_periodic_request_ready_and_accept() {
        let source = include_str!("main.rs");
        let main_start = source.find("async fn run_daemon").expect("main function");
        let helper_start = source[main_start..]
            .find("fn recursive_startup_recovery_budget")
            .map(|offset| main_start + offset)
            .expect("startup recovery helper follows main");
        let main_body = &source[main_start..helper_start];

        let socket_bind = main_body.find("UnixListener::bind").expect("socket bind");
        let socket_bound = main_body
            .find("startup_milestone = \"socket_bound\"")
            .expect("truthful socket-bound milestone");
        let restore = main_body
            .find("session_manager.restore_sessions().await")
            .expect("session restore");
        let program_runs = main_body
            .find("reconcile_program_runs_at_startup")
            .expect("ProgramRun reconciliation");
        let app_server = main_body
            .find("start_app_server_seal_worker")
            .expect("AppServer writer-admission reconciliation");
        let agent_spawns = main_body
            .find("reconcile_incomplete_agent_spawns")
            .expect("agent spawn reconciliation");
        let successors = main_body
            .find("reconcile_incomplete_agent_successors_at_startup")
            .expect("master successor reconciliation");
        let authority_initialized = main_body
            .find("startup_milestone = \"authority_initialized\"")
            .expect("final authority milestone");
        let maintenance = main_body
            .find("session_manager.start_sandbox_build_cache_maintenance()")
            .expect("startup maintenance submission");
        let request_ready = main_body
            .find("startup_milestone = \"request_ready\"")
            .expect("request-ready milestone");
        let accept = main_body.find("listener.accept()").expect("accept loop");

        assert!(socket_bind < socket_bound);
        assert!(socket_bound < restore);
        assert!(restore < program_runs);
        assert!(program_runs < app_server);
        assert!(app_server < agent_spawns);
        assert!(agent_spawns < successors);
        assert!(successors < authority_initialized);
        assert!(authority_initialized < maintenance);
        assert!(maintenance < request_ready);
        assert!(request_ready < accept);

        let launch = include_str!("session/launch.rs");
        let maintenance_start = launch
            .find("pub fn start_sandbox_build_cache_maintenance")
            .expect("maintenance producer");
        let maintenance_end = launch[maintenance_start..]
            .find("pub async fn recover_prepared_target_reclaims")
            .map(|offset| maintenance_start + offset)
            .expect("next maintenance helper");
        let maintenance_body = &launch[maintenance_start..maintenance_end];
        let startup_submit = maintenance_body
            .find("self.submit_sandbox_build_cache_reclaim(false, false, \"startup\")")
            .expect("synchronous startup submission");
        let producer = maintenance_body
            .find("tokio::spawn(async move")
            .expect("periodic producer");
        let periodic = maintenance_body
            .find("\"periodic\"")
            .expect("periodic submission");
        assert!(startup_submit < producer);
        assert!(producer < periodic);
    }

    #[test]
    fn daemon_instance_lease_precedes_store_open_and_socket_unlink() {
        let source = include_str!("main.rs");
        let main_start = source.find("async fn run_daemon").expect("main function");
        let helper_start = source[main_start..]
            .find("fn recursive_startup_recovery_budget")
            .map(|offset| main_start + offset)
            .expect("startup recovery helper follows main");
        let main_body = &source[main_start..helper_start];

        let lease = main_body
            .find("DaemonInstanceGuard::acquire")
            .expect("instance lease acquisition");
        let socket_probe = main_body
            .find("refuse_live_daemon_socket")
            .expect("legacy socket liveness probe");
        let provider_probe = main_body
            .find("ClaudeClient::is_available")
            .expect("provider availability probe");
        let store_open = main_body.find("Store::open").expect("Store open");
        let socket_unlink = main_body
            .find("remove_file(&config.socket_path)")
            .expect("stale socket unlink");

        assert!(lease < socket_probe);
        let ownership_fence = main_body
            .find("reap_startup_process_ownership_checked")
            .expect("process ownership fence");

        assert!(socket_probe < store_open);
        assert!(store_open < ownership_fence);
        assert!(ownership_fence < provider_probe);
        assert!(store_open < socket_unlink);
    }

    #[test]
    fn panic_hook_routes_through_tracing() {
        use std::sync::{Arc, Mutex};
        use tracing::field::{Field, Visit};
        use tracing::span::{Attributes, Id, Record};
        use tracing::{Event, Metadata, Subscriber};

        #[derive(Debug)]
        struct CapturedEvent {
            level: tracing::Level,
            fields: String,
        }

        #[derive(Default)]
        struct FieldVisitor(String);
        impl Visit for FieldVisitor {
            fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
                self.0.push_str(&format!("{}={:?} ", field.name(), value));
            }
        }

        // Minimal in-process `tracing::Subscriber` that just records events; avoids
        // pulling in a new dev-dependency for a single assertion.
        struct CaptureSubscriber {
            events: Arc<Mutex<Vec<CapturedEvent>>>,
        }
        impl Subscriber for CaptureSubscriber {
            fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
                true
            }
            fn new_span(&self, _span: &Attributes<'_>) -> Id {
                Id::from_u64(1)
            }
            fn record(&self, _span: &Id, _values: &Record<'_>) {}
            fn record_follows_from(&self, _span: &Id, _follows: &Id) {}
            fn event(&self, event: &Event<'_>) {
                let mut visitor = FieldVisitor::default();
                event.record(&mut visitor);
                self.events.lock().unwrap().push(CapturedEvent {
                    level: *event.metadata().level(),
                    fields: visitor.0,
                });
            }
            fn enter(&self, _span: &Id) {}
            fn exit(&self, _span: &Id) {}
        }

        let events: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let subscriber = CaptureSubscriber {
            events: events.clone(),
        };

        tracing::subscriber::with_default(subscriber, || {
            install_panic_hook();
            let result = std::panic::catch_unwind(|| {
                panic!("regression-test panic payload");
            });
            assert!(result.is_err(), "catch_unwind should observe the panic");
        });

        let captured = events.lock().unwrap();
        let found = captured.iter().any(|e| {
            e.level == tracing::Level::ERROR
                && e.fields.contains("main.rs")
                && e.fields.contains("regression-test panic payload")
        });
        assert!(
            found,
            "expected an ERROR event with panic location + message, got: {:?}",
            captured
                .iter()
                .map(|e| format!("{}: {}", e.level, e.fields))
                .collect::<Vec<_>>()
        );
    }
}

/// Initialize the memory subsystem: open store, create embedding provider, spawn worker.
async fn init_memory_system(
    memory_config: &rsid::memory::types::MemoryConfig,
    main_db_path: &std::path::Path,
    event_bus: &Arc<EventBus>,
    runtime_config: &Arc<rsid::config::RuntimeConfig>,
) -> Result<MemoryManager> {
    use rsid::memory::embedding::create_embedding_provider;
    use rsid::memory::store::MemoryStore;
    use rsid::memory::worker::spawn_memory_worker;

    let memory_store = MemoryStore::open(&memory_config.db_path)?;
    let main_store = Arc::new(tokio::sync::Mutex::new(rsid::store::Store::open(
        main_db_path,
    )?));

    let http = reqwest::Client::new();
    let mut embedding_result = create_embedding_provider(memory_config, &http).await;
    if embedding_result.provider.is_some() {
        let request = rsid::model_control::ModelAdmissionRequest {
            purpose: rsi_common::model_control::ModelInvocationPurpose::MemoryEmbeddingIndex,
            provider: Some(embedding_result.provider_label.clone()),
            model: Some(embedding_result.model_name().to_string()),
            backend: Some(embedding_result.backend.clone()),
            effort: None,
            trigger: "memory_embedding_provider_select".to_string(),
            owner: rsi_common::model_control::InvocationOwner {
                operator: Some("memory_embedding_provider_select".to_string()),
                ..Default::default()
            },
            dedup_key: Some(rsid::model_control::stable_dedup_key(
                "memory-embedding-provider-select",
                &[
                    &embedding_result.provider_label,
                    &embedding_result.backend,
                    embedding_result.base_url.as_deref().unwrap_or(""),
                    embedding_result.model_name(),
                ],
            )),
            request_fingerprint: Some(rsid::model_control::hash_request_fingerprint(&[
                &embedding_result.provider_label,
                &embedding_result.backend,
                embedding_result.base_url.as_deref().unwrap_or(""),
                embedding_result.model_name(),
            ])),
            parent_invocation_id: None,
            retry_of_invocation_id: None,
            expected_usage: Some(rsid::model_control::explicit_expected_usage(
                rsi_common::model_control::ModelInvocationPurpose::MemoryEmbeddingIndex,
                Some(embedding_result.provider_label.as_str()),
                Some(embedding_result.backend.as_str()),
                Some(embedding_result.model_name()),
            )),
            baseline_input_tokens: 0,
            baseline_output_tokens: 0,
            baseline_cache_creation_tokens: 0,
            baseline_cache_read_tokens: 0,
            baseline_reasoning_tokens: 0,
            baseline_embedding_input_count: 0,
            baseline_wall_time_ms: 0,
        };
        match rsid::model_control::admit_invocation(&main_store, request, event_bus).await {
            Ok(rsid::model_control::AdmissionDecision::Admitted(permit)) => {
                rsid::model_control::complete_invocation(
                    &main_store,
                    &permit,
                    rsid::model_control::InvocationCompletion::default(),
                    event_bus,
                )
                .await?;
            }
            Ok(rsid::model_control::AdmissionDecision::Duplicate { .. }) => {}
            Err(error) => {
                warn!("Embedding provider denied by model control: {error}");
                embedding_result.provider = None;
                embedding_result.unavailable_reason =
                    Some(format!("model control denied embedding provider: {error}"));
            }
        }
    }
    if let Some(ref reason) = embedding_result.fallback_reason {
        warn!("Embedding provider fallback: {}", reason);
    }
    if let Some(ref reason) = embedding_result.unavailable_reason {
        info!("Embedding unavailable (FTS-only mode): {}", reason);
    }

    let handle = spawn_memory_worker(
        memory_config.clone(),
        memory_store,
        main_db_path.to_path_buf(),
        main_store,
        Arc::new(embedding_result),
        Arc::clone(event_bus),
        memory_config.memory_dir.clone(),
        memory_config.db_path.clone(),
        Arc::clone(runtime_config),
    );

    Ok(MemoryManager::new(handle, memory_config.memory_dir.clone()))
}
