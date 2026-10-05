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
- New migrations: one file `src/store/migrations/vNNN.rs` (see AGENTS.md hard
  rule 3). `build.rs` collects the files and hashes `src/store` for the test
  schema template.
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
- A test that needs an `rsid` module (codegraph host, native tools, monitor,
  provider capabilities, the RPC error mapping) cannot live here: put it in
  `crates/rsid/src/store_split_tests/`.
- Test shards: a gate line `#[cfg(any(not(feature = "test-shard-mode"), feature =
  "test-shard-store-01"))]` names a shard declared in this crate's `Cargo.toml`.
  The shard names are shared with `rsid`; `scripts/run-rsid-test-shards.sh`
  runs one shard across both packages and
  `python3 scripts/check-rsid-test-shards.py --require-gates` checks them.
- Released migration regions (`RSI-RELEASED-MIGRATION-BEGIN/END`) are
  byte-immutable; `python3 tools/check-released-migrations.py` guards them.
