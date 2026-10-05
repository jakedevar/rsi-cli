//! Operator adoption of legacy scratch (#1147).
//!
//! #1140 deletes only directories whose creation record binds them in the
//! allocation registry. Worker TMPDIRs made by hand, `/var/tmp/rsi-*` and every
//! `rsi-rolling-land-*` workspace made before #1140 have none, so the pass
//! retains them (`kept_unrecorded`). This module is the one sanctioned way to
//! make such a directory eligible, and only an operator surface calls it.
//!
//! **Authority.** Adoption RECORDS a directory; it DELETES NOTHING. It runs
//! the same proof as a reclaim pass with only the provenance step replaced (an
//! in-memory allocation built from the open descriptors stands in for the
//! registry entry): the root and ancestors authenticate, the candidate is a
//! real directory (never a symlink) of the daemon user on the root's mount, its
//! name is allowlisted for the root, it is old enough or its lander owner is
//! gone, no process or thread holds it, and every git repository below it is
//! clean and published. Only a candidate that would be reclaimed is adopted.
//! The later reclaim pass re-proves all of it again before it deletes anything.

use std::path::{Component, Path, PathBuf};
use std::time::{Instant, SystemTime};

use serde::Serialize;

use super::holders::ProofFailure;
use super::registry::Registry;
use super::{
    Assessment, Candidate, Decision, Pass, Provenance, RootKind, ScratchConfig, current_uid, fsys,
    parse_aside, record,
};

/// Most candidates one listing reports.
pub const MAX_LISTED: usize = 128;

/// Why a legacy directory was not (or could not be) adopted: the reclaim
/// proof's verdicts, minus provenance (the wire type is shared with the TUI).
pub use rsi_common::scratch_adopt::ScratchAdoptRefusal as AdoptRefusal;

const fn refusal_of(decision: Decision) -> AdoptRefusal {
    match decision {
        Decision::Young => AdoptRefusal::Young,
        Decision::Held => AdoptRefusal::Held,
        Decision::DirtyWorktree => AdoptRefusal::DirtyWorktree,
        Decision::Unpublished => AdoptRefusal::Unpublished,
        Decision::Changed => AdoptRefusal::Changed,
        Decision::Reclaim | Decision::Partial | Decision::Failed => AdoptRefusal::Failed,
        Decision::Unrecorded | Decision::Unproven => AdoptRefusal::Unproven,
    }
}

/// One unrecorded directory and whether it can be adopted now.
#[derive(Clone, Debug, Serialize)]
pub struct LegacyCandidate {
    pub path: PathBuf,
    /// The root kind tag (`worker_tmp`, `var_tmp`, `lander`).
    pub kind: &'static str,
    /// `None` when every proof passed (adoptable now).
    pub blocker: Option<AdoptRefusal>,
    pub bytes: u64,
    /// For an unproven holder proof: why, and the blocking processes (pid,
    /// comm, uid; kernel-reported, display only), on one bounded line.
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct LegacyListing {
    pub candidates: Vec<LegacyCandidate>,
    pub refused_roots: u32,
    pub budget_exhausted: bool,
}

/// The result for one requested path.
#[derive(Clone, Debug, Serialize)]
pub struct AdoptOutcome {
    pub path: PathBuf,
    pub adopted: bool,
    pub refusal: Option<AdoptRefusal>,
    /// As [`LegacyCandidate::detail`].
    pub detail: Option<String>,
}

impl AdoptOutcome {
    fn refused(path: &Path, refusal: AdoptRefusal, detail: Option<String>) -> Self {
        Self {
            path: path.to_path_buf(),
            adopted: false,
            refusal: Some(refusal),
            detail,
        }
    }
}

/// One bounded line naming why the holder proof was incomplete and which
/// processes blocked it (their `summary` carries pid, comm and uid).
fn describe_failure(failure: &ProofFailure) -> String {
    let mut text = format!("holder proof incomplete ({})", failure.cause);
    for blocker in &failure.blockers {
        text.push_str("; ");
        text.push_str(&blocker.summary);
    }
    if failure.omitted > 0 {
        text.push_str(&format!("; {} more not named", failure.omitted));
    }
    text
}

/// A refusal and, when the holder proof was the reason, its detail.
type Refused = (AdoptRefusal, Option<String>);

/// List every unrecorded, allowlisted directory under the configured roots with
/// the verdict an adoption would get now. Read-only.
#[must_use]
pub fn list_legacy(config: &ScratchConfig, now: SystemTime) -> LegacyListing {
    let uid = current_uid();
    let mut pass = Pass::new(config, now, Instant::now(), uid);
    let mut listing = LegacyListing::default();
    'roots: for (root_path, kind) in &config.roots {
        let root = match fsys::open_root(root_path, uid) {
            Ok(root) => root,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => {
                listing.refused_roots += 1;
                continue;
            }
        };
        let candidates = match pass.enumerate(&root, *kind) {
            Ok(candidates) => candidates,
            Err(_) if pass.budget.is_exhausted() => {
                listing.budget_exhausted = true;
                break 'roots;
            }
            Err(_) => {
                listing.refused_roots += 1;
                continue;
            }
        };
        for cand in candidates {
            if pass.budget.expired() || listing.candidates.len() >= MAX_LISTED {
                listing.budget_exhausted = true;
                break 'roots;
            }
            if parse_aside(&cand.name).is_some() {
                continue;
            }
            let path = root_path.join(&cand.name);
            pass.failure = None;
            let (blocker, bytes) =
                match pass.assess(&root, root_path, *kind, &cand, Provenance::Legacy) {
                    Assessment::Skip | Assessment::AlreadyRecorded => continue,
                    Assessment::Keep(decision) => (Some(refusal_of(decision)), 0),
                    Assessment::Reclaim(prepared) => (None, prepared.census.bytes),
                };
            let detail = pass.failure.take().as_ref().map(describe_failure);
            listing.candidates.push(LegacyCandidate {
                path,
                kind: kind.tag(),
                blocker,
                bytes,
                detail,
            });
        }
    }
    listing.budget_exhausted |= pass.budget.is_exhausted();
    listing
}

/// Adopt each of `paths`: run the full proof minus provenance and, only when it
/// passes, register and record the directory. Deletes nothing. One outcome per
/// path, in order.
#[must_use]
pub fn adopt_legacy(
    config: &ScratchConfig,
    now: SystemTime,
    paths: &[PathBuf],
) -> Vec<AdoptOutcome> {
    let uid = current_uid();
    let Ok(registry) = Registry::open_or_create(&config.registry, uid) else {
        return paths
            .iter()
            .map(|path| AdoptOutcome::refused(path, AdoptRefusal::Failed, None))
            .collect();
    };
    paths
        .iter()
        .map(|path| match adopt_one(config, now, &registry, uid, path) {
            Ok(()) => AdoptOutcome {
                path: path.clone(),
                adopted: true,
                refusal: None,
                detail: None,
            },
            Err((refusal, detail)) => AdoptOutcome::refused(path, refusal, detail),
        })
        .collect()
}

/// The configured root `path` is a direct child of, and its kind, when the
/// name is allowlisted for it. Lexical only: nothing is trusted until the root
/// is opened component by component.
fn locate<'c>(config: &'c ScratchConfig, path: &Path) -> Option<(&'c Path, RootKind, String)> {
    let clean = path.is_absolute()
        && path
            .components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_)));
    if !clean {
        return None;
    }
    let name = path.file_name()?.to_str()?.to_string();
    let parent = path.parent()?;
    let (root_path, kind) = config
        .roots
        .iter()
        .find(|(root_path, kind)| root_path == parent && kind.allows(&name))?;
    Some((root_path, *kind, name))
}

/// A path resolved against its root: the opened root and the candidate as it
/// stands now.
struct Resolved<'c> {
    root: fsys::Pinned,
    root_path: &'c Path,
    kind: RootKind,
    cand: Candidate,
}

fn resolve<'c>(
    config: &'c ScratchConfig,
    uid: u32,
    path: &Path,
) -> Result<Resolved<'c>, AdoptRefusal> {
    let (root_path, kind, name) = locate(config, path).ok_or(AdoptRefusal::OutsideRoots)?;
    let root = fsys::open_root(root_path, uid).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => AdoptRefusal::Missing,
        _ => AdoptRefusal::Unproven,
    })?;
    let stat = fsys::stat_at(root.raw(), &name).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => AdoptRefusal::Missing,
        _ => AdoptRefusal::Unproven,
    })?;
    if fsys::is_symlink(&stat) {
        return Err(AdoptRefusal::Symlink);
    }
    if !fsys::is_dir(&stat) {
        return Err(AdoptRefusal::NotDirectory);
    }
    let cand = Candidate {
        mtime_ns: fsys::mtime_ns(&stat),
        ident: fsys::ident_of(&stat),
        name,
    };
    Ok(Resolved {
        root,
        root_path,
        kind,
        cand,
    })
}

/// Assess `path` with a pass of its own, so no process inventory or other state
/// is shared with an earlier assessment.
fn assess_fresh<'c>(
    config: &'c ScratchConfig,
    now: SystemTime,
    uid: u32,
    path: &Path,
) -> Result<(Pass<'c>, Box<super::Prepared>), Refused> {
    let resolved = resolve(config, uid, path).map_err(|refusal| (refusal, None))?;
    let mut pass = Pass::new(config, now, Instant::now(), uid);
    match pass.assess(
        &resolved.root,
        resolved.root_path,
        resolved.kind,
        &resolved.cand,
        Provenance::Legacy,
    ) {
        Assessment::Skip => Err((AdoptRefusal::Missing, None)),
        Assessment::AlreadyRecorded => Err((AdoptRefusal::AlreadyRecorded, None)),
        Assessment::Keep(decision) => Err((
            refusal_of(decision),
            pass.failure.as_ref().map(describe_failure),
        )),
        Assessment::Reclaim(prepared) => Ok((pass, prepared)),
    }
}

/// Adopt one path. The whole proof runs twice, each time from the opened root
/// with a fresh process inventory; the record is written only if the second run
/// passes on the very same directory and tree as the first (a late holder, a
/// replaced directory or a changed tree is refused), immediately before the
/// write.
fn adopt_one(
    config: &ScratchConfig,
    now: SystemTime,
    registry: &Registry,
    uid: u32,
    path: &Path,
) -> Result<(), Refused> {
    // Keep only what is compared: the first run's open descriptors must be
    // closed before the second run, or this process would count as a holder.
    let (first_ident, first_manifest) = {
        let (_, first) = assess_fresh(config, now, uid, path)?;
        (first.pinned.ident, first.census.manifest.clone())
    };
    let (mut pass, last) = assess_fresh(config, now, uid, path)?;
    if last.pinned.ident != first_ident || last.census.manifest != first_manifest {
        return Err((AdoptRefusal::Changed, None));
    }
    // Still at its name: the record must not land in a directory that was moved
    // away between the proof and the write.
    let resolved = resolve(config, uid, path).map_err(|refusal| (refusal, None))?;
    if resolved.cand.ident != last.pinned.ident {
        return Err((AdoptRefusal::Changed, None));
    }
    record::adopt(
        registry,
        &last.pinned,
        &last.entry,
        &last.census.manifest,
        &mut pass.budget,
        || {
            #[cfg(test)]
            if let Some(hook) = config.after_record_write {
                hook(path);
            }
        },
    )
    .map_err(|_| (AdoptRefusal::Failed, None))
}
