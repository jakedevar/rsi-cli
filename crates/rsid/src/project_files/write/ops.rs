//! The unix implementation of every project-file write; the contract is in
//! `write.rs`, the handle-level primitives in `fsx.rs`.

use super::fsx::{self, Identity, Kind, Stamp, TreeError};
use super::{
    CONFLICT, EXISTS, FileOps, Hook, IO_FAILED, NO_LONGER_IN_TRASH, NOTHING_TO_UNDO, STALE,
    TOO_LARGE, UNDO_CONFLICT, UNSUPPORTED, components, ensure_supported, file_version,
    version_token,
};
use crate::error::{DaemonError, Result};
use crate::project_files::{
    INVALID_REQUEST, NOT_A_DIRECTORY, NOT_A_FILE, NOT_FOUND, OUTSIDE_ROOT, UNKNOWN_ROOT, refuse,
    resolve_in_root, root_string,
};
use cap_std::fs::Dir;
use rsi_common::project_files::{
    MAX_WRITE_BYTES, ProjectEntryKindV1, ProjectFileOperationKindV1,
    ProjectFileOperationResponseV1, ProjectFileUndoEffectV1, UNDO_DEPTH,
    UndoProjectFileOperationResponseV1, WriteProjectFileResponseV1,
};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Most `..` steps a confinement climb takes.
const MAX_CLIMB: usize = 4096;

/// One reversible operation, paths relative to the canonical root (`/`
/// separated), with the identity of what it left behind.
#[derive(Debug, Clone)]
pub(crate) enum UndoOp {
    /// An add: every entry it made, top first; each next entry is a child of
    /// the one before it.
    Created { entries: Vec<(String, Stamp)> },
    /// A copy: the published copy and the seal of its whole tree.
    Copied { path: String, seal: [u8; 32] },
    Moved {
        from: String,
        to: String,
        stamp: Stamp,
    },
    Trashed {
        original: String,
        batch: PathBuf,
        name: OsString,
        stamp: Stamp,
    },
}

/// The open root of one operation.
struct Root {
    path: PathBuf,
    dir: Dir,
    id: Identity,
    trash: Option<Identity>,
    /// The trash and its ancestors strictly below the root; empty when the
    /// trash is not under the root.
    trash_line: Vec<Identity>,
    /// Resolve configured links once, before any trash acquisition or mutation.
    trash_path: PathBuf,
}

/// An entry: its parent directory handle and its (unfollowed) name.
struct Place {
    dir: Dir,
    name: OsString,
    rel: String,
}

/// An entry whose parent may not exist yet: the deepest existing parent and
/// the missing directory names below it.
struct Probe {
    dir: Dir,
    base: String,
    missing: Vec<OsString>,
    name: OsString,
    rel: String,
}

/// A directory this call made, for a rollback.
struct Made {
    parent: Dir,
    name: OsString,
    id: Identity,
    rel: String,
}

struct Trashed {
    batch: PathBuf,
    name: OsString,
    stamp: Stamp,
}

type UndoResult<T> = std::result::Result<T, (DaemonError, bool)>;

/// Reuse the common absolute no-follow walker, then retain its pinned handle.
fn pinned_dir(path: &Path) -> io::Result<Dir> {
    rsi_common::asset_fs::AssetDir::open(path).map(|dir| Dir::from_std_file(dir.into_std_file()))
}

/// Choose the configured location (which may legitimately contain symlinks),
/// including missing descendants. Acquisition must not canonicalize it again.
fn trash_location(path: &Path) -> io::Result<PathBuf> {
    match path.canonicalize() {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let parent = path.parent().ok_or(error)?;
            let name = path
                .file_name()
                .ok_or_else(|| io::Error::other("invalid trash path"))?;
            Ok(trash_location(parent)?.join(name))
        }
        Err(error) => Err(error),
    }
}

/// Create missing trash ancestors through pinned parents. Existing components
/// always use AssetDir's walker; no canonicalization can bless a replacement.
fn create_trash(path: &Path) -> io::Result<Dir> {
    match pinned_dir(path) {
        Ok(dir) => Ok(dir),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let parent = path.parent().ok_or(error)?;
            let name = path
                .file_name()
                .ok_or_else(|| io::Error::other("invalid trash path"))?;
            let parent = create_trash(parent)?;
            match fsx::make_dir(&parent, name) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
            fsx::open_dir(&parent, name)
        }
        Err(error) => Err(error),
    }
}

/// Remove only the empty batch still named by this parent, and then its
/// sidecar. A replacement link is never followed or removed as a directory.
fn prune_batch(parent: &Dir, name: &OsStr, id: Identity) {
    if fsx::lstat(parent, name)
        .ok()
        .flatten()
        .is_some_and(|now| now.kind == Kind::Dir && now.id == id)
        && fsx::rmdir(parent, name).is_ok()
    {
        let _ = fsx::unlink(parent, Path::new(name).with_extension("origin").as_os_str());
    }
}

fn io_refusal(what: &str, error: impl std::fmt::Display) -> DaemonError {
    refuse(IO_FAILED, format!("{what}: {error}"))
}

fn has_code(error: &DaemonError, code: &str) -> bool {
    error.to_string().contains(&format!("{code}:"))
}

/// cap-std's refusal of a resolution that would leave the root.
fn is_escape(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::PermissionDenied
        && error.to_string().contains("outside of the filesystem")
}

fn open_refusal(requested: &str, error: io::Error) -> DaemonError {
    if is_escape(&error) {
        return refuse(
            OUTSIDE_ROOT,
            format!("'{requested}' resolves outside the root"),
        );
    }
    match error.kind() {
        io::ErrorKind::NotFound => refuse(
            NOT_FOUND,
            format!("the directory of '{requested}' does not exist"),
        ),
        io::ErrorKind::NotADirectory => refuse(
            NOT_A_DIRECTORY,
            format!("'{requested}': a parent is not a directory"),
        ),
        _ if fsx::is_loop(&error) => refuse(
            NOT_A_DIRECTORY,
            format!("'{requested}': a parent is a symlink"),
        ),
        _ => io_refusal(requested, error),
    }
}

/// A no-replace rename's refusal: an existing destination is
/// `project_file_exists`.
fn transfer_refusal(what: &str, error: io::Error) -> DaemonError {
    match error.kind() {
        io::ErrorKind::AlreadyExists => refuse(EXISTS, format!("'{what}' already exists")),
        io::ErrorKind::Unsupported => refuse(UNSUPPORTED, format!("'{what}': {error}")),
        _ => io_refusal(what, error),
    }
}

fn tree_refusal(what: &str, error: TreeError) -> DaemonError {
    match error {
        TreeError::Changed(_) => refuse(CONFLICT, format!("'{what}': {error}; nothing was lost")),
        TreeError::Special(_) => refuse(NOT_A_FILE, format!("'{what}': {error}")),
        TreeError::TooLarge => refuse(TOO_LARGE, format!("'{what}': {error}")),
        TreeError::Io(error) => io_refusal(what, error),
    }
}

/// A fresh private name beside `name` (`.<name>.rsi-<tag>-<id><suffix>`).
fn private_name(tag: &str, name: &OsStr, suffix: &str) -> OsString {
    let id = uuid::Uuid::new_v4().simple().to_string();
    let stem: String = name.to_string_lossy().chars().take(64).collect();
    OsString::from(format!(".{stem}.rsi-{tag}-{}{suffix}", &id[..12]))
}

fn join(parts: &[OsString]) -> String {
    parts
        .iter()
        .map(|part| part.to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn child_rel(parent: &str, name: &OsStr) -> String {
    if parent.is_empty() {
        name.to_string_lossy().into_owned()
    } else {
        format!("{parent}/{}", name.to_string_lossy())
    }
}

/// Walk `..` from `start` (inclusive) to `stop` (exclusive): the identities
/// passed and whether `stop` was reached before the filesystem root.
fn climb(start: &Dir, stop: Identity) -> io::Result<(Vec<Identity>, bool)> {
    let mut line = Vec::new();
    let mut current = start.try_clone()?;
    for _ in 0..MAX_CLIMB {
        let id = fsx::fstat(&current)?.id;
        if id == stop {
            return Ok((line, true));
        }
        line.push(id);
        let up = fsx::parent(&current)?;
        if fsx::fstat(&up)?.id == id {
            return Ok((line, false));
        }
        current = up;
    }
    Ok((line, false))
}

fn rollback(made: &[Made]) {
    for dir in made.iter().rev() {
        if fsx::lstat(&dir.parent, &dir.name)
            .ok()
            .flatten()
            .is_some_and(|now| now.id == dir.id)
        {
            let _ = fsx::rmdir(&dir.parent, &dir.name);
        }
    }
}

fn op_response(
    root: &Root,
    op: ProjectFileOperationKindV1,
    path: String,
    to: Option<String>,
    created_top: Option<String>,
    undo_depth: usize,
) -> ProjectFileOperationResponseV1 {
    ProjectFileOperationResponseV1 {
        root: root_string(&root.path),
        op,
        path,
        to,
        created_top,
        undo_depth,
    }
}

fn undo_response(
    root: &Root,
    undone: ProjectFileOperationKindV1,
    effect: ProjectFileUndoEffectV1,
    path: String,
    from: Option<String>,
) -> UndoProjectFileOperationResponseV1 {
    UndoProjectFileOperationResponseV1 {
        root: root_string(&root.path),
        undone,
        effect,
        path,
        from,
        undo_depth: 0,
    }
}

/// An undo error that keeps the operation unless the entry is gone.
fn keep(error: DaemonError) -> (DaemonError, bool) {
    let keep = !has_code(&error, NOT_FOUND);
    (error, keep)
}

fn kept(error: DaemonError) -> (DaemonError, bool) {
    (error, true)
}

/// An entry taken out from under its name (hub #3147): the identity check
/// and the act of an undo are one atomic rename onto a private name, so a
/// writer cannot swap the entry between them.
struct Detached {
    /// The entry under its private name.
    place: Place,
    stamp: Stamp,
    /// The name it had when it was taken.
    original: OsString,
}

impl Detached {
    /// Rename `place` to a private name beside it without replacing, and keep
    /// it only while `accept` approves what is there. A different entry is
    /// put back under its own name (`Ok(None)`, as when nothing was there).
    fn take(place: &Place, accept: impl FnOnce(&Stamp) -> bool) -> UndoResult<Option<Detached>> {
        let dir = place
            .dir
            .try_clone()
            .map_err(|e| kept(io_refusal(&place.rel, e)))?;
        let aside = private_name("undo", &place.name, "");
        match fsx::rename_noreplace(&dir, &place.name, &dir, &aside) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(kept(transfer_refusal(&place.rel, error))),
        }
        let seen = fsx::lstat(&dir, &aside).map_err(|e| kept(io_refusal(&place.rel, e)))?;
        let Some(seen) = seen else {
            return Ok(None);
        };
        let approved = accept(&seen);
        let taken = Detached {
            place: Place {
                dir,
                name: aside,
                rel: place.rel.clone(),
            },
            stamp: seen,
            original: place.name.clone(),
        };
        if approved {
            return Ok(Some(taken));
        }
        taken.restore()?;
        Ok(None)
    }

    /// Put the entry back under its own name, never replacing anything.
    fn restore(&self) -> UndoResult<()> {
        let place = &self.place;
        fsx::rename_noreplace(&place.dir, &place.name, &place.dir, &self.original).map_err(|_| {
            kept(refuse(
                UNDO_CONFLICT,
                format!(
                    "'{}' was replaced during the undo; the entry it held stays beside it as '{}'",
                    place.rel,
                    place.name.to_string_lossy()
                ),
            ))
        })
    }
}

impl FileOps {
    fn push(log: &mut HashMap<PathBuf, VecDeque<UndoOp>>, root: &Path, op: UndoOp) -> usize {
        let entries = log.entry(root.to_path_buf()).or_default();
        entries.push_back(op);
        while entries.len() > UNDO_DEPTH {
            entries.pop_front();
        }
        entries.len()
    }

    fn depth(log: &HashMap<PathBuf, VecDeque<UndoOp>>, root: &Path) -> usize {
        log.get(root).map_or(0, VecDeque::len)
    }

    /// A rename between two places, never replacing. Tests can make it
    /// cross filesystems.
    fn transfer(&self, from_dir: &Dir, from: &OsStr, to_dir: &Dir, to: &OsStr) -> io::Result<()> {
        #[cfg(test)]
        if self.cross_device.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(io::Error::from(io::ErrorKind::CrossesDevices));
        }
        fsx::rename_noreplace(from_dir, from, to_dir, to)
    }

    fn open_root(&self, path: &Path) -> Result<Root> {
        self.at(Hook::BeforeRootOpen);
        let dir = pinned_dir(path)
            .map_err(|error| refuse(UNKNOWN_ROOT, format!("{}: {error}", path.display())))?;
        let id = fsx::fstat(&dir).map_err(|e| io_refusal("the root", e))?.id;
        let trash_path = trash_location(&self.trash_dir).map_err(|e| io_refusal("the trash", e))?;
        self.at(Hook::TrashResolved);
        let (trash, trash_line) = match pinned_dir(&trash_path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => (None, Vec::new()),
            Err(error) => return Err(io_refusal("the trash", error)),
            Ok(trash_dir) => {
                let trash = fsx::fstat(&trash_dir)
                    .map_err(|e| io_refusal("the trash", e))?
                    .id;
                let (line, under) =
                    climb(&trash_dir, id).map_err(|e| io_refusal("the trash", e))?;
                (Some(trash), if under { line } else { Vec::new() })
            }
        };
        if let Some(trash) = trash
            && climb(&dir, trash).map_err(|e| io_refusal("the root", e))?.1
        {
            return Err(refuse(
                INVALID_REQUEST,
                "the root is inside the daemon's project-file trash",
            ));
        }
        Ok(Root {
            path: path.to_path_buf(),
            dir,
            id,
            trash,
            trash_line,
            trash_path,
        })
    }

    /// Resolve the parent of `requested` beneath the root as far as it
    /// exists.
    fn probe(&self, root: &Root, requested: &str) -> Result<Probe> {
        let parts = components(requested)?;
        let (name, parents) = parts.split_last().expect("components is non-empty");
        let mut dir = root.dir.try_clone().map_err(|e| io_refusal(requested, e))?;
        let mut found = 0;
        for depth in 1..=parents.len() {
            let prefix: PathBuf = parents[..depth].iter().collect();
            match root.dir.open_dir(&prefix) {
                Ok(next) => {
                    dir = next;
                    found = depth;
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => break,
                Err(error) => return Err(open_refusal(requested, error)),
            }
        }
        Ok(Probe {
            dir,
            base: join(&parents[..found]),
            missing: parents[found..].to_vec(),
            name: name.clone(),
            rel: join(&parts),
        })
    }

    /// The entry `requested` names; its parent must exist.
    fn locate(&self, root: &Root, requested: &str) -> Result<Place> {
        let probe = self.probe(root, requested)?;
        if !probe.missing.is_empty() {
            return Err(refuse(
                NOT_FOUND,
                format!("the directory of '{requested}' does not exist"),
            ));
        }
        Ok(Place {
            dir: probe.dir,
            name: probe.name,
            rel: probe.rel,
        })
    }

    /// Create the missing parents of a probed entry, one handle at a time.
    fn materialize(&self, probe: Probe, requested: &str) -> Result<(Place, Vec<Made>)> {
        let Probe {
            mut dir,
            base,
            missing,
            name,
            rel,
        } = probe;
        let mut made = Vec::new();
        let mut at = base;
        for part in missing {
            at = child_rel(&at, &part);
            let created = match fsx::make_dir(&dir, &part) {
                Ok(()) => true,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => false,
                Err(error) => {
                    rollback(&made);
                    return Err(io_refusal(requested, error));
                }
            };
            let child = match fsx::open_dir(&dir, &part) {
                Ok(child) => child,
                Err(error) => {
                    rollback(&made);
                    return Err(open_refusal(requested, error));
                }
            };
            if created {
                let id = match fsx::fstat(&child) {
                    Ok(stamp) => stamp.id,
                    Err(error) => {
                        rollback(&made);
                        return Err(io_refusal(requested, error));
                    }
                };
                made.push(Made {
                    parent: dir,
                    name: part,
                    id,
                    rel: at.clone(),
                });
            }
            dir = child;
        }
        Ok((Place { dir, name, rel }, made))
    }

    /// Confinement and trash protection of an entry in `dir`: `dir` is still
    /// beneath the root and not in the daemon trash, and `entry` (a
    /// directory) does not hold the trash. Returns the identities from `dir`
    /// up to the root.
    fn guard(
        &self,
        root: &Root,
        dir: &Dir,
        entry: Option<&Stamp>,
        rel: &str,
    ) -> Result<Vec<Identity>> {
        let (line, under) = climb(dir, root.id).map_err(|e| io_refusal(rel, e))?;
        if !under {
            return Err(refuse(
                OUTSIDE_ROOT,
                format!("'{rel}' is no longer under the root"),
            ));
        }
        if root.trash.is_some_and(|trash| line.contains(&trash)) {
            return Err(refuse(
                INVALID_REQUEST,
                format!("'{rel}' is in the daemon's project-file trash"),
            ));
        }
        if entry.is_some_and(|entry| entry.kind == Kind::Dir && root.trash_line.contains(&entry.id))
        {
            return Err(refuse(
                INVALID_REQUEST,
                format!("'{rel}' holds the daemon's project-file trash"),
            ));
        }
        Ok(line)
    }

    /// Save `content` to `requested` (see `WriteProjectFileRequestV1`).
    pub(crate) fn save(
        &self,
        root: &Path,
        requested: &str,
        content: &str,
        expected_version: Option<&str>,
        force: bool,
    ) -> Result<WriteProjectFileResponseV1> {
        ensure_supported()?;
        if content.len() > MAX_WRITE_BYTES {
            return Err(refuse(
                TOO_LARGE,
                format!(
                    "content is {} bytes; the limit is {MAX_WRITE_BYTES}",
                    content.len()
                ),
            ));
        }
        let _log = self.log();
        let root = self.open_root(root)?;
        let mut place = self.locate(&root, requested)?;
        let lstat = |place: &Place| {
            fsx::lstat(&place.dir, &place.name).map_err(|e| io_refusal(requested, e))
        };
        let mut leaf = lstat(&place)?;
        if leaf.is_some_and(|stamp| stamp.kind == Kind::Symlink) {
            // The file's own symlink is saved through, to a file in the root.
            let scoped = resolve_in_root(&root.path, requested).map_err(|error| {
                if has_code(&error, NOT_FOUND) {
                    refuse(NOT_A_FILE, format!("'{requested}' is a dangling symlink"))
                } else {
                    error
                }
            })?;
            place = self.locate(&root, &scoped.real_relative)?;
            leaf = lstat(&place)?;
            if leaf.is_none_or(|stamp| stamp.kind == Kind::Symlink) {
                return Err(refuse(
                    STALE,
                    format!("'{requested}' changed while it was resolved"),
                ));
            }
        }
        // hub #3149: the resolved target, not only the requested name.
        self.guard(&root, &place.dir, None, &place.rel)?;
        let (observed, mode) = match leaf {
            None if !force => {
                return Err(refuse(
                    STALE,
                    format!(
                        "'{requested}' does not exist on disk; save with force:true to recreate it"
                    ),
                ));
            }
            None => (None, None),
            // hub #3150: decided from lstat; a FIFO or device is never opened.
            Some(stamp) if stamp.kind != Kind::File => {
                return Err(refuse(
                    NOT_A_FILE,
                    format!("'{requested}' is not a regular file"),
                ));
            }
            Some(stamp) => (
                Some(self.check_version(&place, stamp, requested, expected_version, force)?),
                Some(stamp.mode),
            ),
        };
        let mismatch = observed.is_none_or(|(_, matches)| !matches);
        self.at(Hook::Checked);

        let temp_name = private_name("save", &place.name, ".tmp");
        let mut temp = fsx::create_file(&place.dir, &temp_name, 0o666)
            .map_err(|e| io_refusal(requested, e))?;
        let temp_id = match fsx::fstat(&temp) {
            Ok(stamp) => stamp.id,
            Err(error) => {
                let _ = fsx::unlink(&place.dir, &temp_name);
                return Err(io_refusal(requested, error));
            }
        };
        let discard = || {
            let _ = fsx::unlink_if(&place.dir, &temp_name, temp_id);
        };
        let written = (|| -> io::Result<fs::Metadata> {
            temp.write_all(content.as_bytes())?;
            if let Some(mode) = mode {
                fsx::set_mode(&temp, mode)?;
            }
            temp.sync_all()?;
            temp.metadata()
        })();
        let metadata = match written {
            Ok(metadata) => metadata,
            Err(error) => {
                discard();
                return Err(io_refusal(requested, error));
            }
        };
        drop(temp);
        // hub #3146: the answer is the published inode's own version.
        let version = version_token(&metadata, &Sha256::digest(content.as_bytes()));

        self.at(Hook::BeforePublish);
        let published = self.publish_save(
            &place,
            &temp_name,
            observed.map(|(stamp, _)| stamp),
            force,
            requested,
        );
        let (raced, created) = published.inspect_err(|_| discard())?;
        self.at(Hook::AfterPublish);
        Ok(WriteProjectFileResponseV1 {
            root: root_string(&root.path),
            path: place.rel,
            size: metadata.len(),
            version,
            forced: mismatch || raced,
            created,
        })
    }

    /// The version check of an existing regular file: answers the stamp it
    /// checked and whether `expected_version` matched.
    fn check_version(
        &self,
        place: &Place,
        stamp: Stamp,
        requested: &str,
        expected_version: Option<&str>,
        force: bool,
    ) -> Result<(Stamp, bool)> {
        let changed = || {
            refuse(
                STALE,
                format!("'{requested}' changed while it was checked; reload it"),
            )
        };
        let mut file = match fsx::open_file(&place.dir, &place.name) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::InvalidInput => {
                return Err(refuse(
                    NOT_A_FILE,
                    format!("'{requested}' is not a regular file"),
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound || fsx::is_loop(&error) => {
                return Err(changed());
            }
            Err(error) => return Err(io_refusal(requested, error)),
        };
        let before = fsx::fstat(&file).map_err(|e| io_refusal(requested, e))?;
        if before.id != stamp.id {
            return Err(changed());
        }
        let current = file_version(&mut file).map_err(|e| io_refusal(requested, e))?;
        let after = fsx::fstat(&file).map_err(|e| io_refusal(requested, e))?;
        let current = current.filter(|_| before.untouched(&after));
        let matches = current.is_some() && current.as_deref() == expected_version;
        if !matches && !force {
            return Err(refuse(
                STALE,
                format!(
                    "'{requested}' changed on disk (current version {}); reload it or save with force:true",
                    current
                        .as_deref()
                        .unwrap_or("none: too large to version or changing")
                ),
            ));
        }
        Ok((after, matches))
    }

    /// Publish a save's temporary file: over the target only when it is
    /// still the checked, untouched object (or `force`), never over a file
    /// that appeared. Answers (published over a concurrent change, created).
    fn publish_save(
        &self,
        place: &Place,
        temp: &OsStr,
        observed: Option<Stamp>,
        force: bool,
        requested: &str,
    ) -> Result<(bool, bool)> {
        let create = || {
            fsx::rename_noreplace(&place.dir, temp, &place.dir, &place.name)
                .map(|()| (true, true))
                .map_err(|error| match error.kind() {
                    io::ErrorKind::AlreadyExists => refuse(
                        STALE,
                        format!("'{requested}' was created on disk while it was saved; reload it"),
                    ),
                    _ => transfer_refusal(requested, error),
                })
        };
        let Some(observed) = observed else {
            return create().map(|(_, created)| (false, created));
        };
        // Immediately before the rename: still the object checked, untouched.
        let now = fsx::lstat(&place.dir, &place.name).map_err(|e| io_refusal(requested, e))?;
        let replace = |raced| {
            fsx::rename_replace(&place.dir, temp, &place.dir, &place.name)
                .map(|()| (raced, false))
                .map_err(|e| io_refusal(requested, e))
        };
        match now {
            Some(now) if now.untouched(&observed) => replace(false),
            Some(_) | None if !force => Err(refuse(
                STALE,
                format!(
                    "'{requested}' changed on disk while it was saved; reload it or save with force:true"
                ),
            )),
            Some(now) if now.kind != Kind::File => Err(refuse(
                NOT_A_FILE,
                format!("'{requested}' is not a regular file"),
            )),
            Some(_) => replace(true),
            None => create(),
        }
    }

    /// Add an empty file or a directory, creating missing parents.
    pub(crate) fn create(
        &self,
        root: &Path,
        requested: &str,
        kind: ProjectEntryKindV1,
    ) -> Result<ProjectFileOperationResponseV1> {
        ensure_supported()?;
        if kind == ProjectEntryKindV1::Symlink {
            return Err(refuse(INVALID_REQUEST, "kind must be file or dir"));
        }
        let mut log = self.log();
        let root = self.open_root(root)?;
        let probe = self.probe(&root, requested)?;
        self.guard(&root, &probe.dir, None, &probe.rel)?;
        if probe.missing.is_empty()
            && fsx::lstat(&probe.dir, &probe.name)
                .map_err(|e| io_refusal(requested, e))?
                .is_some()
        {
            return Err(refuse(EXISTS, format!("'{}' already exists", probe.rel)));
        }
        self.at(Hook::Checked);
        let (place, made) = self.materialize(probe, requested)?;
        let leaf = if kind == ProjectEntryKindV1::Dir {
            fsx::make_dir(&place.dir, &place.name)
        } else {
            fsx::create_file(&place.dir, &place.name, 0o666).map(drop)
        };
        if let Err(error) = leaf {
            rollback(&made);
            return Err(match error.kind() {
                io::ErrorKind::AlreadyExists => {
                    refuse(EXISTS, format!("'{}' already exists", place.rel))
                }
                _ => io_refusal(requested, error),
            });
        }
        // Stamped once everything is made (a child moves its parent's mtime).
        let stamp_of = |dir: &Dir, name: &OsStr| -> Result<Stamp> {
            fsx::lstat(dir, name)
                .map_err(|e| io_refusal(requested, e))?
                .ok_or_else(|| {
                    refuse(
                        CONFLICT,
                        format!("'{requested}' was removed while it was added"),
                    )
                })
        };
        let mut entries = Vec::with_capacity(made.len() + 1);
        for dir in &made {
            entries.push((dir.rel.clone(), stamp_of(&dir.parent, &dir.name)?));
        }
        entries.push((place.rel.clone(), stamp_of(&place.dir, &place.name)?));
        let top = entries[0].0.clone();
        let depth = Self::push(&mut log, &root.path, UndoOp::Created { entries });
        Ok(op_response(
            &root,
            ProjectFileOperationKindV1::Created,
            place.rel,
            None,
            Some(top),
            depth,
        ))
    }

    /// The source and destination of a move or copy, checked.
    fn transfer_ends(&self, root: &Root, from: &str, to: &str) -> Result<(Place, Stamp, Probe)> {
        let source = self.locate(root, from)?;
        let Some(stamp) = fsx::lstat(&source.dir, &source.name).map_err(|e| io_refusal(from, e))?
        else {
            return Err(refuse(NOT_FOUND, format!("'{from}' does not exist")));
        };
        let dest = self.probe(root, to)?;
        self.guard(root, &source.dir, Some(&stamp), &source.rel)?;
        let line = self.guard(root, &dest.dir, None, &dest.rel)?;
        if dest.missing.is_empty()
            && fsx::lstat(&dest.dir, &dest.name)
                .map_err(|e| io_refusal(to, e))?
                .is_some()
        {
            return Err(refuse(EXISTS, format!("'{}' already exists", dest.rel)));
        }
        if stamp.kind == Kind::Dir && line.contains(&stamp.id) {
            return Err(refuse(
                INVALID_REQUEST,
                format!("cannot put '{}' inside itself", source.rel),
            ));
        }
        Ok((source, stamp, dest))
    }

    /// Rename or move one entry (not followed) to a path that does not exist.
    pub(crate) fn rename(
        &self,
        root: &Path,
        from: &str,
        to: &str,
    ) -> Result<ProjectFileOperationResponseV1> {
        ensure_supported()?;
        let mut log = self.log();
        let root = self.open_root(root)?;
        let (source, stamp, dest) = self.transfer_ends(&root, from, to)?;
        self.at(Hook::Checked);
        let (dest, made) = self.materialize(dest, to)?;
        let moved = match self.transfer(&source.dir, &source.name, &dest.dir, &dest.name) {
            Ok(()) => fsx::lstat(&dest.dir, &dest.name)
                .map_err(|e| io_refusal(to, e))?
                .ok_or_else(|| refuse(CONFLICT, format!("'{to}' vanished after the move"))),
            Err(error) if error.kind() == io::ErrorKind::CrossesDevices => {
                self.move_across(&root, &source, stamp, &dest)
            }
            Err(error) => Err(transfer_refusal(&dest.rel, error)),
        };
        let moved = moved.inspect_err(|_| rollback(&made))?;
        let depth = Self::push(
            &mut log,
            &root.path,
            UndoOp::Moved {
                from: source.rel.clone(),
                to: dest.rel.clone(),
                stamp: moved,
            },
        );
        Ok(op_response(
            &root,
            ProjectFileOperationKindV1::Moved,
            source.rel,
            Some(dest.rel),
            None,
            depth,
        ))
    }

    /// A move between filesystems (hub #3148): copy under a private name
    /// beside the destination, check that the source is still exactly what
    /// was copied, publish without replacing, then move the source into the
    /// trash (never unlinked). Answers the published entry's stamp.
    fn move_across(
        &self,
        root: &Root,
        source: &Place,
        stamp: Stamp,
        dest: &Place,
    ) -> Result<Stamp> {
        let staging = private_name("move", &dest.name, "");
        let discard = || {
            let _ = fsx::remove_owned(&dest.dir, &staging);
        };
        let copied = fsx::copy_tree(&source.dir, &source.name, &dest.dir, &staging, &mut || {
            self.at(Hook::CopiedEntry)
        })
        .map_err(|error| {
            discard();
            tree_refusal(&source.rel, error)
        })?;
        self.at(Hook::CrossDeviceCopied);
        let unchanged = copied
            .entries
            .first()
            .is_some_and(|(_, top)| top.same_object(&stamp))
            && fsx::inventory(&source.dir, &source.name).is_ok_and(|now| copied.untouched_in(&now));
        if !unchanged {
            discard();
            return Err(refuse(
                CONFLICT,
                format!(
                    "'{}' changed while it was copied to another filesystem; nothing was moved",
                    source.rel
                ),
            ));
        }
        if let Err(error) = fsx::rename_noreplace(&dest.dir, &staging, &dest.dir, &dest.name) {
            discard();
            return Err(transfer_refusal(&dest.rel, error));
        }
        let published = fsx::lstat(&dest.dir, &dest.name)
            .map_err(|e| io_refusal(&dest.rel, e))?
            .ok_or_else(|| refuse(CONFLICT, format!("'{}' vanished after the move", dest.rel)))?;
        self.move_to_trash(root, source).map_err(|error| {
            refuse(
                CONFLICT,
                format!(
                    "copied '{}' to '{}' but its source stays: {error}",
                    source.rel, dest.rel
                ),
            )
        })?;
        Ok(published)
    }

    /// Copy one entry (recursively, symlinks as links) to a new path.
    pub(crate) fn copy(
        &self,
        root: &Path,
        from: &str,
        to: &str,
    ) -> Result<ProjectFileOperationResponseV1> {
        ensure_supported()?;
        let mut log = self.log();
        let root = self.open_root(root)?;
        let (source, _, dest) = self.transfer_ends(&root, from, to)?;
        self.at(Hook::Checked);
        let (dest, made) = self.materialize(dest, to)?;
        // hub #3144: built under a private name, published without
        // replacing; a failure removes only the private copy.
        let staging = private_name("copy", &dest.name, "");
        let fail = |error: DaemonError| {
            let _ = fsx::remove_owned(&dest.dir, &staging);
            rollback(&made);
            error
        };
        fsx::copy_tree(&source.dir, &source.name, &dest.dir, &staging, &mut || {
            self.at(Hook::CopiedEntry)
        })
        .map_err(|error| fail(tree_refusal(from, error)))?;
        fsx::rename_noreplace(&dest.dir, &staging, &dest.dir, &dest.name)
            .map_err(|error| fail(transfer_refusal(&dest.rel, error)))?;
        let seal = fsx::inventory(&dest.dir, &dest.name)
            .map_err(|error| tree_refusal(to, error))?
            .seal();
        let depth = Self::push(
            &mut log,
            &root.path,
            UndoOp::Copied {
                path: dest.rel.clone(),
                seal,
            },
        );
        Ok(op_response(
            &root,
            ProjectFileOperationKindV1::Copied,
            source.rel,
            Some(dest.rel),
            None,
            depth,
        ))
    }

    /// Move one entry into a fresh trash batch. Across filesystems the entry
    /// is copied into the batch and removed only where it is still exactly
    /// what was copied (hub #3148).
    fn move_to_trash(&self, root: &Root, place: &Place) -> Result<Trashed> {
        let rel = place.rel.as_str();
        let trash = create_trash(&root.trash_path).map_err(|e| io_refusal("the trash", e))?;
        let id = uuid::Uuid::new_v4().simple().to_string();
        let batch_name = format!(
            "{}-{}",
            chrono::Utc::now().format("%Y%m%dT%H%M%S"),
            &id[..8]
        );
        let batch = root.trash_path.join(&batch_name);
        let batch_name = OsStr::new(&batch_name);
        fsx::make_dir(&trash, batch_name).map_err(|e| io_refusal("the trash", e))?;
        self.at(Hook::TrashBatchCreated);
        // Check the full hierarchy again without following links. A swapped
        // ancestor or batch cannot redirect either the sidecar or the entry.
        let batch_dir =
            fsx::open_dir(&trash, batch_name).map_err(|e| io_refusal("the trash", e))?;
        let batch_id = fsx::fstat(&batch_dir)
            .map_err(|e| io_refusal("the trash", e))?
            .id;
        let current = pinned_dir(&batch).map_err(|e| io_refusal("the trash", e))?;
        if fsx::fstat(&current)
            .map_err(|e| io_refusal("the trash", e))?
            .id
            != batch_id
        {
            return Err(refuse(CONFLICT, "the trash batch was replaced"));
        }
        let discard = || prune_batch(&trash, batch_name, batch_id);
        let origin = Path::new(batch_name).with_extension("origin");
        let mut sidecar = fsx::create_file(&trash, origin.as_os_str(), 0o600).map_err(|e| {
            // A conflicting sidecar belongs to somebody else.
            let _ = fsx::rmdir(&trash, batch_name);
            io_refusal("the trash origin", e)
        })?;
        sidecar
            .write_all(root.path.join(rel).to_string_lossy().as_bytes())
            .map_err(|e| {
                discard();
                io_refusal("the trash origin", e)
            })?;
        match self.transfer(&place.dir, &place.name, &batch_dir, &place.name) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::CrossesDevices => {
                let copied = fsx::copy_tree(
                    &place.dir,
                    &place.name,
                    &batch_dir,
                    &place.name,
                    &mut || self.at(Hook::CopiedEntry),
                )
                .map_err(|error| {
                    let _ = fsx::remove_owned(&batch_dir, &place.name);
                    discard();
                    tree_refusal(rel, error)
                })?;
                self.at(Hook::CrossDeviceCopied);
                let left = fsx::remove_verified(&place.dir, &place.name, &copied)
                    .map_err(|e| io_refusal(rel, e))?;
                if left > 0 {
                    return Err(refuse(
                        CONFLICT,
                        format!(
                            "'{rel}': {left} entries changed or appeared while it was trashed to another filesystem and stay in place; trash batch {} holds the copy",
                            batch_name.to_string_lossy()
                        ),
                    ));
                }
            }
            Err(error) => {
                discard();
                return Err(transfer_refusal(rel, error));
            }
        }
        let stamp = fsx::lstat(&batch_dir, &place.name)
            .map_err(|e| io_refusal("the trash", e))?
            .ok_or_else(|| refuse(CONFLICT, format!("the trashed '{rel}' vanished")))?;
        Ok(Trashed {
            batch,
            name: place.name.clone(),
            stamp,
        })
    }

    /// Move one entry (not followed) into the daemon-owned trash.
    pub(crate) fn trash(
        &self,
        root: &Path,
        requested: &str,
    ) -> Result<ProjectFileOperationResponseV1> {
        ensure_supported()?;
        let mut log = self.log();
        let root = self.open_root(root)?;
        let place = self.locate(&root, requested)?;
        let Some(stamp) =
            fsx::lstat(&place.dir, &place.name).map_err(|e| io_refusal(requested, e))?
        else {
            return Err(refuse(NOT_FOUND, format!("'{requested}' does not exist")));
        };
        self.guard(&root, &place.dir, Some(&stamp), &place.rel)?;
        self.at(Hook::Checked);
        let trashed = self.move_to_trash(&root, &place)?;
        let depth = Self::push(
            &mut log,
            &root.path,
            UndoOp::Trashed {
                original: place.rel.clone(),
                batch: trashed.batch,
                name: trashed.name,
                stamp: trashed.stamp,
            },
        );
        self.prune_if_due();
        Ok(op_response(
            &root,
            ProjectFileOperationKindV1::Trashed,
            place.rel,
            None,
            None,
            depth,
        ))
    }

    /// Reverse the root's most recent operation. A conflict keeps the entry
    /// so the undo can be retried.
    pub(crate) fn undo(&self, root: &Path) -> Result<UndoProjectFileOperationResponseV1> {
        ensure_supported()?;
        let mut log = self.log();
        let Some(op) = log.get_mut(root).and_then(VecDeque::pop_back) else {
            return Err(refuse(
                NOTHING_TO_UNDO,
                "no operation to undo for this root",
            ));
        };
        let result = self
            .open_root(root)
            .map_err(kept)
            .and_then(|opened| self.apply_undo(&opened, &op));
        match result {
            Ok(mut response) => {
                response.undo_depth = Self::depth(&log, root);
                Ok(response)
            }
            Err((error, keep)) => {
                if keep {
                    log.entry(root.to_path_buf()).or_default().push_back(op);
                }
                Err(error)
            }
        }
    }

    fn apply_undo(
        &self,
        root: &Root,
        op: &UndoOp,
    ) -> UndoResult<UndoProjectFileOperationResponseV1> {
        match op {
            UndoOp::Trashed {
                original,
                batch,
                name,
                stamp,
            } => self.undo_trash(root, original, batch, name, stamp),
            UndoOp::Moved { from, to, stamp } => self.undo_move(root, from, to, stamp),
            UndoOp::Copied { path, seal } => self.undo_copy(root, path, seal),
            UndoOp::Created { entries } => self.undo_create(root, entries),
        }
    }

    /// The probed endpoint an undo restores to: confined, out of the trash,
    /// and free.
    fn undo_destination(
        &self,
        root: &Root,
        path: &str,
        what: &str,
    ) -> UndoResult<(Probe, Vec<Identity>)> {
        let dest = self.probe(root, path).map_err(kept)?;
        // hub #3149: an undo's endpoints are trash-protected too.
        let line = self.guard(root, &dest.dir, None, &dest.rel).map_err(kept)?;
        if dest.missing.is_empty()
            && fsx::lstat(&dest.dir, &dest.name)
                .map_err(|e| kept(io_refusal(path, e)))?
                .is_some()
        {
            return Err(kept(refuse(
                UNDO_CONFLICT,
                format!("cannot {what} '{path}': it exists again"),
            )));
        }
        Ok((dest, line))
    }

    fn undo_trash(
        &self,
        root: &Root,
        original: &str,
        batch: &Path,
        name: &OsStr,
        stamp: &Stamp,
    ) -> UndoResult<UndoProjectFileOperationResponseV1> {
        // local #120: a pruned (or hand-removed) batch drops the entry.
        let gone = || {
            (
                refuse(
                    NO_LONGER_IN_TRASH,
                    format!(
                        "'{original}' is no longer in the trash: its batch was pruned (the trash keeps entries 7 days) or removed"
                    ),
                ),
                false,
            )
        };
        self.at(Hook::BeforeTrashOpen);
        let batch_parent = match pinned_dir(batch.parent().expect("recorded batch has a parent")) {
            Ok(dir) => dir,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Err(gone()),
            Err(error) => return Err(kept(io_refusal("the trash", error))),
        };
        let batch_name = batch.file_name().expect("recorded batch has a name");
        let batch_dir = match fsx::open_dir(&batch_parent, batch_name) {
            Ok(dir) => dir,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Err(gone()),
            Err(error) => return Err(kept(io_refusal("the trash", error))),
        };
        let batch_id = fsx::fstat(&batch_dir)
            .map_err(|e| kept(io_refusal("the trash", e)))?
            .id;
        match fsx::lstat(&batch_dir, name).map_err(|e| kept(io_refusal("the trash", e)))? {
            None => return Err(gone()),
            Some(now) if !now.same_entry(stamp) => {
                return Err(kept(refuse(
                    UNDO_CONFLICT,
                    format!("the trashed copy of '{original}' changed in the trash"),
                )));
            }
            Some(_) => {}
        }
        let (dest, _) = self.undo_destination(root, original, "restore")?;
        self.at(Hook::Checked);
        let (dest, made) = self.materialize(dest, original).map_err(kept)?;
        let conflict = || {
            kept(refuse(
                UNDO_CONFLICT,
                format!("cannot restore '{original}': it exists again"),
            ))
        };
        match self.transfer(&batch_dir, name, &dest.dir, &dest.name) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                rollback(&made);
                return Err(conflict());
            }
            Err(error) if error.kind() == io::ErrorKind::CrossesDevices => {
                // Restore a copy; the trash copy goes only where it is still
                // exactly what was copied.
                let staging = private_name("restore", &dest.name, "");
                let discard = || {
                    let _ = fsx::remove_owned(&dest.dir, &staging);
                    rollback(&made);
                };
                let copied = fsx::copy_tree(&batch_dir, name, &dest.dir, &staging, &mut || {
                    self.at(Hook::CopiedEntry)
                })
                .map_err(|error| {
                    discard();
                    kept(tree_refusal(original, error))
                })?;
                if let Err(error) =
                    fsx::rename_noreplace(&dest.dir, &staging, &dest.dir, &dest.name)
                {
                    discard();
                    return Err(if error.kind() == io::ErrorKind::AlreadyExists {
                        conflict()
                    } else {
                        kept(transfer_refusal(original, error))
                    });
                }
                let _ = fsx::remove_verified(&batch_dir, name, &copied);
            }
            Err(error) => {
                rollback(&made);
                return Err(kept(transfer_refusal(original, error)));
            }
        }
        self.at(Hook::BeforeTrashPrune);
        prune_batch(&batch_parent, batch_name, batch_id);
        Ok(undo_response(
            root,
            ProjectFileOperationKindV1::Trashed,
            ProjectFileUndoEffectV1::Restored,
            dest.rel,
            None,
        ))
    }

    fn undo_move(
        &self,
        root: &Root,
        from: &str,
        to: &str,
        stamp: &Stamp,
    ) -> UndoResult<UndoProjectFileOperationResponseV1> {
        let source = self.locate(root, to).map_err(keep)?;
        let Some(now) =
            fsx::lstat(&source.dir, &source.name).map_err(|e| kept(io_refusal(to, e)))?
        else {
            return Err((refuse(NOT_FOUND, format!("'{to}' no longer exists")), false));
        };
        // hub #3147: only the object the move left behind is moved back.
        if !now.same_entry(stamp) {
            return Err(kept(refuse(
                UNDO_CONFLICT,
                format!("'{to}' was replaced or changed since the move"),
            )));
        }
        self.guard(root, &source.dir, Some(&now), &source.rel)
            .map_err(kept)?;
        let (dest, line) = self.undo_destination(root, from, "move back to")?;
        if now.kind == Kind::Dir && line.contains(&now.id) {
            return Err(kept(refuse(
                INVALID_REQUEST,
                format!("cannot put '{to}' inside itself"),
            )));
        }
        self.at(Hook::Checked);
        let (dest, made) = self.materialize(dest, from).map_err(kept)?;
        // Take the entry out from under its name; what is moved is what was
        // checked, whatever a writer does to the name meanwhile.
        let taken = match Detached::take(&source, |seen| seen.same_entry(stamp)) {
            Ok(Some(taken)) => taken,
            other => {
                rollback(&made);
                return Err(match other {
                    Err(error) => error,
                    _ => kept(refuse(
                        UNDO_CONFLICT,
                        format!("'{to}' was replaced or changed since the move"),
                    )),
                });
            }
        };
        let moved = match self.transfer(&taken.place.dir, &taken.place.name, &dest.dir, &dest.name)
        {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::CrossesDevices => self
                .move_across(root, &taken.place, taken.stamp, &dest)
                .map(drop),
            Err(error) => Err(transfer_refusal(from, error)),
        };
        if let Err(error) = moved {
            let restored = taken.restore();
            rollback(&made);
            restored?;
            return Err(kept(if has_code(&error, EXISTS) {
                refuse(
                    UNDO_CONFLICT,
                    format!("cannot move back to '{from}': it exists again"),
                )
            } else {
                error
            }));
        }
        Ok(undo_response(
            root,
            ProjectFileOperationKindV1::Moved,
            ProjectFileUndoEffectV1::MovedBack,
            dest.rel,
            Some(source.rel),
        ))
    }

    fn undo_copy(
        &self,
        root: &Root,
        path: &str,
        seal: &[u8; 32],
    ) -> UndoResult<UndoProjectFileOperationResponseV1> {
        let place = self.locate(root, path).map_err(keep)?;
        let Some(now) =
            fsx::lstat(&place.dir, &place.name).map_err(|e| kept(io_refusal(path, e)))?
        else {
            return Err((
                refuse(NOT_FOUND, format!("'{path}' no longer exists")),
                false,
            ));
        };
        self.guard(root, &place.dir, Some(&now), &place.rel)
            .map_err(kept)?;
        // hub #3147: the whole copied tree, exactly as the copy left it.
        let tree = fsx::inventory(&place.dir, &place.name)
            .map_err(|error| kept(tree_refusal(path, error)))?;
        if tree.seal() != *seal {
            return Err(kept(refuse(
                UNDO_CONFLICT,
                format!("'{path}' changed since the copy (an entry was replaced, edited or added)"),
            )));
        }
        self.at(Hook::Checked);
        self.move_to_trash(root, &place).map_err(kept)?;
        Ok(undo_response(
            root,
            ProjectFileOperationKindV1::Copied,
            ProjectFileUndoEffectV1::Trashed,
            place.rel,
            None,
        ))
    }

    fn undo_create(
        &self,
        root: &Root,
        entries: &[(String, Stamp)],
    ) -> UndoResult<UndoProjectFileOperationResponseV1> {
        let conflict = |rel: &str| {
            kept(refuse(
                UNDO_CONFLICT,
                format!("'{rel}' changed since the add (replaced, edited, or holding new entries)"),
            ))
        };
        // Validate everything before removing anything (hub #3147).
        let mut places = Vec::with_capacity(entries.len());
        for (index, (rel, stamp)) in entries.iter().enumerate() {
            let place = match self.locate(root, rel) {
                Ok(place) => place,
                Err(error) if index == 0 => return Err(keep(error)),
                Err(_) => return Err(conflict(rel)),
            };
            match fsx::lstat(&place.dir, &place.name).map_err(|e| kept(io_refusal(rel, e)))? {
                None if index == 0 => {
                    return Err((
                        refuse(NOT_FOUND, format!("'{rel}' no longer exists")),
                        false,
                    ));
                }
                Some(now) if now.same_object(stamp) => {}
                _ => return Err(conflict(rel)),
            }
            if index == 0 {
                self.guard(root, &place.dir, Some(stamp), rel)
                    .map_err(kept)?;
            }
            if stamp.kind == Kind::Dir {
                let dir = fsx::open_dir(&place.dir, &place.name).map_err(|_| conflict(rel))?;
                let expected: Vec<OsString> = entries
                    .get(index + 1)
                    .and_then(|(child, _)| Path::new(child).file_name())
                    .map(OsStr::to_os_string)
                    .into_iter()
                    .collect();
                if fsx::names(&dir).map_err(|_| conflict(rel))? != expected {
                    return Err(conflict(rel));
                }
            }
            places.push(place);
        }
        self.at(Hook::Checked);
        for (place, (rel, stamp)) in places.iter().zip(entries).rev() {
            // Taken out from under its name first, so the identity checked
            // is the identity removed.
            let Some(taken) = Detached::take(place, |seen| {
                if stamp.kind == Kind::Dir {
                    seen.id == stamp.id && seen.kind == Kind::Dir
                } else {
                    seen.same_object(stamp)
                }
            })?
            else {
                return Err(conflict(rel));
            };
            let removed = if stamp.kind == Kind::Dir {
                fsx::rmdir(&taken.place.dir, &taken.place.name)
            } else {
                fsx::unlink(&taken.place.dir, &taken.place.name)
            };
            if removed.is_err() {
                taken.restore()?;
                return Err(conflict(rel));
            }
        }
        Ok(undo_response(
            root,
            ProjectFileOperationKindV1::Created,
            ProjectFileUndoEffectV1::Removed,
            entries[0].0.clone(),
            None,
        ))
    }
}
