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
use rsid::instance_guard::{
    DaemonInstanceGuard, inherited_daemon_listener, refuse_live_daemon_socket,
};
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
/// Stack of every tokio worker thread (#3431). The default 2 MiB overflowed
/// ("thread 'tokio-runtime-worker' has overflowed its stack") in an unoptimized
/// daemon on the session launch/continue path, whose nested async state
/// machines are polled inline on the worker. Stack pages are mapped lazily, so
/// the larger limit costs address space, not resident memory.
const WORKER_STACK_BYTES: usize = 16 * 1024 * 1024;

/// The daemon's multi-thread runtime, with worker stacks sized by
/// `WORKER_STACK_BYTES`.
fn build_daemon_runtime() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(WORKER_STACK_BYTES)
        .build()
}

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
    // #2594: read the supervisor's shutdown-marker path and remove it from the
    // environment while the process is still single-threaded, so no worker,
    // job unit, lander or test this daemon spawns can write the real marker.
    rsid::supervisor_shutdown::capture_marker_path();
    // #2384: the path a supervised restart's compaction helper is launched
    // from; after a deploy swap it names the new build.
    rsid::cold_compact::capture_executable();
    // #130: the offline opt-in (`RSI_OFFLINE=1`, tests and isolated harness
    // runs only) is fixed here, before any background task can probe, poll
    // or check a provider.
    if rsid::offline::init_from_env() {
        eprintln!(
            "rsid: offline mode (RSI_OFFLINE): no provider probe, poll, catalog fetch or credential check opens a network connection"
        );
    }
    // Informational flags exit here, before any daemon state exists. The
    // daemon starts after the match so no arm owns daemon startup: the
    // provider-capability scan reads a match arm's callees as one mapping.
    match cli_action(&std::env::args_os().skip(1).collect::<Vec<_>>()) {
        CliAction::Run => {}
        CliAction::ColdArchiveCompact(args) => {
            // #2384 (plan §5): the isolated compaction helper. Hidden; never
            // starts a daemon, runs no migration and no startup work.
            std::process::exit(rsid::cold_compact::run_helper(&args));
        }
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
        CliAction::RestoreDatabase(database, old_binary) => {
            // Offline recovery only: take the same lease as daemon startup.
            let socket = database.with_file_name("daemon.sock");
            let _guard = DaemonInstanceGuard::acquire(&database, &socket)?;
            rsid::deploy::restore_database_for_binary(&database, &old_binary)?;
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
    let startup = prepare_daemon()?;
    let runtime = build_daemon_runtime()?;
    run_to_bounded_exit(runtime, Box::pin(run_daemon(startup)))
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
    if let Some(code) = supervised_exit_code(
        result.is_ok(),
        rsid::deploy::exit_75_requested(),
        rsid::cold_compact::compact_before_exit,
    ) {
        std::process::exit(code);
    }
    result
}

/// #1045: a deploy restart exits 75 so `rsid-supervisor.sh` relaunches the new
/// binary with its own environment. #2384 (plan §5): first the compaction this
/// restart scheduled runs in its helper, bounded by its deadline (killed and
/// reaped past it); the exit code is 75 whatever the helper did.
fn supervised_exit_code(
    daemon_ok: bool,
    exit_75: bool,
    compact: impl FnOnce() -> Option<rsid::cold_compact::HelperOutcome>,
) -> Option<i32> {
    if !(daemon_ok && exit_75) {
        return None;
    }
    if let Some(outcome) = compact() {
        eprintln!("rsid: cold-archive compaction helper ended: {outcome:?}");
    }
    Some(rsid::watchdog::WATCHDOG_EXIT_CODE)
}

#[derive(Debug, PartialEq, Eq)]
enum CliAction {
    Run,
    Help,
    Version,
    BuildInfo,
    RestoreDatabase(std::path::PathBuf, std::path::PathBuf),
    ColdArchiveCompact(rsid::cold_compact::HelperArgs),
    Invalid,
}

fn cli_action(args: &[std::ffi::OsString]) -> CliAction {
    match args {
        [] => CliAction::Run,
        [mode, rest @ ..]
            if mode.to_str() == Some(rsid::store::cold_archive::reclaim::COMPACT_HELPER_ARG) =>
        {
            rsid::cold_compact::parse_helper_args(rest)
                .map_or(CliAction::Invalid, CliAction::ColdArchiveCompact)
        }
        [flag] if flag == "--help" || flag == "-h" => CliAction::Help,
        [flag] if flag == "--version" || flag == "-V" => CliAction::Version,
        [flag] if flag == "--build-info" => CliAction::BuildInfo,
        [flag, database, old_binary] if flag == "--restore-pre-migration-db" => {
            CliAction::RestoreDatabase(database.into(), old_binary.into())
        }
        _ => CliAction::Invalid,
    }
}

struct DaemonStartup {
    started_at: std::time::Instant,
    config: Config,
    data_dir: std::path::PathBuf,
    db_path: std::path::PathBuf,
    instance_guard: DaemonInstanceGuard,
    inherited_listener: Option<rsid::instance_guard::InheritedListener>,
}

/// Adopt activation and clear its environment before any threads or children.
fn prepare_daemon() -> Result<DaemonStartup> {
    // First action (#1852): nothing this process creates, including a daemon.log
    // the launcher redirected into a still-0755 data directory, may be
    // group/other-readable before the directory is hardened below.
    let previous_umask = rsid_core::private_state::restrict_umask();
    let started_at = std::time::Instant::now();

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

    // Load configuration
    let config = Config::default();

    // Daemon state is owner-only (#1844): 0700 directory, 0600 database, logs
    // and spools on Unix; tightened when an older daemon left them looser. This
    // runs before the first log line (#1852) so a log redirected into an old
    // 0755 directory is tightened before anything is written; results are
    // logged once tracing exists.
    let hardened = config.socket_path.parent().map(|parent| {
        (
            parent,
            rsid_core::private_state::harden_data_dir_top(parent),
        )
    });
    let hardened = match hardened {
        Some((parent, Err(error))) => {
            let created = std::fs::create_dir_all(parent);
            Some((parent, Err(error), created))
        }
        Some((parent, result)) => Some((parent, result, Ok(()))),
        None => None,
    };
    // Children (providers, git worktrees) get the operator's umask back; the
    // directory is private now and private files request 0600 explicitly.
    previous_umask.restore();

    // Initialize tracing
    // The fmt layer writes to stdout. launchd/systemd point that at a file, so
    // colour escapes only belong on an interactive terminal (#240).
    tracing_subscriber::fmt()
        .with_ansi(log_ansi_enabled(
            std::io::IsTerminal::is_terminal(&std::io::stdout()),
            std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty()),
        ))
        .init();

    // Route panics through tracing so they land in the same sink as everything
    // else instead of writing an unstructured line to raw stderr and vanishing
    // from whatever collects daemon logs.
    install_panic_hook();

    info!(
        process_elapsed_ms = rsid::startup_clock::process_elapsed_ms(),
        prepare_elapsed_ms = started_at.elapsed().as_millis() as u64,
        "rsid starting..."
    );

    match load_daemon_dotenv() {
        DaemonDotenvStatus::Loaded(_) | DaemonDotenvStatus::Missing(_) => {}
        DaemonDotenvStatus::Failed { .. } => {
            warn!("Continuing without daemon dotenv values");
        }
    }

    rsi_common::identity::initialize_process_ownership_namespace(&config.socket_path)
        .map_err(rsid::error::DaemonError::Process)?;
    info!(
        socket = %config.socket_path.display(),
        context_rotation_enabled = config.context_rotation_enabled,
        memory_enabled = config.memory_enabled,
        sandbox_base = %config.sandbox_base.display(),
        "Configuration loaded"
    );

    match hardened {
        Some((parent, Ok(report), _)) => {
            if !report.changed.is_empty() {
                info!(
                    count = report.changed.len(),
                    dir = %parent.display(),
                    "Tightened daemon state modes to owner-only"
                );
            }
            report.log_failures();
        }
        Some((parent, Err(error), created)) => {
            warn!(dir = %parent.display(), %error, "Could not secure daemon data directory");
            created?;
        }
        None => {}
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

    // #3480: journal this boot beside the socket so a restart loop is visible.
    // After the lease, so a second instance that is refused leaves no record.
    rsid::daemon_lifecycle::set_journal_path(rsid::daemon_lifecycle::journal_path_beside(
        &config.socket_path,
    ));
    rsid::daemon_lifecycle::journal_record_start();

    // Bridge the first guarded rollout: an already-running legacy binary does
    // not hold the new file lease. A connectable socket is positive incumbent
    // evidence and must never be unlinked. Missing/refused is the stale case.
    // SAFETY: startup is single-threaded, before Tokio or any subprocess.
    let inherited_listener = unsafe { inherited_daemon_listener(&config.socket_path)? };
    Ok(DaemonStartup {
        started_at,
        config,
        data_dir,
        db_path,
        instance_guard,
        inherited_listener,
    })
}

/// Tightens the contents of the daemon-private spools on a background thread
/// (#2375); the directory, its private files and the spools' own modes were
/// settled before the first log line.
fn spawn_private_subtree_hardening(data_dir: std::path::PathBuf) {
    let spawned = std::thread::Builder::new()
        .name("private-state-harden".into())
        .spawn(move || {
            let started = std::time::Instant::now();
            let report = rsid_core::private_state::harden_private_subtrees(&data_dir);
            if !report.changed.is_empty() {
                info!(
                    count = report.changed.len(),
                    dir = %data_dir.display(),
                    "Tightened daemon state modes to owner-only"
                );
            }
            report.log_failures();
            info!(
                elapsed_ms = started.elapsed().as_millis() as u64,
                "Daemon-private spool modes checked"
            );
        });
    if let Err(error) = spawned {
        warn!(%error, "Could not start the daemon-private spool hardening pass");
    }
}

async fn run_daemon(startup: DaemonStartup) -> Result<()> {
    let DaemonStartup {
        started_at,
        mut config,
        data_dir,
        db_path,
        instance_guard,
        inherited_listener,
    } = startup;
    // tokio-console subscriber — compile-time + runtime double opt-in; zero overhead when off.
    #[cfg(feature = "tokio-console")]
    {
        if std::env::var("RSI_TOKIO_CONSOLE").as_deref() == Ok("1") {
            console_subscriber::init();
        }
    }

    let mut spans = rsid::startup_clock::StartupSpans::new();
    // #2375: daemon loops whose first pass walks the store start only after the
    // `request_ready` milestone, so they never hold the store lock inside the
    // authority-initialization window. Set once, right after the milestone.
    let (readiness_tx, readiness_rx) = tokio::sync::watch::channel(false);
    // #2375: the walk of the private spools' contents is not a startup gate.
    // It starts here, not in `prepare_daemon`, which stays single-threaded
    // until the inherited listener is taken.
    spawn_private_subtree_hardening(data_dir.clone());
    // Keep a redirected daemon.log bounded (#960); a no-op under journald.
    rsid::log_rotation::spawn_bounded_log(rsid::log_rotation::configured_log_path(&data_dir));
    let socket_inherited = inherited_listener.is_some();
    if !socket_inherited {
        refuse_live_daemon_socket(&config.socket_path).await?;
    }

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
    spans.mark("store_open");
    // #289: the store's successor-seat check reads the live Claude ladders.
    rsid::claude::install_live_effort_ladder_source();
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
    spans.mark("process_ownership_reap");

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
    spans.mark("provider_probes");

    let mut pending_client = None;
    let listener = if let Some(listener) = inherited_listener {
        pending_client = listener
            .pending
            .map(tokio::net::UnixStream::from_std)
            .transpose()?;
        UnixListener::from_std(listener.listener)?
    } else {
        // Only the daemon-owned path probes, unlinks, binds and sets mode.
        if config.socket_path.exists() {
            warn!(socket = %config.socket_path.display(), "Removing stale socket file");
            tokio::fs::remove_file(&config.socket_path).await?;
        }
        let listener = UnixListener::bind(&config.socket_path)?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&config.socket_path, std::fs::Permissions::from_mode(0o600))?;
        listener
    };

    rsid::deploy::note_startup_milestone();
    info!(
        startup_milestone = "socket_bound",
        socket = %config.socket_path.display(),
        "Unix socket bound; RPC acceptance pending initialization"
    );

    spans.mark("socket_bind");
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
    rsid::deploy::write_supervisor_restart_budget_snapshot(
        &rsi_common::identity::data_path("deploy-restart-budget", "deploy-restart-budget"),
        &runtime_config,
    )?;
    // #1036: publish the operator's spend caps for rsi-cloud spend.
    if let Err(error) = rsid::cloud_spend::write_caps_file(
        &rsid::cloud_spend::default_cloud_dir(),
        &runtime_config.cloud_spend_caps(),
    ) {
        warn!(%error, "could not write the remote-gate spend caps file");
    }
    // #3154: with an OpenRouter spend cap armed, take the first usage probe
    // before sessions are restored (restored sessions are judged against it).
    if runtime_config.openrouter_spend_config().cap_usd > 0.0 {
        rsid::openrouter_spend::prime_probe().await;
    }
    if persisted_daemon_settings > 0 {
        config.apply_runtime_config_snapshot(&runtime_config);
        info!(
            count = persisted_daemon_settings,
            "Applied persisted daemon settings"
        );
    }
    // #2119: the hub build/test CPU budget (`rsi-build.slice`) is in force
    // before any job or gate starts in it. Every launch re-applies it, so a
    // failure here is only logged. #2282: the value the store just loaded is
    // published first, and only the user's own hub daemon writes the slices.
    rsid::build_budget::apply_at_start(
        &data_dir,
        runtime_config
            .hub_build_cpu_percent
            .load(std::sync::atomic::Ordering::Relaxed),
    )
    .await;
    // #3999: keep the slice's MemoryHigh following the operator's own memory use.
    rsid::build_budget::spawn_memory_watch();

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

    // hub #3039: copy the TUI's legacy custom providers (and their plaintext
    // keys, into the vault) from state.json once, without touching the file,
    // then reapply the stored model-role assignments whose keys live only in
    // memory. Neither ever stops the daemon from starting.
    {
        let paths = rsid::operator_config::OperatorConfigPaths::production();
        let vault = rsid::vault::global();
        match rsid::operator_config::legacy_import::import_legacy_tui_state(&store, &vault, &paths)
        {
            Ok(report) if !report.already_imported => info!(
                source_present = report.source_present,
                providers_imported = report.providers_imported,
                keys_imported = report.keys_imported,
                providers_skipped = report.providers_skipped,
                keys_skipped = report.keys_skipped,
                "Imported legacy custom providers from state.json"
            ),
            Ok(_) => {}
            Err(error) => {
                warn!(%error, "Legacy custom provider import failed; it retries at the next start")
            }
        }
        match rsid::operator_config::custom_providers::reapply_all_routes(
            &store,
            &vault,
            &runtime_config,
        ) {
            Ok(0) => {}
            Ok(applied) => info!(applied, "Applied custom provider model roles"),
            Err(error) => warn!(%error, "Could not apply custom provider model roles"),
        }
    }

    spans.mark("settings_and_vault");
    // Codegraph uses its own project-bound databases. An unavailable project
    // or index subsystem must not prevent the session daemon from starting.
    let index_root = db_path.with_file_name("codegraph");
    // The registry snapshot, the project index databases and the watches are
    // installed by `attach_registry` after readiness (#2375).
    let mut _codegraph_runtime = IndexRuntime::start_deferred(
        index_root,
        Some(Arc::clone(&event_bus)),
        Arc::clone(&runtime_config.codegraph_indexing_enabled),
    )
    .inspect_err(|error| warn!(%error, "Codegraph startup failed"))
    .ok();

    spans.mark("codegraph_start");
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
    rsid::session_start_preamble::initialize(session_manager.store());
    if let Some(runtime) = &_codegraph_runtime {
        session_manager.set_codegraph_handle(runtime.handle().clone());
    }
    // Hub #3315: the built-in "RSI home" project, where global and portfolio
    // manager seats run. Idempotent; a failure leaves those seats in their
    // coverage project, as before, and never stops the daemon.
    match dirs::home_dir() {
        Some(home) => {
            if let Err(error) = session_manager.ensure_home_project(home).await {
                warn!(%error, "home project was not registered");
            }
        }
        None => warn!("no home directory: home project was not registered"),
    }

    spans.mark("session_manager_new");
    if codex_available && let Err(error) = session_manager.refresh_codex_catalog_at_startup().await
    {
        warn!(%error, "Codex catalog startup refresh failed; using repository fallbacks");
    }

    spans.mark("codex_catalog");
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

    spans.mark("queue_and_receivers");
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

    spans.mark("operator_message_reconcile");
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
    // #3178: bench trials in flight at a restart become `infra_interrupted` and
    // are never resumed (the restart journal below cannot spend for them: a
    // model call of a task that is not running is refused), and every active
    // bench run gets its adapter socket back.
    {
        let bench = rsid::bench::BenchService::new(
            Arc::clone(session_manager.store()),
            Arc::new(rsid::bench::DaemonBenchHost::new(
                Arc::clone(&session_manager),
                Arc::clone(&runtime_config),
            )),
            rsid::bench::default_root(),
        );
        match bench.restore().await {
            Ok(0) => {}
            Ok(count) => warn!(count, "Bench trials interrupted by the restart"),
            Err(error) => warn!(error = %error, "Bench run restore failed"),
        }
        // #4013: group the finished trials that launched at the project root
        // before #3992. Idempotent; one cheap query when nothing is left.
        match bench.group_orphan_trials().await {
            Ok(0) => {}
            Ok(count) => warn!(count, "Bench trials grouped into their run's Group"),
            Err(error) => warn!(error = %error, "Bench trial backfill failed"),
        }
        rsid::bench::install_global(bench);
        // #3181: the gates of the `bench` job kind read the operator settings and
        // the spend cap through the same installed pieces.
        rsid::agent_jobs::install_bench_job_ports(
            Arc::clone(&runtime_config),
            Arc::clone(session_manager.store()),
        );
    }
    // The exact journal owns graceful interruptions. Claim it after
    // invocation and custody restore, before schedulers or watch dispatch.
    // Only the claim runs here: the claimed intents stay durable (journal-owned
    // for every reader), and their provider continuations, about 400 ms per
    // session, are dispatched in journal order after request_ready (#2375).
    let restart_intents_to_continue = if session_restore_ready {
        session_manager.claim_restart_intents_at_startup().await?
    } else {
        Vec::new()
    };

    spans.mark("restore_sessions");
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

    spans.mark("program_runs");
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

    spans.mark("app_server_seal");
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
        tokio::spawn(after_request_ready(
            readiness_rx.clone(),
            manager.run_agent_message_reconciliation_loop(),
        ));
    }
    // #2201/#2202: admission starts only after the staged-deploy hold is
    // restored from the durable live deploy row. `DeployDrain` starts empty and
    // the deploy runner reloads the row only after startup verification, so a
    // loop spawned before this would admit queued jobs and the next merge-queue
    // batch while a staged deploy is still draining. A restore failure fails
    // startup rather than admitting past an unread hold.
    rsid::deploy::start_admission_after_restore(
        session_manager.store(),
        &session_manager.deploy_drain(),
        &runtime_config,
        || async {
            // Issue #1007: the daemon-owned rolling merge queue. It reconciles
            // interrupted runs once, then claims work only while the operator
            // setting is on.
            tokio::spawn(after_request_ready(
                readiness_rx.clone(),
                rsid::rolling_queue::run_rolling_queue_loop(
                    Arc::clone(session_manager.store()),
                    Arc::clone(&runtime_config),
                    rsid::rolling_queue::LanderLauncher::discover()
                        .with_build_slice()
                        .with_compile_probe(),
                    session_manager.deploy_drain(),
                    session_manager.host_load(),
                ),
            ));

            // Issue #1045: daemon-owned deploys. Verifies a `restarting` deploy
            // once, then waits for a quiet point and restarts through the
            // graceful drain.
            {
                let service = rsid::deploy::DeployService::init_global(config.sandbox_base.clone());
                let manager = Arc::clone(&session_manager);
                service.set_restart_trigger(Arc::new(move || {
                    rsid::deploy::request_exit_75();
                    manager.request_drain_restart();
                }));
                tokio::spawn(after_request_ready(
                    readiness_rx.clone(),
                    rsid::deploy::run_deploy_loop(
                        Arc::clone(session_manager.store()),
                        service,
                        session_manager.deploy_drain(),
                        Arc::clone(&runtime_config),
                    ),
                ));
            }

            // Issue #1002: daemon-owned durable agent jobs. The first poll
            // reconciles units that finished while rsid was down; each job
            // settles and wakes its owner exactly once.
            tokio::spawn(after_request_ready(
                readiness_rx.clone(),
                rsid::agent_jobs::run_agent_jobs_loop(
                    Arc::clone(session_manager.store()),
                    rsid::agent_jobs::platform_job_runtime(),
                    session_manager.deploy_drain(),
                ),
            ));
            // #644: a disk-floor hold that persists is told to its owner and the
            // manager once; no model polling, floor and job timeout unchanged.
            tokio::spawn(after_request_ready(
                readiness_rx.clone(),
                rsid::agent_jobs::run_disk_hold_watch_loop(
                    Arc::clone(session_manager.store()),
                    Arc::clone(&runtime_config),
                ),
            ));
        },
    )
    .await?;

    spans.mark("admission_restore");
    // Restore has armed all retry candidates; now perform the one bounded C5
    // pending-journal replay without delaying startup or scanning sessions.
    {
        let control = session_manager.agent_control();
        let ready = readiness_rx.clone();
        tokio::spawn(after_request_ready(ready, async move {
            control.replay_c5_autofile_pending().await;
        }));
    }

    spans.mark("c5_replay_spawn");
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

    spans.mark("recursive_live_attempts");
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

    spans.mark("recursive_live_outputs");
    // #2375: the Closure terminal-output startup pass (about 290 ms) runs in a
    // task spawned after request_ready, followed by the five-second sweep loop;
    // capture is idempotent and terminal sessions also capture at their own end.

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

    spans.mark("recursive_task_graphs");
    // #634: durable topology executions left in flight by a previous daemon
    // incarnation resume beside the recursive passes, gated by the kill
    // switch and bounded by the same startup recovery budget.
    // Only the lease claim runs here; the claimed executions' drivers (whose
    // first advance launches node sessions) start after request_ready (#2526).
    let recovered_topology_executions = Arc::clone(&session_manager)
        .start_topology_executor(
            config.recursive_dag_startup_recovery_max_graphs as usize,
            std::time::Duration::from_millis(config.recursive_dag_startup_recovery_time_budget_ms),
        )
        .await;

    spans.mark("topology_executor");
    match session_manager.reconcile_topology_workflows().await {
        Ok(count) => info!(
            topology_count = count,
            "Topology workflow bridge reconciled"
        ),
        Err(e) => warn!(error = %e, "Failed to reconcile topology workflow bridge"),
    }

    spans.mark("topology_bridge");
    // Stall classifier signal channel — moved BEFORE stall_detector spawn
    // so the detector can receive a clone of the Sender. Channel is created
    // unconditionally; the classifier task and the detector's classifier
    // branch are both gated on `stall_classifier_enabled`.
    let (stall_classifier_tx, stall_classifier_rx) = tokio::sync::mpsc::channel::<uuid::Uuid>(64);
    let (nudge_tx, nudge_rx) =
        tokio::sync::mpsc::channel::<(uuid::Uuid, rsid::stall_classifier::NudgeAction)>(64);

    // Spawn stall detection background task
    let _stall_detector_handle = {
        use rsid::reconciliation::StallAction;
        // Keep the receiver wired even when disabled at boot. The detector
        // and classifier read their runtime switches before doing work.
        let detector_classifier_tx = Some(stall_classifier_tx.clone());
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
    let reconciliation_heartbeat = Some(rsid::watchdog::LoopHeartbeat::new());
    let _reconciliation_handle = {
        use rsid::reconciliation::{ReconciliationConfig, StallAction};
        info!(
            liveness_interval_secs = config.reconciliation_liveness_interval_secs,
            consistency_interval_secs = config.reconciliation_consistency_interval_secs,
            "Reconciliation loop ready (runtime gated)"
        );
        Some(
            rsid::reconciliation::spawn_reconciliation_loop_with_runtime(
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
                Some(Arc::clone(&runtime_config)),
            ),
        )
    };

    spans.mark("stall_and_chain_setup");
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
    let _stall_classifier_handle = {
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
            "Stall classifier ready (runtime gated)"
        );
        Some(handle)
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
    spans.mark("issue_tracker_setup");
    session_manager.clear_stale_harness_process_wakes().await;
    spans.mark("clear_stale_harness_process_wakes");
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

    spans.mark("scheduler_and_watch_services");
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
    rsid::start_satellite_enqueue_intake(
        Arc::clone(session_manager.store()),
        session_manager.agent_control(),
        Arc::clone(&runtime_config),
    );

    spans.mark("compile_engine_and_warmup");
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
    // #1639: a daemon start follows every deploy and `make release-install`;
    // restart the RSI Remote gateway if Remote is enabled and it is down.
    {
        let rpc_server = Arc::clone(&rpc_server);
        tokio::spawn(async move { rpc_server.converge_remote_gateway().await });
    }

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
    spans.mark("rpc_server_setup");
    if let Err(error) = session_manager.reconcile_incomplete_agent_spawns().await {
        warn!(error = %error, "Incomplete durable agent spawn reconciliation deferred");
    }
    if let Err(error) = session_manager
        .reconcile_incomplete_agent_successors_at_startup()
        .await
    {
        warn!(error = %error, "Incomplete master successor reconciliation deferred");
    }
    spans.mark("agent_spawn_successor_reconcile");
    let session_manager_for_successor_backstop = Arc::clone(&session_manager);
    tokio::spawn(rsid::session::retention::run_retention_loop(Arc::clone(
        &session_manager,
    )));
    // #1333: the andon files one kaizen Issue per repeating friction signature.
    tokio::spawn(rsid::friction::run_andon_loop(Arc::clone(&session_manager)));
    // #3789: import external harness reports filed as GitHub Issues (outbound `gh` only).
    tokio::spawn(rsid::external_report_sync::run_sync_loop(Arc::clone(
        &session_manager,
    )));
    // #2246: a project graph stuck Degraded or Failed for over 24 hours gets one Issue.
    if let Some(runtime) = &_codegraph_runtime {
        tokio::spawn(rsid::codegraph::run_codegraph_health_loop(
            runtime.handle().clone(),
            session_manager.store().clone(),
        ));
    }
    // #2757: Codex plan windows into the same table Claude's rate_limit_event
    // feeds (interval: the live setting `codex_rate_limit_poll_secs`, 0 = off).
    tokio::spawn(rsid::codex_rate_limits::run_codex_rate_limit_poll_loop(
        session_manager.store().clone(),
        Arc::clone(session_manager.event_bus()),
        Arc::clone(&runtime_config),
    ));
    // #3154: while an OpenRouter spend cap is armed, keep the account-usage
    // probe fresh so the launch backstop and the warn notice judge a current
    // figure (idle while no cap is armed).
    tokio::spawn(rsid::openrouter_spend::run_spend_probe_loop(
        Arc::clone(&runtime_config),
        session_manager.store().clone(),
    ));
    // #1337: the CPU-time andon per agent-owned process tree.
    tokio::spawn(rsid::cpu_andon::run_cpu_andon_loop(
        Arc::clone(&session_manager),
        Arc::clone(&runtime_config),
    ));
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
                // #2494: the tick always runs a pass; an event only brings
                // the next one forward (see `rsid::manager_wake`).
                let wake = tokio::select! {
                    _=interval.tick()=>None,
                    event=manager_events.recv()=>match event {
                        Ok(event)=>Some(rsid::manager_wake::ManagerWake::classify(event.as_ref())),
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_))=>Some(rsid::manager_wake::ManagerWake::Hint),
                        Err(tokio::sync::broadcast::error::RecvError::Closed)=>break,
                    }
                };
                let runs = wake
                    .unwrap_or(rsid::manager_wake::ManagerWake::Ignore)
                    .runs(wake.is_none(), last_scan.elapsed());
                if !runs {
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
                // #1005: rotate capped coordinating seats at their idle
                // boundary with a daemon-written handoff.
                if let Err(error) = succession_runtime.rotate_capped_coordinators().await {
                    warn!(error = %error, "Coordinator context cap pass deferred");
                }
                // #1254: finish worker batons a previous process recorded.
                if let Err(error) = succession_runtime.deliver_pending_worker_batons().await {
                    warn!(error = %error, "Worker baton pass deferred");
                }
                // #1294: write tier-mail outcomes whose first write failed;
                // settle abandoned claims uncertain. Never redelivers.
                if let Err(error) = succession_runtime.resettle_tier_mail().await {
                    warn!(error = %error, "Tier mail settlement pass deferred");
                }
                // #3390: launch ready `dispatch:auto` Issues into their
                // starter without a manager turn.
                if let Err(error) = succession_runtime.dispatch_auto_issues().await {
                    warn!(error = %error, "Issue auto-dispatch pass deferred");
                }
            }
        });
    }

    // Subscribe at boot; the runtime switch gates every stall event.
    {
        let session_manager_for_stall = Arc::clone(&session_manager);
        let stall_runtime = Arc::clone(&runtime_config);
        let mut stall_rx = event_bus.subscribe();
        tokio::spawn(async move {
            while let Some(session_id) =
                rsid::stall_detector::next_retryable_stall(&mut stall_rx, &stall_runtime).await
            {
                if session_manager_for_stall
                    .interrupt_if_stall_retryable(session_id)
                    .await
                {
                    info!(session_id = %session_id, "Stall-triggered retry interrupt sent");
                }
            }
        });
        info!("Stall-triggered retry handler ready (runtime gated)");
    }

    spans.mark("authority_tail");
    let authority_initialized_at = std::time::Instant::now();
    rsid::deploy::note_startup_milestone();
    info!(
        startup_milestone = "authority_initialized",
        "Daemon authority initialization complete"
    );

    // Issue #25/#69: startup maintenance is accepted only after every
    // authority-bearing restore and reconciliation above. Its daemon-owned
    // receiver monitor keeps the lifecycle cancellation signal truthful while
    // request readiness advances independently of the pass result.
    let sandbox_cache_maintenance = session_manager.start_sandbox_build_cache_maintenance();
    let source_worktree_settlement = session_manager.start_source_worktree_auto_settlement();

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

    // #999: reclaim stale agent scratch outside sandboxes (/var/tmp and worker
    // TMPDIRs). Fixed conservative policy (#1140): only directories RSI
    // recorded creating, 72 h old, unheld, in authenticated unmounted roots,
    // holding no dirty or unpublished git work; bounded per pass. Runs on a
    // blocking thread, first pass after ten minutes.
    let scratch_store = Arc::clone(session_manager.store());
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval_at(
            tokio::time::Instant::now() + std::time::Duration::from_secs(600),
            std::time::Duration::from_secs(3600),
        );
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            // #646: the open jobs let a build target whose worktree is gone go
            // at once; an unreadable store keeps the age floor.
            let open_jobs =
                match tokio::time::timeout(std::time::Duration::from_secs(5), scratch_store.lock())
                    .await
                {
                    Ok(store) => rsid::agent_scratch_reclaim::open_jobs(&store),
                    Err(_) => None,
                };
            // #1150: deletion is on after the hardened core's final check
            // (registry pruning stays off; see agent_scratch_reclaim).
            let _ = tokio::task::spawn_blocking(move || {
                rsid::agent_scratch_reclaim::run_and_log_with_jobs(false, "periodic", open_jobs)
            })
            .await;
        }
    });

    // local #120 (hub #3103): the project-file trash keeps the TUI's 7-day
    // rule; one bounded pass at start, later passes follow trash writes.
    tokio::task::spawn_blocking(rsid::prune_project_file_trash_at_start);

    // local #86: an image UploadPasteImage stored that nothing ever named is
    // removed 7 days after its last upload; a named one only under the opt-in
    // setting paste_retention_referenced_days. One bounded pass at start,
    // later passes follow uploads (rsid::paste_retention).
    tokio::spawn(rsid::paste_retention::prune_paste_images_at_start(
        std::sync::Arc::clone(session_manager.store()),
    ));

    spans.mark("post_authority_services");
    info!(
        startup_milestone = "request_ready",
        total_elapsed_ms = started_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
        process_elapsed_ms = rsid::startup_clock::process_elapsed_ms(),
        post_authority_elapsed_ms = authority_initialized_at
            .elapsed()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64,
        "Daemon initialized, ready to accept connections"
    );
    rsid::process_memory::on_request_ready();
    // #2375: release the loops that wait for readiness (agent-message
    // reconciliation, merge queue, deploy, agent jobs, C5 replay).
    let _ = readiness_tx.send(true);
    // #2375: deploy-interrupted and other restart-journal sessions were
    // claimed before readiness; their provider continuations start now, in
    // journal order, ahead of the topology drivers below. They never gate RPC
    // acceptance.
    tokio::spawn({
        let session_manager = Arc::clone(&session_manager);
        async move {
            session_manager
                .continue_claimed_restart_intents(restart_intents_to_continue)
                .await
        }
    });
    // #2753: probe the provider CLI logins (`claude`, `codex`) once the daemon
    // is up, so launch admission and the provider status know whether each is
    // logged in before the first session is spent. Background, never gating.
    session_manager.provider_login().spawn_refresh_all();
    // #2755: the provider CLI contract canary (Codex hook trust and handshake,
    // Claude flags and initialize) runs once now and again whenever a CLI's
    // version changes. Background, never gating; a failed check only marks the
    // provider degraded.
    session_manager.spawn_provider_contract_check();
    // #2375: the Closure terminal-output startup pass (a bounded store walk) and
    // the five-second sweep loop that follows it start only now; neither may
    // gate RPC acceptance. The pass is idempotent, so a reader that races it
    // sees the same state the periodic sweep already allows.
    tokio::spawn({
        let session_manager = Arc::clone(&session_manager);
        async move {
            session_manager
                .run_closure_output_recovery_after_readiness()
                .await
        }
    });
    // #2375: the codegraph registry snapshot, the project index databases and
    // the inotify watches (about 470 ms) are installed by the registry task,
    // which starts only now; they never gate RPC acceptance.
    if let Some(runtime) = &mut _codegraph_runtime {
        runtime.attach_registry(
            session_manager.store().clone(),
            config.workspace_roots.clone(),
        );
    }

    // #2561: a shutdown the supervisor had to force-end is surfaced to the
    // managers by this start. A store pass, so it follows readiness.
    tokio::spawn({
        let session_manager = Arc::clone(&session_manager);
        async move {
            let store = session_manager.store().lock().await;
            let surfaced = rsid::supervisor_shutdown::surface_incidents(
                &store,
                &rsid::supervisor_shutdown::incident_path(),
            );
            if surfaced > 0 {
                warn!(
                    surfaced,
                    "Recorded forced daemon shutdown incidents from the supervisor"
                );
            }
        }
    });
    // #2375: starter topology seeding (a store pass) follows readiness; it
    // must never gate RPC acceptance.
    tokio::spawn({
        let session_manager = Arc::clone(&session_manager);
        async move {
            session_manager
                .seed_starter_topologies_after_restart()
                .await
        }
    });
    // Retained settlement journals already fence launch, archive and cleanup.
    // Process-holder proofs can be expensive and must never gate RPC acceptance.
    tokio::spawn({
        let session_manager = Arc::clone(&session_manager);
        async move {
            if let Err(error) = session_manager
                .recover_startup_source_worktree_settlements()
                .await
            {
                warn!(error = %error, "Post-readiness settlement recovery deferred; durable fences remain");
            }
        }
    });
    // #2526: a recovered topology execution's first advance launches a node
    // session (git-worktree sandbox allocation, target seed, provider spawn)
    // and the archived sandbox purge runs git proofs; neither may sit on the
    // readiness path, so both start only now.
    tokio::spawn({
        let session_manager = Arc::clone(&session_manager);
        async move {
            session_manager
                .drive_recovered_topology_executions(recovered_topology_executions)
                .await
        }
    });
    {
        let manager = Arc::clone(&session_manager);
        tokio::spawn(manager.run_archived_sandbox_purge_loop());
    }
    // Topology pending discards and settlement cleanup release sandboxes via
    // git proofs (~129 git subprocesses on a big retained set, #2375). They
    // are durable and idempotent and the supervisor already runs them beside
    // RPC traffic, so they must never gate RPC acceptance.
    tokio::spawn({
        let session_manager = Arc::clone(&session_manager);
        async move {
            session_manager
                .recover_topology_settlements_after_restart()
                .await
        }
    });
    // Sandbox diagnostics classify every retained sandbox; they run after
    // readiness so they never delay restarts (#961).
    tokio::spawn({
        let session_manager = Arc::clone(&session_manager);
        async move { session_manager.run_startup_sandbox_diagnostics().await }
    });
    // #2384: the cold-storage archive loop is spawned only after readiness and
    // waits a further 15 minutes before its first look at the setting, so an
    // archive pass can never sit on the startup or readiness path. The manager's
    // pass-thread lifecycle becomes the process-wide one here too (#205): the
    // deploy loop and the exit path reach it through that slot, and no pass can
    // exist before it (no connection is accepted until this block is past).
    rsid::cold_archive_lifecycle::install_process(Arc::clone(
        session_manager.cold_archive_lifecycle(),
    ));
    tokio::spawn(Arc::clone(&session_manager).run_cold_archive_loop());
    tokio::spawn(Arc::clone(&session_manager).run_error_disposition_loop());

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
    // #3433: SIGINT is registered eagerly (what `ctrl_c()` does on first poll),
    // so the sender-capture handler installed below chains to a live runtime
    // handler instead of being replaced when the first `select!` polls it.
    let mut sigint = signal::unix::signal(signal::unix::SignalKind::interrupt())?;

    // SIGTERM is what process supervisors actually send (`systemctl stop`,
    // bare `kill`, `docker stop`). Without an explicit handler its default
    // disposition is immediate process termination, which bypasses the
    // graceful shutdown below entirely — orphaning live provider subprocesses
    // instead of interrupting them cleanly, and dropping in-flight persistence
    // writes. Handling it here is what makes that shutdown path reachable in
    // the standard "stop the daemon" case. `Signal::recv` is cancel-safe, so
    // losing this branch to another `select!` arm cannot drop a signal.
    let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate())?;
    // #3433: after both handlers exist, record who sends SIGINT/SIGTERM.
    rsid::signal_sender::install_shutdown_signal_capture();
    // #2936: after the SIGTERM handler exists, so the watch's signal drains.
    #[cfg(target_os = "macos")]
    rsid::supervisor_shutdown::spawn_supervisor_parent_watch();

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
                    let first_request = operator_drain_started.is_none();
                    let started = *operator_drain_started.get_or_insert_with(|| {
                        info!("Operator DRAIN restart requested; status reads remain available during the wait");
                        rsid::supervisor_shutdown::mark_shutdown_begun("drain_restart");
                        tokio::time::Instant::now()
                    });
                    if first_request {
                        rsid::daemon_lifecycle::journal_record_stop(session_manager.store(), "drain_restart").await;
                    }
                    shutdown_deadline = started + OPERATOR_DRAIN_LIMIT + SHUTDOWN_LIMIT;
                    // Complete when nothing is active, when every active session
                    // waits only on a call the daemon is serving (a turn that
                    // cannot finish while the daemon drains, #2353), or at the limit.
                    let decision = session_manager
                        .operator_drain_decision(started.elapsed(), OPERATOR_DRAIN_LIMIT)
                        .await;
                    if let rsid::session::OperatorDrainDecision::Complete {
                        active_count,
                        daemon_waiting,
                        deploy_interrupted,
                        limit_reached,
                    } = decision
                    {
                        if daemon_waiting > 0 && !limit_reached {
                            info!(daemon_waiting, "Operator DRAIN wait ends early: every remaining session waits only on the daemon");
                        }
                        if deploy_interrupted > 0 && !limit_reached {
                            info!(deploy_interrupted, "Operator DRAIN wait ends early: the remaining workers are the ones the deploy interrupts");
                        }
                        info!(active_count, "Operator DRAIN wait complete");
                        if let Some(watchdog) = watchdog.take() { watchdog.stop(); }
                        if let Some(handle) = &scheduler_handle {
                            let _ = shutdown_stage("scheduler", shutdown_deadline, handle.shutdown()).await;
                        }
                        drain_deadline = Some(tokio::time::Instant::now() + CONNECTION_DRAIN_LIMIT);
                        rpc_server.begin_connection_drain();
                    }
                }
            }
            _ = operator_message_tick.tick(), if drain_deadline.is_none() => {
                if let Err(error) = session_manager.dispatch_operator_messages_once().await {
                    warn!(%error, "Operator message dispatch deferred");
                }
            }
            _ = sigint.recv(), if drain_deadline.is_none() => {
                info!("Received shutdown signal (Ctrl+C)");
                let sender = rsid::signal_sender::take_sender(true);
                rsid::supervisor_shutdown::mark_shutdown_begun_from(
                    "ctrl_c",
                    sender.as_ref().map(|sender| sender.pid),
                );
                rsid::supervisor_shutdown::log_shutdown_signal("SIGINT", sender.as_ref());
                rsid::daemon_lifecycle::journal_record_stop(session_manager.store(), "ctrl_c").await;
                hard_shutdown_requested = true;
                shutdown_deadline = tokio::time::Instant::now() + SHUTDOWN_LIMIT;
                session_manager.begin_restart_drain();
                if let Some(watchdog) = watchdog.take() { watchdog.stop(); }
                if let Some(handle) = &scheduler_handle {
                    let _ = shutdown_stage("scheduler", shutdown_deadline, handle.shutdown()).await;
                }
                drain_deadline = Some(tokio::time::Instant::now() + CONNECTION_DRAIN_LIMIT);
                rpc_server.begin_connection_drain();
                info!(connections = connections.len(), "Connection drain started");
                rsid::supervisor_shutdown::record_shutdown_signal(
                    session_manager.store(),
                    "SIGINT",
                    sender.as_ref(),
                )
                .await;
            }
            _ = sigterm.recv(), if drain_deadline.is_none() => {
                info!("Received shutdown signal (SIGTERM)");
                let sender = rsid::signal_sender::take_sender(false);
                rsid::supervisor_shutdown::mark_shutdown_begun_from(
                    "sigterm",
                    sender.as_ref().map(|sender| sender.pid),
                );
                rsid::supervisor_shutdown::log_shutdown_signal("SIGTERM", sender.as_ref());
                rsid::daemon_lifecycle::journal_record_stop(session_manager.store(), "sigterm").await;
                hard_shutdown_requested = true;
                shutdown_deadline = tokio::time::Instant::now() + SHUTDOWN_LIMIT;
                session_manager.begin_restart_drain();
                if let Some(watchdog) = watchdog.take() { watchdog.stop(); }
                if let Some(handle) = &scheduler_handle {
                    let _ = shutdown_stage("scheduler", shutdown_deadline, handle.shutdown()).await;
                }
                drain_deadline = Some(tokio::time::Instant::now() + CONNECTION_DRAIN_LIMIT);
                rpc_server.begin_connection_drain();
                info!(connections = connections.len(), "Connection drain started");
                rsid::supervisor_shutdown::record_shutdown_signal(
                    session_manager.store(),
                    "SIGTERM",
                    sender.as_ref(),
                )
                .await;
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
            result = async {
                if let Some(stream) = pending_client.take() {
                    let address = stream.peer_addr()?;
                    Ok((stream, address))
                } else {
                    listener.accept().await
                }
            }, if drain_deadline.is_none() => {
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
    source_worktree_settlement.abort();

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

    // An external holder owns the inherited front door across daemon exits.
    if !socket_inherited {
        if let Err(e) = tokio::fs::remove_file(&config.socket_path).await {
            warn!(error = %e, "Failed to remove socket file");
        }
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

/// Runs `work` once the `request_ready` milestone has been signalled. A daemon
/// that exits before readiness drops the sender; the work then starts so a
/// shutdown path never leaves it parked.
async fn after_request_ready<F: std::future::Future>(
    mut ready: tokio::sync::watch::Receiver<bool>,
    work: F,
) -> F::Output {
    let _ = ready.wait_for(|ready| *ready).await;
    work.await
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

/// Whether the tracing fmt layer may emit ANSI colour escapes: only when the
/// sink is an interactive terminal and `NO_COLOR` is not set (#240).
fn log_ansi_enabled(sink_is_terminal: bool, no_color: bool) -> bool {
    sink_is_terminal && !no_color
}

#[cfg(test)]
mod tests {
    /// #3431: a worker thread of the daemon runtime has room for a stack the
    /// default 2 MiB would overflow (aborting the process).
    #[test]
    fn daemon_runtime_workers_have_a_deep_stack() {
        #[inline(never)]
        fn burn(depth: usize) -> usize {
            let frame = [depth as u8; 1024];
            let below = if depth == 0 { 0 } else { burn(depth - 1) };
            std::hint::black_box(&frame);
            below + usize::from(frame[0] != 7)
        }
        let runtime = super::build_daemon_runtime().expect("runtime");
        // ~4 MiB of frames: beyond the 2 MiB tokio default, inside 16 MiB.
        let used = runtime
            .block_on(async { tokio::spawn(async { burn(4096) }).await })
            .expect("worker task");
        assert!(used > 0);
    }

    /// #240: a file sink (launchd `daemon.log`) never gets ANSI escapes.
    #[test]
    fn log_ansi_follows_the_sink_and_no_color() {
        assert!(!super::log_ansi_enabled(false, false));
        assert!(!super::log_ansi_enabled(false, true));
        assert!(super::log_ansi_enabled(true, false));
        assert!(!super::log_ansi_enabled(true, true));
    }

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
        health_rpc_with_read_timeout(socket_path, std::time::Duration::from_secs(1))
    }

    fn health_rpc_with_read_timeout(
        socket_path: &std::path::Path,
        read_timeout: std::time::Duration,
    ) -> std::io::Result<String> {
        use std::io::{BufRead, Write};

        let mut stream = std::os::unix::net::UnixStream::connect(socket_path)?;
        stream.set_read_timeout(Some(read_timeout))?;
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
        let startup = prepare_daemon().expect("single-threaded fixture startup");
        let runtime = build_daemon_runtime().expect("fixture runtime");
        if std::env::var_os("RSI_TEST_STUCK_BLOCKING_TASK").is_some() {
            // Stands in for a blocking read of a provider child's pipe.
            runtime.spawn_blocking(|| std::thread::sleep(Duration::from_secs(600)));
        }
        run_to_bounded_exit(runtime, run_daemon(startup)).expect("isolated daemon fixture");
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
            // Generous: only a log poll, and the hold (30 s safety net)
            // outlasts it plus the health deadline below.
            std::time::Duration::from_secs(10),
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

        // The daemon holds reclaim until this test writes `reclaim_release`
        // (its own safety timeout is 30 s, `pause_reclaim_after_single_flight_for_test`),
        // so the deadline only has to be generous: a host under load may take
        // seconds to answer, and a read that times out (EAGAIN/WouldBlock) is
        // retried. The deadline plus one read timeout stays well inside the
        // hold, and the checks below prove the hold was still in force when
        // the answer arrived.
        const REAL_DAEMON_HEALTH_DEADLINE: std::time::Duration = std::time::Duration::from_secs(15);
        const REAL_DAEMON_HEALTH_ATTEMPT_TIMEOUT: std::time::Duration =
            std::time::Duration::from_secs(5);
        let health_deadline = std::time::Instant::now() + REAL_DAEMON_HEALTH_DEADLINE;
        let health_response = loop {
            match health_rpc_with_read_timeout(&socket, REAL_DAEMON_HEALTH_ATTEMPT_TIMEOUT) {
                Ok(response) if response.contains("\"result\"") => break response,
                _ if std::time::Instant::now() < health_deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                other => panic!("health RPC did not complete during reclaim hold: {other:?}"),
            }
        };
        // The hold is still in force when the answer arrives: the test has not
        // released it, the daemon's own 30 s safety net cannot have expired, and
        // the entered marker is still there (checked again via the log below).
        assert!(
            reclaim_entered.exists()
                && !reclaim_release.exists()
                && held_at.elapsed() < std::time::Duration::from_secs(28),
            "health answered only after the reclaim hold could have ended"
        );
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
            // The hold is released; only a loaded host delays the log line.
            std::time::Duration::from_secs(20),
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
    fn settlement_process_proofs_are_dispatched_after_rpc_readiness() {
        let source = include_str!("main.rs");
        let main_start = source.find("async fn run_daemon").unwrap();
        let main_end = source[main_start..]
            .find("fn recursive_startup_recovery_budget")
            .unwrap()
            + main_start;
        let main_body = &source[main_start..main_end];
        let ready = main_body
            .find(r#"startup_milestone = "request_ready""#)
            .unwrap();
        let recovery = main_body
            .find("recover_startup_source_worktree_settlements()")
            .unwrap();
        assert!(ready < recovery);
        let launch = include_str!("session/launch.rs");
        let restore = launch
            .split_once("pub async fn restore_sessions(&self)")
            .unwrap()
            .1;
        let restore = restore
            .split_once(r#"startup_milestone = "authority_restore_complete""#)
            .unwrap()
            .0;
        assert_eq!(
            restore
                .matches("reap_startup_settlement_orphans_checked")
                .count(),
            0
        );
        assert_eq!(
            restore
                .matches("recover_source_worktree_settlements")
                .count(),
            0
        );
    }

    /// #2384 (plan §5, review #2398): a supervised restart exits 75 after the
    /// compaction helper ran or was killed at its deadline; nothing else runs it.
    #[test]
    fn a_supervised_exit_runs_the_compaction_first_and_always_exits_75() {
        use rsid::cold_compact::HelperOutcome;
        for outcome in [
            Some(HelperOutcome::Killed),
            Some(HelperOutcome::Exited(Some(1))),
            Some(HelperOutcome::SpawnFailed("gone".into())),
            Some(HelperOutcome::Deferred(
                "a pass thread was still running".into(),
            )),
            None,
        ] {
            let mut ran = false;
            let code = supervised_exit_code(true, true, || {
                ran = true;
                outcome.clone()
            });
            assert_eq!(
                code,
                Some(rsid::watchdog::WATCHDOG_EXIT_CODE),
                "{outcome:?}"
            );
            assert!(ran);
        }
        // No restart requested, or the daemon failed: no helper, no exit 75.
        for (ok, exit_75) in [(true, false), (false, true), (false, false)] {
            let code = supervised_exit_code(ok, exit_75, || panic!("no compaction here"));
            assert_eq!(code, None);
        }
        // The real exit path calls the helper after the runtime teardown and
        // before the exit.
        let source = include_str!("main.rs");
        let start = source.find("fn run_to_bounded_exit").unwrap();
        let body = &source[start..start + source[start..].find("\n}\n").unwrap()];
        let teardown = body
            .find("runtime.shutdown_timeout(RUNTIME_DROP_GRACE)")
            .unwrap();
        let compact = body
            .find("rsid::cold_compact::compact_before_exit")
            .unwrap();
        let exit = body.find("std::process::exit(code)").unwrap();
        assert!(teardown < compact && compact < exit);
    }

    #[test]
    fn the_compaction_helper_mode_parses_before_any_daemon_starts() {
        let args: Vec<std::ffi::OsString> = [
            "cold-archive-compact",
            "--db",
            "/d/rsi.db",
            "--tmp-dir",
            "/d/tmp/compact-1",
            "--result",
            "/d/tmp/compact-1.json",
            "--run-id",
            "00000000-0000-4000-8000-000000000001",
        ]
        .iter()
        .map(std::ffi::OsString::from)
        .collect();
        assert!(matches!(
            cli_action(&args),
            CliAction::ColdArchiveCompact(_)
        ));
        assert_eq!(cli_action(&args[..3]), CliAction::Invalid);
        let source = include_str!("main.rs");
        let main_start = source.find("fn main() -> Result<()>").unwrap();
        let helper = source[main_start..]
            .find("CliAction::ColdArchiveCompact(args)")
            .unwrap();
        let daemon = source[main_start..].find("prepare_daemon()?").unwrap();
        assert!(
            helper < daemon,
            "the helper mode exits before prepare_daemon"
        );
    }

    /// #2384: the compaction decision lives at the deploy loop's quiet point
    /// (before `mark_agent_deploy_restarting`) and its reconcile in the
    /// cold-archive loop; neither is on the startup or readiness path.
    #[test]
    fn the_compaction_decision_and_reconcile_are_never_on_the_startup_path() {
        let main = include_str!("main.rs");
        let main_start = main.find("async fn run_daemon").unwrap();
        let main_end = main[main_start..]
            .find("fn recursive_startup_recovery_budget")
            .unwrap()
            + main_start;
        let daemon = &main[main_start..main_end];
        for name in [
            "decide_at_quiet_point",
            "cold_claim_compaction",
            "cold_reconcile_compactions",
            "run_compaction",
        ] {
            assert!(!daemon.contains(name), "run_daemon names {name}");
        }
        let deploy = include_str!("deploy.rs");
        let poll = deploy.find("async fn poll_step(").unwrap();
        let poll_end = poll + deploy[poll..].find("\n}\n").unwrap();
        let decide = deploy
            .find("crate::cold_compact::decide_at_quiet_point(&store)")
            .unwrap();
        let restarting = deploy[poll..poll_end]
            .find("store.mark_agent_deploy_restarting(row.id, now)")
            .unwrap()
            + poll;
        assert!(
            poll < decide && decide < restarting,
            "decided at the quiet point, before the restart mark"
        );
        assert_eq!(deploy.matches("decide_at_quiet_point").count(), 1);
        // The store reconcile has one caller, `reconcile_cold_compactions`, and
        // that runs only first thing in the loop spawned after readiness.
        let service = include_str!("session/cold_archive.rs");
        assert_eq!(service.matches("cold_reconcile_compactions(").count(), 1);
        assert_eq!(service.matches(".reconcile_cold_compactions()").count(), 1);
        let loop_start = service.find("pub async fn run_cold_archive_loop").unwrap();
        let call = service.find(".reconcile_cold_compactions()").unwrap();
        let delay = service
            .find("tokio::time::sleep(COLD_ARCHIVE_INITIAL_DELAY)")
            .unwrap();
        assert!(
            loop_start < call && call < delay,
            "reconciled first thing in the archive loop"
        );
    }

    /// #2384 review (#205): the session manager's archive lifecycle becomes the
    /// process-wide one after readiness (nothing on the startup path touches the
    /// cold archive) and before the loop that can start a pass, so the deploy loop
    /// and the exit path reach it.
    #[test]
    fn the_archive_lifecycle_is_installed_after_readiness_and_before_the_archive_loop() {
        let source = include_str!("main.rs");
        let main_start = source.find("async fn run_daemon").unwrap();
        let main_end = source[main_start..]
            .find("fn recursive_startup_recovery_budget")
            .unwrap()
            + main_start;
        let body = &source[main_start..main_end];
        let ready = body.find(r#"startup_milestone = "request_ready""#).unwrap();
        let install = body.find("install_process(").unwrap();
        let spawn = body.find("run_cold_archive_loop()").unwrap();
        assert!(ready < install, "never on the startup or readiness path");
        assert!(install < spawn, "installed before the loop is spawned");
        assert_eq!(body.matches("install_process(").count(), 1);
    }

    /// #2384: the cold-archive loop is spawned after `request_ready`, never on
    /// the startup or readiness path, and nothing before readiness starts a pass.
    #[test]
    fn cold_archive_loop_is_spawned_after_request_ready() {
        let source = include_str!("main.rs");
        let main_start = source.find("async fn run_daemon").unwrap();
        let main_end = source[main_start..]
            .find("fn recursive_startup_recovery_budget")
            .unwrap()
            + main_start;
        let main_body = &source[main_start..main_end];
        let ready = main_body
            .find(r#"startup_milestone = "request_ready""#)
            .unwrap();
        let spawn = main_body.find("run_cold_archive_loop()").unwrap();
        assert!(ready < spawn, "the archive loop must follow readiness");
        assert_eq!(main_body.matches("run_cold_archive_loop").count(), 1);
        assert!(
            !main_body[..ready].contains("cold_archive"),
            "nothing on the startup path may touch the cold archive"
        );
        let service = include_str!("session/cold_archive.rs");
        let initial_delay = service
            .find("tokio::time::sleep(COLD_ARCHIVE_INITIAL_DELAY)")
            .unwrap();
        let first_tick = service.find("interval.tick().await").unwrap();
        assert!(
            initial_delay < first_tick,
            "the loop waits its initial delay before its first tick"
        );
    }

    #[test]
    fn provider_inventory_fence_precedes_probes_workers_and_restore() {
        let source = include_str!("main.rs");
        let main_start = source.find("fn prepare_daemon").expect("main function");
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
            .find("let listener = if let Some(listener) = inherited_listener")
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

        let socket_bind = main_body
            .find("let listener = if let Some(listener) = inherited_listener")
            .expect("socket bind");
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

    /// #2375: the topology settlement cleanup (about 129 git subprocesses)
    /// must not sit between authority_restore_complete and request_ready.
    /// Only the execution-resume pass is awaited before readiness; the
    /// settlement recovery is a spawned task after the request_ready milestone.
    #[test]
    fn topology_settlement_recovery_runs_after_request_ready_in_a_spawned_task() {
        let source = include_str!("main.rs");
        let main_start = source.find("async fn run_daemon").expect("main function");
        let helper_start = source[main_start..]
            .find("fn recursive_startup_recovery_budget")
            .map(|offset| main_start + offset)
            .expect("startup recovery helper follows main");
        let main_body = &source[main_start..helper_start];

        let executor_start = main_body
            .find(".start_topology_executor(")
            .expect("topology executor resume pass");
        let request_ready = main_body
            .find("startup_milestone = \"request_ready\"")
            .expect("request-ready milestone");
        let cleanup = main_body
            .find(".recover_topology_settlements_after_restart()")
            .expect("deferred settlement recovery");
        let accept = main_body.find("listener.accept()").expect("accept loop");
        assert!(executor_start < request_ready);
        assert!(request_ready < cleanup, "cleanup must follow readiness");
        assert!(cleanup < accept);
        // It is detached: the closest preceding statement is a tokio::spawn,
        // not an inline await in the readiness path.
        let spawn = main_body[request_ready..cleanup]
            .rfind("tokio::spawn(")
            .expect("cleanup is spawned");
        assert!(spawn < cleanup - request_ready);
        assert!(
            !main_body[..request_ready].contains("recover_topology_settlements_after_restart"),
            "the cleanup is not referenced on the readiness path"
        );

        // The awaited pass owns no settlement work.
        let graph = include_str!("session/graph_executions.rs");
        let start = graph
            .find("pub async fn start_topology_executor")
            .expect("start_topology_executor");
        let end = graph[start..]
            .find("pub async fn recover_topology_settlements_after_restart")
            .map(|offset| start + offset)
            .expect("deferred recovery follows");
        let pre_readiness = &graph[start..end];
        assert!(pre_readiness.contains("claim_recovery_leases("));
        assert!(!pre_readiness.contains("recover_executions("));
        assert!(!pre_readiness.contains("recover_after_restart("));
        assert!(!pre_readiness.contains("recover_settlements("));
    }

    /// #2375: starter topology seeding and codegraph watch installation are
    /// off the readiness path, and the milestone logs the process clock.
    #[test]
    fn starter_seeding_and_codegraph_watches_follow_request_ready() {
        let source = include_str!("main.rs");
        let main_start = source.find("async fn run_daemon").expect("main function");
        let helper_start = source[main_start..]
            .find("fn recursive_startup_recovery_budget")
            .map(|offset| main_start + offset)
            .expect("startup recovery helper follows main");
        let main_body = &source[main_start..helper_start];
        let request_ready = main_body
            .find("startup_milestone = \"request_ready\"")
            .expect("request-ready milestone");
        let accept = main_body.find("listener.accept()").expect("accept loop");
        let (before, after) = main_body.split_at(request_ready);

        assert!(!before.contains("seed_starter_topologies_after_restart"));
        let seed = after
            .find(".seed_starter_topologies_after_restart()")
            .expect("starter seeding follows readiness");
        assert!(request_ready + seed < accept);
        assert!(
            after[..seed].rfind("tokio::spawn(").is_some(),
            "starter seeding is detached from the readiness path"
        );
        assert!(
            after[..after
                .find("rsid::process_memory::on_request_ready()")
                .unwrap()]
                .contains("process_elapsed_ms ="),
            "the milestone logs process_elapsed_ms beside total_elapsed_ms"
        );

        // The awaited executor pass no longer seeds starters.
        let graph = include_str!("session/graph_executions.rs");
        let start = graph
            .find("pub async fn start_topology_executor")
            .expect("start_topology_executor");
        let end = graph[start..]
            .find("pub async fn drive_recovered_topology_executions")
            .map(|offset| start + offset)
            .expect("post-readiness driver follows");
        assert!(!graph[start..end].contains("starters::seed_all("));
        assert!(graph.contains("pub async fn seed_starter_topologies_after_restart"));

        // Codegraph startup registers nothing: the registry snapshot, the index
        // databases and the watches are the registry task's first step, which
        // starts only after the request_ready milestone.
        assert!(!before.contains("IndexRuntime::start_from_store"));
        assert!(!before.contains(".attach_registry("));
        let attach = after
            .find(".attach_registry(")
            .expect("registry task starts after readiness");
        assert!(request_ready + attach < accept);
        let runtime = include_str!("codegraph/runtime.rs");
        let deferred = runtime
            .find("pub fn start_deferred")
            .expect("start_deferred");
        let deferred_end = runtime[deferred..]
            .find("fn start_with_workspaces")
            .map(|offset| deferred + offset)
            .expect("start_with_workspaces");
        assert!(runtime[deferred..deferred_end].contains("Vec::new()"));
    }

    /// #2375: the process-start-to-first-log window (~540 ms) was the walk of
    /// the private spools' contents. Only the shallow pass precedes tracing;
    /// the walk runs on its own thread once the runtime exists, and the
    /// first log line carries the clock so the window stays visible.
    #[test]
    fn private_spool_walk_is_off_the_pre_tracing_path() {
        let source = include_str!("main.rs");
        let prepare = source.find("fn prepare_daemon").expect("prepare_daemon");
        let run = source.find("async fn run_daemon").expect("run_daemon");
        let prepare_end = source
            .find("fn spawn_private_subtree_hardening")
            .expect("hardening helper");
        let prepare_body = &source[prepare..prepare_end];
        assert!(prepare_body.contains("harden_data_dir_top("));
        assert!(!prepare_body.contains("harden_data_dir("));
        assert!(!prepare_body.contains("harden_private_subtrees("));
        assert!(
            !prepare_body.contains("thread::"),
            "prepare stays single-threaded"
        );
        let tracing_init = prepare_body.find("tracing_subscriber::fmt()").unwrap();
        let first_line = prepare_body.find("\"rsid starting...\"").unwrap();
        assert!(tracing_init < first_line);
        assert!(
            prepare_body[tracing_init..first_line].contains("process_elapsed_ms"),
            "the first log line names the process clock"
        );
        let run_body = &source[run..];
        let spawn = run_body
            .find("spawn_private_subtree_hardening(data_dir.clone())")
            .expect("walk starts in run_daemon");
        let socket_bound = run_body
            .find("startup_milestone = \"socket_bound\"")
            .unwrap();
        assert!(spawn < socket_bound);
        assert!(source.contains("harden_private_subtrees(&data_dir)"));
    }

    /// #2375: every awaited gap before `request_ready` has a named span.
    #[test]
    fn startup_spans_tile_the_authority_gap() {
        let source = include_str!("main.rs");
        let run = source.find("async fn run_daemon").expect("run_daemon");
        let body = &source[run..];
        let restore = body.find("spans.mark(\"restore_sessions\")").unwrap();
        let tail = body.find("spans.mark(\"authority_tail\")").unwrap();
        let initialized = body
            .find("startup_milestone = \"authority_initialized\"")
            .unwrap();
        let ready = body.find("startup_milestone = \"request_ready\"").unwrap();
        assert!(restore < tail && tail < initialized && initialized < ready);
        for name in [
            "operator_message_reconcile",
            "program_runs",
            "admission_restore",
            "topology_executor",
            "topology_bridge",
            "clear_stale_harness_process_wakes",
            "post_authority_services",
        ] {
            assert!(body.contains(&format!("spans.mark(\"{name}\")")), "{name}");
        }
    }

    /// #2375 slice 4: the Closure terminal-output startup pass is not awaited
    /// between authority_restore_complete and request_ready. A task spawned
    /// after the milestone runs the pass and then the sweep loop; the pass is
    /// idempotent (capture-or-replay), so it still completes.
    #[test]
    fn closure_output_recovery_runs_after_request_ready_in_a_spawned_task() {
        let source = include_str!("main.rs");
        let run = source.find("async fn run_daemon").expect("run_daemon");
        let tests = source
            .find("#[cfg(test)]\nmod tests")
            .expect("tests module");
        let body = &source[run..tests];
        let ready = body.find("startup_milestone = \"request_ready\"").unwrap();
        let task = body
            .find(".run_closure_output_recovery_after_readiness()")
            .expect("closure recovery task");
        let accept = body.find("// Main loop").unwrap();
        assert!(ready < task && task < accept);
        assert!(
            body[ready..task].rfind("tokio::spawn(").is_some(),
            "the closure pass is a detached task"
        );
        assert!(
            !body[..ready].contains("reconcile_closure_outputs_after_restart")
                && !body[..ready].contains("run_closure_output_reconciliation_loop"),
            "no closure sweep runs on the readiness path"
        );
        assert!(!body.contains("spans.mark(\"closure_outputs\")"));

        // The task runs the pass first, then the loop, so they never overlap.
        let session = include_str!("session/mod.rs");
        let start = session
            .find("pub async fn run_closure_output_recovery_after_readiness")
            .expect("recovery task body");
        let pass = session[start..]
            .find("self.reconcile_closure_outputs_after_restart()")
            .expect("startup pass");
        let sweep = session[start..]
            .find("self.run_closure_output_reconciliation_loop()")
            .expect("sweep loop");
        assert!(pass < sweep);
    }

    /// #2375 slice 5: the daemon loops whose first pass walks the store
    /// (agent-message reconciliation, merge queue, deploy, agent jobs, C5
    /// replay) are spawned before readiness but wait for the `request_ready`
    /// signal, so none holds the store lock inside the authority window. The
    /// signal is sent right after the milestone, and each loop still runs.
    #[test]
    fn store_walking_loops_wait_for_the_request_ready_signal() {
        let source = include_str!("main.rs");
        let run = source.find("async fn run_daemon").expect("run_daemon");
        let tests = source
            .find("#[cfg(test)]\nmod tests")
            .expect("tests module");
        let body = &source[run..tests];
        let ready = body.find("startup_milestone = \"request_ready\"").unwrap();
        let signal = body.find("readiness_tx.send(true)").expect("signal");
        assert!(ready < signal);
        for spawn in [
            "run_agent_message_reconciliation_loop()",
            "run_rolling_queue_loop(",
            "run_deploy_loop(",
            "run_agent_jobs_loop(",
            "run_disk_hold_watch_loop(",
            "replay_c5_autofile_pending()",
        ] {
            let at = body.find(spawn).unwrap_or_else(|| panic!("{spawn}"));
            let gate = body[..at]
                .rfind("after_request_ready(")
                .unwrap_or_else(|| panic!("{spawn} is not gated"));
            assert!(at - gate < 600, "{spawn} must sit inside its gate");
            assert!(gate < ready, "{spawn} is spawned before the milestone");
        }
        assert!(
            !body.contains("tokio::spawn(rsid::agent_jobs::run_agent_jobs_loop"),
            "no ungated jobs loop"
        );
    }

    /// The gate runs its work after the signal, and still runs it when the
    /// sender is dropped without one.
    #[tokio::test]
    async fn after_request_ready_runs_work_after_the_signal_and_on_sender_drop() {
        let (tx, rx) = tokio::sync::watch::channel(false);
        let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = Arc::clone(&ran);
        let task = tokio::spawn(after_request_ready(rx.clone(), async move {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        }));
        tokio::task::yield_now().await;
        assert!(!ran.load(std::sync::atomic::Ordering::SeqCst));
        tx.send(true).unwrap();
        task.await.unwrap();
        assert!(ran.load(std::sync::atomic::Ordering::SeqCst));

        let (tx, rx) = tokio::sync::watch::channel(false);
        drop(tx);
        assert_eq!(after_request_ready(rx, async { 7 }).await, 7);
    }

    /// #2375: restart-journal continuations (a provider spawn each, about 400 ms
    /// per deploy-interrupted worker) are dispatched by a task spawned after
    /// `request_ready`, ahead of the topology drivers; before readiness only
    /// the claim runs, and the whole pass never runs inline.
    #[test]
    fn restart_journal_continuations_are_spawned_after_request_ready() {
        let source = include_str!("main.rs");
        let run = source.find("async fn run_daemon").expect("run_daemon");
        let tests = source
            .find("#[cfg(test)]\nmod tests")
            .expect("tests module");
        let body = &source[run..tests];
        let restore = body.find("spans.mark(\"restore_sessions\")").unwrap();
        let claim = body.find(".claim_restart_intents_at_startup()").unwrap();
        let ready = body.find("startup_milestone = \"request_ready\"").unwrap();
        let cont = body.find(".continue_claimed_restart_intents(").unwrap();
        let drivers = body.find(".drive_recovered_topology_executions(").unwrap();
        let accept = body.find("// Main loop").unwrap();
        assert!(
            claim < restore,
            "the claim is authority work before readiness"
        );
        assert!(ready < cont, "the continuation follows readiness");
        assert!(
            cont < drivers,
            "journal resumes start before the topology drivers"
        );
        assert!(cont < accept);
        assert!(
            body[ready..cont].rfind("tokio::spawn(").is_some(),
            "the continuation is a detached task"
        );
        assert!(
            !body[..ready].contains(".continue_claimed_restart_intents(")
                && !body.contains(".reconcile_restart_intents_at_startup()"),
            "the whole journal pass never runs inline on the startup path"
        );
    }

    /// #2526: no topology node launch and no sandbox work (archived purge)
    /// may happen before request_ready. Before it, restart recovery only
    /// claims leases; the drivers (whose first advance launches node sessions)
    /// and the purge loop are spawned after the milestone.
    #[test]
    fn topology_node_launch_and_sandbox_purge_start_after_request_ready() {
        let source = include_str!("main.rs");
        let main_start = source.find("async fn run_daemon").expect("main function");
        let helper_start = source[main_start..]
            .find("fn recursive_startup_recovery_budget")
            .map(|offset| main_start + offset)
            .expect("startup recovery helper follows main");
        let main_body = &source[main_start..helper_start];
        let request_ready = main_body
            .find("startup_milestone = \"request_ready\"")
            .expect("request-ready milestone");
        let accept = main_body.find("listener.accept()").expect("accept loop");
        let (before, after) = main_body.split_at(request_ready);

        for call in [
            "drive_recovered_topology_executions(",
            "run_archived_sandbox_purge_loop()",
        ] {
            assert!(!before.contains(call), "{call} must follow request_ready");
            let at = after.find(call).unwrap_or_else(|| panic!("{call} spawned"));
            assert!(request_ready + at < accept);
            assert!(
                after[..at].rfind("tokio::spawn(").is_some(),
                "{call} is detached from the readiness path"
            );
        }
        // The awaited startup pass never advances an execution or drives one.
        assert!(!before.contains("recover_topology_settlements_after_restart"));
        let graph = include_str!("session/graph_executions.rs");
        let start = graph
            .find("pub async fn start_topology_executor")
            .expect("start_topology_executor");
        let end = graph[start..]
            .find("pub async fn drive_recovered_topology_executions")
            .map(|offset| start + offset)
            .expect("post-readiness driver follows");
        // The supervisor loop is a detached task that first sleeps a tick; the
        // awaited part is everything before it.
        let supervisor = graph[start..end]
            .find("tokio::spawn(async move")
            .expect("supervisor task");
        let pre_readiness = &graph[start..start + supervisor];
        assert!(!pre_readiness.contains(".advance("));
        assert!(!pre_readiness.contains("drive_topology_execution("));
        assert!(!pre_readiness.contains("spawn_driver("));
        // The claim helper itself never advances.
        let recovery = include_str!("topology/recovery.rs");
        let claim_start = recovery
            .find("pub(crate) async fn claim_recovery_leases")
            .expect("claim helper");
        let claim_end = recovery[claim_start..]
            .find("pub(crate) async fn recover_after_restart")
            .or_else(|| recovery[claim_start..].find("pub(crate) async fn recover_executions"))
            .map(|offset| claim_start + offset)
            .expect("claim helper end");
        assert!(!recovery[claim_start..claim_end].contains(".advance("));
    }

    #[test]
    fn daemon_instance_lease_precedes_store_open_and_socket_unlink() {
        let source = include_str!("main.rs");
        let main_start = source.find("fn prepare_daemon").expect("main function");
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

        let inherited_validation = main_body
            .find("inherited_daemon_listener(&config.socket_path)")
            .expect("validate inherited listener");
        assert!(lease < inherited_validation);
        assert!(inherited_validation < socket_probe);
        assert!(main_body.contains("if !socket_inherited {\n        refuse_live_daemon_socket"));
        let ordinary_socket_branch = main_body
            .find("UnixListener::from_std(listener.listener)?\n    } else {")
            .expect("only an ordinary daemon owns socket cleanup");
        assert!(ordinary_socket_branch < socket_unlink);
        assert!(main_body.contains("UnixListener::from_std(listener.listener)?\n    } else {"));
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
    let main_store = Arc::new(rsid_core::store_lock::Mutex::new(rsid::store::Store::open(
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
