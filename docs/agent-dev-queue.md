# Opt-in agent-dev / batched-QA pilot

`scripts/agent-dev-queue.py` is a bounded, local pilot for Issue #750. It retains
immutable author commits, composes exact batches and records one owner's QA.
Intake and author check/test declarations are **unverified**. A successful QA
receipt grants eligibility for the existing independent review, accepted-source
admission and rolling landing gates. It never changes `rolling` or bypasses a
gate. This is a manually driven CLI, not a scheduler or daemon integration.

Nothing activates on installation or import. An operator must explicitly run
`init` in the intended repository. Do not initialize this pilot from an RSI
worker without an operator-authorized QA workspace and runner. Source sandbox
custody remains with the daemon. The pilot never deletes a worktree, branch,
source retention ref or cache, and never changes a source worktree's branch.

## Setup and commands

Requirements: Python 3.11+, Unix `flock`, Git with `merge-tree --write-tree`, and a
working user systemd manager (version 244+ for scope runtime limits) with delegated
memory/swap controllers. The QA
workspace must be a new canonical absolute path outside source/Git directories.
There is one queue, one explicitly recorded QA owner, one detached workspace and
one retained Cargo target directory per Git common directory (including all
linked worktrees). Ownership strings are recorded provenance, not authentication
or daemon authority. Use daemon-authenticated session/custody identifiers when
preparing requests; branch names are never treated as identity.

Write `pilot.json` **outside the source worktree**. Replace every example path,
SHA and identity below with actual values. Use absolute executables from the
pinned toolchain installation rather than mutable `rustup` proxy selectors.
Record any additional build inputs outside Git in `external_inputs` (for example,
a runner wrapper, toolchain libraries or a test fixture). The configured command
and toolchain probe are argv arrays; there is no shell expansion.

```json
{
  "base_ref": "refs/heads/rolling",
  "base": "FULL_CURRENT_BASE_COMMIT_SHA",
  "qa_owner": "DAEMON_QA_SESSION_AND_CUSTODY_ID",
  "workspace": "/srv/rsi-qa/worktree",
  "max_wip": 8,
  "batch_size": 4,
  "oldest_seconds": 900,
  "max_batches": 10,
  "environment": {
    "PATH": "/opt/pinned-rust/bin:/usr/bin:/bin",
    "HOME": "/home/operator",
    "XDG_RUNTIME_DIR": "/run/user/1000",
    "DBUS_SESSION_BUS_ADDRESS": "unix:path=/run/user/1000/bus"
  },
  "command": ["/opt/pinned-rust/bin/cargo", "test", "-p", "rsid", "--lib", "focused_filter"],
  "toolchain_command": ["/opt/pinned-rust/bin/rustc", "--version", "--verbose"],
  "external_inputs": ["/opt/pinned-rust/bin/rustc"]
}
```

Only the explicit environment is passed to QA and the runner. `GIT_*` and
`RSI_*` entries are refused. The pilot sets `CARGO_BUILD_JOBS=1`,
`CARGO_PROFILE_DEV_DEBUG=line-tables-only` and `CARGO_TARGET_DIR` to its persistent
cache outside the QA tree. Supply required user-bus variables explicitly; setup
failure never falls back to running tests in the daemon scope. Environment and
configuration are durable receipt contents, so supply only build-relevant data.

The resource boundary is an independently created user scope. Memory defaults to
6 GiB high / 8 GiB maximum. For a workload with a measured 8 GiB / 10 GiB need,
select this optional field in `pilot.json` **before initialization**:

```json
"memory": {"high_gib": 8, "max_gib": 10}
```

`high_gib` accepts integers 1..8; `max_gib` accepts integers 1..10, with
`high_gib <= max_gib`. Omitted fields default to 6 and 8 respectively. Zero,
negative, boolean, fractional, string, unbounded and above-ceiling values are
refused, as are unknown memory fields. Memory is immutable pilot configuration;
reinitialization with different values is refused. Swap remains zero, CPU weight
remains 50, and the sole-owner/concurrency and runtime/output bounds still apply.

The effective requested `MemoryHigh`/`MemoryMax` values are recorded in **bytes**
(1 GiB = 1073741824 bytes), along with `MemorySwapMax` and `CPUWeight`, in the
input fingerprint, job and worker terminal evidence. The exact runner argv is
also included in the job and terminal evidence and checked against the launched
argv before acceptance. Changed memory inputs or edited evidence invalidate the
receipt. These are configured launch values, not a live cgroup measurement; fake
runner tests establish validation and argument/evidence binding only.

The concrete platform invocation equivalent to the QA portion, using the default
6 GiB / 8 GiB memory values, is:

```bash
systemd-run --user --scope --quiet --unit=rsi-agent-dev-UNIQUE_UUID \
  --property=MemoryHigh=6G --property=MemoryMax=8G \
  --property=MemorySwapMax=0 --property=CPUWeight=50 \
  --property=RuntimeMaxSec=1860 --property=KillMode=control-group \
  --property=KillSignal=SIGKILL --property=SendSIGKILL=yes --same-dir -- \
  env CARGO_BUILD_JOBS=1 CARGO_PROFILE_DEV_DEBUG=line-tables-only \
  cargo test -p rsid --lib focused_filter
```

The script constructs these scope options with a unique batch UUID, serializing
memory as exact byte counts (defaults: `MemoryHigh=6442450944`,
`MemoryMax=8589934592`; 8/10 GiB: `8589934592` / `10737418240`). Inside
the scope it runs its Python worker, which first probes the toolchain, then runs
the configured QA argv in the exact environment. Both processes inherit the
scope. The worker exists to capture toolchain/terminal evidence; `_worker` is an
internal runner entry point and must not be invoked directly. The receipt records
the full scope argv as well as the probe and QA argv. If systemd fails, the probe
fails, or terminal evidence is missing, the batch is ineligible. There is no
alternate runner path. Tests provide a fake `systemd-run` on a temporary PATH;
they test orchestration, not Linux cgroup enforcement. `systemctl` is also required
on the explicit PATH and its executable digest is recorded. Scope inspection and
shutdown always target only this batch's unique `.scope` unit.

The following runtime/output limits are finite defaults **and hard ceilings**.
An optional `limits` object in `pilot.json` can lower individual values; omitted
keys use the defaults. Each value must be a positive integer (booleans are
rejected). Limits are immutable configuration and recorded in the job and receipt.

| Limit key | Default / maximum | Applies to |
| --- | ---: | --- |
| `probe_seconds` | 30 | Toolchain probe wall time |
| `qa_seconds` | 1800 | QA command wall time |
| `runner_seconds` | 1860 | Entire scope launcher wall time and scope `RuntimeMaxSec` |
| `probe_bytes` | 65536 | Combined probe stdout/stderr retained in terminal evidence |
| `qa_bytes` | 16777216 | Combined QA stdout/stderr forwarded into the log |
| `log_bytes` | 17825792 | Entire `qa.log`, including launcher diagnostics |

For example, `"limits": {"qa_seconds": 600, "qa_bytes": 1048576}` selects a
10-minute QA deadline and 1 MiB QA output cap. The outer deadline/cap can be
smaller than the inner limits; the first breached limit wins. Byte ceilings
truncate the retained prefix exactly, and any excess fails QA even if the
command races a successful exit. Metadata records the breached limit, retained
byte count, observed exit, reap outcome and scope shutdown evidence. UTF-8
replacement/JSON escaping can enlarge encoded probe evidence; terminal JSON is
separately capped at 1 MiB when read. No subprocess `capture_output` or unbounded
`wait` is used for probe, QA, launcher or scope-control commands.

The Python supervisor drains stdout/stderr incrementally with selectors into
bounded buffers or the capped log. It starts a process group for each command,
uses monotonic deadlines, kills that group on a limit, and waits at most 5 seconds
for its leader. A child retaining inherited output pipes cannot extend the
deadline indefinitely. The scope's independent runtime bound remains in effect
if the Python supervisor dies. The supervisor also checks the exact scope after
every launcher outcome. A scope still active after launcher exit is stopped and
makes the attempt fail, even if the command returned zero. This covers descendants
that moved outside the launcher's process group. Scope control has at most three
calls (`show`, `stop`, `show`), each limited to 10 seconds plus a 5-second reap and
4 KiB combined output; it cannot hold the queue lock indefinitely. Allow at most
50 seconds beyond the launcher deadline for reap and scope settlement (excluding
Git/filesystem operations).

A limit breach is never eligible. If launcher reaping and an inactive scope are
confirmed, the terminal failed receipt can be inspected/retried idempotently and
retired for owner repair. If shutdown/reaping is unconfirmed, evidence is still
journaled with owners, but status stays `running`; neither promotion, retirement
nor automatic rerun is permitted. A failed control command never substitutes
process-group termination for verified scope custody. Kernel-uninterruptible
processes or an unavailable user manager require external custody resolution.
The caps bound captured process output, not arbitrary files the trusted QA command
writes itself or retained build-cache size. Pinned absolute toolchains and declared
external inputs remain required as described above.

```bash
python3 scripts/agent-dev-queue.py --repo /path/to/repo init /tmp/pilot.json
python3 scripts/agent-dev-queue.py --repo /path/to/repo intake /tmp/source.json
python3 scripts/agent-dev-queue.py --repo /path/to/repo status
python3 scripts/agent-dev-queue.py --repo /path/to/repo freeze --owner QA_OWNER
python3 scripts/agent-dev-queue.py --repo /path/to/repo run --owner QA_OWNER
python3 scripts/agent-dev-queue.py --repo /path/to/repo eligible
```

Exit status is 0 for a successful operation, 1 for a recorded conflict/failed QA
or ineligible receipt, and 2 for a refusal or operational error. Always inspect
the JSON outcome; `status` is descriptive and does not itself validate receipts.

An intake request has exactly these fields:

```json
{
  "key": "work-750-revision-1",
  "source": "FULL_IMMUTABLE_SOURCE_COMMIT_SHA",
  "source_path": "/absolute/daemon-owned/source-sandbox",
  "owner": "AUTHOR_SESSION_ID",
  "custody": "DAEMON_CUSTODY_ID_AND_EPOCH",
  "checks": "Declared cheap checks and their observed results",
  "tests": "Declared regression tests; execution remains unverified"
}
```

The source must belong to the same repository, have exactly that HEAD and be
clean, including ignored files. Repeat an identical intake key safely; changing
its payload or submitting the same SHA under another key is refused. No check or
test declaration is promoted into a passing result. Retention begins atomically
with intake, before the source worktree is eligible for any later custody review.

`freeze` takes oldest pending members, at most `batch_size`. It becomes ready
when the count reaches that size **or** the oldest age reaches `oldest_seconds`.
There is no background age timer: an external authorized caller invokes the CLI.
Hard ceilings are 64 unverified WIP, 16 members per batch, 100 batches per pilot,
and an age threshold between 1 and 86,400 seconds. Active batch members count
against WIP. No second batch can run while one is active. The configured base
must still match its ref; each successful external landing observation advances
the accepted base for the next batch. An unrelated base advance fails closed.

## Evidence, recovery and repairs

All mutations use one nonblocking lock in the Git common directory; the lock
covers QA execution. Git commits under `refs/agent-dev-pilot/journal` hold the
append-only state history. A single `git update-ref --stdin` transaction commits
state, source/batch retention refs and expected-ref fences together. There is no
separately authoritative filesystem state file. A crash before a transaction can
leave unreachable Git objects, but cannot publish a partially admitted member or
batch. Normal Git garbage collection can reclaim unreachable objects; the pilot
never removes retention refs.

Batches retain exact base/candidate SHAs, candidate tree and ordered member keys.
`merge-tree` and `commit-tree` compose candidates without touching source trees.
On conflict, all sources, owners, custody records and conflict output survive in
the journal. No manual conflict resolution in the QA tree is accepted. The whole
bounded batch is attributed for repair when a single responsible member cannot
be established. After conflict or a terminal failed receipt:

```bash
python3 scripts/agent-dev-queue.py --repo /path/to/repo retire \
  --owner QA_OWNER --reason 'Owners will reconcile the conflict in new source commits'
```

Retirement preserves all records and refs, stops old eligibility and releases WIP
for **new** source commits. It does not erase or reset anything. Batch budget is
not replenished. A passing batch stays active until external reviewed landing:

```bash
python3 scripts/agent-dev-queue.py --repo /path/to/repo landed --owner QA_OWNER
```

`landed` only observes that the exact passing candidate is an ancestor of the
current base ref, verifies the receipt, and settles queue accounting. It does not
perform a merge, promotion or review, or establish that external review occurred.
The external landing owner must perform the existing gates. The same QA
workspace and target cache are then reused for the next batch.

Before runner launch, `running` is committed. A crash or uncertain launch never
causes an automatic rerun. The unique systemd unit, workspace, log and sources
are preserved for the custody owner. A `running` batch without committed terminal
evidence, or with unconfirmed scope shutdown, requires external custody resolution; this pilot intentionally provides
no force-reset or retry that could race an orphan runner. It cannot be retired.
An exhausted pilot or stale accepted base also needs an external operator decision;
there is no automatic reinitialization or retention cleanup.

Terminal evidence is journaled before the filesystem receipt mirror is published.
If only that mirror is missing after a crash, recover without rerunning QA:

```bash
python3 scripts/agent-dev-queue.py --repo /path/to/repo recover --owner QA_OWNER
```

Recovery checks all remaining inputs and evidence first. It reconstructs a
**missing** mirror from the journal and refuses a changed mirror. A crash before
terminal journaling remains uncertain even if a terminal file exists. This is
intentional at-most-once execution, not a claim of distributed exactly-once QA.

Receipts bind candidate SHA/tree, base, ordered source SHA/tree/owner/custody,
QA owner, config, environment, argv, executable/driver digests, toolchain output,
Git local config, all tracked inputs (including lockfiles and toolchain/config
files), declared external inputs, discovered Cargo ancestor/home configs and
Rustup settings (including absent files), log digest and terminal exit. Before
and after QA and on eligibility checks, exact inputs must remain clean and refs
must match. Edited logs, receipts, configs, executable files, workspace custody or
source retention refs fail closed. Changes during QA preserve the workspace and
leave an uncertain ineligible batch. Submodules, sparse/assume-unchanged indexes
and symlinks escaping the worktree are unsupported. Ignored build output belongs
in the retained external cache; unexpected output inside the worktree is dirty.

The local journal protects against accidental receipt edits and stale inputs; it
is not a cryptographic authentication boundary against a user able to rewrite Git
refs or replace trusted executables. The operator is responsible for pinned,
trusted runner/toolchain binaries, declared external build inputs, and a working
systemd user resource controller. Network dependencies and arbitrary external
services are not hermetic inputs. No performance or cgroup-enforcement claim is
made by the fake-runner tests.

## Cleanup evidence and verification

`cleanup-evidence` emits exact source identities, retention refs, cleanliness and
observed landing status. `candidate_for_daemon_custody_check` is only a candidate
for inspection. `cleanup_authorized` is always false. The daemon custody owner
must independently verify terminal status, current custody identity, clean exact
inputs, retention and absence of active, sealed, review, pinned and wake custody.
Never infer those facts from a branch name or this report. Retain the QA workspace
and cache. This script performs no sandbox/branch deletion or daemon DB writes.

Portable regression tests use real temporary Git repositories and Python fake
commands; they never run Cargo:

```bash
python3 -m unittest discover -s scripts/tests -p test_agent_dev_queue.py
git diff --check
```
