use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

const SCHEMA_VERSION: u8 = 1;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(super) struct BaseShardEntry {
    pub schema_version: u8,
    pub rolling_sha: String,
    pub shard: String,
    pub fingerprint: String,
    pub failures: BTreeSet<String>,
    pub provenance: String,
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
            provenance: provenance.into(),
        }
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
    }
}

pub(super) fn default_root() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("RSI_ROLLING_BASE_CACHE_DIR") {
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
    pub(super) async fn acquire(
        root: &Path,
        rolling_sha: &str,
        shard: &str,
        fingerprint: &str,
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
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .open(directory.join(format!("{shard}.lock")))
            .map_err(|error| format!("cannot open base shard cache lock: {error}"))?;
        loop {
            match lock.try_lock() {
                Ok(()) => break,
                Err(fs::TryLockError::WouldBlock) => {
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
