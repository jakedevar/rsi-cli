//! The per-task USD cap (#3178 item 4), checked before every model call of a
//! bench session.
//!
//! It is built on the same seam as M4's per-call spend check
//! (`openrouter_spend::SpendGuardedControl`): a [`ModelCallControl`] wrapper.
//! Before each call the session's own spend (the increment SQL, never the
//! session-cumulative rows) plus a reserve for the next call must stay below the
//! run's per-task cap. Reaching it ends the session with the terminal reason
//! `bench_task_budget_exhausted`; the task is then graded normally.
//!
//! The reserve is a conservative UPPER BOUND on what one more call can cost
//! (#3350), so an admitted call can never take the task past its cap: the
//! prompt ceiling (the model window the loop keeps every request inside) at the
//! higher of the model's input and cache-write prices plus the largest output at its
//! output price ([`call_upper_bound_usd`]). Neither the 1% floor nor the largest
//! earlier call is a bound (the first call has no history, a later call can be
//! larger than any earlier one). A model with no known price has no bound, so
//! its calls are refused (`bench_task_cost_unknown`) rather than guessed.
//!
//! The guard also fails closed: a session whose task is no longer `running` (a
//! cancelled task, or a session resumed after a daemon restart) never spends,
//! and a session with a completed call of unknown cost cannot be judged.

use crate::bench::trace::TraceSink;
use crate::error::{DaemonError, Result};
use crate::model_control::call_control::{
    AdmittedModelCall, ModelCallControl, ModelCallKind, ModelCallSettlement, ModelCallUsage,
};
use crate::store::Store;
use rsi_common::bench::BenchTaskState;
use rsi_common::bench::{
    BENCH_TASK_BUDGET_EXHAUSTED, BENCH_TASK_COST_UNKNOWN, BENCH_TASK_NOT_RUNNING,
    BENCH_TRACE_WRITE_FAILED,
};
use std::sync::Arc;
use uuid::Uuid;

/// The floor of the next-call reserve, as a fraction of the cap: the first call
/// of a session has no history to size it from.
pub const RESERVE_FLOOR_FRACTION: f64 = 0.01;
/// A spend this close to the line counts as on it (float sums).
const LINE_EPSILON_USD: f64 = 1e-9;

/// A refused call: the stable code and a detail.
#[derive(Debug, Clone, PartialEq)]
pub struct CapRefusal {
    pub code: &'static str,
    pub detail: String,
}

impl CapRefusal {
    #[must_use]
    pub fn message(&self) -> String {
        format!("{}: {}", self.code, self.detail)
    }
}

/// The most one model call can cost (#3350): `prompt_tokens` at the input
/// price or cache-write price, whichever is higher, plus
/// `max_output_tokens` at the output price (reasoning tokens are output).
#[must_use]
pub fn call_upper_bound_usd(
    price: crate::model_control::ModelPrice,
    prompt_tokens: u64,
    max_output_tokens: u64,
) -> f64 {
    let prompt_rate = price
        .input_per_mtok
        .max(price.cache_write_per_mtok.unwrap_or(price.input_per_mtok));
    (prompt_tokens as f64 * prompt_rate + max_output_tokens as f64 * price.output_per_mtok)
        / 1_000_000.0
}

/// The reserve for the next call: its upper bound, never below the largest
/// single call of the session so far or [`RESERVE_FLOOR_FRACTION`] of the cap.
#[must_use]
pub fn next_call_reserve(cap_usd: f64, max_call_usd: f64, call_bound_usd: f64) -> f64 {
    call_bound_usd
        .max(max_call_usd)
        .max(cap_usd * RESERVE_FLOOR_FRACTION)
}

/// The pure verdict: may the session make one more model call?
///
/// # Errors
/// [`BENCH_TASK_COST_UNKNOWN`] when a completed call has no known cost or the
/// model has no price to bound the next call (`call_bound_usd` is `None`),
/// [`BENCH_TASK_BUDGET_EXHAUSTED`] when `spent + reserve` reaches the cap.
pub fn judge_call(
    cap_usd: f64,
    spent_usd: Option<f64>,
    max_call_usd: f64,
    unpriced_calls: u64,
    call_bound_usd: Option<f64>,
) -> std::result::Result<(), CapRefusal> {
    if unpriced_calls > 0 {
        return Err(CapRefusal {
            code: BENCH_TASK_COST_UNKNOWN,
            detail: format!(
                "{unpriced_calls} completed model call(s) reported no cost, so the ${cap_usd:.4} \
                 per-task cap cannot be judged"
            ),
        });
    }
    let Some(call_bound_usd) = call_bound_usd else {
        return Err(CapRefusal {
            code: BENCH_TASK_COST_UNKNOWN,
            detail: format!(
                "the model has no known price, so one call cannot be bounded under the \
                 ${cap_usd:.4} per-task cap"
            ),
        });
    };
    let spent = spent_usd.unwrap_or(0.0);
    let reserve = next_call_reserve(cap_usd, max_call_usd, call_bound_usd);
    if spent + reserve + LINE_EPSILON_USD >= cap_usd {
        return Err(CapRefusal {
            code: BENCH_TASK_BUDGET_EXHAUSTED,
            detail: format!(
                "the task spent ${spent:.4} of its ${cap_usd:.4} cap (next-call reserve \
                 ${reserve:.4})"
            ),
        });
    }
    Ok(())
}

/// A model-call control that checks the task's cap before every call.
pub struct BenchTaskGuardedControl {
    inner: Arc<dyn ModelCallControl>,
    store: Arc<rsid_core::store_lock::Mutex<Store>>,
    session_id: Option<Uuid>,
    /// The cap the launch carried (millionths of a dollar); the stored run's
    /// cap wins once the session is bound to its task row.
    launch_cap_micro_usd: Option<u64>,
    /// The most one call of this session can cost ([`call_upper_bound_usd`]);
    /// `None` when the model has no known price.
    call_bound_usd: Option<f64>,
    trace: Option<Arc<TraceSink>>,
}

impl BenchTaskGuardedControl {
    #[must_use]
    pub fn new(
        inner: Arc<dyn ModelCallControl>,
        store: Arc<rsid_core::store_lock::Mutex<Store>>,
        session_id: Option<Uuid>,
        launch_cap_micro_usd: Option<u64>,
        call_bound_usd: Option<f64>,
        trace: Option<Arc<TraceSink>>,
    ) -> Self {
        Self {
            inner,
            store,
            session_id,
            launch_cap_micro_usd,
            call_bound_usd,
            trace,
        }
    }

    /// The refusal for one more call, if any.
    pub async fn refusal(&self) -> Option<CapRefusal> {
        if let Some(trace) = &self.trace
            && !trace.healthy()
        {
            return Some(CapRefusal {
                code: BENCH_TRACE_WRITE_FAILED,
                detail: "a trace line could not be written, so the trial has no full trace".into(),
            });
        }
        let session_id = self.session_id?;
        let store = self.store.lock().await;
        let task = match store.get_bench_task_by_session(session_id) {
            Ok(task) => task,
            Err(error) => {
                return Some(CapRefusal {
                    code: BENCH_TASK_NOT_RUNNING,
                    detail: format!("the task row cannot be read: {error}"),
                });
            }
        };
        let cap_usd = if let Some(task) = &task {
            if task.state != BenchTaskState::Running {
                return Some(CapRefusal {
                    code: BENCH_TASK_NOT_RUNNING,
                    detail: format!(
                        "the task is {}; a session of it spends nothing",
                        task.state.as_str()
                    ),
                });
            }
            match store.get_bench_run(task.run_id) {
                Ok(Some(run)) => run.per_task_cap_usd,
                _ => {
                    return Some(CapRefusal {
                        code: BENCH_TASK_NOT_RUNNING,
                        detail: "the task's run cannot be read".into(),
                    });
                }
            }
        } else if let Some(micro) = self.launch_cap_micro_usd {
            // Not bound yet: the launch's own cap applies until the row is.
            micro as f64 / 1_000_000.0
        } else {
            return None;
        };
        let usage = match store.bench_session_usage(session_id) {
            Ok(usage) => usage,
            Err(error) => {
                return Some(CapRefusal {
                    code: BENCH_TASK_COST_UNKNOWN,
                    detail: format!("the session's spend cannot be read: {error}"),
                });
            }
        };
        judge_call(
            cap_usd,
            usage.cost_usd,
            usage.max_call_usd,
            usage.unpriced_calls,
            self.call_bound_usd,
        )
        .err()
    }
}

#[async_trait::async_trait]
impl ModelCallControl for BenchTaskGuardedControl {
    async fn admit(
        &self,
        kind: ModelCallKind,
        dedup_hint: &str,
        fingerprint_hint: &str,
        retry_of_invocation_id: Option<Uuid>,
    ) -> Result<AdmittedModelCall> {
        if let Some(refusal) = self.refusal().await {
            // The invocation row carries the refusal's own class.
            let _ = self.inner.fail_pending(refusal.code).await;
            return Err(DaemonError::PolicyDenied(refusal.message()));
        }
        self.inner
            .admit(kind, dedup_hint, fingerprint_hint, retry_of_invocation_id)
            .await
    }

    async fn complete(&self, call: ModelCallSettlement, usage: ModelCallUsage) -> Result<()> {
        self.inner.complete(call, usage).await
    }

    async fn fail(&self, call: ModelCallSettlement, error_class: &str) -> Result<()> {
        self.inner.fail(call, error_class).await
    }

    async fn fail_pending(&self, error_class: &str) -> Result<()> {
        self.inner.fail_pending(error_class).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn the_reserve_is_the_call_bound_and_never_drops_below_history_or_one_percent() {
        assert!((next_call_reserve(0.40, 0.0, 0.0) - 0.004).abs() < 1e-12);
        assert!((next_call_reserve(0.40, 0.05, 0.0) - 0.05).abs() < 1e-12);
        assert!((next_call_reserve(0.40, 0.001, 0.0) - 0.004).abs() < 1e-12);
        // The bound wins over a smaller history, and a larger history over it.
        assert!((next_call_reserve(0.40, 0.05, 0.20) - 0.20).abs() < 1e-12);
        assert!((next_call_reserve(0.40, 0.30, 0.20) - 0.30).abs() < 1e-12);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn the_call_bound_prices_the_whole_prompt_window_and_the_largest_output() {
        let price = crate::model_control::ModelPrice {
            input_per_mtok: 3.0,
            output_per_mtok: 15.0,
            cache_read_per_mtok: Some(0.3),
            cache_write_per_mtok: None,
        };
        // 100k prompt tokens at $3/M + 32k output tokens at $15/M.
        let bound = call_upper_bound_usd(price, 100_000, 32_768);
        assert!((bound - (0.30 + 0.49152)).abs() < 1e-9, "{bound}");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn claude_call_bound_covers_a_prompt_written_entirely_to_cache() {
        let price = crate::model_control::price_for_model("claude-sonnet-5").unwrap();
        // 100k cache-write tokens at $2.50/M + 10k output at $10/M.
        let bound = call_upper_bound_usd(price, 100_000, 10_000);
        assert!((bound - 0.35).abs() < 1e-12, "{bound}");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn a_call_is_admitted_until_spend_plus_reserve_reaches_the_cap() {
        // First call: nothing spent, the floor reserve fits.
        assert_eq!(judge_call(0.40, None, 0.0, 0, Some(0.0)), Ok(()));
        // 0.30 spent, biggest call 0.05: 0.35 < 0.40.
        assert_eq!(judge_call(0.40, Some(0.30), 0.05, 0, Some(0.0)), Ok(()));
        // 0.36 spent, biggest call 0.05: the next call could cross the cap.
        let refused = judge_call(0.40, Some(0.36), 0.05, 0, Some(0.0)).unwrap_err();
        assert_eq!(refused.code, BENCH_TASK_BUDGET_EXHAUSTED);
        assert!(
            refused
                .message()
                .starts_with("bench_task_budget_exhausted: ")
        );
        // Exactly on the line counts as reached.
        assert!(judge_call(0.40, Some(0.396), 0.0, 0, Some(0.0)).is_err());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn the_first_call_is_refused_when_one_call_could_cost_the_whole_cap() {
        // No history yet, but one call could cost $0.50 of a $0.40 cap: the
        // 1% floor is no bound, so the very first call is refused.
        let refused = judge_call(0.40, None, 0.0, 0, Some(0.50)).unwrap_err();
        assert_eq!(refused.code, BENCH_TASK_BUDGET_EXHAUSTED);
        // A bound that fits is admitted.
        assert_eq!(judge_call(0.40, None, 0.0, 0, Some(0.20)), Ok(()));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn a_call_larger_than_any_earlier_call_cannot_cross_the_cap() {
        // Earlier calls cost at most $0.02, so the history reserve alone
        // ($0.02) would admit at $0.35 spent; the next call may cost $0.20.
        assert!(judge_call(0.40, Some(0.35), 0.02, 0, Some(0.0)).is_ok());
        let refused = judge_call(0.40, Some(0.35), 0.02, 0, Some(0.20)).unwrap_err();
        assert_eq!(refused.code, BENCH_TASK_BUDGET_EXHAUSTED);
        // Admitted calls stay under the cap: spent + bound < cap.
        assert!(judge_call(0.40, Some(0.15), 0.02, 0, Some(0.20)).is_ok());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn a_model_with_no_price_cannot_be_bounded_and_fails_closed() {
        let refused = judge_call(0.40, None, 0.0, 0, None).unwrap_err();
        assert_eq!(refused.code, BENCH_TASK_COST_UNKNOWN);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn an_unpriced_call_makes_the_spend_unknown_and_fails_closed() {
        let refused = judge_call(0.40, Some(0.0), 0.0, 1, Some(0.0)).unwrap_err();
        assert_eq!(refused.code, BENCH_TASK_COST_UNKNOWN);
    }
}
