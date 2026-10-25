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
The wait for a governor build slot inside the job is not part of that budget (#3284): it has its own
cumulative cap of one hour (`--slot-wait-max-sec`), the unit's runtime cap carries it, and exceeding it
ends the job `job_admission_timed_out` (starved capacity, not a red run), so a loaded hub never spends
the hour you need for building and testing on queueing. Sandbox/artifact-lock waits still count against
the budget. Resubmit a starved job when the hub has capacity.
Before admitting a cargo unit, the governor independently requires physical free disk to meet the configured floor. With positive running growth reservations, it also subtracts all running leases' and the newcomer's expected remaining target growth: a conservative 24 GiB target estimate (raised by observed per-target high-water sizes), minus current allocated target bytes. Without running growth reservations, a lone newcomer is admitted at the physical floor without projecting its own growth, preserving cold-build liveness. Unavailable or stale measurements reserve the full default; the hold names the running holder leases, requested growth and projected free space.
This only holds newcomers and never cancels holders. Verified own-target reuse reduces only the requesting job's estimated growth, saturating at zero, by a quarter of its allocated target bytes capped at 2 GiB and half the floor. Reuse already deducted by the target-growth probe is not counted twice. It never adds free space or offsets another lease's reservation: physical free space must always meet the full configured floor, and projected free space must also meet it when running growth reservations exist, including at the exact boundary (#569; no floor-minus-2 GiB exception). Platforms without a process-context probe use the conservative default.
The default runs tests once with the lander's private tmpfs TMPDIR on Linux
when available; builds keep the host TMPDIR. Otherwise it runs the same tests
normally and reports parity unavailable. The receipt records `scratch_mode`
and completed tests; no second tmpfs verification job is required (#1910).
A `scoped_test` job runs the tool from the `origin/rolling` tip (materialized read-only
under `<data>/jobs/scoped-test-tools/<rev>/`), not your sandbox's copy, so tool fixes reach
you without a merge; a candidate that edits `scripts/scoped-test` or its helpers runs its own
copy. The receipt's `tool` names the copy (`path`, `blob`, `source`, `rev`; #1938).

**PID isolation (#3478).** A test must not see or signal host processes: a Python test that ran
`install-release.sh --link-only --now` found the real hub daemon with a host-wide `pgrep -x rsid` and
interrupted it in every merge-queue gate (#3476). On Linux `scripts/pid-isolate.py` starts the test command in
`unshare --user --map-user=<uid> --map-group=<gid> --pid --fork --kill-child --mount-proc` (the caller's own
single id, so files keep their owner; `--map-root-user` only for an `unshare` without `--map-user`): `pgrep`,
`pkill`, `kill -1` and `/proc` see only the command's own tree, and killing the wrapper takes the namespace
down. `scripts/scoped-test` runs the bounded cargo worker (inside its systemd unit, so the unit's cgroup, CPU
quota and `KillMode` are unchanged), the Python lane and the desktop lane that way, and the lander's `run_one_guard`
runs every cargo test/nextest, shard-runner and Python-test guard that way (the `RSI_LANDER_LOCAL_SLOT_WRAPPER`
slot stays outermost: `cargo-slot` reads host pids; a tree without `scripts/pid-isolate.py`, such as a base side
older than #3478, runs unchanged). The mode is a canary result, not a version check: the probe starts a real
namespace and requires the child to be PID 1 and unable to see the probing process or its parent. The receipt's
`pid_isolation` is `namespaced` or `unavailable`, with `pid_isolation_reason` for the latter (`not_linux`,
`unshare_missing`, `disabled` for `RSI_PID_ISOLATION=off`, `host_pids_visible`, or the `unshare` refusal when
unprivileged user namespaces are off); the lander's gate log carries the same `pid-isolate: pid_isolation=...`
line. Where it is `unavailable` (macOS, Windows, a host that refuses user namespaces) the lanes run as before;
a test that signals a pid outside its own tree is not yet detected there. `scripts/tests/test_pid_isolate.py`
holds the decoy canary (a host `sleep` the namespaced command cannot see, find with `pgrep` or signal).

The helper set is derived, not only listed (hub #3024): `TOOL_FILES` in `agent_jobs/scoped_tool.rs` is the
floor, and every sibling the tool scripts name (`.with_name("x")`, `.parent / "x"` in `scripts/scoped-test` and
`scripts/check-touched-shards`, read at the rolling tip and at the candidate head) joins it. A candidate that
changes any file of the set runs its own copy; the rolling copy materializes the whole rolling set, so a helper
added after the daemon was built is never missing. The receipt's `tool.files` maps each name to its blob (`null`
when absent). A copy that would load a helper its own tree lacks is refused `scoped_test_tool_helper_missing`
naming the file, never run with another copy of the tool.
A source built on an unlanded tool change (the change is in your `base`, so `base...HEAD` does not
touch the set) also runs its own copy when the set changed on its side of rolling
(`origin/rolling...HEAD`) and its bytes still differ from rolling's; `tool.source` is then
`candidate_differs_from_rolling` (#279). A sandbox merely behind rolling still runs the rolling copy.

Cross-module invariant and cost-bound tests can declare additive coverage in
`tools/test-guards.json` (`version: 1`, `guards` array). Each entry names a
`package` (`rsid` or `rsid-store`), its full `test` name and a nonempty `modules`
array of crate-relative, file-backed library module paths. For example, a test
can guard `store::manager_resources`, `model_control::cost` and
`store::manager_intent` even though it lives in `store::portfolio_nodes`.
Changing a guarded module's file or a descendant's file selects that exact test
alongside ordinary coverage, including comment-only changes; changing the table
selects all declared tests. Tests must be runnable and carry a per-test shard
gate. Unknown modules, missing tests and malformed declarations stop derivation.
`scripts/check-touched-shards` reads declarations from the candidate commit;
`scripts/scoped-test` and merge-queue `derive_filters` both use that shared path.
The derivation receipt lists selected observers under `declared_guards`.

A candidate that declares a provisional migration (`tools/provisional-migrations/*.json`) whose number
`origin/rolling` has taken is tested as the lander will land it (#3339): `scoped-test` runs the base tip's
`scripts/repo-guard rolling-migration-renumber` (`--transform`, then `--build`), checks the renumbered merge commit out
into a private worktree and derives and runs the tests there. The receipt's `head` stays your committed
source (so it still verifies your HEAD); `renumber` records `source`, `target`, `old_version`,
`new_version` and the tested `candidate`, and `merges_cleanly_onto_rolling` describes that candidate. A
tool refusal is `renumber.status: refused` with `reason`, and the run proceeds as before.

The final receipt's `reverts_recent_landing` object and matching log line give warning-only
feedback about possible stale-copy overwrites (#437). The check reuses the trial merge tree
from `git merge-tree --write-tree <rolling> <source>` and examines `git diff <rolling> <tree>`
(never a three-dot diff, including with multiple merge bases). It blames deleted text lines
at the pinned rolling SHA. A line warns when its introducing commit is not an ancestor of
the oldest source first-parent commit not reachable from rolling (the source's first own
commit). Merging rolling into a source does not move that first own commit. An ordinary edit
of lines already present at that commit produces no warning.

`reverts_recent_landing.warnings` groups findings by `path` and `introducing_commit` (full SHA),
with `line_count` and `line_ranges` (inclusive, one-based line numbers at rolling). The object
also carries the pinned `rolling` SHA, `first_own_commit` (when present), `lines_blamed`,
`line_limit` (2000) and `file_limit` (64).
Blame batches all deleted ranges per file, skips binary files, and limits history to
`<first_own_commit>..<rolling>`: lines that reach the source's ancestry need no older
introducer lookup. The total check has a 10-second budget.
`status` is `complete`, `partial` (a bound or Git error interrupted coverage),
`unavailable` (Git/history could not be read), `skipped` (trial merge conflicted or unavailable),
or `opted_out`; non-complete coverage has a `reason` except for an opt-out. This feedback never
changes the exit code or refuses landing. A source with no source-only commits has no findings.

For intentional removals, put `Allow-Recent-Landing-Removals: true` in the trailer block of
any source-only commit message (after a blank line). This suppresses all findings for that
source and records `status: "opted_out"` and `opt_out_commit`; a marker inherited from rolling
does not suppress findings. Reviewers should inspect the intended removals when using it.

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
Public library item edits also run dependent modules' tests (#3408), alongside the
reverse-dependency test-target compilation (#473). Both scoped-test and the queue's
`derive_filters` use `check-touched-shards`: it compares public const/static, function
(signature and body), struct/enum and type definitions, then searches code identifiers
in every transitive workspace dependent, including imports/re-exports and qualified paths.
It selects exact runnable tests under referencing modules with their rsid/rsid-store shard
gates (`PACKAGE=test(=NAME)` for unsharded libraries); existing package/job budgets
and wider unfiltered leaf gates still apply.
`downstream_tests` in both receipts records items, dependent sources/modules, tests and
filters. Private, test-only and comment/format-only changes add no runtime observers.
Names shorter than four characters, common associated names (`new`, `default`, `from`,
`into`, `get`, `set`, `len`, `is_empty`, `clone`, `fmt`), or identifiers referenced in more
than 40 dependent files keep compile coverage without extra runtime selection; the receipt's
`notes` names each skip. This lexical scan conservatively includes same-name references.

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

**Lander environment allowlist (#3391).** The gate builds and tests agent-written
sources (build scripts, `RUSTC_WRAPPER`, python test modules, git hooks), so the
lander and every child it starts get an allowlisted environment, never the
daemon's whole one: the queue starts the lander (and its `systemd-run` scope and the
`check-touched-shards` deriver) through `rsid::lander_env_policy::apply`, and the
lander snapshots its own startup environment through the same filter. Passed:
`PATH`, `HOME`, `USER`, locale (`LANG`, `LC_*`, `TZ`), `TMPDIR`, toolchain and
cache families (`CARGO_*`, `RUSTUP_*`, `RUST*`, `SCCACHE_*`, `NEXTEST_*`), `XDG_*`,
the `RSI_*` knobs (`RSI_LANDER_*`, `RSI_ROLLING_*`, ...) and a few exact git names.
Dropped: every other name, `RSI_SESSION_TOKEN` always, and any name that looks like
a credential (`*_TOKEN`, `*_SECRET`, `*_API_KEY`, `*_PASSWORD`, `AWS_*`,
`ANTHROPIC_*`, `OPENAI_*`, `OPENROUTER_*`, `GITHUB_*`, ...) even in an allowed family
(`CARGO_REGISTRY_TOKEN`). A name that must pass anyway goes in
`lander_env_policy::EXTRA` by exact name with a reason (today: `SSH_AUTH_SOCK`,
`DBUS_SESSION_BUS_ADDRESS`, proxy and CA-bundle names). A new gate variable that is
missing in a child means adding it to that module in the same change.

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

**Lander shell suites (#3923).** Before Cargo receipt lookup or Cargo execution,
the lander independently selects TAP shell suites from the committed base and
candidate with the native `scoped_runner::residual::lane` selector. Changed
suites, wrapper-covered shell tests and covered script subjects add their suites
even when Rust filters are already selected. The existing Python bridge retains
its separate lane; this change does not cut over worker `scoped_test`.
The native `residual_run::execute` runner streams each selected suite and its TAP
output into the gate log, bounds each suite to 300 seconds and the lane to the
lander's guard timeout, and owns all TAP, timeout and missing-runner verdicts.
The lander resolves the installed guard through the trusted-path and identity
checks in `repo_guard_call.rs`, then invokes `rsi-repo-guard residual-lane`
with the committed base, candidate, checkout, receipt path and lane timeout. The
guard owns its PID canary/init, signal cancellation and suite logs; the daemon
package does not link the guard crate. The lander parses the receipt, checks its
commit identity and complete inventory, and refuses any red or malformed result.

Every shell lane, including an empty selection, writes `shell-suites/gate-*/receipt.json` under the
candidate receipt root (or `gate-receipts` beside the candidate worktree when
that root is disabled). The `shell_gate_receipt=<path> <json>` log line retains
the same receipt after private checkout cleanup. It names the committed
`receipt_head`, the complete `selected` inventory, and typed `residual` results
including suite paths, executed counts, status and suite logs. A failed suite
makes `ok:false` and refuses the gate before Cargo. Shell suites run again on
cached Cargo gates and stale re-gates, so reuse cannot omit shell coverage.

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
- Desktop build and install evidence (hub #3239) is a job, never a hand-launched `systemd-run` or
  foreground cargo: `AgentSubmitJob {"kind":"test","params":{"recipe":"desktop-install"}}` (declared in
  `.rsi/jobs.toml`; `desktop/src-tauri` is a standalone workspace, so no `build` job names it). The
  recipe runs `make desktop-install-job` -> `scripts/desktop-install-job.sh`, which runs
  `scripts/install-desktop.sh --required` (`npm ci`, `npm run build`, a release `cargo build` of
  `desktop/src-tauri`) in the same `rsi-build.slice` budget as every build/test job (recipe default
  90 minutes, 400% CPU, lowered by the hub budget). The script gives the install a job-owned
  `HOME` and `XDG_{CONFIG,DATA,CACHE,STATE}_HOME` under the job's private `TMPDIR`, removes them on exit and
  never writes the operator's real HOME, `~/.local/bin` or app menu (`CARGO_HOME`/`RUSTUP_HOME` keep their
  real values so the toolchain and crate cache still resolve). The receipt carries the exit code and the log
  path; the log ends with `desktop-install-job: exit=<n>`. The recipe takes no argument or other
  workspace: the script refuses any argument (`desktop_install_job_args_refused`) and a request for any
  other recipe name is `job_recipe_not_allowed`. Test: `scripts/tests/test_desktop_install_job.sh`.
- Real-rsid desktop graph-run evidence (hub #3279, #3000 criterion 4) is a job:
  `AgentSubmitJob {"kind":"test","params":{"recipe":"desktop-e2e-graph"}}` (`make desktop-e2e-graph-job` ->
  `scripts/desktop-e2e-graph-job.sh`). The `desktop-tests` lane's `RSI_E2E_RSID` test is skipped without the
  opt-in, so the lane's green is not proof it ran. The job builds `rsid` + `rsi-e2e-seed` (`--features e2e-seed`,
  debug), runs the `graph-run` scenario against the isolated rsid and judges `e2e-report.json` with
  `desktop/scripts/graph-run-receipt.mjs`: it exits non-zero unless `graph-seeded-daemon`,
  `graph-seeded-render` and `graph-seeded-readback` are all present and `pass`. The receipt
  (`graph-run-receipt.json`: `executed`, `skipped`, source SHA, daemon sha256, command, exit code) and the raw
  `e2e-report.json` are both printed into the log (the default output directory sits under the job's removed TMPDIR; #3292). Tests: `desktop/tests/graph-run-receipt.test.mjs`, `scripts/tests/test_desktop_e2e_graph_job.py`.
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

## Linux compile lane on a non-Linux host (#253)

A Mac (or Windows) `scoped_test` builds and tests only its own target, so the Linux side of code
gated on `cfg(target_os = ...)` (for example a `cfg(any(target_os = "macos", test))` seam with
macOS-only `libc` parts) is never compiled there. `scripts/scoped-test` therefore adds one lane on
a non-Linux host: after the native package lanes, and inside the same build admission (cargo-slot
lease, foreground process, the run's remaining budget, no detached process), it runs

`cargo check --target x86_64-unknown-linux-gnu --all-targets --keep-going --message-format=json -p <pkg> ...`

for every workspace package under `crates/` whose `.rs` sources or `Cargo.toml` the diff changes,
plus the downstream compile set, with the features their native lanes built. A docs-only file
inside a crate selects nothing; a root `Cargo.toml`/`Cargo.lock` change is left to the native lanes
and the merge-queue gate. A Linux host has no lane: its native lanes are the Linux build.

The receipt's `linux_check` object (absent on a Linux host) has `status`, `reason`, `target`,
`packages`, and when it ran `command`, `log`, `exit_code`, `elapsed_sec` and `rust_errors`:

| `status` | `reason` | Meaning |
| --- | --- | --- |
| `passed` | | The Linux check compiled every target. |
| `failed` | `linux_check_failed` | A rustc error; the run is red (`linux_check_failed: <first error>`) and `first_error` names it with its file and line. A red Linux check also keeps a baseline-only run from being `ok`. |
| `not_covered` | `linux_target_missing` | The rustup target is not installed. The job never installs it; `remedy` holds the one-line `rustup target add x86_64-unknown-linux-gnu` for the operator. |
| `not_covered` | `c_toolchain_missing` | No C cross toolchain was found before the run (`remedy`: `brew install zig` on a Mac), or only build scripts failed for want of one or a sysroot (cc-rs, pkg-config; `build_scripts` names them). Never a red. |
| `not_covered` | `c_toolchain_setup_failed`, `build_script_failed`, `cargo_failed`, `timed_out`, `budget_exhausted`, `native_build_failed`, `admission_lost`, `rustup_unavailable`, `package_selection_failed`, `not_reached` | The lane could not say; never a red. `not_reached` is a run that stopped before Cargo. |
| `not_applicable` | `no_touched_rust_package` | No Rust package changed. |
| `planned` | | `--dry-run`: the command the run would execute. |

When the lane runs, `c_toolchain` names the C compiler its build scripts used (`kind`
`configured`, `cross_gcc` or `zig`; `env` lists the variables the lane set).

### C cross toolchain and one-time Mac setup (#256)

`cargo check` links nothing for the target, but build scripts compile C for it: bundled SQLite
(`libsqlite3-sys`, `sqlite-vec` in `rsid`), `ring`, `zstd-sys`, `blake3` and the tree-sitter
grammars. The lane picks the first of:

1. `configured`: the environment already names a compiler for the target
   (`CC_x86_64_unknown_linux_gnu`, `CC_x86_64-unknown-linux-gnu` or `TARGET_CC`); the lane adds nothing.
2. `cross_gcc`: `x86_64-linux-gnu-gcc` on `PATH` (cc-rs finds it by its target prefix).
3. `zig`: `zig` on `PATH` (not on Windows). The lane writes `cc`, `c++` and `ar` wrappers around
   `zig cc -target x86_64-linux-gnu` under `<target dir>/x86_64-unknown-linux-gnu/rsi-zig-cc/` (the
   wrapper drops cc-rs's clang `--target=` argument, which zig refuses) and sets
   `CC_`/`CXX_`/`AR_x86_64_unknown_linux_gnu` plus `ZIG_GLOBAL_CACHE_DIR`/`ZIG_LOCAL_CACHE_DIR`
   (`rsi-zig-cc/zig-cache`) in the lane's own process environment only. A wrapper file is rewritten
   only when its text differs, so the compiler path build scripts fingerprint stays stable and a warm
   run reruns no build script.

None of these is set in a shell profile or a global Cargo config, and the native lanes never see
them. With none found the lane is `not_covered(c_toolchain_missing)` before Cargo starts.

One-time setup on a Mac satellite (the operator or a worker allowed host setup runs it once):

```bash
rustup target add x86_64-unknown-linux-gnu   # for the pinned toolchain in rust-toolchain.toml
brew install zig
```

Measured on the Mac Mini satellite (12 cores, `CARGO_BUILD_JOBS=4`, `nice -n 19`) for `-p rsid`
(#256): 153 s from an empty Linux target dir, about 21 s after a one-file `rsid` edit, under 1 s with
nothing changed. Mac job units run with `CARGO_BUILD_JOBS=2`, so a cold lane there takes longer; it
stays inside the run's remaining budget like every other lane.

`not_covered` is the signal for the hub PM or the lander to add a Linux all-targets check for the
source; `passed` or `failed` makes that hand check unnecessary. Linux-lane warnings are not judged by
the compiler-warning gate (it reads the native build only).

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
and macOS and Windows builds (macOS: see the host warning lane below). The gate is per package, not per target: a warning from any unit of a
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

Host warning lane (#348). The hub gate builds Linux only and a Mac `scoped_test` judges only the packages it
touches, so a macOS-only warning (Linux-only helpers or imports that are dead code on another target) lands on
rolling unseen and reddens the next Mac `scoped_test` touching that crate (sixth time: #9, #12, #19, #145, #154,
#347). `scripts/check-host-warnings.sh` is the detector for the host it runs on: it runs `cargo check
--workspace --all-targets --message-format=json`, counts warnings exactly as the gate does (per package, key
`file|lint code|message`, positions deduplicated) and exits 1 naming each baselined package/key above its
baseline; exit 2 means no verdict (missing `jq`/`cargo`, failed build, unreadable baseline), never green.
The macOS satellite runs it on current `origin/rolling` (a warm run is about two minutes) on a regular
cadence and files or fixes the warning before a worker trips over it; `--from-json FILE` judges a saved
stream, which `scripts/check-host-warnings.selftest.sh` uses (it sits outside `scripts/tests/` until the Python-owned `SHELL_TESTS` list in `scripts/tests/test_shell_tests.py` may name it). The fix is the same as for any host-only
warning: `cfg`-gate the item to the platform that uses it, `#[cfg(test)]` a test-only helper, narrow
visibility; the baseline is never raised and holds the Linux-reachable list only. A lander- or
`scoped-test`-side macOS target check is a gate change in the Python bridge (#3482) and belongs to the Rust
port of the gate (#423), where a per-host baseline section can be added to `tools/warning-baseline.json`.

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
outcome wake carries either result. A derivation that exceeds its budget (#3372: it times out at
180 s, or the diff changes more than 300 files, which the checker cannot trace in time) is not
refused: the entry is kept with `filters_state: lander_selects` and only the filters you named, and
the lander's own bounded closure selection plus the automatic full-library QA cover it. Always
derive; never re-enqueue without `derive_filters` to work around a slow derivation. End the turn; never sleep, poll or call again for the
filters (a replay under the same `idempotency_key` returns the same entry). A `queue_probe_timeout`
or `queue_tip_read_timeout` refusal (a git read of the call stalled at the bound and was killed,
#2912) records nothing and is real git trouble, so it stays andon friction (#2992): call again
with the same `idempotency_key`. Combine sources into one gate only when they
conflict and you have resolved the conflict, or when they are docs-only. A
gate's cost must scale with the change: if its filters span more than a few
rsid shards or any rsid-store module, split it. Batch waits (several jobs, one
`mode:"when"` wake), never landings. Evidence: `docs/agents/reference.md`.

A merge that conflicts only in the root `Cargo.lock` (queued sources that each add a workspace
member, #3717) is not a conflict: the integration engine takes the target's lock, runs
`cargo update --workspace --offline` in the candidate worktree and commits the merge when every
registry/git package of the result is already in one of the two locks (`integration/cargo_lock.rs`);
a `Cargo.toml` conflict, a lock Cargo cannot derive offline or a version neither side locked is
still refused `Conflict { paths: ["Cargo.lock"] }`. The normal compile gate verifies the result.

`queue_disabled` is an operator setting: keep the commit pushed and report it
with `AgentReportUp`, or send the result with `AgentReportToHub` on a satellite.
The operator enables the queue or lands with `rsi land` or
`EnqueueRollingQueueSource`.

The queue publishes with `rsi-rolling-land` (installed on PATH next to `rsid` by
`make release-install`, with the Rust `rsi-repo-guard` executable it runs as a bounded
child, #451: built in its own cargo invocation and never linked into `rsid`, since
`rsi-repo-guards` enables `serde_json/preserve_order`); agents never run it directly, because a direct landing
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
the candidate first and builds the base only for a candidate failure (#1244),
with one exception for entirely known-red e2e selections (hub #3483): before the
run, a successful, untruncated libtest/nextest listing (taken only for e2e targets before the run, and for other shards only after a candidate timeout, so a finishing candidate pays no listing invocation) must establish the complete,
nonempty selected test set. A listing that reaches the output tail byte limit is
incomplete. When every selected e2e test has a live known-failure record, the
gate obtains base evidence first to establish the timeout cap. Other shards
consult the base for timeout attribution only after the candidate times out.
Only if every selected test actually fails on that base and its base output
matches its known-failure signature may a whole-command `TimedOut` be recorded
as those base reds (`known_red_timeout`),
continuing the comparison without `candidate_failures_unparsed` from that shard.
A mixed selection, an unknown/empty/incomplete listing, missing base evidence,
unavailable base compilation, missing signature text, or a claimed fix preserves
the existing refusal; a partial timeout log never proves selection. The base
shard cache and narrowed per-test outcome cache persist each base run's own
failure text and restore it on reuse. Older names-only entries remain readable
but cannot excuse or shorten a timeout. Existing names-only full-result slots
are not upgraded on a hit; their timeout refusal persists until rolling advances
and a new base slot is filled.
A positively green candidate needs no base exemption; an unused early base
probe's failure does not prevent reusing that green result. Non-timeout guard
errors retain their existing policy.
If another selected command is red or proves no tests ran, the compile-repair
check probes skipped green targets on base with `--no-run` (#3839). It never
repeats their test suites or caches these compile probes as test outcomes.
An uncompilable base still requires every selected candidate run to be proven
green; baseline reds and empty runs do not excuse a partial compile repair.
An e2e shard with this complete proof runs with at most **120 seconds** of
candidate guard time (or its existing shorter timeout). This bounds one known-red
hang to two minutes instead of consuming the gate budget; mixed/unknown shards,
base evidence collection and prebuilds retain their existing timeouts. Known-red
records alone never shorten a shard that could select a non-red test.
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
excuses nothing, and a Rust test the selection never runs counts as red, so add a
filter that selects it; a Python unittest id `scripts.tests.<module>.<Class>.<method>`
or `tools.tests.` needs no filter, the gate adds that module to its Python selection
and runs it, #3320) or the landing is refused `claimed_fix_still_red` naming
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
bounded by the wall time of each lander run (`rolling_queue_gate_timeout_mins`,
default 60, per run, #3267; past it the run's still-suspect members are refused
`queue_gate_timeout`), not by a count. Direct
landers and the queue both publish by fast-forward, so each is just an advance to
the other; global-manager and laptop lanes land through the queue. Detail:
`docs/agents/rsid-rpc-contracts.md` ("Rolling merge queue"). The lander's policy is mechanical and
runs before any test: no Work binding, seal or hot-file claim is required, and a
new migration must be the rolling tip's schema head + 1 (the highest
`store/migrations/vNNN.rs`) as its own file with its `if version < N` block;
one landing may carry several migrations only as a contiguous run from that
number, each in its own file (the refusal names the number). The pin in
`tools/released-migrations.json` is derived by the lander (#1949: after the merge it runs
`scripts/repo-guard released-migrations --refresh <tip>` in the candidate worktree and commits
the manifest as `rsi rolling landing: derive released-migration pin`, so the candidate the gates test
and the commit it publishes carry the pin; a stale retry drops that commit and derives it again on the advanced tip;
a refresh that fails, such as a changed released block, derives nothing and the released-migration
guard refuses with the same reason) and `released-migrations` accepts a source whose pin file is
still the base's (`pin deferred to the lander`), while a hand-edited pin is judged strictly as before.
A slice that follows a reviewed but unlanded sibling and omits its migration
declares the absent number as `pending_predecessors` (#1285). If a merged source
contains both complete migrations, an origin/rolling-based `--transform` refuses
the two new units even when the released-migration guard passes (#545).
The PM lands the sibling first; the worker then merges rolling, reconciles the
active declaration and validates the remaining unit. An unlanded sibling head
with a deferred pin cannot serve as the renumber tool's base/target workaround.
See `tools/provisional-migrations/README.md`, "Preflight when the source contains
an unlanded sibling", for the full sequence. A single accepted source may
contain one provisional-migration declaration; multiple declarations
are refused at `AgentEnqueueLandingSource` with
`queue_multiple_provisional_migrations`; drop the extra provisional declarations
or split the migrations across separate landings. The
lander never reads the manager ledger (#1927).
The operator lands a branch the same way: `rsi land <session-id|branch>`, the
TUI `<Space>gl` / `:land`, or `rsi-rpc EnqueueRollingQueueSource` (#2055). A manual
merge and push skips every gate, so the pre-push hook runs the fast static guards
on an unmarked rolling push (`RSI_SKIP_STATIC_GUARDS=1` overrides). The static guards
refuse added tracked `.py`/`.pyw`/`.pyi` files and Python shebangs, including
non-executable sources (`scripts/repo-guard new-python BASE HEAD`, #407, #583,
operator directive #358). Existing modifications/deletions stay allowed during
conversion; only non-executable regular archived data under `thoughts/` without
a Python shebang is exempt. Classification reads exact index/commit blobs;
symlinks resolve inside that Git tree, with unavailable/unsafe evidence refused.
Rename/copy destinations count as additions. Pre-commit checks the index before
the role-specific checks; pre-push checks the exact pushed diff before legacy
checks. The unchanged legacy scoped runner reaches the guard through its mandatory
`starter-pins BASE HEAD` Rust entry point; the Rust runner has a mandatory
`new_python` verdict directly. New static guards and their tests go in
`crates/rsi-repo-guards`, through `scripts/repo-guard`; follow
[`crates/rsi-repo-guards/README.md`](../../crates/rsi-repo-guards/README.md).
The other static guards remain mandatory. Those that
`scripts/scoped-test` runs on every verification include `starter_pins`
(`scripts/repo-guard starter-pins`, #3070): a changed starter topology JSON must move its
digest fixture in `starters_tests.rs` and append the digest it replaced to
`SHIPPED_BEFORE` in `starters.rs`, so a half-updated starter fails before any test.
`personal_paths` (`scripts/repo-guard personal-paths`, #3269) runs there too: a tracked file
outside `thoughts/` with a personal home path or account name, or a globally routable
unicast IP literal (multicast 224/4 and reserved 240/4 are not), is refused at landing,
so the public export stays publishable. The pre-push hook runs the same guard on the
files an unmarked push changes.
`agents_md_size` (`scripts/repo-guard agents-md-size`, #3450) runs there and in the pre-push
hook too: a regular `crates/*/AGENTS.md` or `CLAUDE.md` over its ceiling (12 KB by default,
an explicit lower or grandfathered entry in `CEILINGS` of its module, never raised) is refused, because
Claude Code loads it into every session that reads the crate. Keep one line per fact there
and put the detail in `docs/agents/` (rsid: `docs/agents/rsid-daemon-facts.md`).
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
the landings; an eight-hour overdue Issue keeps stalled obligations pending (#2092). Each pass first credits a pending landing that is an ancestor of the project's latest green run (`qa_green`): it settles from that run (a `credited` finished receipt, `qa_green` unchanged), its overdue Issue closes, and none is filed (#3733).
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
workers follow `docs/agents/worker-contract.md`.
