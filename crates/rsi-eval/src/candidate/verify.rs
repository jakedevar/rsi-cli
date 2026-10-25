//! `rsi-eval bench candidate verify`: the checks, in order, and the `verify.json` they write.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use rsi_common::bench_overlay::OVERLAY_FILE_NAME;
use rsi_common::bench_profile::LoopKnobs;
use serde::{Deserialize, Serialize};

use super::{
    DIGEST_DIR, NOTES_FILE, REQUIRED_HEADINGS, RUN_FILE, Repo, SCHEMA_VERSION, VERIFY_FILE, audit,
    body_is_filled, check_plain_id, inputs_digest, instructions_name, load_overlay, notes_parent,
    parse_id, read_records, read_small_text, sections, to_stable_json,
};
use crate::errors::EvalError;

/// The only run config schema (`rsid` `RUN_CONFIG_SCHEMA`).
const RUN_CONFIG_SCHEMA: u32 = 1;
/// The most attempts per task a run config may ask for (`rsid` `MAX_ATTEMPTS`).
const MAX_ATTEMPTS: u32 = 10;
/// The most USD a single task may be capped at (`rsid` `MAX_TASK_CAP_USD`).
const MAX_TASK_CAP_USD: f64 = 100.0;

/// One check of the report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Check {
    pub name: String,
    pub ok: bool,
    /// False when the check was skipped (an earlier failure left it nothing to check) or, for
    /// the read audit, when no proposer events were given.
    pub ran: bool,
    pub findings: Vec<String>,
}

impl Check {
    fn done(name: &str, findings: Vec<String>) -> Self {
        Self {
            name: name.into(),
            ok: findings.is_empty(),
            ran: true,
            findings,
        }
    }

    fn skipped(name: &str, ok: bool, why: &str) -> Self {
        Self {
            name: name.into(),
            ok,
            ran: false,
            findings: vec![why.into()],
        }
    }
}

/// `verify.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct VerifyReport {
    pub schema_version: u32,
    pub candidate: String,
    pub ok: bool,
    /// The canonical overlay digest, when the overlay resolved.
    pub overlay_digest: Option<String>,
    /// [`inputs_digest`] at verification time: a later edit makes this report stale.
    pub inputs_sha256: String,
    pub read_audit_ran: bool,
    pub tool_calls_audited: usize,
    pub truncated_tool_calls: usize,
    pub checks: Vec<Check>,
}

// ---------------------------------------------------------------------------------------------
// run.toml
// ---------------------------------------------------------------------------------------------

/// Mirrors `rsid`'s `AgentSection` (a `pub(crate)` type the eval crate cannot reach). Unknown
/// fields are refused as in the daemon; the shipped `runs/*.toml` are parsed in the tests so a
/// drift shows.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentSection {
    kind: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    effort: Option<String>,
    #[serde(default)]
    per_task_cap_usd: Option<f64>,
}

/// Mirrors `rsid`'s `RunFile`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunFile {
    schema_version: u32,
    name: String,
    #[serde(default)]
    #[allow(dead_code)]
    description: String,
    host: String,
    #[serde(default)]
    split: Option<String>,
    #[serde(default)]
    tasks: Vec<String>,
    #[serde(default = "one")]
    k: u32,
    #[serde(default = "one_f64")]
    timeout_multiplier: f64,
    #[serde(default = "four")]
    #[allow(dead_code)]
    concurrency: u32,
    agent: AgentSection,
    #[serde(default, rename = "loop")]
    #[allow(dead_code)]
    loop_knobs: LoopKnobs,
    #[serde(default)]
    overlay: Option<String>,
}

const fn one() -> u32 {
    1
}
const fn four() -> u32 {
    4
}
const fn one_f64() -> f64 {
    1.0
}

/// Parses `run.toml` and checks what a candidate run needs: the search split at a bounded `k`, the
/// native harness with a model and effort, and the overlay of this very candidate.
pub(super) fn check_run_config(dir: &Path, id: &str) -> Vec<String> {
    let text = match read_small_text(&dir.join(RUN_FILE)) {
        Ok(text) => text,
        Err(why) => return vec![why],
    };
    let run: RunFile = match toml::from_str(&text) {
        Ok(run) => run,
        Err(e) => {
            return vec![format!(
                "run_parse_failed: {}",
                e.to_string().replace('\n', " ")
            )];
        }
    };
    let mut findings = Vec::new();
    let mut need = |ok: bool, why: String| {
        if !ok {
            findings.push(why);
        }
    };
    need(
        run.schema_version == RUN_CONFIG_SCHEMA,
        format!("schema_version must be {RUN_CONFIG_SCHEMA}"),
    );
    need(!run.name.trim().is_empty(), "name is empty".into());
    need(
        run.host.starts_with("ssh://"),
        "host must be ssh://<host>".into(),
    );
    need(
        run.split.as_deref() == Some("search") && run.tasks.is_empty(),
        "split must be \"search\" with no explicit tasks".into(),
    );
    need(
        (1..=MAX_ATTEMPTS).contains(&run.k),
        format!("k must be 1..={MAX_ATTEMPTS}"),
    );
    need(
        (run.timeout_multiplier - 1.0).abs() < f64::EPSILON,
        "timeout_multiplier must be 1.0".into(),
    );
    need(run.agent.kind == "rsi", "agent.kind must be \"rsi\"".into());
    need(
        run.agent
            .model
            .as_deref()
            .is_some_and(|m| !m.trim().is_empty()),
        "agent.model is required".into(),
    );
    need(
        run.agent
            .effort
            .as_deref()
            .is_some_and(|e| !e.trim().is_empty()),
        "agent.effort is required".into(),
    );
    need(
        run.agent
            .per_task_cap_usd
            .is_none_or(|cap| cap > 0.0 && cap <= MAX_TASK_CAP_USD),
        format!("agent.per_task_cap_usd must be in (0, {MAX_TASK_CAP_USD}]"),
    );
    let expected = Repo::overlay_rel(id);
    need(
        run.overlay.as_deref() == Some(expected.as_str()),
        format!("overlay must be {expected}"),
    );
    findings
}

// ---------------------------------------------------------------------------------------------
// notes.md and parent
// ---------------------------------------------------------------------------------------------

fn check_notes(dir: &Path) -> Vec<String> {
    let text = match read_small_text(&dir.join(NOTES_FILE)) {
        Ok(text) => text,
        Err(why) => return vec![why],
    };
    let found = sections(&text);
    let mut findings = Vec::new();
    for required in REQUIRED_HEADINGS {
        match found.iter().find(|(t, _)| t.eq_ignore_ascii_case(required)) {
            None => findings.push(format!("missing heading: {required}")),
            Some((_, body)) if !body_is_filled(body) => {
                findings.push(format!("empty section: {required}"));
            }
            Some(_) => {}
        }
    }
    findings
}

fn check_parent(repo: &Repo, dir: &Path, id: &str) -> Vec<String> {
    let Ok(text) = read_small_text(&dir.join(NOTES_FILE)) else {
        return vec!["notes.md is unreadable".into()];
    };
    let Some(parent) = notes_parent(&text) else {
        return vec!["no parent named under ## Parent".into()];
    };
    let is_seed = parse_id(id).is_some_and(|(ordinal, _)| ordinal == 0);
    if parent == "none" {
        return if is_seed {
            Vec::new()
        } else {
            vec!["only the seed (c00) has no parent".into()]
        };
    }
    if parent == id {
        return vec!["a candidate cannot be its own parent".into()];
    }
    if parse_id(&parent).is_none() || check_plain_id(&parent).is_err() {
        return vec![format!("parent {parent:?} is not a candidate id")];
    }
    if repo.dir(&parent).join(OVERLAY_FILE_NAME).is_file() {
        Vec::new()
    } else {
        vec![format!("parent {parent} is not in the candidate store")]
    }
}

// ---------------------------------------------------------------------------------------------
// The runs the store lists
// ---------------------------------------------------------------------------------------------

/// Run names the candidate store lists: the stems of every `records.json` pointer, and the runs
/// the digest tool's files cite as `~/.rsi/bench/<run>`.
pub(crate) fn listed_runs(repo: &Repo) -> Result<BTreeSet<String>, EvalError> {
    let mut runs = BTreeSet::new();
    for id in repo.candidate_ids()? {
        let dir = repo.dir(&id);
        for pointer in read_records(&dir).unwrap_or_default() {
            if let Some(stem) = pointer
                .rsplit('/')
                .next()
                .and_then(|name| name.strip_suffix(".json"))
            {
                runs.insert(stem.to_string());
            }
        }
        if let Ok(entries) = std::fs::read_dir(dir.join(DIGEST_DIR)) {
            for entry in entries.flatten() {
                if let Ok(text) = read_small_text(&entry.path()) {
                    for reference in audit::bench_refs(&text) {
                        if let audit::BenchRef::Run(run) = reference {
                            runs.insert(run);
                        }
                    }
                }
            }
        }
    }
    Ok(runs)
}

// ---------------------------------------------------------------------------------------------
// verify
// ---------------------------------------------------------------------------------------------

/// Duplicate-digest findings: another candidate with the same overlay digest.
fn duplicate_digests(repo: &Repo, id: &str, digest: &str) -> Result<Vec<String>, EvalError> {
    let mut findings = Vec::new();
    for other in repo.candidate_ids()? {
        if other != id
            && load_overlay(&repo.dir(&other)).is_ok_and(|loaded| loaded.digest() == digest)
        {
            findings.push(format!("same overlay digest as {other}"));
        }
    }
    Ok(findings)
}

fn digest_check(repo: &Repo, id: &str, digest: Option<&str>) -> Result<Check, EvalError> {
    let Some(digest) = digest else {
        return Ok(Check::skipped(
            "digest",
            false,
            "skipped: the overlay did not resolve",
        ));
    };
    let mut findings = Vec::new();
    match parse_id(id) {
        None => findings.push("id is not c<NN>-<12 hex>".to_string()),
        Some((_, hex)) => {
            let expected = digest.strip_prefix("sha256:").unwrap_or(digest);
            if !expected.starts_with(hex) {
                findings.push(format!(
                    "id_digest_mismatch: the overlay digest names {} (run `rsi-eval bench candidate rename {id}`)",
                    super::make_id(parse_id(id).map_or(0, |p| p.0), digest),
                ));
            }
        }
    }
    findings.extend(duplicate_digests(repo, id, digest)?);
    Ok(Check::done("digest", findings))
}

/// Runs every check of candidate `id`, writes `verify.json` and returns the report.
///
/// # Errors
/// [`EvalError`] for an unusable id, an unreadable proposer export or an unwritable report. A
/// failing candidate is a report with `ok: false`, not an error.
pub(crate) fn verify(
    repo: &Repo,
    id: &str,
    proposer_events: Option<&Path>,
    checkout: Option<&Path>,
) -> Result<VerifyReport, EvalError> {
    check_plain_id(id)?;
    let dir = repo.dir(id);
    if !dir.is_dir() {
        return Err(EvalError::Bench(format!(
            "{}: no such candidate",
            dir.display()
        )));
    }
    let mut checks = Vec::new();

    // 1. schema: the overlay type the daemon uses.
    let loaded = load_overlay(&dir);
    let digest = loaded.as_ref().ok().map(super::LoadedOverlay::digest);
    checks.push(Check::done(
        "schema",
        loaded
            .as_ref()
            .err()
            .map(ToString::to_string)
            .into_iter()
            .collect(),
    ));
    // 2. digest and id.
    checks.push(digest_check(repo, id, digest.as_deref())?);
    // 3. run.toml, 4. notes.md, 5. parent.
    checks.push(Check::done("run_config", check_run_config(&dir, id)));
    checks.push(Check::done("notes", check_notes(&dir)));
    checks.push(Check::done("parent", check_parent(repo, &dir, id)));
    checks.push(records_check(&dir));

    // 6. leak audit: held-out terms everywhere, generality terms where the model reads.
    let instructions = loaded
        .as_ref()
        .ok()
        .and_then(|l| l.instructions.clone())
        .or_else(|| instructions_name(&dir));
    let mut model_files: Vec<PathBuf> = vec![dir.join(OVERLAY_FILE_NAME)];
    if let Some(name) = &instructions {
        model_files.push(dir.join(name));
    }
    model_files.retain(|f| f.is_file());
    let mut all_files = model_files.clone();
    if dir.join(NOTES_FILE).is_file() {
        all_files.push(dir.join(NOTES_FILE));
    }
    checks.push(
        match audit::leak_audit(repo, &all_files, &model_files, checkout) {
            Ok(findings) => Check::done("leak_audit", findings),
            Err(why) => Check::done("leak_audit", vec![format!("audit_failed: {why}")]),
        },
    );

    // 7. proposer read audit, when the session export is given.
    let (read_check, ran, calls, truncated) = match proposer_events {
        None => (
            Check::skipped("read_audit", true, "not run: no --proposer-events"),
            false,
            0,
            0,
        ),
        Some(path) => read_audit_check(repo, path)?,
    };
    checks.push(read_check);

    let report = VerifyReport {
        schema_version: SCHEMA_VERSION,
        candidate: id.to_string(),
        ok: checks.iter().all(|c| c.ok),
        overlay_digest: digest,
        inputs_sha256: inputs_digest(&dir, instructions.as_deref()),
        read_audit_ran: ran,
        tool_calls_audited: calls,
        truncated_tool_calls: truncated,
        checks,
    };
    std::fs::write(dir.join(VERIFY_FILE), to_stable_json(&report)?)?;
    Ok(report)
}

fn records_check(dir: &Path) -> Check {
    match read_records(dir) {
        Ok(_) => Check::done("records", Vec::new()),
        Err(why) => Check::done("records", vec![why]),
    }
}

fn read_audit_check(repo: &Repo, path: &Path) -> Result<(Check, bool, usize, usize), EvalError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| EvalError::Bench(format!("{}: {e}", path.display())))?;
    let export: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| EvalError::Bench(format!("{}: {e}", path.display())))?;
    let split = repo.split()?;
    let listed = listed_runs(repo)?;
    Ok(match audit::read_audit(&export, &split.held_out, &listed) {
        Ok(report) => (
            Check::done("read_audit", report.findings),
            true,
            report.tool_calls,
            report.truncated_tool_calls,
        ),
        Err(why) => (
            Check::done("read_audit", vec![format!("audit_failed: {why}")]),
            true,
            0,
            0,
        ),
    })
}
