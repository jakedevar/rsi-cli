# Database Schema — Topology-on-Epic Phase 1

Two migrations ship in Phase 1. Both are additive (no DROP, no column type
changes) and safe to roll back at the SQLite level by decrementing
`user_version`.

---

## V43 Migration (P1.1)

Source: `crates/rsid/src/store/mod.rs:789-830`

Adds three schema objects. All DDL is idempotent (`CREATE ... IF NOT EXISTS`,
`add_column_if_not_exists`).

### `sessions.tag` column

```sql
ALTER TABLE sessions ADD COLUMN tag TEXT NOT NULL DEFAULT '';
CREATE INDEX IF NOT EXISTS idx_sessions_tag ON sessions(tag);
```

| Attribute | Value |
|-----------|-------|
| Type | `TEXT NOT NULL DEFAULT ''` |
| Null semantics | Never NULL — empty string means "no primary tag" |
| Population | Set by `refresh_legacy_tag_column()` to the alphabetically-first tag in `session_tags` for this session |
| Legacy purpose | Single-tag compatibility; pre-P1.5 callsites that read `sessions.tag` continue to work |

**Why dual columns?** The multi-tag rollout is incremental. `sessions.tag`
keeps pre-P1.5 TUI callsites and DB queries working without a flag day.
`session_tags` is the authoritative source; `sessions.tag` is a derived
summary. Every write to `session_tags` calls `refresh_legacy_tag_column()` to
keep them in sync.

### `session_tags` join table

```sql
CREATE TABLE IF NOT EXISTS session_tags (
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    tag        TEXT NOT NULL,
    PRIMARY KEY (session_id, tag)
);

CREATE INDEX IF NOT EXISTS idx_session_tags_tag ON session_tags(tag);
```

| Column | Type | Constraints |
|--------|------|-------------|
| `session_id` | TEXT | NOT NULL, FK → sessions(id) ON DELETE CASCADE |
| `tag` | TEXT | NOT NULL |
| — | — | PRIMARY KEY (session_id, tag) — composite, enforces uniqueness |

**FK note:** The foreign key clause is documentation-grade in V43 — SQLite
FK enforcement requires `PRAGMA foreign_keys = ON` at each connection. The
daemon does NOT currently enable this pragma; cascade deletes are handled at
the application layer. A future migration may enable FK enforcement.

**Index:** `idx_session_tags_tag` covers tag-prefix scans used by `ListTags`
and the TUI autocomplete. A safety-net call to `ensure_tag_indexes()` at
daemon startup makes this index idempotent even across schema re-runs.

### `topologies` table

```sql
CREATE TABLE IF NOT EXISTS topologies (
    id              TEXT PRIMARY KEY,
    name            TEXT NOT NULL UNIQUE,
    definition_json TEXT NOT NULL,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_topologies_name ON topologies(name);
```

| Column | Type | Notes |
|--------|------|-------|
| `id` | TEXT PK | Lowercase canonical UUID (8-4-4-4-12) |
| `name` | TEXT NOT NULL UNIQUE | Global uniqueness enforced at DB layer; daemon also pre-checks |
| `definition_json` | TEXT NOT NULL | Serialized `TopologyDefinition` (serde_json); see types |
| `created_at` | TEXT NOT NULL | RFC 3339 with nanosecond precision |
| `updated_at` | TEXT NOT NULL | RFC 3339 with nanosecond precision; bumped on every `UpdateTopology` |

**`definition_json` format:** The `until` field uses a tagged-enum encoding:
```json
{"type": "lead_halt"}
{"type": "max_iterations", "value": 5}
{"type": "predicate", "value": "some-predicate-string"}
```
Source: `crates/rsi-common/src/types.rs:1053-1062`

**Name uniqueness race:** The daemon does a SELECT pre-check before INSERT.
Under concurrent writes (rare: single-user tool), the DB UNIQUE constraint on
`name` is the final guard and returns a rusqlite error that the daemon maps to
`InvalidParam("topology name already exists")`.

---

## V47 Migration (P1.7)

Source: `crates/rsid/src/store/mod.rs:896-912`

Adds two columns to `sessions` and one compound index.

```sql
ALTER TABLE sessions ADD COLUMN topology_node_id TEXT;
ALTER TABLE sessions ADD COLUMN topology_iteration INTEGER NOT NULL DEFAULT 0;
CREATE INDEX IF NOT EXISTS idx_sessions_topology_node
    ON sessions(parent_id, topology_node_id);
```

### `sessions.topology_node_id`

| Attribute | Value |
|-----------|-------|
| Type | `TEXT` (nullable) |
| Null semantics | `NULL` = unbound session (no topology node binding) |
| Population | Set at spawn time by `SpawnCoordinator` when `directive.topology_node` is `Some`; `NULL` for manual spawns and pre-P1.7 rows |
| Deserialization | `#[serde(default)]` → `Option<String>` — V46-era JSON without this field deserializes to `None` |

### `sessions.topology_iteration`

| Attribute | Value |
|-----------|-------|
| Type | `INTEGER NOT NULL DEFAULT 0` |
| Null semantics | `0` = first iteration or unbound; never NULL |
| Hard cap | `MAX_ITERATIONS = 32` enforced at spawn time by `SpawnCoordinator` |
| Deserialization | `#[serde(default)]` → `u32` — old JSON deserializes to `0` |

### `idx_sessions_topology_node`

```sql
CREATE INDEX IF NOT EXISTS idx_sessions_topology_node
    ON sessions(parent_id, topology_node_id);
```

Compound index used by the duplicate-binding query in `SpawnCoordinator`:
```sql
SELECT COUNT(*) FROM sessions
WHERE parent_id = ?1 AND topology_node_id = ?2 AND topology_iteration = ?3
```
Source: `crates/rsid/src/session/spawn_coordinator.rs:576-588`

The `topology_iteration` column is not in the index because the query already
scans a small set of children per Epic.

---

## Migration Rollback Notes

| Migration | Rollback safety |
|-----------|-----------------|
| V43 | Safe — additive only. Pre-V43 daemon ignores the new columns/tables. To re-run: `PRAGMA user_version = 42`. |
| V47 | Safe — additive only. Pre-V47 daemon ignores `topology_node_id` / `topology_iteration`. To re-run: `PRAGMA user_version = 46`. |

**Warning:** `user_version` stays at the current level after rollback. You
must manually decrement it to force the migration to re-run upward. This
matches the sandbox pattern noted in CLAUDE.md.

---

## Full Column Reference — `sessions` table additions (Phase 1)

| Column | Migration | Type | Default | Nullable |
|--------|-----------|------|---------|----------|
| `tag` | V43 | TEXT | `''` | No |
| `topology_node_id` | V47 | TEXT | NULL | Yes |
| `topology_iteration` | V47 | INTEGER | `0` | No |

All pre-existing columns (including `workflow_id`, `workflow_id_override`) are
unchanged by Phase 1.

---

## See Also

- [README.md](README.md) — Phase 1 overview and Quickstart
- [spawn-child-grammar.md](spawn-child-grammar.md) — V47 `topology_node_id` / `topology_iteration` columns used by `/spawn_child` kwargs
- [topology-validation.md](topology-validation.md) — DAG validators that run before any `topologies` table insert
