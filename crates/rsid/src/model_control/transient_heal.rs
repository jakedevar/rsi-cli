//! Transient failure classifier and heal-eligibility predicate (Issue #1015 slices A+B).
//!
//! Pure functions only — no I/O, no DB, no async. `store::transient_heal` wires
//! these into the Failed-transition path and heal scheduling.

use rsi_common::types::{SessionKind, SessionStatus};

use super::retry::{RetryClassification, classify_error_message};

// ── Slice A: transient failure classification ──────────────────────────

/// Labels whose retry only repeats the same failure. Checked first; they win
/// over any transient label in either position.
const EXCLUDED_LABELS: [&str; 4] = [
    "restart_reconciled_failed",
    "codex_resume_tool_history_invalid",
    // Context is full: a resume fails again. Rotation (E04) owns this case.
    "blocking_limit",
    // The operator's own message interrupted the tool call: a human action.
    "aborted_tools",
];

/// Terminal-reason / invocation error-class labels that mark a transient,
/// pre-provider-effect or process-level failure.
const TRANSIENT_LABELS: [&str; 10] = [
    // `stop_reason` values written by the finalizer's closed C5 cause set: a
    // killed or crashed provider process (for example an OOM kill, exit 137).
    "terminal_failure:non-zero-exit",
    "terminal_failure:process-died",
    "aborted_streaming",
    "error_during_execution",
    "provider_spawn_failed",
    "manager_resource_denied",
    "policy_denied",
    "circuit_open",
    "budget_denied",
    "manager_v2_concurrency_capacity",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransientVerdict {
    Transient { reason: &'static str },
    NotTransient { reason: &'static str },
}

impl TransientVerdict {
    pub const fn is_transient(self) -> bool {
        matches!(self, Self::Transient { .. })
    }

    pub const fn reason(self) -> &'static str {
        match self {
            Self::Transient { reason } | Self::NotTransient { reason } => reason,
        }
    }
}

fn matched_label(labels: [Option<&str>; 3], set: &[&'static str]) -> Option<&'static str> {
    labels
        .into_iter()
        .flatten()
        .find_map(|label| set.iter().copied().find(|known| *known == label))
}

/// Classify a Failed session from both its `terminal_reason` and its last
/// invocation `error_class` (either may be stale or absent), then fall back to
/// explicit retryable signals in recent error text. `classify_error_message`'s
/// catch-all `Transient` for unmatched text is not a heal signal: an unknown
/// failure is not healed.
pub fn classify_transient_failure(
    terminal_reason: Option<&str>,
    error_class: Option<&str>,
    recent_error_text: &[&str],
) -> TransientVerdict {
    classify_transient_failure_with_stop(terminal_reason, None, error_class, recent_error_text)
}

/// [`classify_transient_failure`] that also reads the persisted `stop_reason`
/// (for example `terminal_failure:non-zero-exit`, `aborted_tools`).
pub fn classify_transient_failure_with_stop(
    terminal_reason: Option<&str>,
    stop_reason: Option<&str>,
    error_class: Option<&str>,
    recent_error_text: &[&str],
) -> TransientVerdict {
    let labels = [terminal_reason, stop_reason, error_class];
    if let Some(reason) = matched_label(labels, &EXCLUDED_LABELS) {
        return TransientVerdict::NotTransient { reason };
    }
    if let Some(reason) = matched_label(labels, &TRANSIENT_LABELS) {
        return TransientVerdict::Transient { reason };
    }

    let mut signal = None;
    for text in recent_error_text {
        match classify_error_message(text) {
            RetryClassification::RateLimited => {
                signal.get_or_insert("rate_limited");
            }
            RetryClassification::Overloaded => {
                signal.get_or_insert("overloaded");
            }
            RetryClassification::Network => {
                signal.get_or_insert("network");
            }
            // Catch-all for unmatched text: no signal either way.
            RetryClassification::Transient => {}
            // Auth, quota, config, input, validation, policy, cancel and
            // terminal classes are never healed; one such line wins.
            _ => {
                return TransientVerdict::NotTransient {
                    reason: "non_retryable_error_text",
                };
            }
        }
    }

    match signal {
        Some(reason) => TransientVerdict::Transient { reason },
        None => TransientVerdict::NotTransient {
            reason: "no_transient_signal",
        },
    }
}

// ── Slice B: heal eligibility predicate ─────────────────────────────────

#[derive(Debug, Clone, Copy)]
pub struct TransientHealObservation {
    pub status: SessionStatus,
    pub kind: SessionKind,
    pub is_lineage_tip: bool,
    pub is_manager_seat_tip: bool,
    pub max_retries: Option<u8>,
    pub retry_attempt: Option<u8>,
    pub has_enabled_resume_job: bool,
    pub paused: bool,
    pub human_gated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealIneligible {
    NotFailed,
    ContainerKind,
    NotLineageTip,
    ManagerSeatTip,
    K2RetryBudgetOpen,
    ResumeJobExists,
    BudgetExhausted,
    Paused,
    HumanGated,
}

pub fn heal_eligibility(obs: &TransientHealObservation) -> Result<(), HealIneligible> {
    if obs.status != SessionStatus::Failed {
        return Err(HealIneligible::NotFailed);
    }

    if !rsi_common::types::is_leaf_kind(obs.kind) {
        return Err(HealIneligible::ContainerKind);
    }

    if !obs.is_lineage_tip {
        return Err(HealIneligible::NotLineageTip);
    }

    if obs.is_manager_seat_tip {
        return Err(HealIneligible::ManagerSeatTip);
    }

    // The K2 fresh-launch retry still owns this row while its budget is open;
    // an absent attempt counter means no attempt has been spent yet.
    let max_retries = obs.max_retries.unwrap_or(0);
    if max_retries > 0 && obs.retry_attempt.unwrap_or(0) < max_retries {
        return Err(HealIneligible::K2RetryBudgetOpen);
    }

    if obs.has_enabled_resume_job {
        return Err(HealIneligible::ResumeJobExists);
    }

    if obs.paused {
        return Err(HealIneligible::Paused);
    }

    if obs.human_gated {
        return Err(HealIneligible::HumanGated);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Slice A tests ───────────────────────────────────────────────

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn aborted_streaming_terminal_reason_is_transient() {
        let verdict = classify_transient_failure(Some("aborted_streaming"), None, &[]);
        assert!(matches!(verdict, TransientVerdict::Transient { .. }));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn error_during_execution_error_class_is_transient() {
        let verdict = classify_transient_failure(None, Some("error_during_execution"), &[]);
        assert!(matches!(verdict, TransientVerdict::Transient { .. }));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn provider_spawn_failed_is_transient() {
        let verdict = classify_transient_failure(None, Some("provider_spawn_failed"), &[]);
        assert!(matches!(verdict, TransientVerdict::Transient { .. }));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn manager_v2_concurrency_capacity_is_transient() {
        let verdict =
            classify_transient_failure(None, Some("manager_v2_concurrency_capacity"), &[]);
        assert!(matches!(verdict, TransientVerdict::Transient { .. }));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn restart_reconciled_failed_is_not_transient() {
        let verdict = classify_transient_failure(Some("restart_reconciled_failed"), None, &[]);
        assert!(matches!(verdict, TransientVerdict::NotTransient { .. }));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn codex_resume_tool_history_invalid_is_not_transient() {
        let verdict =
            classify_transient_failure(None, Some("codex_resume_tool_history_invalid"), &[]);
        assert!(matches!(verdict, TransientVerdict::NotTransient { .. }));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn auth_and_quota_remain_not_transient() {
        let verdict = classify_transient_failure(None, None, &["401 unauthorized invalid api key"]);
        assert!(matches!(verdict, TransientVerdict::NotTransient { .. }));

        let verdict2 =
            classify_transient_failure(None, None, &["resource_exhausted quota exceeded"]);
        assert!(matches!(verdict2, TransientVerdict::NotTransient { .. }));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn exclusion_wins_over_transient_label() {
        let verdict = classify_transient_failure(
            Some("restart_reconciled_failed"),
            Some("aborted_streaming"),
            &[],
        );
        assert!(matches!(verdict, TransientVerdict::NotTransient { .. }));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn rate_limited_text_is_transient() {
        let verdict = classify_transient_failure(None, None, &["429 rate limit exceeded"]);
        assert!(matches!(verdict, TransientVerdict::Transient { .. }));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn no_signal_is_not_transient() {
        let verdict = classify_transient_failure(None, None, &[]);
        assert!(matches!(verdict, TransientVerdict::NotTransient { .. }));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn blocking_limit_context_full_is_not_healed() {
        let verdict = classify_transient_failure_with_stop(
            Some("blocking_limit"),
            Some("terminal_failure:non-zero-exit"),
            None,
            &["Prompt is too long"],
        );
        assert_eq!(
            verdict,
            TransientVerdict::NotTransient {
                reason: "blocking_limit"
            }
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn oom_killed_non_zero_exit_is_transient() {
        let verdict = classify_transient_failure_with_stop(
            None,
            Some("terminal_failure:non-zero-exit"),
            Some("failed"),
            &[],
        );
        assert!(verdict.is_transient());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn operator_interrupted_tool_call_is_not_healed() {
        let verdict = classify_transient_failure_with_stop(
            None,
            Some("aborted_tools"),
            Some("error_during_execution"),
            &[],
        );
        assert_eq!(
            verdict,
            TransientVerdict::NotTransient {
                reason: "aborted_tools"
            }
        );
    }

    // ── Slice B tests ───────────────────────────────────────────────

    fn eligible_leaf_obs() -> TransientHealObservation {
        TransientHealObservation {
            status: SessionStatus::Failed,
            kind: SessionKind::Standard,
            is_lineage_tip: true,
            is_manager_seat_tip: false,
            max_retries: None,
            retry_attempt: None,
            has_enabled_resume_job: false,
            paused: false,
            human_gated: false,
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn container_kind_never_eligible() {
        let mut obs = eligible_leaf_obs();
        obs.kind = SessionKind::Epic;
        assert_eq!(heal_eligibility(&obs), Err(HealIneligible::ContainerKind));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn non_tip_lineage_never_eligible() {
        let mut obs = eligible_leaf_obs();
        obs.is_lineage_tip = false;
        assert_eq!(heal_eligibility(&obs), Err(HealIneligible::NotLineageTip));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn archived_and_deleted_never_eligible() {
        let mut obs = eligible_leaf_obs();
        obs.status = SessionStatus::Archived;
        assert_eq!(heal_eligibility(&obs), Err(HealIneligible::NotFailed));

        obs.status = SessionStatus::Deleted;
        assert_eq!(heal_eligibility(&obs), Err(HealIneligible::NotFailed));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn manager_seat_tip_excluded() {
        let mut obs = eligible_leaf_obs();
        obs.is_manager_seat_tip = true;
        assert_eq!(heal_eligibility(&obs), Err(HealIneligible::ManagerSeatTip));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn k2_fresh_retry_budget_still_open_excluded() {
        let mut obs = eligible_leaf_obs();
        obs.max_retries = Some(3);
        obs.retry_attempt = Some(1);
        assert_eq!(
            heal_eligibility(&obs),
            Err(HealIneligible::K2RetryBudgetOpen)
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn existing_enabled_resume_job_excluded() {
        let mut obs = eligible_leaf_obs();
        obs.has_enabled_resume_job = true;
        assert_eq!(heal_eligibility(&obs), Err(HealIneligible::ResumeJobExists));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn paused_managed_epic_excluded() {
        let mut obs = eligible_leaf_obs();
        obs.paused = true;
        assert_eq!(heal_eligibility(&obs), Err(HealIneligible::Paused));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn human_gated_excluded() {
        let mut obs = eligible_leaf_obs();
        obs.human_gated = true;
        assert_eq!(heal_eligibility(&obs), Err(HealIneligible::HumanGated));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn otherwise_eligible_leaf_session_is_eligible() {
        assert_eq!(heal_eligibility(&eligible_leaf_obs()), Ok(()));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn k2_retry_budget_exhausted_is_eligible() {
        let mut obs = eligible_leaf_obs();
        obs.max_retries = Some(2);
        obs.retry_attempt = Some(2);
        assert_eq!(heal_eligibility(&obs), Ok(()));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn completed_is_not_failed() {
        let mut obs = eligible_leaf_obs();
        obs.status = SessionStatus::Completed;
        assert_eq!(heal_eligibility(&obs), Err(HealIneligible::NotFailed));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn exclusion_in_error_class_wins_over_transient_terminal_reason() {
        let verdict = classify_transient_failure(
            Some("aborted_streaming"),
            Some("restart_reconciled_failed"),
            &[],
        );
        assert_eq!(
            verdict,
            TransientVerdict::NotTransient {
                reason: "restart_reconciled_failed"
            }
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn stale_terminal_reason_does_not_hide_transient_error_class() {
        let verdict =
            classify_transient_failure(Some("completed"), Some("provider_spawn_failed"), &[]);
        assert_eq!(
            verdict,
            TransientVerdict::Transient {
                reason: "provider_spawn_failed"
            }
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn transient_verdict_carries_the_matched_label() {
        let verdict = classify_transient_failure(None, Some("circuit_open"), &[]);
        assert!(verdict.is_transient());
        assert_eq!(verdict.reason(), "circuit_open");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn unmatched_error_text_is_not_transient() {
        let verdict =
            classify_transient_failure(None, None, &["task could not be completed as written"]);
        assert_eq!(
            verdict,
            TransientVerdict::NotTransient {
                reason: "no_transient_signal"
            }
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn cancelled_text_is_not_healed_even_with_a_network_line() {
        let verdict = classify_transient_failure(
            None,
            None,
            &["connection reset by peer", "turn cancelled by user"],
        );
        assert_eq!(
            verdict,
            TransientVerdict::NotTransient {
                reason: "non_retryable_error_text"
            }
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn overloaded_text_is_transient_with_its_signal() {
        let verdict = classify_transient_failure(None, None, &["529 overloaded"]);
        assert_eq!(
            verdict,
            TransientVerdict::Transient {
                reason: "overloaded"
            }
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn k2_budget_open_when_no_attempt_recorded() {
        let mut obs = eligible_leaf_obs();
        obs.max_retries = Some(3);
        obs.retry_attempt = None;
        assert_eq!(
            heal_eligibility(&obs),
            Err(HealIneligible::K2RetryBudgetOpen)
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn zero_k2_budget_is_eligible() {
        let mut obs = eligible_leaf_obs();
        obs.max_retries = Some(0);
        obs.retry_attempt = None;
        assert_eq!(heal_eligibility(&obs), Ok(()));
    }
}
