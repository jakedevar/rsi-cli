# Recovering from the V111 branched Epic lineage startup failure

Operator procedure. Read it end to end before running anything.

## Symptom

`rsid` refuses to start and the log ends with a line like:

```text
ERROR Database open/migration failed; daemon cannot start
  db=/home/jakedevar/.rsi/rsi.db
  error=V99 identity backfill found a branched Epic lineage
```

("V99" is a stale label inside a sealed migration; the code is the V112
identity backfill. A build carrying this fix appends the correct version and a
pointer to this document.)

## What causes it

The V112 migration models an Epic child lineage as linear — one
`continued_from` successor per predecessor. Five predecessors in this database
have two, each pair produced by two independent, committed authority paths:

- four cases where a context rotation completed *after* `AgentReserveSuccessor`
  had already committed a master turnover and archived the predecessor, leaving
  a second `continued_from` edge plus a `harness_manager_rotation_edges`
  receipt;
- one case where a predecessor served two separate lead terms, producing two
  committed `agent_successor_reservations` at different lead generations.

Both branches are real history, not corruption. The fix converges each branch
onto the reservation-backed lineage, re-roots the other successor, and records
*why* in two places so nothing is lost.

## What the fix does, and does not do

Does:

- sets `continued_from = NULL` on **exactly five** session rows (the
  non-canonical successor of each branch), at schema version 111, before V112
  runs;
- writes one `rotation_events` audit row per detachment
  (`phase = 'v111_branch_normalization'`, `event_type = 'lineage_detached'`)
  carrying the predecessor, the canonical successor, the receipt class and the
  decision rule;
- at schema version 114, records each detachment in a new immutable
  `session_lineage_detachments` table, reconstructed from the receipts
  themselves, so the manager subsystem can still prove the original
  attribution.

It also **repairs two things that are broken today**, which you will notice:

- The harness manager can resolve these lineages again. Before the fix,
  `manager_lineage_tip` refuses with `manager_lineage_ambiguous` on all five
  branches, which breaks manager identity resolution and manager mail routing.
- The manager's lineage *root* calculation stops silently stopping short. That
  changes the key of the manager's scheduled watch for the affected Epics, so
  **you will see new `harness_manager_watches` rows appear**. This is the
  repair, not a fault: the old key was computed from a truncated ancestry. The
  old rows are left in place — nothing is deleted — and are simply never
  matched again.

Does **not**:

- **stop new branches from forming.** The normalization is gated on schema
  version 111 and can never run again. That is a separate fix, described below.
- delete any row, from any table;
- retire, alter or remove any `harness_manager_rotation_edges` row, any
  `agent_successor_reservations` row, or any custody/audit record;
- change any session's `title`, `parent_id`, `status`, or Epic ordinal input;
- touch anything outside those five `continued_from` values.

## Why no sixth branch can form

The convergence above heals the five branches that already exist. A separate,
schema-free fix stops new ones, and it is the reason you should not expect to
run this procedure again.

A predecessor now publishes exactly **one** continuation. The
successor-reservation kernel (`AgentReserveSuccessor`) is authoritative for Epic
lead lineage, so it is context rotation that defers — the direction
`preflight_rotation_lead_transfer` already documented, extended to the case it
used to miss. The same predicate is proved at three points:

- when a rotation is admitted, before any provider, controller or custody
  effect;
- inside the transaction that inserts the rotation successor row, so the branch
  cannot be raced;
- when a new reservation is admitted, so one predecessor cannot reserve a second
  baton under a different idempotency key.

Operationally you may now see, in the daemon log:

```text
WARN Context rotation deferred by durable Epic lead fence
  parent_id=… error=agent_successor_predecessor_already_continued:<predecessor>:<reservation>
```

That is the fix working. The predecessor already handed its baton to a committed
successor, so rotating it again would launch a second agent to redo the same
handoff. The predecessor is left `Completed` and restorable — **not** archived —
and nothing is created or rolled back.

A reservation that settled as `failed` transferred no authority and does **not**
fence anything: ordinary context rotation of that predecessor stays available,
and it may reserve a fresh successor.

## Before you start

1. **Stop `rsid`.** Nothing below is safe against a running daemon; the daemon
   also holds a single-instance lease on the database file.

   ```bash
   pkill -x rsid          # or however you normally stop it
   pgrep -x rsid          # must print nothing
   ```

2. **Confirm the current state.**

   ```bash
   sqlite3 "file:$HOME/.rsi/rsi.db?mode=ro" "PRAGMA user_version;"
   ```

   Expect `111`. If it is already `114`, the fix has run — skip to
   *Confirming success*.

3. **Check disk.** The database is ~3.4 GB.

   ```bash
   ls -lh "$HOME/.rsi/rsi.db"
   df -h /
   ```

   You need at least ~7 GB free on `/` for a backup plus a dry-run copy.

## Step 1 — Back up the database

**Do not stage the backup in `/tmp`.** `/tmp` is a 27 GB tmpfs on this machine:
it is RAM-backed, it is volatile, and a 3.4 GB copy there competes with builds
and sandboxes. Put it on `/`, which has ~243 GB free.

```bash
mkdir -p "$HOME/rsi-db-backups"
STAMP=$(date -u +%Y%m%dT%H%M%SZ)
sqlite3 "file:$HOME/.rsi/rsi.db?mode=ro" ".backup '$HOME/rsi-db-backups/rsi-$STAMP.db'"
ls -lh "$HOME/rsi-db-backups/rsi-$STAMP.db"
```

`sqlite3 .backup` is used rather than `cp` because it produces a consistent
copy even if a WAL is present. Verify the copy before trusting it:

```bash
sqlite3 "file:$HOME/rsi-db-backups/rsi-$STAMP.db?mode=ro" "PRAGMA integrity_check;"
sqlite3 "file:$HOME/rsi-db-backups/rsi-$STAMP.db?mode=ro" "PRAGMA user_version;"
```

Expect `ok` and `111`. **Do not continue until both are correct.**

Record the "before" counts — you will compare against them afterwards:

```bash
sqlite3 "file:$HOME/rsi-db-backups/rsi-$STAMP.db?mode=ro" "
SELECT 'sessions', count(*) FROM sessions
UNION ALL SELECT 'reservations', count(*) FROM agent_successor_reservations
UNION ALL SELECT 'transitions', count(*) FROM agent_successor_transitions
UNION ALL SELECT 'rotation_edges', count(*) FROM harness_manager_rotation_edges
UNION ALL SELECT 'spawn_requests', count(*) FROM agent_spawn_requests
UNION ALL SELECT 'rotation_events', count(*) FROM rotation_events;
" | tee "$HOME/rsi-db-backups/counts-before-$STAMP.txt"
```

## Step 2 — Dry run against the backup copy, never the live file

Make a **second** copy for the dry run, in its own directory, and point a
daemon at it. `rsid` derives its database path from the socket path's parent
directory, so `RSI_DAEMON_SOCKET_PATH` fully isolates the run — the live
database and the live socket are never touched.

```bash
DRY="$HOME/rsi-db-backups/dryrun-$STAMP"
mkdir -p "$DRY"
cp "$HOME/rsi-db-backups/rsi-$STAMP.db" "$DRY/rsi.db"

RSI_DAEMON_SOCKET_PATH="$DRY/daemon.sock" rsid 2>&1 | tee "$DRY/rsid-dryrun.log"
```

Let it reach "Database opened", then stop it (Ctrl-C).

### Expected log lines

In order, roughly:

```text
INFO  Acquired daemon single-instance lease  lock=.../dryrun-.../... db=.../dryrun-.../rsi.db
INFO  V111 branch normalization detached a non-canonical Epic lineage successor
        detached=<uuid> predecessor=<uuid> canonical=<uuid>
        receipt=harness_manager_rotation_edge rule=R1
      ... one such line per detachment, 5 total ...
INFO  V111 branch normalization converged branched Epic lineages  detached=5
INFO  V112 migration complete: target and Operator Views catalogs converged
INFO  V114 migration complete: lineage detachment receipts installed  reconstructed=5
INFO  Database opened  db=.../dryrun-.../rsi.db
```

A second run of the same command must print **no** normalization lines at all —
the work is idempotent and the branch set is empty once converged.

### If the dry run fails

Read the error and stop. Do **not** run against the live database.

| error text | meaning | action |
|---|---|---|
| `cannot classify successor … no committed reservation and no manager rotation edge` | a branch exists whose successors carry no authority receipt | report it with the named UUIDs; this needs a code change, not an operator action |
| `found no committed reservation among N successors` | same class of problem | as above |
| `refuses to detach session … it carries an execution-origin claim` | a candidate has an execution-origin claim; the fix declines rather than fight a trigger | report it with the named UUID |
| `V112 requires one exact authenticated V111 source catalog` | the database's schema catalog does not match any known V111 shape | report the full message; the normalization deliberately made no change |
| `lost a race on session …` | the row changed mid-run | make sure no `rsid` is running and retry |

In every case the dry-run copy is left at `PRAGMA user_version = 111` with its
catalog unchanged, and the live database was never opened.

### Verify the dry run

```bash
sqlite3 "file:$DRY/rsi.db?mode=ro" "PRAGMA user_version;"                      # 114
sqlite3 "file:$DRY/rsi.db?mode=ro" "PRAGMA integrity_check;"                    # ok
sqlite3 "file:$DRY/rsi.db?mode=ro" "SELECT count(*) FROM pragma_foreign_key_check;"  # 0
sqlite3 "file:$DRY/rsi.db?mode=ro" "
SELECT count(*) FROM (
  SELECT child.continued_from
    FROM sessions child
    JOIN sessions epic ON epic.id = child.parent_id AND epic.session_kind = 'Epic'
   WHERE child.continued_from IS NOT NULL
   GROUP BY child.continued_from HAVING count(*) > 1);"                         # 0
sqlite3 "file:$DRY/rsi.db?mode=ro" "SELECT count(*) FROM session_lineage_detachments;"  # 5
```

And confirm nothing was lost — the counts must match `counts-before` exactly,
except `rotation_events`, which is up by 5:

```bash
sqlite3 "file:$DRY/rsi.db?mode=ro" "
SELECT 'sessions', count(*) FROM sessions
UNION ALL SELECT 'reservations', count(*) FROM agent_successor_reservations
UNION ALL SELECT 'transitions', count(*) FROM agent_successor_transitions
UNION ALL SELECT 'rotation_edges', count(*) FROM harness_manager_rotation_edges
UNION ALL SELECT 'spawn_requests', count(*) FROM agent_spawn_requests
UNION ALL SELECT 'rotation_events', count(*) FROM rotation_events;
" | diff - "$HOME/rsi-db-backups/counts-before-$STAMP.txt"
```

Only the `rotation_events` line may differ, and only by `+5`.
(`harness_manager_watches` is not in this list because the fix legitimately
adds rows to it — see *What the fix does*.)

Read the journal to see exactly what changed and why:

```bash
sqlite3 -line "file:$DRY/rsi.db?mode=ro" "
SELECT session_id, created_at, metadata
  FROM rotation_events
 WHERE phase = 'v111_branch_normalization'
 ORDER BY id;"
```

## Step 3 — Run against the live database

Only after Step 2 passed every check.

```bash
pgrep -x rsid            # must still print nothing
rsid                     # normal startup, normal socket
```

## Confirming success

```bash
sqlite3 "file:$HOME/.rsi/rsi.db?mode=ro" "PRAGMA user_version;"                       # 114
sqlite3 "file:$HOME/.rsi/rsi.db?mode=ro" "PRAGMA integrity_check;"                    # ok
sqlite3 "file:$HOME/.rsi/rsi.db?mode=ro" "SELECT count(*) FROM pragma_foreign_key_check;"  # 0
sqlite3 "file:$HOME/.rsi/rsi.db?mode=ro" "
SELECT count(*) FROM (
  SELECT child.continued_from
    FROM sessions child
    JOIN sessions epic ON epic.id = child.parent_id AND epic.session_kind = 'Epic'
   WHERE child.continued_from IS NOT NULL
   GROUP BY child.continued_from HAVING count(*) > 1);"                                # 0
sqlite3 "file:$HOME/.rsi/rsi.db?mode=ro" "SELECT count(*) FROM session_lineage_detachments;"  # 5
```

Expect new manager watch rows for the repaired lineages (see *What the fix
does*). The old ones remain and are harmless:

```bash
sqlite3 "file:$HOME/.rsi/rsi.db?mode=ro" "SELECT count(*) FROM harness_manager_watches;"
```

This count should be **greater than or equal to** the backup's — never lower.

Then start the TUI and confirm the session list renders, Epic children carry
ordinals, session titles are unchanged, and the harness manager board
(`:manager board`) opens without a `manager_v2_rotation_attribution_changed`
error.

## Rolling back

If anything is wrong after Step 3:

```bash
pkill -x rsid
pgrep -x rsid                                   # must print nothing
mv "$HOME/.rsi/rsi.db" "$HOME/rsi-db-backups/rsi-failed-$STAMP.db"
cp "$HOME/rsi-db-backups/rsi-$STAMP.db" "$HOME/.rsi/rsi.db"
sqlite3 "file:$HOME/.rsi/rsi.db?mode=ro" "PRAGMA user_version;"   # 111
sqlite3 "file:$HOME/.rsi/rsi.db?mode=ro" "PRAGMA integrity_check;" # ok
```

Keep `rsi-failed-$STAMP.db` — do not delete it. It is the evidence for the bug
report, along with the dry-run log.

`rsid` will refuse to start again at `user_version = 111` with the branch
error, which is the expected pre-fix state. Check out the pre-fix build, or
report the failure with the failed copy and the log.

## Cleaning up

Once the daemon has been healthy for a few days:

```bash
rm -rf "$HOME/rsi-db-backups/dryrun-$STAMP"
```

Keep `rsi-$STAMP.db` as long as you have the disk for it.

## Related

- Plan: `thoughts/shared/plans/2026-09-09-v111-branched-epic-lineage-convergence.md`
- Research: `thoughts/shared/research/2026-09-09-v111-branched-epic-lineage-convergence.md`
- Manager subsystem: `docs/harness-manager.md`
