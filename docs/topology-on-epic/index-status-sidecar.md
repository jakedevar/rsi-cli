# INDEX.status.json Sidecar — Reference (P1.9)

Machine-managed companion file to `INDEX.md` for tracking per-ticket pipeline
status in `thoughts/shared/projects/<project>/`.

---

## Why a Sidecar?

`INDEX.md` is human-authored narrative. `INDEX.status.json` is machine-authored
state. Keeping them separate means:

- Agents can update ticket status via RPC without touching the human-readable index.
- Git diffs on the sidecar are clean (deterministic BTreeMap key order).
- The sidecar is the authoritative source for automated status queries; `INDEX.md`
  is the authoritative source for human-readable descriptions and design decisions.

---

## File Location

```
thoughts/shared/projects/<project>/INDEX.status.json
```

Collocated with `INDEX.md` for editor proximity. The `<project>` slug is validated
against `^[a-zA-Z0-9_-]+$` before any path construction.

---

## Schema

```json
{
  "schema_version": 1,
  "project": "topology-on-epic",
  "last_updated": "2026-05-16T00:00:00Z",
  "tickets": {
    "P1.1": {
      "status": "shipped",
      "last_shipped_commit": "929bca1b",
      "last_shipped_at": "2026-05-16T13:30:00Z",
      "last_shipped_branch": "P1.4-topology-rpc-surface"
    },
    "P1.9": {
      "status": "in_progress",
      "last_shipped_commit": null,
      "last_shipped_at": null,
      "last_shipped_branch": null
    }
  }
}
```

### Field Reference

| Field | Type | Description |
|-------|------|-------------|
| `schema_version` | `u32` | Always `1` for this revision. |
| `project` | `string` | Project slug — matches directory name. |
| `last_updated` | `DateTime<Utc>` | Timestamp of the last write (RFC 3339). |
| `tickets` | `BTreeMap<string, IndexTicketStatus>` | Per-ticket records, sorted alphabetically. |

### `IndexTicketStatus` Fields

| Field | Type | Description |
|-------|------|-------------|
| `status` | `IndexStatusValue` | Current pipeline status (see below). |
| `last_shipped_commit` | `string \| null` | Short commit SHA when last shipped. |
| `last_shipped_at` | `DateTime<Utc> \| null` | Timestamp when last shipped. |
| `last_shipped_branch` | `string \| null` | Branch name when last shipped. |

**Note:** `last_shipped_*` fields are automatically set to `null` whenever
`status != shipped`. Callers do not need to clear them explicitly.

---

## Status Enum

| Value | Meaning |
|-------|---------|
| `not_started` | Ticket exists but work has not begun. |
| `ready` | Fully specified, blocked only on prior tickets. |
| `in_progress` | Active implementation underway. |
| `shipped` | Implementation merged; `last_shipped_*` fields populated. |
| `blocked` | Waiting on external dependency or unresolved decision. |

---

## RPC Reference

### `UpdateIndexStatus`

Creates or updates a single ticket entry in the sidecar.

**Params:**
```json
{
  "project": "topology-on-epic",
  "ticket_id": "P1.9",
  "status": "shipped",
  "last_shipped_commit": "abc123",
  "last_shipped_branch": "P1.9-index-status-sidecar"
}
```

- `last_shipped_commit` and `last_shipped_branch` are optional (`null` if omitted).
- `last_shipped_at` is set automatically to the current UTC time when `status = shipped`.
- When `status != shipped`, all `last_shipped_*` fields are silently set to `null`.

**Returns:** `{"ok": true}`

**Errors:**
- `InvalidParam` — project name fails `^[a-zA-Z0-9_-]+$` validation.
- `InvalidParam` — no workspace root configured (`RSI_WORKSPACE_ROOTS` empty).
- `Rpc` — filesystem I/O failure.

---

### `GetIndexStatus`

Reads the full sidecar for a project.

**Params:**
```json
{
  "project": "topology-on-epic"
}
```

**Returns:** Full `IndexStatusSidecar` JSON object.

**Errors:**
- `InvalidParam` — project name fails validation.
- `InvalidParam` — sidecar file does not exist yet.
- `Rpc` — filesystem I/O or parse failure.

---

## Bootstrap Workflow

Use `tools/bootstrap-index-status.sh` to seed an `INDEX.status.json` from an
existing `INDEX.md`:

```bash
# Build rsi-rpc first (requires running daemon)
cargo build -p rsi-common --bin rsi-rpc

# Seed from INDEX.md (requires a running rsid daemon)
./tools/bootstrap-index-status.sh topology-on-epic \
    thoughts/shared/projects/topology-on-epic/INDEX.md
```

The script:
1. Parses ticket IDs (`P1.1`, `P2.3`, etc.) from markdown table rows.
2. Infers status from the row text (shipped/in_progress/blocked/ready → `not_started` fallback).
3. Calls `UpdateIndexStatus` via `rsi-rpc` for each ticket.

For projects where the daemon is not running, write the JSON directly. The
`topology-on-epic/INDEX.status.json` sidecar was seeded this way at P1.9 ship time.

---

## Atomic Write Guarantee

Every write uses `tempfile::NamedTempFile::new_in(parent_dir)` + `.persist(target)`.
The temp file is created in the **same directory** as the target, so the rename
is atomic on POSIX (no cross-filesystem rename). Partial writes never corrupt the
existing sidecar.

---

## TUI Client Wrappers

The TUI daemon client (`crates/rsi/src/client.rs`) exposes:

```rust
client.update_index_status(project, ticket_id, status, commit, branch).await?;
let sidecar = client.get_index_status(project).await?;
```

These are thin wrappers around the RPC methods above.

---

## See Also

- [README.md](README.md) — Phase 1 overview and Quickstart
- [rpc-reference.md](rpc-reference.md) — `UpdateIndexStatus` / `GetIndexStatus` RPC method details with JSON examples
