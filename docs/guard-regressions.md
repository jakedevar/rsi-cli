# Static guard regression gate

`scripts/repo-guard guard-regressions BASE [HEAD] [--repo REPO] [--guard GUARD ...]
[--historical-binary COMMIT=PATH ...]` compares every changed static guard: a
`scripts/check-*.py` or `tools/check-*.py` (also `check_*.py`) that still exists, and
every native guard module registered in `GUARDS` (`crates/rsi-repo-guards/src`).
The scoped-test static lane runs it with the derived merge base and candidate head;
`static_guards.guard_regressions.detail` names every lost production finding, with
guard, source path, line and finding. Unsupported guards are named, not gated. An
undeclared loss exits 1; a supported guard's adapter or execution error, or a
malformed declaration, exits 2.

Both guard versions inspect **the same committed head tree**, read directly from Git
objects (export attributes, symlinks, gitlinks, dirty files and untracked output
cannot affect the result). Added guards have no baseline; deleted guards lose their
base findings. Unchanged guards do no work. No historical source is ever
interpreted: each supported checker is rehashed against its committed blob id and
runs only through a trusted adapter.

## Adapters

* **Python-era `entries(text)` guards** (`scripts/check-target-reclaim.py`) are
  covered by a frozen native registry keyed by the exact historical blob id
  (`BuiltinAdapters`). A supported blob that is not registered is an error, never a
  skip. This compares raw detection, before policy allowlists: `debug` is
  reclaimable, but losing its detection still needs review.
* **Native guards** opt in with a `// guard-regressions: stdout` line in either
  version (`target_reclaim.rs` and `store_boundary.rs` do). The guard must accept
  `<subcommand> --root TREE`, inspect only that tree and emit only stable
  `relative/path:LINE: finding` lines. Exit 0 or 1 is accepted; exit 1 must emit a
  finding. Each version runs as the **trusted `rsi-repo-guard` built from that exact
  commit** (a `--historical-binary FULL_COMMIT=PATH` entry for the base and for the
  head), as a separate killable process group with a 120-second hard deadline,
  against an export of the head tree. This crate never compiles historical source;
  the trusted materialization (hub #3621) supplies the binaries, and a commit with
  no entry fails closed. Finding strings must stay stable; a wording change counts
  as a loss and can be declared.
* Guards with neither adapter are reported as `unsupported: PATH (...; not compared)`
  and pass this gate without executing either version. If either version supports
  an adapter, the comparison stays gated; removing an adapter fails.

Losses under `tests/`, `fixtures/`, `__tests__/`, conventional `test_*`, `*_test`,
`*_tests` directories/files, `tests.rs`, `.test` and `.spec` files are test-only.
Rust items explicitly marked `#[cfg(test)]`, `#[test]` or `#[tokio::test]` are
excluded too, with strings and nested comments masked before balancing braces. Other
code is conservatively production, including ambiguous conditional compilation.
This is source analysis, not a Rust interpreter.

Paths are carried losslessly: a path that is not valid UTF-8 appears as the
private-use stand-ins of `crates/rsi-repo-guards/src/os_escape.rs` internally and
prints as `\udcXX` escapes, as the original surrogate-escaped paths did.

The runner calls the gate as a killable child of its own binary with a 15-minute
hard stop (`converged::GUARD_REGRESSIONS_TIMEOUT`).

## Intended losses

The CLI prints a JSON declaration template for each undeclared loss. Copy exact
entries into a committed list at `scripts/guard-regressions-intended.json`, and
replace each reason with a reviewable explanation. Each entry requires `guard`,
`base_guard`, `head_guard` (Git blob hashes; null for deletion), `path`, `line`,
`finding`, and a nonempty `reason`. No wildcards or file-wide exemptions exist.
The hash pair prevents reusing an old approval for a later guard edit. Historical
declarations can remain; they approve only their exact transition.

`--guard` limits a historical replay to a named guard; normal verification omits it
and discovers all changed guards. The #515 case (rolling's broader target-reclaim
guard against #515's narrower one over #515's tree) is covered by the
`frozen_historical_comparison_detects_the_scoped_alias_regression` test, which
reads the real historical checker blobs from Git as data.

Tests: the `guard_regressions` module of `crates/rsi-repo-guards`.
