//! #3176: the container exec target, driven through the real tools with a fake
//! `docker` shim (never a real docker): argv shape, the scrubbed environment,
//! guards skipped for the container only, unregistered host tools, the timeout
//! wrapper and the in-container kill, and every validation refusal.

use super::exec::{ExecCommandTool, WriteStdinTool, default_process_registry};
use super::exec_target::test_support::{Exec, PROJECT, Shim, container_json, image_json, target};
use super::exec_target::{
    CLIENT_ENV_VARS, CONTAINER_TARGET_TOOLS, HOST_TOOLS, container_command,
    validate_container_target,
};
use super::shell::ShellTool;
use super::{HarnessTool, HarnessToolRegistry, ToolContext, ToolPolicy};
use crate::session::harness::types::{ChatMessage, ToolResult};
use rsi_common::egress_policy::{EgressMode, EgressPolicy};
use rsi_common::exec_target::{CONTAINER_TARGET_HOST_TOOL_REFUSED, ExecTarget};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const HOST_PATH: &str = "/definitely/not/a/path/on/this/host";

/// Host variables the docker-client scrub test plants; unique to it (#187).
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
const SCRUB_SECRET_VAR: &str = "RSI_3176_SCRUB_TEST_SECRET";
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
const SCRUB_PROVIDER_KEY_VAR: &str = "RSI_3176_SCRUB_TEST_OPENROUTER_API_KEY";
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
const SCRUB_CARGO_VAR: &str = "CARGO_RSI_3176_SCRUB_TEST_TARGET_DIR";

fn shell_tool() -> ShellTool {
    ShellTool::new(Some(uuid::Uuid::new_v4()), Some(uuid::Uuid::new_v4()), None)
        .with_exec_target(ExecTarget::Container(target()))
}

fn json_of(result: &ToolResult) -> Value {
    let text = ChatMessage::tool_result("test-result", result.output.clone()).visible_tool_text();
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}: {text}"))
}

fn context(cancel: CancellationToken, egress: EgressMode) -> ToolContext {
    ToolContext {
        session_id: None,
        working_dir: HOST_PATH.into(),
        cancel,
        event_sink: None,
        policy: ToolPolicy {
            egress: EgressPolicy::for_mode(egress),
            ..ToolPolicy::default()
        },
    }
}

fn container_exec(registry: &Arc<super::exec::ProcessRegistry>) -> ExecCommandTool {
    ExecCommandTool::new(
        Arc::clone(registry),
        Some(uuid::Uuid::new_v4()),
        Some(uuid::Uuid::new_v4()),
        None,
    )
    .with_exec_target(ExecTarget::Container(target()))
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_string()).collect()
}

/// `shell` runs `docker -H <host> exec -i -w <workdir> -u <user> <container>
/// timeout -k 5 <ceiling> bash -c <cmd>`: the host path never appears, the
/// output is the container command's, and the timeout is the target's ceiling.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
#[tokio::test]
async fn shell_runs_through_docker_exec_with_the_exact_argv() {
    let shim = Shim::new(Exec::Run);
    let result = shell_tool()
        .execute(json!({"command": "echo hi"}), Path::new(HOST_PATH))
        .await;
    assert!(result.success, "{:?}", result.error_msg);
    assert_eq!(result.output.trim(), "hi");
    assert_eq!(
        shim.execs(),
        vec![strings(&[
            "-H",
            "ssh://arch-laptop",
            "exec",
            "-i",
            "-w",
            "/app",
            "-u",
            "agent",
            "tb2-fix-git-main-1",
            "timeout",
            "-k",
            "5",
            "900",
            "bash",
            "-c",
            &container_command("echo hi", false),
        ])]
    );
    assert!(
        !shim
            .invocations()
            .iter()
            .flatten()
            .any(|a| a.contains(HOST_PATH)),
        "the host path reached the docker client"
    );
}

/// The shell timeout is the target's ceiling: a larger `timeout_secs` is lowered
/// to it, a smaller one wins, and the limit is enforced in the container (exit
/// 124 reads as a timeout) as well as by the client.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
#[tokio::test]
async fn shell_timeout_is_the_targets_ceiling_enforced_in_the_container() {
    let shim = Shim::new(Exec::Run);
    let tool = shell_tool();
    let capped = tool
        .execute(
            json!({"command": "true", "timeout_secs": 5000}),
            Path::new(HOST_PATH),
        )
        .await;
    assert!(capped.success, "{:?}", capped.error_msg);
    let timed_out = tool
        .execute(
            json!({"command": "sleep 30", "timeout_secs": 2}),
            Path::new(HOST_PATH),
        )
        .await;
    assert!(!timed_out.success);
    assert_eq!(
        timed_out.error_msg.as_deref(),
        Some("Command timed out after 2s")
    );
    let wrappers: Vec<Vec<String>> = shim
        .execs()
        .into_iter()
        .map(|argv| argv[9..13].to_vec())
        .collect();
    assert_eq!(
        wrappers,
        vec![
            strings(&["timeout", "-k", "5", "900"]),
            strings(&["timeout", "-k", "5", "2"]),
        ]
    );
}

/// The docker client gets an explicit environment only (no CARGO_*, TMPDIR or
/// daemon secret), and a container target never gets the host `unshare`
/// wrapper, whatever the session's egress mode.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
#[tokio::test]
async fn the_docker_client_environment_is_scrubbed_and_never_wrapped_in_unshare() {
    let shim = Shim::new(Exec::Inert);
    // #187: libtest runs this module's tests on parallel threads of one
    // process, so a write here is seen by every concurrent test. Only names no
    // other test or production path reads are written: setting (then removing)
    // the real `CARGO_TARGET_DIR` sent concurrent shell tests to a missing cwd
    // (`process spawn failed: No such file or directory`), and a real provider
    // key name would be seen by the vault's ambient lookup. The allowlist
    // check below catches any leaked name, these included.
    // SAFETY: no other thread reads these names.
    unsafe {
        std::env::set_var(SCRUB_SECRET_VAR, "supersecret");
        std::env::set_var(SCRUB_PROVIDER_KEY_VAR, "sk-or-supersecret");
        std::env::set_var(SCRUB_CARGO_VAR, "/host/target");
    }
    let session_id = uuid::Uuid::new_v4();
    let tool = ShellTool::new(Some(session_id), Some(uuid::Uuid::new_v4()), None)
        .with_exec_target(ExecTarget::Container(target()));
    let result = tool
        .execute_with_context(
            json!({"command": "echo hi"}),
            &context(CancellationToken::new(), EgressMode::Offline),
        )
        .await;
    unsafe {
        std::env::remove_var(SCRUB_SECRET_VAR);
        std::env::remove_var(SCRUB_PROVIDER_KEY_VAR);
        std::env::remove_var(SCRUB_CARGO_VAR);
    }
    assert!(result.success, "{:?}", result.error_msg);
    assert_eq!(result.output.trim(), "inert-ok");
    let allowed = [
        "PATH",
        "HOME",
        "USER",
        "LANG",
        "SSH_AUTH_SOCK",
        // the worker scope's `systemd-run --user` reaches the user bus through these
        "XDG_RUNTIME_DIR",
        "DBUS_SESSION_BUS_ADDRESS",
        rsi_common::identity::ENV_PROCESS_OWNERSHIP_NAMESPACE,
        rsi_common::identity::ENV_SESSION_ID,
        rsi_common::identity::ENV_MODEL_INVOCATION_ID,
        // #4029: the escape-sweep marker that lets the shell find a docker helper that left its group
        crate::process_control::ESCAPE_SWEEP_ENV,
        // set by bash itself
        "PWD",
        "SHLVL",
        "_",
        "OLDPWD",
    ];
    let environments = shim.environments();
    assert_eq!(environments.len(), 1);
    for line in &environments[0] {
        let key = line.split('=').next().unwrap();
        assert!(allowed.contains(&key), "unexpected variable {line}");
        assert!(!line.contains("supersecret"), "{line}");
    }
    assert!(
        environments[0].contains(&format!(
            "{}={session_id}",
            rsi_common::identity::ENV_SESSION_ID
        )),
        "{:?}",
        environments[0]
    );
    // #3935: the scoped docker client must reach the user bus, or every tool call fails
    // with `Failed to connect to user scope bus`.
    for name in ["XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS"] {
        assert!(
            CLIENT_ENV_VARS.contains(&name),
            "{name} must reach systemd-run"
        );
    }
    let argv = shim.execs().remove(0);
    assert!(
        !argv.iter().any(|a| a == "unshare" || a == "--net"),
        "{argv:?}"
    );
}

/// The kill guard and the custody guard are skipped for a container target
/// only: `git stash` and a pattern kill reach the container, and the host tools
/// still refuse the same commands.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
#[tokio::test]
async fn guards_are_skipped_for_the_container_target_only() {
    let shim = Shim::new(Exec::Inert);
    let registry = default_process_registry();
    let container_exec = container_exec(&registry);
    let host_exec = ExecCommandTool::new(Arc::clone(&registry), None, None, None);
    let commands = [
        "git stash",
        "git checkout -b other",
        "killall rsi-3176-no-such-process",
    ];
    for command in commands {
        let shell = shell_tool()
            .execute(json!({"command": command}), Path::new(HOST_PATH))
            .await;
        assert!(
            shell.success,
            "{command}: {:?} {}",
            shell.error_msg, shell.output
        );
        let exec = container_exec
            .execute_with_context(
                json!({"cmd": command, "yield_time_ms": 1000}),
                &context(CancellationToken::new(), EgressMode::DenyPrivate),
            )
            .await;
        let exec_json = json_of(&exec);
        assert!(exec.success, "{command}: {exec_json}");
        assert!(
            exec_json["output"].as_str().unwrap().contains("inert-ok"),
            "{command}"
        );
    }
    let sent: Vec<String> = shim
        .execs()
        .into_iter()
        .map(|argv| argv.last().cloned().unwrap())
        .collect();
    for command in commands {
        assert_eq!(
            sent.iter()
                .filter(|c| c.ends_with(&format!("\n{command}")))
                .count(),
            2,
            "{command}"
        );
    }

    // The host tools keep both guards.
    let host_shell = ShellTool::default()
        .execute(
            json!({"command": "echo a; killall rsi-3176-no-such-process"}),
            Path::new("/tmp"),
        )
        .await;
    assert_eq!(
        host_shell.error_msg.as_deref(),
        Some(rsi_common::kill_guard::BROAD_KILL_REFUSAL)
    );
    let host_refusal = host_exec
        .execute_with_context(
            json!({"cmd": "killall rsi-3176-no-such-process"}),
            &context(CancellationToken::new(), EgressMode::DenyPrivate),
        )
        .await;
    assert_eq!(json_of(&host_refusal)["error_code"], "broad_kill_refused");
}

/// `exec_command` and `write_stdin`: the argv carries the container, the user and
/// a marker, a relative `workdir` resolves inside the container (never against
/// the host path), and stdin bytes reach the container command.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
#[tokio::test]
async fn exec_command_and_write_stdin_run_in_the_container() {
    let shim = Shim::new(Exec::Run);
    let registry = default_process_registry();
    let exec = container_exec(&registry);
    let write = WriteStdinTool::new(Arc::clone(&registry));
    let cancel = CancellationToken::new();
    let started = exec
        .execute_with_context(
            json!({"cmd": "cat", "workdir": "sub/dir", "yield_time_ms": 250}),
            &context(cancel.clone(), EgressMode::DenyPrivate),
        )
        .await;
    let session_id = json_of(&started)["session_id"]
        .as_u64()
        .expect("cat keeps running");
    let echoed = write
        .execute_with_context(
            json!({"session_id": session_id, "chars": "hello\n", "yield_time_ms": 1000}),
            &context(cancel.clone(), EgressMode::DenyPrivate),
        )
        .await;
    assert!(
        json_of(&echoed)["output"]
            .as_str()
            .unwrap()
            .contains("hello"),
        "{echoed:?}"
    );
    // Absolute and default workdirs.
    for (args, expected) in [
        (json!({"cmd": "true", "workdir": "/tmp/x"}), "/tmp/x"),
        (json!({"cmd": "true"}), "/app"),
    ] {
        let done = exec
            .execute_with_context(args, &context(cancel.clone(), EgressMode::DenyPrivate))
            .await;
        assert_eq!(json_of(&done)["exit_code"], 0, "{done:?}");
        let argv = shim.execs().pop().unwrap();
        let w = argv.iter().position(|a| a == "-w").unwrap();
        assert_eq!(argv[w + 1], expected);
    }
    let execs = shim.execs();
    let argv = &execs[0];
    assert_eq!(
        &argv[..8],
        &strings(&[
            "-H",
            "ssh://arch-laptop",
            "exec",
            "-i",
            "-w",
            "/app/sub/dir",
            "-u",
            "agent"
        ])[..]
    );
    assert_eq!(argv[8], "-e");
    assert!(argv[9].starts_with("RSI_EXEC_ID="), "{argv:?}");
    assert_eq!(
        &argv[10..],
        &strings(&[
            "tb2-fix-git-main-1",
            "bash",
            "-c",
            &container_command("cat", false)
        ])[..]
    );
    registry.shutdown_processes().await;
}

async fn wait_for_pid_file(path: &Path) -> i32 {
    for _ in 0..100 {
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Ok(pid) = text.trim().parse() {
                return pid;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the container command never wrote its pid");
}

/// macOS has no `/proc`: `ps -o stat= -p <pid>` prints nothing for a gone
/// process and a `Z...` state for a zombie.
#[cfg(not(target_os = "linux"))]
fn pid_alive(pid: i32) -> bool {
    let out = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .expect("ps runs");
    let stat = String::from_utf8_lossy(&out.stdout);
    let stat = stat.trim();
    !stat.is_empty() && !stat.starts_with('Z')
}

#[cfg(target_os = "linux")]
fn pid_alive(pid: i32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
        && !std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .map(|stat| stat.contains(") Z"))
            .unwrap_or(true)
}

async fn assert_gone(pid: i32) {
    for _ in 0..120 {
        if !pid_alive(pid) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let _ = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid),
        nix::sys::signal::Signal::SIGKILL,
    );
    panic!("the in-container command {pid} survived the cancel");
}

async fn start_background(shim: &Shim, exec: &ExecCommandTool, cancel: &CancellationToken) -> i32 {
    let pidfile = shim.path("container.pid");
    let started = exec
        .execute_with_context(
            json!({"cmd": format!("echo $$ > {}; exec sleep 120", pidfile.display()), "yield_time_ms": 250}),
            &context(cancel.clone(), EgressMode::DenyPrivate),
        )
        .await;
    assert!(json_of(&started)["session_id"].is_u64(), "{started:?}");
    wait_for_pid_file(&pidfile).await
}

/// Killing the local docker client does not stop a command inside the
/// container; cancelling the session must. The shim detaches the command like
/// a real `docker exec`, so only the marker-based in-container kill ends it.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
#[tokio::test]
async fn cancelling_an_exec_command_kills_the_in_container_process() {
    let shim = Shim::new(Exec::Detach);
    let registry = default_process_registry();
    let exec = container_exec(&registry);
    let cancel = CancellationToken::new();
    let pid = start_background(&shim, &exec, &cancel).await;
    assert!(pid_alive(pid));
    cancel.cancel();
    assert_gone(pid).await;

    let execs = shim.execs();
    let marker = execs[0]
        .iter()
        .find_map(|a| a.strip_prefix("RSI_EXEC_ID="))
        .expect("the exec carries its marker")
        .to_string();
    let kill = execs.last().unwrap();
    assert_eq!(
        &kill[..5],
        &strings(&["-H", "ssh://arch-laptop", "exec", "-u", "agent"])[..]
    );
    assert_eq!(kill[5], "tb2-fix-git-main-1");
    assert_eq!(&kill[6..8], &strings(&["bash", "-c"])[..]);
    assert!(kill[8].contains(&marker), "the kill names the exec marker");
    registry.shutdown_processes().await;
}

/// Ending the session (turn teardown) reaches the container too.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
#[tokio::test]
async fn shutting_the_registry_down_kills_the_in_container_process() {
    let shim = Shim::new(Exec::Detach);
    let registry = default_process_registry();
    let exec = container_exec(&registry);
    let pid = start_background(&shim, &exec, &CancellationToken::new()).await;
    registry.shutdown_processes().await;
    assert_gone(pid).await;
}

/// A normal exit does not run the in-container kill: a command that started a
/// background service keeps it, as it would on the host.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
#[tokio::test]
async fn a_command_that_exits_on_its_own_triggers_no_in_container_kill() {
    let shim = Shim::new(Exec::Run);
    let registry = default_process_registry();
    let exec = container_exec(&registry);
    let done = exec
        .execute_with_context(
            json!({"cmd": "echo done", "yield_time_ms": 2000}),
            &context(CancellationToken::new(), EgressMode::DenyPrivate),
        )
        .await;
    assert_eq!(json_of(&done)["exit_code"], 0);
    registry.shutdown_processes().await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(shim.execs().len(), 1, "{:?}", shim.execs());
}

fn container_registry() -> HarnessToolRegistry {
    HarnessToolRegistry::container_target_tools(
        &target(),
        None,
        Some(uuid::Uuid::new_v4()),
        Some(uuid::Uuid::new_v4()),
        default_process_registry(),
    )
}

/// A container-bound session registers no host-touching tool, and a call that
/// still names one is refused with the typed code, whatever the entry point.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
#[tokio::test]
async fn host_tools_are_unregistered_and_refused_for_a_container_target() {
    let _shim = Shim::new(Exec::Inert);
    let registry = container_registry();
    let mut names: Vec<String> = registry.specs().into_iter().map(|s| s.name).collect();
    names.sort();
    let mut expected = strings(CONTAINER_TARGET_TOOLS);
    expected.sort();
    assert_eq!(names, expected);
    assert!(registry.exec_target().is_container());

    let cancel = CancellationToken::new();
    for name in
        HOST_TOOLS
            .iter()
            .copied()
            .chain(["codegraph_search", "rsi_control_spawn", "mcp__fs__read"])
    {
        for result in [
            registry
                .execute_with_context(
                    name,
                    json!({"path": "/etc/passwd"}),
                    Path::new("/tmp"),
                    &cancel,
                    None,
                )
                .await,
            registry
                .execute(name, json!({"path": "/etc/passwd"}), Path::new("/tmp"))
                .await,
            registry
                .execute_cancellable(
                    name,
                    json!({"path": "/etc/passwd"}),
                    Path::new("/tmp"),
                    &cancel,
                )
                .await,
        ] {
            assert!(!result.success, "{name}");
            let message = result.error_msg.unwrap_or_default();
            assert!(
                message.starts_with(CONTAINER_TARGET_HOST_TOOL_REFUSED),
                "{name}: {message}"
            );
        }
    }
    // An unknown tool is still just unknown.
    let unknown = registry
        .execute_with_context("frobnicate", json!({}), Path::new("/tmp"), &cancel, None)
        .await;
    assert!(
        !unknown
            .error_msg
            .unwrap()
            .starts_with(CONTAINER_TARGET_HOST_TOOL_REFUSED)
    );
    // The container tools stay callable through the registry.
    let echoed = registry
        .execute_with_context(
            "shell",
            json!({"command": "echo through-registry"}),
            Path::new(HOST_PATH),
            &cancel,
            None,
        )
        .await;
    assert!(echoed.success, "{:?}", echoed.error_msg);
    assert_eq!(echoed.output.trim(), "inert-ok");
    registry.shutdown_processes().await;
}

/// A completion gate would run a host shell, so a container-bound session fails
/// it closed without running anything.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
#[tokio::test]
async fn a_completion_gate_never_runs_on_the_host_for_a_container_target() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("gate-ran");
    let gate = rsi_common::completion_gates::CompletionGate {
        name: "g".into(),
        command: format!("touch {}", marker.display()),
        timeout_secs: 5,
        max_output_bytes: 1024,
    };
    let outcome = container_registry()
        .execute_completion_gate(&gate, dir.path(), &CancellationToken::new())
        .await;
    assert!(!outcome.success);
    assert!(
        outcome
            .output
            .starts_with(CONTAINER_TARGET_HOST_TOOL_REFUSED)
    );
    assert!(!marker.exists(), "the gate ran on the host");

    // A host registry still registers the file tools.
    let host = HarnessToolRegistry::default_tools(
        None, None, None, None, None, None, None, None, None, false,
    );
    assert!(!host.exec_target().is_container());
    assert!(host.specs().iter().any(|s| s.name == "read_file"));
}

fn allowed() -> Vec<String> {
    vec!["ssh://arch-laptop".to_string()]
}

async fn validate(shim_setup: impl FnOnce(&Shim)) -> Result<(), String> {
    let shim = Shim::new(Exec::Inert);
    shim_setup(&shim);
    validate_container_target(&target(), &allowed(), &CancellationToken::new()).await
}

/// A running, correctly labelled container on an allowed host with a pinned
/// image is accepted, after exactly the two inspect calls.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
#[tokio::test]
async fn a_valid_container_target_passes_after_two_inspect_calls() {
    let shim = Shim::new(Exec::Inert);
    validate_container_target(&target(), &allowed(), &CancellationToken::new())
        .await
        .expect("valid target");
    assert_eq!(
        shim.invocations(),
        vec![
            strings(&[
                "-H",
                "ssh://arch-laptop",
                "inspect",
                "--type",
                "container",
                "tb2-fix-git-main-1"
            ]),
            strings(&["-H", "ssh://arch-laptop", "image", "inspect", "sha256:img"]),
        ]
    );
}

/// Each allowlist, label, state and digest failure is a typed refusal.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
#[tokio::test]
async fn every_validation_failure_is_a_typed_refusal() {
    let other = format!("sha256:{}", "b".repeat(64));
    type Setup = Box<dyn FnOnce(&Shim)>;
    let cases: Vec<(&str, Setup)> = vec![
        (
            "exec_target_invalid:container_not_running",
            Box::new(|s| s.set_container_json(&container_json(false, "main", PROJECT))),
        ),
        (
            "exec_target_invalid:service_label_mismatch",
            Box::new(|s| s.set_container_json(&container_json(true, "db", PROJECT))),
        ),
        (
            "exec_target_invalid:project_label_mismatch",
            Box::new(|s| s.set_container_json(&container_json(true, "main", "someone-elses"))),
        ),
        (
            "exec_target_invalid:image_digest_not_pinned",
            Box::new(move |s| s.set_image_json(&image_json(&other))),
        ),
        (
            "exec_target_invalid:image_digest_not_pinned",
            Box::new(|s| s.set_image_json(r#"[{"RepoDigests":[]}]"#)),
        ),
        (
            "exec_target_invalid:inspect_failed",
            Box::new(|s| s.fail("container")),
        ),
        (
            "exec_target_invalid:inspect_failed",
            Box::new(|s| s.fail("image")),
        ),
        (
            "exec_target_invalid:inspect_malformed",
            Box::new(|s| s.set_container_json("not json")),
        ),
        (
            "exec_target_invalid:inspect_malformed",
            Box::new(|s| s.set_container_json("[]")),
        ),
        (
            "exec_target_invalid:inspect_malformed",
            Box::new(|s| s.set_image_json(r#"[{"Id":"x"}]"#)),
        ),
    ];
    for (expected, setup) in cases {
        let result = validate(setup).await;
        assert_eq!(result, Err(expected.to_string()), "{expected}");
    }
}

/// The docker host must be on the operator allowlist; an empty list refuses every
/// target, and nothing reaches the docker client before the refusal.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
#[tokio::test]
async fn the_docker_host_must_be_on_the_operator_allowlist() {
    let shim = Shim::new(Exec::Inert);
    for hosts in [vec![], vec!["ssh://elsewhere".to_string()]] {
        let refused = validate_container_target(&target(), &hosts, &CancellationToken::new()).await;
        assert_eq!(
            refused,
            Err("exec_target_invalid:docker_host_not_allowed".to_string())
        );
    }
    assert!(shim.invocations().is_empty(), "docker was contacted");
    // A malformed target never reaches the docker client either.
    let mut bad = target();
    bad.container = "--privileged".into();
    assert_eq!(
        validate_container_target(&bad, &allowed(), &CancellationToken::new()).await,
        Err("exec_target_invalid:container_syntax".to_string())
    );
    assert!(shim.invocations().is_empty());
}

/// #3162: a tty `exec_command` asks `docker exec` for a terminal with `-t` and
/// a `TERM`; without tty the argv is the pipe one.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
#[test]
fn exec_argv_asks_docker_for_a_terminal_only_when_tty() {
    use super::exec_target::exec_argv;
    let target = target();
    let piped = exec_argv(&target, "/app", "echo hi", None, None, false);
    assert!(!piped.contains(&"-t".to_string()), "{piped:?}");
    let tty = exec_argv(&target, "/app", "echo hi", None, None, true);
    let exec_at = tty.iter().position(|arg| arg == "exec").expect("exec");
    assert_eq!(
        &tty[exec_at + 1..exec_at + 3],
        &strings(&["-i", "-t"])[..],
        "{tty:?}"
    );
    assert!(
        tty.windows(2).any(|w| w == ["-e", "TERM=xterm-256color"]),
        "{tty:?}"
    );
    assert_eq!(
        &tty[tty.len() - 3..],
        &strings(&["bash", "-c", &container_command("echo hi", true)])[..]
    );
    // A pipe launch forces TERM=dumb after the defaults; a tty one leaves TERM
    // to `docker exec -e`.
    assert!(piped.last().unwrap().contains("export TERM=dumb;"));
    assert!(!tty.last().unwrap().contains("TERM"));
}

/// #3169: a container command gets the non-interactive defaults only where the
/// container leaves the name unset, and `TERM=dumb` on pipes.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
#[test]
fn container_command_overlays_defaults_only_where_unset() {
    let run = |env: &[(&str, &str)], tty: bool| {
        let mut cmd = std::process::Command::new("bash");
        cmd.env_clear()
            .args([
                "-c",
                &container_command("printf '%s|%s|%s|%s|%s' \"$PAGER\" \"$NO_COLOR\" \"$LANG\" \"$LC_ALL\" \"$TERM\"", tty),
            ])
            .envs(env.iter().copied());
        let output = cmd.output().expect("bash runs");
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap()
    };
    assert_eq!(run(&[], false), "cat|1|C.UTF-8|C.UTF-8|dumb");
    // The container's own values stay; an empty PAGER is a value too.
    assert_eq!(
        run(&[("PAGER", "less"), ("LANG", "en_US.UTF-8")], false),
        "less|1|en_US.UTF-8|C.UTF-8|dumb"
    );
    assert_eq!(run(&[("PAGER", "")], false), "|1|C.UTF-8|C.UTF-8|dumb");
    // On a tty the TERM is left to `docker exec -e TERM=...`.
    assert_eq!(
        run(&[("TERM", "xterm-256color")], true),
        "cat|1|C.UTF-8|C.UTF-8|xterm-256color"
    );
}

/// #3169 through the real shell tool and the shim: the command sees the overlay.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
#[tokio::test]
async fn container_shell_sees_the_non_interactive_defaults() {
    let _shim = Shim::new(Exec::Run);
    let result = shell_tool()
        .execute(
            json!({"command": "printf '%s|%s|%s' \"$PAGER\" \"$GIT_PAGER\" \"$NO_COLOR\""}),
            Path::new(HOST_PATH),
        )
        .await;
    assert!(result.success, "{:?}", result.error_msg);
    assert_eq!(result.output, "cat|cat|1");
}

/// #4029: the docker client starts its `ssh://` helper with `setsid`, which the
/// no-escape seccomp filter denied, so every container `shell` call failed. The
/// `Detach` shim calls `setsid` like that client; the call succeeds, the output
/// is the container command's and the output cap still holds.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
#[tokio::test]
async fn container_shell_succeeds_when_the_docker_client_calls_setsid() {
    let _shim = Shim::new(Exec::Detach);
    let tool = shell_tool();
    let result = tool
        .execute(json!({"command": "echo hi"}), Path::new(HOST_PATH))
        .await;
    assert!(result.success, "{:?} {:?}", result.error_msg, result.output);
    assert_eq!(result.output.trim(), "hi");

    let big = tool
        .execute(
            json!({"command": "head -c 3000000 /dev/zero | tr '\\0' x"}),
            Path::new(HOST_PATH),
        )
        .await;
    assert!(big.success, "{:?}", big.error_msg);
    assert!(
        big.output.len() <= 1024 * 1024 + 4096,
        "output cap lost: {} bytes",
        big.output.len()
    );
}

/// #4029: the timeout bound survives the containment change for a client that
/// calls `setsid`: the in-container `timeout` ends the command and the call
/// reads as a timeout well before the command would have finished.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
#[tokio::test]
async fn container_shell_timeout_holds_when_the_docker_client_calls_setsid() {
    let _shim = Shim::new(Exec::Detach);
    let started = std::time::Instant::now();
    let result = shell_tool()
        .execute(
            json!({"command": "sleep 30", "timeout_secs": 2}),
            Path::new(HOST_PATH),
        )
        .await;
    assert!(!result.success);
    assert_eq!(
        result.error_msg.as_deref(),
        Some("Command timed out after 2s")
    );
    assert!(started.elapsed() < Duration::from_secs(20));
}
