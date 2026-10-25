//! #3176: the container exec target of a Harness session.
//!
//! A session bound to a benchmark container runs `shell`, `exec_command` and
//! `write_stdin` through `docker -H <host> exec` instead of on the daemon host.
//! This module builds that command, validates the target against the live
//! docker host, kills what a cancelled `exec_command` left in the container, and
//! names the tools that stay unregistered because they would touch the host.
//!
//! Model calls and the OpenRouter key stay in the daemon; only the tool
//! execution crosses to the container. Nothing here ever falls back to the host:
//! a target that cannot be built or validated is a typed refusal.

use super::shell::ShellLaunchIdentity;
use crate::process_control::{
    CaptureError, CaptureLimits, OverflowBehavior, ProcessContainment, capture_bounded,
};
use rsi_common::exec_target::{
    CONTAINER_TARGET_HOST_TOOL_REFUSED, ContainerExecTarget, EXEC_TARGET_INVALID,
};
use std::ffi::OsString;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// The container-side marker variable. Every command an `exec_command` session
/// starts carries `RSI_EXEC_ID=<uuid>`, so the kill finds the whole process tree
/// (children inherit it) even where a descendant left its process group.
pub(crate) const EXEC_MARKER_ENV: &str = "RSI_EXEC_ID";

/// Grace between the in-container `timeout`'s TERM and its KILL.
const TIMEOUT_KILL_GRACE_SECS: u64 = 5;
/// The client waits this much longer than the in-container timeout, so the
/// in-container one fires first and reports a timeout.
pub(crate) const CLIENT_TIMEOUT_GRACE: Duration = Duration::from_secs(15);
/// Bound of one `docker inspect` call.
const INSPECT_TIMEOUT: Duration = Duration::from_secs(20);
const INSPECT_MAX_BYTES: usize = 1024 * 1024;

/// The compose service Harbor runs a task in.
const COMPOSE_SERVICE_LABEL: &str = "com.docker.compose.service";
const COMPOSE_PROJECT_LABEL: &str = "com.docker.compose.project";
const COMPOSE_SERVICE_MAIN: &str = "main";

/// Environment of the local docker client, and nothing else: enough for `ssh`
/// to find its keys and for `systemd-run --user --scope` (the worker scope this
/// command runs in) to reach the user bus, never the daemon's own variables.
/// Without the two bus variables every container tool call failed with `Failed to
/// connect to user scope bus` (#3935 batch 1: 890 of 974 tool results).
pub(crate) const CLIENT_ENV_VARS: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LANG",
    "SSH_AUTH_SOCK",
    "XDG_RUNTIME_DIR",
    "DBUS_SESSION_BUS_ADDRESS",
];

/// The tools a container-bound session registers, besides the three that run in
/// the container: the ones that never touch a filesystem or a process.
pub(crate) const CONTAINER_TARGET_TOOLS: &[&str] = &[
    "update_plan",
    "context_status",
    "compact",
    "shell",
    "exec_command",
    "write_stdin",
    "read_output",
];

/// Host-touching tools that are never registered for a container target. A call
/// that reaches one (a stale transcript, a forced call) is refused with
/// [`CONTAINER_TARGET_HOST_TOOL_REFUSED`].
pub(crate) const HOST_TOOLS: &[&str] = &[
    "read_file",
    "write_file",
    "edit_file",
    "apply_patch",
    "list_files",
    "grep",
    "glob",
    "git",
    "view_image",
    "web_fetch",
    "web_search",
    "memory_search",
    "schedule_wake",
];

/// Whether `name` is refused for a container-bound session: a known host tool,
/// a codegraph, rsi_control or MCP tool, or anything else outside the allowed
/// set that happens to be registered.
pub(crate) fn is_host_tool(name: &str) -> bool {
    HOST_TOOLS.contains(&name)
        || name.starts_with("codegraph")
        || name.starts_with("rsi_control")
        || name.starts_with("mcp__")
}

/// The refusal text of a host tool called on a container-bound session.
pub(crate) fn host_tool_refusal_message(name: &str) -> String {
    format!(
        "{CONTAINER_TARGET_HOST_TOOL_REFUSED}: `{name}` acts on the daemon host and is not \
         available in a container-bound session; use shell, exec_command and write_stdin"
    )
}

#[cfg(test)]
thread_local! {
    /// Test seam: the fake `docker` shim a test installs for this thread.
    static DOCKER_PROGRAM_OVERRIDE: std::cell::RefCell<Option<OsString>> =
        const { std::cell::RefCell::new(None) };
}

/// The docker client program.
pub(crate) fn docker_program() -> OsString {
    #[cfg(test)]
    if let Some(program) = DOCKER_PROGRAM_OVERRIDE.with(|cell| cell.borrow().clone()) {
        return program;
    }
    OsString::from("docker")
}

#[cfg(all(test, unix))]
pub(crate) fn set_docker_program_for_test(program: Option<OsString>) {
    DOCKER_PROGRAM_OVERRIDE.with(|cell| *cell.borrow_mut() = program);
}

/// The explicit environment of the local docker client.
fn client_env() -> Vec<(String, String)> {
    CLIENT_ENV_VARS
        .iter()
        .filter_map(|name| {
            std::env::var(name)
                .ok()
                .map(|value| ((*name).into(), value))
        })
        .collect()
}

/// #3169: the shell prelude of a container command. The container keeps its own
/// environment; a non-interactive default is exported only where the container
/// leaves the name unset, and `TERM` is `dumb` on pipes (a tty launch gets
/// `xterm-256color` from `docker exec -e`).
pub(crate) fn container_command(command: &str, tty: bool) -> String {
    let mut prelude = String::new();
    for (name, value) in super::shell::NON_INTERACTIVE_ENV_DEFAULTS {
        prelude.push_str(&format!("export {name}=\"${{{name}-{value}}}\"; "));
    }
    if !tty {
        prelude.push_str(&format!("export TERM={}; ", super::shell::PIPE_TERM));
    }
    prelude.push('\n');
    prelude.push_str(command);
    prelude
}

/// The `docker exec` argv (after the program) that runs `command` in the target.
///
/// `workdir` is a path inside the container. `marker` stamps the command for the
/// in-container kill; `timeout_secs` wraps it in `timeout -k 5 <secs>`; `tty`
/// asks `docker exec` for a terminal.
pub(crate) fn exec_argv(
    target: &ContainerExecTarget,
    workdir: &str,
    command: &str,
    marker: Option<Uuid>,
    timeout_secs: Option<u64>,
    tty: bool,
) -> Vec<String> {
    let mut argv: Vec<String> = vec![
        "-H".into(),
        target.docker_host.clone(),
        "exec".into(),
        "-i".into(),
    ];
    if tty {
        // #3162: the local docker client runs on a PTY; `-t` gives the command
        // a terminal inside the container.
        argv.extend(["-t".into(), "-e".into(), "TERM=xterm-256color".into()]);
    }
    argv.extend(["-w".into(), workdir.into()]);
    if let Some(user) = &target.user {
        argv.extend(["-u".into(), user.clone()]);
    }
    if let Some(marker) = marker {
        argv.extend(["-e".into(), format!("{EXEC_MARKER_ENV}={marker}")]);
    }
    argv.push(target.container.clone());
    if let Some(secs) = timeout_secs {
        argv.extend([
            "timeout".into(),
            "-k".into(),
            TIMEOUT_KILL_GRACE_SECS.to_string(),
            secs.to_string(),
        ]);
    }
    argv.extend(["bash".into(), "-c".into(), container_command(command, tty)]);
    argv
}

/// `workdir` as a container path: absolute, one line, valid UTF-8.
pub(crate) fn container_workdir(path: &std::path::Path) -> Result<String, String> {
    let text = path
        .to_str()
        .ok_or_else(|| format!("{EXEC_TARGET_INVALID}:workdir_syntax"))?;
    if !text.starts_with('/') || text.chars().any(char::is_control) {
        return Err(format!("{EXEC_TARGET_INVALID}:workdir_syntax"));
    }
    Ok(text.to_string())
}

/// Resolve an `exec_command` `workdir` argument inside the container: absolute
/// as given, relative against the target's workdir. Never the host path.
pub(crate) fn resolve_container_workdir(
    target: &ContainerExecTarget,
    requested: Option<&str>,
) -> String {
    match requested {
        Some(path) if path.starts_with('/') => path.to_string(),
        Some(path) if !path.is_empty() => {
            format!("{}/{}", target.workdir.trim_end_matches('/'), path)
        }
        _ => target.workdir.clone(),
    }
}

/// The scoped, environment-scrubbed `docker exec` command of one tool call. The
/// caller sets stdio and spawns it.
pub(crate) fn scoped_container_command(
    target: &ContainerExecTarget,
    workdir: &str,
    command: &str,
    identity: &ShellLaunchIdentity,
    timeout_secs: Option<u64>,
) -> Result<tokio::process::Command, String> {
    target
        .validate_shape()
        .map_err(|reason| format!("{EXEC_TARGET_INVALID}:{reason}"))?;
    let limits = crate::process_scope::WorkerScopeLimits::from_launcher_snapshot()
        .map_err(|error| error.to_string())?;
    let scoped = crate::process_scope::ScopedWorkerCommand::new(
        &docker_program(),
        identity.invocation_id.unwrap_or_else(Uuid::new_v4),
        limits,
    )
    .map_err(|error| error.to_string())?;
    let mut cmd = scoped.into_command();
    cmd.args(exec_argv(
        target,
        workdir,
        command,
        identity.exec_marker,
        timeout_secs,
        identity.tty,
    ));
    // The docker client runs in a neutral directory, never the host session
    // path, and with an explicit environment only: no CARGO_*, no TMPDIR, no
    // credential the daemon holds.
    cmd.current_dir(std::env::temp_dir()).env_clear();
    for (key, value) in client_env() {
        cmd.env(key, value);
    }
    cmd.env(
        rsi_common::identity::ENV_PROCESS_OWNERSHIP_NAMESPACE,
        rsi_common::identity::process_ownership_namespace(),
    );
    if let Some(session_id) = identity.session_id {
        cmd.env(rsi_common::identity::ENV_SESSION_ID, session_id.to_string());
    }
    if let Some(invocation_id) = identity.invocation_id {
        cmd.env(
            rsi_common::identity::ENV_MODEL_INVOCATION_ID,
            invocation_id.to_string(),
        );
    }
    Ok(cmd)
}

/// The in-container script that kills every process stamped with `marker`: TERM,
/// a second, then KILL. Pure bash and `/proc`, so it needs no `pkill`.
fn kill_script(marker: Uuid) -> String {
    format!(
        "id={marker}\n\
         pids=\n\
         for p in /proc/[0-9]*; do\n\
           pid=${{p#/proc/}}\n\
           [ \"$pid\" = \"$$\" ] && continue\n\
           while IFS= read -r -d '' kv; do\n\
             if [ \"$kv\" = \"{EXEC_MARKER_ENV}=$id\" ]; then pids=\"$pids $pid\"; break; fi\n\
           done 2>/dev/null < \"$p/environ\"\n\
         done\n\
         [ -n \"$pids\" ] || exit 0\n\
         kill -TERM $pids 2>/dev/null\n\
         sleep 1\n\
         kill -KILL $pids 2>/dev/null\n\
         exit 0\n"
    )
}

/// What kills an `exec_command` session's process tree inside its container.
/// Killing the local docker client alone leaves the in-container command
/// running; this reaches it.
#[derive(Debug, Clone)]
pub(crate) struct RemoteKill {
    target: ContainerExecTarget,
    marker: Uuid,
    session_id: Option<Uuid>,
}

impl RemoteKill {
    pub(crate) fn new(
        target: &ContainerExecTarget,
        marker: Uuid,
        session_id: Option<Uuid>,
    ) -> Self {
        Self {
            target: target.clone(),
            marker,
            session_id,
        }
    }

    /// The `docker exec` argv of the kill command.
    pub(crate) fn argv(&self) -> Vec<String> {
        let mut argv: Vec<String> =
            vec!["-H".into(), self.target.docker_host.clone(), "exec".into()];
        if let Some(user) = &self.target.user {
            argv.extend(["-u".into(), user.clone()]);
        }
        argv.push(self.target.container.clone());
        argv.extend(["bash".into(), "-c".into(), kill_script(self.marker)]);
        argv
    }

    /// Fire the kill and return at once; a detached task runs the client under
    /// the bounded capture (30 s). Best effort: a failure is logged, the local
    /// client is already gone.
    pub(crate) fn fire(&self) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            tracing::warn!("in-container kill has no runtime; the command may still run");
            return;
        };
        let mut cmd = tokio::process::Command::new(docker_program());
        cmd.args(self.argv())
            .current_dir(std::env::temp_dir())
            .env_clear();
        for (key, value) in client_env() {
            cmd.env(key, value);
        }
        cmd.env(
            rsi_common::identity::ENV_PROCESS_OWNERSHIP_NAMESPACE,
            rsi_common::identity::process_ownership_namespace(),
        );
        if let Some(session_id) = self.session_id {
            cmd.env(rsi_common::identity::ENV_SESSION_ID, session_id.to_string());
        }
        let limits = CaptureLimits::new(
            INSPECT_MAX_BYTES,
            64 * 1024,
            Duration::from_secs(30),
            OverflowBehavior::TruncateAndDrain,
            ProcessContainment::Group,
        );
        runtime.spawn(async move {
            if let Err(error) = capture_bounded(cmd, limits, &CancellationToken::new()).await {
                tracing::warn!(%error, "in-container kill failed; the command may still run");
            }
        });
    }
}

fn invalid(reason: &str) -> String {
    format!("{EXEC_TARGET_INVALID}:{reason}")
}

async fn docker_json(
    host: &str,
    args: &[&str],
    cancel: &CancellationToken,
) -> Result<serde_json::Value, String> {
    let mut cmd = tokio::process::Command::new(docker_program());
    cmd.arg("-H").arg(host).args(args);
    cmd.current_dir(std::env::temp_dir()).env_clear();
    for (key, value) in client_env() {
        cmd.env(key, value);
    }
    let limits = CaptureLimits::new(
        INSPECT_MAX_BYTES,
        64 * 1024,
        INSPECT_TIMEOUT,
        OverflowBehavior::Error,
        ProcessContainment::Group,
    );
    let output = capture_bounded(cmd, limits, cancel)
        .await
        .map_err(|error| match error {
            CaptureError::ExecutionTimedOut => invalid("inspect_timed_out"),
            _ => invalid("inspect_failed"),
        })?;
    if !output.status.success() {
        return Err(invalid("inspect_failed"));
    }
    serde_json::from_slice(&output.stdout).map_err(|_| invalid("inspect_malformed"))
}

/// Check a container target against the operator allowlist and the live docker
/// host: the host is allowed, the container is running as compose service `main`
/// of the registered project, and its image carries a pinned digest.
///
/// # Errors
/// `exec_target_invalid:<reason>`; any failure to learn the facts refuses.
pub(crate) async fn validate_container_target(
    target: &ContainerExecTarget,
    allowed_hosts: &[String],
    cancel: &CancellationToken,
) -> Result<(), String> {
    target.validate_shape().map_err(invalid)?;
    if !allowed_hosts.iter().any(|host| host == &target.docker_host) {
        return Err(invalid("docker_host_not_allowed"));
    }
    let inspected = docker_json(
        &target.docker_host,
        &["inspect", "--type", "container", &target.container],
        cancel,
    )
    .await?;
    let container = inspected
        .as_array()
        .and_then(|list| list.first())
        .ok_or_else(|| invalid("inspect_malformed"))?;
    if container.pointer("/State/Running") != Some(&serde_json::Value::Bool(true)) {
        return Err(invalid("container_not_running"));
    }
    let label = |name: &str| {
        container
            .pointer("/Config/Labels")
            .and_then(|labels| labels.get(name))
            .and_then(serde_json::Value::as_str)
    };
    if label(COMPOSE_SERVICE_LABEL) != Some(COMPOSE_SERVICE_MAIN) {
        return Err(invalid("service_label_mismatch"));
    }
    if label(COMPOSE_PROJECT_LABEL) != Some(target.compose_project.as_str()) {
        return Err(invalid("project_label_mismatch"));
    }
    let image = container
        .get("Image")
        .and_then(serde_json::Value::as_str)
        .filter(|image| !image.is_empty() && !image.starts_with('-'))
        .ok_or_else(|| invalid("inspect_malformed"))?;
    let image_info = docker_json(&target.docker_host, &["image", "inspect", image], cancel).await?;
    let digests: Vec<&str> = image_info
        .as_array()
        .and_then(|list| list.first())
        .and_then(|info| info.get("RepoDigests"))
        .and_then(serde_json::Value::as_array)
        .map(|list| list.iter().filter_map(serde_json::Value::as_str).collect())
        .ok_or_else(|| invalid("inspect_malformed"))?;
    if !target.digest_is_pinned(&digests) {
        return Err(invalid("image_digest_not_pinned"));
    }
    Ok(())
}

// The shim is a bash script made executable with unix permissions, so the
// fixture and the tests built on it are unix-only.
#[cfg(all(test, unix))]
pub(crate) mod test_support {
    //! A fake `docker` shim for the container-target tests: a shell script that
    //! logs its argv and environment, serves canned `inspect` answers and, for an
    //! `exec`, runs the command part locally. Never a real docker.

    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    pub(crate) const PINNED: &str =
        "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    pub(crate) const PROJECT: &str = "tb2-fix-git-trial";

    pub(crate) fn target() -> ContainerExecTarget {
        ContainerExecTarget {
            docker_host: "ssh://arch-laptop".into(),
            container: "tb2-fix-git-main-1".into(),
            workdir: "/app".into(),
            user: Some("agent".into()),
            compose_project: PROJECT.into(),
            pinned_digests: vec![PINNED.into()],
            per_call_ceiling_secs: 900,
        }
    }

    /// How the shim treats an `exec` invocation.
    #[derive(Clone, Copy)]
    pub(crate) enum Exec {
        /// Replace the shim with the command (a killed client kills it).
        Run,
        /// Run the command in its own session and wait for it, like a real
        /// `docker exec`: killing the client leaves the command running.
        Detach,
        /// Do not run the command; print `inert-ok`.
        Inert,
    }

    /// A portable `timeout SECS CMD...` for hosts without coreutils (macOS).
    const FALLBACK_TIMEOUT: &str = r#"#!/usr/bin/perl
use POSIX qw(:sys_wait_h);
my $secs = shift @ARGV;
my $pid = fork();
die "fork: $!" unless defined $pid;
if ($pid == 0) { exec @ARGV or exit 127; }
my $timed_out = 0;
$SIG{ALRM} = sub { $timed_out = 1; kill 'TERM', $pid; };
alarm $secs;
waitpid($pid, 0);
my $status = $?;
exit 124 if $timed_out;
exit($status & 127 ? 128 + ($status & 127) : $status >> 8);
"#;

    pub(crate) struct Shim {
        dir: tempfile::TempDir,
    }

    impl Shim {
        pub(crate) fn new(exec: Exec) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let shim = Self { dir };
            shim.set_container_json(&container_json(true, "main", PROJECT));
            shim.set_image_json(&image_json(PINNED));
            std::fs::write(
                shim.path("mode"),
                match exec {
                    Exec::Run => "run",
                    Exec::Detach => "detach",
                    Exec::Inert => "inert",
                },
            )
            .unwrap();
            let script = format!(
                r#"#!/bin/bash
D="{dir}"
printf '%s\0' "$@" >> "$D/argv.log"
printf '\036' >> "$D/argv.log"
env > "$D/env.$$"
case "$*" in
  *"image inspect"*) [ -e "$D/image.fail" ] && exit 1; cat "$D/image.json"; exit 0;;
  *" inspect --type container "*) [ -e "$D/container.fail" ] && exit 1; cat "$D/container.json"; exit 0;;
esac
shift 2
shift
while [ $# -gt 0 ]; do
  case "$1" in
    -i) shift;;
    -w|-u) shift 2;;
    -e) export "$2"; case "$2" in RSI_EXEC_ID=*) marker=${{2#RSI_EXEC_ID=}};; esac; shift 2;;
    *) break;;
  esac
done
shift
# The production kill script scans /proc (the container is Linux). A host
# without /proc (macOS) stands in for the container here and `ps eww` no longer
# shows other processes' environments there, so a detached command's marker is
# recorded at start and the stand-in kill signals that command's session.
if [ ! -r /proc/self/environ ] && [ "$1" = bash ] && [ "$2" = -c ] && [ "${{3#id=}}" != "$3" ]; then
  id=${{3#id=}}; id=${{id%%$'\n'*}}
  pid=$(cat "$D/exec.$id.pid" 2>/dev/null) || exit 0
  kill -TERM -- "-$pid" 2>/dev/null
  sleep 1
  kill -KILL -- "-$pid" 2>/dev/null
  exit 0
fi
mode=$(cat "$D/mode")
if [ "$mode" = inert ]; then echo inert-ok; exit 0; fi
if [ "$1" = timeout ]; then shift 3; set -- timeout "$@"; fi
command -v timeout >/dev/null 2>&1 || PATH="$D:$PATH"
if [ "$mode" = detach ]; then
  perl -MPOSIX -e 'POSIX::setsid() != -1 or die "setsid: $!"; exec @ARGV or die "exec: $!"' "$@" &
  child=$!
  [ -z "$marker" ] || echo "$child" > "$D/exec.$marker.pid"
  wait $child; exit $?
fi
exec "$@"
"#,
                dir = shim.dir.path().display()
            );
            let path = shim.path("docker");
            std::fs::write(&path, script).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            // macOS ships no timeout(1) either; the shim falls back to this
            // stand-in (`timeout SECS CMD...`, exit 124 on expiry) when the
            // host has none, so the in-container timeout stays testable.
            let timeout = shim.path("timeout");
            std::fs::write(&timeout, FALLBACK_TIMEOUT).unwrap();
            std::fs::set_permissions(&timeout, std::fs::Permissions::from_mode(0o755)).unwrap();
            set_docker_program_for_test(Some(path.into_os_string()));
            shim
        }

        pub(crate) fn path(&self, name: &str) -> PathBuf {
            self.dir.path().join(name)
        }

        pub(crate) fn set_container_json(&self, json: &str) {
            std::fs::write(self.path("container.json"), json).unwrap();
        }

        pub(crate) fn set_image_json(&self, json: &str) {
            std::fs::write(self.path("image.json"), json).unwrap();
        }

        /// Make the named inspect (`container` or `image`) exit non-zero.
        pub(crate) fn fail(&self, which: &str) {
            std::fs::write(self.path(&format!("{which}.fail")), "").unwrap();
        }

        /// Every logged invocation's argv.
        pub(crate) fn invocations(&self) -> Vec<Vec<String>> {
            read_invocations(&self.path("argv.log"))
        }

        /// The `exec` invocations only.
        pub(crate) fn execs(&self) -> Vec<Vec<String>> {
            self.invocations()
                .into_iter()
                .filter(|argv| argv.get(2).map(String::as_str) == Some("exec"))
                .collect()
        }

        /// The environment of every shim run, one `KEY=VALUE` list per run.
        pub(crate) fn environments(&self) -> Vec<Vec<String>> {
            std::fs::read_dir(self.dir.path())
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().starts_with("env."))
                .map(|entry| {
                    std::fs::read_to_string(entry.path())
                        .unwrap()
                        .lines()
                        .map(str::to_string)
                        .collect()
                })
                .collect()
        }
    }

    impl Drop for Shim {
        fn drop(&mut self) {
            set_docker_program_for_test(None);
        }
    }

    pub(crate) fn container_json(running: bool, service: &str, project: &str) -> String {
        format!(
            r#"[{{"Image":"sha256:img","State":{{"Running":{running}}},"Config":{{"Labels":{{"com.docker.compose.service":"{service}","com.docker.compose.project":"{project}"}}}}}}]"#
        )
    }

    pub(crate) fn image_json(digest: &str) -> String {
        format!(r#"[{{"RepoDigests":["registry.example/tb2/fix-git@{digest}"]}}]"#)
    }

    pub(crate) fn read_invocations(path: &Path) -> Vec<Vec<String>> {
        let Ok(text) = std::fs::read(path) else {
            return Vec::new();
        };
        text.split(|b| *b == 0x1e)
            .filter(|record| !record.is_empty())
            .map(|record| {
                record
                    .split(|b| *b == 0)
                    .filter(|arg| !arg.is_empty())
                    .map(|arg| String::from_utf8_lossy(arg).into_owned())
                    .collect()
            })
            .collect()
    }
}
