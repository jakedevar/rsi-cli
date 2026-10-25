use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const SCHEMA_VERSION: u8 = 1;
/// Longest a test waits on a base-shard lock when it gives no tighter bound;
/// the lander always passes its own bound (`acquire_within`).
#[cfg(test)]
const DEFAULT_LOCK_WAIT: Duration = Duration::from_secs(2 * 60 * 60);
const LOCK_NOTICE_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(super) struct BaseShardEntry {
    pub schema_version: u8,
    pub rolling_sha: String,
    pub shard: String,
    pub fingerprint: String,
    pub failures: BTreeSet<String>,
    /// Signature evidence from the base report. Older names-only entries
    /// remain readable, but cannot prove a known-red timeout exemption.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub failure_texts: BTreeMap<String, String>,
    pub provenance: String,
    /// `<package>:<target>` the base run proved absent, which is why an empty
    /// `failures` is empty (#1812). Entries written before it carry none.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub absent_targets: BTreeSet<String>,
}

impl BaseShardEntry {
    pub(super) fn new(
        rolling_sha: &str,
        shard: &str,
        fingerprint: &str,
        failures: BTreeSet<String>,
        provenance: &str,
    ) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            rolling_sha: rolling_sha.into(),
            shard: shard.into(),
            fingerprint: fingerprint.into(),
            failures,
            failure_texts: BTreeMap::new(),
            provenance: provenance.into(),
            absent_targets: BTreeSet::new(),
        }
    }

    pub(super) fn with_absent_targets(mut self, absent_targets: BTreeSet<String>) -> Self {
        self.absent_targets = absent_targets;
        self
    }

    pub(super) fn with_failure_texts(mut self, texts: &BTreeMap<String, String>) -> Self {
        self.failure_texts = texts
            .iter()
            .filter(|(name, _)| self.failures.contains(*name))
            .map(|(name, text)| (name.clone(), text.clone()))
            .collect();
        self
    }

    fn matches(&self, rolling_sha: &str, shard: &str, fingerprint: &str) -> bool {
        self.schema_version == SCHEMA_VERSION
            && self.rolling_sha == rolling_sha
            && self.shard == shard
            && self.fingerprint == fingerprint
            && !self.provenance.is_empty()
            && self.failures.iter().all(|name| {
                !name.is_empty()
                    && name
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"_:.-".contains(&byte))
            })
            && self.absent_targets.iter().all(|target| {
                !target.is_empty()
                    && target
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"_:.-".contains(&byte))
            })
    }
}

/// Most test names one per-test outcome file holds (#2059). A gate that would
/// grow it past the bound records nothing more; the file lives in the base
/// SHA's directory, so a tip change starts a fresh, empty file.
const TEST_OUTCOME_LIMIT: usize = 4096;

fn plain_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_:.-".contains(&byte))
}

/// What the base (the rolling tip) did to individual tests under one command
/// class and environment class (#2059): the narrowed base run of a red
/// candidate answers per test name, so a later gate on the same tip reads the
/// answer instead of running the base again.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
struct BaseTestOutcomes {
    schema_version: u8,
    rolling_sha: String,
    class: String,
    red: BTreeSet<String>,
    green: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    failure_texts: BTreeMap<String, String>,
}

pub(super) struct TestOutcome {
    pub red: bool,
    pub failure_text: Option<String>,
}

impl BaseTestOutcomes {
    fn valid(&self, rolling_sha: &str, class: &str) -> bool {
        self.schema_version == SCHEMA_VERSION
            && self.rolling_sha == rolling_sha
            && self.class == class
            && self
                .red
                .iter()
                .chain(&self.green)
                .all(|name| plain_name(name))
            && self.red.is_disjoint(&self.green)
    }
}

fn test_outcome_paths(
    root: &Path,
    rolling_sha: &str,
    class: &str,
) -> Result<(PathBuf, PathBuf), String> {
    let digest = class
        .strip_prefix("sha256:")
        .filter(|value| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or("invalid base test outcome class")?;
    if rolling_sha.len() != 40 || !rolling_sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("invalid base test outcome key".into());
    }
    let directory = root.join(rolling_sha);
    Ok((
        directory.join(format!("tests-{digest}.json")),
        directory.join(format!("tests-{digest}.lock")),
    ))
}

/// The recorded outcome and optional failure text per test on `rolling_sha`.
/// Older files still prove membership but cannot prove a known-red timeout.
/// An unreadable, mismatched or self-contradicting file answers nothing, so
/// every test is run.
pub(super) fn load_test_outcomes(
    root: &Path,
    rolling_sha: &str,
    class: &str,
) -> BTreeMap<String, TestOutcome> {
    let Ok((path, _)) = test_outcome_paths(root, rolling_sha, class) else {
        return BTreeMap::new();
    };
    let Ok(bytes) = fs::read(path) else {
        return BTreeMap::new();
    };
    let Ok(file) = serde_json::from_slice::<BaseTestOutcomes>(&bytes) else {
        return BTreeMap::new();
    };
    if !file.valid(rolling_sha, class) {
        return BTreeMap::new();
    }
    file.red
        .into_iter()
        .map(|name| {
            let failure_text = file.failure_texts.get(&name).cloned();
            (
                name,
                TestOutcome {
                    red: true,
                    failure_text,
                },
            )
        })
        .chain(file.green.into_iter().map(|name| {
            (
                name,
                TestOutcome {
                    red: false,
                    failure_text: None,
                },
            )
        }))
        .collect()
}

/// Merge one base run's per-test outcomes into the file for `rolling_sha`. A
/// name the file already holds with the opposite outcome is flaky evidence: it
/// is dropped from both sides, so the next gate measures it again.
pub(super) fn record_test_outcomes(
    root: &Path,
    rolling_sha: &str,
    class: &str,
    red: &BTreeSet<String>,
    green: &BTreeSet<String>,
    failure_texts: &BTreeMap<String, String>,
) -> Result<(), String> {
    let (path, lock_path) = test_outcome_paths(root, rolling_sha, class)?;
    let parent = path.parent().ok_or("outcome file has no parent")?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)
        .map_err(|error| format!("cannot create base test outcome cache: {error}"))?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .open(&lock_path)
        .map_err(|error| format!("cannot open base test outcome lock: {error}"))?;
    // Held for one read-merge-write of a small file.
    lock.lock()
        .map_err(|error| format!("cannot lock base test outcomes: {error}"))?;
    let mut file = fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<BaseTestOutcomes>(&bytes).ok())
        .filter(|file| file.valid(rolling_sha, class))
        .unwrap_or_else(|| BaseTestOutcomes {
            schema_version: SCHEMA_VERSION,
            rolling_sha: rolling_sha.into(),
            class: class.into(),
            ..BaseTestOutcomes::default()
        });
    for name in red.iter().filter(|name| plain_name(name)) {
        if file.green.remove(name) {
            file.failure_texts.remove(name);
            continue;
        }
        if file.red.len() + file.green.len() < TEST_OUTCOME_LIMIT {
            file.red.insert(name.clone());
            // Never retain text from an earlier report when this run has none.
            file.failure_texts.remove(name);
            if let Some(text) = failure_texts.get(name) {
                file.failure_texts.insert(name.clone(), text.clone());
            }
        }
    }
    for name in green.iter().filter(|name| plain_name(name)) {
        if file.red.remove(name) {
            file.failure_texts.remove(name);
            continue;
        }
        if file.red.len() + file.green.len() < TEST_OUTCOME_LIMIT {
            file.green.insert(name.clone());
        }
    }
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .map_err(|error| format!("cannot create base test outcome temp: {error}"))?;
    serde_json::to_writer(&mut temporary, &file)
        .map_err(|error| format!("cannot serialize base test outcomes: {error}"))?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|error| format!("cannot sync base test outcome temp: {error}"))?;
    temporary
        .persist(&path)
        .map_err(|error| format!("cannot publish base test outcomes: {}", error.error))?;
    Ok(())
}

pub(super) fn default_root() -> Result<PathBuf, String> {
    if let Some(path) = crate::lander_env::var_os("RSI_ROLLING_BASE_CACHE_DIR") {
        return Ok(PathBuf::from(path));
    }
    dirs::home_dir()
        .map(|home| home.join(".rsi/cache/rolling-base-shards-v1"))
        .ok_or_else(|| "cannot locate home for rolling base shard cache".into())
}

pub(super) struct BaseShardSlot {
    path: PathBuf,
    rolling_sha: String,
    shard: String,
    fingerprint: String,
    _lock: File,
}

impl BaseShardSlot {
    /// Acquire with the default bounded wait (`DEFAULT_LOCK_WAIT`).
    #[cfg(test)]
    pub(super) async fn acquire(
        root: &Path,
        rolling_sha: &str,
        shard: &str,
        fingerprint: &str,
    ) -> Result<Self, String> {
        Self::acquire_within(root, rolling_sha, shard, fingerprint, DEFAULT_LOCK_WAIT).await
    }

    /// Acquire the per-shard slot lock, waiting at most `max_wait`. Another
    /// lander holds the lock while it computes this base shard, so a wait is
    /// normal, but never silent or unbounded: a periodic diagnostic names the
    /// lock file, and an expired wait refuses (fail closed) instead of wedging
    /// the lander behind a stuck holder.
    pub(super) async fn acquire_within(
        root: &Path,
        rolling_sha: &str,
        shard: &str,
        fingerprint: &str,
        max_wait: Duration,
    ) -> Result<Self, String> {
        let digest = fingerprint
            .strip_prefix("sha256:")
            .filter(|value| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .ok_or("invalid base shard fingerprint")?;
        if rolling_sha.len() != 40
            || !rolling_sha.bytes().all(|byte| byte.is_ascii_hexdigit())
            || shard.is_empty()
            || !shard
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err("invalid base shard cache key".into());
        }
        let directory = root.join(rolling_sha).join(digest);
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&directory)
            .map_err(|error| format!("cannot create base shard cache: {error}"))?;
        let path = directory.join(format!("{shard}.json"));
        let lock_path = directory.join(format!("{shard}.lock"));
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .open(&lock_path)
            .map_err(|error| format!("cannot open base shard cache lock: {error}"))?;
        let started = Instant::now();
        let mut last_notice = started;
        loop {
            match lock.try_lock() {
                Ok(()) => break,
                Err(fs::TryLockError::WouldBlock) => {
                    let waited = started.elapsed();
                    if waited >= max_wait {
                        return Err(format!(
                            "base shard cache lock {} is still held after {}s: another lander is computing base shard {shard}; refusing to proceed without it (retry the landing once that lander finishes)",
                            lock_path.display(),
                            waited.as_secs()
                        ));
                    }
                    if last_notice.elapsed() >= LOCK_NOTICE_INTERVAL {
                        last_notice = Instant::now();
                        eprintln!(
                            "waiting for base shard cache lock {} (held by another lander computing base shard {shard}; waited {}s of {}s)",
                            lock_path.display(),
                            waited.as_secs(),
                            max_wait.as_secs()
                        );
                    }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
                Err(fs::TryLockError::Error(error)) => {
                    return Err(format!("cannot lock base shard cache: {error}"));
                }
            }
        }
        Ok(Self {
            path,
            rolling_sha: rolling_sha.into(),
            shard: shard.into(),
            fingerprint: fingerprint.into(),
            _lock: lock,
        })
    }

    pub(super) fn read(&self) -> Result<Option<BaseShardEntry>, String> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("cannot read base shard cache: {error}")),
        };
        let entry: BaseShardEntry = match serde_json::from_slice(&bytes) {
            Ok(entry) => entry,
            Err(_) => return Ok(None),
        };
        Ok(entry
            .matches(&self.rolling_sha, &self.shard, &self.fingerprint)
            .then_some(entry))
    }

    pub(super) fn write(&self, entry: &BaseShardEntry) -> Result<(), String> {
        if !entry.matches(&self.rolling_sha, &self.shard, &self.fingerprint) {
            return Err("base shard cache entry does not match its key".into());
        }
        if let Some(previous) = self.read()? {
            if previous.failures == entry.failures {
                return Ok(());
            }
            return Err(format!(
                "conflicting base shard results for {} {}",
                self.rolling_sha, self.shard
            ));
        }
        let parent = self.path.parent().ok_or("cache entry has no parent")?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)
            .map_err(|error| format!("cannot create base shard cache temp: {error}"))?;
        serde_json::to_writer(&mut temporary, entry)
            .map_err(|error| format!("cannot serialize base shard cache: {error}"))?;
        temporary
            .as_file()
            .sync_all()
            .map_err(|error| format!("cannot sync base shard cache temp: {error}"))?;
        temporary
            .persist(&self.path)
            .map_err(|error| format!("cannot publish base shard cache: {}", error.error))?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| format!("cannot sync base shard cache directory: {error}"))?;
        Ok(())
    }
}
