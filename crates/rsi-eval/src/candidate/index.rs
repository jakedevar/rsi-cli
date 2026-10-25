//! `rsi-eval bench candidate index`: rebuild `index.json` from the candidate directories and the
//! records their `records.json` point at. Deterministic: no timestamps, sorted by id, and floats
//! rounded to a fixed number of places.

use std::collections::BTreeMap;
use std::path::PathBuf;

use rsi_common::bench_record::BenchRunRecord;
use serde::Serialize;

use super::verify::VerifyReport;
use super::{
    INDEX_FILE, NOTES_FILE, Repo, SCHEMA_VERSION, VERIFY_FILE, inputs_digest, instructions_name,
    load_overlay, notes_parent, parse_id, read_records, read_small_text, to_stable_json,
};
use crate::bench::load_record;
use crate::errors::EvalError;

/// What a record store tells about a candidate, from its pooled trials.
#[derive(Debug, Default)]
struct Pooled {
    /// Valid-trial pass counts per task: `(valid, passed)`.
    per_task: BTreeMap<String, (u32, u32)>,
    trials: u64,
    cost_usd: f64,
    invalid_route: u64,
    solution_lookup: u64,
    infra_interrupted: u64,
}

impl Pooled {
    fn add(&mut self, record: &BenchRunRecord) {
        for trial in &record.trials {
            self.trials += 1;
            self.cost_usd += trial.cost_usd;
            self.invalid_route += u64::from(trial.invalid_route);
            self.solution_lookup += u64::from(trial.suspected_solution_lookup);
            self.infra_interrupted += u64::from(trial.infra_interrupted);
            if trial.is_valid() {
                let entry = self.per_task.entry(trial.task.clone()).or_default();
                entry.0 += 1;
                entry.1 += u32::from(trial.passed);
            }
        }
    }

    fn fraction(&self, task: &str) -> Option<f64> {
        self.per_task
            .get(task)
            .map(|(valid, passed)| f64::from(*passed) / f64::from(*valid))
    }
}

/// The mean per-task pass-fraction difference over the tasks both sides scored.
fn gain(candidate: &Pooled, baseline: &Pooled) -> Option<f64> {
    let diffs: Vec<f64> = candidate
        .per_task
        .keys()
        .filter_map(|task| Some(candidate.fraction(task)? - baseline.fraction(task)?))
        .collect();
    #[allow(clippy::cast_precision_loss)]
    (!diffs.is_empty()).then(|| diffs.iter().sum::<f64>() / diffs.len() as f64)
}

fn round6(value: f64) -> f64 {
    (value * 1e6).round() / 1e6
}

#[derive(Debug, Serialize)]
struct Eligibility {
    /// `clean`, `failed`, `stale` (an input changed after `verify`) or `missing`.
    verify: &'static str,
    read_audit_ran: bool,
    records_loaded: usize,
    records_missing: usize,
    trials: Option<u64>,
    invalid_route_trials: Option<u64>,
    solution_lookup_trials: Option<u64>,
    infra_interrupted_trials: Option<u64>,
}

#[derive(Debug, Serialize)]
struct Entry {
    id: String,
    parent: Option<String>,
    /// `seed`, `proposed` (no record yet), `screened` (one record) or `extended` (more).
    status: &'static str,
    overlay_digest: Option<String>,
    records: Vec<String>,
    /// Mean per-task pass-rate difference from the seed's records, pooled over this candidate's
    /// records; absent until both have one.
    gain: Option<f64>,
    cost_per_trial_usd: Option<f64>,
    eligibility: Eligibility,
    /// Reserved for the selection step (#3196); never written by `index`.
    decision: Option<String>,
}

#[derive(Debug, Serialize)]
struct Index {
    schema_version: u32,
    candidates: Vec<Entry>,
}

struct Loaded {
    id: String,
    dir: PathBuf,
    parent: Option<String>,
    pointers: Vec<String>,
    pooled: Pooled,
    records_loaded: usize,
    records_missing: usize,
}

fn load(repo: &Repo, id: &str) -> Loaded {
    let dir = repo.dir(id);
    let parent = read_small_text(&dir.join(NOTES_FILE))
        .ok()
        .and_then(|notes| notes_parent(&notes))
        .filter(|parent| parent != "none");
    let pointers = read_records(&dir).unwrap_or_default();
    let mut pooled = Pooled::default();
    let (mut records_loaded, mut records_missing) = (0, 0);
    for pointer in &pointers {
        match load_record(&repo.root.join(pointer)) {
            Ok(record) => {
                pooled.add(&record);
                records_loaded += 1;
            }
            Err(_) => records_missing += 1,
        }
    }
    Loaded {
        id: id.to_string(),
        dir,
        parent,
        pointers,
        pooled,
        records_loaded,
        records_missing,
    }
}

fn verify_state(loaded: &Loaded) -> (&'static str, bool) {
    let Ok(text) = read_small_text(&loaded.dir.join(VERIFY_FILE)) else {
        return ("missing", false);
    };
    let Ok(report) = serde_json::from_str::<VerifyReport>(&text) else {
        return ("failed", false);
    };
    let current = inputs_digest(&loaded.dir, instructions_name(&loaded.dir).as_deref());
    let state = if report.candidate != loaded.id || report.inputs_sha256 != current {
        "stale"
    } else if report.ok {
        "clean"
    } else {
        "failed"
    };
    (state, report.read_audit_ran)
}

/// The seed of `id`'s lineage: follow `parent` until a candidate with none.
fn root_of<'a>(by_id: &'a BTreeMap<String, Loaded>, id: &str) -> Option<&'a Loaded> {
    let mut current = by_id.get(id)?;
    for _ in 0..=by_id.len() {
        match &current.parent {
            None => return Some(current),
            Some(parent) => current = by_id.get(parent)?,
        }
    }
    None
}

fn build(repo: &Repo) -> Result<Index, EvalError> {
    let by_id: BTreeMap<String, Loaded> = repo
        .candidate_ids()?
        .iter()
        .map(|id| (id.clone(), load(repo, id)))
        .collect();
    let mut candidates = Vec::new();
    for (id, loaded) in &by_id {
        let overlay_digest = load_overlay(&loaded.dir).ok().map(|o| o.digest());
        let is_seed = loaded.parent.is_none();
        let baseline = root_of(&by_id, id)
            .filter(|root| root.id != *id && root.records_loaded > 0)
            .map(|root| &root.pooled);
        let pooled = &loaded.pooled;
        let has_trials = loaded.records_loaded > 0;
        let (verify, read_audit_ran) = verify_state(loaded);
        #[allow(clippy::cast_precision_loss)]
        let cost =
            (has_trials && pooled.trials > 0).then(|| pooled.cost_usd / pooled.trials as f64);
        candidates.push(Entry {
            id: id.clone(),
            parent: loaded.parent.clone(),
            status: match loaded.pointers.len() {
                _ if is_seed && parse_id(id).is_some_and(|p| p.0 == 0) => "seed",
                0 => "proposed",
                1 => "screened",
                _ => "extended",
            },
            overlay_digest,
            records: loaded.pointers.clone(),
            gain: baseline.and_then(|b| gain(pooled, b)).map(round6),
            cost_per_trial_usd: cost.map(round6),
            eligibility: Eligibility {
                verify,
                read_audit_ran,
                records_loaded: loaded.records_loaded,
                records_missing: loaded.records_missing,
                trials: has_trials.then_some(pooled.trials),
                invalid_route_trials: has_trials.then_some(pooled.invalid_route),
                solution_lookup_trials: has_trials.then_some(pooled.solution_lookup),
                infra_interrupted_trials: has_trials.then_some(pooled.infra_interrupted),
            },
            decision: None,
        });
    }
    Ok(Index {
        schema_version: SCHEMA_VERSION,
        candidates,
    })
}

/// Writes `index.json` and returns its path.
pub(super) fn write_index(repo: &Repo) -> Result<PathBuf, EvalError> {
    let index = build(repo)?;
    let path = repo.candidates_dir().join(INDEX_FILE);
    std::fs::create_dir_all(repo.candidates_dir())?;
    std::fs::write(&path, to_stable_json(&index)?)?;
    Ok(path)
}
