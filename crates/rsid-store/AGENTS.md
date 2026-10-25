# Store crate (`rsid-store`)

The daemon's durable state: the SQLite `store` (schema migrations, every table
accessor), runtime `config`, the event `bus`, `model_control`, the `sandbox`
custody layer, the credential `vault`, `bedrock` and `idea_control`. Moved out of
`rsid` for issue #1021 (S4); `rsid` re-exports each module at its old path, so
`crate::store::...` still resolves there.

Facts that bite:

- Nothing here may depend on `rsid` (session, rpc, provider, scheduler, ...).
  A helper the store needs from a higher module moves down into `rsid-core` or
  `store_support`, and the higher module re-exports it at the old path.
- New migrations: one file `src/store/migrations/vNNN.rs` holding the `if version < N`
  block and the `user_version` bump. Its pin in `tools/released-migrations.json` is
  derived: the lander runs `--refresh` on the candidate and commits the result when it
  publishes (#1949), and the guard accepts a source whose pin file is still the base's,
  so a worker never edits that file, concurrent migrations never conflict on it, and no
  worker runs `--refresh` (run it locally only to see the pin or to clear an old
  conflict over markers; a source that already carries its own pin lands as before).
  N is the rolling tip's schema head (the highest `vNNN.rs`) + 1; the tooling verifies it.
  Every new migration also carries one `tools/provisional-migrations/*.json` declaration in
  the same source (format: that README). It is what lets the merge queue assign the final
  number at landing: when another queued source takes N first, a declared migration is
  renumbered to the next free head, and an undeclared collision is refused
  `queue_migration_out_of_order`. Recover by adding a complete provisional declaration
  for the source's existing number, committing it, and submitting the new SHA for
  re-enqueue. The queue's lander runs `scripts/repo-guard rolling-migration-renumber` to assign
  the final number and gate the transformed candidate; workers never renumber by hand.
  The renumber tool refuses a collision without a declaration with
  `one new provisional site declaration is required`.
  Every `files[]` entry of that declaration has exactly the four keys `path`,
  `path_template`, `source_blob` and `sites` (each site exactly `anchor`, `replacement`,
  `scope`); for an unversioned file such as `store/tests.rs`, `path_template` equals
  `path`. A missing key is refused with the declaration, file entry and key named by
  `scripts/repo-guard released-migrations` (scoped-test and the pre-push guard run it) and by
  the renumber tool, even when the source needs no renumber (#3304).
  A Store test assertion of the provisional schema head is a version site with
  `scope: "head"`. The renumber path requires declared Store helpers/test files
  to be pinned; for a new unpinned module, put the version constant in already
  pinned `store/tests.rs`, declare that constant's site, and reference it from
  the new module. See the README example.
  A slice built on a reviewed-but-unlanded sibling migration may skip its number by naming it in
  `pending_predecessors` of that declaration only when the sibling's migration is absent
  from the source tree (#1285). If the source contains both complete migrations,
  an origin/rolling-based `--transform` cannot preflight them as one unit (#545):
  the PM lands the sibling first, then the worker merges rolling and validates the
  remaining unit. See that README's "Preflight when the source contains an unlanded
  sibling" for the sequence and the deferred-pin base limitation.
  To let the lander renumber, declare the versioned filename with `path_template` and
  its `if version < N` gate, `REWIND_TEARDOWN_COVERED_THROUGH`, its rewind step and its
  `MIGRATION_ALTERED_OBJECTS` entry as version sites with the appropriate `unit` or
  `head` scope (see the README).
  Released migrations are immutable. Add the rewind teardown,
  the fixture/replay assertions and any replay-pin exemptions, then run
  `scripts/repo-guard released-migrations`. `build.rs` collects the files and hashes
  `src/store` for the test schema template.
  Migration author checklist (the rolling guard `scripts/repo-guard released-migrations
  origin/rolling HEAD` fails without the first two; run it before reporting migration
  verification done. Name the base: after you merge `origin/rolling` the bare form's default
  base `HEAD^` is your own commit and reports a false FAIL, #151):
  1. In `store/tests.rs` add the rewind teardown beside the V80/V81/V82/V90/V91 fixtures,
     extend `assert_fixture_matches_claimed_version` and `assert_post_v77_chain_replayed`,
     then bump `REWIND_TEARDOWN_COVERED_THROUGH` to the new head.
  2. A migration that `ALTER`s, drops, renames or rebuilds an older object changes its
     `sqlite_master` SQL, which breaks catalog-pin tests (for example
     `provider_turn_custody::tests::migration_replays_from_previous_head_without_changing_existing_rows`).
     Add one entry `(N, &["object", ...])` to `MIGRATION_ALTERED_OBJECTS` in
     `store/migrations/altered_objects.rs` (#2064). That is the only edit: catalog-pin
     tests call `assert_catalog_preserved_across_replay` and read the table, so none keeps an
     exemption of its own, and the guard refuses a NEW migration file without the entry and names
     the line to add (conservative: it reads only the migration file, so a rebuild hidden behind
     a helper not named `rebuild_*` is yours to list). The table is complete only above
     `MIGRATION_ALTERED_OBJECTS_COMPLETE_ABOVE`; a test that rewinds lower panics until the
     versions in between are backfilled.
  3. `scripts/check-touched-shards` selects every rewind/replay/catalog-pin test when a
     `migrations/vNNN.rs` changes (#2029); keep new such tests calling the rewind helpers
     so they are found.
  4. The slow tier (#2270): the released-migration history tests named in
     `.config/slow-tier.toml` are left out of the default nextest profiles and run when a diff
     touches `store/migrations/`, the pin file, the runner or such a test, and in the full QA.
     Run `make test-slow-tier` after a migration change (it needs cargo-nextest; on a host
     without it the `scoped_test` job covers the slow tier, which it runs in full when a
     migration changed, #152). A new heavy history test (over about
     2 s in `make test-timings`) is added to that file by name, then `python3
     scripts/check-touched-shards --write-slow-tier-filter` regenerates the nextest filter.
     Membership is by subject (#2298): only a test that rewinds, upgrades or replays a
     historical schema belongs; a heavy test of current behaviour (request replay, CAS,
     round-trips, trigger or CHECK predicates) stays in the default tier whatever its name says.
- Items `rsid` calls are `pub`; everything else keeps `pub(crate)`. Widen only
  what the compiler asks for.
- Test fixtures that `rsid`'s tests share (`Store::open_in_memory`,
  `test_support`, the schema template, failure-injection hooks) are compiled
  under `#[cfg(any(test, feature = "test-seam"))]`. `test-seam` is enabled only
  through the dev-dependency edges; no RPC, setting or environment variable
  turns it on and release builds never compile it. Do not put it on enum
  variants, struct fields or match arms (that would change the type's shape for
  `rsid`); keep those `#[cfg(test)]`.
- `test-seam` never ships: the build scripts of `rsid-store`, `rsid-core` and
  `rsid` refuse it in the release profile (`RSID_ALLOW_TEST_SEAM_RELEASE=1`
  opts a release-mode test run in), and `scripts/check-release-seam.sh` runs
  before `make release`, `install-release.sh` and the AWS bundle build. It
  carries fake capabilities and route-validation bypasses, so it is enabled
  only through dev-dependency edges.
- Authority-bearing entry points that `rsid` calls (`claim_*`, `for_system`)
  are `pub`; Rust has no friend crates, so the boundary is the dependency
  graph: `crates/rsid/tests/store_crate_seal.rs` fails when any crate but
  `rsid` depends on this one, and `rsid` re-exports the capability and
  `idea_control` items crate-private (the `compile_fail` doctests in its
  `lib.rs`). Raw minting (`BoundControllerWriteAuthority::new`) is
  crate-private outside the seam.
- Tests that seed manager operations (`harness_manager_v2_operations`) use the real seed path:
  `seed_manager_action_with_context_for_test` writes the operation row and its `lifecycle_context`
  record the way `enqueue_manager_action` does. Never hand-insert rows with an invented
  `payload_json`: the target session lives in the context record, not in the payload, so a
  predicate over a hand-shaped payload passes tests and matches nothing in production (#2221).
  When the Issue cites observed rows, also replay the predicate read-only against them.
- A test that needs an `rsid` module (codegraph host, native tools, monitor,
  provider capabilities, the RPC error mapping) cannot live here: put it in
  `crates/rsid/src/store_split_tests/`.
- Test shards: a gate line `#[cfg(any(not(feature = "test-shard-mode"), feature =
  "test-shard-store-01"))]` names a shard declared in this crate's `Cargo.toml`.
  The shard names are shared with `rsid`; `scripts/run-rsid-test-shards.sh`
  runs one shard across both packages and
  `scripts/repo-guard rsid-test-shards --require-gates` checks them.
- Released migration regions (`RSI-RELEASED-MIGRATION-BEGIN/END`) are
  byte-immutable; `scripts/repo-guard released-migrations` guards them.
- Stored `ToolResult` bodies are capped (#2373): `insert_event_in_transaction`, the
  one writer of `conversation_events`, stores an over-cap body as head + marker + tail
  (`rsid-core tool_result_cap`, operator setting `tool_result_storage_cap_kib`, default 32
  KiB, 0 off) and records the cut under `metadata.storage_truncation`. Storage only: the
  event the caller holds stays whole, and rows already stored are never rewritten (that is
  an operator decision, #2377). A reader of a stored ToolResult (Harness resume replay, the
  outcome parser) sees the marker text; a new consumer of ToolResult content must not
  assume the body is complete. Tests that change or depend on the cap hold
  `tool_result_cap::test_serial_guard()`.
- Startup custody publication (`publish_startup_*`) is idempotent (#2354): each
  `UPDATE` carries a `NOT (<row already holds the verdict>)` guard, so a settled
  store rewrites no row. Keep it that way: every updated
  `session_execution_projections` row also fires
  `execution_projections_v120_session_health_after_update`, and a restore that
  rewrote ~9,000 identical rows spent seconds in commits and that trigger. Use
  `IS` in the guard (NULL-safe), and cover a new startup write with
  `settled_startup_reconcile_writes_nothing_2354` and `vm_ops_during` (VM
  instruction counts, not wall clock) in `sandbox/startup_restore_tests.rs`. Keep them
  out of `store/tests.rs`: a test there selects the released-migration slow tier.
- The Session dependency-health relevance classifier is versioned (#2050). The
  V120 function stays registered and byte-immutable; V175 swaps only the
  `source_worktree_session_dependency_evidence_v120` view onto
  `rsi_swc_v175_session_dependency_relevance`
  (`store/source_worktree_v120/health_v175.rs`), and a V175+ store is checked by
  `validate_current_catalog` (retained-object fingerprint plus the exact view),
  not by `validate_v120_catalog`. The terminal-shape predicates
  (`historical_*`, `quarantined`, `invalid`) live once in
  `store/terminal_custody_shape.rs` and are used by both the health classifier
  and the settlement decoder (`cohort_settlement`). A new shape a startup
  publisher writes is added there, with a case in
  `publisher_shapes_classify_identically_for_health_and_settlement`; the next
  classifier change is a new migration that replaces the view again, never an
  edit of a released helper. Health 0 means settled history, 1 relevant (a
  quarantine with cleared Session paths still protects its retained root), 2 a
  custody contradiction.
- Every SQL string handed to a `prepare`/`execute`/`query_row`/`execute_batch` sink in
  production code must be statically resolvable: a literal, a `concat!` of literals,
  a `const &str`, or a `for (name, sql) in CONST_TABLE` loop over consts. The
  protected-DML source audit
  (`session::issue21_phase2_tests::exactly_one_production_writer_of_gate_closing_and_effect_permits`)
  reports SQL it cannot resolve (a string assembled at run time, a runtime parameter),
  whatever the statement does. Repeat shared SQL text as consts, and compare the copies in
  a test (`movable_statements_share_one_filter`).
- The cold-storage archive (#2384, `store/cold_archive/`, V176) copies the bulky rows of old
  `Archived`/`Deleted` sessions into append-only archive files (contract:
  `docs/agents/rsid-rpc-contracts.md`, "Cold-storage archive"). Slices S1-S3 stage, verify,
  read and restore; slice S4's move (`cold_archive/mover.rs`, `Store::cold_move_batch_admitted`) is
  the only production code that deletes `conversation_events` and
  `conversation_event_provenance` rows of an archived session, and only for chunks the same
  pass read back and verified (columns included), and only while a qualifying rsi.db backup is
  re-derived before every delete transaction (`cold_archive/backup.rs`, `admit_backup`: never a
  once-true flag) and the batch gate, wall budget and cancel flag admit that batch
  (`Mover::attempt`), both outside the locks and again at the transaction boundary after the
  store and SQLite write-lock waits (`cold_move_batch_admitted`, `admit_at_boundary`). A test that must prove a pass closed its SQLite connection uses
  `cold_archive::live_connections` (test-seam), not file-descriptor counts: SQLite postpones
  closing a descriptor while another connection of the process holds a lock on the file and
  reuses it for the next connection. The archive is on by default from S5
  (`COLD_ARCHIVE_ENABLED_DEFAULT`; an operator's off is a persisted `false`, see
  `a_store_with_no_cold_archive_setting_row_archives_and_an_explicit_choice_wins`).
  **A new reader of `conversation_events` must be
  classified in `cold_archive/read_classes.rs` (`COLD_READER_CLASSES`)**: a cold session's
  bulky rows are not in the live table, so a reader is correct only if it goes through
  `Store::merge_cold_events` (transcripts), reads a logical aggregate
  (`LOGICAL_EVENT_COUNT_SQL`), observes only rows the live tail keeps (plan §2.4), or
  targets a session that is never cold. `cold_archive_reader_classification` fails on an
  unclassified site and `cold_archive_reader_invariance` runs the probes before the move,
  after it and after the restore. A migration that adds, drops or retypes a column of
  either table must say how archived chunks map (`columns_json` records each chunk's
  columns; a chunk whose columns are not a subset of the live table is refused). The pass
  checks the live columns against `cold_archive::candidates::EVENT_COLUMNS` and stops with
  `cold_archive_schema_drift` first. A new foreign key into `conversation_events(id)` must be
  added to `EVENT_FK_REFERENCES` (a test compares it with `pragma_foreign_key_list`): the
  referenced row stays in the live tail.

## Model-control registry (new production HTTP POST or utility subprocess)

`rsi-model-control-validate` (a scoped_test gate) fails a production `reqwest` POST or CLI spawn
that no registry contract covers. A new fixed-host, non-model POST (a search vendor, a webhook)
needs all three edits in one round:

1. A `NON_INVOCATIONS` row in `src/model_control/registry.rs` with
   `AllowedExclusionOperation::NonModelHttpPost`, naming the path and item.
2. In that item, a module-level `const X: &str = "<literal url>"` that is the direct argument of
   exactly one `.post(X)...send()` chain. A `#[cfg(test)]` twin carries any loopback-server test;
   `cfg(not(test))` is also treated as a test item and hides the sink, so it cannot hold the
   production path.
3. The regenerated tables in `thoughts/shared/reference/model-invocation-registry.md`:
   `cargo run -p rsid --bin rsi-model-control-validate -- --print-exclusions` (and `--print` for a
   registry purpose row) prints the text to paste under the section marker. The `Markdown drift`
   error names this command.

A new utility subprocess (for example, `git` or `systemctl`) needs the corresponding steps:

1. Add a `NON_INVOCATIONS` row in `crates/rsid-store/src/model_control/registry.rs` with
   `AllowedExclusionOperation::UtilityCliSpawn`, the source path and item, a `proof_ident`
   identifying the utility executable or command factory, and the exact spawn `occurrences`.
2. Keep the spawn in that source item and tie it to the named proof. The exclusion permits
   only utility spawns, with no model executable, HTTP POST/send, or model request primitive.
3. Run `cargo run -p rsid --bin rsi-model-control-validate -- --print-exclusions` and paste the
   generated table under `## Explicit Non-Invocation Exclusions` in
   `thoughts/shared/reference/model-invocation-registry.md` in the same change.
