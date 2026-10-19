//! Read-only local release detection. Never infer a build from checkout HEAD,
//! never install it, and never execute an artifact on the RPC request path.

use chrono::{DateTime, SecondsFormat, Utc};
use rsi_common::agent_daemon_info::LocalBuildV1;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime};
use tokio::io::AsyncReadExt;

pub(crate) const REFRESH_INTERVAL: Duration = Duration::from_secs(30);
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_BUILD_INFO_BYTES: u64 = 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
struct Stamp {
    modified: SystemTime,
    size: u64,
}

fn stamp(path: &Path) -> Option<Stamp> {
    let metadata = path.metadata().ok().filter(|metadata| metadata.is_file())?;
    Some(Stamp {
        modified: metadata.modified().ok()?,
        size: metadata.len(),
    })
}

pub(crate) fn candidates(repo: Option<&Path>, data_dir: &Path) -> Vec<PathBuf> {
    let binary = format!("rsid{}", std::env::consts::EXE_SUFFIX);
    let mut paths = Vec::new();
    if let Some(repo) = repo {
        paths.push(repo.join("target/release").join(&binary));
    }
    paths.push(
        crate::release_target::release_target_dir_in(data_dir)
            .join("release")
            .join(binary),
    );
    paths
}

pub(crate) struct Detector {
    paths: Vec<PathBuf>,
    running_sha: String,
    running_modified: Option<SystemTime>,
    cache: HashMap<PathBuf, (Stamp, Option<String>)>,
    warned: HashSet<String>,
}

impl Detector {
    pub(crate) fn new(paths: Vec<PathBuf>, running_sha: &str, running: &Path) -> Self {
        Self {
            paths,
            running_sha: running_sha.to_owned(),
            // Capture the running artifact's age before a later install replaces it.
            running_modified: stamp(running).map(|stamp| stamp.modified),
            cache: HashMap::new(),
            warned: HashSet::new(),
        }
    }

    pub(crate) async fn refresh(&mut self) -> Option<LocalBuildV1> {
        if !known_sha(&self.running_sha) {
            return None;
        }
        let mut newest: Option<(SystemTime, LocalBuildV1)> = None;
        for path in &self.paths {
            let Some(before) = stamp(path) else {
                self.cache.remove(path);
                continue;
            };
            let sha = match self.cache.get(path) {
                Some((cached, sha)) if *cached == before => sha.clone(),
                _ => {
                    let sha = probe(path, PROBE_TIMEOUT).await;
                    // A concurrent rebuild cannot pair one build's SHA with another's age.
                    if stamp(path) != Some(before) {
                        self.cache.remove(path);
                        continue;
                    }
                    self.cache.insert(path.clone(), (before, sha.clone()));
                    sha
                }
            };
            let Some(sha) = sha else { continue };
            if newest
                .as_ref()
                .is_some_and(|(time, _)| *time >= before.modified)
            {
                continue;
            }
            let differs = sha != self.running_sha
                && self
                    .running_modified
                    .is_some_and(|time| before.modified > time);
            newest = Some((
                before.modified,
                LocalBuildV1 {
                    sha,
                    path: path.display().to_string(),
                    built_at: DateTime::<Utc>::from(before.modified)
                        .to_rfc3339_opts(SecondsFormat::Nanos, true),
                    differs,
                },
            ));
        }
        let build = newest.map(|(_, build)| build)?;
        if build.differs && self.warned.insert(build.sha.clone()) {
            tracing::warn!(running_build = %self.running_sha, built_build = %build.sha,
                path = %build.path, "newer local rsid build: run make release-install");
        }
        Some(build)
    }
}

fn known_sha(sha: &str) -> bool {
    sha.len() == 40 && sha.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Bounded output and runtime, empty environment, no inherited token or input.
async fn probe(path: &Path, timeout: Duration) -> Option<String> {
    let mut child = tokio::process::Command::new(path)
        .arg("--build-info")
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?.take(MAX_BUILD_INFO_BYTES + 1);
    let mut bytes = Vec::new();
    let result = tokio::time::timeout(timeout, async {
        tokio::try_join!(child.wait(), stdout.read_to_end(&mut bytes))
    })
    .await;
    let Ok(Ok((status, _))) = result else {
        let _ = child.kill().await;
        let _ = child.wait().await;
        return None;
    };
    if !status.success() || bytes.len() > MAX_BUILD_INFO_BYTES as usize {
        return None;
    }
    let (sha, _) = crate::deploy::parse_build_info(std::str::from_utf8(&bytes).ok()?)?;
    known_sha(&sha).then_some(sha)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    fn fixture(path: &Path, sha: &str, time: u64) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            path,
            format!("#!/bin/sh\nprintf x >> \"$0.calls\"\nprintf '{sha} 1\\n'\n"),
        )
        .unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::File::open(path)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(time))
            .unwrap();
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn newest_artifact_refreshes_caches_and_clears_without_installing() {
        let dir = tempfile::tempdir().unwrap();
        let running = dir.path().join("installed/rsid");
        let old = "a".repeat(40);
        let new = "b".repeat(40);
        let newer = "c".repeat(40);
        fixture(&running, &old, 100);
        let paths = candidates(Some(dir.path()), &dir.path().join("data"));
        let mut detector = Detector::new(paths.clone(), &old, &running);
        let service = crate::daemon_info::DaemonInfoService::new(
            dir.path().join("data"),
            dir.path().join("sandboxes"),
            running.clone(),
        );
        assert_eq!(detector.refresh().await, None);
        fixture(&paths[0], &new, 90);
        assert!(!detector.refresh().await.unwrap().differs);
        fixture(&paths[0], &new, 200);
        let build = detector.refresh().await.unwrap();
        assert!(build.differs);
        assert_eq!(build.sha, new);
        assert_eq!(build.built_at, "1970-01-01T00:03:20.000000000Z");
        service.refresh_local_build(&mut detector).await;
        let snapshot = service.snapshot(1);
        assert_eq!(snapshot.local_build, Some(build.clone()));
        assert_eq!(
            serde_json::to_value(snapshot).unwrap()["local_build"]["sha"],
            new
        );
        assert_eq!(
            std::fs::read(paths[0].with_file_name("rsid.calls")).unwrap(),
            b"xx"
        );
        fixture(&paths[1], &newer, 300);
        assert_eq!(detector.refresh().await.unwrap().sha, newer);
        fixture(&paths[0], &old, 400);
        let matching = detector.refresh().await.unwrap();
        assert_eq!(matching.sha, old);
        assert!(!matching.differs);
        assert_eq!(detector.warned.len(), 2);
        assert_eq!(
            stamp(&running).unwrap().modified,
            SystemTime::UNIX_EPOCH + Duration::from_secs(100)
        );
        std::fs::remove_file(&paths[0]).unwrap();
        assert_eq!(detector.refresh().await.unwrap().sha, newer);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn missing_unknown_failed_and_unbounded_probes_are_optional() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rsid");
        fixture(&path, "unknown", 100);
        assert_eq!(probe(&path, PROBE_TIMEOUT).await, None);
        fixture(&path, &"a".repeat(2048), 101);
        assert_eq!(probe(&path, PROBE_TIMEOUT).await, None);
        std::fs::write(&path, "#!/bin/sh\nexec /bin/sleep 10\n").unwrap();
        assert_eq!(probe(&path, Duration::from_millis(25)).await, None);
        std::fs::write(&path, "#!/bin/sh\nexit 1\n").unwrap();
        assert_eq!(probe(&path, PROBE_TIMEOUT).await, None);
        assert_eq!(candidates(None, dir.path()).len(), 1);
        let mut detector = Detector::new(vec![path.clone()], "unknown", &path);
        assert_eq!(detector.refresh().await, None);
    }
}
