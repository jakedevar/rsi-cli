//! Authenticated, disk-backed execution scratch beneath a sandbox custody root.
//!
//! This is deliberately a descriptor, not a caller-selectable path.  The
//! provider launch funnel receives it only after a custody ContextRead permit
//! has authenticated the sandbox root.

use crate::error::{DaemonError, Result};
use crate::sandbox::custody::CustodyEffectPermit;
use nix::fcntl::{OFlag, open, openat};
use nix::sys::stat::{Mode, SFlag, fstat, mkdirat};
use std::ffi::CStr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};

const TARGET_NAME: &CStr = c"target";
const TEMP_NAME: &CStr = c".rsi-tmp";
// `mode_t` is `u32` on Linux but `u16` on macOS/BSD; type this from the
// platform's actual `libc::mode_t` alias instead of hardcoding a width so it
// matches both `fchmod`'s parameter type and `stat.st_mode`'s field type on
// every target.
const PRIVATE_DIRECTORY_MODE: nix::libc::mode_t = 0o700;
const TMPFS_MAGIC: i64 = 0x0102_1994;
const RAMFS_MAGIC: i64 = 0x8584_58f6;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxExecutionScratch {
    root: PathBuf,
    target: PathBuf,
    temp: PathBuf,
    root_identity: FileIdentity,
    target_identity: FileIdentity,
    temp_identity: FileIdentity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

impl SandboxExecutionScratch {
    /// Prepare the fixed scratch layout beneath an already authenticated
    /// custody root.  All child creation and inspection is descriptor-relative
    /// and no caller-controlled path enters this API.
    fn prepare(root: &Path) -> Result<Self> {
        let root_fd = open_directory(root, None)?;
        let root_identity = inspect_directory(root_fd.as_raw_fd(), "sandbox root")?;
        reject_memory_filesystem(root_fd.as_raw_fd(), "sandbox root")?;

        ensure_directory(root_fd.as_raw_fd(), TARGET_NAME, "target")?;
        let target_fd = open_directory(Path::new("target"), Some(root_fd.as_raw_fd()))?;
        let target_identity = inspect_directory(target_fd.as_raw_fd(), "target")?;
        require_private_mode(target_fd.as_raw_fd(), "target")?;
        reject_memory_filesystem(target_fd.as_raw_fd(), "target")?;
        require_same_device(root_identity, target_identity, "target")?;

        ensure_directory(target_fd.as_raw_fd(), TEMP_NAME, ".rsi-tmp")?;
        let temp_fd = open_directory(Path::new(".rsi-tmp"), Some(target_fd.as_raw_fd()))?;
        let temp_identity = inspect_directory(temp_fd.as_raw_fd(), ".rsi-tmp")?;
        reject_memory_filesystem(temp_fd.as_raw_fd(), ".rsi-tmp")?;
        require_private_mode(temp_fd.as_raw_fd(), ".rsi-tmp")?;
        require_same_device(root_identity, temp_identity, ".rsi-tmp")?;
        Ok(Self {
            root: root.to_path_buf(),
            target: root.join("target"),
            temp: root.join("target").join(".rsi-tmp"),
            root_identity,
            target_identity,
            temp_identity,
        })
    }

    /// Derive scratch only from a typed, authenticated ContextRead permit.
    /// Ordinary custody has no daemon-selected target and therefore receives
    /// no execution scratch or TMPDIR stamp.
    pub(crate) fn from_context_permit(permit: &CustodyEffectPermit) -> Result<Option<Self>> {
        if !permit.is_context_read() {
            return Err(rejected(
                "execution scratch requires a ContextRead effect permit",
            ));
        }
        let Some(target) = permit.cargo_target_dir() else {
            return Ok(None);
        };
        let expected = permit.effective_cwd().join("target");
        if target != expected {
            return Err(rejected(
                "ContextRead target does not match authenticated custody root",
            ));
        }
        Self::prepare(permit.effective_cwd()).map(Some)
    }

    #[cfg(test)]
    pub(crate) fn prepare_for_test(root: &Path) -> Result<Self> {
        Self::prepare(root)
    }

    pub(crate) fn target(&self) -> &Path {
        &self.target
    }
    pub(crate) fn temp(&self) -> &Path {
        &self.temp
    }

    /// Reopen all fixed names below the pinned root and compare their exact
    /// identities immediately before command construction.  Replacement,
    /// symlinks, device drift, and tmpfs/ramfs all fail closed.
    pub(crate) fn revalidate(&self) -> Result<()> {
        let root_fd = open_directory(&self.root, None)?;
        let root_identity = inspect_directory(root_fd.as_raw_fd(), "sandbox root")?;
        reject_memory_filesystem(root_fd.as_raw_fd(), "sandbox root")?;
        let target_fd = open_directory(Path::new("target"), Some(root_fd.as_raw_fd()))?;
        let target_identity = inspect_directory(target_fd.as_raw_fd(), "target")?;
        require_private_mode(target_fd.as_raw_fd(), "target")?;
        reject_memory_filesystem(target_fd.as_raw_fd(), "target")?;
        let temp_fd = open_directory(Path::new(".rsi-tmp"), Some(target_fd.as_raw_fd()))?;
        let temp_identity = inspect_directory(temp_fd.as_raw_fd(), ".rsi-tmp")?;
        require_private_mode(temp_fd.as_raw_fd(), ".rsi-tmp")?;
        reject_memory_filesystem(temp_fd.as_raw_fd(), ".rsi-tmp")?;
        if root_identity != self.root_identity
            || target_identity != self.target_identity
            || temp_identity != self.temp_identity
        {
            return Err(rejected(
                "execution scratch identity drifted after preparation",
            ));
        }
        require_same_device(root_identity, target_identity, "target")?;
        require_same_device(root_identity, temp_identity, ".rsi-tmp")?;
        Ok(())
    }
}

fn rejected(reason: &str) -> DaemonError {
    DaemonError::ExecutionScratchUnavailable(format!(
        "sandbox execution scratch rejected: {reason}"
    ))
}

fn open_directory(path: &Path, parent: Option<RawFd>) -> Result<OwnedFd> {
    let flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW;
    match parent {
        Some(fd) => openat(Some(fd), path, flags, Mode::empty()),
        None => open(path, flags, Mode::empty()),
    }
    .map(|fd| {
        // SAFETY: `open`/`openat` returned a new owned descriptor and this is
        // its sole conversion into an RAII owner.
        unsafe { OwnedFd::from_raw_fd(fd) }
    })
    .map_err(|error| {
        rejected(&format!(
            "cannot open directory {}: {error}",
            path.display()
        ))
    })
}

fn ensure_directory(parent: RawFd, name: &CStr, label: &str) -> Result<()> {
    match mkdirat(Some(parent), name, Mode::from_bits_truncate(0o700)) {
        Ok(()) | Err(nix::errno::Errno::EEXIST) => {}
        Err(error) => return Err(rejected(&format!("cannot create {label}: {error}"))),
    }
    let fd = openat(
        Some(parent),
        name,
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|error| rejected(&format!("cannot reopen {label}: {error}")))?;
    // SAFETY: `openat` returned a new owned descriptor.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: `fd` is the live no-follow directory descriptor opened above.
    if unsafe { nix::libc::fchmod(fd.as_raw_fd(), PRIVATE_DIRECTORY_MODE) } == 0 {
        Ok(())
    } else {
        Err(rejected(&format!("cannot set {label} mode")))
    }
}

fn require_private_mode(fd: RawFd, label: &str) -> Result<()> {
    let stat = fstat(fd).map_err(|error| rejected(&format!("cannot stat {label}: {error}")))?;
    if (stat.st_mode & 0o7777) != PRIVATE_DIRECTORY_MODE {
        return Err(rejected(&format!("{label} mode is not 0700")));
    }
    Ok(())
}

fn require_same_device(
    root_identity: FileIdentity,
    child_identity: FileIdentity,
    label: &str,
) -> Result<()> {
    if child_identity.device != root_identity.device {
        return Err(rejected(&format!(
            "{label} crosses the sandbox-root device"
        )));
    }
    Ok(())
}

fn inspect_directory(fd: RawFd, label: &str) -> Result<FileIdentity> {
    let stat = fstat(fd).map_err(|error| rejected(&format!("cannot stat {label}: {error}")))?;
    if SFlag::from_bits_truncate(stat.st_mode) != SFlag::S_IFDIR {
        return Err(rejected(&format!("{label} is not a directory")));
    }
    Ok(FileIdentity {
        // `dev_t` is `u64` on Linux but `i32` on macOS/BSD. `as` is a valid
        // conversion for both source types (unlike `From`/`TryFrom`, which
        // would need a per-platform impl); device ids are always small
        // non-negative numbers in practice, so the reinterpretation is safe.
        device: stat.st_dev as u64,
        inode: stat.st_ino,
    })
}

fn reject_memory_filesystem(fd: RawFd, label: &str) -> Result<()> {
    let mut stat: nix::libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: `fd` is a live directory descriptor and `stat` is writable.
    if unsafe { nix::libc::fstatfs(fd, &mut stat) } != 0 {
        return Err(rejected(&format!("cannot inspect filesystem for {label}")));
    }
    let filesystem_type = stat.f_type as i64;
    if filesystem_type == TMPFS_MAGIC || filesystem_type == RAMFS_MAGIC {
        return Err(rejected(&format!("{label} is on tmpfs/ramfs")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::custody::EffectKind;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::fs::symlink;

    /// Per-process open-descriptor directory (`/proc` is Linux-only).
    #[cfg(target_os = "linux")]
    const OPEN_DESCRIPTOR_DIR: &str = "/proc/self/fd";
    #[cfg(not(target_os = "linux"))]
    const OPEN_DESCRIPTOR_DIR: &str = "/dev/fd";

    /// Permission bits as `mode_t` (`u32` on Linux, `u16` on macOS).
    fn permission_bits(path: &Path) -> nix::libc::mode_t {
        nix::sys::stat::stat(path).unwrap().st_mode & 0o7777
    }

    fn disk_backed_fixture() -> tempfile::TempDir {
        let base = std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap().join("target"))
            .join("slice8-execution-scratch-fixtures");
        std::fs::create_dir_all(&base).unwrap();
        tempfile::Builder::new()
            .prefix("scratch-")
            .tempdir_in(base)
            .unwrap()
    }

    #[test]
    fn execution_scratch_prepares_fixed_private_layout() {
        let fixture = disk_backed_fixture();
        let scratch = SandboxExecutionScratch::prepare(fixture.path()).unwrap();
        assert_eq!(scratch.target(), fixture.path().join("target"));
        assert_eq!(scratch.temp(), fixture.path().join("target/.rsi-tmp"));
        assert_eq!(permission_bits(scratch.target()), PRIVATE_DIRECTORY_MODE);
        assert_eq!(permission_bits(scratch.temp()), PRIVATE_DIRECTORY_MODE);
        scratch.revalidate().unwrap();
    }

    #[test]
    fn execution_scratch_accepts_only_context_read_permits_and_preserves_ordinary_none() {
        let fixture = disk_backed_fixture();
        let target = fixture.path().join("target");
        let context = CustodyEffectPermit::for_execution_scratch_test(
            fixture.path().to_path_buf(),
            Some(target),
            EffectKind::ContextRead,
        );
        assert!(
            SandboxExecutionScratch::from_context_permit(&context)
                .unwrap()
                .is_some()
        );

        for kind in [
            EffectKind::ProviderLaunch,
            EffectKind::ProviderTurn,
            EffectKind::ToolExecution,
            EffectKind::BuildCacheReclaim,
        ] {
            let permit = CustodyEffectPermit::for_execution_scratch_test(
                fixture.path().to_path_buf(),
                Some(fixture.path().join("target")),
                kind,
            );
            assert!(matches!(
                SandboxExecutionScratch::from_context_permit(&permit),
                Err(DaemonError::ExecutionScratchUnavailable(_))
            ));
        }

        let ordinary = CustodyEffectPermit::for_execution_scratch_test(
            fixture.path().to_path_buf(),
            None,
            EffectKind::ContextRead,
        );
        assert_eq!(
            SandboxExecutionScratch::from_context_permit(&ordinary).unwrap(),
            None
        );
    }

    #[test]
    fn execution_scratch_rejects_target_and_temp_symlinks_and_replacement() {
        let fixture = disk_backed_fixture();
        let target = fixture.path().join("target");
        symlink(fixture.path(), &target).unwrap();
        assert!(SandboxExecutionScratch::prepare(fixture.path()).is_err());
        std::fs::remove_file(&target).unwrap();
        let scratch = SandboxExecutionScratch::prepare(fixture.path()).unwrap();
        std::fs::remove_dir_all(scratch.target()).unwrap();
        std::fs::create_dir(scratch.target()).unwrap();
        assert!(scratch.revalidate().is_err());

        let temp_fixture = disk_backed_fixture();
        let scratch = SandboxExecutionScratch::prepare(temp_fixture.path()).unwrap();
        std::fs::remove_dir(scratch.temp()).unwrap();
        symlink(temp_fixture.path(), scratch.temp()).unwrap();
        assert!(scratch.revalidate().is_err());
    }

    #[test]
    fn execution_scratch_revalidation_rejects_mode_drift() {
        let fixture = disk_backed_fixture();
        let scratch = SandboxExecutionScratch::prepare(fixture.path()).unwrap();
        std::fs::set_permissions(scratch.target(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(scratch.revalidate().is_err());

        std::fs::set_permissions(scratch.target(), std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(scratch.temp(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(scratch.revalidate().is_err());
    }

    #[test]
    fn repeated_execution_scratch_refusal_keeps_file_descriptors_bounded() {
        let fixture = disk_backed_fixture();
        let scratch = SandboxExecutionScratch::prepare(fixture.path()).unwrap();
        std::fs::set_permissions(scratch.temp(), std::fs::Permissions::from_mode(0o755)).unwrap();
        let descriptor_count = || std::fs::read_dir(OPEN_DESCRIPTOR_DIR).unwrap().count();
        let before = descriptor_count();
        for _ in 0..512 {
            assert!(matches!(
                scratch.revalidate(),
                Err(DaemonError::ExecutionScratchUnavailable(_))
            ));
        }
        let after = descriptor_count();
        assert!(
            // The lib-test process runs unrelated descriptor-using tests in
            // parallel. A fixed allowance absorbs that ambient churn while a
            // per-refusal leak (512 target/temp descriptors) remains decisive.
            after <= before + 64,
            "repeated refusal leaked descriptors: before={before}, after={after}"
        );
    }

    #[test]
    fn repeated_prepare_refusal_after_target_open_keeps_file_descriptors_bounded() {
        let fixture = disk_backed_fixture();
        let target = fixture.path().join("target");
        std::fs::create_dir(&target).unwrap();
        symlink(fixture.path(), target.join(".rsi-tmp")).unwrap();

        let descriptor_count = || std::fs::read_dir(OPEN_DESCRIPTOR_DIR).unwrap().count();
        let before = descriptor_count();
        let attempts = 512;
        let mut refusals = 0;
        for _ in 0..attempts {
            let error = SandboxExecutionScratch::prepare(fixture.path())
                .expect_err("symlinked .rsi-tmp must fail after target opens");
            assert!(
                error.to_string().contains("cannot reopen .rsi-tmp"),
                "refusal must occur after target_fd opens: {error}"
            );
            refusals += 1;
        }
        let after = descriptor_count();
        assert_eq!(refusals, attempts, "prepare refusal loop must be nonzero");
        assert!(
            // The lib-test process may run unrelated descriptor-using tests in
            // parallel. A fixed allowance still makes the original leak of
            // one target descriptor per refusal decisive.
            after <= before + 64,
            "repeated prepare refusal leaked descriptors: refusals={refusals}, before={before}, after={after}"
        );
    }

    #[test]
    fn execution_scratch_rejects_cross_device_identity() {
        let root = FileIdentity {
            device: 1,
            inode: 10,
        };
        let child = FileIdentity {
            device: 2,
            inode: 11,
        };
        assert!(require_same_device(root, child, "target").is_err());
    }

    #[test]
    fn execution_scratch_rejects_tmpfs_root_when_available() {
        let tmpfs = Path::new("/dev/shm");
        if tmpfs.is_dir() {
            assert!(SandboxExecutionScratch::prepare(tmpfs).is_err());
        }
    }
}
