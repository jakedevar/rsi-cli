# Workspace Safety Invariants

## Overview

Workspace safety enforces four guarantees for every session `working_dir` before a subprocess is spawned:

1. **Path canonicalization** — symlinks are resolved and `.`/`..` components are normalized via `std::fs::canonicalize`.
2. **Existence validation** — the path must exist on disk and be a directory (not a file).
3. **Containment boundaries** — when `MOTHERSHIP_WORKSPACE_ROOTS` is set, the canonical path must be a descendant of at least one configured root.
4. **Symlink safety** — canonicalization eliminates symlink-based containment escapes; the `resolve_sandboxed_path` function also handles the non-existing-file case via manual component walking with `..` escape detection.

The goal is to surface invalid `working_dir` values as JSON-RPC error responses (visible in the TUI notification bar) rather than silently creating sessions that immediately fail.

## Architecture

Workspace safety is enforced at three independent layers, providing defense in depth:

```
TUI LaunchSession RPC call
        │
        ▼
[RPC Layer — rpc.rs]
  canonicalize_working_dir()     ← returns INVALID_PARAMS on failure
  validate_containment()         ← returns INVALID_PARAMS if outside roots
        │
        ▼
[Session Launch — session/launch.rs]
  working_dir.canonicalize()     ← defense-in-depth, falls back gracefully
        │
        ▼
[Project Index — project_cache.rs]
  ProjectIndex::new() canonicalizes project paths at construction
  find_project_for_path() uses starts_with() on canonical paths
        │
        ▼
[Provider Tool Execution — openai.rs]
  resolve_sandboxed_path()       ← per-file containment for tool calls
```

The RPC layer is the primary enforcement point. The session launch layer is a secondary defense for internal callers (rotation, retry) that bypass RPC validation. The project cache layer ensures prefix matching is symlink-safe. Provider tool execution applies containment at the per-file level during agent tool loops.

## Configuration

### `MOTHERSHIP_WORKSPACE_ROOTS`

Controls which directories sessions are permitted to run in. The env var is parsed at daemon startup in `Config::from_env()`.

**Format:** Comma-separated list of absolute paths.

```sh
MOTHERSHIP_WORKSPACE_ROOTS=/home/user/work,/home/user/personal
```

**Behavior:**

- Each path in the list is canonicalized at daemon startup via `path.canonicalize()`.
- Paths that do not exist or are not accessible are **silently skipped** with a warning printed to stderr:
  ```
  Warning: workspace root '/path/to/missing' is not accessible: <error>. Skipping.
  ```
- If the environment variable is not set, or if parsing produces an empty list, `workspace_roots` defaults to an empty `Vec<PathBuf>`.
- When `workspace_roots` is empty, **all paths are permitted** (opt-in containment — no restrictions).
- The canonicalized roots are stored in `Config.workspace_roots: Vec<PathBuf>`, passed into `SessionManager` at construction, and exposed via `SessionManager::workspace_roots() -> &[PathBuf]`.

**Example with multiple roots:**

```sh
export MOTHERSHIP_WORKSPACE_ROOTS=/home/user/work,/home/user/personal,/opt/tools
```

Sessions with `working_dir` outside all three roots will be rejected with:
```
working_dir '/some/other/path' is outside all allowed workspace roots
```

## Key Components

### Path Canonicalization

**Function:** `canonicalize_working_dir(path: &Path) -> Result<PathBuf>`

**Location:** `crates/rsid/src/path_safety.rs`

Calls `path.canonicalize()` which resolves symlinks and normalizes `.` and `..` components using the OS. On success, verifies the result `is_dir()` — canonicalize succeeds on files too, so this check is explicit.

Returns `DaemonError::InvalidParam` with a user-friendly message in two cases:
- Path is not accessible (does not exist, permissions denied, etc.): `"working_dir '<path>' is not accessible: <io error>"`
- Path exists but is not a directory: `"working_dir '<path>' is not a directory"`

The error message is suitable for display in the TUI notification bar.

### Containment Validation

**Function:** `validate_containment(path: &Path, allowed_roots: &[PathBuf]) -> Result<()>`

**Location:** `crates/rsid/src/path_safety.rs`

Requires both `path` and each root to be pre-canonicalized (no symlinks, no `..`). Uses `path.starts_with(root)` for each root in sequence. Returns `Ok(())` on the first match.

When `allowed_roots` is empty, returns `Ok(())` immediately — containment is opt-in.

On failure (no root matched), returns `DaemonError::InvalidParam`:
```
working_dir '<path>' is outside all allowed workspace roots
```

### Sandboxed Path Resolution

**Function:** `resolve_sandboxed_path(working_dir: &Path, relative: &str) -> Result<PathBuf, String>`

**Location:** `crates/rsid/src/path_safety.rs` (extracted from `openai.rs` for cross-provider reuse)

Used by provider tool execution loops (`read_file`, `write_file`, `list_dir`, `run_command`) to prevent agent-requested paths from escaping the session's `working_dir`.

Two code paths:

1. **Existing files** — canonicalizes the candidate path and checks `canon.starts_with(canon_wd)`. Symlinks are resolved by `canonicalize()`, so symlinks pointing outside the working directory are caught.

2. **Non-existing files (write targets)** — manually walks path components starting from the canonical `working_dir`, applying `ParentDir` (`.pop()`), `Normal` (`.push()`), and ignoring `CurDir`. Absolute paths and prefix paths (Windows) within relative strings are rejected immediately. After the walk, checks `normalized.starts_with(canon_wd)`.

Path handling for the `relative` argument:
- Relative paths: joined with `working_dir`.
- Absolute paths: used as-is and then checked for containment within `working_dir`. Absolute paths outside `working_dir` are rejected.

Returns `Err(String)` with a human-readable message on any rejection:
- `"Path escapes working directory: <path>"`
- `"Cannot resolve working directory: <error>"`
- `"Cannot resolve path: <error>"`
- `"Absolute paths not allowed"` (for absolute components inside a relative path during the manual walk)
- `"Prefix paths not allowed"` (Windows path prefixes)

### Non-Strict Canonicalization (reserved)

**Function:** `canonicalize_non_strict(path: &Path) -> Result<PathBuf>`

**Location:** `crates/rsid/src/path_safety.rs`

Resolves symlinks for existing path segments and appends non-existing segments verbatim. Protects against infinite symlink cycles using a depth counter capped at `MAX_SYMLINK_DEPTH = 40` (matching Linux's `MAXSYMLINKS`). Currently `#[allow(dead_code)]` — reserved for future use cases where paths must be validated before their target directory is created. Described in code comments as the Rust equivalent of Symphony's `PathSafety.canonicalize/1`.

## Enforcement Points

### RPC Layer

**Location:** `crates/rsid/src/rpc.rs`, `handle_launch_session()`

The primary enforcement point. Validation runs synchronously before any background work is started, so errors surface as RPC responses rather than silent session failures.

```rust
// Validate and canonicalize working_dir before any background work.
let validated_working_dir = params
    .working_dir
    .as_deref()
    .map(crate::path_safety::canonicalize_working_dir)
    .transpose()?;

// Optional workspace root containment check.
if let Some(ref canon_dir) = validated_working_dir {
    let roots = self.session_manager.workspace_roots();
    crate::path_safety::validate_containment(canon_dir, roots)?;
}
```

If `canonicalize_working_dir` or `validate_containment` returns `Err(DaemonError::InvalidParam(_))`, the outer error handler maps it to JSON-RPC error code `-32602` (`INVALID_PARAMS`) and returns the error message to the TUI.

The canonical path, if produced, is used as `working_dir` in the `LaunchConfig` passed to `SessionManager::launch_session()`. The original (uncanonical) path is used as fallback if `validated_working_dir` is `None` (i.e., the caller did not provide a `working_dir`).

### Session Launch

**Location:** `crates/rsid/src/session/launch.rs`, `SessionManager::launch_session()`

Defense-in-depth canonicalization that covers internal callers (context rotation, retry) which bypass RPC validation:

```rust
// Defense-in-depth: canonicalize working_dir even if the RPC layer already did.
// Direct callers (rotation, retry) may bypass RPC validation. If canonicalize
// fails here (directory deleted between RPC call and launch), fall back to the
// original path and let cmd.current_dir() produce the Io error downstream.
let working_dir = working_dir.canonicalize().unwrap_or(working_dir);
```

Unlike the RPC layer, this canonicalization uses `unwrap_or` — if the directory was deleted between the RPC call and subprocess spawn, the error is deferred to `cmd.current_dir()` at spawn time. This is intentional: the RPC has already returned success; the session will record a `Failed` status instead.

The canonical `working_dir` is then passed to `ProjectIndex::find_project_for_path()` for project resolution.

### Project Cache

**Location:** `crates/rsid/src/project_cache.rs`, `ProjectIndex::new()`

Project paths are canonicalized at index construction time so that symlink-based differences do not break prefix matching in `find_project_for_path()`:

```rust
// Projects without a path are excluded. Project paths are canonicalized at
// construction time so that symlink-based differences don't break prefix matching.
// Paths that don't exist on disk (or can't be canonicalized) are silently skipped.
match path.canonicalize() {
    Ok(canon) => Some((canon, p.id)),
    Err(_) => None,  // silently excluded from the index
}
```

Projects with stale or inaccessible paths are silently excluded from index lookups. Updating the project record restores it to the index.

The index entries are sorted by path length (descending) so that `find_project_for_path()` returns the longest (most specific) matching prefix.

## Capability Negotiation

**Location:** `crates/rsi-common/src/rpc.rs`, `DaemonCapabilities`

The `workspace_safety` field in `DaemonCapabilities` signals to the TUI that the daemon performs path validation at RPC ingestion:

```rust
/// Daemon validates and canonicalizes session working_dir at RPC ingestion.
/// Invalid paths return INVALID_PARAMS error instead of creating failed sessions.
/// Symlinked working directories are resolved before project index lookup.
#[serde(default)]
pub workspace_safety: bool,
```

In `DaemonCapabilities::default()`, `workspace_safety` is `true`. The TUI queries capabilities via the `GetDaemonCapabilities` RPC method at startup to determine which features the daemon supports.

## Error Handling

Path validation errors propagate as `DaemonError::InvalidParam`, which the RPC handler maps to JSON-RPC error code `-32602` (`INVALID_PARAMS`):

```rust
let code = match &e {
    DaemonError::InvalidParam(_) => INVALID_PARAMS,   // -32602
    DaemonError::SessionNotFound(_) => INVALID_PARAMS,
    _ => INTERNAL_ERROR,
};
RpcResponse::error(request.id.clone(), RpcError { code, message: e.to_string(), data: None })
```

The `message` field of the RPC error contains the user-readable string produced by `canonicalize_working_dir` or `validate_containment`. The TUI displays this in the notification bar.

No session record is created when path validation fails — the RPC returns an error before calling `SessionManager::launch_session()`.

Invalid workspace roots in `MOTHERSHIP_WORKSPACE_ROOTS` are handled at daemon startup: inaccessible paths are skipped with a stderr warning, not a fatal error. The daemon starts with whatever valid roots remain.

## Tests

### `path_safety.rs` tests

| Test | What it covers |
|---|---|
| `test_canonicalize_working_dir_existing_dir` | Succeeds on an existing directory; result is absolute |
| `test_canonicalize_working_dir_nonexistent` | Returns error containing "working_dir" and "not accessible" |
| `test_canonicalize_working_dir_file_not_dir` | Returns error containing "not a directory" for file paths |
| `test_validate_containment_empty_roots_allows_all` | Empty roots permits any path |
| `test_validate_containment_matching_root` | Path inside a root returns `Ok(())` |
| `test_validate_containment_no_match` | Path outside all roots returns error containing "outside all allowed workspace roots" |
| `test_validate_containment_multiple_roots` | Path must match at least one root; unmatched paths fail |
| `test_resolve_sandboxed_path_normal` | Relative path inside working dir is accepted |
| `test_resolve_sandboxed_path_traversal_rejected` | `../../etc/passwd` traversal is rejected |
| `test_resolve_sandboxed_path_absolute_outside_rejected` | Absolute path outside working dir is rejected |
| `test_canonicalize_non_strict_existing_path` | Existing path produces absolute result |
| `test_canonicalize_non_strict_partially_existing` | Non-existing segments are appended verbatim after existing prefix |

### `config.rs` tests

| Test | What it covers |
|---|---|
| `test_workspace_roots_default_empty` | No env var → empty `workspace_roots` |
| `test_workspace_roots_with_tmp` | Valid path is canonicalized and stored as absolute |
| `test_workspace_roots_nonexistent_skipped` | Inaccessible root in comma list is skipped; valid root survives |

### `project_cache.rs` tests

| Test | What it covers |
|---|---|
| `test_canonicalization_in_index` | Project paths are canonicalized at construction; lookup with canonical form matches |

### `openai.rs` tests (duplicate coverage after extraction)

The `openai.rs` module contains `test_resolve_sandboxed_path_normal`, `test_resolve_sandboxed_path_traversal_rejected`, and `test_resolve_sandboxed_path_absolute_rejected` which exercise the same function now implemented in `path_safety.rs` via the re-export.

## Source Files

| File | Role |
|---|---|
| `crates/rsid/src/path_safety.rs` | Core path safety module: `canonicalize_working_dir`, `validate_containment`, `resolve_sandboxed_path`, `canonicalize_non_strict` |
| `crates/rsid/src/config.rs` | `Config.workspace_roots` field; `MOTHERSHIP_WORKSPACE_ROOTS` parsing in `Config::from_env()` |
| `crates/rsid/src/rpc.rs` | Primary enforcement: calls `canonicalize_working_dir` and `validate_containment` in `handle_launch_session()` |
| `crates/rsid/src/session/launch.rs` | Defense-in-depth `working_dir.canonicalize()` in `SessionManager::launch_session()` |
| `crates/rsid/src/session/mod.rs` | `SessionManager.workspace_roots` field; `workspace_roots()` accessor |
| `crates/rsid/src/project_cache.rs` | `ProjectIndex::new()` canonicalizes project paths; `find_project_for_path()` uses `starts_with()` |
| `crates/rsid/src/openai.rs` | Imports `resolve_sandboxed_path` from `path_safety` for provider tool execution |
| `crates/rsi-common/src/rpc.rs` | `DaemonCapabilities.workspace_safety: bool` capability flag |
