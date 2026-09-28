//! Provider key vault (#694 K1).
//!
//! Every consumer of a provider API key calls [`VaultHandle::resolve`] inside
//! rsid; nothing else reads credential env vars. Resolution order per slot:
//!
//! 1. a vault entry resolves to that entry;
//! 2. otherwise a tombstone resolves to `None` — a clear suppresses every
//!    fallback, including env and the generator;
//! 3. otherwise, when `vault.env_compat` is on (default `true`), the legacy env
//!    vars resolve;
//! 4. otherwise a dynamic source resolves where the slot has one (today only
//!    `bedrock`'s token-generator venv; generated tokens are never persisted);
//! 5. otherwise `None`.
//!
//! Threat model: keys never reach the environment of an agent-facing process
//! (see [`env_scrub`]), tool output, transcripts, logs, the DB or a read RPC.
//! Same-UID deliberate access (reading the 0600 file, a tokenless operator
//! RPC, `/proc/<pid>/environ` of the Codex fallback process — residual R1) is
//! out of scope and surfaced, not hidden.

pub mod check;
pub mod env_scrub;
pub mod operator;
pub mod secret;
pub mod slots;
pub mod store;

pub use check::{CredentialProbe, HttpCredentialProbe, ProbeOutcome};
pub use env_scrub::{inject_route_credential, scrub_credential_env, scrub_std_credential_env};
pub use secret::SecretString;
pub use slots::Slot;
pub use store::VaultStoreError;

use chrono::{DateTime, Utc};
use rsi_common::provider_credentials::{
    CliExposureConsumer, CliExposureReason, CredentialCheckClass, CredentialCheckMetadata,
    CredentialState, DEFAULT_CHECK_TTL_SECS, ImportProviderCredentialsResult, ImportSkipReason,
    ImportSkipped, ImportedCredential, ListProviderCredentialsResult, ProviderCredentialMetadata,
};
use rsi_common::types::SessionProvider;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use store::{StoredEntry, Tombstone, VaultFile};

/// Maximum accepted secret length (bytes).
pub const MAX_SECRET_BYTES: usize = 4096;

/// Live vault settings, shared with `RuntimeConfig` so `UpdateDaemonConfig`
/// takes effect on the next resolution.
#[derive(Debug)]
pub struct VaultSettings {
    pub env_compat: AtomicBool,
    pub check_ttl_secs: AtomicU64,
}

impl Default for VaultSettings {
    fn default() -> Self {
        Self {
            env_compat: AtomicBool::new(true),
            check_ttl_secs: AtomicU64::new(DEFAULT_CHECK_TTL_SECS),
        }
    }
}

impl VaultSettings {
    #[must_use]
    pub fn env_compat(&self) -> bool {
        self.env_compat.load(Ordering::Acquire)
    }

    #[must_use]
    pub fn check_ttl_secs(&self) -> u64 {
        self.check_ttl_secs.load(Ordering::Acquire)
    }
}

/// Where a resolved credential came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialSource {
    Vault,
    EnvCompat,
    Generator,
}

/// A resolved credential. Zeroized on drop; `Debug` is redacted.
#[derive(Clone, Debug)]
pub struct Resolved {
    pub secret: SecretString,
    pub source: CredentialSource,
    /// The legacy env var a `EnvCompat` value came from (a name only).
    pub env_var: Option<&'static str>,
}

/// A per-launch dynamic credential source. Never persisted.
pub trait DynamicSource: Send + Sync {
    /// Whether the slot has a dynamic source that could run (cheap; never
    /// generates).
    fn available(&self, slot: Slot) -> bool;
    /// Generate a credential; `None` when the slot has no dynamic source.
    fn generate(&self, slot: Slot) -> Option<Result<SecretString, String>>;
}

/// Production dynamic source: Bedrock's token-generator venv.
pub struct BedrockGenerator;

impl DynamicSource for BedrockGenerator {
    fn available(&self, slot: Slot) -> bool {
        slot == Slot::Bedrock && crate::bedrock::token_generator_available()
    }

    fn generate(&self, slot: Slot) -> Option<Result<SecretString, String>> {
        (slot == Slot::Bedrock && crate::bedrock::token_generator_available())
            .then(|| crate::bedrock::generate_token().map(SecretString::new))
    }
}

/// Probe that never touches the network (legacy handle).
struct NoProbe;

#[async_trait::async_trait]
impl CredentialProbe for NoProbe {
    async fn probe(&self, _slot: Slot, _secret: &SecretString) -> ProbeOutcome {
        ProbeOutcome::Unsupported
    }
}

type EnvReader = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;
type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

/// Typed vault mutation failures. None carries a secret.
#[derive(Debug, thiserror::Error)]
pub enum VaultError {
    #[error(transparent)]
    Store(#[from] VaultStoreError),
    #[error("provider credential is invalid: {0}")]
    InvalidSecret(&'static str),
    #[error("slot {0} has no vault entry to rotate; use SetProviderCredential")]
    NothingToRotate(Slot),
}

/// Launch-time admission refusal from an authoritative, unexpired check.
#[derive(Clone, Debug, PartialEq)]
pub struct CredentialRefusal {
    pub slot: Slot,
    pub check: CredentialCheckMetadata,
}

impl CredentialRefusal {
    pub const MESSAGE: &'static str = "provider_credential_refused";

    #[must_use]
    pub fn into_daemon_error(self) -> crate::error::DaemonError {
        let class = match self.check.class {
            CredentialCheckClass::Invalid => "invalid",
            CredentialCheckClass::Exhausted => "exhausted",
            CredentialCheckClass::Valid => "valid",
            CredentialCheckClass::Unknown => "unknown",
        };
        crate::error::DaemonError::StructuredRpc {
            rpc_code: rsi_common::rpc::INVALID_PARAMS,
            message: format!(
                "{}: slot {} credential is {class} ({})",
                Self::MESSAGE,
                self.slot,
                self.check.detail_code
            ),
            data: serde_json::json!({
                "kind": "provider_credential",
                "code": Self::MESSAGE,
                "slot": self.slot,
                "class": self.check.class,
                "detail_code": self.check.detail_code,
                "http_status": self.check.http_status,
                "fingerprint": self.check.fingerprint,
                "checked_at": self.check.at,
                "next_action": "set, rotate or top up the credential, then retry; CheckProviderCredential rechecks now",
            }),
        }
    }
}

struct Inner {
    dir: Option<PathBuf>,
    file: parking_lot::Mutex<VaultFile>,
    settings: Arc<VaultSettings>,
    env: EnvReader,
    dynamic: Arc<dyn DynamicSource>,
    probe: Arc<dyn CredentialProbe>,
    clock: Clock,
    flights: parking_lot::Mutex<HashMap<Slot, Arc<tokio::sync::Mutex<()>>>>,
    /// Last CLI key injection per slot since daemon start (never persisted).
    cli_exposures: parking_lot::Mutex<HashMap<Slot, DateTime<Utc>>>,
}

/// Cheaply clonable handle held on the daemon state.
#[derive(Clone)]
pub struct VaultHandle {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for VaultHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VaultHandle")
            .field("dir", &self.inner.dir)
            .finish_non_exhaustive()
    }
}

/// Builder for production and test handles.
pub struct VaultHandleBuilder {
    dir: Option<PathBuf>,
    settings: Arc<VaultSettings>,
    env: EnvReader,
    dynamic: Arc<dyn DynamicSource>,
    probe: Arc<dyn CredentialProbe>,
    clock: Clock,
}

impl VaultHandleBuilder {
    #[must_use]
    pub fn new(settings: Arc<VaultSettings>) -> Self {
        Self {
            dir: None,
            settings,
            env: Arc::new(|name| std::env::var(name).ok()),
            dynamic: Arc::new(BedrockGenerator),
            probe: Arc::new(HttpCredentialProbe::default()),
            clock: Arc::new(Utc::now),
        }
    }

    #[must_use]
    pub fn dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.dir = Some(dir.into());
        self
    }

    #[must_use]
    pub fn env(mut self, env: impl Fn(&str) -> Option<String> + Send + Sync + 'static) -> Self {
        self.env = Arc::new(env);
        self
    }

    #[must_use]
    pub fn dynamic(mut self, dynamic: Arc<dyn DynamicSource>) -> Self {
        self.dynamic = dynamic;
        self
    }

    #[must_use]
    pub fn probe(mut self, probe: Arc<dyn CredentialProbe>) -> Self {
        self.probe = probe;
        self
    }

    #[must_use]
    pub fn clock(mut self, clock: impl Fn() -> DateTime<Utc> + Send + Sync + 'static) -> Self {
        self.clock = Arc::new(clock);
        self
    }

    /// Validate modes and load the vault file (when a directory is set).
    ///
    /// # Errors
    ///
    /// Returns a typed [`VaultStoreError`] naming the path for a looser mode, a symlink, malformed contents or I/O failure.
    pub fn open(self) -> Result<VaultHandle, VaultStoreError> {
        let file = match &self.dir {
            Some(dir) => store::load(dir)?,
            None => VaultFile::default(),
        };
        Ok(VaultHandle {
            inner: Arc::new(Inner {
                dir: self.dir,
                file: parking_lot::Mutex::new(file),
                settings: self.settings,
                env: self.env,
                dynamic: self.dynamic,
                probe: self.probe,
                clock: self.clock,
                flights: parking_lot::Mutex::new(HashMap::new()),
                cli_exposures: parking_lot::Mutex::new(HashMap::new()),
            }),
        })
    }
}

static GLOBAL: OnceLock<VaultHandle> = OnceLock::new();
static LEGACY: OnceLock<VaultHandle> = OnceLock::new();

/// Install the daemon's vault. Called once at startup after the mode check.
///
/// # Errors
///
/// Returns the rejected handle when a vault is already installed.
pub fn install_global(handle: VaultHandle) -> Result<(), VaultHandle> {
    GLOBAL.set(handle)
}

/// The daemon's vault. Before (or without) [`install_global`] — unit tests,
/// auxiliary binaries — this is an env-only, file-less, check-less handle,
/// i.e. exactly the pre-vault behaviour.
#[must_use]
pub fn global() -> VaultHandle {
    if let Some(handle) = GLOBAL.get() {
        return handle.clone();
    }
    LEGACY
        .get_or_init(|| {
            VaultHandleBuilder::new(Arc::new(VaultSettings::default()))
                .probe(Arc::new(NoProbe))
                .open()
                .unwrap_or_else(|_| unreachable!("a file-less vault cannot fail to open"))
        })
        .clone()
}

/// Open the production vault at `~/.rsi/vault`, refusing looser modes.
///
/// # Errors
///
/// See [`VaultHandleBuilder::open`].
pub fn open_default(settings: Arc<VaultSettings>) -> Result<VaultHandle, VaultStoreError> {
    VaultHandleBuilder::new(settings)
        .dir(store::default_vault_dir())
        .open()
}

fn validate_secret(secret: &str) -> Result<String, VaultError> {
    let trimmed = secret.trim();
    if trimmed.is_empty() {
        return Err(VaultError::InvalidSecret("empty"));
    }
    if trimmed.len() > MAX_SECRET_BYTES {
        return Err(VaultError::InvalidSecret("longer than 4096 bytes"));
    }
    if trimmed
        .chars()
        .any(|ch| ch.is_whitespace() || ch.is_control())
    {
        return Err(VaultError::InvalidSecret(
            "contains whitespace or control characters",
        ));
    }
    Ok(trimmed.to_string())
}

/// Slots with a defined validity/credit check endpoint.
#[must_use]
pub const fn has_check_endpoint(slot: Slot) -> bool {
    matches!(
        slot,
        Slot::Openrouter | Slot::Anthropic | Slot::Openai | Slot::Bedrock
    )
}

impl VaultHandle {
    #[must_use]
    pub fn settings(&self) -> &Arc<VaultSettings> {
        &self.inner.settings
    }

    #[must_use]
    pub fn dir(&self) -> Option<&Path> {
        self.inner.dir.as_deref()
    }

    fn now(&self) -> DateTime<Utc> {
        (self.inner.clock)()
    }

    fn env_value(&self, slot: Slot) -> Option<(&'static str, String)> {
        slots::env_vars(slot).iter().find_map(|name| {
            (self.inner.env)(name)
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .map(|value| (*name, value))
        })
    }

    /// Steps 1-3 of the resolution order (no dynamic generation).
    fn resolve_static(&self, slot: Slot) -> Result<Option<Resolved>, ()> {
        let file = self.inner.file.lock();
        if let Some(entry) = file.entries.get(&slot) {
            let env_var = entry.source_env_var.as_deref().and_then(|name| {
                slots::env_vars(slot)
                    .iter()
                    .copied()
                    .find(|known| *known == name)
            });
            return Ok(Some(Resolved {
                secret: entry.secret.clone(),
                source: CredentialSource::Vault,
                env_var,
            }));
        }
        if file.cleared.contains_key(&slot) {
            // Tombstone: suppress every fallback.
            return Err(());
        }
        drop(file);
        if self.inner.settings.env_compat()
            && let Some((name, value)) = self.env_value(slot)
        {
            return Ok(Some(Resolved {
                secret: SecretString::new(value),
                source: CredentialSource::EnvCompat,
                env_var: Some(name),
            }));
        }
        Ok(None)
    }

    /// Resolve `slot` using the documented five-step order. `Err` only when a
    /// dynamic source exists but failed to generate.
    ///
    /// # Errors
    ///
    /// Returns the generator failure message when the slot's dynamic source fails.
    pub fn resolve(&self, slot: Slot) -> Result<Option<Resolved>, String> {
        match self.resolve_static(slot) {
            Err(()) => Ok(None),
            Ok(Some(resolved)) => Ok(Some(resolved)),
            Ok(None) => match self.inner.dynamic.generate(slot) {
                None => Ok(None),
                Some(Ok(secret)) => Ok(Some(Resolved {
                    secret,
                    source: CredentialSource::Generator,
                    env_var: None,
                })),
                Some(Err(error)) => Err(error),
            },
        }
    }

    /// Harness consumers additionally honour the generic legacy fallbacks
    /// (`RSI_API_KEY`, ...) after the slot, exactly as before, unless the
    /// slot is cleared or env compatibility is off. `slot = None` is a
    /// keyless route (local server) that historically still read them.
    #[must_use]
    pub fn resolve_with_generic_fallback(&self, slot: Option<Slot>) -> Option<Resolved> {
        if let Some(slot) = slot {
            match self.resolve_static(slot) {
                Err(()) => return None,
                Ok(Some(resolved)) => return Some(resolved),
                Ok(None) => {}
            }
        }
        if !self.inner.settings.env_compat() {
            return None;
        }
        slots::GENERIC_FALLBACK_ENV_VARS.iter().find_map(|name| {
            (self.inner.env)(name)
                .filter(|value| !value.is_empty())
                .map(|value| Resolved {
                    secret: SecretString::new(value),
                    source: CredentialSource::EnvCompat,
                    env_var: Some(*name),
                })
        })
    }

    /// Current state per the resolution order, without running a generator.
    #[must_use]
    pub fn state(&self, slot: Slot) -> CredentialState {
        match self.resolve_static(slot) {
            Err(()) => CredentialState::Cleared,
            Ok(Some(resolved)) => match resolved.source {
                CredentialSource::Vault => CredentialState::Vault,
                _ => CredentialState::EnvCompat,
            },
            Ok(None) if self.inner.dynamic.available(slot) => CredentialState::Generator,
            Ok(None) => CredentialState::Absent,
        }
    }

    /// Cheap availability probe: would [`Self::resolve`] find a credential?
    #[must_use]
    pub fn resolvable(&self, slot: Slot) -> bool {
        matches!(
            self.state(slot),
            CredentialState::Vault | CredentialState::EnvCompat | CredentialState::Generator
        )
    }

    fn persist(&self, file: &VaultFile, checks_only: bool) -> Result<(), VaultStoreError> {
        let Some(dir) = &self.inner.dir else {
            return Ok(());
        };
        // A lazy check result alone never creates the vault on disk.
        if checks_only && !dir.join(store::VAULT_FILE_NAME).exists() {
            return Ok(());
        }
        store::save(dir, file)
    }

    fn mutate<T>(
        &self,
        apply: impl FnOnce(&mut VaultFile) -> Result<T, VaultError>,
    ) -> Result<T, VaultError> {
        // The lock is held across persist so concurrent mutations serialize
        // and memory never runs ahead of disk.
        let mut guard = self.inner.file.lock();
        let mut next = guard.clone();
        let value = apply(&mut next)?;
        self.persist(&next, false)?;
        *guard = next;
        drop(guard);
        Ok(value)
    }

    /// `SetProviderCredential`: store (or replace) the entry, remove any
    /// tombstone, invalidate the check. Returns the new fingerprint.
    ///
    /// # Errors
    ///
    /// Returns [`VaultError`] for an invalid secret or a persistence failure.
    pub fn set(&self, slot: Slot, secret: &str) -> Result<String, VaultError> {
        let value = validate_secret(secret)?;
        let now = self.now();
        self.mutate(|file| {
            let secret = SecretString::new(value);
            let fingerprint = secret.fingerprint();
            file.entries.insert(
                slot,
                StoredEntry {
                    secret,
                    fingerprint: fingerprint.clone(),
                    set_at: now,
                    rotated_from_fingerprint: None,
                    source_env_var: None,
                },
            );
            file.cleared.remove(&slot);
            file.bump(slot);
            Ok(fingerprint)
        })
    }

    /// `RotateProviderCredential`: replace an existing entry, recording the
    /// previous fingerprint.
    ///
    /// # Errors
    ///
    /// Returns [`VaultError::NothingToRotate`] without an entry, or an invalid-secret/persistence error.
    pub fn rotate(&self, slot: Slot, secret: &str) -> Result<String, VaultError> {
        let value = validate_secret(secret)?;
        let now = self.now();
        self.mutate(|file| {
            let previous = file
                .entries
                .get(&slot)
                .map(|entry| entry.fingerprint.clone())
                .ok_or(VaultError::NothingToRotate(slot))?;
            let secret = SecretString::new(value);
            let fingerprint = secret.fingerprint();
            file.entries.insert(
                slot,
                StoredEntry {
                    secret,
                    fingerprint: fingerprint.clone(),
                    set_at: now,
                    rotated_from_fingerprint: Some(previous),
                    source_env_var: None,
                },
            );
            file.cleared.remove(&slot);
            file.bump(slot);
            Ok(fingerprint)
        })
    }

    /// `ClearProviderCredential`: remove the entry and write a durable
    /// tombstone that suppresses env and generator fallbacks.
    ///
    /// # Errors
    ///
    /// Returns [`VaultError::Store`] when the tombstone cannot be persisted.
    pub fn clear(&self, slot: Slot) -> Result<(), VaultError> {
        let now = self.now();
        let env_fingerprint = self
            .env_value(slot)
            .map(|(_, value)| secret::fingerprint(&value));
        self.mutate(|file| {
            let fingerprint = file
                .entries
                .remove(&slot)
                .map(|entry| entry.fingerprint)
                .or(env_fingerprint);
            file.cleared.insert(
                slot,
                Tombstone {
                    at: now,
                    fingerprint,
                },
            );
            file.bump(slot);
            Ok(())
        })
    }

    /// `ImportProviderCredentialsFromEnv`: explicit copy of static env keys.
    /// Skips slots with a vault entry or a tombstone; never runs a generator.
    ///
    /// # Errors
    ///
    /// Returns [`VaultError::Store`] when the imported entries cannot be persisted.
    pub fn import_from_env(&self) -> Result<ImportProviderCredentialsResult, VaultError> {
        let now = self.now();
        self.mutate(|file| {
            let mut result = ImportProviderCredentialsResult {
                imported: Vec::new(),
                skipped: Vec::new(),
            };
            for slot in Slot::ALL {
                let reason = if file.entries.contains_key(&slot) {
                    Some(ImportSkipReason::PresentInVault)
                } else if file.cleared.contains_key(&slot) {
                    Some(ImportSkipReason::Cleared)
                } else {
                    None
                };
                if let Some(reason) = reason {
                    result.skipped.push(ImportSkipped { slot, reason });
                    continue;
                }
                let Some((name, value)) = self.env_value(slot) else {
                    result.skipped.push(ImportSkipped {
                        slot,
                        reason: ImportSkipReason::AbsentFromEnv,
                    });
                    continue;
                };
                let Ok(value) = validate_secret(&value) else {
                    result.skipped.push(ImportSkipped {
                        slot,
                        reason: ImportSkipReason::AbsentFromEnv,
                    });
                    continue;
                };
                let secret = SecretString::new(value);
                let fingerprint = secret.fingerprint();
                file.entries.insert(
                    slot,
                    StoredEntry {
                        secret,
                        fingerprint: fingerprint.clone(),
                        set_at: now,
                        rotated_from_fingerprint: None,
                        source_env_var: Some(name.to_string()),
                    },
                );
                file.bump(slot);
                result.imported.push(ImportedCredential {
                    slot,
                    fingerprint,
                    env_var: name.to_string(),
                });
            }
            Ok(result)
        })
    }

    /// Secret-free metadata for one slot.
    #[must_use]
    pub fn metadata(&self, slot: Slot) -> ProviderCredentialMetadata {
        let state = self.state(slot);
        let file = self.inner.file.lock();
        let entry = file.entries.get(&slot);
        let tombstone = file.cleared.get(&slot);
        let check = file.checks.get(&slot).cloned();
        let (entry_fp, set_at, rotated_from) = entry.map_or((None, None, None), |entry| {
            (
                Some(entry.fingerprint.clone()),
                Some(entry.set_at),
                entry.rotated_from_fingerprint.clone(),
            )
        });
        let tombstone_fp = tombstone.and_then(|tombstone| tombstone.fingerprint.clone());
        let cleared_at = tombstone.map(|tombstone| tombstone.at);
        let generation = file.generation_of(slot);
        drop(file);
        let last_cli_exposure_at = self.inner.cli_exposures.lock().get(&slot).copied();
        let fingerprint = match state {
            CredentialState::Vault => entry_fp,
            CredentialState::EnvCompat => self
                .env_value(slot)
                .map(|(_, value)| secret::fingerprint(&value)),
            CredentialState::Cleared => tombstone_fp,
            CredentialState::Generator | CredentialState::Absent => None,
        };
        ProviderCredentialMetadata {
            slot,
            state,
            fingerprint,
            set_at,
            rotated_from_fingerprint: rotated_from,
            cleared_at,
            check,
            generation,
            route: slots::route(slot),
            cli_exposure: slots::cli_exposure(slot),
            last_cli_exposure_at,
        }
    }

    /// `ListProviderCredentials`: every slot, never a secret.
    #[must_use]
    pub fn list(&self) -> ListProviderCredentialsResult {
        ListProviderCredentialsResult {
            env_compat: self.inner.settings.env_compat(),
            check_ttl_secs: self.inner.settings.check_ttl_secs(),
            credentials: Slot::ALL
                .into_iter()
                .map(|slot| self.metadata(slot))
                .collect(),
        }
    }

    fn check_fresh(
        &self,
        check: &CredentialCheckMetadata,
        fingerprint: &str,
        now: DateTime<Utc>,
    ) -> bool {
        if check.fingerprint.as_deref() != Some(fingerprint) {
            return false;
        }
        let age = now.signed_duration_since(check.at).num_seconds();
        let lifetime = match check.class {
            CredentialCheckClass::Unknown => check::TRANSIENT_RECHECK_SECS,
            _ => i64::try_from(self.inner.settings.check_ttl_secs()).unwrap_or(i64::MAX),
        };
        (0..lifetime).contains(&age)
    }

    fn flight(&self, slot: Slot) -> Arc<tokio::sync::Mutex<()>> {
        Arc::clone(self.inner.flights.lock().entry(slot).or_default())
    }

    /// Commit a check result only if the slot's generation is unchanged
    /// since the check started (compare-and-set under the vault lock). A
    /// check of a secret replaced or cleared mid-flight is discarded.
    fn record_check(&self, slot: Slot, check: &CredentialCheckMetadata) -> bool {
        let mut guard = self.inner.file.lock();
        if guard.generation_of(slot) != check.generation {
            drop(guard);
            tracing::info!(
                event = "credential_check_discarded",
                slot = %slot,
                started_generation = check.generation,
                fingerprint = ?check.fingerprint,
                "stale provider credential check discarded"
            );
            return false;
        }
        let mut next = guard.clone();
        next.checks.insert(slot, check.clone());
        if let Err(error) = self.persist(&next, true) {
            tracing::warn!(slot = %slot, error = %error, "vault check result not persisted");
        }
        *guard = next;
        drop(guard);
        true
    }

    /// Capture `(generation, statically resolved credential)`. The
    /// generation is read first, so a concurrent mutation can only make the
    /// later compare-and-set fail, never commit an old key's result under
    /// the new generation.
    fn check_target(&self, slot: Slot) -> Option<(u64, Resolved)> {
        let generation = self.inner.file.lock().generation_of(slot);
        match self.resolve_static(slot) {
            Ok(Some(resolved)) => Some((generation, resolved)),
            _ => None,
        }
    }

    async fn run_probe(
        &self,
        slot: Slot,
        generation: u64,
        resolved: &Resolved,
    ) -> CredentialCheckMetadata {
        let outcome = tokio::time::timeout(
            check::PROBE_TIMEOUT,
            self.inner.probe.probe(slot, &resolved.secret),
        )
        .await
        .unwrap_or(ProbeOutcome::Timeout);
        let mut check = check::classify(
            slot,
            &outcome,
            self.now(),
            Some(&resolved.secret.fingerprint()),
        );
        check.generation = generation;
        tracing::info!(
            event = "credential_check",
            slot = %slot,
            class = ?check.class,
            detail_code = %check.detail_code,
            fingerprint = ?check.fingerprint,
            "provider credential checked"
        );
        check
    }

    /// Lazy launch-time check: single-flight per slot, 5 s timeout, only when
    /// the stored check is missing, for another credential, or expired
    /// (authoritative/valid after `vault.check_ttl_secs`, `unknown` after
    /// 60 s). Generator-sourced and endpoint-less slots are not checked.
    pub async fn ensure_fresh_check(&self, slot: Slot) {
        if !has_check_endpoint(slot) {
            return;
        }
        let Some((generation, resolved)) = self.check_target(slot) else {
            return;
        };
        let fingerprint = resolved.secret.fingerprint();
        let is_fresh = |handle: &Self| {
            handle
                .inner
                .file
                .lock()
                .checks
                .get(&slot)
                .is_some_and(|check| handle.check_fresh(check, &fingerprint, handle.now()))
        };
        if is_fresh(self) {
            return;
        }
        let flight = self.flight(slot);
        let _guard = flight.lock().await;
        if is_fresh(self) {
            return;
        }
        let check = self.run_probe(slot, generation, &resolved).await;
        self.record_check(slot, &check);
    }

    /// `CheckProviderCredential` (and Set/Rotate): probe now, bypassing
    /// freshness but still single-flight. `None` when nothing resolves
    /// statically, or when the slot changed while the probe was in flight
    /// (the stale result is discarded).
    pub async fn check_now(&self, slot: Slot) -> Option<CredentialCheckMetadata> {
        let (generation, resolved) = self.check_target(slot)?;
        if !has_check_endpoint(slot) {
            let mut check = check::classify(
                slot,
                &ProbeOutcome::Unsupported,
                self.now(),
                Some(&resolved.secret.fingerprint()),
            );
            check.generation = generation;
            return Some(check);
        }
        let flight = self.flight(slot);
        let _guard = flight.lock().await;
        let check = self.run_probe(slot, generation, &resolved).await;
        self.record_check(slot, &check).then_some(check)
    }

    /// Synchronous admission from the stored check: refuse only on a fresh
    /// authoritative `invalid`/`exhausted` for the current credential.
    ///
    /// # Errors
    ///
    /// Returns [`CredentialRefusal`] for a fresh authoritative `invalid`/`exhausted` check.
    pub fn admission(&self, slot: Slot) -> Result<(), CredentialRefusal> {
        let Ok(Some(resolved)) = self.resolve_static(slot) else {
            return Ok(());
        };
        let fingerprint = resolved.secret.fingerprint();
        let now = self.now();
        let file = self.inner.file.lock();
        match file.checks.get(&slot) {
            Some(check)
                if check.class.is_authoritative_refusal()
                    && self.check_fresh(check, &fingerprint, now) =>
            {
                Err(CredentialRefusal {
                    slot,
                    check: check.clone(),
                })
            }
            _ => Ok(()),
        }
    }

    /// Inject `slot`'s key into a CLI child as its single route credential
    /// (plus the Codex tool-shell exclude), record `last_cli_exposure_at`,
    /// and emit `credential_cli_exposure {slot, fingerprint, consumer,
    /// reason}`. The secret itself is never logged. Every CLI key injection
    /// goes through here.
    pub fn inject_cli_credential(
        &self,
        cmd: &mut tokio::process::Command,
        slot: Slot,
        env_var: &'static str,
        secret: &SecretString,
        consumer: CliExposureConsumer,
        reason: CliExposureReason,
    ) {
        env_scrub::inject_route_credential(cmd, env_var, secret, true);
        let at = self.now();
        self.inner.cli_exposures.lock().insert(slot, at);
        tracing::info!(
            event = "credential_cli_exposure",
            slot = %slot,
            fingerprint = %secret.fingerprint(),
            consumer = ?consumer,
            reason = ?reason,
            env_var,
            "provider key injected into a CLI process environment (R1)"
        );
    }

    /// Lazy check then admission: the full launch-time gate.
    ///
    /// # Errors
    ///
    /// See [`Self::admission`].
    pub async fn admit_launch(&self, slot: Slot) -> Result<(), CredentialRefusal> {
        self.ensure_fresh_check(slot).await;
        self.admission(slot)
    }

    /// Admission for the synchronous spawn chokepoint: decide from the
    /// stored check and, when it is stale, refresh it in the background so
    /// the next launch sees a current result (a stale check admits).
    ///
    /// # Errors
    ///
    /// See [`Self::admission`].
    pub fn admission_with_background_refresh(&self, slot: Slot) -> Result<(), CredentialRefusal> {
        let result = self.admission(slot);
        if result.is_ok()
            && has_check_endpoint(slot)
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            let handle = self.clone();
            runtime.spawn(async move { handle.ensure_fresh_check(slot).await });
        }
        result
    }
}

/// The vault slot a launch authenticates with, if any.
///
/// Codex-launched API providers use their slot; the Harness uses the model's
/// slot unless the launch supplies an explicit base URL or explicit key.
#[must_use]
pub fn launch_slot(
    provider: SessionProvider,
    config: &crate::claude::LaunchConfig,
) -> Option<Slot> {
    match provider {
        SessionProvider::Harness => {
            if config.openai_base_url.is_some()
                || config
                    .openai_api_key
                    .as_deref()
                    .is_some_and(|key| !key.is_empty())
            {
                return None;
            }
            slots::slot_for_harness_model(config.model.as_deref().unwrap_or("claude-sonnet-5"))
        }
        provider => slots::slot_for_provider(provider),
    }
}

/// Launch-time credential admission for the synchronous spawn chokepoint.
///
/// Refuses (typed, before any provider process exists) only on a fresh
/// authoritative `invalid`/`exhausted` check for the current credential.
///
/// # Errors
///
/// Returns the typed `provider_credential_refused` refusal.
pub fn admit_provider_spawn(
    provider: SessionProvider,
    config: &crate::claude::LaunchConfig,
) -> crate::error::Result<()> {
    launch_slot(provider, config).map_or(Ok(()), |slot| {
        global()
            .admission_with_background_refresh(slot)
            .map_err(CredentialRefusal::into_daemon_error)
    })
}

/// Async launch warm-up: run the lazy single-flight check (5 s timeout) so
/// the synchronous chokepoint decides on a current result. Never refuses.
pub async fn warm_launch_check(config: &crate::claude::LaunchConfig) {
    if let Some(slot) = config
        .provider
        .and_then(|provider| launch_slot(provider, config))
    {
        global().ensure_fresh_check(slot).await;
    }
}

/// Provider availability: credential resolvable AND (route = harness OR the
/// Codex CLI is present). Slice R extends [`slots::route`].
#[must_use]
pub fn provider_available(vault: &VaultHandle, slot: Slot, codex_present: bool) -> bool {
    use rsi_common::provider_credentials::CredentialRoute;
    vault.resolvable(slot)
        && match slots::route(slot) {
            CredentialRoute::Harness => true,
            CredentialRoute::CodexCli => codex_present,
        }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;
