use rsi::client::DaemonClient;
use rsi_common::issue_workspace::CreateIssueV2RequestV1;
use rsi_common::types::{SandboxKind, SandboxSpec, SessionKind, SessionProvider, SessionStatus};
use std::{
    collections::HashSet,
    error::Error,
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
use termwright::prelude::*;

#[path = "e2e_tui/manager_presets.rs"]
mod manager_presets;
#[path = "e2e_tui/manager_surface.rs"]
mod manager_surface;

type E2eResult<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

async fn select_settings_row(term: &Terminal, target: &str, max_steps: usize) -> E2eResult<()> {
    let mut observed = HashSet::new();
    let target_marker = format!("▌ {target}");
    for step in 0..=max_steps {
        let screen = term.screen().await.text();
        let marker = screen
            .lines()
            .find_map(|line| {
                line.find("▌ ").map(|index| {
                    line[index..]
                        .split("  ")
                        .next()
                        .expect("selected marker has a first column")
                        .trim_end()
                        .to_string()
                })
            })
            .ok_or_else(|| {
                test_error(format!(
                    "settings target {target:?} missing selected row at step {step}; screen:\n{screen}"
                ))
            })?;
        if marker == target_marker {
            return Ok(());
        }
        if !observed.insert(marker.clone()) {
            return Err(test_error(format!(
                "settings target {target:?} repeated selected row {marker:?} at step {step}; screen:\n{screen}"
            )));
        }
        if step == max_steps {
            return Err(test_error(format!(
                "settings target {target:?} exhausted {max_steps} steps; last selected {marker:?}; screen:\n{screen}"
            )));
        }
        let (selected_index, total_items) = screen
            .split_whitespace()
            .find_map(|token| {
                let (selected, total) = token.split_once('/')?;
                Some((selected.parse::<usize>().ok()?, total.parse::<usize>().ok()?))
            })
            .ok_or_else(|| {
                test_error(format!(
                    "settings target {target:?} missing focus counter at step {step}; screen:\n{screen}"
                ))
            })?;
        if selected_index >= total_items {
            return Err(test_error(format!(
                "settings target {target:?} is unreachable below final row {selected_index}/{total_items}; screen:\n{screen}"
            )));
        }
        term.send_key(Key::Char('j'))
            .await
            .map_err(|error| test_error(format!("navigate to {target:?}: {error:?}")))?;
        term.expect_gone(&marker)
            .timeout(Duration::from_secs(5))
            .await
            .map_err(|error| {
                test_error(format!(
                    "settings target {target:?} did not advance from {marker:?}: {error:?}"
                ))
            })?;
        let next_focus = format!("{}/{total_items}", selected_index + 1);
        term.expect(&next_focus)
            .timeout(Duration::from_secs(5))
            .await
            .map_err(|error| {
                test_error(format!(
                    "settings target {target:?} did not reach focus {next_focus} after {marker:?}: {error:?}"
                ))
            })?;
    }
    unreachable!("bounded settings traversal always returns")
}

const GROUP_NAME: &str = "E2E Group";
const EPIC_NAME: &str = "E2E Epic";
const PROJECT_NAME: &str = "E2E Project";
const ISSUE_TITLE: &str = "E2E issue workspace identity";
const TAG: &str = "e2e";
const PTY_COLS: u16 = 100;
const PTY_ROWS: u16 = 36;
const REQUIRED_ARTIFACTS: &[&str] = &[
    "rsid.log",
    "tui.log",
    "screen.txt",
    "env.txt",
    "fixture.json",
    "scenario.txt",
    "timings.txt",
];

/// Advisory-only latency thresholds for keypress->visible-state transitions.
/// These NEVER gate the test: a breach emits a warning line and is recorded in
/// the artifacts, but the scenario result is unaffected by timing alone.
const TIMING_WARN_MS: u128 = 500;
const TIMING_ADVISORY_FAIL_MS: u128 = 1500;

/// One measured keypress->visible-state transition in the TUI scenario.
struct TransitionTiming {
    label: &'static str,
    elapsed_ms: u128,
    /// Advisory breach level: `Some("warn")`, `Some("advisory-fail")`, or `None`.
    breach: Option<&'static str>,
}

/// Classify a transition's latency against the advisory thresholds.
/// Advisory only — the returned level is never used to fail the test.
fn classify_breach(elapsed_ms: u128) -> Option<&'static str> {
    if elapsed_ms >= TIMING_ADVISORY_FAIL_MS {
        Some("advisory-fail")
    } else if elapsed_ms >= TIMING_WARN_MS {
        Some("warn")
    } else {
        None
    }
}

/// Record a transition timing, emitting an advisory line. Pure bookkeeping:
/// this can never return an error or otherwise gate the scenario.
fn record_transition(timings: &mut Vec<TransitionTiming>, label: &'static str, elapsed: Duration) {
    let elapsed_ms = elapsed.as_millis();
    let breach = classify_breach(elapsed_ms);
    match breach {
        Some(level) => eprintln!(
            "[e2e-timing][{level}] transition '{label}' took {elapsed_ms} ms \
             (advisory only, not a gate)"
        ),
        None => println!("[e2e-timing] transition '{label}' took {elapsed_ms} ms"),
    }
    timings.push(TransitionTiming {
        label,
        elapsed_ms,
        breach,
    });
}

struct DaemonProcess {
    child: Child,
}

impl DaemonProcess {
    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        self.child.try_wait()
    }
}

impl Drop for DaemonProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Default)]
struct FixtureState {
    project_id: Option<uuid::Uuid>,
    group_id: Option<uuid::Uuid>,
    epic_id: Option<uuid::Uuid>,
    issue_id: Option<uuid::Uuid>,
    issue_number: Option<i64>,
}

struct E2eHarness {
    _temp_dir: tempfile::TempDir,
    _sandbox_temp_dir: tempfile::TempDir,
    home_dir: PathBuf,
    socket_path: PathBuf,
    sandbox_base_dir: PathBuf,
    bin_dir: PathBuf,
    artifacts_dir: PathBuf,
    isolated_path: String,
    real_git: PathBuf,
    git_hold: PathBuf,
    git_entered: PathBuf,
    git_release: PathBuf,
    rsi_bin: String,
    rsid_bin_path: PathBuf,
    daemon: Option<DaemonProcess>,
    fixture: FixtureState,
    phase: &'static str,
    final_screen_text: Option<String>,
    transitions: Vec<TransitionTiming>,
    eligible_target: Option<PathBuf>,
    /// When true, the scenario injects a deliberately-wrong expectation so the
    /// assertions provably go red. Used only by the double-gated induced test.
    inject_failure: bool,
}

#[tokio::test]
async fn test_e2e_tui() -> E2eResult<()> {
    if std::env::var("RSI_E2E").unwrap_or_default() != "1" {
        return Ok(());
    }

    let mut harness = E2eHarness::new()?;
    let result = harness.run().await;

    harness.shutdown_daemon();

    let failure = result.as_ref().err().map(|error| error.to_string());
    let keep_artifacts = std::env::var("RSI_E2E_KEEP_ARTIFACTS").unwrap_or_default() == "1";
    let artifact_result = if result.is_err() || keep_artifacts {
        Some(harness.preserve_artifacts(failure.as_deref()))
    } else {
        None
    };

    match (result, artifact_result) {
        (Ok(()), Some(Err(error))) => Err(error),
        (Ok(()), _) => Ok(()),
        (Err(error), Some(Err(artifact_error))) => Err(test_error(format!(
            "{error}; additionally failed to preserve E2E artifacts: {artifact_error}"
        ))),
        (Err(error), _) => Err(error),
    }
}

/// Red-proof companion to `test_e2e_tui`. Drives the SAME harness but injects a
/// deliberately-wrong expectation, then propagates the resulting `Err` so the
/// test exits non-zero — proving the scenario assertions genuinely go red.
///
/// Double-gated so it can NEVER run in default `cargo test`, in `cargo test
/// --workspace`, or under plain `RSI_E2E=1 cargo test` (the normal gate stays
/// green): it is `#[ignore]` AND requires `RSI_E2E_INDUCED=1`. It fails red only
/// when explicitly invoked with `RSI_E2E=1 RSI_E2E_INDUCED=1 ... -- --ignored`.
#[tokio::test]
#[ignore = "red-proof induced failure; run explicitly with RSI_E2E_INDUCED=1 -- --ignored"]
async fn test_e2e_tui_induced_failure() -> E2eResult<()> {
    if std::env::var("RSI_E2E").unwrap_or_default() != "1"
        || std::env::var("RSI_E2E_INDUCED").unwrap_or_default() != "1"
    {
        return Ok(());
    }

    let mut harness = E2eHarness::new()?;
    harness.inject_failure = true;
    let result = harness.run().await;

    harness.shutdown_daemon();

    match result {
        Ok(()) => Err(test_error(
            "induced-failure scenario unexpectedly passed; red-proof assertion did not trigger",
        )),
        // Propagate the induced error so the test reports red, demonstrating the
        // harness assertions fail as expected when an expectation is wrong.
        Err(error) => Err(test_error(format!(
            "induced failure observed as expected (test is red by design): {error}"
        ))),
    }
}

/// ST-NEWSESSION-UNIFY Termwright proof. Drives the create-entity modal to
/// completion in a real TUI + daemon and asserts (via the daemon) that the
/// session it creates carries the launch fields the unified option builder
/// resolves (kind = the chosen leaf, provider = the unified default).
///
/// The modal is the keystroke-reachable session-creation entry point. The
/// input-bar quick-new path (`App::launch_session`) shares the SAME
/// `App::launch_session_with_options` dispatch + `LaunchOptions` builder; that
/// both entry points produce byte-identical options for equivalent inputs is
/// pinned by the deterministic unit test
/// `overlay::create_entity_form::tests::input_bar_and_modal_build_identical_launch_options`
/// (the legacy bottom input bar has no robust standalone keybinding to drive
/// here, so the unit test is its equivalent, stronger parity proof).
///
/// Gated behind `RSI_E2E=1` like the other E2E targets; a no-op otherwise.
#[tokio::test]
async fn test_e2e_newsession_modal_unified() -> E2eResult<()> {
    if std::env::var("RSI_E2E").unwrap_or_default() != "1" {
        return Ok(());
    }

    let mut harness = E2eHarness::new()?;
    let result = harness.run_newsession_modal_unified().await;
    harness.shutdown_daemon();
    result
}

/// Issue 69A operator-path proof. This uses the ordinary rsid binary, but its
/// HOME, socket, database, sandbox base, and staged recovery entry all live in
/// this harness's TempDir.
#[tokio::test]
async fn test_e2e_sandbox_storage_settings() -> E2eResult<()> {
    if std::env::var("RSI_E2E").unwrap_or_default() != "1" {
        return Ok(());
    }

    let mut harness = E2eHarness::new()?;
    let result = harness.run_sandbox_storage_settings().await;
    harness.shutdown_daemon();
    result
}

/// A daemon socket that accepts the transport but never answers proves the
/// first frame and input path do not depend on any startup RPC completing.
#[tokio::test]
async fn test_e2e_startup_readiness_withheld_rpc() -> E2eResult<()> {
    if std::env::var("RSI_E2E").unwrap_or_default() != "1" {
        return Ok(());
    }

    let temp_dir = tempfile::Builder::new()
        .prefix("rsi-e2e-startup-held-")
        .tempdir()?;
    let home_dir = temp_dir.path().join("home");
    let dot_rsi = home_dir.join(".rsi");
    fs::create_dir_all(&dot_rsi)?;
    let socket_path = dot_rsi.join("daemon.sock");
    let listener = tokio::net::UnixListener::bind(&socket_path)?;
    let held_server = tokio::spawn(async move {
        if let Ok((stream, _)) = listener.accept().await {
            let _held_stream = stream;
            std::future::pending::<()>().await;
        }
    });

    let rsi_bin = env!("CARGO_BIN_EXE_rsi");
    let spawn_started = Instant::now();
    let term = Terminal::builder()
        .size(100, 36)
        .timeout(Duration::from_secs(5))
        .env("HOME", home_dir.to_string_lossy())
        .env("RSI_DAEMON_SOCKET_PATH", socket_path.to_string_lossy())
        .env("RSI_TUI_NO_AUTO_START_DAEMON", "1")
        .env("TERM", "xterm-256color")
        .env("LANG", "C.UTF-8")
        .env("TZ", "UTC")
        .spawn(rsi_bin, &[])
        .await
        .map_err(|error| test_error(format!("spawn withheld-RPC TUI: {error:?}")))?;

    let scenario = async {
        term.expect("Connecting to daemon")
            .timeout(Duration::from_secs(5))
            .await
            .map_err(|error| test_error(format!("positive bootstrap frame missing: {error:?}")))?;
        println!(
            "[startup-readiness] withheld first frame: {} ms",
            spawn_started.elapsed().as_millis()
        );

        let input_started = Instant::now();
        term.send_key(Key::Char('?'))
            .await
            .map_err(|error| test_error(format!("help while RPC held: {error:?}")))?;
        term.expect("Context Help — Session List")
            .timeout(Duration::from_secs(5))
            .await
            .map_err(|error| test_error(format!("help while RPC held missing: {error:?}")))?;
        println!(
            "[startup-readiness] withheld input response: {} ms",
            input_started.elapsed().as_millis()
        );
        term.send_key(Key::Char('?'))
            .await
            .map_err(|error| test_error(format!("close held-RPC help: {error:?}")))?;
        term.expect_gone("Context Help — Session List")
            .timeout(Duration::from_secs(5))
            .await
            .map_err(|error| test_error(format!("held-RPC help did not close: {error:?}")))?;

        term.send_key(Key::Char(' '))
            .await
            .map_err(|error| test_error(format!("blank-prompt leader: {error:?}")))?;
        term.send_key(Key::Char('m'))
            .await
            .map_err(|error| test_error(format!("open blank prompt: {error:?}")))?;
        term.type_str("startup draft stays here")
            .await
            .map_err(|error| test_error(format!("type held-RPC draft: {error:?}")))?;
        term.expect("startup draft stays here")
            .timeout(Duration::from_secs(5))
            .await
            .map_err(|error| test_error(format!("held-RPC draft missing: {error:?}")))?;
        term.send_key(Key::Ctrl('t'))
            .await
            .map_err(|error| test_error(format!("submit held-RPC draft: {error:?}")))?;
        term.expect("Daemon configuration is still loading")
            .timeout(Duration::from_secs(5))
            .await
            .map_err(|error| test_error(format!("config-pending refusal missing: {error:?}")))?;
        term.expect("startup draft stays here")
            .timeout(Duration::from_secs(5))
            .await
            .map_err(|error| test_error(format!("refused draft was not preserved: {error:?}")))?;

        term.resize(120, 40)
            .await
            .map_err(|error| test_error(format!("resize while RPC held: {error:?}")))?;
        term.expect("startup draft stays here")
            .timeout(Duration::from_secs(5))
            .await
            .map_err(|error| {
                test_error(format!("draft after held-RPC resize missing: {error:?}"))
            })?;

        term.send_key(Key::Ctrl('c'))
            .await
            .map_err(|error| test_error(format!("quit held-RPC TUI: {error:?}")))?;
        term.wait_exit()
            .await
            .map_err(|error| test_error(format!("held-RPC TUI did not quit cleanly: {error:?}")))?;
        Ok(())
    }
    .await;

    if scenario.is_err() {
        eprintln!(
            "[startup-readiness-held] final screen:\n{}",
            term.screen().await.text()
        );
    }
    let _ = term.kill().await;
    held_server.abort();
    scenario
}

/// A real isolated daemon with Git discovery held proves sessions and input
/// settle independently of the fresh storage preview.
#[tokio::test]
async fn test_e2e_startup_readiness_delayed_preview() -> E2eResult<()> {
    if std::env::var("RSI_E2E").unwrap_or_default() != "1" {
        return Ok(());
    }

    let mut harness = E2eHarness::new()?;
    let result = harness.run_startup_readiness_delayed_preview().await;
    harness.release_git_hold()?;
    harness.shutdown_daemon();
    result
}

/// Deterministic capability proof: default and configured Codex launches use a
/// stubbed catalog/token stream across the real daemon/RPC boundary, preserve
/// raw configuration through restart, and render the configured runtime
/// denominator through the F3 detail panel.
#[tokio::test]
async fn test_e2e_codex_capability_display() -> E2eResult<()> {
    if std::env::var("RSI_E2E").unwrap_or_default() != "1" {
        return Ok(());
    }

    let mut harness = E2eHarness::new()?;
    let result = harness.run_codex_capability_display().await;
    harness.shutdown_daemon();
    result
}

impl E2eHarness {
    fn new() -> E2eResult<Self> {
        let temp_dir = tempfile::Builder::new().prefix("rsi-e2e-tui-").tempdir()?;
        let temp_path = temp_dir.path();

        let home_dir = temp_path.join("home");
        let dot_rsi = home_dir.join(".rsi");
        fs::create_dir_all(&dot_rsi)?;

        let socket_path = dot_rsi.join("daemon.sock");
        // Keep the short-lived socket in /tmp, but put sandbox storage on the
        // repository filesystem. The storage scenario intentionally drives
        // pressure-mode reclaim; an almost-empty tmpfs would make that branch
        // host-dependent even with the minimum valid pressure thresholds.
        let sandbox_temp_dir = tempfile::Builder::new()
            .prefix(".rsi-e2e-sandboxes-")
            .tempdir_in(std::env::current_dir()?)?;
        let sandbox_base_dir = sandbox_temp_dir.path().to_path_buf();

        let bin_dir = temp_path.join("bin");
        fs::create_dir_all(&bin_dir)?;

        let artifacts_dir = temp_path.join("artifacts");
        fs::create_dir_all(&artifacts_dir)?;
        fs::File::create(artifacts_dir.join("rsid.log"))?;

        install_provider_stubs(&bin_dir)?;
        let real_git = find_executable("git")?;
        install_git_hold_wrapper(&bin_dir)?;
        let git_hold = temp_path.join("hold-git-worktree-list");
        let git_entered = temp_path.join("git-worktree-list-entered");
        let git_release = temp_path.join("release-git-worktree-list");

        let rsi_bin = env!("CARGO_BIN_EXE_rsi").to_string();
        let rsi_bin_path = Path::new(&rsi_bin);
        let rsid_bin_path = rsi_bin_path
            .parent()
            .ok_or_else(|| test_error("rsi binary path has no parent"))?
            .join("rsid");

        let original_path = std::env::var("PATH").unwrap_or_default();
        let isolated_path = format!("{}:{}", bin_dir.display(), original_path);

        Ok(Self {
            _temp_dir: temp_dir,
            _sandbox_temp_dir: sandbox_temp_dir,
            home_dir,
            socket_path,
            sandbox_base_dir,
            bin_dir,
            artifacts_dir,
            isolated_path,
            real_git,
            git_hold,
            git_entered,
            git_release,
            rsi_bin,
            rsid_bin_path,
            daemon: None,
            fixture: FixtureState::default(),
            phase: "setup",
            final_screen_text: None,
            transitions: Vec::new(),
            eligible_target: None,
            inject_failure: false,
        })
    }

    async fn run(&mut self) -> E2eResult<()> {
        self.phase = "daemon-startup";
        self.start_daemon()?;

        self.phase = "daemon-readiness";
        let mut client = self.wait_for_daemon().await?;

        self.phase = "fixture-seeding";
        let fixture_result = self.seed_fixture(&mut client).await;
        client.disconnect();
        fixture_result?;

        self.phase = "tui-scenario";
        self.run_tui_scenario().await
    }

    fn start_daemon(&mut self) -> E2eResult<()> {
        let rsid_log_file = fs::File::create(self.artifacts_dir.join("rsid.log"))?;

        let mut cmd = Command::new(&self.rsid_bin_path);
        cmd.env_remove("RSI_SESSION_TOKEN")
            .env_remove("RSI_SESSION_ID")
            .env("HOME", &self.home_dir)
            .env("CODEX_HOME", self.home_dir.join(".codex"))
            .env("RSI_DAEMON_SOCKET_PATH", &self.socket_path)
            .env("RSI_SANDBOX_BASE", &self.sandbox_base_dir)
            .env("PATH", &self.isolated_path)
            .env("RSI_E2E_REAL_GIT", &self.real_git)
            .env("RSI_E2E_GIT_HOLD", &self.git_hold)
            .env("RSI_E2E_GIT_ENTERED", &self.git_entered)
            .env("RSI_E2E_GIT_RELEASE", &self.git_release)
            .env("RSI_MEMORY_ENABLED", "false")
            .env("RSI_DREAM_ENABLED", "false")
            .env("RSI_QUEUE_ENABLED", "false")
            .env("RSI_RECONCILIATION_ENABLED", "false")
            .env("RSI_STALL_DETECTION_ENABLED", "false")
            .env("RSI_SMOKE_SUPPRESS_RETRY_RESTORE", "true")
            .env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor")
            .env("LANG", "C.UTF-8")
            .env("TZ", "UTC")
            // Isolate optional startup discovery/warmup from the operator's
            // local model server as well as the provider CLI fixtures.
            .env("LOCAL_LLM_BASE_URL", "http://127.0.0.1:0/v1")
            .env("RSI_OLLAMA_URL", "http://127.0.0.1:0/api/generate")
            .stdout(Stdio::from(rsid_log_file.try_clone()?))
            .stderr(Stdio::from(rsid_log_file));

        let child = cmd
            .spawn()
            .map_err(|error| test_error(format!("failed to spawn rsid: {error}")))?;
        self.daemon = Some(DaemonProcess { child });
        Ok(())
    }

    async fn wait_for_daemon(&mut self) -> E2eResult<DaemonClient> {
        let mut client = DaemonClient::new(self.socket_path.clone());

        for _ in 0..50 {
            if let Some(daemon) = self.daemon.as_mut() {
                if let Some(status) = daemon.try_wait()? {
                    return Err(test_error(format!(
                        "rsid exited before RPC readiness with status {status}"
                    )));
                }
            }

            match client.connect().await {
                Ok(_) => match client.get_health_status().await {
                    Ok(_) => return Ok(client),
                    Err(_) => client.disconnect(),
                },
                Err(_) => {}
            }

            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        Err(test_error("rsid did not become ready within 5s"))
    }

    async fn seed_fixture(&mut self, client: &mut DaemonClient) -> E2eResult<()> {
        let project = client
            .create_project(
                PROJECT_NAME,
                None,
                Some("Issue workspace E2E fixture"),
                None,
            )
            .await
            .map_err(|error| test_error(format!("failed to seed project: {error}")))?;
        self.fixture.project_id = Some(project.id);

        let group_id = client
            .create_container(
                SessionKind::Group,
                GROUP_NAME,
                None,
                None,
                &[TAG.into()],
                None,
            )
            .await
            .map_err(|error| test_error(format!("failed to seed group container: {error}")))?;
        self.fixture.group_id = Some(group_id);

        let epic_id = client
            .create_container(
                SessionKind::Epic,
                EPIC_NAME,
                Some(group_id),
                None,
                &[TAG.into()],
                None,
            )
            .await
            .map_err(|error| test_error(format!("failed to seed epic container: {error}")))?;
        self.fixture.epic_id = Some(epic_id);

        client
            .update_session_project(group_id, Some(project.id))
            .await
            .map_err(|error| test_error(format!("failed to assign group project: {error}")))?;
        client
            .update_session_project(epic_id, Some(project.id))
            .await
            .map_err(|error| test_error(format!("failed to assign epic project: {error}")))?;

        let issue = client
            .create_issue_v2(CreateIssueV2RequestV1 {
                project_id: project.id,
                title: ISSUE_TITLE.to_string(),
                body: "Open, copy, cancel, and reopen this exact Issue".to_string(),
                priority: Some(2),
                labels: vec![TAG.to_string()],
                assignee: Some("operator".to_string()),
                idempotency_key: "e2e-issue-workspace-fixture-v1".to_string(),
            })
            .await
            .map_err(|error| test_error(format!("failed to seed Issue workspace row: {error}")))?
            .issue;
        self.fixture.issue_id = Some(issue.id);
        self.fixture.issue_number = Some(issue.display_number);

        Ok(())
    }

    async fn run_tui_scenario(&mut self) -> E2eResult<()> {
        let home_str = self.path_string(&self.home_dir)?;
        let socket_str = self.path_string(&self.socket_path)?;

        let term = Terminal::builder()
            .size(PTY_COLS, PTY_ROWS)
            .env("HOME", &home_str)
            .env("RSI_DAEMON_SOCKET_PATH", &socket_str)
            .env("RSI_TUI_NO_AUTO_START_DAEMON", "1")
            .env("PATH", &self.isolated_path)
            .env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor")
            .env("LANG", "C.UTF-8")
            .env("TZ", "UTC")
            .spawn(&self.rsi_bin, &[])
            .await
            .map_err(|error| test_error(format!("failed to spawn rsi: {error:?}")))?;

        let inject_failure = self.inject_failure;
        let mut timings: Vec<TransitionTiming> = Vec::new();

        let scenario_result = async {
            term.expect("Sessions")
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| {
                    test_error(format!(
                        "timed out waiting for root session list: {error:?}"
                    ))
                })?;

            term.expect(GROUP_NAME)
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| {
                    test_error(format!(
                        "timed out waiting for {GROUP_NAME} on root: {error:?}"
                    ))
                })?;

            // Slice 1 action-discovery/theme proof. The normal list owns the
            // reclaimed row; only transient input/feedback brings it back.
            let normal_screen = term.screen().await.text();
            if normal_screen.contains("j/k move") {
                return Err(test_error(format!(
                    "normal Session List still renders its persistent hint footer:\n{normal_screen}"
                )));
            }

            term.send_key(Key::Char('/'))
                .await
                .map_err(|error| test_error(format!("enter search: {error:?}")))?;
            for ch in "slice1-no-match".chars() {
                term.send_key(Key::Char(ch))
                    .await
                    .map_err(|error| test_error(format!("type search: {error:?}")))?;
            }
            term.expect("/slice1-no-match")
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| test_error(format!("transient search row missing: {error:?}")))?;
            term.send_key(Key::Escape)
                .await
                .map_err(|error| test_error(format!("clear search: {error:?}")))?;
            term.expect_gone("/slice1-no-match")
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| test_error(format!("search row did not clear: {error:?}")))?;
            term.expect(GROUP_NAME)
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| test_error(format!("search did not restore list: {error:?}")))?;

            term.send_key(Key::Char('?'))
                .await
                .map_err(|error| test_error(format!("open Session List help: {error:?}")))?;
            term.expect("Context Help — Session List")
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| test_error(format!("Session List help missing: {error:?}")))?;
            term.send_key(Key::Char('q'))
                .await
                .map_err(|error| test_error(format!("close Session List help: {error:?}")))?;

            term.send_key(Key::Char(' '))
                .await
                .map_err(|error| test_error(format!("settings leader: {error:?}")))?;
            term.send_key(Key::Char(','))
                .await
                .map_err(|error| test_error(format!("open settings: {error:?}")))?;
            term.expect("Settings")
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| test_error(format!("settings missing: {error:?}")))?;
            term.send_key(Key::Char('?'))
                .await
                .map_err(|error| test_error(format!("open category help: {error:?}")))?;
            term.expect("Context Help — Settings / Categories")
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| test_error(format!("category help missing: {error:?}")))?;
            term.send_key(Key::Char('q'))
                .await
                .map_err(|error| test_error(format!("return from category help: {error:?}")))?;

            term.send_key(Key::Char('j'))
                .await
                .map_err(|error| test_error(format!("select Theme & Colors: {error:?}")))?;
            term.expect("Theme & Colors")
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| test_error(format!("Theme & Colors missing: {error:?}")))?;
            term.send_key(Key::Char('l'))
                .await
                .map_err(|error| test_error(format!("focus theme items: {error:?}")))?;
            term.send_key(Key::Char('?'))
                .await
                .map_err(|error| test_error(format!("open item help: {error:?}")))?;
            term.expect("Context Help — Settings / Items")
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| test_error(format!("item help missing: {error:?}")))?;
            term.send_key(Key::Char('q'))
                .await
                .map_err(|error| test_error(format!("return from item help: {error:?}")))?;

            // Existing picker previews live and Escape restores its opening theme.
            term.enter()
                .await
                .map_err(|error| test_error(format!("open built-in themes: {error:?}")))?;
            term.expect("j/k: navigate  Enter: select")
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| test_error(format!("theme picker missing: {error:?}")))?;
            term.send_key(Key::Char('j'))
                .await
                .map_err(|error| test_error(format!("preview theme: {error:?}")))?;
            term.send_key(Key::Escape)
                .await
                .map_err(|error| test_error(format!("rollback theme preview: {error:?}")))?;
            term.expect("Built-in theme")
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| test_error(format!("theme picker did not return: {error:?}")))?;

            // Commit a readable Panel override, then reset that exact role.
            for _ in 0..2 {
                term.send_key(Key::Char('j'))
                    .await
                    .map_err(|error| test_error(format!("select Panel role: {error:?}")))?;
            }
            term.enter()
                .await
                .map_err(|error| test_error(format!("open Panel editor: {error:?}")))?;
            term.expect("Role: Panel (panel)")
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| test_error(format!("Panel editor missing: {error:?}")))?;
            for _ in 0..7 {
                term.send_key(Key::Backspace)
                    .await
                    .map_err(|error| test_error(format!("clear Panel color: {error:?}")))?;
            }
            for ch in "#FFFFFF".chars() {
                term.send_key(Key::Char(ch))
                    .await
                    .map_err(|error| test_error(format!("type Panel color: {error:?}")))?;
            }
            term.enter()
                .await
                .map_err(|error| test_error(format!("commit Panel color: {error:?}")))?;
            term.expect("Readable:")
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| test_error(format!("Panel assessment missing: {error:?}")))?;
            term.send_key(Key::Delete)
                .await
                .map_err(|error| test_error(format!("reset Panel role: {error:?}")))?;
            term.expect("Panel reset to built-in")
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| test_error(format!("Panel reset feedback missing: {error:?}")))?;
            term.send_key(Key::Char('q'))
                .await
                .map_err(|error| test_error(format!("close settings: {error:?}")))?;

            // Slice 4 first-class Issues workspace proof. Enter the fixture's
            // project workspace so Local reads are project-scoped.
            term.send_key(Key::Char(' '))
                .await
                .map_err(|error| test_error(format!("project leader: {error:?}")))?;
            term.send_key(Key::Char('p'))
                .await
                .map_err(|error| test_error(format!("open project picker: {error:?}")))?;
            term.expect("Workspaces")
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| test_error(format!("workspace picker missing: {error:?}")))?;
            term.expect(PROJECT_NAME)
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| test_error(format!("fixture project missing: {error:?}")))?;
            term.send_key(Key::Char('j'))
                .await
                .map_err(|error| test_error(format!("select fixture project: {error:?}")))?;
            term.enter()
                .await
                .map_err(|error| test_error(format!("open fixture workspace: {error:?}")))?;
            term.expect(GROUP_NAME)
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| test_error(format!("project workspace missing: {error:?}")))?;

            term.send_key(Key::Char(' '))
                .await
                .map_err(|error| test_error(format!("issue leader: {error:?}")))?;
            term.send_key(Key::Char('i'))
                .await
                .map_err(|error| test_error(format!("open Issues workspace: {error:?}")))?;
            term.expect(ISSUE_TITLE)
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| test_error(format!("Local Issue row missing: {error:?}")))?;
            term.send_key(Key::Char('?'))
                .await
                .map_err(|error| test_error(format!("open Issue help: {error:?}")))?;
            term.expect("Context Help — Issues")
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| test_error(format!("Issue help missing: {error:?}")))?;
            term.send_key(Key::Char('q'))
                .await
                .map_err(|error| test_error(format!("return from Issue help: {error:?}")))?;
            term.expect(ISSUE_TITLE)
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| test_error(format!("Issues pane was not restored: {error:?}")))?;

            // G exercises the bounded last-page path. With this one-row
            // fixture the canonical selected UUID must remain visible.
            term.send_key(Key::Char('G'))
                .await
                .map_err(|error| test_error(format!("load final Issue page: {error:?}")))?;
            term.expect(ISSUE_TITLE)
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| test_error(format!("final Issue page missing row: {error:?}")))?;

            // The create form is a real pane child; dirty Escape is a two-step
            // discard and returns to the same UUID-owned Local selection.
            term.send_key(Key::Char('n'))
                .await
                .map_err(|error| test_error(format!("open Issue create form: {error:?}")))?;
            term.expect("Create Issue")
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| test_error(format!("Issue create form missing: {error:?}")))?;
            term.type_str("discarded e2e draft")
                .await
                .map_err(|error| test_error(format!("type Issue draft: {error:?}")))?;
            term.send_key(Key::Escape)
                .await
                .map_err(|error| test_error(format!("arm Issue draft discard: {error:?}")))?;
            term.expect("UNSAVED CHANGES")
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| test_error(format!("dirty Issue warning missing: {error:?}")))?;
            term.send_key(Key::Escape)
                .await
                .map_err(|error| test_error(format!("discard Issue draft: {error:?}")))?;
            term.expect(ISSUE_TITLE)
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| {
                    test_error(format!("Local selection lost after form: {error:?}"))
                })?;

            let issue_id = self.fixture.issue_id.expect("seeded Issue UUID");
            let issue_number = self.fixture.issue_number.expect("seeded Issue number");
            term.send_key(Key::Char('y'))
                .await
                .map_err(|error| test_error(format!("arm Issue UUID copy: {error:?}")))?;
            term.send_key(Key::Char('y'))
                .await
                .map_err(|error| test_error(format!("copy Issue UUID: {error:?}")))?;
            term.expect(&format!("issue {issue_id} copied"))
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| test_error(format!("Issue UUID toast missing: {error:?}")))?;
            term.send_key(Key::Char('y'))
                .await
                .map_err(|error| test_error(format!("arm Issue number copy: {error:?}")))?;
            term.send_key(Key::Char('#'))
                .await
                .map_err(|error| test_error(format!("copy Issue number: {error:?}")))?;
            term.expect(&format!("issue #{issue_number} copied"))
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| test_error(format!("Issue number toast missing: {error:?}")))?;

            term.send_key(Key::Char('d'))
                .await
                .map_err(|error| test_error(format!("arm Issue cancellation: {error:?}")))?;
            term.send_key(Key::Char('d'))
                .await
                .map_err(|error| test_error(format!("confirm Issue cancellation: {error:?}")))?;
            term.expect("Cancelled")
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| test_error(format!("Cancelled Issue missing: {error:?}")))?;

            term.send_key(Key::Char('S'))
                .await
                .map_err(|error| test_error(format!("open Issue status form: {error:?}")))?;
            term.expect("Status Issue")
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| test_error(format!("Issue status form missing: {error:?}")))?;
            term.send_key(Key::Down)
                .await
                .map_err(|error| test_error(format!("select Open Issue status: {error:?}")))?;
            // CSI-u encodes Ctrl+Enter without relying on a terminal-specific
            // legacy byte that cannot distinguish it from plain Enter.
            term.send_raw(b"\x1b[13;5u")
                .await
                .map_err(|error| test_error(format!("submit Issue status form: {error:?}")))?;
            term.expect_gone("Status Issue")
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| {
                    test_error(format!("Issue status form did not close: {error:?}"))
                })?;
            term.expect("Open")
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| test_error(format!("reopened Issue missing: {error:?}")))?;

            term.send_key(Key::Char(']'))
                .await
                .map_err(|error| test_error(format!("open Dispatched Issues tab: {error:?}")))?;
            term.expect("No dispatched issues")
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| test_error(format!("Dispatched tab missing: {error:?}")))?;
            term.send_key(Key::Char(']'))
                .await
                .map_err(|error| test_error(format!("open Sync Issues tab: {error:?}")))?;
            term.expect("Tracker:")
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| test_error(format!("Sync tab missing: {error:?}")))?;
            term.send_key(Key::Char('P'))
                .await
                .map_err(|error| test_error(format!("run Issue tracker poll now: {error:?}")))?;
            term.expect("Last manual poll error:")
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| test_error(format!("manual poll result missing: {error:?}")))?;
            term.expect("Issue tracker not configured")
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| test_error(format!("manual poll error missing: {error:?}")))?;

            term.send_key(Key::Char('q'))
                .await
                .map_err(|error| test_error(format!("restore pre-Issues pane: {error:?}")))?;
            term.expect(GROUP_NAME)
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| test_error(format!("pre-Issues pane not restored: {error:?}")))?;

            term.send_key(Key::Char('g'))
                .await
                .map_err(|error| test_error(format!("schedule prefix: {error:?}")))?;
            term.send_key(Key::Char('K'))
                .await
                .map_err(|error| test_error(format!("open Scheduled Jobs: {error:?}")))?;
            term.expect("No scheduled jobs")
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| test_error(format!("Scheduled Jobs missing: {error:?}")))?;
            term.send_key(Key::Char('?'))
                .await
                .map_err(|error| test_error(format!("open Schedule help: {error:?}")))?;
            term.expect("Context Help — Scheduled Jobs")
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| test_error(format!("Schedule help missing: {error:?}")))?;
            term.send_key(Key::Char('q'))
                .await
                .map_err(|error| test_error(format!("return from Schedule help: {error:?}")))?;
            term.expect("No scheduled jobs")
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| {
                    test_error(format!("Schedule overlay was not restored: {error:?}"))
                })?;
            term.send_key(Key::Char('q'))
                .await
                .map_err(|error| test_error(format!("close Scheduled Jobs: {error:?}")))?;

            tokio::time::sleep(Duration::from_millis(200)).await;

            // Transition 1: Enter, Root -> Group. Time keypress to visible state.
            let t_root_to_group = Instant::now();
            term.enter().await.map_err(|error| {
                test_error(format!("failed to press Enter (Root -> Group): {error:?}"))
            })?;

            term.expect("Sessions / E2E Group")
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| {
                    test_error(format!("timed out waiting for Group screen: {error:?}"))
                })?;
            record_transition(&mut timings, "Enter Root->Group", t_root_to_group.elapsed());

            term.expect(EPIC_NAME)
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| {
                    test_error(format!(
                        "timed out waiting for {EPIC_NAME} in list: {error:?}"
                    ))
                })?;

            tokio::time::sleep(Duration::from_millis(200)).await;

            // Transition 2: Enter, Group -> Epic.
            let t_group_to_epic = Instant::now();
            term.enter().await.map_err(|error| {
                test_error(format!("failed to press Enter (Group -> Epic): {error:?}"))
            })?;

            term.expect("Sessions / E2E Group / E2E Epic")
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| {
                    test_error(format!("timed out waiting for Epic screen: {error:?}"))
                })?;
            record_transition(&mut timings, "Enter Group->Epic", t_group_to_epic.elapsed());

            tokio::time::sleep(Duration::from_millis(200)).await;

            // Transition 3: '-' ascent, Epic -> Group.
            let t_ascent_to_group = Instant::now();
            term.send_key(Key::Char('-'))
                .await
                .map_err(|error| test_error(format!("failed to press '-': {error:?}")))?;

            term.expect_gone("Sessions / E2E Group / E2E Epic")
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| {
                    test_error(format!(
                        "timed out waiting for Epic to disappear: {error:?}"
                    ))
                })?;
            record_transition(
                &mut timings,
                "'-' Ascent->Group",
                t_ascent_to_group.elapsed(),
            );

            let screen_text = term.screen().await.text();
            if !screen_text.contains("Sessions / E2E Group") {
                return Err(test_error(format!(
                    "Group breadcrumb not present after ascent. Screen:\n{screen_text}"
                )));
            }

            // Red-proof hook (double-gated induced test only). The normal path
            // above is byte-identical; this injects ONE deliberately-wrong
            // expectation so the scenario provably returns Err.
            if inject_failure {
                term.expect("RSI_E2E_INDUCED_NONEXISTENT_MARKER")
                    .timeout(Duration::from_secs(2))
                    .await
                    .map_err(|error| {
                        test_error(format!(
                            "induced failure (expected): wrong marker never appears: {error:?}"
                        ))
                    })?;
            }

            Ok(())
        }
        .await;

        self.transitions = timings;
        self.final_screen_text = Some(term.screen().await.text());
        let _ = term.kill().await;
        scenario_result
    }

    /// Lifecycle wrapper for the ST-NEWSESSION-UNIFY modal-create proof:
    /// daemon startup -> readiness -> seed Group/Epic -> drive the modal.
    async fn run_newsession_modal_unified(&mut self) -> E2eResult<()> {
        self.phase = "daemon-startup";
        self.start_daemon()?;

        self.phase = "daemon-readiness";
        let mut client = self.wait_for_daemon().await?;

        self.phase = "fixture-seeding";
        let fixture_result = self.seed_fixture(&mut client).await;
        client.disconnect();
        fixture_result?;

        self.phase = "tui-modal-create-scenario";
        self.drive_modal_create_scenario().await
    }

    async fn run_sandbox_storage_settings(&mut self) -> E2eResult<()> {
        self.phase = "sandbox-storage-daemon-startup";
        self.start_daemon()?;
        self.phase = "sandbox-storage-daemon-readiness";
        let mut client = self.wait_for_daemon().await?;
        client
            .update_daemon_config(
                "sandbox_build_cache_reclaim_ttl_secs",
                serde_json::json!(12_345),
            )
            .await
            .map_err(|error| test_error(format!("seed non-preset TTL: {error}")))?;
        client
            .update_daemon_config(
                "sandbox_build_cache_reclaim_low_watermark_pct",
                serde_json::json!(1),
            )
            .await
            .map_err(|error| test_error(format!("seed pressure low watermark: {error}")))?;
        client
            .update_daemon_config(
                "sandbox_build_cache_reclaim_high_watermark_pct",
                serde_json::json!(2),
            )
            .await
            .map_err(|error| test_error(format!("seed pressure high watermark: {error}")))?;
        self.eligible_target = Some(
            self.seed_authenticated_eligible_target(&mut client, None)
                .await?,
        );
        client.disconnect();
        self.phase = "sandbox-storage-tui-scenario";
        self.drive_sandbox_storage_settings().await
    }

    async fn run_startup_readiness_delayed_preview(&mut self) -> E2eResult<()> {
        self.phase = "startup-readiness-daemon-startup";
        self.start_daemon()?;
        self.phase = "startup-readiness-daemon-readiness";
        let mut client = self.wait_for_daemon().await?;
        const PREVIEW_TTL_SECS: u64 = 1;
        client
            .update_daemon_config(
                "sandbox_build_cache_reclaim_ttl_secs",
                serde_json::json!(PREVIEW_TTL_SECS),
            )
            .await
            .map_err(|error| test_error(format!("seed preview minimum TTL: {error}")))?;
        client
            .update_daemon_config(
                "sandbox_build_cache_reclaim_low_watermark_pct",
                serde_json::json!(1),
            )
            .await
            .map_err(|error| test_error(format!("seed preview low watermark: {error}")))?;
        client
            .update_daemon_config(
                "sandbox_build_cache_reclaim_high_watermark_pct",
                serde_json::json!(2),
            )
            .await
            .map_err(|error| test_error(format!("seed preview high watermark: {error}")))?;
        self.eligible_target = Some(
            self.seed_authenticated_eligible_target(
                &mut client,
                Some(Duration::from_secs(PREVIEW_TTL_SECS)),
            )
            .await?,
        );
        client.disconnect();
        fs::write(&self.git_hold, b"hold\n")?;

        self.phase = "startup-readiness-delayed-preview";
        let home_str = self.path_string(&self.home_dir)?;
        let socket_str = self.path_string(&self.socket_path)?;
        let spawn_started = Instant::now();
        let term = Terminal::builder()
            .size(180, PTY_ROWS)
            .timeout(Duration::from_secs(10))
            .env("HOME", &home_str)
            .env("RSI_DAEMON_SOCKET_PATH", &socket_str)
            .env("RSI_TUI_NO_AUTO_START_DAEMON", "1")
            .env("PATH", &self.isolated_path)
            .env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor")
            .env("LANG", "C.UTF-8")
            .env("TZ", "UTC")
            .spawn(&self.rsi_bin, &[])
            .await
            .map_err(|error| test_error(format!("spawn delayed-preview TUI: {error:?}")))?;

        let scenario = async {
            term.expect("1 session")
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| {
                    test_error(format!("positive warm ready frame missing: {error:?}"))
                })?;
            println!(
                "[startup-readiness] warm first frame: {} ms",
                spawn_started.elapsed().as_millis()
            );

            for _ in 0..200 {
                if self.git_entered.exists() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            if !self.git_entered.exists() {
                return Err(test_error(
                    "fresh preview never reached held git worktree list",
                ));
            }

            term.expect("1 session")
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| {
                    test_error(format!(
                        "authoritative session did not render before preview release: {error:?}"
                    ))
                })?;
            term.send_key(Key::Char('/'))
                .await
                .map_err(|error| test_error(format!("search during preview hold: {error:?}")))?;
            term.type_str("startup-ready-input")
                .await
                .map_err(|error| test_error(format!("type during preview hold: {error:?}")))?;
            term.expect("/startup-ready-input")
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| {
                    test_error(format!(
                        "input did not render during preview hold: {error:?}"
                    ))
                })?;
            term.escape()
                .await
                .map_err(|error| test_error(format!("close held-preview search: {error:?}")))?;
            term.expect("1 session")
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| test_error(format!("session did not return: {error:?}")))?;

            term.send_key(Key::Char(' '))
                .await
                .map_err(|error| test_error(format!("settings leader: {error:?}")))?;
            term.send_key(Key::Char(','))
                .await
                .map_err(|error| test_error(format!("open settings: {error:?}")))?;
            term.expect("CATEGORIES")
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| test_error(format!("settings did not open: {error:?}")))?;
            term.send_key(Key::Char('h'))
                .await
                .map_err(|error| test_error(format!("focus settings categories: {error:?}")))?;
            for category in [
                "Theme & Colors",
                "Session Defaults",
                "API Models",
                "List Fields",
                "Daemon Features",
            ] {
                term.send_key(Key::Char('j'))
                    .await
                    .map_err(|error| test_error(format!("select Daemon Features: {error:?}")))?;
                term.expect(&format!("▶ {category}"))
                    .timeout(Duration::from_secs(5))
                    .await
                    .map_err(|error| {
                        test_error(format!("settings category {category:?} missing: {error:?}"))
                    })?;
            }
            term.expect("▶ Daemon Features")
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| test_error(format!("Daemon Features missing: {error:?}")))?;
            term.send_key(Key::Char('l'))
                .await
                .map_err(|error| test_error(format!("enter Daemon Features: {error:?}")))?;
            term.expect("Sandbox storage")
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| test_error(format!("storage status row missing: {error:?}")))?;
            term.expect("Refreshing · no prev")
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| {
                    test_error(format!("refreshing-unknown state missing: {error:?}"))
                })?;

            self.release_git_hold()?;
            term.expect("Fresh ")
                .timeout(Duration::from_secs(10))
                .await
                .map_err(|error| test_error(format!("fresh preview state missing: {error:?}")))?;

            term.send_key(Key::Ctrl('c'))
                .await
                .map_err(|error| test_error(format!("quit delayed-preview TUI: {error:?}")))?;
            term.wait_exit().await.map_err(|error| {
                test_error(format!(
                    "delayed-preview TUI did not quit cleanly: {error:?}"
                ))
            })?;
            Ok(())
        }
        .await;

        self.final_screen_text = Some(term.screen().await.text());
        if scenario.is_err() {
            eprintln!(
                "[startup-readiness-preview] final screen:\n{}",
                self.final_screen_text.as_deref().unwrap_or_default()
            );
            if let Ok(log) = fs::read_to_string(self.artifacts_dir.join("rsid.log")) {
                eprintln!("[startup-readiness-preview] rsid log:\n{log}");
            }
            if let Ok(log) = fs::read_to_string(self.home_dir.join(".rsi/tui.log")) {
                eprintln!("[startup-readiness-preview] TUI log:\n{log}");
            }
        }
        self.release_git_hold()?;
        let _ = term.kill().await;
        scenario
    }

    fn release_git_hold(&self) -> E2eResult<()> {
        fs::write(&self.git_release, b"release\n")?;
        Ok(())
    }

    async fn run_codex_capability_display(&mut self) -> E2eResult<()> {
        const DEFAULT_TITLE: &str = "CODEX CAPABILITY DEFAULT E2E";
        const CONFIGURED_TITLE: &str = "CODEX CAPABILITY CONFIGURED E2E";

        self.phase = "codex-capability-daemon-startup";
        self.start_daemon()?;
        self.phase = "codex-capability-daemon-readiness";
        let mut client = self.wait_for_daemon().await?;

        self.phase = "codex-capability-launch";
        let working_dir = std::env::current_dir()?;
        client
            .launch_session_with_opts(
                "report deterministic context capacity",
                Some(DEFAULT_TITLE),
                Some(&working_dir),
                SessionProvider::Codex,
                Some("gpt-6-astra"),
                None,
                Some(SessionKind::Standard),
                None,
                Some(0),
                Some("low"),
                None,
                None,
                &[TAG.to_string()],
                None,
                None,
            )
            .await
            .map_err(|error| test_error(format!("launch Codex capability fixture: {error}")))?;

        let mut observed = None;
        for _ in 0..100 {
            observed = client
                .list_sessions()
                .await
                .map_err(|error| test_error(format!("poll Codex capability fixture: {error}")))?
                .into_iter()
                .find(|session| session.title.as_deref() == Some(DEFAULT_TITLE));
            if observed.as_ref().is_some_and(|session| {
                matches!(
                    session.status,
                    SessionStatus::Completed | SessionStatus::Failed
                ) && session.input_tokens == Some(34_000)
                    && session.resolved_context_budget.is_some()
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let session =
            observed.ok_or_else(|| test_error("Codex capability row was not persisted"))?;
        assert_codex_capability_session(&session, "default live RPC", 258_400, None)?;

        self.phase = "codex-capability-configure";
        let codex_home = self.home_dir.join(".codex");
        fs::create_dir_all(&codex_home)?;
        fs::write(
            codex_home.join("config.toml"),
            "model_context_window = 400000\n",
        )?;

        self.phase = "codex-capability-configured-launch";
        client
            .launch_session_with_opts(
                "report configured deterministic context capacity",
                Some(CONFIGURED_TITLE),
                Some(&working_dir),
                SessionProvider::Codex,
                Some("gpt-6-astra"),
                None,
                Some(SessionKind::Standard),
                None,
                Some(0),
                Some("low"),
                None,
                None,
                &[TAG.to_string()],
                None,
                None,
            )
            .await
            .map_err(|error| {
                test_error(format!(
                    "launch configured Codex capability fixture: {error}"
                ))
            })?;

        let mut configured_observed = None;
        for _ in 0..100 {
            configured_observed = client
                .list_sessions()
                .await
                .map_err(|error| {
                    test_error(format!("poll configured Codex capability fixture: {error}"))
                })?
                .into_iter()
                .find(|session| session.title.as_deref() == Some(CONFIGURED_TITLE));
            if configured_observed.as_ref().is_some_and(|session| {
                matches!(
                    session.status,
                    SessionStatus::Completed | SessionStatus::Failed
                ) && session.input_tokens == Some(34_000)
                    && session.resolved_context_budget.is_some()
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let configured = configured_observed
            .ok_or_else(|| test_error("configured Codex capability row was not persisted"))?;
        assert_codex_capability_session(
            &configured,
            "configured live RPC",
            380_000,
            Some(400_000),
        )?;
        client.disconnect();

        self.phase = "codex-capability-restart";
        self.shutdown_daemon();
        self.start_daemon()?;
        let mut client = self.wait_for_daemon().await?;
        let reopened_rows = client
            .list_sessions()
            .await
            .map_err(|error| test_error(format!("read restarted Codex fixtures: {error}")))?;
        let reopened_default = reopened_rows
            .iter()
            .find(|session| session.title.as_deref() == Some(DEFAULT_TITLE))
            .ok_or_else(|| test_error("Codex capability row was not restored after restart"))?;
        assert_codex_capability_session(reopened_default, "default restarted RPC", 258_400, None)?;
        let reopened_configured = reopened_rows
            .iter()
            .find(|session| session.title.as_deref() == Some(CONFIGURED_TITLE))
            .ok_or_else(|| {
                test_error("configured Codex capability row was not restored after restart")
            })?;
        assert_codex_capability_session(
            reopened_configured,
            "configured restarted RPC",
            380_000,
            Some(400_000),
        )?;
        client.disconnect();

        self.phase = "codex-capability-tui";
        let home_str = self.path_string(&self.home_dir)?;
        let socket_str = self.path_string(&self.socket_path)?;
        let term = Terminal::builder()
            .size(120, 40)
            .env("HOME", &home_str)
            .env("RSI_DAEMON_SOCKET_PATH", &socket_str)
            .env("RSI_TUI_NO_AUTO_START_DAEMON", "1")
            .env("PATH", &self.isolated_path)
            .env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor")
            .env("LANG", "C.UTF-8")
            .env("TZ", "UTC")
            .spawn(&self.rsi_bin, &[])
            .await
            .map_err(|error| test_error(format!("spawn Codex capability TUI: {error:?}")))?;

        let scenario = async {
            term.expect(CONFIGURED_TITLE)
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| test_error(format!("Codex capability row missing: {error:?}")))?;
            term.send_key(Key::Char('j'))
                .await
                .map_err(|error| test_error(format!("select configured Codex row: {error:?}")))?;
            term.send_key(Key::F(3))
                .await
                .map_err(|error| test_error(format!("open Session Info: {error:?}")))?;
            for detail in [
                "Session Info",
                "provider: Codex",
                "model: gpt-6-astra",
                "context: 34k / 380k runtime",
                "provider: default 272k · max 872k · effective factor 95%",
                "API: context max 1.05m · output max 128k",
                "source: runtime telemetry",
            ] {
                term.expect(detail)
                    .timeout(Duration::from_secs(5))
                    .await
                    .map_err(|error| {
                        test_error(format!(
                            "Codex capability detail {detail:?} missing: {error:?}"
                        ))
                    })?;
            }
            Ok(())
        }
        .await;

        self.final_screen_text = Some(term.screen().await.text());
        if scenario.is_err() {
            eprintln!(
                "[codex-capability-e2e] final screen:\n{}",
                self.final_screen_text.as_deref().unwrap_or_default()
            );
        }
        let _ = term.kill().await;
        scenario
    }

    async fn drive_sandbox_storage_settings(&mut self) -> E2eResult<()> {
        let home_str = self.path_string(&self.home_dir)?;
        let socket_str = self.path_string(&self.socket_path)?;
        let term = Terminal::builder()
            // Keep both checked/eligible labels visible in this focused
            // fixture; the complete capacity wording is asserted from the
            // ordinary notification footer below.
            .size(180, PTY_ROWS)
            .env("HOME", &home_str)
            .env("RSI_DAEMON_SOCKET_PATH", &socket_str)
            .env("RSI_TUI_NO_AUTO_START_DAEMON", "1")
            .env("PATH", &self.isolated_path)
            .env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor")
            .env("LANG", "C.UTF-8")
            .env("TZ", "UTC")
            .spawn(&self.rsi_bin, &[])
            .await
            .map_err(|error| test_error(format!("spawn storage settings TUI: {error:?}")))?;

        let scenario = async {
            term.expect("Sessions")
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| test_error(format!("storage TUI root timeout: {error:?}")))?;
            term.send_key(Key::Char(' '))
                .await
                .map_err(|error| test_error(format!("settings leader: {error:?}")))?;
            term.send_key(Key::Char(','))
                .await
                .map_err(|error| test_error(format!("open settings: {error:?}")))?;
            term.expect("Settings")
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| test_error(format!("settings did not open: {error:?}")))?;

            for _ in 0..5 {
                term.send_key(Key::Char('j'))
                    .await
                    .map_err(|error| test_error(format!("select Daemon Features: {error:?}")))?;
            }
            term.expect("▶ Daemon Features")
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| test_error(format!("Daemon Features not selected: {error:?}")))?;
            term.send_key(Key::Char('l'))
                .await
                .map_err(|error| test_error(format!("enter Daemon Features: {error:?}")))?;
            term.expect("▌ Model control mode")
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| {
                    test_error(format!("Daemon Features items not focused: {error:?}"))
                })?;

            // Move to the TTL row and prove the daemon's non-preset value was
            // injected and selected exactly rather than displayed as a preset.
            select_settings_row(&term, "Cache reclaim TTL", 64).await?;
            term.expect("▌ Cache reclaim TTL")
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| test_error(format!("cache TTL row missing: {error:?}")))?;
            term.expect("12345")
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| test_error(format!("exact non-preset TTL missing: {error:?}")))?;

            // Preview is index 22. The isolated fixture has one authenticated,
            // nonempty eligible target, while the conservative wire capacity
            // fields remain zero.
            select_settings_row(&term, "Preview cache reclaim", 64).await?;
            term.expect("▌ Preview cache reclaim")
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| test_error(format!("preview row missing: {error:?}")))?;
            term.send_key(Key::Char(' '))
                .await
                .map_err(|error| test_error(format!("run preview: {error:?}")))?;
            term.expect("1 eligible /")
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| {
                    test_error(format!("preview candidate count missing: {error:?}"))
                })?;
            term.send_key(Key::Char('q'))
                .await
                .map_err(|error| test_error(format!("close settings after preview: {error:?}")))?;
            term.expect("Preview: 1 eligible of 1 checked; capacity estimate unavailable")
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| {
                    test_error(format!("preview capacity wording missing: {error:?}"))
                })?;

            // Emulate a crash after a durable stage. The actual operator pass
            // must recover it, then its fresh dry-run refresh must show the
            // retained custody row as checked but no longer eligible.
            let staged = self.seed_isolated_recovery_stage()?;
            term.send_key(Key::Char(' '))
                .await
                .map_err(|error| test_error(format!("settings leader for actual: {error:?}")))?;
            term.send_key(Key::Char(','))
                .await
                .map_err(|error| test_error(format!("reopen settings for actual: {error:?}")))?;
            term.expect("Settings")
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| test_error(format!("settings did not reopen: {error:?}")))?;
            for _ in 0..5 {
                term.send_key(Key::Char('j'))
                    .await
                    .map_err(|error| test_error(format!("reselect Daemon Features: {error:?}")))?;
            }
            term.expect("▶ Daemon Features")
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| {
                    test_error(format!("Daemon Features not reselected: {error:?}"))
                })?;
            term.send_key(Key::Char('l'))
                .await
                .map_err(|error| test_error(format!("reenter Daemon Features: {error:?}")))?;
            term.expect("▌ Model control mode")
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| {
                    test_error(format!("Daemon Features items not refocused: {error:?}"))
                })?;
            select_settings_row(&term, "Reclaim sandbox caches now", 64).await?;
            term.expect("▌ Reclaim sandbox caches now")
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| test_error(format!("actual row missing: {error:?}")))?;
            term.send_key(Key::Char(' '))
                .await
                .map_err(|error| test_error(format!("run actual reclaim: {error:?}")))?;
            term.expect("2 rm · 0 pend")
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| test_error(format!("actual outcome missing: {error:?}")))?;
            term.expect("Fresh ")
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| test_error(format!("fresh status missing: {error:?}")))?;
            term.send_key(Key::Char('q'))
                .await
                .map_err(|error| test_error(format!("close settings: {error:?}")))?;
            term.expect("Reclaim: 2 removed, 1 staged, 0 pending")
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|error| test_error(format!("actual notification missing: {error:?}")))?;
            if staged.exists() {
                return Err(test_error(format!(
                    "actual pass did not recover isolated stage: {}",
                    staged.display()
                )));
            }
            if self
                .eligible_target
                .as_ref()
                .is_some_and(|target| target.exists())
            {
                return Err(test_error(
                    "actual pass retained authenticated eligible target",
                ));
            }
            Ok(())
        }
        .await;

        self.final_screen_text = Some(term.screen().await.text());
        if scenario.is_err() {
            eprintln!(
                "[sandbox-storage-e2e] final screen:\n{}",
                self.final_screen_text.as_deref().unwrap_or_default()
            );
            if let Ok(log) = fs::read_to_string(self.artifacts_dir.join("rsid.log")) {
                eprintln!("[sandbox-storage-e2e] rsid log:\n{log}");
            }
            if let Ok(log) = fs::read_to_string(self.home_dir.join(".rsi/tui.log")) {
                eprintln!("[sandbox-storage-e2e] TUI log:\n{log}");
            }
        }
        let _ = term.kill().await;
        scenario
    }

    async fn seed_authenticated_eligible_target(
        &self,
        client: &mut DaemonClient,
        minimum_age: Option<Duration>,
    ) -> E2eResult<PathBuf> {
        const TITLE: &str = "E2E authenticated cache fixture";
        const QUERY: &str = "create an isolated authenticated cache fixture";

        let repo = self.home_dir.join("sandbox-storage-repo");
        fs::create_dir(&repo)?;
        for args in [
            vec!["init", "-q", "."],
            vec!["config", "user.email", "rsi-e2e@example.invalid"],
            vec!["config", "user.name", "RSI E2E"],
        ] {
            let status = Command::new("git")
                .args(args)
                .current_dir(&repo)
                .env("HOME", &self.home_dir)
                .status()?;
            if !status.success() {
                return Err(test_error(format!("git fixture setup failed: {status}")));
            }
        }
        fs::write(repo.join("fixture.txt"), b"sandbox storage fixture\n")?;
        for args in [vec!["add", "fixture.txt"], vec!["commit", "-m", "fixture"]] {
            let status = Command::new("git")
                .args(args)
                .current_dir(&repo)
                .env("HOME", &self.home_dir)
                .status()?;
            if !status.success() {
                return Err(test_error(format!("git fixture commit failed: {status}")));
            }
        }

        client
            .launch_session_with_opts(
                QUERY,
                Some(TITLE),
                Some(&repo),
                SessionProvider::Claude,
                None,
                None,
                Some(SessionKind::Task),
                None,
                Some(0),
                None,
                None,
                Some(SandboxSpec {
                    kind: Some(SandboxKind::GitWorktree),
                    branch: None,
                }),
                &[TAG.to_string()],
                None,
                None,
            )
            .await
            .map_err(|error| test_error(format!("launch eligible cache fixture: {error}")))?;

        let mut last_observation = None;
        for _ in 0..100 {
            let session = client
                .list_sessions()
                .await
                .map_err(|error| test_error(format!("poll eligible cache fixture: {error}")))?
                .into_iter()
                .find(|session| session.query == QUERY);
            if let Some(session) = session {
                let age_seconds = (chrono::Utc::now() - session.updated_at).num_seconds();
                last_observation = Some((session.status, age_seconds));
                if matches!(
                    session.status,
                    SessionStatus::Completed | SessionStatus::Failed | SessionStatus::Interrupted
                ) && let Some(root) = &session.sandbox_root
                {
                    let target = root.join("target");
                    fs::create_dir(&target)?;
                    fs::write(target.join("artifact.bin"), vec![0x69_u8; 64 * 1024])?;
                    if let Some(minimum_age) = minimum_age {
                        let minimum_age_secs = i64::try_from(minimum_age.as_secs())?;
                        let age_deadline = Instant::now() + minimum_age + Duration::from_secs(2);
                        loop {
                            let source = client
                                .list_sessions()
                                .await
                                .map_err(|error| {
                                    test_error(format!(
                                        "poll authenticated cache fixture age: {error}"
                                    ))
                                })?
                                .into_iter()
                                .find(|candidate| candidate.query == QUERY)
                                .ok_or_else(|| {
                                    test_error(
                                        "authenticated cache fixture disappeared while aging",
                                    )
                                })?;
                            let age_seconds =
                                (chrono::Utc::now() - source.updated_at).num_seconds();
                            if age_seconds >= minimum_age_secs {
                                break;
                            }
                            if Instant::now() >= age_deadline {
                                return Err(test_error(format!(
                                    "authenticated cache fixture did not reach legal TTL age; required={minimum_age_secs}s observed={age_seconds}s"
                                )));
                            }
                            tokio::time::sleep(Duration::from_millis(50)).await;
                        }
                    }
                    // Provider teardown and title fallback can briefly retain
                    // StoreBusy after terminal status is visible. Require two
                    // separated authenticated previews before arming the Git
                    // barrier so the held TUI preview reaches Git inspection.
                    let mut last_preview = None;
                    let mut consecutive_eligible = 0;
                    for _ in 0..100 {
                        let report_wire =
                            client.get_sandbox_storage_status().await.map_err(|error| {
                                test_error(format!("warm authenticated preview: {error}"))
                            })?;
                        let report = report_wire.report();
                        last_preview = Some(format!(
                            "considered={} eligible={} skip_counts={:?}",
                            report.candidates_considered,
                            report.eligible_candidates,
                            report.skip_counts
                        ));
                        if report.eligible_candidates >= 1 {
                            consecutive_eligible += 1;
                            if consecutive_eligible == 2 {
                                return Ok(target);
                            }
                        } else {
                            consecutive_eligible = 0;
                        }
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                    return Err(test_error(format!(
                        "authenticated cache fixture never became preview-eligible; last preview: {last_preview:?}"
                    )));
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Err(test_error(format!(
            "authenticated eligible cache fixture did not become terminal within 5s; last observation: {last_observation:?}"
        )))
    }

    fn seed_isolated_recovery_stage(&self) -> E2eResult<PathBuf> {
        let queue = self.sandbox_base_dir.join(".rsi-target-reclaim-v1");
        fs::create_dir(&queue)?;
        fs::set_permissions(&queue, fs::Permissions::from_mode(0o700))?;
        let temporary = queue.join("pending-stage");
        fs::create_dir(&temporary)?;
        fs::write(temporary.join("artifact.bin"), vec![0x69_u8; 4096])?;
        let metadata = fs::metadata(&temporary)?;
        let staged = queue.join(format!(
            "v1_{}_1_{}_{}",
            uuid::Uuid::new_v4(),
            metadata.dev(),
            metadata.ino()
        ));
        fs::rename(temporary, &staged)?;
        Ok(staged)
    }

    /// Descend Root -> Group -> Epic, open the Task create-entity modal via the
    /// `gT` chord, fill name + tag, submit, then assert the daemon persisted a
    /// Task session whose launch fields match the unified builder's output.
    async fn drive_modal_create_scenario(&mut self) -> E2eResult<()> {
        const MODAL_NAME: &str = "PARITYMODALSESSION";

        let home_str = self.path_string(&self.home_dir)?;
        let socket_str = self.path_string(&self.socket_path)?;

        let term = Terminal::builder()
            .size(PTY_COLS, PTY_ROWS)
            .env("HOME", &home_str)
            .env("RSI_DAEMON_SOCKET_PATH", &socket_str)
            .env("RSI_TUI_NO_AUTO_START_DAEMON", "1")
            .env("PATH", &self.isolated_path)
            .env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor")
            .env("LANG", "C.UTF-8")
            .env("TZ", "UTC")
            .spawn(&self.rsi_bin, &[])
            .await
            .map_err(|error| test_error(format!("failed to spawn rsi: {error:?}")))?;

        let scenario = async {
            term.expect("Sessions")
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|e| test_error(format!("no root session list: {e:?}")))?;
            term.expect(GROUP_NAME)
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|e| test_error(format!("no {GROUP_NAME} on root: {e:?}")))?;

            // Descend into the Epic so `gT` creates a Task under it (Task is a
            // legal child of Epic; illegal at root).
            term.enter()
                .await
                .map_err(|e| test_error(format!("enter Group: {e:?}")))?;
            term.expect(EPIC_NAME)
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|e| test_error(format!("no {EPIC_NAME} in Group: {e:?}")))?;
            term.enter()
                .await
                .map_err(|e| test_error(format!("enter Epic: {e:?}")))?;
            term.expect("Sessions / E2E Group / E2E Epic")
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|e| test_error(format!("no Epic screen: {e:?}")))?;

            // `gT` opens the Task create-entity modal under the Epic, focused on
            // the Name field already in insert mode.
            term.send_key(Key::Char('g'))
                .await
                .map_err(|e| test_error(format!("press 'g' (gT chord): {e:?}")))?;
            term.send_key(Key::Char('T'))
                .await
                .map_err(|e| test_error(format!("press 'T' (gT chord): {e:?}")))?;
            for ch in MODAL_NAME.chars() {
                term.send_key(Key::Char(ch))
                    .await
                    .map_err(|e| test_error(format!("type modal name: {e:?}")))?;
            }
            term.expect(MODAL_NAME)
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|e| test_error(format!("modal name never rendered: {e:?}")))?;

            // Exit Name insert, focus Tag, type a tag, then Esc to commit the
            // chip and leave insert. A committed tag is mandatory to submit.
            // The TUI processes PTY bytes on its event loop, so each mode
            // transition needs a beat to settle before the next key (mirrors
            // the inter-transition sleeps in `run_tui_scenario`).
            term.escape()
                .await
                .map_err(|e| test_error(format!("esc Name insert: {e:?}")))?;
            tokio::time::sleep(Duration::from_millis(200)).await;
            term.send_key(Key::Char('T'))
                .await
                .map_err(|e| test_error(format!("focus Tag field: {e:?}")))?;
            tokio::time::sleep(Duration::from_millis(200)).await;
            for ch in TAG.chars() {
                term.send_key(Key::Char(ch))
                    .await
                    .map_err(|e| test_error(format!("type tag: {e:?}")))?;
            }
            // Tag chip renders -> confirms focus moved to Tag and chars landed.
            term.expect(TAG)
                .timeout(Duration::from_secs(5))
                .await
                .map_err(|e| test_error(format!("tag chip never rendered: {e:?}")))?;
            term.escape()
                .await
                .map_err(|e| test_error(format!("esc to commit tag: {e:?}")))?;
            tokio::time::sleep(Duration::from_millis(200)).await;

            // Enter submits the modal -> the unified launch dispatch. Wait for
            // the positive child count before the harness captures + kills;
            // the daemon-side assertions below then prove the exact persisted
            // launch identity without depending on raw provider stderr being
            // exposed by the rolling session renderer.
            term.enter()
                .await
                .map_err(|e| test_error(format!("submit modal: {e:?}")))?;
            term.expect("1 session")
                .timeout(Duration::from_secs(10))
                .await
                .map_err(|e| test_error(format!("modal session count never rendered: {e:?}")))?;
            Ok(())
        }
        .await;

        let final_screen = term.screen().await.text();
        let _ = term.kill().await;
        if let Err(e) = scenario {
            eprintln!("[modal-e2e] scenario failed; final screen:\n{final_screen}");
            if let Ok(log) = fs::read_to_string(self.artifacts_dir.join("rsid.log")) {
                eprintln!("[modal-e2e] rsid log:\n{log}");
            }
            if let Ok(log) = fs::read_to_string(self.home_dir.join(".rsi/tui.log")) {
                eprintln!("[modal-e2e] TUI log:\n{log}");
            }
            return Err(e);
        }

        // The modal launch is fire-and-forget; poll the daemon until the new
        // row persists, then assert the unified launch fields. Provider-stub
        // failure may flip the session to Failed, but the row (and its launch
        // config) is written before the subprocess is spawned.
        let mut client = self.wait_for_daemon().await?;
        let epic_id = self
            .fixture
            .epic_id
            .ok_or_else(|| test_error("modal fixture omitted Epic identity"))?;
        let project_id = self
            .fixture
            .project_id
            .ok_or_else(|| test_error("modal fixture omitted project identity"))?;
        let outcome = async {
            for _ in 0..50 {
                let sessions = client
                    .list_sessions()
                    .await
                    .map_err(|e| test_error(format!("list_sessions failed: {e}")))?;
                if let Some(session) = sessions.iter().find(|session| {
                    session.session_kind == SessionKind::Task && session.parent_id == Some(epic_id)
                }) {
                    if session.provider != rsi_common::types::SessionProvider::Claude {
                        return Err(test_error(format!(
                            "modal session provider expected Claude (unified default), got {:?}",
                            session.provider
                        )));
                    }
                    if session.project_id != Some(project_id) {
                        return Err(test_error(format!(
                            "modal session project expected {project_id}, got {:?}",
                            session.project_id
                        )));
                    }
                    // Acceptance criterion 1/2 (#338): the explicit modal name
                    // is the session's identity and must survive asynchronous
                    // generated title enrichment. Asserted positively, so this
                    // fails loudly if an explicit title is ever overwritten.
                    assert_eq!(
                        session.title.as_deref(),
                        Some(MODAL_NAME),
                        "explicit modal title must survive generated enrichment, got {:?}",
                        session.title
                    );
                    assert!(
                        session.query.is_empty(),
                        "modal session query expected empty Body, got {:?}",
                        session.query
                    );
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            let all = client.list_sessions().await.unwrap_or_default();
            let launch_rows: Vec<(Option<String>, SessionKind, Option<uuid::Uuid>, String)> = all
                .iter()
                .map(|session| {
                    (
                        session.title.clone(),
                        session.session_kind,
                        session.parent_id,
                        session.query.clone(),
                    )
                })
                .collect();
            eprintln!("[modal-e2e] sessions present at timeout: {launch_rows:?}");
            Err(test_error(
                "modal-created session never appeared in the daemon within 5s",
            ))
        }
        .await;
        client.disconnect();
        outcome
    }

    fn shutdown_daemon(&mut self) {
        drop(self.daemon.take());
    }

    fn preserve_artifacts(&self, failure: Option<&str>) -> E2eResult<PathBuf> {
        self.write_required_artifacts(failure)?;

        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let workspace_root = manifest_dir
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| test_error("failed to resolve workspace root"))?;
        let e2e_artifacts_base = workspace_root.join("target").join("e2e-artifacts");
        let timestamp = chrono::Utc::now().format("%Y%m%d-%H%M%S%.6f");
        let dest_dir = e2e_artifacts_base.join(timestamp.to_string());
        fs::create_dir_all(&dest_dir)?;

        for name in REQUIRED_ARTIFACTS {
            let source_path = self.artifacts_dir.join(name);
            if !source_path.exists() {
                return Err(test_error(format!(
                    "required E2E artifact was not generated: {}",
                    source_path.display()
                )));
            }
            fs::copy(&source_path, dest_dir.join(name))?;
        }

        println!("E2E artifacts preserved at: {}", dest_dir.display());
        Ok(dest_dir)
    }

    fn write_required_artifacts(&self, failure: Option<&str>) -> E2eResult<()> {
        let tui_log_source = self.home_dir.join(".rsi").join("tui.log");
        if tui_log_source.exists() {
            fs::copy(&tui_log_source, self.artifacts_dir.join("tui.log"))?;
        } else {
            fs::write(
                self.artifacts_dir.join("tui.log"),
                "tui.log was not created before harness cleanup\n",
            )?;
        }

        let screen_text = self
            .final_screen_text
            .as_deref()
            .unwrap_or("screen was unavailable because the TUI process did not start\n");
        fs::write(self.artifacts_dir.join("screen.txt"), screen_text)?;

        fs::write(self.artifacts_dir.join("env.txt"), self.env_summary())?;
        fs::write(
            self.artifacts_dir.join("fixture.json"),
            serde_json::to_string_pretty(&self.fixture_summary())?,
        )?;
        fs::write(
            self.artifacts_dir.join("scenario.txt"),
            self.scenario_summary(failure),
        )?;
        fs::write(
            self.artifacts_dir.join("timings.txt"),
            self.timings_summary(),
        )?;

        Ok(())
    }

    fn timings_summary(&self) -> String {
        let mut summary = String::new();
        summary.push_str("Transition timings (advisory only — never a gate)\n");
        summary.push_str(&format!(
            "Thresholds: warn>={TIMING_WARN_MS}ms advisory-fail>={TIMING_ADVISORY_FAIL_MS}ms\n"
        ));
        if self.transitions.is_empty() {
            summary.push_str("No transitions recorded (scenario did not reach navigation phase)\n");
            return summary;
        }
        for timing in &self.transitions {
            let breach = timing.breach.unwrap_or("ok");
            summary.push_str(&format!(
                "{label}: {elapsed_ms} ms [{breach}]\n",
                label = timing.label,
                elapsed_ms = timing.elapsed_ms,
            ));
        }
        summary
    }

    fn env_summary(&self) -> String {
        [
            format!("phase={}", self.phase),
            format!("temp_home={}", self.home_dir.display()),
            format!("socket_path={}", self.socket_path.display()),
            format!("sandbox_base={}", self.sandbox_base_dir.display()),
            format!("stub_bin={}", self.bin_dir.display()),
            format!("rsi_bin={}", self.rsi_bin),
            format!("rsid_bin={}", self.rsid_bin_path.display()),
            "RSI_DAEMON_SOCKET_PATH=<temp>/home/.rsi/daemon.sock".to_string(),
            "RSI_SANDBOX_BASE=<temp>/home/.rsi/sandboxes".to_string(),
            "RSI_TUI_NO_AUTO_START_DAEMON=1".to_string(),
            "RSI_MEMORY_ENABLED=false".to_string(),
            "RSI_DREAM_ENABLED=false".to_string(),
            "RSI_QUEUE_ENABLED=false".to_string(),
            "RSI_RECONCILIATION_ENABLED=false".to_string(),
            "RSI_STALL_DETECTION_ENABLED=false".to_string(),
            "RSI_SMOKE_SUPPRESS_RETRY_RESTORE=true".to_string(),
            "LOCAL_LLM_BASE_URL=http://127.0.0.1:0/v1".to_string(),
            "RSI_OLLAMA_URL=http://127.0.0.1:0/api/generate".to_string(),
            format!(
                "RSI_E2E_KEEP_ARTIFACTS={}",
                std::env::var("RSI_E2E_KEEP_ARTIFACTS").unwrap_or_default()
            ),
        ]
        .join("\n")
            + "\n"
    }

    fn fixture_summary(&self) -> serde_json::Value {
        serde_json::json!({
            "phase": self.phase,
            "project": {
                "id": self.fixture.project_id,
                "name": PROJECT_NAME,
            },
            "group": {
                "id": self.fixture.group_id,
                "name": GROUP_NAME,
                "kind": "Group",
            },
            "epic": {
                "id": self.fixture.epic_id,
                "name": EPIC_NAME,
                "kind": "Epic",
                "parent_id": self.fixture.group_id,
            },
            "issue": {
                "id": self.fixture.issue_id,
                "number": self.fixture.issue_number,
                "title": ISSUE_TITLE,
            },
            "tags": [TAG],
            "leaf_sessions_seeded": false,
        })
    }

    fn scenario_summary(&self, failure: Option<&str>) -> String {
        let mut summary = String::new();
        summary.push_str("Scenario: Root -> Group -> Epic -> Group\n");
        summary.push_str(&format!("Timestamp: {}\n", chrono::Utc::now().to_rfc3339()));
        summary.push_str(&format!("Phase: {}\n", self.phase));
        summary.push_str(&format!("PTY: {PTY_COLS}x{PTY_ROWS}\n"));
        match failure {
            Some(error) => {
                summary.push_str("Status: FAILED\n");
                summary.push_str(&format!("Error: {error}\n"));
            }
            None => summary.push_str("Status: SUCCESS\n"),
        }
        summary.push_str(&format!(
            "Transition timing thresholds (advisory): warn>={TIMING_WARN_MS}ms \
             advisory-fail>={TIMING_ADVISORY_FAIL_MS}ms\n"
        ));
        if self.transitions.is_empty() {
            summary.push_str("Transitions: none recorded\n");
        } else {
            summary.push_str("Transitions:\n");
            for timing in &self.transitions {
                let breach = timing.breach.unwrap_or("ok");
                summary.push_str(&format!(
                    "  - {label}: {elapsed_ms} ms [{breach}]\n",
                    label = timing.label,
                    elapsed_ms = timing.elapsed_ms,
                ));
            }
        }
        summary
    }

    fn path_string(&self, path: &Path) -> E2eResult<String> {
        path.to_str()
            .map(ToOwned::to_owned)
            .ok_or_else(|| test_error(format!("path is not valid UTF-8: {}", path.display())))
    }
}

fn install_provider_stubs(bin_dir: &Path) -> E2eResult<()> {
    let stub_content =
        "#!/bin/sh\necho \"rsi e2e provider stub invoked unexpectedly\" >&2\nexit 1\n";
    for stub in &["claude", "agy", "antigravity", "antigravity-cli"] {
        let stub_path = bin_dir.join(stub);
        fs::write(&stub_path, stub_content)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mut perms = fs::metadata(&stub_path)?.permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&stub_path, perms)?;
        }
    }

    fs::write(
        bin_dir.join("codex-models.json"),
        include_bytes!("../../rsid/tests/fixtures/codex-models-0.155.1.json"),
    )?;
    let codex_stub = r#"#!/bin/sh
if [ "$1" = "--version" ]; then
  printf '%s\n' 'codex-cli 0.155.1'
  exit 0
fi
if [ "$1" = "debug" ] && [ "$2" = "models" ] && [ "$3" = "--bundled" ]; then
  cat "$(dirname "$0")/codex-models.json"
  exit 0
fi
if [ "$1" = "app-server" ]; then
  while IFS= read -r request; do
    case "$request" in
      *'"method":"initialize"'*)
        printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{}}'
        ;;
      *'"method":"config/read"'*)
        if grep -q '^model_context_window = 400000$' "$CODEX_HOME/config.toml" 2>/dev/null; then
          printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"config":{"model_context_window":400000}}}'
        else
          printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"config":{}}}'
        fi
        ;;
    esac
  done
  exit 0
fi
if [ "$1" = "exec" ]; then
  context_window=258400
  thread_id=e2e-codex-capability-default
  for arg in "$@"; do
    if [ "$arg" = "model_context_window=400000" ]; then
      context_window=380000
      thread_id=e2e-codex-capability-configured
    fi
  done
  printf '{"type":"thread.started","thread_id":"%s"}\n' "$thread_id"
  printf '{"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"total_tokens":34000,"output_tokens":120},"model_context_window":%s}}}\n' "$context_window"
  printf '%s\n' '{"type":"item.completed","item":{"type":"agent_message","text":"deterministic capability fixture complete"}}'
  printf '%s\n' '{"type":"turn.completed","usage":{"input_tokens":34000,"cached_input_tokens":0,"output_tokens":120}}'
  exit 0
fi
printf '%s\n' 'unsupported codex e2e invocation' >&2
exit 1
"#;
    let codex_path = bin_dir.join("codex");
    fs::write(&codex_path, codex_stub)?;
    let mut perms = fs::metadata(&codex_path)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&codex_path, perms)?;

    Ok(())
}

fn assert_codex_capability_session(
    session: &rsi_common::types::Session,
    phase: &str,
    active_tokens: u64,
    configured_tokens: Option<u64>,
) -> E2eResult<()> {
    let budget = session
        .resolved_context_budget
        .as_ref()
        .ok_or_else(|| test_error(format!("{phase} Codex row lacks resolved budget")))?;
    if session.input_tokens != Some(34_000)
        || budget.active_tokens != active_tokens
        || budget.capacity.provider_default_tokens != Some(272_000)
        || budget.capacity.provider_max_tokens != Some(872_000)
        || budget.capacity.effective_percent != Some(95)
        || budget.capacity.configured_tokens != configured_tokens
        || budget.capacity.runtime_effective_tokens != Some(active_tokens)
        || budget.capacity.advertised_max_tokens != Some(1_050_000)
        || budget.capacity.max_output_tokens != Some(128_000)
        || budget.evidence.source
            != rsi_common::provider_capabilities::CapabilitySource::RuntimeTelemetry
        || budget.evidence.source_version.as_deref() != Some("codex-cli 0.155.1")
        || budget.evidence.source_digest.is_none()
    {
        return Err(test_error(format!(
            "{phase} Codex capability facts drifted: input={:?}, budget={budget:?}",
            session.input_tokens
        )));
    }
    Ok(())
}

fn find_executable(name: &str) -> E2eResult<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|directory| directory.join(name))
        .find(|candidate| {
            fs::metadata(candidate)
                .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        })
        .ok_or_else(|| {
            test_error(format!(
                "required executable {name:?} was not found on PATH"
            ))
        })
}

fn install_git_hold_wrapper(bin_dir: &Path) -> E2eResult<()> {
    let wrapper = bin_dir.join("git");
    fs::write(
        &wrapper,
        r#"#!/bin/sh
if [ -f "$RSI_E2E_GIT_HOLD" ] && [ ! -f "$RSI_E2E_GIT_RELEASE" ]; then
    case " $* " in
        *" worktree list "*)
            : > "$RSI_E2E_GIT_ENTERED"
            while [ ! -f "$RSI_E2E_GIT_RELEASE" ]; do
                sleep 0.02
            done
            ;;
    esac
fi
exec "$RSI_E2E_REAL_GIT" "$@"
"#,
    )?;
    let mut permissions = fs::metadata(&wrapper)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(wrapper, permissions)?;
    Ok(())
}

fn test_error(message: impl Into<String>) -> Box<dyn Error + Send + Sync> {
    std::io::Error::new(std::io::ErrorKind::Other, message.into()).into()
}
