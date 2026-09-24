# Tag System

Tags provide lightweight classification for sessions. Phase 1 (P1.1 + P1.5)
ships a multi-tag join table alongside a legacy single-tag column for backward
compatibility.

---

## Tag Normalization

Source: `crates/rsi-common/src/tag.rs`

Every raw tag input passes through `normalize_tag()` before touching the DB.
This function is shared by the daemon and TUI (`rsi-common`).

### Steps

1. **Trim** surrounding whitespace.
2. **Lowercase** the entire string.
3. **Collapse** internal whitespace runs into a single `-` (using
   `WHITESPACE_RE = \s+`).
4. **Validate** against `^[a-z0-9][a-z0-9\-_/]*$`.

### Rules

| Rule | Detail |
|------|--------|
| Must start with `[a-z0-9]` | Leading hyphen, underscore, slash are rejected |
| Allowed body chars | `[a-z0-9\-_/]` — lowercase alnum, hyphen, underscore, forward-slash |
| Uppercase → lowercase | Always |
| Spaces → hyphen | `"foo bar"` becomes `"foo-bar"` |
| Empty (after trim) | `TagError::Empty` |
| Fails regex | `TagError::Malformed` |

### Error type

```rust
pub enum TagError {
    Empty,     // trimmed input is empty
    Malformed, // fails the regex after normalization
}
```

Mapped to `DaemonError::InvalidParam("tag_malformed: <raw>")` in the daemon.

### Examples

| Input | Output |
|-------|--------|
| `"FooBar"` | `"foobar"` |
| `"  foo  "` | `"foo"` |
| `"foo bar baz"` | `"foo-bar-baz"` |
| `"foo-bar_baz/qux"` | `"foo-bar_baz/qux"` |
| `"-foo"` | `TagError::Malformed` |
| `"foo!"` | `TagError::Malformed` |
| `""` | `TagError::Empty` |
| `"   "` | `TagError::Empty` |

### Idempotency

`normalize_tag(normalize_tag(x)) == normalize_tag(x)` for all valid inputs.
The normalization pipeline is safe to run multiple times. Source:
`crates/rsi-common/src/tag.rs:114-121`

---

## Dual-Column Architecture

### Why two columns?

The multi-tag rollout is incremental. Pre-P1.5 code (queries, TUI filters,
sort logic) reads `sessions.tag` as a scalar string. Replacing that with a
join query everywhere in one commit is a large flag day. Instead:

- `sessions.tag`: single "primary tag" string, always kept in sync as a
  derived summary. Legacy callsites continue to work.
- `session_tags`: authoritative multi-tag join table. P1.5 owns all writes.

### `sessions.tag` semantics

| State | Value |
|-------|-------|
| Session has tags | Alphabetically-first normalized tag from `session_tags` |
| Session has no tags | `""` (empty string, never NULL) |

Updated by `refresh_legacy_tag_column()` (in `crates/rsid/src/session/tag_ops.rs:15-35`)
after every mutation to `session_tags`. Called inside the same transaction or
immediately after a mutation to guarantee consistency.

### `session_tags` semantics

```
PRIMARY KEY (session_id, tag)
```

- Each row is one `(session, normalized-tag)` pair.
- No duplicates possible — the composite PK enforces uniqueness at the DB layer.
- `INSERT OR IGNORE` in `add_session_tag` makes add idempotent.
- `ON DELETE CASCADE` from `sessions(id)` — deleting a session cascades
  `session_tags` rows automatically (enforcement depends on FK pragma).

---

## Hydration Timing

`Session.tags` (the `Vec<String>` field in the Rust struct) is NOT stored in
the `sessions` table. It is populated by `hydrate_tags()` at read time.

### When does hydration happen?

Hydration runs in every Store read path that returns `Session` objects:

| Store method | Hydrates? |
|-------------|-----------|
| `list_sessions()` | Yes — `crates/rsid/src/store/sessions.rs:319` |
| `get_session(id)` | Yes — `crates/rsid/src/store/sessions.rs:341` |
| `list_archived_sessions()` | Yes — `crates/rsid/src/store/sessions.rs:430` |

### How hydration works

`hydrate_tags()` (`crates/rsid/src/store/sessions.rs:442`) runs a single
batched query:

```sql
SELECT session_id, tag FROM session_tags
WHERE session_id IN (?, ?, ...)
ORDER BY session_id, tag ASC
```

It maps the results back onto the `Session` structs in memory. Tags are
sorted alphabetically; the first becomes `session.tag` (overwriting the
column value already loaded from the base SELECT — these should match, but
hydration is authoritative).

Sessions with no `session_tags` rows keep `tags: vec![]` and `tag: ""`.

### LaunchSession / spawn path

Freshly-spawned sessions start with `tag = ""` and `tags = vec![]` (set in
`spawn_coordinator.rs`). Tags are added post-launch via `UpdateSessionTags`,
`AddSessionTag`, or the `tags` field in `LaunchSessionParams` (which the
daemon normalizes and inserts into `session_tags` immediately).

---

## "Untagged" Semantics

A session with no rows in `session_tags` is untagged. The TUI may render an
"Untagged" pill for these sessions.

| Condition | `sessions.tag` | `session_tags` rows | `Session.tags` |
|-----------|----------------|---------------------|----------------|
| Untagged | `""` | 0 rows | `vec![]` |
| One tag | `"foo"` | 1 row | `["foo"]` |
| Two tags | `"apple"` (first alpha) | 2 rows | `["apple", "zebra"]` |

---

## Deduplication

`UpdateSessionTags` deduplicates after normalization (sort + dedup before
insert). Tags that normalize to the same string (e.g. `"FOO"`, `"Foo"`,
`"foo"`) are collapsed to one entry.

`AddSessionTag` uses `INSERT OR IGNORE`, which is idempotent at the DB level.
No dedup needed on the single-add path.

---

## Tag Characters in Practice

The slash `/` in the allowed set enables hierarchical tagging:

```
ci/lint
ci/test
feature/auth
team-a/backend
```

This is purely convention — the daemon treats these as opaque strings. The
`ListTags` prefix filter (`LIKE 'ci/%'`) makes the hierarchy queryable.

---

## See Also

- [README.md](README.md) — Phase 1 overview and Quickstart
- [rpc-reference.md](rpc-reference.md) — `UpdateSessionTags`, `AddSessionTag`, `RemoveSessionTag`, `ListTags` RPC method details
- [schema.md](schema.md) — V43 migration that added `sessions.tag` and `session_tags` table
