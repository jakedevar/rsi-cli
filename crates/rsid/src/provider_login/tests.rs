//! Tests of the provider CLI login probes (#2753). The CLIs are stub scripts:
//! no real `claude` or `codex` ever starts.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
use super::*;
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
use std::os::unix::fs::PermissionsExt;

/// Account text a real CLI prints; none of it may reach a record or a log.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
const CANARY_EMAIL: &str = "operator-canary@example.test";
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
const CANARY_ORG: &str = "Canary Org Ltd";
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
const CANARY_DIR: &str = "/home/canary/.claude";

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
fn stub(dir: &std::path::Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
fn service(
    claude: Option<PathBuf>,
    codex: Option<PathBuf>,
    timeout: Duration,
) -> ProviderLoginService {
    let mut binaries = HashMap::new();
    if let Some(path) = claude {
        binaries.insert(LoginProvider::Claude, path);
    }
    if let Some(path) = codex {
        binaries.insert(LoginProvider::Codex, path);
    }
    ProviderLoginService::new(
        Arc::new(CliLoginRunner::with_binaries(binaries, timeout)),
        Duration::from_secs(60),
    )
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
fn claude_status_body(logged_in: bool, method: &str) -> String {
    format!(
        "echo '{{\"loggedIn\":{logged_in},\"authMethod\":\"{method}\",\"email\":\"{CANARY_EMAIL}\",\"orgName\":\"{CANARY_ORG}\",\"configDir\":\"{CANARY_DIR}\"}}'"
    )
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn codex_exit_1_is_logged_out_and_refuses_the_launch_with_the_host_side_fix() {
    let dir = tempfile::tempdir().unwrap();
    let codex = stub(dir.path(), "codex", "echo 'Not logged in' >&2\nexit 1");
    let service = service(None, Some(codex), Duration::from_secs(5));

    let login = service.refresh(LoginProvider::Codex).await;
    assert_eq!(login.state, ProviderLoginState::LoggedOut);
    assert_eq!(login.method, ProviderLoginMethod::None);
    assert!(login.checked_at.is_some());

    let refusal = service.launch_refusal(LoginProvider::Codex).await.unwrap();
    assert!(refusal.starts_with("provider_login_required:"), "{refusal}");
    assert!(refusal.contains("`codex login`"), "{refusal}");
    // Claude was never probed: unknown admits.
    assert_eq!(service.launch_refusal(LoginProvider::Claude).await, None);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn codex_exit_0_is_logged_in_and_admits() {
    let dir = tempfile::tempdir().unwrap();
    let codex = stub(dir.path(), "codex", "echo 'Logged in using ChatGPT'");
    let service = service(None, Some(codex), Duration::from_secs(5));
    let login = service.refresh(LoginProvider::Codex).await;
    assert_eq!(login.state, ProviderLoginState::LoggedIn);
    assert_eq!(login.method, ProviderLoginMethod::Chatgpt);
    assert_eq!(service.launch_refusal(LoginProvider::Codex).await, None);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn a_probe_that_times_out_is_unknown_and_never_refuses_a_launch() {
    let dir = tempfile::tempdir().unwrap();
    let codex = stub(dir.path(), "codex", "exec sleep 30");
    let service = service(None, Some(codex), Duration::from_millis(300));
    let started = Instant::now();
    let login = service.refresh(LoginProvider::Codex).await;
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(login.state, ProviderLoginState::Unknown);
    assert_eq!(service.launch_refusal(LoginProvider::Codex).await, None);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn a_missing_cli_and_unreadable_output_are_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let broken = stub(
        dir.path(),
        "claude",
        "echo 'something else entirely'\nexit 7",
    );
    let service = service(Some(broken), None, Duration::from_secs(5));
    assert_eq!(
        service.refresh(LoginProvider::Claude).await.state,
        ProviderLoginState::Unknown
    );
    // No codex binary configured at all.
    assert_eq!(
        service.refresh(LoginProvider::Codex).await.state,
        ProviderLoginState::Unknown
    );
    // A Codex exit 1 that printed something other than "not logged in" is a
    // different failure, not a logout.
    let other = stub(
        dir.path(),
        "codex",
        "echo 'config.toml: parse error' >&2\nexit 1",
    );
    let service = service_for_codex(other);
    assert_eq!(
        service.refresh(LoginProvider::Codex).await.state,
        ProviderLoginState::Unknown
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
fn service_for_codex(codex: PathBuf) -> ProviderLoginService {
    service(None, Some(codex), Duration::from_secs(5))
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn claude_auth_status_json_maps_to_state_and_the_closed_method() {
    let dir = tempfile::tempdir().unwrap();
    let out = stub(
        dir.path(),
        "claude-out",
        &format!("{}\nexit 1", claude_status_body(false, "none")),
    );
    let service_out = service(Some(out), None, Duration::from_secs(5));
    let login = service_out.refresh(LoginProvider::Claude).await;
    assert_eq!(login.state, ProviderLoginState::LoggedOut);
    assert_eq!(login.method, ProviderLoginMethod::None);
    let refusal = service_out
        .launch_refusal(LoginProvider::Claude)
        .await
        .unwrap();
    assert!(refusal.contains("`claude auth login`"), "{refusal}");

    for (wire, method) in [
        ("claude.ai", ProviderLoginMethod::ClaudeAi),
        ("oauth_token", ProviderLoginMethod::OauthToken),
        ("api_key", ProviderLoginMethod::ApiKey),
        ("api_key_helper", ProviderLoginMethod::ApiKeyHelper),
        ("third_party", ProviderLoginMethod::ThirdParty),
        ("a-method-from-the-future", ProviderLoginMethod::Unknown),
    ] {
        let cli = stub(dir.path(), "claude-in", &claude_status_body(true, wire));
        let service = service(Some(cli), None, Duration::from_secs(5));
        let login = service.refresh(LoginProvider::Claude).await;
        assert_eq!(login.state, ProviderLoginState::LoggedIn, "{wire}");
        assert_eq!(login.method, method, "{wire}");
        assert_eq!(service.launch_refusal(LoginProvider::Claude).await, None);
    }
}

/// The record that is stored and served holds the state, the method and the
/// check time, and nothing else the CLI printed; the log names the same three
/// facts and none of the account text.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn the_served_record_and_the_log_carry_only_state_method_and_time() {
    #[derive(Clone, Default)]
    struct LogBuffer(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for LogBuffer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let logs = LogBuffer::default();
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .with_writer(move || writer.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let dir = tempfile::tempdir().unwrap();
    let claude = stub(
        dir.path(),
        "claude",
        &format!("{}\nexit 1", claude_status_body(false, "none")),
    );
    // Codex prints account text on a successful status too.
    let codex = stub(
        dir.path(),
        "codex",
        &format!("echo 'Logged in using ChatGPT as {CANARY_EMAIL} ({CANARY_ORG}) in {CANARY_DIR}'"),
    );
    let service = service(Some(claude), Some(codex), Duration::from_secs(5));
    let served = service
        .state_result(&ProviderLoginRequestV1::default(), true)
        .await;

    let json = serde_json::to_value(&served).unwrap();
    let rows = json["providers"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    for row in rows {
        let mut row_keys: Vec<_> = row.as_object().unwrap().keys().cloned().collect();
        row_keys.sort();
        assert_eq!(row_keys, ["login", "provider", "remedy"]);
        let mut login_keys: Vec<_> = row["login"].as_object().unwrap().keys().cloned().collect();
        login_keys.sort();
        assert_eq!(login_keys, ["checked_at", "method", "state"]);
    }
    assert_eq!(rows[0]["login"]["state"], "logged_out");
    assert_eq!(rows[1]["login"]["state"], "logged_in");
    assert_eq!(rows[1]["login"]["method"], "chatgpt");

    // The satellite health report is the same three facts per provider.
    let health = serde_json::to_value(service.health_reports()).unwrap();
    for entry in health.as_array().unwrap() {
        let mut keys: Vec<_> = entry.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, ["checked_at", "method", "provider", "state"]);
    }

    let captured = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert!(
        captured.contains("provider login probe finished"),
        "{captured}"
    );
    assert!(captured.contains("state=\"logged_out\""), "{captured}");
    for canary in [CANARY_EMAIL, CANARY_ORG, CANARY_DIR] {
        let payload = format!("{json}{health}{captured}");
        assert_eq!(payload.matches(canary).count(), 0, "{canary} leaked");
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
struct CountingRunner {
    calls: std::sync::atomic::AtomicUsize,
    outcome: Mutex<ProbeRun>,
    delay: Duration,
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
impl CountingRunner {
    fn new(outcome: ProbeRun, delay: Duration) -> Arc<Self> {
        Arc::new(Self {
            calls: std::sync::atomic::AtomicUsize::new(0),
            outcome: Mutex::new(outcome),
            delay,
        })
    }
    fn set(&self, outcome: ProbeRun) {
        *self.outcome.lock().unwrap() = outcome;
    }
    fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[async_trait::async_trait]
impl LoginProbeRunner for CountingRunner {
    async fn run(&self, _provider: LoginProvider) -> ProbeRun {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        tokio::time::sleep(self.delay).await;
        self.outcome.lock().unwrap().clone()
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
fn exited(code: i32, stdout: &str) -> ProbeRun {
    ProbeRun::Exited {
        code: Some(code),
        stdout: stdout.as_bytes().to_vec(),
        stderr: Vec::new(),
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn results_are_cached_and_one_probe_per_provider_is_in_flight() {
    let runner = CountingRunner::new(
        exited(0, "Logged in using ChatGPT"),
        Duration::from_millis(150),
    );
    let service = Arc::new(ProviderLoginService::new(
        runner.clone(),
        Duration::from_secs(60),
    ));

    // Ten concurrent callers share one probe.
    let callers = (0..10).map(|_| {
        let service = Arc::clone(&service);
        async move { service.ensure_fresh(LoginProvider::Codex).await }
    });
    let logins = futures::future::join_all(callers).await;
    assert!(
        logins
            .iter()
            .all(|login| login.state == ProviderLoginState::LoggedIn)
    );
    assert_eq!(runner.calls(), 1);
    assert_eq!(service.probes_started(), 1);

    // Inside the TTL a read does not probe; an operator refresh does.
    service.ensure_fresh(LoginProvider::Codex).await;
    assert_eq!(runner.calls(), 1);
    service.refresh(LoginProvider::Codex).await;
    assert_eq!(runner.calls(), 2);

    // Concurrent refreshes started together also share one probe.
    let before = runner.calls();
    let refreshes = (0..5).map(|_| {
        let service = Arc::clone(&service);
        async move { service.refresh(LoginProvider::Codex).await }
    });
    futures::future::join_all(refreshes).await;
    assert_eq!(runner.calls(), before + 1);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn a_stale_result_is_probed_again_after_the_ttl() {
    let runner = CountingRunner::new(exited(0, "Logged in using ChatGPT"), Duration::ZERO);
    let service = ProviderLoginService::new(runner.clone(), Duration::from_millis(50));
    service.ensure_fresh(LoginProvider::Codex).await;
    tokio::time::sleep(Duration::from_millis(120)).await;
    service.ensure_fresh(LoginProvider::Codex).await;
    assert_eq!(runner.calls(), 2);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn a_stale_logged_out_record_is_probed_before_a_launch_is_refused() {
    let runner = CountingRunner::new(exited(1, "Not logged in"), Duration::ZERO);
    let service = ProviderLoginService::new(runner.clone(), Duration::from_millis(50));
    service.refresh(LoginProvider::Codex).await;
    assert!(service.launch_refusal(LoginProvider::Codex).await.is_some());
    // The operator logs in; once the record is stale the next launch re-probes
    // and is admitted at once.
    runner.set(exited(0, "Logged in using ChatGPT"));
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert_eq!(service.launch_refusal(LoginProvider::Codex).await, None);
    assert_eq!(
        service.login(LoginProvider::Codex).state,
        ProviderLoginState::LoggedIn
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn logged_in_to_logged_out_is_one_transition_until_the_login_returns() {
    let runner = CountingRunner::new(exited(0, "Logged in using ChatGPT"), Duration::ZERO);
    let service = ProviderLoginService::new(runner.clone(), Duration::from_secs(60));

    // Logged out from the first probe is not a transition (nothing was lost).
    runner.set(exited(1, "Not logged in"));
    service.refresh(LoginProvider::Codex).await;
    assert!(service.logout_transitions().is_empty());

    runner.set(exited(0, "Logged in using ChatGPT"));
    service.refresh(LoginProvider::Codex).await;
    assert!(service.logout_transitions().is_empty());

    // A timeout between two definite answers does not hide the transition.
    runner.set(ProbeRun::TimedOut);
    service.refresh(LoginProvider::Codex).await;
    runner.set(exited(1, "Not logged in"));
    service.refresh(LoginProvider::Codex).await;
    let first = service.logout_transitions();
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].0, "codex");

    // A second logged-out probe keeps the same transition (one notice).
    service.refresh(LoginProvider::Codex).await;
    assert_eq!(service.logout_transitions(), first);

    // Logging back in clears it; a later logout is a new transition.
    runner.set(exited(0, "Logged in using ChatGPT"));
    service.refresh(LoginProvider::Codex).await;
    assert!(service.logout_transitions().is_empty());
    runner.set(exited(1, "Not logged in"));
    service.refresh(LoginProvider::Codex).await;
    let second = service.logout_transitions();
    assert_eq!(second.len(), 1);
    assert!(second[0].1 >= first[0].1);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[test]
fn only_a_cli_login_launch_needs_a_probed_login() {
    use rsi_common::types::SessionProvider;
    let claude = |model: Option<&str>| LoginProvider::for_launch(SessionProvider::Claude, model);
    assert_eq!(claude(None), Some(LoginProvider::Claude));
    assert_eq!(claude(Some("claude-sonnet-5")), Some(LoginProvider::Claude));
    // A Bedrock Claude model runs on the injected Bedrock credential.
    assert_eq!(claude(Some("us.anthropic.claude-sonnet-5-v1:0")), None);
    for provider in [SessionProvider::Codex, SessionProvider::CodexAppServer] {
        assert_eq!(
            LoginProvider::for_launch(provider, None),
            Some(LoginProvider::Codex)
        );
    }
    for provider in [
        SessionProvider::OpenRouter,
        SessionProvider::Pioneer,
        SessionProvider::Bedrock,
        SessionProvider::Local,
        SessionProvider::Harness,
    ] {
        assert_eq!(
            LoginProvider::for_launch(provider, None),
            None,
            "{provider:?}"
        );
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn the_operator_method_refuses_a_session_token_and_an_unknown_provider() {
    let runner = CountingRunner::new(exited(1, "Not logged in"), Duration::ZERO);
    let service = ProviderLoginService::new(runner.clone(), Duration::from_secs(60));

    let refused = handle_operator_method(
        &service,
        rsi_common::provider_login::METHOD_GET,
        true,
        &serde_json::Value::Null,
    )
    .await
    .unwrap_err();
    assert!(
        refused
            .to_string()
            .contains("not available to session-attributed callers"),
        "{refused}"
    );
    assert_eq!(runner.calls(), 0);

    let unknown = handle_operator_method(
        &service,
        rsi_common::provider_login::METHOD_GET,
        false,
        &serde_json::json!({"provider": "openrouter"}),
    )
    .await
    .unwrap_err();
    assert!(
        unknown
            .to_string()
            .contains("provider_login_unknown_provider"),
        "{unknown}"
    );

    let refreshed = handle_operator_method(
        &service,
        rsi_common::provider_login::METHOD_REFRESH,
        false,
        &serde_json::json!({"provider": "codex"}),
    )
    .await
    .unwrap();
    let result: ProviderLoginStateResultV1 = serde_json::from_value(refreshed).unwrap();
    assert_eq!(result.providers.len(), 1);
    assert_eq!(result.providers[0].provider, "codex");
    assert_eq!(
        result.providers[0].login.state,
        ProviderLoginState::LoggedOut
    );
    assert!(result.providers[0].remedy.contains("`codex login`"));
    assert_eq!(runner.calls(), 1);
}

/// #2810: the status command is built with the launch scrub, so every provider
/// credential variable a launch removes is removed from the probe too, while
/// nothing the launch keeps (the login and config directories) is touched.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[test]
fn the_probe_command_removes_every_credential_variable_a_launch_removes() {
    for provider in LoginProvider::ALL {
        let command = probe_command(std::path::Path::new("/bin/true"), provider);
        let removed: Vec<String> = command
            .as_std()
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(name, _)| name.to_string_lossy().into_owned())
            .collect();
        for name in crate::vault::slots::scrubbed_env_var_names() {
            assert!(
                removed.iter().any(|removed| removed == name),
                "{} probe keeps {name}",
                provider.name()
            );
        }
        for name in ["ANTHROPIC_API_KEY", "OPENAI_API_KEY"] {
            assert!(removed.iter().any(|removed| removed == name), "{name}");
        }
        for kept in ["HOME", "CLAUDE_CONFIG_DIR", "CODEX_HOME", "PATH"] {
            assert!(
                command
                    .as_std()
                    .get_envs()
                    .all(|(name, _)| name.to_string_lossy() != kept),
                "{kept} must stay inherited"
            );
        }
    }
}

/// Environment the re-executed fixture below reads; set only on that child.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
const ENV_FIXTURE_DIR: &str = "RSI_LOGIN_PROBE_ENV_FIXTURE_DIR";
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
const ENV_FIXTURE_CONTROL: &str = "RSI_LOGIN_PROBE_ENV_FIXTURE_CONTROL";

/// Child half of the test below: runs in a re-executed test process whose own
/// environment inherited synthetic provider keys (the daemon's situation),
/// probes the stub `claude` and writes the verdict. A plain `--ignored` run
/// without the fixture directory does nothing.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
#[ignore = "re-executed by an_inherited_api_key_never_reaches_the_probe_or_reads_as_logged_in"]
async fn inherited_env_probe_fixture() {
    let Some(dir) = std::env::var_os(ENV_FIXTURE_DIR).map(PathBuf::from) else {
        return;
    };
    assert!(
        std::env::var_os("ANTHROPIC_API_KEY").is_some(),
        "the fixture process itself holds the inherited key"
    );
    let runner = CliLoginRunner::with_binaries(
        HashMap::from([(LoginProvider::Claude, dir.join("claude"))]),
        Duration::from_secs(10),
    );
    let run = runner.run(LoginProvider::Claude).await;
    let (state, _) = interpret(LoginProvider::Claude, &run);
    std::fs::write(dir.join("verdict"), state.as_str()).unwrap();
}

/// #2810: a daemon that inherited `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` while
/// the CLI holds no login must not read `logged_in`: a launch scrubs those keys
/// and would fail logged out, so the probe sees exactly that environment.
///
/// #2826: the fixture never sees the caller's environment. It is re-executed
/// with `env_clear()` and only `PATH`, synthetic login/config directories, the
/// synthetic keys and the fixture's control values, and the stub records only
/// whether each variable under test is present (never a value it was not
/// given by this test). A synthetic canary stands in for an unrelated secret
/// the daemon holds: it reaches the probe like any ordinary variable, and
/// nothing the stub writes may contain it.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[test]
fn an_inherited_api_key_never_reaches_the_probe_or_reads_as_logged_in() {
    const CANARY_NAME: &str = "RSI_TEST_SECRET_CANARY";
    const CANARY_VALUE: &str = "canary-6d1f0c";
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let claude_config = dir.path().join("claude-config");
    let codex_home = dir.path().join("codex-home");
    for path in [&home, &claude_config, &codex_home] {
        std::fs::create_dir(path).unwrap();
    }
    let report = dir.path().join("probe-report");
    // An api-key-only CLI: logged in exactly when the key reaches it. It
    // reports presence only, plus whether the control and config values are
    // the synthetic ones this test set.
    stub(
        dir.path(),
        "claude",
        &format!(
            "report='{report}'\n\
             for name in ANTHROPIC_API_KEY OPENAI_API_KEY {CANARY_NAME}; do\n\
             eval \"value=\\${{$name+set}}\"\n\
             echo \"$name=${{value:-unset}}\" >> \"$report\"\n\
             done\n\
             [ \"${ENV_FIXTURE_CONTROL}\" = inherited ] && echo control=inherited >> \"$report\"\n\
             [ \"$CLAUDE_CONFIG_DIR\" = '{claude_config}' ] && echo claude_config=inherited >> \"$report\"\n\
             [ \"$HOME\" = '{home}' ] && echo home=inherited >> \"$report\"\n\
             if [ -n \"$ANTHROPIC_API_KEY\" ]; then\n\
             echo '{{\"loggedIn\":true,\"authMethod\":\"api_key\"}}'\n\
             else\n\
             echo '{{\"loggedIn\":false,\"authMethod\":\"none\"}}'\n\
             exit 1\n\
             fi",
            report = report.display(),
            claude_config = claude_config.display(),
            home = home.display(),
        ),
    );
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "provider_login::tests::inherited_env_probe_fixture",
            "--nocapture",
            "--test-threads=1",
        ])
        .env_clear()
        .env(
            "PATH",
            std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into()),
        )
        .env("HOME", &home)
        .env("CLAUDE_CONFIG_DIR", &claude_config)
        .env("CODEX_HOME", &codex_home)
        .env(ENV_FIXTURE_DIR, dir.path())
        .env(ENV_FIXTURE_CONTROL, "inherited")
        .env(CANARY_NAME, CANARY_VALUE)
        .env("ANTHROPIC_API_KEY", "sk-test-synthetic-anthropic")
        .env("OPENAI_API_KEY", "sk-test-synthetic-openai")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "fixture failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("verdict")).unwrap(),
        ProviderLoginState::LoggedOut.as_str(),
        "an api-key-only environment reads as the scrubbed launch sees it"
    );
    let report = std::fs::read_to_string(&report).unwrap();
    let canary_present = format!("{CANARY_NAME}=set");
    assert_eq!(
        report.lines().collect::<Vec<_>>(),
        [
            "ANTHROPIC_API_KEY=unset",
            "OPENAI_API_KEY=unset",
            canary_present.as_str(),
            "control=inherited",
            "claude_config=inherited",
            "home=inherited",
        ],
        "the keys are scrubbed; the daemon's ordinary environment and login \
         directories reach the probe"
    );
    // Nothing the stub or the fixture wrote or printed holds the canary's value.
    for printed in [&output.stdout, &output.stderr] {
        assert!(!String::from_utf8_lossy(printed).contains(CANARY_VALUE));
    }
    for entry in std::fs::read_dir(dir.path()).unwrap() {
        let path = entry.unwrap().path();
        if path.is_file() {
            let bytes = std::fs::read(&path).unwrap();
            assert!(
                !String::from_utf8_lossy(&bytes).contains(CANARY_VALUE),
                "{} holds the canary's value",
                path.display()
            );
        }
    }
}

/// Whether `pid` no longer runs (gone, or a zombie waiting for its reaper).
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
fn process_gone(pid: i32) -> bool {
    if nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None)
        == Err(nix::errno::Errno::ESRCH)
    {
        return true;
    }
    #[cfg(target_os = "linux")]
    {
        // `/proc/<pid>/stat` is `pid (comm) state ...`; `Z` is a zombie.
        if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            && let Some(rest) = stat.rsplit_once(')').map(|(_, rest)| rest.trim_start())
        {
            return rest.starts_with('Z') || rest.starts_with('X');
        }
    }
    false
}

/// The pid a stub recorded, then whether it is gone within a few seconds.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
async fn descendant_is_reaped(pid_file: &std::path::Path) -> bool {
    let pid: i32 = std::fs::read_to_string(pid_file)
        .expect("the stub recorded its descendant")
        .trim()
        .parse()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if process_gone(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    // Do not leave it behind for the rest of the run.
    let _ = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid),
        nix::sys::signal::Signal::SIGKILL,
    );
    false
}

/// #2812: a status command that forks a descendant and hangs is stopped as a
/// whole tree at the probe deadline, not only its immediate process.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn a_timed_out_probe_leaves_no_descendant_running() {
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("descendant.pid");
    let codex = stub(
        dir.path(),
        "codex",
        &format!("sleep 300 &\necho $! > '{}'\nwait", pid_file.display()),
    );
    let service = service(None, Some(codex), Duration::from_secs(2));
    let started = Instant::now();
    let login = service.refresh(LoginProvider::Codex).await;
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(login.state, ProviderLoginState::Unknown);
    assert!(
        descendant_is_reaped(&pid_file).await,
        "the forked descendant survived the probe deadline"
    );
}

/// #2812: a caller that stops waiting (the status read's own deadline, a
/// cancelled task) drops the probe; the dropped probe stops its whole tree.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn a_cancelled_probe_leaves_no_descendant_running() {
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("descendant.pid");
    let codex = stub(
        dir.path(),
        "codex",
        &format!("sleep 300 &\necho $! > '{}'\nwait", pid_file.display()),
    );
    let runner = CliLoginRunner::with_binaries(
        HashMap::from([(LoginProvider::Codex, codex)]),
        Duration::from_secs(60),
    );
    let probe = runner.run(LoginProvider::Codex);
    let pid_written = async {
        while !pid_file.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // The pid is written by `echo`; give it a moment to be complete.
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    tokio::select! {
        _ = probe => panic!("the hanging probe finished on its own"),
        () = pid_written => {}
    }
    assert!(
        descendant_is_reaped(&pid_file).await,
        "the forked descendant survived the cancelled probe"
    );
}

/// #2812: a status command that exits while a helper it forked keeps running
/// (holding the output pipe, or detached from it) is answered normally, and
/// the helper is stopped with it. Linux and macOS: the leader is observed
/// unreaped, so its group can still be signalled after it exits.
#[cfg(any(target_os = "linux", target_vendor = "apple"))]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn a_probe_that_exits_stops_the_helpers_it_left_running() {
    for (name, background) in [
        ("holds-pipe", "sleep 300 &"),
        ("detached", "sleep 300 </dev/null >/dev/null 2>&1 &"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("descendant.pid");
        let codex = stub(
            dir.path(),
            "codex",
            &format!(
                "{background}\necho $! > '{}'\necho 'Logged in using ChatGPT'",
                pid_file.display()
            ),
        );
        let service = service(None, Some(codex), Duration::from_secs(10));
        let started = Instant::now();
        let login = service.refresh(LoginProvider::Codex).await;
        assert!(started.elapsed() < Duration::from_secs(5), "{name}");
        assert_eq!(login.state, ProviderLoginState::LoggedIn, "{name}");
        assert!(descendant_is_reaped(&pid_file).await, "{name} survived");
    }
}

/// #130: an offline daemon starts no status command (`claude auth status`
/// reaches the provider's API); each login reads `unknown` and never refuses.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn an_offline_login_probe_starts_no_cli_and_reads_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("started");
    let body = format!("echo \"$0 $@\" >> '{}'\nexit 1", marker.display());
    let mut binaries = HashMap::new();
    binaries.insert(LoginProvider::Claude, stub(dir.path(), "claude", &body));
    binaries.insert(LoginProvider::Codex, stub(dir.path(), "codex", &body));
    let runner = CliLoginRunner::with_binaries(binaries, Duration::from_secs(5)).with_offline(true);
    let service = ProviderLoginService::new(Arc::new(runner), Duration::from_secs(60));
    for provider in LoginProvider::ALL {
        let login = service.refresh(provider).await;
        assert_eq!(login.state, ProviderLoginState::Unknown);
        assert_eq!(service.launch_refusal(provider).await, None);
    }
    assert!(!marker.exists(), "an offline probe started a CLI");
}
