# Landing and test-gate mechanics (on-demand)

Moved verbatim from `AGENTS.md` (#2314, rule-debt pass) so the repository rules
every worker reads at launch stay short. Nothing here is a hard rule: the daemon
(merge queue, lander, job runner) and the hooks enforce it. `AGENTS.md` keeps
the worker-facing essentials and points here. Read a section when you run,
review or debug the gate it describes; managers and operators read "Landing".

## Exact integration-test jobs

For a focused regression check, `AgentSubmitJob` accepts
`{"kind":"test","params":{"package":"rsi-common","test_target":"manifest_dir_guard","exact":true,"filters":["<whole test name>"]}}`.
`test_target` selects one integration-test harness with Cargo's `--test <name>`;
its name contains only ASCII letters, digits, underscores or hyphens, cannot start
with a hyphen, and is limited to 200 bytes. Paths and shell syntax are refused.
It requires a package, `exact:true` and nonempty filters, and is mutually exclusive
with `lib_only`, shard, recipe, scoped-test and candidate-receipt jobs. Invalid
combinations are refused as `job_invalid_params`. The job runs
`cargo test -p <package> --test <name> --no-fail-fast -- <filters> --exact`.
Omitting `test_target` preserves the existing package-test behavior. This focused
check is optional feedback; the committed `scoped_test` remains the acceptance gate.

## Scoped test, hub build budget and cargo targets

`scripts/scoped-test --base origin/rolling` verifies the committed diff to HEAD.
Workers run it as a job: `AgentSubmitJob {"kind":"test","wake":"none","params":
{"scoped_test":{"base":"origin/rolling"}}}`, then ONE `AgentScheduleWake
{"mode":"when","message":"scoped_test finished: read the receipt","when":{"jobs_terminal":["<job id>"]}}`
(`message` is required), then end the turn; you are resumed once with
`exit_code`, the log path and the typed receipt (#1638). A foreground call outlives
the provider's 10-minute tool cap on a busy host.
Use `--dry-run` to inspect the plan without building; add `--head <rev>` to
inspect another candidate. Execution requires the candidate to be this HEAD. It builds each selected package once and runs its matching tests under
a per-package budget (build included): `--runtime-max-sec 1800 --cpu-quota 200` run by hand.
As a job the budgets derive from the job timeout (`--runtime-max-sec` T-120 s, `--total-max-sec`
T-60 s), and a `scoped_test` job defaults to 60 minutes (or `job_test_timeout_mins` if larger,
max 180; #1979) so several long packages can finish; other test jobs keep `job_test_timeout_mins`.
The stamped `params.timeout_minutes` and the receipt's `job_timeout_minutes` show the timeout.
The default runs tests once with the lander's private tmpfs TMPDIR on Linux
when available; builds keep the host TMPDIR. Otherwise it runs the same tests
normally and reports parity unavailable. The receipt records `scratch_mode`
and completed tests; no second tmpfs verification job is required (#1910).
A `scoped_test` job runs the tool from the `origin/rolling` tip (materialized read-only
under `<data>/jobs/scoped-test-tools/<rev>/`), not your sandbox's copy, so tool fixes reach
you without a merge; a candidate that edits `scripts/scoped-test` or its helpers runs its own
copy. The receipt's `tool` names the copy (`path`, `blob`, `source`, `rev`; #1938).
The helper set is derived, not only listed (hub #3024): `TOOL_FILES` in `agent_jobs/scoped_tool.rs` is the
floor, and every sibling the tool scripts name (`.with_name("x")`, `.parent / "x"` in `scripts/scoped-test` and
`scripts/check-touched-shards`, read at the rolling tip and at the candidate head) joins it. A candidate that
changes any file of the set runs its own copy; the rolling copy materializes the whole rolling set, so a helper
added after the daemon was built is never missing. The receipt's `tool.files` maps each name to its blob (`null`
when absent). A copy that would load a helper its own tree lacks is refused `scoped_test_tool_helper_missing`
naming the file, never run with another copy of the tool.
Tests run on cargo-nextest when installed (libtest fallback) at load-adaptive threads capped by
`--cpu-quota`. A plan over 12 filters is never broadened to whole packages: it runs as named, most
specific first, inside a 10-minute test box (receipt `plan: targeted_subset`, with selected and run
filters derived from executed test identities, plus partial and not-run filters) and the merge-queue gate covers the rest (#2084).
Inside the box, added/changed tests run first, then the tests of every module whose own source changed
(receipt `changed_modules`, `changed_module_tests_selected`/`_run`; smallest module first, a module that
selects more than 300 tests keeps its specificity tier), then the proven direct observers of a stopped broad trace, nearest
first (#2588), then the other observers by specificity (#2182).
check-touched-shards follows an exported helper whose name another module also defines through the calls
that provably reach it (qualified path, import, own-module or inline-`mod` definition, glob of the owner);
only an unplaceable reference keeps the broad fallback, and `ambiguous exported helper: <item> (<why>)` names it (#2247).
A path is followed through `pub use` re-exports, re-exported modules, glob chains and `use .. as` aliases
down to the defining module (at most 6 modules deep): an alias or re-export of an unrelated same-name item stays
out, a module segment that merely shares the helper's name (`crate::topology::resolve::f`) is not a reference,
and a re-export cycle, an over-deep chain, a path naming no known module or a name two different items could supply
stays conservative and says so (#2592).
The walk never follows a high-fan-in token (a type such as `SessionManager` that more than 150 functions
mention): it follows the changed method by its own name and stops at the hub. A trace that stops (hub, 150-function
or test bound, unresolved identity) stays uncertain, keeps the module filter in the derivation's `suggested_filters` (the
merge-queue gate's selection, which never changes), and names the tests it proved observe the change, nearest first
(derivation `direct_observer_filters`/`direct_observers`; receipt `direct_observers`,
`broad_filters[].direct_tests`/`hubs`/`trace_stopped`, `direct_observer_tests_selected`/`_run`). They run after the added
and changed-module tests and ahead of every unranked far test whatever its specificity tier (an unrelated exact filter's
test included), across all binaries of a package, and a package holding them runs before the packages without (#2588, #2092).
A package's binaries are all listed before any runs, so a list failure in a later binary now stops the package before its
earlier binaries ran.
**A cap never widens the worker's plan (#2592).** A trace that stopped at a cap (a hub, the function bound or the test
bound, every identity placed; `broad_filters[].stop`, `worker_bounded: true`) is bounded for the worker: scoped-test selects,
from that module filter, only the changed-module tests (a module of at most 300 tests, smallest first), and every proven
direct observer in its own right, nearest first inside the box, including one that sits outside every plan filter (it is
built with its package and counted in the executed identities, but is not a filter of the plan). The named filters the
module filter had absorbed (`merged_filters`) return, so a source scanner or catalog pin is never lost with its covering
filter. The rest of what the filter matches is DEFERRED:
never selected, run or counted passed. The receipt says so: `worker_policy: "bounded_deferred"`, `worker_broad_filters`
(broad worker filters left, 0 when every broad entry was a cap), `filters_bounded` (per filter kept, executed and deferred,
apart from run/partial/not_run) and `deferred` (the filters and the cap that stopped them, `scope_tests_at_most` from the
derivation, `tests` counted from the package listings once a run lists them, `routes` the merge-queue gate and the pinned
full QA, the `candidate` SHA and `status: "pending"`). `pending` is the only status a worker can report: the gate and the full
QA of that SHA do the proving, and the queue entry and the `qa_green` field of `AgentListRollingQueue` show when they have. An unresolved
receiver or helper identity, or a diff that cannot be traced at all (`worker_bounded: false`), is an ambiguity, not a cap:
it keeps the broad worker filter, boxed as before, and the receipt says so. Both lanes read the same derivation; only the
worker's plan is bounded, so the queue gate and the full QA keep covering everything the worker deferred.
While a hub deploy is staged with `interrupt_workers`, a new `test`/`build` job is
held (`held_for_deploy`, no unit, no timeout clock; the receipt's `release_by` names
the latest release) and starts when the deploy settles, so one submit is enough (#2190).
The released-migration history tests (`.config/slow-tier.toml`, ~47% of the
`rsid`+`rsid-store` lib time when measured) are a slow tier left out of the default profiles; a diff that
touches `store/migrations/`, `tools/released-migrations.json`, the migration runner or such a
test runs them, and the full QA always does (`docs/testing.md` "Slow tier", `make test-slow-tier`, #2270).
Linux uses a foreground systemd user service; other platforms use a plain timeout
and explicitly report that no CPU cap is available. Output is tee'd under
`/tmp/rsi-scoped-test-*`; a timeout names the active test and exits nonzero.

**Hub build budget (#2119).** The hub is the operator's desktop, so every cargo
build and test unit RSI starts (job units, `scripts/scoped-test` execute units,
merge-queue lander scopes, the library QA recipe) runs in one user slice,
`rsi-build.slice`, whose `CPUQuota` is the operator setting
`hub_build_cpu_percent` (default 50, range 10-100; TUI Settings > Orchestration)
of the host's logical CPUs (16 of 32) at `CPUWeight=20`; the parent `rsi.slice` is
held at `CPUWeight=30` (the desktop, `app.slice`, is 100) and the same `CPUQuota`,
so worker-scope cargo is bounded too (#2126). The daemon rewrites it
live on a change; read it back with `systemctl --user show rsi-build.slice -p
CPUQuotaPerSecUSec` (and `rsi.slice -p CPUWeight -p CPUQuotaPerSecUSec`). Concurrent jobs share that budget, and each job's own ceiling
is `min(its host-derived ceiling, budget)`. Linux only; other platforms report that
no slice is available. Detail: `docs/agents/rsid-rpc-contracts.md` ("Hub build CPU budget").

The Linux slice also has aggregate `MemoryHigh=60%` and `MemoryMax=75%` of
physical host RAM, rounded down to MiB (hub #3084). This is fixed policy, independent
of the CPU percentage and of the number of concurrent jobs. MemoryHigh starts
reclaim/throttling; MemoryMax can fail a build with a cgroup OOM instead of
letting admitted compiles exhaust the host. The parent and rsid service do not
receive these memory ceilings. The existing ownership/test-seam guards apply:
fixtures never change the operator's slices. The slice report and job log name
the intended byte bounds and whether the write succeeded; inspect with
`systemctl --user show rsi-build.slice -p MemoryHigh -p MemoryMax`.
This protection is a documented no-op on macOS and Windows. Unavailable host
RAM or memory controllers are reported; builds retain the existing best-effort
launch policy. These limits cover admitted build units, not arbitrary worker
processes or other applications; swap and other workloads can still stall I/O.
The deterministic fixture tests check the aggregate policy and property
application, not measured responsiveness under live pressure. A pressure and
RPC/store latency measurement must run as an admitted test job on a disposable
fixture host; no live unit changes or daemon restart are part of this slice.

**Detached landers (#1192).** `systemd-run --user` units do not inherit
your environment: pass `--setenv=CARGO_TARGET_DIR=<sandbox>/target` (the lander
refuses without it). `--accepted` takes a full or abbreviated SHA; an unknown or
ambiguous abbreviation is refused naming the id, so pass the full 40-hex id.

**Cargo targets (#1741).** Each sandbox builds only in its own
`<sandbox>/target` (the daemon stamps `CARGO_TARGET_DIR`; job units,
`scripts/scoped-test` and `cargo-slot` inherit it with the rest of the build
settings). A new worker target is seeded from the warm target pool
(`scripts/target-pool`): keep the stamped `RUSTC_WRAPPER`, and on a `not
writeable` build error run `scripts/clean-target` (keeps the harness `target/.rsi-tmp`; a plain `rm -rf target` breaks the running call, #288) and rebuild. Use
`rsi_common::manifest_dir!()`, never `env!("CARGO_MANIFEST_DIR")`. Detail:
`docs/sandbox-storage.md`.


**Candidate gate receipts (#1312).** A candidate gate that passes in full leaves a
receipt under `~/.rsi/cache/rolling-candidate-receipts-v1/<candidate tree>/<key>.json`
(`RSI_ROLLING_CANDIDATE_RECEIPT_DIR` moves it) before publication starts, so a
publication that then fails (declined push, lost race, killed lander) does not make the
next invocation gate the same content again. The key digests the candidate's tree, the base
tip, every resolved gate command and its environment (not the per-run scratch path or the
adaptive `-j`), the test filters, claimed fixes, scratch class, slow tier, toolchain and the
running lander binary; anything else is a miss, as is a receipt older than 12 hours. Only
the candidate's first full gate reads or writes one: a stale re-gate, the post-publication
canary, a remote-executor gate, a provisional-migration landing and a green gate with
unverified evidence always run. Policy checks, the landing-guard static checks and the
fast-forward publication stay live; the log shows `candidate_receipt=reused|recorded`.

## Desktop lane (hub #3001)

`desktop/` is a Tauri v2 app with its own Cargo workspace (`desktop/src-tauri`) and a node
frontend, so no rsid shard filter covers it. A diff that touches an input of the desktop suite
therefore selects a `desktop` lane (`scripts/desktop-test-gate.py`), and a diff elsewhere pays nothing
for it. The inputs are one list (hub #3042, `trigger_inputs` in the gate script): `desktop/`;
`docs/keybindings.md` (the keymap and parity tests); `crates/rsi/src` (the parity test lists the names
under `ui/` and `ui/overlay/` and checks the module files it names exist; the whole tree is the trigger
because a narrower list would silently stop being right once a test reads a source file's text); and
every repository directory `desktop/src-tauri/Cargo.toml` reaches through a `path = ...` dependency,
followed through those crates' own manifests and read at the candidate's head, so a new path dependency
needs no edit. Today the manifest declares no path dependency (the app reaches rsid over its socket and
compiles no workspace crate, and `desktop/src-tauri/Cargo.lock` holds no local package), so the set is those three
paths and a change under `crates/rsi-graph` or `crates/rsi-common` alone does not select the lane; a crate the app
starts consuming selects it by its `path = ...` entry. The selector (scoped-test, check-touched-shards, the lander) uses it to decide WHETHER the lane runs. What the lane
runs on is the whole candidate tree (#3129: tracked and untracked-not-ignored files minus build output,
`node_modules` and `.git`), never a selection of it, so an `include_str!` outside `desktop/`, a `build.rs` read or
workspace inheritance resolves as in a checkout; cargo still builds only `desktop/src-tauri`'s own closure in the
lane's private target.

The lane fails closed (#355): the script is the selector, so a candidate that has no
`scripts/desktop-test-gate.py` cannot say what it owes. When the diff deletes or renames the script, or touches
`desktop/`, `docs/keybindings.md` or `crates/rsi/src` in a tree without it, the lander's selection is an
error naming the script (the entry is refused), `check-touched-shards` reports
`desktop.error` and fails, and `scoped-test` prints the refusal and exits 1. A diff that
touches none of those and leaves the script alone selects nothing, as before. Without the script the lander
cannot read the manifest's path dependencies, so only the static inputs apply (a test pins them to the script's).

- `scripts/check-touched-shards` reports it as the receipt's `desktop` object
  (`selected`, `total`, `paths`); `scripts/scoped-test` runs it after the Python tests and before
  Cargo (fail-fast; `phase_order` names `desktop`) and records the `desktop` receipt list
  (`status` `passed`/`failed`/`runner_unavailable`/`no_tests_executed`/`planned`, `npm_tests`,
  `cargo_tests`, `executed_tests`, `failed_step`); the merge-queue lander selects it with the same
  script (`desktop_test_commands`) and runs it in the candidate's `python-tests` step group as the
  strict `desktop-tests` step (no base triage: a red desktop test refuses publication).
- The lane runs `npm ci`, `npm test` and `cargo test --locked` (in `desktop/src-tauri`) on a
  private copy of `desktop/` made in a fresh temp directory, never the committed tree (the tracked
  `node_modules` would be rewritten). The copy also carries the other trigger inputs (`docs/keybindings.md`, `crates/rsi/src` and the
  path-dependency directories, minus any `target/`), which the app's tests and Cargo read from the copy's
  parent (`outside_inputs`). The npm cache is per run inside that copy's temp directory
  unless the caller names a trusted one in `$DESKTOP_LANE_NPM_CACHE`; the Cargo target is `$DESKTOP_LANE_CARGO_TARGET`, else
  `$CARGO_TARGET_DIR/desktop-tauri` (a job's own target, a Cargo tree the sandbox reclaim
  recognizes), else inside the copy. The caller's `RUSTC_WRAPPER` and the repository's `rust-toolchain.toml` are kept.
- The lander's target (hub #3023): a gate that leased a warm slot (`warm_slot=N`) points the lane at
  that slot's own `desktop-target/` beside its `test-target/` and `check-target/`, so a second
  desktop gate compiles only `rsi-desktop` (no `Compiling tauri`); a gate with no slot keeps
  `<worktree>-desktop-tauri-target`, cold per candidate. The slot rules cover it unchanged: the
  lease is an exclusive `flock` (one lander writes it), a toolchain change, a size over
  `MAX_SLOT_BYTES` (which now counts it) or a relocated slot empties it, and the idle reclaim removes
  it with the slot. After every lane run, pass or fail, the lander trims the candidate's own crates
  (`[package]`, `[lib]` and `[[bin]]` names of `desktop/src-tauri/Cargo.toml` and of every workspace
  `path = ...` dependency crate it reaches, transitively, `lander_warm_target::candidate_crate_names`,
  #354) from it, so only registry artifacts, which Cargo keys on the `--locked` package, reach the
  next candidate; an unreadable manifest in that walk empties the target. The log names it before the lane runs:
  `desktop_warm_target slot=N state=warm|cold|toolchain_changed|over_size bytes=B` (`slot=none
  state=cold` without a slot), then `desktop_warm_target slot=N trimmed=K`; the step's seconds are
  `desktop-tests@S` in the `gate_timing` `check_steps`.
- A missing runner (node, npm, cargo, or on Linux pkg-config and `webkit2gtk-4.1`) is the typed
  `runner_unavailable` with `executed_tests: 0` and the run is red; a set that executes zero tests
  is `no_tests_executed`, also red. macOS and Windows need no webkit probe (the Tauri cargo tests
  use the platform webview SDK). Run `python3 scripts/desktop-test-gate.py --run` to reproduce.

## Compiler-warning gate

`tools/warning-baseline.json` (`{"schema":1,"packages":{NAME:{"configs":[...],"warnings":{KEY:COUNT}}}}`)
is the checked-in list of compiler warnings accepted per Cargo package (hub #1897; `rsid-store` is the
first package, other crates join by adding an entry). The evidence is the build `scripts/scoped-test`
already supervises, not a second compile: the `--message-format=json` stream carries every rustc
diagnostic with its `package_id`, and Cargo replays cached diagnostics for a fresh unit, so a warm
build shows the same warnings. A warning's key is `file|lint code|message` (never a line number); two
reports at the same position, as the lib and lib-test units make, count once, and two at different
positions count twice. A warning is attributed to the package that emitted it, so a change in another
crate never moves the store's count.

The run fails (`warning_gate_failed: <status>`, receipt `warnings.status`) when:

- `new_warnings`: a baselined package the run built has a key above its baseline count;
- `baseline_raised`: your baseline has a count above the merge base's, or drops a package the base
  has (raising the baseline to hide a warning is refused; adding a package the base lacks is fine);
- `unavailable`: the diagnostics cannot be trusted: `RUSTFLAGS`/`CARGO_ENCODED_RUSTFLAGS`/
  `CARGO_BUILD_RUSTFLAGS` allow warnings (`-A warnings`, `-A unused*`, `-A dead_code`,
  `--cap-lints`), a rendered rustc diagnostic arrived outside the JSON stream, or a warning carried
  no package id. This is never read as a clean count.

A baselined package the run did not build is `not_measured`, never clean, and does not fail the run.
A fixed warning is reported ("lower the baseline") only when the run built every unit the entry's
`configs` names; a stale baseline never fails. Lower counts as warnings go; regenerate a package entry
from a finished job log with `scripts/scoped-test --warning-baseline-from <log>`.

The shard-feature builds (`--no-default-features --features test-shard-NAME`) compile configurations
the run above does not, so they are judged by the same gate through the `build` job
`{"command":"test_no_run","package":"rsid","shards":[...]}` (#2692): `scripts/shard-feature-sweep`
feeds each shard's JSON stream to the same collector, reports per-shard warning counts and exit
codes in its receipt, and fails on `new_warnings`, `baseline_raised` or `unavailable` exactly as
`scoped-test` does. A worker verifies shard features with that job, never with a self-launched
cargo unit (contract: `docs/agents/rsid-rpc-contracts.md`, "Shard-feature test builds").

Not measured: Cargo's `lib` and `lib test` units of one target share a key space (the JSON stream
does not say which unit emitted a message); release builds without the `test-seam` feature, doctests,
and macOS and Windows builds. The gate is per package, not per target: a warning from any unit of a
baselined package the run built (a `[[bin]]` included) is a key above its baseline, so no per-target
config is needed. Items inside an `RSI-RELEASED-MIGRATION` region cannot be gated `cfg(test)` without
editing released bytes, so a warning there stays in the baseline with its reason.

Coverage checked by hand (#2666, #2671, the last #1897 slices), all at zero new warnings: every workspace
`[[bin]]` (`cargo build --workspace --bins`, which also builds `rsid-store` without `test-seam`, where
the two released-region constants above are the only warnings), and each of the 16 `rsid`/`rsid-store`
test-shard feature builds (22 package/shard harness builds). The shard check is `scripts/shard-build-warnings.py`,
which runs `cargo test -p PKG --lib --no-run --no-default-features --features test-shard-NAME
--message-format=json` per shard (its output names the `(lib test)` target). `cargo check --profile test
--lib` is not that check: `--profile test` selects the profile, not the test harness, so it compiles the
plain library and neither `cfg(test)` code nor the shard lint attribute. The script prints per-shard
counts by lint code and fails on an error or on any warning other than the two allowed lints that
`tools/warning-baseline.json` does not accept (a shard-only `unused_variables`, `unused_mut`, ...). It
builds each shard cold, so run it under `systemd-run --user --collect` with `--clean`, not as a gate.

Why the crate roots allow `dead_code` and `unused_imports` for the shard-mode test unit
(`#![cfg_attr(all(test, feature = "test-shard-mode"), allow(dead_code, unused_imports))]`): a shard build
compiles a strict subset of the lib tests by construction, so "unused in shard X" is not a defect. The
ordinary lib-test build compiles every test without `test-shard-mode`; the allow does not apply to it,
`tools/warning-baseline.json` measures it, and a helper or import no test uses is reported there. Gating
helpers and imports per shard instead would couple source to shard assignment (every new test or shard
move that reuses a gated helper becomes a compile error in a shard build) and touched hundreds of files
in the abandoned attempt (#2671). A new non-test item that only a test uses gets `#[cfg(any(test,
feature = "test-seam"))]` (as `session_diagnostics::install_schema` now does), never an allow.

Lander decision (#1897 acceptance 4): the merge-queue lander does not enforce the baseline. A worker's
source reaches the queue only with a `scoped_test` receipt for its exact commit, and that receipt already
carries `warnings.status`; the lander's gate builds its own filtered `nextest` set, whose JSON stream
and `package_id` attribution would need a second parser and a second per-build baseline merge for no
extra coverage. A source with no worker receipt (an operator `rsi land`) is the only gap, and the
operator's `scripts/scoped-test --base origin/rolling` closes it.

## Landing

**Land one accepted change per gate, filtered to what it touches.** Every
landing goes through the daemon merge queue: the manager enqueues the accepted
SHA with `AgentEnqueueLandingSource {derive_filters:true}` and the queue derives
the filters itself, so workers do not run `scripts/check-touched-shards` or
hand-copy filters; never guess shards. The enqueue call returns within about 5 s and
**one call is enough** (#2993): a derivation still running at the call's budget (a real Rust
diff derives in 10-45 s) does not refuse the call. The entry is recorded `queued` with
`filters_state: deriving` and returned; the lander admits it only once the derived filters
land, a derivation that fails settles it `refused` (`queue_filter_derivation_failed`, or
`filter_matches_no_tests` for a derived filter that selects no test), and the enqueuer's single
outcome wake carries either result. End the turn; never sleep, poll or call again for the
filters (a replay under the same `idempotency_key` returns the same entry). A `queue_probe_timeout`
or `queue_tip_read_timeout` refusal (a git read of the call stalled at the bound and was killed,
#2912) records nothing and is real git trouble, so it stays andon friction (#2992): call again
with the same `idempotency_key`. Combine sources into one gate only when they
conflict and you have resolved the conflict, or when they are docs-only. A
gate's cost must scale with the change: if its filters span more than a few
rsid shards or any rsid-store module, split it. Batch waits (several jobs, one
`mode:"when"` wake), never landings. Evidence: `docs/agents/reference.md`.

`queue_disabled` is an operator setting: keep the commit pushed and report it
with `AgentReportUp`, or send the result with `AgentReportToHub` on a satellite.
The operator enables the queue or lands with `rsi land` or
`EnqueueRollingQueueSource`.

The queue publishes with `rsi-rolling-land` (installed on PATH next to `rsid` by
`make release-install`); agents never run it directly, because a direct landing
moves `rolling` under a queue gate and gets that gate discarded. Its
`--test-filter PACKAGE=FILTER` semantics, for reading gate logs: a filtered
package's gate is a compile check plus the named tests (no new failures against
the base); the QA sweep runs the rest. The `rsid` and `rsid-store` library tests
are partitioned into 16 shards (`test-shard-*` features, `store-01..04`,
`session-01..05`, `memory-01..02`, `other-01..05`); a shard is one feature name
that every package with tests in it declares, and `scripts/run-rsid-test-shards.sh`
runs it across those packages (`scripts/check-rsid-test-shards.py
--list-shard-packages`). Name one as `--test-filter rsid=shard:store-01:test(NAME)`
whichever package holds the test (the store tests live in `crates/rsid-store`).
A focused `rsid`/`rsid-store` filter (`shard:S:test(NAME)` or a plain name) runs
on one lib test build of both packages without shard features, so the name may
sit in any shard; a filter on either package scopes both. An unfiltered change
runs every library test that can observe it (the tests that name the changed
items, transitively, plus the source scanners; a closure over 400 selects the nearest
400 by call-graph distance, retaining every direct observer and every test at
the minimum distance even when they exceed the budget, #2092; every library
test when distance cannot be computed, #1280) and the tests of a changed bin or
integration target, plus `cargo check`; a bin or integration target that names
an affected library item or scans the sources counts as changed (#1306); one
that owns, mounts, reads or directly calls the changed code always runs, and
past 5 targets the rest run nearest-first by call-graph distance
(`targets=bounded selected=N reached=M` in `gate_timing`, #2094); only a
crate root, manifest or build-script change runs every library test: the 16 shard
builds collapse into one `all()` run on the one lib build without shard features,
so the rsid test code compiles once, not once per shard (#2631; `gate_timing`
names `build_mode=sharded|unsharded|focused|none`; a gate with
`--disk-scratch-shard` keeps the per-shard builds). Any other
unfiltered package runs its library tests, the tests of each bin or integration
target whose sources, mounted modules or read data files changed (a manifest or
build-script change runs every target; a package with no library target has no
`--lib` run, #1937, #1650). The same name closure, built over the touched
non-rsid packages, also runs the unchanged bin and integration targets whose
tests reach a changed library item (anything the names cannot place runs every
target of the package), and a package whose production library code changed and
that has a bin or integration target gets `cargo check -p PKG --all-targets`
(`unfiltered_targets ... check=true`, #1952). In the rsid family, a test that
asserts prose (a string literal of 16+ characters with a space, between format
braces and newlines) a changed production item writes is a direct observer of
that item, base side included, so a launch-brief edit selects the tests that
pin its wording (#1952); a plain `PKG=NAME` filter selects library tests only, an
integration target is `PKG=test:NAME` and a binary `PKG=bin:NAME`. Another
package's lib filters run in one
libtest invocation, and a filter that selects no test is refused after the
build (#1282). The gate runs
the candidate first and builds the base only for a candidate failure (#1244).
A whole diff proven production-unchanged drops its compile checks (dev
`--all-targets`, non-test, dependents; log line `test_only_diff`, #2512): every
path a mounted Rust source of an rsid-family package whose non-test tokens match on
both sides (parsed `#[cfg(test)]`/`cfg(all(test, ..))` regions; a cfg boundary,
`cfg(any(test, feature = ..))` seam, doc comment, manifest, build script, text-read
source or other file keeps the checks), and the selected test runs must compile each
edited target (the focused lib build for rsid/rsid-store, never a shard alone).
A fix for known reds passes `fixes_issue: <N>` (the queue reads that Issue's
`Failing tests:` line) and/or `fixes_tests: [..]` to `AgentEnqueueLandingSource`:
its gate must run each named test green on the candidate (a red the base shares
excuses nothing, and a test the selection never runs counts as red, so add a
filter that selects it) or the landing is refused `claimed_fix_still_red` naming
them; every other landing keeps "no new failures relative to rolling" (#2167).
When `rolling` moves under a gate (a direct `rsi-rolling-land`, an operator merge,
the queue's previous batch) the lander does not restart it (#1940): it merges the
candidate onto the new tip and re-runs only the compile checks, test targets and
Python suites the advance selects (`scripts/check-touched-shards --filters-only` on
the advance plus the changed crates and their dependents). `--filters-only` starts no cargo; compiling needs the explicit
`--compile` inside a supervised job, and a bare run is refused (#2564). A docs, script or
command-file advance that selects none of the candidate's tests costs only the
static guards; a file shared with the candidate, a manifest or toolchain change or
an unreadable advance re-runs everything. Under the merge queue the re-runs are
bounded by the batch's wall time (`rolling_queue_gate_timeout_mins`, default 60,
past which the batch is refused `queue_gate_timeout`), not by a count. Direct
landers and the queue both publish by fast-forward, so each is just an advance to
the other; global-manager and laptop lanes land through the queue. Detail:
`docs/agents/rsid-rpc-contracts.md` ("Rolling merge queue"). The lander's policy is mechanical and
runs before any test: no Work binding, seal or hot-file claim is required, and a
new migration must be the rolling tip's schema head + 1 (the highest
`store/migrations/vNNN.rs`) as its own file with its `if version < N` block; its pin in
`tools/released-migrations.json` is derived by the lander (#1949: after the merge it runs
`python3 tools/check-released-migrations.py --refresh <tip>` in the candidate worktree and commits
the manifest as `rsi rolling landing: derive released-migration pin`, so the candidate the gates test
and the commit it publishes carry the pin; a stale retry drops that commit and derives it again on the advanced tip;
a refresh that fails, such as a changed released block, derives nothing and the released-migration
guard refuses with the same reason) and `check-released-migrations.py` accepts a source whose pin file is
still the base's (`pin deferred to the lander`), while a hand-edited pin is judged strictly as before;
one landing may carry several migrations only as a contiguous run from that number,
each in its own file (the refusal names the number); a slice that follows a reviewed but
unlanded sibling declares the skipped number as `pending_predecessors` (#1285, see
`tools/provisional-migrations/README.md`). A single accepted source
may contain one provisional-migration declaration; multiple new declarations
are refused at `AgentEnqueueLandingSource` with
`queue_multiple_provisional_migrations`, directing the enqueuer to drop the
extra declarations or split the migrations across separate landings. The
lander never reads the manager ledger (#1927).
The operator lands a branch the same way: `rsi land <session-id|branch>`, the
TUI `<Space>gl` / `:land`, or `rsi-rpc EnqueueRollingQueueSource` (#2055). A manual
merge and push skips every gate, so the pre-push hook runs the fast static guards
on an unmarked rolling push (`RSI_SKIP_STATIC_GUARDS=1` overrides). The static guards
`scripts/scoped-test` runs on every verification include `starter_pins`
(`tools/check_starter_pins.py`, #3070): a changed starter topology JSON must move its
digest fixture in `starters_tests.rs` and append the digest it replaced to
`SHIPPED_BEFORE` in `starters.rs`, so a half-updated starter fails before any test.
Fast-forward-only publication and the canary remain. The pre-push hook permits
the lander's marked `rolling` push; unmarked agent pushes to `rolling` and every
agent push to `main` remain refused.
The hook is a cooperative guard rail, not a security boundary: agents can spoof
the lander's `RSI_ROLLING_LANDER=1` marker or bypass hooks. Only the lander
enforces test gates and fast-forward publication.
Work is landed when `git merge-base --is-ancestor <SHA> origin/rolling` holds.
`rolling` is agent-dev intake: land after your tests pass. Pre-merge review
only for new schema migrations and credential/IAM/network-exposure changes (one
plain reviewer pass); other authority or custody changes land first and get one
post-land review.
Every bounded landing (library tests or targets) and every landing gated on
queue-derived filters (#2112; operator or agent explicit filters are exempt)
also gets an automatic pinned full `rsid` and `rsid-store` library, bin and
integration test job (#2094) at low CPU priority, at most one start per
`rolling_queue_full_qa_interval_mins` (default 60). Pending ancestors coalesce; queue audit events retain
coverage across restarts. A red or incomplete attempt files a `qa` Issue naming
the landings; an eight-hour overdue Issue keeps stalled obligations pending (#2092).
The run goes to a healthy, reachable, compatible-build project satellite (the
laptop) over the #1017 link, so the hub stays cool (#2120): the satellite runs
the same recipe on the exact SHA in one reused target, and its exit, failing
tests and log come back into the same receipts and `qa` Issue. Only when no
satellite qualifies does the hub run it, recording `ran_on: hub` and why.
The automatic full-library QA files regressions back as Issues and, when a run
passes with every landing it owed covered, records the receipt that makes its SHA
the latest QA-green `rolling` commit: `AgentListRollingQueue` returns it as
`qa_green` (#2646). The old pointer file `thoughts/shared/qa/qa-green.sha` is
retired and no longer advances.
`rolling` is agent-managed (merge and push fast-forwards freely); `main` is
operator-only. The manager integrates worker commits (`.claude/skills/rsi-project-manager/SKILL.md`);
workers follow `thoughts/shared/manager/worker-contract.md`.
