---
name: db
description: Interact with the rsi SQLite database — discover schema, query state, and respect persistence invariants
---

# Database

You are tasked with interacting with the rsi SQLite database safely and efficiently. This skill tells you **how to discover the schema** and **which access path to use** — it deliberately contains **no hardcoded table or column list**, because that would drift the moment a migration lands. The live database is the source of truth; introspect it.

## Locations

| Resource | Path | Notes |
| --- | --- | --- |
| SQLite database | `~/.rsi/rsi.db` | Override with `$RSI_DB` (`${RSI_DB:-$HOME/.rsi/rsi.db}`) |
| Unix socket | `~/.rsi/daemon.sock` | JSON-RPC endpoint; daemon must be running for RPC |
| Schema truth (code) | `crates/rsid/src/store/mod.rs` | `init_schema()` migrations + `LATEST_SCHEMA_VERSION` |

## Discover the schema (do this FIRST — never guess columns)

Use the read-only `rsi-diag schema` subcommand. It reads the **live** DB, so it is never stale:

```bash
cargo run -q -p rsi-diag -- schema                 # list every table with row counts
cargo run -q -p rsi-diag -- schema sessions        # columns, indexes, foreign keys for one table
cargo run -q -p rsi-diag -- schema --grep token    # every table/column whose name contains "token"
cargo run -q -p rsi-diag -- schema --version       # PRAGMA user_version (schema applied to this DB)
```

Add `--json` to any of the above for machine-readable output. Add `--db <PATH>` to target a non-default database.

`--grep` is the "retrieve **parts** of the schema" path: pull only the slice you need (e.g. `--grep sandbox`, `--grep parent`) instead of ingesting all 40+ tables.

If `rsi-diag` is not built, `sqlite3 "$RSI_DB" ".schema <table>"` and `sqlite3 "$RSI_DB" ".tables"` are an equivalent read-only fallback.

## Choose the right access path

1. **Prefer the daemon RPC for normal session flows and any mutation.** RPC preserves the status machine, timestamps, UUIDs, and validation invariants. Use the client:
   ```bash
   cargo run -q -p rsi-common --bin rsi-rpc -- <Method> --params '{...}'
   ```
   Read methods include `GetSession`, `ListSessions`, `GetConversation`, `GetTurnMetrics`, `ListProjects`, `GetHealthStatus`, `GetDaemonCapabilities`. See the RPC surface in `AGENTS.md` and `crates/rsid/src/rpc.rs` for the full method list and param shapes.

2. **Raw `sqlite3` is allowed for read-only diagnostics, repairs, and bulk fixes** when it is the right tool:
   ```bash
   sqlite3 "$RSI_DB" "SELECT id, status, created_at FROM sessions ORDER BY created_at DESC LIMIT 5;"
   ```
   Confirm column names with `rsi-diag schema <table>` first.

## Hard rules for any write (from AGENTS.md "Database Rules")

Only break out raw SQL writes with explicit user consent. When you do:

1. **Never change schema without a versioned migration** in `crates/rsid/src/store/` plus a matching `PRAGMA user_version` bump and the `LATEST_SCHEMA_VERSION` constant.
2. **Never hard-delete rows without explicit user consent.** Prefer logical delete, archive, or a status transition.
3. **Timestamps** must be RFC3339 with nanosecond precision.
4. **UUIDs** must be lowercase canonical strings.
5. **Enum strings** must match the serde variant names exactly (e.g. `SessionStatus`, `capability_class` snake_case).
6. **Sandbox tombstone updates** must flip cleanup state and null the sandbox path/branch atomically.

## Remember

- Discover before you query: run `rsi-diag schema <table>` so you never go back and forth on missing or misnamed columns.
- Read-only by default. Mutations go through the daemon RPC unless the user explicitly approves raw SQL.
- The schema is large and evolving (`user_version` is in the 60s). Pull only the slice you need with `--grep`.
