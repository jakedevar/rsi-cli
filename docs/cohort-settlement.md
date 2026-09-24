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

The only positive proof in this release is `integrated_ancestor`. A dirty,
active, nonterminal, missing, symlinked, unregistered, shared, effectful,
misidentified, target-ambiguous, or non-ancestor root is retained. A clean
non-ancestor is still retained: patch equivalence and a plausible merge are not
preservation proofs.

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

The daemon resumes nonterminal journaled runs during startup, before ordinary
custody reconciliation. Recovery authenticates current Git and database state
against the recorded expected identities. It completes only effects it can
prove, records partial or recovery-required state otherwise, and does not
create a new run. Receipt-backed settlement archives are historical purge
records and cannot be restored with ordinary unarchive; preserve the receipt
for diagnosis. A Session named directly by a settlement receipt may be
archived or soft-deleted, but it is intentionally ineligible for permanent
hard purge because the immutable receipt is its destructive-maintenance audit
trail.

The recovery matrix is deliberately narrow:

| Durable and observed state | Recovery |
|---|---|
| No marker; original present, quarantine absent, source exact | Revalidate, move to quarantine, then prove and persist authority. |
| No marker; only quarantine present, source exact | Prove or narrowly repair the interrupted exact move, then persist authority. |
| Marker; quarantine present, source exact | Reprove identical destructive authority, compare-delete the ref, then remove quarantine. |
| Marker; quarantine present, source missing | Authenticate the marker and recorded commit, restore only the missing exact source ref, repair an exact detached sentinel when possible, then replay. Drift preserves the restored branch and enters `recovery_required`. |
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
