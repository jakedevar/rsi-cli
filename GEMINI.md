# GEMINI.md

This file provides foundational mandates and instructional context for Gemini CLI when working in the `rsi` repository.

## Project Overview

**Rsi** is a Vim-like Terminal User Interface (TUI) designed for managing multiple AI coding sessions across various providers (Claude, Codex, Gemini, and Local models). It is optimized for power users who prefer keyboard-driven workflows and high information density.

### Core Technologies
- **Language:** Rust (Edition 2024)
- **TUI Framework:** `ratatui` + `crossterm`
- **Vim Bindings:** `modalkit`
- **Async Runtime:** `tokio`
- **Database:** `rusqlite` (SQLite)
- **Providers:** Claude CLI, Codex CLI, Gemini CLI, OpenAI-compatible APIs

## Architecture

The project is organized as a Rust workspace with three primary crates:

1.  **`crates/rsi` (TUI Client):**
    - Handles terminal rendering, keyboard input (Vim machine), and user interaction.
    - Communicates with the daemon over a Unix socket using JSON-RPC 2.0.
    - Manages local UI state (tabs, splits, scroll positions).

2.  **`crates/rsid` (Daemon):**
    - Manages AI session lifecycles and spawns provider CLI subprocesses.
    - Handles persistence via SQLite (`~/.rsi/rsi.db`).
    - Implements a memory system with embedding-based search and FTS.
    - Provides an `EventBus` for real-time updates to connected clients.

3.  **`crates/rsi-common` (Shared Types):**
    - Contains shared data structures, RPC request/response contracts, and serialization logic.

## Building and Running

### Prerequisites
- Rust toolchain (latest stable/nightly)
- Provider binaries (e.g., `claude`, `codex`, `gemini`) must be in your `PATH`.

### Key Commands
```bash
# Build the entire workspace
cargo build --workspace

# Run tests
cargo test --workspace

# Run the daemon (required for TUI)
cargo run --bin rsid

# Run the TUI client
cargo run --bin rsi

# Development scripts (require cargo-watch)
./scripts/dev-daemon.sh   # Hot-reload daemon
./scripts/dev-tui.sh      # Hot-reload TUI
```

*Note: The TUI will attempt to auto-start the daemon if it is not running.*

## Development Conventions

### The Seven-Expert Framework
All implementation decisions must be filtered through these lenses (in order):
1.  **Software Engineer:** Clean Rust architecture, no duplication, correct abstractions.
2.  **Tech Wizard:** Zero-waste correctness; build on the right foundation the first time. (Veto power on order).
3.  **UI/UX Power User:** Maximum information density, keyboard efficiency (1-2 keystrokes).
4.  **Cognitive Flow Engineer:** ADHD-aware; protect flow state, zero-friction switching, minimal latency.
5.  **Vim Language Designer:** Compositional keyboard grammar (verbs + nouns). (Veto power on keybindings).
6.  **Systems Performance Engineer:** Zero unnecessary allocations, event-driven, efficient rendering.
7.  **Reliability Engineer:** Graceful degradation, crash recovery, no silent failures.

### Keybinding Updates (MANDATORY)
When modifying keybindings, you **MUST** update `docs/keybindings.md`. Keybindings are defined in:
- `crates/rsi/src/keybindings.rs` (Normal mode)
- `crates/rsi/src/commands.rs` (Ex-commands)
- `crates/rsi/src/overlay.rs` (Modals)
- `crates/rsi/src/input_bar.rs` (Insert/Input mode)

### Database Rules
Agents MAY query and mutate `~/.rsi/rsi.db` directly when raw SQL is the right tool. The daemon's JSON-RPC interface is the **preferred** path for normal session flows because it auto-enforces timestamp formats (nanosecond RFC 3339), UUID generation, and status-machine invariants — but raw SQL is allowed.

**Hard rules — non-negotiable:**
- **NEVER alter the schema without a migration.** Schema changes (`CREATE`/`ALTER`/`DROP TABLE`, `CREATE`/`DROP INDEX`, view/trigger DDL, etc.) MUST land as a versioned migration in `crates/rsid/src/store/migrations/` with a `user_version` bump. Ad-hoc DDL silently desyncs dev and production schemas.
- **NEVER hard-delete rows without explicit user consent.** Default to logical delete (`status = 'Deleted'`, `pending_archive`). If you genuinely need a `DELETE FROM ...`, surface the exact rows and SQL and wait for confirmation.

When writing raw mutations, honor daemon-enforced invariants: RFC 3339 nanosecond timestamps, lowercase canonical UUIDs, exact enum strings for `SessionStatus` / `SessionKind` / `SandboxCleanupState`, and atomic sandbox tombstones (cleanup_state + null root + null branch in one UPDATE — see `Store::mark_sandbox_purged`).

### Context Rotation (MANDATORY)
If a system message indicates "CONTEXT ROTATION REQUIRED" or "AUTO-COMPACT INTERCEPTED", immediately execute `/rotate_context`. This preserves structured knowledge in handoff documents before context window limits are reached.

## Key Files and Directories

- `crates/rsi/src/app.rs`: Main TUI application state and polling logic.
- `crates/rsi/src/action_handler/`: Logic for handling `LcAction` (central action enum).
- `crates/rsid/src/session.rs`: `SessionManager` for process orchestration.
- `crates/rsid/src/rpc.rs`: JSON-RPC server implementation.
- `crates/rsid/src/memory/`: Vector search and embedding management.
- `thoughts/shared/plans/`: Historical and active development plans.
- `docs/`: Technical documentation (keybindings, architecture, memory).

## RPC and Data Flow
- **Socket:** `~/.rsi/daemon.sock`
- **Protocol:** JSON-RPC 2.0 over Unix Domain Socket.
- **State:** TUI polls for updates using a phased state machine (`Connect` -> `ListSessions` -> `FetchEvents` -> `Done`).
- **Events:** The daemon pushes events (stdout streams, status changes) to the `EventBus`, which the TUI consumes.
