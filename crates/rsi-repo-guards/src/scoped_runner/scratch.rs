//! Private test scratch, with quota-aware tmpfs parity rather than a second run.
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub struct Scratch {
    pub path: PathBuf,
}
impl Scratch {
    pub fn tmpfs(parent: &Path) -> io::Result<Self> {
        if !is_tmpfs(parent)? {
            return Err(io::Error::other(
                "tmpfs parity requested on a non-tmpfs filesystem",
            ));
        }
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_nanos();
        let path = parent.join(format!("rsi-scoped-test-{}-{stamp}", std::process::id()));
        std::fs::create_dir(&path)?;
        Ok(Self { path })
    }
    pub fn environment(&self) -> [(String, String); 3] {
        let value = self.path.to_string_lossy().into_owned();
        [
            ("TMPDIR".into(), value.clone()),
            ("TMP".into(), value.clone()),
            ("TEMP".into(), value),
        ]
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[cfg(target_os = "linux")]
fn is_tmpfs(path: &Path) -> io::Result<bool> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(io::Error::other)?;
    // SAFETY: statfs writes one initialized record and reads a NUL-terminated path.
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(path.as_ptr(), &raw mut stat) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(stat.f_type as u64 == 0x0102_1994)
}
#[cfg(not(target_os = "linux"))]
fn is_tmpfs(_: &Path) -> io::Result<bool> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "tmpfs parity unavailable on this host",
    ))
}

pub fn choose(ctx: &crate::Ctx) -> io::Result<Option<PathBuf>> {
    let parent = ctx
        .var("CHECK_TOUCHED_SHARDS_TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/dev/shm"));
    if !parent.is_dir() {
        return Ok(None);
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = parent;
        Ok(None)
    }
    #[cfg(target_os = "linux")]
    {
        if !is_tmpfs(&parent)? {
            return Ok(None);
        }
        let min = ctx
            .var("RSI_SCOPED_TEST_TMPFS_MIN_GB")
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(12);
        Ok(
            (crate::cargo_slot::user_free_bytes(&parent)? >= min.saturating_mul(1 << 30))
                .then_some(parent),
        )
    }
}
