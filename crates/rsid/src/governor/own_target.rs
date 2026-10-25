//! Bounded, read-only evidence for #514's per-job incremental disk credit.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rsi_common::agent_jobs::{AgentJobV1, JobKind, JobState};
use uuid::Uuid;

/// Read only the two build identity fields we need; never retain or log the
/// rest of a process's environment. Other platforms keep the ordinary floor.
pub(crate) fn process_context(pid: u32) -> Option<(Uuid, PathBuf, PathBuf)> {
    #[cfg(target_os = "linux")]
    {
        use std::io::Read;
        use std::os::unix::ffi::OsStrExt;
        let holder = super::resolve_proc_holder(pid)?;
        let mut bytes = Vec::new();
        std::fs::File::open(format!("/proc/{pid}/environ"))
            .ok()?
            .take(65_536)
            .read_to_end(&mut bytes)
            .ok()?;
        let value = |name: &[u8]| {
            bytes
                .split(|byte| *byte == 0)
                .find_map(|entry| entry.strip_prefix(name))
        };
        let job = Uuid::parse_str(std::str::from_utf8(value(b"RSI_JOB_ID=")?).ok()?).ok()?;
        let target = PathBuf::from(std::ffi::OsStr::from_bytes(value(b"CARGO_TARGET_DIR=")?));
        let cwd = std::fs::read_link(format!("/proc/{pid}/cwd")).ok()?;
        (super::resolve_proc_holder(pid) == Some(holder)).then_some((job, target, cwd))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

/// All paths must resolve inside the durable job owner's own sandbox. A
/// delegated/sibling target, symlinked target or non-job caller earns nothing.
pub(crate) fn eligible_target(
    job: &AgentJobV1,
    sandbox: &Path,
    target: &Path,
    process_cwd: &Path,
) -> Option<PathBuf> {
    if job.state != JobState::Running || !matches!(job.kind, JobKind::Build | JobKind::Test) {
        return None;
    }
    let root = sandbox.canonicalize().ok()?;
    let own = root.join("target");
    if !target.is_absolute()
        || target != own
        || !Path::new(&job.cwd).canonicalize().ok()?.starts_with(&root)
        || !process_cwd.canonicalize().ok()?.starts_with(&root)
        || !std::fs::symlink_metadata(&own).ok()?.is_dir()
        || own.canonicalize().ok()? != own
    {
        return None;
    }
    Some(own)
}

/// A lower bound is sufficient. Stop at 8 GiB (maximum useful evidence),
/// 100,000 entries or 100 ms; skip harness scratch, symlinks and other devices.
/// No directory from any other sandbox is walked, and nothing is removed.
pub(crate) fn reusable_bytes(target: &Path) -> u64 {
    reusable_bytes_on_disk(target, Path::new("/"))
}

fn reusable_bytes_on_disk(target: &Path, disk: &Path) -> u64 {
    allocated_bytes_on_disk(target, disk, 8 * 1_073_741_824)
}

/// Same bounded lower-bound scan, without the incremental-credit size cap.
/// Under-counting current usage conservatively over-reserves future growth.
pub(super) fn allocated_bytes(target: &Path) -> u64 {
    allocated_bytes_on_disk(target, Path::new("/"), u64::MAX)
}

fn allocated_bytes_on_disk(target: &Path, disk: &Path, max_bytes: u64) -> u64 {
    let Ok(root) = std::fs::symlink_metadata(target) else {
        return 0;
    };
    if !root.is_dir() {
        return 0;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // The governor measures `/`; a target on a different volume cannot
        // contribute reusable space to that disk gate.
        if !std::fs::metadata(disk).is_ok_and(|disk| disk.dev() == root.dev()) {
            return 0;
        }
    }
    #[cfg(not(unix))]
    let _ = disk;
    let start = Instant::now();
    let mut pending = vec![target.to_path_buf()];
    let mut entries = 0;
    let mut bytes = 0_u64;
    #[cfg(unix)]
    let mut seen = std::collections::HashSet::new();
    while let Some(directory) = pending.pop() {
        let Ok(children) = std::fs::read_dir(&directory) else {
            continue;
        };
        for child in children.flatten() {
            entries += 1;
            if entries > 100_000 || start.elapsed() >= Duration::from_millis(100) {
                return bytes;
            }
            if directory == target && child.file_name() == ".rsi-tmp" {
                continue;
            }
            let Ok(meta) = std::fs::symlink_metadata(child.path()) else {
                continue;
            };
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if meta.dev() != root.dev() {
                    continue;
                }
            }
            if meta.is_dir() {
                pending.push(child.path());
            } else if meta.is_file() {
                // Count allocated blocks, not sparse-file logical lengths.
                #[cfg(unix)]
                let size = {
                    use std::os::unix::fs::MetadataExt;
                    if !seen.insert((meta.dev(), meta.ino())) {
                        continue;
                    }
                    meta.blocks().saturating_mul(512)
                };
                #[cfg(not(unix))]
                let size = meta.len();
                bytes = bytes.saturating_add(size).min(max_bytes);
                if bytes == max_bytes {
                    return bytes;
                }
            }
        }
    }
    bytes
}

#[cfg(all(
    test,
    any(not(feature = "test-shard-mode"), feature = "test-shard-other-01")
))]
mod tests {
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    fn running_job(cwd: &Path) -> AgentJobV1 {
        serde_json::from_value(serde_json::json!({
            "id": Uuid::new_v4(), "owner_session_id": Uuid::new_v4(),
            "kind": "build", "state": "running", "unit_name": "rsi-job-fixture",
            "cwd": cwd, "log_path": "fixture.log", "params": {"kind": "build", "command": "check"},
            "created_at": "2026-10-20T00:00:00.000000000Z"
        }))
        .unwrap()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn target_identity_requires_the_running_jobs_own_existing_sandbox_target() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().canonicalize().unwrap();
        let own = root.join("own");
        let other = root.join("other");
        std::fs::create_dir_all(own.join("nested")).unwrap();
        std::fs::create_dir_all(other.join("target")).unwrap();
        let mut job = running_job(&own);
        let target = own.join("target");
        assert_eq!(eligible_target(&job, &own, &target, &own), None);
        std::fs::create_dir(&target).unwrap();
        assert_eq!(
            eligible_target(&job, &own, &target, &own.join("nested")),
            Some(target.clone())
        );
        assert_eq!(
            eligible_target(&job, &own, &other.join("target"), &own),
            None
        );
        assert_eq!(eligible_target(&job, &own, &target, &other), None);
        job.state = JobState::Succeeded;
        assert_eq!(eligible_target(&job, &own, &target, &own), None);
        job.state = JobState::Running;
        job.cwd = other.display().to_string();
        assert_eq!(eligible_target(&job, &own, &target, &own), None);
        job.cwd = own.display().to_string();
        job.kind = JobKind::Landing;
        assert_eq!(eligible_target(&job, &own, &target, &own), None);
        #[cfg(unix)]
        {
            job.kind = JobKind::Build;
            std::fs::remove_dir(&target).unwrap();
            std::os::unix::fs::symlink(other.join("target"), &target).unwrap();
            assert_eq!(eligible_target(&job, &own, &target, &own), None);
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn own_target_bytes_ignore_other_sandboxes_and_harness_scratch() {
        let fixture = tempfile::tempdir().unwrap();
        let own = fixture.path().join("own/target");
        let other = fixture.path().join("other/target");
        std::fs::create_dir_all(own.join(".rsi-tmp")).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(own.join("artifact"), vec![1; 8192]).unwrap();
        let before = reusable_bytes_on_disk(&own, fixture.path());
        assert!(before > 0);
        std::fs::write(other.join("artifact"), vec![2; 65_536]).unwrap();
        std::fs::write(own.join(".rsi-tmp/harness"), vec![3; 65_536]).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&other, own.join("sibling")).unwrap();
        assert_eq!(reusable_bytes_on_disk(&own, fixture.path()), before);
        assert_eq!(reusable_bytes(&fixture.path().join("absent/target")), 0);
        assert_eq!(
            std::fs::read(other.join("artifact")).unwrap(),
            vec![2; 65_536]
        );
        assert_eq!(
            std::fs::read(own.join(".rsi-tmp/harness")).unwrap(),
            vec![3; 65_536]
        );
    }
}
