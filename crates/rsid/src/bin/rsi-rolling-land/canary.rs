#![allow(unreachable_pub)]
#![allow(clippy::redundant_pub_crate)]
#![cfg_attr(test, allow(dead_code))]

use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

pub(super) const SCHEMA_VERSION: u8 = 1;

pub(super) fn default_root() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("RSI_ROLLING_CANARY_DIR") {
        return Ok(PathBuf::from(path));
    }
    dirs::home_dir()
        .map(|home| home.join(".rsi/cache/rolling-canary-v1"))
        .ok_or_else(|| "cannot locate home for rolling canary registry".into())
}

// ---------------------------------------------------------------------------
// CanaryRequest
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct CanaryRequest {
    pub schema_version: u8,
    pub published_tip: String,
    pub target: String,
    pub accepted: Vec<(String, String)>,
    pub test_filters: Vec<String>,
    pub source_repo: PathBuf,
    pub remote_url: String,
    pub enqueued_at: String,
}

pub(super) fn is_40_lower_hex(s: &str) -> bool {
    s.len() == 40
        && s.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

impl CanaryRequest {
    pub(super) fn new(
        published_tip: String,
        target: String,
        accepted: Vec<(String, String)>,
        test_filters: Vec<String>,
        source_repo: PathBuf,
        remote_url: String,
    ) -> Result<Self, String> {
        if !is_40_lower_hex(&published_tip) {
            return Err(format!("invalid published_tip: {published_tip}"));
        }
        if !is_40_lower_hex(&target) {
            return Err(format!("invalid target: {target}"));
        }
        Ok(Self {
            schema_version: SCHEMA_VERSION,
            published_tip,
            target,
            accepted,
            test_filters,
            source_repo,
            remote_url,
            enqueued_at: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
        })
    }
}

// ---------------------------------------------------------------------------
// Verdict and VerdictRecord
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(super) enum Verdict {
    Green {
        gate_base: String,
        gate_tip: String,
        covered_by: Option<String>,
    },
    Red {
        gate_base: String,
        gate_tip: String,
        error: String,
        forward_revert: Option<String>,
        revert_error: Option<String>,
    },
    /// The gate could not run (environment or setup error): no test result
    /// exists, so nothing is attributed to the landing and nothing is reverted
    /// (#1025).
    Unverified {
        gate_base: String,
        gate_tip: String,
        error: String,
    },
    /// The gate base was already red on the same check, so the tip could not
    /// be judged. Nothing is reverted (#1025).
    BaseRed {
        gate_base: String,
        gate_tip: String,
        error: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct VerdictRecord {
    pub schema_version: u8,
    pub tip: String,
    pub verdict: Verdict,
    pub recorded_at: String,
}

impl VerdictRecord {
    fn new(tip: &str, verdict: Verdict) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            tip: tip.into(),
            verdict,
            recorded_at: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
        }
    }
}

// ---------------------------------------------------------------------------
// BatchRecord
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct BatchRecord {
    pub schema_version: u8,
    pub gate_base: String,
    pub gate_tip: String,
    pub green: bool,
    pub covered: Vec<String>,
    pub recorded_at: String,
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub(super) struct Registry {
    pub root: PathBuf,
}

fn atomic_write_json<T: Serialize>(dir: &Path, filename: &str, value: &T) -> Result<(), String> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|error| {
            format!(
                "cannot create registry directory {}: {}",
                dir.display(),
                error
            )
        })?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)
        .map_err(|error| format!("cannot create temp in {}: {}", dir.display(), error))?;
    serde_json::to_writer(&mut tmp, value)
        .map_err(|error| format!("cannot serialize to temp: {error}"))?;
    tmp.as_file()
        .sync_all()
        .map_err(|error| format!("cannot sync temp: {error}"))?;
    let dest = dir.join(filename);
    tmp.persist(&dest)
        .map_err(|error| format!("cannot publish {}: {}", dest.display(), error.error))?;
    File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(|error| format!("cannot sync directory {}: {}", dir.display(), error))?;
    Ok(())
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>, String> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!("cannot read {}: {}", path.display(), error));
        }
    };
    serde_json::from_slice(&bytes)
        .map_err(|error| format!("cannot deserialize {}: {}", path.display(), error))
}

fn read_dir_json<T: for<'de> Deserialize<'de>>(dir: &Path) -> Result<Vec<T>, String> {
    let mut entries: Vec<T> = Vec::new();
    let dir_entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(entries),
        Err(error) => {
            return Err(format!(
                "cannot read directory {}: {}",
                dir.display(),
                error
            ));
        }
    };
    for entry in dir_entries {
        let entry = entry
            .map_err(|error| format!("cannot read dir entry in {}: {}", dir.display(), error))?;
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        if let Ok(Some(value)) = read_json::<T>(&path) {
            entries.push(value);
        }
    }
    Ok(entries)
}

impl Registry {
    pub(super) fn open(root: &Path) -> Result<Self, String> {
        let registry = Self {
            root: root.to_path_buf(),
        };
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(root)
            .map_err(|error| {
                format!("cannot create registry root {}: {}", root.display(), error)
            })?;
        for sub in &["pending", "verdicts", "batches"] {
            let subdir = root.join(sub);
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&subdir)
                .map_err(|error| {
                    format!(
                        "cannot create registry {} dir {}: {}",
                        sub,
                        subdir.display(),
                        error
                    )
                })?;
        }
        Ok(registry)
    }

    pub(super) fn enqueue(&self, request: &CanaryRequest) -> Result<(), String> {
        let pending_dir = self.root.join("pending");
        atomic_write_json(
            &pending_dir,
            &format!("{}.json", request.published_tip),
            request,
        )
    }

    pub(super) fn pending(&self) -> Result<Vec<CanaryRequest>, String> {
        let pending_dir = self.root.join("pending");
        let entries: Vec<CanaryRequest> = read_dir_json(&pending_dir)?;
        Ok(entries
            .into_iter()
            .filter(|r| {
                r.schema_version == SCHEMA_VERSION
                    && is_40_lower_hex(&r.published_tip)
                    && is_40_lower_hex(&r.target)
            })
            .collect())
    }

    pub(super) fn record_verdict(&self, tip: &str, verdict: Verdict) -> Result<(), String> {
        let verdicts_dir = self.root.join("verdicts");
        let record = VerdictRecord::new(tip, verdict);
        atomic_write_json(&verdicts_dir, &format!("{tip}.json"), &record)?;
        let pending_path = self.root.join("pending").join(format!("{tip}.json"));
        match fs::remove_file(&pending_path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!(
                "cannot remove pending {}: {}",
                pending_path.display(),
                error
            )),
        }
    }

    pub(super) fn verdict(&self, tip: &str) -> Result<Option<VerdictRecord>, String> {
        let path = self.root.join("verdicts").join(format!("{tip}.json"));
        read_json(&path)
    }

    pub(super) fn record_batch(&self, batch: &BatchRecord) -> Result<(), String> {
        let batches_dir = self.root.join("batches");
        atomic_write_json(&batches_dir, &format!("{}.json", batch.gate_tip), batch)
    }

    pub(super) fn green_batches(&self) -> Result<Vec<BatchRecord>, String> {
        let batches_dir = self.root.join("batches");
        let entries: Vec<BatchRecord> = read_dir_json(&batches_dir)?;
        Ok(entries.into_iter().filter(|b| b.green).collect())
    }

    pub(super) fn set_running(&self, base: &str, tip: &str) -> Result<(), String> {
        #[derive(Serialize)]
        struct Running {
            base: String,
            tip: String,
        }
        let running = Running {
            base: base.into(),
            tip: tip.into(),
        };
        atomic_write_json(&self.root, "running.json", &running)
    }

    pub(super) fn clear_running(&self) -> Result<(), String> {
        let path = self.root.join("running.json");
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!(
                "cannot remove running {}: {}",
                path.display(),
                error
            )),
        }
    }

    pub(super) fn try_runner_lock(&self) -> Result<Option<RunnerLock>, String> {
        let lock_path = self.root.join("runner.lock");
        let lock_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&lock_path)
            .map_err(|error| {
                format!("cannot open runner lock {}: {}", lock_path.display(), error)
            })?;
        match lock_file.try_lock() {
            Ok(()) => Ok(Some(RunnerLock { _file: lock_file })),
            Err(fs::TryLockError::WouldBlock) => Ok(None),
            Err(fs::TryLockError::Error(error)) => Err(format!(
                "cannot lock runner {}: {}",
                lock_path.display(),
                error
            )),
        }
    }

    pub(super) fn runner_lock_held(&self) -> Result<bool, String> {
        let lock_path = self.root.join("runner.lock");
        let lock_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&lock_path)
            .map_err(|error| {
                format!("cannot open runner lock {}: {}", lock_path.display(), error)
            })?;
        match lock_file.try_lock() {
            Ok(()) => Ok(false),
            Err(fs::TryLockError::WouldBlock) => Ok(true),
            Err(fs::TryLockError::Error(error)) => Err(format!(
                "cannot lock runner {}: {}",
                lock_path.display(),
                error
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// RunnerLock
// ---------------------------------------------------------------------------

pub(super) struct RunnerLock {
    _file: File,
}

// ---------------------------------------------------------------------------
// CanaryGate trait
// ---------------------------------------------------------------------------

/// Outcome of one canary gate that actually ran. An `Err` from `run_gate`
/// means the gate could not run (infrastructure: fetch, worktree, missing
/// repository); the runner then keeps the requests pending and reverts
/// nothing. Only `Red` attributes a failure to landings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum GateVerdict {
    Green,
    /// The candidate introduced a test failure relative to the base. The only
    /// verdict that may forward-revert a landing.
    Red(String),
    /// The gate ran into an environment or setup error before it produced a
    /// test result (a lander refusal, missing target dir, guard timeout, load
    /// gate). Parked as `unverified`; never reverted (#1025).
    Unverified(String),
    /// The base the tip is compared against was already red on the same check.
    /// Parked as `base_red`; never reverted (#1025).
    BaseRed(String),
}

#[allow(async_fn_in_trait)]
pub(super) trait CanaryGate {
    fn is_ancestor(&self, older: &str, newer: &str) -> Result<bool, String>;
    async fn refresh(&mut self) -> Result<(), String>;
    async fn run_gate(
        &mut self,
        base: &str,
        tip: &str,
        test_filters: &[String],
    ) -> Result<GateVerdict, String>;
    async fn forward_revert(&mut self, request: &CanaryRequest) -> Result<String, String>;
}

// ---------------------------------------------------------------------------
// Batch
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Batch {
    pub gate_base: String,
    pub gate_tip: String,
    pub test_filters: Vec<String>,
    pub requests: Vec<CanaryRequest>,
}

// ---------------------------------------------------------------------------
// plan_batch
// ---------------------------------------------------------------------------

#[allow(clippy::unnecessary_wraps)]
pub(super) fn plan_batch(
    pending: &[CanaryRequest],
    is_ancestor: impl Fn(&str, &str) -> Result<bool, String>,
) -> Result<Option<Batch>, String> {
    if pending.is_empty() {
        return Ok(None);
    }

    let tips: Vec<&str> = pending.iter().map(|r| r.published_tip.as_str()).collect();

    // Find the gate_tip: a tip that every other tip is an ancestor of.
    // If none, pick the tip with the most ancestors among pending.
    let gate_tip: &str = {
        let mut best_tip = tips[0];
        let mut best_count = 0usize;
        for &candidate in &tips {
            let count = tips
                .iter()
                .filter(|&&t| t == candidate || is_ancestor(t, candidate).unwrap_or(false))
                .count();
            if count == tips.len() {
                best_tip = candidate;
                break;
            }
            if count > best_count {
                best_count = count;
                best_tip = candidate;
            }
        }
        best_tip
    };

    // Collect requests whose tip is ancestor-or-equal of gate_tip, sorted oldest first.
    let requests: Vec<CanaryRequest> = pending
        .iter()
        .filter(|r| {
            r.published_tip == gate_tip || is_ancestor(&r.published_tip, gate_tip).unwrap_or(false)
        })
        .cloned()
        .collect();

    // Oldest first. The key counts how many batch tips are ancestors of a
    // tip, which is a total order (a pairwise ancestry comparator is not one
    // when history diverges, and `sort_by` may panic on such a comparator).
    let tip_depth = |tip: &str| {
        requests
            .iter()
            .filter(|r| {
                r.published_tip == tip || is_ancestor(&r.published_tip, tip).unwrap_or(false)
            })
            .count()
    };
    let mut keyed = requests
        .iter()
        .map(|r| (tip_depth(&r.published_tip), r.clone()))
        .collect::<Vec<_>>();
    keyed.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.1.published_tip.cmp(&b.1.published_tip))
    });
    let requests = keyed.into_iter().map(|(_, r)| r).collect::<Vec<_>>();

    // gate_base is the oldest target: the one with the fewest batch targets
    // at or below it.
    let target_depth = |target: &str| {
        requests
            .iter()
            .filter(|r| r.target == target || is_ancestor(&r.target, target).unwrap_or(false))
            .count()
    };
    let gate_base = requests
        .iter()
        .map(|r| (target_depth(&r.target), r.target.clone()))
        .min()
        .map(|(_, target)| target)
        .unwrap_or_default();

    // test_filters: common list if every batch request has an identical list, else empty
    let test_filters: Vec<String> = {
        let first = &requests[0].test_filters;
        let all_same = requests.iter().all(|r| &r.test_filters == first);
        if all_same { first.clone() } else { Vec::new() }
    };

    Ok(Some(Batch {
        gate_base,
        gate_tip: gate_tip.into(),
        test_filters,
        requests,
    }))
}

// ---------------------------------------------------------------------------
// covering_batch
// ---------------------------------------------------------------------------

pub(super) fn covering_batch<'a>(
    batches: &'a [BatchRecord],
    request: &CanaryRequest,
    is_ancestor: impl Fn(&str, &str) -> Result<bool, String>,
) -> Result<Option<&'a BatchRecord>, String> {
    for batch in batches {
        if !batch.green {
            continue;
        }
        if is_ancestor(&batch.gate_base, &request.target)?
            && is_ancestor(&request.published_tip, &batch.gate_tip)?
        {
            return Ok(Some(batch));
        }
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// RunSummary
// ---------------------------------------------------------------------------

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct RunSummary {
    pub gates_run: usize,
    pub verified: Vec<String>,
    pub covered: Vec<(String, String)>,
    pub red: Vec<String>,
    pub reverted: Vec<(String, String)>,
    /// `(tip, error)` parked because the gate could not run (#1025).
    pub unverified: Vec<(String, String)>,
    /// `(tip, error)` parked because the gate base was already red (#1025).
    pub base_red: Vec<(String, String)>,
    pub lock_busy: bool,
}

/// Record a non-judgement for `req` (no revert) and note it in the summary.
fn park_request(
    registry: &Registry,
    summary: &mut RunSummary,
    req: &CanaryRequest,
    verdict: &GateVerdict,
) -> Result<(), String> {
    let (record, error, parked) = match verdict {
        GateVerdict::Unverified(error) => (
            Verdict::Unverified {
                gate_base: req.target.clone(),
                gate_tip: req.published_tip.clone(),
                error: error.clone(),
            },
            error,
            &mut summary.unverified,
        ),
        GateVerdict::BaseRed(error) => (
            Verdict::BaseRed {
                gate_base: req.target.clone(),
                gate_tip: req.published_tip.clone(),
                error: error.clone(),
            },
            error,
            &mut summary.base_red,
        ),
        GateVerdict::Green | GateVerdict::Red(_) => {
            return Err("only an unverified or base_red gate parks a request".into());
        }
    };
    registry.record_verdict(&req.published_tip, record)?;
    parked.push((req.published_tip.clone(), error.clone()));
    Ok(())
}

// ---------------------------------------------------------------------------
// run_queue
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_lines)]
pub(super) async fn run_queue<G: CanaryGate>(
    registry: &Registry,
    gate: &mut G,
) -> Result<RunSummary, String> {
    let mut summary = RunSummary::default();

    loop {
        let Some(lock) = registry.try_runner_lock()? else {
            summary.lock_busy = true;
            return Ok(summary);
        };

        loop {
            let mut pending = registry.pending()?;
            if pending.is_empty() {
                break;
            }

            gate.refresh().await?;

            // Cover any request already covered by an existing green batch.
            let green_batches = registry.green_batches()?;
            for req in &pending {
                if let Some(batch) = covering_batch(&green_batches, req, |older, newer| {
                    gate.is_ancestor(older, newer)
                })? {
                    let covered_by = if req.published_tip == batch.gate_tip {
                        None
                    } else {
                        Some(batch.gate_tip.clone())
                    };
                    let verdict = Verdict::Green {
                        gate_base: batch.gate_base.clone(),
                        gate_tip: batch.gate_tip.clone(),
                        covered_by: covered_by.clone(),
                    };
                    registry.record_verdict(&req.published_tip, verdict)?;
                    summary
                        .covered
                        .push((req.published_tip.clone(), batch.gate_tip.clone()));
                }
            }

            // Re-read pending after coverage recording.
            pending = registry.pending()?;
            if pending.is_empty() {
                break;
            }

            let Some(batch) = plan_batch(&pending, |older, newer| gate.is_ancestor(older, newer))?
            else {
                break;
            };

            registry.set_running(&batch.gate_base, &batch.gate_tip)?;
            summary.gates_run += 1;

            // An infrastructure error propagates here: requests stay pending
            // and nothing is reverted.
            let result = gate
                .run_gate(&batch.gate_base, &batch.gate_tip, &batch.test_filters)
                .await?;

            match result {
                GateVerdict::Unverified(_) | GateVerdict::BaseRed(_) => {
                    // No test ran, or the base was already red: park every
                    // request in the batch without a revert. A per-landing
                    // fallback would only hit the same error.
                    for req in &batch.requests {
                        park_request(registry, &mut summary, req, &result)?;
                    }
                }
                GateVerdict::Green => {
                    // Every planned request is inside [gate_base, gate_tip] by
                    // construction. Re-read pending so late arrivals within
                    // the range are covered too.
                    let mut covered_tips: Vec<String> = batch
                        .requests
                        .iter()
                        .map(|r| r.published_tip.clone())
                        .collect();
                    for late in registry.pending()? {
                        if covered_tips.contains(&late.published_tip) {
                            continue;
                        }
                        let tip_inside = late.published_tip == batch.gate_tip
                            || gate.is_ancestor(&late.published_tip, &batch.gate_tip)?;
                        let base_inside = late.target == batch.gate_base
                            || gate.is_ancestor(&batch.gate_base, &late.target)?;
                        if tip_inside && base_inside {
                            covered_tips.push(late.published_tip);
                        }
                    }

                    let batch_record = BatchRecord {
                        schema_version: SCHEMA_VERSION,
                        gate_base: batch.gate_base.clone(),
                        gate_tip: batch.gate_tip.clone(),
                        green: true,
                        covered: covered_tips.clone(),
                        recorded_at: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
                    };
                    registry.record_batch(&batch_record)?;

                    for tip in &covered_tips {
                        let covered_by = if *tip == batch.gate_tip {
                            None
                        } else {
                            Some(batch.gate_tip.clone())
                        };
                        let verdict = Verdict::Green {
                            gate_base: batch.gate_base.clone(),
                            gate_tip: batch.gate_tip.clone(),
                            covered_by: covered_by.clone(),
                        };
                        registry.record_verdict(tip, verdict)?;
                        summary.verified.push(tip.clone());
                        if let Some(cb) = covered_by {
                            summary.covered.push((tip.clone(), cb));
                        }
                    }
                }
                GateVerdict::Red(error) => {
                    if batch.requests.len() == 1 {
                        // Single-request batch: forward_revert directly
                        let req = &batch.requests[0];
                        let (forward_revert, revert_error) = match gate.forward_revert(req).await {
                            Ok(id) => (Some(id), None),
                            Err(e) => (None, Some(e)),
                        };
                        let verdict = Verdict::Red {
                            gate_base: req.target.clone(),
                            gate_tip: req.published_tip.clone(),
                            error: error.clone(),
                            forward_revert: forward_revert.clone(),
                            revert_error: revert_error.clone(),
                        };
                        registry.record_verdict(&req.published_tip, verdict)?;
                        summary.red.push(req.published_tip.clone());
                        if let Some(id) = forward_revert {
                            summary.reverted.push((req.published_tip.clone(), id));
                        }
                    } else {
                        // Multi-request batch: record red batch, then per-landing
                        let batch_record = BatchRecord {
                            schema_version: SCHEMA_VERSION,
                            gate_base: batch.gate_base.clone(),
                            gate_tip: batch.gate_tip.clone(),
                            green: false,
                            covered: Vec::new(),
                            recorded_at: Utc::now()
                                .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
                        };
                        registry.record_batch(&batch_record)?;

                        // Per-landing canaries, oldest first
                        for req in &batch.requests {
                            summary.gates_run += 1;
                            let per_result = gate
                                .run_gate(&req.target, &req.published_tip, &req.test_filters)
                                .await?;
                            match per_result {
                                GateVerdict::Unverified(_) | GateVerdict::BaseRed(_) => {
                                    park_request(registry, &mut summary, req, &per_result)?;
                                }
                                GateVerdict::Green => {
                                    let verdict = Verdict::Green {
                                        gate_base: req.target.clone(),
                                        gate_tip: req.published_tip.clone(),
                                        covered_by: None,
                                    };
                                    registry.record_verdict(&req.published_tip, verdict)?;
                                    summary.verified.push(req.published_tip.clone());
                                }
                                GateVerdict::Red(per_error) => {
                                    let (forward_revert, revert_error) =
                                        match gate.forward_revert(req).await {
                                            Ok(id) => (Some(id), None),
                                            Err(e) => (None, Some(e)),
                                        };
                                    let verdict = Verdict::Red {
                                        gate_base: req.target.clone(),
                                        gate_tip: req.published_tip.clone(),
                                        error: per_error,
                                        forward_revert: forward_revert.clone(),
                                        revert_error: revert_error.clone(),
                                    };
                                    registry.record_verdict(&req.published_tip, verdict)?;
                                    summary.red.push(req.published_tip.clone());
                                    if let Some(id) = forward_revert {
                                        summary.reverted.push((req.published_tip.clone(), id));
                                    }
                                }
                            }
                        }
                    }
                }
            }

            registry.clear_running()?;
        }

        drop(lock);

        if registry.pending()?.is_empty() {
            return Ok(summary);
        }
    }
}

// ---------------------------------------------------------------------------
// runner_command
// ---------------------------------------------------------------------------

/// Build a detached runner command.
///
/// Phase 2 will append stdout+stderr to `root/runner.log`.
pub(super) fn runner_command(
    exe: &Path,
    root: &Path,
    wrapper: Option<&Path>,
) -> std::process::Command {
    let (program, args) = wrapper.map_or_else(
        || {
            let a: Vec<&std::ffi::OsStr> = vec![std::ffi::OsStr::new("--canary-runner")];
            (exe.to_path_buf(), a)
        },
        |w| {
            let a: Vec<&std::ffi::OsStr> =
                vec![exe.as_ref(), std::ffi::OsStr::new("--canary-runner")];
            (w.to_path_buf(), a)
        },
    );
    let mut cmd = std::process::Command::new(&program);
    cmd.args(&args);
    cmd.env("RSI_ROLLING_CANARY_DIR", root);
    cmd.env_remove("RSI_SESSION_TOKEN");
    cmd.stdin(std::process::Stdio::null());
    cmd
}

/// Build `systemd-run --user` for the runner, so it runs in its own unit
/// and survives the lander's service unit or session cgroup. Only an explicit
/// allowlist of environment reaches the unit; the session token never does.
pub(super) fn runner_systemd_command(
    exe: &Path,
    root: &Path,
    wrapper: Option<&Path>,
    unit: &str,
) -> std::process::Command {
    let mut cmd = std::process::Command::new("systemd-run");
    cmd.args(["--user", "--collect", "--quiet", "--unit", unit]);
    for key in [
        "PATH",
        "HOME",
        "RSI_LANDER_GUARD_TIMEOUT_SECS",
        "RSI_ROLLING_BASE_CACHE_DIR",
        "CARGO_HOME",
        "RUSTUP_HOME",
    ] {
        if let Some(value) = std::env::var_os(key) {
            let mut assignment = std::ffi::OsString::from(format!("{key}="));
            assignment.push(value);
            cmd.arg("-E").arg(assignment);
        }
    }
    let mut canary_dir = std::ffi::OsString::from("RSI_ROLLING_CANARY_DIR=");
    canary_dir.push(root);
    let mut target_dir = std::ffi::OsString::from("CARGO_TARGET_DIR=");
    target_dir.push(root.join("target"));
    cmd.arg("-E")
        .arg(canary_dir)
        .arg("-E")
        .arg(target_dir)
        .arg("--");
    if let Some(wrapper) = wrapper {
        cmd.arg(wrapper);
    }
    cmd.arg(exe).arg("--canary-runner");
    cmd.env_remove("RSI_SESSION_TOKEN");
    cmd.stdin(std::process::Stdio::null());
    cmd
}

pub(super) fn default_wrapper() -> Option<PathBuf> {
    let path = dirs::home_dir()?.join(".rsi/bin/cargo-slot");
    if path.is_file() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let meta = path.metadata().ok()?;
            let mode = meta.permissions().mode();
            if mode & 0o111 != 0 {
                return Some(path);
            }
        }
        #[cfg(not(unix))]
        {
            return Some(path);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(
    clippy::pedantic,
    clippy::unwrap_used,
    clippy::significant_drop_tightening,
    clippy::useless_vec
)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::Mutex;

    fn tip(n: u8) -> String {
        format!("{:0>40}", n)
    }

    struct FakeGate {
        pub history: Vec<String>,
        pub run_gate_calls: Mutex<Vec<(String, String, Vec<String>)>>,
        pub failing_gates: Mutex<HashSet<(String, String)>>,
        pub forced: Mutex<std::collections::HashMap<(String, String), GateVerdict>>,
        pub forward_revert_calls: Mutex<Vec<(CanaryRequest, String)>>,
        pub revert_results: Mutex<Vec<String>>,
        pub revert_call_count: Mutex<usize>,
    }

    impl FakeGate {
        fn new(history: Vec<&str>) -> Self {
            Self {
                history: history.into_iter().map(|s| s.to_string()).collect(),
                run_gate_calls: Mutex::new(Vec::new()),
                failing_gates: Mutex::new(HashSet::new()),
                forced: Mutex::new(std::collections::HashMap::new()),
                forward_revert_calls: Mutex::new(Vec::new()),
                revert_results: Mutex::new(Vec::new()),
                revert_call_count: Mutex::new(0),
            }
        }

        fn set_verdict(&self, base: &str, tip: &str, verdict: GateVerdict) {
            self.forced
                .lock()
                .unwrap()
                .insert((base.to_string(), tip.to_string()), verdict);
        }

        fn set_failing(&self, base: &str, tip: &str) {
            self.failing_gates
                .lock()
                .unwrap()
                .insert((base.to_string(), tip.to_string()));
        }
    }

    impl CanaryGate for FakeGate {
        fn is_ancestor(&self, older: &str, newer: &str) -> Result<bool, String> {
            let older_idx = self.history.iter().position(|h| h == older);
            let newer_idx = self.history.iter().position(|h| h == newer);
            match (older_idx, newer_idx) {
                (Some(oi), Some(ni)) => Ok(oi <= ni),
                _ => Ok(false),
            }
        }

        async fn refresh(&mut self) -> Result<(), String> {
            Ok(())
        }

        async fn run_gate(
            &mut self,
            base: &str,
            tip: &str,
            test_filters: &[String],
        ) -> Result<GateVerdict, String> {
            self.run_gate_calls.lock().unwrap().push((
                base.to_string(),
                tip.to_string(),
                test_filters.to_vec(),
            ));
            let key = (base.to_string(), tip.to_string());
            if let Some(verdict) = self.forced.lock().unwrap().get(&key) {
                return Ok(verdict.clone());
            }
            if self.failing_gates.lock().unwrap().contains(&key) {
                return Ok(GateVerdict::Red(format!("gate failed: {base}..{tip}")));
            }
            Ok(GateVerdict::Green)
        }

        async fn forward_revert(&mut self, request: &CanaryRequest) -> Result<String, String> {
            let mut count = self.revert_call_count.lock().unwrap();
            *count += 1;
            let revert_id = format!("revert-{}", *count);
            self.forward_revert_calls
                .lock()
                .unwrap()
                .push((request.clone(), revert_id.clone()));
            let results = self.revert_results.lock().unwrap();
            if *count <= results.len() {
                if results[*count - 1].is_empty() {
                    Err("revert simulated failure".into())
                } else {
                    Ok(results[*count - 1].clone())
                }
            } else {
                Ok(revert_id)
            }
        }
    }

    fn make_request(target: &str, published_tip: &str, test_filters: Vec<&str>) -> CanaryRequest {
        CanaryRequest {
            schema_version: SCHEMA_VERSION,
            published_tip: published_tip.to_string(),
            target: target.to_string(),
            accepted: Vec::new(),
            test_filters: test_filters.into_iter().map(|s| s.to_string()).collect(),
            source_repo: PathBuf::from("/tmp/test-repo"),
            remote_url: "file:///tmp/test-remote".to_string(),
            enqueued_at: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
        }
    }

    // --- plan_batch tests ---

    #[test]
    fn plan_batch_uses_common_filters_only_when_identical() {
        let c1 = tip(1);
        let c2 = tip(2);
        let p1 = tip(3);
        let p2 = tip(4);
        let history = vec![c1.as_str(), c2.as_str(), p1.as_str(), p2.as_str()];
        let history_clone = history.clone();
        let is_ancestor = |older: &str, newer: &str| -> Result<bool, String> {
            let oi = history_clone.iter().position(|h| *h == older);
            let ni = history_clone.iter().position(|h| *h == newer);
            match (oi, ni) {
                (Some(oi), Some(ni)) => Ok(oi <= ni),
                _ => Ok(false),
            }
        };

        // Identical filters: common filters used
        let r1 = make_request(&c2, &p1, vec!["--lib"]);
        let r2 = make_request(&p1, &p2, vec!["--lib"]);
        let batch = plan_batch(&[r1, r2], is_ancestor).unwrap().unwrap();
        assert_eq!(batch.test_filters, vec!["--lib".to_string()]);

        // Different filters: empty
        let r1 = make_request(&c2, &p1, vec!["--lib"]);
        let r2 = make_request(&p1, &p2, vec!["--test", "foo"]);
        let history_clone = history.clone();
        let is_ancestor2 = |older: &str, newer: &str| -> Result<bool, String> {
            let oi = history_clone.iter().position(|h| *h == older);
            let ni = history_clone.iter().position(|h| *h == newer);
            match (oi, ni) {
                (Some(oi), Some(ni)) => Ok(oi <= ni),
                _ => Ok(false),
            }
        };
        let batch = plan_batch(&[r1, r2], is_ancestor2).unwrap().unwrap();
        assert!(batch.test_filters.is_empty());
    }

    // --- run_queue tests ---

    #[tokio::test]
    async fn two_sequential_publications_run_one_canary_gate() {
        let tmp = tempfile::TempDir::new().unwrap();
        let registry = Registry::open(tmp.path()).unwrap();

        let c1 = tip(1);
        let p1 = tip(2);
        let p2 = tip(3);

        registry
            .enqueue(&make_request(&c1, &p1, vec!["--lib"]))
            .unwrap();
        registry
            .enqueue(&make_request(&p1, &p2, vec!["--lib"]))
            .unwrap();

        let history = vec![c1.as_str(), p1.as_str(), p2.as_str()];
        let mut gate = FakeGate::new(history);

        let summary = run_queue(&registry, &mut gate).await.unwrap();

        assert_eq!(summary.gates_run, 1);
        assert_eq!(summary.verified.len(), 2);
        assert!(summary.verified.contains(&p1));
        assert!(summary.verified.contains(&p2));
        assert_eq!(summary.red.len(), 0);

        let calls = gate.run_gate_calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, c1);
        assert_eq!(calls[0].1, p2);

        // Verdict(P1) Green covered_by Some(P2)
        let v1 = registry.verdict(&p1).unwrap().unwrap();
        match &v1.verdict {
            Verdict::Green { covered_by, .. } => {
                assert_eq!(covered_by.as_deref(), Some(p2.as_str()));
            }
            _ => panic!("expected Green verdict for P1"),
        }

        // Verdict(P2) Green covered_by None
        let v2 = registry.verdict(&p2).unwrap().unwrap();
        match &v2.verdict {
            Verdict::Green { covered_by, .. } => {
                assert_eq!(*covered_by, None);
            }
            _ => panic!("expected Green verdict for P2"),
        }

        // Pending empty
        assert!(registry.pending().unwrap().is_empty());
    }

    #[tokio::test]
    async fn existing_green_batch_covers_late_request() {
        let tmp = tempfile::TempDir::new().unwrap();
        let registry = Registry::open(tmp.path()).unwrap();

        let b = tip(1);
        let n = tip(2);
        let c = tip(1);
        let p = tip(2);

        // Pre-populate a green batch (B, N)
        let green_batch = BatchRecord {
            schema_version: SCHEMA_VERSION,
            gate_base: b.clone(),
            gate_tip: n.clone(),
            green: true,
            covered: vec![n.clone()],
            recorded_at: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
        };
        registry.record_batch(&green_batch).unwrap();

        // Enqueue (C, P) with B <= C < P <= N
        registry
            .enqueue(&make_request(&c, &p, vec!["--lib"]))
            .unwrap();

        let history = vec![b.as_str(), n.as_str()];
        let mut gate = FakeGate::new(history);

        let summary = run_queue(&registry, &mut gate).await.unwrap();

        // Zero gate calls
        assert_eq!(summary.gates_run, 0);
        assert_eq!(summary.covered.len(), 1);
        assert_eq!(summary.covered[0].0, p);
        assert_eq!(summary.covered[0].1, n);

        let calls = gate.run_gate_calls.lock().unwrap();
        assert!(calls.is_empty());

        // Pending empty
        assert!(registry.pending().unwrap().is_empty());
    }

    #[tokio::test]
    async fn red_batch_falls_back_to_per_landing_canaries() {
        let tmp = tempfile::TempDir::new().unwrap();
        let registry = Registry::open(tmp.path()).unwrap();

        let c1 = tip(1);
        let p1 = tip(2);
        let c2 = tip(3);
        let p2 = tip(4);

        let history = vec![c1.as_str(), p1.as_str(), c2.as_str(), p2.as_str()];

        registry
            .enqueue(&make_request(&c1, &p1, vec!["--lib"]))
            .unwrap();
        registry
            .enqueue(&make_request(&c2, &p2, vec!["--lib"]))
            .unwrap();

        let mut gate = FakeGate::new(history);
        // Batch gate (C1, P2) fails
        gate.set_failing(&c1, &p2);
        // Per-landing (C2, P2) also fails
        gate.set_failing(&c2, &p2);

        let summary = run_queue(&registry, &mut gate).await.unwrap();

        // 3 gate calls: batch (C1,P2) red, then per-landing (C1,P1), (C2,P2)
        let calls = gate.run_gate_calls.lock().unwrap();
        assert_eq!(calls.len(), 3);
        assert_eq!(
            calls[0],
            (c1.clone(), p2.clone(), vec!["--lib".to_string()])
        );
        assert_eq!(
            calls[1],
            (c1.clone(), p1.clone(), vec!["--lib".to_string()])
        );
        assert_eq!(
            calls[2],
            (c2.clone(), p2.clone(), vec!["--lib".to_string()])
        );

        // P1 green
        assert!(summary.verified.contains(&p1));
        // P2 red
        assert!(summary.red.contains(&p2));

        // forward_revert called once for P2
        assert_eq!(summary.reverted.len(), 1);
        assert_eq!(summary.reverted[0].0, p2);
    }

    #[tokio::test]
    async fn single_red_request_forward_reverts() {
        let tmp = tempfile::TempDir::new().unwrap();
        let registry = Registry::open(tmp.path()).unwrap();

        let c = tip(1);
        let p = tip(2);

        registry
            .enqueue(&make_request(&c, &p, vec!["--lib"]))
            .unwrap();

        let history = vec![c.as_str(), p.as_str()];
        let mut gate = FakeGate::new(history);
        gate.set_failing(&c, &p);

        let summary = run_queue(&registry, &mut gate).await.unwrap();

        assert_eq!(summary.gates_run, 1);
        assert!(summary.red.contains(&p));
        assert_eq!(summary.reverted.len(), 1);
        assert_eq!(summary.reverted[0].0, p);

        // Verify verdict is Red
        let v = registry.verdict(&p).unwrap().unwrap();
        match v.verdict {
            Verdict::Red {
                gate_base,
                gate_tip,
                error,
                forward_revert: Some(_),
                revert_error: None,
            } => {
                assert_eq!(gate_base, c);
                assert_eq!(gate_tip, p);
                assert!(!error.is_empty());
            }
            _ => panic!("expected Red verdict with forward_revert"),
        }
    }

    /// #1025: a gate that could not run (an environment error such as the
    /// lander's CARGO_TARGET_DIR refusal) parks the verdict as `unverified`
    /// and never forward-reverts.
    #[tokio::test]
    async fn environment_error_parks_unverified_without_revert() {
        let tmp = tempfile::TempDir::new().unwrap();
        let registry = Registry::open(tmp.path()).unwrap();
        let (c, p) = (tip(1), tip(2));
        registry
            .enqueue(&make_request(&c, &p, vec!["--lib"]))
            .unwrap();
        let mut gate = FakeGate::new(vec![c.as_str(), p.as_str()]);
        let env_error = "CARGO_TARGET_DIR must point to the current session's existing sandbox target directory";
        gate.set_verdict(&c, &p, GateVerdict::Unverified(env_error.into()));

        let summary = run_queue(&registry, &mut gate).await.unwrap();

        assert!(gate.forward_revert_calls.lock().unwrap().is_empty());
        assert!(summary.red.is_empty() && summary.reverted.is_empty());
        assert_eq!(summary.unverified, vec![(p.clone(), env_error.to_string())]);
        assert!(registry.pending().unwrap().is_empty());
        assert_eq!(
            registry.verdict(&p).unwrap().unwrap().verdict,
            Verdict::Unverified {
                gate_base: c,
                gate_tip: p,
                error: env_error.into(),
            }
        );
    }

    /// #1025: a tip whose gate base was already red is not judged: no revert,
    /// a `base_red` verdict.
    #[tokio::test]
    async fn red_base_parks_base_red_without_revert() {
        let tmp = tempfile::TempDir::new().unwrap();
        let registry = Registry::open(tmp.path()).unwrap();
        let (c, p) = (tip(1), tip(2));
        registry
            .enqueue(&make_request(&c, &p, vec!["--lib"]))
            .unwrap();
        let mut gate = FakeGate::new(vec![c.as_str(), p.as_str()]);
        gate.set_verdict(&c, &p, GateVerdict::BaseRed("static inventory red".into()));

        let summary = run_queue(&registry, &mut gate).await.unwrap();

        assert!(gate.forward_revert_calls.lock().unwrap().is_empty());
        assert!(summary.red.is_empty() && summary.reverted.is_empty());
        assert_eq!(summary.base_red.len(), 1);
        assert!(registry.pending().unwrap().is_empty());
        assert!(matches!(
            registry.verdict(&p).unwrap().unwrap().verdict,
            Verdict::BaseRed { .. }
        ));
    }

    /// #1025: a batch gate that could not run parks every request without the
    /// per-landing fallback or any revert; a later real red still reverts.
    #[tokio::test]
    async fn unverified_batch_parks_every_request_and_real_red_still_reverts() {
        let tmp = tempfile::TempDir::new().unwrap();
        let registry = Registry::open(tmp.path()).unwrap();
        let (c1, p1, p2) = (tip(1), tip(2), tip(3));
        registry
            .enqueue(&make_request(&c1, &p1, vec!["--lib"]))
            .unwrap();
        registry
            .enqueue(&make_request(&p1, &p2, vec!["--lib"]))
            .unwrap();
        let mut gate = FakeGate::new(vec![c1.as_str(), p1.as_str(), p2.as_str()]);
        gate.set_verdict(&c1, &p2, GateVerdict::Unverified("load gate".into()));

        let summary = run_queue(&registry, &mut gate).await.unwrap();
        assert_eq!(summary.gates_run, 1, "no per-landing fallback");
        assert_eq!(summary.unverified.len(), 2);
        assert!(gate.forward_revert_calls.lock().unwrap().is_empty());

        // The test red path is unchanged.
        let (d, q) = (tip(4), tip(5));
        registry
            .enqueue(&make_request(&d, &q, vec!["--lib"]))
            .unwrap();
        let mut gate = FakeGate::new(vec![d.as_str(), q.as_str()]);
        gate.set_failing(&d, &q);
        let summary = run_queue(&registry, &mut gate).await.unwrap();
        assert_eq!(summary.reverted.len(), 1);
    }

    #[tokio::test]
    async fn held_runner_lock_runs_no_gate() {
        let tmp = tempfile::TempDir::new().unwrap();
        let registry = Registry::open(tmp.path()).unwrap();

        registry
            .enqueue(&make_request(&tip(1), &tip(2), vec!["--lib"]))
            .unwrap();

        // Hold the lock before running
        let _lock = registry.try_runner_lock().unwrap().unwrap();

        let t1 = tip(1);
        let t2 = tip(2);
        let history = vec![t1.as_str(), t2.as_str()];
        let mut gate = FakeGate::new(history);

        let summary = run_queue(&registry, &mut gate).await.unwrap();

        assert!(summary.lock_busy);
        assert_eq!(summary.gates_run, 0);

        // Pending unchanged
        assert_eq!(registry.pending().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn refresh_error_leaves_requests_pending() {
        let tmp = tempfile::TempDir::new().unwrap();
        let registry = Registry::open(tmp.path()).unwrap();

        registry
            .enqueue(&make_request(&tip(1), &tip(2), vec!["--lib"]))
            .unwrap();

        struct RefreshFailGate;
        #[allow(unreachable_pub)]
        impl CanaryGate for RefreshFailGate {
            fn is_ancestor(&self, _older: &str, _newer: &str) -> Result<bool, String> {
                Ok(true)
            }
            async fn refresh(&mut self) -> Result<(), String> {
                Err("refresh failed".into())
            }
            async fn run_gate(
                &mut self,
                _base: &str,
                _tip: &str,
                _test_filters: &[String],
            ) -> Result<GateVerdict, String> {
                Ok(GateVerdict::Green)
            }
            async fn forward_revert(&mut self, _request: &CanaryRequest) -> Result<String, String> {
                Ok("revert-id".into())
            }
        }

        let mut gate = RefreshFailGate;
        let result = run_queue(&registry, &mut gate).await;
        assert!(result.is_err());

        // Request still pending
        assert_eq!(registry.pending().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn gate_infrastructure_error_keeps_requests_pending_without_revert() {
        let tmp = tempfile::TempDir::new().unwrap();
        let registry = Registry::open(tmp.path()).unwrap();
        let (c1, p1) = (tip(1), tip(2));
        registry
            .enqueue(&make_request(&c1, &p1, Vec::new()))
            .unwrap();

        struct InfraFailGate {
            reverts: usize,
        }
        impl CanaryGate for InfraFailGate {
            fn is_ancestor(&self, older: &str, newer: &str) -> Result<bool, String> {
                Ok(older <= newer)
            }
            async fn refresh(&mut self) -> Result<(), String> {
                Ok(())
            }
            async fn run_gate(
                &mut self,
                _base: &str,
                _tip: &str,
                _test_filters: &[String],
            ) -> Result<GateVerdict, String> {
                Err("cannot create canary worktree".into())
            }
            async fn forward_revert(&mut self, _request: &CanaryRequest) -> Result<String, String> {
                self.reverts += 1;
                Ok("revert-id".into())
            }
        }

        let mut gate = InfraFailGate { reverts: 0 };
        // An infrastructure failure must not settle the request.
        let result = run_queue(&registry, &mut gate).await;
        assert_eq!(result, Err("cannot create canary worktree".to_string()));
        assert_eq!(gate.reverts, 0);
        let pending = registry.pending().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].published_tip, p1);
    }

    #[test]
    fn runner_systemd_command_runs_its_own_unit_without_session_token() {
        let exe = PathBuf::from("/usr/bin/rsi-rolling-land");
        let root = PathBuf::from("/home/u/.rsi/cache/rolling-canary-v1");
        let wrapper = PathBuf::from("/home/u/.rsi/bin/cargo-slot");
        let cmd = runner_systemd_command(&exe, &root, Some(&wrapper), "rsi-canary-runner-1-2");
        assert_eq!(cmd.get_program(), "systemd-run");
        let args = cmd
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            &args[..5],
            [
                "--user",
                "--collect",
                "--quiet",
                "--unit",
                "rsi-canary-runner-1-2"
            ]
        );
        let Some(separator) = args.iter().position(|arg| arg == "--") else {
            panic!("systemd-run arguments end with a -- separator");
        };
        assert_eq!(
            &args[separator + 1..],
            [
                "/home/u/.rsi/bin/cargo-slot",
                "/usr/bin/rsi-rolling-land",
                "--canary-runner"
            ]
        );
        assert!(args.contains(&format!("RSI_ROLLING_CANARY_DIR={}", root.display())));
        assert!(args.contains(&format!(
            "CARGO_TARGET_DIR={}",
            root.join("target").display()
        )));
        let passed = args[..separator]
            .windows(2)
            .filter(|pair| pair[0] == "-E")
            .map(|pair| pair[1].split('=').next().unwrap_or_default().to_string())
            .collect::<Vec<_>>();
        for key in &passed {
            assert!(
                [
                    "PATH",
                    "HOME",
                    "RSI_LANDER_GUARD_TIMEOUT_SECS",
                    "RSI_ROLLING_BASE_CACHE_DIR",
                    "CARGO_HOME",
                    "RUSTUP_HOME",
                    "RSI_ROLLING_CANARY_DIR",
                    "CARGO_TARGET_DIR",
                ]
                .contains(&key.as_str()),
                "unexpected unit environment {key}"
            );
        }
    }

    #[test]
    fn runner_command_removes_session_token() {
        let exe = PathBuf::from("/usr/bin/rsi-rolling-land");
        let root = PathBuf::from("/tmp/canary-test");
        let cmd = runner_command(&exe, &root, None);

        // Check envs
        let envs: std::collections::HashMap<_, _> = cmd.get_envs().collect();
        assert_eq!(
            envs.get(std::ffi::OsStr::new("RSI_ROLLING_CANARY_DIR")),
            Some(&Some(std::ffi::OsStr::new(&root)))
        );
        assert_eq!(
            envs.get(std::ffi::OsStr::new("RSI_SESSION_TOKEN")),
            Some(&None)
        );
    }

    // --- covering_batch test ---

    #[test]
    fn covering_batch_matches_ancestry() {
        let b = tip(1);
        let n = tip(3);
        let c = tip(2);
        let p = tip(2);

        let batch = BatchRecord {
            schema_version: SCHEMA_VERSION,
            gate_base: b.clone(),
            gate_tip: n.clone(),
            green: true,
            covered: vec![n.clone()],
            recorded_at: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
        };

        let req = make_request(&c, &p, vec!["--lib"]);

        let history = vec![b.as_str(), c.as_str(), n.as_str()];
        let is_ancestor = |older: &str, newer: &str| -> Result<bool, String> {
            let oi = history.iter().position(|h| *h == older);
            let ni = history.iter().position(|h| *h == newer);
            match (oi, ni) {
                (Some(oi), Some(ni)) => Ok(oi <= ni),
                _ => Ok(false),
            }
        };

        let batches = [batch];
        let result = covering_batch(&batches, &req, is_ancestor).unwrap();
        assert!(result.is_some());
    }

    // --- default_root test ---

    #[test]
    fn default_root_uses_env_var() {
        unsafe { std::env::set_var("RSI_ROLLING_CANARY_DIR", "/custom/canary/dir") };
        let root = default_root().unwrap();
        assert_eq!(root, PathBuf::from("/custom/canary/dir"));
        unsafe { std::env::remove_var("RSI_ROLLING_CANARY_DIR") };
    }
}
