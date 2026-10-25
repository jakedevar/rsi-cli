//! Record assembly: join each trial's Harbor `result.json` with its `bench_tasks`
//! row into the `BenchRunRecord` v1 (#3179) and the job's typed summary.
//!
//! Harbor says whether the verifier passed the trial; the daemon says what the
//! session spent, which route it ran and what its trace holds. The join key is
//! the trial name: the adapter starts each trial with Harbor's own trial name, so
//! `bench_tasks.trial` is the Harbor trial directory name. A trial Harbor never
//! ran (the run stopped first) becomes a row that is invalid by construction
//! (`reward_valid` false), so it is counted and never read as a pass or a fail.
//!
//! R3a (#3680, design #3628): [`assemble_runner`] builds the same record from the Rust TB2
//! runner's `TrialOutcomeV1`s (oracle, graded, invalid, infra-interrupted and not-run rows),
//! naming the Rust evaluator (`run.harbor_version`, `run.adapter_tree_hash`, `sampling.runner`) so
//! a Harbor record and a Rust record are never one comparable observation. [`assemble`] stays the
//! legacy reader for historical Harbor trees until R3b/R7 retire the Harbor runtime.

use super::config::ResolvedRun;
use crate::store::bench::BenchTaskRow;
use chrono::{DateTime, Utc};
use rsi_common::agent_jobs::{BenchJobResultV1, BenchShardResultV1};
use rsi_common::bench::BenchTaskState;
use rsi_common::bench_profile::BENCH_INVALID_ROUTE;
use rsi_common::bench_record::{
    BenchRunRecord, EffectiveInputs, RunInfo, SCHEMA_VERSION, Timeouts, Totals, TrialRow,
};
use rsi_common::bench_runner::{
    RUNNER_VERSION_DISCRIMINATOR, RunnerMode, RunnerSourceManifest, TrialExceptionKind,
    TrialOutcomeV1, TrialState, trial_key,
};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;
use uuid::Uuid;

/// Harbor exception types whose trial has no trustworthy reward: the adapter's
/// own refusals, an infrastructure interruption and a verifier that produced no
/// reward. (An agent timeout is graded normally: the verifier still runs.)
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const INVALID_REWARD_EXCEPTIONS: [&str; 10] = [
    "RsiBenchRouteError",
    "RsiBenchSpendCapError",
    "RsiBenchInfraInterrupted",
    "VerifierTimeoutError",
    "RewardFileNotFoundError",
    "RewardFileEmptyError",
    "VerifierOutputParseError",
    "ApiUsageLimitError",
    "AgentAuthenticationError",
    "ModelNotFoundError",
];
/// Harbor exception raised by the adapter after a daemon restart.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const INFRA_EXCEPTION: &str = "RsiBenchInfraInterrupted";
/// Harbor exception raised by the adapter on a route refusal.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const ROUTE_EXCEPTION: &str = "RsiBenchRouteError";
/// `failure_cause` and stop reason of a trial the run stopped before it started.
pub(crate) const NOT_RUN_BUDGET: &str = "budget_exhausted";
pub(crate) const NOT_RUN_STOPPED: &str = "not_run";

/// The largest trace the daemon reads back to scan for solution lookups.
pub(crate) const MAX_TRACE_SCAN_BYTES: u64 = 64 * 1024 * 1024;

/// One Harbor `result.json`, read tolerantly (the fields Harbor 0.24.0 writes
/// and the two a later Harbor may add).
#[derive(Debug, Clone, Default, PartialEq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct HarborTrial {
    pub trial_name: String,
    pub task_name: String,
    pub reward: Option<f64>,
    pub exception_type: Option<String>,
    pub failure_cause: Option<String>,
    pub reward_valid: Option<bool>,
    pub n_input_tokens: Option<u64>,
    pub n_cache_tokens: Option<u64>,
    pub n_output_tokens: Option<u64>,
    pub cost_usd: Option<f64>,
    pub wall_ms: Option<u64>,
    pub started_at: Option<DateTime<Utc>>,
}

#[cfg_attr(not(test), allow(dead_code))]

fn timestamp(value: Option<&Value>) -> Option<DateTime<Utc>> {
    value
        .and_then(Value::as_str)
        .and_then(|text| DateTime::parse_from_rfc3339(text).ok())
        .map(|at| at.with_timezone(&Utc))
}

#[cfg_attr(not(test), allow(dead_code))]

impl HarborTrial {
    /// Parse one trial's `result.json`.
    ///
    /// # Errors
    /// The file has no `trial_name` and `task_name`.
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        let value: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
        let string = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_string);
        let (Some(trial_name), Some(task_name)) = (string("trial_name"), string("task_name"))
        else {
            return Err("result.json has no trial_name and task_name".into());
        };
        let reward = value
            .pointer("/verifier_result/rewards")
            .and_then(Value::as_object)
            .and_then(|rewards| {
                rewards
                    .get("reward")
                    .or_else(|| rewards.values().next())
                    .and_then(Value::as_f64)
            });
        let exception_type = value
            .pointer("/exception_info/exception_type")
            .and_then(Value::as_str)
            .map(str::to_string);
        let agent = value.get("agent_result");
        let count = |key: &str| agent.and_then(|a| a.get(key)).and_then(Value::as_u64);
        let started_at = timestamp(value.get("started_at"));
        let finished_at = timestamp(value.get("finished_at"));
        let wall_ms = started_at
            .zip(finished_at)
            .and_then(|(start, end)| u64::try_from((end - start).num_milliseconds()).ok());
        Ok(Self {
            trial_name,
            task_name,
            reward,
            exception_type,
            failure_cause: string("failure_cause"),
            reward_valid: value.get("reward_valid").and_then(Value::as_bool),
            n_input_tokens: count("n_input_tokens"),
            n_cache_tokens: count("n_cache_tokens"),
            n_output_tokens: count("n_output_tokens"),
            cost_usd: agent
                .and_then(|a| a.get("cost_usd"))
                .and_then(Value::as_f64),
            wall_ms,
            started_at,
        })
    }
}

/// Every `<root>/<shard>/<job>/<trial>/result.json` under a collected Harbor tree.
/// A job-level `result.json` (directly under `<job>/`) is not a trial.
///
/// # Errors
/// A directory that cannot be read; an unparsable trial file is skipped and
/// named in the returned notes.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn read_harbor_trials(
    harbor_root: &Path,
) -> std::io::Result<(Vec<HarborTrial>, Vec<String>)> {
    let mut trials = Vec::new();
    let mut notes = Vec::new();
    if !harbor_root.is_dir() {
        return Ok((trials, notes));
    }
    let sorted_dirs = |dir: &Path| -> std::io::Result<Vec<std::path::PathBuf>> {
        let mut entries: Vec<_> = std::fs::read_dir(dir)?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        entries.sort();
        Ok(entries)
    };
    for shard in sorted_dirs(harbor_root)? {
        for job in sorted_dirs(&shard)? {
            for trial_dir in sorted_dirs(&job)? {
                let file = trial_dir.join("result.json");
                if !file.is_file() {
                    continue;
                }
                match std::fs::read_to_string(&file)
                    .map_err(|e| e.to_string())
                    .and_then(|text| HarborTrial::parse(&text))
                {
                    Ok(trial) => trials.push(trial),
                    Err(error) => notes.push(format!("{}: {error}", file.display())),
                }
            }
        }
    }
    Ok((trials, notes))
}

/// Why the run did not run every trial.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StopNote {
    /// `budget_exhausted`, `cancelled`, `pin_check_failed`, `transport_failed`.
    pub reason: String,
    /// The `failure_cause` an unrun trial carries.
    pub cause: &'static str,
}

/// Everything [`assemble`] reads.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct RecordInputs<'a> {
    pub resolved: &'a ResolvedRun,
    pub host: &'a str,
    pub concurrency: u32,
    pub adapter_tree_hash: &'a str,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub harbor_trials: Vec<HarborTrial>,
    pub bench_tasks: Vec<BenchTaskRow>,
    /// Settled model calls per session (the trial's turns).
    pub calls: BTreeMap<Uuid, u64>,
    pub stop: Option<StopNote>,
    /// The trace text of a trial, when it can be read (bounded).
    pub trace_text: &'a dyn Fn(&BenchTaskRow) -> Option<String>,
    /// Where each trial session's harness came from (#3192), keyed by session.
    pub provenance: BTreeMap<Uuid, TrialProvenance>,
    /// #3191: the run overlay's digest and the sha256 of each file it references,
    /// when the run had one. Part of the run's sampling, so the fingerprint names it
    /// even for a run whose trials never wrote a build digest.
    pub overlay: Option<Value>,
}

/// Where one trial session's harness came from (#3192): its `bench_build_digest` row
/// (`rsid_build_sha`, `harness_source_digest`, `effective_inputs`) and its
/// `Session.harness_version_hash`, read by the caller from the store.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct TrialProvenance {
    pub build_sha: Option<String>,
    pub harness_source_digest: Option<String>,
    pub harness_version_hash: Option<String>,
    pub effective_inputs: Option<EffectiveInputs>,
}

impl TrialProvenance {
    /// The provenance of a session from its last `bench_build_digest` row (if it wrote one) and
    /// its `harness_version_hash`.
    pub(crate) fn from_digest_row(
        row: Option<&Value>,
        harness_version_hash: Option<String>,
    ) -> Self {
        let text = |key: &str| {
            row.and_then(|r| r.get(key))
                .and_then(Value::as_str)
                .filter(|v| !v.trim().is_empty())
                .map(str::to_string)
        };
        Self {
            build_sha: text("rsid_build_sha"),
            harness_source_digest: text("harness_source_digest"),
            harness_version_hash: harness_version_hash.filter(|h| !h.trim().is_empty()),
            effective_inputs: row
                .and_then(|r| r.get("effective_inputs"))
                .and_then(|v| serde_json::from_value(v.clone()).ok()),
        }
    }
}

/// The run's own provenance: the first trial session's (in daemon row order, like
/// `harness_build_digest`), else this daemon's build. A trial row with no provenance of its own
/// takes these values, so a sessionless row never reads as a mixed harness source.
struct RunProvenance {
    build_sha: String,
    harness_source_digest: String,
    effective_inputs: EffectiveInputs,
}

fn run_provenance(
    resolved: &ResolvedRun,
    rows: &[BenchTaskRow],
    provenance: &BTreeMap<Uuid, TrialProvenance>,
) -> RunProvenance {
    let known: Vec<&TrialProvenance> = rows
        .iter()
        .filter_map(|row| row.session_id)
        .filter_map(|id| provenance.get(&id))
        .collect();
    let marker = if resolved.is_paid() {
        "unknown"
    } else {
        "none"
    };
    RunProvenance {
        build_sha: known
            .iter()
            .find_map(|p| p.build_sha.clone())
            .unwrap_or_else(|| crate::daemon_info::BUILD_SHA.to_string()),
        harness_source_digest: known
            .iter()
            .find_map(|p| p.harness_source_digest.clone())
            .unwrap_or_else(|| crate::daemon_info::HARNESS_SOURCE_DIGEST.to_string()),
        effective_inputs: known
            .iter()
            .find_map(|p| p.effective_inputs.clone())
            .unwrap_or_else(|| EffectiveInputs {
                loop_knobs: marker.into(),
                overlay: marker.into(),
                policy: marker.into(),
                prompt: marker.into(),
                tool_spec: marker.into(),
            }),
    }
}

fn clamp_u64(value: f64) -> u64 {
    if value.is_finite() && value > 0.0 {
        value as u64
    } else {
        0
    }
}

/// The record and the per-trial classification the summary counts.
pub(crate) struct Assembled {
    pub record: BenchRunRecord,
    pub not_run: u32,
}

/// What both assemblers read, borrowed from their input struct.
struct Context<'a> {
    resolved: &'a ResolvedRun,
    host: &'a str,
    concurrency: u32,
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
    bench_tasks: &'a [BenchTaskRow],
    calls: &'a BTreeMap<Uuid, u64>,
    stop: Option<&'a StopNote>,
    trace_text: &'a dyn Fn(&BenchTaskRow) -> Option<String>,
    provenance: &'a BTreeMap<Uuid, TrialProvenance>,
    overlay: Option<&'a Value>,
}

#[cfg_attr(not(test), allow(dead_code))]

impl<'a> RecordInputs<'a> {
    fn context(&'a self) -> Context<'a> {
        Context {
            resolved: self.resolved,
            host: self.host,
            concurrency: self.concurrency,
            started_at: self.started_at,
            finished_at: self.finished_at,
            bench_tasks: &self.bench_tasks,
            calls: &self.calls,
            stop: self.stop.as_ref(),
            trace_text: self.trace_text,
            provenance: &self.provenance,
            overlay: self.overlay.as_ref(),
        }
    }
}

/// Which evaluator produced the trials: the historical Harbor adapter, or the Rust runner
/// (R3a). It decides `run.harbor_version`, `run.adapter_tree_hash` and `sampling.runner`, the
/// fields that make a Harbor record and a Rust record `NotComparable`.
enum Evaluator<'a> {
    #[cfg_attr(not(test), allow(dead_code))]
    Harbor {
        adapter_tree_hash: &'a str,
    },
    Rust(&'a RunnerSourceManifest),
}

/// Join Harbor's results with the daemon's rows into the sealed record. This is the legacy
/// reader: it keeps historical Harbor trees assemblable and is retired with the Harbor runtime.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn assemble(inputs: RecordInputs<'_>) -> Assembled {
    let ctx = inputs.context();
    let resolved = ctx.resolved;
    let run_prov = run_provenance(resolved, ctx.bench_tasks, ctx.provenance);
    let by_trial: BTreeMap<&str, &BenchTaskRow> = ctx
        .bench_tasks
        .iter()
        .map(|row| (row.trial.as_str(), row))
        .collect();
    // Attempt numbers for trials the daemon never saw: Harbor's start order.
    let mut ordered: Vec<&HarborTrial> = inputs.harbor_trials.iter().collect();
    ordered.sort_by(|a, b| {
        (a.task_name.as_str(), a.started_at, a.trial_name.as_str()).cmp(&(
            b.task_name.as_str(),
            b.started_at,
            b.trial_name.as_str(),
        ))
    });
    let mut next_attempt: BTreeMap<&str, u32> = BTreeMap::new();
    let mut trials: Vec<TrialRow> = Vec::new();
    for harbor in ordered {
        let counter = next_attempt.entry(harbor.task_name.as_str()).or_insert(0);
        *counter += 1;
        let bench = by_trial.get(harbor.trial_name.as_str()).copied();
        let attempt = bench.map_or(*counter, |row| row.attempt);
        trials.push(trial_row(&ctx, harbor, bench, attempt, &run_prov));
    }
    finish_record(
        &ctx,
        &Evaluator::Harbor {
            adapter_tree_hash: inputs.adapter_tree_hash,
        },
        trials,
        &run_prov,
    )
}

/// Everything [`assemble_runner`] reads (R3a): the Rust runner's [`TrialOutcomeV1`]s instead of
/// Harbor `result.json` files, plus the same daemon rows and provenance.
pub(crate) struct RunnerRecordInputs<'a> {
    pub resolved: &'a ResolvedRun,
    pub host: &'a str,
    pub concurrency: u32,
    /// The runner's stamped source identities (`rsi-eval bench tb2 stamp`), verified by the
    /// caller at admission. They define the record's evaluator identity.
    pub evaluator: &'a RunnerSourceManifest,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    /// One per attempt the runner made: oracle, graded, invalid and infra-interrupted alike.
    pub outcomes: Vec<TrialOutcomeV1>,
    /// Joined to an outcome by [`rsi_common::bench_runner::trial_key`] == `bench_tasks.trial`.
    pub bench_tasks: Vec<BenchTaskRow>,
    pub calls: BTreeMap<Uuid, u64>,
    pub stop: Option<StopNote>,
    pub trace_text: &'a dyn Fn(&BenchTaskRow) -> Option<String>,
    pub provenance: BTreeMap<Uuid, TrialProvenance>,
    pub overlay: Option<Value>,
}

/// Assemble the sealed v1 record from the Rust runner's outcomes (design "Outcomes, records,
/// provenance and budgets"): the daemon rows keep authority over spend, tokens, route and trace;
/// the runner's outcome says whether the verifier passed. Attempts are numbered per task in
/// `(slot, retry)` order, and an infrastructure-interrupted attempt keeps its row and cost but
/// does not fill its statistical slot.
pub(crate) fn assemble_runner(inputs: RunnerRecordInputs<'_>) -> Assembled {
    let ctx = Context {
        resolved: inputs.resolved,
        host: inputs.host,
        concurrency: inputs.concurrency,
        started_at: inputs.started_at,
        finished_at: inputs.finished_at,
        bench_tasks: &inputs.bench_tasks,
        calls: &inputs.calls,
        stop: inputs.stop.as_ref(),
        trace_text: inputs.trace_text,
        provenance: &inputs.provenance,
        overlay: inputs.overlay.as_ref(),
    };
    let run_prov = run_provenance(ctx.resolved, ctx.bench_tasks, ctx.provenance);
    let by_trial: BTreeMap<&str, &BenchTaskRow> = ctx
        .bench_tasks
        .iter()
        .map(|row| (row.trial.as_str(), row))
        .collect();
    let mut ordered: Vec<&TrialOutcomeV1> = inputs.outcomes.iter().collect();
    ordered.sort_by(|a, b| {
        let key =
            |o: &&TrialOutcomeV1| (o.identity.task.clone(), o.identity.slot, o.identity.retry);
        key(a).cmp(&key(b))
    });
    let mut next_attempt: BTreeMap<&str, u32> = BTreeMap::new();
    let mut trials: Vec<TrialRow> = Vec::new();
    for outcome in ordered {
        let counter = next_attempt
            .entry(outcome.identity.task.as_str())
            .or_insert(0);
        *counter += 1;
        let bench = by_trial.get(trial_key(&outcome.identity).as_str()).copied();
        trials.push(runner_trial_row(&ctx, outcome, bench, *counter, &run_prov));
    }
    finish_record(&ctx, &Evaluator::Rust(inputs.evaluator), trials, &run_prov)
}

/// Fill the slots the run never ran, then build and seal the record. Shared by both assemblers.
fn finish_record(
    ctx: &Context<'_>,
    evaluator: &Evaluator<'_>,
    mut trials: Vec<TrialRow>,
    run_prov: &RunProvenance,
) -> Assembled {
    let resolved = ctx.resolved;
    trials.sort_by(|a, b| (a.task.as_str(), a.attempt).cmp(&(b.task.as_str(), b.attempt)));

    // Trials the run never ran: k valid-or-counted slots per task.
    let mut not_run = 0_u32;
    if let Some(stop) = ctx.stop {
        let cause = stop.cause;
        for task in &resolved.tasks {
            let present = trials
                .iter()
                .filter(|t| &t.task == task && !t.infra_interrupted)
                .count();
            let missing = (resolved.file.k as usize).saturating_sub(present);
            let mut attempt = trials
                .iter()
                .filter(|t| &t.task == task)
                .map(|t| t.attempt)
                .max()
                .unwrap_or(0);
            for _ in 0..missing {
                attempt += 1;
                not_run += 1;
                trials.push(not_run_row(resolved, task, attempt, cause, run_prov));
            }
        }
        trials.sort_by(|a, b| (a.task.as_str(), a.attempt).cmp(&(b.task.as_str(), b.attempt)));
    }

    let harness_build_digest = ctx
        .bench_tasks
        .iter()
        .find_map(|row| row.harness_build_digest.clone())
        .unwrap_or_else(|| {
            if resolved.is_paid() {
                "unknown".to_string()
            } else {
                "none".to_string()
            }
        });
    let tasks_dataset = |pick: fn(&super::config::DatasetTask) -> f64| {
        resolved
            .tasks
            .iter()
            .filter_map(|t| resolved.task(t))
            .map(pick)
            .fold(0.0_f64, f64::max)
    };
    let mut sampling: BTreeMap<String, Value> = BTreeMap::new();
    sampling.insert(
        "agent".into(),
        Value::String(if resolved.is_paid() { "rsi" } else { "oracle" }.into()),
    );
    sampling.insert(
        "loop_knobs".into(),
        serde_json::to_value(resolved.file.loop_knobs).unwrap_or(Value::Null),
    );
    if let Some(cap) = resolved.file.agent.per_task_cap_usd {
        sampling.insert("per_task_cap_usd".into(), serde_json::json!(cap));
    }
    // A run with no overlay (or an empty one) adds no key, so its record and its
    // fingerprint are the ones a run had before overlays existed.
    if let Some(overlay) = ctx.overlay {
        sampling.insert("overlay".into(), overlay.clone());
    }
    let (adapter_tree_hash, harbor_version) = match evaluator {
        Evaluator::Harbor { adapter_tree_hash } => (
            (*adapter_tree_hash).to_string(),
            resolved.dataset.header.harbor_version.clone(),
        ),
        Evaluator::Rust(manifest) => {
            // The Rust runner is named in the sampling too, so the fingerprint carries its
            // version, retry policy and source digest beside the legacy fields.
            sampling.insert("runner".into(), manifest.sampling_runner());
            (
                manifest.evaluator_digest(),
                RUNNER_VERSION_DISCRIMINATOR.to_string(),
            )
        }
    };
    let mut record = BenchRunRecord {
        schema_version: SCHEMA_VERSION,
        run: RunInfo {
            adapter_tree_hash,
            build_sha: run_prov.build_sha.clone(),
            concurrency: ctx.concurrency,
            config_digest: resolved.config_digest.clone(),
            dataset_commit: resolved.dataset.header.commit.clone(),
            dataset_repo: resolved.dataset.header.repo.clone(),
            effective_inputs: run_prov.effective_inputs.clone(),
            effort: resolved
                .file
                .agent
                .effort
                .clone()
                .unwrap_or_else(|| "none".into()),
            finished_at: ctx
                .finished_at
                .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
            harbor_version,
            harness_build_digest,
            harness_source_digest: run_prov.harness_source_digest.clone(),
            host: ctx.host.to_string(),
            image_digests: resolved
                .tasks
                .iter()
                .filter_map(|t| {
                    resolved
                        .task(t)
                        .map(|d| (t.clone(), d.image_digest.clone()))
                })
                .collect(),
            k: resolved.file.k,
            model: resolved
                .file
                .agent
                .model
                .clone()
                .unwrap_or_else(|| "oracle".into()),
            observation_fingerprint: String::new(),
            route: if resolved.is_paid() {
                rsi_common::bench_profile::RequiredRoute::Harness
                    .as_str()
                    .to_string()
            } else {
                "oracle".to_string()
            },
            sampling,
            split: resolved.split_label.clone(),
            started_at: ctx
                .started_at
                .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
            tasks: resolved.tasks.clone(),
            timeouts: Timeouts {
                agent_sec: clamp_u64(tasks_dataset(|t| t.agent_timeout_sec)),
                multiplier: resolved.file.timeout_multiplier,
                verifier_sec: clamp_u64(tasks_dataset(|t| t.verifier_timeout_sec)),
            },
            totals: Totals::default(),
        },
        trials,
    };
    record.seal();
    Assembled { record, not_run }
}

#[cfg_attr(not(test), allow(dead_code))]

fn trial_row(
    ctx: &Context<'_>,
    harbor: &HarborTrial,
    bench: Option<&BenchTaskRow>,
    attempt: u32,
    run_prov: &RunProvenance,
) -> TrialRow {
    let resolved = ctx.resolved;
    let exception = harbor.exception_type.as_deref();
    let infra = bench.is_some_and(|row| row.state == BenchTaskState::InfraInterrupted)
        || exception == Some(INFRA_EXCEPTION);
    let invalid_route = bench
        .is_some_and(|row| row.route_verdict.as_deref() == Some(BENCH_INVALID_ROUTE))
        || exception == Some(ROUTE_EXCEPTION);
    let invalid_exception = exception.is_some_and(|e| INVALID_REWARD_EXCEPTIONS.contains(&e));
    let reward_valid = harbor
        .reward_valid
        .unwrap_or(harbor.reward.is_some() && !invalid_exception)
        && !infra
        && !invalid_route;
    let reward = harbor.reward.unwrap_or(0.0);
    let usage_cost = bench.and_then(|row| row.cost_usd);
    let failure_cause = harbor.failure_cause.clone().or_else(|| {
        if infra {
            Some("infra_interrupted".to_string())
        } else if invalid_route {
            Some(BENCH_INVALID_ROUTE.to_string())
        } else {
            harbor.exception_type.clone()
        }
    });
    let session = bench.and_then(|r| r.session_id);
    let prov = session.and_then(|id| ctx.provenance.get(&id));
    let mut row = TrialRow {
        attempt,
        build_sha: prov
            .and_then(|p| p.build_sha.clone())
            .unwrap_or_else(|| run_prov.build_sha.clone()),
        cached_tokens: bench
            .and_then(|r| r.cached_tokens)
            .or(harbor.n_cache_tokens)
            .unwrap_or(0),
        cost_usd: usage_cost.or(harbor.cost_usd).unwrap_or(0.0),
        exception_type: harbor.exception_type.clone(),
        failure_cause,
        harness_source_digest: prov
            .and_then(|p| p.harness_source_digest.clone())
            .unwrap_or_else(|| run_prov.harness_source_digest.clone()),
        harness_version_hash: prov
            .and_then(|p| p.harness_version_hash.clone())
            .unwrap_or_else(|| if session.is_some() { "unknown" } else { "none" }.to_string()),
        image_digest: resolved
            .task(&harbor.task_name)
            .map(|t| t.image_digest.clone())
            .unwrap_or_default(),
        infra_interrupted: infra,
        invalid_route,
        output_tokens: bench
            .and_then(|r| r.output_tokens)
            .or(harbor.n_output_tokens)
            .unwrap_or(0),
        passed: reward_valid && reward >= 1.0,
        prompt_tokens: bench
            .and_then(|r| r.prompt_tokens)
            .or(harbor.n_input_tokens)
            .unwrap_or(0),
        reasoning_tokens: bench.and_then(|r| r.reasoning_tokens).unwrap_or(0),
        reward,
        reward_valid,
        session_id: bench.and_then(|r| r.session_id).map(|id| id.to_string()),
        split: resolved.task_split(&harbor.task_name).to_string(),
        stop_reason: bench
            .and_then(|r| r.stop_reason.clone())
            .or_else(|| harbor.exception_type.clone()),
        read_output_calls: 0,
        spills: 0,
        suspected_solution_lookup: false,
        task: harbor.task_name.clone(),
        trace_sha256: bench.and_then(|r| r.trace_sha256.clone()),
        turns: bench
            .and_then(|r| r.session_id)
            .and_then(|id| ctx.calls.get(&id).copied())
            .map_or(0, |calls| u32::try_from(calls).unwrap_or(u32::MAX)),
        wall_ms: bench
            .and_then(|r| r.wall_ms)
            .or(harbor.wall_ms)
            .unwrap_or(0),
    };
    if let Some(row_bench) = bench
        && let Some(text) = (ctx.trace_text)(row_bench)
    {
        row.apply_trace_scan(&text, None);
    }
    row
}

/// `failure_cause` of a trial whose verdict is invalid but whose outcome names no exception.
pub(crate) const REWARD_INVALID: &str = "reward_invalid";
/// `failure_cause` of an oracle outcome that nevertheless reports model usage: an oracle never
/// opens a session, so a daemon row or a session count is a defect, never a pass.
pub(crate) const ORACLE_MODEL_USAGE: &str = "oracle_model_usage";

/// One trial row from a Rust runner outcome (R3a; design "Outcomes, records, provenance and
/// budgets"). The outcome supplies the verdict, the typed cause and the wall time; the daemon
/// row, when the trial had a session, supplies tokens, spend, route, session and trace. An
/// oracle has no row, so its usage is zero by construction.
fn runner_trial_row(
    ctx: &Context<'_>,
    outcome: &TrialOutcomeV1,
    bench: Option<&BenchTaskRow>,
    attempt: u32,
    run_prov: &RunProvenance,
) -> TrialRow {
    let resolved = ctx.resolved;
    let task = outcome.identity.task.as_str();
    let has = |kind: TrialExceptionKind| outcome.exceptions.iter().any(|e| e.kind == kind);
    let infra = has(TrialExceptionKind::InfraInterrupted)
        || bench.is_some_and(|row| row.state == BenchTaskState::InfraInterrupted);
    let invalid_route = has(TrialExceptionKind::RouteRejected)
        || bench.is_some_and(|row| row.route_verdict.as_deref() == Some(BENCH_INVALID_ROUTE));
    // An oracle never opens a session. A daemon row or a session count on an oracle outcome is a
    // wiring defect: keep its cost visible and make the trial invalid rather than a pass.
    let oracle_model_usage =
        outcome.mode == RunnerMode::Oracle && (bench.is_some() || outcome.model_sessions > 0);
    // A reward claim cannot substitute for the runner's phase evidence or the daemon's
    // settlement receipt. Cancel changes state before Finish settles usage and shell execs.
    let settled = [
        TrialState::AgentSettled,
        TrialState::Verifying,
        TrialState::Terminal,
    ]
    .iter()
    .all(|state| outcome.states.contains(state))
        && (outcome.mode == RunnerMode::Oracle
            || bench.is_some_and(|row| {
                !row.state.is_live() && row.finished_at.is_some() && row.session_id.is_some()
            }));
    let reward_valid = outcome.reward_valid
        && outcome.reward.is_some_and(f64::is_finite)
        && settled
        && outcome
            .exceptions
            .iter()
            .all(|e| e.kind == TrialExceptionKind::AgentTimeout)
        && !infra
        && !invalid_route
        && !oracle_model_usage;
    let reward = outcome.reward.unwrap_or(0.0);
    let last_kind = outcome.exceptions.last().map(|e| e.kind.as_str());
    let failure_cause = if infra {
        Some("infra_interrupted".to_string())
    } else if invalid_route {
        Some(BENCH_INVALID_ROUTE.to_string())
    } else if oracle_model_usage {
        Some(ORACLE_MODEL_USAGE.to_string())
    } else {
        last_kind
            .map(str::to_string)
            .or_else(|| {
                (!settled).then(|| TrialExceptionKind::AgentSettleFailed.as_str().to_string())
            })
            .or_else(|| (!reward_valid).then(|| REWARD_INVALID.to_string()))
    };
    let session = bench.and_then(|r| r.session_id);
    let prov = session.and_then(|id| ctx.provenance.get(&id));
    let phase_wall_ms: u64 = outcome.phases.iter().map(|p| p.elapsed_ms).sum();
    let mut row = TrialRow {
        attempt,
        build_sha: prov
            .and_then(|p| p.build_sha.clone())
            .unwrap_or_else(|| run_prov.build_sha.clone()),
        cached_tokens: bench.and_then(|r| r.cached_tokens).unwrap_or(0),
        cost_usd: bench.and_then(|r| r.cost_usd).unwrap_or(0.0),
        exception_type: last_kind.map(str::to_string),
        failure_cause,
        harness_source_digest: prov
            .and_then(|p| p.harness_source_digest.clone())
            .unwrap_or_else(|| run_prov.harness_source_digest.clone()),
        harness_version_hash: prov
            .and_then(|p| p.harness_version_hash.clone())
            .unwrap_or_else(|| if session.is_some() { "unknown" } else { "none" }.to_string()),
        image_digest: resolved
            .task(task)
            .map(|t| t.image_digest.clone())
            .unwrap_or_default(),
        infra_interrupted: infra,
        invalid_route,
        output_tokens: bench.and_then(|r| r.output_tokens).unwrap_or(0),
        passed: reward_valid && reward >= 1.0,
        prompt_tokens: bench.and_then(|r| r.prompt_tokens).unwrap_or(0),
        reasoning_tokens: bench.and_then(|r| r.reasoning_tokens).unwrap_or(0),
        reward,
        reward_valid,
        session_id: session.map(|id| id.to_string()),
        split: resolved.task_split(task).to_string(),
        stop_reason: bench
            .and_then(|r| r.stop_reason.clone())
            .or_else(|| last_kind.map(str::to_string)),
        read_output_calls: 0,
        spills: 0,
        suspected_solution_lookup: false,
        task: task.to_string(),
        trace_sha256: bench.and_then(|r| r.trace_sha256.clone()),
        turns: session
            .and_then(|id| ctx.calls.get(&id).copied())
            .map_or(0, |calls| u32::try_from(calls).unwrap_or(u32::MAX)),
        wall_ms: bench.and_then(|r| r.wall_ms).unwrap_or(phase_wall_ms),
    };
    if let Some(row_bench) = bench
        && let Some(text) = (ctx.trace_text)(row_bench)
    {
        row.apply_trace_scan(&text, None);
    }
    row
}

fn not_run_row(
    resolved: &ResolvedRun,
    task: &str,
    attempt: u32,
    cause: &'static str,
    run_prov: &RunProvenance,
) -> TrialRow {
    TrialRow {
        attempt,
        build_sha: run_prov.build_sha.clone(),
        cached_tokens: 0,
        cost_usd: 0.0,
        exception_type: None,
        failure_cause: Some(cause.to_string()),
        harness_source_digest: run_prov.harness_source_digest.clone(),
        harness_version_hash: "none".to_string(),
        image_digest: resolved
            .task(task)
            .map(|t| t.image_digest.clone())
            .unwrap_or_default(),
        infra_interrupted: false,
        invalid_route: false,
        output_tokens: 0,
        passed: false,
        prompt_tokens: 0,
        reasoning_tokens: 0,
        reward: 0.0,
        reward_valid: false,
        session_id: None,
        split: resolved.task_split(task).to_string(),
        stop_reason: Some(cause.to_string()),
        read_output_calls: 0,
        spills: 0,
        suspected_solution_lookup: false,
        task: task.to_string(),
        trace_sha256: None,
        turns: 0,
        wall_ms: 0,
    }
}

/// The job's typed summary of a sealed record.
pub(crate) fn summarize(
    run_id: Uuid,
    config_path: &str,
    resolved: &ResolvedRun,
    assembled: &Assembled,
    shards: Vec<BenchShardResultV1>,
    record_path: Option<String>,
    budget_usd: f64,
    stopped: Option<String>,
) -> BenchJobResultV1 {
    let record = &assembled.record;
    let counted = |pick: &dyn Fn(&TrialRow) -> bool| {
        u32::try_from(record.trials.iter().filter(|t| pick(t)).count()).unwrap_or(u32::MAX)
    };
    let micro = |usd: f64| clamp_u64((usd * 1_000_000.0).round());
    BenchJobResultV1 {
        run_id: run_id.to_string(),
        config: config_path.to_string(),
        config_digest: resolved.config_digest.clone(),
        record_path,
        observation_fingerprint: Some(record.run.observation_fingerprint.clone()),
        shards,
        expected_trials: resolved.expected_trials(),
        trials: counted(&|t| {
            t.failure_cause.as_deref() != Some(NOT_RUN_BUDGET)
                && t.failure_cause.as_deref() != Some(NOT_RUN_STOPPED)
                && t.stop_reason.as_deref() != Some(NOT_RUN_BUDGET)
                && t.stop_reason.as_deref() != Some(NOT_RUN_STOPPED)
        }),
        valid_trials: counted(&TrialRow::is_valid),
        passed_trials: counted(&|t| t.is_valid() && t.passed),
        invalid_trials: counted(&|t| !t.is_valid()),
        infra_interrupted_trials: counted(&|t| t.infra_interrupted),
        not_run_trials: assembled.not_run,
        spent_micro_usd: micro(record.run.totals.cost_usd),
        budget_micro_usd: micro(budget_usd),
        stopped,
    }
}

#[cfg(test)]
pub(crate) fn harbor_json(
    task: &str,
    trial: &str,
    reward: Option<f64>,
    exception: Option<&str>,
) -> String {
    let mut value = serde_json::json!({
        "task_name": task,
        "trial_name": trial,
        "started_at": "2026-10-20T01:00:00Z",
        "finished_at": "2026-10-20T01:02:30Z",
        "agent_result": {"n_input_tokens": 100, "n_cache_tokens": 40, "n_output_tokens": 7, "cost_usd": 0.5},
    });
    if let Some(reward) = reward {
        value["verifier_result"] = serde_json::json!({"rewards": {"reward": reward}});
    }
    if let Some(exception) = exception {
        value["exception_info"] = serde_json::json!({
            "exception_type": exception, "exception_message": "m",
            "exception_traceback": "t", "occurred_at": "2026-10-20T01:01:00Z"});
    }
    value.to_string()
}

#[cfg(test)]
mod tests {
    use super::super::config::{Dataset, RunFile, fixtures};
    use super::*;

    fn resolved(text: &str) -> ResolvedRun {
        let dataset = Dataset::parse(fixtures::DATASET_TOML, fixtures::SPLIT_JSON).unwrap();
        RunFile::parse(text)
            .unwrap()
            .resolve(text, dataset)
            .unwrap()
    }

    fn bench_row(trial: &str, task: &str, attempt: u32) -> BenchTaskRow {
        BenchTaskRow {
            run_id: Uuid::nil(),
            trial: trial.into(),
            task: task.into(),
            attempt,
            session_id: Some(Uuid::from_u128(u128::from(attempt) + 100)),
            compose_project: trial.into(),
            container: format!("{trial}-main-1"),
            workdir: "/app".into(),
            user: None,
            agent_timeout_sec: 900,
            request_digest: "d".into(),
            state: BenchTaskState::Finished,
            stop_reason: Some("completed".into()),
            cost_usd: Some(0.0123),
            prompt_tokens: Some(1000),
            cached_tokens: Some(900),
            output_tokens: Some(50),
            reasoning_tokens: Some(5),
            wall_ms: Some(61_000),
            harness_build_digest: Some("sha256:build".into()),
            trace_path: None,
            trace_sha256: Some("sha256:trace".into()),
            route_verdict: Some("valid".into()),
            route_violations: Vec::new(),
            started_at: Utc::now(),
            finished_at: Some(Utc::now()),
        }
    }

    fn inputs<'a>(
        resolved: &'a ResolvedRun,
        harbor: Vec<HarborTrial>,
        rows: Vec<BenchTaskRow>,
        stop: Option<StopNote>,
        trace: &'a dyn Fn(&BenchTaskRow) -> Option<String>,
    ) -> RecordInputs<'a> {
        let calls = rows
            .iter()
            .filter_map(|r| r.session_id.map(|id| (id, 12)))
            .collect();
        RecordInputs {
            resolved,
            host: "ssh://arch-laptop",
            concurrency: 4,
            adapter_tree_hash: "sha256:adapter",
            started_at: Utc::now(),
            finished_at: Utc::now(),
            harbor_trials: harbor,
            bench_tasks: rows,
            calls,
            stop,
            trace_text: trace,
            provenance: BTreeMap::new(),
            overlay: None,
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_harbor_result_parses_reward_exception_tokens_and_wall_time() {
        let trial =
            HarborTrial::parse(&harbor_json("fix-git", "fix-git__a", Some(1.0), None)).unwrap();
        assert_eq!(trial.reward, Some(1.0));
        assert_eq!(trial.n_cache_tokens, Some(40));
        assert_eq!(trial.wall_ms, Some(150_000));
        let failed = HarborTrial::parse(&harbor_json(
            "fix-git",
            "fix-git__b",
            None,
            Some("RsiBenchRouteError"),
        ))
        .unwrap();
        assert_eq!(failed.reward, None);
        assert_eq!(failed.exception_type.as_deref(), Some("RsiBenchRouteError"));
        // A later Harbor that records the verdict itself wins.
        let explicit = HarborTrial::parse(
            r#"{"task_name":"t","trial_name":"t__1","reward_valid":false,"failure_cause":"verifier_error"}"#,
        )
        .unwrap();
        assert_eq!(explicit.reward_valid, Some(false));
        assert_eq!(explicit.failure_cause.as_deref(), Some("verifier_error"));
        assert!(HarborTrial::parse("{}").is_err());
        assert!(HarborTrial::parse("nope").is_err());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_fixture_harbor_tree_is_read_trial_by_trial() {
        let dir = tempfile::tempdir().unwrap();
        let job = dir.path().join("search-01/2026-10-20__01-00-00");
        for (trial, task) in [("fix-git__aaa", "fix-git"), ("regex-log__bbb", "regex-log")] {
            let t = job.join(trial);
            std::fs::create_dir_all(&t).unwrap();
            std::fs::write(
                t.join("result.json"),
                harbor_json(task, trial, Some(1.0), None),
            )
            .unwrap();
        }
        // The job-level result and a broken trial file are not trials.
        std::fs::write(job.join("result.json"), "{}").unwrap();
        let broken = job.join("broken__ccc");
        std::fs::create_dir_all(&broken).unwrap();
        std::fs::write(broken.join("result.json"), "not json").unwrap();
        let (trials, notes) = read_harbor_trials(dir.path()).unwrap();
        let names: Vec<_> = trials.iter().map(|t| t.trial_name.as_str()).collect();
        assert_eq!(names, ["fix-git__aaa", "regex-log__bbb"]);
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("broken__ccc"));
        assert!(
            read_harbor_trials(&dir.path().join("absent"))
                .unwrap()
                .0
                .is_empty()
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_join_takes_money_and_route_from_the_daemon_and_the_verdict_from_harbor() {
        let run = resolved(&fixtures::rsi_config());
        let harbor = vec![
            HarborTrial::parse(&harbor_json("fix-git", "fix-git__a", Some(1.0), None)).unwrap(),
            HarborTrial::parse(&harbor_json(
                "big-one",
                "big-one__b",
                Some(0.0),
                Some("AgentTimeoutError"),
            ))
            .unwrap(),
        ];
        let rows = vec![
            bench_row("fix-git__a", "fix-git", 1),
            bench_row("big-one__b", "big-one", 1),
        ];
        let trace = |_: &BenchTaskRow| {
            Some("curl https://github.com/laude-institute/terminal-bench".to_string())
        };
        let assembled = assemble(inputs(&run, harbor, rows, None, &trace));
        let record = &assembled.record;
        assert_eq!(record.trials.len(), 2);
        let fix = record.trials.iter().find(|t| t.task == "fix-git").unwrap();
        assert!(fix.passed && fix.reward_valid);
        // The daemon's increments, not Harbor's cost or tokens.
        assert!((fix.cost_usd - 0.0123).abs() < 1e-12);
        assert_eq!(
            (fix.prompt_tokens, fix.cached_tokens, fix.output_tokens),
            (1000, 900, 50)
        );
        assert_eq!(fix.reasoning_tokens, 5);
        assert_eq!(fix.turns, 12);
        assert_eq!(fix.wall_ms, 61_000);
        assert_eq!(fix.image_digest, run.task("fix-git").unwrap().image_digest);
        assert_eq!(fix.split, "search");
        // The scan flagged the lookup; the hash now covers the text it read.
        assert!(fix.suspected_solution_lookup);
        assert!(fix.trace_sha256.as_deref().unwrap().starts_with("sha256:"));
        // An agent timeout is graded normally: a valid failure.
        let big = record.trials.iter().find(|t| t.task == "big-one").unwrap();
        assert!(big.reward_valid && !big.passed);
        assert_eq!(big.failure_cause.as_deref(), Some("AgentTimeoutError"));
        assert_eq!(record.run.model, "deepseek/deepseek-v4.1-flash");
        assert_eq!(record.run.route, "harness");
        assert_eq!(record.run.harness_build_digest, "sha256:build");
        assert_eq!(record.run.k, 1);
        assert_eq!(record.run.timeouts.agent_sec, 1200);
        assert_eq!(record.run.timeouts.verifier_sec, 1200);
        assert_eq!(record.run.totals.trials, 2);
        assert_eq!(record.run.totals.passed_trials, 1);
        assert!(
            record.validate().is_ok(),
            "the record is sealed and well-formed"
        );
        assert_eq!(assembled.not_run, 0);
    }

    /// #3191: a run with an overlay names it in its sampling, so its fingerprint differs
    /// from the plain run's; a run with no overlay (or an empty one) adds nothing, so its
    /// record and fingerprint are the ones a run had before overlays existed.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn an_overlay_is_part_of_the_fingerprint_and_no_overlay_adds_nothing() {
        let run = resolved(&fixtures::rsi_config());
        let trace = |_: &BenchTaskRow| None;
        let record_with = |overlay: Option<Value>| {
            let harbor = vec![
                HarborTrial::parse(&harbor_json("fix-git", "fix-git__a", Some(1.0), None)).unwrap(),
            ];
            let rows = vec![bench_row("fix-git__a", "fix-git", 1)];
            let mut given = inputs(&run, harbor, rows, None, &trace);
            given.overlay = overlay;
            assemble(given).record
        };
        let plain = record_with(None);
        assert!(!plain.run.sampling.contains_key("overlay"));
        let overlay = |digest: &str, file: &str| serde_json::json!({"digest": digest, "files": {"instructions.md": file}});
        let (a, a_again, b, c) = (
            record_with(Some(overlay("sha256:a", "sha256:f1"))),
            record_with(Some(overlay("sha256:a", "sha256:f1"))),
            record_with(Some(overlay("sha256:b", "sha256:f1"))),
            record_with(Some(overlay("sha256:a", "sha256:f2"))),
        );
        assert_eq!(
            a.run.sampling.get("overlay"),
            Some(&overlay("sha256:a", "sha256:f1"))
        );
        assert_eq!(
            a.run.observation_fingerprint,
            a_again.run.observation_fingerprint
        );
        for other in [&plain, &b, &c] {
            assert_ne!(
                a.run.observation_fingerprint,
                other.run.observation_fingerprint
            );
        }
        assert_ne!(b.run.observation_fingerprint, c.run.observation_fingerprint);
        assert_eq!(
            plain.run.observation_fingerprint,
            record_with(None).run.observation_fingerprint
        );
        assert!(a.validate().is_ok() && plain.validate().is_ok());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn route_spend_and_infra_exceptions_make_a_trial_invalid_not_a_failure() {
        let run = resolved(&fixtures::rsi_config());
        let mut interrupted = bench_row("fix-git__a", "fix-git", 1);
        interrupted.state = BenchTaskState::InfraInterrupted;
        let mut bad_route = bench_row("big-one__b", "big-one", 1);
        bad_route.route_verdict = Some("invalid_route".into());
        let harbor = vec![
            HarborTrial::parse(&harbor_json(
                "fix-git",
                "fix-git__a",
                None,
                Some("RsiBenchInfraInterrupted"),
            ))
            .unwrap(),
            HarborTrial::parse(&harbor_json("big-one", "big-one__b", Some(1.0), None)).unwrap(),
        ];
        let none = |_: &BenchTaskRow| None;
        let assembled = assemble(inputs(
            &run,
            harbor,
            vec![interrupted, bad_route],
            None,
            &none,
        ));
        let by = |task: &str| {
            assembled
                .record
                .trials
                .iter()
                .find(|t| t.task == task)
                .unwrap()
        };
        assert!(by("fix-git").infra_interrupted && !by("fix-git").reward_valid);
        assert_eq!(
            by("fix-git").failure_cause.as_deref(),
            Some("infra_interrupted")
        );
        // A passing reward on an invalid route never counts.
        assert!(
            by("big-one").invalid_route && !by("big-one").passed && !by("big-one").reward_valid
        );
        assert_eq!(assembled.record.run.totals.valid_trials, 0);
        assert_eq!(assembled.record.run.totals.invalid_trials, 2);
        // Without a daemon row the Harbor exception alone decides.
        let spend = vec![
            HarborTrial::parse(&harbor_json(
                "fix-git",
                "fix-git__z",
                Some(1.0),
                Some("RsiBenchSpendCapError"),
            ))
            .unwrap(),
        ];
        let assembled = assemble(inputs(&run, spend, Vec::new(), None, &none));
        assert!(!assembled.record.trials[0].reward_valid);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_run_that_stopped_records_the_trials_it_never_ran_as_invalid_rows() {
        let mut text = fixtures::rsi_config();
        text = text.replace("k = 1", "k = 2");
        let run = resolved(&text);
        let harbor = vec![
            HarborTrial::parse(&harbor_json("fix-git", "fix-git__a", Some(1.0), None)).unwrap(),
            HarborTrial::parse(&harbor_json("fix-git", "fix-git__b", Some(0.0), None)).unwrap(),
        ];
        let none = |_: &BenchTaskRow| None;
        let stop = StopNote {
            reason: "budget_exhausted".into(),
            cause: NOT_RUN_BUDGET,
        };
        let assembled = assemble(inputs(&run, harbor, Vec::new(), Some(stop), &none));
        // fix-git ran both attempts; big-one (6 GB shard) never started: 2 slots.
        assert_eq!(assembled.not_run, 2);
        let unrun: Vec<_> = assembled
            .record
            .trials
            .iter()
            .filter(|t| t.task == "big-one")
            .collect();
        assert_eq!(unrun.len(), 2);
        assert_eq!(unrun.iter().map(|t| t.attempt).collect::<Vec<_>>(), [1, 2]);
        for t in unrun {
            assert!(!t.reward_valid && !t.passed && !t.is_valid());
            assert_eq!(t.failure_cause.as_deref(), Some(NOT_RUN_BUDGET));
            assert_eq!(t.stop_reason.as_deref(), Some(NOT_RUN_BUDGET));
            assert_eq!(t.cost_usd, 0.0);
        }
        // The sealed record still validates and counts the stop as invalid trials.
        assert!(assembled.record.validate().is_ok());
        assert_eq!(assembled.record.run.totals.invalid_trials, 2);
        assert_eq!(assembled.record.run.totals.valid_trials, 2);
        let summary = summarize(
            Uuid::nil(),
            "eval/bench/tb2/runs/x.toml",
            &run,
            &assembled,
            vec![BenchShardResultV1 {
                shard: "search-01".into(),
                status: "ok".into(),
                trials: 2,
            }],
            Some("/r/record.json".into()),
            6.0,
            Some("budget_exhausted".into()),
        );
        assert_eq!(summary.expected_trials, 4);
        assert_eq!(summary.trials, 2);
        assert_eq!(summary.not_run_trials, 2);
        assert_eq!(summary.valid_trials, 2);
        assert_eq!(summary.passed_trials, 1);
        assert_eq!(summary.invalid_trials, 2);
        assert_eq!(summary.budget_micro_usd, 6_000_000);
        assert_eq!(summary.stopped.as_deref(), Some("budget_exhausted"));
        assert_eq!(
            summary.observation_fingerprint.as_deref(),
            Some(assembled.record.run.observation_fingerprint.as_str())
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn an_oracle_run_records_no_model_and_no_route() {
        let run = resolved(&fixtures::oracle_config());
        let harbor = vec![
            HarborTrial::parse(&harbor_json("fix-git", "fix-git__o", Some(1.0), None)).unwrap(),
        ];
        let none = |_: &BenchTaskRow| None;
        let assembled = assemble(inputs(&run, harbor, Vec::new(), None, &none));
        let record = &assembled.record;
        assert_eq!(record.run.model, "oracle");
        assert_eq!(record.run.route, "oracle");
        assert_eq!(record.run.effort, "none");
        assert_eq!(record.run.harness_build_digest, "none");
        // Harbor's own cost and tokens stand in when the daemon saw no session.
        assert!((record.trials[0].cost_usd - 0.5).abs() < 1e-12);
        assert_eq!(record.trials[0].prompt_tokens, 100);
        assert!(record.trials[0].passed);
        // The record round-trips through JSON and keeps its fingerprint.
        let text = record.to_json().unwrap();
        assert_eq!(&BenchRunRecord::from_json(&text).unwrap(), record);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    fn effective(tag: &str) -> EffectiveInputs {
        EffectiveInputs {
            loop_knobs: format!("{tag}-loop"),
            overlay: format!("{tag}-overlay"),
            policy: format!("{tag}-policy"),
            prompt: format!("{tag}-prompt"),
            tool_spec: format!("{tag}-tools"),
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn each_trial_records_the_build_and_harness_its_session_ran_on() {
        // #3192: a trial carries its own session's build SHA, harness source digest and
        // harness_version_hash; the run takes the first trial session's, so a redeploy that keeps
        // the harness source stays one valid record, and one that changes it does not.
        let run = resolved(&fixtures::rsi_config());
        let harbor = || {
            vec![
                HarborTrial::parse(&harbor_json("fix-git", "fix-git__a", Some(1.0), None)).unwrap(),
                HarborTrial::parse(&harbor_json("big-one", "big-one__b", Some(0.0), None)).unwrap(),
                HarborTrial::parse(&harbor_json("fix-git", "fix-git__c", Some(0.0), None)).unwrap(),
            ]
        };
        let (first, second, unknown) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
        let rows = || {
            let mut a = bench_row("fix-git__a", "fix-git", 1);
            a.session_id = Some(first);
            let mut b = bench_row("big-one__b", "big-one", 1);
            b.session_id = Some(second);
            let mut c = bench_row("fix-git__c", "fix-git", 2);
            c.session_id = Some(unknown);
            vec![a, b, c]
        };
        let provenance = |second_source: &str| {
            BTreeMap::from([
                (
                    first,
                    TrialProvenance {
                        build_sha: Some("a".repeat(40)),
                        harness_source_digest: Some("sha256:src".into()),
                        harness_version_hash: Some("hvh-first".into()),
                        effective_inputs: Some(effective("first")),
                    },
                ),
                (
                    second,
                    TrialProvenance {
                        build_sha: Some("b".repeat(40)),
                        harness_source_digest: Some(second_source.into()),
                        harness_version_hash: Some("hvh-second".into()),
                        effective_inputs: None,
                    },
                ),
            ])
        };
        let none = |_: &BenchTaskRow| None;
        let mut input = inputs(&run, harbor(), rows(), None, &none);
        input.provenance = provenance("sha256:src");
        let record = assemble(input).record;
        assert_eq!(record.run.build_sha, "a".repeat(40));
        assert_eq!(record.run.harness_source_digest, "sha256:src");
        assert_eq!(record.run.effective_inputs, effective("first"));
        let trial = |task: &str, attempt: u32| {
            record
                .trials
                .iter()
                .find(|t| t.task == task && t.attempt == attempt)
                .unwrap()
        };
        let (a, b, c) = (
            trial("fix-git", 1),
            trial("big-one", 1),
            trial("fix-git", 2),
        );
        assert_eq!(
            (a.build_sha.as_str(), a.harness_version_hash.as_str()),
            ("a".repeat(40).as_str(), "hvh-first")
        );
        // A redeploy mid-run: another build, the same harness source.
        assert_eq!(
            (b.build_sha.as_str(), b.harness_version_hash.as_str()),
            ("b".repeat(40).as_str(), "hvh-second")
        );
        // A session that wrote no digest row takes the run's build and source.
        assert_eq!(c.build_sha, record.run.build_sha);
        assert_eq!(c.harness_source_digest, "sha256:src");
        assert_eq!(c.harness_version_hash, "unknown");
        assert!(record.validate().is_ok(), "{:?}", record.validate());

        // A redeploy that changed the harness source is refused as a mixed record.
        let mut input = inputs(&run, harbor(), rows(), None, &none);
        input.provenance = provenance("sha256:other");
        let mixed = assemble(input).record;
        assert!(
            matches!(
                mixed.validate(),
                Err(rsi_common::bench_record::BenchError::MixedHarnessSource { .. })
            ),
            "{:?}",
            mixed.validate()
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn an_oracle_run_records_this_daemons_build_and_no_harness() {
        let run = resolved(&fixtures::oracle_config());
        let harbor = vec![
            HarborTrial::parse(&harbor_json("fix-git", "fix-git__o", Some(1.0), None)).unwrap(),
        ];
        let none = |_: &BenchTaskRow| None;
        let record = assemble(inputs(&run, harbor, Vec::new(), None, &none)).record;
        assert_eq!(record.run.build_sha, crate::daemon_info::BUILD_SHA);
        assert_eq!(
            record.run.harness_source_digest,
            crate::daemon_info::HARNESS_SOURCE_DIGEST
        );
        assert_eq!(record.run.effective_inputs, effective_none());
        let trial = &record.trials[0];
        assert_eq!(trial.build_sha, crate::daemon_info::BUILD_SHA);
        assert_eq!(
            trial.harness_source_digest,
            record.run.harness_source_digest
        );
        assert_eq!(trial.harness_version_hash, "none");
        assert!(record.validate().is_ok(), "{:?}", record.validate());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    fn effective_none() -> EffectiveInputs {
        EffectiveInputs {
            loop_knobs: "none".into(),
            overlay: "none".into(),
            policy: "none".into(),
            prompt: "none".into(),
            tool_spec: "none".into(),
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn provenance_reads_the_stored_digest_row() {
        // The persisted `bench_build_digest` health row: the stream data plus its kind.
        let row = serde_json::json!({
            "kind": "bench_build_digest",
            "harness_build_digest": "d".repeat(64),
            "harness_source_digest": "sha256:src",
            "rsid_build_sha": "f".repeat(40),
            "effective_inputs": {
                "loop_knobs": "first-loop", "overlay": "first-overlay", "policy": "first-policy",
                "prompt": "first-prompt", "tool_spec": "first-tools",
            },
            "inputs": {},
        });
        let prov = TrialProvenance::from_digest_row(Some(&row), Some("hvh".into()));
        assert_eq!(
            prov,
            TrialProvenance {
                build_sha: Some("f".repeat(40)),
                harness_source_digest: Some("sha256:src".into()),
                harness_version_hash: Some("hvh".into()),
                effective_inputs: Some(effective("first")),
            }
        );
        // No row and a blank hash leave every field to the run's fallback.
        assert_eq!(
            TrialProvenance::from_digest_row(None, Some("  ".into())),
            TrialProvenance::default()
        );
    }

    // ---- R3a: Rust runner outcomes -> v1 records ----

    use rsi_common::bench_record::{BenchError, check_comparable};
    use rsi_common::bench_runner::{
        CleanupStatus, CollectionStatus, PhaseTiming, TrialException, TrialIdentity,
    };

    fn manifest(tag: char) -> RunnerSourceManifest {
        RunnerSourceManifest::from_entries([
            ("crates/a.rs".to_string(), tag.to_string().repeat(64)),
            ("crates/b.rs".to_string(), "b".repeat(64)),
        ])
        .unwrap()
    }

    fn outcome(
        task: &str,
        slot: u32,
        retry: u32,
        mode: RunnerMode,
        reward: Option<f64>,
        kinds: &[TrialExceptionKind],
    ) -> TrialOutcomeV1 {
        let phase = |state, elapsed_ms| PhaseTiming {
            state,
            deadline_ms: Some(900_000),
            elapsed_ms,
        };
        TrialOutcomeV1 {
            schema_version: 1,
            identity: TrialIdentity {
                run_id: "r1".into(),
                task: task.into(),
                slot,
                retry,
            },
            mode,
            reward_map: reward
                .map(|r| BTreeMap::from([("reward".to_string(), r)]))
                .unwrap_or_default(),
            reward,
            // An agent timeout is graded; every other exception leaves the reward invalid.
            reward_valid: reward.is_some()
                && kinds.iter().all(|k| *k == TrialExceptionKind::AgentTimeout),
            oracle_exit_code: Some(0),
            verifier_exit_code: Some(0),
            exceptions: kinds
                .iter()
                .map(|kind| TrialException {
                    kind: *kind,
                    message: "m".into(),
                })
                .collect(),
            states: vec![
                TrialState::Prepared,
                TrialState::AgentSettled,
                TrialState::Verifying,
                TrialState::Terminal,
            ],
            phases: vec![
                phase(TrialState::AgentRunning, 40_000),
                phase(TrialState::Verifying, 5_000),
            ],
            collection: CollectionStatus {
                attempted: true,
                collected: true,
                error: None,
            },
            cleanup: CleanupStatus {
                attempted: true,
                cleaned: true,
                error: None,
            },
            model_sessions: 0,
        }
    }

    fn oracle(
        task: &str,
        slot: u32,
        reward: Option<f64>,
        kinds: &[TrialExceptionKind],
    ) -> TrialOutcomeV1 {
        outcome(task, slot, 0, RunnerMode::Oracle, reward, kinds)
    }

    fn runner_inputs<'a>(
        resolved: &'a ResolvedRun,
        evaluator: &'a RunnerSourceManifest,
        outcomes: Vec<TrialOutcomeV1>,
        rows: Vec<BenchTaskRow>,
        stop: Option<StopNote>,
        trace: &'a dyn Fn(&BenchTaskRow) -> Option<String>,
    ) -> RunnerRecordInputs<'a> {
        let calls = rows
            .iter()
            .filter_map(|r| r.session_id.map(|id| (id, 12)))
            .collect();
        RunnerRecordInputs {
            resolved,
            host: "ssh://arch-laptop",
            concurrency: 2,
            evaluator,
            started_at: Utc::now(),
            finished_at: Utc::now(),
            outcomes,
            bench_tasks: rows,
            calls,
            stop,
            trace_text: trace,
            provenance: BTreeMap::new(),
            overlay: None,
        }
    }

    fn runner_row(task: &str, slot: u32, retry: u32, attempt: u32, cost: f64) -> BenchTaskRow {
        let id = TrialIdentity {
            run_id: "r1".into(),
            task: task.into(),
            slot,
            retry,
        };
        let mut row = bench_row(&trial_key(&id), task, attempt);
        row.cost_usd = Some(cost);
        row
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn runner_rewards_require_confirmed_phase_and_daemon_settlement() {
        let run = resolved(&fixtures::rsi_config());
        let evaluator = manifest('a');
        let none = |_: &BenchTaskRow| None;
        let graded = outcome("fix-git", 0, 0, RunnerMode::Harness, Some(1.0), &[]);
        let settled = runner_row("fix-git", 0, 0, 1, 0.5);
        let assemble = |outcome, row: Option<BenchTaskRow>| {
            assemble_runner(runner_inputs(
                &run,
                &evaluator,
                vec![outcome],
                row.into_iter().collect(),
                None,
                &none,
            ))
            .record
            .trials[0]
                .clone()
        };
        assert!(assemble(graded.clone(), Some(settled.clone())).passed);
        assert!(!assemble(graded.clone(), None).reward_valid);
        for state in [BenchTaskState::Starting, BenchTaskState::Running] {
            let mut row = settled.clone();
            row.state = state;
            let trial = assemble(graded.clone(), Some(row));
            assert!(!trial.reward_valid && !trial.passed && !trial.is_valid());
            assert_eq!(trial.cost_usd, 0.5, "unsettled spend remains visible");
            assert_eq!(trial.failure_cause.as_deref(), Some("agent_settle_failed"));
        }
        let mut row = settled.clone();
        row.finished_at = None;
        assert!(!assemble(graded.clone(), Some(row)).reward_valid);
        let mut row = settled.clone();
        row.session_id = None;
        assert!(!assemble(graded.clone(), Some(row)).reward_valid);
        for state in [
            TrialState::AgentSettled,
            TrialState::Verifying,
            TrialState::Terminal,
        ] {
            let mut incomplete = graded.clone();
            incomplete.states.retain(|s| *s != state);
            assert!(!assemble(incomplete, Some(settled.clone())).reward_valid);
        }
        let mut unsettled = graded.clone();
        unsettled.exceptions.push(TrialException {
            kind: TrialExceptionKind::AgentSettleFailed,
            message: "still live".into(),
        });
        assert!(!assemble(unsettled, Some(settled.clone())).reward_valid);
        let mut timed_out = graded;
        timed_out.exceptions.push(TrialException {
            kind: TrialExceptionKind::AgentTimeout,
            message: "deadline".into(),
        });
        let mut cancelled = settled;
        cancelled.state = BenchTaskState::Cancelled;
        assert!(
            assemble(timed_out, Some(cancelled)).passed,
            "a settled agent timeout is graded"
        );
        assert!(assemble(oracle("fix-git", 0, Some(1.0), &[]), None).passed);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn oracle_outcomes_assemble_into_a_valid_v1_record_with_zero_usage() {
        let mut text = fixtures::oracle_config();
        text = text.replace("k = 1", "k = 3");
        let run = resolved(&text);
        let evaluator = manifest('a');
        let none = |_: &BenchTaskRow| None;
        let outcomes = vec![
            oracle("fix-git", 0, Some(1.0), &[]),
            // A graded agent timeout is a valid failure, with the stop reason kept.
            oracle("fix-git", 1, Some(0.0), &[TrialExceptionKind::AgentTimeout]),
            // No reward file: invalid, with the typed cause.
            oracle("fix-git", 2, None, &[TrialExceptionKind::RewardMissing]),
        ];
        let assembled = assemble_runner(runner_inputs(
            &run,
            &evaluator,
            outcomes,
            Vec::new(),
            None,
            &none,
        ));
        let record = &assembled.record;
        assert!(record.validate().is_ok(), "sealed and well-formed");
        assert_eq!(assembled.not_run, 0);
        assert_eq!(record.trials.len(), 3);
        assert_eq!(
            record.trials.iter().map(|t| t.attempt).collect::<Vec<_>>(),
            [1, 2, 3]
        );
        for t in &record.trials {
            // Zero oracle usage: no session, no tokens, no spend, no trace, no harness.
            assert_eq!(
                (
                    t.prompt_tokens,
                    t.cached_tokens,
                    t.output_tokens,
                    t.reasoning_tokens
                ),
                (0, 0, 0, 0)
            );
            assert_eq!((t.cost_usd, t.turns), (0.0, 0));
            assert_eq!(t.session_id, None);
            assert_eq!(t.trace_sha256, None);
            assert_eq!(t.harness_version_hash, "none");
            assert_eq!(t.wall_ms, 45_000, "the sum of the phase times");
            assert_eq!(t.image_digest, run.task("fix-git").unwrap().image_digest);
            assert_eq!(t.split, "search");
        }
        let [pass, timeout, missing] = &record.trials[..] else {
            panic!("three trials")
        };
        assert!(pass.passed && pass.reward_valid && pass.failure_cause.is_none());
        assert!(timeout.reward_valid && !timeout.passed);
        assert_eq!(timeout.failure_cause.as_deref(), Some("agent_timeout"));
        assert_eq!(timeout.stop_reason.as_deref(), Some("agent_timeout"));
        assert!(!missing.reward_valid && !missing.passed && !missing.is_valid());
        assert_eq!(missing.failure_cause.as_deref(), Some("reward_missing"));
        assert_eq!(missing.exception_type.as_deref(), Some("reward_missing"));
        let totals = &record.run.totals;
        assert_eq!(
            (
                totals.trials,
                totals.valid_trials,
                totals.passed_trials,
                totals.invalid_trials
            ),
            (3, 2, 1, 1)
        );
        assert_eq!(totals.cost_usd, 0.0);
        // The run block names the Rust evaluator, not Harbor.
        assert_eq!(record.run.harbor_version, RUNNER_VERSION_DISCRIMINATOR);
        assert_eq!(record.run.adapter_tree_hash, evaluator.evaluator_digest());
        assert_eq!(record.run.model, "oracle");
        assert_eq!(record.run.route, "oracle");
        assert_eq!(record.run.harness_build_digest, "none");
        assert_eq!(
            record.run.sampling.get("runner"),
            Some(&evaluator.sampling_runner())
        );
        assert_eq!(record.run.sampling["agent"], "oracle");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn an_oracle_outcome_that_reports_model_usage_is_invalid_and_keeps_its_cost() {
        let run = resolved(&fixtures::oracle_config());
        let evaluator = manifest('a');
        let none = |_: &BenchTaskRow| None;
        // A session count on an oracle is a defect, not a pass.
        let mut counted = oracle("fix-git", 0, Some(1.0), &[]);
        counted.model_sessions = 1;
        let record = assemble_runner(runner_inputs(
            &run,
            &evaluator,
            vec![counted],
            Vec::new(),
            None,
            &none,
        ))
        .record;
        let t = &record.trials[0];
        assert!(!t.reward_valid && !t.passed);
        assert_eq!(t.failure_cause.as_deref(), Some(ORACLE_MODEL_USAGE));
        // So is a daemon row; its spend stays visible in the totals.
        let record = assemble_runner(runner_inputs(
            &run,
            &evaluator,
            vec![oracle("fix-git", 0, Some(1.0), &[])],
            vec![runner_row("fix-git", 0, 0, 1, 0.5)],
            None,
            &none,
        ))
        .record;
        assert!(!record.trials[0].passed);
        assert_eq!(
            record.trials[0].failure_cause.as_deref(),
            Some(ORACLE_MODEL_USAGE)
        );
        assert!((record.run.totals.cost_usd - 0.5).abs() < 1e-12);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn an_interrupted_attempt_keeps_its_cost_and_does_not_fill_its_slot() {
        let run = resolved(&fixtures::rsi_config());
        let evaluator = manifest('a');
        let none = |_: &BenchTaskRow| None;
        let harness = |task: &str, slot, retry, reward, kinds: &[TrialExceptionKind]| {
            outcome(task, slot, retry, RunnerMode::Harness, reward, kinds)
        };
        let outcomes = vec![
            harness(
                "fix-git",
                0,
                0,
                None,
                &[TrialExceptionKind::InfraInterrupted],
            ),
            harness("fix-git", 0, 1, Some(1.0), &[]),
            harness("big-one", 0, 0, Some(0.0), &[]),
        ];
        let mut interrupted = runner_row("fix-git", 0, 0, 1, 0.2);
        interrupted.state = BenchTaskState::InfraInterrupted;
        let rows = vec![
            interrupted,
            runner_row("fix-git", 0, 1, 2, 0.3),
            runner_row("big-one", 0, 0, 3, 0.1),
        ];
        let assembled = assemble_runner(runner_inputs(
            &run,
            &evaluator,
            outcomes.clone(),
            rows.clone(),
            None,
            &none,
        ));
        let record = &assembled.record;
        assert_eq!(record.trials.len(), 3);
        let fix: Vec<_> = record
            .trials
            .iter()
            .filter(|t| t.task == "fix-git")
            .collect();
        assert_eq!(fix.iter().map(|t| t.attempt).collect::<Vec<_>>(), [1, 2]);
        assert!(fix[0].infra_interrupted && !fix[0].reward_valid && !fix[0].passed);
        assert_eq!(fix[0].failure_cause.as_deref(), Some("infra_interrupted"));
        assert!(
            (fix[0].cost_usd - 0.2).abs() < 1e-12,
            "the interrupted attempt's spend is kept"
        );
        assert!(fix[1].passed && !fix[1].infra_interrupted);
        // Daemon-authoritative usage and the session reach the row.
        assert_eq!(fix[1].prompt_tokens, 1000);
        assert!(fix[1].session_id.is_some());
        assert_eq!(fix[1].turns, 12);
        assert!(
            (record.run.totals.cost_usd - 0.6).abs() < 1e-12,
            "cost includes the interrupted attempt"
        );
        assert_eq!(record.run.totals.valid_trials, 2);
        assert_eq!(record.run.totals.invalid_trials, 1);
        assert_eq!(assembled.not_run, 0);
        assert!(record.validate().is_ok());

        // With only the interrupted attempt and a stop, the slot is still owed: one not-run row.
        let stop = StopNote {
            reason: "budget_exhausted".into(),
            cause: NOT_RUN_BUDGET,
        };
        let stopped = assemble_runner(runner_inputs(
            &run,
            &evaluator,
            vec![outcomes[0].clone(), outcomes[2].clone()],
            vec![rows[0].clone(), rows[2].clone()],
            Some(stop),
            &none,
        ));
        assert_eq!(stopped.not_run, 1);
        let fix: Vec<_> = stopped
            .record
            .trials
            .iter()
            .filter(|t| t.task == "fix-git")
            .collect();
        assert_eq!(fix.len(), 2);
        assert_eq!(fix[1].failure_cause.as_deref(), Some(NOT_RUN_BUDGET));
        assert!(!fix[1].reward_valid && fix[1].cost_usd == 0.0);
        assert!(stopped.record.validate().is_ok());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn route_and_spend_rejections_are_invalid_not_failures_and_never_infra() {
        let run = resolved(&fixtures::rsi_config());
        let evaluator = manifest('a');
        let none = |_: &BenchTaskRow| None;
        let mut route = outcome(
            "fix-git",
            0,
            0,
            RunnerMode::Harness,
            Some(1.0),
            &[TrialExceptionKind::RouteRejected],
        );
        // Even an outcome that claims a valid pass cannot count on an invalid route.
        route.reward_valid = true;
        let spend = outcome(
            "big-one",
            0,
            0,
            RunnerMode::Harness,
            None,
            &[TrialExceptionKind::SpendCapRejected],
        );
        let record = assemble_runner(runner_inputs(
            &run,
            &evaluator,
            vec![route, spend],
            Vec::new(),
            None,
            &none,
        ))
        .record;
        let by = |task: &str| record.trials.iter().find(|t| t.task == task).unwrap();
        assert!(
            by("fix-git").invalid_route && !by("fix-git").passed && !by("fix-git").reward_valid
        );
        assert_eq!(
            by("fix-git").failure_cause.as_deref(),
            Some(BENCH_INVALID_ROUTE)
        );
        assert!(!by("big-one").reward_valid && !by("big-one").infra_interrupted);
        assert_eq!(
            by("big-one").failure_cause.as_deref(),
            Some("spend_cap_rejected")
        );
        assert_eq!(record.run.totals.valid_trials, 0);
        assert_eq!(record.run.totals.invalid_trials, 2);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_run_that_stopped_records_unrun_runner_slots_as_invalid_rows() {
        let run = resolved(&fixtures::oracle_config().replace("k = 1", "k = 2"));
        let evaluator = manifest('a');
        let none = |_: &BenchTaskRow| None;
        let stop = StopNote {
            reason: "cancelled".into(),
            cause: NOT_RUN_STOPPED,
        };
        let assembled = assemble_runner(runner_inputs(
            &run,
            &evaluator,
            vec![oracle("fix-git", 0, Some(1.0), &[])],
            Vec::new(),
            Some(stop),
            &none,
        ));
        assert_eq!(assembled.not_run, 1);
        let unrun = &assembled.record.trials[1];
        assert_eq!(unrun.attempt, 2);
        assert_eq!(unrun.failure_cause.as_deref(), Some(NOT_RUN_STOPPED));
        assert!(!unrun.reward_valid && !unrun.is_valid());
        assert!(assembled.record.validate().is_ok());
        let summary = summarize(
            Uuid::nil(),
            "eval/bench/tb2/runs/x.toml",
            &run,
            &assembled,
            Vec::new(),
            None,
            0.0,
            Some("cancelled".into()),
        );
        assert_eq!(
            (
                summary.expected_trials,
                summary.trials,
                summary.not_run_trials
            ),
            (2, 1, 1)
        );
        assert_eq!((summary.valid_trials, summary.passed_trials), (1, 1));
        assert_eq!(summary.spent_micro_usd, 0);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn fingerprints_are_stable_and_follow_the_evaluator_identity() {
        let run = resolved(&fixtures::oracle_config());
        let none = |_: &BenchTaskRow| None;
        let record_with = |evaluator: &RunnerSourceManifest, host: &'static str, pass: bool| {
            let mut given = runner_inputs(
                &run,
                evaluator,
                vec![oracle(
                    "fix-git",
                    0,
                    Some(if pass { 1.0 } else { 0.0 }),
                    &[],
                )],
                Vec::new(),
                None,
                &none,
            );
            given.host = host;
            given.concurrency = 7;
            assemble_runner(given).record
        };
        let (first, same_evaluator) = (manifest('a'), manifest('a'));
        let a = record_with(&first, "ssh://arch-laptop", true);
        // Host, concurrency, timestamps and the observed results are not the observation domain.
        let b = record_with(&same_evaluator, "ssh://other", false);
        assert_eq!(a.run.observation_fingerprint, b.run.observation_fingerprint);
        assert_eq!(a.run.observation_fingerprint, a.computed_fingerprint());
        // One changed evaluator source changes the identity and the fingerprint.
        let changed = manifest('c');
        let c = record_with(&changed, "ssh://arch-laptop", true);
        assert_ne!(a.run.adapter_tree_hash, c.run.adapter_tree_hash);
        assert_ne!(a.run.observation_fingerprint, c.run.observation_fingerprint);
        assert!(a.validate().is_ok() && c.validate().is_ok());
        // Different evaluators are never one paired observation, and the refusal names the field.
        match check_comparable(&a, &c) {
            Err(BenchError::NotComparable { field, .. }) => assert_eq!(field, "adapter_tree_hash"),
            other => panic!("expected NotComparable, got {other:?}"),
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_legacy_harbor_record_and_a_rust_record_are_not_comparable() {
        let run = resolved(&fixtures::oracle_config());
        let none = |_: &BenchTaskRow| None;
        // The legacy reader still assembles a historical Harbor tree.
        let harbor = vec![
            HarborTrial::parse(&harbor_json("fix-git", "fix-git__a", Some(1.0), None)).unwrap(),
        ];
        let legacy = assemble(inputs(&run, harbor, Vec::new(), None, &none)).record;
        assert_eq!(legacy.run.harbor_version, "0.24.0");
        assert_eq!(legacy.run.adapter_tree_hash, "sha256:adapter");
        assert!(!legacy.run.sampling.contains_key("runner"));
        assert!(legacy.validate().is_ok());

        let evaluator = manifest('a');
        let rust = assemble_runner(runner_inputs(
            &run,
            &evaluator,
            vec![oracle("fix-git", 0, Some(1.0), &[])],
            Vec::new(),
            None,
            &none,
        ))
        .record;
        assert_ne!(
            legacy.run.observation_fingerprint,
            rust.run.observation_fingerprint
        );
        for (baseline, candidate) in [(&legacy, &rust), (&rust, &legacy)] {
            match check_comparable(baseline, candidate) {
                Err(BenchError::NotComparable { field, .. }) => {
                    assert_eq!(field, "harbor_version");
                }
                other => panic!("expected NotComparable, got {other:?}"),
            }
        }
    }
}
