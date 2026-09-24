# App-Server Protocol

## Overview

The app-server protocol is a bidirectional JSON-RPC 2.0 communication mode for provider sessions,
contrasted with the unidirectional (CLI stdout) mode used by all other providers. It is currently
implemented for the `CodexAppServer` provider, which wraps a `codex app-server` subprocess.

The core abstraction is the `ProviderSession` trait (`crates/rsid/src/provider.rs`). Every
provider — whether a plain CLI subprocess or a bidirectional app-server — exposes the same interface
to the monitor loop. CLI providers are wrapped by `CliProviderSession`. The Codex app-server is
wrapped by `CodexAppServerSession`.

The app-server protocol adds three capabilities that CLI providers cannot support:

- **Structured approvals**: the provider sends a JSON-RPC request asking for explicit user approval,
  and the daemon sends back a structured response.
- **Multi-turn continuation**: the daemon sends a `turn/start` request to continue a session without
  spawning a new subprocess.
- **Dynamic tools**: tools are registered with a `ToolRegistry` and advertised to the provider at
  session start. The provider can call them; the daemon dispatches to the registered handler and
  sends back the result.

---

## Architecture

```
TUI  ←— JSON-RPC 2.0 / Unix socket —→  rsid daemon
                                             │
                               ┌─────────────┼─────────────────┐
                               │             │                  │
                        CliProviderSession   CodexAppServerSession
                        (mpsc::Receiver)     (bidirectional stdio)
                               │             │
                          stdout/stderr      stdin/stdout
                               │             │
                          Claude / Codex    codex app-server
                          / Gemini / etc.   subprocess
```

All providers share the same `monitor_session` loop. The loop calls
`provider_session.next_event()` and dispatches on `StreamEvent.event_type`. The only monitor-loop
difference between CLI and app-server providers is how synthetic `approval_request` and `tool_call`
events are generated and responded to — those code paths exist in the provider implementation
layer, not in the monitor.

---

## Key Components

### ProviderSession Trait

File: `crates/rsid/src/provider.rs`

```rust
#[async_trait::async_trait]
pub trait ProviderSession: Send {
    async fn next_event(&mut self) -> Option<StreamEvent>;

    async fn send_approval(&mut self, request_id: i64, decision: ApprovalDecision) -> Result<()> {
        Ok(())  // default: no-op
    }

    async fn start_turn(&mut self, config: &TurnConfig) -> Result<TurnId> {
        Err(DaemonError::Process("start_turn requires app-server protocol"))
    }

    async fn send_tool_result(&mut self, call_id: i64, result: Value) -> Result<()> {
        Err(DaemonError::Process("send_tool_result requires app-server protocol"))
    }

    fn supports_multi_turn(&self) -> bool { false }
    fn supports_approvals(&self) -> bool  { false }
}
```

The trait is `Send` (no `Sync`) so the monitor loop can hold it exclusively. Supporting types:

- `ApprovalDecision` — `Approve`, `ApproveForSession`, or `Deny`.
- `TurnConfig` — holds `input: String` and `working_dir: Option<PathBuf>`.
- `TurnId` — opaque `String` wrapper returned by `start_turn`.

Default implementations of `send_approval`, `start_turn`, and `send_tool_result` allow CLI
providers to compile against the full interface without any extra code. CLI providers get the
no-op approval default and an error-returning default for `start_turn` / `send_tool_result`.

### CliProviderSession

File: `crates/rsid/src/provider.rs`

```rust
pub struct CliProviderSession {
    event_rx: tokio::sync::mpsc::Receiver<StreamEvent>,
}
```

Wraps the `mpsc::Receiver<StreamEvent>` produced by every CLI provider's `launch()` call. All
existing providers (Claude, Codex, OpenCode, Local, Gemini, Nullclaw) are wrapped in
`CliProviderSession` before being handed to `monitor_session`. The wrapper overrides only
`next_event()` — all other trait methods use the defaults.

`supports_multi_turn()` returns `false`. `supports_approvals()` returns `false`.

### CodexAppServerSession

File: `crates/rsid/src/codex_app_server.rs`

`CodexAppServerSession` manages the reader, writer, and response-correlation state for a
running `codex app-server` process. Fields:

| Field | Type | Purpose |
|---|---|---|
| `thread_id` | `String` | Thread ID returned by `thread/start` response |
| `working_dir` | `PathBuf` | Default working directory for `turn/start` |
| `write_tx` | `mpsc::Sender<Vec<u8>>` | Send serialized JSON-RPC lines to writer task |
| `event_rx` | `mpsc::Receiver<StreamEvent>` | Receive normalized events from reader task |
| `next_id` | `Arc<AtomicI64>` | Shared outbound request ID counter (sequential, starts at 1) |
| `pending_responses` | `HashMap<i64, oneshot::Sender<Value>>` | In-flight response correlation |

The session is created after a successful launch+handshake. It overrides all four non-default
trait methods:

- `next_event()` — reads from `event_rx`.
- `send_approval(request_id, decision)` — serializes a JSON-RPC response with
  `{ "approved": bool, "approveForSession": bool }` as the result body, sent as a response to the
  provider's inbound request ID.
- `start_turn(config)` — sends a `turn/start` JSON-RPC request with
  `{ "threadId", "input", "cwd" }`. Returns `TurnId(thread_id)` immediately (the turn ID is the
  thread ID).
- `send_tool_result(call_id, result)` — serializes a JSON-RPC response for the given `call_id`.

`supports_multi_turn()` returns `true`. `supports_approvals()` returns `true`.

`CodexAppServerSession::writer()` splits off an `AppServerWriter` containing a clone of
`write_tx` and a clone of the `Arc<AtomicI64>` counter. This allows RPC handlers to write to the
app-server's stdin without owning the full session.

### CodexAppServerProcess

File: `crates/rsid/src/codex_app_server.rs`

Owns the `Child` handle for the `codex app-server` subprocess. Methods:

- `interrupt()` — sends `SIGINT` via `nix::sys::signal::kill`.
- `kill()` — force-kills via `tokio::process::Child::kill()`.
- `try_wait()` — non-blocking exit status check.

This is the type stored as `ProviderProcess::CodexAppServer(...)` in `TrackedSession`.

### CodexAppServerClient

File: `crates/rsid/src/codex_app_server.rs`

```rust
pub struct CodexAppServerClient {
    binary_path: PathBuf,
}
```

Responsible for:

1. Locating the `codex` binary via `which::which("codex")`.
2. Spawning `codex app-server [-m <model>]` with piped stdin/stdout/stderr.
3. Running the initialization handshake (see Session Lifecycle / Launch below).
4. Returning `(CodexAppServerProcess, CodexAppServerSession)`.

`CodexAppServerClient::is_available()` calls `which::which("codex")` without allocating a full
client; used in the health-status response and in launch-time binary checks.

### Reader Task

Spawned inside `CodexAppServerClient::launch()`. Reads `stdout` line-by-line and routes each
parsed JSON value into one of three channels:

| Condition | Destination |
|---|---|
| `id` field present and `result` or `error` present | `response_tx` (handshake response correlation) |
| `method` field present and method is in `APPROVAL_METHODS` | `event_tx` as synthetic `approval_request` StreamEvent |
| `method == "item/tool/call"` | `event_tx` as synthetic `tool_call` StreamEvent |
| `method` present but not an approval or tool call | `notification_tx` (notifications, currently dropped) |
| No `method` field | delegated to `map_app_server_compatible_event()` in `crates/rsid/src/codex.rs` |

**Approval methods** recognized:

```
item/commandExecution/requestApproval
item/fileWrite/requestApproval
item/fileDelete/requestApproval
item/networkRequest/requestApproval
item/mcp/requestApproval
```

A synthetic `approval_request` event carries:

```json
{
  "request_id": <i64>,
  "method": "<approval method>",
  "description": "<description | command | path from params>",
  "params": { ... }
}
```

A synthetic `tool_call` event carries:

```json
{
  "call_id": <i64>,
  "name": "<tool name>",
  "arguments": { ... }
}
```

### Writer Task

Spawned inside `CodexAppServerClient::launch()`. Owns `stdin` and drains a
`write_rx: mpsc::Receiver<Vec<u8>>` channel. Each received `Vec<u8>` is written to stdin and
flushed immediately.

### Stderr Task

Spawned inside `CodexAppServerClient::launch()`. Collects all non-empty stderr lines and emits a
single `process_error` StreamEvent when stderr is closed. The monitor loop surfaces `process_error`
events as assistant-role error messages in the conversation.

### ToolRegistry

File: `crates/rsid/src/tool_registry.rs`

```rust
pub struct ToolRegistry {
    tools: HashMap<String, (ToolSpec, ToolHandler)>,
}
```

`ToolSpec` holds `name: String`, `description: String`, and `parameters: Value` (a JSON Schema
object). `ToolHandler` is:

```rust
pub type ToolHandler = Arc<dyn Fn(Value) -> Pin<Box<dyn Future<Output = Result<Value>> + Send>> + Send + Sync>;
```

Key methods:

- `register(spec, handler)` — inserts a tool by name.
- `tool_specs() -> Vec<ToolSpec>` — returns all registered specs; used to build the
  `dynamicTools` array in the `thread/start` request.
- `execute(name, args) -> Result<Value>` — looks up the handler by name and awaits it. Returns
  `DaemonError::Process("Unknown tool: ...")` if the name is not registered.

`register_builtin_tools(registry, memory_handle)` conditionally registers one built-in tool:

**`rsi_memory_search`** — if a `MemoryHandle` is available, registers a tool that calls
`handle.search(query, Some(max_results), None)`. JSON Schema:

```json
{
  "type": "object",
  "properties": {
    "query":       { "type": "string", "description": "Search query" },
    "max_results": { "type": "integer", "description": "Max results (default 5)" }
  },
  "required": ["query"]
}
```

### ProviderProcess Enum

File: `crates/rsid/src/session/types.rs`

```rust
pub enum ProviderProcess {
    Claude(ClaudeProcess),
    Codex(CodexProcess),
    OpenCode(OpenCodeProcess),
    Local(OpenAiProcess),
    Gemini(GeminiProcess),
    Nullclaw(NullclawProcess),
    CodexAppServer(CodexAppServerProcess),
}
```

Delegates `interrupt()`, `kill()`, `is_alive()`, and `try_exit_status()` to the inner process
handle. `CodexAppServer` uses `SIGINT` for interrupt (same as the other subprocess variants).
For `try_exit_status()`, `CodexAppServer` behaves like the subprocess variants and returns
`Some(exit_code)` on exit; `Local` and `Harness` (tokio task variants) return `None`.

`TrackedSession.process` is `Option<ProviderProcess>`. For `CodexAppServer` sessions, it is
`None` at the moment `launch_session()` inserts the `TrackedSession` into the active map; the
background launch task updates it to `Some(ProviderProcess::CodexAppServer(...))` after the
JSON-RPC handshake succeeds.

---

## Session Lifecycle

### Launch

File: `crates/rsid/src/session/launch.rs`

**Binary availability check** — before accepting the RPC request, `launch_session()` calls
`CodexAppServerClient::is_available()`. If the `codex` binary is not on `PATH`, it returns
`DaemonError::CodexBinaryNotFound` immediately.

**Sync vs. deferred launch** — all CLI providers complete their launch synchronously (process
spawn is fast) and produce `(ProviderProcess, mpsc::Receiver<StreamEvent>)` immediately. For
`CodexAppServer`, the launch is tagged `SyncLaunchResult::AppServerDeferred`. The `TrackedSession`
is inserted into the active map with `process: None` and `cli_event_rx: None`.

**Background task** — the deferred app-server launch happens inside the same `tokio::spawn` block
that handles all post-RPC work. The sequence is:

1. Create `CodexAppServerClient` (binary path lookup).
2. Call `client.launch(&config, &tool_specs)` — spawns subprocess and runs handshake.
3. On success, write `ProviderProcess::CodexAppServer(process)` into `TrackedSession.process`.
4. Pass the resulting `Box<dyn ProviderSession>` to `monitor_session()`.

On handshake failure, the background task logs an error and returns without calling
`monitor_session()`. The session stays in the active map in `Starting` status until the
reconciliation loop detects it as stale. (Tool specs are currently passed as an empty `Vec` —
noted as "Phase 6: tool registry integration" in a code comment.)

**Context pipeline** — the `CodexAppServer` provider is not listed among the providers that
receive context-pipeline injection (`Claude | Local | Gemini | Nullclaw`). System
prompt assembly is skipped for `CodexAppServer` and `Codex`; tools are the intended injection
mechanism.

### JSON-RPC Handshake

Performed inside `CodexAppServerClient::launch()`. Three steps with individual timeouts:

1. **`initialize`** — sent immediately after spawn. Protocol version `"2024-11-05"`, client
   name `"rsi"`, version from `env!("CARGO_PKG_VERSION")`. Waits up to **10 seconds** for
   the response.

2. **`notifications/initialized`** — sent as a notification (no `id` field) immediately after
   receiving the `initialize` response.

3. **`thread/start`** — sent with `{ "input": query, "cwd": working_dir, "dynamicTools": [...] }`.
   Waits up to **30 seconds** for the response. The response must contain `"threadId"` or
   `"thread_id"`. A synthetic `system` StreamEvent with `subtype: "init"` is emitted after the
   thread ID is captured.

### Monitor Loop

File: `crates/rsid/src/session/monitor.rs`

`monitor_session()` takes `mut provider_session: Box<dyn ProviderSession>` and drives the main
`tokio::select!` loop. The loop branches are:

- `stop_rx.recv()` — stop signal (interrupt or rotation).
- `snapshot_interval.tick()` — periodic context snapshot.
- `tokio::time::sleep_until(phase_deadline)` — rotation deadline.
- `provider_session.next_event()` — the primary event branch.

The monitor does not inspect `supports_approvals()` or `supports_multi_turn()` flags directly;
those are used by the RPC layer. The `process_error` event type is handled uniformly across all
providers: it surfaces the stderr text as an assistant-role `ConversationEvent`.

### Interrupt / Continue

**Interrupt** (`interrupt_session()`, `crates/rsid/src/session/lifecycle.rs`):

Provider-agnostic. Sets `tracked.interrupt_requested = true`, calls
`tracked.process.interrupt()` (which sends `SIGINT` for `CodexAppServer`), and sends on
`stop_tx`. No special app-server path.

**Continue** (`continue_session()`):

`CodexAppServer` is handled in the provider dispatch match as:

```rust
SessionProvider::CodexAppServer => {
    // Continue via Codex CLI for compatibility with the synchronous continue path.
    // App-server sessions that support multi-turn use TurnController instead.
    let codex = self.codex_client ...;
    let (p, rx) = codex.launch(&config)?;
    (ProviderProcess::Codex(p), rx)
}
```

The synchronous `ContinueSession` RPC path falls back to Codex CLI for `CodexAppServer`
sessions. In-process multi-turn continuation via `start_turn()` is intended to be handled by a
separate TurnController (not yet wired into RPC as of the current implementation).

### Rotation

File: `crates/rsid/src/session/rotation.rs`

Rotation produces child sessions. When the parent provider is `CodexAppServer`, both rotation
paths (`resume_for_handoff_write` and `spawn_rotation_child`) fall back to Codex CLI:

```rust
SessionProvider::CodexAppServer => {
    // For rotation children, fall back to Codex CLI mode.
    // App-server sessions do not use subprocess rotation (TurnController handles continuation).
    match CodexClient::new() { ... }
}
```

The comment in both cases states that app-server sessions are not expected to use subprocess
rotation — multi-turn continuation via `start_turn()` is the intended mechanism for avoiding
context exhaustion. The Codex CLI fallback ensures rotation remains operational while the
TurnController path is not yet implemented.

The rotation child session preserves the parent's `provider` field (set to `CodexAppServer`),
so a rotated CodexAppServer session would itself be a `CodexAppServer` session — but it would
actually launch as a Codex CLI process due to the fallback. This is a known in-progress state.

---

## Health and Discovery

File: `crates/rsid/src/session/queries.rs`

`GetHealthStatus` populates `HealthStatusResponse.provider_codex_app_server_available` using:

```rust
provider_codex_app_server_available: crate::codex_app_server::CodexAppServerClient::is_available(),
```

`is_available()` calls `which::which("codex")` — no subprocess is spawned.

File: `crates/rsi-common/src/rpc.rs`

`DaemonCapabilities` includes:

```rust
/// Supports app-server bidirectional JSON-RPC protocol for CodexAppServer sessions.
pub app_server_protocol: bool,
```

The default implementation of `DaemonCapabilities::default()` sets `app_server_protocol: true`.
The TUI can gate app-server UI features on this flag via the `GetDaemonCapabilities` RPC.

**Model discovery** (`DiscoverModels` RPC, `crates/rsid/src/rpc.rs`):

```rust
SessionProvider::CodexAppServer => {
    // Reuse Codex model list for app-server mode
    let codex_client = CodexClient::new()?;
    codex_client.discover_models()
}
```

The app-server provider reuses the Codex CLI model list.

---

## TUI Integration

File: `crates/rsi/src/ui/status.rs`

The TUI displays `CodexAppServer` sessions with the label `"Codex(AS)"` in both the session
list and the content view:

```rust
rsi_common::types::SessionProvider::CodexAppServer => "Codex(AS)",
```

This label appears in the compact inline display and in the metadata parts of the content header.

**Approval flow** — the `WaitingApproval` session status is used when a session emits an
`AskUserQuestion` tool call (checked in the monitor loop). For app-server approval requests,
`send_approval()` on `CodexAppServerSession` sends a structured JSON-RPC response back to the
provider. The TUI sends answers via the `AnswerQuestion` RPC which calls
`session_manager.answer_question()`, which in turn calls `continue_session()`.

---

## Source Files

| File | Purpose |
|---|---|
| `crates/rsid/src/provider.rs` | `ProviderSession` trait, `CliProviderSession`, `ApprovalDecision`, `TurnConfig`, `TurnId` |
| `crates/rsid/src/codex_app_server.rs` | `CodexAppServerSession`, `CodexAppServerProcess`, `CodexAppServerClient`, `AppServerWriter`, reader/writer/stderr tasks, handshake |
| `crates/rsid/src/tool_registry.rs` | `ToolRegistry`, `ToolSpec`, `ToolHandler`, `register_builtin_tools` |
| `crates/rsid/src/session/types.rs` | `ProviderProcess` enum, `TrackedSession` fields |
| `crates/rsid/src/session/launch.rs` | Provider dispatch, deferred app-server launch, binary availability check |
| `crates/rsid/src/session/monitor.rs` | `monitor_session()` main event loop, `ProviderSession` usage |
| `crates/rsid/src/session/lifecycle.rs` | `interrupt_session()`, `continue_session()` with `CodexAppServer` fallback |
| `crates/rsid/src/session/rotation.rs` | `resume_for_handoff_write()` and `spawn_rotation_child()` with `CodexAppServer` Codex CLI fallback |
| `crates/rsid/src/session/queries.rs` | `provider_codex_app_server_available` in `GetHealthStatus` |
| `crates/rsi-common/src/types.rs` | `SessionProvider::CodexAppServer` variant |
| `crates/rsi-common/src/rpc.rs` | `HealthStatusResponse.provider_codex_app_server_available`, `DaemonCapabilities.app_server_protocol` |
| `crates/rsi/src/ui/status.rs` | `"Codex(AS)"` provider label in TUI status and content views |
