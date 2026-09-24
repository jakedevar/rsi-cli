//! Centralized project identity constants and path helpers.
//!
//! Every runtime-visible project name reference flows through this module.
//! To rename the project, change the constants here.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;

/// The hidden directory name under `$HOME` for all runtime data.
pub const PROJECT_DIR_NAME: &str = ".rsi";

/// The per-project configuration filename.
pub const PROJECT_CONFIG_FILENAME: &str = "RSI.md";

/// The daemon binary name (used when TUI auto-starts the daemon).
pub const DAEMON_BINARY: &str = "rsid";

/// The SQLite database filename.
pub const DB_FILENAME: &str = "rsi.db";

/// Prefix for `/tmp` fallback paths when `$HOME` is unavailable.
pub const FALLBACK_PREFIX: &str = "rsi";

/// Environment variable for the daemon socket path.
pub const ENV_SOCKET: &str = "RSI_SOCKET";

/// Environment variable for an isolated daemon socket override (RSI-006 +
/// V1.1 verifier-daemon pattern). Set this in tests, eval-driver, and
/// pipeline-verify shells so non-TUI binaries never accidentally hit
/// `~/.rsi/daemon.sock`.
pub const ENV_DAEMON_SOCKET_PATH: &str = "RSI_DAEMON_SOCKET_PATH";

/// Environment variable for the session ID (injected into provider subprocesses).
pub const ENV_SESSION_ID: &str = "RSI_SESSION_ID";

/// Non-secret durable model invocation correlation for structured producers.
/// This value grants no authority; daemon import re-derives it from SQLite.
pub const ENV_MODEL_INVOCATION_ID: &str = "RSI_MODEL_INVOCATION_ID";

/// Non-secret ownership namespace shared by every external process launched
/// by one daemon socket/database domain.
///
/// Unlike Session and model-invocation IDs, this value is not copied from
/// SQLite and therefore prevents a copied database with overlapping UUIDs from
/// granting one daemon authority over another daemon's children. It is an
/// ownership marker, not an authentication credential.
pub const ENV_PROCESS_OWNERSHIP_NAMESPACE: &str = "RSI_PROCESS_OWNERSHIP_NAMESPACE";

static PROCESS_OWNERSHIP_NAMESPACE: OnceLock<String> = OnceLock::new();

/// Environment variable for the per-session authority token (P0 attribution
/// gate). Minted by the daemon at launch, injected into provider
/// subprocesses, and read by `rsi-rpc` to attach `RpcRequest.session_token`.
/// Never typed into `--params` — it must ride the transport-only field so it
/// never lands in persisted request params.
pub const ENV_SESSION_TOKEN: &str = "RSI_SESSION_TOKEN";

/// Environment variable for the pipeline-agent role.
///
/// Read by `tools/git-hooks/pre-commit` to gate branch/path invariants.
/// Derived from `SessionKind` via `rsid`'s `claude_agent_role_for_kind` and
/// stamped into the provider subprocess env at launch, mirroring
/// `ENV_SESSION_ID`/`ENV_SOCKET`/`ENV_SESSION_TOKEN`. Only implementer kinds
/// are stamped (`pipeline-implement`, "never commit on main"); the hook's
/// `pipeline-research`/`pipeline-plan` arms assume the standalone RPI
/// pipeline's main-only flow and are not stamped for rsi sessions — see
/// `claude_agent_role_for_kind`. Empty/unset means a human commit — the hook
/// no-ops.
pub const ENV_CLAUDE_AGENT_ROLE: &str = "CLAUDE_AGENT_ROLE";

/// Environment variable for the per-session cargo build scratch directory
/// (issue #25). Not an RSI-invented variable — this is cargo's own
/// `CARGO_TARGET_DIR`, stamped into sandboxed provider subprocess envs so
/// every `cargo` invocation a worker runs lands its build artifacts inside
/// the session's sandbox root (disk-backed, reclaimed with the sandbox)
/// instead of an improvised `/tmp` path on a size-limited tmpfs. Tmpfs
/// exhaustion surfaces as spurious `SQLITE_IOERR_WRITE` test failures that
/// masquerade as real defects — placement is the durable fix.
pub const ENV_CARGO_TARGET_DIR: &str = "CARGO_TARGET_DIR";

/// Environment variable for a session's daemon-authenticated temporary
/// directory. Sandboxed provider and Harness processes receive this alongside
/// [`ENV_CARGO_TARGET_DIR`]; non-sandboxed launches retain their ambient value.
pub const ENV_TMPDIR: &str = "TMPDIR";

/// Directory name (under a session's sandbox root) that the daemon
/// designates as the session's cargo build scratch via
/// [`ENV_CARGO_TARGET_DIR`]. Kept as a named constant so the launch-time
/// stamping and the terminal build-cache reclamation sweep can never drift
/// apart on the path they own.
pub const SANDBOX_BUILD_CACHE_DIR_NAME: &str = "target";

/// Resolve the project data directory (`~/.rsi/`).
///
/// Falls back to `/tmp` if `$HOME` is unavailable.
pub fn data_dir() -> PathBuf {
    dirs::home_dir()
        .map(|h| h.join(PROJECT_DIR_NAME))
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

/// Resolve a path within the project data directory.
///
/// - `subpath`: path relative to the data dir (e.g., `"daemon.sock"`, `"paste"`)
/// - `fallback_name`: filename for the `/tmp/{FALLBACK_PREFIX}-{name}` fallback
///
/// Example: `data_path("daemon.sock", "daemon.sock")` →
///   `~/.rsi/daemon.sock` or `/tmp/rsi-daemon.sock`
pub fn data_path(subpath: &str, fallback_name: &str) -> PathBuf {
    dirs::home_dir()
        .map(|h| h.join(PROJECT_DIR_NAME).join(subpath))
        .unwrap_or_else(|| PathBuf::from(format!("/tmp/{}-{}", FALLBACK_PREFIX, fallback_name)))
}

/// Resolve the default daemon socket path, respecting `$RSI_DAEMON_SOCKET_PATH`
/// first, then `$RSI_SOCKET` (with `$MOTHERSHIP_SOCKET` and
/// `$FLYWHEEL_SOCKET` legacy fallbacks).
pub fn default_socket_path() -> PathBuf {
    env_with_legacy(
        ENV_DAEMON_SOCKET_PATH,
        &[ENV_SOCKET, "MOTHERSHIP_SOCKET", "FLYWHEEL_SOCKET"],
    )
    .map(PathBuf::from)
    .unwrap_or_else(|_| data_path("daemon.sock", "daemon.sock"))
}

/// Resolve the daemon socket path for non-TUI callers (`rsi-rpc`, `rsi-eval`,
/// pipeline-verify), honoring `RSI_DAEMON_SOCKET_PATH` first, then falling
/// back to the standard `default_socket_path()` resolution.
///
/// Set `RSI_DAEMON_SOCKET_PATH` in tests, the eval driver, and verification
/// agents to redirect to an isolated daemon. Mirrors the V1.1 verifier-daemon
/// pattern documented in `thoughts/shared/projects/verification-pipeline/INDEX.md`.
pub fn resolve_socket_path() -> PathBuf {
    if let Ok(p) = std::env::var(ENV_DAEMON_SOCKET_PATH) {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    default_socket_path()
}

/// Derive the exact process-ownership namespace for the configured/default
/// daemon socket and its sibling database.
pub fn process_ownership_namespace() -> String {
    PROCESS_OWNERSHIP_NAMESPACE
        .get()
        .cloned()
        .unwrap_or_else(|| process_ownership_namespace_for_socket(&default_socket_path()))
}

/// Bind the process-wide ownership namespace to the daemon's resolved config
/// before any external command can spawn. Repeating the same initialization is
/// harmless; attempting to change daemon domains in one process fails closed.
pub fn initialize_process_ownership_namespace(socket_path: &Path) -> Result<(), String> {
    let expected = process_ownership_namespace_for_socket(socket_path);
    if let Some(current) = PROCESS_OWNERSHIP_NAMESPACE.get() {
        return if current == &expected {
            Ok(())
        } else {
            Err("process ownership namespace was already bound to another daemon domain".into())
        };
    }
    match PROCESS_OWNERSHIP_NAMESPACE.set(expected) {
        Ok(()) => Ok(()),
        Err(value) if PROCESS_OWNERSHIP_NAMESPACE.get() == Some(&value) => Ok(()),
        Err(_) => Err(
            "process ownership namespace initialization raced with another daemon domain".into(),
        ),
    }
}

/// Derive a stable, collision-free textual ownership namespace from the exact
/// socket/database domain bytes.
///
/// Hex encoding is deliberate: paths need not be UTF-8 on Unix, and a lossy
/// conversion could collapse two distinct daemon domains into one kill
/// authority. The versioned framing also keeps the socket and database fields
/// unambiguous if the representation ever evolves.
pub fn process_ownership_namespace_for_socket(socket_path: &Path) -> String {
    let database_path = socket_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(DB_FILENAME);
    format!(
        "rsi-process-owner-v1:socket={}:database={}",
        encode_path_bytes(socket_path),
        encode_path_bytes(&database_path),
    )
}

#[cfg(unix)]
fn encode_path_bytes(path: &Path) -> String {
    encode_hex(path.as_os_str().as_bytes())
}

// The daemon itself targets Unix (Unix sockets and `/proc` fencing). Keeping a
// fallback makes rsi-common portable for tooling builds without pretending a
// lossy value is used by the Unix process-ownership fence.
#[cfg(not(unix))]
fn encode_path_bytes(path: &Path) -> String {
    encode_hex(path.to_string_lossy().as_bytes())
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

/// Read an environment variable, trying the new name first, then each legacy name in order.
///
/// Returns `Err(VarError::NotPresent)` if none of the names are set; treats empty
/// values as not-present so a stray `RSI_X=` in the shell doesn't shadow a real
/// `MOTHERSHIP_X=foo` legacy value.
///
/// Example: `env_with_legacy("RSI_SOCKET", &["MOTHERSHIP_SOCKET", "FLYWHEEL_SOCKET"])`
/// checks `$RSI_SOCKET`, then `$MOTHERSHIP_SOCKET`, then `$FLYWHEEL_SOCKET`.
pub fn env_with_legacy(
    new_name: &str,
    legacy_names: &[&str],
) -> Result<String, std::env::VarError> {
    if let Ok(v) = std::env::var(new_name) {
        if !v.is_empty() {
            return Ok(v);
        }
    }
    for legacy in legacy_names {
        if let Ok(v) = std::env::var(legacy) {
            if !v.is_empty() {
                return Ok(v);
            }
        }
    }
    Err(std::env::VarError::NotPresent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_ownership_namespace_is_stable_and_socket_scoped() {
        let first = process_ownership_namespace_for_socket(Path::new("/tmp/rsi-a/daemon.sock"));
        let repeated = process_ownership_namespace_for_socket(Path::new("/tmp/rsi-a/daemon.sock"));
        let other = process_ownership_namespace_for_socket(Path::new("/tmp/rsi-b/daemon.sock"));

        assert_eq!(first, repeated);
        assert_ne!(first, other);
        assert!(first.starts_with("rsi-process-owner-v1:socket="));
        assert!(first.contains(":database="));
    }

    #[cfg(unix)]
    #[test]
    fn process_ownership_namespace_preserves_non_utf8_path_identity() {
        use std::os::unix::ffi::OsStringExt;

        let first = PathBuf::from(std::ffi::OsString::from_vec(vec![
            b'/', b't', b'm', b'p', 0x80,
        ]));
        let other = PathBuf::from(std::ffi::OsString::from_vec(vec![
            b'/', b't', b'm', b'p', 0x81,
        ]));
        assert_ne!(
            process_ownership_namespace_for_socket(&first),
            process_ownership_namespace_for_socket(&other),
        );
    }
}
