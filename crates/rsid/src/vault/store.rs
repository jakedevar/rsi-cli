//! `~/.rsi/vault/credentials.json`: dir 0700, file 0600.
//!
//! Writes go through temp + fsync + rename. Deliberately not in `SQLite`, so
//! DB dumps, diagnostics and the `db` skill never carry secrets (and no
//! migration exists for it).

use super::secret::{SecretString, serde_exposed};
use super::slots::Slot;
use chrono::{DateTime, Utc};
use rsi_common::provider_credentials::CredentialCheckMetadata;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

pub const VAULT_DIR_NAME: &str = "vault";
pub const VAULT_FILE_NAME: &str = "credentials.json";
const LEGACY_VAULT_FILE_VERSION: u32 = 1;
pub const VAULT_FILE_VERSION: u32 = 2;
const DIR_MODE: u32 = 0o700;
const FILE_MODE: u32 = 0o600;

/// Typed vault store failures. Every variant names the offending path; none
/// carries file contents.
#[derive(Debug, thiserror::Error)]
pub enum VaultStoreError {
    #[error(
        "vault path {path} has mode {mode:04o}, looser than required {required:04o}; run `chmod {required:o} {path}`"
    )]
    LooseMode {
        path: PathBuf,
        mode: u32,
        required: u32,
    },
    #[error("vault path {path} must be a regular {expected}, not a symlink or other file type")]
    WrongFileType {
        path: PathBuf,
        expected: &'static str,
    },
    #[error("vault file {path} is malformed ({reason})")]
    Malformed { path: PathBuf, reason: String },
    #[error("vault I/O failed at {path}: {kind:?}")]
    Io {
        path: PathBuf,
        kind: std::io::ErrorKind,
    },
}

fn io_error(path: &Path) -> impl FnOnce(std::io::Error) -> VaultStoreError + '_ {
    move |error| VaultStoreError::Io {
        path: path.to_path_buf(),
        kind: error.kind(),
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredEntry {
    #[serde(with = "serde_exposed")]
    pub secret: SecretString,
    pub fingerprint: String,
    pub set_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotated_from_fingerprint: Option<String>,
    /// Legacy env var name an imported entry came from (a name, never a
    /// value). Pioneer keeps `PIONEER_AI_INFERENCE` vs `PIONEER_API_KEY` so
    /// the Codex `env_key` mapping is unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_env_var: Option<String>,
}

/// Durable tombstone: a clear suppresses every fallback for the slot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tombstone {
    pub at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VaultFile {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub entries: BTreeMap<Slot, StoredEntry>,
    #[serde(default)]
    pub cleared: BTreeMap<Slot, Tombstone>,
    #[serde(default)]
    pub checks: BTreeMap<Slot, CredentialCheckMetadata>,
    /// Per-slot generation, bumped on every Set, Rotate, Clear and import.
    #[serde(default)]
    pub generation: BTreeMap<Slot, u64>,
    /// Namespaced MCP credentials (#788). Ids are validated before reaching
    /// the store; no environment fallback applies to this namespace.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub mcp_entries: BTreeMap<String, StoredEntry>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub mcp_cleared: BTreeMap<String, Tombstone>,
    /// Per-server generation, bumped on every Set, Rotate and Clear.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub mcp_generation: BTreeMap<String, u64>,
}

impl VaultFile {
    /// Current generation of `slot` (0 before any mutation).
    #[must_use]
    pub fn generation_of(&self, slot: Slot) -> u64 {
        self.generation.get(&slot).copied().unwrap_or(0)
    }

    /// Bump `slot`'s generation and drop its (now stale) check.
    pub fn bump(&mut self, slot: Slot) {
        let next = self.generation_of(slot).saturating_add(1);
        self.generation.insert(slot, next);
        self.checks.remove(&slot);
    }

    /// Current generation of an MCP server credential (0 before any mutation).
    #[must_use]
    pub fn mcp_generation_of(&self, id: &str) -> u64 {
        self.mcp_generation.get(id).copied().unwrap_or(0)
    }

    /// Bump an MCP server's generation. MCP credentials have no provider check
    /// cache to invalidate.
    pub fn bump_mcp(&mut self, id: &str) {
        let next = self.mcp_generation_of(id).saturating_add(1);
        self.mcp_generation.insert(id.to_owned(), next);
    }
}

impl Default for VaultFile {
    fn default() -> Self {
        Self {
            version: VAULT_FILE_VERSION,
            entries: BTreeMap::new(),
            cleared: BTreeMap::new(),
            checks: BTreeMap::new(),
            generation: BTreeMap::new(),
            mcp_entries: BTreeMap::new(),
            mcp_cleared: BTreeMap::new(),
            mcp_generation: BTreeMap::new(),
        }
    }
}

const fn default_version() -> u32 {
    LEGACY_VAULT_FILE_VERSION
}

fn persisted_version(file: &VaultFile) -> u32 {
    if file.mcp_entries.is_empty() && file.mcp_cleared.is_empty() && file.mcp_generation.is_empty()
    {
        LEGACY_VAULT_FILE_VERSION
    } else {
        VAULT_FILE_VERSION
    }
}

/// The default vault directory under the RSI data dir.
#[must_use]
pub fn default_vault_dir() -> PathBuf {
    rsi_common::identity::data_dir().join(VAULT_DIR_NAME)
}

fn require_mode(
    path: &Path,
    metadata: &fs::Metadata,
    required: u32,
) -> Result<(), VaultStoreError> {
    let mode = metadata.mode() & 0o7777;
    if mode & !required != 0 {
        return Err(VaultStoreError::LooseMode {
            path: path.to_path_buf(),
            mode,
            required,
        });
    }
    Ok(())
}

/// Validate the directory (and file, if present) and load the vault. A
/// missing directory or file is an empty vault; a looser mode, a symlink or
/// malformed contents is a typed error naming the path.
///
/// # Errors
///
/// Returns a typed [`VaultStoreError`] naming the path.
pub fn load(dir: &Path) -> Result<VaultFile, VaultStoreError> {
    let dir_meta = match fs::symlink_metadata(dir) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(VaultFile::default());
        }
        Err(error) => return Err(io_error(dir)(error)),
    };
    if !dir_meta.is_dir() {
        return Err(VaultStoreError::WrongFileType {
            path: dir.to_path_buf(),
            expected: "directory",
        });
    }
    require_mode(dir, &dir_meta, DIR_MODE)?;
    let path = dir.join(VAULT_FILE_NAME);
    let file_meta = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(VaultFile::default());
        }
        Err(error) => return Err(io_error(&path)(error)),
    };
    if !file_meta.is_file() {
        return Err(VaultStoreError::WrongFileType {
            path,
            expected: "file",
        });
    }
    require_mode(&path, &file_meta, FILE_MODE)?;
    let bytes = fs::read(&path).map_err(io_error(&path))?;
    let bytes = zeroize::Zeroizing::new(bytes);
    let file: VaultFile =
        serde_json::from_slice(&bytes).map_err(|error| VaultStoreError::Malformed {
            path: path.clone(),
            // serde_json errors carry only a category and position, never
            // the offending value.
            reason: format!(
                "{:?} at line {} column {}",
                error.classify(),
                error.line(),
                error.column()
            ),
        })?;
    if file.version != 1 && file.version != VAULT_FILE_VERSION {
        return Err(VaultStoreError::Malformed {
            path,
            reason: format!("unsupported version {}", file.version),
        });
    }
    Ok(file)
}

/// Atomically persist the vault: temp file (0600, create-new) in the same
/// directory, write, fsync, rename over the target, fsync the directory.
///
/// # Errors
///
/// Returns a typed [`VaultStoreError`] naming the path.
pub fn save(dir: &Path, file: &mut VaultFile) -> Result<(), VaultStoreError> {
    ensure_dir(dir)?;
    file.version = persisted_version(file);
    let path = dir.join(VAULT_FILE_NAME);
    let bytes = zeroize::Zeroizing::new(serde_json::to_vec_pretty(file).map_err(|_| {
        VaultStoreError::Malformed {
            path: path.clone(),
            reason: "serialization failed".into(),
        }
    })?);
    let temp = dir.join(format!(
        ".{VAULT_FILE_NAME}.{}.{}.tmp",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    let write = || -> Result<(), VaultStoreError> {
        let mut handle = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(FILE_MODE)
            .open(&temp)
            .map_err(io_error(&temp))?;
        handle.write_all(&bytes).map_err(io_error(&temp))?;
        handle.sync_all().map_err(io_error(&temp))?;
        fs::rename(&temp, &path).map_err(io_error(&path))?;
        fs::File::open(dir)
            .and_then(|handle| handle.sync_all())
            .map_err(io_error(dir))
    };
    let result = write();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn ensure_dir(dir: &Path) -> Result<(), VaultStoreError> {
    match fs::symlink_metadata(dir) {
        Ok(metadata) => {
            if !metadata.is_dir() {
                return Err(VaultStoreError::WrongFileType {
                    path: dir.to_path_buf(),
                    expected: "directory",
                });
            }
            require_mode(dir, &metadata, DIR_MODE)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = dir.parent() {
                fs::create_dir_all(parent).map_err(io_error(parent))?;
            }
            fs::DirBuilder::new()
                .mode(DIR_MODE)
                .create(dir)
                .map_err(io_error(dir))?;
            // DirBuilder honours the umask; force the exact mode.
            fs::set_permissions(dir, std::os::unix::fs::PermissionsExt::from_mode(DIR_MODE))
                .map_err(io_error(dir))
        }
        Err(error) => Err(io_error(dir)(error)),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn entry(secret: &str) -> StoredEntry {
        let secret = SecretString::new(secret.into());
        StoredEntry {
            fingerprint: secret.fingerprint(),
            secret,
            set_at: Utc::now(),
            rotated_from_fingerprint: None,
            source_env_var: None,
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn missing_vault_loads_empty_without_creating_anything() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("vault");
        let file = load(&dir).unwrap();
        assert!(file.entries.is_empty());
        assert!(!dir.exists());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn save_creates_0700_dir_and_0600_file_and_round_trips() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("vault");
        let mut file = VaultFile::default();
        file.entries
            .insert(Slot::Openrouter, entry("sk-test-roundtrip"));
        save(&dir, &mut file).unwrap();
        let dir_mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        let file_mode = fs::metadata(dir.join(VAULT_FILE_NAME))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700);
        assert_eq!(file_mode, 0o600);
        let loaded = load(&dir).unwrap();
        assert_eq!(
            loaded.entries[&Slot::Openrouter].secret.expose(),
            "sk-test-roundtrip"
        );
        // No temp files are left behind.
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn provider_only_vault_stays_version_one_for_older_binaries() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("vault");
        let mut file = VaultFile::default();
        file.entries
            .insert(Slot::Openrouter, entry("sk-test-provider"));
        save(&dir, &mut file).unwrap();

        let raw = fs::read_to_string(dir.join(VAULT_FILE_NAME)).unwrap();
        assert!(raw.contains("\"version\": 1"), "{raw}");
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct LegacyVaultFile {
            version: u32,
            entries: BTreeMap<Slot, StoredEntry>,
            cleared: BTreeMap<Slot, Tombstone>,
            checks: BTreeMap<Slot, CredentialCheckMetadata>,
            generation: BTreeMap<Slot, u64>,
        }
        let legacy: LegacyVaultFile = serde_json::from_str(&raw).unwrap();
        assert_eq!(legacy.version, 1);
        assert_eq!(legacy.entries.len(), 1);
        assert!(legacy.cleared.is_empty());
        assert!(legacy.checks.is_empty());
        assert!(legacy.generation.is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn mcp_state_upgrades_vault_to_version_two_and_backloads() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("vault");
        let mut file = VaultFile::default();
        file.entries
            .insert(Slot::Openrouter, entry("sk-test-provider"));
        file.mcp_entries
            .insert("docs".into(), entry("mcp-test-secret"));
        file.bump_mcp("docs");
        save(&dir, &mut file).unwrap();

        let raw = fs::read_to_string(dir.join(VAULT_FILE_NAME)).unwrap();
        assert!(raw.contains("\"version\": 2"), "{raw}");
        let loaded = load(&dir).unwrap();
        assert_eq!(loaded.mcp_generation.get("docs"), Some(&1));
        assert_eq!(
            loaded.mcp_entries["docs"].secret.expose(),
            "mcp-test-secret"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn cleared_mcp_state_keeps_vault_version_two() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("vault");
        let mut file = VaultFile::default();
        file.mcp_entries
            .insert("docs".into(), entry("mcp-test-secret"));
        file.bump_mcp("docs");
        save(&dir, &mut file).unwrap();

        let fingerprint = file
            .mcp_entries
            .remove("docs")
            .map(|entry| entry.fingerprint);
        file.mcp_cleared.insert(
            "docs".into(),
            Tombstone {
                at: Utc::now(),
                fingerprint,
            },
        );
        file.bump_mcp("docs");
        save(&dir, &mut file).unwrap();

        let raw = fs::read_to_string(dir.join(VAULT_FILE_NAME)).unwrap();
        assert!(raw.contains("\"version\": 2"), "{raw}");
        assert!(load(&dir).unwrap().mcp_cleared.contains_key("docs"));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn legacy_version_one_provider_vault_survives_round_trip() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("vault");
        let mut file = VaultFile::default();
        file.version = 1;
        file.entries
            .insert(Slot::Openrouter, entry("sk-test-legacy"));
        save(&dir, &mut file).unwrap();

        let loaded = load(&dir).unwrap();
        assert_eq!(loaded.version, 1);
        assert_eq!(
            loaded.entries[&Slot::Openrouter].secret.expose(),
            "sk-test-legacy"
        );
        let mut loaded = loaded;
        save(&dir, &mut loaded).unwrap();
        let reloaded = load(&dir).unwrap();
        assert_eq!(reloaded.version, 1);
        assert_eq!(
            reloaded.entries[&Slot::Openrouter].secret.expose(),
            "sk-test-legacy"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn looser_file_mode_is_refused_with_typed_error_naming_path() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("vault");
        let mut file = VaultFile::default();
        save(&dir, &mut file).unwrap();
        let path = dir.join(VAULT_FILE_NAME);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        match load(&dir) {
            Err(VaultStoreError::LooseMode {
                path: reported,
                mode,
                required,
            }) => {
                assert_eq!(reported, path);
                assert_eq!(mode, 0o644);
                assert_eq!(required, 0o600);
            }
            other => panic!("expected LooseMode, got {other:?}"),
        }
        let message = load(&dir).unwrap_err().to_string();
        assert!(message.contains(&path.display().to_string()), "{message}");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn looser_dir_mode_is_refused_for_load_and_save() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("vault");
        let mut file = VaultFile::default();
        save(&dir, &mut file).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(
            load(&dir),
            Err(VaultStoreError::LooseMode {
                required: 0o700,
                ..
            })
        ));
        assert!(matches!(
            save(&dir, &mut VaultFile::default()),
            Err(VaultStoreError::LooseMode {
                required: 0o700,
                ..
            })
        ));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn symlinked_vault_file_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("vault");
        let mut file = VaultFile::default();
        save(&dir, &mut file).unwrap();
        let path = dir.join(VAULT_FILE_NAME);
        let target = root.path().join("elsewhere.json");
        fs::rename(&path, &target).unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(matches!(
            load(&dir),
            Err(VaultStoreError::WrongFileType { .. })
        ));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn malformed_file_error_does_not_echo_contents() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("vault");
        let mut file = VaultFile::default();
        save(&dir, &mut file).unwrap();
        let path = dir.join(VAULT_FILE_NAME);
        fs::write(&path, br#"{"entries": "sk-test-malformed-canary"}"#).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let message = load(&dir).unwrap_err().to_string();
        assert!(message.contains("malformed"), "{message}");
        assert!(!message.contains("sk-test-malformed-canary"), "{message}");
    }
}
