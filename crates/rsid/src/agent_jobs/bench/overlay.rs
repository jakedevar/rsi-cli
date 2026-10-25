//! #3191: the run overlay of a bench job: read it from the job's worktree,
//! resolve it, audit it for benchmark leaks and describe it.
//!
//! The overlay is a committed candidate file (`eval/bench/tb2/candidates/<id>/
//! overlay.toml`) and its optional instructions file. The schema and the limits
//! are `rsi_common::bench_overlay`; this module is the part that touches the
//! worktree. It reads only regular files that resolve inside the worktree (no
//! symlink), bounded, and never writes.

use rsi_common::bench_overlay::{
    BenchOverlay, OVERLAY_FILE_NAME, OVERLAY_INSTRUCTIONS_INVALID, OVERLAY_MAX_INSTRUCTIONS_BYTES,
    OVERLAY_PARSE_FAILED, OVERLAY_PATH_INVALID, OverlayError, RawOverlay, candidate_dir,
};
use std::io::Read;
use std::path::{Path, PathBuf};

/// The overlay file is a short TOML table; anything larger is not one.
const MAX_OVERLAY_TOML_BYTES: u64 = 64 * 1024;
/// The input pins must be committed at the admitted HEAD. The audit is compiled into rsid.
pub(crate) const AUDIT_INPUTS: [&str; 2] =
    ["eval/bench/tb2/dataset.toml", "eval/bench/tb2/split.json"];
/// A candidate's free-form notes: audited for held-out terms, never sent to a model.
const NOTES_FILE: &str = "notes.md";
/// The most audit output kept in a refusal.
const MAX_AUDIT_DETAIL_BYTES: usize = 2000;

/// A run's overlay as read from the worktree.
#[derive(Debug, Clone)]
pub(crate) struct LoadedOverlay {
    /// `None` when the overlay sets nothing: the run is the same as one with no overlay.
    pub overlay: Option<BenchOverlay>,
    /// Repo-relative files whose text reaches a model (the overlay, the instructions).
    pub model_files: Vec<String>,
    /// Repo-relative files audited for held-out content (the above, and `notes.md`).
    pub audited_files: Vec<String>,
}

fn read_file(cwd: &Path, relative: &str, limit: u64) -> Result<Vec<u8>, OverlayError> {
    let invalid =
        |why: String| OverlayError::new(OVERLAY_PATH_INVALID, format!("{relative}: {why}"));
    let path = cwd.join(relative);
    let meta = std::fs::symlink_metadata(&path).map_err(|e| invalid(e.to_string()))?;
    if !meta.is_file() {
        return Err(invalid("not a regular file (a symlink is refused)".into()));
    }
    let root = std::fs::canonicalize(cwd).map_err(|e| invalid(e.to_string()))?;
    let real = std::fs::canonicalize(&path).map_err(|e| invalid(e.to_string()))?;
    if !real.starts_with(&root) {
        return Err(invalid("resolves outside the worktree".into()));
    }
    let mut bytes = Vec::new();
    std::fs::File::open(&real)
        .and_then(|file| file.take(limit + 1).read_to_end(&mut bytes))
        .map_err(|e| invalid(e.to_string()))?;
    Ok(bytes)
}

/// Read, parse and resolve the overlay `overlay_path` names, under `cwd`.
///
/// # Errors
/// The overlay's typed refusal: a path outside its candidate directory, a missing
/// or oversized file, a malformed table, an unknown field, an out-of-range value, a
/// widened tool list or a reserved table.
pub(crate) fn load(cwd: &Path, overlay_path: &str) -> Result<LoadedOverlay, OverlayError> {
    let dir = candidate_dir(overlay_path).ok_or_else(|| {
        OverlayError::new(
            OVERLAY_PATH_INVALID,
            format!("{overlay_path}: not eval/bench/tb2/candidates/<id>/{OVERLAY_FILE_NAME}"),
        )
    })?;
    let bytes = read_file(cwd, overlay_path, MAX_OVERLAY_TOML_BYTES)?;
    if bytes.len() as u64 > MAX_OVERLAY_TOML_BYTES {
        return Err(OverlayError::new(
            OVERLAY_PARSE_FAILED,
            format!("{overlay_path}: larger than {MAX_OVERLAY_TOML_BYTES} bytes"),
        ));
    }
    let text = String::from_utf8(bytes).map_err(|_| {
        OverlayError::new(OVERLAY_PARSE_FAILED, format!("{overlay_path}: not UTF-8"))
    })?;
    let raw: RawOverlay = toml::from_str(&text).map_err(|e| {
        OverlayError::new(
            OVERLAY_PARSE_FAILED,
            format!("{overlay_path}: {}", e.to_string().replace('\n', " ")),
        )
    })?;
    let mut model_files = vec![overlay_path.to_string()];
    let instructions = match raw.instructions_path()? {
        None => None,
        Some(name) => {
            let relative = format!("{dir}{name}");
            let bytes = read_file(cwd, &relative, OVERLAY_MAX_INSTRUCTIONS_BYTES as u64).map_err(
                |error| OverlayError::new(OVERLAY_INSTRUCTIONS_INVALID, error.to_string()),
            )?;
            model_files.push(relative);
            Some(bytes)
        }
    };
    let overlay = raw.resolve(instructions.as_deref())?;
    let mut audited_files = model_files.clone();
    let notes = format!("{dir}{NOTES_FILE}");
    if std::fs::symlink_metadata(cwd.join(&notes)).is_ok_and(|meta| meta.is_file()) {
        audited_files.push(notes);
    }
    Ok(LoadedOverlay {
        overlay,
        model_files,
        audited_files,
    })
}

/// What the leak audit runs over, with the benchmark checkout when one is available.
#[derive(Debug, Clone)]
pub(crate) struct AuditPlan {
    pub model_files: Vec<String>,
    pub audited_files: Vec<String>,
    pub checkout: Option<PathBuf>,
}

fn run_audit(
    cwd: &Path,
    terms: &str,
    files: &[String],
    checkout: Option<&Path>,
) -> Result<(), String> {
    let mode = match terms {
        "held_out" => rsi_eval::tb2::audit::Terms::HeldOut,
        "search" => rsi_eval::tb2::audit::Terms::Search,
        _ => return Err("invalid audit terms".into()),
    };
    let paths: Vec<_> = files.iter().map(PathBuf::from).collect();
    let findings = rsi_eval::tb2::audit::audit_files(cwd, &paths, checkout, mode)
        .map_err(|e| format!("native leak audit failed: {e}"))?;
    if findings.is_empty() {
        return Ok(());
    }
    let detail = findings
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ");
    let mut cut = MAX_AUDIT_DETAIL_BYTES.min(detail.len());
    while !detail.is_char_boundary(cut) {
        cut -= 1;
    }
    Err(format!(
        "native leak audit --terms {terms}: {}",
        &detail[..cut]
    ))
}

/// Run the native leak audit in candidate mode (#3193): held-out content in every candidate
/// file, and search-split names in the files a model reads. Any non-zero exit, hit or
/// failure to run refuses (fail closed).
///
/// # Errors
/// The audit's hits or why it could not run.
pub(crate) fn audit(cwd: &Path, plan: &AuditPlan) -> Result<(), String> {
    run_audit(
        cwd,
        "held_out",
        &plan.audited_files,
        plan.checkout.as_deref(),
    )?;
    run_audit(cwd, "search", &plan.model_files, plan.checkout.as_deref())
}

/// The dry-run description of a resolved overlay (no model call): the effective
/// prompt digest, the tool list and the knobs a run would launch with.
pub(crate) fn describe(
    overlay: Option<&BenchOverlay>,
    config_knobs: rsi_common::bench_profile::LoopKnobs,
) -> serde_json::Value {
    use rsi_common::harness_tool_policy::HarnessToolPolicy;
    let policy = overlay.map_or_else(HarnessToolPolicy::bench, BenchOverlay::tool_policy);
    let knobs = overlay.map_or(config_knobs, |o| o.apply_knobs(config_knobs));
    let body = overlay.and_then(BenchOverlay::instructions_text);
    let tools: Vec<String> = policy
        .enabled_tools
        .iter()
        .flatten()
        .filter(|tool| !policy.denied_tools.contains(tool))
        .cloned()
        .collect();
    serde_json::json!({
        "overlay_digest": overlay.map(|o| o.digest.clone()),
        "files": overlay.map(BenchOverlay::files),
        "prompt_digest": crate::session::harness::base_prompt::bench_base_digest(body),
        "tools": tools,
        "max_iterations": knobs.max_iterations,
        "compact_budget_tokens": knobs.compaction_budget_tokens,
        "rotation_off": knobs.rotation_off,
        "compaction": overlay.and_then(|o| o.compaction.as_ref()),
    })
}
