//! `BenchRunRecord` v1: the one versioned record every published benchmark number comes from
//! (#3179, program #3135 decisions D5 and D7).
//!
//! * Each trial row is traceable to its pinned inputs (image digest, trace hash, session), its
//!   cost and its failure cause.
//! * Statistics respect that trials of one task are correlated: the pass rate is the mean of
//!   per-task pass fractions, with a Wilson interval for `k = 1` and a seeded task-clustered
//!   percentile bootstrap for `k > 1`. Invalid trials are excluded and counted separately.
//! * [`observation_fingerprint`] is a sha256 of the canonical JSON of everything that defines
//!   the observation domain; a stored record is reused only for an identical fingerprint.
//! * **Comparable runs across a redeploy (#3192).** The rsid build SHA is provenance only
//!   (`RunInfo::build_sha`, `TrialRow::build_sha`) and is NOT a fingerprint input: the hub daemon
//!   is redeployed often, and a redeploy that leaves the harness code alone must keep an earlier
//!   baseline usable. What the fingerprint and [`check_comparable`] hold fixed instead is
//!   `harness_source_digest`, a sha256 over the git tree or blob ids, at the build commit, of the
//!   code in the committed path list `data/harness_source_paths.txt` (one reason per path; see
//!   [`crate::harness_source_digest`]), plus the effective-input digests ([`EffectiveInputs`]:
//!   prompt, tool spec, policy, loop knobs, overlay), model, effort, route and the dataset, Harbor
//!   and adapter pins. A change under a listed path changes the digest; a change anywhere else does
//!   not. [`compare`] refuses any other difference with [`BenchError::NotComparable`], which names
//!   the first differing field, and also refuses a pair whose `harness_version_hash` differs on a
//!   shared task.
//!
//! JSON keys are alphabetical: struct fields are declared in alphabetical order and
//! `serde_json` maps are sorted.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

/// Current `schema_version` of [`BenchRunRecord`].
pub const SCHEMA_VERSION: u32 = 1;
/// Bootstrap resamples for every task-clustered interval.
pub const BOOTSTRAP_RESAMPLES: usize = 10_000;
/// Default bootstrap seed ("RSI BENCH"); a fixed seed makes every report reproducible.
pub const DEFAULT_BOOTSTRAP_SEED: u64 = 0x5253_4942_454e_4348;
/// Two-sided 95% normal quantile.
const Z_95: f64 = 1.959_963_984_540_054;
/// Per-task differences smaller than this count as unchanged.
const DIFF_EPSILON: f64 = 1e-9;

/// Terminal-Bench source markers a trace must not reach over the network (#3179 item 5).
pub const SOLUTION_LOOKUP_MARKERS: [&str; 4] = [
    "terminal-bench",
    "tbench.ai",
    "harbor-framework",
    "laude-institute",
];

/// Why a record operation was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BenchError {
    #[error("unsupported bench record schema_version {0} (expected {SCHEMA_VERSION})")]
    UnsupportedSchema(u32),
    #[error("pass@k needs k >= 1")]
    ZeroK,
    #[error("run.k must be >= 1 (attempts per task), got 0")]
    ZeroRunK,
    #[error("pass@{k} is undefined for task {task}: only {n} valid trial(s)")]
    InsufficientTrials { task: String, n: usize, k: usize },
    #[error("records cover different task sets: {0}")]
    TaskSetMismatch(String),
    #[error("records pin different datasets: {0}")]
    DatasetMismatch(String),
    #[error("no task has a valid trial in both records")]
    NothingToCompare,
    #[error("observation_fingerprint {stored} does not match the record's inputs ({computed})")]
    FingerprintMismatch { stored: String, computed: String },
    #[error(
        "records are not comparable: {field} differs (baseline {baseline}, candidate {candidate})"
    )]
    NotComparable {
        /// The first differing field, in the order [`check_comparable`] checks them.
        field: String,
        baseline: String,
        candidate: String,
    },
    #[error(
        "trial {task}#{attempt} ran on harness source {trial}, not the run's {run}: a run measures one harness source"
    )]
    MixedHarnessSource {
        task: String,
        attempt: u32,
        trial: String,
        run: String,
    },
    #[error("bench record JSON: {0}")]
    Json(String),
}

#[allow(clippy::trivially_copy_pass_by_ref)] // serde's `skip_serializing_if` takes a reference.
fn is_zero(value: &u32) -> bool {
    *value == 0
}

/// One trial of one task. Fields are alphabetical.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrialRow {
    pub attempt: u32,
    /// The rsid build (`RSI_BUILD_SHA`) the trial ran on; provenance, not a comparison input.
    pub build_sha: String,
    pub cached_tokens: u64,
    pub cost_usd: f64,
    /// Harbor `exception_info.exception_type`.
    pub exception_type: Option<String>,
    /// Harbor `failure_cause`.
    pub failure_cause: Option<String>,
    /// `harness_source_digest` of the daemon build the trial ran on.
    pub harness_source_digest: String,
    /// `Session.harness_version_hash` of the trial's session.
    pub harness_version_hash: String,
    /// Image digest the trial ran.
    pub image_digest: String,
    /// The trial was cut short by infrastructure, not by the agent.
    pub infra_interrupted: bool,
    /// The model route differs from the pinned one.
    pub invalid_route: bool,
    pub output_tokens: u64,
    pub passed: bool,
    pub prompt_tokens: u64,
    /// `read_output` calls the model made (#3173). Absent in a record from before it, and
    /// omitted when zero, so a record without spills is byte-identical to one from before.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub read_output_calls: u32,
    pub reasoning_tokens: u64,
    pub reward: f64,
    /// Harbor `reward_valid`; false when the verifier did not produce a trustworthy reward.
    pub reward_valid: bool,
    pub session_id: Option<String>,
    pub split: String,
    /// Command results the harness replaced with a spill stub (#3173), counted from the trace.
    /// Absent in a record from before it, and omitted when zero.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub spills: u32,
    pub stop_reason: Option<String>,
    /// The trace shows network access to Terminal-Bench sources ([`scan_solution_lookup`]).
    pub suspected_solution_lookup: bool,
    pub task: String,
    pub trace_sha256: Option<String>,
    pub turns: u32,
    pub wall_ms: u64,
}

impl TrialRow {
    /// A trial counts toward pass rates only when its reward is valid, its route is the pinned
    /// one and infrastructure did not interrupt it.
    #[must_use]
    pub const fn is_valid(&self) -> bool {
        self.reward_valid && !self.invalid_route && !self.infra_interrupted
    }

    /// Hashes `trace` into `trace_sha256` and sets `suspected_solution_lookup` from
    /// [`scan_solution_lookup`]. Returns the scan so the caller can keep the evidence lines.
    pub fn apply_trace_scan(
        &mut self,
        trace: &str,
        upstream_repo: Option<&str>,
    ) -> SolutionLookupScan {
        self.trace_sha256 = Some(format!("sha256:{:x}", Sha256::digest(trace.as_bytes())));
        let scan = scan_solution_lookup(trace, upstream_repo);
        self.suspected_solution_lookup = scan.suspected;
        let counts = scan_output_spills(trace);
        self.spills = counts.spills;
        self.read_output_calls = counts.read_output_calls;
        scan
    }
}

/// What a trace shows of the output spill (#3173): how often a tool result was a spill stub and
/// how often the model asked for one back.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OutputSpillCounts {
    pub spills: u32,
    pub read_output_calls: u32,
}

/// Counts spill stubs (a `tool_result` of any tool but `read_output` whose output starts with
/// [`crate::spill::STUB_PREFIX`]) and `read_output` calls in a bench trace (`trace.jsonl`, one JSON
/// line per event). A line that is not an event is ignored.
#[must_use]
pub fn scan_output_spills(trace: &str) -> OutputSpillCounts {
    #[derive(Deserialize)]
    struct Line {
        #[serde(default)]
        kind: String,
        #[serde(default)]
        name: String,
        #[serde(default)]
        result: Option<LineResult>,
    }
    #[derive(Deserialize)]
    struct LineResult {
        #[serde(default)]
        output: String,
    }
    let mut counts = OutputSpillCounts::default();
    for line in trace.lines() {
        // Most of a trace is model requests that repeat the whole transcript; only a tool
        // event can matter, so skip the rest without parsing it.
        if !line.contains("\"kind\":\"tool_") {
            continue;
        }
        let Ok(event) = serde_json::from_str::<Line>(line) else {
            continue;
        };
        match event.kind.as_str() {
            "tool_call" if event.name == "read_output" => {
                counts.read_output_calls = counts.read_output_calls.saturating_add(1);
            }
            "tool_result"
                if event.name != "read_output"
                    && event
                        .result
                        .is_some_and(|r| r.output.starts_with(crate::spill::STUB_PREFIX)) =>
            {
                counts.spills = counts.spills.saturating_add(1);
            }
            _ => {}
        }
    }
    counts
}

/// Timeouts that bound a trial. Fields are alphabetical.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Timeouts {
    pub agent_sec: u64,
    pub multiplier: f64,
    pub verifier_sec: u64,
}

/// Run totals over every trial (valid or not). Fields are alphabetical.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Totals {
    pub cached_tokens: u64,
    pub cost_usd: f64,
    pub invalid_trials: u64,
    pub output_tokens: u64,
    pub passed_trials: u64,
    pub prompt_tokens: u64,
    pub reasoning_tokens: u64,
    pub trials: u64,
    pub valid_trials: u64,
    pub wall_ms: u64,
}

impl Totals {
    #[must_use]
    pub fn from_trials(trials: &[TrialRow]) -> Self {
        let mut totals = Self::default();
        for t in trials {
            totals.trials += 1;
            totals.cost_usd += t.cost_usd;
            totals.prompt_tokens += t.prompt_tokens;
            totals.cached_tokens += t.cached_tokens;
            totals.output_tokens += t.output_tokens;
            totals.reasoning_tokens += t.reasoning_tokens;
            totals.wall_ms += t.wall_ms;
            if t.is_valid() {
                totals.valid_trials += 1;
                if t.passed {
                    totals.passed_trials += 1;
                }
            } else {
                totals.invalid_trials += 1;
            }
        }
        totals
    }
}

/// Digests of the effective inputs a run's harness interpreted. Fields are alphabetical.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectiveInputs {
    /// Per-launch loop knobs and the daemon globals they replace.
    pub loop_knobs: String,
    /// The bench prompt overlay.
    pub overlay: String,
    /// The effective tool policy.
    pub policy: String,
    /// The base system prompt.
    pub prompt: String,
    /// The advertised tool catalog.
    pub tool_spec: String,
}

/// The run part of the record. Fields are alphabetical.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunInfo {
    pub adapter_tree_hash: String,
    /// The rsid build (`RSI_BUILD_SHA`) the run started on; provenance. Each trial records its own.
    pub build_sha: String,
    pub concurrency: u32,
    pub config_digest: String,
    pub dataset_commit: String,
    pub dataset_repo: String,
    pub effective_inputs: EffectiveInputs,
    pub effort: String,
    pub finished_at: String,
    pub harbor_version: String,
    /// Identifies the harness configuration across tasks (unlike `harness_version_hash`, which
    /// folds in the task query). It does not contain the rsid build SHA (#3192).
    pub harness_build_digest: String,
    /// Digest of the harness source at the build commit ([`crate::harness_source_digest`]).
    pub harness_source_digest: String,
    pub host: String,
    /// Image digest per task.
    pub image_digests: BTreeMap<String, String>,
    /// Trials per task.
    pub k: u32,
    pub model: String,
    pub observation_fingerprint: String,
    pub route: String,
    pub sampling: BTreeMap<String, serde_json::Value>,
    /// Split label, e.g. `held_out`.
    pub split: String,
    pub started_at: String,
    pub tasks: Vec<String>,
    pub timeouts: Timeouts,
    pub totals: Totals,
}

/// `BenchRunRecord` v1. Fields are alphabetical.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BenchRunRecord {
    pub run: RunInfo,
    pub schema_version: u32,
    pub trials: Vec<TrialRow>,
}

/// Everything that defines the observation domain. Fields are alphabetical.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FingerprintInputs {
    pub adapter_tree_hash: String,
    pub dataset_commit: String,
    pub dataset_repo: String,
    pub effective_inputs: EffectiveInputs,
    pub effort: String,
    pub harbor_version: String,
    pub harness_build_digest: String,
    pub harness_source_digest: String,
    pub image_digests: BTreeMap<String, String>,
    pub k: u32,
    pub model: String,
    pub route: String,
    pub sampling: BTreeMap<String, serde_json::Value>,
    pub split: String,
    /// Sorted and deduplicated.
    pub tasks: Vec<String>,
    pub timeouts: Timeouts,
}

/// `sha256:<hex>` of the canonical (JCS) JSON of `inputs`.
#[must_use]
pub fn observation_fingerprint(inputs: &FingerprintInputs) -> String {
    let mut inputs = inputs.clone();
    inputs.tasks.sort();
    inputs.tasks.dedup();
    // Serialising plain strings, numbers and maps cannot fail; an empty digest would never match.
    let bytes = serde_jcs::to_vec(&inputs).unwrap_or_default();
    format!("sha256:{:x}", Sha256::digest(&bytes))
}

impl BenchRunRecord {
    #[must_use]
    pub fn fingerprint_inputs(&self) -> FingerprintInputs {
        let run = &self.run;
        FingerprintInputs {
            adapter_tree_hash: run.adapter_tree_hash.clone(),
            dataset_commit: run.dataset_commit.clone(),
            dataset_repo: run.dataset_repo.clone(),
            effective_inputs: run.effective_inputs.clone(),
            effort: run.effort.clone(),
            harbor_version: run.harbor_version.clone(),
            harness_build_digest: run.harness_build_digest.clone(),
            harness_source_digest: run.harness_source_digest.clone(),
            image_digests: run.image_digests.clone(),
            k: run.k,
            model: run.model.clone(),
            route: run.route.clone(),
            sampling: run.sampling.clone(),
            split: run.split.clone(),
            tasks: run.tasks.clone(),
            timeouts: run.timeouts.clone(),
        }
    }

    /// Fingerprint recomputed from the record's inputs.
    #[must_use]
    pub fn computed_fingerprint(&self) -> String {
        observation_fingerprint(&self.fingerprint_inputs())
    }

    /// Sets `observation_fingerprint` and `totals` from the other fields.
    pub fn seal(&mut self) {
        self.run.totals = Totals::from_trials(&self.trials);
        self.run.observation_fingerprint = self.computed_fingerprint();
    }

    /// Checks that the record is well-formed: known schema version, `run.k >= 1`, every trial on
    /// the run's harness source and a fingerprint that matches the record's own inputs.
    ///
    /// # Errors
    /// [`BenchError::UnsupportedSchema`], [`BenchError::ZeroRunK`],
    /// [`BenchError::MixedHarnessSource`] or [`BenchError::FingerprintMismatch`].
    pub fn validate(&self) -> Result<(), BenchError> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(BenchError::UnsupportedSchema(self.schema_version));
        }
        if self.run.k == 0 {
            return Err(BenchError::ZeroRunK);
        }
        if let Some(trial) = self
            .trials
            .iter()
            .find(|t| t.harness_source_digest != self.run.harness_source_digest)
        {
            return Err(BenchError::MixedHarnessSource {
                task: trial.task.clone(),
                attempt: trial.attempt,
                trial: trial.harness_source_digest.clone(),
                run: self.run.harness_source_digest.clone(),
            });
        }
        let computed = self.computed_fingerprint();
        if computed != self.run.observation_fingerprint {
            return Err(BenchError::FingerprintMismatch {
                stored: self.run.observation_fingerprint.clone(),
                computed,
            });
        }
        Ok(())
    }

    /// Parses and validates a record ([`Self::validate`]).
    ///
    /// # Errors
    /// [`BenchError::Json`] or any [`Self::validate`] error.
    pub fn from_json(text: &str) -> Result<Self, BenchError> {
        let record: Self =
            serde_json::from_str(text).map_err(|error| BenchError::Json(error.to_string()))?;
        record.validate()?;
        Ok(record)
    }

    /// Pretty JSON with a trailing newline.
    ///
    /// # Errors
    /// [`BenchError::Json`] when serialisation fails.
    pub fn to_json(&self) -> Result<String, BenchError> {
        let mut text = serde_json::to_string_pretty(self)
            .map_err(|error| BenchError::Json(error.to_string()))?;
        text.push('\n');
        Ok(text)
    }

    /// Returns `Some(self)` only for an identical observation fingerprint.
    #[must_use]
    pub fn reusable_for(&self, fingerprint: &str) -> Option<&Self> {
        (self.run.observation_fingerprint == fingerprint
            && self.computed_fingerprint() == fingerprint)
            .then_some(self)
    }
}

/// The record in `records` whose observation fingerprint is identical to `fingerprint`.
#[must_use]
pub fn find_reusable<'a>(
    records: &'a [BenchRunRecord],
    fingerprint: &str,
) -> Option<&'a BenchRunRecord> {
    records.iter().find_map(|r| r.reusable_for(fingerprint))
}

// ---------------------------------------------------------------------------------------------
// Solution-lookup scan
// ---------------------------------------------------------------------------------------------

/// One trace line that shows network access to a Terminal-Bench source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanHit {
    pub line: usize,
    pub marker: String,
    /// The line, truncated to 200 characters.
    pub text: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SolutionLookupScan {
    pub suspected: bool,
    /// At most [`MAX_SCAN_HITS`] hits.
    pub hits: Vec<ScanHit>,
}

const MAX_SCAN_HITS: usize = 5;
const NETWORK_INDICATORS: [&str; 12] = [
    "://",
    "curl ",
    "wget ",
    "git clone",
    "git fetch",
    "git pull",
    "git@",
    "gh repo",
    "gh api",
    "pip install",
    "pip download",
    "npm install",
];

fn normalize_repo(repo: &str) -> String {
    let lower = repo.trim().to_ascii_lowercase();
    let lower = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"))
        .or_else(|| lower.strip_prefix("git@"))
        .unwrap_or(&lower);
    lower
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .replace(':', "/")
}

/// Flags a trace that shows network access to a Terminal-Bench source: a line that both names
/// one of [`SOLUTION_LOOKUP_MARKERS`] (or the task's upstream repository) and carries a network
/// indicator (a URL, `curl`, `wget`, `git clone`, ...). Merely mentioning a marker, for example
/// in a file path, is not a hit.
#[must_use]
pub fn scan_solution_lookup(trace: &str, upstream_repo: Option<&str>) -> SolutionLookupScan {
    let upstream = upstream_repo.map(normalize_repo).filter(|r| !r.is_empty());
    let mut scan = SolutionLookupScan::default();
    for (index, line) in trace.lines().enumerate() {
        let lower = line.to_ascii_lowercase();
        if !NETWORK_INDICATORS.iter().any(|n| lower.contains(n)) {
            continue;
        }
        let normalized = lower.replace(':', "/");
        let marker = SOLUTION_LOOKUP_MARKERS
            .iter()
            .find(|m| lower.contains(*m))
            .map(|m| (*m).to_string())
            .or_else(|| {
                upstream
                    .as_ref()
                    .filter(|u| normalized.contains(u.as_str()))
                    .cloned()
            });
        let Some(marker) = marker else { continue };
        scan.suspected = true;
        if scan.hits.len() < MAX_SCAN_HITS {
            scan.hits.push(ScanHit {
                line: index + 1,
                marker,
                text: line.chars().take(200).collect(),
            });
        }
    }
    scan
}

// ---------------------------------------------------------------------------------------------
// Statistics
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Interval {
    pub lo: f64,
    pub hi: f64,
}

/// 95% Wilson score interval for `passed` of `n` trials. `None` when `n == 0`.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn wilson_interval(passed: u64, n: u64) -> Option<Interval> {
    if n == 0 {
        return None;
    }
    let n = n as f64;
    let p = passed as f64 / n;
    let z2 = Z_95 * Z_95;
    let denom = 1.0 + z2 / n;
    let center = (p + z2 / (2.0 * n)) / denom;
    let half = Z_95 * (p * (1.0 - p) / n + z2 / (4.0 * n * n)).sqrt() / denom;
    Some(Interval {
        lo: (center - half).max(0.0),
        hi: (center + half).min(1.0),
    })
}

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

    /// Uniform in `0..bound` (multiply-shift; `bound > 0`).
    #[allow(clippy::cast_possible_truncation, clippy::cast_lossless)]
    const fn below(&mut self, bound: usize) -> usize {
        ((self.next_u64() as u128 * bound as u128) >> 64) as usize
    }
}

/// Linear-interpolated quantile of an ascending slice: position `q * (len - 1)`.
#[must_use]
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
pub fn percentile(sorted: &[f64], q: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let pos = q.clamp(0.0, 1.0) * (sorted.len() - 1) as f64;
    let lo = pos.floor() as usize;
    let hi = pos.ceil() as usize;
    let frac = pos - lo as f64;
    Some(sorted[lo].mul_add(1.0 - frac, sorted[hi] * frac))
}

#[allow(clippy::cast_precision_loss)]
fn mean(values: &[f64]) -> f64 {
    values.iter().sum::<f64>() / values.len() as f64
}

/// Seeded percentile bootstrap of the mean over `values` (one value per task, so whole tasks
/// are resampled): `resamples` draws with replacement, 95% interval at the 2.5th and 97.5th
/// percentiles. `None` for empty input.
#[must_use]
pub fn bootstrap_mean_interval(values: &[f64], resamples: usize, seed: u64) -> Option<Interval> {
    if values.is_empty() || resamples == 0 {
        return None;
    }
    let mut rng = SplitMix64(seed);
    let m = values.len();
    let mut means: Vec<f64> = (0..resamples)
        .map(|_| {
            let sum: f64 = (0..m).map(|_| values[rng.below(m)]).sum();
            #[allow(clippy::cast_precision_loss)]
            let m = m as f64;
            sum / m
        })
        .collect();
    means.sort_by(f64::total_cmp);
    Some(Interval {
        lo: percentile(&means, 0.025)?,
        hi: percentile(&means, 0.975)?,
    })
}

/// Unbiased pass@k estimator for one task: `1 - C(n-c, k) / C(n, k)`.
///
/// # Errors
/// [`BenchError::ZeroK`] for `k == 0`; [`BenchError::InsufficientTrials`] (with an empty task
/// name) when `n < k`.
#[allow(clippy::cast_precision_loss)]
pub fn pass_at_k(n: usize, c: usize, k: usize) -> Result<f64, BenchError> {
    if k == 0 {
        return Err(BenchError::ZeroK);
    }
    if n < k {
        return Err(BenchError::InsufficientTrials {
            task: String::new(),
            n,
            k,
        });
    }
    let c = c.min(n);
    if n - c < k {
        return Ok(1.0);
    }
    // C(n-c, k) / C(n, k) = prod_{i<k} (n-c-i) / (n-i), without overflowing binomials.
    let ratio: f64 = (0..k)
        .map(|i| (n - c - i) as f64 / (n - i) as f64)
        .product();
    Ok(1.0 - ratio)
}

/// Valid-trial counts per task (tasks with no valid trial are absent).
fn per_task_counts(trials: &[TrialRow]) -> BTreeMap<&str, (usize, usize)> {
    let mut map: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    for t in trials.iter().filter(|t| t.is_valid()) {
        let entry = map.entry(t.task.as_str()).or_default();
        entry.0 += 1;
        if t.passed {
            entry.1 += 1;
        }
    }
    map
}

/// Mean pass@k over tasks, from valid trials only.
///
/// # Errors
/// [`BenchError::ZeroK`], or [`BenchError::InsufficientTrials`] naming the first task with
/// fewer than `k` valid trials.
#[allow(clippy::cast_precision_loss)]
pub fn mean_pass_at_k(trials: &[TrialRow], k: usize) -> Result<Option<f64>, BenchError> {
    let counts = per_task_counts(trials);
    let mut values = Vec::with_capacity(counts.len());
    for (task, (n, c)) in &counts {
        values.push(pass_at_k(*n, *c, k).map_err(|error| match error {
            BenchError::InsufficientTrials { n, k, .. } => BenchError::InsufficientTrials {
                task: (*task).to_string(),
                n,
                k,
            },
            other => other,
        })?);
    }
    Ok((!values.is_empty()).then(|| mean(&values)))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntervalMethod {
    Wilson,
    ClusteredBootstrap,
}

/// Aggregate statistics of one record. Cost metrics span every trial, valid or not: invalid
/// trials still spent the money.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Summary {
    pub cached_tokens_per_output_token: Option<f64>,
    pub ci: Option<Interval>,
    pub ci_method: Option<IntervalMethod>,
    pub cost_per_task_usd: Option<f64>,
    pub cost_per_trial_usd: Option<f64>,
    pub failure_causes: BTreeMap<String, u64>,
    pub infra_interrupted_trials: u64,
    pub invalid_route_trials: u64,
    pub invalid_trials: u64,
    pub k: u32,
    pub passed_trials: u64,
    pub pass_at_k: Option<f64>,
    /// Why `pass_at_k` is absent (for example `n < k`).
    pub pass_at_k_refusal: Option<String>,
    pub pass_rate: Option<f64>,
    pub reward_invalid_trials: u64,
    pub suspected_solution_lookup_trials: u64,
    pub tasks_scored: u64,
    pub tasks_unscored: u64,
    pub usd_per_solved_trial: Option<f64>,
    pub valid_trials: u64,
}

fn ratio(numerator: f64, denominator: f64) -> Option<f64> {
    (denominator > 0.0).then(|| numerator / denominator)
}

/// Computes [`Summary`] for `record` with the given bootstrap `seed`.
///
/// # Errors
/// Any [`BenchRunRecord::validate`] error (for example [`BenchError::ZeroRunK`]): a published
/// number must come from a well-formed record, not only one that passed through `from_json`.
#[allow(clippy::cast_precision_loss)]
pub fn summarize(record: &BenchRunRecord, seed: u64) -> Result<Summary, BenchError> {
    record.validate()?;
    let trials = &record.trials;
    let counts = per_task_counts(trials);
    let all_tasks: BTreeSet<&str> = trials.iter().map(|t| t.task.as_str()).collect();
    let fractions: Vec<f64> = counts
        .values()
        .map(|(n, c)| *c as f64 / *n as f64)
        .collect();
    let valid_trials: u64 = counts.values().map(|(n, _)| *n as u64).sum();
    let passed_trials: u64 = counts.values().map(|(_, c)| *c as u64).sum();
    let max_n = counts.values().map(|(n, _)| *n).max().unwrap_or(0);

    let (ci, ci_method) = if fractions.is_empty() {
        (None, None)
    } else if record.run.k <= 1 && max_n <= 1 {
        (
            wilson_interval(passed_trials, valid_trials),
            Some(IntervalMethod::Wilson),
        )
    } else {
        (
            bootstrap_mean_interval(&fractions, BOOTSTRAP_RESAMPLES, seed),
            Some(IntervalMethod::ClusteredBootstrap),
        )
    };

    let (pass_at_k, pass_at_k_refusal) = if record.run.k <= 1 {
        (None, None)
    } else {
        match mean_pass_at_k(trials, record.run.k as usize) {
            Ok(value) => (value, None),
            Err(error) => (None, Some(error.to_string())),
        }
    };

    let mut failure_causes: BTreeMap<String, u64> = BTreeMap::new();
    for t in trials {
        let cause = t.failure_cause.as_deref().filter(|c| !c.is_empty());
        let cause = cause.or_else(|| t.exception_type.as_deref().filter(|c| !c.is_empty()));
        if let Some(cause) = cause {
            *failure_causes.entry(cause.to_string()).or_default() += 1;
        }
    }

    let totals = Totals::from_trials(trials);
    let count = |pred: fn(&TrialRow) -> bool| trials.iter().filter(|t| pred(t)).count() as u64;
    Ok(Summary {
        cached_tokens_per_output_token: ratio(
            totals.cached_tokens as f64,
            totals.output_tokens as f64,
        ),
        ci,
        ci_method,
        cost_per_task_usd: ratio(totals.cost_usd, all_tasks.len() as f64),
        cost_per_trial_usd: ratio(totals.cost_usd, totals.trials as f64),
        failure_causes,
        infra_interrupted_trials: count(|t| t.infra_interrupted),
        invalid_route_trials: count(|t| t.invalid_route),
        invalid_trials: totals.invalid_trials,
        k: record.run.k,
        passed_trials,
        pass_at_k,
        pass_at_k_refusal,
        pass_rate: (!fractions.is_empty()).then(|| mean(&fractions)),
        reward_invalid_trials: count(|t| !t.reward_valid),
        suspected_solution_lookup_trials: count(|t| t.suspected_solution_lookup),
        tasks_scored: counts.len() as u64,
        tasks_unscored: (all_tasks.len() - counts.len()) as u64,
        usd_per_solved_trial: ratio(totals.cost_usd, passed_trials as f64),
        valid_trials,
    })
}

// ---------------------------------------------------------------------------------------------
// Paired compare
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TaskDiff {
    pub a: f64,
    pub b: f64,
    pub diff: f64,
    pub task: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// The 95% interval of the mean difference lies above zero.
    Improvement,
    /// The 95% interval of the mean difference lies below zero.
    Regression,
    /// The interval contains zero.
    Inconclusive,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Comparison {
    pub a_pass_rate: f64,
    pub b_pass_rate: f64,
    pub ci: Option<Interval>,
    pub improved: usize,
    pub mean_diff: f64,
    pub per_task: Vec<TaskDiff>,
    pub regressed: usize,
    /// Tasks left out because one side has no valid trial.
    pub unscored_tasks: Vec<String>,
    pub unchanged: usize,
    pub verdict: Verdict,
}

fn task_set(record: &BenchRunRecord) -> BTreeSet<&str> {
    record
        .run
        .tasks
        .iter()
        .map(String::as_str)
        .chain(record.trials.iter().map(|t| t.task.as_str()))
        .collect()
}

/// `harness_version_hash` values per task, for the tasks of `record`.
fn version_hashes(record: &BenchRunRecord) -> BTreeMap<&str, BTreeSet<&str>> {
    let mut map: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for t in &record.trials {
        map.entry(t.task.as_str())
            .or_default()
            .insert(t.harness_version_hash.as_str());
    }
    map
}

/// Accepts `baseline` and `candidate` as one paired observation only when they differ at most in
/// the rsid build SHA (#3192): the dataset pin (`dataset_repo`, `dataset_commit`), `harbor_version`,
/// `adapter_tree_hash`, `model`, `effort`, `route`, `harness_source_digest` (a digest that is
/// [`crate::harness_source_digest::is_unavailable`] is never comparable), the
/// [`EffectiveInputs`] digests and, task by task, `harness_version_hash` must all match. Anything
/// else (`k`, timeouts, sampling, host, split, build SHA) is not examined here.
///
/// # Errors
/// [`BenchError::NotComparable`] naming the first differing field in the order above.
pub fn check_comparable(
    baseline: &BenchRunRecord,
    candidate: &BenchRunRecord,
) -> Result<(), BenchError> {
    let (a, b) = (&baseline.run, &candidate.run);
    let refuse = |field: &str, x: &str, y: &str| BenchError::NotComparable {
        field: field.to_string(),
        baseline: x.to_string(),
        candidate: y.to_string(),
    };
    let pairs: [(&str, &str, &str); 13] = [
        ("dataset_repo", &a.dataset_repo, &b.dataset_repo),
        ("dataset_commit", &a.dataset_commit, &b.dataset_commit),
        ("harbor_version", &a.harbor_version, &b.harbor_version),
        (
            "adapter_tree_hash",
            &a.adapter_tree_hash,
            &b.adapter_tree_hash,
        ),
        ("model", &a.model, &b.model),
        ("effort", &a.effort, &b.effort),
        ("route", &a.route, &b.route),
        (
            "harness_source_digest",
            &a.harness_source_digest,
            &b.harness_source_digest,
        ),
        (
            "effective_inputs.prompt",
            &a.effective_inputs.prompt,
            &b.effective_inputs.prompt,
        ),
        (
            "effective_inputs.tool_spec",
            &a.effective_inputs.tool_spec,
            &b.effective_inputs.tool_spec,
        ),
        (
            "effective_inputs.policy",
            &a.effective_inputs.policy,
            &b.effective_inputs.policy,
        ),
        (
            "effective_inputs.loop_knobs",
            &a.effective_inputs.loop_knobs,
            &b.effective_inputs.loop_knobs,
        ),
        (
            "effective_inputs.overlay",
            &a.effective_inputs.overlay,
            &b.effective_inputs.overlay,
        ),
    ];
    for (field, x, y) in pairs {
        if x != y {
            return Err(refuse(field, x, y));
        }
        if field == "harness_source_digest" && crate::harness_source_digest::is_unavailable(x) {
            return Err(refuse(field, x, y));
        }
    }
    let (hashes_a, hashes_b) = (version_hashes(baseline), version_hashes(candidate));
    for (task, x) in &hashes_a {
        if let Some(y) = hashes_b.get(task) {
            if x != y {
                let join = |set: &BTreeSet<&str>| set.iter().copied().collect::<Vec<_>>().join(",");
                return Err(refuse(
                    &format!("harness_version_hash[{task}]"),
                    &join(x),
                    &join(y),
                ));
            }
        }
    }
    Ok(())
}

/// Paired compare of `a` (baseline) against `b` (candidate) on the same task set: per-task
/// pass-fraction differences `b - a`, a seeded 95% bootstrap interval of their mean over tasks
/// and the counts of improved and regressed tasks.
///
/// # Errors
/// Any [`BenchRunRecord::validate`] error (for example [`BenchError::ZeroRunK`]) for either
/// record; [`BenchError::NotComparable`] ([`check_comparable`]); [`BenchError::DatasetMismatch`] or [`BenchError::TaskSetMismatch`] when the records do not
/// share a dataset pin and task set; [`BenchError::NothingToCompare`] when no task has a valid
/// trial on both sides.
#[allow(clippy::cast_precision_loss)]
pub fn compare(
    a: &BenchRunRecord,
    b: &BenchRunRecord,
    seed: u64,
) -> Result<Comparison, BenchError> {
    a.validate()?;
    b.validate()?;
    if (&a.run.dataset_repo, &a.run.dataset_commit) != (&b.run.dataset_repo, &b.run.dataset_commit)
    {
        return Err(BenchError::DatasetMismatch(format!(
            "{}@{} vs {}@{}",
            a.run.dataset_repo, a.run.dataset_commit, b.run.dataset_repo, b.run.dataset_commit
        )));
    }
    check_comparable(a, b)?;
    let (tasks_a, tasks_b) = (task_set(a), task_set(b));
    if tasks_a != tasks_b {
        let only_a: Vec<&str> = tasks_a.difference(&tasks_b).copied().collect();
        let only_b: Vec<&str> = tasks_b.difference(&tasks_a).copied().collect();
        return Err(BenchError::TaskSetMismatch(format!(
            "only in a: [{}]; only in b: [{}]",
            only_a.join(", "),
            only_b.join(", ")
        )));
    }
    let (counts_a, counts_b) = (per_task_counts(&a.trials), per_task_counts(&b.trials));
    let mut per_task = Vec::new();
    let mut unscored_tasks = Vec::new();
    for task in &tasks_a {
        match (counts_a.get(task), counts_b.get(task)) {
            (Some((na, ca)), Some((nb, cb))) => {
                let (fa, fb) = (*ca as f64 / *na as f64, *cb as f64 / *nb as f64);
                per_task.push(TaskDiff {
                    a: fa,
                    b: fb,
                    diff: fb - fa,
                    task: (*task).to_string(),
                });
            }
            _ => unscored_tasks.push((*task).to_string()),
        }
    }
    if per_task.is_empty() {
        return Err(BenchError::NothingToCompare);
    }
    let diffs: Vec<f64> = per_task.iter().map(|t| t.diff).collect();
    let ci = bootstrap_mean_interval(&diffs, BOOTSTRAP_RESAMPLES, seed);
    let improved = diffs.iter().filter(|d| **d > DIFF_EPSILON).count();
    let regressed = diffs.iter().filter(|d| **d < -DIFF_EPSILON).count();
    let verdict = match ci {
        Some(i) if i.lo > 0.0 => Verdict::Improvement,
        Some(i) if i.hi < 0.0 => Verdict::Regression,
        _ => Verdict::Inconclusive,
    };
    let a_values: Vec<f64> = per_task.iter().map(|t| t.a).collect();
    let b_values: Vec<f64> = per_task.iter().map(|t| t.b).collect();
    Ok(Comparison {
        a_pass_rate: mean(&a_values),
        b_pass_rate: mean(&b_values),
        ci,
        improved,
        mean_diff: mean(&diffs),
        regressed,
        unchanged: per_task.len() - improved - regressed,
        per_task,
        unscored_tasks,
        verdict,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::float_cmp)]
mod tests {
    use super::*;

    const GOLDEN: &str = include_str!("../fixtures/bench_run_record_v1.json");

    fn source_digest() -> String {
        format!("sha256:{}", "f".repeat(64))
    }

    fn trial(task: &str, attempt: u32, passed: bool) -> TrialRow {
        TrialRow {
            attempt,
            build_sha: "1".repeat(40),
            cached_tokens: 600,
            cost_usd: 0.25,
            exception_type: None,
            failure_cause: (!passed).then(|| "verifier_failed".to_string()),
            harness_source_digest: source_digest(),
            harness_version_hash: format!("hvh-{task}"),
            image_digest: format!("sha256:{}", "a".repeat(64)),
            infra_interrupted: false,
            invalid_route: false,
            output_tokens: 300,
            passed,
            prompt_tokens: 1000,
            reasoning_tokens: 50,
            reward: if passed { 1.0 } else { 0.0 },
            reward_valid: true,
            session_id: Some(format!("session-{task}-{attempt}")),
            split: "held_out".to_string(),
            stop_reason: Some("end_turn".to_string()),
            read_output_calls: 0,
            spills: 0,
            suspected_solution_lookup: false,
            task: task.to_string(),
            trace_sha256: Some(format!("sha256:{}", "b".repeat(64))),
            turns: 7,
            wall_ms: 12_000,
        }
    }

    fn record(trials: Vec<TrialRow>, k: u32) -> BenchRunRecord {
        let mut tasks: Vec<String> = trials.iter().map(|t| t.task.clone()).collect();
        tasks.sort();
        tasks.dedup();
        let image_digests = tasks
            .iter()
            .map(|t| (t.clone(), format!("sha256:{}", "a".repeat(64))))
            .collect();
        let mut sampling = BTreeMap::new();
        sampling.insert("temperature".to_string(), serde_json::json!(0.0));
        let mut record = BenchRunRecord {
            run: RunInfo {
                adapter_tree_hash: "c".repeat(40),
                build_sha: "1".repeat(40),
                concurrency: 4,
                config_digest: format!("sha256:{}", "d".repeat(64)),
                dataset_commit: "2fd12b88aafdd04a52c298e3940bcb189f9766d6".to_string(),
                dataset_repo: "harbor-framework/terminal-bench-2".to_string(),
                effective_inputs: EffectiveInputs {
                    loop_knobs: format!("sha256:{}", "1".repeat(64)),
                    overlay: format!("sha256:{}", "2".repeat(64)),
                    policy: format!("sha256:{}", "3".repeat(64)),
                    prompt: format!("sha256:{}", "4".repeat(64)),
                    tool_spec: format!("sha256:{}", "5".repeat(64)),
                },
                effort: "high".to_string(),
                finished_at: "2026-10-19T13:00:00.000000000Z".to_string(),
                harbor_version: "0.24.0".to_string(),
                harness_build_digest: format!("sha256:{}", "e".repeat(64)),
                harness_source_digest: source_digest(),
                host: "bench-host-1".to_string(),
                image_digests,
                k,
                model: "claude-sonnet-5-5".to_string(),
                observation_fingerprint: String::new(),
                route: "direct".to_string(),
                sampling,
                split: "held_out".to_string(),
                started_at: "2026-10-19T12:00:00.000000000Z".to_string(),
                tasks,
                timeouts: Timeouts {
                    agent_sec: 900,
                    multiplier: 1.0,
                    verifier_sec: 600,
                },
                totals: Totals::default(),
            },
            schema_version: SCHEMA_VERSION,
            trials,
        };
        record.seal();
        record
    }

    fn golden_record() -> BenchRunRecord {
        let mut failed = trial("fix-git", 0, false);
        failed.exception_type = Some("AgentTimeoutError".to_string());
        failed.stop_reason = None;
        let mut rec = record(vec![trial("build-pov-ray", 0, true), failed], 1);
        rec.run.dataset_commit = "2fd12b88aafdd04a52c298e3940bcb189f9766d6".to_string();
        rec
    }

    #[test]
    fn golden_json_round_trip() {
        let golden_value: serde_json::Value = serde_json::from_str(GOLDEN).unwrap();
        let rec = golden_record();
        assert_eq!(serde_json::to_value(&rec).unwrap(), golden_value);
        // Declaration order is alphabetical: compact output equals the sorted-map rendering.
        assert_eq!(
            serde_json::to_string(&rec).unwrap(),
            serde_json::to_string(&golden_value).unwrap()
        );
        let parsed = BenchRunRecord::from_json(GOLDEN).unwrap();
        assert_eq!(parsed, rec);
        assert_eq!(
            BenchRunRecord::from_json(&rec.to_json().unwrap()).unwrap(),
            rec
        );
        assert_eq!(parsed.schema_version, 1);
    }

    #[test]
    fn wrong_schema_version_and_tampered_fingerprint_are_refused() {
        let mut rec = golden_record();
        rec.schema_version = 2;
        assert_eq!(
            BenchRunRecord::from_json(&rec.to_json().unwrap()),
            Err(BenchError::UnsupportedSchema(2))
        );
        let mut rec = golden_record();
        rec.run.model = "other".to_string();
        assert!(matches!(
            BenchRunRecord::from_json(&rec.to_json().unwrap()),
            Err(BenchError::FingerprintMismatch { .. })
        ));
    }

    #[test]
    fn in_memory_summarize_and_compare_refuse_a_zero_k_record() {
        let good = record(vec![trial("t1", 0, true)], 1);
        let mut zero = good.clone();
        zero.run.k = 0;
        assert_eq!(summarize(&zero, 1).unwrap_err(), BenchError::ZeroRunK);
        zero.seal();
        assert_eq!(summarize(&zero, 1).unwrap_err(), BenchError::ZeroRunK);
        assert_eq!(compare(&zero, &good, 5).unwrap_err(), BenchError::ZeroRunK);
        assert_eq!(compare(&good, &zero, 5).unwrap_err(), BenchError::ZeroRunK);
        assert!(summarize(&good, 1).is_ok());
    }

    #[test]
    fn zero_run_k_is_refused_even_with_a_valid_fingerprint() {
        let mut rec = golden_record();
        rec.run.k = 0;
        rec.seal();
        assert_eq!(rec.run.observation_fingerprint, rec.computed_fingerprint());
        assert_eq!(rec.validate(), Err(BenchError::ZeroRunK));
        let error = BenchRunRecord::from_json(&rec.to_json().unwrap()).unwrap_err();
        assert_eq!(error, BenchError::ZeroRunK);
        assert!(error.to_string().contains("run.k"), "{error}");
        // k = 1 and k > 1 stay valid.
        for k in [1, 3] {
            let mut rec = golden_record();
            rec.run.k = k;
            rec.seal();
            assert_eq!(rec.validate(), Ok(()));
        }
    }

    #[test]
    fn wilson_matches_hand_computed_fixtures() {
        // 5/10: centre 0.5, half-width 1.959964 * sqrt(0.0346037) / 1.3841459 = 0.263408.
        let i = wilson_interval(5, 10).unwrap();
        assert!((i.lo - 0.2366).abs() < 1e-3, "{i:?}");
        assert!((i.hi - 0.7634).abs() < 1e-3, "{i:?}");
        // 0/10: lower 0, upper z^2 / (n + z^2) = 3.841459 / 13.841459 = 0.27753.
        let i = wilson_interval(0, 10).unwrap();
        assert_eq!(i.lo, 0.0);
        assert!((i.hi - 0.27753).abs() < 1e-4, "{i:?}");
        // 10/10 mirrors it.
        let i = wilson_interval(10, 10).unwrap();
        assert!((i.lo - (1.0 - 0.27753)).abs() < 1e-4, "{i:?}");
        assert!((i.hi - 1.0).abs() < 1e-12, "{i:?}");
        assert_eq!(wilson_interval(0, 0), None);
    }

    #[test]
    fn percentile_matches_hand_computed_fixture() {
        let sorted: Vec<f64> = (1..=101).map(f64::from).collect();
        // position 0.025 * 100 = 2.5 -> halfway between 3 and 4.
        assert_eq!(percentile(&sorted, 0.025), Some(3.5));
        assert_eq!(percentile(&sorted, 0.975), Some(98.5));
        assert_eq!(percentile(&sorted, 0.0), Some(1.0));
        assert_eq!(percentile(&sorted, 1.0), Some(101.0));
        assert_eq!(percentile(&[], 0.5), None);
    }

    #[test]
    fn bootstrap_matches_hand_computed_fixtures() {
        // Identical tasks: every resample has the same mean.
        let i = bootstrap_mean_interval(&[0.5, 0.5, 0.5], 1000, 7).unwrap();
        assert_eq!((i.lo, i.hi), (0.5, 0.5));
        // Two tasks {0, 1}: resample means are 0, 0.5, 1 with probability 1/4, 1/2, 1/4, so the
        // 2.5th and 97.5th percentiles are exactly 0 and 1.
        let i = bootstrap_mean_interval(&[0.0, 1.0], BOOTSTRAP_RESAMPLES, 7).unwrap();
        assert_eq!((i.lo, i.hi), (0.0, 1.0));
        // Seeded: identical across calls, different across seeds only in the draws.
        let values = [0.0, 0.5, 1.0, 1.0, 0.0, 0.5, 1.0];
        let first = bootstrap_mean_interval(&values, BOOTSTRAP_RESAMPLES, 42).unwrap();
        assert_eq!(
            first,
            bootstrap_mean_interval(&values, BOOTSTRAP_RESAMPLES, 42).unwrap()
        );
        let mean_all = mean(&values);
        assert!(first.lo < mean_all && mean_all < first.hi);
        assert_eq!(bootstrap_mean_interval(&[], 100, 1), None);
    }

    #[test]
    fn pass_at_k_edge_cases() {
        assert_eq!(pass_at_k(5, 0, 3).unwrap(), 0.0);
        assert_eq!(pass_at_k(5, 5, 3).unwrap(), 1.0);
        // n - c < k: any sample of k contains a pass.
        assert_eq!(pass_at_k(5, 3, 3).unwrap(), 1.0);
        // 1 - C(3,2)/C(5,2) = 1 - 3/10.
        assert!((pass_at_k(5, 2, 2).unwrap() - 0.7).abs() < 1e-12);
        // k = 1 is the pass fraction.
        assert!((pass_at_k(4, 1, 1).unwrap() - 0.25).abs() < 1e-12);
        assert!(matches!(
            pass_at_k(2, 1, 3),
            Err(BenchError::InsufficientTrials { n: 2, k: 3, .. })
        ));
        assert_eq!(pass_at_k(2, 1, 0), Err(BenchError::ZeroK));
    }

    #[test]
    fn mean_pass_at_k_averages_tasks_and_names_the_short_task() {
        let mut trials = vec![];
        for a in 0..4 {
            trials.push(trial("t1", a, a < 2)); // n=4, c=2
            trials.push(trial("t2", a, false)); // n=4, c=0
        }
        // t1: 1 - C(2,2)/C(4,2) = 5/6; t2: 0; mean 5/12.
        let value = mean_pass_at_k(&trials, 2).unwrap().unwrap();
        assert!((value - 5.0 / 12.0).abs() < 1e-12);
        trials.push(trial("t3", 0, true));
        match mean_pass_at_k(&trials, 2) {
            Err(BenchError::InsufficientTrials { task, n: 1, k: 2 }) => assert_eq!(task, "t3"),
            other => panic!("expected refusal, got {other:?}"),
        }
    }

    #[test]
    fn invalid_trials_are_excluded_and_counted() {
        let mut bad_reward = trial("t1", 1, true);
        bad_reward.reward_valid = false;
        let mut bad_route = trial("t2", 0, true);
        bad_route.invalid_route = true;
        let mut interrupted = trial("t3", 0, true);
        interrupted.infra_interrupted = true;
        let rec = record(
            vec![
                trial("t1", 0, true),
                bad_reward,
                bad_route,
                interrupted,
                trial("t4", 0, false),
            ],
            1,
        );
        let s = summarize(&rec, 1).unwrap();
        assert_eq!(
            (s.valid_trials, s.invalid_trials, s.passed_trials),
            (2, 3, 1)
        );
        assert_eq!(
            (
                s.reward_invalid_trials,
                s.invalid_route_trials,
                s.infra_interrupted_trials
            ),
            (1, 1, 1)
        );
        // t1 and t4 are scored; t2 and t3 have no valid trial.
        assert_eq!((s.tasks_scored, s.tasks_unscored), (2, 2));
        assert_eq!(s.pass_rate, Some(0.5));
        assert_eq!(s.ci_method, Some(IntervalMethod::Wilson));
        assert_eq!(rec.run.totals.invalid_trials, 3);
        // Cost spans every trial: 5 x 0.25.
        assert_eq!(s.cost_per_trial_usd, Some(0.25));
        assert_eq!(s.usd_per_solved_trial, Some(1.25));
        assert_eq!(s.cost_per_task_usd, Some(1.25 / 4.0));
    }

    #[test]
    fn summary_uses_clustered_bootstrap_and_pass_at_k_for_k_above_one() {
        let mut trials = vec![];
        for task in ["a", "b", "c"] {
            for attempt in 0..2 {
                trials.push(trial(
                    task,
                    attempt,
                    task == "a" || attempt == 0 && task == "b",
                ));
            }
        }
        let rec = record(trials, 2);
        let s = summarize(&rec, DEFAULT_BOOTSTRAP_SEED).unwrap();
        // fractions: a=1, b=0.5, c=0.
        assert!((s.pass_rate.unwrap() - 0.5).abs() < 1e-12);
        assert_eq!(s.ci_method, Some(IntervalMethod::ClusteredBootstrap));
        let ci = s.ci.unwrap();
        assert!(ci.lo <= 0.5 && 0.5 <= ci.hi);
        // pass@2: a=1, b=1-C(1,2)/C(2,2)=1, c=0 -> 2/3.
        assert!((s.pass_at_k.unwrap() - 2.0 / 3.0).abs() < 1e-12);
        assert_eq!(s.pass_at_k_refusal, None);
        // 6 x 600 cached / 6 x 300 output.
        assert_eq!(s.cached_tokens_per_output_token, Some(2.0));
        assert_eq!(s.failure_causes.get("verifier_failed"), Some(&3));
    }

    #[test]
    fn pass_at_k_refusal_is_reported_in_the_summary() {
        let mut short = trial("a", 1, true);
        short.reward_valid = false;
        let rec = record(vec![trial("a", 0, true), short], 2);
        let s = summarize(&rec, 1).unwrap();
        assert_eq!(s.pass_at_k, None);
        assert!(s.pass_at_k_refusal.unwrap().contains("task a"));
    }

    #[test]
    fn compare_reports_paired_differences() {
        let a = record(
            vec![
                trial("t1", 0, false),
                trial("t2", 0, true),
                trial("t3", 0, false),
                trial("t4", 0, true),
            ],
            1,
        );
        let b = record(
            vec![
                trial("t1", 0, true),
                trial("t2", 0, true),
                trial("t3", 0, true),
                trial("t4", 0, false),
            ],
            1,
        );
        let c = compare(&a, &b, 5).unwrap();
        assert_eq!((c.improved, c.regressed, c.unchanged), (2, 1, 1));
        assert!((c.mean_diff - 0.25).abs() < 1e-12);
        assert_eq!((c.a_pass_rate, c.b_pass_rate), (0.5, 0.75));
        let diffs: Vec<f64> = c.per_task.iter().map(|t| t.diff).collect();
        assert_eq!(diffs, vec![1.0, 0.0, 1.0, -1.0]);
        assert_eq!(c.verdict, Verdict::Inconclusive);
        let ci = c.ci.unwrap();
        assert!(ci.lo < 0.25 && 0.25 < ci.hi);
    }

    #[test]
    fn compare_flags_a_clear_regression_and_refuses_other_task_sets() {
        let passing: Vec<TrialRow> = (0..20).map(|i| trial(&format!("t{i}"), 0, true)).collect();
        let failing: Vec<TrialRow> = (0..20).map(|i| trial(&format!("t{i}"), 0, false)).collect();
        let c = compare(&record(passing.clone(), 1), &record(failing, 1), 5).unwrap();
        assert_eq!((c.improved, c.regressed), (0, 20));
        assert_eq!(c.verdict, Verdict::Regression);

        let other: Vec<TrialRow> = (1..21).map(|i| trial(&format!("t{i}"), 0, true)).collect();
        let err = compare(&record(passing.clone(), 1), &record(other, 1), 5).unwrap_err();
        assert!(
            matches!(err, BenchError::TaskSetMismatch(m) if m.contains("t0") && m.contains("t20"))
        );

        let mut moved = record(passing.clone(), 1);
        moved.run.dataset_commit = "f".repeat(40);
        moved.seal();
        assert!(matches!(
            compare(&record(passing, 1), &moved, 5),
            Err(BenchError::DatasetMismatch(_))
        ));
    }

    #[test]
    fn compare_leaves_out_tasks_without_a_valid_trial() {
        let mut dead = trial("t2", 0, true);
        dead.infra_interrupted = true;
        let a = record(vec![trial("t1", 0, false), trial("t2", 0, true)], 1);
        let b = record(vec![trial("t1", 0, true), dead], 1);
        let c = compare(&a, &b, 5).unwrap();
        assert_eq!(c.unscored_tasks, vec!["t2".to_string()]);
        assert_eq!(c.per_task.len(), 1);
        let none = record(vec![trial("t1", 0, true)], 1);
        let mut invalid = trial("t1", 0, true);
        invalid.reward_valid = false;
        assert_eq!(
            compare(&none, &record(vec![invalid], 1), 5).unwrap_err(),
            BenchError::NothingToCompare
        );
    }

    #[test]
    fn fingerprint_changes_when_any_single_field_changes() {
        let base = golden_record().fingerprint_inputs();
        let reference = observation_fingerprint(&base);
        assert!(reference.starts_with("sha256:") && reference.len() == 7 + 64);
        type Mutation = fn(&mut FingerprintInputs);
        let mutations: Vec<(&str, Mutation)> = vec![
            ("adapter_tree_hash", |i| i.adapter_tree_hash.push('x')),
            ("dataset_commit", |i| i.dataset_commit.push('x')),
            ("dataset_repo", |i| i.dataset_repo.push('x')),
            ("effort", |i| i.effort = "low".to_string()),
            ("harbor_version", |i| {
                i.harbor_version = "0.25.0".to_string()
            }),
            ("harness_build_digest", |i| i.harness_build_digest.push('x')),
            ("harness_source_digest", |i| {
                i.harness_source_digest.push('x')
            }),
            ("effective_inputs.prompt", |i| {
                i.effective_inputs.prompt.push('x')
            }),
            ("effective_inputs.tool_spec", |i| {
                i.effective_inputs.tool_spec.push('x')
            }),
            ("effective_inputs.policy", |i| {
                i.effective_inputs.policy.push('x')
            }),
            ("effective_inputs.loop_knobs", |i| {
                i.effective_inputs.loop_knobs.push('x')
            }),
            ("effective_inputs.overlay", |i| {
                i.effective_inputs.overlay.push('x')
            }),
            ("image_digests", |i| {
                i.image_digests
                    .insert("fix-git".to_string(), "sha256:0".to_string());
            }),
            ("k", |i| i.k += 1),
            ("model", |i| i.model.push('x')),
            ("route", |i| i.route = "bedrock".to_string()),
            ("sampling", |i| {
                i.sampling
                    .insert("temperature".to_string(), serde_json::json!(0.7));
            }),
            ("split", |i| i.split = "search".to_string()),
            ("tasks", |i| i.tasks.push("extra-task".to_string())),
            ("timeouts.agent_sec", |i| i.timeouts.agent_sec += 1),
            ("timeouts.multiplier", |i| i.timeouts.multiplier = 2.0),
            ("timeouts.verifier_sec", |i| i.timeouts.verifier_sec += 1),
        ];
        for (name, mutate) in mutations {
            let mut changed = base.clone();
            mutate(&mut changed);
            assert_ne!(observation_fingerprint(&changed), reference, "field {name}");
        }
        // Task order is not part of the domain; host, concurrency and times are not either.
        let mut reordered = base.clone();
        reordered.tasks.reverse();
        assert_eq!(observation_fingerprint(&reordered), reference);
        let mut rec = golden_record();
        rec.run.host = "other".to_string();
        rec.run.build_sha = "9".repeat(40);
        rec.run.concurrency = 99;
        rec.run.started_at = "2020-01-01T00:00:00.000000000Z".to_string();
        assert_eq!(rec.computed_fingerprint(), reference);
    }

    #[test]
    fn reuse_lookup_returns_only_an_identical_fingerprint() {
        let a = golden_record();
        let mut other = golden_record();
        other.run.model = "different".to_string();
        other.seal();
        let records = vec![other.clone(), a.clone()];
        let found = find_reusable(&records, &a.run.observation_fingerprint).unwrap();
        assert_eq!(found, &a);
        assert!(find_reusable(&records, "sha256:deadbeef").is_none());
        // A stale stored fingerprint does not match its own contents.
        let mut tampered = a.clone();
        tampered.run.k = 9;
        assert!(find_reusable(&[tampered], &a.run.observation_fingerprint).is_none());
    }

    /// A copy of `base` that ran on another daemon build, sealed.
    fn redeployed(base: &BenchRunRecord) -> BenchRunRecord {
        let mut other = base.clone();
        other.run.build_sha = "2".repeat(40);
        for t in &mut other.trials {
            t.build_sha = "2".repeat(40);
        }
        other.seal();
        other
    }

    fn two_task_baseline() -> BenchRunRecord {
        record(vec![trial("t1", 0, false), trial("t2", 0, true)], 1)
    }

    #[test]
    fn a_redeploy_that_changes_only_the_build_sha_stays_comparable_and_reusable() {
        let baseline = two_task_baseline();
        let mut candidate = redeployed(&baseline);
        candidate.trials[0].passed = true;
        candidate.trials[0].reward = 1.0;
        candidate.seal();
        assert_ne!(baseline.run.build_sha, candidate.run.build_sha);
        assert_eq!(baseline.trials[0].build_sha, "1".repeat(40));
        assert_eq!(candidate.trials[0].build_sha, "2".repeat(40));
        // The build SHA is provenance: the fingerprint, baseline reuse and the compare ignore it.
        assert_eq!(
            baseline.run.observation_fingerprint,
            candidate.run.observation_fingerprint
        );
        assert!(
            find_reusable(&[baseline.clone()], &candidate.run.observation_fingerprint).is_some()
        );
        assert_eq!(check_comparable(&baseline, &candidate), Ok(()));
        assert!(compare(&baseline, &candidate, 5).is_ok());
        // Both records survive a round trip with their own builds.
        let again = BenchRunRecord::from_json(&candidate.to_json().unwrap()).unwrap();
        assert_eq!(again.run.build_sha, "2".repeat(40));
    }

    #[test]
    fn compare_refuses_each_other_difference_naming_the_field() {
        let baseline = two_task_baseline();
        type Mutation = fn(&mut BenchRunRecord);
        let cases: Vec<(&str, Mutation)> = vec![
            ("dataset_repo", |r| r.run.dataset_repo.push('x')),
            ("dataset_commit", |r| r.run.dataset_commit = "f".repeat(40)),
            ("harbor_version", |r| {
                r.run.harbor_version = "0.25.0".to_string()
            }),
            ("adapter_tree_hash", |r| r.run.adapter_tree_hash.push('x')),
            ("model", |r| r.run.model = "other-model".to_string()),
            ("effort", |r| r.run.effort = "low".to_string()),
            ("route", |r| r.run.route = "bedrock".to_string()),
            ("harness_source_digest", |r| {
                r.run.harness_source_digest.push('x');
                for t in &mut r.trials {
                    t.harness_source_digest.push('x');
                }
            }),
            ("effective_inputs.prompt", |r| {
                r.run.effective_inputs.prompt.push('x')
            }),
            ("effective_inputs.tool_spec", |r| {
                r.run.effective_inputs.tool_spec.push('x')
            }),
            ("effective_inputs.policy", |r| {
                r.run.effective_inputs.policy.push('x')
            }),
            ("effective_inputs.loop_knobs", |r| {
                r.run.effective_inputs.loop_knobs.push('x')
            }),
            ("effective_inputs.overlay", |r| {
                r.run.effective_inputs.overlay.push('x')
            }),
            ("harness_version_hash[t2]", |r| {
                r.trials[1].harness_version_hash = "hvh-changed".to_string()
            }),
        ];
        for (field, mutate) in cases {
            let mut candidate = baseline.clone();
            mutate(&mut candidate);
            candidate.seal();
            let error = compare(&baseline, &candidate, 5).unwrap_err();
            match &error {
                BenchError::DatasetMismatch(_) => assert!(field.starts_with("dataset"), "{field}"),
                BenchError::NotComparable { field: named, .. } => assert_eq!(named, field),
                other => panic!("{field}: unexpected {other:?}"),
            }
            if field != "dataset_repo" && field != "dataset_commit" {
                assert!(error.to_string().contains(field), "{error}");
            }
            // The refusal also holds when the candidate is also a redeploy.
            let mut redeploy = redeployed(&candidate);
            redeploy.seal();
            assert!(compare(&baseline, &redeploy, 5).is_err(), "{field}");
        }
    }

    #[test]
    fn the_first_differing_field_is_the_one_named() {
        let baseline = two_task_baseline();
        let mut candidate = baseline.clone();
        candidate.run.effective_inputs.overlay.push('x');
        candidate.run.effective_inputs.prompt.push('x');
        candidate.run.model = "other-model".to_string();
        candidate.seal();
        match check_comparable(&baseline, &candidate).unwrap_err() {
            BenchError::NotComparable {
                field,
                baseline: from,
                candidate: to,
            } => {
                assert_eq!(field, "model");
                assert_eq!(
                    (from.as_str(), to.as_str()),
                    ("claude-sonnet-5-5", "other-model")
                );
            }
            other => panic!("{other:?}"),
        }
        candidate.run.model = baseline.run.model.clone();
        candidate.seal();
        assert!(matches!(
            check_comparable(&baseline, &candidate),
            Err(BenchError::NotComparable { field, .. }) if field == "effective_inputs.prompt"
        ));
    }

    #[test]
    fn an_unavailable_source_digest_is_never_comparable() {
        let digest = crate::harness_source_digest::unavailable_digest(&"1".repeat(40));
        let mut baseline = two_task_baseline();
        baseline.run.harness_source_digest = digest.clone();
        for t in &mut baseline.trials {
            t.harness_source_digest = digest.clone();
        }
        baseline.seal();
        let candidate = baseline.clone();
        assert!(matches!(
            compare(&baseline, &candidate, 5),
            Err(BenchError::NotComparable { field, .. }) if field == "harness_source_digest"
        ));
    }

    #[test]
    fn a_run_cannot_mix_harness_sources_across_trials() {
        let mut mixed = two_task_baseline();
        mixed.trials[1].harness_source_digest = format!("sha256:{}", "9".repeat(64));
        mixed.seal();
        assert!(matches!(
            mixed.validate(),
            Err(BenchError::MixedHarnessSource { task, .. }) if task == "t2"
        ));
        assert!(compare(&mixed, &two_task_baseline(), 5).is_err());
    }

    #[test]
    fn compare_ignores_differences_the_issue_does_not_name() {
        let baseline = two_task_baseline();
        let mut candidate = redeployed(&baseline);
        candidate.run.host = "other-host".to_string();
        candidate.run.concurrency = 1;
        candidate.seal();
        assert!(compare(&baseline, &candidate, 5).is_ok());
    }

    #[test]
    fn solution_lookup_scan_hits() {
        for line in [
            "$ git clone https://github.com/harbor-framework/terminal-bench-2",
            "curl -s https://tbench.ai/tasks/fix-git/solution.sh",
            "wget https://github.com/Laude-Institute/terminal-bench/archive/main.zip",
            "pip install terminal-bench",
        ] {
            let scan = scan_solution_lookup(&format!("ok\n{line}\ndone"), None);
            assert!(scan.suspected, "{line}");
            assert_eq!(scan.hits[0].line, 2);
        }
        let scan = scan_solution_lookup(
            "git clone git@github.com:Someone/Fix-Git-Upstream.git /tmp/x",
            Some("https://github.com/someone/fix-git-upstream.git"),
        );
        assert!(scan.suspected);
        assert_eq!(scan.hits[0].marker, "github.com/someone/fix-git-upstream");
    }

    #[test]
    fn solution_lookup_scan_misses() {
        // A marker without network access, network access without a marker, an unrelated
        // upstream.
        for trace in [
            "cat /app/notes/terminal-bench-readme.txt",
            "curl -s https://example.com/data.csv",
            "git clone https://github.com/someone/other-repo",
            "",
        ] {
            let scan = scan_solution_lookup(trace, Some("github.com/someone/fix-git-upstream"));
            assert!(!scan.suspected, "{trace}");
            assert!(scan.hits.is_empty());
        }
    }

    #[test]
    fn apply_trace_scan_sets_flag_and_hash() {
        let mut t = trial("t1", 0, true);
        t.trace_sha256 = None;
        let scan = t.apply_trace_scan("curl https://tbench.ai/x", None);
        assert!(scan.suspected && t.suspected_solution_lookup);
        assert!(t.trace_sha256.as_deref().unwrap().starts_with("sha256:"));
        let scan = t.apply_trace_scan("ls -la", None);
        assert!(!scan.suspected && !t.suspected_solution_lookup);
    }

    #[test]
    fn the_trace_scan_counts_spill_stubs_and_read_output_calls() {
        let call = |name: &str| format!("{{\"kind\":\"tool_call\",\"name\":\"{name}\",\"seq\":1}}");
        let result = |name: &str, output: &str| {
            format!(
                "{{\"kind\":\"tool_result\",\"name\":\"{name}\",\"result\":{{\"output\":{output:?},\"success\":true}},\"seq\":2}}"
            )
        };
        let trace = [
            call("shell"),
            result("shell", "[rsi-spill ab12/1] shell exit=0 9000B 300 lines"),
            result("shell", "short output"),
            call("read_output"),
            // The retrieval result is never itself a spill.
            result("read_output", "[rsi-spill ab12/1] quoted by the model"),
            result(
                "exec_command",
                "[rsi-spill ab12/2] exec_command exit=1 1B 1 lines",
            ),
            call("read_output"),
            "{\"kind\":\"model_request\",\"note\":\"\\\"kind\\\":\\\"tool_call\\\"\"}".to_string(),
            "not json".to_string(),
        ]
        .join("\n");
        let counts = scan_output_spills(&trace);
        assert_eq!(counts.spills, 2);
        assert_eq!(counts.read_output_calls, 2);

        let mut t = trial("t1", 0, true);
        t.apply_trace_scan(&trace, None);
        assert_eq!((t.spills, t.read_output_calls), (2, 2));
        // Zero counts are omitted, so a record without spills is unchanged; an old record parses.
        let mut quiet = trial("t2", 0, true);
        quiet.apply_trace_scan("ls", None);
        let json = serde_json::to_value(&quiet).unwrap();
        assert!(json.get("spills").is_none() && json.get("read_output_calls").is_none());
        let back: TrialRow = serde_json::from_value(json).unwrap();
        assert_eq!((back.spills, back.read_output_calls), (0, 0));
        let json = serde_json::to_value(&t).unwrap();
        assert_eq!(json["spills"], 2);
        assert_eq!(json["read_output_calls"], 2);
    }
}
