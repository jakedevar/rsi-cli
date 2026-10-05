//! `:manager restart [status|now|cancel]` (#1122): the operator sees, forces or
//! cancels the quiet-point daemon restart that `make release-install` requested.
//! All calls are operator-only daemon RPCs.

use rsi_common::operator_restart::OperatorRestartStatusV1;

use crate::app::App;

const USAGE: &str = "Use :manager restart [status|now|cancel].";

/// What the operator is told about a restart status.
pub(crate) fn describe(status: &OperatorRestartStatusV1) -> String {
    match status.summary() {
        Some(line) => line,
        None => match (&status.state, &status.reason) {
            (Some(state), Some(reason)) => format!("Last restart {}: {reason}", state.as_str()),
            (Some(state), None) => format!("Last restart {}", state.as_str()),
            _ => "No restart pending.".to_string(),
        },
    }
}

pub(crate) async fn dispatch(app: &mut App, command: &str) {
    match run(app, command.trim()).await {
        Ok(message) => app.notify_success(message),
        Err(error) => app.notify_error(error),
    }
    app.mark_dirty();
}

async fn run(app: &mut App, command: &str) -> Result<String, String> {
    let status = match command {
        "status" => app.client.get_operator_restart().await,
        "now" => app.client.force_operator_restart().await,
        "cancel" => app.client.cancel_operator_restart().await,
        _ => return Err(USAGE.to_string()),
    }
    .map_err(|error| format!("Restart: {error}"))?;
    if let Some(resources) = app.daemon_resources.as_mut() {
        resources.restart_pending = status.summary();
    }
    Ok(describe(&status))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::agent_deploy::DeployState;

    fn status(pending: bool, state: DeployState) -> OperatorRestartStatusV1 {
        OperatorRestartStatusV1 {
            pending,
            deploy_id: None,
            state: Some(state),
            sha: Some("a".repeat(40)),
            release_by: Some("2026-10-03T01:00:00Z".into()),
            forced: false,
            blockers: vec!["worker_mid_turn".into(), "landing_in_progress".into()],
            turns_in_flight: 3,
            reason: None,
            supervised: true,
        }
    }

    #[test]
    fn a_waiting_restart_names_what_it_waits_for_and_when_it_releases() {
        let line = describe(&status(true, DeployState::Staged));
        assert_eq!(
            line,
            "restart pending: waiting for a landing + 3 turns, release by 2026-10-03T01:00:00Z"
        );
    }

    #[test]
    fn a_forced_or_settled_restart_reads_plainly() {
        let mut forced = status(true, DeployState::Staged);
        forced.forced = true;
        assert_eq!(describe(&forced), "restart pending: restarting now");
        let mut cancelled = status(false, DeployState::Failed);
        cancelled.reason = Some("cancelled_by_operator".into());
        assert_eq!(
            describe(&cancelled),
            "Last restart failed: cancelled_by_operator"
        );
    }
}
