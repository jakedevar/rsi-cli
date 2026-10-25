//! `rsid-quiet-restart`: the `RequestOperatorRestart` front of
//! `scripts/rsid-quiet-restart.sh request_quiet_restart`, which used to run it as
//! an inline interpreter heredoc (#465, umbrella #358).
//!
//! ```text
//! rsi-repo-guard rsid-quiet-restart <rsi-rpc> <socket> <binaries-dir> [max-wait-secs]
//! ```
//!
//! Exit status (the contract `request_quiet_restart` maps): 0 a quiet-point
//! restart is pending (status printed on stdout); 10 the daemon answered that it
//! is not under the supervisor, so a direct restart is safe; 11 its socket is
//! absent or refusing (the shell then checks for an rsid process); 1 anything
//! else (a refusal, a timeout, a malformed reply, a socket that fails some other
//! way): the daemon may still be running turns, so nothing is restarted and the
//! message names `make release-install NOW=1`.
//!
//! `RSID_QUIET_RESTART_RPC_TIMEOUT_SECS` shortens the 120 s wait for the RPC (tests).

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::Ctx;
use crate::bounded_run::{RunError, run_bounded};

const NOW_HINT: &str =
    "Retry later, or force an immediate restart with: make release-install NOW=1";

/// `rsid-quiet-restart <rpc> <socket> <dir> [wait]`.
pub fn main(args: &[String], ctx: &Ctx, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let (Some(rpc), Some(socket), Some(dir)) = (args.first(), args.get(1), args.get(2)) else {
        let _ = writeln!(
            err,
            "usage: rsi-repo-guard rsid-quiet-restart <rsi-rpc> <socket> <binaries-dir> [max-wait-secs]"
        );
        return 64;
    };
    let wait = args.get(3).map(String::as_str).unwrap_or_default();
    let timeout = ctx
        .var("RSID_QUIET_RESTART_RPC_TIMEOUT_SECS")
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .map_or(Duration::from_secs(120), Duration::from_secs);
    request(rpc, socket, dir, wait, timeout, out, err)
}

fn ambiguous(err: &mut dyn Write, reason: &str) -> i32 {
    let _ = writeln!(err, "quiet-point restart not requested: {reason}");
    let _ = writeln!(
        err,
        "The daemon may still be running turns, so it was not restarted. {NOW_HINT}"
    );
    1
}

/// Python's `str()` of a JSON value, as the old heredoc printed it.
fn text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => "None".to_string(),
        other => other.to_string(),
    }
}

fn request(
    rpc: &str,
    socket: &str,
    dir: &str,
    wait: &str,
    timeout: Duration,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    // Positive evidence that the daemon is down: its socket is absent or refuses.
    match UnixStream::connect(socket) {
        Ok(probe) => drop(probe),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            return 11;
        }
        Err(error) => return ambiguous(err, &format!("cannot probe the daemon socket ({error})")),
    }

    // The params keep the `json.dumps` spacing the daemon-facing contract has always had.
    let dir_json = serde_json::to_string(dir).unwrap_or_else(|_| "\"\"".to_string());
    let params = if wait.is_empty() {
        format!("{{\"binaries_dir\": {dir_json}}}")
    } else {
        let Ok(wait) = wait.trim().parse::<i64>() else {
            let _ = writeln!(
                err,
                "max wait must be an integer number of seconds, got {wait:?}"
            );
            return 1;
        };
        format!("{{\"binaries_dir\": {dir_json}, \"max_wait_secs\": {wait}}}")
    };
    let mut cmd = Command::new(rpc);
    cmd.args([
        "--socket",
        socket,
        "RequestOperatorRestart",
        "--params",
        &params,
    ]);
    let stdout = match run_bounded(cmd, Instant::now() + timeout) {
        Ok((_ok, stdout)) => stdout,
        Err(RunError::Spawn(error)) => {
            return ambiguous(err, &format!("cannot run {rpc} ({error})"));
        }
        Err(_) => {
            return ambiguous(
                err,
                "the daemon did not answer RequestOperatorRestart in time",
            );
        }
    };
    let reply: Option<Value> = serde_json::from_slice(&stdout).ok();
    let Some(reply) = reply
        .as_ref()
        .filter(|reply| reply.get("jsonrpc").and_then(Value::as_str) == Some("2.0"))
    else {
        return ambiguous(
            err,
            "the daemon's reply to RequestOperatorRestart was not understood",
        );
    };
    if let Some(error) = reply.get("error").filter(|error| is_truthy(error)) {
        let message = match error.as_object() {
            Some(object) => object.get("message").map_or_else(|| text(error), text),
            None => text(error),
        };
        if message.contains("deploy_needs_supervisor") {
            let _ = writeln!(
                err,
                "rsid is not under rsid-supervisor.sh; restarting it directly."
            );
            return 10;
        }
        let _ = writeln!(err, "quiet-point restart refused: {message}");
        let _ = writeln!(err, "{NOW_HINT}");
        return 1;
    }
    let status = reply.get("result").filter(|result| is_truthy(result));
    let field = |name: &str| {
        status
            .and_then(|s| s.get(name))
            .map_or_else(|| "None".to_string(), text)
    };
    let _ = writeln!(
        out,
        "Restart pending: rsid swaps in the new binaries and restarts at the next quiet point (sha {}, release by {}).",
        field("sha"),
        field("release_by")
    );
    let _ = writeln!(
        out,
        "  Waiting for running turns, jobs and landings to finish; the TUI shows the countdown."
    );
    let _ = writeln!(
        out,
        "  Force it now:  rsi-rpc ForceOperatorRestart    Cancel it:  rsi-rpc CancelOperatorRestart"
    );
    0
}

/// Python truthiness of a JSON value (`if error:`, `reply.get("result") or {}`).
fn is_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_none_or(|n| n != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

#[cfg(test)]
#[path = "rsid_quiet_restart_tests.rs"]
mod tests;
