//! Handle-relative input-asset I/O. Canonicalization chooses an allowed path;
//! these handles prevent a subsequent symlink swap from redirecting the I/O.
//! Unix uses openat with no-follow on every component (including ancestors).
//! Other platforms explicitly refuse until an equivalent implementation exists.

use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::path::Path;

#[cfg(unix)]
use rustix::fs::{AtFlags, Mode, OFlags, openat};

/// Deepest directory nesting [`AssetDir::remove_tree`] removes.
pub const MAX_REMOVE_DEPTH: usize = 128;
#[cfg(unix)]
const REMOVE_BATCH: usize = 1024;

/// A pinned directory. All child operations reject symlinks and traversal.
pub struct AssetDir {
    #[cfg(unix)]
    file: File,
}

fn invalid_path() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "asset path must stay below its directory",
    )
}

#[cfg(not(unix))]
fn unsupported() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "safe input-asset I/O is unsupported on this platform",
    )
}

impl AssetDir {
    /// Transfer this pinned directory to another handle-based filesystem API.
    #[cfg(unix)]
    pub fn into_std_file(self) -> File {
        self.file
    }

    /// Open an absolute, already canonical directory, without following any
    /// replacement symlink in it or its ancestors.
    pub fn open(path: &Path) -> io::Result<Self> {
        #[cfg(unix)]
        {
            if !path.is_absolute() {
                return Err(invalid_path());
            }
            let root = Self {
                file: File::open("/")?,
            };
            root.open_dir(path.strip_prefix("/").map_err(|_| invalid_path())?)
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            Err(unsupported())
        }
    }

    /// Clone the pinned descriptor for APIs that consume a standard file
    /// (for example capability metadata or a child's handle-relative cwd).
    /// The returned handle must not be converted back into an ambient path.
    pub fn try_clone_file(&self) -> io::Result<File> {
        #[cfg(unix)]
        {
            self.file.try_clone()
        }
        #[cfg(not(unix))]
        {
            Err(unsupported())
        }
    }

    /// Resolve a legitimate home/data root, creating missing directories via
    /// pinned parents. Callers must create the untrusted paste/upload children
    /// separately, so existing links in those children are always refused.
    pub fn create_data_root(path: &Path) -> io::Result<Self> {
        match path.canonicalize() {
            Ok(path) => Self::open(&path),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let parent = path.parent().ok_or_else(invalid_path)?;
                let name = path.file_name().ok_or_else(invalid_path)?;
                Self::create_data_root(parent)?.create_dir(Path::new(name))
            }
            Err(e) => Err(e),
        }
    }

    pub fn open_dir(&self, relative: &Path) -> io::Result<Self> {
        #[cfg(unix)]
        {
            let mut file = self.file.try_clone()?;
            for component in relative.components() {
                let std::path::Component::Normal(name) = component else {
                    return Err(invalid_path());
                };
                file = openat(
                    &file,
                    name,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )?
                .into();
            }
            Ok(Self { file })
        }
        #[cfg(not(unix))]
        {
            let _ = relative;
            Err(unsupported())
        }
    }

    fn parent(&self, relative: &Path) -> io::Result<(Self, OsString)> {
        let name = relative.file_name().ok_or_else(invalid_path)?;
        // Reject terminal `..` / `.` as well as non-normal ancestors.
        if relative
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return Err(invalid_path());
        }
        Ok((
            self.open_dir(relative.parent().ok_or_else(invalid_path)?)?,
            name.to_owned(),
        ))
    }

    /// Open a regular file only. NONBLOCK prevents a replaced FIFO from hanging
    /// before we can inspect the opened handle's metadata.
    pub fn read_file(&self, relative: &Path) -> io::Result<File> {
        let (parent, name) = self.parent(relative)?;
        #[cfg(unix)]
        {
            let file: File = openat(
                &parent.file,
                &name,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            )?
            .into();
            if !file.metadata()?.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "asset is not a regular file",
                ));
            }
            Ok(file)
        }
        #[cfg(not(unix))]
        {
            let _ = (parent, name);
            Err(unsupported())
        }
    }

    /// Bounded names from the pinned directory, including one extra entry so
    /// callers can report truncation. Paths are never reopened for enumeration.
    pub fn entries(&self, limit: usize) -> io::Result<Vec<OsString>> {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt as _;
            let mut names = Vec::new();
            for entry in rustix::fs::Dir::read_from(&self.file)? {
                let entry = entry?;
                let name = entry.file_name().to_bytes();
                if name == b"." || name == b".." {
                    continue;
                }
                names.push(std::ffi::OsStr::from_bytes(name).to_owned());
                if names.len() > limit {
                    break;
                }
            }
            Ok(names)
        }
        #[cfg(not(unix))]
        {
            let _ = limit;
            Err(unsupported())
        }
    }

    pub fn create_dir(&self, name: &Path) -> io::Result<Self> {
        let (parent, name) = self.parent(name)?;
        #[cfg(unix)]
        {
            match rustix::fs::mkdirat(&parent.file, &name, Mode::RUSR | Mode::WUSR | Mode::XUSR) {
                Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                Err(e) => return Err(e.into()),
            }
            parent.open_dir(Path::new(&name))
        }
        #[cfg(not(unix))]
        {
            let _ = (parent, name);
            Err(unsupported())
        }
    }

    pub fn create_file(&self, name: &Path) -> io::Result<File> {
        let (parent, name) = self.parent(name)?;
        #[cfg(unix)]
        {
            Ok(openat(
                &parent.file,
                &name,
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::RUSR | Mode::WUSR,
            )?
            .into())
        }
        #[cfg(not(unix))]
        {
            let _ = (parent, name);
            Err(unsupported())
        }
    }

    pub fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        let (src, from) = self.parent(from)?;
        let (dst, to) = self.parent(to)?;
        #[cfg(unix)]
        {
            Ok(rustix::fs::renameat(&src.file, &from, &dst.file, &to)?)
        }
        #[cfg(not(unix))]
        {
            let _ = (src, from, dst, to);
            Err(unsupported())
        }
    }

    /// Move `from` below this directory to `to` below `dest`, another pinned
    /// directory, with one `renameat` (the entry itself is moved, never
    /// followed). Different filesystems fail `CrossesDevices`; nothing moves.
    pub fn rename_into(&self, from: &Path, dest: &AssetDir, to: &Path) -> io::Result<()> {
        let (src, from) = self.parent(from)?;
        let (dst, to) = dest.parent(to)?;
        #[cfg(unix)]
        {
            Ok(rustix::fs::renameat(&src.file, &from, &dst.file, &to)?)
        }
        #[cfg(not(unix))]
        {
            let _ = (src, from, dst, to);
            Err(unsupported())
        }
    }

    /// What `name` is, without following a final link: `None` when missing,
    /// `Some(true)` for a real directory, `Some(false)` for anything else (a
    /// symlink, a regular file, a FIFO, a socket, a device).
    pub fn entry_is_dir(&self, name: &Path) -> io::Result<Option<bool>> {
        let (parent, name) = self.parent(name)?;
        #[cfg(unix)]
        {
            match rustix::fs::statat(&parent.file, &name, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(stat) => Ok(Some(
                    rustix::fs::FileType::from_raw_mode(stat.st_mode as _)
                        == rustix::fs::FileType::Directory,
                )),
                Err(rustix::io::Errno::NOENT) => Ok(None),
                Err(e) => Err(e.into()),
            }
        }
        #[cfg(not(unix))]
        {
            let _ = (parent, name);
            Err(unsupported())
        }
    }

    /// Remove the real directory `name` and everything below it, like
    /// `std::fs::remove_dir_all`, through pinned handles: no link is ever
    /// followed (a link inside is unlinked, never its target), and `name`
    /// itself must be a directory, not a link (`ELOOP`/`ENOTDIR` otherwise).
    /// Deeper than [`MAX_REMOVE_DEPTH`] levels is refused.
    pub fn remove_tree(&self, name: &Path) -> io::Result<()> {
        let (parent, name) = self.parent(name)?;
        #[cfg(unix)]
        {
            let dir = parent.open_dir(Path::new(&name))?;
            dir.clear(0)?;
            Ok(rustix::fs::unlinkat(
                &parent.file,
                &name,
                AtFlags::REMOVEDIR,
            )?)
        }
        #[cfg(not(unix))]
        {
            let _ = (parent, name);
            Err(unsupported())
        }
    }

    /// Empty this directory without following links (see [`Self::remove_tree`]).
    #[cfg(unix)]
    fn clear(&self, depth: usize) -> io::Result<()> {
        if depth >= MAX_REMOVE_DEPTH {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "directory tree is too deep to remove",
            ));
        }
        loop {
            // Each listing reopens the directory, so a pass sees what is left.
            let names = self.entries(REMOVE_BATCH)?;
            if names.is_empty() {
                return Ok(());
            }
            for name in names {
                match openat(
                    &self.file,
                    &name,
                    OFlags::RDONLY
                        | OFlags::DIRECTORY
                        | OFlags::NOFOLLOW
                        | OFlags::NONBLOCK
                        | OFlags::CLOEXEC,
                    Mode::empty(),
                ) {
                    Ok(fd) => {
                        Self { file: fd.into() }.clear(depth + 1)?;
                        rustix::fs::unlinkat(&self.file, &name, AtFlags::REMOVEDIR)?;
                    }
                    Err(rustix::io::Errno::NOTDIR | rustix::io::Errno::LOOP) => {
                        rustix::fs::unlinkat(&self.file, &name, AtFlags::empty())?;
                    }
                    Err(e) => return Err(e.into()),
                }
            }
        }
    }

    pub fn remove_file(&self, name: &Path) -> io::Result<()> {
        let (parent, name) = self.parent(name)?;
        #[cfg(unix)]
        {
            Ok(rustix::fs::unlinkat(&parent.file, &name, AtFlags::empty())?)
        }
        #[cfg(not(unix))]
        {
            let _ = (parent, name);
            Err(unsupported())
        }
    }
}
