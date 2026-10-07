//! `:manager friction` (#1333): the operator's friction rollup, the andon.
//! One operator-only daemon RPC, `ListFrictionRollup`.
//!
//! - `:manager friction` summarizes the last 24 hours across every project;
//! - `:manager friction <hours>` widens the window (1..=720).
//!
//! The board's Inspect · Friction section shows one project's rows in full.

use rsi_common::friction::{
    FRICTION_MAX_WINDOW_HOURS, FrictionRollupRowV1, ListFrictionRollupRequestV1,
    ListFrictionRollupResultV1,
};

use crate::app::App;

const USAGE: &str = "Use :manager friction [<hours>] (1-720, default 24).";
/// Rows listed in the notification; the rest are counted.
const SHOWN_ROWS: usize = 8;

pub(crate) fn parse(command: &str) -> Result<ListFrictionRollupRequestV1, String> {
    let command = command.trim();
    if command.is_empty() {
        return Ok(ListFrictionRollupRequestV1::default());
    }
    match command.parse::<u32>() {
        Ok(hours) if (1..=FRICTION_MAX_WINDOW_HOURS).contains(&hours) => {
            Ok(ListFrictionRollupRequestV1 {
                window_hours: Some(hours),
                ..ListFrictionRollupRequestV1::default()
            })
        }
        _ => Err(USAGE.to_string()),
    }
}

fn row_summary(row: &FrictionRollupRowV1) -> String {
    let state = match row.filed_display_number {
        Some(number) => format!("filed #{number}"),
        None if row.due => "due".to_string(),
        None => "watching".to_string(),
    };
    format!(
        "×{} {} · {} session{} · {}",
        row.occurrences,
        row.signature,
        row.sessions,
        if row.sessions == 1 { "" } else { "s" },
        state
    )
}

pub(crate) fn rollup_summary(rollup: &ListFrictionRollupResultV1) -> String {
    let mut lines = vec![format!(
        "Friction, last {} h: {} signature{}{}; andon filed {} of {} in the last 24 h.",
        rollup.window_hours,
        rollup.rows.len(),
        if rollup.rows.len() == 1 { "" } else { "s" },
        if rollup.truncated {
            " (more not shown)"
        } else {
            ""
        },
        rollup.filings_last_24h,
        rollup.daily_filing_cap,
    )];
    lines.extend(rollup.rows.iter().take(SHOWN_ROWS).map(row_summary));
    if rollup.rows.len() > SHOWN_ROWS {
        lines.push(format!(
            "… {} more; the board's Inspect · Friction section lists a project's rows.",
            rollup.rows.len() - SHOWN_ROWS
        ));
    }
    lines.join("\n")
}

pub(crate) async fn dispatch_friction_command(app: &mut App, command: &str) {
    match run(app, command).await {
        Ok(message) => app.notify_success(message),
        Err(error) => app.notify_error(error),
    }
    app.mark_dirty();
}

pub(crate) async fn run(app: &mut App, command: &str) -> Result<String, String> {
    let request = parse(command)?;
    let rollup = app
        .client
        .list_friction_rollup(request)
        .await
        .map_err(|error| format!("Friction: {error}"))?;
    Ok(rollup_summary(&rollup))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use uuid::Uuid;

    fn row(
        signature: &str,
        occurrences: u64,
        filed: Option<u64>,
        due: bool,
    ) -> FrictionRollupRowV1 {
        FrictionRollupRowV1 {
            project_id: Some(Uuid::nil()),
            signature: signature.into(),
            kind: signature.split(':').next().unwrap().into(),
            occurrences,
            sessions: 2,
            first_at: Utc::now(),
            last_at: Utc::now(),
            evidence_refs: Vec::new(),
            filed_issue_id: filed.map(|_| Uuid::nil()),
            filed_display_number: filed,
            due,
        }
    }

    #[test]
    fn parse_accepts_an_optional_window() {
        assert_eq!(parse("").unwrap(), ListFrictionRollupRequestV1::default());
        assert_eq!(parse(" 72 ").unwrap().window_hours, Some(72));
        for bad in ["0", "721", "all", "24 extra"] {
            assert_eq!(parse(bad).unwrap_err(), USAGE, "{bad}");
        }
    }

    #[test]
    fn summary_names_each_signature_with_its_filing_state() {
        let rollup = ListFrictionRollupResultV1 {
            window_hours: 24,
            observed_at: Utc::now(),
            rows: vec![
                row("lander:refused:queue_empty_filter", 7, Some(1400), false),
                row("deploy_timeout:worker_mid_turn", 4, None, true),
                row(
                    "terminal:terminal_handoff_superseded_by_tool",
                    1,
                    None,
                    false,
                ),
            ],
            truncated: false,
            filings_last_24h: 1,
            daily_filing_cap: 5,
        };
        let text = rollup_summary(&rollup);
        assert_eq!(
            text,
            "Friction, last 24 h: 3 signatures; andon filed 1 of 5 in the last 24 h.\n\
             ×7 lander:refused:queue_empty_filter · 2 sessions · filed #1400\n\
             ×4 deploy_timeout:worker_mid_turn · 2 sessions · due\n\
             ×1 terminal:terminal_handoff_superseded_by_tool · 2 sessions · watching"
        );
    }
}
