//! #3346: dry-run the lander's provisional-migration proof for a queued source.
//!
//! The lander renumbers a source that declares a provisional migration with
//! the `rolling-migration-renumber` guard: `--transform`, then `--build` (merge,
//! resolve the version conflicts, prove the result). A source the tool refuses
//! (`unproved migration conflict`, a stale declaration, ...) used to be found
//! only at gate time, where it failed the whole batch and split it by bisect.
//! The queue's sweep ([`super::tip_conflict`]) runs the same steps here, in a
//! scratch clone, against the rolling tip, and refuses the source at once with
//! the tool's own reason (`provisional_unprovable`).
//!
//! The same proof classifies a CONFLICT (#3336, #3362). A conflict between a
//! source and a target commit (the rolling tip, or a queue entry ahead) is
//! `renumberable`, not a conflict, only when the source adds a declaration and
//! the tool proves the renumber against that target ([`Proof::Renumbered`]):
//! its own schema and site validation decides which paths are renumber sites
//! and its resolve step refuses a conflict on any other path
//! (`nonmigration conflict`). A path merely named in a declaration exempts
//! nothing, and no second parser of the declaration exists.
//!
//! Incomplete proofs carry an infrastructure cause. Enqueue refuses them with
//! a retry instruction; the best-effort sweep and conflict classifier keep
//! their unknown outcome. Sources without a new declaration need no proof. A
//! missing trusted guard executable is also an infrastructure cause; this probe
//! never runs a tool copied from the source tree.
//!
//! Nothing here touches the project repository: the tool runs in a
//! `git clone --shared --no-checkout` under a private temporary directory
//! (alternate object store, no refs written back), exactly as the lander's
//! private clone. The guard is never code of that clone or of the source: it is
//! the trusted executable, started as a killable child with its own `--repo`.

use super::{PROVISIONAL_DIR, git_program, isolate_probe_git};
use crate::repo_guard_call::trusted_tool;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The guard that proves a renumber (`rsi-repo-guard rolling-migration-renumber`).
const GUARD: &str = "rolling-migration-renumber";
/// The tool's exit code for a refusal (a verdict on the source, not a crash).
const REFUSAL_EXIT: i32 = 2;
/// The longest one step (clone, transform or build) may run.
const STEP_TIMEOUT: Duration = Duration::from_secs(45);
/// The most characters of the tool's reason a refusal detail carries.
const MAX_REASON_CHARS: usize = 600;
/// The most stdout (a JSON unit) read from one tool step.
const MAX_TOOL_STDOUT: u64 = 8 << 20;

/// Why a probe could not reach a verdict: the tool could not run or its answer
/// could not be trusted. That includes no trusted guard executable at all:
/// the guard is never a file of the tip or the source (#465), so an
/// unresolvable guard is this and never `Proof::Clear` (hub #3545, fail closed).
#[derive(Debug, PartialEq, Eq)]
pub(super) enum InconclusiveCause {
    Infrastructure(String),
}

impl std::fmt::Display for InconclusiveCause {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Infrastructure(cause) => f.write_str(cause),
        }
    }
}

/// What the dry run learned about one source against one tip.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Proof {
    /// Nothing to refuse: the source declares no new migration, it is already in
    /// the tip, or the tool proves the lander merges
    /// it as is (nothing to renumber).
    Clear,
    /// The source declares a provisional migration and the tool proves the
    /// renumber onto the target: a version clash with the target is resolved at
    /// landing. The versions are the tool's `old_version` / `new_version`.
    Renumbered {
        old_version: Option<u64>,
        new_version: Option<u64>,
    },
    /// The tool refused the source, with this reason.
    Unprovable(String),
    /// Infrastructure prevented a verdict on the source.
    Inconclusive { cause: InconclusiveCause },
}

impl Proof {
    /// "declared migration v185 will be renumbered to v186 at landing".
    pub(super) fn renumber_note(&self) -> Option<String> {
        let Self::Renumbered {
            old_version,
            new_version,
        } = self
        else {
            return None;
        };
        Some(match (old_version, new_version) {
            (Some(old), Some(new)) if old != new => {
                format!("declared migration v{old} will be renumbered to v{new} at landing")
            }
            (Some(old), _) => format!("declared migration v{old} will be renumbered at landing"),
            _ => "declared migration will be renumbered at landing".to_owned(),
        })
    }
}

/// What a conflict between a source and a target commit is (#3336, #3362).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Renumber {
    /// Every conflict is a version clash the tool proves it renumbers away;
    /// the note says which declared migration (`declared migration v185 will be
    /// renumbered to v186 at landing`).
    Renumberable(String),
    /// A real conflict. The tool's reason when the source carries a declaration
    /// the tool refuses against the target (including a conflict on a path that
    /// is not a renumber site); `None` when nothing is declared or applicable.
    Conflict(Option<String>),
    /// The proof could not run or finish: nothing is known.
    Unknown,
}

/// Blocking: whether the conflicts of merging `source` onto `target` are all
/// version clashes of the provisional migration `source` declares, proved by
/// the lander's own renumber tool (one proof, no second parser).
pub(super) fn classify_conflict(
    repo: &Path,
    target: &str,
    source: &str,
    deadline: Instant,
) -> Renumber {
    match prove_against_tip(repo, target, source, deadline) {
        proof @ Proof::Renumbered { .. } => {
            Renumber::Renumberable(proof.renumber_note().unwrap_or_default())
        }
        // #3406: a queued target may still carry the released inventory from
        // its base. Publication pins/renumbers that provisional unit first.
        // Rejecting its unpublished inventory proves no conflict in `source`;
        // the pair must be checked again against the published target.
        Proof::Unprovable(reason)
            if reason.starts_with(&format!("invalid migration inventory at {target}:")) =>
        {
            Renumber::Unknown
        }
        Proof::Unprovable(reason) => Renumber::Conflict(Some(reason)),
        Proof::Clear => Renumber::Conflict(None),
        Proof::Inconclusive { .. } => Renumber::Unknown,
    }
}

/// Blocking: transform, resolve and prove `source`'s provisional migration
/// against `tip` in a scratch clone of `repo`, every step killed at `deadline`.
pub(super) fn prove_against_tip(repo: &Path, tip: &str, source: &str, deadline: Instant) -> Proof {
    let git = |repo: &Path, args: &[&str]| {
        if Instant::now() >= deadline {
            None
        } else {
            super::probe_git(repo, args, deadline)
        }
    };
    // The tool accepts commit object ids only, including for a still-free
    // version's validation. Never pass a rolling ref or a caller's shorthand.
    let Some(tip) = git(
        repo,
        &["rev-parse", "--verify", &format!("{tip}^{{commit}}")],
    ) else {
        return inconclusive(
            "could not resolve target commit (missing rolling ref/object, git failure or timeout)",
        );
    };
    let Some(source) = git(
        repo,
        &["rev-parse", "--verify", &format!("{source}^{{commit}}")],
    ) else {
        return inconclusive(
            "could not resolve source commit (missing object, git failure or timeout)",
        );
    };
    let (tip, source) = (tip.trim(), source.trim());
    let Some(base) = git(repo, &["merge-base", tip, source]) else {
        return inconclusive("could not read merge base (git failure or timeout)");
    };
    let base = base.trim();
    if base.is_empty() {
        return inconclusive("git returned an empty merge base");
    }
    // The source is contained in the tip: the lander merges nothing.
    if base == source {
        return Proof::Clear;
    }
    // Only a source that ADDS a declaration is renumbered (#3337).
    let Some(added) = git(
        repo,
        &[
            "diff",
            "--name-only",
            "--no-renames",
            "--diff-filter=A",
            base,
            source,
            "--",
            PROVISIONAL_DIR,
        ],
    ) else {
        return inconclusive(
            "could not list added provisional declarations (git failure or timeout)",
        );
    };
    if !added.lines().any(|path| path.ends_with(".json")) {
        return Proof::Clear;
    }
    // No trusted executable: nothing here can say what the lander does.
    let Some(tool) = trusted_tool(repo, tip) else {
        return inconclusive("no trusted rolling-migration-renumber guard executable");
    };
    let Ok(workspace) = tempfile::Builder::new()
        .prefix("rsi-provisional-probe-")
        .tempdir()
    else {
        return inconclusive("could not create provisional proof workspace");
    };
    run_steps(repo, workspace.path(), &tool, (base, source, tip), deadline)
}

fn run_steps(
    repo: &Path,
    workspace: &Path,
    guard: &Path,
    (base, source, tip): (&str, &str, &str),
    deadline: Instant,
) -> Proof {
    let clone = workspace.join("repo");
    let scratch = workspace.join("scratch");
    let unit_file = workspace.join("unit.json");
    if std::fs::create_dir(&scratch).is_err() {
        return inconclusive("could not create provisional proof scratch directory");
    }
    let mut command = Command::new(git_program());
    isolate_probe_git(&mut command);
    command
        .args([
            "clone",
            "--shared",
            "--no-checkout",
            "--quiet",
            "-c",
            "gc.auto=0",
            "-c",
            "maintenance.auto=false",
        ])
        .arg(repo)
        .arg(&clone);
    match run_step(command, deadline, Stdio::null()) {
        Ok((Some(0), _)) => {}
        Ok((code, _)) => return inconclusive(format!("scratch git clone exited {code:?}")),
        Err(cause) => return inconclusive(format!("scratch git clone: {cause}")),
    }
    let log = workspace.join("tool.stderr");
    let tool = |args: &[&std::ffi::OsStr]| -> Result<(Option<i32>, Vec<u8>), String> {
        let stderr = std::fs::File::create(&log)
            .map_err(|error| format!("could not open tool stderr: {error}"))?;
        let mut command = Command::new(guard);
        command
            .arg(GUARD)
            .arg("--repo")
            .arg(&clone)
            .args(args)
            .current_dir(&clone)
            .env_clear()
            .envs(super::inherited_git_env(std::env::vars_os()));
        run_step(command, tool_deadline(repo, deadline), Stdio::from(stderr))
    };
    let reason = || {
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        let text = text.trim();
        let text = text
            .strip_prefix("provisional migration refused:")
            .map_or(text, str::trim);
        let shown: String = text.chars().take(MAX_REASON_CHARS).collect();
        if shown.is_empty() {
            "the renumber tool refused the source without a reason".to_owned()
        } else {
            shown
        }
    };

    let transformed = tool(&[
        "--transform".as_ref(),
        "--base".as_ref(),
        base.as_ref(),
        "--source".as_ref(),
        source.as_ref(),
        "--target".as_ref(),
        tip.as_ref(),
    ]);
    let unit = match transformed {
        Ok((Some(0), stdout)) => match parse_unit(&stdout) {
            Ok(unit) => unit,
            Err(cause) => return inconclusive(format!("renumber transform: {cause}")),
        },
        Ok((Some(REFUSAL_EXIT), _)) => return Proof::Unprovable(reason()),
        Ok((code, _)) => {
            return inconclusive(format!(
                "renumber transform exited {code:?}; expected 0 or 2"
            ));
        }
        Err(cause) => return inconclusive(format!("renumber transform: {cause}")),
    };
    if unit["renumber_noop"].as_bool() == Some(true) {
        // #3397: --transform skips the site inventory at rolling head + 1.
        // Inspection without --transform runs the tool's full validation at
        // that same latest+1, without rewriting or building a candidate.
        // An identical migration already landed (#1768) has no new unit.
        if unit["migration_already_landed"].as_bool() == Some(true) {
            return Proof::Clear;
        }
        return match tool(&[
            "--base".as_ref(),
            base.as_ref(),
            "--source".as_ref(),
            source.as_ref(),
            "--target".as_ref(),
            tip.as_ref(),
        ]) {
            Ok((Some(0), stdout)) => match parse_unit(&stdout) {
                Ok(_) => Proof::Clear,
                Err(cause) => inconclusive(format!("still-free inspection: {cause}")),
            },
            Ok((Some(REFUSAL_EXIT), _)) => Proof::Unprovable(reason()),
            Ok((code, _)) => inconclusive(format!(
                "still-free inspection exited {code:?}; expected 0 or 2"
            )),
            Err(cause) => inconclusive(format!("still-free inspection: {cause}")),
        };
    }
    let (old_version, new_version) = (unit["old_version"].as_u64(), unit["new_version"].as_u64());
    if std::fs::write(&unit_file, unit.to_string()).is_err() {
        return inconclusive("could not write transformed unit to proof workspace");
    }
    match tool(&[
        "--build".as_ref(),
        "--unit-file".as_ref(),
        unit_file.as_os_str(),
        "--scratch".as_ref(),
        scratch.as_os_str(),
    ]) {
        Ok((Some(0), stdout)) => {
            let output = serde_json::from_slice::<serde_json::Value>(&stdout);
            if !output
                .as_ref()
                .ok()
                .and_then(|unit| unit["candidate"].as_str())
                .is_some_and(|id| id.len() == 40 && id.bytes().all(|byte| byte.is_ascii_hexdigit()))
            {
                return inconclusive(
                    "renumber build: malformed output (expected candidate commit)",
                );
            }
            Proof::Renumbered {
                old_version,
                new_version,
            }
        }
        Ok((Some(REFUSAL_EXIT), _)) => Proof::Unprovable(reason()),
        Ok((code, _)) => inconclusive(format!("renumber build exited {code:?}; expected 0 or 2")),
        Err(cause) => inconclusive(format!("renumber build: {cause}")),
    }
}

fn inconclusive(cause: impl Into<String>) -> Proof {
    Proof::Inconclusive {
        cause: InconclusiveCause::Infrastructure(cause.into()),
    }
}

/// Success must include a usable unit, not just a zero exit status.
fn parse_unit(stdout: &[u8]) -> Result<serde_json::Value, String> {
    let unit: serde_json::Value =
        serde_json::from_slice(stdout).map_err(|error| format!("malformed output: {error}"))?;
    if !unit.is_object()
        || unit["old_version"].as_u64().is_none()
        || unit["new_version"].as_u64().is_none()
    {
        return Err(
            "malformed output: expected a provisional unit with old_version and new_version".into(),
        );
    }
    Ok(unit)
}

fn tool_deadline(_repo: &Path, deadline: Instant) -> Instant {
    #[cfg(test)]
    if let Some(millis) = std::fs::read_to_string(_repo.join(".rsi-provisional-step-timeout-ms"))
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
    {
        return deadline.min(Instant::now() + Duration::from_millis(millis));
    }
    deadline
}

/// Run `command` for at most [`STEP_TIMEOUT`] and never past `deadline`.
fn run_step(
    command: Command,
    deadline: Instant,
    stderr: Stdio,
) -> Result<(Option<i32>, Vec<u8>), String> {
    let remaining = deadline
        .saturating_duration_since(Instant::now())
        .min(STEP_TIMEOUT);
    super::run_bounded(command, remaining, MAX_TOOL_STDOUT, stderr)
}
