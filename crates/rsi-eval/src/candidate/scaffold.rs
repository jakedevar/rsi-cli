//! `new` and `rename`: scaffold a candidate from its parent, and move an edited candidate to the
//! id its overlay digest now names.

use std::path::Path;

use rsi_common::bench_overlay::OVERLAY_FILE_NAME;
use serde::Serialize;

use super::{
    MAX_SMALL_FILE_BYTES, NOTES_FILE, RECORDS_FILE, RECORDS_PREFIX, REQUIRED_HEADINGS, RUN_FILE,
    Repo, check_plain_id, load_overlay, make_id, parse_id, read_regular, read_small_text,
    to_stable_json,
};
use crate::errors::EvalError;

pub(super) const DEFAULT_HOST: &str = "ssh://arch-laptop";
pub(super) const DEFAULT_MODEL: &str = "deepseek/deepseek-v4.1-flash";
pub(super) const DEFAULT_EFFORT: &str = "high";
const DEFAULT_TASK_CAP_USD: f64 = 0.40;
const SEED_OVERLAY: &str =
    "# The seed overlay sets nothing: the run is the default bench harness (#3194).\n";

pub(super) struct NewSpec {
    pub parent: Option<String>,
    pub seed: bool,
    pub ordinal: Option<u32>,
    pub records: Vec<String>,
    pub host: String,
    pub model: String,
    pub effort: String,
}

#[derive(Debug, Serialize)]
pub(super) struct Created {
    pub candidate: String,
    pub path: String,
    pub overlay_digest: String,
}

fn run_toml(id: &str, spec: &NewSpec) -> String {
    format!(
        "# Candidate run config (#3194): the search split at k=1 with the native harness and this\n\
         # candidate's overlay. The bench job reads it from its committed, clean HEAD.\n\
         schema_version = 1\n\
         name = \"{id}\"\n\
         description = \"Meta-Harness candidate {id}: search split, k=1\"\n\
         host = \"{host}\"\n\
         split = \"search\"\n\
         k = 1\n\
         concurrency = 4\n\
         overlay = \"{overlay}\"\n\
         \n\
         [agent]\n\
         kind = \"rsi\"\n\
         model = \"{model}\"\n\
         effort = \"{effort}\"\n\
         per_task_cap_usd = {cap:.2}\n",
        host = spec.host,
        overlay = Repo::overlay_rel(id),
        model = spec.model,
        effort = spec.effort,
        cap = DEFAULT_TASK_CAP_USD,
    )
}

fn notes_template(id: &str, parent: &str) -> String {
    let mut text = format!("# {id}\n");
    for heading in REQUIRED_HEADINGS {
        text.push_str(&format!("\n## {heading}\n"));
        text.push_str(match heading {
            "Parent" => parent,
            "Hypothesis" => {
                "<what the model gets wrong today, with the evidence and the digest path>"
            }
            "Single change" => "<the one change this overlay makes to its parent>",
            "Expected effect" => "<which behaviour changes, in which direction, and the cost>",
            _ => "<candidate ids this one builds on, or none>",
        });
        text.push('\n');
    }
    text
}

fn next_ordinal(repo: &Repo) -> Result<u32, EvalError> {
    Ok(repo
        .candidate_ids()?
        .iter()
        .filter_map(|id| parse_id(id).map(|p| p.0))
        .max()
        .map_or(1, |max| max + 1))
}

fn check_records(records: &[String]) -> Result<(), EvalError> {
    for pointer in records {
        let plain = pointer.starts_with(RECORDS_PREFIX)
            && pointer.ends_with(".json")
            && !pointer.contains("..")
            && !pointer[RECORDS_PREFIX.len()..].contains('/');
        if !plain {
            return Err(EvalError::Bench(format!(
                "{pointer:?} is not {RECORDS_PREFIX}<run>.json"
            )));
        }
    }
    Ok(())
}

/// Creates the candidate directory. The id carries the parent's overlay digest until the overlay
/// is edited; `rename` then moves it.
pub(super) fn new_candidate(repo: &Repo, spec: &NewSpec) -> Result<Created, EvalError> {
    check_records(&spec.records)?;
    let (parent_label, overlay_text, instructions, digest) = if spec.seed {
        (
            "none".to_string(),
            SEED_OVERLAY.to_string(),
            None,
            rsi_common::bench_overlay::empty_overlay_digest(),
        )
    } else {
        let parent = spec.parent.as_deref().unwrap_or_default();
        check_plain_id(parent)?;
        let parent_dir = repo.dir(parent);
        let loaded = load_overlay(&parent_dir)
            .map_err(|e| EvalError::Bench(format!("parent {parent}: {e}")))?;
        let overlay_text =
            read_small_text(&parent_dir.join(OVERLAY_FILE_NAME)).map_err(EvalError::Bench)?;
        let instructions = match &loaded.instructions {
            None => None,
            Some(name) => {
                let bytes = read_regular(&parent_dir.join(name), MAX_SMALL_FILE_BYTES)
                    .map_err(EvalError::Bench)?;
                Some((name.clone(), bytes))
            }
        };
        (
            parent.to_string(),
            overlay_text,
            instructions,
            loaded.digest(),
        )
    };
    let ordinal = match spec.ordinal {
        Some(n) => n,
        None if spec.seed => 0,
        None => next_ordinal(repo)?,
    };
    let id = make_id(ordinal, &digest);
    let dir = repo.dir(&id);
    if repo
        .candidate_ids()?
        .iter()
        .any(|other| parse_id(other).map(|p| p.0) == Some(ordinal))
    {
        return Err(EvalError::Bench(format!(
            "ordinal {ordinal:02} is already taken"
        )));
    }
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join(OVERLAY_FILE_NAME), overlay_text)?;
    if let Some((name, bytes)) = instructions {
        let target = dir.join(&name);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(target, bytes)?;
    }
    std::fs::write(dir.join(RUN_FILE), run_toml(&id, spec))?;
    std::fs::write(dir.join(NOTES_FILE), notes_template(&id, &parent_label))?;
    std::fs::write(dir.join(RECORDS_FILE), to_stable_json(&spec.records)?)?;
    Ok(Created {
        candidate: id.clone(),
        path: format!("{}/{id}", super::CANDIDATES_REL),
        overlay_digest: digest,
    })
}

/// Moves candidate `id` to the id its overlay digest names, and points its `run.toml` at the new
/// directory.
pub(super) fn rename(repo: &Repo, id: &str) -> Result<Created, EvalError> {
    check_plain_id(id)?;
    let (ordinal, _) =
        parse_id(id).ok_or_else(|| EvalError::Bench(format!("{id:?} is not c<NN>-<12 hex>")))?;
    let dir = repo.dir(id);
    let loaded = load_overlay(&dir).map_err(|e| EvalError::Bench(format!("{id}: overlay: {e}")))?;
    let digest = loaded.digest();
    let new_id = make_id(ordinal, &digest);
    if new_id != id {
        let target = repo.dir(&new_id);
        if target.exists() {
            return Err(EvalError::Bench(format!("{new_id} already exists")));
        }
        std::fs::rename(&dir, &target)?;
        rewrite_references(&target, id, &new_id)?;
        // A verification names the old id and the old files: it must be run again.
        let _ = std::fs::remove_file(target.join(super::VERIFY_FILE));
    }
    Ok(Created {
        candidate: new_id.clone(),
        path: format!("{}/{new_id}", super::CANDIDATES_REL),
        overlay_digest: digest,
    })
}

/// `run.toml` and the `# <id>` title of `notes.md` name the old id; point them at the new one.
fn rewrite_references(dir: &Path, old: &str, new: &str) -> Result<(), EvalError> {
    let run_path = dir.join(RUN_FILE);
    if let Ok(text) = read_small_text(&run_path) {
        std::fs::write(&run_path, text.replace(old, new))?;
    }
    let notes_path = dir.join(NOTES_FILE);
    if let Ok(text) = read_small_text(&notes_path)
        && let Some(rest) = text.strip_prefix(&format!("# {old}"))
    {
        std::fs::write(&notes_path, format!("# {new}{rest}"))?;
    }
    Ok(())
}
