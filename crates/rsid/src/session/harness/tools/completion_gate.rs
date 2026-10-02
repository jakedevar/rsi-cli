//! Operator-configured completion gates for Harness sessions.
//!
//! A gate uses the same scoped shell process as the agent's shell tool, so it
//! is never more privileged than the session it checks.

use super::shell::{ShellLaunchIdentity, scoped_shell_command};
use crate::process_control::{CaptureError, CaptureLimits, capture_bounded};
use crate::session::harness::tools::truncation::truncate_text;
use std::path::Path;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub(super) const COMPLETION_GATE_STREAM_EVENT: &str = "completion_gate";

#[derive(Debug, Clone)]
pub(crate) struct CompletionGateOutcome {
    pub(crate) success: bool,
    pub(crate) output: String,
    pub(crate) exit_code: Option<i32>,
    pub(crate) timed_out: bool,
    pub(crate) cancelled: bool,
}

pub(super) async fn execute_completion_gate(
    command: &str,
    working_dir: &Path,
    timeout: Duration,
    max_output_bytes: usize,
    session_id: Option<uuid::Uuid>,
    invocation_id: Option<uuid::Uuid>,
    execution_scratch: Option<&crate::sandbox::execution_scratch::SandboxExecutionScratch>,
    egress: rsi_common::egress_policy::EgressMode,
    cancel: &CancellationToken,
) -> CompletionGateOutcome {
    let identity = ShellLaunchIdentity {
        session_id,
        invocation_id,
        execution_scratch: execution_scratch.cloned(),
        egress,
    };
    let Ok(command) = scoped_shell_command(command, working_dir, &identity) else {
        return CompletionGateOutcome {
            success: false,
            output: String::new(),
            exit_code: None,
            timed_out: false,
            cancelled: false,
        };
    };

    let mut limits = CaptureLimits::session_tool();
    limits.execution_timeout = timeout;
    limits.max_stdout_bytes = max_output_bytes;
    limits.max_stderr_bytes = max_output_bytes;
    match capture_bounded(command, limits, cancel).await {
        Ok(output) => {
            let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
            if !output.stderr.is_empty() {
                if !combined.is_empty() {
                    combined.push_str("\n--- stderr ---\n");
                }
                combined.push_str(&String::from_utf8_lossy(&output.stderr));
            }
            let combined = truncate_text(
                &combined,
                max_output_bytes,
                output.stdout_truncated || output.stderr_truncated,
            )
            .content;
            let failed_on_output_cap = output.stdout_truncated || output.stderr_truncated;
            CompletionGateOutcome {
                success: output.status.success() && !failed_on_output_cap,
                output: combined,
                exit_code: output.status.code(),
                timed_out: false,
                cancelled: false,
            }
        }
        Err(CaptureError::ExecutionTimedOut) => CompletionGateOutcome {
            success: false,
            output: String::new(),
            exit_code: None,
            timed_out: true,
            cancelled: false,
        },
        Err(CaptureError::Cancelled) => CompletionGateOutcome {
            success: false,
            output: String::new(),
            exit_code: None,
            timed_out: false,
            cancelled: true,
        },
        Err(_) => CompletionGateOutcome {
            success: false,
            output: String::new(),
            exit_code: None,
            timed_out: false,
            cancelled: false,
        },
    }
}
