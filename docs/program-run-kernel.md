# ProgramRun kernel

ProgramRun V1 is the daemon-owned, durable serial execution kernel introduced
by schema V78 and custody-hardened by the forward-only V79 migration. It coordinates one active run per Idea without depending on the
workflow or recursive execution engines. A provider Session is an execution
fact; its terminal status never commits output, passes a gate, or settles a run.

## Durable model

V78 adds seven append-preserving tables: runs, transitions, gate evaluations,
six-dimensional budgets, FIFO lock tickets, transactional outbox actions, and
attempt references. Runs snapshot a normalized template and its SHA-256 digest.
Transitions and gate evaluations are immutable; all seven tables reject delete.
Every semantic mutation uses one immediate SQLite transaction and appends one
transition plus one linked Idea event at the same resulting Idea version.

The closed lifecycle is `pending`, `ready`, `running`, `awaiting_gate`,
`retry_pending`, `blocked`, and the terminal states `settled`, `cancelled`, and
`failed`. Cursors advance only after explicit output commitment and all required
gates pass. A cursor with no gates advances or settles only during that explicit
commit.

## Replay and fencing

Creation and transition idempotency are resolved before stale-version checks.
An exact canonical replay returns the immutable projection produced by the
original transition even after later transitions; changed content fails as a
replay conflict. Semantic work claim and wake acknowledgement use dedicated
typed requests. Each request binds the exact action ID, daemon boot ID, claim
generation, claim run version, and claim lease generation in addition to the
durable run and Idea versions. The immediate semantic transaction validates
those witnesses, the active action, current run and Idea projections, locks,
controller identity/epoch, D03 grant incarnation, and A6 token before budget or
attempt mutation. The manager revalidates the current live incarnation, D03
grant, and A6 binding at each mutation while holding the active → Store → A6
lock order. Raw identity/epoch constructors remain private to the Store module;
production capabilities are derived from a live D03 grant and never appear in
RPC or provider tool schemas.

## Product budgets

Every run owns exactly six finite, positive counters:

- `productive_transitions`
- `work_attempts`
- `launch_retries`
- `revisions`
- `wake_reservations`
- `action_publication_retries`

Each charged edge first reserves its dimension and then consumes that exact
reservation at the documented semantic effect. A confirmed pre-effect work
failure releases its reservation; uncertain or post-effect failure never does.
Both reserve and consume/release increment the budget row version. Limits are
immutable in D05. Exhaustion blocks or fails the requested edge; an
operator unblock cannot enlarge a limit. Session retry defaults do not fund a
ProgramRun retry.

## Locks and actions

Templates request only the closed `idea_controller` and
`dangerous_mutation` conflict domains. The daemon derives lock keys, inserts
tickets in canonical order, caps queues, and grants an entire run's set only
when every ticket is FIFO-eligible. Lease expiry is eligibility, not ownership:
heartbeat, typed expiry recovery, grant, and reassignment use controller,
epoch, the daemon's actual boot UUID, and monotonically incremented lease
generation fences to prevent ABA. A run's canonical multi-key set is granted
atomically only when it is oldest on every key; requested queues are capped at
64 per key and 1,024 per project.

Each productive edge creates at most one deterministic outbox action. SQLite's
partial unique index permits only one reserved, claimed, or published action per
run. The dispatcher attempts at most 32 external effects per one-second tick,
with a 30-second claim lease. Reconciliation visits are separate: one tick
loads and processes at most 128 action rows: a 32-row quantum for each of the
four scheduler-relevant states. A versioned cursor in the existing
`daemon_settings` table stores one keyset position per state, ordered by
`not_before` and action ID. One CAS advances every processed state position;
each position wraps independently and survives dispatcher, manager, provider,
grant-incarnation, A6, and daemon-boot recreation. A crash before the cursor
CAS can repeat a prefix, but the durable action identity and downstream dedup
make that replay effect-safe.

Candidate selection is anchored on the action table rather than the live
Session portfolio. It performs one cursor read and at most two bounded keyset
reads per state (nine selection reads total), uses
`idx_idea_program_run_actions_due`, allocates one 128-entry vector, and performs
no Rust sort. The allocation bound assumes the schema-owned cursor value was
written by this implementation; an oversized locally corrupted
`daemon_settings` value is still read before JSON validation and remains a
documented LOW recovery risk. Cursor advancement is one immediate CAS
transaction. At most 128
selected current/due rows can attempt authority binding and an exact candidate
claim transaction; the external-effect budget remains independently fixed at
32. The in-memory A6 registry indexes both token-to-session and
session-to-current-token, so each bounded authority binding preserves exact
supersession fencing without scanning the live token portfolio. Future, failed,
removed, stale-controller, foreign-project/Idea,
provider-replaced, and unsupported rows can consume only their state's 32-row
visit quantum, never effect capacity, and its durable position rotates past
them. Every exact claim begins with the action primary key, has no nullable-
candidate branch or ordering, and applies the reloaded live D03 grant's
controller Session, epoch, project, and Idea before mutation. Current
acknowledgement uses the creating-transition primary key and the partial unique
active-action index; monotonic run versions and one action per creating
transition eliminate a latest-history scan while preserving all generation
fences.

For a stable state containing `N` scheduler-relevant action rows, its durable
position completes a cycle in `ceil(N / 32)` fully reconciliatory ticks. When
rows require external publication, the independent global effect cap can stop
the processed prefix after 32 attempts, but the finite stable prefix advances
on every successful cursor CAS and cannot be reset by recreation. Unchanged
published-reference or acknowledged-semantic failures cannot pin any state's
stable prefix. The all-reconciliation case advances up to 128 rows per tick.
Continuous insertion ahead of the durable cursor remains outside D05's
stable-set contract. The durable outbox resumes the same action through
claimed, published-unbound, published-bound, and acknowledged states. Wake
publication inserts or exactly replays an immutable one-time ScheduledJob whose
UUID equals the action UUID. Successful delivery is not complete until the
acknowledged action drives the exact-action semantic `WakeAcknowledged`
transition. That commit clears the action's current-claim witness, making the
acknowledgement immutable consumed history that can neither be rebound nor
returned by the bounded current-claim scan. A crash at an intervening boundary
recovers only the single acknowledged wake still active for a `retry_pending`
run. D05 deliberately has no work publisher; status reports
`await_work_publisher` until a later slice adds one through the publisher trait.

An unexpired claimed or published-unbound action is in flight and is not
republished by a safety tick. A published action with a durable external
reference is reconciled immediately without calling the publisher again.
Otherwise, expiry admits the original action to one generation-CAS reclaim
path. Every second or later external publication increments the action attempt
count and consumes one `action_publication_retries` unit. Within one exact grant
scope, the due scan performs only one additional bounded batch of local
exhaustion settlement, so exhausted rows cannot indefinitely hide later due
work. Attempt or retry-budget exhaustion
marks the action, and any still-reserved attempt, failed; status then exposes
only operator cancellation rather than another automatic external effect.

A confirmed pre-effect failure of a semantically claimed work action marks its
reserved attempt failed and releases only its still-reserved work budget. The
derived status then advertises the executable `operator_cancel_only` recovery,
not `commit_output`. Uncertain failures keep the reservation charged, and a
failure after a durable launch never refunds the consumed work budget. Exact
failure replay returns the original failed action even after later run progress;
changed failure content fails closed. Its durable private replay identity covers
the action, scrubbed error class/message, and the confirmed-pre-effect refund
classification. The typed action projection continues to expose the original
public error text. Exact replay is checked against the current run/Idea
controller authority before obsolete boot, claim-generation, or claim-expiry
witnesses, so it remains a no-write replay after Store reopen while stale or
wrong authority still fails closed.

## Recovery and operations

Startup fails closed if Session restoration fails. After successful Session
repair it restores live grants, runs bounded ProgramRun reconciliation, and
only then starts the dispatcher. Reconciliation pages are
ordered by `(updated_at,id)`, limited to 256 rows and 2,000 ms, and yield between
pages. Dry-run performs the same classification without a write. Unknown
publication and changed downstream replay retain the original action identity;
expired claims are reclaimed in place, published actions and at most one
semantically unsettled acknowledged wake are rebound to the current boot in
place, and consumed acknowledgements are excluded. Unsafe states are explicitly
quarantined. Recovery never manufactures replacement work. Operational status
includes Idea/run controller and epoch facts, required-gate projections, lock
queue depth/expiry eligibility, and publication timestamps/references. An
acknowledged wake is active only when its claim and creating-transition witnesses
match the exact current `retry_pending` run projection; acknowledged rows whose
semantic transaction cleared those witnesses remain history. Failed reserved
attempts and launched attempts are therefore classified from the current
attempt, never displaced by older acknowledged actions.

The operator-only RPCs are `CreateProgramRun`, `GetProgramRun`,
`ListProgramRuns`, `ListProgramRunTransitions`,
`GetProgramRunOperationalStatus`, `CancelProgramRun`,
`ResumeBlockedProgramRun`, and `ReconcileProgramRuns`. Session-attributed
callers are denied before Store access. Results omit canonical request/template
JSON, raw output/evidence, credentials, tokens, prompts, and transcripts.

## Migration and rollback

V78 requires an exact V77 source and migrates all tables, indexes, triggers,
foreign keys, fingerprint validation, and `user_version` in one immediate
transaction. V79 never rewrites that reviewed custody: it requires exact V78,
validates existing UUID/timestamp/state values, adds claim run/lease witnesses,
installs strict future-write guards, pins the exact catalog fingerprint, and
sets `user_version` last in the same immediate transaction.

### Legacy V78 baseline repair

Because V79 requires an *exact* V78 source, a database whose V78 baseline
diverged from the reviewed DDL can never satisfy the V79 gate: the migration
fails, rolls back, and the daemon exits *after* binding its socket, so clients
see only a broken pipe and the real error is invisible. This is not
hypothetical. `9b598e84` amended the already-shipped V78 DDL in place, adding
`DEFERRABLE INITIALLY DEFERRED` to transition foreign keys on
`idea_program_run_gates` and `idea_program_run_attempt_refs`. Every database
that had already reached V78 under the original DDL was therefore locked out of
V79 permanently, on every boot.

`Store::repair_legacy_d05_v78_baseline()` closes that trap. It is gated
`if version == 78` and runs *before* the `if version < 79` block, so V79 remains
byte-for-byte the reviewed migration. The repair is convergent rather than a
second target shape: it returns early when the baseline already matches
`D05_V78_SCHEMA_FINGERPRINT`, otherwise it rebuilds the two affected tables and
re-asserts that same fingerprint as a pre-commit post-condition, so it proves
its own convergence or aborts.

A table rebuild is the only available fix: foreign-key clause divergence is
visible only in `sqlite_master.sql` text, because `PRAGMA foreign_key_list` does
not expose deferrability, and there is no `ALTER`/`PRAGMA` path to a changed FK
clause. Each rebuild captures the table's index and trigger DDL from
`sqlite_master` `ORDER BY rowid` and replays it in that order — creation order
is load-bearing, since the catalog fingerprint hashes the per-table index
sequence. Rows are preserved through a scratch copy; the positional reinsert is
exact because the amendment changed foreign-key clauses only, never columns. The
whole repair is one transaction: atomic, idempotent, and a no-op on a canonical
V78 baseline.

Tests cover fresh
V79, copied V78/V77, a V76-to-V77-to-V78-to-V79 chain, all 15 durable
failpoints in each migration, negative malformed rows, reopen, integrity, FK, fingerprint, index,
and query-plan behavior, plus forward repair of a divergent legacy V78 baseline
(`d05_legacy_v78_baseline_repairs_forward_to_v79`) and non-interference with a
canonical one (`d05_canonical_v78_baseline_is_untouched_by_the_repair`). After V78/V79 is committed, rollback is roll-forward: operator exposure or the
dispatcher may be disabled while durable rows and schema history are retained.
