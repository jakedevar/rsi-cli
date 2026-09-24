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

The profiles in `.config/nextest.toml` contain no default filters or quarantine
expressions. A failing test therefore remains part of both applicable lanes.

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
