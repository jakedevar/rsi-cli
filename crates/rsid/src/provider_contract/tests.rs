//! Tests of the provider CLI contract canary (#2755). The CLIs are stub
//! scripts: no real `claude` or `codex` starts, except in the two
//! real-binary tests, which skip when the CLI is absent.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
use super::*;
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
use std::os::unix::fs::PermissionsExt;

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
const HOOK_COMMAND: &str = "'/opt/rsi/rsi-rpc' boundary-mail-hook --mail-only";

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
fn stub(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
fn service_for(
    claude: Option<PathBuf>,
    codex: Option<PathBuf>,
    timeout: Duration,
) -> ProviderContractService {
    let mut binaries = HashMap::new();
    if let Some(path) = claude {
        binaries.insert(ContractProvider::Claude, path);
    }
    if let Some(path) = codex {
        binaries.insert(ContractProvider::Codex, path);
    }
    ProviderContractService::new(Arc::new(
        CliContractRunner::with_binaries(binaries, Some(HOOK_COMMAND.to_string()))
            .with_timeout(timeout),
    ))
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
const ON: CheckContext = CheckContext {
    enabled: true,
    claude_oauth_login: false,
    force: false,
};

/// A stub `codex`: `--version` answers `version`; `app-server` logs its argv and
/// answers `initialize` and `hooks/list` (from `hooks.json` beside the stub).
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
fn codex_stub(dir: &Path, version: &str, initialize: &str) -> PathBuf {
    stub(
        dir,
        "codex",
        &format!(
            r#"d=$(dirname "$0")
for a in "$@"; do case "$a" in --version) echo "{version}"; exit 0 ;; esac; done
echo "$@" >> "$d/argv"
while IFS= read -r line; do
  case "$line" in
    *'"initialize"'*) echo '{initialize}' ;;
    *'"hooks/list"'*) cat "$d/hooks.json"; echo ;;
  esac
done"#
        ),
    )
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
const OK_INITIALIZE: &str = r#"{"id":1,"result":{"userAgent":"stub","codexHome":"/x"}}"#;

/// `hooks.json` listing both session-flag hooks as `(trust status, hash)`,
/// and the probed lifecycle hook (#2766) trusted with RSI's hash.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
fn write_hooks(dir: &Path, mail: (&str, Option<&str>), custody: (&str, Option<&str>)) {
    write_hooks_with_lifecycle(dir, mail, custody, ("trusted", None));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
fn write_hooks_with_lifecycle(
    dir: &Path,
    mail: (&str, Option<&str>),
    custody: (&str, Option<&str>),
    lifecycle: (&str, Option<&str>),
) {
    let expected = crate::codex::codex_session_hook_expectations(HOOK_COMMAND);
    let lifecycle_expected = crate::codex::codex_lifecycle_hook_expectations(
        HOOK_COMMAND,
        &crate::codex::CODEX_PROBED_LIFECYCLE_EVENTS,
    );
    let row = |key: &str, (status, hash): (&str, Option<&str>), expected_hash: &str| {
        serde_json::json!({
            "key": key,
            "eventName": "x",
            "trustStatus": status,
            "currentHash": hash.unwrap_or(expected_hash),
            "source": "sessionFlags",
        })
    };
    let mut hooks = vec![
        row(expected[0].0, mail, &expected[0].1),
        row(expected[1].0, custody, &expected[1].1),
    ];
    for (key, hash) in &lifecycle_expected {
        hooks.push(row(key, lifecycle, hash));
    }
    write_hook_rows(dir, &hooks);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
fn write_hook_rows(dir: &Path, hooks: &[serde_json::Value]) {
    std::fs::write(
        dir.join("hooks.json"),
        serde_json::json!({"id": 2, "result": {"data": [
            {"cwd": "/tmp", "hooks": hooks, "warnings": [], "errors": []}
        ]}})
        .to_string(),
    )
    .unwrap();
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn trusted_hooks_with_rsis_hashes_are_ok_and_the_probe_passes_rsis_real_overrides() {
    let dir = tempfile::tempdir().unwrap();
    let codex = codex_stub(dir.path(), "codex-cli 9.9.9", OK_INITIALIZE);
    write_hooks(dir.path(), ("trusted", None), ("trusted", None));
    let service = service_for(None, Some(codex), Duration::from_secs(5));

    let contract = service.check(ContractProvider::Codex, ON).await;
    assert_eq!(
        contract.contract_state,
        ProviderContractState::Ok,
        "{contract:?}"
    );
    assert_eq!(contract.reason, None);
    assert_eq!(contract.cli_version.as_deref(), Some("codex-cli 9.9.9"));
    assert!(contract.checked_at.is_some());
    assert_eq!(service.codex_custody_refusal(true), None);

    // The app-server was started with the overrides a launch passes, ahead of
    // the subcommand: both hooks, the probed lifecycle hooks (#2766
    // PreCompact, hub #2919 SessionStart) and the trust state for all four.
    let argv = std::fs::read_to_string(dir.path().join("argv")).unwrap();
    let launch_overrides = crate::codex::codex_session_hook_overrides_with_lifecycle(
        HOOK_COMMAND,
        &crate::codex::CODEX_PROBED_LIFECYCLE_EVENTS,
    );
    assert_eq!(launch_overrides.len(), 5);
    assert!(
        launch_overrides
            .iter()
            .any(|value| value.starts_with("hooks.SessionStart=")),
        "{launch_overrides:?}"
    );
    for hook_override in launch_overrides {
        assert!(argv.contains(&hook_override), "{argv}");
    }
    assert!(argv.trim_end().ends_with("app-server"), "{argv}");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn an_untrusted_hook_degrades_codex_and_refuses_a_custody_launch_while_the_setting_is_on() {
    let dir = tempfile::tempdir().unwrap();
    let codex = codex_stub(dir.path(), "codex-cli 9.9.9", OK_INITIALIZE);
    write_hooks(dir.path(), ("trusted", None), ("untrusted", None));
    let service = service_for(None, Some(codex), Duration::from_secs(5));

    let contract = service.check(ContractProvider::Codex, ON).await;
    assert_eq!(contract.contract_state, ProviderContractState::Degraded);
    assert_eq!(contract.reason.as_deref(), Some("codex_hook_untrusted"));
    assert!(contract.custody_failed());

    let refusal = service.codex_custody_refusal(true).expect("refuses");
    assert!(refusal.starts_with("codex_hook_untrusted:"), "{refusal}");
    // The setting off: the provider reports degraded, the launch is admitted.
    assert_eq!(service.codex_custody_refusal(false), None);
    assert_eq!(
        service
            .degraded_episodes()
            .first()
            .map(|(p, r, _, _)| (*p, r.as_str())),
        Some(("codex", "codex_hook_untrusted"))
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn a_different_current_hash_or_a_modified_hook_is_a_hash_mismatch() {
    for (mail, custody) in [
        (("trusted", None), ("trusted", Some("sha256:deadbeef"))),
        (("modified", None), ("trusted", None)),
        (("trusted", Some("sha256:deadbeef")), ("untrusted", None)),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let codex = codex_stub(dir.path(), "codex-cli 9.9.9", OK_INITIALIZE);
        write_hooks(dir.path(), mail, custody);
        let service = service_for(None, Some(codex), Duration::from_secs(5));
        let contract = service.check(ContractProvider::Codex, ON).await;
        assert_eq!(
            contract.reason.as_deref(),
            Some("codex_hook_hash_mismatch"),
            "{mail:?} {custody:?}"
        );
        let refusal = service.codex_custody_refusal(true).expect("refuses");
        assert!(
            refusal.starts_with("codex_hook_hash_mismatch:"),
            "{refusal}"
        );
    }
}

/// #2766: a lifecycle hook Codex does not trust (or hashes differently, or
/// does not list) degrades the contract under its own reason, which is not a
/// custody reason: a Codex launch is never refused for it.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn an_untrusted_lifecycle_hook_degrades_codex_but_never_refuses_a_launch() {
    for lifecycle in [
        ("untrusted", None),
        ("trusted", Some("sha256:deadbeef")),
        ("modified", None),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let codex = codex_stub(dir.path(), "codex-cli 9.9.9", OK_INITIALIZE);
        write_hooks_with_lifecycle(dir.path(), ("trusted", None), ("trusted", None), lifecycle);
        let service = service_for(None, Some(codex), Duration::from_secs(5));
        let contract = service.check(ContractProvider::Codex, ON).await;
        assert_eq!(contract.contract_state, ProviderContractState::Degraded);
        assert_eq!(
            contract.reason.as_deref(),
            Some("codex_lifecycle_hook_untrusted"),
            "{lifecycle:?}"
        );
        assert!(!contract.custody_failed());
        assert_eq!(service.codex_custody_refusal(true), None, "{lifecycle:?}");
    }
    // A custody failure outranks a lifecycle failure.
    let dir = tempfile::tempdir().unwrap();
    let codex = codex_stub(dir.path(), "codex-cli 9.9.9", OK_INITIALIZE);
    write_hooks_with_lifecycle(
        dir.path(),
        ("trusted", None),
        ("untrusted", None),
        ("untrusted", None),
    );
    let service = service_for(None, Some(codex), Duration::from_secs(5));
    let contract = service.check(ContractProvider::Codex, ON).await;
    assert_eq!(contract.reason.as_deref(), Some("codex_hook_untrusted"));
    assert!(service.codex_custody_refusal(true).is_some());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn a_hook_codex_does_not_list_is_missing() {
    let dir = tempfile::tempdir().unwrap();
    let codex = codex_stub(dir.path(), "codex-cli 9.9.9", OK_INITIALIZE);
    let expected = crate::codex::codex_session_hook_expectations(HOOK_COMMAND);
    write_hook_rows(
        dir.path(),
        &[serde_json::json!({
            "key": expected[0].0, "trustStatus": "trusted", "currentHash": expected[0].1,
        })],
    );
    let service = service_for(None, Some(codex), Duration::from_secs(5));
    let contract = service.check(ContractProvider::Codex, ON).await;
    assert_eq!(contract.reason.as_deref(), Some("codex_hook_missing"));
    assert!(service.codex_custody_refusal(true).is_some());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn a_refused_handshake_is_a_drift_that_never_refuses_a_launch() {
    let dir = tempfile::tempdir().unwrap();
    let codex = codex_stub(
        dir.path(),
        "codex-cli 9.9.9",
        r#"{"id":1,"error":{"code":-32600,"message":"Invalid request: missing field"}}"#,
    );
    let service = service_for(None, Some(codex), Duration::from_secs(5));
    let contract = service.check(ContractProvider::Codex, ON).await;
    assert_eq!(contract.contract_state, ProviderContractState::Degraded);
    assert_eq!(contract.reason.as_deref(), Some("codex_handshake_failed"));
    assert!(!contract.custody_failed());
    assert_eq!(service.codex_custody_refusal(true), None);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn a_hung_app_server_is_unchecked_not_a_drift_and_is_retried_later() {
    let dir = tempfile::tempdir().unwrap();
    let codex = stub(
        dir.path(),
        "codex",
        r#"for a in "$@"; do case "$a" in --version) echo "codex-cli 9.9.9"; exit 0 ;; esac; done
sleep 30"#,
    );
    let service = service_for(None, Some(codex), Duration::from_millis(600));
    let contract = service.check(ContractProvider::Codex, ON).await;
    assert_eq!(contract.contract_state, ProviderContractState::Unchecked);
    assert_eq!(contract.reason.as_deref(), Some("canary_failed"));
    assert_eq!(contract.cli_version.as_deref(), Some("codex-cli 9.9.9"));
    assert_eq!(service.codex_custody_refusal(true), None);
    assert!(service.degraded_episodes().is_empty());
    // Not repeated at once (no loop), but due again after the retry window.
    let _ = service.check(ContractProvider::Codex, ON).await;
    assert_eq!(service.runs_started(), 1);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn a_version_change_reruns_the_canary_once_and_nothing_else_does() {
    let dir = tempfile::tempdir().unwrap();
    let codex = codex_stub(dir.path(), "codex-cli 9.9.9", OK_INITIALIZE);
    write_hooks(dir.path(), ("trusted", None), ("trusted", None));
    let service = service_for(None, Some(codex.clone()), Duration::from_secs(5));

    let first = service.check(ContractProvider::Codex, ON).await;
    assert_eq!(first.contract_state, ProviderContractState::Ok);
    assert_eq!(service.runs_started(), 1);
    // Passes with the binary unchanged run nothing.
    for _ in 0..3 {
        service.ensure_current_all(ON).await;
    }
    assert_eq!(service.runs_started(), 1);

    // The CLI is replaced by one that no longer trusts the custody hook.
    write_hooks(dir.path(), ("trusted", None), ("untrusted", None));
    codex_stub(dir.path(), "codex-cli 10.0.0-new", OK_INITIALIZE);
    let second = service.check(ContractProvider::Codex, ON).await;
    assert_eq!(service.runs_started(), 2);
    assert_eq!(second.contract_state, ProviderContractState::Degraded);
    assert_eq!(second.cli_version.as_deref(), Some("codex-cli 10.0.0-new"));
    assert!(second.checked_at >= first.checked_at);
    for _ in 0..3 {
        service.ensure_current_all(ON).await;
    }
    assert_eq!(service.runs_started(), 2, "one run per version change");

    // A rewritten binary that reports the same version is not a new contract.
    codex_stub(dir.path(), "codex-cli 10.0.0-new", OK_INITIALIZE);
    {
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&codex)
            .unwrap();
        file.write_all(b"# rebuilt\n").unwrap();
    }
    let third = service.check(ContractProvider::Codex, ON).await;
    assert_eq!(service.runs_started(), 2);
    assert_eq!(third.contract_state, ProviderContractState::Degraded);
    assert_eq!(third.checked_at, second.checked_at, "the verdict is kept");

    // An operator refresh runs regardless.
    let forced = service
        .check(ContractProvider::Codex, CheckContext { force: true, ..ON })
        .await;
    assert_eq!(service.runs_started(), 3);
    assert_eq!(forced.contract_state, ProviderContractState::Degraded);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn a_disabled_canary_runs_no_cli_and_is_unchecked_until_it_is_turned_back_on() {
    let dir = tempfile::tempdir().unwrap();
    let codex = codex_stub(dir.path(), "codex-cli 9.9.9", OK_INITIALIZE);
    write_hooks(dir.path(), ("trusted", None), ("untrusted", None));
    let service = service_for(None, Some(codex), Duration::from_secs(5));
    let off = CheckContext {
        enabled: false,
        ..ON
    };

    let contract = service.check(ContractProvider::Codex, off).await;
    assert_eq!(contract.contract_state, ProviderContractState::Unchecked);
    assert_eq!(contract.reason.as_deref(), Some("canary_disabled"));
    assert_eq!(service.runs_started(), 0);
    assert!(!dir.path().join("argv").exists(), "no app-server started");
    assert_eq!(service.codex_custody_refusal(true), None);

    let on = service.check(ContractProvider::Codex, ON).await;
    assert_eq!(on.reason.as_deref(), Some("codex_hook_untrusted"));
    // Turning it off again drops the verdict, so a stale one cannot refuse.
    let off_again = service.check(ContractProvider::Codex, off).await;
    assert_eq!(off_again.reason.as_deref(), Some("canary_disabled"));
    assert_eq!(service.codex_custody_refusal(true), None);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn a_cli_that_is_not_installed_is_unchecked_and_admits() {
    let service = service_for(None, None, Duration::from_secs(5));
    for provider in ContractProvider::ALL {
        let contract = service.check(provider, ON).await;
        assert_eq!(contract.contract_state, ProviderContractState::Unchecked);
        assert_eq!(contract.reason.as_deref(), Some("cli_unavailable"));
        assert_eq!(contract.cli_version, None);
    }
    assert_eq!(service.codex_custody_refusal(true), None);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[test]
fn a_verdict_taken_against_a_replaced_binary_no_longer_refuses() {
    struct Prints(Mutex<Option<String>>);
    #[async_trait::async_trait]
    impl ContractRunner for Prints {
        fn fingerprint(&self, _: ContractProvider) -> Option<String> {
            self.0.lock().unwrap().clone()
        }
        async fn cli_version(&self, _: ContractProvider) -> Option<String> {
            None
        }
        async fn codex_hooks(&self) -> Result<(Vec<HookRow>, Vec<(String, String)>), ProbeFailure> {
            Err(ProbeFailure::Inconclusive)
        }
        async fn claude_help(&self) -> Option<String> {
            None
        }
        async fn claude_flag_probe(&self, _: &str) -> FlagProbe {
            FlagProbe::Inconclusive
        }
        async fn claude_initialize(&self) -> Result<Value, ProbeFailure> {
            Err(ProbeFailure::Inconclusive)
        }
    }
    let runner = Arc::new(Prints(Mutex::new(Some("v1".into()))));
    let service = ProviderContractService::new(runner.clone());
    service.seed_for_test(
        ContractProvider::Codex,
        ProviderContractState::Degraded,
        Some("codex_hook_untrusted"),
    );
    assert!(service.codex_custody_refusal(true).is_some());
    *runner.0.lock().unwrap() = Some("v2".into());
    assert_eq!(
        service.codex_custody_refusal(true),
        None,
        "the next canary decides for the new binary"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
fn all_flags_help(except: &[&str]) -> String {
    crate::claude::CLAUDE_LAUNCH_FLAGS
        .iter()
        .filter(|flag| !except.contains(flag))
        .map(|flag| format!("  {flag} <value>   something\n"))
        .collect()
}

/// A stub `claude`: `--help` prints `help`; `-p <flag>` logs the flag and
/// rejects `rejected` as an unknown option, else refuses for want of a prompt;
/// stream-json mode (`--input-format`) answers `initialize` with `init`.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
fn claude_stub(dir: &Path, help: &str, rejected: &str, init: &str) -> PathBuf {
    std::fs::write(dir.join("help.txt"), help).unwrap();
    std::fs::write(dir.join("init.json"), init).unwrap();
    stub(
        dir,
        "claude",
        &format!(
            r#"d=$(dirname "$0")
case "$1" in
  --version) echo "9.9.9 (Claude Code)"; exit 0 ;;
  --help) cat "$d/help.txt"; exit 0 ;;
  -p)
    if [ "$2" = "--input-format" ]; then
      cat > /dev/null
      cat "$d/init.json"; echo
      exit 0
    fi
    echo "$2" >> "$d/probed"
    if [ "$2" = "{rejected}" ]; then echo "error: unknown option '$2'" >&2; exit 1; fi
    echo "Error: Input must be provided either through stdin or as a prompt argument" >&2
    exit 1 ;;
esac
exit 2"#
        ),
    )
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
fn init_reply(inner: serde_json::Value) -> String {
    serde_json::json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": crate::claude::model_discovery::PROBE_REQUEST_ID,
            "response": inner,
        }
    })
    .to_string()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
fn healthy_init() -> String {
    init_reply(serde_json::json!({
        "claude_code_version": "9.9.9",
        "capabilities": ["ui_surface_v1"],
        "account": {"email": "someone@example.test", "subscriptionType": "max"},
        "models": [],
    }))
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn a_flag_missing_from_help_that_the_cli_rejects_is_claude_flag_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let claude = claude_stub(
        dir.path(),
        &all_flags_help(&["--effort", "--max-turns"]),
        "--effort",
        &healthy_init(),
    );
    let service = service_for(Some(claude), None, Duration::from_secs(5));
    let contract = service.check(ContractProvider::Claude, ON).await;
    assert_eq!(contract.contract_state, ProviderContractState::Degraded);
    assert_eq!(
        contract.reason.as_deref(),
        Some("claude_flag_unknown:--effort")
    );
    assert_eq!(contract.cli_version.as_deref(), Some("9.9.9 (Claude Code)"));
    assert_eq!(
        contract.detail.as_deref(),
        Some(
            "probe=flag element=--effort expected=listed_in_help_or_accepted \
             observed=unknown_option cli_version=9.9.9 (Claude Code)"
        )
    );
    // A degraded Claude never refuses Codex custody.
    assert!(!contract.custody_failed());
    // Both help-less flags were probed; `--max-turns` was accepted (hidden).
    let probed = std::fs::read_to_string(dir.path().join("probed")).unwrap();
    assert!(probed.lines().any(|line| line == "--max-turns"), "{probed}");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn a_hidden_flag_the_cli_accepts_passes_and_only_hidden_flags_are_probed() {
    let dir = tempfile::tempdir().unwrap();
    let claude = claude_stub(
        dir.path(),
        &all_flags_help(&["--max-turns"]),
        "--no-such-flag",
        &healthy_init(),
    );
    let service = service_for(Some(claude), None, Duration::from_secs(5));
    let contract = service.check(ContractProvider::Claude, ON).await;
    assert_eq!(
        contract.contract_state,
        ProviderContractState::Ok,
        "{contract:?}"
    );
    let probed = std::fs::read_to_string(dir.path().join("probed")).unwrap();
    assert_eq!(probed.lines().collect::<Vec<_>>(), ["--max-turns"]);

    // A hidden flag the CLI drops is flagged by name.
    let dir = tempfile::tempdir().unwrap();
    let claude = claude_stub(
        dir.path(),
        &all_flags_help(&["--max-turns"]),
        "--max-turns",
        &healthy_init(),
    );
    let service = service_for(Some(claude), None, Duration::from_secs(5));
    let contract = service.check(ContractProvider::Claude, ON).await;
    assert_eq!(
        contract.reason.as_deref(),
        Some("claude_flag_unknown:--max-turns")
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn an_initialize_reply_without_the_contract_fields_or_at_all_is_a_drift() {
    for (init, reason) in [
        (String::new(), "claude_initialize_failed"),
        (
            serde_json::json!({
                "type": "control_response",
                "response": {"subtype": "error", "request_id": "x", "error": "nope"},
            })
            .to_string(),
            "claude_initialize_failed",
        ),
        (
            init_reply(serde_json::json!({"capabilities": []})),
            "claude_contract_shape",
        ),
        (
            init_reply(serde_json::json!({"claude_code_version": "9.9.9"})),
            "claude_contract_shape",
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let claude = claude_stub(dir.path(), &all_flags_help(&[]), "--none", &init);
        let service = service_for(Some(claude), None, Duration::from_secs(5));
        let contract = service.check(ContractProvider::Claude, ON).await;
        assert_eq!(contract.reason.as_deref(), Some(reason), "{init}");
    }
}

/// Hub #2981: a degraded canary names the failing contract element in one
/// bounded line with the CLI version, and never repeats what the CLI printed.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[test]
fn a_wrong_shape_initialize_reply_names_the_first_failing_element() {
    let cases = [
        (
            serde_json::json!({"capabilities": []}),
            "probe=initialize element=claude_code_version expected=non_empty_string observed=missing",
        ),
        (
            serde_json::json!({"claude_code_version": "  ", "capabilities": []}),
            "probe=initialize element=claude_code_version expected=non_empty_string observed=empty_string",
        ),
        (
            serde_json::json!({"claude_code_version": 2, "capabilities": []}),
            "probe=initialize element=claude_code_version expected=non_empty_string observed=number",
        ),
        (
            serde_json::json!({"claude_code_version": "2.1.292", "capabilities": {"SECRET-TOKEN": 1}}),
            "probe=initialize element=capabilities expected=array observed=object",
        ),
        (
            serde_json::json!({"claude_code_version": "2.1.292"}),
            "probe=initialize element=capabilities expected=array observed=missing",
        ),
    ];
    for (init, detail) in cases {
        let mismatch = claude_initialize_mismatch(&init, false).expect("a mismatch");
        assert_eq!(mismatch.reason, REASON_CLAUDE_CONTRACT_SHAPE, "{init}");
        assert_eq!(mismatch.detail, detail, "{init}");
        assert!(!mismatch.detail.contains("SECRET-TOKEN"));
        assert_eq!(
            evaluate_claude_initialize(&init, false),
            Some(REASON_CLAUDE_CONTRACT_SHAPE)
        );
    }
    let ambient = claude_initialize_mismatch(
        &serde_json::json!({"claude_code_version": "1", "capabilities": [], "account": {}}),
        true,
    )
    .expect("a mismatch");
    assert_eq!(ambient.reason, REASON_CLAUDE_AMBIENT_AUTH_DROPPED);
    assert_eq!(
        ambient.detail,
        "probe=initialize element=account expected=email_or_organization_or_subscriptionType observed=object"
    );
    assert_eq!(
        claude_flag_detail("--max-turns"),
        "probe=flag element=--max-turns expected=listed_in_help_or_accepted observed=unknown_option"
    );

    // The CLI version joins the line, which stays within the bound.
    let line = detail_with_version(&ambient.detail, "2.1.292 (Claude Code)");
    assert!(
        line.ends_with(" cli_version=2.1.292 (Claude Code)"),
        "{line}"
    );
    let huge = detail_with_version(&"x".repeat(1000), &format!("v{}\n", "9".repeat(500)));
    assert!(huge.len() <= MAX_CONTRACT_DETAIL_BYTES, "{}", huge.len());
    assert!(huge.contains(" cli_version=v99"), "{huge}");
    assert!(!huge.contains('\n'));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn a_degraded_canary_serves_its_detail_on_the_contract_and_the_episode() {
    let dir = tempfile::tempdir().unwrap();
    let wrong_shape = init_reply(serde_json::json!({
        "claude_code_version": "9.9.9",
        "capabilities": "not-an-array",
    }));
    let claude = claude_stub(dir.path(), &all_flags_help(&[]), "--none", &wrong_shape);
    let service = service_for(Some(claude), None, Duration::from_secs(5));
    let contract = service.check(ContractProvider::Claude, ON).await;
    assert_eq!(contract.reason.as_deref(), Some("claude_contract_shape"));
    let expected = "probe=initialize element=capabilities expected=array observed=string \
                    cli_version=9.9.9 (Claude Code)";
    assert_eq!(contract.detail.as_deref(), Some(expected));
    assert!(contract.detail.as_deref().unwrap().len() <= MAX_CONTRACT_DETAIL_BYTES);

    // The status report, the operator state and the notice episode carry it.
    let status = service.contracts_for_status();
    let claude_status = status.iter().find(|(name, _)| name == "claude").unwrap();
    assert_eq!(claude_status.1.detail.as_deref(), Some(expected));
    let result = service
        .state_result(&ProviderContractRequestV1::default(), ON, true)
        .await;
    let json = serde_json::to_value(&result).unwrap();
    assert_eq!(json["providers"][0]["contract"]["detail"], expected);
    let episodes = service.degraded_episodes();
    assert_eq!(episodes.len(), 1);
    assert_eq!(episodes[0].0, "claude");
    assert_eq!(episodes[0].3.as_deref(), Some(expected));

    // A healthy result carries no detail.
    let healthy_dir = tempfile::tempdir().unwrap();
    let healthy = claude_stub(
        healthy_dir.path(),
        &all_flags_help(&[]),
        "--none",
        &healthy_init(),
    );
    let service = service_for(Some(healthy), None, Duration::from_secs(5));
    let contract = service.check(ContractProvider::Claude, ON).await;
    assert_eq!(contract.contract_state, ProviderContractState::Ok);
    assert_eq!(contract.detail, None);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn an_oauth_login_the_non_bare_launch_cannot_see_is_ambient_auth_dropped() {
    let bare_shaped = init_reply(serde_json::json!({
        "claude_code_version": "9.9.9",
        "capabilities": [],
        "account": {"tokenSource": "none", "apiProvider": "firstParty"},
    }));
    let dir = tempfile::tempdir().unwrap();
    let claude = claude_stub(dir.path(), &all_flags_help(&[]), "--none", &bare_shaped);
    let service = service_for(Some(claude), None, Duration::from_secs(5));
    let oauth = CheckContext {
        claude_oauth_login: true,
        ..ON
    };
    let contract = service.check(ContractProvider::Claude, oauth).await;
    assert_eq!(
        contract.reason.as_deref(),
        Some("claude_ambient_auth_dropped")
    );

    // The same reply on a host that is not logged in with OAuth is not drift.
    let service = service_for(
        Some(dir.path().join("claude")),
        None,
        Duration::from_secs(5),
    );
    let contract = service.check(ContractProvider::Claude, ON).await;
    assert_eq!(contract.contract_state, ProviderContractState::Ok);

    // An OAuth login the launch sees (account e-mail) is fine.
    let dir = tempfile::tempdir().unwrap();
    let claude = claude_stub(dir.path(), &all_flags_help(&[]), "--none", &healthy_init());
    let service = service_for(Some(claude), None, Duration::from_secs(5));
    let contract = service.check(ContractProvider::Claude, oauth).await;
    assert_eq!(contract.contract_state, ProviderContractState::Ok);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[test]
fn pure_helpers_match_whole_tokens_and_the_cli_wording() {
    let help = "  -p, --print           Print\n  --model <model>  Model\n  --max-turns-foo <n>\n  --disallowedTools, --disallowed-tools <tools...>  x\n";
    assert!(help_lists_flag(help, "-p"));
    assert!(help_lists_flag(help, "--model"));
    assert!(help_lists_flag(help, "--disallowedTools"));
    assert!(
        !help_lists_flag(help, "--max-turns"),
        "not inside --max-turns-foo"
    );
    assert!(!help_lists_flag(help, "--mod"));
    assert!(unknown_option_rejected(
        b"error: unknown option '--max-turns'\n",
        "--max-turns"
    ));
    assert!(!unknown_option_rejected(
        b"error: unknown option '--max-turns-foo'\n",
        "--max-turns"
    ));
    assert!(!unknown_option_rejected(
        b"Error: Input must be provided",
        "--max-turns"
    ));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn the_operator_state_names_the_settings_and_refresh_forces_a_run() {
    let dir = tempfile::tempdir().unwrap();
    let codex = codex_stub(dir.path(), "codex-cli 9.9.9", OK_INITIALIZE);
    write_hooks(dir.path(), ("trusted", None), ("trusted", None));
    let service = service_for(None, Some(codex), Duration::from_secs(5));

    let request = ProviderContractRequestV1 {
        provider: Some("codex".into()),
    };
    let result = service.state_result(&request, ON, true).await;
    assert!(result.canary_enabled && result.codex_refuse_untrusted_custody_hook);
    assert_eq!(result.providers.len(), 1);
    assert_eq!(result.providers[0].provider, "codex");
    assert_eq!(
        result.providers[0].contract.contract_state,
        ProviderContractState::Ok
    );
    assert_eq!(service.runs_started(), 1);
    let again = service.state_result(&request, ON, true).await;
    assert_eq!(service.runs_started(), 1, "a read does not rerun");
    assert_eq!(again.providers[0].contract, result.providers[0].contract);
    let forced = service
        .state_result(&request, CheckContext { force: true, ..ON }, false)
        .await;
    assert_eq!(service.runs_started(), 2);
    assert!(!forced.codex_refuse_untrusted_custody_hook);
    // Every provider when none is named.
    let all = service
        .state_result(&ProviderContractRequestV1::default(), ON, true)
        .await;
    assert_eq!(all.providers.len(), 2);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn the_operator_methods_refuse_a_tokened_caller_and_unknown_fields() {
    let service = ProviderContractService::new(Arc::new(NoCliRunner));
    let login = crate::provider_login::ProviderLoginService::for_manager();
    let runtime = crate::config::RuntimeConfig::from_config(&crate::config::Config::default());
    let call = |method: &'static str, token: bool, params: serde_json::Value| {
        let (service, login, runtime) = (&service, &login, &runtime);
        async move { handle_operator_method(service, login, runtime, method, token, &params).await }
    };
    let method = rsi_common::provider_contract::METHOD_GET;
    assert!(call(method, true, serde_json::Value::Null).await.is_err());
    assert!(
        call(method, false, serde_json::json!({"caller": "x"}))
            .await
            .is_err()
    );
    assert!(
        call(method, false, serde_json::json!({"provider": "openrouter"}))
            .await
            .is_err()
    );
    let ok = call(method, false, serde_json::Value::Null).await.unwrap();
    assert_eq!(ok["canary_enabled"], true);
    assert_eq!(ok["codex_refuse_untrusted_custody_hook"], true);
    assert_eq!(ok["providers"].as_array().unwrap().len(), 2);
    assert_eq!(
        ok["providers"][0]["contract"]["contract_state"],
        "unchecked"
    );
    assert!(is_operator_method(method));
    assert!(is_operator_method(
        rsi_common::provider_contract::METHOD_REFRESH
    ));
    assert!(!is_operator_method("AgentGetProviderStatus"));
}

/// Real-binary test (#2755 acceptance 2): the canary against the installed
/// codex-cli. Skipped when `codex` is not installed. On the validated 0.163.0
/// the mail, custody, `PreCompact` (#2766) and `SessionStart` (hub #2919)
/// lifecycle hooks must be trusted
/// with RSI's hashes; on any other version the
/// verdict is printed and only its shape is asserted, so a CLI update shows up
/// in the daemon's provider status (and a failing run here is the cue to read
/// it) without turning an unrelated landing red.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn the_installed_codex_cli_honours_the_hook_contract() {
    let Some(binary) = crate::provider_cli::resolve("codex") else {
        eprintln!("skipped: codex is not installed");
        return;
    };
    let service = ProviderContractService::new(Arc::new(CliContractRunner::new()));
    let contract = service.check(ContractProvider::Codex, ON).await;
    eprintln!("{} -> {contract:?}", binary.display());
    assert!(contract.checked_at.is_some());
    let version = contract.cli_version.as_deref().expect("a runnable codex");
    if version == "codex-cli 0.163.0" {
        assert_eq!(
            contract.contract_state,
            ProviderContractState::Ok,
            "{contract:?}"
        );
        assert_eq!(contract.reason, None);
        assert_eq!(service.codex_custody_refusal(true), None);
    }
}

/// Real-binary test: the canary against the installed Claude Code. Skipped
/// when `claude` is not installed; asserts the verdict only on the validated
/// 2.1.301 (see the codex test).
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn the_installed_claude_cli_knows_every_flag_rsi_passes() {
    let Some(binary) = crate::provider_cli::resolve("claude") else {
        eprintln!("skipped: claude is not installed");
        return;
    };
    let service = ProviderContractService::new(Arc::new(CliContractRunner::new()));
    let contract = service.check(ContractProvider::Claude, ON).await;
    eprintln!("{} -> {contract:?}", binary.display());
    assert!(contract.checked_at.is_some());
    let version = contract.cli_version.as_deref().expect("a runnable claude");
    if version.starts_with("2.1.301 ") {
        assert_eq!(
            contract.contract_state,
            ProviderContractState::Ok,
            "{contract:?}"
        );
    }
}

/// hub #2765: an operator launch-override flag is checked against the
/// installed Claude CLI the way the canary checks RSI's own flags: listed in
/// `--help` passes unprobed, a hidden flag the CLI accepts passes, a flag the
/// CLI rejects as an unknown option is named, and no CLI fails open.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn launch_override_flags_are_checked_against_the_installed_claude_cli() {
    let dir = tempfile::tempdir().unwrap();
    let help = "  --betas <betas...>   Beta headers\n  --fallback-model <model>  Fallback\n";
    let claude = claude_stub(dir.path(), help, "--max-budget-usd", &healthy_init());
    let service = service_for(Some(claude), None, Duration::from_secs(5));
    let unknown = service
        .claude_flags_unknown_to_installed_cli(&[
            "--betas",
            "--fallback-model",
            "--append-system-prompt",
            "--max-budget-usd",
        ])
        .await;
    assert_eq!(unknown, vec!["--max-budget-usd".to_string()]);
    let probed = std::fs::read_to_string(dir.path().join("probed")).unwrap();
    assert_eq!(
        probed.lines().collect::<Vec<_>>(),
        ["--append-system-prompt", "--max-budget-usd"],
        "only the flags missing from --help are probed"
    );
    assert!(
        service
            .claude_flags_unknown_to_installed_cli(&[])
            .await
            .is_empty()
    );

    let absent = service_for(None, None, Duration::from_secs(5));
    assert!(
        absent
            .claude_flags_unknown_to_installed_cli(&["--max-budget-usd"])
            .await
            .is_empty(),
        "no installed CLI: unchecked, never a refusal"
    );
}

/// A fixture `ConfigReadResponse.json`: `Config` is open (like the real one: a
/// partial view), `agents` is a closed object.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
const CODEX_CONFIG_SCHEMA: &str = r##"{
  "definitions": {
    "Config": {
      "additionalProperties": true,
      "properties": {
        "model_verbosity": {"anyOf": [{"$ref": "#/definitions/Verbosity"}, {"type": "null"}]},
        "agents": {"anyOf": [{"$ref": "#/definitions/AgentsConfig"}, {"type": "null"}]}
      },
      "type": "object"
    },
    "Verbosity": {"enum": ["low", "medium", "high"], "type": "string"},
    "AgentsConfig": {
      "additionalProperties": false,
      "properties": {"max_threads": {"type": "integer"}},
      "type": "object"
    }
  },
  "properties": {},
  "type": "object"
}"##;

/// A stub `codex` for the launch-override key check (#281): `app-server
/// generate-json-schema --out DIR` logs `schema` to `calls` and writes
/// `schema.json` (beside the stub) as `DIR/v2/ConfigReadResponse.json` unless
/// `schema_fails`; `-c KEY={..} features list` logs `KEY` to `probed` and fails
/// naming the key when `typed.txt` lists it (`broken_probe`: fails without
/// naming it).
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
fn codex_keys_stub(
    dir: &Path,
    schema: &str,
    typed: &[&str],
    schema_fails: bool,
    broken_probe: bool,
) -> PathBuf {
    std::fs::write(dir.join("schema.json"), schema).unwrap();
    std::fs::write(dir.join("typed.txt"), typed.join("\n")).unwrap();
    let schema_step = if schema_fails {
        "echo boom >&2; exit 1".to_string()
    } else {
        r#"out="$4"; mkdir -p "$out/v2"; cp "$d/schema.json" "$out/v2/ConfigReadResponse.json"; exit 0"#
            .to_string()
    };
    let probe_step = if broken_probe {
        r#"echo "Error: failed to load bootstrap configuration" >&2; exit 1"#.to_string()
    } else {
        r#"if grep -qx "$key" "$d/typed.txt"; then echo "Error: failed to load bootstrap configuration

Caused by:
    invalid type: map, expected a boolean
    in \`$key\`" >&2; exit 1; fi
exit 0"#
            .to_string()
    };
    stub(
        dir,
        "codex",
        &format!(
            r#"d=$(dirname "$0")
case "$1" in
  --version) echo "codex-cli 9.9.9"; exit 0 ;;
  app-server)
    echo schema >> "$d/calls"
    {schema_step} ;;
  -c)
    key="${{2%%=*}}"
    echo "$key" >> "$d/probed"
    {probe_step} ;;
esac
exit 2"#
        ),
    )
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
fn lines_of(path: PathBuf) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

/// #281: a Codex key is checked against the installed CLI's own schema. A
/// listed key passes with no probe; a key the schema lacks is probed with a
/// nonsense value (the schema is a partial view): the CLI typing it passes, the
/// CLI ignoring it refuses; a closed schema object refuses without a probe.
/// The CLI is asked once per binary: a second check reads the cache and a
/// replaced binary is asked again.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn codex_launch_override_keys_are_checked_against_the_installed_codex_schema() {
    let dir = tempfile::tempdir().unwrap();
    let codex = codex_keys_stub(
        dir.path(),
        CODEX_CONFIG_SCHEMA,
        &["hide_agent_reasoning"],
        false,
        false,
    );
    let service = service_for(None, Some(codex.clone()), Duration::from_secs(5));
    let keys = [
        "model_verbosity",
        "agents.max_threads",
        "hide_agent_reasoning",
        "removed_key_zz",
        "agents.nope",
    ];
    let check = service.codex_keys_unknown_to_installed_cli(&keys).await;
    assert_eq!(
        check.unknown,
        vec!["removed_key_zz".to_string(), "agents.nope".to_string()]
    );
    assert_eq!(check.degraded, None);
    assert_eq!(check.to_json()["codex"], "checked");
    assert_eq!(
        lines_of(dir.path().join("probed")),
        ["hide_agent_reasoning", "removed_key_zz"],
        "only a key the schema does not model is probed"
    );

    let again = service.codex_keys_unknown_to_installed_cli(&keys).await;
    assert_eq!(again, check);
    assert_eq!(
        lines_of(dir.path().join("calls")).len(),
        1,
        "one schema per binary"
    );
    assert_eq!(
        lines_of(dir.path().join("probed")).len(),
        2,
        "probes are cached"
    );

    // A replaced binary (new size) is asked again, once.
    let mut body = std::fs::read_to_string(&codex).unwrap();
    body.push_str("# upgraded\n");
    std::fs::write(&codex, body).unwrap();
    let upgraded = service.codex_keys_unknown_to_installed_cli(&keys).await;
    assert_eq!(upgraded, check);
    assert_eq!(lines_of(dir.path().join("calls")).len(), 2);

    assert_eq!(
        service.codex_keys_unknown_to_installed_cli(&[]).await,
        CodexKeyCheck::default()
    );
}

/// #281: when the CLI cannot answer the check is degraded and refuses nothing:
/// no codex installed, a schema that cannot be generated, and a probe that
/// fails without naming the key.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn codex_launch_override_key_check_degrades_and_never_refuses_when_the_cli_cannot_answer() {
    let keys = ["model_verbosity", "removed_key_zz"];

    let absent = service_for(None, None, Duration::from_secs(5));
    let check = absent.codex_keys_unknown_to_installed_cli(&keys).await;
    assert!(check.unknown.is_empty());
    assert_eq!(check.degraded, Some("codex_cli_unavailable"));
    assert_eq!(check.to_json()["codex"], "degraded");
    assert_eq!(check.to_json()["degraded_reason"], "codex_cli_unavailable");

    let no_schema = tempfile::tempdir().unwrap();
    let codex = codex_keys_stub(no_schema.path(), CODEX_CONFIG_SCHEMA, &[], true, false);
    let service = service_for(None, Some(codex), Duration::from_secs(5));
    let check = service.codex_keys_unknown_to_installed_cli(&keys).await;
    assert!(check.unknown.is_empty());
    assert_eq!(check.degraded, Some("codex_schema_unavailable"));
    service.codex_keys_unknown_to_installed_cli(&keys).await;
    assert_eq!(
        lines_of(no_schema.path().join("calls")).len(),
        1,
        "a failed schema is not re-run on every write"
    );

    let broken = tempfile::tempdir().unwrap();
    let codex = codex_keys_stub(broken.path(), CODEX_CONFIG_SCHEMA, &[], false, true);
    let service = service_for(None, Some(codex), Duration::from_secs(5));
    let check = service.codex_keys_unknown_to_installed_cli(&keys).await;
    assert!(
        check.unknown.is_empty(),
        "an inconclusive probe never refuses"
    );
    assert_eq!(check.degraded, Some("codex_key_probe_inconclusive"));

    let malformed = tempfile::tempdir().unwrap();
    let codex = codex_keys_stub(
        malformed.path(),
        "{\"not\": \"a schema\"}",
        &[],
        false,
        false,
    );
    let service = service_for(None, Some(codex), Duration::from_secs(5));
    let check = service.codex_keys_unknown_to_installed_cli(&keys).await;
    assert!(check.unknown.is_empty());
    assert_eq!(check.degraded, Some("codex_schema_unavailable"));
}

/// #281 against the real installed codex-cli (skipped when absent): the keys
/// #278 allows are all known to it, and a made-up key is not.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn the_installed_codex_cli_knows_every_allowlisted_launch_override_key() {
    let Some(binary) = crate::provider_cli::resolve("codex") else {
        eprintln!("skipped: codex is not installed");
        return;
    };
    let service = ProviderContractService::new(Arc::new(CliContractRunner::new()));
    let mut keys: Vec<&str> = rsi_common::provider_launch_overrides::CODEX_KNOWN_KEYS
        .iter()
        .map(|known| known.key)
        .collect();
    keys.push("rsi_no_such_key_zz");
    let check = service.codex_keys_unknown_to_installed_cli(&keys).await;
    eprintln!("{} -> {check:?}", binary.display());
    if check.degraded.is_some() {
        return;
    }
    assert!(check.unknown.contains(&"rsi_no_such_key_zz".to_string()));
    let version = CliContractRunner::new()
        .cli_version(ContractProvider::Codex)
        .await
        .unwrap_or_default();
    if version.starts_with("codex-cli 0.163.") {
        assert_eq!(check.unknown, vec!["rsi_no_such_key_zz".to_string()]);
    }
}

/// #130: an offline daemon's canary starts no CLI (not even `--version`) for
/// either provider, and each contract reads `unchecked` with reason `offline`.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn an_offline_canary_starts_no_cli_and_reads_unchecked_offline() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("started");
    let body = format!("echo \"$0 $@\" >> '{}'\nexit 0", marker.display());
    let claude = stub(dir.path(), "claude", &body);
    let codex = stub(dir.path(), "codex", &body);
    let service = service_for(Some(claude), Some(codex), Duration::from_secs(5));
    for provider in [ContractProvider::Claude, ContractProvider::Codex] {
        let forced = CheckContext { force: true, ..ON };
        let contract = service.check_gated(provider, forced, true).await;
        assert_eq!(contract.contract_state, ProviderContractState::Unchecked);
        assert_eq!(
            contract.reason.as_deref(),
            Some(crate::offline::REASON_OFFLINE)
        );
    }
    assert_eq!(service.runs_started(), 0);
    assert!(!marker.exists(), "an offline canary started a CLI");
    assert_eq!(service.codex_custody_refusal(true), None);
}
