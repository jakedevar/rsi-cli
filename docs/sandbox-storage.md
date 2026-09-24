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

Generations fence completions, so an older automatic preview cannot overwrite a
newer operator action or its post-action result. These states and observation
times are not persisted and introduce no daemon cache or wire contract. Every
`GetSandboxStorageStatus` remains a fresh dry-run preview.

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
