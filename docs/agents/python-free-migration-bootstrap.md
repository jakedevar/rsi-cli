# Migration tooling bootstrap interfaces (#361)

The operator forbids Python execution, including legacy wrappers, hooks and
tests. The Rust migration libraries are staged; this note describes the caller
contract required before switching the remaining canonical paths. It is not an
acceptance receipt or a claim that those paths are ready.

## Shared APIs

`rsi-repo-guards` provides:

| Module | Entry points | Command |
| --- | --- | --- |
| `released_migrations` | `check(ctx, base, new)`, `refresh(ctx, base, extra_paths)`, inventory/validation helpers | `released-migrations` |
| `migration_renumber` | `inspect(repo, base, source, target, allow_noop)`, `transform(repo, unit)`, `build_candidate(repo, unit, scratch)`, `prove(repo, unit, unit_candidate, final_candidate)` | `rolling-migration-renumber` |
| `migration_declaration` | declaration digest validation and surgical refresh | `refresh-migration-declaration-digests` |

All commands use the crate's `main(args, ctx, out, err)` dispatcher. The renumber
command retains `--repo`, exact commit arguments, `--transform`, `--build`,
`--prove`, `--unit-file` and `--scratch`. Successful JSON and proof field names
remain the old tool's contract. A renumber refusal exits 2; a policy refusal
exits 1; argument errors exit 2. `inspect` returns `None` for a nonmigration
source. No caller should implement another declaration parser or infer a proof
from merely naming a file in a declaration.

Python-free author entries are `tools/check-released-migrations` and
`tools/rolling-migration-renumber` (POSIX shell delegates to `scripts/repo-guard`).
Their existence does not authorize using an untrusted local build for a
target-authenticated queue or daemon proof. The retained Python files are
source-only parity specifications while those callers converge.

`released_migrations::refresh` checks the released base before changing the pin.
The pin format and versioned migration bytes are preserved. The publication
caller must keep its clean exact-candidate check, source-owned-pin exclusion,
derived-pin commit identity, stale retry behavior and subsequent strict check.

## Trusted executable and snapshot closure

`rolling_queue/provisional_probe.rs` currently exports the target commit's
renumber tool and two support files into a shared, no-checkout private clone.
Replacing just the pathname with `scripts/repo-guard` is insufficient: that
clone has no checked-out Cargo workspace or Rust module sources. Running a
binary built from the candidate/current worktree would also change which code
decides the target's proof.

Hub #3621 owns trusted-tool materialization and Linux integration. The migration
adapter needs a daemon-authenticated executable built from the exact selected
target's closed tool snapshot. Its recorded identity must bind the commit,
input blobs and binary; file timestamps or an arbitrary existing binary do not
establish that binding. Snapshot closure includes the workspace Cargo manifest
and lock, toolchain/build configuration, the guard crate manifest and all its
transitive module/include inputs and dependency manifests/sources required by
that build. The crate's source list is changing under #423/#426/#362/#363, so a
hard-coded export of only `migration_renumber.rs` is not a closure. Materialize
the complete selected tree or derive and verify the complete build closure.
Candidate-authored executable code must not replace target-authenticated code.

Keep this executable separate from the probe repository: the private clone is
the Git object/ref/worktree space for transformation and candidate proof, not a
source of executable fallback code. The executable takes its explicit `--repo`
there. Private build/index paths and returned proof identities remain bound to
the accepted base, source, target and exact candidate parents.

## Deadline and result contract

Keep the probe subprocess boundary. Every clone/transform/inspect/build step
gets `min(45 seconds, remaining overall deadline)`. Compilation/materialization
must also fit a bounded admission/deadline path, not happen inside an unbounded
wrapper before the timed child. Cancellation and timeout must terminate owned
descendants, drain bounded output, and return `Unknown`; the command's normal
refusal exit 2 alone gives `Unprovable`. Crashes, malformed JSON and unavailable
trusted tools are not source refusals or successful proofs. Keep the existing
8 MiB stdout and 600-character refusal-detail limits. The no-op path still
performs full inspection unless the migration already landed unchanged.

An in-process call is suitable only where it preserves the caller's trust and
execution contract. In particular it cannot replace the probe's killable child
with an unbounded library call. #363 owns landing guard/lost-hunk and QA ports;
#423 owns runner/admission; #426 owns derivation. They should consume these
migration APIs without duplicating inventory, declaration or proof policy.

## Verification still required

Native exact Rust package jobs supply incremental evidence. Complete legacy
intent parity, target-authenticated dispatch, closed snapshot tests, deadline
and descendant-kill tests, caller integration, and exact-head full scoped
acceptance against `652af6e1b2486872ff05b34708aacdacae0c282c` remain required.
No old Python execution is an allowed parity oracle. Use the retained source,
existing fixtures and pinned byte/diagnostic expectations. Full acceptance
stays held until the shared Python-free bootstrap converges.

The native fixtures now exercise conflicted refresh index-stage union and
disagreement, stacked-base declaration shape/ownership, the explicit-base
merge diagnostic, inline helper renames and three published units, inherited
and malformed/unprovable target declarations, prior-source owner diagnostics,
already-landed migrations after head advancement and differing full-file bytes,
paired fixture-query grammar, and the recorded V189 constant/attribute collision.
The serialized CLI test covers transform/build/prove plus tampered unit refusal.
These cases do not replace stale-retry, landing lost-hunk/canary or timeout and
trusted-dispatch acceptance.

The pending-sibling success fixture intentionally matches the legacy inline
layout case. Per-file gapped units omit `base_latest` in both implementations;
the default target tail then excludes the pending sibling from masking. Native
reproduction and the separate behavior fix are tracked in #438. Preserve that
distinction while verifying this tooling conversion; do not claim the per-file
case passed or silently incorporate an unrelated behavior change.

## Converted callers and the process boundary (continuation)

Converted without Python: the lander's released-migration check and pin
derivation (`landing_policy::check_released_migrations`, `derive_released_pin`),
its provisional `--transform`/`--build`/`--prove` steps (`provisional_command`),
the queue probe (`rolling_queue/provisional_probe.rs`), the pre-push hook step 2
(`scripts/repo-guard released-migrations`, base-built like its other guards) and
the CI immutability job (builds the BASE tree's guard, falls back to this tree's
only when the base predates it).

The lander and the probe run the guard as a bounded child of
`rsid::repo_guard_call` (`<exe> <guard> <args...>`, exact environment, own process
group killed on the deadline, output bound 8 MiB). The lander's child gets the
lander environment without `GIT_*`, `RSI_*`, `PYTHON*`; the probe keeps its
min(45 s, overall) step bound and Unknown-on-timeout contract.

`rsid` must never link `rsi-repo-guards`: the crate enables
`serde_json/preserve_order`, Cargo unifies features per build, and that reorders
every `serde_json::Map` in the daemon (starter digests moved, three different
values across runs). The executable is separate and built in its own cargo
invocation. `repo_guard_call::trusted_tool` is the single trust seam: today the
`rsi-repo-guard` installed beside the running binary, none installed = fail
closed (lander refuses landings touching protected paths; probe answers
Unknown). Hub #3621 replaces its answer with a target-authenticated executable;
deploy installation of the binary is tracked in #451. Tests of the wrapper use
stub executables; the real guard semantics stay in the native
`rsi-repo-guards` suites.

### Real-guard fixtures (#452)

The two lander and enqueue tests removed with their Python fixtures are back,
built in Rust (`crates/rsid/src/provisional_fixture.rs`, compiled into the lib
and the `rsi-rolling-land` test builds): `tests::provisional_stale_retry_regenerates_or_refuses_proof`
(eight variants: disjoint, store, catalog, cohort, protected, version, proof,
advance) and `session::rolling_queue_verb::enqueue_bound_tests::a_free_provisional_version_is_validated_before_enqueue_3397`.
The fixture writes the inline V129 layout, derives the pin with
`released-migrations --refresh`, authors sources with declarations, and builds
candidates with `rolling-migration-renumber` through the real executable
(`provisional_fixture::real_guard()`: `RSI_TEST_REPO_GUARD` if set, else
`scripts/repo-guard --bin`, which builds it in its own cargo invocation). A
no-op source is published as a plain merge, as the lander does. The lander test
installs the guard through a wrapper that fails the renumber proof once the gate
has marked it (the old "changed tool" variant); the `renumber_tool`,
`released_tool` and `guard_tool` variants no longer apply because those tools
are not committed code.

The lander prefers the trusted native `rolling-landing-guard` (#363) when an
`rsi-repo-guard` is installed and falls back to the committed legacy script only
when none is, so a deploy that installs the binary switches the landing guard
too; an installed guard that does not authenticate for the target is a refusal,
never a fallback.

### What deploy must install (#451, hub #3621)

`rsi-repo-guard` beside `rsid` and `rsi-rolling-land` (the lookup is
`current_exe().parent()/rsi-repo-guard`), built in its OWN cargo invocation
(never in the `cargo build --bin rsid --bin rsi-rolling-land` call). Until it is
installed: the lander refuses landings that touch protected migration paths and
the queue probe answers Unknown. Nothing may land on a source that depends on
this before the deploy ships it. A materializer that builds the guard per target
writes `<exe>.identity.json` (`{"schema":1,"commit","tree","binary_sha256"}`,
`rsid::repo_guard_call::check_identity`): a tool with a record is trusted only
for a target whose tree equals the record's `tree` and whose binary matches the
digest; without a record the installed tool is trusted as installed (interim).

### Bounded child tests

`repo_guard_call` tests cover the exact environment, the process-group kill of
descendants on the deadline (`/proc` on Linux, `kill -0` on other Unix; off Unix
only the direct child is killed and a portable test pins the failed-run path),
and the 8 MiB output cap on stdout and stderr; the queue probe has a hanging-guard
test for the shared deadline.

Not yet converted (owned elsewhere or blocked): `scripts/scoped-test` (#423) and
`scripts/check-touched-shards` (#426) still name the Python tools;
`scripts/rolling-landing-guard.py` (#363) invokes the renumber tool. The Python
tools and their suites stay as source-only parity specifications until those
callers move; delete them only then.
