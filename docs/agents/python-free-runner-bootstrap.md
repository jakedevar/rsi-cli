# Python-free runner bootstrap (#423)

This is a staged implementation, not a scoped-test acceptance route. The
canonical `scripts/scoped-test`, `scripts/cargo-slot`, daemon tool-copy selection,
and mandatory gate callers have not switched. Do not run the old Python entry
points. Exact Rust package/build jobs on native macOS use `/usr/bin/env` and can
verify this crate without invoking the old runner.

The S1 `rsi-repo-guards` crate now contains the governor client and scoped worker
components. `rsi-scoped-test --execute-plan FILE` requires a held admission and
checks the package plan's exact `head`, clean worktree and compiled source stamp before execution, then rechecks source on completion. The package
worker emits the existing `SCOPED_*` protocol, never a successful acceptance
receipt. The staged command is now the full native scoped-test (see "Native command" below);
it can only finish green when every mandatory contract is supplied, and
explicit filters and dry runs cannot bypass any gate.

`scripts/cargo-slot-rust` preserves the canonical shell's policy, deadlines,
pools, FIFO order, reclaim and lease release. A Rust foreground supervisor forwards termination to its own child group and reaps descendants, with its embedded
interpreter fragments replaced by `rsi-repo-guard cargo-slot` operations. Its
local resource probes are Linux-only; another Unix host needs the governor and
gets an explicit unsupported error if local fallback would be needed. Socket
requests have a total deadline, bounded replies and strict response/grant
validation. Timeouts/EOF/reset retain the queued ticket and retry; an explicitly
bad ticket resets it. Status includes daemon and live local holders/waiters.

Admission must not build its own executable before acquiring a lease.
`scripts/repo-guard --existing-bin` therefore only locates a prebuilt binary and
verifies its embedded content identity against all crate Rust sources, its build
script, both manifests, Cargo.lock and the toolchain pin. Missing/stale binaries
fail closed without invoking Cargo. A daemon build job must prepare/install the
binary before enabling the new cargo-slot path; deployment must carry this
bootstrap binary independently of a clean sandbox's target directory. Normal
S1 guard bootstrap remains unchanged.

Integration dependencies:

- #426: Python-free `check-touched-shards` derivation and inventories. Its
  existing CLI and queue JSON are a contract; the runner also needs the current
  shard ownership, required-feature inventory, changed-module/direct-observer
  ranks, slow-tier and downstream compile/test decisions. #362 owns six helpers,
  not this derivation program.
- #361: released/provisional migration validation and renumbering.
- #362: six static/shard/failure/recent-landing helper ports. Preserve their
  current CLI arguments/JSON/refusal fields and expose reusable Rust entry
  points through the S1 wrapper; record final subcommand/API names in its handoff.
- #363: desktop/QA/lander helper ports, including isolated desktop execution and
  lane selection. Missing desktop tooling stays red when required.
- #366: residual suite selection/execution replacing the Python lane, preserving
  actual executed counts and fail-fast runner-unavailable/zero-test behavior.

The parent scheduler now has a Rust `orchestration::Backend` boundary with no
successful default implementations: static guards precede fail-fast residual
suites, then desktop, then one admission across bounded package workers. Desktop
reds retain their evidence while Cargo continues. Package budgets exclude the
admission wait. Changed-module tests survive a spent test box; remaining filters
retain explicit coverage and merge-gate deferral. Warning evaluation, named
baseline comparison, Linux cross-check, merge provenance and cleanup are required
backend operations. Progress is always red. Terminal aggregation requires all 13
static gates and both lane slots, clean-source attestation, complete warning
evidence and classified failures. Missing/malformed evidence cannot become green.

The worker supports concurrent libtest shards, nextest exact filtersets (with
unsafe names falling back to libtest), optional e2e prebuilds and private tmpfs
scratch. It reports only observed verdicts; a zero-test or missing-verdict success
is red. Started tests killed by a signal remain unfinished, with binary-scoped
completion and coverage accounting. Linux tmpfs probes are gated, and unavailable
parity remains explicit. The non-Linux foreground path has no systemd CPU cap.

Still incomplete in #423: production adapters from #361/#362/#363/#366/#426 into
the parent scheduler; integrated-base/provisional-renumber scratch provenance;
actual baseline worktree replay and non-job classifier fallback; native Linux
cross-check package selection/wiring; daemon
materialization/bootstrap and the canonical CLI switch. The source pin checks
start/phase/end and now monitors long package work once per second. Each monitor
probe has a shared ten-second deadline across HEAD and worktree status checks;
movement cancels the owned work and retains `job_source_moved` as the error.
This is bounded observation, not a continuously frozen checkout; transient
changes restored between samples can still escape observation. None of the
remaining parity requirements are waived by the component tests.

The orchestration path now annotates reproduced baseline failures directly from
the original admitted repository's
`target/.rsi-tmp/scoped-test/known-failures.v1.json`. It reads one bounded regular
file per batch, validates the complete export, rejects future/24-hour-old data,
and caches exact test names inside a ten-second annotation budget. Missing or
malformed data is `unavailable`; a valid export with no active exact record is
`unknown`. Published `4e530593e` QA Issue advisories are exact-name, sorted,
deduplicated context only. Bad optional advisories retain `unknown` and never
turn it into `unavailable` or establish baseline evidence. Annotations now match
all four package/kind/target/test identity fields before baseline-only acceptance
can qualify. The backend still owes an actual complete base replay before this
annotation step; snapshot records and advisories cannot establish that replay.

`scoped_runner::persistence` now publishes the complete immutable selected scope
before lanes and atomically updates compact `progress.json` on package phases.
`RunPlan` supplies a pre-created private `log_dir` plus full `downstream_compile`
and `downstream_tests` values. In production, keep that log directory beneath
ignored job scratch so progress writes do not dirty the pinned checkout.
`selection.json` has sorted ASCII JSON, a trailing newline and SHA-256 identity;
its per-field identities omit the newline, matching the retained contract.
Progress always stays red, even after a baseline-only result. Terminal wire
output compacts only oversized selection fields and keeps all acceptance,
failure, coverage and merge evidence inline. Artifact failures remain red.
Native backend/CLI log-directory allocation and downstream derivation wiring
are still required; the staged default CLI does not claim to execute this path.
The shared `wire_json` primitives were extracted from #363's tested
`c80a8762f:qa_fingerprint.rs`; the integrated fingerprint module re-exports
`sha256`/`legacy_json` from `wire_json` to keep one implementation.

`scoped_runner::linux_check` provides configured-compiler/cross-GCC/Zig
discovery, bounded installed-target lookup, stable quoted Zig wrappers and
isolated cross-lane environment construction. `run_pending` requires a matching
clean `SourcePin`, selected scope and held host admission, supervises the owned
command with source monitoring and writes `linux-check.log`. Rust errors stay
red; unavailable targets/toolchains, build-script capability failures and cross
timeouts keep their explicit `not_covered` reasons. No target/compiler is
installed. The native backend still must supply the full changed-Rust/manifest
and downstream package set, preserve features and call this helper after native
lanes. Shell-stub tests are process-contract evidence, not a real Linux cross
compile or Linux acceptance.

The first-publication boundary is source-checked in
`crates/rsid/src/agent_jobs/scoped_tool.rs`: `resolve` already permits candidate
tools when the candidate changes a watched tool file. `TOOL_FILES`, helper
extraction and `materialize` currently cover script siblings, not a Rust workspace
source closure or binaries. `agent_landing_candidate::check` requires an actual
owned daemon `scoped_test` job with a matching receipt head. An ordinary Rust job
or this staged executable's diagnostic receipt cannot satisfy that check.
The coherent switch must extend the existing source-pinned materialization to
include the full runner/helper Rust closure and build the content-verified
executables under supported admission. It must preserve the real `scoped_test`
job kind and receipt/candidate checks; no candidate exception or manually
injected receipt is a bootstrap solution.

Before switching callers, integrate every dependency, finish the parent and
worker parity items, teach tool-copy selection to include Rust sources and the
content-verified binaries, then run an admitted exact-head full scoped test and
candidate gate. Delete legacy files only in the same coherent switch. No
candidate-check exceptions, hand-authored acceptance receipts, or omissions of
static, residual or desktop lanes are permitted.

Shared PID isolation contract (#433): `rsi_repo_guards::pid_isolation` is the
single probe/wrapper implementation for the runner, retained QA paths and hub
materialization. `probe(ctx, canary_command)` takes an already-built helper
command, e.g. `[rsi-repo-guard, pid-isolate, --canary]`. The same binary runs the
Rust PID-1/fresh-proc canary. It tries the current UID/GID first, then root
mapping, with a 20-second bound per attempt and bounded stderr diagnostics.
`Isolation::wrap` returns the unshare prefix plus the child argv only when the
probe succeeds. `status().record` writes `pid_isolation` and the optional
`pid_isolation_reason`, removing a stale reason after success. Keep one immutable
probe result per run; the orchestration backend now passes it to residual,
desktop and package operations. The host admission owner stays outside; each
adapter must wrap its bounded child unit inside that owner's lifecycle. Never
wrap the lease holder itself. Non-Linux/unavailable commands retain their former
unwrapped behavior; required isolation refuses with exit 3.

The registered `pid-isolate` guard supports `--status`, `[--require] -- COMMAND`
and the internal `--canary HOST_PID HOST_PARENT` entry. The staged
`scripts/pid-isolate-rust` entry locates a content-verified prebuilt binary and
never builds before admission. It does not replace the canonical legacy path
until all its callers and trusted source snapshots switch together. Production
adapters still need to apply `Isolation::wrap`; passing/recording status alone
is not execution parity. The staged diagnostic receipt explicitly says
`unavailable/not_probed`, stays red and is not an isolation claim. Native Rust
integration tests cover the CLI on macOS; Linux-only tests exercise a real host
decoy and namespace output/exit behavior when available. Full Linux acceptance,
including wrapper termination and bounded unit placement, remains a hub gate.

Reaping init (hub #3667): inside the namespace the first process is PID 1 and
never reaps an orphaned grandchild, so a dead orphan stayed a zombie and still
answered `kill(pid, 0)` (legacy `scripts/tests/test_install_release_start.py`
failed 2 of 38 under `pid-isolate.py` for exactly that). `Isolation::wrap` now
inserts `[helper, pid-isolate, --init, --]` between the `unshare` prefix and the
command, derived from the canary command's own helper path. `pid_init` runs the
command as its child, forwards TERM/INT/HUP/QUIT/USR1/USR2, loops on
`waitpid(-1)` until the command exits and exits with its status (128+signal when
killed). On Linux it also sets `PR_SET_CHILD_SUBREAPER`, so the reaping contract
is testable without a namespace; other Unix hosts rely on the host init, and
non-Unix reports `--init` unsupported (exit 3). The orphan/ESRCH tests are
Linux-only (`init_reaps_an_orphaned_grandchild_so_kill_zero_reports_esrch` and
`namespaced_command_orphans_are_reaped_by_the_init`); the init is tested only
through the built binary because `waitpid(-1)` in the shared test harness would
steal other tests' children. The legacy Python file is not edited.

## Native command (#423 checkpoint)

`rsi-scoped-test` (staged entry `scripts/scoped-test-rust`, never the canonical
`scripts/scoped-test`) now plans and runs the whole lane set:

- `runner::run` parses the retained flags (`options`), resolves and pins the
  source (`SourcePin`, clean worktree, `HEAD == --head`), detects a declared
  provisional migration (`tools/provisional-migrations/*.json` added since the
  merge base) and, only then, asks `Contracts::renumber`; builds the integrated
  base (`provision::integrated_base`), derives through `Contracts`, orders and
  boxes the plan (`planner`), allocates a private scratch `log_dir` outside every
  protected root and prints the typed `SCOPED_TEST_RECEIPT`. Every pre-run
  refusal is a red receipt with `failure_class` (`incomplete_contract`,
  `job_source_moved`, `runner_unavailable`, `derive_failed`, ...).
- `host::Host` is the production `orchestration::Backend`: the thirteen
  static gates (`guards`), the retained desktop lane (`desktop_selection` +
  `desktop_execution::ProcessBackend`), one held admission (`Admission`, the
  `cargo-slot` holder is this binary's `--hold-admission`), bounded worker units
  (`provenance::bounded_command` on Linux, direct elsewhere) under the source
  monitor, the warning gate, the cross-compile lane on non-Linux hosts, the
  trial merge and `--reclaim-incremental`. `rsi-scoped-test pid-isolate` and
  `rolling-migration-renumber` make this one binary its own content-verified
  helper; the renumber step is a killable 300 s child, never an in-process call.
- Scratch provenance: `integrated_base` writes a synthetic merge commit into the
  original repository's object store; a renumbered candidate runs in a
  `ScratchWorktree` (detached, outside the checkout, deregistered on drop) while
  the compiled source stamp is still verified against the original checkout
  (`PackagePlan.tool_repo`). The receipt `head` is the worker's commit.
- `baseline::compare` replays only the failing tests, by exact identity, on a
  detached scratch worktree of the base in a replay-owned target
  (`<target>/scoped-baselines/run-*`, swept after six hours), then
  `with_fork_point` replays failures the tip does not reproduce at the fork
  point (`stale_base_reds`). A nonzero exit without the worker's `SCOPED_EXECUTED`
  attestation is incomplete; nothing is inferred. Before the first package the
  empty replay target is offered the warm pool through `rsi-scoped-test
  target-pool seed` (a killable 300 s child; `target_pool.rs`, #446), so replays
  start from the nearest ready ancestor pool instead of a cold build. Every
  `target-pool` subcommand is native now (`seed`, `key`, `build`, `seed-from`, `gc`,
  `reclaim-idle`, `refresh`, `status`, `summary`; #446/#453, see "target-pool" below);
  the Python tool and its callers stay until the switch it names.
- Contracts: `converged::Converged` consumes what siblings published as callable
  Rust, in process or as a killable child of this binary:
  - #426 (`derivation.rs`): `derive` is `touched_shards::derive::{Loaded::load,
    derive}` with `filters_only`; the `RSI_TOUCHED_SHARDS_STAGED` opt-in guards
    only the standalone `check-touched-shards` CLI, never this call. A refusal
    is exit 2 with an empty selection and the message in `derivation_output`;
    nothing partial is ever used. `feature_test_inventory`, `load_slow_tier` (an
    unreadable config is the full tier with `config_unreadable: ...`, never a
    silent skip) and `downstream_test_plan` read the committed trees at `head`.
    The call runs on a worker thread under a 10 minute deadline and the run's
    cancel flag.
  - #362: the shard inventory, and every static gate except `whitespace` (shard,
    target-reclaim, store-boundary, guard-regressions and the seven in-crate
    gates) runs as a killable child of this binary (30 min; guard-regressions
    15 min). `recent_landing::git_diagnostic`/`reason` build the trial-merge
    diagnostic (`merge_check_reason` and `merge_check_diagnostic` are byte-equal
    to the recent-landing check's) and `recent_landing::advisory` fills
    `reverts_recent_landing` from the clean trial merge's tree (a renumbered
    candidate merges, the worker's own head is blamed).
  - #361: the released-migrations gate and the renumber tool.
  - #363: only the desktop gate is in the retained scoped-test path (already
    `desktop_selection` + `desktop_execution`, not duplicated). The landing
    guard and `run-rolling-qa` are lander steps, not scoped-test steps; they stay
    with the lander switch (#363/#3621).
  - #366 (`residual.rs`, `residual_run.rs`): the `scripts/python-test-gate.py` selection
    is ported exactly (read from the committed trees at `head`) and extended to bash TAP
    suites, which the runner executes (see "Residual suites" below). A suite that is
    still a Python file is a red `incomplete_contract` entry (`owner: "#366"`,
    `executed_tests: 0`) naming it; a suite stops being selected when its `.py` is
    deleted by the port. Nothing is skipped, waived or run by another runner.
  Sibling checkpoints were merged by ancestry: #363 `88e205472`, #362 `42f13a33d`
  (which contains #426 `bc6fb37bc`), #361 `88f500e7a`.
- Receipt details closed in this pass: a dry run may inspect a `--head` other than
  the checked-out HEAD (`SourcePin::inspect_only`; a real run still refuses it as
  `job_source_moved`); `--tmpfs` with no tmpfs skips with exit 0 as the legacy
  runner did, but the receipt is `ok: false`, `failure_class: tmpfs_unavailable`,
  `skipped: true`, `scratch_mode: unavailable` so no gate can read it as
  evidence; the `deferred` block's `tests`/`tests_basis` and the receipt's
  `filters_bounded`/`deferred_tests` are filled from the workers' listed identities
  (never a pass) instead of staying `pending`.

Verification at this checkpoint is component evidence only: native exact jobs
over the modules above (real Git fixtures; scripted workers), a host-neutral
end-to-end run of the production `Host` against a scripted worker (macOS/other
Unix; Linux wraps the worker in a systemd unit and needs the hub lane), the CLI
contract (`tests/scoped_cli.rs`) and the full crate library suite. None is a
daemon `scoped_test` receipt.


### target-pool (#446 / #453)

`rsi-repo-guard target-pool` (and `rsi-scoped-test target-pool`) implements every
subcommand of `scripts/target-pool`, byte-compatible with it: pool key, marker and event
files (`<sha>-<key>.json`, `events.state`, `repo-path`), the `<pool>.use` shared/exclusive
locks and `.build.lock`, and every printed JSON line, so a pool directory is read and
written by either tool.

- `target_pool.rs`: `seed` (incl. `--build-missing`: nearby pool skips the one-off build and
  records `pool_skipped_nearby`; a missing pool or a seed at the `origin/rolling` tip requests
  a detached build) and `key`.
- `target_pool_ops.rs`: `gc` (keep N, pressure, stale pools beyond the seed distance of a
  ref, a plan outlived by a rebuilt pool of the same name removes nothing), `reclaim-idle`
  (bounded by `--min-interval`, skips pools a seed reads), `refresh`, `status`, `summary`,
  the disk floors (`pool_skipped_disk_headroom`, quarter-of-disk cap, the workers' floor) and
  the events they record.
- `target_pool_build.rs`: `build` (detached worktree, seed from the nearest ready ancestor,
  `cargo test --workspace --no-run` through `cargo-slot` when installed and not held,
  `--keep-going` plus `cargo build --workspace` for a `degraded` pool, `pool_aborted_disk_floor`
  stop with the partial pool removed), the detached request (`systemd-run` into
  `rsi-build.slice` with CPUQuota and RuntimeMaxSec, else a niced detached process) and
  `seed-from` (ssh + `tar | zstd`, `du -sk` on the peer; needs `zstd`).
- `target_pool_cli.rs`: the command lines, with argparse-style usage errors (exit 2).

The callers are switched (#453). `scripts/target-pool` is a POSIX shim that finds
`rsi-repo-guard` and runs `target-pool "$@"` (lookup order and the exit-127 refusal are in the
script and in `scripts/tests/test_target_pool_shim.sh`); every caller keeps the one path
`~/.rsi/bin/target-pool`: `scripts/cargo-slot` (`reclaim-idle`), `rsid`'s `execution_scratch.rs`
and `target_pool.rs` (`seed`/`gc`/`refresh`/`summary`), `install-release.sh` (which now builds
`rsi-repo-guard` and publishes it beside the shim in `~/.rsi/bin`), and the deploy (which
syntax-checks the shim with `bash -n` instead of parsing Python). `make target-pool-build` runs
`scripts/repo-guard target-pool build`. `scripts/tests/test_target_pool.py` is deleted; its 75
cases are mapped, one by one, in `crates/rsi-repo-guards/src/target_pool_case_map.rs` (an
executable table: a mapped test that does not exist fails), and the real-Cargo ones run in
`crates/rsid/tests/target_pool_cargo.rs`.

Still open, and why:

- **The guard binary is not a `DEPLOY_BINARIES` member.** A hub deploy replaces the shim but
  installs no `rsi-repo-guard` beside it; until #451 (hub #3621) makes it a managed binary, a
  host that only ever deploys (never runs `make release-install`) has a shim and no tool: every
  `target-pool` call exits 127 with the install instruction, which every caller treats as "no
  pool" (a seed is best effort, `cargo-slot` records "no report"). Land the managed binary
  before the shim reaches such a host.
- Found and fixed while porting: copying a read-only pool output (an executable made
  read-only by `mark_shared_read_only`) failed with `Permission denied` before its times
  were restored; `copy_file` now makes the copy owner-writable first (the `seed` read side
  had the same defect for any pool built without the unlinking wrapper).
- Found and fixed while porting (macOS): rustc keeps a unit's `.o` files there, the Python tool
  left them writable, and a build without the wrapper over a pool seeded behind it rewrote the
  pool's shared objects. `.o` is now an in-place suffix (read-only in the pool), so that build
  stops instead; `shared_executables_fail_closed_without_the_wrapper` pins it.
- Found while porting (#494): the Python and first Rust `seed-from` sized the peer pool with
  `du -sb`, which BSD `du` lacks; it is `du -sk` now.

### Exact-head acceptance: what the hub #3621 lane must run

Nothing here is claimed by #423 and nothing may be waived, excepted or replaced
with a synthetic receipt. In order:

1. **Trusted materialization.** The complete Rust source closure at the exact
   selected commit (workspace manifests and lock, toolchain config, the guard
   crate and all its modules/includes/dependencies) and content-verified
   `rsi-repo-guard` and `rsi-scoped-test` binaries bound to commit, input blobs
   and binary. The runner never builds or trusts candidate-authored executable
   code to judge itself.
2. **Prebuilt release binaries (#447).** The landing guard's 141 s plan-only
   budget (the lander gives it 120 s) cannot cover a debug build: the current
   tree's `target_reclaim` check alone is about 140 s in a dev profile, and the
   guard binary took 141 s end to end. Supply release builds; never build inside
   the budgeted step.
3. **Trusted per-commit guard binaries (#362).** `rsi-repo-guard` for each
   historical commit a `// guard-regressions: stdout` guard names
   (`--historical-binary COMMIT=PATH`), each run in its own process group under a
   120 s deadline; an unmaterialized commit fails closed.
4. **Linux-only runs.** The integration tests in `crates/rsi-repo-guards/tests/
   pid_isolation.rs`, `init_reaps_an_orphaned_grandchild_so_kill_zero_reports_esrch`
   and `namespaced_command_orphans_are_reaped_by_the_init` (the init is tested
   only through the built binary: `waitpid(-1)` in a shared harness would steal
   other tests' children); the systemd-wrapped worker unit (`provenance::
   bounded_command`, `--slice=rsi-build.slice`, CPUQuota, RuntimeMaxSec); the
   non-Linux cross-check lane's Linux counterpart (`linux_check`); and a hard
   kill/overall-timeout run of a wedged gate and a wedged worker.
5. **Full exact-head scoped run.** An admitted `scoped_test` of the exact final
   committed HEAD against baseline `66b1d435a267a0266771b731930191a6b032acc2`,
   through the daemon (`scoped_test` job kind, `agent_landing_candidate::check`),
   with the typed receipt's `receipt_head` equal to that HEAD, the warning gate
   judged (`tools/warning-baseline.json`) and every required static gate `ok`.
   Expect it red until the open items below are closed; do not paper over them.
6. **Caller switch and deletion.** Only after parity is proven and nothing is
   left red, and one caller at a time per "add replacement, switch caller, then
   delete": `scripts/scoped-test` and `scripts/cargo-slot`, then the callers
   listed in #450 (pre-push hook sections and `tools/test_pre_push.py`,
   `scripts/test-suite-benchmark.sh`, `cloud-sweep-json.py`,
   `shard-build-warnings.py`, `check-touched-shards`, the lander).

Open items that keep the binary red today, with their owners:

- #366: the ported bash suites run natively (see "Residual suites"); 40 Python suites remain in scripts/tests, listed there by blocker, and each selected one is a red `incomplete_contract` entry.
- #426: the assembled derivation's parity with the legacy selector is unproven
  (235/244 same-named ports; downstream selections ordered by path; a missing
  shard manifest refuses where legacy fell back to `store-01`).
- #453: `target-pool` is native, but its callers (`cargo-slot`, rsid `execution_scratch`,
  the deploy parse check, `install-release.sh`, the Makefile target) still run the Python
  tool, and the Python suite's real-Cargo cases are unported (see "target-pool").
- #362/#363/#361 residuals named in their own handoffs (historical replay,
  lander/QA production backends, target-authenticated migration dispatch).

## Product-script helpers (#465): `rsi-repo-guard` subcommands

The product shell scripts no longer start an interpreter at runtime. What shell cannot do
portably is a subcommand of `rsi-repo-guard`, found by `scripts/repo-guard-bin.sh`
(`rsid_resolve_repo_guard [<rsi-rpc> [<subcommand>]]`: `RSI_REPO_GUARD_BIN`, beside the rsi-rpc,
`~/.rsi/bin`, then a build through `scripts/repo-guard`; a copy that lacks the subcommand is
passed over; none found fails closed and is never read as a pass). `cargo-slot` has its own
lookup (`~/.rsi/lib` and `~/.rsi/bin`; it never builds, because it runs while a build waits for a
slot) and exits 127 naming the install step.

| Script | Subcommand | Replaces |
| --- | --- | --- |
| `rsid-health.sh` `rsid_rpc_ready` | `rsid-health-probe <rsi-rpc> <socket> [launcher]` (exit 0 ready, 1 not, 2 launcher/daemon gone, 3 responder gone) | the GetHealthStatus socket probe (SCM_CREDENTIALS / LOCAL_PEERPID, process-tree check) |
| `rsid-quiet-restart.sh` `request_quiet_restart` | `rsid-quiet-restart <rsi-rpc> <socket> <dir> [wait]` (exit 0/10/11/1) | the `RequestOperatorRestart` heredoc |
| `release-target-guard.sh` | `release-guard lock-fd \| links-into \| kept \| receipt-status` (the receipt target check is the hub's `release-target` guard) | the flock, the symlink scan, the hashed keep/prune of installed links, and the receipt JSON checks |
| `cargo-slot` | the existing `cargo-slot status \| free-gb \| start \| queue-head \| rpc` (#423) | the governor client, quota probe, process start time and ticket head heredocs |
| `rsid-launchd.sh` | none: awk renders the plist (one pass, XML-escaped values, unknown placeholders refused), `plutil -lint` (or `xmllint`) refuses an unparsable result | the plistlib writer |
| `install-release.sh --stage-dir`, `install-desktop.sh`, `desktop.sh` | none: `sed`/`shasum` on `cargo metadata` (`target_directory`, the `{"name":..,"version":..` package objects) and `tauri.conf.json` | the inline JSON and MANIFEST.sha256 heredocs (#454) |
| `candidate-receipt.sh` | `scripts/repo-guard check-touched-shards` | `python3 scripts/check-touched-shards` |
| `check-socket-activation-macos.sh` | none: plain heredocs for the fixture package | the fixture writer |

Ported bash TAP suites (case maps in each header): `test_install_release_{links,git_hooks,stage}.sh`,
`test_install_desktop.sh`, `test_cargo_slot.sh` (15 governor-path cases run everywhere; the 15 local-fallback and
`/dev/shm` cases are Linux-only skips and were not executed on the macOS host that ported them: hub #3621
Linux checklist). `test_shell_tests.sh` is green (its `runs_python` refusal is proven by a negative case) and runs
`test_rsid_health_wait.sh` and `test_rsid_quiet_restart.sh` against the Python-free scripts.
Not done here, with owners: `cloud-*` (needs Rust ports of `cloud-spend.py`, `cloud-pool-state.py`,
`cloud-sweep-*.py`), `searxng-local.sh` (policy), `refresh-codex-schema-fixtures.sh` (JSON projection),
`test-suite-benchmark.sh` (the benchmark harness's own heredocs), `scripts/check-touched-shards` (still a
Python file; `candidate-receipt.sh` already calls the Rust guard).

The install-release provenance suite is the hub's Rust `tests/install_release_provenance.rs`. The links and stage suites are bash here
because the old `.py` versions fail against the Python-free installer, which needs an `rsi-repo-guard` for its release lock
(`scoped_test` a518dcb9: all 9 links cases died at `release_guard_lock`); the hub's #3771 Rust port supersedes them.


## Residual suites (#366): what runs, what is ported, what remains

**Execution.** `scoped_runner::residual` selects the suites (the retired
`python-test-gate.py` rules, unchanged) and `scoped_runner::residual_run` runs the
ported ones. A ported suite is a bash TAP file `scripts/tests/test_<stem>.sh` (or
`tools/test_<stem>.sh`) that declares `# suite-gate: tap` and sources
`scripts/tests/lib/tap.sh`; a bash file without the declaration is never a suite
(`test_rsid_*.sh` and `check-corpus-pins.test.sh` stay exit-status tests, run through
`test_shell_tests.sh`). `# suite-gate: scans <dir>` is the runner-neutral spelling of the
old `# python-test-gate: scans <dir>` (both are read). Each selected suite is a killable
child (`bash <suite>`, 300 s ceiling, the run's PID isolation and cancel flag, cwd = the
candidate checkout, log `residual-<n>.log` in the run's log dir). The runner owns the
verdict: `passed` only with exit 0, no `not ok` (numbered or not), a
well-formed TAP stream (a `1..N` plan that equals the case lines, cases numbered 1, 2, 3
without a repeat or gap, no `Bail out!`), at least one EXECUTED `ok N` case and complete
output; otherwise `failed` (the reason names the first malformed line or the plan
mismatch; `tap_violations` counts them), `timed_out`, `no_tests_executed` or
`runner_unavailable` (no bash), each with `executed_tests`, `failed_tests`,
`skipped_tests`, `exit_code` and `log`. A case reported `# SKIP` is recorded in
`skipped_tests` and never counted as executed; a suite whose every case is a skip (or
that ran no case) is RED `no_tests_executed` (#467: skipping is not passing, and a green
that proves nothing is the failure this lane exists to prevent), so a platform-bound suite
keeps at least one case that runs on every host and lists its platform-only cases by name
as skips (`skip_linux_only`, which names the hub checklist below). `tap.sh` additionally
refuses such a suite itself (`tap_done`), marks a case failed through a marker file when an
assertion fails inside a command substitution, pipeline or subshell the case outlives
(#468), and offers `must`, `assert_before`, `toml_get`, `assert_file_is`/`read_exact` (byte
exact; `$(cat)` strips trailing newlines, #474) and `assert_stdout_is`. The first red suite
stops the lane, as the Python phase did. A dry run leaves a ported suite `planned` and executes nothing. A suite that
is still a `.py` file remains a red `incomplete_contract` entry (`owner: "#366"`).

**Ported in this pass** (each `.py` deleted in the same change; the cases are the Python
cases one for one, plus a few that pin the new harness): `check_standalone_packages`,
`check_merge_whitespace`, `systemd_test_fallback`, `shell_tests`, `build_aws_artifacts`,
`fmt_changed`, `check_release_seam`, `clean_target`, `desktop_e2e_graph_job`,
`desktop_install_job`, `socket_holders`, `seam_release_guard`, `rsi_stash`,
`bash32_arrays`, `satellite_templates`, `ios_test_timeout`, `ios_weekly_agent`,
`cloud_sweep_origins`, `fixture_git_guard` (the shell scanner for `.sh`; the Python-source half
is native: `fixture_git_py` in `rsi-repo-guards` tokenizes the committed `.py` fixtures as text
and carries the thirteen Python-specific legacy cases, #469), `python_verification_briefs` (replaced by
`test_worker_briefs.sh`; the briefs no longer name a Python runner at all),
`tools/test_commit_msg`, `tools/test_git_rerere_guard`,
`tools/test_pre_commit_research_sidecar` and `tools/test_wake_when_message_docs`; after the #466 review
`run_rsid_test_shards` (its fixture was stale: the subject reads the native `scripts/repo-guard rsid-test-shards`,
not a Python checker; the flock-dependent cases are Linux-only SKIPs on the checklist below).
After #488 `e2e_prebuild_single_definition` is bash TAP too (`test_e2e_prebuild_single_definition.sh`): the Python-function case
is retargeted at the consumers that remain, the Rust runner (`runner.rs`/`planner.rs`), the lander, `e2e.sh` (run against a recording `cargo`) and the Python `scoped-test` until it is deleted.
Three subjects ran Python at runtime and were fixed to be interpreter-free so their
suites can run (`scripts/check-standalone-packages.sh` and `scripts/build-aws-artifacts.sh`
read cargo metadata with `grep`/`sed`). Cases that need Linux/systemd (`systemd_test_fallback`
real unit, `build_aws_artifacts`, the `install-direct.sh` overwrite protection) are reported
as `# SKIP` elsewhere and were NOT run on this macOS host; the first Linux run of the hub
lane is their first execution. `check_release_seam` and `seam_release_guard` compile or
call cargo/rustc; they were run here except the real-workspace `cargo tree` case.


**Host-dependent suite cases: the hub #3621 Linux checklist (#466).**

The ported suites were run natively on macOS as exact `rsi-repo-guards` jobs
(`ported_suites_tests::green_*`: the production `residual_run::execute` on every suite, plus
scratch-copy mutation and neutered-subject tests that require each suite RED when its subject is
broken). These cases did NOT run there; each reports an explicit `# SKIP ... Linux-only, NOT run on
<os>` line naming this list. The hub Linux lane must run them and attach the executed TAP lines;
none may be waived:

1. `scripts/tests/test_systemd_test_fallback.sh` - "a real unit preserves bash variables and
   failure": needs Linux, `systemd-run` and a systemd user manager (`systemctl --user
   show-environment`). It launches a real transient unit; the unit is stopped from an `EXIT`
   trap only AFTER its exit record is observed (#473: stopping the asynchronous unit first
   raced the record). The first real run also verifies that race and that the byte-exact
   log/record comparisons hold.
2. `scripts/tests/test_satellite_templates.sh` - the seven `install-direct.sh` cases ("a fresh
   install writes the repo template", "it refuses a differing template when another instance has
   no env file", "the refusal also finds instances by drop-in directory", "it overwrites when
   other instances have env files", "it overwrites when the only instance is the one being
   installed", "--replace-template overrides the refusal", "an identical template is never
   refused"): Linux, GNU `timeout`, `stat -c`, and Perl with `Socket` (the unix-socket stub that
   replaced the Python one). The non-Linux refusal case is the only installer case that runs
   elsewhere, and is SKIP on Linux.
3. `scripts/tests/test_build_aws_artifacts.sh` - "a global target dir without the env var is the
   directory packaged": Linux x86_64/aarch64 (GNU `tar --sort/--owner`, `sha256sum`, the native
   cargo builder). The three host-independent cases (unresolvable metadata, metadata without a
   `target_directory`, a path needing JSON escaping) run everywhere. Also run the subject's
   `grep`/`sed` `target_directory` reader against REAL `cargo metadata --format-version 1
   --no-deps --locked` output of this workspace: it was only exercised on synthetic metadata.
4. `scripts/tests/test_bash32_arrays.sh` - the three cases per bash class: the `bash < 4.4` class
   needs the macOS `/bin/bash` 3.2 lane (a Linux hub has bash >= 5 and SKIPs it by name); the
   `bash >= 5` class needs a Linux or Homebrew bash. Both classes must each execute somewhere.
5. `scripts/tests/test_rsi_stash.sh` - on Linux the GNU `stat -c %d` branch of `device_of` and the
   `git` wrapper device probe run for real (they fall back to BSD `stat -f` on macOS).
6. `scripts/tests/test_socket_holders.sh` - the Linux and Darwin launcher branches are MOCKED
   `uname` branches (not platform evidence): a real Linux `systemd-run` replacement is the hub's
   own deploy check, not this suite.
7. `scripts/tests/test_run_rsid_test_shards.sh` - the eleven artifact-lock cases (peer survives
   cleanup during list and run, a new build waits for an exclusive clean, an alone run cleans, only
   build/lander `RSI_*` reach the shard, keep/lander artifacts retained, a keep run protects its binary,
   fast mode cleans 17 times, a multi-shard run restores the shared lock, failed tests clean and keep
   their status, a real fast run executes the remote-read leaf) need util-linux `flock`; the six
   dry-run cases run everywhere.
8. Toolchain prerequisites, red (not skipped) when absent: `rustfmt` (`test_fmt_changed.sh`),
   `rustc` (`test_seam_release_guard.sh`), `/bin/bash` (`test_ios_test_timeout.sh`).

**Retired pilot:** #717 removes `scripts/agent-dev-queue.py` and its
`scripts/tests/test_agent_dev_queue.py` suite together. All 52 cases retire with
the feature; saved state is preserved. See the [retirement note](../agent-dev-queue.md)
for the removed CLI and compatibility limits.

**Still Python, by blocker** (every one is a red `incomplete_contract` entry when
selected; none is skipped, waived or run by another runner):

1. Python subject with a published Rust port: the suite and its subject are deleted
   together at the caller cutover (hub #3621), when the Rust tests of the port are the
   coverage. `check_touched_shards`, `slow_tier`, `downstream_public_items`,
   `scoped_test_platform_cfg` (the derivation: `touched_shards::*`, #426), `scoped_test`,
   `scoped_test_linux_check`, `scoped_test_panic_signal`, `scoped_test_failure`,
   `shard_build_warnings`, `python_test_gate`,
   (`scoped_runner::*`, #423), `check_guard_regressions`, `check_rsid_test_shards`,
   `check_store_boundary`, `check_target_reclaim`, `recent_landing_check`,
   `check_released_migrations`, `rolling_migration_renumber` (#361/#362 modules),
   `rolling_landing_guard`, `run_rolling_qa`, `rolling_shard_fingerprint`,
   `import_rolling_qa_cache`, `qa_host_class`, `desktop_test_gate` (#363),
   `pid_isolate`, `scratch_git`, `target_pool` (#453: the real-Cargo incremental and clobber cases are not
   ported, see below).
2. Python subject with no Rust port: `bench_transport`, `cloud_spend`,
   `cloud_watchdog`, `cloud_sweep_artifacts`, `export_client_contract`,
   `pair_direct`, `qa_resume`,
   `rsid_sample_stacks`, `shard_feature_sweep`, `eval_bench_tb2`, `eval_harbor`.
3. Shell subject that still runs `python3` at runtime (port the subject first): `cloud_pool`
   (`cloud-gate.sh`, `cloud-pool.sh` run `cloud-spend.py` and `cloud-pool-state.py`), the
   `cloud-sweep.sh` family (`cloud-sweep-{json,report,verdict}.py`, python3.11 on the sweep host) and
   `searxng_local` (a SearXNG venv installer: a policy decision, not a port). #465 ported the rest
   (see "Product-script helpers").
4. Python-free subject, not yet ported: `rsid_supervisor` (50 timing and fd-handoff cases;
   needs a socket-holder helper),
   `tools/test_pre_push` (the pre-push hook has Python sections, #450).
5. Eval harnesses, hook fixtures and archived evidence outside `scripts/tests`: `eval/harbor`,
   `eval/bench/tb2`, `crates/rsi/tests/fixtures/*.py`, the `scripts/*.py` helpers above and
   about 30 archived `thoughts/` files (delete or record as archived evidence, #358).

The two historical issue-pipeline measurement CLIs were
[retired together with their own suites](issue-pipeline-measurement-retirement.md) (#725).

Final steps of #366 that remain: delete `scripts/python-test-gate.py`, drop the Python phase
from `scripts/scoped-test` / `scoped_tool.rs` / `rsi-rolling-land.rs` with the legacy runner,
the Makefile targets, and add the static guard that refuses a newly added `.py` file.

## Pieces awaiting an independent re-review (integration pass after the rolling merge)

A different-vendor reviewer re-read four of them at 85ffb6ee0 (Issue #466) and filed six
findings, #478-#483; each is fixed here with a test that failed before and passes after (the
red-before run is quoted in the #423 handoff). The reviewer has NOT yet seen the fixes, so every
row below still awaits a second look at the fixed text (component evidence: the native jobs
named in `crates/rsi-repo-guards/src/ported_suites_tests.rs`, `fixture_git_py_bypass_tests.rs` and
the lib suite). `sql_expr` has had no independent read at all. None may be called accepted
until a reviewer other than the author has read it:

| Piece | Where | What the reviewer must check |
| --- | --- | --- |
| `fixture_git_py` tokenizer | `crates/rsi-repo-guards/src/fixture_git_py.rs` | It scans committed `.py` fixtures as text and restores the 13 Python-specific cases of the retired `test_fixture_git.py`. #478 fixed: string escapes are decoded (`\x`, octal, `\u`, `\U`; `\N{...}` and malformed escapes are refused as unsupported source, FAIL CLOSED), every Name target of a chained assignment is recorded, and every f-string replacement field is tokenized and scanned as code (nested f-strings recurse). Check the decoder against Python's rules, that the f-string field splitter cannot lose a field (quotes or braces inside a field are the weak spot), and that no input can pass by skipping text; the tests are in `fixture_git_py_bypass_tests.rs`. |
| `plist.awk` | `scripts/tests/lib/plist.awk` | It parses the iOS plist for the satellite template suites. #479 fixed, fail closed: records are unforgeable (a tab, CR or line break in a key or non-`data` text is refused; a key containing `.` is refused; `<data>` whitespace is stripped), only an exact `<plist version="1.0">` (or bare `<plist>`) and attribute-less elements are accepted, and CDATA, numeric or unknown entities, a bare `&`, a DOCTYPE with an internal subset, an element inside a leaf and duplicate keys are all refused rather than misread (plistlib keeps the last duplicate: the over-refusal is deliberate). Check nesting and that no accepted document can yield a record a different document would. |
| `tap.sh` helpers | `scripts/tests/lib/tap.sh` | The subshell failure marker file (#468), `tap_done` refusing an all-skipped suite, and the ordering, TOML, byte-exact file and directory helpers. #480 fixed: `read_exact` and `run_capture` check the `cat` status before the sentinel/assignment. #481 fixed: `toml_get` requires a closing quote and only whitespace or a comment after it, accepts literal strings and a table header with a trailing comment, and refuses any bare value that is not a plain scalar word (arrays and inline tables are over-refused). Check each helper fails on a missing input and cannot be satisfied by an empty one. |
| `sql_expr` allowlist | `crates/rsi-repo-guards/src/touched_shards/sql_expr.rs` | The #462 allowlist (the proof that a CHECK, DEFAULT or WHEN is isolated). Check that every allowed construct really is accepted by SQLite in the shipped store migrations, that over-refusal is the only failure direction, and that the depth/token/IN-list bounds hold. |
| `commit-msg` hook widening | `tools/git-hooks/commit-msg` | The "Generated with" banner strip. #482 fixed: any leading whitespace, then up to four contiguous non-alphanumeric non-space characters (a C/POSIX locale makes the emoji four bytes) and spaces, as the old `.?.?.?.?` allowed; the product name must end at a word boundary, so `> > > Generated with ClaudeCompat` and `Generated with ClaudeCompat` are documentary text and kept, while nine-space-indented and tab-indented banners are stripped. It edits commit messages: confirm it only removes banner lines. Also #483: the TAP judge matches `# skip` in any letter case (`crates/rsi-repo-guards/src/scoped_runner/residual_run.rs`). |

Merge notes for this pass: rolling's operator Python policy text is the base in
`worker_preamble.md` and `worker-contract.md`; the literal `python3 -m unittest` was removed
from both because `scripts/tests/test_worker_briefs.sh` (correctly) refuses a brief that
contains a Python runner invocation, prohibition or not. Linux evidence stays the hub #3621
checklist above.
