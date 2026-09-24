# Dream Consolidation

## Overview

Dream consolidation is Rsi's background memory-consolidation system, inspired by Honcho's approach to distilling agent experience into persistent knowledge. After sessions complete, the daemon runs a "dream cycle" — a multi-phase pipeline that reads raw conversation events, extracts atomic facts (observations), reasons over those facts to derive new knowledge, and then synthesises cross-cutting behavioural patterns. The result is an ever-growing, self-correcting observation store that other features (e.g. the Dialectic query interface) can query for user and project context.

The name mirrors the biological analogy: just as the brain consolidates memories during sleep when it is otherwise idle, Rsi consolidates session knowledge when no active coding sessions are running.

## Architecture

The dream cycle is a three-phase sequential pipeline executed by the scheduler background task:

```
Scheduler (5-min tick + idle/threshold gate)
    |
    v
Phase 1 — Extractor
    Completed sessions with no explicit observations
    → LLM call → Vec<Observation(Explicit)>
    → Store::insert_observations_batch()
    |
    v
Phase 2 — Deduction Specialist
    All Explicit observations (scoped per project)
    → LLM call → INFER / SUPERSEDE / CONTRADICT lines
    → Store: insert Deductive obs, soft-delete superseded, insert Contradiction obs
    |
    v
Phase 3 — Induction Specialist
    Explicit + Deductive + Inductive observations (scoped per project, min 3)
    → LLM call → PATTERN lines
    → Store: insert Inductive obs with ObservationConfidence
```

All three phases share a single `DreamerLlmClient` instance that talks to an OpenAI-compatible chat-completions endpoint.

## Scheduler

**File:** `crates/rsid/src/dreamer/scheduler.rs`

`spawn_dreamer()` (scheduler.rs:64) launches a tokio task and returns a `DreamerHandle` for imperative control. The task loops on a 5-minute `tokio::time::interval` (scheduler.rs:74) with `MissedTickBehavior::Skip` so missed ticks are dropped rather than bunched.

### Auto-trigger conditions (scheduler.rs:182)

Both conditions must pass:

1. **Idle gate** — `active_sessions` map must be empty (no session is currently `Running` or `Starting`). Checked outside the store lock to avoid blocking. (scheduler.rs:85-92)
2. **Observation threshold** — `count_observations_since(last_dream_at) + (pending_sessions * 5) >= observation_threshold`. The `* 5` estimates expected yield from unextracted sessions. (scheduler.rs:191-199)
3. **Cooldown** — `elapsed since last_dream_at >= cooldown_secs`. On the first ever dream there is no `last_dream_at`, so the cooldown gate is skipped. (scheduler.rs:201-210)

### Manual trigger

`DreamerHandle::trigger_now()` (scheduler.rs:46) sends `DreamerCommand::TriggerNow` over an `mpsc::channel(4)`, bypassing all threshold and cooldown checks.

### Shutdown

`DreamerHandle::shutdown()` (scheduler.rs:54) sends `DreamerCommand::Shutdown`; the loop breaks on the next `select!` iteration.

### Post-cycle state

After every cycle (auto or manual), `last_dream_at` is written to the `dream_state` table via `Store::set_dream_state()` (scheduler.rs:173-177).

## Extraction

**File:** `crates/rsid/src/dreamer/extractor.rs`

`extract_observations_with_client()` (extractor.rs:17) is called once per unprocessed session (sessions with status `Completed` or `Archived` that have no `explicit` observations).

Steps:

1. **Guard** — skips if the session has fewer than `min_events` (hardcoded to `4` by the scheduler, extractor.rs:262 in scheduler.rs). (extractor.rs:26-34)
2. **Text extraction** — calls `extract_session_text(events)` from `crates/rsid/src/memory/session_text.rs` to flatten `ConversationEvent` slices into a single string.
3. **Truncation** — caps input at `max_input_chars` (hardcoded to `16_000` by the scheduler) at a valid UTF-8 char boundary. (extractor.rs:47-55)
4. **LLM call** — builds a prompt via `build_extraction_prompt()` and calls `llm.complete(EXTRACTION_SYSTEM_PROMPT, &prompt)` (extractor.rs:57-58). The system prompt instructs the model to output a JSON array of strings.
5. **Parse** — `parse_observations_response()` deserialises the JSON array. (extractor.rs:59)
6. **Construct** — each string becomes an `Observation` with `level: ObservationLevel::Explicit`, `confidence: None`, `source_ids: []`. (extractor.rs:70-84)

## Deduction Specialist

**File:** `crates/rsid/src/dreamer/deduction.rs`

`DeductionSpecialist::run()` (deduction.rs:37) takes the full set of `Explicit` observations for one project scope (up to 500) and calls the LLM once. The system prompt (deduction.rs:9) instructs three response operations:

| Prefix | Meaning | Outcome |
|--------|---------|---------|
| `INFER: <text> SOURCES: <uuid,...>` | New logical necessity derived from explicit facts | Inserted as `ObservationLevel::Deductive` observation with `source_ids` set |
| `SUPERSEDE: <uuid> REASON: <text>` | Existing observation is outdated | Soft-deleted via `Store::soft_delete_observation()` |
| `CONTRADICT: <uuid1> vs <uuid2> REASON: <text>` | Two observations conflict | Inserted as `ObservationLevel::Contradiction` observation with `source_ids: [uuid1, uuid2]` and the reason as content |

Parsing is line-by-line (deduction.rs:77-103). Lines that do not start with a recognised prefix are silently ignored, making the format tolerant of LLM prose.

`DeductionResult` (deduction.rs:26) aggregates all three outcome lists and is returned to the scheduler.

## Induction Specialist

**File:** `crates/rsid/src/dreamer/induction.rs`

`InductionSpecialist::run()` (induction.rs:34) receives all active observations (Explicit + Deductive + Inductive) for one project scope. It requires at least 3 observations and skips otherwise (induction.rs:39).

The system prompt (induction.rs:9) instructs a single response format:

```
PATTERN: <description> CONFIDENCE: <LOW|MEDIUM|HIGH> SOURCES: <uuid,...>
```

Confidence tiers and their source-count thresholds, as stated in the prompt (induction.rs:14-15):

| Tier | Sources required |
|------|-----------------|
| LOW | 2-3 |
| MEDIUM | 4-6 |
| HIGH | 7+ |

Each parsed `PATTERN` line becomes an `Observation` with `level: ObservationLevel::Inductive` and `confidence: Some(ObservationConfidence::{Low|Medium|High})`. Source IDs are stored in `source_ids`. Unrecognised confidence strings default to `Low` (induction.rs:107).

`InductionResult` (induction.rs:26) wraps the `Vec<Observation>` and is returned to the scheduler.

## LLM Client

**File:** `crates/rsid/src/dreamer/llm_client.rs`

`DreamerLlmClient` (llm_client.rs:10) is a thin wrapper around `reqwest::Client`. It targets any OpenAI-compatible chat-completions endpoint.

Key properties:

- `api_url` — full URL including path (e.g. `https://api.anthropic.com/v1/chat/completions`)
- `api_key` — sent as `Authorization: Bearer <key>` when present
- `model` — passed verbatim in the `model` field of the request body
- Fixed parameters: `max_tokens: 4096`, `temperature: 0.3` (llm_client.rs:65-66)
- HTTP timeout: 120 seconds (llm_client.rs:72)

`complete(system_prompt, user_prompt)` (llm_client.rs:52) posts a two-message chat request and returns the first choice's content string. Non-2xx responses surface as `DaemonError::Process` with the status and response body.

No retries are performed; transient failures cause the scheduler to log a warning and move on.

## Configuration

All dream settings are read once at daemon startup in `crates/rsid/src/config.rs` (config.rs:243-277).

| Env var | Type | Default | Description |
|---------|------|---------|-------------|
| `MOTHERSHIP_DREAM_ENABLED` | bool | `true` | Set to `false`/`0`/`no`/`off` to disable the dreamer entirely |
| `MOTHERSHIP_DREAM_OBSERVATION_THRESHOLD` | u64 | `50` | Min effective observation count before a cycle can auto-trigger |
| `MOTHERSHIP_DREAM_IDLE_SECS` | u64 | `3600` | Reserved field on `DreamConfig` (currently enforced via active session count check, not a wall-clock idle timer) |
| `MOTHERSHIP_DREAM_COOLDOWN_SECS` | u64 | `28800` (8 h) | Minimum seconds between successive auto dream cycles |
| `MOTHERSHIP_DREAM_MODEL` | String | `None` | LLM model name; daemon falls back to a sensible default when absent |
| `MOTHERSHIP_DREAM_API_URL` | String | `None` | Chat-completions endpoint; defaults to Anthropic API |
| `MOTHERSHIP_DREAM_API_KEY` | String | `ANTHROPIC_API_KEY` | API key; falls back to `ANTHROPIC_API_KEY` env var (config.rs:270-272) |
| `MOTHERSHIP_DREAM_BATCH_SIZE` | usize | `20` | Max sessions to process per cycle (extraction phase only) |

When `dream_enabled` is `false`, `spawn_dreamer()` is never called and `dreamer_handle` on the RPC server is `None`.

## RPC Methods

### `TriggerDream`

**Handler:** `rpc.rs:787` — `handle_trigger_dream()`

No parameters required.

Sends `DreamerCommand::TriggerNow` to the dreamer task, bypassing all auto-trigger gates (threshold, cooldown, active session check). Returns immediately once the command is enqueued; the dream cycle runs asynchronously.

**Success response:**
```json
{ "triggered": true }
```

**Error response:** Returns an RPC error if the dreamer is not enabled (`MOTHERSHIP_DREAM_ENABLED=false`):
```
"Dreamer is not enabled (set MOTHERSHIP_DREAM_ENABLED=true)"
```

## Bus Events

Both events are published on the `EventBus` as `DaemonEvent` variants (bus.rs:124-131) and converted to `BusEvent` for TUI consumption via the `From<DaemonEvent>` impl (bus.rs:157).

### `DreamStarted`

- `event_type`: `"dream_started"`
- No payload fields.
- Published at the very start of `run_and_report()` before any phase runs (scheduler.rs:141).

### `DreamCompleted`

- `event_type`: `"dream_completed"`
- Payload fields:

| Field | Type | Description |
|-------|------|-------------|
| `observations_extracted` | usize | Explicit observations added from session extraction |
| `deductions_created` | usize | Deductive observations created by the deduction specialist |
| `patterns_identified` | usize | Inductive patterns created by the induction specialist |

- Published after all three phases succeed (scheduler.rs:155-159).
- On failure, a `SystemMessage` bus event with `level: "error"` is published instead (scheduler.rs:163-166).

## Key Files

| File | Role |
|------|------|
| `crates/rsid/src/dreamer/mod.rs` | Module re-exports |
| `crates/rsid/src/dreamer/scheduler.rs` | Background task, `DreamerHandle`, `DreamConfig`, three-phase orchestration |
| `crates/rsid/src/dreamer/extractor.rs` | Session text → Explicit observations via LLM |
| `crates/rsid/src/dreamer/deduction.rs` | INFER/SUPERSEDE/CONTRADICT specialist |
| `crates/rsid/src/dreamer/induction.rs` | PATTERN / confidence specialist |
| `crates/rsid/src/dreamer/llm_client.rs` | OpenAI-compatible HTTP chat client |
| `crates/rsid/src/store/observations.rs` | Observation CRUD, dream state key-value, `sessions_needing_extraction()` |
| `crates/rsid/src/bus.rs` | `DaemonEvent::DreamStarted` / `DreamCompleted` definitions and conversion |
| `crates/rsid/src/config.rs` | `MOTHERSHIP_DREAM_*` env var parsing (lines 243-277) |
| `crates/rsid/src/rpc.rs` | `TriggerDream` RPC handler (line 787) |
