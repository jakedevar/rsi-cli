//! `rsi-eval bench select` (#3196): the preregistered M6 loop decisions of program #3135
//! (plan `thoughts/shared/plans/2026-10-19-issue-3135-m6-meta-harness-loop.md`, decisions D6-D8).
//!
//! The Epic lead (or a later topology Gate) applies these rules instead of judging scores by eye:
//! at 30 search tasks and k=3 the paired standard error is about 5.6 points and the best of six
//! null candidates shows about +5 points by chance.
//!
//! Inputs, all read from one state file (see [`StateFile`]): the M5 baseline records, per candidate
//! its screen, extension and held-out validation records (`BenchRunRecord` v1), whether its
//! `verify.json` was clean, the program spend so far and the measured `$/trial`. No network and
//! no model calls: every number is a pure function of the files and the fixed seed.
//!
//! Decisions recorded for #3196:
//! * **Pooling.** Records of one candidate pool when [`check_comparable`] accepts them and their
//!   split, timeouts and sampling agree; they may differ in `k`, attempt indices and (for a rerun)
//!   task subset. A `(task, attempt)` seen twice is allowed only when the earlier trial was
//!   `infra_interrupted`: the rerun replaces it ("after the one rerun").
//! * **Candidate against baseline.** A candidate differs from the baseline in its overlay by design,
//!   so only the run-level pins (dataset, Harbor, adapter, model, effort, route, harness source
//!   digest, timeouts, sampling) are held equal; the effective-input digests and
//!   `harness_version_hash` are not.
//! * **Statistics.** Per-task pass fractions over valid trials; gain is the mean paired difference;
//!   the p value is a one-sided Monte Carlo sign-flip test (10,000 flips, add-one corrected); the
//!   interval is a 90% task-clustered percentile bootstrap. The SE is the sample standard
//!   deviation of the differences over sqrt(tasks).
//! * **Screens and ranking.** Screens count in the order the state lists candidates. Rule (c) only
//!   counts an eligible screen as progress. Extension candidates are ranked on screen-only
//!   eligibility and screen gain; the winner needs eligibility on all its search records.
//! * **Budget.** The go/no-go plan is the issue's 4 screens + 1 extension + validation, with the
//!   parts already run credited, so at the start it is exactly `357c` / `239c`. `m` is fixed from the
//!   spend before any extension ran (spend minus the cost of the extension trials in the records).

// Statistics code: float arithmetic on small counts, where the pedantic float and cast lints add
// noise without catching real defects.
#![allow(
    clippy::suboptimal_flops,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::many_single_char_names,
    clippy::too_many_lines,
    clippy::struct_excessive_bools,
    clippy::items_after_statements,
    clippy::too_long_first_doc_paragraph
)]

use std::collections::{BTreeMap, BTreeSet, btree_map::Entry};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use rsi_common::bench_record::{BenchRunRecord, Interval, RunInfo, TrialRow, check_comparable};
use rsi_common::harness_source_digest::is_unavailable;
use serde::{Deserialize, Serialize};

use crate::errors::EvalError;

/// Sign-flip permutations for every p value.
pub const FLIPS: usize = 10_000;
/// Bootstrap resamples for every interval.
pub const BOOTSTRAP_RESAMPLES: usize = 10_000;
/// Stop rule (a): screened candidates.
pub const MAX_SCREENS: usize = 8;
/// Stop rule (c): screens needed before a plateau can be declared.
pub const PLATEAU_MIN_SCREENS: usize = 4;
/// Stop rule (c): how many last screens must beat the earlier best.
pub const PLATEAU_WINDOW: usize = 3;
/// Eligibility: candidate cost per trial against the baseline's.
pub const COST_RATIO_LIMIT: f64 = 1.5;
/// Eligibility: `infra_interrupted` share of trials, percent, after the rerun.
pub const INFRA_LIMIT_PERCENT: usize = 10;
/// Validated: one-sided held-out p must be below this.
pub const VALIDATION_P: f64 = 0.10;
/// Attempts added by one extension.
pub const EXTENSION_ATTEMPTS: usize = 2;
/// Screens, extensions the go/no-go plan assumes (the issue's `357c` and `239c`).
pub const PLAN_SCREENS: usize = 4;
pub const PLAN_EXTENSIONS: usize = 1;
/// Extensions when the reserve allows, otherwise 1.
pub const EXTENSION_CANDIDATES: usize = 2;
/// MDE multiplier: z(0.95) + z(0.80) = 1.645 + 0.842.
pub const MDE_FACTOR: f64 = 2.486;
/// E[p(1-p)] of plan D6, used only when the baseline has no task with two valid trials.
pub const DEFAULT_PQ: f64 = 0.14;
/// The M6 stop line, USD.
pub const DEFAULT_STOP_LINE_USD: f64 = 11.0;
/// Search tasks (`split.json`).
pub const DEFAULT_SEARCH_TASKS: usize = 30;
/// Held-out tasks (`split.json`).
pub const DEFAULT_HELDOUT_TASKS: usize = 59;
/// Per-run `budget_usd` suggestion: this many times the baseline cost of the planned trials.
pub const BUDGET_HEADROOM: f64 = 1.5;
/// Validation attempts per task when the go/no-go says k=3 and k=1.
pub const VALIDATION_K_FULL: u32 = 3;
pub const VALIDATION_K_MIN: u32 = 1;

const EPS: f64 = 1e-9;

fn bench_err(message: impl Into<String>) -> EvalError {
    EvalError::Bench(message.into())
}

// ---------------------------------------------------------------------------------------------
// Randomness and statistics
// ---------------------------------------------------------------------------------------------

/// `SplitMix64`: tiny, seedable and identical on every platform.
struct SplitMix64(u64);

impl SplitMix64 {
    const fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_lossless)]
    const fn below(&mut self, bound: usize) -> usize {
        ((self.next_u64() as u128 * bound as u128) >> 64) as usize
    }
}

#[allow(clippy::cast_precision_loss)]
fn mean(values: &[f64]) -> f64 {
    if values.is_empty() {
        0.0
    } else {
        values.iter().sum::<f64>() / values.len() as f64
    }
}

/// Sample standard deviation over sqrt(n); `None` for fewer than two values.
#[allow(clippy::cast_precision_loss)]
fn standard_error(values: &[f64]) -> Option<f64> {
    if values.len() < 2 {
        return None;
    }
    let m = mean(values);
    let n = values.len() as f64;
    let var = values.iter().map(|v| (v - m) * (v - m)).sum::<f64>() / (n - 1.0);
    Some((var / n).sqrt())
}

fn percentile(sorted: &[f64], q: f64) -> Option<f64> {
    rsi_common::bench_record::percentile(sorted, q)
}

/// One-sided paired sign-flip permutation p for "mean difference > 0": the share of
/// [`FLIPS`] random sign assignments whose sum is at least the observed sum, with the observed
/// assignment counted (add-one), so p is never 0. Zero differences never change the sum.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn sign_flip_p(diffs: &[f64], seed: u64) -> f64 {
    let observed: f64 = diffs.iter().sum();
    let nonzero: Vec<f64> = diffs.iter().copied().filter(|d| d.abs() > EPS).collect();
    let mut rng = SplitMix64(seed);
    let mut at_least = 0usize;
    for _ in 0..FLIPS {
        let mut sum = 0.0;
        let mut bits = 0u64;
        for (i, d) in nonzero.iter().enumerate() {
            if i.is_multiple_of(64) {
                bits = rng.next_u64();
            }
            if (bits >> (i % 64)) & 1 == 1 {
                sum += d;
            } else {
                sum -= d;
            }
        }
        if sum >= observed - EPS {
            at_least += 1;
        }
    }
    (at_least + 1) as f64 / (FLIPS + 1) as f64
}

/// 90% task-clustered percentile bootstrap interval of the mean difference.
#[must_use]
pub fn bootstrap_interval_90(diffs: &[f64], seed: u64) -> Option<Interval> {
    if diffs.is_empty() {
        return None;
    }
    let mut rng = SplitMix64(seed);
    let m = diffs.len();
    let mut means: Vec<f64> = (0..BOOTSTRAP_RESAMPLES)
        .map(|_| {
            let sum: f64 = (0..m).map(|_| diffs[rng.below(m)]).sum();
            #[allow(clippy::cast_precision_loss)]
            let m = m as f64;
            sum / m
        })
        .collect();
    means.sort_by(f64::total_cmp);
    Some(Interval {
        lo: percentile(&means, 0.05)?,
        hi: percentile(&means, 0.95)?,
    })
}

/// Standard normal cdf (Abramowitz-Stegun 7.1.26 erf, absolute error below 2e-7).
fn normal_cdf(x: f64) -> f64 {
    let z = x.abs() / std::f64::consts::SQRT_2;
    let t = 1.0 / 0.327_591_1f64.mul_add(z, 1.0);
    let poly = t * 1.061_405_429f64
        .mul_add(t, -1.453_152_027)
        .mul_add(t, 1.421_413_741)
        .mul_add(t, -0.284_496_736)
        .mul_add(t, 0.254_829_592);
    let erf = 1.0 - poly * (-z * z).exp();
    if x >= 0.0 {
        0.5 * (1.0 + erf)
    } else {
        0.5 * (1.0 - erf)
    }
}

/// E[max of `n` independent standard normals], by Simpson integration of `n x phi(x) Phi(x)^(n-1)`.
/// 0 for `n <= 1`.
#[must_use]
#[allow(clippy::cast_precision_loss, clippy::cast_possible_wrap)]
pub fn expected_max_normal(n: usize) -> f64 {
    if n <= 1 {
        return 0.0;
    }
    const STEPS: usize = 20_000;
    const LIMIT: f64 = 10.0;
    let h = 2.0 * LIMIT / STEPS as f64;
    let exponent = (n - 1) as i32;
    let f = |x: f64| {
        let pdf = (-0.5 * x * x).exp() / (2.0 * std::f64::consts::PI).sqrt();
        n as f64 * x * pdf * normal_cdf(x).powi(exponent)
    };
    let mut sum = f(-LIMIT) + f(LIMIT);
    for i in 1..STEPS {
        let x = (i as f64).mul_add(h, -LIMIT);
        sum += f(x) * if i.is_multiple_of(2) { 2.0 } else { 4.0 };
    }
    sum * h / 3.0
}

// ---------------------------------------------------------------------------------------------
// Pooling and pairing
// ---------------------------------------------------------------------------------------------

/// The final trials of one or more pooled records (a rerun replaces an `infra_interrupted` trial).
#[derive(Debug, Clone)]
pub struct TrialSet {
    run: RunInfo,
    trials: Vec<TrialRow>,
    tasks: BTreeSet<String>,
}

impl TrialSet {
    /// Valid-trial `(count, passed)` per task.
    fn counts(&self) -> BTreeMap<&str, (usize, usize)> {
        let mut map: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
        for t in self.trials.iter().filter(|t| t.is_valid()) {
            let entry = map.entry(t.task.as_str()).or_default();
            entry.0 += 1;
            if t.passed {
                entry.1 += 1;
            }
        }
        map
    }

    /// Largest number of distinct attempts any task has: the pooled `k`.
    #[must_use]
    pub fn k(&self) -> u32 {
        let mut attempts: BTreeMap<&str, BTreeSet<u32>> = BTreeMap::new();
        for t in &self.trials {
            attempts
                .entry(t.task.as_str())
                .or_default()
                .insert(t.attempt);
        }
        attempts
            .values()
            .map(|a| u32::try_from(a.len()).unwrap_or(u32::MAX))
            .max()
            .unwrap_or(0)
    }

    fn cost_usd(&self) -> f64 {
        self.trials.iter().map(|t| t.cost_usd).sum()
    }

    #[allow(clippy::cast_precision_loss)]
    fn cost_per_trial(&self) -> Option<f64> {
        (!self.trials.is_empty()).then(|| self.cost_usd() / self.trials.len() as f64)
    }
}

fn run_pins_agree(a: &RunInfo, b: &RunInfo, same_split: bool) -> Result<(), EvalError> {
    let refuse = |field: &str, x: String, y: String| {
        bench_err(format!(
            "records are not poolable: {field} differs ({x} vs {y})"
        ))
    };
    if same_split && a.split != b.split {
        return Err(refuse("split", a.split.clone(), b.split.clone()));
    }
    if a.timeouts != b.timeouts {
        return Err(refuse(
            "timeouts",
            format!("{:?}", a.timeouts),
            format!("{:?}", b.timeouts),
        ));
    }
    if a.sampling != b.sampling {
        return Err(refuse(
            "sampling",
            format!("{:?}", a.sampling),
            format!("{:?}", b.sampling),
        ));
    }
    for (task, digest) in &a.image_digests {
        if let Some(other) = b.image_digests.get(task).filter(|other| *other != digest) {
            return Err(refuse(
                &format!("image_digests[{task}]"),
                digest.clone(),
                other.clone(),
            ));
        }
    }
    Ok(())
}

/// Pools `records` (non-empty) into one [`TrialSet`].
fn pool(label: &str, records: &[&BenchRunRecord], same_split: bool) -> Result<TrialSet, EvalError> {
    let first = records
        .first()
        .ok_or_else(|| bench_err(format!("{label}: no records")))?;
    let mut by_key: BTreeMap<(String, u32), TrialRow> = BTreeMap::new();
    let mut tasks: BTreeSet<String> = BTreeSet::new();
    for (index, record) in records.iter().enumerate() {
        record
            .validate()
            .map_err(|error| bench_err(format!("{label}[{index}]: {error}")))?;
        if index > 0 {
            check_comparable(first, record)
                .map_err(|error| bench_err(format!("{label}[{index}]: {error}")))?;
            run_pins_agree(&first.run, &record.run, same_split)
                .map_err(|error| bench_err(format!("{label}[{index}]: {error}")))?;
        }
        tasks.extend(record.run.tasks.iter().cloned());
        for trial in &record.trials {
            tasks.insert(trial.task.clone());
            match by_key.entry((trial.task.clone(), trial.attempt)) {
                Entry::Vacant(slot) => {
                    slot.insert(trial.clone());
                }
                Entry::Occupied(mut slot) => {
                    if slot.get().infra_interrupted {
                        slot.insert(trial.clone());
                    } else {
                        return Err(bench_err(format!(
                            "{label}[{index}]: trial {}#{} appears twice and the earlier one \
                             was not infra_interrupted (a pooled record needs fresh attempt indices)",
                            trial.task, trial.attempt
                        )));
                    }
                }
            }
        }
    }
    Ok(TrialSet {
        run: first.run.clone(),
        trials: by_key.into_values().collect(),
        tasks,
    })
}

/// A candidate and the baseline are paired when the run-level pins agree. The overlay (and with it
/// the effective-input digests and `harness_version_hash`) differs by design.
fn check_pairable(baseline: &RunInfo, candidate: &RunInfo) -> Result<(), EvalError> {
    let (a, b) = (baseline, candidate);
    let pairs: [(&str, String, String); 10] = [
        (
            "dataset_repo",
            a.dataset_repo.clone(),
            b.dataset_repo.clone(),
        ),
        (
            "dataset_commit",
            a.dataset_commit.clone(),
            b.dataset_commit.clone(),
        ),
        (
            "harbor_version",
            a.harbor_version.clone(),
            b.harbor_version.clone(),
        ),
        (
            "adapter_tree_hash",
            a.adapter_tree_hash.clone(),
            b.adapter_tree_hash.clone(),
        ),
        ("model", a.model.clone(), b.model.clone()),
        ("effort", a.effort.clone(), b.effort.clone()),
        ("route", a.route.clone(), b.route.clone()),
        (
            "harness_source_digest",
            a.harness_source_digest.clone(),
            b.harness_source_digest.clone(),
        ),
        (
            "timeouts",
            format!("{:?}", a.timeouts),
            format!("{:?}", b.timeouts),
        ),
        (
            "sampling",
            format!("{:?}", a.sampling),
            format!("{:?}", b.sampling),
        ),
    ];
    for (field, x, y) in pairs {
        if x != y || (field == "harness_source_digest" && is_unavailable(&x)) {
            return Err(bench_err(format!(
                "records are not comparable: {field} differs (baseline {x}, candidate {y})"
            )));
        }
    }
    Ok(())
}

/// One task of a paired comparison.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TaskDelta {
    pub baseline: f64,
    pub candidate: f64,
    pub diff: f64,
    pub task: String,
}

/// Paired comparison of one candidate record set against the baseline.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PairedStats {
    pub baseline_pass_rate: f64,
    pub candidate_pass_rate: f64,
    /// 90% task-clustered bootstrap interval of the gain.
    pub ci90: Option<Interval>,
    /// Mean per-task pass-rate difference, candidate minus baseline.
    pub gain: f64,
    pub improved: usize,
    /// One-sided sign-flip p for gain > 0.
    pub p_value: f64,
    pub per_task: Vec<TaskDelta>,
    pub regressed: usize,
    pub se: Option<f64>,
    pub tasks: usize,
    /// Tasks of the candidate left out because a side has no valid trial.
    pub unscored_tasks: Vec<String>,
}

#[allow(clippy::cast_precision_loss)]
fn paired(baseline: &TrialSet, candidate: &TrialSet, seed: u64) -> Result<PairedStats, EvalError> {
    check_pairable(&baseline.run, &candidate.run)?;
    let (base, cand) = (baseline.counts(), candidate.counts());
    let mut per_task = Vec::new();
    let mut unscored_tasks = Vec::new();
    for task in &candidate.tasks {
        match (base.get(task.as_str()), cand.get(task.as_str())) {
            (Some((nb, cb)), Some((nc, cc))) => {
                let (fb, fc) = (*cb as f64 / *nb as f64, *cc as f64 / *nc as f64);
                per_task.push(TaskDelta {
                    baseline: fb,
                    candidate: fc,
                    diff: fc - fb,
                    task: task.clone(),
                });
            }
            _ => unscored_tasks.push(task.clone()),
        }
    }
    if per_task.is_empty() {
        return Err(bench_err(
            "no task has a valid trial in both the baseline and the candidate",
        ));
    }
    let diffs: Vec<f64> = per_task.iter().map(|t| t.diff).collect();
    let bs: Vec<f64> = per_task.iter().map(|t| t.baseline).collect();
    let cs: Vec<f64> = per_task.iter().map(|t| t.candidate).collect();
    Ok(PairedStats {
        baseline_pass_rate: mean(&bs),
        candidate_pass_rate: mean(&cs),
        ci90: bootstrap_interval_90(&diffs, seed),
        gain: mean(&diffs),
        improved: diffs.iter().filter(|d| **d > EPS).count(),
        p_value: sign_flip_p(&diffs, seed),
        regressed: diffs.iter().filter(|d| **d < -EPS).count(),
        se: standard_error(&diffs),
        tasks: per_task.len(),
        unscored_tasks,
        per_task,
    })
}

// ---------------------------------------------------------------------------------------------
// Eligibility
// ---------------------------------------------------------------------------------------------

/// The five eligibility checks of D7 rule 1.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Eligibility {
    pub baseline_cost_per_trial_usd: Option<f64>,
    pub cost_per_trial_usd: Option<f64>,
    pub cost_ratio: Option<f64>,
    pub eligible: bool,
    pub failures: Vec<String>,
    pub infra_interrupted_trials: usize,
    pub invalid_route_trials: usize,
    pub solution_lookup_flags: usize,
    pub trials: usize,
    pub verify_clean: bool,
}

fn eligibility(verify_clean: bool, search: &TrialSet, baseline: &TrialSet) -> Eligibility {
    let trials = search.trials.len();
    let invalid_route = search.trials.iter().filter(|t| t.invalid_route).count();
    let lookups = search
        .trials
        .iter()
        .filter(|t| t.suspected_solution_lookup)
        .count();
    let infra = search.trials.iter().filter(|t| t.infra_interrupted).count();
    let base_trials: Vec<&TrialRow> = baseline
        .trials
        .iter()
        .filter(|t| search.tasks.contains(&t.task))
        .collect();
    #[allow(clippy::cast_precision_loss)]
    let base_cost = (!base_trials.is_empty())
        .then(|| base_trials.iter().map(|t| t.cost_usd).sum::<f64>() / base_trials.len() as f64);
    let cost = search.cost_per_trial();
    let ratio = match (cost, base_cost) {
        (Some(c), Some(b)) if b > 0.0 => Some(c / b),
        _ => None,
    };
    let cost_ok = match (cost, base_cost) {
        (Some(c), Some(b)) => c <= COST_RATIO_LIMIT * b * (1.0 + EPS) + EPS,
        _ => false,
    };
    let mut failures = Vec::new();
    if !verify_clean {
        failures.push("verify.json is not clean".to_string());
    }
    if invalid_route > 0 {
        failures.push(format!("{invalid_route} invalid_route trial(s)"));
    }
    if lookups > 0 {
        failures.push(format!("{lookups} solution-lookup flag(s)"));
    }
    if !cost_ok {
        failures.push(format!(
            "cost per trial {} exceeds {COST_RATIO_LIMIT}x the baseline's {}",
            money(cost),
            money(base_cost)
        ));
    }
    if infra * 100 > INFRA_LIMIT_PERCENT * trials {
        failures.push(format!(
            "{infra} of {trials} trials infra_interrupted after the rerun (limit {INFRA_LIMIT_PERCENT}%)"
        ));
    }
    Eligibility {
        baseline_cost_per_trial_usd: base_cost,
        cost_per_trial_usd: cost,
        cost_ratio: ratio,
        eligible: failures.is_empty(),
        failures,
        infra_interrupted_trials: infra,
        invalid_route_trials: invalid_route,
        solution_lookup_flags: lookups,
        trials,
        verify_clean,
    }
}

fn money(value: Option<f64>) -> String {
    value.map_or_else(|| "n/a".to_string(), |v| format!("${v:.4}"))
}

// ---------------------------------------------------------------------------------------------
// Budget
// ---------------------------------------------------------------------------------------------

/// The outcome of the cost go/no-go (D8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GoNoGo {
    /// Held-out validation at k=3 fits.
    GoK3,
    /// Only held-out validation at k=1 fits.
    GoK1,
    Defer,
}

impl GoNoGo {
    const fn validation_k(self) -> Option<u32> {
        match self {
            Self::GoK3 => Some(VALIDATION_K_FULL),
            Self::GoK1 => Some(VALIDATION_K_MIN),
            Self::Defer => None,
        }
    }
}

/// Progress already made, which the go/no-go plan credits.
#[derive(Debug, Clone, Copy)]
struct Progress {
    screened: usize,
    extended: usize,
    validated: bool,
}

#[derive(Debug, Clone, Copy)]
struct Prices {
    c: f64,
    search_tasks: usize,
    heldout_tasks: usize,
}

impl Prices {
    #[allow(clippy::cast_precision_loss)]
    fn trials_usd(self, trials: usize) -> f64 {
        trials as f64 * self.c
    }

    fn screen_usd(self) -> f64 {
        self.trials_usd(self.search_tasks)
    }

    fn extension_usd(self) -> f64 {
        self.trials_usd(self.search_tasks * EXTENSION_ATTEMPTS)
    }

    fn validation_usd(self, k: u32) -> f64 {
        self.trials_usd(self.heldout_tasks * k as usize)
    }

    /// Plan cost with k-attempt validation, parts already run credited.
    fn plan_usd(self, progress: Progress, k: u32) -> f64 {
        let screens = PLAN_SCREENS.saturating_sub(progress.screened);
        let extensions = PLAN_EXTENSIONS.saturating_sub(progress.extended);
        let mut usd = self.screen_usd() * screens as f64 + self.extension_usd() * extensions as f64;
        if !progress.validated {
            usd += self.validation_usd(k);
        }
        usd
    }
}

fn go_no_go(prices: Prices, progress: Progress, remaining: f64) -> GoNoGo {
    if prices.plan_usd(progress, VALIDATION_K_FULL) <= remaining + EPS {
        GoNoGo::GoK3
    } else if prices.plan_usd(progress, VALIDATION_K_MIN) <= remaining + EPS {
        GoNoGo::GoK1
    } else {
        GoNoGo::Defer
    }
}

/// Budget view for the report.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CostReport {
    pub extension_candidates_m: usize,
    pub extension_usd_per_candidate: f64,
    pub go_no_go: GoNoGo,
    pub plan_k1_usd: f64,
    pub plan_k3_usd: f64,
    pub remaining_usd: f64,
    /// Extensions `m` in the reserve of stop rule (b): 2 when the budget after one more screen
    /// affords them with the validation run, else 1.
    pub reserve_extensions_m: usize,
    pub reserve_usd: f64,
    /// 1.5 x the baseline cost of the trials of the next run, when the next action runs trials.
    pub run_budget_usd: Option<f64>,
    pub screen_usd: f64,
    pub spent_usd: f64,
    pub stop_line_usd: f64,
    pub usd_per_trial: f64,
    pub usd_per_trial_source: String,
    pub validation_k: Option<u32>,
    pub validation_usd_k1: f64,
    pub validation_usd_k3: f64,
}

// ---------------------------------------------------------------------------------------------
// Inputs
// ---------------------------------------------------------------------------------------------

/// Scalar inputs of a selection.
#[derive(Debug, Clone, PartialEq)]
pub struct SelectParams {
    pub heldout_tasks: usize,
    /// Mean `$/trial` measured by calibration (#3184) or M5; the baseline's own mean when absent.
    pub measured_usd_per_trial: Option<f64>,
    /// The proposer reported `no_further_hypothesis`.
    pub no_further_hypothesis: bool,
    pub search_tasks: usize,
    /// Program spend so far, USD.
    pub spent_usd: f64,
    pub stop_line_usd: f64,
}

impl Default for SelectParams {
    fn default() -> Self {
        Self {
            heldout_tasks: DEFAULT_HELDOUT_TASKS,
            measured_usd_per_trial: None,
            no_further_hypothesis: false,
            search_tasks: DEFAULT_SEARCH_TASKS,
            spent_usd: 0.0,
            stop_line_usd: DEFAULT_STOP_LINE_USD,
        }
    }
}

/// One candidate's loaded records.
#[derive(Debug, Clone)]
pub struct CandidateData {
    /// Records of the extension attempts (fresh attempt indices), search split.
    pub extension: Vec<BenchRunRecord>,
    pub id: String,
    /// Records of the first screen (and any rerun of its interrupted trials), search split.
    pub screen: Vec<BenchRunRecord>,
    /// Held-out records of the winner.
    pub validation: Vec<BenchRunRecord>,
    /// `verify.json` of the candidate was clean.
    pub verify_clean: bool,
}

/// Everything [`select`] needs.
#[derive(Debug, Clone)]
pub struct SelectData {
    /// M5 baseline records (any split; they pool by task).
    pub baseline: Vec<BenchRunRecord>,
    /// Candidates in the order they were screened.
    pub candidates: Vec<CandidateData>,
    pub params: SelectParams,
}

// ---------------------------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------------------------

/// The next action; each has its own process exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// Screen one more candidate.
    Propose,
    /// Run the listed candidates' extension attempts.
    Extend,
    /// Run the winner on the held-out split.
    Validate,
    /// Nothing more to run; report the result.
    Stop,
    /// The budget cannot pay for the plan; wait.
    Defer,
}

impl Action {
    /// Exit codes: 10 propose, 11 extend, 12 validate, 13 stop, 14 defer (1 is a setup error).
    #[must_use]
    pub const fn exit_code(self) -> u8 {
        match self {
            Self::Propose => 10,
            Self::Extend => 11,
            Self::Validate => 12,
            Self::Stop => 13,
            Self::Defer => 14,
        }
    }

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Propose => "propose",
            Self::Extend => "extend",
            Self::Validate => "validate",
            Self::Stop => "stop",
            Self::Defer => "defer",
        }
    }
}

/// Where a candidate stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateStatus {
    /// No screen record yet.
    Pending,
    Screened,
    Extended,
    Validated,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CandidateReport {
    pub eligibility: Option<Eligibility>,
    /// Gain of the extension attempts alone (free of the screening selection).
    pub extension_fresh: Option<PairedStats>,
    pub id: String,
    /// All search records pooled (screen and extension).
    pub pooled: Option<PairedStats>,
    pub pooled_k: Option<u32>,
    pub screen: Option<PairedStats>,
    /// Eligibility on the screen records alone (the extension ranking uses this).
    pub screen_eligible: Option<bool>,
    pub status: CandidateStatus,
}

/// D7 rule 2.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StopReport {
    /// (a) 8 candidates screened.
    pub a_max_screens: bool,
    /// (b) one more screen would leave less than the reserve.
    pub b_reserve: bool,
    /// (c) at least 4 screened and the last 3 did not beat the earlier best.
    pub c_plateau: bool,
    /// (d) the proposer reported `no_further_hypothesis`.
    pub d_no_further_hypothesis: bool,
    pub fired: Vec<String>,
    pub screened: usize,
    pub stop_proposing: bool,
}

/// Best-of-N note: what the best of N null candidates shows by chance.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BestOfN {
    pub expected_max_z: f64,
    /// SE x E[max of N standard normals], in pass-rate units.
    pub noise_floor: Option<f64>,
    pub n: usize,
    pub se: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WinnerReport {
    pub best_of_n: BestOfN,
    pub extended: Vec<String>,
    pub fresh_gain: f64,
    pub id: String,
    pub pooled_gain: f64,
    /// Fresh gain above 0.
    pub promoted: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ValidationReport {
    pub candidate: String,
    pub stats: PairedStats,
    /// Gain above 0 with one-sided p below 0.10.
    pub validated: bool,
}

/// One MDE row: `2.486 * sqrt(E[p(1-p)] * (1/k_b + 1/k_c) / n)`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MdeRow {
    pub baseline_k: f64,
    pub candidate_k: u32,
    pub mde: f64,
    pub n: usize,
    pub phase: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MdeTable {
    pub pq: f64,
    /// `baseline` when estimated from the baseline records, `default` for the plan's 0.14.
    pub pq_source: String,
    pub rows: Vec<MdeRow>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Selection {
    pub action: Action,
    pub bootstrap_resamples: usize,
    pub candidates: Vec<CandidateReport>,
    pub cost: CostReport,
    /// Candidates the `extend` action names.
    pub extend: Vec<String>,
    pub flips: usize,
    pub mde: MdeTable,
    /// Outcome label: `validated`, `not_validated`, `no_candidate_replicated_on_search`, ...
    pub outcome: Option<String>,
    pub reason: String,
    pub seed: u64,
    pub stop: StopReport,
    pub validation: Option<ValidationReport>,
    pub winner: Option<WinnerReport>,
}

// ---------------------------------------------------------------------------------------------
// The rules
// ---------------------------------------------------------------------------------------------

struct Prepared {
    extension: Option<TrialSet>,
    id: String,
    pooled: Option<TrialSet>,
    screen: Option<TrialSet>,
    validation: Option<TrialSet>,
    verify_clean: bool,
}

fn refs(records: &[BenchRunRecord]) -> Vec<&BenchRunRecord> {
    records.iter().collect()
}

fn prepare(candidate: &CandidateData) -> Result<Prepared, EvalError> {
    let label = |part: &str| format!("candidate {} {part}", candidate.id);
    let screen = (!candidate.screen.is_empty())
        .then(|| pool(&label("screen"), &refs(&candidate.screen), true))
        .transpose()?;
    let extension = (!candidate.extension.is_empty())
        .then(|| pool(&label("extension"), &refs(&candidate.extension), true))
        .transpose()?;
    let validation = (!candidate.validation.is_empty())
        .then(|| pool(&label("validation"), &refs(&candidate.validation), true))
        .transpose()?;
    if screen.is_none() && (extension.is_some() || validation.is_some()) {
        return Err(bench_err(format!(
            "candidate {} has extension or validation records but no screen",
            candidate.id
        )));
    }
    if extension.is_none() && validation.is_some() {
        return Err(bench_err(format!(
            "candidate {} has validation records but no extension",
            candidate.id
        )));
    }
    let pooled = if screen.is_some() {
        let all: Vec<&BenchRunRecord> = candidate
            .screen
            .iter()
            .chain(&candidate.extension)
            .collect();
        Some(pool(&label("search"), &all, true)?)
    } else {
        None
    };
    Ok(Prepared {
        extension,
        id: candidate.id.clone(),
        pooled,
        screen,
        validation,
        verify_clean: candidate.verify_clean,
    })
}

fn plateau(gains: &[Option<f64>]) -> bool {
    if gains.len() < PLATEAU_MIN_SCREENS {
        return false;
    }
    let split = gains.len() - PLATEAU_WINDOW;
    let best_before = gains[..split]
        .iter()
        .flatten()
        .copied()
        .fold(f64::NEG_INFINITY, f64::max);
    !gains[split..]
        .iter()
        .any(|g| g.is_some_and(|v| v > best_before + EPS))
}

/// Mean valid trials per task and E[p(1-p)] of a baseline, unbiased per task by k/(k-1).
#[allow(clippy::cast_precision_loss)]
fn baseline_moments(baseline: &TrialSet) -> (f64, Option<f64>) {
    let counts = baseline.counts();
    if counts.is_empty() {
        return (1.0, None);
    }
    let k_b = counts.values().map(|(n, _)| *n as f64).sum::<f64>() / counts.len() as f64;
    let pqs: Vec<f64> = counts
        .values()
        .filter(|(n, _)| *n >= 2)
        .map(|(n, c)| {
            let (n, p) = (*n as f64, *c as f64 / *n as f64);
            p * (1.0 - p) * n / (n - 1.0)
        })
        .collect();
    (k_b, (!pqs.is_empty()).then(|| mean(&pqs)))
}

#[allow(clippy::cast_precision_loss)]
fn mde_table(baseline: &TrialSet, prices: Prices, validation_k: u32, pooled_k: u32) -> MdeTable {
    let (k_b, pq) = baseline_moments(baseline);
    let (pq_value, pq_source) = pq.map_or((DEFAULT_PQ, "default"), |v| (v, "baseline"));
    let row = |phase: &str, n: usize, k_c: u32| MdeRow {
        baseline_k: k_b,
        candidate_k: k_c,
        mde: MDE_FACTOR * (pq_value * (1.0 / k_b + 1.0 / f64::from(k_c)) / n as f64).sqrt(),
        n,
        phase: phase.to_string(),
    };
    MdeTable {
        pq: pq_value,
        pq_source: pq_source.to_string(),
        rows: vec![
            row("search screen", prices.search_tasks, 1),
            row("search pooled", prices.search_tasks, pooled_k),
            row("held-out validation", prices.heldout_tasks, validation_k),
        ],
    }
}

/// Applies the preregistered rules to `data` and returns the next action.
///
/// # Errors
/// [`EvalError::Bench`] for records that cannot be pooled or paired, a candidate with extension
/// or validation records that the rules did not select, or missing baseline data.
#[allow(clippy::too_many_lines, clippy::cast_precision_loss)]
pub fn select(data: &SelectData, seed: u64) -> Result<Selection, EvalError> {
    let params = &data.params;
    let baseline = pool("baseline", &refs(&data.baseline), false)?;
    if baseline.trials.is_empty() {
        return Err(bench_err("baseline records hold no trials"));
    }
    let mut seen_ids = BTreeSet::new();
    for candidate in &data.candidates {
        if !seen_ids.insert(candidate.id.as_str()) {
            return Err(bench_err(format!(
                "duplicate candidate id {}",
                candidate.id
            )));
        }
    }
    let prepared: Vec<Prepared> = data
        .candidates
        .iter()
        .map(prepare)
        .collect::<Result<_, _>>()?;

    // Per-candidate statistics.
    let mut reports = Vec::with_capacity(prepared.len());
    let mut screen_eligible: Vec<Option<bool>> = Vec::new();
    for p in &prepared {
        let screen = p.screen.as_ref();
        let screen_stats = screen.map(|s| paired(&baseline, s, seed)).transpose()?;
        let ext_stats = p
            .extension
            .as_ref()
            .map(|s| paired(&baseline, s, seed))
            .transpose()?;
        let pooled_stats = p
            .extension
            .as_ref()
            .and(p.pooled.as_ref())
            .map(|s| paired(&baseline, s, seed))
            .transpose()?;
        let elig = p
            .pooled
            .as_ref()
            .map(|s| eligibility(p.verify_clean, s, &baseline));
        let screen_elig = screen.map(|s| eligibility(p.verify_clean, s, &baseline).eligible);
        screen_eligible.push(screen_elig);
        let status = if p.validation.is_some() {
            CandidateStatus::Validated
        } else if p.extension.is_some() {
            CandidateStatus::Extended
        } else if p.screen.is_some() {
            CandidateStatus::Screened
        } else {
            CandidateStatus::Pending
        };
        reports.push(CandidateReport {
            eligibility: elig,
            extension_fresh: ext_stats,
            id: p.id.clone(),
            pooled: pooled_stats,
            pooled_k: p.extension.as_ref().and(p.pooled.as_ref()).map(TrialSet::k),
            screen: screen_stats,
            screen_eligible: screen_elig,
            status,
        });
    }

    let screened_idx: Vec<usize> = (0..prepared.len())
        .filter(|&i| prepared[i].screen.is_some())
        .collect();
    let extended_idx: Vec<usize> = (0..prepared.len())
        .filter(|&i| prepared[i].extension.is_some())
        .collect();
    let validated_idx: Vec<usize> = (0..prepared.len())
        .filter(|&i| prepared[i].validation.is_some())
        .collect();
    if validated_idx.len() > 1 {
        return Err(bench_err("only the winner may have validation records"));
    }

    // Prices and budget.
    let (c, c_source) = match params.measured_usd_per_trial {
        Some(c) => (c, "measured"),
        None => (baseline.cost_per_trial().unwrap_or(0.0), "baseline"),
    };
    if !c.is_finite() || c < 0.0 {
        return Err(bench_err("usd per trial must be a non-negative number"));
    }
    let prices = Prices {
        c,
        heldout_tasks: params.heldout_tasks,
        search_tasks: params.search_tasks,
    };
    let progress = Progress {
        extended: extended_idx.len(),
        screened: screened_idx.len(),
        validated: !validated_idx.is_empty(),
    };
    let remaining = params.stop_line_usd - params.spent_usd;
    let go = go_no_go(prices, progress, remaining);
    let k_for_reserve = go.validation_k().unwrap_or(VALIDATION_K_MIN);

    // Stop rule (b) and the extension count m use the budget before any extension ran.
    let ext_spent: f64 = extended_idx
        .iter()
        .filter_map(|&i| prepared[i].extension.as_ref())
        .map(TrialSet::cost_usd)
        .sum();
    let pre_ext_remaining = params.stop_line_usd - (params.spent_usd - ext_spent).max(0.0);
    let pre_progress = Progress {
        extended: 0,
        screened: progress.screened,
        validated: false,
    };
    let pre_go = go_no_go(prices, pre_progress, pre_ext_remaining);
    let pre_k = pre_go.validation_k().unwrap_or(VALIDATION_K_MIN);
    let reserve_with =
        |m: usize, k: u32| prices.extension_usd() * m as f64 + prices.validation_usd(k);
    let m = if pre_ext_remaining + EPS >= reserve_with(EXTENSION_CANDIDATES, pre_k) {
        EXTENSION_CANDIDATES
    } else {
        1
    };

    let gains: Vec<Option<f64>> = screened_idx
        .iter()
        .map(|&i| {
            (screen_eligible[i] == Some(true))
                .then(|| reports[i].screen.as_ref().map(|s| s.gain))
                .flatten()
        })
        .collect();
    let a = screened_idx.len() >= MAX_SCREENS;
    // Rule (b): the budget left after one more screen must still cover the reserve, which is m
    // extensions plus the validation run with m = 2 when the reserve allows it, else 1.
    let after_screen = remaining - prices.screen_usd();
    let reserve_m = if after_screen + EPS >= reserve_with(EXTENSION_CANDIDATES, k_for_reserve) {
        EXTENSION_CANDIDATES
    } else {
        1
    };
    let reserve_usd = reserve_with(reserve_m, k_for_reserve);
    let b = after_screen + EPS < reserve_usd;
    let cc = plateau(&gains);
    let d = params.no_further_hypothesis;
    let mut fired = Vec::new();
    for (flag, name) in [
        (a, "a: 8 candidates screened"),
        (b, "b: one more screen would leave less than the reserve"),
        (
            cc,
            "c: the last 3 screens did not beat the best screen gain",
        ),
        (d, "d: proposer reported no_further_hypothesis"),
    ] {
        if flag {
            fired.push(name.to_string());
        }
    }
    let stop = StopReport {
        a_max_screens: a,
        b_reserve: b,
        c_plateau: cc,
        d_no_further_hypothesis: d,
        fired,
        screened: screened_idx.len(),
        stop_proposing: a || b || cc || d,
    };

    // Extension set: top m screen-eligible candidates with screen gain above 0, by screen gain.
    let mut ranked: Vec<usize> = screened_idx
        .iter()
        .copied()
        .filter(|&i| {
            screen_eligible[i] == Some(true)
                && reports[i].screen.as_ref().is_some_and(|s| s.gain > EPS)
        })
        .collect();
    ranked.sort_by(|&x, &y| {
        let (gx, gy) = (
            reports[x].screen.as_ref().map_or(0.0, |s| s.gain),
            reports[y].screen.as_ref().map_or(0.0, |s| s.gain),
        );
        gy.total_cmp(&gx).then(x.cmp(&y))
    });
    ranked.truncate(m);

    let run_budget = |trials: usize| BUDGET_HEADROOM * baseline_cost(&baseline) * trials as f64;
    let mut cost = CostReport {
        extension_candidates_m: m,
        extension_usd_per_candidate: prices.extension_usd(),
        go_no_go: go,
        plan_k1_usd: prices.plan_usd(progress, VALIDATION_K_MIN),
        plan_k3_usd: prices.plan_usd(progress, VALIDATION_K_FULL),
        remaining_usd: remaining,
        reserve_extensions_m: reserve_m,
        reserve_usd,
        run_budget_usd: None,
        screen_usd: prices.screen_usd(),
        spent_usd: params.spent_usd,
        stop_line_usd: params.stop_line_usd,
        usd_per_trial: c,
        usd_per_trial_source: c_source.to_string(),
        validation_k: go.validation_k(),
        validation_usd_k1: prices.validation_usd(VALIDATION_K_MIN),
        validation_usd_k3: prices.validation_usd(VALIDATION_K_FULL),
    };
    let pooled_k_hint = 1 + EXTENSION_ATTEMPTS as u32;

    let finish = |action: Action,
                  reason: String,
                  outcome: Option<&str>,
                  extend: Vec<String>,
                  cost: CostReport,
                  winner: Option<WinnerReport>,
                  validation: Option<ValidationReport>| {
        let vk = cost.validation_k.unwrap_or(VALIDATION_K_MIN);
        Selection {
            action,
            bootstrap_resamples: BOOTSTRAP_RESAMPLES,
            candidates: reports.clone(),
            extend,
            flips: FLIPS,
            mde: mde_table(&baseline, prices, vk, pooled_k_hint),
            outcome: outcome.map(str::to_string),
            reason,
            seed,
            stop: stop.clone(),
            validation,
            winner,
            cost,
        }
    };

    if let Some(&stray) = extended_idx.iter().find(|i| !ranked.contains(i)) {
        return Err(bench_err(format!(
            "candidate {} has extension records but is not in the preregistered extension set",
            prepared[stray].id
        )));
    }

    // Phase 4: a validated candidate ends the loop.
    if let Some(&vi) = validated_idx.first() {
        let decision = decide_winner(&prepared, &reports, &extended_idx, screened_idx.len())?;
        let Some(w) = decision
            .winner
            .as_ref()
            .filter(|w| w.id == prepared[vi].id && w.promoted)
        else {
            return Err(bench_err(format!(
                "candidate {} has validation records but is not the promoted winner",
                prepared[vi].id
            )));
        };
        let held = prepared[vi]
            .validation
            .as_ref()
            .ok_or_else(|| bench_err("validation set"))?;
        let search = prepared[vi]
            .pooled
            .as_ref()
            .ok_or_else(|| bench_err("search set"))?;
        if let Some(task) = held.tasks.intersection(&search.tasks).next() {
            return Err(bench_err(format!(
                "validation task {task} is also a search task: held-out records must cover the held-out split only"
            )));
        }
        let stats = paired(&baseline, held, seed)?;
        let validated = stats.gain > EPS && stats.p_value < VALIDATION_P;
        let outcome = if validated {
            "validated"
        } else {
            "not_validated"
        };
        let reason = format!(
            "held-out gain {:+.1} points, one-sided p {:.4} (limit {VALIDATION_P}): {}",
            stats.gain * 100.0,
            stats.p_value,
            if validated {
                "validated"
            } else {
                "not validated; report the measured gain and interval anyway"
            }
        );
        let winner = Some(w.clone());
        return Ok(finish(
            Action::Stop,
            reason,
            Some(outcome),
            Vec::new(),
            cost,
            winner,
            Some(ValidationReport {
                candidate: prepared[vi].id.clone(),
                stats,
                validated,
            }),
        ));
    }

    // Phase 3: extensions have started.
    if !extended_idx.is_empty() {
        let pending: Vec<String> = ranked
            .iter()
            .filter(|i| prepared[**i].extension.is_none())
            .map(|&i| prepared[i].id.clone())
            .collect();
        if !pending.is_empty() {
            cost.run_budget_usd = Some(run_budget(params.search_tasks * EXTENSION_ATTEMPTS));
            return Ok(finish(
                Action::Extend,
                format!("extension still to run for {}", pending.join(", ")),
                None,
                pending,
                cost,
                None,
                None,
            ));
        }
        let decision = decide_winner(&prepared, &reports, &extended_idx, screened_idx.len())?;
        let Some(winner) = decision.winner else {
            return Ok(finish(
                Action::Stop,
                decision.note,
                Some("no_eligible_extended_candidate"),
                Vec::new(),
                cost,
                None,
                None,
            ));
        };
        if !winner.promoted {
            let reason = format!(
                "no candidate replicated on search: the winner {}'s fresh extension gain is {:+.1} points (not above 0)",
                winner.id,
                winner.fresh_gain * 100.0
            );
            return Ok(finish(
                Action::Stop,
                reason,
                Some("no_candidate_replicated_on_search"),
                Vec::new(),
                cost,
                Some(winner),
                None,
            ));
        }
        let Some(k) = go.validation_k() else {
            let reason = format!(
                "winner {} is promoted but validation does not fit: {:.2} USD remain and k=1 needs {:.2}",
                winner.id,
                remaining,
                prices.validation_usd(VALIDATION_K_MIN)
            );
            return Ok(finish(
                Action::Defer,
                reason,
                None,
                Vec::new(),
                cost,
                Some(winner),
                None,
            ));
        };
        cost.run_budget_usd = Some(run_budget(params.heldout_tasks * k as usize));
        let reason = format!(
            "winner {} (pooled {:+.1}, fresh {:+.1} points) is promoted: validate on the held-out split at k={k}",
            winner.id,
            winner.pooled_gain * 100.0,
            winner.fresh_gain * 100.0
        );
        return Ok(finish(
            Action::Validate,
            reason,
            None,
            Vec::new(),
            cost,
            Some(winner),
            None,
        ));
    }

    // Phase 1: nothing screened, and the budget decides whether to start at all.
    if screened_idx.is_empty() && go == GoNoGo::Defer {
        let reason = format!(
            "defer: the plan needs {:.2} USD at k=1 validation ({:.2} at k=3) and {:.2} remain at {:.4} USD per trial",
            cost.plan_k1_usd, cost.plan_k3_usd, remaining, c
        );
        return Ok(finish(
            Action::Defer,
            reason,
            None,
            Vec::new(),
            cost,
            None,
            None,
        ));
    }

    // Phase 2: screening.
    if !stop.stop_proposing {
        cost.run_budget_usd = Some(run_budget(params.search_tasks));
        let reason = format!(
            "{} of at most {MAX_SCREENS} candidates screened and no stop rule fired",
            screened_idx.len()
        );
        return Ok(finish(
            Action::Propose,
            reason,
            None,
            Vec::new(),
            cost,
            None,
            None,
        ));
    }
    if ranked.is_empty() {
        let reason = format!(
            "stop proposing ({}); no eligible candidate has a screen gain above 0",
            stop.fired.join("; ")
        );
        return Ok(finish(
            Action::Stop,
            reason,
            Some("no_candidate_beat_baseline_on_screen"),
            Vec::new(),
            cost,
            None,
            None,
        ));
    }
    cost.run_budget_usd = Some(run_budget(params.search_tasks * EXTENSION_ATTEMPTS));
    let names: Vec<String> = ranked.iter().map(|&i| prepared[i].id.clone()).collect();
    let reason = format!(
        "stop proposing ({}); extend the top {} by screen gain",
        stop.fired.join("; "),
        names.len()
    );
    Ok(finish(
        Action::Extend,
        reason,
        None,
        names,
        cost,
        None,
        None,
    ))
}

fn baseline_cost(baseline: &TrialSet) -> f64 {
    baseline.cost_per_trial().unwrap_or(0.0)
}

struct WinnerDecision {
    note: String,
    winner: Option<WinnerReport>,
}

/// The highest pooled gain among extended candidates that are eligible on all their search
/// records; ties go to the earlier candidate.
fn decide_winner(
    prepared: &[Prepared],
    reports: &[CandidateReport],
    extended_idx: &[usize],
    screened: usize,
) -> Result<WinnerDecision, EvalError> {
    let mut best: Option<usize> = None;
    for &i in extended_idx {
        let eligible = reports[i].eligibility.as_ref().is_some_and(|e| e.eligible);
        let Some(pooled) = reports[i].pooled.as_ref() else {
            continue;
        };
        if !eligible {
            continue;
        }
        let better = best.is_none_or(|b| {
            reports[b]
                .pooled
                .as_ref()
                .is_some_and(|bp| pooled.gain > bp.gain + EPS)
        });
        if better {
            best = Some(i);
        }
    }
    let Some(i) = best else {
        return Ok(WinnerDecision {
            note: "no extended candidate is eligible on all its search records".to_string(),
            winner: None,
        });
    };
    let pooled = reports[i]
        .pooled
        .as_ref()
        .ok_or_else(|| bench_err("pooled statistics"))?;
    let fresh = reports[i]
        .extension_fresh
        .as_ref()
        .ok_or_else(|| bench_err("extension statistics"))?;
    let n = screened;
    let z = expected_max_normal(n);
    Ok(WinnerDecision {
        note: String::new(),
        winner: Some(WinnerReport {
            best_of_n: BestOfN {
                expected_max_z: z,
                n,
                noise_floor: pooled.se.map(|se| se * z),
                se: pooled.se,
            },
            extended: extended_idx
                .iter()
                .map(|&j| prepared[j].id.clone())
                .collect(),
            fresh_gain: fresh.gain,
            id: prepared[i].id.clone(),
            pooled_gain: pooled.gain,
            promoted: fresh.gain > EPS,
        }),
    })
}

// ---------------------------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------------------------

fn pts(value: f64) -> String {
    format!("{:+.1}", value * 100.0)
}

fn interval(ci: Option<Interval>) -> String {
    ci.map_or_else(
        || "n/a".to_string(),
        |i| format!("[{}, {}]", pts(i.lo), pts(i.hi)),
    )
}

fn stats_cells(stats: Option<&PairedStats>) -> String {
    stats.map_or_else(
        || "n/a | n/a | n/a".to_string(),
        |s| format!("{} | {:.4} | {}", pts(s.gain), s.p_value, interval(s.ci90)),
    )
}

/// Markdown report of a [`Selection`].
#[must_use]
pub fn render_markdown(selection: &Selection) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# rsi-eval bench select");
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "- **Next action**: `{}` (exit {})",
        selection.action.name(),
        selection.action.exit_code()
    );
    let _ = writeln!(out, "- **Reason**: {}", selection.reason);
    if let Some(outcome) = &selection.outcome {
        let _ = writeln!(out, "- **Outcome**: {outcome}");
    }
    if !selection.extend.is_empty() {
        let _ = writeln!(out, "- **Extend**: {}", selection.extend.join(", "));
    }
    if let Some(budget) = selection.cost.run_budget_usd {
        let _ = writeln!(out, "- **Suggested run `budget_usd`**: {budget:.2}");
    }
    let _ = writeln!(
        out,
        "- Gains are in points of pass rate, candidate minus baseline; p is a one-sided paired \
         sign-flip test ({} flips); intervals are 90% task-clustered bootstrap ({} resamples); seed {}.",
        selection.flips, selection.bootstrap_resamples, selection.seed
    );
    let _ = writeln!(out);
    let _ = writeln!(out, "## Candidates (search, exploratory)");
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "| candidate | status | eligible | screen gain | p | 90% interval | fresh gain | pooled gain | pooled p |"
    );
    let _ = writeln!(out, "|---|---|---|---|---|---|---|---|---|");
    for c in &selection.candidates {
        let eligible = c.eligibility.as_ref().map_or_else(
            || "n/a".to_string(),
            |e| {
                if e.eligible {
                    "yes".to_string()
                } else {
                    format!("no: {}", e.failures.join("; "))
                }
            },
        );
        let screen = stats_cells(c.screen.as_ref());
        let fresh = c
            .extension_fresh
            .as_ref()
            .map_or_else(|| "n/a".to_string(), |s| pts(s.gain));
        let (pooled, pooled_p) = c.pooled.as_ref().map_or_else(
            || ("n/a".to_string(), "n/a".to_string()),
            |s| (pts(s.gain), format!("{:.4}", s.p_value)),
        );
        let _ = writeln!(
            out,
            "| {} | {:?} | {} | {} | {} | {} | {} |",
            c.id, c.status, eligible, screen, fresh, pooled, pooled_p
        );
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "## Stop rules");
    let _ = writeln!(out);
    let _ = writeln!(out, "| rule | fired |");
    let _ = writeln!(out, "|---|---|");
    for (name, flag) in [
        ("(a) 8 candidates screened", selection.stop.a_max_screens),
        (
            "(b) less than the reserve after one more screen",
            selection.stop.b_reserve,
        ),
        (
            "(c) last 3 screens did not beat the best",
            selection.stop.c_plateau,
        ),
        (
            "(d) no_further_hypothesis",
            selection.stop.d_no_further_hypothesis,
        ),
    ] {
        let _ = writeln!(out, "| {name} | {} |", if flag { "yes" } else { "no" });
    }
    let _ = writeln!(out);
    let cost = &selection.cost;
    let _ = writeln!(out, "## Cost go/no-go");
    let _ = writeln!(out);
    let _ = writeln!(out, "| item | USD |");
    let _ = writeln!(out, "|---|---|");
    let _ = writeln!(
        out,
        "| mean per trial c ({}) | {:.4} |",
        cost.usd_per_trial_source, cost.usd_per_trial
    );
    let _ = writeln!(
        out,
        "| spent / stop line | {:.2} / {:.2} |",
        cost.spent_usd, cost.stop_line_usd
    );
    let _ = writeln!(out, "| remaining | {:.2} |", cost.remaining_usd);
    let _ = writeln!(
        out,
        "| plan with k=3 validation | {:.2} |",
        cost.plan_k3_usd
    );
    let _ = writeln!(
        out,
        "| plan with k=1 validation | {:.2} |",
        cost.plan_k1_usd
    );
    let _ = writeln!(
        out,
        "| reserve ({} extension(s) + validation) | {:.2} |",
        cost.reserve_extensions_m, cost.reserve_usd
    );
    let _ = writeln!(out, "| extensions m | {} |", cost.extension_candidates_m);
    let _ = writeln!(out);
    let _ = writeln!(out, "Decision: `{:?}`.", cost.go_no_go);
    if let Some(w) = &selection.winner {
        let _ = writeln!(out);
        let _ = writeln!(out, "## Winner");
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "- `{}`: pooled search gain {} points, fresh extension gain {} points, {}.",
            w.id,
            pts(w.pooled_gain),
            pts(w.fresh_gain),
            if w.promoted {
                "promoted"
            } else {
                "not promoted"
            }
        );
        let floor = w
            .best_of_n
            .noise_floor
            .map_or_else(|| "n/a".to_string(), |v| format!("{:.1}", v * 100.0));
        let se = w
            .best_of_n
            .se
            .map_or_else(|| "n/a".to_string(), |v| format!("{:.1}", v * 100.0));
        let _ = writeln!(
            out,
            "- Best-of-N note: with N = {} screened candidates of zero true effect the best \
             observed gain is expected near SE {se} x {:.3} = {floor} points.",
            w.best_of_n.n, w.best_of_n.expected_max_z
        );
    }
    if let Some(v) = &selection.validation {
        let _ = writeln!(out);
        let _ = writeln!(out, "## Validation (held-out, the claim)");
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "- `{}`: gain {} points, one-sided p {:.4}, 90% interval {}: **{}**.",
            v.candidate,
            pts(v.stats.gain),
            v.stats.p_value,
            interval(v.stats.ci90),
            if v.validated {
                "validated"
            } else {
                "not validated"
            }
        );
    }
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "## MDE (80% power, one-sided alpha 0.05; E[p(1-p)] = {:.3} from the {})",
        selection.mde.pq, selection.mde.pq_source
    );
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "| phase | n | k baseline | k candidate | MDE (points) |"
    );
    let _ = writeln!(out, "|---|---|---|---|---|");
    for r in &selection.mde.rows {
        let _ = writeln!(
            out,
            "| {} | {} | {:.1} | {} | {:.1} |",
            r.phase,
            r.n,
            r.baseline_k,
            r.candidate_k,
            r.mde * 100.0
        );
    }
    out
}

// ---------------------------------------------------------------------------------------------
// State file and CLI
// ---------------------------------------------------------------------------------------------

/// One candidate in the state file. Paths are relative to the state file's directory.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateFile {
    #[serde(default)]
    pub extension: Vec<PathBuf>,
    pub id: String,
    #[serde(default)]
    pub screen: Vec<PathBuf>,
    #[serde(default)]
    pub validation: Vec<PathBuf>,
    /// `verify.json` of the candidate is clean (leak and proposer read audits included).
    pub verify_clean: bool,
}

/// The selection state file: the candidate index of #3194 reduced to what the rules read.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateFile {
    pub baseline_records: Vec<PathBuf>,
    /// In the order they were screened.
    pub candidates: Vec<CandidateFile>,
    pub heldout_tasks: Option<usize>,
    pub measured_usd_per_trial: Option<f64>,
    #[serde(default)]
    pub no_further_hypothesis: bool,
    pub search_tasks: Option<usize>,
    #[serde(default)]
    pub spent_usd: f64,
    pub stop_line_usd: Option<f64>,
}

fn load_all(base: &Path, paths: &[PathBuf]) -> Result<Vec<BenchRunRecord>, EvalError> {
    paths
        .iter()
        .map(|p| crate::bench::load_record(&base.join(p)))
        .collect()
}

/// Loads a state file and every record it names.
///
/// # Errors
/// [`EvalError::Io`], [`EvalError::Json`] or [`EvalError::Bench`].
pub fn load_state(path: &Path) -> Result<SelectData, EvalError> {
    let text = std::fs::read_to_string(path)?;
    let state: StateFile = serde_json::from_str(&text)?;
    let base = path.parent().unwrap_or_else(|| Path::new("."));
    let candidates = state
        .candidates
        .iter()
        .map(|c| {
            Ok(CandidateData {
                extension: load_all(base, &c.extension)?,
                id: c.id.clone(),
                screen: load_all(base, &c.screen)?,
                validation: load_all(base, &c.validation)?,
                verify_clean: c.verify_clean,
            })
        })
        .collect::<Result<Vec<_>, EvalError>>()?;
    Ok(SelectData {
        baseline: load_all(base, &state.baseline_records)?,
        candidates,
        params: SelectParams {
            heldout_tasks: state.heldout_tasks.unwrap_or(DEFAULT_HELDOUT_TASKS),
            measured_usd_per_trial: state.measured_usd_per_trial,
            no_further_hypothesis: state.no_further_hypothesis,
            search_tasks: state.search_tasks.unwrap_or(DEFAULT_SEARCH_TASKS),
            spent_usd: state.spent_usd,
            stop_line_usd: state.stop_line_usd.unwrap_or(DEFAULT_STOP_LINE_USD),
        },
    })
}

/// Output format of `bench select`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum SelectFormat {
    Markdown,
    Json,
}

/// Runs `bench select`: prints the report and returns the next action.
///
/// # Errors
/// Any [`load_state`] or [`select`] error, or a failed write of `json_out`.
pub fn run_action(
    state: &Path,
    seed: u64,
    format: SelectFormat,
    json_out: Option<&Path>,
) -> Result<Action, EvalError> {
    let data = load_state(state)?;
    let selection = select(&data, seed)?;
    let json = serde_json::to_string_pretty(&selection)?;
    if let Some(path) = json_out {
        std::fs::write(path, format!("{json}\n"))?;
    }
    match format {
        SelectFormat::Markdown => println!("{}", render_markdown(&selection)),
        SelectFormat::Json => println!("{json}"),
    }
    Ok(selection.action)
}

/// [`run_action`] as a process exit code: 10 propose, 11 extend, 12 validate, 13 stop, 14 defer.
///
/// # Errors
/// See [`run_action`].
pub fn run(
    state: &Path,
    seed: u64,
    format: SelectFormat,
    json_out: Option<&Path>,
) -> Result<ExitCode, EvalError> {
    run_action(state, seed, format, json_out).map(|action| ExitCode::from(action.exit_code()))
}

#[cfg(test)]
mod tests;
