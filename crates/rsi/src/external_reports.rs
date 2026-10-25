//! `:external-reports` (#3789): the operator view of the external harness-report
//! import. Reports filed as GitHub Issues by RSI installs anywhere are imported
//! by the hub as untrusted `external-report` Issues (P4 until triage promotes
//! them). This command shows the sync status (last run, counts, errors), runs a
//! pass now, and filters the focused Issues pane to the `external-report` label.

use crate::app::App;
use rsi_common::external_report::SyncStatusV1;

/// Usage line for an unknown argument.
const USAGE: &str = ":external-reports [status|sync|filter]";

fn report(app: &mut App, status: &SyncStatusV1) {
    let line = status.summary_line();
    if status.errors.is_empty() {
        app.notify(line);
    } else {
        app.notify_error(line);
    }
}

pub(crate) async fn external_reports_command(app: &mut App, args: Option<&str>) {
    match args.map(str::trim).unwrap_or_default() {
        "" | "status" => match app.client.get_external_report_sync_status().await {
            Ok(status) => report(app, &status),
            Err(error) => {
                app.notify_error(format!("External reports: status read failed: {error}"))
            }
        },
        "sync" => match app.client.sync_external_reports_now().await {
            Ok(status) => report(app, &status),
            Err(error) => app.notify_error(format!("External reports: sync failed: {error}")),
        },
        "filter" => {
            if app.filter_focused_issues_to_external_reports() {
                app.notify("Issues filtered to the external-report label");
            } else {
                app.notify_error(
                    "External reports: focus an Issues pane first, then :external-reports filter",
                );
            }
        }
        other => app.notify_error(format!(
            "Unknown external-reports action `{other}`; use {USAGE}"
        )),
    }
}
