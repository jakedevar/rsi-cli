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
| Schema truth (code) | `crates/rsid-store/src/store/migrations/vNNN.rs` (runner: `store/mod.rs`) | One file per version; `crates/rsid-store/build.rs` discovers the files and generates `LATEST_SCHEMA_VERSION` from the highest version |

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

If `rsi-diag` is not built, use the read-only SQLite fallback (replace `<table>` with a discovered table name):

```bash
sqlite3 -readonly "${RSI_DB:-$HOME/.rsi/rsi.db}" ".tables"
sqlite3 -readonly "${RSI_DB:-$HOME/.rsi/rsi.db}" ".schema <table>"
```

## Choose the right access path

1. **For an rsi-managed agent, start with `AgentGetAuthorityCatalog`.** Use only the permitted `Agent*` controls it lists, within their current scope. Prefer their native `rsi_control_*` tools; `rsi-rpc <AgentVerb>` is the validated fallback:
   ```bash
   rsi-rpc AgentGetAuthorityCatalog --params '{}'
   rsi-rpc AgentGetAuthorityCatalog --params '{"verb":"AgentGetStatus"}'
   ```
   The second call returns that control's schema, example and refusal codes. Use a listed control such as `AgentGetStatus`, `AgentGetProgress` or `AgentReadSessionEvents` when it covers the diagnostic. Refresh the catalog after an authority change or refusal. `$RSI_SESSION_TOKEN` is transport-only and supplied automatically; never put it in parameters, files or logs. Operator RPC methods are not callable directly by tokened agents, even for reads; changing the client or invoking it through `cargo run` does not grant authority.

2. **Use raw read-only `sqlite3` for diagnostics outside the permitted controls.** Discover tables and columns first, then query only the needed rows:
   ```bash
   sqlite3 -readonly "${RSI_DB:-$HOME/.rsi/rsi.db}" "SELECT id, status, created_at FROM sessions ORDER BY created_at DESC LIMIT 5;"
   ```
   Confirm these column names with `rsi-diag schema sessions` or `.schema sessions` before running the example. Read-only diagnostics do not authorize repairs or bulk writes.

3. **Prefer permitted daemon controls for mutations.** They preserve the status machine, timestamps, UUIDs and validation invariants. When an operator operation is needed, only an appointed manager with the required operator-saved grant and current catalog permission may use `AgentManagerControl` with action `operator_call` and `call: {"method":"<allowlisted method>","params":{...}}`. The catalog supplies the current delegation allowlist, grants and schema; delegation never permits a direct operator RPC call. A worker without that authority records the needed operation in its handoff for the manager or operator; it does not call manager controls. Raw SQL writes require the explicit consent and invariants below.

## Hard rules for any write (from AGENTS.md "Database Rules")

Only break out raw SQL writes with explicit user consent. When you do:

1. **Never change schema without a new versioned migration file** at `crates/rsid-store/src/store/migrations/vNNN.rs`. Choose the highest migration version on `rolling` plus one; put the `if version < N` block and matching `PRAGMA user_version` bump in that file. `build.rs` discovers the files and generates the runner list and `LATEST_SCHEMA_VERSION`; do not edit a shared list or constant. Released migrations are immutable.
2. **Never hard-delete rows without explicit user consent.** Prefer logical delete, archive, or a status transition.
3. **Timestamps** must be RFC3339 with nanosecond precision.
4. **UUIDs** must be lowercase canonical strings.
5. **Enum strings** must match the serde variant names exactly (e.g. `SessionStatus`, `capability_class` snake_case).
6. **Sandbox tombstone updates** must flip cleanup state and null the sandbox path/branch atomically.

## Remember

- Discover before you query: run `rsi-diag schema <table>` so you never go back and forth on missing or misnamed columns.
- Read-only by default. Use catalog-permitted agent controls, delegated `operator_call` only with manager authority, or explicitly approved raw SQL writes.
- The schema evolves with migrations. Read the applied version with `rsi-diag schema --version`, and pull only the slice you need with `--grep`.
