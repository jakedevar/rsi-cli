//! Owner-only modes for daemon-private state (#1844).
//!
//! `~/.rsi` holds session transcripts, prompts, Issue bodies and manager mail,
//! so only the owning user may read it. On Unix the data directory is 0700 and
//! the database (plus its `-wal`/`-shm` sidecars, backups, spools and logs) is
//! 0600. Tightening only ever *removes* group/other bits: the owner's bits are
//! kept (so an owner-executable script stays executable) and nothing outside the
//! paths a caller names is touched. Symlinks are never followed or changed.
//!
//! Windows: no mode bits are applied. Per-user profile directories already
//! inherit an ACL limited to the user, SYSTEM and administrators, and the
//! daemon does not rewrite ACLs. Behaviour there is unchanged.

use std::io;
use std::path::{Path, PathBuf};

/// Mode of a daemon-private directory.
#[cfg(unix)]
pub const PRIVATE_DIR_MODE: u32 = 0o700;
/// Mode of a daemon-private file.
#[cfg(unix)]
pub const PRIVATE_FILE_MODE: u32 = 0o600;

/// Create `dir` and any missing parents, each new directory 0700 on Unix.
/// Existing directories are left alone (use [`tighten_dir`] for those).
pub fn create_private_dir_all(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(PRIVATE_DIR_MODE)
            .create(dir)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
    }
}

/// Open `path` for read/write, creating it 0600 on Unix when absent. An
/// existing file keeps its content; use [`tighten_file`] to fix its mode.
pub fn create_private_file(path: &Path) -> io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(PRIVATE_FILE_MODE);
    }
    options.open(path).map(drop)
}

/// Applies a mode to a path. The real implementation is `set_permissions`;
/// tests inject a failing one.
type SetMode<'a> = &'a dyn Fn(&Path, u32) -> io::Result<()>;

#[cfg(unix)]
fn chmod(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn chmod(_path: &Path, _mode: u32) -> io::Result<()> {
    Ok(())
}

/// One path whose mode could not be tightened.
#[derive(Debug)]
pub struct TightenFailure {
    pub path: PathBuf,
    pub error: io::Error,
}

/// What a tightening pass did: the paths whose mode changed and the paths it
/// could not tighten. A failure never stops the pass (startup continues) but is
/// never silent either: [`TightenReport::log_failures`] names each path once.
#[derive(Debug, Default)]
pub struct TightenReport {
    pub changed: Vec<PathBuf>,
    pub failures: Vec<TightenFailure>,
}

impl TightenReport {
    fn record(&mut self, path: &Path, result: io::Result<bool>) {
        match result {
            Ok(true) => self.changed.push(path.to_path_buf()),
            Ok(false) => {}
            Err(error) => self.failures.push(TightenFailure {
                path: path.to_path_buf(),
                error,
            }),
        }
    }

    fn absorb(&mut self, other: TightenReport) {
        self.changed.extend(other.changed);
        self.failures.extend(other.failures);
    }

    /// Log every failure with its path and errno, once per path per process.
    /// Returns how many were logged.
    pub fn log_failures(&self) -> usize {
        static LOGGED: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());
        let mut logged = LOGGED.lock().unwrap_or_else(|poison| poison.into_inner());
        let mut count = 0;
        for failure in &self.failures {
            if logged.contains(&failure.path) {
                continue;
            }
            logged.push(failure.path.clone());
            count += 1;
            tracing::warn!(
                path = %failure.path.display(),
                errno = ?failure.error.raw_os_error(),
                error = %failure.error,
                "Could not tighten daemon state to owner-only; it stays readable by others"
            );
        }
        count
    }
}

/// Remove group/other access from one regular file. Returns whether the mode
/// changed. Missing paths and symlinks are skipped (`Ok(false)`).
pub fn tighten_file(path: &Path) -> io::Result<bool> {
    tighten(path, false, &chmod)
}

/// Make one directory owner-only (0700). Returns whether the mode changed.
/// Missing paths and symlinks are skipped (`Ok(false)`).
pub fn tighten_dir(path: &Path) -> io::Result<bool> {
    tighten(path, true, &chmod)
}

#[cfg(unix)]
fn tighten(path: &Path, directory: bool, set_mode: SetMode) -> io::Result<bool> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() || metadata.is_dir() != directory {
        return Ok(false);
    }
    let current = metadata.permissions().mode() & 0o7777;
    // Keep the owner's bits; a directory must stay traversable by its owner.
    let wanted = if directory {
        PRIVATE_DIR_MODE
    } else {
        current & 0o700
    };
    if current == wanted {
        return Ok(false);
    }
    set_mode(path, wanted)?;
    Ok(true)
}

#[cfg(not(unix))]
fn tighten(_path: &Path, _directory: bool, _set_mode: SetMode) -> io::Result<bool> {
    Ok(false)
}

/// Tighten `root` (a directory) and everything below it: directories 0700,
/// files owner-only. Symlinks are neither followed nor changed. Returns the
/// paths whose mode changed; an unreadable entry is skipped and a path that
/// cannot be tightened is logged ([`TightenReport::log_failures`]).
pub fn tighten_tree(root: &Path) -> Vec<PathBuf> {
    let report = tighten_tree_report(root, &chmod);
    report.log_failures();
    report.changed
}

fn tighten_tree_report(root: &Path, set_mode: SetMode) -> TightenReport {
    let mut report = TightenReport::default();
    tighten_tree_into(root, set_mode, &mut report);
    report
}

fn tighten_tree_into(dir: &Path, set_mode: SetMode, report: &mut TightenReport) {
    report.record(dir, tighten(dir, true, set_mode));
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            tighten_tree_into(&path, set_mode, report);
        } else if file_type.is_file() {
            report.record(&path, tighten(&path, false, set_mode));
        }
    }
}

/// Tighten the database file and its SQLite sidecars (`-wal`, `-shm`,
/// `-journal`, `.migration-backup.json`) next to it. Returns changed paths;
/// failures are logged.
pub fn tighten_database_files(database: &Path) -> Vec<PathBuf> {
    let mut report = TightenReport::default();
    for suffix in ["", "-wal", "-shm", "-journal", ".migration-backup.json"] {
        let mut name = database.as_os_str().to_os_string();
        name.push(suffix);
        let path = PathBuf::from(name);
        report.record(&path, tighten(&path, false, &chmod));
    }
    report.log_failures();
    report.changed
}

/// Subdirectories of the data directory that hold daemon-private state
/// (database backups, tool-output spills, logs, memory, prompts, job and queue
/// spools, recovery evidence). Everything else under the data directory is
/// left as it is, in particular `sandboxes/` (the agents' git worktrees).
pub const PRIVATE_SUBDIRS: &[&str] = &[
    "backups", "spill", "logs", "memory", "prompts", "jobs", "queue", "recovery",
];

/// File-name prefixes directly under the data directory that are daemon-private
/// (`rsi.db*` with its sidecars, `memory.sqlite*`, `daemon.log*` with its
/// rotated archives).
pub const PRIVATE_FILE_PREFIXES: &[&str] = &["rsi.db", "memory.sqlite", "daemon.log"];

/// Startup hardening of the daemon data directory: create it 0700 when absent,
/// tighten it and the daemon-private state named above when looser. Only Unix
/// changes anything; elsewhere the directory is just created. Failing to create
/// the directory is an error; failing to tighten a path is recorded in the
/// returned report, which the caller logs once its tracing subscriber exists
/// (this runs before that, so the log file is never created ahead of it).
pub fn harden_data_dir(data_dir: &Path) -> io::Result<TightenReport> {
    harden_data_dir_with(data_dir, &chmod)
}

/// The part of [`harden_data_dir`] that must precede the first log line: the
/// directory itself, the daemon-private files directly under it and the
/// private subdirectories' own modes. It reads one directory, so it is cheap.
/// The recursive walk of the subdirectories' contents is
/// [`harden_private_subtrees`] (#2375): the spill, job and queue spools hold
/// tens of thousands of entries and walking them cost ~500 ms of every
/// daemon start. A private subdirectory is 0700 here, so nothing below it is
/// reachable by others while the walk runs.
pub fn harden_data_dir_top(data_dir: &Path) -> io::Result<TightenReport> {
    harden_data_dir_top_with(data_dir, &chmod)
}

/// Tighten everything below the private subdirectories (files owner-only,
/// directories 0700). Run it after startup, off the critical path; it reports
/// like [`harden_data_dir`].
pub fn harden_private_subtrees(data_dir: &Path) -> TightenReport {
    harden_private_subtrees_with(data_dir, &chmod)
}

fn harden_data_dir_with(data_dir: &Path, set_mode: SetMode) -> io::Result<TightenReport> {
    let mut report = harden_data_dir_top_with(data_dir, set_mode)?;
    report.absorb(harden_private_subtrees_with(data_dir, set_mode));
    Ok(report)
}

fn harden_data_dir_top_with(data_dir: &Path, set_mode: SetMode) -> io::Result<TightenReport> {
    create_private_dir_all(data_dir)?;
    let mut report = TightenReport::default();
    report.record(data_dir, tighten(data_dir, true, set_mode));
    if let Ok(entries) = std::fs::read_dir(data_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let is_file = entry.file_type().is_ok_and(|kind| kind.is_file());
            if is_file
                && PRIVATE_FILE_PREFIXES
                    .iter()
                    .any(|prefix| name.starts_with(prefix))
            {
                let path = entry.path();
                report.record(&path, tighten(&path, false, set_mode));
            }
        }
    }
    for subdir in PRIVATE_SUBDIRS {
        let path = data_dir.join(subdir);
        report.record(&path, tighten(&path, true, set_mode));
    }
    Ok(report)
}

fn harden_private_subtrees_with(data_dir: &Path, set_mode: SetMode) -> TightenReport {
    let mut report = TightenReport::default();
    for subdir in PRIVATE_SUBDIRS {
        report.absorb(tighten_tree_report(&data_dir.join(subdir), set_mode));
    }
    report
}

/// The process umask before [`restrict_umask`], to hand back to
/// [`PreviousUmask::restore`].
#[derive(Debug)]
pub struct PreviousUmask {
    #[cfg(unix)]
    mask: nix::sys::stat::Mode,
}

/// Set umask 0o077 so nothing the process creates from here on is readable by
/// group or other, even before the data directory is hardened. Call it first
/// thing, while the process is single-threaded. Windows: no-op.
pub fn restrict_umask() -> PreviousUmask {
    #[cfg(unix)]
    {
        use nix::sys::stat::{Mode, umask};
        PreviousUmask {
            mask: umask(Mode::from_bits_truncate(0o077)),
        }
    }
    #[cfg(not(unix))]
    {
        PreviousUmask {}
    }
}

impl PreviousUmask {
    /// Put the original umask back. The daemon does this once its data
    /// directory is 0700 (and every private file it creates asks for 0600
    /// explicitly), so provider subprocesses and git worktrees do not inherit a
    /// 077 mask that would change the modes of the operator's own files.
    pub fn restore(self) {
        #[cfg(unix)]
        {
            nix::sys::stat::umask(self.mask);
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
    }

    fn set(path: &Path, mode: u32) {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn fresh_creation_is_owner_only_regardless_of_umask() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("a").join("b");
        create_private_dir_all(&dir).unwrap();
        assert_eq!(mode(&dir), 0o700);
        let file = dir.join("state.db");
        create_private_file(&file).unwrap();
        assert_eq!(mode(&file), 0o600);
    }

    #[test]
    fn tightening_removes_group_and_other_bits_and_keeps_owner_bits() {
        let temp = tempfile::tempdir().unwrap();
        let loose = temp.path().join("loose.db");
        std::fs::write(&loose, b"x").unwrap();
        set(&loose, 0o644);
        let script = temp.path().join("run.sh");
        std::fs::write(&script, b"#!/bin/sh").unwrap();
        set(&script, 0o755);
        let dir = temp.path().join("spill");
        std::fs::create_dir(&dir).unwrap();
        set(&dir, 0o755);

        assert!(tighten_file(&loose).unwrap());
        assert!(tighten_file(&script).unwrap());
        assert!(tighten_dir(&dir).unwrap());
        assert_eq!(mode(&loose), 0o600);
        assert_eq!(mode(&script), 0o700);
        assert_eq!(mode(&dir), 0o700);
        assert!(!tighten_file(&loose).unwrap(), "already tight is a no-op");
        assert!(!tighten_file(&temp.path().join("missing")).unwrap());
    }

    #[test]
    fn tree_tightening_skips_symlinks_and_reports_changes() {
        let temp = tempfile::tempdir().unwrap();
        let outside = temp.path().join("operator-owned.txt");
        std::fs::write(&outside, b"x").unwrap();
        set(&outside, 0o644);
        let root = temp.path().join("backups");
        std::fs::create_dir_all(root.join("nested")).unwrap();
        let file = root.join("nested").join("old.db");
        std::fs::write(&file, b"x").unwrap();
        set(&file, 0o644);
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();

        let changed = tighten_tree(&root);
        assert!(changed.contains(&file));
        assert_eq!(mode(&file), 0o600);
        assert_eq!(mode(&root.join("nested")), 0o700);
        assert_eq!(mode(&root), 0o700);
        assert_eq!(mode(&outside), 0o644, "symlink target is not ours");
    }

    #[test]
    fn data_dir_hardening_covers_private_state_and_leaves_the_rest() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join(".rsi");
        std::fs::create_dir(&data).unwrap();
        set(&data, 0o755);
        for name in ["rsi.db", "rsi.db-wal", "rsi.db-shm", "daemon.log.2026"] {
            std::fs::write(data.join(name), b"x").unwrap();
            set(&data.join(name), 0o644);
        }
        std::fs::create_dir(data.join("backups")).unwrap();
        std::fs::write(data.join("backups").join("pre.db"), b"x").unwrap();
        set(&data.join("backups").join("pre.db"), 0o644);
        std::fs::create_dir(data.join("sandboxes")).unwrap();
        std::fs::write(data.join("sandboxes").join("keep.txt"), b"x").unwrap();
        set(&data.join("sandboxes").join("keep.txt"), 0o644);

        let report = harden_data_dir(&data).unwrap();
        assert!(report.failures.is_empty());
        let changed = report.changed;
        assert!(changed.contains(&data));
        assert_eq!(mode(&data), 0o700);
        for name in ["rsi.db", "rsi.db-wal", "rsi.db-shm", "daemon.log.2026"] {
            assert_eq!(mode(&data.join(name)), 0o600, "{name}");
        }
        assert_eq!(mode(&data.join("backups").join("pre.db")), 0o600);
        assert_eq!(mode(&data.join("sandboxes").join("keep.txt")), 0o644);
        assert!(
            harden_data_dir(&data).unwrap().changed.is_empty(),
            "second pass is quiet"
        );

        let fresh = temp.path().join("fresh").join(".rsi");
        assert!(harden_data_dir(&fresh).unwrap().changed.is_empty());
        assert_eq!(mode(&fresh), 0o700);
    }

    #[test]
    fn top_level_hardening_leaves_the_subtree_walk_to_its_own_pass() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join(".rsi");
        let nested = data.join("spill").join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        set(&data, 0o755);
        set(&data.join("spill"), 0o755);
        set(&nested, 0o755);
        let deep = nested.join("out.txt");
        std::fs::write(&deep, b"x").unwrap();
        set(&deep, 0o644);
        std::fs::write(data.join("rsi.db"), b"x").unwrap();
        set(&data.join("rsi.db"), 0o644);

        let top = harden_data_dir_top(&data).unwrap();
        assert!(top.failures.is_empty());
        assert_eq!(mode(&data), 0o700);
        assert_eq!(mode(&data.join("rsi.db")), 0o600);
        assert_eq!(
            mode(&data.join("spill")),
            0o700,
            "the subdirectory is closed"
        );
        assert_eq!(mode(&nested), 0o755, "contents wait for the subtree pass");
        assert_eq!(mode(&deep), 0o644);

        let subtrees = harden_private_subtrees(&data);
        assert!(subtrees.failures.is_empty());
        assert!(subtrees.changed.contains(&deep));
        assert_eq!(mode(&nested), 0o700);
        assert_eq!(mode(&deep), 0o600);
        assert!(
            harden_private_subtrees(&data).changed.is_empty(),
            "second pass is quiet"
        );
    }

    fn fail_for<'a>(names: &'a [&str]) -> impl Fn(&Path, u32) -> io::Result<()> + 'a {
        move |path, mode| {
            if path
                .file_name()
                .is_some_and(|n| names.iter().any(|name| n == *name))
            {
                Err(io::Error::from_raw_os_error(1)) // EPERM
            } else {
                chmod(path, mode)
            }
        }
    }

    #[derive(Clone, Default)]
    struct Capture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl io::Write for Capture {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
        type Writer = Capture;
        fn make_writer(&'a self) -> Capture {
            self.clone()
        }
    }

    #[test]
    fn a_failed_chmod_is_reported_logged_once_and_does_not_stop_the_pass() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join(".rsi");
        std::fs::create_dir_all(data.join("backups").join("nested")).unwrap();
        set(&data, 0o755);
        let stuck_top = data.join("daemon.log");
        let stuck_deep = data.join("backups").join("nested").join("stuck.db");
        let fine = data.join("backups").join("fine.db");
        for path in [&stuck_top, &stuck_deep, &fine] {
            std::fs::write(path, b"x").unwrap();
            set(path, 0o644);
        }

        let capture = Capture::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(capture.clone())
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            let stuck = fail_for(&["daemon.log", "stuck.db"]);
            let top = harden_data_dir_with(&data, &stuck).unwrap();
            // Both stuck paths fail inside one pass; the rest is still tightened.
            assert_eq!(top.failures.len(), 2);
            assert!(top.failures.iter().any(|f| f.path == stuck_deep));
            assert!(top.changed.contains(&fine), "the walk continued");
            let top_failure = top.failures.iter().find(|f| f.path == stuck_top).unwrap();
            assert_eq!(top_failure.error.raw_os_error(), Some(1));
            assert_eq!(mode(&stuck_top), 0o644, "the failing file stays as it was");

            assert_eq!(mode(&fine), 0o600);
            let deep = tighten_tree_report(&data.join("backups"), &stuck);
            assert_eq!(deep.failures.len(), 1);
            assert_eq!(deep.failures[0].path, stuck_deep);

            assert_eq!(top.log_failures(), 2);
            assert_eq!(deep.log_failures(), 0, "each path is logged once");
            assert_eq!(top.log_failures(), 0, "each path is logged once");
        });
        let log = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
        assert!(log.contains(&stuck_top.display().to_string()), "{log}");
        assert!(log.contains(&stuck_deep.display().to_string()), "{log}");
        assert!(log.contains("errno=Some(1)"), "{log}");
    }

    #[test]
    fn restricted_umask_makes_new_files_owner_only_and_restores() {
        use std::os::unix::fs::OpenOptionsExt;
        let temp = tempfile::tempdir().unwrap();
        let previous = restrict_umask();
        let file = temp.path().join("created.log");
        std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .mode(0o666)
            .open(&file)
            .unwrap();
        previous.restore();
        assert_eq!(mode(&file), 0o600);
    }
}
