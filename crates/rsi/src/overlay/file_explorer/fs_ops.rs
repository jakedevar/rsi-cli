//! Filesystem operations behind the explorer's add / rename / move / copy /
//! delete / undo keys.
//!
//! Invariants: every destination is validated to stay inside the explorer
//! root (lexically and through symlinks), nothing is ever overwritten, and
//! deletions move the entry into a trash directory so `u` can restore it.

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

/// A completed operation that `u` can reverse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UndoOp {
    /// `original` was moved into the explorer trash at `trashed`.
    Trashed { original: PathBuf, trashed: PathBuf },
    /// `from` was renamed or moved to `to`.
    Moved { from: PathBuf, to: PathBuf },
    /// `path` is the top-most entry an add created.
    Created { path: PathBuf },
    /// `path` was created by a copy.
    Copied { path: PathBuf },
}

/// What an undo did, so the caller can update the tree and open viewers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndoOutcome {
    pub message: String,
    /// Path to select after the tree refreshes.
    pub select: Option<PathBuf>,
    /// A path that moved (`from`, `to`) and whose open viewers must follow.
    pub moved: Option<(PathBuf, PathBuf)>,
}

/// File name for messages (falls back to the full path).
pub fn name_of(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

fn exists_no_follow(path: &Path) -> bool {
    path.symlink_metadata().is_ok()
}

/// Lexically normalize `path`: resolve `.` and `..` without touching disk.
/// `..` never climbs above the filesystem root.
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::RootDir | Component::Prefix(_)) => {}
                _ => out.push(".."),
            },
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Whether prompt input asks for a directory (trailing `/`).
pub fn wants_directory(input: &str) -> bool {
    input.trim().ends_with('/')
}

/// Resolve prompt `input` (relative to `base`, or absolute) to a destination
/// strictly inside `root`.
pub fn resolve_destination(root: &Path, base: &Path, input: &str) -> Result<PathBuf, String> {
    let trimmed = input.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err("Name cannot be empty".to_string());
    }
    if trimmed.contains('\0') {
        return Err("Name cannot contain a NUL byte".to_string());
    }
    let candidate = Path::new(trimmed);
    let joined = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        base.join(candidate)
    };
    let dest = normalize(&joined);
    let root_n = normalize(root);
    if dest == root_n || !dest.starts_with(&root_n) {
        return Err(format!("Path must stay inside {}", name_of(root)));
    }
    // A symlinked directory inside the project must not carry the
    // destination outside it: check the deepest existing ancestor.
    if let Ok(canon_root) = root.canonicalize()
        && let Some(ancestor) = dest.ancestors().find(|a| a.exists())
        && let Ok(canon_ancestor) = ancestor.canonicalize()
        && !canon_ancestor.starts_with(&canon_root)
    {
        return Err(format!("Path must stay inside {}", name_of(root)));
    }
    Ok(dest)
}

/// Create a file (or directory when `as_dir`) at `path`, creating missing
/// parents. Returns the top-most entry that did not exist before, which is
/// what undo removes.
pub fn create_path(path: &Path, as_dir: bool) -> Result<PathBuf, String> {
    if exists_no_follow(path) {
        return Err(format!("{} already exists", name_of(path)));
    }
    let mut top = path.to_path_buf();
    for ancestor in path.ancestors().skip(1) {
        if exists_no_follow(ancestor) {
            break;
        }
        top = ancestor.to_path_buf();
    }
    let result = if as_dir {
        fs::create_dir_all(path)
    } else {
        path.parent()
            .map_or(Ok(()), fs::create_dir_all)
            .and_then(|()| {
                fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(path)
                    .map(|_| ())
            })
    };
    result.map_err(|e| format!("Create failed: {e}"))?;
    Ok(top)
}

fn remove_all(path: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if meta.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

#[cfg(unix)]
fn copy_symlink(from: &Path, to: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(fs::read_link(from)?, to)
}

#[cfg(not(unix))]
fn copy_symlink(from: &Path, to: &Path) -> io::Result<()> {
    fs::copy(from, to).map(|_| ())
}

/// Copy `from` to `to` recursively. Symlinks are recreated, not followed.
fn copy_recursive(from: &Path, to: &Path) -> io::Result<()> {
    let file_type = fs::symlink_metadata(from)?.file_type();
    if file_type.is_symlink() {
        copy_symlink(from, to)
    } else if file_type.is_dir() {
        fs::create_dir(to)?;
        for entry in fs::read_dir(from)? {
            let entry = entry?;
            copy_recursive(&entry.path(), &to.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        fs::copy(from, to).map(|_| ())
    }
}

fn check_transfer(from: &Path, to: &Path, verb: &str) -> Result<(), String> {
    if !exists_no_follow(from) {
        return Err(format!("{} no longer exists", name_of(from)));
    }
    if exists_no_follow(to) {
        return Err(format!("{} already exists", name_of(to)));
    }
    if to.starts_with(from) {
        return Err(format!("Cannot {verb} a directory into itself"));
    }
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Cannot create {}: {e}", name_of(parent)))?;
    }
    Ok(())
}

/// Rename/move `from` to `to` without overwriting. Falls back to copy and
/// remove across filesystems.
pub fn move_path(from: &Path, to: &Path) -> Result<(), String> {
    check_transfer(from, to, "move")?;
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {
            if let Err(e) = copy_recursive(from, to) {
                // Only the partial copy this call just made is removed.
                let _ = remove_all(to);
                return Err(format!("Move failed: {e}"));
            }
            remove_all(from).map_err(|e| format!("Copied, but could not remove the original: {e}"))
        }
        Err(e) => Err(format!("Move failed: {e}")),
    }
}

/// Copy `from` to `to` (recursively for directories) without overwriting.
pub fn copy_path(from: &Path, to: &Path) -> Result<(), String> {
    check_transfer(from, to, "copy")?;
    copy_recursive(from, to).map_err(|e| {
        // Only the partial copy this call just made is removed.
        let _ = remove_all(to);
        format!("Copy failed: {e}")
    })
}

/// Move `path` into a fresh batch directory under `trash_dir` and return
/// where it now lives. A sibling `<batch>.origin` file records the original
/// path for manual recovery after the explorer closes.
pub fn move_to_trash(trash_dir: &Path, path: &Path) -> Result<PathBuf, String> {
    let name = path
        .file_name()
        .ok_or_else(|| "Cannot delete the filesystem root".to_string())?;
    let id = uuid::Uuid::new_v4().simple().to_string();
    let batch = trash_dir.join(format!(
        "{}-{}",
        chrono::Local::now().format("%Y%m%dT%H%M%S"),
        &id[..8]
    ));
    fs::create_dir_all(&batch).map_err(|e| format!("Cannot create trash: {e}"))?;
    let origin = batch.with_extension("origin");
    let _ = fs::write(&origin, path.display().to_string());
    let dest = batch.join(name);
    match move_path(path, &dest) {
        Ok(()) => Ok(dest),
        Err(e) => {
            // Never discard a batch that holds data.
            if !exists_no_follow(&dest) {
                let _ = fs::remove_dir(&batch);
                let _ = fs::remove_file(&origin);
            }
            Err(e)
        }
    }
}

/// Move a trashed entry back to `original`.
pub fn restore_from_trash(original: &Path, trashed: &Path) -> Result<(), String> {
    if exists_no_follow(original) {
        return Err(format!(
            "Cannot restore: {} exists again",
            name_of(original)
        ));
    }
    move_path(trashed, original)?;
    if let Some(batch) = trashed.parent() {
        let _ = fs::remove_dir(batch);
        let _ = fs::remove_file(batch.with_extension("origin"));
    }
    Ok(())
}

/// Remove trash batches older than `max_age` (best effort).
pub fn prune_trash(trash_dir: &Path, max_age: Duration) {
    let Ok(read_dir) = fs::read_dir(trash_dir) else {
        return;
    };
    let now = SystemTime::now();
    for entry in read_dir.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let expired = meta
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > max_age);
        if expired {
            let _ = remove_all(&entry.path());
        }
    }
}

fn is_empty_entry(path: &Path) -> bool {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => fs::read_dir(path).is_ok_and(|mut d| d.next().is_none()),
        Ok(meta) if meta.is_file() => meta.len() == 0,
        _ => false,
    }
}

/// Reverse `op`. Created/copied entries are removed outright when empty and
/// otherwise moved to the trash, so undo never destroys content.
pub fn undo(op: &UndoOp, trash_dir: &Path) -> Result<UndoOutcome, String> {
    match op {
        UndoOp::Trashed { original, trashed } => {
            restore_from_trash(original, trashed)?;
            Ok(UndoOutcome {
                message: format!("Restored {}", name_of(original)),
                select: Some(original.clone()),
                moved: None,
            })
        }
        UndoOp::Moved { from, to } => {
            move_path(to, from)?;
            Ok(UndoOutcome {
                message: format!("Moved {} back", name_of(from)),
                select: Some(from.clone()),
                moved: Some((to.clone(), from.clone())),
            })
        }
        UndoOp::Created { path } | UndoOp::Copied { path } => {
            if !exists_no_follow(path) {
                return Err(format!("{} no longer exists", name_of(path)));
            }
            let message = if is_empty_entry(path) {
                remove_all(path).map_err(|e| format!("Undo failed: {e}"))?;
                format!("Removed {}", name_of(path))
            } else {
                move_to_trash(trash_dir, path)?;
                format!("Moved {} to trash", name_of(path))
            };
            Ok(UndoOutcome {
                message,
                select: path.parent().map(Path::to_path_buf),
                moved: None,
            })
        }
    }
}

/// Human-readable byte count.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

#[cfg(unix)]
fn permissions_string(meta: &fs::Metadata) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;
    let mode = meta.permissions().mode();
    let bits = ['r', 'w', 'x'];
    Some(
        (0..9)
            .map(|i| {
                if mode & (0o400 >> i) != 0 {
                    bits[i % 3]
                } else {
                    '-'
                }
            })
            .collect(),
    )
}

#[cfg(not(unix))]
fn permissions_string(_meta: &fs::Metadata) -> Option<String> {
    None
}

/// One-line summary for the `i` key: kind, size, modification time, mode.
pub fn describe(path: &Path) -> Result<String, String> {
    let meta = fs::symlink_metadata(path).map_err(|e| format!("Cannot stat: {e}"))?;
    let mut parts = vec![name_of(path)];
    if meta.file_type().is_symlink() {
        let target = fs::read_link(path)
            .map(|t| t.display().to_string())
            .unwrap_or_else(|_| "?".to_string());
        parts.push(format!("symlink → {target}"));
    } else if meta.is_dir() {
        let count = fs::read_dir(path).map(Iterator::count).unwrap_or(0);
        parts.push(format!("{count} item{}", if count == 1 { "" } else { "s" }));
    } else {
        parts.push(human_size(meta.len()));
    }
    if let Ok(modified) = meta.modified() {
        let local: chrono::DateTime<chrono::Local> = modified.into();
        parts.push(format!("modified {}", local.format("%Y-%m-%d %H:%M")));
    }
    if let Some(mode) = permissions_string(&meta) {
        parts.push(mode);
    }
    Ok(parts.join(" · "))
}
