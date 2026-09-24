# Dialectic Query Interface

## Overview

The Dialectic Query Interface is a natural-language Q&A overlay that lets the user ask questions about their accumulated knowledge: session history, projects, patterns, decisions, and indexed memory notes. Rather than requiring the user to manually search or browse sessions, they type a plain-language question and an agentic LLM loop gathers evidence from multiple sources before synthesizing a concise answer.

The name "dialectic" reflects the conversational nature: the overlay maintains multi-turn history, so follow-up questions have full context from prior exchanges.

## Architecture

```
TUI overlay (OverlayState::Dialectic)
  |-- Enter key → tokio::spawn RPC call on fresh DaemonClient connection
  |-- oneshot channel → app.dialectic_rx (polled in event loop, event.rs:705)
  ↓
DaemonClient::query_memory()  [crates/rsi/src/client.rs:997]
  ↓  JSON-RPC 2.0 over Unix socket (~/.rsi/daemon.sock)
RpcServer::handle_query_memory()  [crates/rsid/src/rpc.rs:843]
  ↓
DialecticEngine::query()  [crates/rsid/src/dialectic/mod.rs:107]
  |-- prefetch_memory(): seed context via MemoryManager::search (top 5 hits)
  |-- build messages: system prompt + prefetched context + conversation history + user query
  |-- agentic tool loop (up to max_iterations, hard cap MAX_TOOL_ITERATIONS=8)
  |     |-- call_llm(): POST to OpenAI-compatible /chat/completions
  |     |-- execute_tool() for each tool call  [tools.rs:90]
  |     |-- accumulate DialecticSource citations
  |     └-- exit loop when LLM returns a text answer (no tool calls)
  |-- if iterations exhausted: re-call LLM with tools=None to force a final answer
  └── return QueryMemoryResponse { answer, sources, tool_calls }
  ↓
TUI event loop receives response, appends assistant message, updates sources, auto-scrolls
```

The RPC call runs on a dedicated daemon connection spawned from `overlay/dialectic.rs:86` so the main TUI connection is not blocked. A 30-second timeout wraps the entire engine call (`mod.rs:114`).

## User Interaction

### Activating the Overlay

| Method | Detail |
|--------|--------|
| `g?` keybinding | Normal mode, defined in `keybindings.rs:564` — toggles overlay open/closed |
| `:ask` command | No argument: opens empty overlay; with argument (`:ask <query>`): opens with query pre-filled and ready to submit (`commands.rs:204`) |

Both activate `LcAction::OpenDialectic` or `LcAction::AskQuery(String)`, dispatched through `action_handler/overlay.rs:540-550`.

### In-Overlay Controls

| Key | Behavior |
|-----|----------|
| Type text | Appends to input buffer |
| `Enter` | Submits current input; sets `in_flight = true`; input cleared |
| `Backspace` | Deletes last character |
| `Esc` (with text) | Clears the input buffer |
| `Esc` (empty input) | Closes the overlay |
| `q` (empty input) | Closes the overlay |
| `s` (empty input) | Toggles sources panel expansion |
| `Up` / `Down` | Scrolls conversation history |
| `Esc` (while in-flight) | Cancels the pending query and closes overlay |

While a query is in-flight, all input except `Esc` is ignored. The border changes color to `status_waiting` and the title shows "thinking...".

## Engine

The engine (`crates/rsid/src/dialectic/mod.rs`) is an agentic loop over an OpenAI-compatible chat completions endpoint.

**Initialization** (`rpc.rs:355`): `RpcServer::init_dialectic()` is called at daemon startup if `dialectic_enabled` is true. It constructs a `DialecticEngine` with a reference to the `MemoryManager` (optional) and the `SessionManager`.

**Query flow** (`mod.rs:127`):

1. **Prefetch** (`mod.rs:285`): `MemoryManager::search` is called with `max_results=5`. If hits exist they are injected into the system prompt after `DIALECTIC_SYSTEM_PROMPT` (`prompts.rs:3`), labeled "PREFETCHED CONTEXT".

2. **Message construction**: System prompt → prior conversation history → current user query. The current query is deduplicated if it already appears at the end of history.

3. **Tool loop** (up to `max_iterations`, hard cap `MAX_TOOL_ITERATIONS=8`): Each iteration POSTs a `ChatRequest` to the configured LLM. If the response contains `tool_calls`, each is executed via `tools::execute_tool()`, results are appended as `role=tool` messages, and the loop continues. When the LLM returns a message with no tool calls, the loop exits with that content as the answer.

4. **Iteration exhaustion**: If the loop reaches `max_iterations` without a plain-text response, a final request is sent with `tools=None` to force a text answer. This prevents infinite loops on tool-heavy models.

**LLM call** (`mod.rs:307`): Single `reqwest::Client` per engine, 60-second per-request timeout. Temperature is fixed at 0.3 and `max_tokens` at 4096 (2048 for the forced-final call).

## Available Tools

Tool definitions are in `crates/rsid/src/dialectic/tools.rs:12`. All five tools are offered to the LLM on every iteration.

| Tool | Description | Key Parameters | Source Kind |
|------|-------------|----------------|-------------|
| `search_memory` | Full-text search over indexed memory files and session transcripts (via `MemoryManager`). Returns scored snippets with file path, line range, and score. | `query` (required), `max_results` (default 5, max 10) | `"memory"` |
| `list_sessions` | Lists recent sessions with title, status, provider, and timestamps. | `limit` (default 10, max 50), `project_id` (optional UUID filter) | `"session"` |
| `get_session_detail` | Detailed metadata for one session: title, status, provider, model, working dir, short summary. | `session_id` (required UUID) | `"session"` |
| `get_conversation_excerpt` | Fetches the most recent `Message`-type events from a session, truncating individual messages at 500 characters. | `session_id` (required UUID), `max_events` (default 20, max 50) | `"session"` |
| `get_project_info` | Project name, filesystem path, and color. | `project_id` (required UUID) | `"project"` |

Each tool returns a `(String, Option<DialecticSource>)` pair. Sources are accumulated across all tool calls in the loop and included in the final `QueryMemoryResponse`.

If memory is not configured, `search_memory` returns a graceful error string rather than failing the request.

## Configuration

All config is read at daemon startup via environment variables (`crates/rsid/src/config.rs:279`).

| Variable | Default | Description |
|----------|---------|-------------|
| `MOTHERSHIP_DIALECTIC_ENABLED` | `true` | Set to `0`, `false`, `no`, or `off` to disable. When disabled, `QueryMemory` RPC returns an error. |
| `MOTHERSHIP_DIALECTIC_URL` | `http://localhost:11434/v1` | OpenAI-compatible API base URL. Works with Ollama, vLLM, LM Studio, OpenAI, etc. |
| `MOTHERSHIP_DIALECTIC_KEY` | _(none)_ | Bearer token. Optional for local models. |
| `MOTHERSHIP_DIALECTIC_MODEL` | `qwen2.5:14b` | Model identifier passed to the `/chat/completions` endpoint. |
| `MOTHERSHIP_DIALECTIC_MAX_ITERATIONS` | `8` | Tool loop iteration cap. Clamped at `MAX_TOOL_ITERATIONS=8` regardless of this value. |

The engine is initialized by `RpcServer::init_dialectic()` called from the daemon main path. If `MOTHERSHIP_DIALECTIC_ENABLED=false`, the `dialectic_engine` field on `RpcServer` remains `None` and all `QueryMemory` calls return an RPC error with a descriptive message.

## RPC Methods

### `QueryMemory`

**Params** (`rsi-common/src/rpc.rs:472`):

```
QueryMemoryParams {
    query: String,                           // natural-language question
    project_id: Option<Uuid>,               // optional project scope
    conversation_history: Vec<(String, String)>, // [(role, content)] from prior turns
}
```

**Response** (`rpc.rs:498`):

```
QueryMemoryResponse {
    answer: String,                 // synthesized answer
    sources: Vec<DialecticSource>,  // citations from tool calls
    tool_calls: u32,                // number of tool iterations used
}
```

**`DialecticSource`** (`rpc.rs:486`):

```
DialecticSource {
    kind: String,           // "memory" | "session" | "project"
    label: String,          // human-readable identifier
    detail: Option<String>, // score/lines for memory, UUID for sessions/projects
}
```

The RPC handler (`rpc.rs:843`) deserializes params, checks the engine is initialized, then delegates to `DialecticEngine::query()`. Errors propagate as standard JSON-RPC error responses.

## TUI Overlay

### State (`crates/rsi/src/types.rs:2001`)

`OverlayState::Dialectic` holds:
- `messages: Vec<(String, String)>` — full conversation history, role/content pairs
- `sources: Vec<DialecticSource>` — citations from the last response
- `input: String` — current text being typed
- `in_flight: bool` — true while awaiting daemon response
- `scroll_offset: usize` — conversation scroll position
- `sources_expanded: bool` — whether the sources panel is expanded
- `project_id: Option<Uuid>` — captured from `app.active_project_id()` at open time

### Rendering (`crates/rsi/src/ui/overlay/dialectic.rs`)

The popup is 85% of terminal width (clamped 50–120 columns) and 80% of height (clamped 12–40 rows), centered on screen.

Layout (top to bottom):
1. **Messages area** (`Constraint::Min(3)`): conversation turns with `You:` (bold, user color) and `  ` (assistant color) prefixes. Shows italic "thinking..." line when in-flight.
2. **Sources panel** (0–6 rows, conditional): collapsed shows `Sources: N [s to expand]`; expanded lists each source as `kind label detail`.
3. **Separator** (1 row): full-width box-drawing horizontal line.
4. **Input line** (1 row): `› ` prompt + current input + block cursor (hidden when in-flight).
5. **Hint line** (1 row): context-sensitive key hints.

Border color is `theme::status_waiting()` while in-flight, `theme::accent()` otherwise. Title shows tool call count when nonzero: ` Dialectic (3 tools used) [g?] `.

### Async Response Handling (`crates/rsi/src/event.rs:704`)

The event loop polls `app.dialectic_rx` (a `tokio::sync::oneshot::Receiver`) alongside terminal events. On receipt:
- `in_flight` is set to false
- On success: assistant message appended to `messages`, `sources` replaced, `scroll_offset` set to `messages.len() - 1`
- On error: error string appended as an assistant message
- `mark_dirty()` triggers a re-render

## Key Files

| File | Purpose |
|------|---------|
| `crates/rsid/src/dialectic/mod.rs` | Engine core: query loop, prefetch, LLM calls |
| `crates/rsid/src/dialectic/tools.rs` | Tool definitions and execution handlers |
| `crates/rsid/src/dialectic/prompts.rs` | System prompt constant |
| `crates/rsid/src/rpc.rs:354,843` | `init_dialectic()` and `handle_query_memory()` |
| `crates/rsid/src/config.rs:69` | Config struct fields and env var parsing |
| `crates/rsi-common/src/rpc.rs:471` | `QueryMemoryParams`, `QueryMemoryResponse`, `DialecticSource` types |
| `crates/rsi/src/overlay/dialectic.rs` | Key handler, overlay open/submit logic, RPC dispatch |
| `crates/rsi/src/ui/overlay/dialectic.rs` | Ratatui rendering |
| `crates/rsi/src/types.rs:1999` | `OverlayState::Dialectic` variant |
| `crates/rsi/src/modalkit_types.rs:357` | `LcAction::OpenDialectic` and `AskQuery` variants |
| `crates/rsi/src/keybindings.rs:560` | `g?` binding |
| `crates/rsi/src/commands.rs:203` | `:ask` command |
| `crates/rsi/src/event.rs:704` | Async response integration in event loop |
| `crates/rsi/src/app/mod.rs:287` | `dialectic_rx` field on `App` |
| `crates/rsi/src/client.rs:997` | `DaemonClient::query_memory()` RPC wrapper |
