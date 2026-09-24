//! Metrics aggregation: pure transform from per-session telemetry to
//! `BaselineSnapshot`. RSI-006 Phase 5.
//!
//! `aggregate()` is `(Vec<Session>, Vec<Vec<TurnMetric>>, Vec<CorpusExpected>)
//! -> BaselineSnapshot` — fully unit-testable, no I/O, no async. Float
//! rounding to 4 sig figs at write time keeps run-to-run noise out of
//! line-diffable JSON output.

use crate::corpus::CorpusExpected;
use rsi_common::types::{Session, SessionStatus, TurnMetric};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Snapshot of one corpus run, written to `eval/baselines/<harness>.json`.
/// Field order is alphabetical at the top level so the resulting JSON is
/// line-diffable across runs (BTreeMap on `tickets` enforces ticket-key
/// ordering).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BaselineSnapshot {
    pub aggregate: AggregateMetrics,
    pub captured_at: String,
    pub corpus: String,
    pub git_commit: String,
    pub harness_version_hash: String,
    pub schema_version: u32,
    pub tickets: BTreeMap<String, TicketMetrics>,
    pub wall_time_seconds: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TicketMetrics {
    pub approval_wait_ms: u64,
    pub asked_clarification: bool,
    pub clippy_passed: Option<bool>,
    pub completion_status: String,
    pub phase_failure_count: u32,
    pub retry_count: u32,
    pub test_passed: Option<bool>,
    pub token_cost_total: u64,
    pub turn_count: u32,
    pub wall_time_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AggregateMetrics {
    pub asked_clarification_rate: f64,
    pub clippy_pass_rate: f64,
    pub completion_rate: f64,
    pub phase_failure_count: u32,
    pub test_pass_rate: f64,
    pub token_cost_total: u64,
}

/// Inputs the collector consumes for every ticket. The driver assembles
/// these tuples in the same order tickets appear in the corpus.
#[derive(Debug, Clone)]
pub struct ReplayResult {
    pub ticket_id: String,
    pub session: Session,
    pub turn_metrics: Vec<TurnMetric>,
    pub expected: CorpusExpected,
}

/// Aggregate per-session telemetry into a baseline snapshot.
///
/// `harness` and `corpus` label the run; `git_commit` is a short SHA captured
/// at write time; `wall_time_seconds` is total elapsed time of the corpus
/// run.
pub fn aggregate(
    results: &[ReplayResult],
    harness_version_hash: &str,
    corpus: &str,
    git_commit: &str,
    wall_time_seconds: f64,
) -> BaselineSnapshot {
    let mut tickets: BTreeMap<String, TicketMetrics> = BTreeMap::new();
    let mut completion_count: u32 = 0;
    let mut phase_failure_count: u32 = 0;
    let mut total_token_cost: u64 = 0;
    // For *_pass_rate: only count rows that have a measurement (Some(_)).
    let mut test_total: u32 = 0;
    let mut test_pass: u32 = 0;
    let mut clippy_total: u32 = 0;
    let mut clippy_pass: u32 = 0;
    // For asked_clarification_rate: only over expected_partial=true rows.
    let mut partial_total: u32 = 0;
    let mut partial_clarified: u32 = 0;
    // For completion_rate: exclude expected_partial rows from strict ratio.
    let mut strict_total: u32 = 0;
    let mut strict_completed: u32 = 0;

    for r in results {
        let s = &r.session;
        let token_cost_total = derive_token_cost(s);
        let wall_time_ms = derive_wall_time_ms(s);
        let phase_fail = matches!(s.status, SessionStatus::Failed | SessionStatus::Interrupted);
        let asked_clarification =
            r.expected.expected_partial && matches!(s.status, SessionStatus::Completed);

        let metrics = TicketMetrics {
            approval_wait_ms: s.approval_wait_ms.unwrap_or(0),
            asked_clarification,
            clippy_passed: s.clippy_passed,
            completion_status: format!("{:?}", s.status),
            phase_failure_count: u32::from(phase_fail),
            retry_count: s.retry_count.unwrap_or(0),
            test_passed: s.test_passed,
            token_cost_total,
            turn_count: s.turn_count.unwrap_or(0),
            wall_time_ms,
        };
        tickets.insert(r.ticket_id.clone(), metrics);

        // Aggregate scalars.
        total_token_cost = total_token_cost.saturating_add(token_cost_total);
        if matches!(s.status, SessionStatus::Completed | SessionStatus::Archived) {
            completion_count = completion_count.saturating_add(1);
        }
        if phase_fail {
            phase_failure_count = phase_failure_count.saturating_add(1);
        }
        if let Some(p) = s.test_passed {
            test_total += 1;
            if p {
                test_pass += 1;
            }
        }
        if let Some(p) = s.clippy_passed {
            clippy_total += 1;
            if p {
                clippy_pass += 1;
            }
        }
        if r.expected.expected_partial {
            partial_total += 1;
            if asked_clarification {
                partial_clarified += 1;
            }
        } else {
            strict_total += 1;
            if matches!(s.status, SessionStatus::Completed | SessionStatus::Archived) {
                strict_completed += 1;
            }
        }
    }

    let aggregate = AggregateMetrics {
        asked_clarification_rate: round_sig_figs(rate(partial_clarified, partial_total), 4),
        clippy_pass_rate: round_sig_figs(rate(clippy_pass, clippy_total), 4),
        completion_rate: round_sig_figs(rate(strict_completed, strict_total), 4),
        phase_failure_count,
        test_pass_rate: round_sig_figs(rate(test_pass, test_total), 4),
        token_cost_total: total_token_cost,
    };

    BaselineSnapshot {
        aggregate,
        captured_at: chrono::Utc::now().to_rfc3339(),
        corpus: corpus.to_string(),
        git_commit: git_commit.to_string(),
        harness_version_hash: harness_version_hash.to_string(),
        schema_version: 1,
        tickets,
        wall_time_seconds: round_sig_figs(wall_time_seconds, 4),
    }
}

fn rate(numer: u32, denom: u32) -> f64 {
    if denom == 0 {
        0.0
    } else {
        f64::from(numer) / f64::from(denom)
    }
}

fn derive_token_cost(s: &Session) -> u64 {
    if let Some(usd) = s.cost_usd {
        // microcents to avoid float-equality issues in diffs.
        // Cap at u64::MAX in the pathological case (negative/NaN cost).
        if usd.is_finite() && usd >= 0.0 {
            return (usd * 1_000_000.0).round() as u64;
        }
        return 0;
    }
    let tin = s.total_input_tokens.unwrap_or(0);
    let tout = s.total_output_tokens.unwrap_or(0);
    let tcr = s.total_cache_creation_tokens.unwrap_or(0);
    let trd = s.total_cache_read_tokens.unwrap_or(0);
    tin.saturating_add(tout)
        .saturating_add(tcr)
        .saturating_add(trd)
}

fn derive_wall_time_ms(s: &Session) -> u64 {
    if let Some(ms) = s.duration_ms {
        return ms;
    }
    let delta = s.updated_at.signed_duration_since(s.created_at);
    u64::try_from(delta.num_milliseconds().max(0)).unwrap_or(0)
}

/// Round to N significant figures. Used so identical inputs produce
/// byte-identical JSON for line diffability.
#[must_use]
pub fn round_sig_figs(x: f64, sig_figs: u32) -> f64 {
    if x == 0.0 || !x.is_finite() {
        return x;
    }
    let d = x.abs().log10().ceil() as i32;
    let sig = i32::try_from(sig_figs).unwrap_or(4);
    let power = sig - d;
    let scale = 10f64.powi(power);
    (x * scale).round() / scale
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::corpus::SessionKindLabel;
    use chrono::{TimeZone, Utc};
    use rsi_common::types::{
        ContextUsageConfidence, Session, SessionKind, SessionProvider, SessionStatus,
    };
    use std::path::PathBuf;
    use uuid::Uuid;

    fn fix_session(status: SessionStatus, test_passed: Option<bool>) -> Session {
        Session {
            context_fill_pct: None,
            id: Uuid::new_v4(),
            provider: SessionProvider::Claude,
            claude_session_id: None,
            query: "Q".to_string(),
            title: None,
            agent_role: None,
            epic_spawn_ordinal: None,
            description: None,
            short_summary: None,
            working_dir: PathBuf::from("/tmp"),
            git_branch: None,
            status,
            project_id: None,
            pinned_at: None,
            testing_needed_at: None,
            rotation_disabled_at: None,
            created_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            updated_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 5).unwrap(),
            cost_usd: None,
            duration_ms: Some(5000),
            num_turns: None,
            model: None,
            input_tokens: None,
            output_tokens: None,
            context_window: None,
            resolved_context_budget: None,
            total_input_tokens: Some(100),
            session_kind: SessionKind::Bug,
            total_output_tokens: Some(50),
            total_cache_creation_tokens: Some(0),
            total_cache_read_tokens: Some(0),
            stop_reason: None,
            continued_from: None,
            context_usage_confidence: ContextUsageConfidence::Missing,
            daemon_input_tokens: None,
            daemon_output_tokens: None,
            handoff_filepath: None,
            active_task: None,
            group_id: None,
            tag: String::new(),
            tags: Vec::new(),
            pipeline_artifact: None,
            workflow_id: None,
            workflow_id_override: None,
            pending_question: None,
            pending_archive: false,
            rotation_depth: 0,
            retry_attempt: None,
            max_retries: None,
            effort: None,
            issue_identifier: None,
            issue_url: None,
            issue_tracker_id: None,
            scheduled_job_id: None,
            rating: None,
            harness_version_hash: Some("h1".to_string()),
            test_passed,
            clippy_passed: None,
            turn_count: Some(3),
            retry_count: Some(0),
            approval_wait_ms: Some(0),
            approval_started_at: None,
            work_time_ms: None,
            sandbox_kind: None,
            sandbox_root: None,
            sandbox_branch: None,
            sandbox_cleanup_state: None,
            parent_id: None,
            lead_session_id: None,
            is_eval: true,
            capability_class: None,
            topology_node_id: None,
            topology_iteration: 0,
            provider_cli_version: None,
            provider_capabilities: Vec::new(),
            thinking_tokens: None,
            service_tier: None,
            cache_creation_1h_tokens: None,
            cache_creation_5m_tokens: None,
            permission_denial_count: None,
            subagent_stats_json: None,
            queued_turn_count: None,
            terminal_reason: None,
        }
    }

    fn fix_expected(partial: bool) -> CorpusExpected {
        CorpusExpected {
            kind: SessionKindLabel("Bug".to_string()),
            expected_completion_status: "Completed".to_string(),
            expected_test_passed: Some(true),
            expected_clippy_passed: Some(true),
            expected_partial: partial,
        }
    }

    #[test]
    fn empty_input_produces_zero_aggregate() {
        let snapshot = aggregate(&[], "h1", "default", "deadbeef", 0.0);
        assert_eq!(snapshot.aggregate.completion_rate, 0.0);
        assert_eq!(snapshot.aggregate.token_cost_total, 0);
        assert_eq!(snapshot.aggregate.test_pass_rate, 0.0);
        assert_eq!(snapshot.aggregate.phase_failure_count, 0);
        assert!(snapshot.tickets.is_empty());
        assert_eq!(snapshot.schema_version, 1);
    }

    #[test]
    fn one_completed_one_failed_yields_half_completion() {
        let results = vec![
            ReplayResult {
                ticket_id: "a".to_string(),
                session: fix_session(SessionStatus::Completed, Some(true)),
                turn_metrics: Vec::new(),
                expected: fix_expected(false),
            },
            ReplayResult {
                ticket_id: "b".to_string(),
                session: fix_session(SessionStatus::Failed, None),
                turn_metrics: Vec::new(),
                expected: fix_expected(false),
            },
        ];
        let snapshot = aggregate(&results, "h1", "default", "deadbeef", 10.0);
        assert_eq!(snapshot.aggregate.completion_rate, 0.5);
        // test_pass_rate: only the Completed row has test_passed=Some(true);
        // the Failed row has None and is excluded from the denominator.
        assert_eq!(snapshot.aggregate.test_pass_rate, 1.0);
        assert_eq!(snapshot.aggregate.phase_failure_count, 1);
        assert_eq!(snapshot.tickets.len(), 2);
    }

    #[test]
    fn diff_stable_across_two_runs_with_same_inputs() {
        let results = vec![ReplayResult {
            ticket_id: "a".to_string(),
            session: fix_session(SessionStatus::Completed, Some(true)),
            turn_metrics: Vec::new(),
            expected: fix_expected(false),
        }];
        let s1 = aggregate(&results, "h1", "default", "deadbeef", 1.234567);
        let s2 = aggregate(&results, "h1", "default", "deadbeef", 1.234567);

        // Aggregate scalars must be byte-identical (rounding deterministic).
        assert_eq!(s1.aggregate, s2.aggregate);
        assert_eq!(s1.tickets, s2.tickets);
        // captured_at differs by definition; aggregate doesn't.
    }

    #[test]
    fn ambiguous_spec_row_uses_clarification_path() {
        let results = vec![
            // expected_partial=true: excluded from completion_rate denominator,
            // counted in asked_clarification_rate when status == Completed.
            ReplayResult {
                ticket_id: "amb".to_string(),
                session: fix_session(SessionStatus::Completed, None),
                turn_metrics: Vec::new(),
                expected: fix_expected(true),
            },
            // strict row.
            ReplayResult {
                ticket_id: "strict".to_string(),
                session: fix_session(SessionStatus::Completed, Some(true)),
                turn_metrics: Vec::new(),
                expected: fix_expected(false),
            },
        ];
        let snapshot = aggregate(&results, "h1", "default", "deadbeef", 5.0);
        assert_eq!(snapshot.aggregate.completion_rate, 1.0); // only strict row
        assert_eq!(snapshot.aggregate.asked_clarification_rate, 1.0);
        assert!(snapshot.tickets["amb"].asked_clarification);
        assert!(!snapshot.tickets["strict"].asked_clarification);
    }

    #[test]
    fn round_sig_figs_truncates_noise() {
        assert!((round_sig_figs(0.123456789, 4) - 0.1235).abs() < f64::EPSILON);
        assert!((round_sig_figs(123.456789, 4) - 123.5).abs() < f64::EPSILON);
        assert_eq!(round_sig_figs(0.0, 4), 0.0);
    }
}
