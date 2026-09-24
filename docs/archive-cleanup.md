# Safe cleanup during ordinary archive

Normal `ArchiveSession` can retire the registered Git worktree for one exact,
terminal, current leaf Session. The public manager/RPC route applies a read-only
structural classifier before generic D00 tree expansion, then delegates the
selected candidate to one private operator-only lifecycle service. It is
not a general directory cleaner. The daemon performs cleanup only when every
custody, Git, content, dependency, process, and topology proof agrees.

The existing archive action and keybinding are unchanged. A successful cleanup
toast identifies the settled run and the source branch and commit that remain
preserved. A refusal leaves the Session visible and reports a safe reason,
whether retry is allowed, and the next action. An RPC timeout is not success.

## Eligible shape

The cleanup path applies only when the requested Session itself is one
spawnable leaf with all of the following properties:

- its durable status is `Completed`, `Failed`, or `Interrupted`;
- it is inactive and agrees with the daemon's completed-session projection;
- it exclusively owns one current, verified, `Live` Git-worktree custody root;
- it has no child or later lineage successor, shared participant, reserved or
  active effect, pending retry, scheduled-job/path dependency, provider
  process, or observable filesystem holder; and
- its root, Git administration, direct source branch, HEAD, content, index,
  submodules, bounded tree, and repository identity all pass exact reproof.

Group/Epic archive, descendant trees, pending automatic archive, delete, purge,
launch failure, provider replacement, rotation history, and generic startup
reconciliation keep their previous behavior. D00 remains the retention-only
decision boundary for every other cleanup shape.

## Preservation proofs

There are exactly two successful preservation classes:

| Class | Required proof |
|---|---|
| `no_output` | The direct source branch, worktree HEAD, and allocation commit are the same exact commit, and the worktree/index have no output. |
| `integrated_ancestor` | The direct source branch and worktree HEAD agree, and that exact source commit is an ancestor of the daemon-derived current symbolic local target ref and OID. |

Cleanliness, age, a plausible merge, or patch-equivalent content is never
enough. Unique, unpushed, unmerged, non-ancestor, changed, symbolic, detached,
missing, unreadable, dirty, hidden, shared, live, or ambiguous evidence is
retained. Replacement-object and graft-based ancestry cannot authorize this
path.

The source branch is the preservation sentinel. Cleanup never deletes,
recreates, rewrites, or repairs it. A settled receipt records its exact direct
ref and commit OID, and recovery rechecks both at every destructive boundary.

## Journal and recovery

Before any Git effect, the daemon commits an `IntentCommitted` run and event.
It then moves the exact registered worktree non-force to a deterministic private
quarantine, repeats all applicable proofs, persists a canonical
removal-authority marker, and asks Git to remove only that registered worktree
without force. Raw recursive deletion, prune, wildcard selection, and fallback
ref mutation are not present.

Only after Git proves the original and quarantine paths and registrations
absent—while the direct branch remains exact—does one immediate database
transaction archive the Session, exhaust pending lifecycle state, tombstone
custody, null linked paths and the execution CWD, and commit the immutable
receipt, event, and one pending success projection. TUI row removal, timers,
maps, bus projection, and the success response follow that commit.

| Durable phase | Restart behavior |
|---|---|
| `intent_committed` | Reprove the exact original, or recognize one exact lost move acknowledgement; ambiguity stops. |
| `quarantined` | Reprove the recorded quarantine, content, holders, refs, target, custody, and Session before creating authority. |
| `removal_authorized` | Reload and exactly match the canonical marker, or recognize one exact lost removal acknowledgement. |
| `worktree_removed` | Reprove absence and the preserved branch, then retry only the final database transaction. |
| `settled` | Return the same immutable receipt and drain only undelivered consumers of the same stable projection. |
| `refused` | No Git effect occurred. Resolve the reported condition before a fresh normal archive attempt. |
| `recovery_required` | Preserve all remaining evidence. V1 performs no further destructive retry. |

Startup resumes only existing authenticated runs, in bounded keyset pages,
after source-worktree cohort recovery and before generic custody
reconciliation. It does not discover an old path and invent cleanup authority.
Exact archive replay and the operator-only `GetArchiveCleanupStatus` contract
provide lost-response readback selected by Session UUID; callers cannot select
a run, path, repository, ref, custody tuple, proof, marker, or receipt.

## Success projection

V112 is the forward-only convergence boundary for the authenticated current
and historical V111 catalog lineages. V113 adds one deterministic
`session_archived` projection per settled run and three durable consumer
receipts: watch/map reconciliation, bus notification, and memory
synchronization. The projection ID is derived from the run ID; replay cannot
manufacture another logical ID.

Each consumer first accepts the stable projection ID and applies its idempotent
effect, then advances its durable receipt to `delivered`. A failure before
application or before acknowledgement leaves the receipt in `delivering`, so
startup retries the same ID. Bus transport may therefore redeliver that ID
after a crash, while a lifecycle-scoped in-flight guard prevents concurrent
application and retains no daemon-lifetime history. Watch/map application is
idempotent. Memory synchronization carries the projection ID as a typed worker
command and waits for an explicit completion result. The worker accepts only a
durably `delivering` memory consumer, runs the synchronization, advances that
exact consumer to `delivered`, and only then reports success. A channel loss,
worker failure, or crash before that durable acknowledgement propagates as a
failure and restart retries the same ID. A repeated command for an already
delivered ID is an idempotent no-op. If no memory worker is available, the
consumer remains `delivering` for later recovery; absence is never acknowledged
as application. Startup excludes memory-only projections while memory is
disabled, so each bounded pass can continue draining watch and bus work without
reclaiming the same unavailable effect. A later available worker revisits those
same stable projection IDs. Normal archive completion applies the same
availability-aware filter for that Session and drains actionable consumers in
bounded pages, so historical memory-only receipts cannot hide the current
archive's watch or bus effect. Delivered receipts are terminal and skipped on restart.
Durable Session, cleanup receipt, and projection rows remain the source of
truth for reconciliation. Historical settled runs are backfilled as delivered
and are never republished during upgrade. Startup drains pending projections
in bounded batches after cleanup-run recovery.

If a refusal is retryable, stop competing maintenance, correct the reported
condition, and use the normal archive action again. If the result is
`recovery_required`, preserve the repository, source branch, database, and any
daemon-owned quarantine so a compatible recovery binary can inspect them. Do
not manipulate the evidence manually.

## Ordinary unarchive

A latest matching `settled` ordinary cleanup receipt admits the existing
fresh-custody `UnarchiveSession` flow. Unarchive keeps the same Session UUID,
raw title, display role, Epic ordinal, hierarchy, and rotation lineage while
allocating a new custody identity, root, and branch and restoring status to
`Completed`. Both the replacement root and its default branch are derived from
the fresh allocation identity. The new branch therefore cannot reuse, delete,
or alter the preserved `rsi/<archived-session-id>` source branch.

Source-worktree cohort-settlement receipts remain historical and continue to
block ordinary unarchive. An ordinary archive receipt is neither cohort
authority nor authority for another cleanup run.

## Operational boundary

This mechanism assumes cooperative single-user operation. The daemon serializes
its own lifecycle, provider-CWD, repository, and custody effects and repeatedly
observes bounded Linux process inventories. It cannot defend against an
actively malicious same-UID process racing raw paths, refs, SQLite, queued file
descriptors, or kernel references outside those inventories. Stop competing
maintenance before archiving an eligible sandbox.

Do not use raw SQLite, filesystem deletion, branch manipulation, worktree
pruning, force flags, or manual quarantine changes as an operating procedure.
The supported surface is the normal operator archive/unarchive flow and typed
receipt/status truth.

Deployment rollback is also fail-closed. Before installing a binary that lacks
this journal version, prove there are no nonterminal archive-cleanup runs, or
retain a compatible recovery binary. Schema migrations are forward-only; do
not downgrade `user_version` or hand-edit the archive catalog.

This path is separate from [source worktree cohort settlement](cohort-settlement.md),
which is an explicit audit/phrase workflow that may retire an integrated source
branch, and from [sandbox storage](sandbox-storage.md), which reclaims only
regenerable target-cache data under the independent 69A policy. Ordinary
archive cleanup changes neither system's authority, receipts, settings, or
scheduling.
