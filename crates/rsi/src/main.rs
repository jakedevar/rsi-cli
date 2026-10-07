use rsi::app::App;
use rsi::client::DaemonClient;
use std::path::{Path, PathBuf};
use std::process::Command;

const ENV_TUI_NO_AUTO_START_DAEMON: &str = "RSI_TUI_NO_AUTO_START_DAEMON";
const RSID_SCOPE_SETTINGS_FILE: &str = "rsid-scope.env";
const RSID_SCOPE_PREFIX: &str = "rsid-tui";
/// Where `scripts/install-release.sh` installs the supervised daemon set
/// (`~/.rsi/install`): `rsid` and `rsid-supervisor.sh`.
#[cfg(target_os = "linux")]
const INSTALL_SUBDIR: &str = "install";
#[cfg(target_os = "linux")]
const SUPERVISOR_SCRIPT: &str = "rsid-supervisor.sh";
#[cfg(target_os = "linux")]
const WORKER_SLICE: &str = "rsi-workers.slice";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WorkerScopeSettings {
    memory_high_mib: u64,
    memory_max_mib: u64,
    memory_swap_max_mib: u64,
    cpu_weight: u32,
}

impl WorkerScopeSettings {
    fn defaults() -> Self {
        let (memory_high_mib, memory_max_mib) = rsi_common::worker_memory::default_limits_mib();
        Self {
            memory_high_mib,
            memory_max_mib,
            memory_swap_max_mib: 0,
            cpu_weight: 20,
        }
    }

    fn validate(self) -> color_eyre::Result<()> {
        if !(256..=1_048_576).contains(&self.memory_high_mib)
            || !(256..=1_048_576).contains(&self.memory_max_mib)
            || self.memory_high_mib >= self.memory_max_mib
            || self.memory_swap_max_mib > 1_048_576
            || !(1..=10_000).contains(&self.cpu_weight)
        {
            return Err(color_eyre::eyre::eyre!("invalid worker slice limits"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RsidScopeSettings {
    memory_high_mib: u64,
    memory_max_mib: u64,
    memory_swap_max_mib: u64,
    cpu_weight: u32,
    worker: WorkerScopeSettings,
}

impl RsidScopeSettings {
    fn defaults() -> Self {
        Self {
            memory_high_mib: 6 * 1024,
            memory_max_mib: 8 * 1024,
            memory_swap_max_mib: 0,
            cpu_weight: 20,
            worker: WorkerScopeSettings::defaults(),
        }
    }

    fn to_env(self) -> String {
        format!(
            "rsid_scope_memory_high_mib={}\nrsid_scope_memory_max_mib={}\nrsid_scope_memory_swap_max_mib={}\nrsid_scope_cpu_weight={}\nworker_scope_memory_high_mib={}\nworker_scope_memory_max_mib={}\nworker_scope_memory_swap_max_mib={}\nworker_scope_cpu_weight={}\n",
            self.memory_high_mib,
            self.memory_max_mib,
            self.memory_swap_max_mib,
            self.cpu_weight,
            self.worker.memory_high_mib,
            self.worker.memory_max_mib,
            self.worker.memory_swap_max_mib,
            self.worker.cpu_weight
        )
    }

    fn parse(contents: &str) -> color_eyre::Result<Self> {
        let mut memory_high_mib = None;
        let mut memory_max_mib = None;
        let mut memory_swap_max_mib = None;
        let mut cpu_weight = None;
        let mut worker_high = None;
        let mut worker_max = None;
        let mut worker_swap = None;
        let mut worker_cpu = None;

        for (line_index, raw_line) in contents.lines().enumerate() {
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (key, raw_value) = line.split_once('=').ok_or_else(|| {
                color_eyre::eyre::eyre!("invalid rsid scope settings line {}", line_index + 1)
            })?;
            let value = raw_value.trim().parse::<u64>().map_err(|_| {
                color_eyre::eyre::eyre!("invalid numeric value for rsid scope setting {key}")
            })?;
            let slot = match key.trim() {
                "rsid_scope_memory_high_mib" => &mut memory_high_mib,
                "rsid_scope_memory_max_mib" => &mut memory_max_mib,
                "rsid_scope_memory_swap_max_mib" => &mut memory_swap_max_mib,
                "worker_scope_memory_high_mib" => &mut worker_high,
                "worker_scope_memory_max_mib" => &mut worker_max,
                "worker_scope_memory_swap_max_mib" => &mut worker_swap,
                "worker_scope_cpu_weight" => {
                    let weight = u32::try_from(value).map_err(|_| {
                        color_eyre::eyre::eyre!("worker_scope_cpu_weight exceeds u32::MAX")
                    })?;
                    if worker_cpu.replace(weight).is_some() {
                        return Err(color_eyre::eyre::eyre!(
                            "duplicate worker scope setting {key}"
                        ));
                    }
                    continue;
                }
                "rsid_scope_cpu_weight" => {
                    let cpu_weight_value = u32::try_from(value).map_err(|_| {
                        color_eyre::eyre::eyre!("rsid_scope_cpu_weight exceeds u32::MAX")
                    })?;
                    if cpu_weight.replace(cpu_weight_value).is_some() {
                        return Err(color_eyre::eyre::eyre!(
                            "duplicate rsid scope setting {key}"
                        ));
                    }
                    continue;
                }
                _ => {
                    return Err(color_eyre::eyre::eyre!(
                        "unknown rsid scope setting {key:?}"
                    ));
                }
            };
            if slot.replace(value).is_some() {
                return Err(color_eyre::eyre::eyre!(
                    "duplicate rsid scope setting {key}"
                ));
            }
        }

        let worker = if [worker_high, worker_max, worker_swap]
            .iter()
            .all(Option::is_none)
            && worker_cpu.is_none()
        {
            WorkerScopeSettings::defaults()
        } else {
            WorkerScopeSettings {
                memory_high_mib: worker_high.ok_or_else(|| {
                    color_eyre::eyre::eyre!("missing worker_scope_memory_high_mib")
                })?,
                memory_max_mib: worker_max.ok_or_else(|| {
                    color_eyre::eyre::eyre!("missing worker_scope_memory_max_mib")
                })?,
                memory_swap_max_mib: worker_swap.ok_or_else(|| {
                    color_eyre::eyre::eyre!("missing worker_scope_memory_swap_max_mib")
                })?,
                cpu_weight: worker_cpu
                    .ok_or_else(|| color_eyre::eyre::eyre!("missing worker_scope_cpu_weight"))?,
            }
        };
        let settings = Self {
            memory_high_mib: memory_high_mib
                .ok_or_else(|| color_eyre::eyre::eyre!("missing rsid_scope_memory_high_mib"))?,
            memory_max_mib: memory_max_mib
                .ok_or_else(|| color_eyre::eyre::eyre!("missing rsid_scope_memory_max_mib"))?,
            memory_swap_max_mib: memory_swap_max_mib
                .ok_or_else(|| color_eyre::eyre::eyre!("missing rsid_scope_memory_swap_max_mib"))?,
            cpu_weight: cpu_weight
                .ok_or_else(|| color_eyre::eyre::eyre!("missing rsid_scope_cpu_weight"))?,
            worker,
        };
        settings.validate()?;
        Ok(settings)
    }

    fn validate(self) -> color_eyre::Result<()> {
        const MEMORY_MIN_MIB: u64 = 256;
        const MEMORY_MAX_MIB: u64 = 1024 * 1024;
        if !(MEMORY_MIN_MIB..=MEMORY_MAX_MIB).contains(&self.memory_high_mib)
            || !(MEMORY_MIN_MIB..=MEMORY_MAX_MIB).contains(&self.memory_max_mib)
            || self.memory_high_mib >= self.memory_max_mib
        {
            return Err(color_eyre::eyre::eyre!(
                "rsid MemoryHigh and MemoryMax must be between {MEMORY_MIN_MIB} and {MEMORY_MAX_MIB} MiB, with MemoryHigh below MemoryMax"
            ));
        }
        if self.memory_swap_max_mib > MEMORY_MAX_MIB {
            return Err(color_eyre::eyre::eyre!(
                "rsid MemorySwapMax must be between 0 and {MEMORY_MAX_MIB} MiB"
            ));
        }
        if !(1..=10_000).contains(&self.cpu_weight) {
            return Err(color_eyre::eyre::eyre!(
                "rsid CPUWeight must be between 1 and 10000"
            ));
        }
        self.worker.validate()?;
        Ok(())
    }

    /// `systemd-run` arguments for one bounded scope running `argv` (the bare
    /// daemon, or `rsid-supervisor.sh <rsid>`), the same properties
    /// `scripts/install-release.sh` `restart_rsid` uses.
    fn systemd_run_args(self, unit: &str, argv: &[PathBuf]) -> Vec<String> {
        let mut args = vec![
            "--user".to_string(),
            "--scope".to_string(),
            "--collect".to_string(),
            format!("--unit={unit}"),
            "--slice=user.slice".to_string(),
            format!("--property=MemoryHigh={}M", self.memory_high_mib),
            format!("--property=MemoryMax={}M", self.memory_max_mib),
            format!("--property=MemorySwapMax={}M", self.memory_swap_max_mib),
            format!("--property=CPUWeight={}", self.cpu_weight),
            "--".to_string(),
        ];
        args.extend(argv.iter().map(|part| part.display().to_string()));
        args
    }
}

/// How the TUI starts a daemon: the argv the launch runs and whether it is
/// under `rsid-supervisor.sh` (managed deploys need it, #1217).
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, PartialEq, Eq)]
struct DaemonLaunch {
    argv: Vec<PathBuf>,
    supervised: bool,
}

#[cfg(target_os = "linux")]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// Choose the daemon launch. A release TUI uses the supervisor script and
/// `rsid` that `install-release.sh` installs under `<data_dir>/install`, never
/// a script from a sandbox or the operator checkout. Without that install, and
/// for dev (`target/debug`) builds, it falls back to the bare daemon.
#[cfg(target_os = "linux")]
fn plan_daemon_launch(
    current_exe: Option<&Path>,
    path_daemon: Option<&Path>,
    data_dir: &Path,
) -> DaemonLaunch {
    let bare = || DaemonLaunch {
        argv: vec![resolve_daemon_command_with(current_exe, path_daemon)],
        supervised: false,
    };
    if current_exe.is_some_and(prefer_sibling_daemon) {
        return bare();
    }
    let install_dir = data_dir.join(INSTALL_SUBDIR);
    let supervisor = install_dir.join(SUPERVISOR_SCRIPT);
    let rsid = install_dir.join(rsi_common::identity::DAEMON_BINARY);
    if is_executable_file(&supervisor) && is_executable_file(&rsid) {
        return DaemonLaunch {
            argv: vec![supervisor, rsid],
            supervised: true,
        };
    }
    tracing::warn!(
        supervisor = %supervisor.display(),
        "no installed rsid-supervisor.sh; starting rsid unsupervised (run `make release-install`)"
    );
    bare()
}

#[cfg(target_os = "linux")]
fn resolve_daemon_launch() -> DaemonLaunch {
    let current_exe = std::env::current_exe().ok();
    let path_daemon = which::which(rsi_common::identity::DAEMON_BINARY).ok();
    plan_daemon_launch(
        current_exe.as_deref(),
        path_daemon.as_deref(),
        &rsi_common::identity::data_dir(),
    )
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;

    // Initialize tracing to file (not stdout — that's the terminal)
    let log_dir = rsi_common::identity::data_dir();
    let _ = std::fs::create_dir_all(&log_dir);

    // #1406: `rsi export --clean` / `rsi init --from` are operator CLI
    // commands, not the TUI. They talk to the daemon and exit.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(parsed) = rsi::portable_cli::parse(&args) {
        let command = match parsed {
            Ok(command) => command,
            Err(error) => {
                eprintln!("rsi: {error}\n\n{}", rsi::portable_cli::USAGE);
                std::process::exit(2);
            }
        };
        let mut client = DaemonClient::new(DaemonClient::default_socket_path());
        let start = || try_auto_start_daemon(&log_dir).map_err(|error| error.to_string());
        if let Err(error) = rsi::portable_cli::run(command, &mut client, start).await {
            eprintln!("rsi: {error}");
            std::process::exit(1);
        }
        return Ok(());
    }

    let log_path = log_dir.join("tui.log");
    if log_path.exists() {
        if let Err(err) = rotate_log_file(&log_path, &log_dir) {
            eprintln!("failed to rotate {:?}: {}", &log_path, err);
        }
    }

    let log_file = std::fs::File::create(&log_path)?;
    tracing_subscriber::fmt()
        .with_writer(std::sync::Mutex::new(log_file))
        .with_ansi(false)
        .init();

    // Chained panic hook: log message + backtrace to ~/.rsi/tui.log, then
    // delegate to the previous (color_eyre) hook. Must be installed after
    // color_eyre::install() (so we wrap its hook) and after the tracing
    // subscriber init (so the log write lands). The hook only LOGS — it must
    // not touch the terminal, because it also fires for tokio-task panics
    // that don't kill the TUI. Terminal restore is TerminalGuard's job.
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // force_capture: deterministic backtrace without RUST_BACKTRACE set.
        let backtrace = std::backtrace::Backtrace::force_capture();
        tracing::error!("panic: {info}\nbacktrace:\n{backtrace}");
        prev_hook(info);
    }));

    tracing::info!("rsi TUI starting...");

    if should_auto_start_daemon() {
        // Auto-start daemon if not running
        if let Err(e) = try_auto_start_daemon(&log_dir) {
            tracing::warn!("daemon auto-start failed: {}", e);
            eprintln!("rsid auto-start failed: {e}");
            // Continue anyway — TUI reconnect loop will show connection status
        }
    } else {
        tracing::info!(
            env_var = ENV_TUI_NO_AUTO_START_DAEMON,
            "daemon auto-start disabled"
        );
    }

    // Setup terminal
    crossterm::terminal::enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    crossterm::execute!(
        stdout,
        crossterm::terminal::EnterAlternateScreen,
        crossterm::event::EnableMouseCapture,
        crossterm::event::EnableBracketedPaste,
        crossterm::event::PushKeyboardEnhancementFlags(
            crossterm::event::KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                .union(crossterm::event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )
    )?;
    // From here on, the guard's Drop is the single terminal-teardown path:
    // it runs on clean return, on `?`-error return, and on panic unwind.
    let _guard = TerminalGuard;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = ratatui::Terminal::new(backend)?;
    terminal.clear()?;

    // Create app
    let socket_path = DaemonClient::default_socket_path();
    let client = DaemonClient::new(socket_path);
    let mut app = App::new(client);

    // Run event loop
    rsi::event::run_event_loop(&mut terminal, &mut app).await;

    // Clean up pasted image temp files
    rsi::clipboard::cleanup_paste_dir(&app.paste_dir);

    // Terminal restore happens in TerminalGuard::drop at end of scope.
    tracing::info!("rsi TUI exited cleanly");

    Ok(())
}

/// Restores the terminal when dropped — on clean exit, `?`-error return, or
/// panic unwind through `main`. Mirrors the inverse of the setup sequence on
/// a fresh stdout handle; every call is best-effort so Drop never panics.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::event::PopKeyboardEnhancementFlags,
            crossterm::event::DisableBracketedPaste,
            crossterm::terminal::LeaveAlternateScreen,
            crossterm::event::DisableMouseCapture,
            crossterm::cursor::Show,
        );
        if std::thread::panicking() {
            // The panic hook already logged details; point the user at them
            // now that stderr is visible again.
            eprintln!("rsi crashed; panic details in ~/.rsi/tui.log");
        }
    }
}

/// Attempt to auto-start the daemon if it's not already running.
/// Returns Ok(()) if daemon is already running or successfully started.
/// Returns Err if spawn fails (e.g., no sibling daemon binary and rsid not in PATH).
fn try_auto_start_daemon(log_dir: &Path) -> color_eyre::Result<()> {
    let socket_path = DaemonClient::default_socket_path();

    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;

        if managed_rsid_service_enabled() {
            return start_managed_rsid_service(&socket_path);
        }

        // A previous rsid.scope may remain loaded after the daemon exits.
        // A fresh name also prevents concurrent TUI launches from colliding.
        let unit = format!("{RSID_SCOPE_PREFIX}-{}.scope", uuid::Uuid::new_v4());
        let daemon_log_path = log_dir.join("daemon.log");
        let mut launched_scope = None;
        let result = ensure_daemon_socket(&socket_path, || {
            let scope = read_or_initialize_rsid_scope_settings()?;
            provision_worker_slice(scope.worker)?;
            let launch = resolve_daemon_launch();
            tracing::info!(supervised = launch.supervised, argv = ?launch.argv, "launching rsid");
            let daemon_log = open_daemon_log(&daemon_log_path)?;
            let child = Command::new("systemd-run")
                .args(scope.systemd_run_args(&unit, &launch.argv))
                .stdin(std::process::Stdio::null())
                .stdout(daemon_log.try_clone()?)
                .stderr(daemon_log)
                .process_group(0)
                .spawn()
                .map_err(|error| {
                    color_eyre::eyre::eyre!(
                        "failed to invoke systemd-run for {unit}: {error}; see {}",
                        daemon_log_path.display()
                    )
                })?;
            launched_scope = Some(scope);
            Ok(child)
        });
        let started = match result {
            Ok(DaemonStart::Existing | DaemonStart::PeerStarted) => false,
            Ok(DaemonStart::Launched(child)) => {
                reap_launcher(child);
                true
            }
            Err(error) => {
                if launched_scope.is_some() {
                    let _ = Command::new("systemctl")
                        .args(["--user", "stop", &unit])
                        .status();
                }
                return Err(error);
            }
        };
        if !started {
            return Ok(());
        }
        let scope = launched_scope.expect("started scope has settings");
        if let Err(error) = verify_rsid_scope(scope, &unit) {
            let _ = Command::new("systemctl")
                .args(["--user", "stop", &unit])
                .status();
            return Err(error);
        }
        tracing::info!(
            unit = %unit,
            "daemon started in its bounded systemd user scope; logs at {:?}",
            daemon_log_path
        );
    }

    #[cfg(target_os = "macos")]
    {
        use std::os::unix::process::CommandExt;

        let daemon_log_path = log_dir.join("daemon.log");
        let start = ensure_daemon_socket(&socket_path, || {
            let daemon_log = open_daemon_log(&daemon_log_path)?;
            Command::new(resolve_daemon_command())
                .stdin(std::process::Stdio::null())
                .stdout(daemon_log.try_clone()?)
                .stderr(daemon_log)
                .process_group(0)
                .spawn()
                .map_err(|error| {
                    color_eyre::eyre::eyre!(
                        "failed to start rsid: {error}; see {}",
                        daemon_log_path.display()
                    )
                })
        })?;
        if let DaemonStart::Launched(child) = start {
            reap_launcher(child);
        }
    }

    Ok(())
}

#[cfg(target_os = "linux")]
fn managed_rsid_service_enabled() -> bool {
    Command::new("systemctl")
        .args(["--user", "is-enabled", "rsid.service"])
        .output()
        .is_ok_and(|output| {
            output.status.success()
                && matches!(
                    output.stdout.as_slice(),
                    b"enabled\n" | b"enabled-runtime\n"
                )
        })
}

#[cfg(target_os = "linux")]
fn start_managed_rsid_service(socket_path: &Path) -> color_eyre::Result<()> {
    if std::os::unix::net::UnixStream::connect(socket_path).is_ok() {
        if managed_rsid_service_active() {
            return Ok(());
        }
        return Err(color_eyre::eyre::eyre!(
            "rsid.service is enabled, but another daemon owns its socket; stop the legacy daemon before starting the service"
        ));
    }
    let status = Command::new("systemctl")
        .args(["--user", "start", "rsid.service"])
        .status()?;
    if !status.success() {
        return Err(color_eyre::eyre::eyre!(
            "cannot start rsid.service ({status}); inspect systemctl --user status rsid.service"
        ));
    }
    let started = std::time::Instant::now();
    while started.elapsed() < std::time::Duration::from_secs(90) {
        if std::os::unix::net::UnixStream::connect(socket_path).is_ok()
            && managed_rsid_service_active()
        {
            tracing::info!("connected to managed rsid.service");
            return Ok(());
        }
        if Command::new("systemctl")
            .args(["--user", "is-failed", "--quiet", "rsid.service"])
            .status()
            .is_ok_and(|status| status.success())
        {
            return Err(color_eyre::eyre::eyre!(
                "rsid.service failed before its socket became available"
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    Err(color_eyre::eyre::eyre!(
        "rsid.service did not provide {} within 90 seconds",
        socket_path.display()
    ))
}

#[cfg(target_os = "linux")]
fn managed_rsid_service_active() -> bool {
    Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", "rsid.service"])
        .status()
        .is_ok_and(|status| status.success())
}

fn open_daemon_log(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
}

enum DaemonStart {
    Existing,
    PeerStarted,
    Launched(std::process::Child),
}

/// Reap the daemon or systemd-run launcher without blocking the TUI. A
/// successful launcher can outlive the TUI event loop by many hours.
fn reap_launcher(mut child: std::process::Child) {
    std::thread::spawn(move || {
        if let Err(error) = child.wait() {
            tracing::warn!("cannot reap rsid launcher: {error}");
        }
    });
}

/// A failed connect, including a stale socket file, must attempt a launch;
/// rsid owns stale-socket cleanup.
fn ensure_daemon_socket(
    socket_path: &Path,
    start: impl FnOnce() -> color_eyre::Result<std::process::Child>,
) -> color_eyre::Result<DaemonStart> {
    if std::os::unix::net::UnixStream::connect(socket_path).is_ok() {
        tracing::info!("daemon already running at {:?}", socket_path);
        return Ok(DaemonStart::Existing);
    }
    tracing::info!(
        "daemon not detected, attempting auto-start at {:?}",
        socket_path
    );
    let mut child = start()?;
    match wait_for_daemon_socket(socket_path, &mut child) {
        Ok(true) => Ok(DaemonStart::Launched(child)),
        Ok(false) => Ok(DaemonStart::PeerStarted),
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(error)
        }
    }
}

#[cfg(target_os = "linux")]
fn read_or_initialize_rsid_scope_settings() -> color_eyre::Result<RsidScopeSettings> {
    let path = rsi_common::identity::data_path(RSID_SCOPE_SETTINGS_FILE, "rsid-scope");
    read_or_initialize_rsid_scope_settings_at(&path)
}

#[cfg(target_os = "linux")]
fn read_or_initialize_rsid_scope_settings_at(path: &Path) -> color_eyre::Result<RsidScopeSettings> {
    match std::fs::read_to_string(&path) {
        Ok(contents) => RsidScopeSettings::parse(&contents),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            publish_default_rsid_scope_settings(path, || {})?;
            RsidScopeSettings::parse(&std::fs::read_to_string(path)?)
        }
        Err(error) => Err(color_eyre::eyre::eyre!(
            "cannot read rsid scope settings at {}: {error}",
            path.display()
        )),
    }
}

#[cfg(target_os = "linux")]
fn publish_default_rsid_scope_settings(
    path: &Path,
    before_publish: impl FnOnce(),
) -> color_eyre::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let temp_path =
        path.with_file_name(format!(".rsid-scope-{}.tmp", uuid::Uuid::new_v4().simple()));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp_path)?;
        file.write_all(RsidScopeSettings::defaults().to_env().as_bytes())?;
        file.sync_all()?;
        before_publish();
        // hard_link creates the destination atomically and never replaces an
        // operator-updated file or another launcher's completed snapshot.
        match std::fs::hard_link(&temp_path, path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
            Err(error) => Err(error),
        }
    })();
    let _ = std::fs::remove_file(&temp_path);
    result.map_err(Into::into)
}

fn wait_for_daemon_socket(
    socket_path: &Path,
    child: &mut std::process::Child,
) -> color_eyre::Result<bool> {
    let mut exited = None;
    for _ in 0..100 {
        if std::os::unix::net::UnixStream::connect(socket_path).is_ok() {
            return Ok(exited.is_none() && child.try_wait()?.is_none());
        }
        if exited.is_none() {
            exited = child.try_wait()?;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    if let Some(status) = exited {
        return Err(color_eyre::eyre::eyre!(
            "rsid launcher exited before its socket was available ({status}); see daemon.log"
        ));
    }
    Err(color_eyre::eyre::eyre!(
        "rsid did not become available at {} within 5 seconds; see daemon.log",
        socket_path.display()
    ))
}

#[cfg(target_os = "linux")]
fn verify_rsid_scope(settings: RsidScopeSettings, unit: &str) -> color_eyre::Result<()> {
    let output = Command::new("systemctl")
        .args([
            "--user",
            "show",
            unit,
            "--property=ActiveState",
            "--property=ControlGroup",
            "--property=MemoryHigh",
            "--property=MemoryMax",
            "--property=MemorySwapMax",
            "--property=CPUWeight",
        ])
        .output()
        .map_err(|error| color_eyre::eyre::eyre!("cannot inspect rsid systemd scope: {error}"))?;
    if !output.status.success() {
        return Err(color_eyre::eyre::eyre!(
            "cannot inspect effective limits for systemd scope {unit}"
        ));
    }
    verify_rsid_scope_properties(settings, unit, &String::from_utf8_lossy(&output.stdout))
}

#[cfg(target_os = "linux")]
fn verify_rsid_scope_properties(
    settings: RsidScopeSettings,
    unit: &str,
    text: &str,
) -> color_eyre::Result<()> {
    let properties: std::collections::HashMap<&str, &str> = text
        .lines()
        .filter_map(|line| line.split_once('='))
        .collect();
    let control_group = properties.get("ControlGroup").copied().unwrap_or_default();
    let expected_memory_high = settings.memory_high_mib * 1024 * 1024;
    let expected_memory_max = settings.memory_max_mib * 1024 * 1024;
    let expected_memory_swap_max = settings.memory_swap_max_mib * 1024 * 1024;
    let matches = properties.get("ActiveState") == Some(&"active")
        && control_group.starts_with("/user.slice/")
        && control_group.ends_with(&format!("/{unit}"))
        && properties
            .get("MemoryHigh")
            .and_then(|value| value.parse::<u64>().ok())
            == Some(expected_memory_high)
        && properties
            .get("MemoryMax")
            .and_then(|value| value.parse::<u64>().ok())
            == Some(expected_memory_max)
        && properties
            .get("MemorySwapMax")
            .and_then(|value| value.parse::<u64>().ok())
            == Some(expected_memory_swap_max)
        && properties
            .get("CPUWeight")
            .and_then(|value| value.parse::<u32>().ok())
            == Some(settings.cpu_weight);
    if !matches {
        return Err(color_eyre::eyre::eyre!(
            "systemd scope {unit} is active without the configured effective limits or user-slice placement"
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn worker_slice_properties() -> color_eyre::Result<String> {
    let output = Command::new("systemctl")
        .args([
            "--user",
            "show",
            WORKER_SLICE,
            "--property=ActiveState",
            "--property=ControlGroup",
            "--property=MemoryHigh",
            "--property=MemoryMax",
            "--property=MemorySwapMax",
            "--property=CPUWeight",
        ])
        .output()?;
    if !output.status.success() {
        return Err(color_eyre::eyre::eyre!("cannot inspect {WORKER_SLICE}"));
    }
    Ok(String::from_utf8(output.stdout)?)
}

#[cfg(target_os = "linux")]
fn verify_worker_slice_properties(
    settings: WorkerScopeSettings,
    text: &str,
) -> color_eyre::Result<()> {
    let properties: std::collections::HashMap<&str, &str> = text
        .lines()
        .filter_map(|line| line.split_once('='))
        .collect();
    let group = properties.get("ControlGroup").copied().unwrap_or_default();
    let mib = 1024 * 1024;
    let matches = properties.get("ActiveState") == Some(&"active")
        && group.starts_with("/user.slice/")
        && group.ends_with("/rsi-workers.slice")
        && properties
            .get("MemoryHigh")
            .and_then(|v| v.parse::<u64>().ok())
            == Some(settings.memory_high_mib * mib)
        && properties
            .get("MemoryMax")
            .and_then(|v| v.parse::<u64>().ok())
            == Some(settings.memory_max_mib * mib)
        && properties
            .get("MemorySwapMax")
            .and_then(|v| v.parse::<u64>().ok())
            == Some(settings.memory_swap_max_mib * mib)
        && properties
            .get("CPUWeight")
            .and_then(|v| v.parse::<u32>().ok())
            == Some(settings.cpu_weight);
    if !matches {
        return Err(color_eyre::eyre::eyre!(
            "{WORKER_SLICE} is missing or has different aggregate limits; restart when existing workers have settled"
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn worker_slice_cgroup_path(properties: &str) -> Option<std::path::PathBuf> {
    let group = properties
        .lines()
        .find_map(|line| line.strip_prefix("ControlGroup="))?;
    let suffix = group.strip_prefix("/user.slice/user-")?;
    let (uid, rest) = suffix.split_once(".slice/user@")?;
    if uid.is_empty() || !uid.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    if rest != format!("{uid}.service/rsi.slice/rsi-workers.slice") {
        return None;
    }
    Some(std::path::Path::new("/sys/fs/cgroup").join(group.trim_start_matches('/')))
}

#[cfg(target_os = "linux")]
fn cgroup_counter(text: &str, name: &str) -> Option<u64> {
    let mut values = text.lines().filter_map(|line| {
        let (key, value) = line.split_once(' ')?;
        (key == name).then_some(value)
    });
    let value = values.next()?.parse().ok()?;
    values.next().is_none().then_some(value)
}

#[cfg(target_os = "linux")]
fn worker_slice_counters_are_empty(events: &str, stat: &str) -> bool {
    // populated includes descendant processes; nr_descendants also catches
    // active worker scopes which currently contain no process.
    cgroup_counter(events, "populated") == Some(0)
        && cgroup_counter(stat, "nr_descendants") == Some(0)
}

#[cfg(target_os = "linux")]
fn worker_slice_is_empty(properties: &str) -> bool {
    let Some(path) = worker_slice_cgroup_path(properties) else {
        return false;
    };
    let Ok(events) = std::fs::read_to_string(path.join("cgroup.events")) else {
        return false;
    };
    let Ok(stat) = std::fs::read_to_string(path.join("cgroup.stat")) else {
        return false;
    };
    worker_slice_counters_are_empty(&events, &stat)
}

#[cfg(target_os = "linux")]
fn provision_worker_slice(settings: WorkerScopeSettings) -> color_eyre::Result<()> {
    let before = worker_slice_properties()?;
    if before.lines().any(|line| line == "ActiveState=active") {
        if verify_worker_slice_properties(settings, &before).is_ok() {
            return Ok(());
        }
        // The daemon must boot to reap owned orphans if any worker scope
        // remains. Admission stays closed while the parent limits differ.
        if !worker_slice_is_empty(&before) {
            tracing::warn!(
                "active {WORKER_SLICE} differs from settings and may contain workers; daemon starts for recovery, worker admission remains closed"
            );
            return Ok(());
        }
        tracing::info!("reconciling empty {WORKER_SLICE} with configured limits");
    }
    let status = Command::new("systemctl")
        .args([
            "--user",
            "set-property",
            "--runtime",
            WORKER_SLICE,
            &format!("MemoryHigh={}M", settings.memory_high_mib),
            &format!("MemoryMax={}M", settings.memory_max_mib),
            &format!("MemorySwapMax={}M", settings.memory_swap_max_mib),
            &format!("CPUWeight={}", settings.cpu_weight),
        ])
        .status()?;
    if !status.success() {
        return Err(color_eyre::eyre::eyre!("cannot configure {WORKER_SLICE}"));
    }
    let status = Command::new("systemctl")
        .args(["--user", "start", WORKER_SLICE])
        .status()?;
    if !status.success() {
        return Err(color_eyre::eyre::eyre!("cannot start {WORKER_SLICE}"));
    }
    verify_worker_slice_properties(settings, &worker_slice_properties()?)
}

fn should_auto_start_daemon() -> bool {
    let value = std::env::var(ENV_TUI_NO_AUTO_START_DAEMON).ok();
    should_auto_start_daemon_with(value.as_deref())
}

fn should_auto_start_daemon_with(no_auto_start: Option<&str>) -> bool {
    !no_auto_start.is_some_and(is_truthy_env_value)
}

fn is_truthy_env_value(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

#[cfg(target_os = "macos")]
fn resolve_daemon_command() -> PathBuf {
    let current_exe = std::env::current_exe().ok();
    let path_daemon = which::which(rsi_common::identity::DAEMON_BINARY).ok();
    resolve_daemon_command_with(current_exe.as_deref(), path_daemon.as_deref())
}

fn resolve_daemon_command_with(current_exe: Option<&Path>, path_daemon: Option<&Path>) -> PathBuf {
    if let Some(exe) = current_exe
        && prefer_sibling_daemon(exe)
        && let Some(sibling) = sibling_daemon_binary(exe)
    {
        return sibling;
    }

    if let Some(path_rsid) = path_daemon {
        return path_rsid.to_path_buf();
    }

    current_exe
        .and_then(sibling_daemon_binary)
        .unwrap_or_else(|| PathBuf::from(rsi_common::identity::DAEMON_BINARY))
}

fn sibling_daemon_binary(current_exe: &Path) -> Option<PathBuf> {
    let sibling = current_exe.with_file_name(rsi_common::identity::DAEMON_BINARY);
    sibling.is_file().then_some(sibling)
}

fn prefer_sibling_daemon(current_exe: &Path) -> bool {
    let Some(parent) = current_exe.parent() else {
        return false;
    };

    match parent.file_name().and_then(|n| n.to_str()) {
        Some("debug") => true,
        Some("deps") => {
            parent
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                == Some("debug")
        }
        _ => false,
    }
}

fn rotate_log_file(current_path: &Path, log_dir: &Path) -> std::io::Result<()> {
    let timestamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let mut rotated_path = log_dir.join(format!("tui-{}.log", timestamp));
    let mut suffix = 1;

    while rotated_path.exists() {
        rotated_path = log_dir.join(format!("tui-{}-{}.log", timestamp, suffix));
        suffix += 1;
    }

    std::fs::rename(current_path, rotated_path)
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    use super::verify_worker_slice_properties;
    use super::{
        DaemonStart, RsidScopeSettings, WorkerScopeSettings, ensure_daemon_socket,
        prefer_sibling_daemon, resolve_daemon_command_with, should_auto_start_daemon_with,
        sibling_daemon_binary,
    };
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command};

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rsi-main-test-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("unix epoch")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn touch(path: &Path) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent dirs");
        }
        std::fs::write(path, b"").expect("write file");
    }

    #[test]
    #[ignore = "fixture entry point for isolated daemon auto-start tests"]
    fn daemon_socket_fixture() {
        let socket = PathBuf::from(std::env::var("RSI_TEST_AUTO_START_SOCKET").unwrap());
        if socket.exists() {
            std::fs::remove_file(&socket).unwrap();
        }
        let listener = std::os::unix::net::UnixListener::bind(socket).unwrap();
        for stream in listener.incoming() {
            drop(stream.unwrap());
        }
    }

    fn launch_daemon_fixture(socket: &Path) -> color_eyre::Result<Child> {
        Ok(Command::new(std::env::current_exe()?)
            .args(["--ignored", "--exact", "tests::daemon_socket_fixture"])
            .env("RSI_TEST_AUTO_START_SOCKET", socket)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?)
    }

    #[test]
    fn daemon_auto_start_handles_first_launch_live_socket_and_stale_relaunch() {
        // Unix socket paths are short (108 bytes on Linux); the sandbox TMPDIR
        // can already consume most of that limit.
        let dir = PathBuf::from(format!(
            "/tmp/rsi-auto-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir(&dir).unwrap();
        let socket = dir.join("daemon.sock");
        let DaemonStart::Launched(mut first) =
            ensure_daemon_socket(&socket, || launch_daemon_fixture(&socket)).unwrap()
        else {
            panic!("first launch starts daemon");
        };
        assert!(std::os::unix::net::UnixStream::connect(&socket).is_ok());

        let already_running = ensure_daemon_socket(&socket, || {
            panic!("live socket must not launch a second daemon")
        })
        .unwrap();
        assert!(matches!(already_running, DaemonStart::Existing));

        first.kill().unwrap();
        first.wait().unwrap();
        assert!(socket.exists(), "the killed daemon leaves a stale socket");
        let DaemonStart::Launched(mut second) =
            ensure_daemon_socket(&socket, || launch_daemon_fixture(&socket)).unwrap()
        else {
            panic!("stale socket triggers relaunch");
        };
        assert!(std::os::unix::net::UnixStream::connect(&socket).is_ok());
        second.kill().unwrap();
        second.wait().unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn daemon_auto_start_accepts_peer_after_own_launcher_loses_race() {
        use std::sync::{Arc, Barrier};

        let dir = PathBuf::from(format!(
            "/tmp/rsi-peer-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir(&dir).unwrap();
        let socket = dir.join("daemon.sock");
        let peer_socket = socket.clone();
        let barrier = Arc::new(Barrier::new(2));
        let peer_barrier = Arc::clone(&barrier);
        let peer = std::thread::spawn(move || {
            ensure_daemon_socket(&peer_socket, || {
                peer_barrier.wait();
                std::thread::sleep(std::time::Duration::from_millis(150));
                launch_daemon_fixture(&peer_socket)
            })
            .unwrap()
        });
        let outcome = ensure_daemon_socket(&socket, || {
            barrier.wait();
            Ok(Command::new("sh").args(["-c", "exit 7"]).spawn()?)
        })
        .unwrap();
        assert!(matches!(outcome, DaemonStart::PeerStarted));
        let DaemonStart::Launched(mut winner) = peer.join().unwrap() else {
            panic!("peer starts the daemon");
        };
        winner.kill().unwrap();
        winner.wait().unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn daemon_auto_start_reports_launcher_failure_without_a_peer() {
        let dir = PathBuf::from(format!(
            "/tmp/rsi-fail-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir(&dir).unwrap();
        let error = ensure_daemon_socket(&dir.join("daemon.sock"), || {
            Ok(Command::new("sh").args(["-c", "exit 7"]).spawn()?)
        })
        .err()
        .expect("missing peer must report launch failure");
        assert!(error.to_string().contains("exit status: 7"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn concurrent_first_launch_never_exposes_partial_scope_settings() {
        use std::sync::{Arc, Barrier};

        let dir = temp_dir("scope-first-launch");
        let path = dir.join("rsid-scope.env");
        let written = Arc::new(Barrier::new(2));
        let published = Arc::new(Barrier::new(2));
        let worker_path = path.clone();
        let worker_written = Arc::clone(&written);
        let worker_published = Arc::clone(&published);
        let writer = std::thread::spawn(move || {
            super::publish_default_rsid_scope_settings(&worker_path, || {
                worker_written.wait();
                worker_published.wait();
            })
            .unwrap();
        });
        written.wait();
        assert!(
            !path.exists(),
            "unpublished snapshot is invisible to readers"
        );
        let settings = super::read_or_initialize_rsid_scope_settings_at(&path).unwrap();
        assert_eq!(settings, RsidScopeSettings::defaults());
        published.wait();
        writer.join().unwrap();
        assert_eq!(
            RsidScopeSettings::parse(&std::fs::read_to_string(&path).unwrap()).unwrap(),
            settings
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn sibling_daemon_binary_prefers_neighboring_rsid() {
        let dir = temp_dir("sibling-present");
        let rsi = dir.join("rsi");
        let rsid = dir.join("rsid");
        touch(&rsi);
        touch(&rsid);

        let resolved = sibling_daemon_binary(&rsi).expect("sibling rsid should resolve");
        assert_eq!(resolved, rsid);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sibling_daemon_binary_returns_none_without_neighbor() {
        let dir = temp_dir("sibling-missing");
        let rsi = dir.join("rsi");
        touch(&rsi);

        assert!(sibling_daemon_binary(&rsi).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prefer_sibling_daemon_for_debug_builds_only() {
        let dir = temp_dir("prefer-sibling");
        let debug_rsi = dir.join("target/debug/rsi");
        let debug_deps_rsi = dir.join("target/debug/deps/rsi-hash");
        let release_rsi = dir.join("target/release/rsi");
        touch(&debug_rsi);
        touch(&debug_deps_rsi);
        touch(&release_rsi);

        assert!(prefer_sibling_daemon(&debug_rsi));
        assert!(prefer_sibling_daemon(&debug_deps_rsi));
        assert!(!prefer_sibling_daemon(&release_rsi));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_daemon_command_prefers_path_for_release_launches() {
        let dir = temp_dir("resolve-release");
        let release_rsi = dir.join("target/release/rsi");
        let sibling_rsid = dir.join("target/release/rsid");
        let path_rsid = dir.join("installed/rsid");
        touch(&release_rsi);
        touch(&sibling_rsid);
        touch(&path_rsid);

        let resolved = resolve_daemon_command_with(Some(&release_rsi), Some(&path_rsid));
        assert_eq!(resolved, path_rsid);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_daemon_command_prefers_sibling_for_debug_launches() {
        let dir = temp_dir("resolve-debug");
        let debug_rsi = dir.join("target/debug/rsi");
        let sibling_rsid = dir.join("target/debug/rsid");
        let path_rsid = dir.join("installed/rsid");
        touch(&debug_rsi);
        touch(&sibling_rsid);
        touch(&path_rsid);

        let resolved = resolve_daemon_command_with(Some(&debug_rsi), Some(&path_rsid));
        assert_eq!(resolved, sibling_rsid);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tui_auto_start_is_enabled_by_default() {
        assert!(should_auto_start_daemon_with(None));
        assert!(should_auto_start_daemon_with(Some("")));
        assert!(should_auto_start_daemon_with(Some("0")));
        assert!(should_auto_start_daemon_with(Some("false")));
        assert!(should_auto_start_daemon_with(Some("no")));
        assert!(should_auto_start_daemon_with(Some("off")));
    }

    #[test]
    fn tui_no_auto_start_env_disables_spawn_for_smoke() {
        assert!(!should_auto_start_daemon_with(Some("1")));
        assert!(!should_auto_start_daemon_with(Some("true")));
        assert!(!should_auto_start_daemon_with(Some("yes")));
        assert!(!should_auto_start_daemon_with(Some("on")));
    }

    #[test]
    fn tui_no_auto_start_invalid_values_do_not_disable_spawn() {
        assert!(should_auto_start_daemon_with(Some("maybe")));
        assert!(should_auto_start_daemon_with(Some("truthy")));
    }

    #[test]
    fn rsid_scope_settings_parse_validated_limits() {
        let settings = RsidScopeSettings::parse(
            "rsid_scope_memory_high_mib=6144\nrsid_scope_memory_max_mib=8192\nrsid_scope_memory_swap_max_mib=0\nrsid_scope_cpu_weight=20\n",
        )
        .unwrap();
        assert_eq!(settings.memory_high_mib, 6144);
        assert_eq!(settings.memory_max_mib, 8192);
        assert_eq!(settings.memory_swap_max_mib, 0);
        assert_eq!(settings.cpu_weight, 20);
        assert_eq!(settings.worker, WorkerScopeSettings::defaults());
        let explicit = settings
            .to_env()
            .replace(
                &format!(
                    "worker_scope_memory_high_mib={}",
                    settings.worker.memory_high_mib
                ),
                "worker_scope_memory_high_mib=512",
            )
            .replace(
                &format!(
                    "worker_scope_memory_max_mib={}",
                    settings.worker.memory_max_mib
                ),
                "worker_scope_memory_max_mib=1024",
            );
        let updated = RsidScopeSettings::parse(&explicit).unwrap();
        assert_eq!(updated.worker.memory_high_mib, 512);
        assert_eq!(updated.worker.memory_max_mib, 1024);
    }

    #[test]
    fn rsid_scope_settings_reject_incomplete_or_unsafe_limits() {
        let missing = "rsid_scope_memory_high_mib=6144\n";
        assert!(RsidScopeSettings::parse(missing).is_err());

        let invalid = "rsid_scope_memory_high_mib=8192\nrsid_scope_memory_max_mib=8192\nrsid_scope_memory_swap_max_mib=0\nrsid_scope_cpu_weight=20\n";
        assert!(RsidScopeSettings::parse(invalid).is_err());

        let malformed = "rsid_scope_memory_high_mib=6144\nrsid_scope_memory_max_mib=8192\nrsid_scope_memory_swap_max_mib=0\nrsid_scope_cpu_weight=10001\n";
        assert!(RsidScopeSettings::parse(malformed).is_err());
        assert!(
            RsidScopeSettings::parse(&format!("{malformed}worker_scope_memory_high_mib=6144\n"))
                .is_err()
        );
    }

    #[test]
    fn tui_launch_builds_bounded_systemd_user_scope_arguments() {
        let settings = RsidScopeSettings::parse(
            "rsid_scope_memory_high_mib=6144\nrsid_scope_memory_max_mib=8192\nrsid_scope_memory_swap_max_mib=0\nrsid_scope_cpu_weight=20\n",
        )
        .unwrap();
        let args =
            settings.systemd_run_args("rsid-tui-123.scope", &[PathBuf::from("/opt/rsi/rsid")]);
        assert_eq!(
            args,
            [
                "--user",
                "--scope",
                "--collect",
                "--unit=rsid-tui-123.scope",
                "--slice=user.slice",
                "--property=MemoryHigh=6144M",
                "--property=MemoryMax=8192M",
                "--property=MemorySwapMax=0M",
                "--property=CPUWeight=20",
                "--",
                "/opt/rsi/rsid",
            ]
        );
    }

    #[cfg(target_os = "linux")]
    fn make_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        touch(path);
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn tui_launch_runs_the_installed_supervisor_with_the_installed_rsid() {
        let data = temp_dir("launch-supervised");
        let install = data.join("install");
        make_executable(&install.join("rsid-supervisor.sh"));
        make_executable(&install.join("rsid"));
        // A sandbox or operator checkout next to the TUI must never be used.
        let release_rsi = data.join("checkout/target/release/rsi");
        make_executable(&release_rsi.with_file_name("rsid"));
        make_executable(&data.join("checkout/scripts/rsid-supervisor.sh"));

        let launch = super::plan_daemon_launch(Some(&release_rsi), None, &data);
        assert!(launch.supervised);
        assert_eq!(
            launch.argv,
            [install.join("rsid-supervisor.sh"), install.join("rsid")]
        );

        let settings = RsidScopeSettings::defaults();
        let args = settings.systemd_run_args("rsid-tui-1.scope", &launch.argv);
        let separator = args.iter().position(|arg| arg == "--").unwrap();
        assert_eq!(
            args[separator + 1..],
            [
                install.join("rsid-supervisor.sh").display().to_string(),
                install.join("rsid").display().to_string()
            ]
        );
        assert!(args.contains(&"--property=MemoryMax=8192M".to_string()));
        assert!(args.contains(&"--slice=user.slice".to_string()));

        let _ = std::fs::remove_dir_all(&data);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn tui_launch_falls_back_to_the_bare_daemon_without_an_installed_supervisor() {
        let data = temp_dir("launch-bare");
        let release_rsi = data.join("target/release/rsi");
        let path_rsid = data.join("bin/rsid");
        make_executable(&path_rsid);
        // The script exists but is not executable, and there is no installed rsid.
        touch(&data.join("install/rsid-supervisor.sh"));

        let launch = super::plan_daemon_launch(Some(&release_rsi), Some(&path_rsid), &data);
        assert!(!launch.supervised);
        assert_eq!(launch.argv, [path_rsid]);

        let _ = std::fs::remove_dir_all(&data);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn tui_dev_build_launch_never_uses_the_installed_supervisor() {
        let data = temp_dir("launch-dev");
        make_executable(&data.join("install/rsid-supervisor.sh"));
        make_executable(&data.join("install/rsid"));
        let debug_rsi = data.join("target/debug/rsi");
        let sibling = data.join("target/debug/rsid");
        make_executable(&sibling);

        let launch = super::plan_daemon_launch(Some(&debug_rsi), None, &data);
        assert!(!launch.supervised);
        assert_eq!(launch.argv, [sibling]);

        let _ = std::fs::remove_dir_all(&data);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn rsid_scope_verification_rejects_missing_or_ineffective_limits() {
        let settings = RsidScopeSettings::defaults();
        let unit = "rsid-tui-123.scope";
        let valid = format!(
            "ActiveState=active\nControlGroup=/user.slice/user-1000.slice/{unit}\nMemoryHigh={}\nMemoryMax={}\nMemorySwapMax={}\nCPUWeight={}\n",
            settings.memory_high_mib * 1024 * 1024,
            settings.memory_max_mib * 1024 * 1024,
            settings.memory_swap_max_mib * 1024 * 1024,
            settings.cpu_weight
        );
        assert!(super::verify_rsid_scope_properties(settings, unit, &valid).is_ok());

        let unbounded = valid.replace("MemoryMax=8589934592", "MemoryMax=infinity");
        assert!(super::verify_rsid_scope_properties(settings, unit, &unbounded).is_err());

        let wrong_slice = valid.replace("/user.slice/", "/app.slice/");
        assert!(super::verify_rsid_scope_properties(settings, unit, &wrong_slice).is_err());

        let wrong_unit = valid.replace(unit, "rsid.scope");
        assert!(super::verify_rsid_scope_properties(settings, unit, &wrong_unit).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn worker_slice_verification_requires_finite_exact_parent() {
        // The defaults derive from host memory (#1014), so build the expected
        // properties from them instead of pinning one host's numbers.
        let settings = WorkerScopeSettings::defaults();
        let mib = 1024 * 1024;
        let memory_max = format!("MemoryMax={}", settings.memory_max_mib * mib);
        let valid = format!(
            "ActiveState=active\nControlGroup=/user.slice/user-1000.slice/user@1000.service/rsi-workers.slice\nMemoryHigh={}\n{memory_max}\nMemorySwapMax={}\nCPUWeight={}\n",
            settings.memory_high_mib * mib,
            settings.memory_swap_max_mib * mib,
            settings.cpu_weight
        );
        assert!(verify_worker_slice_properties(settings, &valid).is_ok());
        assert!(
            verify_worker_slice_properties(
                settings,
                &valid.replace(&memory_max, "MemoryMax=infinity")
            )
            .is_err()
        );
        assert!(
            verify_worker_slice_properties(
                settings,
                &valid.replace("ActiveState=active", "ActiveState=inactive")
            )
            .is_err()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn worker_slice_reconciles_only_without_processes_or_child_scopes() {
        let properties = "ControlGroup=/user.slice/user-1000.slice/user@1000.service/rsi.slice/rsi-workers.slice\n";
        assert!(super::worker_slice_cgroup_path(properties).is_some());
        assert!(
            super::worker_slice_cgroup_path(&properties.replace("user@1000", "user@1001"))
                .is_none()
        );
        assert!(super::worker_slice_counters_are_empty(
            "populated 0\nfrozen 0\n",
            "nr_descendants 0\nnr_dying_descendants 0\n"
        ));
        assert!(!super::worker_slice_counters_are_empty(
            "populated 1\n",
            "nr_descendants 0\n"
        ));
        assert!(!super::worker_slice_counters_are_empty(
            "populated 0\n",
            "nr_descendants 1\n"
        ));
        assert!(!super::worker_slice_counters_are_empty("populated 0\n", ""));
        assert!(!super::worker_slice_counters_are_empty(
            "populated 0\npopulated 0\n",
            "nr_descendants 0\n"
        ));
    }
}
