# Entity Cards

## Overview

Entity cards are structured, user-managed knowledge documents that get injected into every session's context at launch. They represent persistent facts about two kinds of entities: the active **project** (tech stack, conventions, current focus) and the **user** (working style, tool preferences, patterns). Unlike dynamic observations or summaries, card facts are curated manually by the user and are always included verbatim — no retrieval, no ranking.

Cards provide instant grounding for new sessions without relying on conversation history or memory search.

## Data Model

Defined in `crates/rsi-common/src/types.rs:601`.

```
EntityCard {
    id: Uuid,
    entity_type: String,   // "project" | "user"
    entity_id: String,     // project UUID string, or "self" for user card
    facts: Vec<String>,    // ordered, max 40 entries
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}
```

- `entity_type` is a plain string discriminant, not an enum. Only `"project"` and `"user"` are used in practice.
- `entity_id` for project cards is the project's UUID serialized to string. For the user card it is the literal string `"self"`.
- `facts` is an ordered `Vec<String>`. Order is preserved and user-controlled (reorderable in the TUI editor). The constant `EntityCard::MAX_FACTS = 40` is enforced by both the daemon and the TUI (see `types.rs:615`).

There is no `EntityType` enum; the type is a free-form string keyed by the `(entity_type, entity_id)` unique constraint.

## Storage

### Migration

Added in schema migration V29 (`crates/rsid/src/store/mod.rs:506`).

```sql
CREATE TABLE IF NOT EXISTS entity_cards (
    id          TEXT PRIMARY KEY,
    entity_type TEXT NOT NULL,
    entity_id   TEXT NOT NULL,
    facts       TEXT NOT NULL DEFAULT '[]',
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL,
    UNIQUE(entity_type, entity_id)
);
CREATE INDEX IF NOT EXISTS idx_entity_cards_lookup
    ON entity_cards(entity_type, entity_id);
```

`facts` is stored as a JSON array (`serde_json::to_string`). Timestamps use nanosecond RFC 3339 format, consistent with other store columns.

### Store Methods

All CRUD lives in `crates/rsid/src/store/cards.rs` as `impl Store`:

| Method | Signature | Notes |
|---|---|---|
| `get_entity_card` | `(&self, entity_type, entity_id) -> Result<Option<EntityCard>>` | Returns `None` if no card for this entity |
| `upsert_entity_card` | `(&self, card: &EntityCard) -> Result<()>` | `INSERT OR REPLACE` on `(entity_type, entity_id)`; preserves `id` and `created_at` for existing cards via conditional logic in `SessionManager` |
| `delete_entity_card` | `(&self, entity_type, entity_id) -> Result<()>` | Hard delete |
| `list_entity_cards` | `(&self, entity_type) -> Result<Vec<EntityCard>>` | Returns all cards of a given type, ordered by `entity_id ASC` |

`SessionManager` wraps store access in `spawn_blocking` calls (see `crates/rsid/src/session/cards.rs`). `set_entity_card` on `SessionManager` enforces `MAX_FACTS`, loads any existing card to preserve its `id` / `created_at`, then delegates to `store.upsert_entity_card`.

## Context Pipeline Integration

Source: `crates/rsid/src/session/context_pipeline.rs`.

When a session launches, `ContextPipeline::assemble()` (`context_pipeline.rs:52`) gathers context from multiple sources in parallel under a 3-second master timeout. Entity cards are two of those sources:

- `gather_project_card` (`context_pipeline.rs:172`) — queries `get_entity_card("project", <project_uuid>)`. Skipped if no project is associated with the session, or if the card has no facts.
- `gather_user_card` (`context_pipeline.rs:201`) — queries `get_entity_card("user", "self")`. Always attempted; skipped if card is empty.

Both are fast synchronous SQLite reads offloaded to `spawn_blocking` (~100 µs, no external I/O, no timeout guard needed). They run in parallel via `tokio::join!` alongside memory search, project files, git log, and session summaries.

The resulting blocks are tagged for injection:

| Source | Tag in context | Priority |
|---|---|---|
| Active Task | `[Active Task]` | 0 (highest) |
| Project Workflow | `[Project Workflow]` | 1 |
| Project Card | `[Project Card]` | 2 |
| User Card | `[User Preferences]` | 3 |
| Session Summary | _(summary tag)_ | 4 |

Project and user card facts are joined with newlines (`card.facts.join("\n")`) before being placed into the block. Blocks are included in priority order up to the token budget (2% of context window).

## TUI Card Editor

### Activation

The `CardEditor` overlay is activated via the `:card` command family, parsed in `crates/rsi/src/commands.rs:168`:

| Command | Action |
|---|---|
| `:card` | Open project card editor for current project |
| `:card user` | Open user card editor |
| `:card add <fact>` | Append a fact to the current project card (no overlay) |
| `:card user add <fact>` | Append a fact to the user card (no overlay) |

Quoted facts (`:card add "fact text"`) have surrounding quotes stripped. These dispatch `LcAction::OpenProjectCard`, `LcAction::OpenUserCard`, `LcAction::AddProjectCardFact(String)`, or `LcAction::AddUserCardFact(String)`.

Action handling is in `crates/rsi/src/action_handler/overlay.rs:420`. On open, the overlay is set to `loading: true`, an async `get_entity_card` RPC call is made, and facts are populated into the overlay state. If there is no active project when `:card` is used, the user is notified.

### Overlay State

`OverlayState::CardEditor` variant in `crates/rsi/src/types.rs:1964`:

| Field | Type | Purpose |
|---|---|---|
| `entity_type` | `String` | `"project"` or `"user"` |
| `entity_id` | `String` | project UUID or `"self"` |
| `display_name` | `String` | Header label (project name or "User") |
| `facts` | `Vec<String>` | Live working copy being edited |
| `selected_index` | `usize` | Cursor position |
| `scroll_offset` | `usize` | Viewport offset for long lists |
| `editing` | `Option<String>` | `Some(text)` when inline edit is active |
| `loading` | `bool` | True while RPC fetch is in flight |
| `pending_delete` | `bool` | First `d` of `dd` chord has been pressed |

### Keybindings

Handled in `crates/rsi/src/overlay/card_editor.rs`. Two modes: navigation and inline edit.

**Navigation mode:**

| Key | Action |
|---|---|
| `j` / Down | Move cursor down |
| `k` / Up | Move cursor up |
| `g` | Jump to top |
| `G` | Jump to bottom |
| `a` | Append new empty fact, enter edit mode |
| `e` / `i` | Edit selected fact inline |
| `dd` | Delete selected fact (two-key chord) |
| `J` (Shift+j) | Move selected fact down (reorder) |
| `K` (Shift+k) | Move selected fact up (reorder) |
| `Ctrl+S` | Save without closing |
| `Esc` / `q` | Save and close |

**Inline edit mode:**

| Key | Action |
|---|---|
| `Enter` | Confirm edit (empty text deletes the fact) |
| `Esc` | Cancel edit (removes fact if it was newly added and still empty) |
| Printable chars | Append to edit buffer |
| `Backspace` | Delete last character |

Reorder operations (`J`/`K`) and `dd` deletions trigger an immediate `save_card` RPC call. `Esc`/`q` and `Ctrl+S` also save. All saves call `app.client.set_entity_card(entity_type, entity_id, facts)` which issues a `SetEntityCard` RPC.

### UI Rendering

`render_card_editor` is called from `crates/rsi/src/ui/overlay/mod.rs:646`, implemented in `crates/rsi/src/ui/overlay/card_editor.rs`. It receives the overlay state fields directly and renders the fact list with the cursor highlight, scroll position, and inline edit widget.

## RPC Methods

Defined in `crates/rsi-common/src/rpc.rs:456` and dispatched in `crates/rsid/src/rpc.rs:580`.

### `GetEntityCard`

Fetch a card by type and ID. Returns `null` JSON if no card exists.

**Params** (`GetEntityCardParams`):
- `entity_type: String`
- `entity_id: String`

**Response:** `EntityCard | null`

### `SetEntityCard`

Full replacement of a card's facts. Preserves the card's `id` and `created_at` if the card already exists. Enforces the 40-fact limit server-side.

**Params** (`SetEntityCardParams`):
- `entity_type: String`
- `entity_id: String`
- `facts: Vec<String>`

**Response:** `EntityCard` (the saved card with updated `updated_at`)

There is no separate delete RPC exposed to the TUI. The store's `delete_entity_card` and `list_entity_cards` methods exist for internal use but are not exposed over the socket.

## Key Files

| File | Role |
|---|---|
| `crates/rsi-common/src/types.rs:601` | `EntityCard` struct definition and `MAX_FACTS` constant |
| `crates/rsi-common/src/rpc.rs:456` | `GetEntityCardParams` / `SetEntityCardParams` |
| `crates/rsid/src/store/mod.rs:506` | V29 schema migration |
| `crates/rsid/src/store/cards.rs` | Store CRUD: `get_entity_card`, `upsert_entity_card`, `delete_entity_card`, `list_entity_cards` |
| `crates/rsid/src/session/cards.rs` | `SessionManager` async wrappers (`get_entity_card`, `set_entity_card`) |
| `crates/rsid/src/session/context_pipeline.rs:172` | `gather_project_card` and `gather_user_card` — pipeline integration |
| `crates/rsid/src/rpc.rs:1527` | RPC handlers `handle_get_entity_card`, `handle_set_entity_card` |
| `crates/rsi/src/commands.rs:168` | `:card` command parsing |
| `crates/rsi/src/modalkit_types.rs:352` | `LcAction::OpenProjectCard`, `OpenUserCard`, `AddProjectCardFact`, `AddUserCardFact` variants |
| `crates/rsi/src/action_handler/overlay.rs:420` | Action dispatch — opens overlay, loads card, handles add-fact shortcuts |
| `crates/rsi/src/types.rs:1964` | `OverlayState::CardEditor` variant |
| `crates/rsi/src/overlay/card_editor.rs` | Input handler for both navigation and inline edit modes |
| `crates/rsi/src/ui/overlay/card_editor.rs` | Ratatui rendering for the card editor overlay |
