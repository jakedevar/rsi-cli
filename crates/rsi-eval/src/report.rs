//! Markdown + JSON report rendering.
//!
//! Markdown is the human-facing diff: a baseline-vs-candidate aggregate
//! table, a per-ticket table, and a Regressions section if the gate fired.
//! JSON is the machine-readable form: baseline-shape with an extra
//! `gate_decision` block.

use crate::gate::GateDecision;
use crate::metrics::BaselineSnapshot;
use serde_json::{Value, json};
use std::fmt::Write;

/// Render a markdown diff between an optional baseline and the candidate.
/// `gate` may be `None` when no baseline was provided (capture-only run).
#[must_use]
pub fn render_markdown(
    baseline: Option<&BaselineSnapshot>,
    candidate: &BaselineSnapshot,
    gate: &GateDecision,
) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# rsi-eval report");
    let _ = writeln!(out);
    let _ = writeln!(out, "- **Harness**: `{}`", candidate.harness_version_hash);
    let _ = writeln!(out, "- **Corpus**: `{}`", candidate.corpus);
    let _ = writeln!(out, "- **Captured**: {}", candidate.captured_at);
    let _ = writeln!(out, "- **Wall time**: {:.1}s", candidate.wall_time_seconds);
    let _ = writeln!(
        out,
        "- **Gate**: {}",
        if gate.passed { "PASS" } else { "FAIL" }
    );
    let _ = writeln!(out);

    let _ = writeln!(out, "## Aggregate metrics");
    let _ = writeln!(out);
    let _ = writeln!(out, "| metric | baseline | candidate | delta |");
    let _ = writeln!(out, "|---|---|---|---|");

    let baseline_aggregate = baseline.map(|b| &b.aggregate);
    let table_rows: [(&str, f64); 6] = [
        ("completion_rate", candidate.aggregate.completion_rate),
        ("test_pass_rate", candidate.aggregate.test_pass_rate),
        ("clippy_pass_rate", candidate.aggregate.clippy_pass_rate),
        (
            "asked_clarification_rate",
            candidate.aggregate.asked_clarification_rate,
        ),
        (
            "phase_failure_count",
            f64::from(candidate.aggregate.phase_failure_count),
        ),
        (
            "token_cost_total",
            candidate.aggregate.token_cost_total as f64,
        ),
    ];
    for (name, c_val) in table_rows {
        let b_val_text = baseline_aggregate
            .map(|b| match name {
                "completion_rate" => format!("{:.4}", b.completion_rate),
                "test_pass_rate" => format!("{:.4}", b.test_pass_rate),
                "clippy_pass_rate" => format!("{:.4}", b.clippy_pass_rate),
                "asked_clarification_rate" => format!("{:.4}", b.asked_clarification_rate),
                "phase_failure_count" => format!("{}", b.phase_failure_count),
                "token_cost_total" => format!("{}", b.token_cost_total),
                _ => "-".to_string(),
            })
            .unwrap_or_else(|| "-".to_string());
        let delta_text = baseline_aggregate
            .map(|b| {
                let b_val = match name {
                    "completion_rate" => b.completion_rate,
                    "test_pass_rate" => b.test_pass_rate,
                    "clippy_pass_rate" => b.clippy_pass_rate,
                    "asked_clarification_rate" => b.asked_clarification_rate,
                    "phase_failure_count" => f64::from(b.phase_failure_count),
                    "token_cost_total" => b.token_cost_total as f64,
                    _ => 0.0,
                };
                format!("{:+.4}", c_val - b_val)
            })
            .unwrap_or_else(|| "-".to_string());
        let c_text = if name == "phase_failure_count" || name == "token_cost_total" {
            format!("{c_val}")
        } else {
            format!("{c_val:.4}")
        };
        let _ = writeln!(out, "| {name} | {b_val_text} | {c_text} | {delta_text} |");
    }
    let _ = writeln!(out);

    if !gate.regressions.is_empty() {
        let _ = writeln!(out, "## Regressions");
        let _ = writeln!(out);
        for reg in &gate.regressions {
            let _ = writeln!(
                out,
                "- **{}** — baseline {:.4} → candidate {:.4} ({:+.2}%, threshold {:.2}%)",
                reg.metric, reg.baseline, reg.candidate, reg.delta_pct, reg.threshold_pct
            );
        }
        let _ = writeln!(out);
    }

    let _ = writeln!(out, "## Per-ticket");
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "| ticket | status | turns | retries | tests | clippy | tokens | wall_ms |"
    );
    let _ = writeln!(out, "|---|---|---|---|---|---|---|---|");
    for (id, t) in &candidate.tickets {
        let test_cell = match t.test_passed {
            Some(true) => "pass".to_string(),
            Some(false) => "FAIL".to_string(),
            None => "-".to_string(),
        };
        let clippy_cell = match t.clippy_passed {
            Some(true) => "pass".to_string(),
            Some(false) => "FAIL".to_string(),
            None => "-".to_string(),
        };
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {} | {} | {} | {} |",
            id,
            t.completion_status,
            t.turn_count,
            t.retry_count,
            test_cell,
            clippy_cell,
            t.token_cost_total,
            t.wall_time_ms,
        );
    }
    out
}

/// Render the machine-readable JSON report. Top-level alphabetical key
/// order matches the BaselineSnapshot serialization.
#[must_use]
pub fn render_json(candidate: &BaselineSnapshot, gate: &GateDecision) -> Value {
    json!({
        "candidate": candidate,
        "gate_decision": gate,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gate::{GateDecision, Regression};
    use crate::metrics::{AggregateMetrics, BaselineSnapshot, TicketMetrics};
    use std::collections::BTreeMap;

    fn fix_candidate() -> BaselineSnapshot {
        let mut tickets = BTreeMap::new();
        tickets.insert(
            "impl-001".to_string(),
            TicketMetrics {
                approval_wait_ms: 0,
                asked_clarification: false,
                clippy_passed: Some(true),
                completion_status: "Completed".to_string(),
                phase_failure_count: 0,
                retry_count: 0,
                test_passed: Some(true),
                token_cost_total: 12345,
                turn_count: 5,
                wall_time_ms: 60_000,
            },
        );
        BaselineSnapshot {
            aggregate: AggregateMetrics {
                asked_clarification_rate: 0.0,
                clippy_pass_rate: 1.0,
                completion_rate: 1.0,
                phase_failure_count: 0,
                test_pass_rate: 1.0,
                token_cost_total: 12345,
            },
            captured_at: "2026-05-08T00:00:00Z".to_string(),
            corpus: "default".to_string(),
            git_commit: "abc".to_string(),
            harness_version_hash: "h1".to_string(),
            schema_version: 1,
            tickets,
            wall_time_seconds: 60.0,
        }
    }

    #[test]
    fn markdown_passes_renders_pass_marker() {
        let candidate = fix_candidate();
        let gate = GateDecision {
            passed: true,
            regressions: Vec::new(),
        };
        let md = render_markdown(None, &candidate, &gate);
        assert!(md.contains("Gate**: PASS"));
        assert!(md.contains("impl-001"));
    }

    #[test]
    fn markdown_renders_regressions_section_when_present() {
        let candidate = fix_candidate();
        let gate = GateDecision {
            passed: false,
            regressions: vec![Regression {
                metric: "completion_rate".to_string(),
                baseline: 1.0,
                candidate: 0.5,
                delta_pct: 50.0,
                threshold_pct: 10.0,
            }],
        };
        let md = render_markdown(Some(&candidate), &candidate, &gate);
        assert!(md.contains("## Regressions"));
        assert!(md.contains("completion_rate"));
        assert!(md.contains("Gate**: FAIL"));
    }

    #[test]
    fn json_render_includes_candidate_and_gate() {
        let candidate = fix_candidate();
        let gate = GateDecision {
            passed: true,
            regressions: Vec::new(),
        };
        let v = render_json(&candidate, &gate);
        assert!(v.get("candidate").is_some());
        assert!(v.get("gate_decision").is_some());
        assert_eq!(v["gate_decision"]["passed"], true);
    }
}
