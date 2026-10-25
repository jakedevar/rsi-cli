# Static helper bootstrap (#362)

The source/verification base is S1 `652af6e1b2486872ff05b34708aacdacae0c282c`.
The Python files are source evidence only: do not execute them, their tests, the
legacy scoped-test runner or its dry-run. This staged change does not establish
scoped acceptance or replace candidate provenance.

The Rust helpers are in `rsi-repo-guards`. `scripts/repo-guard` retains the
sandbox's stamped target and Rust wrapper, builds when needed and fails closed.
Until the runner and all required helpers converge, canonical callers stay
intact. In particular #423 owns `scripts/scoped-test` and `scripts/cargo-slot`;
#426 owns `check-touched-shards`; #363 owns lander/desktop/QA wiring.

| Legacy helper | Shell compatibility entry | In-process API |
| --- | --- | --- |
| `check-store-boundary.py` | `scripts/repo-guard store-boundary --root ROOT` | `store_boundary::check(&Path) -> Result<Vec<String>, String>` |
| `check-target-reclaim.py` | `scripts/repo-guard target-reclaim --root ROOT` | `target_reclaim::check(&Path) -> Result<Vec<String>, String>` |
| `scoped-test-failure.py` | `scripts/repo-guard scoped-test-failure RECEIPT.json` | `scoped_test_failure::infrastructure_interrupted(&serde_json::Value) -> bool` |
| `recent-landing-check.py` | `scripts/repo-guard recent-landing-check SOURCE ROLLING MERGED_TREE` | `recent_landing::recent_landing_deletions(repo, source, rolling, merged_tree, Limits)` |
| `check-rsid-test-shards.py` | `scripts/repo-guard rsid-test-shards [legacy flags] [--root ROOT]` | `rsid_test_shards::{read_shard_packages, source_inventory, validate_inventory, check_runtime}` |

For the two static checks, write each returned finding to stderr, with a newline;
exit 1 for nonempty findings, 0 otherwise. An unavailable/invalid scan writes
`store boundary guard: ERROR` or `target-reclaim guard: ERROR` and exits 2. The
`--root` default is the working directory; archive callers must supply the
exported candidate root explicitly. No mutable worktree may stand in for the
candidate's tree.

The failure CLI prints a JSON boolean and exits 0 for a valid receipt. Missing or
invalid JSON exits 2. Classification is advisory; #423 must preserve the runner's
existing condition `exit_code == 1 && infrastructure_interrupted(receipt)` before
adding `failure_class: infrastructure_interrupted`. In particular the helper
never changes `ok`, exit code, tested head, package records or other receipt
fields. Signal logging and receipt emission remain #423's responsibility.

The recent-landing adapter emits the advisory JSON and exits 0, including for
partial/unavailable probes. Write that identical JSON to both the advisory log
and `reverts_recent_landing` receipt field; never change the candidate verdict.
Use the net `git merge-tree` result as `MERGED_TREE`, never a three-dot diff.
The helper preserves all diagnostic argv tokens, with explicit per-argument
abbreviation and a bounded stderr tail. Runner trial-merge errors and advisory
log/receipt integration still need #423's ported tests. Non-UTF8 Git paths are
currently reported as unavailable/invalid_output, rather than being lossily
converted into incorrect warning identities; surrogate-escaped legacy path
output remains an equivalence gap to resolve before switching this lane.

The target guard retains its conservative literal/alias scan and parses the
complete Rust reclaim predicate, refusing unfamiliar grammar. #424's local
`stamped_repository(&temp)` transfer requires a unique test-only helper with an
immutable path derived from its TempDir parameter and a final return of that
same path. Unknown implementations, escaping joins, shadowing, production
arguments and standalone production writes remain subject to the guard. The
retention registry's new entry is limited to exact source-string fixture names
in the ported Rust test file; no production policy entry is made reclaimable.

Still required before the atomic switch: shard helper verification and caller
integration, plus changed-guard comparison. The latter currently executes both committed Python guard versions;
a replacement needs a safe historical adapter strategy compatible with the
operator's ban, rather than silently dropping baseline comparisons. Do not
switch or remove a required lane until its full replacement is ready. Keep the
legacy source/test files until their callers switch together.

## Authoritative shard inventory checkpoint

The staged Rust shard entry is `scripts/repo-guard rsid-test-shards`; it accepts
all legacy flags, plus `--root ROOT` for an exported committed tree. Its default
root is the context working directory. Both packages are required by the CLI.
`--shards-only --list-shards` and `--shards-only --list-shard-packages` read only
Cargo manifests; every other mode validates the source inventory, module mounts,
feature views and declared package population before returning usable output.
Runtime mode validates the selected Nextest lists and prints the runtime union
line before the static summary or listing, as the original CLI does.

Public library interfaces in `rsid_test_shards`:

- `read_shard_packages(root) -> Result<Vec<Shard>, String>` is the ordered union:
  store, session, memory, other, then numeric suffix; package order is rsid,
  rsid-store. `Shard` has `name` and ordered `packages`. No selector should infer
  this inventory from filenames or manufacture a missing package's shards.
- `read_sources(root) -> Result<Sources, String>` and
  `source_inventory(&Sources, &[Shard], require_gates, &Host)` return `Inventory`
  with ordered `tests: Vec<TestIdentity>`, `gated`, and host-local counted
  `platform_conditional` names. Each identity records source, name, line, shard.
  Ungated fallback tests follow gated identities, matching the legacy pass.
- `validate_inventory` checks unmounted tests, opt-in versus shard module
  visibility and every declared package/shard population. Call it before using
  a full inventory. Runtime exceptions cannot bypass module visibility checks.
- `mount_declarations`, `mounted_sources`, `unmounted_test_sources` and
  `check_shard_module_mounts` expose the same path/include/inline module model
  for #426 parity integration. Path-mounted files resolve their children beside
  themselves. The legacy selector has not been changed or executed.
- `runtime::check_runtime` returns the summary line, with `RuntimeExceptions`
  supplying the historical exceptions by default. Platform exceptions are held
  on each inventory, with no process-global state to leak between scans.

The 20 exact Rust shard regressions passed in job
`37853fba-4eb8-4186-9324-bac9a3a9fac5` at `d41409144`, together with seven
recent-landing regressions. This is component evidence pending full bootstrap
integration and remaining parity evidence. It is not full scoped acceptance.
The changed-guard historical adapter, non-UTF8 advisory path parity and #423
runner advisory/receipt integration still remain. Canonical callers and legacy
source specifications remain in place until the whole replacement path works.

## Historical changed-guard adapter checkpoint

`guard_regressions::{compare, compare_with}` resolves base/head commits once and
reads regular blobs from the exact committed head with `ls-tree` and `cat-file`.
It ignores export attributes, symlinks, gitlinks, dirty files and untracked build
output. Both detector versions scan that same in-memory snapshot. Every supported
checker is rehashed before adapter dispatch; losses retain guard path, base/head
blob ids (null for deletion), source path, line and finding. Declarations come
from that same committed head and require the exact six-part identity plus a
nonempty reason. Rust test-line exclusions preserve shared production lines.

The CLI is `scripts/repo-guard guard-regressions BASE [HEAD] [--repo REPO]
[--guard GUARD ...]`. Its losses, declaration templates and unsupported notices
retain the historical streams/exit semantics. Native registered guard module
paths are also eligible, so changing a supported Rust detector cannot quietly
escape comparison after the eventual cutover.

The built-in target adapters are frozen by the original Git blob identity:

| Git blob | Detector semantics |
| --- | --- |
| `81faa4333945c22c8b33dd6eecf12532e5a72e1e` | Original unscoped fixed-point aliases |
| `fb0d7260ca75130409700cbd1e8f8280bc4059c7` | Same detector, nextest policy change |
| `350d231e47ae13c534efc991aadfe9dc962c6912` | Scoped aliases, unknown bindings shadow roots |
| `b27d79cf21809643b6bdef22701cf2d0e0d78602` | Conservative parameters and unknown rebindings |
| `1b56d6ccfbab271b395038724695e7f9f5fdf817` | Conservative detector at the shared bootstrap base |

Historical modes do not inherit the new stamped-repository fixture proof or a
new reclaim allowlist. Source snapshots are read-only data, never interpreted or
executed as Python. A supported blob absent from the authenticated adapter set
is an error, never an unsupported skip. A caller that supplies another trusted
`HistoricalAdapters` implementation must dispatch exact path/blob identities and
preserve stable findings; `parse_stdout` validates native stdout adapter results.
It must never substitute the current worktree detector for historical source.

The 14 exact Rust guard-regression tests passed in job
`dce288ea-7727-4335-95a4-4a8e3729e4d4` at `a03cb73e6`, including real-Git
committed-head/declaration checks, all five registered historical blobs,
stdout adapter contracts and a small historical regression replay. The full
historical #515 repository replay remains unrun. The library checks the 120-second budget
between source files, but the original independently killable per-version worker
is not yet supplied. A Python-free trusted subprocess/supervisor adapter remains
required before switching the canonical guard lane; cooperative budget checks
are not equivalent to that hard deadline. The non-UTF8 advisory and runner
log/receipt integration remain separate outstanding work. Full scoped acceptance
is still unrun; no canonical caller has switched or lost required coverage.

The target fixture-scope regression is resolved: closing-line suffixes of
multiline literals stay in their containing statement, and adjacent literal
tokens are scanned before those suffixes are hidden. Job
`1051b6d7-fac1-4f35-a0c4-5cedc1d67932` passed all 11 target regressions,
including `ported_current_tree_passes`, at `8eb6f52b2`. They passed again at
`0511aab27` in job `962b117d-f265-4e30-bb75-a6bb027ae043`; that combined job
was red only on the subsequently fixed file-level test-exclusion bound.
#424's local fixture proof still passes; no production reclaim allowance changed.

The large-history cutoff regression now passes with a 50,000-line file, 24 old
commits and an intentionally missing obsolete fixture blob. A bounded probe
uses the source boundary and still identifies the recent deleted line. Native
advisory non-UTF8 filenames, runner receipt/log integration, independent
hard-deadline historical workers and full historical repository replay remain
owed before canonical integration. The legacy source files are specification
data only: strict no Python execution or authoring still applies to successors.

## Continuation checkpoint (68eeee54): shard runner, non-UTF8, hard deadlines

Merged by ancestry: #363 `88e205472`, #423 `80f0e757d` (already contained), #426
`bc6fb37bc`, and `origin/rolling`; `2e083de63` stays an ancestor.

* `scripts/run-rsid-test-shards.sh` no longer calls an interpreter. Its shard
  listing, runtime check and target-directory lookup run
  `scripts/repo-guard rsid-test-shards` (new `--target-directory` reads
  `cargo metadata` JSON on stdin). This unblocks #363's shard lanes (#448).
  `make test-full` and the CI target-reclaim step call the Rust guards too.
* `os_escape.rs` carries any byte path losslessly (private-use stand-ins for
  undecodable bytes, `\udcXX` on output, ambiguous valid text refused).
  `recent_landing` (argv, numstat paths, advisory JSON), `guard_regressions`
  (snapshot, template, printing), `store_boundary` and `target_reclaim` use it.
  `recent_landing::advisory`/`advisory_line` are the one call #423's runner makes
  for `reverts_recent_landing`.
* Hard deadlines: `converged::Converged::guard` runs every static gate as a
  killable child of the runner binary (30 min; guard-regressions 15 min).
  Native historical guard versions (`// guard-regressions: stdout`) run through
  `ExecutableAdapters` + `BinaryResolver`: the trusted `rsi-repo-guard` of each
  commit (`--historical-binary COMMIT=PATH`, supplied by hub #3621) in its own
  process group with a 120 s deadline; an unmaterialized commit fails closed.
* Not switched (blocked, owners named in the #362 handoff): the pre-push hook and
  `tools/test_pre_push.py`, the lander (`rsi-rolling-land.rs`), `scripts/scoped-test`,
  `check-touched-shards`, `cloud-sweep-json.py`, `shard-build-warnings.py`,
  `test-suite-benchmark.sh`. No `.py` is deleted: each still has a caller.
