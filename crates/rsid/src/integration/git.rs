//! Hardened Git invocation for the integration engine.
//!
//! Every call runs through the crate's bounded process supervisor, ignores
//! hooks, replacement objects, grafts, signing, rerere and inherited repository
//! overrides, and uses a fixed locale so diagnostics classify deterministically.

use super::{IntegrationConfig, IntegrationError, Result};
use crate::process_control::{
    CaptureError, CaptureLimits, OverflowBehavior, ProcessContainment, capture_bounded,
    capture_bounded_with_spawn,
};
use std::io::Write;
use std::path::Path;
use std::process::ExitStatus;
use std::process::Stdio;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

const MAX_STDOUT_BYTES: usize = 2 * 1024 * 1024;
const MAX_STDERR_BYTES: usize = 64 * 1024;
const DIAGNOSTIC_TAIL_BYTES: usize = 2 * 1024;

/// Upper bound for the `update-ref --stdin` transaction payload fed to the
/// reference-transaction hook. The payload is three short commands plus OIDs.
const MAX_TRANSACTION_STDIN_BYTES: usize = 64 * 1024;

pub(super) struct GitOutput {
    pub(super) status: ExitStatus,
    pub(super) stdout: Vec<u8>,
    pub(super) stderr: Vec<u8>,
    pub(super) stdout_truncated: bool,
}

impl GitOutput {
    pub(super) fn stderr_tail(&self) -> String {
        let start = self.stderr.len().saturating_sub(DIAGNOSTIC_TAIL_BYTES);
        String::from_utf8_lossy(&self.stderr[start..])
            .trim()
            .to_string()
    }
}

/// Build the hardened git invocation with optional global `-c` overrides.
/// Overrides are inserted after the hardened defaults but before `-C` and the
/// subcommand, so the last value of any key wins while argument order stays
/// valid for the subcommand.
fn command_with_config(
    config: &IntegrationConfig,
    cwd: &Path,
    args: &[&str],
    config_overrides: &[(&str, &str)],
) -> Command {
    let mut command = Command::new("git");
    command.args([
        "--no-optional-locks",
        "-c",
        "core.fsmonitor=false",
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "commit.gpgsign=false",
        "-c",
        "rerere.enabled=false",
        "-c",
        "gc.auto=0",
    ]);
    for (key, value) in config_overrides {
        command.arg("-c").arg(format!("{key}={value}"));
    }
    command
        .arg("-C")
        .arg(cwd)
        .args(args)
        // Candidates name canonical objects, never locally substituted history.
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_GRAFT_FILE", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_MERGE_AUTOEDIT", "no")
        .env("GIT_EDITOR", "true")
        // Never let a caller's global or XDG Git configuration select a
        // different repository, object store, hook, or executable.
        .env("HOME", "/nonexistent")
        .env("LC_ALL", "C")
        .env("GIT_AUTHOR_NAME", &config.identity.name)
        .env("GIT_AUTHOR_EMAIL", &config.identity.email)
        .env("GIT_COMMITTER_NAME", &config.identity.name)
        .env("GIT_COMMITTER_EMAIL", &config.identity.email);
    for key in [
        "XDG_CONFIG_HOME",
        "XDG_CONFIG_DIRS",
        "GIT_CONFIG",
        "GIT_CONFIG_GLOBAL",
        "GIT_CONFIG_SYSTEM",
        "GIT_CONFIG_NOSYSTEM",
        "GIT_CONFIG_COUNT",
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_COMMON_DIR",
        "GIT_NAMESPACE",
        "GIT_PREFIX",
        "GIT_CEILING_DIRECTORIES",
        "GIT_REPLACE_REF_BASE",
        "GIT_GRAFT_FILE",
        "GIT_NO_REPLACE_OBJECTS",
        "GIT_HOOKS_PATH",
        "GIT_EDITOR",
        "GIT_SEQUENCE_EDITOR",
        "GIT_ASKPASS",
        "SSH_ASKPASS",
        "GIT_TERMINAL_PROMPT",
    ] {
        command.env_remove(key);
    }
    // Re-add only controlled values after the scrub list above.
    command
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_GRAFT_FILE", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "true");
    command
}

fn command(config: &IntegrationConfig, cwd: &Path, args: &[&str]) -> Command {
    command_with_config(config, cwd, args, &[])
}

fn command_with_index(
    config: &IntegrationConfig,
    cwd: &Path,
    args: &[&str],
    index: &Path,
) -> Command {
    let mut command = command(config, cwd, args);
    command.env("GIT_INDEX_FILE", index);
    command
}

/// Run one Git command to completion. A non-zero exit is returned, not raised:
/// callers classify it. Only supervision failures (spawn, timeout) are errors.
pub(super) async fn run(
    config: &IntegrationConfig,
    cwd: &Path,
    args: &[&str],
) -> Result<GitOutput> {
    let limits = CaptureLimits::new(
        MAX_STDOUT_BYTES,
        MAX_STDERR_BYTES,
        config.git_timeout,
        // A mutating Git command must not be killed for being chatty.
        OverflowBehavior::TruncateAndDrain,
        ProcessContainment::Group,
    );
    let captured = capture_bounded(
        command(config, cwd, args),
        limits,
        &CancellationToken::new(),
    )
    .await
    .map_err(|error| IntegrationError::Git(format!("git {}: {error}", verb(args))))?;
    Ok(GitOutput {
        status: captured.status,
        stdout: captured.stdout,
        stderr: captured.stderr,
        stdout_truncated: captured.stdout_truncated,
    })
}

async fn run_with_index(
    config: &IntegrationConfig,
    cwd: &Path,
    args: &[&str],
    index: &Path,
) -> Result<GitOutput> {
    let limits = CaptureLimits::new(
        MAX_STDOUT_BYTES,
        MAX_STDERR_BYTES,
        config.git_timeout,
        OverflowBehavior::TruncateAndDrain,
        ProcessContainment::Group,
    );
    let captured = capture_bounded(
        command_with_index(config, cwd, args, index),
        limits,
        &CancellationToken::new(),
    )
    .await
    .map_err(|error| IntegrationError::Git(format!("git {}: {error}", verb(args))))?;
    Ok(GitOutput {
        status: captured.status,
        stdout: captured.stdout,
        stderr: captured.stderr,
        stdout_truncated: captured.stdout_truncated,
    })
}

/// Run one hardened Git command whose stdin is a bounded prefilled payload and
/// whose reference-transaction hooks path is the caller's daemon-owned
/// directory. The remaining hardening is identical to [`run`]; only the
/// `core.hooksPath` configuration is overridden so the caller's own constant
/// hook can run inside the ref transaction.
pub(super) async fn run_reference_transaction(
    config: &IntegrationConfig,
    cwd: &Path,
    args: &[&str],
    stdin: &[u8],
    hooks_path: &Path,
    variables: &[(String, String)],
) -> Result<GitOutput> {
    if stdin.len() > MAX_TRANSACTION_STDIN_BYTES {
        return Err(IntegrationError::InvalidInput(
            "reference transaction stdin exceeds the bound",
        ));
    }
    let hooks_text = hooks_path.to_str().ok_or(IntegrationError::InvalidInput(
        "hooks path must be valid UTF-8",
    ))?;
    // The override is a global `-c` that precedes `-C <cwd>` and the
    // subcommand, so git parses it as configuration rather than subcommand
    // syntax; the last `core.hooksPath` (our directory) wins over the hardened
    // `/dev/null` default.
    let mut command = command_with_config(config, cwd, args, &[("core.hooksPath", hooks_text)]);
    for (key, value) in variables {
        command.env(key.as_str(), value.as_str());
    }

    let input = tempfile::Builder::new()
        .prefix("rsi-custody-ref-transaction-")
        .tempfile()
        .map_err(|error| {
            IntegrationError::Git(format!(
                "reference transaction stdin tempfile create failed: {error}"
            ))
        })?;
    {
        let mut file = input.as_file();
        file.write_all(stdin).map_err(|error| {
            IntegrationError::Git(format!(
                "reference transaction stdin tempfile write failed: {error}"
            ))
        })?;
        file.sync_all().map_err(|error| {
            IntegrationError::Git(format!(
                "reference transaction stdin tempfile sync failed: {error}"
            ))
        })?;
    }
    let input_reader = input.reopen().map_err(|error| {
        IntegrationError::Git(format!(
            "reference transaction stdin tempfile reopen failed: {error}"
        ))
    })?;

    let limits = CaptureLimits::new(
        MAX_STDOUT_BYTES,
        MAX_STDERR_BYTES,
        config.git_timeout,
        OverflowBehavior::TruncateAndDrain,
        ProcessContainment::Group,
    );
    let captured =
        capture_bounded_with_spawn(command, limits, &CancellationToken::new(), |mut command| {
            command
                .stdin(Stdio::from(input_reader))
                .spawn()
                .map_err(|error| CaptureError::Spawn(error.to_string()))
        })
        .await
        .map_err(|error| IntegrationError::Git(format!("git {}: {error}", verb(args))))?;
    Ok(GitOutput {
        status: captured.status,
        stdout: captured.stdout,
        stderr: captured.stderr,
        stdout_truncated: captured.stdout_truncated,
    })
}

/// Trimmed UTF-8 stdout of a command that must succeed with complete output.
pub(super) async fn stdout(
    config: &IntegrationConfig,
    cwd: &Path,
    args: &[&str],
) -> Result<String> {
    let output = run(config, cwd, args).await?;
    if !output.status.success() {
        return Err(failed(args, &output));
    }
    text(args, output)
}

/// Raw stdout of a command that must succeed with complete output.
pub(super) async fn stdout_raw(
    config: &IntegrationConfig,
    cwd: &Path,
    args: &[&str],
) -> Result<Vec<u8>> {
    let output = run(config, cwd, args).await?;
    if !output.status.success() {
        return Err(failed(args, &output));
    }
    if output.stdout_truncated {
        return Err(IntegrationError::Git(format!(
            "git {}: output exceeded bound",
            verb(args)
        )));
    }
    Ok(output.stdout)
}

pub(super) async fn stdout_with_index(
    config: &IntegrationConfig,
    cwd: &Path,
    args: &[&str],
    index: &Path,
) -> Result<String> {
    let output = run_with_index(config, cwd, args, index).await?;
    if !output.status.success() {
        return Err(failed(args, &output));
    }
    text(args, output)
}

pub(super) async fn stdout_raw_with_index(
    config: &IntegrationConfig,
    cwd: &Path,
    args: &[&str],
    index: &Path,
) -> Result<Vec<u8>> {
    let output = run_with_index(config, cwd, args, index).await?;
    if !output.status.success() {
        return Err(failed(args, &output));
    }
    if output.stdout_truncated {
        return Err(IntegrationError::Git(format!(
            "git {}: output exceeded bound",
            verb(args)
        )));
    }
    Ok(output.stdout)
}

pub(super) async fn predicate_with_index(
    config: &IntegrationConfig,
    cwd: &Path,
    args: &[&str],
    index: &Path,
) -> Result<bool> {
    let output = run_with_index(config, cwd, args, index).await?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(failed(args, &output)),
    }
}

/// Exit-status predicate: `0` is true, `1` is false, anything else is an error.
pub(super) async fn predicate(
    config: &IntegrationConfig,
    cwd: &Path,
    args: &[&str],
) -> Result<bool> {
    let output = run(config, cwd, args).await?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(failed(args, &output)),
    }
}

pub(super) fn failed(args: &[&str], output: &GitOutput) -> IntegrationError {
    IntegrationError::Git(format!(
        "git {} exited {:?}: {}",
        verb(args),
        output.status.code(),
        output.stderr_tail()
    ))
}

fn text(args: &[&str], output: GitOutput) -> Result<String> {
    if output.stdout_truncated {
        return Err(IntegrationError::Git(format!(
            "git {}: output exceeded bound",
            verb(args)
        )));
    }
    String::from_utf8(output.stdout)
        .map(|text| text.trim().to_string())
        .map_err(|_| IntegrationError::Git(format!("git {}: non-UTF-8 output", verb(args))))
}

fn verb<'a>(args: &[&'a str]) -> &'a str {
    args.first().copied().unwrap_or("")
}
