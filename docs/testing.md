# Test lanes

The repository has two unfiltered parallel test lanes and one compatibility
lane. All Nextest profiles use eight workers by default, disable retries, and
keep fail-fast disabled so every failure in the selected scope remains visible.

```bash
make test-fast
make test-full
make test-serial
```

`test-fast` runs every `rsid` library test. It is the normal edit/verify loop.
`test-full` runs every workspace test through Nextest, then all workspace
doctests and the offline production model-control validator. The full target
attempts all three stages even if an earlier stage reports failures.
`test-serial` preserves the historical single-threaded `rsid` library command
for investigations that genuinely require it; it is not the default gate.

Set `NEXTEST_JOBS` to change the worker bound for a constrained machine:

```bash
make test-fast NEXTEST_JOBS=4
```

## Slow tier

The `rsid` + `rsid-store` library suite spent half its time re-proving released,
immutable historical migrations (failpoint, rewind, replay and hostile-classifier
matrices, `store::tests` `h1_*`, `d04`/`d05`, `v95`, `v97`, `v112`/`v113`, ...). Those
tests matter when a migration or the migration runner changes; they do not need to run
on every landing gate, every `scoped_test` and every QA sweep (#2270). They form a
**slow tier**, and it is the only quarantine the profiles carry. It is an exact list,
never a wildcard, and it is mechanical:

- `.config/slow-tier.toml` names the tests (`tests`) and the paths that make a diff run
  them (`trigger_paths`: the `store/migrations/` tree, `tools/released-migrations.json`,
  the migration runner and its helpers, `build.rs`, the tier's own config).
- `.config/nextest.toml` `[profile.default] default-filter` leaves exactly those tests
  out, so `rsid-fast` (the merge-queue gate), `test-timings` and the default profile skip
  them. The line is generated: `python3 scripts/check-touched-shards
  --write-slow-tier-filter`; `scripts/tests/test_slow_tier.py` fails on drift or on a name
  no source defines.
- `ci-full` and `rsid-qa` set `default-filter = "all()"`: `make test-full`, the satellite
  `make rolling-library-qa`, the QA sweep (`scripts/run-rolling-qa.py`, which passes
  `--slow-tier` to `scripts/run-rsid-test-shards.sh`) and the cloud sweep run the tier.
  `--ignore-default-filter` runs the full suite from any profile.
- `scripts/check-touched-shards` decides per candidate diff (`slow_tier` in its receipt;
  `--slow-tier-only` prints just the decision): `full` when a changed path is in
  `trigger_paths` or a changed line of a file that declares a slow test lies outside a fast
  `#[test]` body (a slow test, a shared helper, a `use`); `excluded` otherwise. The lander
  (`rsi-rolling-land`, gate line `slow_tier=...`) and `scripts/scoped-test` (receipt
  `slow_tier`, `skipped_tests`) follow it: on `full` they run the tier, on `excluded` they
  do not. Explicit `--filter`s always run exactly what they name.
- `make test-slow-tier` runs only the tier. After editing a migration, run it.

Add a test to the tier by name when `make test-timings` shows a released-migration test
over about two seconds. A test that is not named runs everywhere, as before.

Membership is decided by the test's subject (#2298), not its name: a test belongs only if
it rewinds, upgrades or replays a historical schema (a rewound or copied database, a
historical fixture, a migration failpoint). A test that opens the current schema and
asserts current behaviour (a request replaying by idempotency key, a CAS winner, an event
round-trip, a trigger or CHECK predicate, a column default) stays in the default tier even
when a version number or the word "replay" appears in its name, because a diff that changes
what it observes touches no trigger path and would otherwise never run it.

A tree with no `.config/slow-tier.toml` runs the tier in full, as the selector decides, and
the lander follows it unless the tree's `.config/nextest.toml` provably excludes no test
(no file, or only `default-filter = "all()"`): a leftover exclusion is never run unrecorded
(#2299).

The other profiles contain no default filters or quarantine expressions beyond this one.
A failing test therefore remains part of both applicable lanes; the slow tier is covered
by the full QA and by every diff that can change what it proves.

## Store fixture cache

`rsid` test processes share a validated, read-only SQLite schema template in
`.rsid-current-schema-cache` beside the compiled test executable. Its key binds
the cache protocol, current schema version, and a build-time digest of Store
production sources and relevant build inputs. Publication is protected by an
OS file lock and atomic rename; each test still receives its own independent
database. Corrupt or unavailable cache infrastructure falls back to the
previous in-process template path without weakening correctness. `cargo clean`
removes the cache with the rest of the target directory.

On the implementation host, repeated execution of one exact Store fixture in
fresh Cargo test processes improved from a 0.916-second median to 0.146 seconds
(about 84%). The protected source-scanner fixture had already improved from
67.928 seconds to 18.034 seconds. These focused numbers establish that the two
dominant setup paths moved; they are not a claim that the full suite is green.

## Benchmark evidence

Capture repeated full-lane evidence only from a clean committed worktree:

```bash
make test-benchmark
```

The defaults run three samples with eight workers and publish
`target/test-suite-benchmark/local-nextest-full.json`. Override
`TEST_BENCHMARK_REPEAT`, `NEXTEST_JOBS`, or `TEST_BENCHMARK_OUT` as needed. The
benchmark performs an artifact-only warmup, then records real unfiltered test
executions; red samples stay red. Source-matched full-suite budgets are deferred
until the existing correctness backlog is closed, so no green baseline is
fabricated here.
