use rsi::app::App;
use rsi::client::DaemonClient;
use std::path::{Path, PathBuf};
use std::process::Command;

const ENV_TUI_NO_AUTO_START_DAEMON: &str = "RSI_TUI_NO_AUTO_START_DAEMON";
const RSID_SCOPE_SETTINGS_FILE: &str = "rsid-scope.env";
const RSID_SCOPE_UNIT: &str = "rsid.scope";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RsidScopeSettings {
    memory_high_mib: u64,
    memory_max_mib: u64,
    memory_swap_max_mib: u64,
    cpu_weight: u32,
}

impl RsidScopeSettings {
    fn defaults() -> Self {
        Self {
            memory_high_mib: 6 * 1024,
            memory_max_mib: 8 * 1024,
            memory_swap_max_mib: 0,
            cpu_weight: 20,
        }
    }

    fn to_env(self) -> String {
        format!(
            "rsid_scope_memory_high_mib={}\nrsid_scope_memory_max_mib={}\nrsid_scope_memory_swap_max_mib={}\nrsid_scope_cpu_weight={}\n",
            self.memory_high_mib, self.memory_max_mib, self.memory_swap_max_mib, self.cpu_weight
        )
    }

    fn parse(contents: &str) -> color_eyre::Result<Self> {
        let mut memory_high_mib = None;
        let mut memory_max_mib = None;
        let mut memory_swap_max_mib = None;
        let mut cpu_weight = None;

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

        let settings = Self {
            memory_high_mib: memory_high_mib
                .ok_or_else(|| color_eyre::eyre::eyre!("missing rsid_scope_memory_high_mib"))?,
            memory_max_mib: memory_max_mib
                .ok_or_else(|| color_eyre::eyre::eyre!("missing rsid_scope_memory_max_mib"))?,
            memory_swap_max_mib: memory_swap_max_mib
                .ok_or_else(|| color_eyre::eyre::eyre!("missing rsid_scope_memory_swap_max_mib"))?,
            cpu_weight: cpu_weight
                .ok_or_else(|| color_eyre::eyre::eyre!("missing rsid_scope_cpu_weight"))?,
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
        Ok(())
    }

    fn systemd_run_args(self, daemon_cmd: &Path) -> Vec<String> {
        vec![
            "--user".to_string(),
            "--scope".to_string(),
            "--collect".to_string(),
            format!("--unit={RSID_SCOPE_UNIT}"),
            "--slice=user.slice".to_string(),
            format!("--property=MemoryHigh={}M", self.memory_high_mib),
            format!("--property=MemoryMax={}M", self.memory_max_mib),
            format!("--property=MemorySwapMax={}M", self.memory_swap_max_mib),
            format!("--property=CPUWeight={}", self.cpu_weight),
            "--".to_string(),
            daemon_cmd.display().to_string(),
        ]
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;

    // Initialize tracing to file (not stdout — that's the terminal)
    let log_dir = rsi_common::identity::data_dir();
    let _ = std::fs::create_dir_all(&log_dir);

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

    // Check if daemon is already running by testing socket connectivity
    if socket_path.exists() {
        // Socket exists — try connecting to verify daemon is alive
        if std::os::unix::net::UnixStream::connect(&socket_path).is_ok() {
            tracing::info!("daemon already running at {:?}", socket_path);
            return Ok(());
        } else {
            // Stale socket file — daemon will clean it up on start
            tracing::info!("stale socket found, daemon will clean up on start");
        }
    }

    tracing::info!("daemon not detected, attempting auto-start...");

    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;

        let scope = read_or_initialize_rsid_scope_settings()?;
        let daemon_cmd = resolve_daemon_command();
        let daemon_log_path = log_dir.join("daemon.log");
        let daemon_log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&daemon_log_path)?;
        let child = Command::new("systemd-run")
            .args(scope.systemd_run_args(&daemon_cmd))
            .stdin(std::process::Stdio::null())
            .stdout(daemon_log.try_clone()?)
            .stderr(daemon_log)
            .process_group(0)
            .spawn()
            .map_err(|error| {
                color_eyre::eyre::eyre!(
                    "failed to invoke systemd-run for rsid scope: {error}; see {}",
                    daemon_log_path.display()
                )
            })?;
        drop(child);
        if let Err(error) =
            wait_for_daemon_socket(&socket_path).and_then(|()| verify_rsid_scope(scope))
        {
            let _ = Command::new("systemctl")
                .args(["--user", "stop", RSID_SCOPE_UNIT])
                .status();
            return Err(error);
        }
        tracing::info!(
            daemon = %daemon_cmd.display(),
            unit = RSID_SCOPE_UNIT,
            "daemon started in its bounded systemd user scope; logs at {:?}",
            daemon_log_path
        );
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = log_dir;
        return Err(color_eyre::eyre::eyre!(
            "rsid auto-start requires Linux systemd user scopes"
        ));
    }

    Ok(())
}

#[cfg(target_os = "linux")]
fn read_or_initialize_rsid_scope_settings() -> color_eyre::Result<RsidScopeSettings> {
    let path = rsi_common::identity::data_path(RSID_SCOPE_SETTINGS_FILE, "rsid-scope");
    match std::fs::read_to_string(&path) {
        Ok(contents) => RsidScopeSettings::parse(&contents),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let defaults = RsidScopeSettings::defaults();
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut file) => {
                    use std::io::Write;
                    file.write_all(defaults.to_env().as_bytes())?;
                    file.sync_all()?;
                    Ok(defaults)
                }
                Err(create_error) if create_error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let contents = std::fs::read_to_string(&path)?;
                    RsidScopeSettings::parse(&contents)
                }
                Err(create_error) => Err(create_error.into()),
            }
        }
        Err(error) => Err(color_eyre::eyre::eyre!(
            "cannot read rsid scope settings at {}: {error}",
            path.display()
        )),
    }
}

#[cfg(target_os = "linux")]
fn wait_for_daemon_socket(socket_path: &Path) -> color_eyre::Result<()> {
    for _ in 0..100 {
        if std::os::unix::net::UnixStream::connect(socket_path).is_ok() {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    Err(color_eyre::eyre::eyre!(
        "rsid did not become available in systemd scope {RSID_SCOPE_UNIT}"
    ))
}

#[cfg(target_os = "linux")]
fn verify_rsid_scope(settings: RsidScopeSettings) -> color_eyre::Result<()> {
    let output = Command::new("systemctl")
        .args([
            "--user",
            "show",
            RSID_SCOPE_UNIT,
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
            "cannot inspect effective limits for systemd scope {RSID_SCOPE_UNIT}"
        ));
    }
    verify_rsid_scope_properties(settings, &String::from_utf8_lossy(&output.stdout))
}

#[cfg(target_os = "linux")]
fn verify_rsid_scope_properties(settings: RsidScopeSettings, text: &str) -> color_eyre::Result<()> {
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
        && control_group.ends_with("/rsid.scope")
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
            "systemd scope {RSID_SCOPE_UNIT} is active without the configured effective limits or user-slice placement"
        ));
    }
    Ok(())
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
    use super::{
        RsidScopeSettings, prefer_sibling_daemon, resolve_daemon_command_with,
        should_auto_start_daemon_with, sibling_daemon_binary,
    };
    use std::path::{Path, PathBuf};

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
    }

    #[test]
    fn rsid_scope_settings_reject_incomplete_or_unsafe_limits() {
        let missing = "rsid_scope_memory_high_mib=6144\n";
        assert!(RsidScopeSettings::parse(missing).is_err());

        let invalid = "rsid_scope_memory_high_mib=8192\nrsid_scope_memory_max_mib=8192\nrsid_scope_memory_swap_max_mib=0\nrsid_scope_cpu_weight=20\n";
        assert!(RsidScopeSettings::parse(invalid).is_err());

        let malformed = "rsid_scope_memory_high_mib=6144\nrsid_scope_memory_max_mib=8192\nrsid_scope_memory_swap_max_mib=0\nrsid_scope_cpu_weight=10001\n";
        assert!(RsidScopeSettings::parse(malformed).is_err());
    }

    #[test]
    fn tui_launch_builds_bounded_systemd_user_scope_arguments() {
        let settings = RsidScopeSettings::parse(
            "rsid_scope_memory_high_mib=6144\nrsid_scope_memory_max_mib=8192\nrsid_scope_memory_swap_max_mib=0\nrsid_scope_cpu_weight=20\n",
        )
        .unwrap();
        let args = settings.systemd_run_args(Path::new("/opt/rsi/rsid"));
        assert_eq!(
            args,
            [
                "--user",
                "--scope",
                "--collect",
                "--unit=rsid.scope",
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
    #[test]
    fn rsid_scope_verification_rejects_missing_or_ineffective_limits() {
        let settings = RsidScopeSettings::defaults();
        let valid = format!(
            "ActiveState=active\nControlGroup=/user.slice/user-1000.slice/rsid.scope\nMemoryHigh={}\nMemoryMax={}\nMemorySwapMax={}\nCPUWeight={}\n",
            settings.memory_high_mib * 1024 * 1024,
            settings.memory_max_mib * 1024 * 1024,
            settings.memory_swap_max_mib * 1024 * 1024,
            settings.cpu_weight
        );
        assert!(super::verify_rsid_scope_properties(settings, &valid).is_ok());

        let unbounded = valid.replace("MemoryMax=8589934592", "MemoryMax=infinity");
        assert!(super::verify_rsid_scope_properties(settings, &unbounded).is_err());

        let wrong_slice = valid.replace("/user.slice/", "/app.slice/");
        assert!(super::verify_rsid_scope_properties(settings, &wrong_slice).is_err());
    }
}
