//! The OpenRouter spend ceiling (#3154, program #3135 M4): the pure rules.
//!
//! The operator arms a cap (`openrouter_spend_cap_usd`, 0 = off). Arming takes
//! a fresh probe of the account `total_usage` and records it as the baseline.
//! The stop line is `baseline + cap`; a warn line sits at `warn_pct` of the cap.
//! Everything here is a pure function of the recorded inputs so the daemon, the
//! agent read (`AgentGetProviderStatus`) and the TUI row cannot disagree.
//!
//! The daemon enforces the ceiling; no agent has to remember it:
//! * launch admission refuses an OpenRouter-billed launch once the spend plus
//!   a per-launch reserve reaches the stop line ([`launch_verdict`]);
//! * the Harness loop checks before every model call against a live estimate;
//! * while the cap is armed a failed or stale probe fails CLOSED for new
//!   launches ([`SpendCapState::ProbeStale`]), never open.

use crate::types::SessionProvider;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Typed refusal / terminal reason: the spend reached the stop line (less the
/// per-launch reserve for a launch).
pub const SPEND_CAP_REACHED_CODE: &str = "openrouter_spend_cap_reached";
/// Typed refusal: the cap is armed but the probe is missing or stale, so the
/// spend is unknown and a new launch fails closed.
pub const SPEND_CAP_PROBE_STALE_CODE: &str = "openrouter_spend_cap_probe_stale";
/// Typed refusal: a benchmark run asked for more than the remainder.
pub const RUN_BUDGET_EXCEEDS_REMAINDER_CODE: &str = "openrouter_run_budget_exceeds_remainder";
/// Typed refusal: a run budget must be a positive finite number of USD.
pub const RUN_BUDGET_INVALID_CODE: &str = "openrouter_run_budget_invalid";
/// Typed refusal: the run already spent its own budget; launch no more tasks.
pub const RUN_BUDGET_EXHAUSTED_CODE: &str = "openrouter_run_budget_exhausted";
/// The most sessions one run-budget read sums.
pub const RUN_SESSIONS_MAX: usize = 256;

/// Default warn line, percent of the cap.
pub const DEFAULT_WARN_PCT: u8 = 80;
/// Largest accepted `openrouter_spend_cap_usd` (a typo guard, not a policy).
pub const SPEND_CAP_USD_LIMIT: f64 = 100_000.0;
/// The probe is refreshed at most this often (the live estimate covers the gap).
pub const PROBE_REFRESH_SECS: i64 = 60;
/// A probe older than this is stale: an armed cap fails closed for new launches.
pub const PROBE_STALE_SECS: i64 = 300;

/// A spend within this of a line counts as on it: usage figures are sums and
/// differences of floats, and a line is a round dollar amount.
const LINE_EPSILON_USD: f64 = 1e-6;

/// Per-launch reserve: a launch is refused while `spent + reserve` reaches the
/// cap, because the launch itself is about to spend. 2% of the cap, bounded to
/// `$0.05..=$0.50`.
#[must_use]
pub fn launch_reserve_usd(cap_usd: f64) -> f64 {
    (cap_usd * 0.02).clamp(0.05, 0.50)
}

/// Where the spend stands against the cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpendCapState {
    /// No cap armed.
    Off,
    /// Below the warn line.
    Ok,
    /// At or past the warn line, below the stop line.
    Warn,
    /// At or past the stop line.
    Reached,
    /// Armed, but the probe is missing or older than [`PROBE_STALE_SECS`]: the
    /// spend is unknown. New launches fail closed.
    ProbeStale,
}

impl SpendCapState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Reached => "reached",
            Self::ProbeStale => "probe_stale",
        }
    }
}

/// The agent-readable cap state (`spend_cap` on the `openrouter` entry of
/// `AgentGetProviderStatus`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenRouterSpendCapV1 {
    /// The operator cap in USD; `0` = off.
    pub cap_usd: f64,
    /// The account `total_usage` recorded when the cap was armed.
    pub baseline_usage: f64,
    /// Spend since arming: the last probed usage plus the invocations settled
    /// since that probe, minus the baseline. `null` while no probe exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spent_usd: Option<f64>,
    /// `cap - spent`, never below 0. `null` while no probe exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining_usd: Option<f64>,
    pub warn_pct: u8,
    pub state: SpendCapState,
    /// `baseline + cap`; the account `total_usage` at which spend stops.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_line_usd: Option<f64>,
    /// The per-launch reserve a launch is checked with.
    pub launch_reserve_usd: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub armed_at: Option<DateTime<Utc>>,
    /// When the account usage was last probed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probed_at: Option<DateTime<Utc>>,
}

/// Everything [`evaluate`] reads.
#[derive(Debug, Clone, Copy)]
pub struct SpendInputs {
    pub cap_usd: f64,
    pub warn_pct: u8,
    /// `(account total_usage at arming, when)`; `None` when never armed.
    pub baseline: Option<(f64, DateTime<Utc>)>,
    /// `(account total_usage, probe time)`; `None` before the first probe.
    pub probe: Option<(f64, DateTime<Utc>)>,
    /// Cost of the invocations settled since `probe`.
    pub since_probe_usd: f64,
    pub now: DateTime<Utc>,
}

/// The state of the cap. Pure.
#[must_use]
pub fn evaluate(inputs: &SpendInputs) -> OpenRouterSpendCapV1 {
    let cap = if inputs.cap_usd.is_finite() && inputs.cap_usd > 0.0 {
        inputs.cap_usd
    } else {
        0.0
    };
    let warn_pct = inputs.warn_pct.clamp(1, 100);
    let mut out = OpenRouterSpendCapV1 {
        cap_usd: cap,
        baseline_usage: inputs.baseline.map_or(0.0, |(usage, _)| usage),
        spent_usd: None,
        remaining_usd: None,
        warn_pct,
        state: SpendCapState::Off,
        stop_line_usd: None,
        launch_reserve_usd: launch_reserve_usd(cap),
        armed_at: inputs.baseline.map(|(_, at)| at),
        probed_at: inputs.probe.map(|(_, at)| at),
    };
    if cap == 0.0 {
        return out;
    }
    // Armed with no recorded baseline cannot be judged: fail closed.
    let Some((baseline, _)) = inputs.baseline else {
        out.state = SpendCapState::ProbeStale;
        return out;
    };
    out.stop_line_usd = Some(baseline + cap);
    let Some((usage, probed_at)) = inputs.probe else {
        out.state = SpendCapState::ProbeStale;
        return out;
    };
    let spent = (usage + inputs.since_probe_usd.max(0.0) - baseline).max(0.0);
    out.spent_usd = Some(spent);
    out.remaining_usd = Some((cap - spent).max(0.0));
    out.state = if (inputs.now - probed_at).num_seconds() > PROBE_STALE_SECS {
        SpendCapState::ProbeStale
    } else if spent + LINE_EPSILON_USD >= cap {
        SpendCapState::Reached
    } else if spent + LINE_EPSILON_USD >= cap * f64::from(warn_pct) / 100.0 {
        SpendCapState::Warn
    } else {
        SpendCapState::Ok
    };
    out
}

/// Why a launch or a model call was refused.
#[derive(Debug, Clone, PartialEq)]
pub struct SpendRefusal {
    pub code: &'static str,
    pub detail: String,
}

impl SpendRefusal {
    /// The `PolicyDenied` text: the stable code first, then the detail.
    #[must_use]
    pub fn message(&self) -> String {
        format!("{}: {}", self.code, self.detail)
    }
}

/// Launch admission for an OpenRouter-billed launch: `Ok` while the spend plus
/// the per-launch reserve is below the stop line.
///
/// # Errors
/// [`SPEND_CAP_REACHED_CODE`] at the line, [`SPEND_CAP_PROBE_STALE_CODE`] when
/// the armed cap cannot be judged (fail closed).
pub fn launch_verdict(cap: &OpenRouterSpendCapV1) -> Result<(), SpendRefusal> {
    match cap.state {
        SpendCapState::Off => Ok(()),
        SpendCapState::ProbeStale => Err(SpendRefusal {
            code: SPEND_CAP_PROBE_STALE_CODE,
            detail: format!(
                "the OpenRouter account usage could not be probed within {PROBE_STALE_SECS}s, \
                 so the ${:.2} spend cap cannot be judged; new OpenRouter-billed launches are \
                 refused until a probe succeeds",
                cap.cap_usd
            ),
        }),
        SpendCapState::Reached => Err(reached_refusal(cap)),
        SpendCapState::Ok | SpendCapState::Warn => {
            let spent = cap.spent_usd.unwrap_or(0.0);
            if spent + cap.launch_reserve_usd + LINE_EPSILON_USD >= cap.cap_usd {
                Err(reached_refusal(cap))
            } else {
                Ok(())
            }
        }
    }
}

/// The per-call check (no launch reserve): refuse once the spend reached the
/// stop line, or when the armed cap cannot be judged.
///
/// # Errors
/// As [`launch_verdict`].
pub fn call_verdict(cap: &OpenRouterSpendCapV1) -> Result<(), SpendRefusal> {
    match cap.state {
        SpendCapState::Off | SpendCapState::Ok | SpendCapState::Warn => Ok(()),
        SpendCapState::Reached => Err(reached_refusal(cap)),
        // A running session keeps going on a stale probe only until the probe
        // is stale: the same fail-closed rule as a launch.
        SpendCapState::ProbeStale => launch_verdict(cap),
    }
}

fn reached_refusal(cap: &OpenRouterSpendCapV1) -> SpendRefusal {
    SpendRefusal {
        code: SPEND_CAP_REACHED_CODE,
        detail: format!(
            "OpenRouter spend since arming is ${:.2} of the ${:.2} cap (launch reserve ${:.2})",
            cap.spent_usd.unwrap_or(0.0),
            cap.cap_usd,
            cap.launch_reserve_usd
        ),
    }
}

/// The one-line summary of the TUI Settings row.
#[must_use]
pub fn summary_line(cap: &OpenRouterSpendCapV1) -> String {
    match cap.state {
        SpendCapState::Off => "off (no cap armed)".to_string(),
        _ => {
            let spent = cap
                .spent_usd
                .map_or_else(|| "?".to_string(), |v| format!("${v:.2}"));
            let remaining = cap
                .remaining_usd
                .map_or_else(|| "?".to_string(), |v| format!("${v:.2}"));
            format!(
                "{}: spent {spent} of ${:.2} since arming, {remaining} left (warn {}%)",
                cap.state.as_str(),
                cap.cap_usd,
                cap.warn_pct
            )
        }
    }
}

/// Whether a launch bills the OpenRouter account: the OpenRouter provider on
/// any route, or a Harness session aimed at the OpenRouter compatible endpoint.
#[must_use]
pub fn launch_is_openrouter_billed(
    provider: SessionProvider,
    openai_base_url: Option<&str>,
) -> bool {
    match provider {
        SessionProvider::OpenRouter => true,
        SessionProvider::Harness => openai_base_url.is_some_and(is_openrouter_url),
        _ => false,
    }
}

/// Whether `url` points at `openrouter.ai` (or a subdomain).
#[must_use]
pub fn is_openrouter_url(url: &str) -> bool {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = host.split(':').next().unwrap_or("").to_ascii_lowercase();
    host == "openrouter.ai" || host.ends_with(".openrouter.ai")
}

/// A benchmark run's budget against the cap's remainder.
///
/// # Errors
/// [`RUN_BUDGET_INVALID_CODE`] for a non-positive or non-finite budget,
/// [`RUN_BUDGET_EXCEEDS_REMAINDER_CODE`] when the budget is larger than what is
/// left under an armed cap, and the launch refusals when the cap is reached or
/// cannot be judged.
pub fn admit_run_budget(
    cap: &OpenRouterSpendCapV1,
    run_budget_usd: f64,
) -> Result<(), SpendRefusal> {
    if !run_budget_usd.is_finite() || run_budget_usd <= 0.0 {
        return Err(SpendRefusal {
            code: RUN_BUDGET_INVALID_CODE,
            detail: "run_budget_usd must be a positive finite number of USD".to_string(),
        });
    }
    launch_verdict(cap)?;
    if let Some(remaining) = cap.remaining_usd
        && cap.state != SpendCapState::Off
        && run_budget_usd > remaining
    {
        return Err(SpendRefusal {
            code: RUN_BUDGET_EXCEEDS_REMAINDER_CODE,
            detail: format!(
                "run budget ${run_budget_usd:.2} is larger than the ${remaining:.2} left under the cap"
            ),
        });
    }
    Ok(())
}

/// Where a run stands against its own budget.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunBudgetStatusV1 {
    pub run_budget_usd: f64,
    pub spent_usd: f64,
    pub remaining_usd: f64,
    /// `true` once the run spent its budget: launch no more tasks.
    pub exhausted: bool,
}

/// The run's budget status from its spend so far. Pure.
#[must_use]
pub fn run_budget_status(run_budget_usd: f64, spent_usd: f64) -> RunBudgetStatusV1 {
    let spent = if spent_usd.is_finite() {
        spent_usd.max(0.0)
    } else {
        0.0
    };
    RunBudgetStatusV1 {
        run_budget_usd,
        spent_usd: spent,
        remaining_usd: (run_budget_usd - spent).max(0.0),
        exhausted: spent + LINE_EPSILON_USD >= run_budget_usd,
    }
}

/// The per-run budget verdict a benchmark runner reads from
/// `AgentGetProviderStatus` (`run_budget` on the `openrouter` entry): whether
/// it may launch the next task and where the run stands.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunBudgetReportV1 {
    /// `true` while the run may launch another task: the run has budget left,
    /// that budget fits under the cap's remainder and the cap is neither
    /// reached nor unjudgeable.
    pub admitted: bool,
    /// The typed code of the refusal when `admitted` is `false`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal_detail: Option<String>,
    pub status: RunBudgetStatusV1,
}

/// Judge a run: `spent_usd` is what its sessions have cost so far (zero for a
/// run about to start). A run with nothing left of its own budget is refused
/// [`RUN_BUDGET_EXHAUSTED_CODE`]; otherwise what is left of it must fit under
/// the cap's remainder ([`admit_run_budget`]). Pure.
#[must_use]
pub fn run_budget_report(
    cap: &OpenRouterSpendCapV1,
    run_budget_usd: f64,
    spent_usd: f64,
) -> RunBudgetReportV1 {
    let status = run_budget_status(run_budget_usd, spent_usd);
    let refusal = if !run_budget_usd.is_finite() || run_budget_usd <= 0.0 {
        Some(SpendRefusal {
            code: RUN_BUDGET_INVALID_CODE,
            detail: "run_budget_usd must be a positive finite number of USD".to_string(),
        })
    } else if status.exhausted {
        Some(SpendRefusal {
            code: RUN_BUDGET_EXHAUSTED_CODE,
            detail: format!(
                "the run spent ${:.4} of its ${run_budget_usd:.2} budget",
                status.spent_usd
            ),
        })
    } else {
        admit_run_budget(cap, status.remaining_usd).err()
    };
    RunBudgetReportV1 {
        admitted: refusal.is_none(),
        refusal_code: refusal.as_ref().map(|r| r.code.to_string()),
        refusal_detail: refusal.map(|r| r.detail),
        status,
    }
}

/// One model's price from OpenRouter's own discovery, per million tokens.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenRouterModelPriceV1 {
    pub model: String,
    pub prompt_usd_per_mtok: f64,
    pub completion_usd_per_mtok: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_cache_read_usd_per_mtok: Option<f64>,
    /// When the daemon fetched the catalog this price came from.
    pub fetched_at: DateTime<Utc>,
}

/// The most models one `AgentGetProviderStatus` price read names.
pub const MODEL_PRICES_MAX: usize = 32;
/// Longest accepted model id in a price read.
pub const MODEL_ID_MAX_LEN: usize = 128;

/// Parse a per-token price string (`"0.00000014"`) to USD per million tokens.
/// OpenRouter reports prices as decimal strings per token; a negative or
/// non-numeric value (`"-1"` marks a dynamic price) is no price.
#[must_use]
pub fn per_token_to_per_mtok(raw: &serde_json::Value) -> Option<f64> {
    let per_token = match raw {
        serde_json::Value::String(text) => text.trim().parse::<f64>().ok()?,
        serde_json::Value::Number(n) => n.as_f64()?,
        _ => return None,
    };
    (per_token.is_finite() && per_token >= 0.0).then(|| per_token * 1_000_000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(secs: i64) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-19T07:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
            + chrono::Duration::seconds(secs)
    }

    fn inputs(usage: f64, since: f64, probe_age: i64) -> SpendInputs {
        SpendInputs {
            cap_usd: 100.0,
            warn_pct: 80,
            baseline: Some((514.968, t(0))),
            probe: Some((usage, t(1000 - probe_age))),
            since_probe_usd: since,
            now: t(1000),
        }
    }

    #[test]
    fn spend_is_probe_plus_since_probe_minus_baseline() {
        let cap = evaluate(&inputs(534.968, 5.0, 10));
        assert_eq!(cap.state, SpendCapState::Ok);
        assert!((cap.spent_usd.unwrap() - 25.0).abs() < 1e-9);
        assert!((cap.remaining_usd.unwrap() - 75.0).abs() < 1e-9);
        assert!((cap.stop_line_usd.unwrap() - 614.968).abs() < 1e-9);
    }

    #[test]
    fn warn_and_reached_lines() {
        assert_eq!(
            evaluate(&inputs(514.968 + 79.9, 0.0, 10)).state,
            SpendCapState::Ok
        );
        assert_eq!(
            evaluate(&inputs(514.968 + 80.0, 0.0, 10)).state,
            SpendCapState::Warn
        );
        assert_eq!(
            evaluate(&inputs(514.968 + 99.0, 0.0, 10)).state,
            SpendCapState::Warn
        );
        assert_eq!(
            evaluate(&inputs(514.968 + 99.0, 1.0, 10)).state,
            SpendCapState::Reached
        );
    }

    #[test]
    fn launch_is_refused_inside_the_reserve_but_a_call_is_not() {
        // $99.60 spent of $100 with a $0.50 reserve: no new launch, but the
        // running session may still make a call.
        let cap = evaluate(&inputs(514.968 + 99.6, 0.0, 10));
        assert_eq!(cap.state, SpendCapState::Warn);
        let refusal = launch_verdict(&cap).unwrap_err();
        assert_eq!(refusal.code, SPEND_CAP_REACHED_CODE);
        assert!(call_verdict(&cap).is_ok());
        let reached = evaluate(&inputs(514.968 + 100.0, 0.0, 10));
        assert_eq!(
            call_verdict(&reached).unwrap_err().code,
            SPEND_CAP_REACHED_CODE
        );
    }

    #[test]
    fn a_stale_or_missing_probe_fails_closed() {
        let stale = evaluate(&inputs(520.0, 0.0, PROBE_STALE_SECS + 1));
        assert_eq!(stale.state, SpendCapState::ProbeStale);
        assert_eq!(
            launch_verdict(&stale).unwrap_err().code,
            SPEND_CAP_PROBE_STALE_CODE
        );
        let mut none = inputs(520.0, 0.0, 0);
        none.probe = None;
        let none = evaluate(&none);
        assert_eq!(none.state, SpendCapState::ProbeStale);
        assert_eq!(none.spent_usd, None);
        assert_eq!(
            launch_verdict(&none).unwrap_err().code,
            SPEND_CAP_PROBE_STALE_CODE
        );
        let mut no_baseline = inputs(520.0, 0.0, 0);
        no_baseline.baseline = None;
        assert_eq!(
            launch_verdict(&evaluate(&no_baseline)).unwrap_err().code,
            SPEND_CAP_PROBE_STALE_CODE
        );
    }

    #[test]
    fn an_off_cap_admits_everything() {
        let mut off = inputs(9999.0, 0.0, 10_000);
        off.cap_usd = 0.0;
        let cap = evaluate(&off);
        assert_eq!(cap.state, SpendCapState::Off);
        assert!(launch_verdict(&cap).is_ok());
        assert!(call_verdict(&cap).is_ok());
    }

    #[test]
    fn run_budget_is_refused_above_the_remainder_and_stops_at_its_own_line() {
        let cap = evaluate(&inputs(514.968 + 25.0, 0.0, 10));
        assert!(admit_run_budget(&cap, 70.0).is_ok());
        assert_eq!(
            admit_run_budget(&cap, 76.0).unwrap_err().code,
            RUN_BUDGET_EXCEEDS_REMAINDER_CODE
        );
        assert_eq!(
            admit_run_budget(&cap, 0.0).unwrap_err().code,
            RUN_BUDGET_INVALID_CODE
        );
        assert!(!run_budget_status(5.0, 4.99).exhausted);
        let status = run_budget_status(5.0, 5.0);
        assert!(status.exhausted);
        assert_eq!(status.remaining_usd, 0.0);
    }

    #[test]
    fn run_budget_report_admits_refuses_and_stops() {
        let cap = evaluate(&inputs(514.968 + 25.0, 0.0, 10));
        let fresh = run_budget_report(&cap, 70.0, 0.0);
        assert!(fresh.admitted && fresh.refusal_code.is_none());
        assert_eq!(
            run_budget_report(&cap, 76.0, 0.0).refusal_code.as_deref(),
            Some(RUN_BUDGET_EXCEEDS_REMAINDER_CODE)
        );
        // Mid-run only what is left of the run's budget must fit the cap.
        assert!(run_budget_report(&cap, 100.0, 40.0).admitted);
        let spent = run_budget_report(&cap, 5.0, 5.0);
        assert!(!spent.admitted && spent.status.exhausted);
        assert_eq!(
            spent.refusal_code.as_deref(),
            Some(RUN_BUDGET_EXHAUSTED_CODE)
        );
        assert_eq!(
            run_budget_report(&cap, 0.0, 0.0).refusal_code.as_deref(),
            Some(RUN_BUDGET_INVALID_CODE)
        );
        // An unjudgeable armed cap refuses the run too.
        let stale = evaluate(&inputs(514.968 + 25.0, 0.0, 10_000));
        assert_eq!(
            run_budget_report(&stale, 1.0, 0.0).refusal_code.as_deref(),
            Some(SPEND_CAP_PROBE_STALE_CODE)
        );
    }

    #[test]
    fn billing_predicate_covers_the_provider_and_the_compatible_endpoint() {
        assert!(launch_is_openrouter_billed(
            SessionProvider::OpenRouter,
            None
        ));
        assert!(launch_is_openrouter_billed(
            SessionProvider::Harness,
            Some("https://openrouter.ai/api/v1")
        ));
        assert!(!launch_is_openrouter_billed(
            SessionProvider::Harness,
            Some("https://api.openai.com/v1")
        ));
        assert!(!launch_is_openrouter_billed(
            SessionProvider::Harness,
            Some("https://openrouter.ai.evil.example/v1")
        ));
        assert!(!launch_is_openrouter_billed(SessionProvider::Harness, None));
        assert!(!launch_is_openrouter_billed(SessionProvider::Claude, None));
    }

    #[test]
    fn per_token_prices_become_per_million() {
        let v = per_token_to_per_mtok(&serde_json::json!("0.00000014")).unwrap();
        assert!((v - 0.14).abs() < 1e-9);
        assert_eq!(per_token_to_per_mtok(&serde_json::json!("-1")), None);
        assert_eq!(per_token_to_per_mtok(&serde_json::json!("free")), None);
        assert_eq!(per_token_to_per_mtok(&serde_json::json!(null)), None);
        assert_eq!(per_token_to_per_mtok(&serde_json::json!("0")), Some(0.0));
    }

    #[test]
    fn reserve_is_two_percent_bounded() {
        assert!((launch_reserve_usd(100.0) - 0.5).abs() < 1e-9);
        assert!((launch_reserve_usd(1.0) - 0.05).abs() < 1e-9);
        assert!((launch_reserve_usd(10.0) - 0.2).abs() < 1e-9);
    }
}
