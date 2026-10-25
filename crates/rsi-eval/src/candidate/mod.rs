//! `rsi-eval bench candidate` (#3194, program #3135 M6): the Meta-Harness candidate store.
//!
//! A candidate is one directory of committed files under `eval/bench/tb2/candidates/<id>/`
//! (`id` = `c<NN>-<first 12 hex of the canonical overlay digest>`):
//!
//! ```text
//! overlay.toml  instructions.md (optional)  run.toml  notes.md
//! records.json  verify.json (written by verify)  digest/ (written by the digest tool)
//! ```
//!
//! * `new --parent <id> | --seed`: scaffold a candidate and write its `run.toml`.
//! * `rename <id>`: move a hand-edited candidate to the id its overlay digest now names.
//! * `verify <id> [--proposer-events FILE] [--checkout DIR]`: schema, digest, run config, notes,
//!   parent, leak audit and (with an `AgentReadSessionEvents` export) the proposer read audit.
//!   Writes `verify.json`; exit 1 on any failure.
//! * `index`: rebuild `index.json` deterministically from the directories and their records.
//!
//! Offline and free: no network, no model call. The schema is the overlay type the daemon uses
//! (`rsi_common::bench_overlay`), and the leak audit runs natively in-process. Output is
//! stable JSON (no timestamps) that a regex search can use. Findings never repeat a held-out task
//! name or term: they name the file, line and kind so the proposer can fix the text.

mod audit;
mod index;
mod scaffold;
mod verify;

#[cfg(test)]
mod tests;

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use regex::Regex;
use rsi_common::bench_overlay::{
    BenchOverlay, OVERLAY_FILE_NAME, OVERLAY_INSTRUCTIONS_INVALID, OVERLAY_MAX_INSTRUCTIONS_BYTES,
    OVERLAY_PARSE_FAILED, OVERLAY_PATH_INVALID, OverlayError, RawOverlay, empty_overlay_digest,
};
use sha2::{Digest, Sha256};

use crate::errors::EvalError;

/// The repository directory candidates live under.
pub(crate) const CANDIDATES_REL: &str = "eval/bench/tb2/candidates";
const SPLIT_REL: &str = "eval/bench/tb2/split.json";
/// Where benchmark run records are committed. A proposer never reads them (D9).
pub(crate) const RECORDS_PREFIX: &str = "eval/bench/tb2/records/";
pub(crate) const RUN_FILE: &str = "run.toml";
pub(crate) const NOTES_FILE: &str = "notes.md";
pub(crate) const RECORDS_FILE: &str = "records.json";
pub(crate) const VERIFY_FILE: &str = "verify.json";
pub(crate) const INDEX_FILE: &str = "index.json";
pub(crate) const DIGEST_DIR: &str = "digest";
/// The overlay file is a short TOML table; anything larger is not one.
const MAX_OVERLAY_TOML_BYTES: u64 = 64 * 1024;
const MAX_SMALL_FILE_BYTES: u64 = 1024 * 1024;
/// `stable` schema version of `verify.json` and `index.json`.
pub(crate) const SCHEMA_VERSION: u32 = 1;

#[derive(Parser, Debug)]
#[command(name = "rsi-eval bench candidate")]
pub struct CandidateCli {
    /// Repository root (default: the nearest parent of the working directory that holds
    /// `eval/bench/tb2/split.json`).
    #[arg(long, global = true)]
    root: Option<PathBuf>,
    #[command(subcommand)]
    command: CandidateCommand,
}

#[derive(Subcommand, Debug)]
enum CandidateCommand {
    /// Scaffold a candidate from its parent (or the empty seed) and write its `run.toml`.
    New {
        /// The parent candidate id; the new overlay and instructions start as copies of its.
        #[arg(long, conflicts_with = "seed", required_unless_present = "seed")]
        parent: Option<String>,
        /// Create the seed: an empty overlay (ordinal 0 unless told otherwise).
        #[arg(long)]
        seed: bool,
        /// The ordinal `NN` of the id (default: the next free one; 0 for the seed).
        #[arg(long)]
        id_ordinal: Option<u32>,
        /// Record pointers (`eval/bench/tb2/records/<run>.json`) the new `records.json` starts with.
        #[arg(long = "record")]
        records: Vec<String>,
        /// Docker host of `run.toml`.
        #[arg(long, default_value = scaffold::DEFAULT_HOST)]
        host: String,
        /// Model of `run.toml` (the M6 model).
        #[arg(long, default_value = scaffold::DEFAULT_MODEL)]
        model: String,
        /// Effort of `run.toml`.
        #[arg(long, default_value = scaffold::DEFAULT_EFFORT)]
        effort: String,
    },
    /// Move a candidate to the id its overlay digest names (after editing the overlay).
    Rename { id: String },
    /// Check a candidate and write its `verify.json`; exit 1 on any failure.
    Verify {
        id: String,
        /// An `AgentReadSessionEvents` export of the proposer session: adds the read audit.
        #[arg(long)]
        proposer_events: Option<PathBuf>,
        /// The Terminal-Bench checkout: adds test-file names and literals to the leak audit.
        #[arg(long)]
        checkout: Option<PathBuf>,
    },
    /// Rebuild `index.json` from the candidate directories and their records.
    Index,
}

// ---------------------------------------------------------------------------------------------
// Repository and candidate ids
// ---------------------------------------------------------------------------------------------

/// The repository checkout a store lives in.
#[derive(Debug, Clone)]
pub(crate) struct Repo {
    pub root: PathBuf,
}

impl Repo {
    /// The nearest parent of `start` (itself included) that holds the pinned split.
    pub(crate) fn discover(start: &Path) -> Option<Self> {
        start
            .ancestors()
            .find(|dir| dir.join(SPLIT_REL).is_file())
            .map(|dir| Self {
                root: dir.to_path_buf(),
            })
    }

    pub(crate) fn candidates_dir(&self) -> PathBuf {
        self.root.join(CANDIDATES_REL)
    }

    pub(crate) fn dir(&self, id: &str) -> PathBuf {
        self.candidates_dir().join(id)
    }

    /// `eval/bench/tb2/candidates/<id>/overlay.toml`, as the run config names it.
    pub(crate) fn overlay_rel(id: &str) -> String {
        format!("{CANDIDATES_REL}/{id}/{OVERLAY_FILE_NAME}")
    }

    /// The `search` and `held_out` task names of the pinned split.
    pub(crate) fn split(&self) -> Result<Split, EvalError> {
        let path = self.root.join(SPLIT_REL);
        let text = std::fs::read_to_string(&path)
            .map_err(|e| EvalError::Bench(format!("{}: {e}", path.display())))?;
        serde_json::from_str(&text)
            .map_err(|e| EvalError::Bench(format!("{}: {e}", path.display())))
    }

    /// Every directory under the candidate store that is shaped like a candidate id, sorted.
    pub(crate) fn candidate_ids(&self) -> Result<Vec<String>, EvalError> {
        let mut ids = Vec::new();
        let entries = match std::fs::read_dir(self.candidates_dir()) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(ids),
            Err(e) => return Err(e.into()),
        };
        for entry in entries {
            let entry = entry?;
            if entry.file_type()?.is_dir()
                && let Some(name) = entry.file_name().to_str()
                && parse_id(name).is_some()
            {
                ids.push(name.to_string());
            }
        }
        ids.sort();
        Ok(ids)
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
pub(crate) struct Split {
    #[cfg(test)]
    pub search: Vec<String>,
    pub held_out: Vec<String>,
}

/// `(ordinal, 12 hex digits)` of a candidate id `c<NN>-<12 hex>`.
pub(crate) fn parse_id(id: &str) -> Option<(u32, &str)> {
    let rest = id.strip_prefix('c')?;
    let (ordinal, hex) = rest.split_once('-')?;
    let well_formed = ordinal.len() >= 2
        && ordinal.bytes().all(|b| b.is_ascii_digit())
        && hex.len() == 12
        && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    if well_formed {
        Some((ordinal.parse().ok()?, hex))
    } else {
        None
    }
}

pub(crate) fn make_id(ordinal: u32, digest: &str) -> String {
    let hex = digest.strip_prefix("sha256:").unwrap_or(digest);
    format!("c{ordinal:02}-{}", &hex[..12.min(hex.len())])
}

/// A command-line id is a plain directory name: no separators, no leading dot or dash.
pub(crate) fn check_plain_id(id: &str) -> Result<(), EvalError> {
    let plain = !id.is_empty()
        && id.len() <= 100
        && !id.starts_with(['-', '.'])
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && !id.contains("..");
    if plain {
        Ok(())
    } else {
        Err(EvalError::Bench(format!(
            "{id:?} is not a candidate id (c<NN>-<12 hex>)"
        )))
    }
}

// ---------------------------------------------------------------------------------------------
// Reading candidate files
// ---------------------------------------------------------------------------------------------

/// Reads a regular file (a symlink is refused) of at most `limit` bytes.
pub(crate) fn read_regular(path: &Path, limit: u64) -> Result<Vec<u8>, String> {
    let name = path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    let meta = std::fs::symlink_metadata(path).map_err(|e| format!("{name}: {e}"))?;
    if !meta.is_file() {
        return Err(format!("{name}: not a regular file (a symlink is refused)"));
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(limit + 1).read_to_end(&mut bytes))
        .map_err(|e| format!("{name}: {e}"))?;
    if bytes.len() as u64 > limit {
        return Err(format!("{name}: larger than {limit} bytes"));
    }
    Ok(bytes)
}

pub(crate) fn read_small_text(path: &Path) -> Result<String, String> {
    let bytes = read_regular(path, MAX_SMALL_FILE_BYTES)?;
    String::from_utf8(bytes).map_err(|_| format!("{}: not UTF-8", path.display()))
}

/// A candidate's overlay as the daemon resolves it.
#[derive(Debug, Clone)]
pub(crate) struct LoadedOverlay {
    /// `None` when the overlay sets nothing (the seed).
    pub overlay: Option<BenchOverlay>,
    /// The instructions file name as written in `overlay.toml`.
    pub instructions: Option<String>,
}

impl LoadedOverlay {
    /// The canonical `sha256:<hex>` digest; the empty overlay has its own.
    pub(crate) fn digest(&self) -> String {
        self.overlay
            .as_ref()
            .map_or_else(empty_overlay_digest, |o| o.digest.clone())
    }
}

/// Parses `overlay.toml` of `dir` (and its instructions file) through the shared overlay type.
pub(crate) fn load_overlay(dir: &Path) -> Result<LoadedOverlay, OverlayError> {
    let bytes = read_regular(&dir.join(OVERLAY_FILE_NAME), MAX_OVERLAY_TOML_BYTES)
        .map_err(|why| OverlayError::new(OVERLAY_PARSE_FAILED, why))?;
    let text = String::from_utf8(bytes).map_err(|_| {
        OverlayError::new(
            OVERLAY_PARSE_FAILED,
            format!("{OVERLAY_FILE_NAME}: not UTF-8"),
        )
    })?;
    let raw: RawOverlay = toml::from_str(&text).map_err(|e| {
        OverlayError::new(
            OVERLAY_PARSE_FAILED,
            format!("{OVERLAY_FILE_NAME}: {}", e.to_string().replace('\n', " ")),
        )
    })?;
    let name = raw.instructions_path()?.map(str::to_string);
    let instructions = match &name {
        None => None,
        Some(name) => Some(
            read_regular(&dir.join(name), OVERLAY_MAX_INSTRUCTIONS_BYTES as u64).map_err(
                |why| {
                    let code = if why.contains("regular") {
                        OVERLAY_PATH_INVALID
                    } else {
                        OVERLAY_INSTRUCTIONS_INVALID
                    };
                    OverlayError::new(code, why)
                },
            )?,
        ),
    };
    let overlay = raw.resolve(instructions.as_deref())?;
    Ok(LoadedOverlay {
        overlay,
        instructions: name,
    })
}

/// `sha256:<hex>` over the files whose text defines a candidate (overlay, instructions, notes and
/// run config), by name. `verify.json` records it so `index` can tell a stale verification.
pub(crate) fn inputs_digest(dir: &Path, instructions: Option<&str>) -> String {
    let mut names: Vec<&str> = vec![OVERLAY_FILE_NAME, RUN_FILE, NOTES_FILE];
    if let Some(name) = instructions {
        names.push(name);
    }
    names.sort_unstable();
    names.dedup();
    let mut hasher = Sha256::new();
    for name in names {
        if let Ok(bytes) = read_regular(&dir.join(name), MAX_SMALL_FILE_BYTES.max(1 << 20)) {
            hasher.update(name.as_bytes());
            hasher.update([0]);
            hasher.update(Sha256::digest(&bytes));
        }
    }
    format!("sha256:{:x}", hasher.finalize())
}

/// The instructions name of an overlay whose schema may not resolve (best effort).
pub(crate) fn instructions_name(dir: &Path) -> Option<String> {
    let text = read_small_text(&dir.join(OVERLAY_FILE_NAME)).ok()?;
    let raw: RawOverlay = toml::from_str(&text).ok()?;
    raw.instructions_path().ok().flatten().map(str::to_string)
}

// ---------------------------------------------------------------------------------------------
// notes.md
// ---------------------------------------------------------------------------------------------

/// The headings `notes.md` must carry, in the order the template writes them.
pub(crate) const REQUIRED_HEADINGS: [&str; 5] = [
    "Parent",
    "Hypothesis",
    "Single change",
    "Expected effect",
    "Prior candidates cited",
];

/// `(heading, body lines)` for every markdown heading of `text`.
pub(crate) fn sections(text: &str) -> Vec<(String, Vec<String>)> {
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        let level = trimmed.bytes().take_while(|b| *b == b'#').count();
        if (1..=6).contains(&level) && trimmed[level..].starts_with(' ') {
            let title = trimmed[level..].trim().trim_end_matches('#').trim();
            out.push((title.to_string(), Vec::new()));
        } else if let Some((_, body)) = out.last_mut() {
            body.push(line.to_string());
        }
    }
    out
}

/// A body counts when it has a line that is not blank and not a `<template placeholder>`.
pub(crate) fn body_is_filled(body: &[String]) -> bool {
    body.iter().any(|line| {
        let line = line.trim();
        !line.is_empty() && !(line.starts_with('<') && line.ends_with('>'))
    })
}

/// The parent id named under `## Parent` (`none` for the seed), or `None` when absent.
pub(crate) fn notes_parent(notes: &str) -> Option<String> {
    sections(notes)
        .into_iter()
        .find(|(title, _)| title.eq_ignore_ascii_case("Parent"))
        .and_then(|(_, body)| {
            body.iter()
                .map(|line| line.trim().trim_matches('`').trim())
                .find(|line| !line.is_empty() && !line.starts_with('<'))
                .map(str::to_string)
        })
}

/// The `eval/bench/tb2/records/<run>.json` pointers of `records.json`.
pub(crate) fn read_records(dir: &Path) -> Result<Vec<String>, String> {
    let text = read_small_text(&dir.join(RECORDS_FILE))?;
    let pointers: Vec<String> =
        serde_json::from_str(&text).map_err(|e| format!("{RECORDS_FILE}: {e}"))?;
    let shape =
        Regex::new(r"^eval/bench/tb2/records/[A-Za-z0-9._-]+\.json$").map_err(|e| e.to_string())?;
    for pointer in &pointers {
        if !shape.is_match(pointer) || pointer.contains("..") {
            return Err(format!(
                "{RECORDS_FILE}: {pointer:?} is not {RECORDS_PREFIX}<run>.json"
            ));
        }
    }
    Ok(pointers)
}

// ---------------------------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------------------------

fn run_command(cli: CandidateCli) -> Result<ExitCode, EvalError> {
    let repo = match cli.root {
        Some(root) => Repo { root },
        None => Repo::discover(&std::env::current_dir()?).ok_or_else(|| {
            EvalError::Bench(format!(
                "no repository with {SPLIT_REL} above the working directory; pass --root"
            ))
        })?,
    };
    match cli.command {
        CandidateCommand::New {
            parent,
            seed,
            id_ordinal,
            records,
            host,
            model,
            effort,
        } => {
            let spec = scaffold::NewSpec {
                parent,
                seed,
                ordinal: id_ordinal,
                records,
                host,
                model,
                effort,
            };
            let created = scaffold::new_candidate(&repo, &spec)?;
            print!("{}", to_stable_json(&created)?);
            Ok(ExitCode::SUCCESS)
        }
        CandidateCommand::Rename { id } => {
            let renamed = scaffold::rename(&repo, &id)?;
            print!("{}", to_stable_json(&renamed)?);
            Ok(ExitCode::SUCCESS)
        }
        CandidateCommand::Verify {
            id,
            proposer_events,
            checkout,
        } => {
            let report =
                verify::verify(&repo, &id, proposer_events.as_deref(), checkout.as_deref())?;
            print!("{}", to_stable_json(&report)?);
            Ok(if report.ok {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            })
        }
        CandidateCommand::Index => {
            let path = index::write_index(&repo)?;
            println!("{}", path.display());
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// Pretty JSON with a trailing newline: field order is the struct's, so output is stable.
pub(crate) fn to_stable_json<T: serde::Serialize>(value: &T) -> Result<String, EvalError> {
    let mut text = serde_json::to_string_pretty(value)?;
    text.push('\n');
    Ok(text)
}

/// Entry point for `rsi-eval bench candidate <args>`; `args` excludes the program name,
/// `bench` and `candidate`.
pub fn run_cli(args: impl IntoIterator<Item = OsString>) -> ExitCode {
    let argv = std::iter::once(OsString::from("rsi-eval bench candidate")).chain(args);
    let cli = CandidateCli::parse_from(argv);
    match run_command(cli) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("rsi-eval: {error}");
            ExitCode::from(1)
        }
    }
}
