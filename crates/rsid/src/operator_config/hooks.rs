//! `ListClaudeHooks` / `PutClaudeHooks` / `DeleteClaudeHooks`: the `hooks` block of
//! `~/.claude/settings.json` (hub #3039, formerly the TUI's own file write).
//!
//! Round-trip preservation: every top-level key other than `hooks` survives a
//! put unchanged (`ClaudeSettingsFile::other`, the TUI's `ClaudeSettings`
//! shape, so the written bytes match what the TUI wrote). The revision covers
//! the file bytes and its modification time, which is the TUI's Q7
//! save-conflict check: a put whose `expected_revision` is stale is refused
//! `claude_hooks_conflict` with the current revision, and the caller decides
//! to overwrite (put again with that revision), reload or cancel.

use crate::error::{DaemonError, Result};
use rsi_common::claude_hooks::ClaudeHooks;
use rsi_common::operator_config::{
    CLAUDE_HOOKS_ABSENT_REVISION, CLAUDE_HOOKS_CONFLICT, ClaudeHooksView, DeleteClaudeHooksParams,
    PutClaudeHooksParams,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::path::Path;

/// On-disk shape: `hooks` typed, everything else verbatim.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ClaudeSettingsFile {
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    hooks: ClaudeHooks,
    #[serde(flatten)]
    other: Map<String, Value>,
}

struct Loaded {
    data: ClaudeSettingsFile,
    exists: bool,
    revision: String,
}

/// Serializes this daemon's own read-check-write so two operator puts cannot
/// interleave between the revision check and the rename.
static PUT_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

fn revision_of(bytes: &[u8], modified: Option<std::time::SystemTime>) -> String {
    let mut hasher = Sha256::new();
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
    let nanos = modified
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |elapsed| elapsed.as_nanos());
    hasher.update(nanos.to_le_bytes());
    let digest = hasher.finalize();
    digest
        .iter()
        .take(16)
        .fold(String::with_capacity(32), |mut out, byte| {
            use std::fmt::Write as _;
            let _ = write!(out, "{byte:02x}");
            out
        })
}

fn load(path: &Path) -> Result<Loaded> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Loaded {
                data: ClaudeSettingsFile::default(),
                exists: false,
                revision: CLAUDE_HOOKS_ABSENT_REVISION.to_owned(),
            });
        }
        Err(error) => {
            return Err(DaemonError::Store(format!(
                "could not read {}: {:?}",
                path.display(),
                error.kind()
            )));
        }
    };
    let modified = std::fs::metadata(path)
        .ok()
        .and_then(|metadata| metadata.modified().ok());
    let data: ClaudeSettingsFile = serde_json::from_slice(&bytes).map_err(|error| {
        // serde_json errors carry a category and position, never content.
        DaemonError::Store(format!(
            "{} is not a valid Claude settings file ({:?} at line {} column {})",
            path.display(),
            error.classify(),
            error.line(),
            error.column()
        ))
    })?;
    Ok(Loaded {
        data,
        exists: true,
        revision: revision_of(&bytes, modified),
    })
}

fn view(path: &Path, loaded: Loaded) -> ClaudeHooksView {
    ClaudeHooksView {
        path: path.display().to_string(),
        exists: loaded.exists,
        revision: loaded.revision,
        hooks: loaded.data.hooks,
    }
}

fn validate(hooks: &ClaudeHooks) -> Result<()> {
    for (event, entries) in hooks {
        let label = event.label();
        if label.trim().is_empty() {
            return Err(DaemonError::InvalidParam(
                "hook event names must be non-empty".into(),
            ));
        }
        for entry in entries {
            if entry.hooks.is_empty() {
                return Err(DaemonError::InvalidParam(format!(
                    "a {label} hook entry has no commands"
                )));
            }
            for command in &entry.hooks {
                if command.kind.trim().is_empty() || command.command.trim().is_empty() {
                    return Err(DaemonError::InvalidParam(format!(
                        "a {label} hook needs a type and a command"
                    )));
                }
                if command.timeout == 0 {
                    return Err(DaemonError::InvalidParam(format!(
                        "a {label} hook timeout must be a positive number of seconds"
                    )));
                }
            }
        }
    }
    Ok(())
}

/// `ListClaudeHooks`.
///
/// # Errors
///
/// `Store` when the file cannot be read or is not valid JSON.
pub fn list(path: &Path) -> Result<ClaudeHooksView> {
    Ok(view(path, load(path)?))
}

/// Refuse a stale `expected_revision` with the typed conflict.
fn check_revision(path: &Path, current: &Loaded, expected: &str) -> Result<()> {
    if current.revision == expected {
        return Ok(());
    }
    Err(DaemonError::StructuredRpc {
        rpc_code: rsi_common::rpc::INVALID_PARAMS,
        message: format!(
            "{CLAUDE_HOOKS_CONFLICT}: {} changed since it was read; reload, or put again with current_revision to overwrite",
            path.display()
        ),
        data: serde_json::json!({
            "code": CLAUDE_HOOKS_CONFLICT,
            "current_revision": current.revision,
            "path": path.display().to_string(),
        }),
    })
}

fn write_settings(path: &Path, data: &ClaudeSettingsFile) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(data).map_err(DaemonError::from)?;
    super::write_atomic(path, &bytes).map_err(|error| {
        DaemonError::Store(format!(
            "could not write {}: {:?}",
            path.display(),
            error.kind()
        ))
    })
}

/// `PutClaudeHooks`.
///
/// # Errors
///
/// `InvalidParam` for an invalid hook, the typed `claude_hooks_conflict` for
/// a stale revision, `Store` for a read or write failure.
pub fn put(path: &Path, request: PutClaudeHooksParams) -> Result<ClaudeHooksView> {
    validate(&request.hooks)?;
    let _guard = PUT_LOCK.lock();
    let current = load(path)?;
    check_revision(path, &current, &request.expected_revision)?;
    let mut data = current.data;
    data.hooks = request.hooks;
    write_settings(path, &data)?;
    list(path)
}

/// `DeleteClaudeHooks`: remove the `hooks` key from the settings file. Every
/// other top-level key is kept. An absent file stays absent (nothing to
/// delete, no file is created); a file with no hooks is rewritten without the
/// key, so the call is idempotent.
///
/// # Errors
///
/// The typed `claude_hooks_conflict` for a stale revision, `Store` for a read
/// or write failure.
pub fn delete(path: &Path, request: DeleteClaudeHooksParams) -> Result<ClaudeHooksView> {
    let _guard = PUT_LOCK.lock();
    let current = load(path)?;
    check_revision(path, &current, &request.expected_revision)?;
    if !current.exists {
        return Ok(view(path, current));
    }
    let mut data = current.data;
    data.hooks = ClaudeHooks::default();
    write_settings(path, &data)?;
    list(path)
}
