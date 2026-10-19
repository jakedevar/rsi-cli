//! Scratch-filesystem pressure (#565): a scratch write refused for quota or
//! space is a disk condition, never a malformed request.
//!
//! * [`classify`] maps an `io::Error` to a typed [`ScratchPressureKind`]
//!   (`EDQUOT` or `ENOSPC`); [`ScratchPressureV1`] is the operator-visible
//!   envelope, carrying the failing path and the next action.
//! * [`quota_report`] answers "whose quota, which mount?" without quota tools:
//!   the mount's filesystem type and options (`usrquota` etc.) plus the calling
//!   uid's bounded usage under the path. The numeric limit needs privileged
//!   `quotactl`, so the report says it is unreadable instead of guessing.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// `code` of a scratch write refused by a per-user (or project) quota.
pub const SCRATCH_DISK_QUOTA_EXCEEDED: &str = "scratch_disk_quota_exceeded";
/// `code` of a scratch write refused because the filesystem is full.
pub const SCRATCH_DISK_FULL: &str = "scratch_disk_full";

/// Linux `EDQUOT`; macOS uses 69. Matched by raw errno as well as by kind.
const EDQUOT_LINUX: i32 = 122;
const EDQUOT_MACOS: i32 = 69;
const ENOSPC: i32 = 28;

/// Maximum directory entries one usage walk visits.
pub const QUOTA_WALK_ENTRY_CAP: usize = 200_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScratchPressureKind {
    DiskQuota,
    DiskFull,
}

impl ScratchPressureKind {
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::DiskQuota => SCRATCH_DISK_QUOTA_EXCEEDED,
            Self::DiskFull => SCRATCH_DISK_FULL,
        }
    }

    #[must_use]
    pub const fn next_action(self) -> &'static str {
        match self {
            Self::DiskQuota => {
                "free space owned by your uid on that filesystem, or write the payload under \
                 an owned directory outside it (for example under ~/.rsi or /var/tmp); \
                 `rsi-rpc --params -` reads the request from stdin and needs no scratch file; \
                 run `rsi-rpc scratch-quota [PATH]` to see the mount's quota options and your usage"
            }
            Self::DiskFull => {
                "free space on that filesystem or write under an owned directory elsewhere; \
                 run `rsi-rpc scratch-quota [PATH]` to see the mount and your usage"
            }
        }
    }
}

/// Classify an `io::Error` as scratch-storage pressure, or `None`.
#[must_use]
pub fn classify(error: &io::Error) -> Option<ScratchPressureKind> {
    if let Some(code) = error.raw_os_error() {
        if code == EDQUOT_LINUX || code == EDQUOT_MACOS {
            return Some(ScratchPressureKind::DiskQuota);
        }
        if code == ENOSPC {
            return Some(ScratchPressureKind::DiskFull);
        }
    }
    match error.kind() {
        io::ErrorKind::QuotaExceeded => Some(ScratchPressureKind::DiskQuota),
        io::ErrorKind::StorageFull => Some(ScratchPressureKind::DiskFull),
        _ => None,
    }
}

/// Operator-visible envelope for a refused scratch write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScratchPressureV1 {
    pub code: String,
    /// Always `disk_pressure`: this is a disk condition, not a request fault.
    pub kind: String,
    pub errno: Option<i32>,
    pub path: Option<String>,
    pub message: String,
    pub next_action: String,
}

impl ScratchPressureV1 {
    /// Envelope for `error` when it is scratch pressure; `path` names the
    /// file whose write failed when the caller knows it.
    #[must_use]
    pub fn from_io(error: &io::Error, path: Option<&Path>) -> Option<Self> {
        let kind = classify(error)?;
        let location = path.map_or_else(String::new, |p| format!(" writing {}", p.display()));
        Some(Self {
            code: kind.code().to_string(),
            kind: "disk_pressure".to_string(),
            errno: error.raw_os_error(),
            path: path.map(|p| p.display().to_string()),
            message: format!("{}{location}: {error}", kind.code()),
            next_action: kind.next_action().to_string(),
        })
    }
}

/// One mount-table row, reduced to what a quota diagnosis needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MountInfoV1 {
    pub mount_point: String,
    pub fs_type: String,
    pub options: Vec<String>,
}

impl MountInfoV1 {
    /// Quota flavours enforced by the mount options (`usrquota`, ...).
    #[must_use]
    pub fn quota_kinds(&self) -> Vec<String> {
        self.options
            .iter()
            .filter(|o| {
                matches!(
                    o.split('=').next().unwrap_or(""),
                    "usrquota"
                        | "grpquota"
                        | "prjquota"
                        | "quota"
                        | "usrjquota"
                        | "grpjquota"
                        | "uqnoenforce"
                        | "gqnoenforce"
                        | "pqnoenforce"
                        | "uquota"
                        | "gquota"
                        | "pquota"
                )
            })
            .cloned()
            .collect()
    }
}

/// The mount containing `path`, from `/proc/self/mountinfo` text (the longest
/// mount-point prefix wins). Options merge the per-mount and per-superblock
/// lists.
#[must_use]
pub fn mount_for(path: &Path, mountinfo: &str) -> Option<MountInfoV1> {
    let mut best: Option<MountInfoV1> = None;
    for line in mountinfo.lines() {
        let Some((head, tail)) = line.split_once(" - ") else {
            continue;
        };
        let fields: Vec<&str> = head.split_whitespace().collect();
        let tail_fields: Vec<&str> = tail.split_whitespace().collect();
        if fields.len() < 6 || tail_fields.len() < 3 {
            continue;
        }
        let mount_point = unescape_mount(fields[4]);
        if !path.starts_with(&mount_point) {
            continue;
        }
        if best
            .as_ref()
            .is_some_and(|b| b.mount_point.len() >= mount_point.len())
        {
            continue;
        }
        let mut options: Vec<String> = Vec::new();
        for list in [fields[5], tail_fields[2]] {
            for option in list.split(',').filter(|o| !o.is_empty()) {
                if !options.iter().any(|seen| seen == option) {
                    options.push(option.to_string());
                }
            }
        }
        best = Some(MountInfoV1 {
            mount_point,
            fs_type: tail_fields[0].to_string(),
            options,
        });
    }
    best
}

fn unescape_mount(raw: &str) -> String {
    raw.replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\134", "\\")
}

/// Bounded quota diagnosis for one scratch path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScratchQuotaReportV1 {
    pub path: String,
    pub mount: Option<MountInfoV1>,
    pub quota_kinds: Vec<String>,
    pub quota_enforced: bool,
    pub uid: Option<u32>,
    /// Allocated bytes under `path` owned by `uid` (512-byte blocks).
    pub uid_used_bytes: u64,
    pub entries_scanned: usize,
    pub truncated: bool,
    /// The numeric limit is only readable through privileged `quotactl`.
    pub limit_readable: bool,
    pub note: String,
}

/// Build the report: mount options from `mountinfo` plus a walk of `path`
/// counting `uid`'s allocated bytes, visiting at most `entry_cap` entries.
#[must_use]
pub fn quota_report(
    path: &Path,
    mountinfo: &str,
    uid: Option<u32>,
    entry_cap: usize,
) -> ScratchQuotaReportV1 {
    let mount = mount_for(path, mountinfo);
    let quota_kinds = mount
        .as_ref()
        .map(MountInfoV1::quota_kinds)
        .unwrap_or_default();
    let (uid_used_bytes, entries_scanned, truncated) = walk_usage(path, uid, entry_cap);
    let quota_enforced = !quota_kinds.is_empty();
    let note = if quota_enforced {
        "per-user quota accounting is enforced on this mount: a write can be refused \
         (EDQUOT) while df still shows free space; the numeric limit needs privileged quotactl"
    } else if mount.is_some() {
        "no quota mount option: a refused write here is a full filesystem (ENOSPC), not a quota"
    } else {
        "mount table unavailable on this platform; quota state unknown"
    };
    ScratchQuotaReportV1 {
        path: path.display().to_string(),
        mount,
        quota_kinds,
        quota_enforced,
        uid,
        uid_used_bytes,
        entries_scanned,
        truncated,
        limit_readable: false,
        note: note.to_string(),
    }
}

#[cfg(unix)]
fn walk_usage(root: &Path, uid: Option<u32>, entry_cap: usize) -> (u64, usize, bool) {
    use std::os::unix::fs::MetadataExt;
    let mut used = 0u64;
    let mut scanned = 0usize;
    let mut pending: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if scanned >= entry_cap {
                return (used, scanned, true);
            }
            scanned += 1;
            let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if uid.is_none_or(|u| meta.uid() == u) {
                used = used.saturating_add(meta.blocks().saturating_mul(512));
            }
            if meta.is_dir() {
                pending.push(entry.path());
            }
        }
    }
    (used, scanned, false)
}

#[cfg(not(unix))]
fn walk_usage(_root: &Path, _uid: Option<u32>, _entry_cap: usize) -> (u64, usize, bool) {
    (0, 0, false)
}

/// The calling uid, from `/proc/self` ownership on Linux.
#[must_use]
pub fn current_uid() -> Option<u32> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata("/proc/self").ok().map(|m| m.uid())
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// Mount table text, or empty where unavailable.
#[must_use]
pub fn read_mountinfo() -> String {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default()
    }
    #[cfg(not(target_os = "linux"))]
    {
        String::new()
    }
}

/// `rsi-rpc scratch-quota [PATH]`: print the quota report for `PATH` (default
/// the process temp dir) as JSON. Returns the process exit code.
pub fn cli_main(args: &[String], out: &mut impl io::Write, err: &mut impl io::Write) -> u8 {
    let path = match args {
        [] => std::env::temp_dir(),
        [one] if !one.starts_with('-') => PathBuf::from(one),
        _ => {
            let _ = writeln!(err, "usage: rsi-rpc scratch-quota [PATH]");
            return 1;
        }
    };
    let report = quota_report(
        &path,
        &read_mountinfo(),
        current_uid(),
        QUOTA_WALK_ENTRY_CAP,
    );
    match serde_json::to_string_pretty(&report) {
        Ok(json) => {
            let _ = writeln!(out, "{json}");
            0
        }
        Err(error) => {
            let _ = writeln!(err, "internal error: {error}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MOUNTINFO: &str = "36 25 0:30 / / rw,relatime - ext4 /dev/sda1 rw\n\
        55 40 0:48 / /tmp rw,nosuid,nodev shared:158 - tmpfs tmpfs rw,size=31650608k,nr_inodes=1048576,inode64,huge=advise,usrquota\n";

    #[test]
    fn edquot_and_enospc_are_typed_disk_pressure_not_param_errors() {
        let quota = io::Error::from_raw_os_error(EDQUOT_LINUX);
        let env = ScratchPressureV1::from_io(&quota, Some(Path::new("/tmp/continue542.txt")))
            .expect("classified");
        // On non-Linux the raw errno 122 is not EDQUOT, so match by kind too.
        let kind_quota = io::Error::from(io::ErrorKind::QuotaExceeded);
        assert_eq!(classify(&kind_quota), Some(ScratchPressureKind::DiskQuota));
        if cfg!(target_os = "linux") {
            assert_eq!(env.code, SCRATCH_DISK_QUOTA_EXCEEDED);
            assert_eq!(env.errno, Some(EDQUOT_LINUX));
            assert!(env.message.contains("/tmp/continue542.txt"));
            assert_eq!(env.path.as_deref(), Some("/tmp/continue542.txt"));
        }
        assert_eq!(env.kind, "disk_pressure");
        assert!(env.next_action.contains("rsi-rpc scratch-quota"));

        let full = io::Error::from(io::ErrorKind::StorageFull);
        assert_eq!(
            ScratchPressureV1::from_io(&full, None).map(|e| e.code),
            Some(SCRATCH_DISK_FULL.to_string())
        );
        assert!(classify(&io::Error::from(io::ErrorKind::NotFound)).is_none());
    }

    #[test]
    fn mount_options_reveal_user_quota_on_tmp() {
        let mount = mount_for(Path::new("/tmp/x/y"), MOUNTINFO).expect("mount");
        assert_eq!(mount.mount_point, "/tmp");
        assert_eq!(mount.fs_type, "tmpfs");
        assert_eq!(mount.quota_kinds(), vec!["usrquota".to_string()]);
        let root = mount_for(Path::new("/var/tmp"), MOUNTINFO).expect("root");
        assert_eq!(root.mount_point, "/");
        assert!(root.quota_kinds().is_empty());
    }

    #[test]
    fn quota_report_counts_only_the_uid_and_stays_bounded() {
        let dir = tempfile::tempdir().expect("tempdir");
        for i in 0..5 {
            std::fs::write(dir.path().join(format!("f{i}")), vec![1u8; 8192]).expect("write");
        }
        let all = quota_report(dir.path(), MOUNTINFO, current_uid(), 100);
        assert!(!all.truncated);
        assert_eq!(all.entries_scanned, 5);
        let capped = quota_report(dir.path(), MOUNTINFO, None, 2);
        assert!(capped.truncated);
        assert_eq!(capped.entries_scanned, 2);
        assert!(!capped.limit_readable);
        if cfg!(unix) {
            assert!(all.uid_used_bytes >= 5 * 8192);
        }
        let mut out = Vec::new();
        let mut err = Vec::new();
        let args = [dir.path().display().to_string()];
        assert_eq!(cli_main(&args, &mut out, &mut err), 0);
        let printed: serde_json::Value = serde_json::from_slice(&out).expect("json");
        assert_eq!(printed["limit_readable"], false);
        assert_eq!(cli_main(&["--bad".to_string()], &mut out, &mut err), 1);
        let other = quota_report(dir.path(), MOUNTINFO, Some(u32::MAX - 1), 100);
        assert_eq!(other.uid_used_bytes, 0);
    }
}
