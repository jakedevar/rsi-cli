# Source worktree cohort settlement

Source worktree settlement is an operator-only maintenance path for terminal
RSI Git-worktree sandboxes whose source commit is already integrated. It
removes only the strongest mechanically proved case: the exact source-ref tip
is an ancestor of the daemon-derived, symbolic local target ref at both audit
and apply time.

This is destructive local maintenance. It is not a general stale-directory
cleaner, a merge detector based on patch similarity, or authority to discard
unique work.

## V120 batch substrate

V120 adds only the durable substrate for a future V2 batch protocol. It does
not activate a new audit, apply, ArchiveSession cleanup, or TUI action. The
existing V1 cohort audit and settlement behavior remains the active operator
surface until the later runtime slices land.

The future V2 request accepts a repository identity and an opaque
`rsi-swc2:` cursor; an initial audit may omit the cursor while apply requires
one. A cursor is at most 4 KiB and encodes canonical JSON plus a keyed BLAKE3
MAC. It contains no paths. The daemon authenticates the MAC with an internal,
immutable random 32-byte database key before decoding or using any claim. The
claims bind the schema and policy versions, repository identity digest,
complete snapshot digest and count, target ref and OID, finite upper custody
ID, page start and end, batch ordinal, `has_more`, and predecessor terminal
receipt. An apply phrase is checked only after that authentication as
`APPLY <identity> BATCH <ordinal> <digest>`. Clients cannot select a path,
root, branch, target, or ordinal.

The V120 Store API captures the complete custody-ID count, digest, upper key,
and requested 256-row detail page inside one short SQLite read transaction,
with a fixed 16,384-root ceiling and `O(256)` enumeration memory. The UUID
upper key bounds key space; the read snapshot supplies insertion-time
consistency. Every later request must recapture and match the complete metadata
before trusting a cursor. Concurrent insertions below the page cursor or inside
the remaining key range therefore change the next capture rather than entering
or escaping an established one.

A page contains at most 256 custody aggregates and at most 1,024 total
participants. Exact candidate inventory caps participant and custody-event
count subqueries at ceiling plus one, then exposes saturated counts with
explicit overflow flags. Overflow retains the aggregate and never exposes a
truncated eligible proof. The inventory also includes owner status, custody
projection, active effects, participants, and custody-event lineage evidence.
Repository-wide source-ref collisions use an indexed normalized-ref query,
including roots on later pages.

The daemon also records canonical scheduled-job working-directory projections.
Changing a raw working directory makes its projection unverified until bounded
reconciliation can prove the canonical directory. Before trusting a cached
negative lookup, dependency proof streams enabled jobs and executable or
restorable leaf Sessions in 256-row keyset pages, with an independent 16,384
relevant-row ceiling for each class. Session relevance comes from raw active
state plus authenticated execution history: malformed active leaves remain in
the health set, while proven containers and internally consistent removed or
quarantined history do not consume its capacity. V120 seeds one indexed
relevance row per Session and refreshes the exact affected rows after Session,
execution-projection, or custody-root inserts, updates, and deletes. Guard
triggers compare every classification write with the same Rust classifier and
forbid removing a classification while its Session exists. Fresh proof pages
only indexed relevant or invalid rows, then reloads and recomputes each one;
large inert history therefore cannot consume the bound or hide a missing,
forged, or stale negative classification.

The fresh pass re-canonicalizes every mutable path and validates its projection.
It detects both an existing alias repointed into a candidate and an ordinary
outside directory or ancestor replaced by an inward symlink. Missing,
malformed, stale, unverified, or over-ceiling relevant evidence makes proof
incomplete and retains the affected root.

After that health pass, indexed raw and canonical path-prefix supersets are
checked with exact current containment and hashed evidence. The proof covers
nested paths, original and quarantine aliases, enabled-job wake owners,
`on_terminal` watched Sessions, and Session raw paths and effective CWDs.
Disabled jobs and non-executable container Sessions are excluded; enabled jobs
without a working directory remain healthy when their wake and watch authority
is valid and links are still accounted for. In particular, `agent_fresh`
requires a canonical origin UUID, and `on_terminal` requires a canonical
watched Session UUID. The projection uses the same wake decoder as scheduled
job reads and retains stricter canonical evidence checks. Distinct
scheduled-job and Session counts and digests make the result stable without
loading custody inventory for unrelated roots.

Normal scheduled-job insertion, guard restoration, program recovery, and
agent-message watch insertion refresh projections in the same transaction, so
refresh failure rolls back the raw mutation. Startup reconciliation is bounded
and prioritizes relevant enabled or unhealthy rows so disabled history cannot
starve a later enabled job. Existing-schema reopen validates the exact V120
catalog fingerprint and refuses catalog poison while preserving malformed
legacy path rows as unhealthy evidence. These projections do not replace the
live process, holder, CWD, cleanliness, custody, or Git checks required before
any effect.

## Open and audit a cohort

Open **Settings** with `<Space>,`, enter **Daemon Features**, select
**⚠ Source worktree settlement**, and press `Enter`. The overlay lists
repository cohorts discovered from live daemon custody plus the latest durable
receipt for a previously settled cohort. Receipt-backed cohorts remain
selectable even when they have zero Live roots; the caller cannot supply an
arbitrary repository, worktree path, or run identifier.

Select a repository with `j`/`k` and press `Enter` to audit it. An audit:

- is bounded to 256 roots, sorted deterministically, and reports `writes=0`;
- derives one current symbolic local target ref and its exact OID;
- authenticates custody, root containment, Git worktree registration, source
  ref and HEAD identity, tracked, untracked, ignored, and index-hidden
  cleanliness, terminal ownership, exclusivity, and absence of reserved or
  active effects;
- refuses enabled scheduled-job dependencies, Session-path or effective-CWD
  dependencies, provider orphans, symbolic ref dependents, unreadable holder
  inventories, or a tree that cannot pass the bounded safe-tree proof;
- classifies every observed root as eligible or retained; and
- produces a canonical `sha256:` plan digest and authorization phrase only
  when at least one root is eligible and the cohort has no global refusal.

The plan digest binds the target and eligible roots with their custody, Git and
dependency evidence. Retained rows stay visible in the audit but their metadata
and unrelated Git ref changes do not invalidate the plan. A change to the
eligible set or its safety evidence still refuses apply. Unrelated refs are
compared immediately around removal to prove the effect preserved them.

Collapsed ignored `__pycache__/` and `.pytest_cache/` directory records, and
ignored records whose final path component ends in `*.pyc`, are regenerable
output and do not make a root dirty; non-force worktree removal discards them.
An individually reported ignored file below a cache directory is accepted only
when its final component ends in `.pyc`; other ignored content stays protected,
including ignored files below a non-ignored cache directory and a collapsed
ignored parent. Audit does not delete regenerable output. Archive cleanup uses
the same worktree observer and therefore applies this same ignored-cache rule.
Tracked edits and untracked files remain protected regardless of their names.

The only positive proof in this release is `integrated_ancestor`. A dirty,
active, nonterminal, missing, symlinked, unregistered, shared, effectful,
misidentified, target-ambiguous, or non-ancestor root is retained. A clean
non-ancestor is still retained: patch equivalence and a plausible merge are not
preservation proofs.

A paused or unfinished worker is retained as `nonterminal_status` even when its
clean tree is already in Git (#247): a `Completed`, `Failed` or `Interrupted`
Session that the operator paused, or that is the live worker of an Issue still
`InProgress` whose latest turn carries no final report (a manager halt for
higher-priority work looks like a finished Interrupted session otherwise). The
completion authority is the existing worker report contract (`RESULT` or
`PIPELINE HANDOFF` in the latest turn) alone: an Issue handoff by itself, old or
written in the current turn, is progress (pre-verification, WIP, baton), never
completion, so it conservatively retains the root until the worker reports or
the Issue ends. The audit diagnostic names the hold, and the last fence before
the first effect re-reads it: a root held after its intent was committed and
before any Git effect closes as `unattempted`, only that root, with its sandbox,
branch and Session untouched. The hold ends with the work: a final report in the
latest turn, a closed or cancelled Issue, a successor launch for the Issue, or
an explicit archive (an `Archived` Session settles normally). The session
retention pass shares the same predicate (`unfinished_issue_worker`), so neither
automatic path retires a root the manager plans to resume, and a finished worker
stays reclaimable.

Changing the selected repository invalidates the displayed audit. Run a new
audit rather than carrying evidence between cohorts.

## Apply the audited plan

With a fresh applyable audit displayed, press `A`. The authorization field is
empty. Type the complete displayed phrase exactly, including its repository
identity and digest:

```text
APPLY <repository-identity> sha256:<64 lowercase hex digits>
```

Press `Enter` to submit or `Esc` to cancel. A wrong phrase is rejected in the
TUI before any apply RPC is sent. Apply then recomputes the complete audit and
requires the same target, evidence, digest, phrase, and repository identity
before it commits a durable intent. Audit drift therefore produces no Git or
database effects.

For each still-valid item, the daemon:

1. commits durable intent, moves the exact registered worktree into a private,
   deterministic `.settlement-quarantine/<run>/<session>` path, and proves its
   tree, Git administration identity, holders, target, source ref, and aliases;
2. persists a canonical removal-authority marker before either destructive
   Git effect;
3. atomically compare-deletes only the expected direct local source ref while
   the registered quarantine remains a sentinel;
4. removes that now-dangling quarantine with non-force `git worktree remove`
   under the source-ref lock and proves the source ref, original root,
   quarantine root, and registrations absent;
5. records the compatibility journal phases `worktree_removed` and
   `branch_removed`; both Git effects already occurred branch-first before
   those labels advance; and
6. atomically archives the session, clears pending lifecycle state, tombstones
   sandbox custody, removes stale C5/lead/retry state, and advances the durable
   receipt.

The target ref, `main`, unrelated local refs, all remotes, and remote-tracking
refs are outside the mutation surface. The daemon never runs forced worktree
removal, forced ref updates, prune, remote deletion, raw `remove_dir_all`, or a
recursive fallback. Existing D00 cleanup paths remain default-deny.

One stable idempotency key is retained while a TUI apply attempt is retried.
An exact replay returns the same receipt; reusing the key with changed content
is a conflict. Do not treat an RPC timeout as success—read the receipt.

## Receipts and restart recovery

After apply, the overlay reads the durable receipt back before reporting the
result. It shows the run phase, settled/refused/recovery/unattempted counts,
each item phase, refusal code, post-observation, and last error. Press `r` to
refresh the displayed receipt; `J`/`K` or `PageDown`/`PageUp` scrolls it.

If the daemon commits a run but its apply response is lost, reopen the overlay
after restart and audit the same repository cohort. A bounded, per-repository
monotonic pointer recovers that repository's latest durable run; this does not
depend on SQLite rowids or the repository path still existing. The TUI reads
the exact receipt and requires its run ID, repository identity, and canonical
repository path to match before displaying or refreshing it. A live cohort can
therefore show both a fresh applyable audit and its preceding durable receipt.
Only the latest receipt for each repository is discoverable through this
recovery path; immutable older runs remain database history rather than an
operator-selectable list.

The daemon resumes nonterminal journaled runs after RPC readiness. Durable
journal fences protect their sessions, custody and paths throughout startup;
ordinary custody reconciliation skips these roots. Launch, cleanup and archive
cannot act on an unproven root while recovery is pending. Recovery authenticates
current Git and database state
against the recorded expected identities. It completes only effects it can
prove, records partial or recovery-required state otherwise, and does not
create a new run. Receipt-backed settlement archives are historical purge
records and cannot be restored with ordinary unarchive; preserve the receipt
for diagnosis. After its root is purged, a Session named directly by a
settlement receipt may be archived or soft-deleted, but it is intentionally ineligible for permanent
hard purge because the immutable receipt is its destructive-maintenance audit
trail.

A holder-inventory deadline records `quarantine_proof_deferred` in the item's
post-observation and retains `intent_committed`, including any durable removal
marker. The next apply replay or startup recovery retries the complete proof;
no source ref or quarantine is deleted on an incomplete proof. On a manual
(agent or operator) run the 300-second settlement ticker also retries it on
every pass, whatever the disk pressure (#271); an automatic run's deferral is
resumed by the automatic pass under disk pressure. The ticker skips a run this
daemon is already driving (a detached apply, #2866) and resumes only unfinished
items: reopening fences stays with startup recovery. A detected untrusted
holder still enters `recovery_required`.
An attempt that stops short on an item whose root it can no longer prove
untouched (the root is already quarantined, or a marker is recorded) keeps its
phase and marker and records why in the post-observation (#273):
`settlement_surrendered: runtime_state_contended` when the provider-cwd
admission or runtime-state wait expired, `settlement_surrendered: root_busy`
when a lock-order-bounded Store or root-lock wait surrendered. A surrender
under a root lock or the repository mutex is noted in memory and written once
the attempt holds the Store with no root lock, so the write never waits for
the Store out of lock order. A deadline deferral already on record is kept.
The ticker retries a manual run's surrendered items like a deadline deferral;
a proven-untouched item is still released as a typed refusal. Holder scans measure their
first pass to budget the remaining passes, with a 60-second attempt ceiling.
A single audited reproof attempt can supersede a run containing a first-proof
fence: the item must have phase `recovery_required`, refusal
`recovery_proof_failed`, no `before_observation` removal marker, and the exact
post-observation `quarantined worktree failed its removal-authority proof`.
Other proof failures remain terminal unless covered by the audited completion retries below. Startup recovery or replay of the
original apply request records the attempt and each item's old fence timestamp
atomically before returning those items to `intent_committed`. A replay whose
receipt is fully terminal but still holds such a revivable fence enters the
ordinary apply path rather than returning the stored receipt (if an owner is
active it still answers with the stored receipt); once nothing is revivable the
stored receipt is returned unchanged. Migration alone
performs no revival or filesystem operation.

The full removal-authority proof runs again, including current custody, runtime,
Git, clean-tree, target-preservation, alias and holder evidence. A real holder
re-fences the item. Later recovery can re-prove that fence through the completion retry journal. A proof deadline
retains the reopened intent for ordinary replay. The original run identity and
quarantine paths remain fixed. Original counters remain frozen in
`source_worktree_settlement_runs`; the distinct attempt in
`source_worktree_settlement_reproof_runs` has its own monotonic counters, and
immutable `source_worktree_settlement_reproof_items` records explain the
transition. Receipts and recent-run summaries project the current attempt's
counts, so current recovery-required counts can decrease across supersession
without rewriting either attempt's counter history.

V172 adds an append-only completion retry journal for ref-delete and worktree
residue fences carrying removal authority, first-proof fences after V171 reproof,
and the exact historical `root_busy` failure. Each recovery pass records the
preceding fence timestamp, refusal and observation before reopening it. It
rechecks custody, runtime, ancestry, Git identity and all holder evidence;
malformed authority, drift and real holders remain fenced. A custody-lock
`root_busy` keeps its existing intent pending for the next pass.

V173 also admits the exact retained-intent fence `retained intent followed an
earlier terminal stop and cannot prove no prior effect`. Before recording a
retry, recovery holds provider-cwd, repository and custody-root exclusion and
proves zero recorded and current reserved/active effects, no removal marker,
no quarantine path, and unchanged original root, clean tree, source ref,
registration and custody. Failed or incomplete evidence preserves the original
fence. A successful proof appends the preceding fence to the immutable
completion retry journal and runs the full settlement proof with fresh initiating
policy. Migration alone changes no item or run; existing counters stay frozen.

Retained markers bind the exact quarantine paths, refs, journal OIDs, Git
administration, root device/inode, and clean-tree digests. The current target
tip and holder inventory may change between complete proofs; the fresh final
proof supplies the target OID used by the ref transaction. Unrelated shared-ref
writes do not invalidate settlement's exact source, target, path and registration
postconditions. Every existing quarantine still requires a fresh final holder proof.

For a registered quarantine in this completion path, a verify-only Git ref
transaction on the live *source* ref runs a prepared hook that re-proves the
direct exact source commit and removes only that worktree without force. The
target ref is not locked during removal, so a large tree cannot hold
`refs/heads/<target>.lock`, and the 30-second Git bound can strand at most the
retired source ref's lock. A short second transaction then verifies the target
and deletes the exact source ref under both locks. A crash or refusal between
the two leaves the branch-only shape (source ref exact, no path, no
registration), which the next retry completes; exact path, registration and ref
postconditions are checked, with source-ref compensation on failure. A
canonical marker and fresh custody/ancestry checks also permit completion when
both paths and all registrations are gone but the expected source ref remains.

If the quarantine path is gone but its Git registration remains (for example
`git worktree remove` deleted the tree and then failed to delete
`<common>/worktrees/<id>`), an audited completion retry removes exactly that
administration directory, under the same source-ref-only transaction, and then
continues the branch-only completion. The directory must be the single one whose
`gitdir` pointer names the absent quarantine, with a matching common dir, no lock,
a `HEAD` naming the source ref or its exact commit, and the id and path recorded
in the retained marker. The source-ref lock does not cover those admin files, so the
transaction's prepared hook repeats the whole proof (regular single-link files, exact
`HEAD` and common dir, no lock, absent original and quarantine paths, unique `gitdir`
claimant) immediately before the deletion; any drift between the daemon's last
observation and the hook refuses and preserves the directory (the item stays fenced
`worktree_residue`). Nothing else is removed and `git worktree prune` is never
run. Earlier run and V171 attempt counters stay frozen;
receipts project current item counts once completion retry history exists.

Recovery shares one preliminary holder inventory across the roots in each
retained run, with one process walk per fixed-point pass. An unreadable task is
rechecked once; an exited or reused process incarnation is discarded, while a
persistent unreadable task still fails the proof. Its pid, uid and comm are
reported once in the shared-pass diagnostic. A shared preliminary snapshot
never replaces the fresh final holder proof after the removal marker is
persisted. Roots whose tree changed since the snapshot receive a fresh initial
proof too. No deletion is authorized by a cached proof.

The recovery matrix is deliberately narrow:

| Durable and observed state | Recovery |
|---|---|
| No marker; original present, quarantine absent, source exact | Revalidate, move to quarantine, then prove and persist authority. |
| No marker; only quarantine present, source exact | Prove or narrowly repair the interrupted exact move, then persist authority. |
| Marker; quarantine present, source exact | Reprove identical destructive authority, compare-delete the ref, then remove quarantine. |
| Marker; quarantine present, source missing | Authenticate the marker and recorded commit, restore only the missing exact source ref, repair an exact detached sentinel when possible, then replay. Drift preserves the restored branch and enters `recovery_required`. |
| Audited completion retry; marker, both roots absent, the exact marked administration still registered, source exact | Remove only that administration directory under the source-ref lock, then continue as the next row. |
| Audited completion retry; marker, both roots and registrations absent, source exact | Revalidate custody and ancestry, verify target/source locks and absence, delete only the expected source ref, then finalize. |
| Marker; both roots and registrations absent, source missing | Treat both exact Git effects as complete and advance the compatibility phases. |
| `worktree_removed` or `branch_removed`; roots, registrations, and source ref absent | Trust only the matching authenticated marker's already-recorded pre-effect authority, then advance or retry database finalization. |
| Path collision, changed/symbolic/non-commit ref, marker mismatch, unreadable proof, or ambiguous registration | Preserve evidence and enter `recovery_required`; never overwrite or force. |

Legacy rows without a matching canonical marker never authorize a new source
ref deletion. A missing source is recreated only at its recorded commit and only
with an atomic missing-only compare-create; an existing ref is never
overwritten.

If a run is partial or requires recovery, stop manual cleanup. Keep every
remaining root, ref, and daemon-owned quarantine intact, restart the daemon if
recovery has not yet run, refresh the receipt, and investigate its exact
refusal and observation. Never move or remove `.settlement-quarantine` by hand.
Do not use raw SQLite, `git worktree remove --force`, `git worktree prune`, or
manual branch deletion as a fallback.

## Concurrency boundary

The daemon fences its own provider spawns and repository mutations and uses
bounded, repeated Linux process inventories for cwd, root, fd, fdinfo, maps,
and mount evidence. Narrow authenticated permission-only exemptions for the
user systemd manager, `sd-pam`, the document portal, and its FUSE helper are
recorded in the canonical authority marker; any readable holder still wins and
retains the root.

This is a non-adversarial single-user maintenance boundary. It does not defend
against an actively malicious same-UID process racing raw paths, refs, or
SQLite; queued `SCM_RIGHTS`; or in-flight AIO, io_uring, or other kernel-held
references absent from the inspected inventories. Stop competing maintenance
before apply.

## Authority and queue boundary

The four RPCs—`ListSourceWorktreeCohorts`, `AuditSourceWorktreeCohort`,
`ApplySourceWorktreeCohort`, and `GetSourceWorktreeSettlementRun`—are
operator-only. They are absent from both agent allowlists, `rsi-rpc agent`,
provider-native tools, preambles, and agent skills. Session-attributed callers
are default-denied even for listing or receipt lookup.

This implementation is the reviewed realization of Issue 69 Slice 69B for the
ancestor-integrated safe subset. It prevents a duplicate source-purge queue,
but it does not silently dispose of the retained queue: clean non-ancestors,
ambiguous targets, dirty or active roots, and all other refused cases remain
preserved for separately reviewed adjudication. Slice 69A target-cache reclaim
remains independent and unchanged; it removes authenticated `target` cache
content only and never removes source worktrees or refs. See
[Sandbox storage](sandbox-storage.md).

This settlement path does not run automatically at a disk watermark. The
periodic 69A worker bounds regenerable build caches; source-root and branch
deletion remains audit-and-phrase gated because retained non-ancestor or dirty
work can be unique. Preventing source-root growth therefore currently requires
periodic operator settlement of the mechanically proved ancestor subset. An
automatic source-retention policy is a separate change and must preserve the
same proof boundary rather than converting age or pressure into deletion
authority.

## Overlay keys

| Key | Action |
|---|---|
| `j` / `k`, `Down` / `Up` | Select a daemon-discovered repository cohort |
| `Enter` | Audit the selected cohort; submit exact text while authorizing |
| `A` | Open empty authorization input for a fresh applyable audit |
| character keys / `Backspace` | Edit authorization text |
| `r` | Refresh the current durable receipt |
| `J` / `K`, `PageDown` / `PageUp` | Scroll by eight lines |
| `Esc` | Cancel authorization input, otherwise close the overlay |
| `q` | Close the overlay when authorization input is inactive |

## Automatic and agent settlement

Current Execute-mode PM and portfolio seats with operator-granted
`storage_control` can list, audit and apply through the separate Agent* verbs.
A repository must have a covered owner, participant or retained receipt owner.
Roots are scoped individually: every foreign owner, participant or receipt
owner, regardless of status, and unknown ownership are retained with typed
dispositions. The digest binds the resulting eligible set; authority is rechecked
before intent and effects. Covered terminal owners, including
Deleted, may settle only after the same live-consumer, custody and Git proofs.
Operator and automatic settlement may reclaim foreign terminal orphaned roots
through those proofs. Deleted Sessions keep their status after custody is tombstoned. Full presets include this capability. The operator methods
and runtime settings stay default-denied to tokened agents.

The local daemon runs automatic settlement every five minutes when
`source_worktree_auto_settlement_enabled` is true and sandbox filesystem free
space is below `disk_pressure_min_free_gb`. Defaults are on and 100 GB, with
1024³ bytes per GB, matching the governor. Settings > Sandbox storage and
operator `GetDaemonConfig`/`UpdateDaemonConfig` edit both settings live; the
configured threshold is bounded to 1..=4096 GB. Its effective value is capped
at 25% of the filesystem, using the launch-admission scaling helper: a 192 GB
laptop uses 48 GB at the default, while a 775 GB hub uses 100 GB. A lower
configured threshold still applies. Pool trimming has separate controls. The
same local loop runs on the hub and satellite, independently.
A failed or unsupported disk sample retains work.

If pressure resolves or the setting is disabled mid-run, remaining untouched
items are marked unattempted immediately, retaining their roots and releasing
the journal fence. A later low-disk tick can audit a new plan. For transient
interruptions that leave an open automatic intent, an enabled low-disk tick
resumes its original key, digest and attribution without adding roots. It never
resumes agent/operator intents through that loop.

Automatic runs audit each repository and call the same digest/phrase-checked,
idempotent, ancestor-only apply. The setting and pressure are rechecked before
intent and under the Store lock before each item removal. Dirty, divergent,
active and dependent work remains protected, and a non-applyable audit creates
no run. Each actual run retains its intent, item outcomes and receipt. V167
adds immutable attribution in the same transaction: operator, agent (with its
authenticated Session id), or automatic daemon. Older receipts remain
unattributed; exact replay preserves the first initiator.

The settlement overlay lists the latest 20 runs and their origins and outcomes;
`r` refreshes the history along with the selected receipt. Operator-only
`ListSourceWorktreeSettlementRuns` accepts `{ "limit": 20 }` (default 20,
1..=50); `GetSourceWorktreeSettlementRun` reads the full receipt by run id.
These methods remain outside the agent/native/CLI catalogs.

Startup recovery resumes only exact retained intents under their existing
fresh Git/custody/runtime proof. Before an untouched automatic item starts an
effect, it requires the enabled setting and disk pressure; an untouched agent
item requires its initiating Session's current storage authority. Failed checks
mark those items unattempted and retain their roots and branches. Items already
moved or removed retain their exact recovery obligation. Recovery never expands
the plan or enumerates new work.

A Git read error probing one untouched recovery item closes that item as
unattempted and recovery continues. Ambiguous prior effects retain an explicit
recovery fence. Agent authority loss closes the remaining untouched items in
the current apply run without waiting for daemon restart.

Per-item refusals (including `runtime_state_contended`) do not stop later roots
in apply or startup recovery. That code means a runtime-map read lock or Store
lock stayed unavailable after a bounded one-second wait before the effect,
not proof of an active owner. A proven untouched intent gets a retryable refusal;
an item with possible prior effects stays pending for exact replay. Contention
alone never makes an item `recovery_required`. Only loss of initiating authority
or automatic-settlement policy stops the remaining plan.
Unexpected failures close proven untouched intents as `unattempted`, retaining
the actual error in the observation. Only items whose prior effects cannot be
excluded get immutable recovery fences. A custody/Store lock-order timeout
leaves intents pending for the next recovery pass. Runs get `finished_at` only
when all items are terminal; uncertain effects never release custody.
Startup recovery selects open partial runs by their nonterminal journal items,
including runs left open by older daemons after an earlier refusal.

Once apply starts its daemon task, that task owns the spawn guards, Git effects,
journal finalization and runtime projection through client disconnects. Apply
and recovery acquire the provider-cwd exclusion separately for each root's final
runtime check and effects, then release it before advancing. Queued launches,
rotations and successions can acquire cwd admission between roots. A cancelled
client cannot release the exclusion while that root's effects are still running.
Lock order (#2081): `provider-cwd admission -> Store -> custody root -> repository
mutex`. Provider launch and archive cleanup hold cwd admission while they wait for
the repository mutex, so apply takes the exclusion first, outside the mutex, and
then re-enters the repository mutex for that one root (setup and the database
tail are separate mutex entries). Both are released between roots, so an
overlapping launch or archive cleanup is admitted there and every root still
settles. A root that cannot win the exclusion within 120 s is released as
`runtime_state_contended` (proven untouched) or stays pending for replay.
Post-readiness recovery uses the same per-root lock order for Git effects;
its database-only retry tail remains a separate, bounded mutex entry.
The exclusion is always the one daemon-wide barrier in production. A unit test
takes a lock of its own instead (#274): the lock a test scope selected, else the
lock of the Tokio runtime the code runs on (a `SessionManager` test's ticker,
recovery and apply, and the test driving them), else the thread's. The barrier
is one lock per process, so a test that held it through a holder proof or a pause
made another test's one-second wait surrender, and a result depended on which
tests shared the process. A test that checks settlement against a launch takes
the launch side of the same lock (`test_launch_cwd_admission`).
One test is the exception (#283): `settlement_surrenders_while_a_launch_holds_the_real_barrier_and_settles_after`
runs a replay on the real daemon-wide barrier (`with_real_cwd_barrier`, serialised
so no second test takes it) while it holds the launch side
(`acquire_provider_cwd_admission`, the call every launch path makes). Settlement
surrenders and records `settlement_surrendered: runtime_state_contended`, then
settles after the launch releases. It is the only test that proves settlement and
launch share the production barrier.
The shared agent RPC client gives `AgentApplySourceWorktreeCohort` a 30-minute
budget (other verbs keep 30 seconds). It remains a synchronous receipt response;
an exact idempotent replay returns the retained run. A daemon crash or unavailable
Store can still leave durable intents for startup recovery; a disconnected client
must use the receipt rather than infer success from its connection state.

Transient database-finalization failures retain `branch_removed` and their typed
retry reason. The driver retries that exact database tail once after attempting
the cohort. If the database still cannot settle it, the intent remains replayable
by startup recovery rather than becoming an inspection-only terminal fence.
