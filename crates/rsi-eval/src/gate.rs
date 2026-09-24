//! Regression gate.
//!
//! Compares two baseline snapshots metric-by-metric and flags any aggregate
//! whose delta exceeds the threshold. Per-metric direction is hard-coded
//! since "lower is better" or "higher is better" is a property of the metric,
//! not a runtime configuration.

use crate::metrics::BaselineSnapshot;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GateDecision {
    pub passed: bool,
    pub regressions: Vec<Regression>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Regression {
    pub metric: String,
    pub baseline: f64,
    pub candidate: f64,
    /// Signed delta in percent; positive when worse.
    pub delta_pct: f64,
    pub threshold_pct: f64,
}

/// Evaluate `candidate` against `baseline` at `threshold_pct`.
#[must_use]
pub fn evaluate(
    baseline: &BaselineSnapshot,
    candidate: &BaselineSnapshot,
    threshold_pct: f64,
) -> GateDecision {
    let mut regressions = Vec::new();

    // Lower is worse: completion_rate, test_pass_rate, clippy_pass_rate.
    for (name, b, c) in [
        (
            "completion_rate",
            baseline.aggregate.completion_rate,
            candidate.aggregate.completion_rate,
        ),
        (
            "test_pass_rate",
            baseline.aggregate.test_pass_rate,
            candidate.aggregate.test_pass_rate,
        ),
        (
            "clippy_pass_rate",
            baseline.aggregate.clippy_pass_rate,
            candidate.aggregate.clippy_pass_rate,
        ),
    ] {
        if let Some(reg) = lower_is_worse(name, b, c, threshold_pct) {
            regressions.push(reg);
        }
    }

    // Higher is worse: token_cost_total.
    let token_b = baseline.aggregate.token_cost_total as f64;
    let token_c = candidate.aggregate.token_cost_total as f64;
    if let Some(reg) = higher_is_worse("token_cost_total", token_b, token_c, threshold_pct) {
        regressions.push(reg);
    }

    // phase_failure_count: higher is worse, with absolute-delta-of-1 floor
    // when the baseline is 0 (avoids divide-by-zero).
    let pf_b = f64::from(baseline.aggregate.phase_failure_count);
    let pf_c = f64::from(candidate.aggregate.phase_failure_count);
    if pf_c > pf_b {
        let abs_delta = pf_c - pf_b;
        let pct = if pf_b == 0.0 {
            f64::INFINITY
        } else {
            (abs_delta / pf_b) * 100.0
        };
        if abs_delta >= 1.0 || pct > threshold_pct {
            regressions.push(Regression {
                metric: "phase_failure_count".to_string(),
                baseline: pf_b,
                candidate: pf_c,
                delta_pct: if pct.is_finite() {
                    pct
                } else {
                    100.0 * abs_delta
                },
                threshold_pct,
            });
        }
    }

    GateDecision {
        passed: regressions.is_empty(),
        regressions,
    }
}

fn lower_is_worse(
    metric: &str,
    baseline: f64,
    candidate: f64,
    threshold_pct: f64,
) -> Option<Regression> {
    if baseline == 0.0 {
        // Can't compute a percentage; fall back to absolute comparison —
        // any candidate worse than baseline (i.e., < baseline) when both
        // are zero is impossible, so this is a no-op.
        if candidate < baseline {
            return Some(Regression {
                metric: metric.to_string(),
                baseline,
                candidate,
                delta_pct: 100.0,
                threshold_pct,
            });
        }
        return None;
    }
    let delta_pct = ((baseline - candidate) / baseline) * 100.0;
    if delta_pct > threshold_pct {
        Some(Regression {
            metric: metric.to_string(),
            baseline,
            candidate,
            delta_pct,
            threshold_pct,
        })
    } else {
        None
    }
}

fn higher_is_worse(
    metric: &str,
    baseline: f64,
    candidate: f64,
    threshold_pct: f64,
) -> Option<Regression> {
    if baseline == 0.0 {
        if candidate > baseline {
            return Some(Regression {
                metric: metric.to_string(),
                baseline,
                candidate,
                delta_pct: 100.0,
                threshold_pct,
            });
        }
        return None;
    }
    let delta_pct = ((candidate - baseline) / baseline) * 100.0;
    if delta_pct > threshold_pct {
        Some(Regression {
            metric: metric.to_string(),
            baseline,
            candidate,
            delta_pct,
            threshold_pct,
        })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{AggregateMetrics, BaselineSnapshot};
    use std::collections::BTreeMap;

    fn fix_baseline(completion: f64, test: f64) -> BaselineSnapshot {
        BaselineSnapshot {
            aggregate: AggregateMetrics {
                asked_clarification_rate: 0.0,
                clippy_pass_rate: 1.0,
                completion_rate: completion,
                phase_failure_count: 0,
                test_pass_rate: test,
                token_cost_total: 1000,
            },
            captured_at: "x".to_string(),
            corpus: "default".to_string(),
            git_commit: "abc".to_string(),
            harness_version_hash: "h1".to_string(),
            schema_version: 1,
            tickets: BTreeMap::new(),
            wall_time_seconds: 0.0,
        }
    }

    #[test]
    fn identical_snapshots_pass() {
        let a = fix_baseline(0.9, 1.0);
        let b = fix_baseline(0.9, 1.0);
        let decision = evaluate(&a, &b, 10.0);
        assert!(decision.passed);
        assert!(decision.regressions.is_empty());
    }

    #[test]
    fn eleven_percent_drop_in_completion_fails_at_10pct() {
        let baseline = fix_baseline(1.0, 1.0);
        let candidate = fix_baseline(0.89, 1.0); // 11% drop
        let decision = evaluate(&baseline, &candidate, 10.0);
        assert!(!decision.passed);
        assert_eq!(decision.regressions.len(), 1);
        assert_eq!(decision.regressions[0].metric, "completion_rate");
    }

    #[test]
    fn boundary_just_under_threshold_passes() {
        let baseline = fix_baseline(1.0, 1.0);
        // 9.99% drop
        let candidate = fix_baseline(0.9001, 1.0);
        let decision = evaluate(&baseline, &candidate, 10.0);
        assert!(decision.passed, "9.99% must pass at 10%: {:?}", decision);
    }

    #[test]
    fn token_cost_increase_fails() {
        let mut baseline = fix_baseline(1.0, 1.0);
        baseline.aggregate.token_cost_total = 1000;
        let mut candidate = fix_baseline(1.0, 1.0);
        candidate.aggregate.token_cost_total = 1200; // 20% over
        let decision = evaluate(&baseline, &candidate, 10.0);
        assert!(!decision.passed);
        assert!(
            decision
                .regressions
                .iter()
                .any(|r| r.metric == "token_cost_total")
        );
    }

    #[test]
    fn phase_failure_increase_from_zero_fails() {
        let mut baseline = fix_baseline(1.0, 1.0);
        baseline.aggregate.phase_failure_count = 0;
        let mut candidate = fix_baseline(1.0, 1.0);
        candidate.aggregate.phase_failure_count = 1;
        let decision = evaluate(&baseline, &candidate, 10.0);
        assert!(!decision.passed, "phase_failure 0 -> 1 must fail");
        assert!(
            decision
                .regressions
                .iter()
                .any(|r| r.metric == "phase_failure_count")
        );
    }
}
