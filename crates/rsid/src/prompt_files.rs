//! The operator prompt-file store (hub #3040, local #74): `<data dir>/prompts`
//! behind the four operator-only RPCs in `rpc/prompt_files.rs`. Wire types,
//! caps and refusal codes are `rsi_common::prompt_files`.
//!
//! Containment: a prompt is one flat `*.md` name
//! (`rsi_common::prompt_files::validate_prompt_name`), joined to the prompts
//! directory, which must itself resolve directly inside the canonical data
//! directory. A prompt that is a symlink (or not a regular file) is refused;
//! on Unix reads open with `O_NOFOLLOW`, so a symlink swapped in after the
//! check is refused too. A put writes a temp file in the prompts directory and
//! renames it over the name, so a reader never sees a partial prompt and a
//! symlink at the name is replaced, never written through. A delete moves the
//! file to `<data dir>/trash/prompts/<UTC timestamp>-<name>` (hard link plus
//! unlink, or copy plus unlink across file systems; never clobbering a trash
//! entry), so the operator can move it back.

use crate::error::{DaemonError, Result};
use rsi_common::prompt_files::{
    CODE_EXISTS, CODE_INVALID_NAME, CODE_NOT_A_FILE, CODE_NOT_FOUND, CODE_NOT_UTF8,
    CODE_OUTSIDE_ROOT, CODE_SYMLINK, CODE_TOO_LARGE, DeletePromptFileResponseV1,
    FIRST_LINE_MAX_CHARS, FIRST_LINE_SCAN_BYTES, GetPromptFileResponseV1,
    ListPromptFilesResponseV1, MAX_LISTED_PROMPTS, MAX_PROMPT_FILE_BYTES, PROMPT_SCAN_LIMIT,
    PROMPT_TRASH_DIR, PROMPTS_DIR_NAME, PromptFileEntryV1, PutPromptFileResponseV1,
    validate_prompt_name,
};
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};

/// Trash names tried for one delete before giving up.
const TRASH_NAME_ATTEMPTS: usize = 100;

pub(crate) fn refuse(code: &str, detail: impl std::fmt::Display) -> DaemonError {
    DaemonError::InvalidParam(format!("{code}: {detail}"))
}

fn checked_name(name: &str) -> Result<()> {
    validate_prompt_name(name)
        .map_err(|reason| refuse(CODE_INVALID_NAME, format!("{name:?}: {reason}")))
}

fn rfc3339(time: std::time::SystemTime) -> String {
    chrono::DateTime::<chrono::Utc>::from(time).to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
}

/// The canonical prompts directory under `data_dir`; `None` when it does not
/// exist and `create` is false. It must resolve directly inside the canonical
/// data directory, so a symlinked prompts directory cannot point elsewhere.
fn prompts_dir(data_dir: &Path, create: bool) -> Result<Option<PathBuf>> {
    let dir = data_dir.join(PROMPTS_DIR_NAME);
    if create {
        std::fs::create_dir_all(&dir)?;
    }
    match std::fs::symlink_metadata(&dir) {
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
        Ok(_) => {}
    }
    let canonical_data = std::fs::canonicalize(data_dir)?;
    let canonical = std::fs::canonicalize(&dir)?;
    if canonical.parent() != Some(canonical_data.as_path()) {
        return Err(refuse(
            CODE_OUTSIDE_ROOT,
            format!(
                "the prompts directory resolves to {}, outside {}",
                canonical.display(),
                canonical_data.display()
            ),
        ));
    }
    if !canonical.is_dir() {
        return Err(refuse(
            CODE_NOT_A_FILE,
            format!("{} is not a directory", canonical.display()),
        ));
    }
    Ok(Some(canonical))
}

/// A regular, non-symlink prompt file's metadata, or `None` when absent.
fn existing_prompt(path: &Path, name: &str) -> Result<Option<std::fs::Metadata>> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
        Ok(meta) if meta.file_type().is_symlink() => Err(refuse(
            CODE_SYMLINK,
            format!("{name:?} is a symlink; prompts must be regular files"),
        )),
        Ok(meta) if !meta.is_file() => Err(refuse(
            CODE_NOT_A_FILE,
            format!("{name:?} is not a regular file"),
        )),
        Ok(meta) => Ok(Some(meta)),
    }
}

/// Open a prompt for reading without following a symlink (Unix).
fn open_no_follow(path: &Path, name: &str) -> Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(nix::fcntl::OFlag::O_NOFOLLOW.bits());
    }
    let file = options.open(path).map_err(|error| match error.kind() {
        ErrorKind::NotFound => refuse(CODE_NOT_FOUND, format!("no prompt named {name:?}")),
        _ if is_symlink_refusal(&error) => refuse(
            CODE_SYMLINK,
            format!("{name:?} is a symlink; prompts must be regular files"),
        ),
        _ => DaemonError::Io(error),
    })?;
    if !file.metadata()?.is_file() {
        return Err(refuse(
            CODE_NOT_A_FILE,
            format!("{name:?} is not a regular file"),
        ));
    }
    Ok(file)
}

/// `O_NOFOLLOW` on a symlink fails `ELOOP`: the name became a symlink after
/// the check.
fn is_symlink_refusal(error: &std::io::Error) -> bool {
    #[cfg(unix)]
    {
        error.raw_os_error() == Some(nix::libc::ELOOP)
    }
    #[cfg(not(unix))]
    {
        let _ = error;
        false
    }
}

fn first_line(path: &Path, name: &str) -> String {
    let mut head = Vec::new();
    if let Ok(file) = open_no_follow(path, name) {
        let _ = file
            .take(FIRST_LINE_SCAN_BYTES as u64)
            .read_to_end(&mut head);
    }
    String::from_utf8_lossy(&head)
        .lines()
        .next()
        .unwrap_or("")
        .trim_start_matches('#')
        .trim()
        .chars()
        .take(FIRST_LINE_MAX_CHARS)
        .collect()
}

/// `ListPromptFiles`: every valid `*.md` regular file, newest first.
pub(crate) fn list(data_dir: &Path) -> Result<ListPromptFilesResponseV1> {
    let Some(dir) = prompts_dir(data_dir, false)? else {
        return Ok(ListPromptFilesResponseV1 {
            dir: data_dir.join(PROMPTS_DIR_NAME).display().to_string(),
            files: Vec::new(),
            truncated: false,
        });
    };
    let mut files = Vec::new();
    let mut truncated = false;
    for (scanned, entry) in std::fs::read_dir(&dir)?.enumerate() {
        if scanned >= PROMPT_SCAN_LIMIT {
            truncated = true;
            break;
        }
        let Ok(entry) = entry else { continue };
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if validate_prompt_name(&name).is_err() {
            continue;
        }
        // `DirEntry::metadata` does not follow a symlink.
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        files.push(PromptFileEntryV1 {
            first_line: first_line(&entry.path(), &name),
            size: meta.len(),
            mtime: meta.modified().ok().map(rfc3339),
            name,
        });
    }
    // RFC3339 UTC with fixed nanoseconds sorts lexically by time.
    files.sort_by(|a, b| b.mtime.cmp(&a.mtime).then_with(|| a.name.cmp(&b.name)));
    if files.len() > MAX_LISTED_PROMPTS {
        files.truncate(MAX_LISTED_PROMPTS);
        truncated = true;
    }
    Ok(ListPromptFilesResponseV1 {
        dir: dir.display().to_string(),
        files,
        truncated,
    })
}

/// `GetPromptFile`: one prompt's UTF-8 text, at most the size cap.
pub(crate) fn get(data_dir: &Path, name: &str) -> Result<GetPromptFileResponseV1> {
    checked_name(name)?;
    let not_found = || refuse(CODE_NOT_FOUND, format!("no prompt named {name:?}"));
    let dir = prompts_dir(data_dir, false)?.ok_or_else(not_found)?;
    let path = dir.join(name);
    existing_prompt(&path, name)?.ok_or_else(not_found)?;
    let file = open_no_follow(&path, name)?;
    let meta = file.metadata()?;
    let mut bytes = Vec::new();
    file.take(MAX_PROMPT_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_PROMPT_FILE_BYTES {
        return Err(refuse(
            CODE_TOO_LARGE,
            format!("{name:?} is larger than {MAX_PROMPT_FILE_BYTES} bytes"),
        ));
    }
    let content = String::from_utf8(bytes)
        .map_err(|_| refuse(CODE_NOT_UTF8, format!("{name:?} is not UTF-8 text")))?;
    Ok(GetPromptFileResponseV1 {
        name: name.to_string(),
        size: content.len() as u64,
        content,
        mtime: meta.modified().ok().map(rfc3339),
    })
}

/// `PutPromptFile`: write atomically (temp file in the prompts directory,
/// fsync, rename). `create_only` refuses an existing name.
pub(crate) fn put(
    data_dir: &Path,
    name: &str,
    content: &str,
    create_only: bool,
) -> Result<PutPromptFileResponseV1> {
    checked_name(name)?;
    if content.len() > MAX_PROMPT_FILE_BYTES {
        return Err(refuse(
            CODE_TOO_LARGE,
            format!("the content is larger than {MAX_PROMPT_FILE_BYTES} bytes"),
        ));
    }
    let exists_refusal = || refuse(CODE_EXISTS, format!("a prompt named {name:?} exists"));
    let dir = prompts_dir(data_dir, true)?
        .ok_or_else(|| refuse(CODE_NOT_FOUND, "the prompts directory could not be created"))?;
    let path = dir.join(name);
    let existed = existing_prompt(&path, name)?.is_some();
    if create_only && existed {
        return Err(exists_refusal());
    }
    let mut temporary = tempfile::Builder::new()
        .prefix(".prompt-")
        .suffix(".tmp")
        .tempfile_in(&dir)?;
    temporary.write_all(content.as_bytes())?;
    temporary.as_file().sync_all()?;
    if create_only {
        temporary.persist_noclobber(&path).map_err(|error| {
            if error.error.kind() == ErrorKind::AlreadyExists {
                exists_refusal()
            } else {
                DaemonError::Io(error.error)
            }
        })?;
    } else {
        temporary
            .persist(&path)
            .map_err(|error| DaemonError::Io(error.error))?;
    }
    sync_dir(&dir);
    let meta = std::fs::symlink_metadata(&path)?;
    Ok(PutPromptFileResponseV1 {
        name: name.to_string(),
        size: meta.len(),
        mtime: meta.modified().ok().map(rfc3339),
        created: !existed,
    })
}

/// Make a rename durable (best effort; Unix only).
fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Ok(handle) = std::fs::File::open(dir) {
        let _ = handle.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

/// The trash directory of deleted prompts, created, inside the data directory.
fn trash_dir(data_dir: &Path) -> Result<PathBuf> {
    let dir = PROMPT_TRASH_DIR
        .iter()
        .fold(data_dir.to_path_buf(), |path, part| path.join(part));
    std::fs::create_dir_all(&dir)?;
    let canonical_data = std::fs::canonicalize(data_dir)?;
    let canonical = std::fs::canonicalize(&dir)?;
    if !canonical.starts_with(&canonical_data) || canonical == canonical_data {
        return Err(refuse(
            CODE_OUTSIDE_ROOT,
            format!(
                "the prompt trash resolves to {}, outside {}",
                canonical.display(),
                canonical_data.display()
            ),
        ));
    }
    Ok(canonical)
}

/// Move `source` to `target` without replacing an existing `target`. Returns
/// false when `target` exists.
fn move_no_clobber(source: &Path, target: &Path) -> Result<bool> {
    match std::fs::hard_link(source, target) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::AlreadyExists => return Ok(false),
        // No hard links here (another file system, FAT): copy, never clobbering.
        Err(_) => {
            let mut output = match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(target)
            {
                Ok(output) => output,
                Err(error) if error.kind() == ErrorKind::AlreadyExists => return Ok(false),
                Err(error) => return Err(error.into()),
            };
            let copied = std::fs::File::open(source)
                .and_then(|mut input| std::io::copy(&mut input, &mut output))
                .and_then(|_| output.sync_all());
            if let Err(error) = copied {
                drop(output);
                let _ = std::fs::remove_file(target);
                return Err(error.into());
            }
        }
    }
    std::fs::remove_file(source)?;
    Ok(true)
}

/// `DeletePromptFile`: move the prompt to
/// `<data dir>/trash/prompts/<UTC timestamp>-<name>`.
pub(crate) fn delete(data_dir: &Path, name: &str) -> Result<DeletePromptFileResponseV1> {
    checked_name(name)?;
    let not_found = || refuse(CODE_NOT_FOUND, format!("no prompt named {name:?}"));
    let dir = prompts_dir(data_dir, false)?.ok_or_else(not_found)?;
    let path = dir.join(name);
    existing_prompt(&path, name)?.ok_or_else(not_found)?;
    let trash = trash_dir(data_dir)?;
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S%.9fZ");
    for attempt in 0..TRASH_NAME_ATTEMPTS {
        let file_name = if attempt == 0 {
            format!("{stamp}-{name}")
        } else {
            format!("{stamp}-{attempt}-{name}")
        };
        let target = trash.join(file_name);
        if move_no_clobber(&path, &target)? {
            sync_dir(&dir);
            sync_dir(&trash);
            return Ok(DeletePromptFileResponseV1 {
                name: name.to_string(),
                trash_path: target.display().to_string(),
            });
        }
    }
    Err(DaemonError::Process(format!(
        "no free prompt trash name for {name:?} after {TRASH_NAME_ATTEMPTS} attempts"
    )))
}

#[cfg(test)]
mod tests;
