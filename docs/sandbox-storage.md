# Sandbox storage

RSI can inspect and reclaim build-cache data from retained sandbox worktrees.
This maintenance is deliberately target-only: it may stage and remove the
authenticated `target` directory, but it never removes a source worktree,
sandbox root, branch, ref, session row, or custody history.

This is a build-cache pressure control, not a complete sandbox-capacity or RAM
backpressure system. Source worktree count has no pre-allocation admission cap,
so new sandbox roots can still accumulate until the operator runs the separate
ancestor-only source-worktree settlement flow. Completed transcripts are lazy
after restart, but once hydrated they are not yet governed by a byte-bounded
LRU. A strict recurrence barrier still requires both a serialized source-root
and free-space admission gate before allocation and a byte-accounted completed-
transcript cache. Neither age nor disk pressure may become authority to delete
dirty or non-ancestor source work.

Target-cache reclamation is available on Linux only. It depends on Linux's
`openat2` containment guarantees; on other platforms the daemon keeps the
cache intact and reports `openat2_unavailable` rather than using a weaker
pathname-based deletion fallback.

## Execution scratch substrate

For an authenticated sandbox custody root, RSI derives one fixed execution
scratch layout: `<sandbox>/target` and `<sandbox>/target/.rsi-tmp`. The daemon
creates and reopens both directories descriptor-relatively with mode `0700`,
then pins the root, target, and temporary-directory device/inode identities.
Immediately before command construction it revalidates identity, device,
filesystem type, symlink resistance, and private mode; a replacement, symlink,
cross-device path, tmpfs/ramfs, or chmod drift fails the launch/tool boundary.

Claude, Codex (including Pioneer), Codex App Server, Antigravity, and Harness
receive the validated target as `CARGO_TARGET_DIR` and the validated temporary
directory as `TMPDIR`. Authorized scratch paths also carry the daemon ownership
namespace plus their existing session and invocation identities. Harness
applies these values after its shell environment clear.

Ordinary non-sandboxed launches remain byte-compatible with their pre-Slice 8
environment contract: they receive no new scratch target or `TMPDIR`; Claude,
Codex/Pioneer, and Antigravity add no ownership override (ambient values remain
untouched); Codex App Server and Harness retain the ownership stamp they already had. Existing provider-
specific session, invocation, socket, token, and role stamps are unchanged.

This is the Slice 8 descriptor and stamping substrate only. Slice 9 mount
namespace containment and the Cargo guard are not yet claimed; environment
stamping alone does not contain absolute `/tmp` writes or command overrides.

## Warm target pool (#1741)

Every sandbox builds in its own `<sandbox>/target`; no two sandboxes share a
target directory. What they share is compiled work: a new worker sandbox's
empty target is seeded from a *pool*, one clean `cargo test --workspace
--no-run` of one commit, and Cargo's fingerprints then rebuild only the crates
whose sources differ from that commit.

- **Mechanism.** At a worker launch whose target is still empty, the daemon
  runs `~/.rsi/bin/target-pool seed --sandbox <root> --build-missing` (installed
  by `make release-install` from `scripts/target-pool`) with exactly the build
  settings it stamps on the session. The seeder picks the nearest ready pool
  whose commit is the sandbox `HEAD` or an ancestor (at most 200 commits back),
  fills the target in ~1 s, and, when no pool is at `HEAD`, starts one pool
  build for it as a detached `systemd-run` unit through `cargo-slot`. Pools
  live in `~/.rsi/target-pool/<repo>/<sha>-<key>/` (a detached worktree plus
  its `target/`); a builder keeps the newest two per repository and refuses to
  build with less than 40 GiB free (`RSI_TARGET_POOL_MIN_FREE_GIB`). A seed
  that fails, times out (120 s) or finds no pool leaves the target empty and
  the session builds cold, as before. A `no pool` result carries its `reason`
  (no ready pool for the build key, no ready pool is an ancestor of `HEAD`, or
  the nearest ancestor is N commits behind the limit, with `nearest_pool`), and
  `build` reads `skipped: only X GiB free (< floor)` when the free-space floor
  would stop the pool build; the daemon logs the line as `warm target pool seed`.
- **Incremental pool builds.** Before building, a new pool seeds its target
  from the nearest ready ancestor with the same build key (at most 200 commits
  back; `build --max-distance` overrides the limit). It uses the same safe
  links, copies, manifest-path exclusions and source mtime rules as worker
  seeding. With no matching ancestor it builds cold. The pool record's `seed`
  reports the ancestor SHA, distance, file counts and seeding time, or `null`
  for a cold build; `build_seconds` includes seeding.
  Submit the `target-pool-build` durable job recipe to build the sandbox's HEAD
  and measure CPU usage from its systemd unit accounting on Linux.
- **Key.** A pool is used only by a sandbox that builds the same way: the key
  hashes `rustc -vV` (the commit's toolchain), `CARGO_INCREMENTAL`, every
  `CARGO_PROFILE_*` variable and the rustc flag variables.
- **No clobbering, by construction.** Library outputs (`.rlib`, `.rmeta`)
  are hard links; rustc replaces them by rename. Executables, `.dwo`, `.so` and (on macOS, where rustc keeps
  a unit's object files) `.o` are written in place by the linker, so they are hard links only behind
  `rsi-build-rustc` (`RUSTC_WRAPPER`), which unlinks a unit's shared or
  read-only outputs before compiling; the pool marks them read-only, so a build
  without the wrapper stops (`not writeable` / `IO failure on output stream`)
  instead of writing into another sandbox's file. Fingerprints, dep-info and
  build-script output are copied. `crates/rsid/tests/target_pool_cargo.rs` builds
  two sandboxes on different commits from one pool concurrently and checks the
  pool's bytes are unchanged.
- **Relocation.** Cargo does not rebuild a unit when `CARGO_MANIFEST_DIR`
  changes, so a unit that read it at compile time would keep reading the
  pool's checkout. The seeder rebuilds every unit whose rustc dep-info records
  that variable, and workspace code uses `rsi_common::manifest_dir!()` (resolved
  at run time) or a file-relative `include_str!` instead
  (`crates/rsi-common/tests/manifest_dir_guard.rs`). Build scripts emit
  package-relative `rerun-if-changed` paths. Tracked files identical to the
  pool commit get an old mtime and every other file the current time, so Cargo
  rebuilds exactly the crates that changed.
- **Same settings everywhere.** The daemon records the settings it stamps in
  `<sandbox>/.rsi-build-env.json`, outside Cargo's disposable `target` tree so
  a full `cargo clean` preserves them; `AgentSubmitJob` units read it back
  (`--setenv`), `scripts/scoped-test` passes them to its unit, and `cargo-slot`
  runs its command as a child. A build in any of them reuses the shell's
  artifacts instead of writing a second set. Sessions launched before this
  layout still use their target-local record until their next launch stamps
  the persistent record.
- **If a seeded build fails** with `not writeable` or `IO failure on output
  stream`, something built without the stamped `RUSTC_WRAPPER`: restore it, or
  `rm -rf target` and build cold. Inspect pools with `target-pool status`,
  trim them with `target-pool gc --keep N`; deleting a pool never breaks a
  seeded sandbox (its links keep the bytes).
- **Stale pools and refresh (#2707).** A seed refuses a pool more than 200
  commits behind its HEAD. `target-pool gc` therefore also removes any ready
  pool more than `--max-distance` (200) commits behind `--ref` (`origin/rolling`;
  `--ref ""` turns it off), whatever the keep count; a pool a seed is reading is
  still skipped. A stale pool otherwise held its disk below the build floor, so
  no fresh pool could ever be built. `target-pool refresh` then starts a detached
  build (in `rsi-build.slice`, only above the free-space floor) at the rolling tip
  when no pool is within half the seed distance of it, using the checkout recorded
  in the pool directory's `repo-path`. The daemon runs both after every sandbox
  reclaim pass (startup, so after a deploy, and periodic; refresh is skipped under
  disk pressure), on the hub and on a satellite alike.
- **Pool failures.** `target-pool status --repo <sandbox>` reports a failed
  build's exit code, persistent `log_path` and bounded `error_tail`. Later
  launches include that record as `build_failure` in the seed summary, even
  while the retry cooldown prevents another build. Logs live beside the pool
  records and are removed with their pool. The detached unit discards launch
  temporary-directory settings; the builder uses short private disk scratch
  so sccache's separate startup socket fits the Unix socket path limit.
- **Global target dir.** `~/.cargo/config.toml` `build.target-dir` (one target
  for every checkout) is unsafe for concurrent checkouts on different commits
  and is unnecessary now; sessions override it with `CARGO_TARGET_DIR`. Remove
  that line on the host (the operator applies it; it is outside the repo).
- **Release builds (#938).** `scripts/install-release.sh` still guards a shared
  target (`scripts/release-target-guard.sh`): it records the worktree that last
  built `release/` (`release/.rsi-release-source`), removes every workspace
  crate's release artifacts before a build from another worktree, checks that
  the built rsid embeds this checkout's commit (`rsid --build-info`), and
  `--link-only` refuses binaries this worktree did not build from its current
  sources: HEAD plus a git tree fingerprint of uncommitted edits and new files
  (#2179; `--allow-foreign-build` overrides, and `--build-receipt FILE` takes a
  `build` job's result JSON instead, #157: it links only a clean-tree job whose
  `source_head` is this HEAD and whose rsid embeds it; the receipt's `target_dir` must resolve to the release target or the caller's cargo target, or `--target-dir` names it, #3513). Before any cleanup it repoints
  installed links that resolve into `release/` (`~/.local/bin`, each worktree's
  `target/release`) at kept copies in `~/.rsi/install-kept` (pruned once no
  installed or recorded link resolves to them, #2188), and `--tui-only`
  links a stable copy in `~/.rsi/install` (#2180). `scripts/install-desktop.sh`
  sources the same guard with its own record (`release/.rsi-desktop-source`) and
  cleans only the `rsi-desktop` crate (#2177); `scripts/build-aws-artifacts.sh`
  packages the target directory cargo reports, not `./target`; and
  `scripts/scoped-test` skips a tmpfs scratch parent that leaves this user under
  12 GiB, quota included (`RSI_SCOPED_TEST_TMPFS_MIN_GB`). Landers build each side in its own
  `<worktree>-cargo-target`; `cargo-slot` never changes `CARGO_TARGET_DIR` and
  puts a lander's `TMPDIR` on `/dev/shm` only when this user's block quota
  there (systemd: 80% of the mount) still leaves 12 GiB.

### Small hosts and peer pools (#1884)

**Free-space floors scale with the disk.** `sandbox_min_free_gib` (Settings,
"Minimum free space (GiB)") is the one operator setting; it is an absolute
number, tuned on the 775 GiB hub, so the daemon caps the floor it enforces at
25% of the filesystem holding the sandbox base
(`sandbox::effective_min_free_bytes`). A 60 GiB setting is 60 GiB on the hub and
48 GiB on a 192 GiB laptop; 30 stays 30; 0 stays 0. A refusal reports the
effective `min_free_bytes`, the `configured_min_free_bytes` and `total_bytes`,
and `GetHealthStatus.launch_floor` reports the effective floor and its margin.
`target-pool` applies the same cap to `RSI_TARGET_POOL_MIN_FREE_GIB` (40), so a
small host builds and seeds pools with the headroom it can afford. Any
automatic-settlement threshold (#1879) must read the effective floor, not the
raw setting.

**Seed a peer's pool instead of building cold.**
`target-pool seed-from <host> --repo <checkout>` picks the newest *ready* pool on
`<host>` with the same build key (same `rustc -vV` and build settings) whose
commit this checkout has, streams its `target/` as `tar | zstd` over ssh
(hard links, modes and mtimes survive; no rsync needed, the laptop has none),
and registers it as a local ready pool with `seeded_from` in its record. The
detached worktree is not copied, so the pool serves as a seed source only. It
refuses when free space is below the floor or the transfer would leave under
5 GiB (`--min-headroom-gib`). `--dry-run` prints the plan (pool, commit, size).
`RSI_TARGET_POOL_SSH` overrides the ssh command (tests use a shim).
It follows the pool protocol of #1882: it holds the build lock and the pool's
exclusive `.use` lock while it fills the directory (a concurrent `gc` skips the
pool), and publishes the marker last, atomically, with `size_bytes`, so no
reader or `gc` sees a half-copied pool; `gc` then counts a seeded pool like any
other (by `built_at`, kept by `--keep`).

Measured 2026-10-12 (hub pool `aeff3e99`, laptop 192 GiB disk, 32 threads,
tailnet): the pool is 11.1 GB on disk in 6.7 k files (2.5 k hard-linked), 2.3 GB
as a zstd-3 stream (8 s to compress locally). Streaming it to the laptop and
unpacking the listing took **36 s (about 64 MB/s on the wire)**. A cold pool
build of the same commit took 185 s on the hub, with every core busy competing
with QA. Seeding is about 5x faster, uses almost no CPU on the receiver, and
leaves the same 11 GB footprint, so seed when a peer's pool is within
`--max-distance` commits of your HEAD and build locally only when no peer has a
matching key. Re-seed after the hub builds a newer pool; the second pool
replaces the oldest (`--keep 2`).

**Runbook (peer seat enabling the warm pool).**

1. Pull the landed commit and `make release-install` (installs the new
   `target-pool` and restarts rsid). Confirm `~/.rsi/bin/target-pool seed-from
   --help` exists.
2. Record the starting point: `df -h ~`.
3. Settle merged sandboxes before the seed: run the sandbox storage reclaim
   (`RunSandboxBuildCacheReclaim` preview, then actual) and the worktree
   settlement the daemon offers (#1879 when landed). The seed needs the floor
   plus the 11 GB pool.
4. Set the floor for the host in Settings > "Minimum free space (GiB)" (60 is
   fine on the laptop: it is capped to 48). Leave the cache pressure watermarks
   at their percentages; they already scale with the disk.
5. Seed: `scripts/target-pool seed-from <hub-host> --repo ~/rsi --dry-run`, then
   without `--dry-run`. If it exits 2 (no matching key), compare
   `target-pool key --repo ~/rsi` on both hosts (a differing `rustc -vV` or
   `CARGO_*` setting is the cause) and fall back to a local build:
   `target-pool build --repo ~/rsi`.
6. Check the next worker or QA pin seeds: `target-pool status`, then a sandbox
   `seed` reports `result: seeded`.
7. Record `df -h ~` after, and the transfer's `seconds`, in the Issue.

## Controls

Open Settings and use the Daemon Features section. The six persisted controls
are:

| Setting | Meaning | Valid values |
|---|---|---|
| Sandbox cache reclaim | Enables selection of new target caches | enabled / disabled |
| Cache reclaim TTL | Minimum idle age outside pressure mode | `1..=2592000` seconds |
| Cache reclaim interval | Periodic pass interval | `60..=86400` seconds |
| Cache pressure high | Used-space level that enters pressure mode | `2..=99` percent |
| Cache pressure low | Used-space level that exits pressure mode | `1..=98` percent |
| Cache reclaim pass limit | Maximum candidates inspected per pass | `1..=1024` |

The low watermark must be below the high watermark. The daemon validates the
complete pair, writes both watermark rows in one SQLite transaction (including
the unchanged counterpart on a first one-sided update), and only then publishes
the pair. A successful update therefore survives restart and a failed write
cannot leak into live configuration. Pressure starts at the high watermark and
continues until usage reaches the low watermark. Under pressure, terminal
caches may bypass the TTL; malformed timestamps still fail closed.

Daemon-reported numeric values do not have to be presets. The TUI inserts an
exact reported value into its sorted cycle and keeps it selected until the
operator deliberately changes it.

These operations are operator-only. `GetDaemonConfig`, `UpdateDaemonConfig`,
`GetSandboxStorageStatus`, and `RunSandboxBuildCacheReclaim` are available to
the TUI/operator RPC surface and are denied to session-attributed agents.

## Preview, actual passes, and scheduling

The TUI claims ownership of one fresh automatic startup preview asynchronously
immediately after a successful transport/configuration handshake. Here,
“immediately” means automatic post-handshake ownership: the preview is shown as
refreshing, but its RPC admission is deterministically held until the
identity-bearing session snapshot request settles. It does not wait for project
or label metadata, conversation hydration, optional configuration writes,
usage, memory, or graph lookups, and the event loop never synchronously awaits
storage. Rendering and input begin before any startup RPC completes, while a
held Git inspection cannot prevent session identity from settling. A manual
preview requested before the handshake cannot suppress that fresh automatic
request; generation fencing and coalescing still bound the work. Opening or
refreshing Daemon Features coalesces with an automatic preview already in
flight instead of starting an unbounded set of requests. If the first
automatic preview overlaps a short-lived daemon Store owner and reports a
`StoreBusy` refusal, the same bootstrap attempt owns exactly one automatic
follow-up preview. A second refusal settles normally; this bounded retry does
not move the session-identity admission barrier or wait for unrelated metadata.

Stats and Daemon Features refreshes are optional post-readiness work. They do
not restart the handshake, replace the primary connection, or withdraw launch
readiness; their completions are separately generation-fenced from startup.

The Sandbox storage row has five process-local states:

- `Unknown` means this TUI process has not requested a preview.
- `Refreshing · no preview yet` means its first fresh preview is in flight.
- `Fresh <time>` identifies the observation time of a successful fresh RPC.
- `Refreshing · stale since <time>` retains the last successful counts while
  explicitly marking them stale during a newer request.
- `Error: <message>` reports failure and, when known, only the time of the last
  success; it does not present old counts as current.

Under disk pressure the appointed manager can use the same status and reclaim
instead of removing caches by hand, when the operator grants it `StorageControl`
in the manager policy (`:manager policy`, "Grant Storage control"; off by
default, but the Full project control preset includes it). The manager calls `GetSandboxStorageStatus` or
`RunSandboxBuildCacheReclaim` (`{"dry_run": true|false}`) through
`AgentManagerControl` `operator_call`; both need Execute mode and an unpaused
policy, run under the configured watermarks and limits, and are journaled like
other delegated manager actions. Daemon storage settings remain operator-only.
See `docs/harness-manager.md`, Operator delegation.

Generations fence completions, so an older automatic preview cannot overwrite a
newer operator action or its post-action result. These states and observation
times are not persisted and introduce no daemon cache or wire contract. Every
`GetSandboxStorageStatus` remains a fresh dry-run preview.

### After a worker finishes: its own `target/` (#158)

A worker or reviewer has no cleanup control for its own `target/`, and none is
needed once you finish: do not delete it by hand (`rm -rf target`, a rename or
a path-checked cleanup repeats the daemon's custody, Git, job and consumer checks without its
descriptor pinning). Verify, commit, publish the handoff and end the turn. Once
the session is terminal the daemon's startup/periodic pass (and the pressure
lane for a session that finished after the sweep cursor passed, #2682) reclaims
only the authenticated `target/`; the source worktree, branch and custody row
stay. To ask for one bounded pass sooner, the operator calls
`RunSandboxBuildCacheReclaim` (`{"dry_run": true}` first, then
`{"dry_run": false}`) directly, and a manager with `StorageControl` calls it
through `AgentManagerControl` `operator_call`; `GetSandboxStorageStatus` is
reached the same two ways. The direct storage RPCs reject session-attributed
callers and are not native agent verbs; only the manager wrapper is open to a
session. A pass takes the next bounded page of candidates, not a named session.

Eligibility is not immediate, and a kept `target/` is not a lifecycle failure:

- **Idle TTL.** Outside pressure a candidate must be idle for the reclaim TTL
  (default 6 hours). Inside the TTL it is still reclaimed when the worktree's
  `HEAD` is already an ancestor of `refs/remotes/origin/rolling`, or the
  session's newest merge-queue entry is `published` (#1684), or the session's
  latest turn ended with a final `PIPELINE HANDOFF` and its `HEAD` is
  contained in any `refs/remotes/origin/*` branch it pushed (a satellite
  worker lands through the hub queue, hub #3026). The probes read the local
  remote-tracking refs and never fetch, so a stale ref only delays the
  reclaim. Otherwise the attempt reports `idle_below_ttl`. Without pressure
  the fair walk visits a finished row once per cycle, so a worker that lands
  after its visit waits for the next cycle. Under disk pressure the TTL is bypassed,
  but an unparseable timestamp and every check below still refuse.
- **Live consumers** keep the target: `running_job` (a `queued` or `running`
  daemon job owned by the session or running inside its sandbox),
  `enabled_wake`, `restart_intent`, `manager_seat`, `manager_current_session`,
  `process_uses_sandbox:<pid>` and a held cargo lock. They clear on their own
  and the next pass retries; a session that parks on a job or wake therefore
  keeps its target.
- **Source safety.** The target is removed only after the sandbox root,
  Git top level, branch, source-commit ancestry, common dir and worktree
  registration still authenticate (`source_commit_not_ancestor` and the other
  Git checks below). Unknown top-level entries keep the whole tree
  (`target_unrecognized_content`, see below).

Neither the periodic pass nor the preview promises a reclaim time or freed
bytes: dry-run reports `eligible_candidates` without a capacity estimate, and
only `reclaimed_bytes` from an actual pass is a measurement.

### Refusals are named (#1575)

Every refused candidate is counted under a skip reason in `skip_counts`, and the
V2 report carries a bounded sample (`refusals`: at most 24 per pass, 4 per
reason) that names the session, its custody and the exact check that failed, so
a manager can read why from `GetSandboxStorageStatus` without daemon logs. The
pass-completed log line carries the same sample (`refusal_samples`). Checks
include `git_toplevel_mismatch`, `git_branch_mismatch`, `source_commit_not_ancestor`,
`git_common_dir_mismatch`, `worktree_not_registered`, `sandbox_root_path_mismatch`,
`target_entry_unrecognized:<entry>`, `live_consumer:<kind>` (`enabled_wake`,
`restart_intent`, `manager_seat`, `running_job`, `manager_current_session`),
`process_uses_sandbox:<pid>` and `process_scan_unreadable:<pid>`. A reason with
no finer check reports its own name (`target_absent`).

`process_scan_unreadable:<pid>` fails closed only for an RSI-unit process whose
`/proc/<pid>` entry stays unreadable. The scan covers every process of the user,
and one caught mid-`exec` (or being torn down) can read as unreadable for a
moment, so each unreadable entry is re-read three times, 10 ms apart, before the
candidate is refused (#2163). A process that exits meanwhile is simply gone.

A candidate withheld by a live consumer is counted too (`live_consumer`): it
spent page budget like any other row.

### What a reclaimable `target/` may hold

The whole-target staging protocol moves the tree, so it runs only on a tree of
Cargo output and RSI scratch: `debug`, `release`, `doc`, `package`, `tmp`,
`cargo-timings`, `criterion`, `dhat`, `.rustc_info.json`, `CACHEDIR.TAG`,
`.cargo-lock`, `.future-incompat-report.json`; the daemon's execution scratch
(`.rsi-tmp`, the sandbox `TMPDIR`, present in every sandbox target), its
legacy build-settings record `.rsi-build-env.json` and the warm pool's seed marker
`.rsi-target-pool.json` (#1741); and the
repository's test-runner output (`rsid-test-shards`, `.rsid-test-shards.lock`
and any `*-fixtures` directory). Entry types are exact: a symlink or special
file never matches. Any other top-level entry may be a manager's or operator's
state, so the whole tree is retained and reported as
`target_unrecognized_content` with the entry's name (#1429). Keep such state
outside `target/`.

`Preview cache reclaim` performs the same custody, Git, generation, active
owner, path, device, and target authentication as an actual pass without
staging new data. The pass keeps one device/inode deletion ledger, re-walks
descendants at the capacity-estimate boundary, and discards every inode whose
allocation, type, observed-link count, or filesystem link count drifted. The
ledger still deduplicates stable hard links across the complete pass.

That point-in-time observation is not a promise of future free space: an
external process can create another retaining link after the last observation.
The wire report therefore conservatively omits definite preview capacity:
`would_reclaim_count` and `would_reclaim_bytes` are zero, and dry-run never
simulates reaching the low watermark. The TUI status, preview row, and
notification instead show the truthful `candidates_considered` and
`eligible_candidates` authentication counts plus the literal message
`capacity estimate unavailable`; they never label the withheld zeros as
found, allocated, or reclaimable. An empty pass therefore reads as zero checked
and zero eligible, while a nonempty authenticated pass remains distinguishable.
Sparse logical length and duplicate links remain excluded, nested symlinks are
not followed, and actual `reclaimed_bytes` remains the only exact capacity
result because it comes from post-delete `statvfs`.

`Reclaim sandbox caches now` runs one actual bounded pass. Its notification
distinguishes fully removed, newly staged, and still-pending targets. “Measured
free” is the increase in filesystem available bytes observed after the pass;
it can differ from the preview estimate because filesystem accounting may be
delayed or shared. After an actual action, the panel performs a fresh preview on
the action's dedicated connection for current state. A failed refresh replaces
the old panel value with an explicit failure instead of presenting stale
success.

The daemon submits exactly one startup pass after authority-bearing restore,
ProgramRun reconciliation, AppServer writer-admission reconciliation, durable
agent-spawn reconciliation, and master-successor reconciliation have completed.
It submits that pass immediately before request readiness, then creates the
periodic sleeper and starts accepting RPCs without waiting for maintenance to
finish. Socket binding is logged separately as `socket_bound`; it means clients
can find the socket but not yet that requests are accepted. `request_ready` is
the final acceptance milestone. A startup pass may therefore complete after a
health request without weakening the authority boundary.

Startup, periodic, preview, and operator passes share one process-wide named OS
worker with a 16-slot synchronous queue. Startup owns an asynchronous receiver
monitor until its accepted result settles; periodic, preview, and operator
callers continue to await their own responses. The worker owns its long-lived
runtime, all recovery/Store/filesystem work, and the trigger-specific start plus
exactly one completion/error log. Cancelling any requester only closes its
response receiver; request-runtime shutdown cannot cancel the queued or active
job. A supervised outer-job panic becomes a stable error and terminal log,
after which the same worker continues without overlap. A second pass cannot read
recovery, pressure, Store, or filesystem inputs until the first terminal log
settles. At most one lifecycle is active and 16 requests are queued; overflow or
worker unavailability fails explicitly. Disabling reclamation prevents
selection of new targets but does not strand an already-authorized staged
target: recovery still runs first.

Under sustained disk pressure, the startup/periodic producer can continue the
terminal-history sweep without sleeping the full operator interval between
pages. It awaits each result on the same serialized worker before submitting
another job; it adds no queue, worker, RPC, setting, or deletion authority.
An advancing reserved cursor through an empty historical page counts as useful
scan progress. Actual removals, new staging, or recovery deletions also permit
continuation. Eligibility alone, a refused candidate page without useful work,
Store contention, errors, an unchanged cursor, and a completed sweep wrap return
to the persisted operator interval. Dry-run and operator requests remain single
passes and never start automatic continuations.

Continuations yield 25 ms between passes. A burst pauses for one second after
16 passes, 256 inspected terminal rows, two seconds elapsed, or 1 GiB of reported
staged/recovered byte observations, whichever threshold is reached first.
These are admission budgets checked at pass boundaries: the last admitted pass
can cross a threshold and retains its existing independent row, filesystem,
byte, and duration ceilings. They do not interrupt a custody/journal operation
or promise a precise amount of reclaimed capacity. This bounded pause repeats
while progress continues; it does not reset the durable sweep cursor.

Every continuation reads fresh policy and filesystem pressure on the worker
after its queue wait. Disable or the low watermark stops accelerated work before
recovery or candidate selection. Ordinary interval-based recovery of previously
staged targets remains available while disabled. Shutdown aborts the producer
before awaiting other services, preventing future submissions while already
accepted worker jobs finish under their original fences. Outside pressure the
saved operator interval governs scheduling; no persisted policy is rewritten.

Reclaim lifecycle logs include bounded monotonic `queue_wait_ms`,
`run_duration_ms`, and `total_elapsed_ms` fields while retaining trigger,
dry-run, cancellation, count, stop, and error details. Startup restore also
reports bounded phase and total durations for settlement recovery, custody,
controller/invocation, session/metrics, lead/retry, and orphan reconciliation.
These measurements add no durable status or deletion authority.

## Refusals and recovery

Candidates are processed oldest first and remain bounded by the operator pass
limit. Independent fixed safety ceilings also bound recovery entries,
filesystem entries, allocated bytes examined, monotonic pass duration, and
tree depth across recovery, sizing, staging, and deletion. These ceilings are
internal fail-closed limits, not daemon settings. Dry-run sizing remains
non-mutating and returns a typed stop when a scan ceiling ends. Actual recovery
does not require a complete sizing walk: it unlinks descriptor-relative entries
as they are enumerated, so filesystem-entry-limited passes leave a smaller
authenticated stage. Leaf unlink is metadata work: it still consumes entry and
time safety work, while its allocated blocks are only a bounded, saturating,
uncertain report observation. A leaf larger than the byte ceiling is therefore
unlinked rather than becoming a repeatable zero-work `ByteBudget` stop. A newly
staged target reuses its bounded preflight entry/byte charges as deletion credits
instead of paying the same ceiling twice.

New staging is registered first in the V119 SQLite intent journal. The immutable
identity includes custody and generation, allocation UUID, deterministic bucket,
slot and payload names, and the authenticated target device/inode. The journal
advances `Prepared` → `Staged` → `Deleting` by positive row-version CAS. A
terminal `Completed` or `Abandoned` event retains that complete identity, and a
later candidate visit cannot recreate the same custody/generation intent.

Registered recovery has its own monotonic `schedule_id` keyset sweep. Each cycle
freezes a finite upper watermark and durably reserves the next bounded page
before filesystem probes. The Store lock is released before descriptor-relative
work. Pending rows therefore remain schedulable after owner, generation,
validation, cleanup, or custody-state changes; those safety transitions are
never weakened or refused to preserve reclaim eligibility. Registered intents
run before legacy recovery and before policy, pressure, or TTL gates.

The active registered namespace is
`.rsi-target-reclaim-v1/bucket_<hash>/v3_<custody-uuid>_<generation>/payload`.
Queue, bucket, and slot publication are parent-fsynced before the only
`RENAME_NOREPLACE`; the source root plus slot, bucket, and queue are fsynced
after it. Recovery authenticates source and destination independently. An exact
destination remains detached deletion authority after custody drift, including
when a different new source target exists. A replaced payload, ambiguous
source/destination pair, cross-device object, or identity mismatch is retained.
An exact source is renamed only while the current custody/session generation,
validation, effects, terminal status, and inactive-owner proof still match;
otherwise the source is preserved and the intent is durably abandoned.

The older v1/v2 raw queue remains compatibility-only. Its 256 buckets and flat
migration scan have an independent fixed raw-entry quantum, and every directory
entry observed charges that ceiling. Registered `v3_` slots are never mutated by
that scanner. Malformed or depth-terminal legacy entries move to the rejected
namespace when safely possible. Legacy residual cardinality remains unknown
unless the whole raw namespace was observed. Dry runs do not advance either
durable scheduler. Registered recovery, legacy recovery, and new-candidate
selection have independent work quanta.

V2 reclaim reports expose independent registered-intent and legacy recovery
cursor/wrap evidence, whether each pass reserved progress, registered FSM and
terminal counts, legacy entries migrated, deleted-entry delta, residual work
when it is known, and non-progress count. Historical V1 and V2 JSON remains
decodable. Directory allocation is recorded as bounded observation rather than
a pre-unlink byte refusal, so an oversized directory cannot permanently prevent
the first delete.

A stage that cannot be traversed without exceeding the fixed depth ceiling is
moved to one strict direct child of the already-pinned sandbox base. For active
name `v1_<custody-uuid>_<generation>_<dev>_<ino>`, the exact terminal basename
is `.rsi-target-reclaim-rejected-v2_v1_<custody-uuid>_<generation>_<dev>_<ino>`.
The name is never truncated, randomized, or suffix-renamed. The daemon pins and
authenticates the active stage, requires the base and active queue to remain on
the expected device with the effective user's ownership and exact mode `0700`,
fsyncs the queue and base, and performs a queue-to-base `RENAME_NOREPLACE`.
After checking the moved inode it fsyncs the queue and base again. Terminal
certification then reauthenticates the pinned base and linearizes at one direct-
child `openat2` of the terminal basename; `fstat` must identify the expected
device and inode on the descriptor returned by that exact resolution.

A collision, unsafe base, cross-device attempt, rename/proof/barrier failure,
or hostile replacement remains non-terminal. A failure before the rename leaves
the expected stage in the active queue for a later retry. Once the rename has
removed a stage from the active queue, any later authentication, durability, or
final-proof failure retains the uncertain direct-terminal names for operator
diagnosis and never moves a name back into the queue. Linux provides no atomic
compare-inode-and-rename operation, so reopening a terminal name, discarding its
inode proof, and later renaming that name would let a same-UID replacement cross
the authority boundary. The daemon therefore performs no post-move rollback,
never overwrites a collision, and never chooses a fallback name. Every
unverifiable, malformed, or colliding reserved entry remains retained.

Active recovery enumerates only `.rsi-target-reclaim-v1`; it never traverses
the legacy `.rsi-target-reclaim-rejected-v1` directory or direct
`.rsi-target-reclaim-rejected-v2_*` entries. Already-retained legacy-v1 and
direct-v2 rejected objects are never automatically traversed, migrated,
renamed, or deleted. Operator discovery is a depth-one, read-only listing of
those two name forms at the configured sandbox base (`~/.rsi/sandboxes` by
default, or the `RSI_SANDBOX_BASE` equivalent). Discovery grants no deletion
authority;
diagnosis and removal are explicit operator actions outside reclaim. Capacity
preview remains unavailable, and public `would_reclaim_count` and
`would_reclaim_bytes` remain literal zero.

Reports count active partial stages as pending; their byte fields are bounded
observed/preflight footprints, while capacity truth remains the post-pass
`reclaimed_bytes`. Reports also distinguish fresh or invalid
timestamps, an active owner, active-map or Store contention,
custody/generation drift, Git/root refusal, missing/non-directory/symlink
targets, mount or device crossing, target identity change, stage-name
collision, invalid or rejected recovery entries, unavailable Linux `openat2`,
unreadable entries, and incomplete staged deletion.

An actual pass atomically renames an authenticated target into its registered
private mode-`0700` slot beneath the sandbox base. Directory descriptors are
pinned; traversal is beneath-base, no-follow, and no-cross-device. There is no
pathname-recursive deletion fallback. If the kernel lacks the required
`openat2` support, the pass refuses the target rather than weakening
containment.

Once renamed, the intent remains pending even if a later unlink or fsync fails.
Startup and every actual pass reserve registered intent pages before selecting
new candidates, verify the journaled identity again, persist `Deleting` before
bounded deletion, and retain the terminal event only after durable namespace
absence. Invalid, symlinked, identity-drifted, mounted, or foreign-device queue
entries remain untouched for diagnosis.

## Agent scratch outside sandboxes (#999, #932, #1140)

Agents, reviewers, tests and landers leave temp trees under `/var/tmp`, worker
`TMPDIR`s under `~/.cache/rsi-*-tmp`, and lander workspaces
(`rsi-rolling-land-*`) under the queue's cargo target or a repository's
`worktrees/<sandbox>` admin directory. The daemon reclaims them hourly (first
pass ten minutes after start) and each lander sweeps its own parent at startup,
with a fixed policy and no setting. **Deletion is ON** (#1150 re-enabled it:
the daemon pass is `run_and_log(false, "periodic")` in `crates/rsid/src/main.rs`
and the lander startup sweep is `LANDER_STARTUP_SWEEP_ENABLED = true` in
`crates/rsid/src/bin/rsi-rolling-land.rs`); allocation and recording stay on so
new scratch is registered. The rule is **when unsure, retain**:
a top-level directory is deleted only when all of these hold.

- **RSI allocated it.** Creating a scratch directory registers it in the
  private registry `~/.rsi/scratch-registry` (mode 0700): an entry binding its
  kind, owner, device, inode, filesystem birth time (to the nanosecond) and parent
  directory. The
  directory's `.rsi-scratch-record` names that entry by nonce. Allocation
  (`create_scratch_dir`, `register_lander_owner`, `scripts/rsi-scratch-mkdir`)
  only accepts a fresh, empty directory made just now; no agent or script can
  adopt an existing directory. Worker `TMPDIR`s are made with
  `scripts/rsi-scratch-mkdir worker NAME` for that reason. The one exception is
  the operator action below (#1147). A copied tree, a tree moved to another root, a record
  replayed after inode reuse, a synthesized record, a legacy or hand-made
  directory, or a filesystem with no birth time never binds and is retained
  (`kept_unrecorded`). Trust boundary: the registry is writable only by the
  daemon user, so this defends against accidents and everything but a process
  running as that user that sets out to forge it. Registry growth (#1171): a reclaim
  that finishes an allocation makes its deletions durable (it syncs the scratch
  directory, removes the directory, checks that the allocation's own inode has
  no links left, syncs its parent), then writes a `<nonce>.reclaimed`
  tombstone, removes the entry and manifest, syncs the registry, and last
  removes the tombstone; each pass finishes any removal that was interrupted.
  Only tombstoned allocations are pruned, and only when the entry still present
  (if any) is readable and agrees with the tombstone on device, inode and birth
  time. No filesystem is inspected to decide that, and a prune never touches a
  scratch tree. A nonce with a tombstone is not registered again (a tombstone
  that appears between that check and the create withdraws the new entry;
  what remains is a UUID collision plus a sub-second window, a documented
  residual). Accepted leaks, each a few hundred bytes of registry and never
  data: a directory removed by hand or by another tool keeps its entry; a crash
  or sync failure after the files are deleted but before the tombstone keeps the
  entry and manifest; a directory kept because late contents arrived, or
  because its name was swapped or the allocation moved before the removal (the
  link-count check: a filesystem that does not report 0 for a removed
  directory also retains, tmpfs and ext4 do), is never tombstoned, so its entry
  stays, and because its record was already deleted it is left as an
  unrecorded directory that normal passes do not reclaim again; a registry too
  large for one pass's scan budget loses the partial listing and starts over
  each pass, so an unchanged oversized registry can leak its tombstones
  indefinitely. Residual that needs a same-user process: forging or racing
  registry files is inside the registry's trust boundary.
- **It is where we think it is.** The root is opened one component at a time
  with `O_NOFOLLOW`; every ancestor is owned by root or the daemon user and
  closed to group and other writes (or sticky); a symlink or an open ancestor
  refuses the root (`refused_roots`). The candidate is on the root's device and
  `statx` mount, with no mount at or under it per `/proc/self/mountinfo`,
  including same-device binds; a kernel that cannot report mount ids, or a mount
  table this reader does not wholly understand, retains. Identity is checked
  after enumeration and after the rename-aside, and the removal works
  descriptor-relative.
- **It is old, or its owner is gone.** Nothing under it was written for 72 hours.
  A lander workspace is reclaimed when its registered owner is gone, read in the
  PID namespace and boot it was recorded in; an owner from another namespace, or
  an unreadable or malformed owner file, is ambiguous and kept, and a recorded
  workspace with no owner file waits an hour.
- **Nothing holds it.** A complete `/proc` proof of every same-user process and
  thread (a thread is skipped only when `kcmp` proves it shares cwd/root, exe and
  mappings, or descriptors with its leader) finds nothing inside it by path or
  inode: cwd, root, exe, open descriptors and file mappings. An unexplained read
  error keeps the directory; only the systemd user manager (authenticated by
  process name and cgroup) is exempt.
- **No work is lost.** A complete walk (build output, `node_modules` and a
  repository's own `.git` included; a depth, entry or budget limit means
  "unproven") finds every git repository below it. Each has a clean tree and no
  commit, tag, stash or detached HEAD that no remote-tracking ref has
  (`kept_dirty`, `kept_unpublished`). The one exemption is a lander's exact
  private clone: the empty `repo/` directory the lander registered by inode
  before cloning into it, still borrowing objects (`alternates`). Any other
  repository, even inside a lander workspace, is checked in full.
- **It did not change.** After the rename the tree is walked again and its
  per-entry manifest (relative path, inode, type, size, mtime) must equal the
  proof's, and the holder and git proofs run again on the renamed tree
  (`kept_changed`).

**Adopting legacy scratch (#1147).** Directories made by hand or by pre-#1140
binaries (worker `TMPDIR`s, `/var/tmp/rsi-*`, existing lander workspaces) have no
record and are retained (`kept_unrecorded`). The operator-only RPCs
`ListLegacyScratch` and `AdoptLegacyScratch` (Settings > Sandbox Storage >
Legacy scratch adoption; not an agent verb, native tool or CLI-catalog entry)
list them and record chosen ones. An adoption runs the same proof as a reclaim
pass with only the provenance step replaced: the root and its ancestors
authenticate, the final component is a real directory (a symlink is refused)
with an allowlisted name directly under a configured scratch root on the root's
mount, nothing holds it (the `/proc` proof covers every thread), it is old
enough (or its lander owner is gone) and every git repository below it is clean
and published. The proof then runs a second time from the opened root with a
fresh process inventory, and the record is written only if that run passes on
the same directory and tree as the first. A clean repository at the candidate
root stays clean: its untracked `.rsi-scratch-record` (that exact line, at the
candidate root only) is excused from the dirtiness check. Writing the record
bumps the directory's mtime; it is put back (so adoption does not restart the
age clock) only if the tree afterwards is exactly the proved tree plus the
record, otherwise the directory is marked written now and waits the full age
again. **Adoption deletes nothing**: the reclaim pass re-proves everything
before it deletes, and **deletion is enabled**: an adopted directory that is old
enough and unheld WILL be deleted automatically. Adopting therefore needs an
explicit confirmation in the TUI naming every full path. Refusals are
typed (`outside_roots`, `missing`, `symlink`, `not_directory`, `held`, `young`,
`dirty_worktree`, `unpublished`, `unproven`, `already_recorded`, `changed`,
`failed`).

The pass renames the entry aside (`.rsi-reclaiming-<name>-<pid>`), re-proves,
persists the proved manifest in the registry (outside the tree) and deletes
only entries the manifest names. An entry that is new, replaced or edited stops
the deletion and keeps the rest. An interrupted delete resumes from the
persisted manifest: every survivor must be in it, otherwise the tree is kept,
and a manifest that exists but cannot be read, is not ours, is group-writable or
does not parse keeps the tree rather than counting as no manifest. Before each
directory is unlinked, and before the aside name itself is removed, the name is
checked to still be the directory that was emptied or proved; a replacement
observed by that final check is left alone. Limits: this is not protection
against arbitrary concurrent same-user mutation. A file (or a provenance file)
renamed onto a checked name between its check and its unlink is unlinked, and a
directory swapped in between the final check and rmdir is removed only if it is
empty. Registry pruning is off (#1150): allocation entries are kept, a few
hundred bytes each.
One time and entry budget (120 seconds, 32 reclaims, 2 million directory
entries) is charged as each entry is read, so enumeration, proofs and removal
are all bounded and a large tree ends as `partial` rather than overrunning.
`GetHealthStatus` reports `launch_floor` (available bytes, floor, and
`margin_bytes` above the `sandbox_min_free_gib` launch floor) and
`agent_scratch_reclaim` (the last pass). Workers should still remove their own
scratch before they finish.

## Manager and legacy build targets (#1864, #1218)

Worker and reviewer builds stay in their daemon-designated sandbox `target/`,
including jobs: Cargo config cannot redirect a job outside that target. Do not
assign ad-hoc `CARGO_TARGET_DIR` paths under `.cargo`, `.cache`, `staging` or
`review-targets`.

Submit manager integration builds through `AgentSubmitJob` with `worktree`.
The daemon creates a registered target under
`~/.rsi/build-targets/rsi-build-<hash-of-canonical-worktree-path>` and writes its
path to the job log's `rsi-build-target` line. Different integration source paths
remain isolated. Manager release builds use #1218's persistent
`~/.rsi/staging/release-target` instead, serialized under its existing admission
lock and bounded by its 16 GiB cap. The job inherits the
manager sandbox's build settings; the existing sccache setting now applies to
manager seats as well as workers, with incremental compilation disabled when
sccache is available. Release output persists after settlement; only the job's
private TMPDIR is removed. Registry allocation requires filesystem birth-time
proof; a platform that cannot supply it refuses manager scratch allocation.
The generic scratch sweep excludes #1218's `staging/release-target`: its own
size-cap/reclaim path preserves the lock inodes between builds.

The existing scratch sweep discovers these targets and the known legacy roots:
`~/.rsi/mgr/*/target`, `review-targets/*`, `review-target-*`, `koplik-mgr/target`,
`staging/*-target`, `cloud/artifact-target`, `.cargo/target-integ`, `target-integ2`,
`sat-target-deploy`, `shared-target`, and `.cache/{rsi,koplik}-review-*`.
Unrecorded targets are retained. The operator can inspect and adopt them through
the existing Settings > Scratch reclaim view and its RPCs; adoption records
provenance and deletes nothing. The sweep's existing three-day idle rule and
complete holder, mount, Git and deletion-manifest proofs apply. Unknown target
entries, symlinks, dirty nested worktrees and live consumers keep the target.
Only Cargo output is eligible: manager seat files and source trees stay outside
the allocation. No new setting or migration is introduced.
