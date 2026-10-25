//! #86 (hub #3044 follow-up): bounded retention for the images
//! `UploadPasteImage` stores in `~/.rsi/paste/rsid` (`<sha256>.<ext>`), and
//! (local #155) for the TUI's own `<lowercase uuid>.png` pastes at the top
//! level of `~/.rsi/paste` ([`prune_tui_paste_dir`]). The TUI deletes nothing
//! at exit any more (it used to remove every top-level `*.png`, including one
//! a queued message still named); the daemon owns all cleanup, for both
//! levels, under one rule.
//!
//! An image a message once named is part of the operator's conversation
//! history, and removing it is deleting user data (hub PM ruling on #86). So
//! by default the daemon removes only ABANDONED uploads. An image is removed
//! only when ALL of these hold:
//! - it is a regular file with one link and a name this daemon or the TUI
//!   writes (`<64 lowercase hex>.<png|jpg|gif|webp>` uploads, or
//!   `<lowercase uuid>.png` at the TUI level), seen by `lstat` through the
//!   pinned directory handle (a symlink, a directory, a hard link or any other
//!   name is kept);
//! - its mtime is more than [`PASTE_IMAGE_MAX_AGE`] (7 days, the TUI's trash
//!   rule, `rsi_common::trash_prune::TRASH_MAX_AGE`) in the past; a re-upload
//!   of the same bytes refreshes the mtime, a future mtime is kept;
//! - and either nothing ever named it: no operator message of any session in
//!   any state (sent or not, its edited or original text), no conversation
//!   event of any type (`Message`, `ToolUse`, `ToolResult`, `System`, ...: its
//!   content, tool input or metadata) and no enabled scheduled job
//!   (`Store::paste_reference_scan_chunk`);
//! - or the operator opted in: the setting `paste_retention_referenced_days`
//!   (`rsid_core::paste_retention_setting`, default 0 = keep forever) is N > 0,
//!   the mtime is more than N days old, and no text a provider may still read
//!   names it (`Store::paste_image_reference_texts`: operator messages not
//!   surely delivered, delivered ones of a starting, running or waiting
//!   session, and enabled scheduled-job messages).
//!
//! Conversation events are read live, then from the cold-storage archive for
//! every session whose rows are in it or moving to or from it, so archiving a
//! session never turns a referenced image into an abandoned one. A move or
//! restore that ran during the scan (`Store::paste_reference_cold_fingerprint`
//! changed) proves nothing. A `ToolResult` body stored cut (#2373) is read as
//! stored: its cut middle is kept nowhere.
//! An archived session whose index row is larger than
//! `PASTE_REFERENCE_ARCHIVE_MAX_RAW_BYTES` (64 MiB) is streamed (#178): its
//! payload is read a buffer at a time and one row is held, so one large
//! session no longer stops cleanup for the whole install. A single archived
//! row over the bound, or an archive record (raw size or stored payload
//! length, checked before the payload is read) over it under an index row
//! within it, fails the scan closed, naming the session (hub #3322 finding 3,
//! hub #3332).
//!
//! Late references (hub #3322 finding 2): the scan starts from a high-water
//! mark of every source ([`Store::paste_reference_marks`]). For the removal
//! the pass takes the store lock, reads only the rows written after the marks
//! ([`Store::paste_reference_late_texts`]: operator messages added or edited,
//! events added, every enabled scheduled job), keeps every image they name,
//! and unlinks the rest before it releases the lock. A move or restore since
//! the marks, more than [`MAX_LATE_REFERENCE_ROWS`] late rows or a check past
//! [`LATE_REFERENCE_BUDGET`] removes nothing.
//!
//! A pass first lists the expired images (no database read when there are
//! none), then checks their references, then removes under the upload lock,
//! re-checking each file: an image that expired after the listing was not
//! checked and is kept for the next pass. Fail-safe: when the references
//! cannot be read (an unreadable archive included), there are more than
//! [`MAX_REFERENCE_TEXTS`] live texts, or the scan outlasts
//! [`REFERENCE_SCAN_BUDGET`], the pass removes nothing. The scan reads
//! [`REFERENCE_SCAN_CHUNK_ROWS`] rows, or one archived session, per store lock.
//!
//! An `.upload-<uuid>.tmp` file left by a crashed upload is removed after
//! the same 7 days. There is no byte cap: a cap would remove young or
//! referenced images. The pass acts only inside the paste directory, opened
//! `O_NOFOLLOW` component by component below the data root (a symlinked
//! `paste` or `rsid` is refused and nothing is pruned), unlinks by name
//! through that handle (an unlink never follows a link) and is bounded per
//! pass ([`MAX_PASTE_PRUNE_SCAN`] names, [`MAX_PASTE_PRUNE_REMOVALS`]
//! removals). It runs once at daemon start and after an upload when the last
//! pass of that directory is at least [`UPLOAD_PRUNE_INTERVAL`] old. Uploads
//! and removals are serialized, so a pass never removes an image an upload is
//! reusing. Unix only; elsewhere the pass reports unsupported and removes
//! nothing.

use crate::store::{PasteReferenceMarks, PasteReferenceSource, Store};
use rsi_common::input_assets::paste_image_names_in;
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

/// Age after which an abandoned image is removed (the TUI's 7-day trash rule).
pub const PASTE_IMAGE_MAX_AGE: Duration = rsi_common::trash_prune::TRASH_MAX_AGE;
/// Most directory names one pass looks at.
pub const MAX_PASTE_PRUNE_SCAN: usize = 10_000;
/// Most files one pass removes; the rest wait for the next pass.
pub const MAX_PASTE_PRUNE_REMOVALS: usize = 1_000;
/// Most live reference texts a pass reads; more means the pass removes nothing.
pub const MAX_REFERENCE_TEXTS: usize = 10_000;
/// Rows the "ever referenced" scan reads per store lock.
pub const REFERENCE_SCAN_CHUNK_ROWS: usize = 1_000;
/// Wall time the "ever referenced" scan may take; past it the pass removes
/// nothing.
pub const REFERENCE_SCAN_BUDGET: Duration = Duration::from_secs(300);
/// Least time between two upload-triggered passes of one directory.
pub const UPLOAD_PRUNE_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// Most rows written during the scan that the late check (hub #3322) reads
/// while it holds the store; more means the pass removes nothing.
pub const MAX_LATE_REFERENCE_ROWS: usize = 100_000;
/// Wall time the late check may hold the store before it gives up and the
/// pass removes nothing.
pub const LATE_REFERENCE_BUDGET: Duration = Duration::from_secs(10);

const SHA256_HEX_LEN: usize = 64;
const UUID_LEN: usize = 36;
const TEMP_PREFIX: &str = ".upload-";
const TEMP_SUFFIX: &str = ".tmp";

/// The default upload directory: `~/.rsi/paste/rsid` (the rsi home helper).
pub fn default_paste_dir() -> PathBuf {
    rsi_common::identity::data_path("paste", "paste")
        .join(rsi_common::input_assets::PASTE_UPLOAD_SUBDIR)
}

static PASTE_DIR_LOCK: Mutex<()> = Mutex::new(());
static LAST_PASS: Mutex<Option<HashMap<PathBuf, Instant>>> = Mutex::new(None);

/// Serializes uploads and prune passes.
pub(crate) fn lock() -> MutexGuard<'static, ()> {
    PASTE_DIR_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// True (and recorded) when `dir` had no pass in the last `interval`.
fn pass_due(dir: &Path, interval: Duration) -> bool {
    let now = Instant::now();
    let mut guard = LAST_PASS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let passes = guard.get_or_insert_with(HashMap::new);
    match passes.get(dir) {
        Some(last) if now.duration_since(*last) < interval => false,
        _ => {
            passes.insert(dir.to_path_buf(), now);
            true
        }
    }
}

/// How long images live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionPolicy {
    /// Age after which an abandoned image (and an upload temporary) goes.
    pub abandoned_age: Duration,
    /// Age after which a referenced image no live text names goes; `None`
    /// keeps referenced images forever (the default).
    pub referenced_age: Option<Duration>,
}

impl RetentionPolicy {
    /// The policy of the operator's live setting.
    pub fn from_settings() -> Self {
        Self {
            abandoned_age: PASTE_IMAGE_MAX_AGE,
            referenced_age: rsid_core::paste_retention_setting::referenced_max_age(),
        }
    }
}

/// What a pass knows about the references of the images it may remove.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct References {
    /// The expired images whose references were checked; any other image is
    /// kept.
    pub scanned: HashSet<String>,
    /// Names a text a provider may still read names (always kept).
    pub live: HashSet<String>,
    /// Names any operator message, conversation event or enabled scheduled
    /// job ever named (kept unless the operator opted in).
    pub ever: HashSet<String>,
}

/// What one pass did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PasteRetentionReport {
    /// Files removed: abandoned images, stale upload temporaries and, under
    /// the opt-in setting, expired referenced images.
    pub removed: usize,
    /// Of `removed`, the referenced images the opt-in setting removed.
    pub removed_referenced: usize,
    /// Daemon-named files not yet older than the limit (or future-dated), and
    /// images that expired after the reference check.
    pub kept_young: usize,
    /// Expired images a message, event or scheduled job names.
    pub kept_referenced: usize,
    /// Everything else: other names, symlinks, directories, hard links.
    pub kept_other: usize,
    /// Entries a `lstat` or unlink failed on (kept).
    pub failed: usize,
    /// The scan or removal bound stopped the pass early.
    pub truncated: bool,
}

impl PasteRetentionReport {
    fn absorb(&mut self, other: PasteRetentionReport) {
        self.removed += other.removed;
        self.removed_referenced += other.removed_referenced;
        self.kept_young += other.kept_young;
        self.kept_referenced += other.kept_referenced;
        self.kept_other += other.kept_other;
        self.failed += other.failed;
        self.truncated |= other.truncated;
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Owned {
    Image,
    UploadTemp,
}

/// Which directory a pass walks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Level {
    /// `~/.rsi/paste/rsid`: `<sha256>.<ext>` uploads and their temporaries.
    Upload,
    /// `~/.rsi/paste`: the TUI's own `<uuid>.png` pastes (local #155).
    Tui,
}

fn classify(name: &str, level: Level) -> Option<Owned> {
    if level == Level::Tui {
        return is_tui_paste_name(name).then_some(Owned::Image);
    }
    if let Some(middle) = name
        .strip_prefix(TEMP_PREFIX)
        .and_then(|rest| rest.strip_suffix(TEMP_SUFFIX))
    {
        return uuid::Uuid::parse_str(middle)
            .is_ok()
            .then_some(Owned::UploadTemp);
    }
    let (stem, ext) = name.split_once('.')?;
    let known_ext = rsi_common::input_assets::PASTE_IMAGE_MIME_ALLOWLIST
        .iter()
        .any(|(_, allowed)| *allowed == ext);
    (known_ext && is_sha256_hex(stem)).then_some(Owned::Image)
}

/// `<lowercase hyphenated uuid>.png`, the name `rsi::clipboard` writes.
fn is_tui_paste_name(name: &str) -> bool {
    name.strip_suffix(".png")
        .is_some_and(|stem| is_lower_uuid(stem.as_bytes()))
}

fn is_lower_uuid(bytes: &[u8]) -> bool {
    bytes.len() == UUID_LEN
        && bytes.iter().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => *b == b'-',
            _ => is_lower_hex(*b),
        })
}

fn is_sha256_hex(stem: &str) -> bool {
    stem.len() == SHA256_HEX_LEN
        && stem
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The images a pass owns (`<sha256>.<ext>` uploads, the TUI's
/// `<uuid>.png` pastes) that `texts` mention anywhere.
pub fn referenced_names(texts: &[String]) -> HashSet<String> {
    let mut names = HashSet::new();
    for text in texts {
        paste_image_names_in(text, &mut |name| {
            names.insert(name);
        });
    }
    names
}

fn is_lower_hex(b: u8) -> bool {
    b.is_ascii_digit() || (b'a'..=b'f').contains(&b)
}

fn expired(mtime: SystemTime, now: SystemTime, max_age: Duration) -> bool {
    now.duration_since(mtime).is_ok_and(|age| age > max_age)
}

/// The daemon-named images in `dir` that are regular single-link files older
/// than `max_age`: the ones whose references a pass must check. A missing
/// directory has none; a symlinked `paste` or `rsid` component is an error.
pub fn expired_images(
    dir: &Path,
    max_age: Duration,
    now: SystemTime,
) -> io::Result<HashSet<String>> {
    imp::expired_images(dir, Level::Upload, max_age, now)
}

/// [`expired_images`] for the TUI's own level `dir` (`<data root>/paste`):
/// its top-level `<uuid>.png` pastes (local #155).
pub fn expired_tui_images(
    dir: &Path,
    max_age: Duration,
    now: SystemTime,
) -> io::Result<HashSet<String>> {
    imp::expired_images(dir, Level::Tui, max_age, now)
}

/// One bounded pass over `dir` (`<data root>/paste/rsid`): remove the files
/// the module rule allows. A missing directory is an empty pass; a symlinked
/// `paste` or `rsid` component is an error and nothing is removed.
pub fn prune_paste_dir(
    dir: &Path,
    references: &References,
    policy: RetentionPolicy,
    now: SystemTime,
) -> io::Result<PasteRetentionReport> {
    let _guard = lock();
    imp::prune(dir, Level::Upload, references, policy, now)
}

/// One bounded pass over the TUI's own level `dir` (`<data root>/paste`):
/// only top-level `<uuid>.png` files, under the same rule as
/// [`prune_paste_dir`] (local #155). The `rsid` subdirectory and every other
/// name are kept.
pub fn prune_tui_paste_dir(
    dir: &Path,
    references: &References,
    policy: RetentionPolicy,
    now: SystemTime,
) -> io::Result<PasteRetentionReport> {
    let _guard = lock();
    imp::prune(dir, Level::Tui, references, policy, now)
}

/// The decision for one expired image.
fn image_verdict(
    name: &str,
    mtime: SystemTime,
    references: &References,
    policy: RetentionPolicy,
    now: SystemTime,
) -> Verdict {
    if !references.scanned.contains(name) {
        // It expired after the reference check: the next pass decides.
        return Verdict::KeepYoung;
    }
    if references.live.contains(name) {
        return Verdict::KeepReferenced;
    }
    if !references.ever.contains(name) {
        return Verdict::Remove { referenced: false };
    }
    match policy.referenced_age {
        Some(age) if expired(mtime, now, age) => Verdict::Remove { referenced: true },
        _ => Verdict::KeepReferenced,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    KeepYoung,
    KeepReferenced,
    Remove { referenced: bool },
}

#[cfg(unix)]
mod imp {
    use super::*;
    use rsi_common::asset_fs::AssetDir;
    use rustix::fd::OwnedFd;
    use rustix::fs::{AtFlags, FileType};
    use std::ffi::{OsStr, OsString};
    use std::os::unix::ffi::OsStrExt as _;

    fn open(dir: &Path, level: Level) -> io::Result<Option<OwnedFd>> {
        let invalid = || {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "paste directory needs a paste parent",
            )
        };
        let (data_root, relative) = match level {
            Level::Upload => {
                let paste = dir.parent().ok_or_else(invalid)?;
                let relative = Path::new(paste.file_name().ok_or_else(invalid)?)
                    .join(dir.file_name().ok_or_else(invalid)?);
                (paste.parent().ok_or_else(invalid)?, relative)
            }
            Level::Tui => (
                dir.parent().ok_or_else(invalid)?,
                PathBuf::from(dir.file_name().ok_or_else(invalid)?),
            ),
        };
        // The same resolution as an upload: the legitimate data root may be
        // reached through links, the paste components may not.
        let root = match data_root.canonicalize() {
            Ok(root) => root,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        match AssetDir::open(&root)?.open_dir(&relative) {
            Ok(handle) => Ok(Some(handle.into_std_file().into())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    #[allow(clippy::unnecessary_cast)]
    fn mtime(st: &rustix::fs::Stat) -> Option<SystemTime> {
        let secs = u64::try_from(st.st_mtime as i64).ok()?;
        let nanos = u32::try_from(st.st_mtime_nsec as i64).unwrap_or(0);
        SystemTime::UNIX_EPOCH.checked_add(Duration::new(secs, nanos.min(999_999_999)))
    }

    /// At most [`MAX_PASTE_PRUNE_SCAN`] names of the directory, and whether
    /// more exist.
    fn names(fd: &OwnedFd) -> io::Result<(Vec<OsString>, bool)> {
        let mut names = Vec::new();
        for entry in rustix::fs::Dir::read_from(fd)? {
            let entry = entry?;
            let name = entry.file_name().to_bytes();
            if name == b"." || name == b".." {
                continue;
            }
            if names.len() == MAX_PASTE_PRUNE_SCAN {
                return Ok((names, true));
            }
            names.push(OsStr::from_bytes(name).to_os_string());
        }
        Ok((names, false))
    }

    enum Entry {
        Gone,
        Failed,
        Other,
        File(Option<SystemTime>),
    }

    /// `lstat` through the handle: only a regular single-link file is ours.
    fn inspect(fd: &OwnedFd, name: &OsStr) -> Entry {
        let st = match rustix::fs::statat(fd, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(st) => st,
            Err(rustix::io::Errno::NOENT) => return Entry::Gone,
            Err(_) => return Entry::Failed,
        };
        #[allow(clippy::unnecessary_cast)]
        let single_link = st.st_nlink as u64 == 1;
        if FileType::from_raw_mode(st.st_mode as _) != FileType::RegularFile || !single_link {
            return Entry::Other;
        }
        Entry::File(mtime(&st))
    }

    pub(super) fn expired_images(
        dir: &Path,
        level: Level,
        max_age: Duration,
        now: SystemTime,
    ) -> io::Result<HashSet<String>> {
        let mut found = HashSet::new();
        let Some(fd) = open(dir, level)? else {
            return Ok(found);
        };
        for name in names(&fd)?.0 {
            let Some(text) = name.to_str() else { continue };
            if classify(text, level) != Some(Owned::Image) {
                continue;
            }
            if let Entry::File(Some(m)) = inspect(&fd, &name)
                && expired(m, now, max_age)
            {
                found.insert(text.to_string());
            }
        }
        Ok(found)
    }

    pub(super) fn prune(
        dir: &Path,
        level: Level,
        references: &References,
        policy: RetentionPolicy,
        now: SystemTime,
    ) -> io::Result<PasteRetentionReport> {
        let mut report = PasteRetentionReport::default();
        let Some(fd) = open(dir, level)? else {
            return Ok(report);
        };
        let (names, truncated) = names(&fd)?;
        report.truncated = truncated;
        for name in names {
            let Some((text, owned)) = name
                .to_str()
                .and_then(|text| classify(text, level).map(|owned| (text, owned)))
            else {
                report.kept_other += 1;
                continue;
            };
            let mtime = match inspect(&fd, &name) {
                Entry::Gone => continue,
                Entry::Failed => {
                    report.failed += 1;
                    continue;
                }
                Entry::Other => {
                    report.kept_other += 1;
                    continue;
                }
                Entry::File(mtime) => mtime,
            };
            let Some(mtime) = mtime.filter(|m| expired(*m, now, policy.abandoned_age)) else {
                report.kept_young += 1;
                continue;
            };
            let referenced = match owned {
                Owned::UploadTemp => false,
                Owned::Image => match image_verdict(text, mtime, references, policy, now) {
                    Verdict::KeepYoung => {
                        report.kept_young += 1;
                        continue;
                    }
                    Verdict::KeepReferenced => {
                        report.kept_referenced += 1;
                        continue;
                    }
                    Verdict::Remove { referenced } => referenced,
                },
            };
            if report.removed == MAX_PASTE_PRUNE_REMOVALS {
                report.truncated = true;
                break;
            }
            // unlinkat without AT_REMOVEDIR removes a name; it never follows a
            // link and refuses a directory swapped in since the lstat.
            match rustix::fs::unlinkat(&fd, &name, AtFlags::empty()) {
                Ok(()) => {
                    report.removed += 1;
                    if referenced {
                        report.removed_referenced += 1;
                    }
                }
                Err(rustix::io::Errno::NOENT) => {}
                Err(_) => report.failed += 1,
            }
        }
        Ok(report)
    }
}

#[cfg(not(unix))]
mod imp {
    use super::*;

    fn unsupported() -> io::Error {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "paste image retention is unsupported on this platform",
        )
    }

    pub(super) fn expired_images(
        _dir: &Path,
        _level: Level,
        _max_age: Duration,
        _now: SystemTime,
    ) -> io::Result<HashSet<String>> {
        Err(unsupported())
    }

    pub(super) fn prune(
        _dir: &Path,
        _level: Level,
        _references: &References,
        _policy: RetentionPolicy,
        _now: SystemTime,
    ) -> io::Result<PasteRetentionReport> {
        Err(unsupported())
    }
}

/// The `candidates` any operator message, conversation event (live or
/// archived, any type) or enabled scheduled job ever named, read in chunks of
/// [`REFERENCE_SCAN_CHUNK_ROWS`] rows (one archived session) with the store
/// released between chunks; an archived session over the read bound is
/// streamed with the store released too (#178). An error when the store or an archive fails, the
/// scan outlasts `budget`, or cold storage moved or restored a session during
/// the walk.
async fn ever_referenced(
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    candidates: &HashSet<String>,
    budget: Duration,
) -> Result<HashSet<String>, String> {
    let started = Instant::now();
    let mut found = HashSet::new();
    let cold_before = store
        .lock()
        .await
        .paste_reference_cold_fingerprint()
        .map_err(|error| error.to_string())?;
    for source in PasteReferenceSource::ALL {
        let mut after = 0;
        loop {
            if found.len() == candidates.len() {
                return Ok(found);
            }
            if started.elapsed() >= budget {
                return Err(format!(
                    "reference scan outlasted its {}s budget",
                    budget.as_secs()
                ));
            }
            // The store guard ends with this statement: an oversized archive
            // (#178) is streamed below without it, so one large or slow
            // archive never blocks other store work. A move or restore racing
            // the stream changes the cold fingerprint, which fails the scan.
            let step = store
                .lock()
                .await
                .paste_reference_scan_step(
                    source,
                    after,
                    REFERENCE_SCAN_CHUNK_ROWS,
                    &mut |text: &str| {
                        paste_image_names_in(text, &mut |name| {
                            if candidates.contains(&name) {
                                found.insert(name);
                            }
                        });
                    },
                )
                .map_err(|error| error.to_string())?;
            if let Some(stream) = step.stream {
                // Blocking archive I/O runs off the async workers.
                let wanted = candidates.clone();
                let mut seen = std::mem::take(&mut found);
                let (result, seen, _stats) = tokio::task::spawn_blocking(move || {
                    let result = stream.run(&mut |text: &str| {
                        paste_image_names_in(text, &mut |name| {
                            if wanted.contains(&name) {
                                seen.insert(name);
                            }
                        });
                    });
                    #[cfg(any(test, feature = "test-seam"))]
                    let stats = crate::store::cold_archive::stream_stats::take();
                    #[cfg(not(any(test, feature = "test-seam")))]
                    let stats = ();
                    (result, seen, stats)
                })
                .await
                .map_err(|error| format!("archive stream task failed: {error}"))?;
                // The stats are thread-local: hand the blocking thread's back.
                #[cfg(any(test, feature = "test-seam"))]
                crate::store::cold_archive::stream_stats::absorb(_stats);
                found = seen;
                result.map_err(|error| error.to_string())?;
            }
            let next = step.next;
            match next {
                Some(cursor) => after = cursor,
                None => break,
            }
            tokio::task::yield_now().await;
        }
    }
    let cold_after = store
        .lock()
        .await
        .paste_reference_cold_fingerprint()
        .map_err(|error| error.to_string())?;
    if cold_after != cold_before {
        return Err("cold storage moved or restored a session during the reference scan".into());
    }
    Ok(found)
}

/// The references of `candidates` and the marks the scan started from (hub
/// #3322: taken before anything is read), or why they could not be proven.
async fn read_references(
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    candidates: HashSet<String>,
) -> Result<(References, Option<PasteReferenceMarks>), String> {
    if candidates.is_empty() {
        return Ok((References::default(), None));
    }
    let (marks, live) = {
        let guard = store.lock().await;
        let marks = guard
            .paste_reference_marks()
            .map_err(|error| error.to_string())?;
        (
            marks,
            guard.paste_image_reference_texts(MAX_REFERENCE_TEXTS),
        )
    };
    let live = match live {
        Ok((_, true)) => {
            return Err(format!(
                "more than {MAX_REFERENCE_TEXTS} live reference texts"
            ));
        }
        Ok((texts, false)) => referenced_names(&texts),
        Err(error) => return Err(error.to_string()),
    };
    let ever = ever_referenced(store, &candidates, REFERENCE_SCAN_BUDGET).await?;
    Ok((
        References {
            scanned: candidates,
            live,
            ever,
        },
        Some(marks),
    ))
}

/// Hub #3322 finding 2: the scanned images a row written since `marks` names
/// (an operator message added or edited, a conversation event added, any
/// enabled scheduled job). The caller holds `store` until it has removed the
/// rest, so nothing can name one in between. An error when cold storage moved
/// or restored a session since the marks, or the check passes its bounds.
fn late_references(
    store: &Store,
    marks: &PasteReferenceMarks,
    scanned: &HashSet<String>,
) -> Result<HashSet<String>, String> {
    let mut late = HashSet::new();
    store
        .paste_reference_late_texts(
            marks,
            MAX_LATE_REFERENCE_ROWS,
            Instant::now() + LATE_REFERENCE_BUDGET,
            &mut |text: &str| {
                paste_image_names_in(text, &mut |name| {
                    if scanned.contains(&name) {
                        late.insert(name);
                    }
                });
            },
        )
        .map_err(|error| format!("late reference check: {error}"))?;
    Ok(late)
}

/// One pass under `policy` over the upload directory `dir` and the TUI level
/// above it: list the expired images, read their references, then prune both
/// on a blocking thread. Logs one line; `None` when the pass
/// removed nothing because it could not prove the references or the
/// directory.
pub async fn run_policy_pass(
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    dir: PathBuf,
    trigger: &'static str,
    policy: RetentionPolicy,
) -> Option<PasteRetentionReport> {
    run_policy_pass_with(store, dir, trigger, policy, None).await
}

/// A test's write between the reference scan and the removal (hub #3322).
type BetweenScanAndRemoval = Box<dyn FnOnce(&mut Store) + Send>;

/// [`run_policy_pass`]; `between` runs with the store after the reference
/// scan and before the removal takes the store (tests only pass one).
async fn run_policy_pass_with(
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    dir: PathBuf,
    trigger: &'static str,
    policy: RetentionPolicy,
    between: Option<BetweenScanAndRemoval>,
) -> Option<PasteRetentionReport> {
    let listing = {
        let dir = dir.clone();
        tokio::task::spawn_blocking(move || {
            let now = SystemTime::now();
            let mut found = expired_images(&dir, policy.abandoned_age, now)?;
            // Local #155: the TUI's `<uuid>.png` pastes one level above.
            if let Some(tui_dir) = dir.parent() {
                found.extend(expired_tui_images(tui_dir, policy.abandoned_age, now)?);
            }
            io::Result::Ok(found)
        })
        .await
    };
    let candidates = match listing {
        Ok(Ok(candidates)) => candidates,
        Ok(Err(error)) => {
            tracing::warn!(trigger, error = %error, "Paste image retention pass refused");
            return None;
        }
        Err(error) => {
            tracing::warn!(trigger, error = %error, "Paste image retention pass panicked");
            return None;
        }
    };
    let (mut references, marks) = match read_references(store, candidates).await {
        Ok(read) => read,
        Err(error) => {
            tracing::warn!(trigger, error = %error, "Paste image retention pass skipped");
            return None;
        }
    };
    if let Some(between) = between {
        between(&mut *store.lock().await);
    }
    // Hub #3322 finding 2: hold the store from the late check until the last
    // unlink, so no reference can be committed between them. The removals are
    // bounded (MAX_PASTE_PRUNE_REMOVALS), so the hold is short.
    let held = Arc::clone(store).lock_owned().await;
    let pass = tokio::task::spawn_blocking(move || {
        if let Some(marks) = &marks {
            let late =
                late_references(&held, marks, &references.scanned).map_err(io::Error::other)?;
            references.live.extend(late);
        }
        let now = SystemTime::now();
        let mut report = prune_paste_dir(&dir, &references, policy, now)?;
        if let Some(tui_dir) = dir.parent() {
            report.absorb(prune_tui_paste_dir(tui_dir, &references, policy, now)?);
        }
        drop(held);
        io::Result::Ok(report)
    })
    .await;
    match pass {
        Ok(Ok(report)) => {
            tracing::info!(
                trigger,
                referenced_days = policy
                    .referenced_age
                    .map_or(0, |age| age.as_secs() / (24 * 60 * 60)),
                removed = report.removed,
                removed_referenced = report.removed_referenced,
                kept_young = report.kept_young,
                kept_referenced = report.kept_referenced,
                kept_other = report.kept_other,
                failed = report.failed,
                truncated = report.truncated,
                "Paste image retention pass finished"
            );
            Some(report)
        }
        Ok(Err(error)) => {
            tracing::warn!(trigger, error = %error, "Paste image retention pass refused");
            None
        }
        Err(error) => {
            tracing::warn!(trigger, error = %error, "Paste image retention pass panicked");
            None
        }
    }
}

/// Daemon start: one bounded pass over `~/.rsi/paste/rsid`.
pub async fn prune_paste_images_at_start(store: Arc<rsid_core::store_lock::Mutex<Store>>) {
    let dir = default_paste_dir();
    pass_due(&dir, Duration::ZERO);
    run_policy_pass(
        &store,
        dir,
        "daemon_start",
        RetentionPolicy::from_settings(),
    )
    .await;
}

/// After an upload: a pass when the last one of `dir` is at least
/// [`UPLOAD_PRUNE_INTERVAL`] old.
pub(crate) async fn prune_after_upload(
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    dir: PathBuf,
) -> Option<PasteRetentionReport> {
    if !pass_due(&dir, UPLOAD_PRUNE_INTERVAL) {
        return None;
    }
    run_policy_pass(store, dir, "upload", RetentionPolicy::from_settings()).await
}

#[cfg(all(test, unix))]
mod tests {
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    use super::*;
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    use std::fs;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    const DAY: Duration = Duration::from_secs(24 * 60 * 60);

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    const DEFAULT_POLICY: RetentionPolicy = RetentionPolicy {
        abandoned_age: PASTE_IMAGE_MAX_AGE,
        referenced_age: None,
    };

    /// The operator opted in: referenced images go after 30 days.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    const OPT_IN_POLICY: RetentionPolicy = RetentionPolicy {
        abandoned_age: PASTE_IMAGE_MAX_AGE,
        referenced_age: Some(Duration::from_secs(30 * 24 * 60 * 60)),
    };

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    fn hex_name(seed: char, ext: &str) -> String {
        format!("{}.{ext}", seed.to_string().repeat(SHA256_HEX_LEN))
    }

    /// Set a path's own mtime (a symlink's too) to `age` before now.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    fn age(path: &Path, age: Duration) {
        let when = SystemTime::now() - age;
        let since = when.duration_since(SystemTime::UNIX_EPOCH).unwrap();
        let stamp = rustix::fs::Timespec {
            tv_sec: since.as_secs() as _,
            tv_nsec: since.subsec_nanos() as _,
        };
        rustix::fs::utimensat(
            rustix::fs::CWD,
            path,
            &rustix::fs::Timestamps {
                last_access: stamp,
                last_modification: stamp,
            },
            rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
        )
        .unwrap();
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    fn write_aged(path: &Path, bytes: &[u8], days: u64) {
        fs::write(path, bytes).unwrap();
        age(path, DAY * days as u32);
    }

    /// A synthetic home: `<tmp>/home/.rsi/paste/rsid`, never the operator's.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    fn synthetic_home() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("home/.rsi/paste/rsid");
        fs::create_dir_all(&dir).unwrap();
        (tmp, dir)
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn referenced_names_finds_content_addressed_names_in_message_text() {
        let a = hex_name('a', "png");
        let b = hex_name('b', "webp");
        let texts = vec![
            format!("look at @/home/x/.rsi/paste/rsid/{a} and @{b}, thanks"),
            format!("not a name: {}.pngx {}.png", "c".repeat(64), "d".repeat(63)),
            format!("{}.png", "e".repeat(65)),
        ];
        let names = referenced_names(&texts);
        assert_eq!(names, HashSet::from([a, b]));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn old_unreferenced_images_are_removed_and_everything_else_is_kept() {
        let (tmp, dir) = synthetic_home();
        let outside = tmp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();

        let old = hex_name('1', "png");
        let old_jpg = hex_name('2', "jpg");
        let referenced = hex_name('3', "png");
        let young = hex_name('4', "gif");
        let future = hex_name('5', "webp");
        let link = hex_name('6', "png");
        let hard = hex_name('7', "png");
        let subdir = hex_name('8', "png");
        let temp = format!(".upload-{}.tmp", uuid::Uuid::new_v4());
        write_aged(&dir.join(&old), b"old", 8);
        write_aged(&dir.join(&old_jpg), b"old jpg", 400);
        write_aged(&dir.join(&referenced), b"named", 30);
        write_aged(&dir.join(&young), b"young", 6);
        fs::write(dir.join(&future), b"future").unwrap();
        let ahead = SystemTime::now() + DAY;
        fs::File::options()
            .write(true)
            .open(dir.join(&future))
            .unwrap()
            .set_modified(ahead)
            .unwrap();
        write_aged(&dir.join(&temp), b"partial", 8);
        write_aged(&dir.join("notes.txt"), b"foreign", 30);
        // A link to an old image outside, itself old: kept, target intact.
        let target = outside.join(hex_name('9', "png"));
        write_aged(&target, b"outside", 30);
        std::os::unix::fs::symlink(&target, dir.join(&link)).unwrap();
        age(&dir.join(&link), DAY * 30);
        // A hard link to an outside file: kept.
        let hard_target = outside.join("hard.png");
        write_aged(&hard_target, b"hard", 30);
        fs::hard_link(&hard_target, dir.join(&hard)).unwrap();
        // A directory with an image name: kept, contents untouched.
        fs::create_dir(dir.join(&subdir)).unwrap();
        write_aged(&dir.join(&subdir).join("inner.png"), b"inner", 30);
        age(&dir.join(&subdir), DAY * 30);
        // The TUI's own level above (`~/.rsi/paste`) is never touched.
        let tui_level = dir.parent().unwrap().join(hex_name('a', "png"));
        write_aged(&tui_level, b"tui", 30);

        // Expired since the reference check: not scanned, so kept.
        let unscanned = hex_name('b', "png");
        write_aged(&dir.join(&unscanned), b"unscanned", 30);

        let references = References {
            scanned: HashSet::from([old.clone(), old_jpg.clone(), referenced.clone()]),
            live: HashSet::new(),
            ever: HashSet::from([referenced.clone()]),
        };
        let report = prune_paste_dir(&dir, &references, DEFAULT_POLICY, SystemTime::now()).unwrap();

        assert_eq!(report.removed, 3, "{report:?}");
        assert_eq!(report.removed_referenced, 0, "{report:?}");
        assert_eq!(report.kept_referenced, 1, "{report:?}");
        assert_eq!(report.kept_young, 3, "{report:?}");
        assert_eq!(report.kept_other, 4, "{report:?}");
        assert_eq!(report.failed, 0);
        assert!(!report.truncated);
        for kept in [
            referenced.as_str(),
            &young,
            &future,
            &hard,
            &unscanned,
            "notes.txt",
        ] {
            assert!(dir.join(kept).is_file(), "{kept} kept");
        }
        assert!(dir.join(&link).symlink_metadata().unwrap().is_symlink());
        assert_eq!(fs::read(&target).unwrap(), b"outside");
        assert_eq!(fs::read(&hard_target).unwrap(), b"hard");
        assert_eq!(
            fs::read(dir.join(&subdir).join("inner.png")).unwrap(),
            b"inner"
        );
        assert_eq!(fs::read(&tui_level).unwrap(), b"tui");
        let mut left: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        let mut expected = vec![
            referenced,
            young,
            future,
            link,
            hard,
            subdir,
            unscanned,
            "notes.txt".to_string(),
        ];
        expected.sort();
        assert_eq!(left, expected);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_symlinked_paste_dir_is_refused_and_its_target_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let elsewhere = tmp.path().join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        let image = elsewhere.join(hex_name('1', "png"));
        write_aged(&image, b"elsewhere", 30);
        let paste = tmp.path().join("home/.rsi/paste");
        fs::create_dir_all(&paste).unwrap();
        std::os::unix::fs::symlink(&elsewhere, paste.join("rsid")).unwrap();

        let everything = References {
            scanned: HashSet::from([hex_name('1', "png")]),
            ..References::default()
        };
        let refused = prune_paste_dir(
            &paste.join("rsid"),
            &everything,
            OPT_IN_POLICY,
            SystemTime::now(),
        );
        assert!(refused.is_err(), "{refused:?}");
        let listing = expired_images(&paste.join("rsid"), PASTE_IMAGE_MAX_AGE, SystemTime::now());
        assert!(listing.is_err(), "{listing:?}");
        assert_eq!(fs::read(&image).unwrap(), b"elsewhere");

        // A symlinked `paste` component is refused the same way.
        let other = tmp.path().join("other-home/.rsi");
        fs::create_dir_all(&other).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("home/.rsi/paste"), other.join("paste"))
            .unwrap();
        assert!(
            prune_paste_dir(
                &other.join("paste/rsid"),
                &everything,
                OPT_IN_POLICY,
                SystemTime::now(),
            )
            .is_err()
        );
        assert_eq!(fs::read(&image).unwrap(), b"elsewhere");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_missing_paste_dir_is_an_empty_pass() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("home/.rsi/paste/rsid");
        let report = prune_paste_dir(
            &missing,
            &References::default(),
            OPT_IN_POLICY,
            SystemTime::now(),
        )
        .unwrap();
        assert_eq!(report, PasteRetentionReport::default());
        assert!(
            expired_images(&missing, PASTE_IMAGE_MAX_AGE, SystemTime::now())
                .unwrap()
                .is_empty()
        );
    }

    /// The store's reference texts decide what a pass keeps, and an upload
    /// pass is throttled per directory.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_pass_keeps_images_a_queued_message_names_and_uploads_throttle_it() {
        let (_tmp, dir) = synthetic_home();
        let named = hex_name('1', "png");
        let unnamed = hex_name('2', "png");
        write_aged(&dir.join(&named), b"named", 30);
        write_aged(&dir.join(&unnamed), b"unnamed", 30);

        let mut store = Store::open_in_memory().unwrap();
        let mut session = rsid_store::test_support::make_test_session();
        session.status = rsi_common::types::SessionStatus::Running;
        store.insert_session(&session).unwrap();
        store
            .queue_operator_message(
                session.id,
                &format!("see @{}", dir.join(&named).display()),
                "paste-key",
            )
            .unwrap();
        let store = Arc::new(rsid_core::store_lock::Mutex::new(store));

        let report = prune_after_upload(&store, dir.clone())
            .await
            .expect("first pass");
        assert_eq!(report.removed, 1);
        assert_eq!(report.kept_referenced, 1);
        assert!(dir.join(&named).is_file());
        assert!(!dir.join(&unnamed).exists());

        // Inside the interval an upload runs no pass.
        let again = hex_name('3', "png");
        write_aged(&dir.join(&again), b"again", 30);
        assert!(prune_after_upload(&store, dir.clone()).await.is_none());
        assert!(dir.join(&again).is_file());
    }

    /// Local #155: the TUI level keeps a paste a queued message names, and
    /// one only history names, and removes an old abandoned one; the opt-in
    /// setting then removes the history-only one.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_tui_level_removes_an_old_unreferenced_paste_only() {
        let (_tmp, dir) = synthetic_home();
        let tui = dir.parent().unwrap().to_path_buf();
        let named = format!("{}.png", uuid::Uuid::new_v4());
        let history = format!("{}.png", uuid::Uuid::new_v4());
        let old = format!("{}.png", uuid::Uuid::new_v4());
        let young = format!("{}.png", uuid::Uuid::new_v4());
        write_aged(&tui.join(&named), b"named", 40);
        write_aged(&tui.join(&history), b"history", 40);
        write_aged(&tui.join(&old), b"old", 8);
        write_aged(&tui.join(&young), b"young", 1);
        write_aged(&tui.join("notes.png"), b"foreign", 30);
        let upload = hex_name('b', "png");
        write_aged(&dir.join(&upload), b"upload", 30);

        // A queued message names the first paste by its full path.
        let text = format!("look @/home/op/.rsi/paste/{named} please");
        let live = referenced_names(&[text]);
        assert!(live.contains(&named));
        let scanned = expired_tui_images(&tui, PASTE_IMAGE_MAX_AGE, SystemTime::now()).unwrap();
        assert_eq!(
            scanned,
            HashSet::from([named.clone(), history.clone(), old.clone()])
        );
        let references = References {
            scanned,
            live,
            ever: HashSet::from([named.clone(), history.clone()]),
        };

        let report =
            prune_tui_paste_dir(&tui, &references, DEFAULT_POLICY, SystemTime::now()).unwrap();
        assert_eq!(report.removed, 1, "{report:?}");
        assert_eq!(report.kept_referenced, 2, "{report:?}");
        assert_eq!(report.kept_young, 1, "{report:?}");
        assert!(tui.join(&named).is_file(), "still named: kept");
        assert!(tui.join(&history).is_file(), "history: kept by default");
        assert!(tui.join(&young).is_file());
        assert!(tui.join("notes.png").is_file());
        assert!(!tui.join(&old).exists(), "old and never named: removed");
        assert!(
            dir.join(&upload).is_file(),
            "the upload level is not walked"
        );

        // Opted in, the history-only paste goes; the live one stays.
        let report =
            prune_tui_paste_dir(&tui, &references, OPT_IN_POLICY, SystemTime::now()).unwrap();
        assert_eq!(report.removed, 1, "{report:?}");
        assert_eq!(report.removed_referenced, 1, "{report:?}");
        assert!(!tui.join(&history).exists());
        assert!(tui.join(&named).is_file());
        assert!(tui.join(&young).is_file());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_tui_paste_symlink_or_uppercase_name_is_kept() {
        let (tmp, dir) = synthetic_home();
        let tui = dir.parent().unwrap().to_path_buf();
        let target = tmp.path().join("outside.png");
        write_aged(&target, b"outside", 30);
        let link = format!("{}.png", uuid::Uuid::new_v4());
        std::os::unix::fs::symlink(&target, tui.join(&link)).unwrap();
        age(&tui.join(&link), DAY * 30);
        let upper = format!("{}.png", uuid::Uuid::new_v4().to_string().to_uppercase());
        write_aged(&tui.join(&upper), b"upper", 30);
        let everything = References {
            scanned: HashSet::from([link.clone(), upper.clone()]),
            ..References::default()
        };

        let report =
            prune_tui_paste_dir(&tui, &everything, OPT_IN_POLICY, SystemTime::now()).unwrap();
        assert_eq!(report.removed, 0, "{report:?}");
        assert_eq!(fs::read(&target).unwrap(), b"outside");
        assert!(tui.join(&link).symlink_metadata().unwrap().is_symlink());
        assert!(tui.join(&upper).is_file());
        assert!(
            expired_tui_images(&tui, PASTE_IMAGE_MAX_AGE, SystemTime::now())
                .unwrap()
                .is_empty()
        );
    }

    /// A store holding a Completed session whose delivered message names
    /// `delivered`, whose `Message` event names `event`, and a Running session
    /// whose queued message names `queued`.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    fn history_store(
        dir: &Path,
        delivered: &str,
        event: &str,
        queued: &str,
    ) -> Arc<rsid_core::store_lock::Mutex<Store>> {
        use rsi_common::types::{ConversationEvent, EventType, Role, SessionStatus};
        let mut store = Store::open_in_memory().unwrap();
        let mut done = rsid_store::test_support::make_test_session();
        done.status = SessionStatus::Running;
        store.insert_session(&done).unwrap();
        let message = store
            .queue_operator_message(
                done.id,
                &format!("look @{}", dir.join(delivered).display()),
                "delivered-key",
            )
            .unwrap();
        assert_eq!(
            store.claim_operator_message(done.id).unwrap().unwrap().id,
            message.id
        );
        store
            .mark_operator_message_effect_possible(message.id)
            .unwrap();
        store.settle_operator_message(message.id, true).unwrap();
        store
            .insert_event(&ConversationEvent {
                id: 0,
                session_id: done.id,
                sequence: 1,
                event_type: EventType::Message,
                role: Some(Role::User),
                content: format!("pasted @{}", dir.join(event).display()),
                tool_name: None,
                tool_input: None,
                created_at: chrono::Utc::now(),
                offload_id: None,
                tool_use_id: None,
                metadata: None,
            })
            .unwrap();
        store
            .update_session_status(done.id, SessionStatus::Completed)
            .unwrap();
        let mut live = rsid_store::test_support::make_test_session();
        live.status = SessionStatus::Running;
        store.insert_session(&live).unwrap();
        store
            .queue_operator_message(
                live.id,
                &format!("next @{}", dir.join(queued).display()),
                "queued-key",
            )
            .unwrap();
        Arc::new(rsid_core::store_lock::Mutex::new(store))
    }

    /// #86 ruling: by default a pass removes only abandoned uploads; an image
    /// a finished session's message or event named is history and is kept.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn by_default_referenced_images_of_a_completed_session_are_kept_and_abandoned_ones_removed()
     {
        let (_tmp, dir) = synthetic_home();
        let delivered = hex_name('1', "png");
        let event = hex_name('2', "jpg");
        let queued = hex_name('3', "gif");
        let abandoned = hex_name('4', "webp");
        let young = hex_name('5', "png");
        for name in [&delivered, &event, &queued, &abandoned] {
            write_aged(&dir.join(name), name.as_bytes(), 400);
        }
        write_aged(&dir.join(&young), b"young", 6);
        // Local #155: the TUI level, one directory up. Its paste named by the
        // finished session's Message event is kept; an abandoned one goes.
        let tui = dir.parent().unwrap().to_path_buf();
        let tui_named = format!("{}.png", uuid::Uuid::new_v4());
        let tui_abandoned = format!("{}.png", uuid::Uuid::new_v4());
        write_aged(&tui.join(&tui_named), b"tui named", 400);
        write_aged(&tui.join(&tui_abandoned), b"tui abandoned", 400);
        let store = history_store(&dir, &delivered, &event, &queued);
        {
            let guard = store.lock().await;
            let session = rsid_store::test_support::make_test_session();
            guard.insert_session(&session).unwrap();
            guard
                .insert_event(&rsi_common::types::ConversationEvent {
                    id: 0,
                    session_id: session.id,
                    sequence: 1,
                    event_type: rsi_common::types::EventType::Message,
                    role: Some(rsi_common::types::Role::User),
                    content: format!("tui paste @{}", tui.join(&tui_named).display()),
                    tool_name: None,
                    tool_input: None,
                    created_at: chrono::Utc::now(),
                    offload_id: None,
                    tool_use_id: None,
                    metadata: None,
                })
                .unwrap();
            guard
                .update_session_status(session.id, rsi_common::types::SessionStatus::Completed)
                .unwrap();
        }

        let report = run_policy_pass(&store, dir.clone(), "test", DEFAULT_POLICY)
            .await
            .expect("pass");

        assert_eq!(report.removed, 2, "{report:?}");
        assert_eq!(report.removed_referenced, 0, "{report:?}");
        assert_eq!(report.kept_referenced, 4, "{report:?}");
        assert_eq!(report.kept_young, 1, "{report:?}");
        for kept in [&delivered, &event, &queued, &young] {
            assert!(dir.join(kept).is_file(), "{kept} kept");
        }
        assert!(!dir.join(&abandoned).exists());
        assert!(tui.join(&tui_named).is_file(), "named TUI paste kept");
        assert!(!tui.join(&tui_abandoned).exists());
    }

    /// #86 ruling ("no conversation event names it"): an image named only in
    /// a `ToolResult` (or a `ToolUse` input) of a Completed session is
    /// history and is kept by default, at both levels; an abandoned one goes.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn by_default_an_image_named_only_in_a_tool_result_of_a_completed_session_is_kept() {
        use rsi_common::types::{ConversationEvent, EventType, Role, SessionStatus};
        let (_tmp, dir) = synthetic_home();
        let tool_result = hex_name('7', "png");
        let tool_input = hex_name('8', "gif");
        let abandoned = hex_name('9', "webp");
        for name in [&tool_result, &tool_input, &abandoned] {
            write_aged(&dir.join(name), name.as_bytes(), 400);
        }
        let tui = dir.parent().unwrap().to_path_buf();
        let tui_tool_result = format!("{}.png", uuid::Uuid::new_v4());
        write_aged(&tui.join(&tui_tool_result), b"tui tool result", 400);

        let store = Store::open_in_memory().unwrap();
        let mut session = rsid_store::test_support::make_test_session();
        session.status = SessionStatus::Running;
        store.insert_session(&session).unwrap();
        let event = |sequence, event_type, content: String, input| ConversationEvent {
            id: 0,
            session_id: session.id,
            sequence,
            event_type,
            role: Some(Role::Assistant),
            content,
            tool_name: Some("Bash".to_string()),
            tool_input: input,
            created_at: chrono::Utc::now(),
            offload_id: None,
            tool_use_id: None,
            metadata: None,
        };
        for row in [
            event(
                1,
                EventType::ToolResult,
                format!(
                    "{}\n{}",
                    dir.join(&tool_result).display(),
                    tui.join(&tui_tool_result).display()
                ),
                None,
            ),
            event(
                2,
                EventType::ToolUse,
                String::new(),
                Some(Box::new(serde_json::json!({
                    "file_path": dir.join(&tool_input).display().to_string()
                }))),
            ),
        ] {
            store.insert_event(&row).unwrap();
        }
        store
            .update_session_status(session.id, SessionStatus::Completed)
            .unwrap();
        let store = Arc::new(rsid_core::store_lock::Mutex::new(store));

        let report = run_policy_pass(&store, dir.clone(), "test", DEFAULT_POLICY)
            .await
            .expect("pass");

        assert_eq!(report.removed, 1, "{report:?}");
        assert_eq!(report.kept_referenced, 3, "{report:?}");
        assert!(
            dir.join(&tool_result).is_file(),
            "ToolResult-named image kept"
        );
        assert!(dir.join(&tool_input).is_file(), "ToolUse-named image kept");
        assert!(
            tui.join(&tui_tool_result).is_file(),
            "ToolResult-named TUI paste kept"
        );
        assert!(!dir.join(&abandoned).exists());
    }

    /// #86 ruling: with `paste_retention_referenced_days` on, referenced
    /// images past that age are removed, but never one a live text names nor
    /// one younger than the setting.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn with_the_setting_on_referenced_images_past_the_age_are_removed() {
        let (_tmp, dir) = synthetic_home();
        let delivered = hex_name('1', "png");
        let event = hex_name('2', "jpg");
        let queued = hex_name('3', "gif");
        let recent = hex_name('6', "png");
        for name in [&delivered, &event, &queued] {
            write_aged(&dir.join(name), name.as_bytes(), 400);
        }
        // Referenced, past the 7-day abandoned age, inside the 30-day setting.
        write_aged(&dir.join(&recent), b"recent", 10);
        let store = history_store(&dir, &delivered, &event, &queued);
        {
            // Named only by a message the operator withdrew (never sent).
            let mut guard = store.lock().await;
            let mut session = rsid_store::test_support::make_test_session();
            session.status = rsi_common::types::SessionStatus::Running;
            guard.insert_session(&session).unwrap();
            let unsent = guard
                .queue_operator_message(
                    session.id,
                    &format!("old @{}", dir.join(&recent).display()),
                    "recent-key",
                )
                .unwrap();
            guard.withdraw_operator_message(unsent.id).unwrap();
        }

        let report = run_policy_pass(&store, dir.clone(), "test", OPT_IN_POLICY)
            .await
            .expect("pass");

        assert_eq!(report.removed, 2, "{report:?}");
        assert_eq!(report.removed_referenced, 2, "{report:?}");
        assert_eq!(report.kept_referenced, 2, "{report:?}");
        assert!(!dir.join(&delivered).exists());
        assert!(!dir.join(&event).exists());
        assert!(
            dir.join(&queued).is_file(),
            "a queued message still names it"
        );
        assert!(dir.join(&recent).is_file(), "younger than the setting");
    }

    /// Fail-safe: a reference scan that cannot finish inside its budget
    /// proves nothing, so the pass that needs it removes nothing.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_reference_scan_past_its_budget_proves_nothing() {
        let (_tmp, dir) = synthetic_home();
        let store = history_store(
            &dir,
            &hex_name('1', "png"),
            &hex_name('2', "png"),
            &hex_name('3', "png"),
        );
        let candidates = HashSet::from([hex_name('1', "png"), hex_name('9', "png")]);
        let refused = ever_referenced(&store, &candidates, Duration::ZERO).await;
        assert!(refused.is_err(), "{refused:?}");
        let found = ever_referenced(&store, &candidates, REFERENCE_SCAN_BUDGET)
            .await
            .unwrap();
        assert_eq!(found, HashSet::from([hex_name('1', "png")]));
    }

    /// Hub #3322 finding 2: a reference committed after the reference scan
    /// and before the removal keeps the image, for every source the scan
    /// reads: a new operator message, an edited one, a new conversation event
    /// (a `ToolResult`) and a new enabled scheduled job. An unnamed image of
    /// the same pass is still removed.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_reference_committed_between_the_scan_and_the_removal_keeps_the_image() {
        use rsi_common::types::{
            ConversationEvent, EventType, Recurrence, Role, ScheduleSpec, ScheduledJob,
            SessionStatus,
        };
        for source in [
            "new message",
            "edited message",
            "tool result",
            "scheduled job",
        ] {
            let (_tmp, dir) = synthetic_home();
            let named = hex_name('a', "png");
            let control = hex_name('b', "jpg");
            for name in [&named, &control] {
                write_aged(&dir.join(name), name.as_bytes(), 400);
            }
            let mut store = Store::open_in_memory().unwrap();
            let mut session = rsid_store::test_support::make_test_session();
            session.status = SessionStatus::Running;
            store.insert_session(&session).unwrap();
            let queued = store
                .queue_operator_message(session.id, "plain words", "before")
                .unwrap();
            let store = Arc::new(rsid_core::store_lock::Mutex::new(store));
            let text = format!("look @{}", dir.join(&named).display());
            let between: BetweenScanAndRemoval = Box::new(move |store: &mut Store| match source {
                "new message" => {
                    store
                        .queue_operator_message(session.id, &text, "late")
                        .unwrap();
                }
                "edited message" => {
                    store.edit_operator_message(queued.id, &text).unwrap();
                }
                "tool result" => {
                    store
                        .insert_event(&ConversationEvent {
                            id: 0,
                            session_id: session.id,
                            sequence: 1,
                            event_type: EventType::ToolResult,
                            role: Some(Role::Assistant),
                            content: text,
                            tool_name: Some("Bash".to_string()),
                            tool_input: None,
                            created_at: chrono::Utc::now(),
                            offload_id: None,
                            tool_use_id: None,
                            metadata: None,
                        })
                        .unwrap();
                }
                _ => {
                    let now = chrono::Utc::now();
                    store
                        .insert_scheduled_job(&ScheduledJob {
                            id: uuid::Uuid::new_v4(),
                            name: "late job".to_string(),
                            message: text,
                            schedule: ScheduleSpec {
                                recurrence: Recurrence::EverySeconds(3600),
                                anchor: now,
                            },
                            last_fired_at: None,
                            next_fire_at: now,
                            enabled: true,
                            working_dir: None,
                            provider: None,
                            model: None,
                            project_id: None,
                            created_at: now,
                            updated_at: now,
                            wake_mode: Default::default(),
                            wake_session_id: None,
                        })
                        .unwrap();
                }
            });

            let report =
                run_policy_pass_with(&store, dir.clone(), "test", DEFAULT_POLICY, Some(between))
                    .await
                    .expect("pass");

            assert!(
                dir.join(&named).is_file(),
                "{source}: late-named image kept"
            );
            assert!(
                !dir.join(&control).exists(),
                "{source}: unnamed image removed"
            );
            assert_eq!(report.removed, 1, "{source}: {report:?}");
            assert_eq!(report.kept_referenced, 1, "{source}: {report:?}");
        }
    }

    /// #179: a running session with a `ToolResult` event carrying `content`.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    fn store_with_tool_results(contents: &[&str]) -> (Store, uuid::Uuid, Vec<i64>) {
        use rsi_common::types::{ConversationEvent, EventType, Role, SessionStatus};
        let store = Store::open_in_memory().unwrap();
        let mut session = rsid_store::test_support::make_test_session();
        session.status = SessionStatus::Running;
        store.insert_session(&session).unwrap();
        let ids = contents
            .iter()
            .zip(1..)
            .map(|(content, sequence)| {
                store
                    .insert_event(&ConversationEvent {
                        id: 0,
                        session_id: session.id,
                        sequence,
                        event_type: EventType::ToolResult,
                        role: Some(Role::Assistant),
                        content: (*content).to_string(),
                        tool_name: Some("Bash".to_string()),
                        tool_input: None,
                        created_at: chrono::Utc::now(),
                        offload_id: None,
                        tool_use_id: None,
                        metadata: None,
                    })
                    .unwrap()
            })
            .collect();
        (store, session.id, ids)
    }

    /// #179: the text a stored event carries now.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    fn event_content(store: &Store, session: uuid::Uuid, id: i64) -> String {
        store
            .load_events(session)
            .unwrap()
            .into_iter()
            .find(|event| event.id == id)
            .expect("stored event")
            .content
    }

    /// #179: an in-place rewrite (compression/offload) that names a pasted
    /// image keeps it through a full pass; the rewrite cannot name an image
    /// its row did not, so the unnamed image of the same pass is removed.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_paste_image_an_in_place_rewrite_names_is_kept_through_a_full_pass() {
        let (_tmp, dir) = synthetic_home();
        let named = hex_name('a', "png");
        let control = hex_name('b', "jpg");
        for name in [&named, &control] {
            write_aged(&dir.join(name), name.as_bytes(), 400);
        }
        let original = format!(
            "cat {}\n{}",
            dir.join(&named).display(),
            "a long tool output line\n".repeat(100)
        );
        let (store, session, ids) = store_with_tool_results(&[&original]);
        let rewritten = format!("[offloaded] read {}", dir.join(&named).display());
        store.update_event_content(ids[0], &rewritten).unwrap();
        let refused = store
            .update_event_content(
                ids[0],
                &format!("{rewritten} and {}", dir.join(&control).display()),
            )
            .unwrap_err();
        assert!(
            refused.to_string().contains("paste_reference_added"),
            "{refused}"
        );
        let store = Arc::new(rsid_core::store_lock::Mutex::new(store));

        let report = run_policy_pass(&store, dir.clone(), "test", DEFAULT_POLICY)
            .await
            .expect("pass");

        assert!(dir.join(&named).is_file(), "rewritten reference kept");
        assert!(!dir.join(&control).exists(), "unnamed image removed");
        assert_eq!(report.removed, 1, "{report:?}");
        assert_eq!(report.kept_referenced, 1, "{report:?}");
        assert_eq!(
            event_content(&*store.lock().await, session, ids[0]),
            rewritten
        );
    }

    /// #179, like the hub #3322 race above: an in-place rewrite between the
    /// reference scan and the removal keeps the image it names, and a rewrite
    /// there that would name an image its row never named (the race the event
    /// mark cannot see) is refused, so no stored text names the removed image.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn an_in_place_rewrite_between_the_scan_and_the_removal_keeps_the_image() {
        let (_tmp, dir) = synthetic_home();
        let named = hex_name('a', "png");
        let control = hex_name('b', "jpg");
        for name in [&named, &control] {
            write_aged(&dir.join(name), name.as_bytes(), 400);
        }
        let original = format!("cat {}\nmany lines", dir.join(&named).display());
        let (store, session, ids) = store_with_tool_results(&[&original, "plain words"]);
        let (named_event, plain_event) = (ids[0], ids[1]);
        let store = Arc::new(rsid_core::store_lock::Mutex::new(store));
        let rewritten = format!("[offloaded] read {}", dir.join(&named).display());
        let adding = format!("[offloaded] read {}", dir.join(&control).display());
        let expected = rewritten.clone();
        let between: BetweenScanAndRemoval = Box::new(move |store: &mut Store| {
            store.update_event_content(named_event, &rewritten).unwrap();
            let refused = store
                .update_event_content(plain_event, &adding)
                .unwrap_err();
            assert!(
                refused.to_string().contains("paste_reference_added"),
                "{refused}"
            );
        });

        let report =
            run_policy_pass_with(&store, dir.clone(), "test", DEFAULT_POLICY, Some(between))
                .await
                .expect("pass");

        assert!(dir.join(&named).is_file(), "rewritten reference kept");
        assert!(!dir.join(&control).exists(), "unnamed image removed");
        assert_eq!(report.removed, 1, "{report:?}");
        assert_eq!(report.kept_referenced, 1, "{report:?}");
        let store = store.lock().await;
        assert_eq!(event_content(&store, session, named_event), expected);
        assert_eq!(event_content(&store, session, plain_event), "plain words");
    }

    /// Hub #3322 finding 2: a cold-storage move or restore between the scan
    /// and the removal proves nothing, so the pass removes nothing.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_cold_move_between_the_scan_and_the_removal_removes_nothing() {
        let (_tmp, dir) = synthetic_home();
        let abandoned = hex_name('c', "png");
        write_aged(&dir.join(&abandoned), b"abandoned", 400);
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let between: BetweenScanAndRemoval = Box::new(|store: &mut Store| {
            let mut archived = rsid_store::test_support::make_test_session();
            archived.status = rsi_common::types::SessionStatus::Archived;
            store.insert_session(&archived).unwrap();
            store.cold_index_row_for_test(archived.id, 1).unwrap();
        });

        let report =
            run_policy_pass_with(&store, dir.clone(), "test", DEFAULT_POLICY, Some(between)).await;

        assert_eq!(report, None);
        assert!(dir.join(&abandoned).is_file(), "kept");
    }

    /// #178 (was hub #3322 finding 3): an archived session over the scan's
    /// size bound is streamed, not refused from its index row; one whose
    /// archive cannot be read still fails the pass closed: nothing is removed.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn an_unreadable_archive_over_the_scan_bound_removes_nothing() {
        let (_tmp, dir) = synthetic_home();
        let abandoned = hex_name('d', "png");
        write_aged(&dir.join(&abandoned), b"abandoned", 400);
        let store = Store::open_in_memory().unwrap();
        let mut archived = rsid_store::test_support::make_test_session();
        archived.status = rsi_common::types::SessionStatus::Archived;
        store.insert_session(&archived).unwrap();
        store
            .cold_index_row_for_test(
                archived.id,
                crate::store::cold_archive::PASTE_REFERENCE_ARCHIVE_MAX_RAW_BYTES + 1,
            )
            .unwrap();
        let store = Arc::new(rsid_core::store_lock::Mutex::new(store));
        let candidates = HashSet::from([abandoned.clone()]);
        let refused = ever_referenced(&store, &candidates, REFERENCE_SCAN_BUDGET)
            .await
            .expect_err("no archive at the indexed path");
        assert!(refused.contains("cold_archive_unavailable"), "{refused}");
        assert!(refused.contains(&archived.id.to_string()), "{refused}");

        let report = run_policy_pass(&store, dir.clone(), "test", DEFAULT_POLICY).await;

        assert_eq!(report, None);
        assert!(dir.join(&abandoned).is_file(), "kept");
    }

    /// #178: text zstd cannot shrink much (hex of a fixed pseudo-random
    /// sequence), so an archived session of a few such rows is over a small
    /// bound.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    fn noise(seed: u64, bytes: usize) -> String {
        let mut state = seed;
        let mut out = String::with_capacity(bytes + 16);
        while out.len() < bytes {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            out.push_str(&format!("{:016x}", state ^ (state >> 29)));
        }
        out.truncate(bytes);
        out
    }

    /// #178: an archived session over the scan's size bound no longer stalls
    /// cleanup: the scan streams its archive, so the image one of its archived
    /// rows names is kept and an image nothing names is removed.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn an_archive_over_the_scan_bound_is_streamed_so_unnamed_images_are_still_removed() {
        use crate::store::cold_archive::{
            self as cold, ColdMode, ColdSettings, DirectStoreAccess, PassLimits, PassParams,
        };
        use rsi_common::types::{ConversationEvent, EventType, Role, SessionStatus};
        const BOUND: i64 = 32 * 1024;
        let (tmp, dir) = synthetic_home();
        let named = hex_name('e', "png");
        let unnamed = hex_name('f', "png");
        write_aged(&dir.join(&named), b"named", 400);
        write_aged(&dir.join(&unnamed), b"unnamed", 400);

        let db = tmp.path().join("rsi.db");
        let store = Store::open_test_fixture(&db).unwrap();
        let mut session = rsid_store::test_support::make_test_session();
        session.status = SessionStatus::Archived;
        session.created_at = chrono::Utc::now() - chrono::Duration::days(40);
        store.insert_session(&session).unwrap();
        let old = chrono::Utc::now() - chrono::Duration::days(41);
        let event = |sequence: i32, event_type, role, content: String| ConversationEvent {
            id: 0,
            session_id: session.id,
            sequence,
            event_type,
            role,
            content,
            tool_name: None,
            tool_input: None,
            created_at: old,
            offload_id: None,
            tool_use_id: None,
            metadata: None,
        };
        store
            .insert_event(&event(
                1,
                EventType::Message,
                Some(Role::User),
                "start".into(),
            ))
            .unwrap();
        for sequence in 2..10 {
            let mut content = noise(u64::try_from(sequence).unwrap(), 20 * 1024);
            if sequence == 5 {
                content.push_str(&format!(" see @{}", dir.join(&named).display()));
            }
            store
                .insert_event(&event(sequence, EventType::ToolResult, None, content))
                .unwrap();
        }
        store
            .insert_event(&event(
                10,
                EventType::Message,
                Some(Role::Assistant),
                "done".into(),
            ))
            .unwrap();

        let settings = ColdSettings {
            enabled: true,
            after_days: 14,
            dir: tmp.path().join("archive"),
            forbidden_roots: Vec::new(),
        };
        let read = cold::open_read_connection(&db).unwrap();
        let run_id = store.cold_begin_run("run_now", ColdMode::Stage).unwrap();
        let staged = cold::run_stage_pass(
            &DirectStoreAccess(&store),
            &read,
            &PassParams {
                run_id,
                settings: &settings,
                limits: PassLimits {
                    batch_max_sessions: 32,
                    batch_max_raw_bytes: 64 * 1024 * 1024,
                    wall_budget: Duration::from_secs(120),
                    batch_sleep: Duration::ZERO,
                    gate_poll: Duration::ZERO,
                },
                batch_gate: None,
                cancel: None,
            },
        )
        .unwrap();
        drop(read);
        assert_eq!(staged.sessions_staged, 1, "{staged:?}");
        assert!(matches!(
            store.cold_move_session_for_test(session.id).unwrap(),
            cold::MoveOutcome::Moved(_)
        ));
        let storage = store.cold_session_storage(&session.id.to_string()).unwrap();
        assert_eq!(storage.tier, "cold");
        assert!(storage.raw_bytes > 2 * BOUND, "{storage:?}");

        let store = Arc::new(rsid_core::store_lock::Mutex::new(store));
        let _bound = cold::override_paste_reference_archive_bound_for_test(BOUND);
        let _ = cold::stream_stats::take();
        let report = run_policy_pass(&store, dir.clone(), "test", DEFAULT_POLICY)
            .await
            .expect("an archive over the bound no longer stalls the pass");
        let stats = cold::stream_stats::take();

        assert!(
            dir.join(&named).is_file(),
            "the image the archive names is kept"
        );
        assert!(!dir.join(&unnamed).exists(), "the unnamed image is removed");
        assert_eq!(report.removed, 1, "{report:?}");
        assert_eq!(report.kept_referenced, 1, "{report:?}");
        assert_eq!(stats.rows, 8, "the archive was streamed: {stats:?}");
        assert!(
            i64::try_from(stats.largest_row).unwrap() <= BOUND,
            "{stats:?}"
        );
    }
}
