//! Read-only efficiency rows for Settings -> Stats (#1018).
//!
//! Projects the operator-only `GetEfficiencyMetrics` response for today (UTC)
//! into label/value rows, each value shown next to its target. A value the
//! daemon reports as `None` is shown as `unknown`, never as zero.

use crate::model_control_stats::{StatsGroup, StatsRow, StatsTone};
use rsi_common::rpc::{EfficiencyMetricTargets, EfficiencyMetricValues, EfficiencyMetricsResponse};

const UNKNOWN: &str = "unknown";

pub(crate) fn efficiency_rows(response: Option<&EfficiencyMetricsResponse>) -> Vec<StatsRow> {
    let Some(response) = response else {
        return vec![row("Efficiency (today, UTC)", "(loading…)")];
    };
    let Some(day) = response.rows.iter().find(|row| row.epic_id.is_none()) else {
        return vec![row("Efficiency (today, UTC)", "no activity yet")];
    };
    let values = &day.values;
    let targets = &response.targets;
    vec![
        row("Efficiency (today, UTC)", &day.day),
        row(
            "Opus tokens / landing",
            &tokens_per_landing(values, targets),
        ),
        row("Poll share of lead turns", &poll_share(values, targets)),
        row("Landings", &optional_count(values.landings)),
        row(
            "Gate-hours / landing",
            &optional_fixed(values.gate_hours_per_landing, 2, ""),
        ),
        row(
            "Seal to rolling p50 / p90",
            &format!(
                "{} / {}",
                optional_fixed(values.seal_to_rolling_p50_seconds, 0, "s"),
                optional_fixed(values.seal_to_rolling_p90_seconds, 0, "s")
            ),
        ),
        row("Reviewer receipt success", &receipt_success(values)),
    ]
}

fn row(label: &str, value: &str) -> StatsRow {
    let description = match label {
        "Efficiency (today, UTC)" => {
            "Delivery metrics for today's UTC calendar day. Missing measurements remain unknown, not zero."
        }
        "Opus tokens / landing" => {
            "Opus tokens consumed per change published to rolling. Lower is better; target shown beside the measurement."
        }
        "Poll share of lead turns" => {
            "Share of lead-agent turns spent checking progress instead of doing work. Lower is better."
        }
        "Landings" => "Number of changes published to rolling today.",
        "Gate-hours / landing" => "Hours spent in build and test gates per published change.",
        "Seal to rolling p50 / p90" => {
            "Time from accepting a change to publishing it: median (p50) and the time within which 90% land (p90)."
        }
        "Reviewer receipt success" => {
            "Share of review receipts submitted successfully; failed submissions count against this rate."
        }
        _ => "Delivery metric for today (UTC).",
    };
    let mut row = StatsRow::info(StatsGroup::Efficiency, label, value, description);
    if value.contains("over target") {
        row.tone = StatsTone::Warning;
    } else if value.contains("within target") {
        row.tone = StatsTone::Positive;
    }
    row
}

fn status(within_target: bool) -> &'static str {
    if within_target {
        "within target"
    } else {
        "over target"
    }
}

fn tokens_per_landing(
    values: &EfficiencyMetricValues,
    targets: &EfficiencyMetricTargets,
) -> String {
    let target = format!("target < {}M", targets.opus_tokens_per_landing / 1_000_000);
    match values.opus_tokens_per_landing {
        None => format!("{UNKNOWN}  ({target})"),
        Some(value) => format!(
            "{:.1}M  ({target}, {})",
            value / 1_000_000.0,
            status(value < targets.opus_tokens_per_landing as f64)
        ),
    }
}

fn poll_share(values: &EfficiencyMetricValues, targets: &EfficiencyMetricTargets) -> String {
    let target = format!("target < {:.0}%", targets.poll_turn_share * 100.0);
    if values.lead_turns == 0 {
        return format!("{UNKNOWN}  ({target}, no lead turns)");
    }
    format!(
        "{:.1}%  ({}/{} turns, {target}, {})",
        values.poll_turn_share * 100.0,
        values.poll_turns,
        values.lead_turns,
        status(values.poll_turn_share < targets.poll_turn_share)
    )
}

fn receipt_success(values: &EfficiencyMetricValues) -> String {
    match values.reviewer_receipt_success_rate {
        None => UNKNOWN.to_string(),
        Some(rate) => format!(
            "{:.0}%  ({} submitted, {} failed)",
            rate * 100.0,
            values.reviews_submitted,
            values.reviews_failed
        ),
    }
}

fn optional_count(value: Option<u64>) -> String {
    value.map_or_else(|| UNKNOWN.to_string(), |count| count.to_string())
}

fn optional_fixed(value: Option<f64>, digits: usize, unit: &str) -> String {
    value.map_or_else(
        || UNKNOWN.to_string(),
        |value| format!("{value:.digits$}{unit}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::rpc::{EfficiencyMetricsGroupBy, EfficiencyMetricsRow};

    fn response(values: EfficiencyMetricValues) -> EfficiencyMetricsResponse {
        let from = chrono::DateTime::parse_from_rfc3339("2026-09-30T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        EfficiencyMetricsResponse {
            from,
            to: from + chrono::Duration::days(1),
            group_by: EfficiencyMetricsGroupBy::Day,
            targets: EfficiencyMetricTargets::default(),
            rows: vec![EfficiencyMetricsRow {
                day: "2026-09-30".to_string(),
                epic_id: None,
                epic_title: None,
                values,
            }],
        }
    }

    fn value_of(rows: &[StatsRow], label: &str) -> String {
        rows.iter()
            .find(|row| row.label == label)
            .unwrap_or_else(|| panic!("row {label} missing"))
            .value
            .clone()
    }

    #[test]
    fn values_render_next_to_their_targets() {
        let rows = efficiency_rows(Some(&response(EfficiencyMetricValues {
            landings: Some(4),
            opus_tokens_per_landing: Some(104_000_000.0),
            lead_turns: 100,
            poll_turns: 3,
            poll_turn_share: 0.03,
            gate_hours_per_landing: Some(0.194),
            seal_to_rolling_p50_seconds: Some(238.0),
            seal_to_rolling_p90_seconds: Some(8807.0),
            reviews_submitted: 2,
            reviews_failed: 1,
            reviewer_receipt_success_rate: Some(2.0 / 3.0),
            ..EfficiencyMetricValues::default()
        })));
        assert_eq!(
            value_of(&rows, "Opus tokens / landing"),
            "104.0M  (target < 20M, over target)"
        );
        assert_eq!(
            value_of(&rows, "Poll share of lead turns"),
            "3.0%  (3/100 turns, target < 5%, within target)"
        );
        assert_eq!(value_of(&rows, "Landings"), "4");
        assert_eq!(value_of(&rows, "Gate-hours / landing"), "0.19");
        assert_eq!(value_of(&rows, "Seal to rolling p50 / p90"), "238s / 8807s");
        assert_eq!(
            value_of(&rows, "Reviewer receipt success"),
            "67%  (2 submitted, 1 failed)"
        );
    }

    #[test]
    fn none_values_render_as_unknown_not_zero() {
        let rows = efficiency_rows(Some(&response(EfficiencyMetricValues::default())));
        assert_eq!(
            value_of(&rows, "Opus tokens / landing"),
            "unknown  (target < 20M)"
        );
        assert_eq!(
            value_of(&rows, "Poll share of lead turns"),
            "unknown  (target < 5%, no lead turns)"
        );
        assert_eq!(value_of(&rows, "Landings"), "unknown");
        assert_eq!(value_of(&rows, "Gate-hours / landing"), "unknown");
        assert_eq!(
            value_of(&rows, "Seal to rolling p50 / p90"),
            "unknown / unknown"
        );
        assert_eq!(value_of(&rows, "Reviewer receipt success"), "unknown");
    }

    #[test]
    fn loading_and_empty_day_have_one_positive_row() {
        let loading = efficiency_rows(None);
        assert_eq!(value_of(&loading, "Efficiency (today, UTC)"), "(loading…)");
        let mut empty = response(EfficiencyMetricValues::default());
        empty.rows.clear();
        let rows = efficiency_rows(Some(&empty));
        assert_eq!(
            value_of(&rows, "Efficiency (today, UTC)"),
            "no activity yet"
        );
    }
}
