# Session Watches — daemon-owned terminal notify (A8)

A **session watch** lets a master agent session say: *"when session Y reaches a
notify-worthy state, resume me with a message saying so"* — and have the
**daemon** own that promise. The watch survives the master's process dying, the
master rotating, and the daemon restarting.

## Why this exists

Seven live incidents (2026-07-04) shared one failure class: every mechanism a
master used to watch a child — in-process background pollers, external hook
watchers, harness monitors — lived inside the master's `claude` process and
died with it. The durable half (children completing unobserved) always worked;
the *notification* half always died. Watches move the notification into rsid,
which never dies with a master.

## How it works (one paragraph)

An armed watch is a **persisted `scheduled_jobs` row** with
`wake_mode = on_terminal:<watched-uuid>` and the wake target bound to the
arming caller. Correctness never depends on the in-memory event bus: the
scheduler's due-poll (default 60s) re-reads the watched session's **database
row** on every attempt — this is the reconcile tick that also catches the two
terminal flips that bypass bus events entirely (boot-restore and
store-desync reconciliation). Bus events (`SessionStatusChanged`,
`SessionReconciled`, `SessionQuestionRaised`) only *accelerate* delivery from
minutes to seconds via `trigger_now`. Delivery resumes the master through the
same continue path as any other wake, so the resumed process carries a working
`RSI_SESSION_TOKEN` (A6).

## Arming a watch (agent surface)

`AgentScheduleWake` gains `mode:"on_terminal"`; the separate create-only
`AgentCreateIssue` verb does not alter watch behavior.

Durably launched `AgentSpawnChild` children are also armed automatically. The
spawn request retains the owner/child natural key, and daemon startup performs
a finite indexed reconciliation over launched requests to repair any missing
enabled watch. Manual and automatic arms share the same deduplicating service,
so they converge on one enabled row.

```bash
# watch.json
{
  "mode": "on_terminal",
  "watch_session_id": "<child-uuid>",
  "message": "optional context appended to the wake message"
}

rsi-rpc AgentScheduleWake --params @watch.json
```

- **Scope**: the watched *subject* is authorized exactly like `AgentGetStatus`
  targets — self is rejected (self-watch is meaningless), a direct child or a
  child of an Epic the caller leads is allowed, as is a session in the current
  appointed manager's live scope; anything else is denied before any row is
  written.
- **Wake target is never suppliable**: `wake_session_id` is bound server-side
  to the token-resolved caller; the params schema has no such field, and a
  smuggled one is ignored (regression-pinned).
- **Idempotent re-arm**: an enabled watch with the same (caller, watched)
  natural key returns the existing `job_id` with `"deduplicated": true`.
- **Cap**: at most 64 enabled watches per master (bounds self-DoS).
- Defaults: recurrence `EverySeconds(60)` (the reconcile cadence), name
  `rsi-watch`.

## Delivery semantics

| Watched session state | Watch behavior |
|---|---|
| Completed / Interrupted / Archived / Deleted | fire (a soft-deleted child is permanently non-progressing — suppressing would recreate the sleeps-forever trap) |
| Failed, retries exhausted or no live retry timer | fire, annotated `retry-eligible: …` |
| Failed, retry-eligible **with a live retry timer** | suppressed — the resurrection flips status non-terminal and the watch keeps waiting (A9 ordering) |
| WaitingApproval (child parked on a question) | fire, annotated `question-pending: true` — the master is the answerer |
| Starting / Running | not ready — recurring row stays armed |
| Row missing (purged) | abandoned — job disabled + system message |

- **Coalescing**: N fire-ready children sharing one master deliver **one**
  resume whose message lists every child:
  `[rsid-watch] <kind> <uuid8> "<title>" → <status> (…)` per line. No token or
  secrets ever appear in the message.
- **Harness-manager specialization**: manager watches keep the same bounded,
  event-accelerated transport but enumerate retained action/session/mail/decision
  subjects. Accepted continuation marks them delivered; only an authorized
  `AgentManagerInbox` retrieval settles them. Provider output alone does not.
  Multiple Epic watches may share one continuation while retaining every subject
  identity. Pre-V117 manager watches without durable subjects retain their legacy
  provider-output confirmation path after upgrade. See [Harness
  manager](harness-manager.md).
- **Busy master**: a resume rejected because the master is mid-turn leaves the
  recurring row armed — requeue-until-idle; an idle transition triggers
  immediate re-check. The wake structurally cannot be lost.
- **Rotation lineage**: delivery chases the master's `continued_from`
  successor chain (depth-capped) so a wake never resumes a superseded
  session. Plain `Resume`-mode wakes get the same tip-chase.
- **Fire-and-consume**: a delivered watch is disabled. A master that answers a
  child's question re-arms in the same turn if it wants the next transition.

## Human surface

Watches are ordinary scheduled-jobs rows: they appear in the existing jobs
list, and the manual disarm is the existing surface (`enabled=false` or
delete). Agents cannot mutate jobs — the tokened surface only arms.

## Restart and downgrade

- **Automatic child-watch repair**: startup arms a watch only when a launched,
  non-terminal child has never had an owner/child natural-key row. An existing
  disabled row is authoritative, whether it was consumed by delivery or
  suppressed by the operator; terminal historical children are never re-armed.
- **Daemon restart**: nothing special — the boot restore sweep flips dead
  children terminal *before* the scheduler's startup catch-up evaluates due
  rows, so armed watches fire on the first pass.
- **Binary downgrade (pre-A8)**: an older daemon parses the `on_terminal:`
  token as `Fresh` and would fresh-launch the watch message on its
  recurrence. Before (or immediately after) a downgrade, disarm watches:

  ```bash
  sqlite3 ~/.rsi/rsi.db "UPDATE scheduled_jobs SET enabled=0 WHERE wake_mode LIKE 'on_terminal%';"
  ```

## Limitations / follow-ups

- Harness (direct-API) sessions arm watches natively (A8.1): the in-process
  `schedule_wake` tool carries the same guarded authority handle as the RPC
  verb, so `mode:"on_terminal"` applies identical watched-subject scoping,
  natural-key dedup, and the per-master cap. The tool path has no arm-time
  scheduler nudge — the recurring row is the reconcile tick, so an
  already-terminal subject fires within ~60s rather than instantly.
  Claude/Codex/Pioneer CLI sessions arm via `rsi-rpc`. CodexAppServer receives
  the construction-bound native `rsi_control` coordination and Issue tools,
  including the argument-free program guard, but its dynamic tool set still
  carries no generic `schedule_wake` tool; that watch-arming gap remains a
  follow-up.
- No agent-side disarm mode (worst case: one informative extra wake); deferred.
- No dedicated TUI watch indicator yet (rows are visible in the jobs list);
  follow-up filed.

See also: [docs/agent-control.md](agent-control.md) (the canonical agent-control
and native-tool surface), `thoughts/shared/plans/2026-07-04-A8-native-watcher.md`
(design and decision record).
