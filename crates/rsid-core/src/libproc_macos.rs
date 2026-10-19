//! macOS libproc primitives shared by the daemon's process scans (#89 build-cache
//! reclaim, #102 archived-sandbox holder proof): the pid list, `proc_pidinfo`
//! records, process credentials, the descriptor table, the vnode of one
//! descriptor and the vnode of each memory region.
//!
//! Each reader reports the raw `errno` and leaves the policy (a vanished
//! process is unrelated, an unreadable one fails closed) to its caller.

use nix::libc;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// `PROC_PIDFDVNODEPATHINFO` from `<sys/proc_info.h>` (not exported by libc).
const PROC_PIDFDVNODEPATHINFO: libc::c_int = 2;

/// Most descriptors one process's table is read for (8 bytes each).
const MAX_LISTED_FDS: usize = 1 << 20;

/// Most pids one listing is grown to hold before it is refused.
const MAX_LISTED_PIDS: usize = 1 << 20;

/// `PROC_PIDREGIONPATHINFO` from `<sys/proc_info.h>` (not exported by libc).
const PROC_PIDREGIONPATHINFO: libc::c_int = 8;

/// `CS_OPS_STATUS` and `CS_PLATFORM_BINARY` from `<sys/codesign.h>`.
const CS_OPS_STATUS: u32 = 0;
const CS_PLATFORM_BINARY: u32 = 0x0400_0000;

unsafe extern "C" {
    /// `int csops(pid_t, unsigned int, void *, size_t)` from libSystem.
    fn csops(
        pid: libc::pid_t,
        ops: u32,
        useraddr: *mut libc::c_void,
        usersize: libc::size_t,
    ) -> libc::c_int;
}

/// `SZOMB` from `<sys/proc.h>`.
const SZOMB: u8 = 5;

mod sealed {
    pub trait Sealed {}
}

/// A record `proc_pidinfo` fills for one flavor.
///
/// # Safety
/// Implementors are `#[repr(C)]` plain data for which every bit pattern is a
/// valid value and whose size is exactly what the kernel writes for
/// `FLAVOR`. The trait is sealed: only this module implements it, so
/// [`pid_info`] cannot be instantiated with a type that has a `Drop`, a
/// niche or a pointer to own.
pub unsafe trait ProcInfoRecord: sealed::Sealed + Sized {
    /// The `PROC_PID*` flavor that fills this record.
    const FLAVOR: libc::c_int;
}

impl sealed::Sealed for libc::proc_bsdinfo {}
// SAFETY: libc's `proc_bsdinfo` is the plain-data `struct proc_bsdinfo`.
unsafe impl ProcInfoRecord for libc::proc_bsdinfo {
    const FLAVOR: libc::c_int = libc::PROC_PIDTBSDINFO;
}

impl sealed::Sealed for libc::proc_vnodepathinfo {}
// SAFETY: libc's `proc_vnodepathinfo` is the plain-data struct of that name.
unsafe impl ProcInfoRecord for libc::proc_vnodepathinfo {
    const FLAVOR: libc::c_int = libc::PROC_PIDVNODEPATHINFO;
}

/// `struct proc_regioninfo` from `<sys/proc_info.h>`.
#[repr(C)]
struct ProcRegionInfo {
    protection: u32,
    max_protection: u32,
    inheritance: u32,
    flags: u32,
    offset: u64,
    // behavior .. depth: fourteen u32 counters this module does not read.
    counters: [u32; 14],
    address: u64,
    size: u64,
}

/// `struct proc_regionwithpathinfo` from `<sys/proc_info.h>`.
#[repr(C)]
struct ProcRegionWithPathInfo {
    region: ProcRegionInfo,
    vnode: libc::vnode_info_path,
}

impl sealed::Sealed for ProcRegionWithPathInfo {}
// SAFETY: plain data mirroring `struct proc_regionwithpathinfo`; its size is
// pinned against the kernel by `region_record_matches_the_kernel_size`.
unsafe impl ProcInfoRecord for ProcRegionWithPathInfo {
    const FLAVOR: libc::c_int = PROC_PIDREGIONPATHINFO;
}

/// `struct proc_fileinfo` from `<sys/proc_info.h>`.
#[repr(C)]
struct ProcFileInfo {
    fi_openflags: u32,
    fi_status: u32,
    fi_offset: libc::off_t,
    fi_type: i32,
    fi_guardflags: u32,
}

/// `struct vnode_fdinfowithpath` from `<sys/proc_info.h>`.
#[repr(C)]
struct VnodeFdInfoWithPath {
    pfi: ProcFileInfo,
    pvip: libc::vnode_info_path,
}

/// A vnode a process refers to: its resolved path and its `st_dev`/`st_ino`
/// (the encoding `std::fs::Metadata::dev`/`ino` report).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VnodeRef {
    pub path: PathBuf,
    pub device: u64,
    pub inode: u64,
}

impl VnodeRef {
    fn from_info(info: &libc::vnode_info_path) -> Self {
        Self {
            path: c_path(&info.vip_path),
            // std widens the signed `dev_t` the same way.
            device: i64::from(i32::from_ne_bytes(
                info.vip_vi.vi_stat.vst_dev.to_ne_bytes(),
            )) as u64,
            inode: info.vip_vi.vi_stat.vst_ino,
        }
    }
}

/// A path as given and as resolved: libproc reports resolved paths
/// (`/private/var/...` for `/var/...`), the caller passes the pinned root.
pub struct Prefixes(Vec<PathBuf>);

impl Prefixes {
    #[must_use]
    pub fn new(path: &Path) -> Self {
        let mut prefixes = vec![path.to_path_buf()];
        if let Ok(resolved) = std::fs::canonicalize(path)
            && resolved != path
        {
            prefixes.push(resolved);
        }
        Self(prefixes)
    }

    #[must_use]
    pub fn contains(&self, path: &Path) -> bool {
        self.0.iter().any(|prefix| path.starts_with(prefix))
    }
}

#[must_use]
pub fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// The process (or its descriptor) is gone: `ESRCH`.
#[must_use]
pub const fn exited(code: i32) -> bool {
    code == libc::ESRCH
}

/// Every pid on the host, or `None` when the list cannot be read or may be
/// truncated. A second call that fills its buffer completely may have been cut
/// short, so the buffer is grown and the call repeated, up to a bound.
#[must_use]
pub fn list_pids() -> Option<Vec<libc::pid_t>> {
    // SAFETY: a null buffer asks only for the current count.
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if count <= 0 {
        return None;
    }
    collect_pids(count as usize, |pids| {
        let Ok(bytes) = libc::c_int::try_from(std::mem::size_of_val(pids)) else {
            return -1;
        };
        // SAFETY: the buffer holds `bytes` bytes of pid_t slots.
        unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) }
    })
}

/// The retry policy of [`list_pids`]: `read` fills the slice and returns how
/// many pids it wrote (`<= 0` on failure).
fn collect_pids(
    count: usize,
    mut read: impl FnMut(&mut [libc::pid_t]) -> libc::c_int,
) -> Option<Vec<libc::pid_t>> {
    // Room for processes started between the two calls.
    let mut capacity = count.saturating_add(64).min(MAX_LISTED_PIDS);
    loop {
        let mut pids = vec![0 as libc::pid_t; capacity];
        let filled = usize::try_from(read(&mut pids)).ok().filter(|n| *n > 0)?;
        if filled < capacity {
            pids.truncate(filled);
            return Some(pids);
        }
        // Full: more pids may exist than were returned.
        if capacity >= MAX_LISTED_PIDS {
            return None;
        }
        capacity = capacity.saturating_mul(2).min(MAX_LISTED_PIDS);
    }
}

/// `proc_pidinfo` into one record `T` for its own flavor.
///
/// # Errors
/// The `errno` of the failed call, or `EIO` for a short record.
pub fn pid_info<T: ProcInfoRecord>(pid: libc::pid_t) -> Result<T, i32> {
    pid_info_at(pid, 0)
}

fn pid_info_at<T: ProcInfoRecord>(pid: libc::pid_t, argument: u64) -> Result<T, i32> {
    let mut value = std::mem::MaybeUninit::<T>::zeroed();
    let size = std::mem::size_of::<T>() as libc::c_int;
    // SAFETY: the buffer is a zeroed `T` of `size` bytes.
    let got =
        unsafe { libc::proc_pidinfo(pid, T::FLAVOR, argument, value.as_mut_ptr().cast(), size) };
    if got != size {
        return Err(if got <= 0 { errno() } else { libc::EIO });
    }
    // SAFETY: the kernel filled all `size` bytes and `ProcInfoRecord`
    // guarantees every bit pattern is a valid `T`.
    Ok(unsafe { value.assume_init() })
}

/// The credentials of a process and whether it is a zombie.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Credentials {
    /// Effective uid.
    pub uid: u32,
    /// Real uid.
    pub ruid: u32,
    pub zombie: bool,
}

/// `sizeof(struct kinfo_proc)` on arm64 and x86_64 macOS, and the offsets this
/// module reads from it (`p_stat`, `p_pid`, `e_pcred.p_ruid`,
/// `e_ucred.cr_uid`), verified against `proc_bsdinfo` by
/// `kinfo_credentials_agree_with_bsdinfo_for_every_readable_process`.
const KINFO_PROC_SIZE: usize = 648;
const KINFO_STAT_OFFSET: usize = 36;
const KINFO_PID_OFFSET: usize = 40;
const KINFO_RUID_OFFSET: usize = 392;
const KINFO_UID_OFFSET: usize = 420;

fn kinfo_word(record: &[u8], offset: usize) -> u32 {
    u32::from_ne_bytes([
        record[offset],
        record[offset + 1],
        record[offset + 2],
        record[offset + 3],
    ])
}

/// Credentials from `sysctl(KERN_PROC_PID)`, readable for a process
/// `proc_pidinfo` withholds from this caller.
fn kinfo_credentials(pid: libc::pid_t) -> Result<Credentials, i32> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_PID, pid];
    let mut record = vec![0u8; KINFO_PROC_SIZE];
    let mut size: libc::size_t = KINFO_PROC_SIZE;
    // SAFETY: `record` holds `size` bytes and `size` says so.
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            4,
            record.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return Err(errno());
    }
    if size == 0 {
        // No such process any more.
        return Err(libc::ESRCH);
    }
    if size != KINFO_PROC_SIZE || kinfo_word(&record, KINFO_PID_OFFSET) != pid as u32 {
        return Err(libc::EIO);
    }
    Ok(Credentials {
        uid: kinfo_word(&record, KINFO_UID_OFFSET),
        ruid: kinfo_word(&record, KINFO_RUID_OFFSET),
        zombie: record[KINFO_STAT_OFFSET] == SZOMB,
    })
}

/// Who `pid` runs as, observed rather than assumed: `proc_pidinfo` first, and
/// when the kernel withholds that (`EPERM`, a process of another user) the
/// process table through `sysctl`, which names every process's credentials.
///
/// # Errors
/// The `errno` of a failed read (`ESRCH` exited); never a guess about the owner.
pub fn credentials(pid: libc::pid_t) -> Result<Credentials, i32> {
    match pid_info::<libc::proc_bsdinfo>(pid) {
        Ok(info) => Ok(Credentials {
            uid: info.pbi_uid,
            ruid: info.pbi_ruid,
            zombie: info.pbi_status == u32::from(SZOMB),
        }),
        Err(code) if code == libc::EPERM => kinfo_credentials(pid),
        Err(code) => Err(code),
    }
}

fn c_path(raw: &[[libc::c_char; 32]; 32]) -> PathBuf {
    // SAFETY: the 1024 bytes are contiguous `c_char`s.
    let bytes: &[u8] = unsafe { std::slice::from_raw_parts(raw.as_ptr().cast::<u8>(), 32 * 32) };
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    PathBuf::from(OsStr::from_bytes(&bytes[..end]))
}

/// The working directory of `pid`.
///
/// # Errors
/// The `errno` of `proc_pidinfo` (`ESRCH` exited, `EPERM` protected).
pub fn working_directory(pid: libc::pid_t) -> Result<VnodeRef, i32> {
    let paths = pid_info::<libc::proc_vnodepathinfo>(pid)?;
    Ok(VnodeRef::from_info(&paths.pvi_cdir))
}

/// The executable path of `pid`. `Ok(None)` when it has none to give: the
/// kernel confirms the process exited (`ESRCH`), or its executable was unlinked
/// while it ran (`ENOENT`, as for a descriptor in [`fd_vnode`]; an unlinked file
/// is no path under any tree, and its text mapping still appears, by vnode
/// identity, in [`region_at`]). `Err(errno)` for every other failure, `EPERM`
/// included: the caller decides whether a protected process is unrelated (the
/// reclaim scan) or an unreadable inventory (the holder proof), and never
/// carries on silently without executable-path evidence.
///
/// # Errors
/// The `errno` of an unexplained `proc_pidpath` failure.
pub fn executable(pid: libc::pid_t) -> Result<Option<PathBuf>, i32> {
    let mut buffer = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: `__error` returns this thread's errno slot; it is cleared so a
    // stale value cannot be read as this call's failure.
    unsafe { *libc::__error() = 0 };
    // SAFETY: the buffer holds PROC_PIDPATHINFO_MAXSIZE bytes.
    let got = unsafe {
        libc::proc_pidpath(
            pid,
            buffer.as_mut_ptr().cast(),
            libc::PROC_PIDPATHINFO_MAXSIZE as u32,
        )
    };
    classify_pidpath(got, errno(), buffer)
}

/// The `proc_pidpath` outcome policy: `got` is its return value, `code` the
/// `errno` it left, `buffer` the bytes it wrote.
fn classify_pidpath(
    got: libc::c_int,
    code: i32,
    mut buffer: Vec<u8>,
) -> Result<Option<PathBuf>, i32> {
    if got > 0 {
        buffer.truncate(got as usize);
        return Ok(Some(PathBuf::from(OsStr::from_bytes(&buffer))));
    }
    if exited(code) || code == libc::ENOENT {
        return Ok(None);
    }
    // A zero return with no errno is still no path: not evidence of absence.
    Err(if code == 0 { libc::EIO } else { code })
}

/// The descriptor table of `pid`, or `Err(errno)`. The size query with a null
/// buffer is not used: it fails `ENOMEM` for a process that raised its
/// descriptor limit, while a real buffer reads the same table. The buffer
/// grows until the kernel returns less than it can hold; a table still full at
/// the cap fails closed.
///
/// # Errors
/// The `errno` of the failed read, or `ENOMEM` for a table over the cap.
pub fn list_fds(pid: libc::pid_t) -> Result<Vec<libc::proc_fdinfo>, i32> {
    let entry = std::mem::size_of::<libc::proc_fdinfo>();
    let mut capacity = 256;
    loop {
        let mut fds: Vec<libc::proc_fdinfo> = Vec::with_capacity(capacity);
        let bytes = libc::c_int::try_from(capacity * entry).map_err(|_| libc::ENOMEM)?;
        // SAFETY: `__error` returns this thread's errno slot; it is cleared so
        // a zero return can tell an empty table from a failure.
        unsafe { *libc::__error() = 0 };
        // SAFETY: the buffer has capacity for `bytes` bytes of proc_fdinfo.
        let got = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDLISTFDS,
                0,
                fds.as_mut_ptr().cast(),
                bytes,
            )
        };
        if got < 0 || (got == 0 && errno() != 0) {
            return Err(errno());
        }
        let got = got as usize;
        if got < capacity * entry {
            // SAFETY: the kernel wrote `got` bytes of whole proc_fdinfo records.
            unsafe { fds.set_len(got / entry) };
            return Ok(fds);
        }
        if capacity >= MAX_LISTED_FDS {
            return Err(libc::ENOMEM);
        }
        capacity *= 4;
    }
}

/// Whether a descriptor is a vnode (a file or directory) rather than a socket,
/// pipe, kqueue, or other kernel object.
#[must_use]
pub const fn is_vnode(fd: &libc::proc_fdinfo) -> bool {
    fd.proc_fdtype == libc::PROX_FDTYPE_VNODE as u32
}

/// The vnode behind descriptor `fd` of `pid`. `Ok(None)` only when the
/// descriptor cannot name a path any more: it closed meanwhile (`EBADF`) or its
/// vnode has no path (`ENOENT`, an unlinked file, which is no longer an entry
/// of any tree). `Err(errno)` for every other failure, `ESRCH` and a withheld
/// descriptor (`EPERM`) included, so the caller decides.
///
/// # Errors
/// The `errno` of an unexpected read failure.
pub fn fd_vnode(pid: libc::pid_t, fd: i32) -> Result<Option<VnodeRef>, i32> {
    let mut info = std::mem::MaybeUninit::<VnodeFdInfoWithPath>::zeroed();
    let size = std::mem::size_of::<VnodeFdInfoWithPath>() as libc::c_int;
    // SAFETY: the buffer is a zeroed record of `size` bytes.
    let got = unsafe {
        libc::proc_pidfdinfo(
            pid,
            fd,
            PROC_PIDFDVNODEPATHINFO,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if got != size {
        let code = errno();
        if code == libc::EBADF || code == libc::ENOENT {
            return Ok(None);
        }
        return Err(code);
    }
    // SAFETY: the kernel filled the whole record; every field is plain data.
    let info = unsafe { info.assume_init() };
    Ok(Some(VnodeRef::from_info(&info.pvip)))
}

/// Whether the kernel marks `pid` a platform binary: code signed by Apple and
/// shipped with the OS (`CS_PLATFORM_BINARY`, read with `csops`). Such system
/// daemons run as the user yet withhold their descriptor paths (`EPERM`).
///
/// # Errors
/// The `errno` of the failed read (`ESRCH` exited).
pub fn is_platform_binary(pid: libc::pid_t) -> Result<bool, i32> {
    let mut flags = 0_u32;
    // SAFETY: `flags` is the 4 bytes `CS_OPS_STATUS` writes.
    let rc = unsafe {
        csops(
            pid,
            CS_OPS_STATUS,
            std::ptr::from_mut(&mut flags).cast(),
            std::mem::size_of::<u32>(),
        )
    };
    if rc != 0 {
        return Err(errno());
    }
    Ok(flags & CS_PLATFORM_BINARY != 0)
}

/// One memory region of a process: its range and, when a file backs it, that
/// file's vnode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Region {
    pub address: u64,
    pub size: u64,
    pub vnode: Option<VnodeRef>,
}

/// The region of `pid` at or after `address`; `Ok(None)` after the last one
/// (the kernel answers `EINVAL` once no region lies at or above the address).
/// A region mapping a file keeps naming it after the descriptor that opened it
/// is closed, so this is the inventory of mapped files.
///
/// # Errors
/// The `errno` of any other failure (`ESRCH` exited, `EPERM` withheld).
pub fn region_at(pid: libc::pid_t, address: u64) -> Result<Option<Region>, i32> {
    match pid_info_at::<ProcRegionWithPathInfo>(pid, address) {
        Ok(record) => {
            let vnode = VnodeRef::from_info(&record.vnode);
            let backed = !vnode.path.as_os_str().is_empty() || vnode.inode != 0;
            Ok(Some(Region {
                address: record.region.address,
                size: record.region.size,
                vnode: backed.then_some(vnode),
            }))
        }
        Err(code) if code == libc::EINVAL => Ok(None),
        Err(code) => Err(code),
    }
}

/// One incarnation of a process: its effective uid, when it started and
/// whether it is a zombie. With the pid, the start time names exactly one
/// process; a later process given the same pid starts later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessStart {
    /// Effective uid.
    pub uid: u32,
    /// Microseconds since the epoch (`pbi_start_tvsec`, `pbi_start_tvusec`).
    pub started_micros: u64,
    pub zombie: bool,
}

/// The [`ProcessStart`] of `pid`, from `proc_bsdinfo`.
///
/// # Errors
/// The `errno` of `proc_pidinfo` (`ESRCH` exited, `EPERM` withheld).
pub fn process_start(pid: libc::pid_t) -> Result<ProcessStart, i32> {
    let info = pid_info::<libc::proc_bsdinfo>(pid)?;
    Ok(ProcessStart {
        uid: info.pbi_uid,
        started_micros: info
            .pbi_start_tvsec
            .saturating_mul(1_000_000)
            .saturating_add(info.pbi_start_tvusec),
        zombie: info.pbi_status == u32::from(SZOMB),
    })
}

/// The raw `KERN_PROCARGS2` record of `pid`: `argc`, the exec path, its NUL
/// padding, `argv`, then the environment, all NUL-separated. A record larger
/// than `max_bytes` is refused before it is read.
///
/// # Errors
/// The `errno` of `sysctl` (`EINVAL` for a process that withholds its
/// arguments or is exiting, `ESRCH` exited), or `E2BIG` over `max_bytes`.
pub fn process_arguments(pid: libc::pid_t, max_bytes: usize) -> Result<Vec<u8>, i32> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut size: libc::size_t = 0;
    // SAFETY: a null buffer asks only for the size.
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return Err(errno());
    }
    if size > max_bytes {
        return Err(libc::E2BIG);
    }
    let mut buffer = vec![0u8; size];
    // SAFETY: the buffer holds `size` bytes and `size` says so.
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buffer.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return Err(errno());
    }
    buffer.truncate(size);
    Ok(buffer)
}

/// The environment of a [`process_arguments`] record laid out as Linux's
/// `/proc/<pid>/environ`: every `NAME=value`, each followed by one NUL. It
/// skips the exec path, its padding and exactly `argc` arguments, then reads
/// variables up to the first empty string (the kernel's own strings follow).
/// `None` for a record too short to hold its `argc` arguments.
///
/// Limit: an empty `argv[0]` cannot be told from the padding, so the first
/// variable would be taken for the last argument. Every RSI launch names its
/// program, so its stamps are not lost this way.
#[must_use]
pub fn procargs_environment(record: &[u8]) -> Option<Vec<u8>> {
    let argc = usize::try_from(i32::from_ne_bytes(record.get(..4)?.try_into().ok()?)).ok()?;
    let rest = &record[4..];
    let path_end = rest.iter().position(|byte| *byte == 0)?;
    let rest = &rest[path_end..];
    let Some(first) = rest.iter().position(|byte| *byte != 0) else {
        return (argc == 0).then(Vec::new);
    };
    let mut rest = &rest[first..];
    for _ in 0..argc {
        let end = rest.iter().position(|byte| *byte == 0)?;
        rest = &rest[end + 1..];
    }
    let mut environment = Vec::new();
    for variable in rest.split(|byte| *byte == 0) {
        if variable.is_empty() {
            break;
        }
        environment.extend_from_slice(variable);
        environment.push(0);
    }
    Some(environment)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pidpath_names_no_path_only_for_an_exit_or_an_unlinked_executable() {
        let path = b"/opt/tool".to_vec();
        assert_eq!(
            classify_pidpath(9, 0, path.clone()),
            Ok(Some(PathBuf::from("/opt/tool")))
        );
        // A confirmed exit and an unlinked executable name no path.
        assert_eq!(classify_pidpath(0, libc::ESRCH, Vec::new()), Ok(None));
        assert_eq!(classify_pidpath(0, libc::ENOENT, Vec::new()), Ok(None));
        // A protected process (EPERM) is the caller's call; every other errno
        // is unreadable, and so is a silent zero return.
        for code in [
            libc::EPERM,
            libc::EACCES,
            libc::ENOMEM,
            libc::EINVAL,
            libc::EIO,
        ] {
            assert_eq!(classify_pidpath(0, code, Vec::new()), Err(code));
        }
        assert_eq!(classify_pidpath(-1, 0, Vec::new()), Err(libc::EIO));
    }

    #[test]
    fn procargs_environment_skips_the_path_padding_and_exactly_argc_arguments() {
        let mut record = 3i32.to_ne_bytes().to_vec();
        record.extend_from_slice(b"/bin/sleep\0\0\0\0");
        // argv, with an empty argument and one that looks like a variable.
        record.extend_from_slice(b"sleep\0\0RSI_SESSION_ID=argv\0");
        record.extend_from_slice(b"HOME=/h\0RSI_SESSION_ID=env\0\0");
        record.extend_from_slice(b"executable_path=/bin/sleep\0\0");
        assert_eq!(
            procargs_environment(&record),
            Some(b"HOME=/h\0RSI_SESSION_ID=env\0".to_vec())
        );
        // No arguments and no environment.
        let mut empty = 0i32.to_ne_bytes().to_vec();
        empty.extend_from_slice(b"/bin/true\0\0\0");
        assert_eq!(procargs_environment(&empty), Some(Vec::new()));
        // Too short to hold its arguments, or no argc at all.
        let mut short = 2i32.to_ne_bytes().to_vec();
        short.extend_from_slice(b"/bin/sleep\0\0sleep\0");
        assert_eq!(procargs_environment(&short), None);
        assert_eq!(procargs_environment(&[1, 0]), None);
    }

    #[test]
    fn a_child_environment_and_start_are_read_and_bounded() {
        // The kernel omits the environment of a platform or hardened binary
        // (`/bin/sleep` shows only its arguments), so the probe is this
        // ad-hoc-signed test binary, parked in a helper that sleeps.
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "libproc_macos::tests::unlinked_executable_child_helper",
                "--test-threads=1",
            ])
            .env("RSI_TEST_UNLINKED_EXE_CHILD", "1")
            .env("RSI_LIBPROC_PROBE", "present")
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("spawn probe child");
        let pid = child.id() as libc::pid_t;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let environment = loop {
            let record = process_arguments(pid, 1 << 20).expect("child arguments");
            let environment = procargs_environment(&record).expect("well-formed record");
            if environment
                .split(|byte| *byte == 0)
                .any(|variable| variable == b"RSI_LIBPROC_PROBE=present")
                || std::time::Instant::now() >= deadline
            {
                break environment;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        assert!(
            environment
                .split(|byte| *byte == 0)
                .any(|variable| variable == b"RSI_LIBPROC_PROBE=present")
        );
        assert_eq!(process_arguments(pid, 4), Err(libc::E2BIG));
        let start = process_start(pid).expect("child start");
        assert_eq!(start.uid, nix::unistd::geteuid().as_raw());
        assert!(!start.zombie);
        assert!(start.started_micros > 0);
        assert_eq!(process_start(pid), Ok(start));
        child.kill().expect("kill probe child");
        child.wait().expect("reap probe child");
        assert_eq!(process_start(pid), Err(libc::ESRCH));
    }

    #[test]
    fn this_process_executable_is_readable() {
        let own = std::process::id() as libc::pid_t;
        let exe = executable(own)
            .expect("own executable")
            .expect("has a path");
        assert!(exe.is_absolute());
    }

    #[test]
    fn this_process_is_listed_and_its_cwd_and_fds_are_readable() {
        let own = std::process::id() as libc::pid_t;
        assert!(list_pids().expect("pid list").contains(&own));
        let cwd = working_directory(own).expect("own cwd");
        let expected = std::fs::canonicalize(std::env::current_dir().unwrap()).unwrap();
        assert_eq!(cwd.path, expected);
        let temp = tempfile::tempdir().unwrap();
        let file = std::fs::File::create(temp.path().join("held")).unwrap();
        let meta = file.metadata().unwrap();
        let held = std::os::unix::io::AsRawFd::as_raw_fd(&file);
        let vnode = list_fds(own)
            .expect("own fds")
            .into_iter()
            .filter(is_vnode)
            .find(|fd| fd.proc_fd == held)
            .map(|fd| fd_vnode(own, fd.proc_fd))
            .expect("the held descriptor is listed")
            .expect("readable")
            .expect("has a path");
        assert!(vnode.path.ends_with("held"));
        assert_eq!(
            (vnode.device, vnode.inode),
            (
                std::os::unix::fs::MetadataExt::dev(&meta),
                std::os::unix::fs::MetadataExt::ino(&meta)
            )
        );
    }

    #[test]
    fn pid_listing_grows_when_the_buffer_comes_back_full() {
        // The host has 5000 pids but the first buffer holds 100.
        let host: Vec<libc::pid_t> = (1..=5000).collect();
        let mut calls = 0;
        let pids = collect_pids(36, |buffer| {
            calls += 1;
            let n = buffer.len().min(host.len());
            buffer[..n].copy_from_slice(&host[..n]);
            n as libc::c_int
        })
        .expect("a growing listing succeeds");
        assert_eq!(pids, host);
        assert!(calls > 1, "a full buffer must be retried");
    }

    #[test]
    fn pid_listing_accepts_a_buffer_with_room_to_spare() {
        let pids = collect_pids(10, |buffer| {
            buffer[..3].copy_from_slice(&[7, 8, 9]);
            3
        });
        assert_eq!(pids, Some(vec![7, 8, 9]));
    }

    #[test]
    fn pid_listing_that_never_stops_filling_is_refused_at_the_bound() {
        let mut largest = 0;
        let pids = collect_pids(10, |buffer| {
            largest = buffer.len();
            buffer.len() as libc::c_int
        });
        assert_eq!(pids, None);
        assert_eq!(largest, MAX_LISTED_PIDS);
    }

    #[test]
    fn pid_listing_failure_is_refused() {
        assert_eq!(collect_pids(10, |_| 0), None);
        assert_eq!(collect_pids(10, |_| -1), None);
    }

    #[test]
    fn region_record_matches_the_kernel_size() {
        // `struct proc_regioninfo` is 96 bytes; the kernel rejects any other size.
        assert_eq!(std::mem::size_of::<ProcRegionInfo>(), 96);
        let own = std::process::id() as libc::pid_t;
        assert!(region_at(own, 0).expect("own regions").is_some());
    }

    #[test]
    fn a_mapped_file_is_listed_after_its_descriptor_is_closed() {
        use std::os::unix::fs::MetadataExt;
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("mapped");
        std::fs::write(&path, vec![1u8; 16384]).unwrap();
        let file = std::fs::File::open(&path).unwrap();
        let meta = file.metadata().unwrap();
        // SAFETY: maps a regular file read-only; the mapping is unmapped below.
        let map = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                16384,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                std::os::unix::io::AsRawFd::as_raw_fd(&file),
                0,
            )
        };
        assert_ne!(map, libc::MAP_FAILED);
        drop(file);
        let own = std::process::id() as libc::pid_t;
        let (mut address, mut found) = (0_u64, false);
        while let Some(region) = region_at(own, address).expect("walk own regions") {
            if let Some(vnode) = &region.vnode
                && (vnode.device, vnode.inode) == (meta.dev(), meta.ino())
            {
                found = true;
            }
            address = region.address + region.size;
        }
        // SAFETY: unmaps exactly the mapping made above.
        unsafe { libc::munmap(map, 16384) };
        assert!(found, "the mapping outlives its closed descriptor");
    }

    #[test]
    fn kinfo_credentials_agree_with_bsdinfo_for_every_readable_process() {
        let (mut compared, mut withheld) = (0, 0);
        for pid in list_pids().expect("pid list") {
            if pid <= 0 {
                continue;
            }
            let Ok(info) = pid_info::<libc::proc_bsdinfo>(pid) else {
                // libproc withholds it: the process table must still name the owner.
                match credentials(pid) {
                    Ok(_) => withheld += 1,
                    Err(code) => assert!(exited(code), "pid {pid}: errno {code}"),
                }
                continue;
            };
            let Ok(kinfo) = kinfo_credentials(pid) else {
                continue;
            };
            assert_eq!(
                (kinfo.uid, kinfo.ruid, kinfo.zombie),
                (
                    info.pbi_uid,
                    info.pbi_ruid,
                    info.pbi_status == u32::from(SZOMB)
                ),
                "pid {pid}"
            );
            compared += 1;
        }
        assert!(compared > 0, "no process was cross-checked");
        let own = credentials(std::process::id() as libc::pid_t).unwrap();
        assert_eq!(own.uid, nix::unistd::geteuid().as_raw());
        assert_eq!(own.ruid, nix::unistd::getuid().as_raw());
        eprintln!("cross-checked {compared}, read {withheld} withheld by libproc");
    }

    #[test]
    fn a_process_that_does_not_exist_is_exited() {
        // pid_max on macOS is 99999.
        assert!(exited(credentials(99_998).expect_err("no such process")));
        assert_eq!(executable(99_998), Ok(None));
    }

    #[test]
    fn platform_binary_flag_tells_launchd_from_this_test_binary() {
        assert_eq!(is_platform_binary(1), Ok(true));
        assert_eq!(
            is_platform_binary(std::process::id() as libc::pid_t),
            Ok(false)
        );
        assert_eq!(is_platform_binary(99_998), Err(libc::ESRCH));
    }

    /// Re-executed from an unlinked copy of the test binary by the test below.
    #[test]
    fn unlinked_executable_child_helper() {
        use std::io::Write;
        if std::env::var_os("RSI_TEST_UNLINKED_EXE_CHILD").is_none() {
            return;
        }
        // libtest prefixes this line with "test <name> ... ".
        println!("UNLINKED-READY");
        std::io::stdout().flush().unwrap();
        std::thread::sleep(std::time::Duration::from_secs(60));
    }

    #[test]
    fn a_process_whose_executable_was_unlinked_names_no_path() {
        use std::io::{BufRead, BufReader};
        let temp = tempfile::tempdir().unwrap();
        let copy = temp.path().join("copy");
        std::fs::copy(std::env::current_exe().unwrap(), &copy).unwrap();
        let mut child = std::process::Command::new(&copy)
            .args([
                "--exact",
                "libproc_macos::tests::unlinked_executable_child_helper",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("RSI_TEST_UNLINKED_EXE_CHILD", "1")
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id() as libc::pid_t;
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let ready = lines.any(|line| line.is_ok_and(|line| line.contains("UNLINKED-READY")));
        let before = executable(pid);
        std::fs::remove_file(&copy).unwrap();
        let after = executable(pid);
        child.kill().ok();
        child.wait().ok();
        assert!(ready, "the child started");
        assert!(matches!(before, Ok(Some(_))), "{before:?}");
        assert_eq!(after, Ok(None), "an unlinked executable names no path");
    }
}
