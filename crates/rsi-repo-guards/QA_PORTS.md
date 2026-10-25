# Staged rolling QA ports (#363)

These Rust library modules and `rsi-repo-guard` commands are staged from S1
`652af6e1b2486872ff05b34708aacdacae0c282c`. They do not switch the lander's
canonical callers or delete legacy scripts. Never execute those Python scripts,
including as comparison oracles. Compare source and pinned byte vectors instead.

| Original | Rust entry point | Behavior tests ported |
| --- | --- | --- |
| `scripts/qa_host_class.py` | `qa_fingerprint::host_class` | Both original host identity intents |
| `scripts/rolling-shard-fingerprint.py` | `qa_fingerprint::fingerprint`, command `rolling-shard-fingerprint` | All three original probe/digest intents |
| `scripts/import-rolling-qa-cache.py` | `qa_cache::import_report`, command `import-rolling-qa-cache` | All 12 original importer intents |
| `scripts/desktop-test-gate.py` selection only | `desktop_selection::lane_selected` | All 31 shared selection cases; static/transitive/workspace/target/dev/build/patch/replace edges and fail-closed fallback |
| `scripts/desktop-test-gate.py` execution foundations | `desktop_execution::{candidate_files,stage_tree,lane_env,runner_status,run_step,run_lane}` | Staged tests for whole-tree staging, Git listing, symlinks, private state, preflight, verdicts and shared process bounds; execution evidence recorded on #363 |
| `scripts/scratch_git.py` | `scratch_git::Policy::{validate,command,protected_roots,host_temp_root,tempdir}` | 29 named Rust tests mirror all original safety intents; execution evidence recorded on #363 |
| `scripts/rolling-landing-guard.py` core | `landing_guard::Repository`, `Accepted::validate`, `evaluate` | Real Git reversion/rename/binary/deletion/stale-merge/criss-cross fixtures; required backend callbacks; policy/proof/production CLI still incomplete |
| `scripts/run-rolling-qa.py` core | `rolling_qa::{plan,State,execute_pending,reports}` | Failure/count parsing, full lane plan, resume/atomic state, v1 reports; real execution backend and CLI still incomplete |

The desktop selector alone is not an executor. `desktop_execution` now stages
the whole candidate, preserves symlinks and file modes, excludes generated and
machine-local output, isolates npm/Cargo state, describes runner/WebKit
preflight, sequences npm ci/test and locked Cargo tests, counts both sets and
preserves stale-lock diagnostics. Its step adapter reuses #423's bounded process
supervisor, merged from f25f54bd6 and #433's API at 7d3fbf93d. Backend methods are
required, with no default successful implementation. `ProcessBackend` supplies
real executable preflight, the 30-second Linux WebKit probe, and test execution
through the caller's shared immutable `Isolation::wrap`; it never probes PID
isolation or holds admission itself. `run_lane_with_cancel` accepts the parent's
signal flag. `stage_tree_with_cancel` checks it between paths, parent-directory
operations and 64KiB copy chunks, preserving file permissions. Lane scratch and
desktop test fixture Git now use the shared `scratch_git::Policy`; protected
scratch is refused before allocation. Git listing uses the same supervisor's new `stream_bytes`, keeping
stdout/stderr identity, NULs and non-UTF8 filenames with a 30-second deadline and
8MiB-per-stream cap; cancellation, output loss and oversize fail closed. Plain
non-repository directories retain the filesystem-walk fallback. The shared
supervisor's Windows tree-kill helper and direct reap now have finite bounds
(#436); actual Windows execution remains unverified.

This remains a library with no runnable desktop command: outer cancellation
registration, runner/receipt wiring and complete production/suite acceptance
still need integration. No actual npm, Cargo desktop or WebKit lane is claimed
by controlled executable stubs or injected-backend tests. Source roots must be
UTF-8 for the shared command API; Unix candidate filenames preserve raw bytes.

Rolling drift: `b9e3c5f25` removed the lander's desktop lane after the S1 pin,
with cleanup in `8664f3bb8`. #430 is resolved: preserve those removals. Current
scoped-test retains desktop selection/execution, so that complete lane remains
mandatory until a published cutover removes its callers and records replacement
coverage. These staged helpers change no canonical policy.

The command context's working directory is the selected repository. Rust
callers can pass their own `Ctx` directly rather than spawn an interpreter.
Fingerprint JSON keeps sorted keys, ASCII escaping (including surrogate pairs),
compact separators, the original v1 runner/checker/config blob identities and
stdout-only version probes. Cache files retain default JSON spaces, no trailing
newline, schema version 1, directory/key folding and provenance. File locks use
the existing per-shard lock paths; writes are flushed, synced and atomically
persisted in the same directory. Reports are fully validated before publication.

Before switching canonical callers, complete the remaining paths:

- `rolling-landing-guard.py`: the matcher and virtual merge bases are staged.
  Integrate #361's migration proof validator and #362's committed target guard;
  complete the real Cargo backend, clean-worktree enforcement and CLI before cutover.
- `desktop-test-gate.py`: wire the staged selector/executor into #423's actual
  runner with its shared PID result, admission and outer cancellation. Port the remaining
  CLI, local Cargo workspace/include/lock fixture, browser descendant, Windows
  fallback and process-survival assertions; run the actual mandatory lane.
- `run-rolling-qa.py`: the typed full lane plan/state/report engine is staged.
  Complete authenticated inventory/static/shard execution, admission, bounded
  processes, classifier/closed-Issue queries and exact-head provenance in the
  production backend before registering a runnable command.
- `scratch_git.py`: the shared library and original safety tests are staged;
  migrate retained callers through it before deleting the legacy module/tests.
- Update Rust lander/queue, remote/cloud/bootstrap and other callers only with
  the corresponding complete Python-free path. The checker-blob identity in
  fingerprint v1 needs coordinated treatment when #362 replaces its old path;
  do not silently re-key historical reports or omit the checker identity.

Incremental evidence uses daemon-owned exact-name Rust package test jobs. Full
scoped, warning, lander/queue, desktop, QA and publication gates remain unrun
until #423 (runner/cargo-slot), #426 (derivation), #362 (helpers), #361 (migration
policy) and relevant #366 residual suites converge. No partial port is a green
acceptance receipt or authority to publish rolling. Lander/queue changes require
one post-land review.

## Scratch Git caller contract

`scratch_git::Policy::new(permanent_checkout, caller_cwd, environment, extra)`
requires absolute existing checkout/cwd directories and owns a native
`BTreeMap<OsString, OsString>` environment. `from_process(checkout)` captures the
real context without changing it. The permanent checkout, the caller's
ceiling-visible worktree, both worktree gitdir pointers, explicit location
overrides and extra roots stay protected. `validate` resolves the physical
target and rejects empty, relative, missing, file, filesystem-root and protected
paths. It never exempts a sandbox's own TMPDIR. Root discovery follows Git's
proper-ancestor ceilings, relative-entry skipping and the empty-entry switch
from physical to lexical symlink handling.

`command(directory, args)` revalidates before preparing `git -C <physical path>`
with repository-location variables removed and a ceiling at the scratch
directory's parent. It rejects location overrides before the subcommand,
including attached `-Cpath`, namespace/super-prefix equals forms and escapes
after config options; `commit -C HEAD` remains legal. This returns a command,
not a process: production callers must retain their existing owned supervisor,
deadline, cancellation and bounded-output contract. There is no new process
supervisor or trusted-tool materializer in this module. Treat its command as
final; do not append global options or replace the validated directory/env.

`host_temp_root` and `tempdir` choose writable external host scratch, honoring a
usable job TMPDIR under its discovery ceiling. They preserve the environment,
try disk-backed host fallback locations, validate the result and clean scratch
on drop. `host_temp_root_from`/`tempdir_in` allow explicit candidate parents and
fail closed when none qualifies. A real temporary allocation checks write
access, including ACLs. Symlink loops and inaccessible protected metadata fail
closed. The legacy scripts remain source-only until all callers migrate;
ordinary native package tests do not establish full scoped acceptance.

## Landing core and shared Git transport

`landing_guard::Repository` owns the selected repository, native environment,
overall deadline and parent's cancellation flag. Git steps use the shared
`scoped_runner::process::stream_git_bytes` pump, a 45-second step cap and 8MiB
per-pipe metadata cap. This restricted adapter admits object/history builtins,
not aliases or external Git commands; `.py` filenames can be read as source
objects without executing them. Normal runner command refusal behavior remains
unchanged. Keep this narrow process.rs extension when merging newer #423 source;
there is still only one owned supervisor, including the Windows cleanup bounds.

The staged matcher follows the legacy no-autojunk longest-block and tie-breaking
rules, keeps byte lines and invalid-byte diagnostics, requires context around
repeated insertions, and distinguishes exact restorations from later evolution.
Real Git tests cover 20%-similarity rename mapping, binary and file deletions,
source-own delta narrowing and synthetic parentless criss-cross merge bases.
`Accepted::validate` binds all four full object identities and their ancestry.
`evaluate` requires candidate-policy and test backend methods and still invokes
tests on lost hunks; injected callbacks are test evidence only. It has no
migration proof parameter or masking authority. Add independently recomputed
#361 proofs and the committed #362 target check before using it as a full gate.
No landing CLI or canonical production backend is registered yet.

## Rolling QA core

`rolling_qa::plan` consumes the authenticated shard/integration inventory and
requires all 16 distinct names from the existing v1 cache schema. Every shard
has slow tier enabled. The five companion lanes retain integration targets,
bins, docs, TUI and shared-core tests; shared-core includes `rsid-remote-read`.
Static and shard actions are typed operations, not guessed executable paths.
`Backend` must supply real source verification and bounded execution under the
parent's admission/Isolation/cancellation context. No production implementation
or runnable QA command is staged yet.

`State` validates completed rows, preserves completed reds on resume, binds jobs,
closed-since, toolchain fingerprint and exact tree/clean-source inputs, and
checkpoints JSON through a synced atomic replacement. Interrupted lanes are not
recorded as completed. `evidence_directory` resolves physical paths, rejects
symlink escape and keeps evidence below a child of the sandbox target directory.
Failure/signal extraction, last-summary counts and known-failure annotation
parsing preserve the retained report semantics. `reports` produces the v1 QA
schema consumed by the existing importer; its caller supplies the actual native
runner identity rather than falsely naming the old Python entry. Existing
fingerprint/cache byte encodings and blob keys are unchanged.

#445 records one fail-closed correction: missing mandatory summary/count evidence
makes the staged sweep return failure even if every recorded subprocess exited
zero. Such a report was already refused by the cache importer. No canonical
caller is switched by this correction. The staged fake-backend tests establish
state/report behavior, not execution of the 16 shard or five companion lanes.

Desktop continuation evidence now also includes actual dependency-free offline
`npm ci`, Node tests and `cargo test --locked` in a private workspace fixture,
with inherited package/dependency fields, root Cargo configuration and an outside
binary include. Its stale-lock variant runs frontend tests then refuses the lock
update without changing the original tree. It uses the daemon's stamped build
environment and sandbox target; it does not run the product desktop suite,
WebKit, Linux namespaced execution or Windows runtime acceptance.

## Native commands (second continuation)

Three commands are now registered in `rsi-repo-guard`. None switches a canonical caller:
the lander and `scripts/scoped-test` still name the legacy files. Hub #3621 owns
the trusted, target-authenticated executable the lander would launch, so a caller
switch waits for it. Never run the Python originals, not even to compare output.

| Original | Command | Notes |
| --- | --- | --- |
| `scripts/rolling-landing-guard.py` | `rolling-landing-guard` | Same flags (`--repo --base --source --target --candidate --worktree --plan-only --test-filter --proof`), JSON report (two-space, ASCII-escaped), exit codes (0, 1 lost hunks, 2 refusal) and `landing guard: <reason>` line. |
| `scripts/desktop-test-gate.py` | `desktop-test-gate` | `--base [--head]` selects; `--run` runs npm and locked Cargo on a private copy and prints `DESKTOP_LANE_RESULT {json}`. |
| `scripts/run-rolling-qa.py` | `run-rolling-qa` | `--evidence-dir [--jobs 1..4] [--closed-since] [--resume]`; resumable, source-pinned, writes `state.json`, `report.json`, `sweep-report.json`. |

### Landing guard

* The candidate's static target-reclaim policy runs in this executable over a
  `git archive` export of the committed candidate (`crates`, `scripts`,
  `desktop/scripts`), so a dirty working tree cannot hide a writer and no
  candidate-authored code decides the verdict. A tree without the guard (neither
  the legacy script nor `target_reclaim.rs`) has nothing to check, as before.
* `--proof` is recomputed with the #361 `migration_renumber::inspect`/`prove`
  APIs and compared byte-for-byte as a JSON value; only independently re-derived
  spans mask the accepted source (`Proof::apply` requires exactly one match), and
  `tools/released-migrations.json` takes the candidate's own derived value. The
  per-file pending-sibling defect (#438) is not folded in.
* Cargo tests run in the verified clean worktree (HEAD equals the candidate;
  `CARGO_BUILD_JOBS=6`, `CARGO_PROFILE_DEV_DEBUG=line-tables-only`, caller
  environment without `GIT_*`/`RSI_*`), one owned process tree per command with a
  4 h bound and signal cancellation. Static Git/matching work shares a 30 min
  bound. The legacy script had no bound; the lander's own deadline still applies.
* Known differences: argument/`KeyError` wording for malformed proof JSON is
  clearer; Python tracebacks do not exist. Debug-build cost of the policy check
  is tracked in #447.

### Desktop gate

* Selection prints `[["scripts/repo-guard", "desktop-test-gate", "--run", "--repo", "."]]`
  (the Python-free `python3 <script> --run`) or `[]`; a missing selector with a
  touching diff exits 1 with the fail-closed message (#355).
* `--run` honours `DESKTOP_LANE_NPM_CACHE`, `DESKTOP_LANE_CARGO_TARGET` and the
  three step bounds (`desktop_execution::LANE_ENV_NAMES`; the scoped runner
  forwards the same five). It prints the legacy progress lines. A signal stops
  the owned step tree, removes the private copy and exits `128 + signal` with a
  typed `cancelled` result (the Python script exited the same way without a
  marker). Linux namespace and Windows behaviour are unverified here.

### Rolling QA

* Lane order, state, resume and report schemas are `rolling_qa`'s; the CLI adds
  the real source pin (fresh fetch, `HEAD^{tree}` equals `origin/rolling^{tree}`,
  clean tracked tree before every lane), the bounded lane process, the optional
  known-failure classifier and the closed-Issue listing through `rsi-rpc`.
* Every shard lane runs `scripts/run-rsid-test-shards.sh`, which still shells out
  to Python in this tree (#362 inventory; conversion tracked in #448). The lane therefore refuses to start with
  `incomplete_contract` naming #362 until that script is Python-free; the static
  lane and the five companion lanes do not depend on it.
* Reports are compatibility reports with `provenance.runner` set to
  `rsi-repo-guard run-rolling-qa`; they are not daemon-attested scoped receipts.
* Not claimed: Linux execution of the wrapped lanes, the product desktop suites,
  and any full exact-head acceptance (hub #3621).

### Desktop lane in the scoped runner (`scoped_runner::host::Host::desktop`)

* The lane runs on the run's one `Isolation` and cancel flag. Its steps share
  the run's remaining wall-clock budget (`min(runtime_max, total_max - elapsed)`,
  like the legacy `budget`): a step never outlives it, and an already exhausted
  budget records a `timed_out` entry without starting the lane.
* Receipt entries keep the legacy fields through `desktop_execution::receipt_entry`:
  `lane_status` is the lane's own typed verdict, `completed` is false for
  `runner_unavailable`/`cancelled`, and only a `passed` verdict with a zero exit is
  `passed`. A step or budget timeout is `timed_out` (the runner's early exit);
  every other typed verdict (`desktop_lock_stale`, `output_incomplete`,
  `cancelled`) is `failed`. A dry run still probes the runners, so a missing
  runner is in the plan instead of hidden.
* The trusted npm cache and Cargo target overrides (`DESKTOP_LANE_NPM_CACHE`,
  `DESKTOP_LANE_CARGO_TARGET`) are forwarded with the three step bounds.
* Not claimed: the product Tauri/WebKit suites, Linux namespace behaviour or
  Windows runtime acceptance. The desktop host tests run on non-Linux hosts
  (the `end_to_end` module); Linux acceptance is hub #3621's.
