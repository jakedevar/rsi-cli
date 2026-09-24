---
name: rsi-agent-control
description: Full contract for driving the rsi daemon from inside an rsi-managed agent session — the closed 30-verb Agent* RPC surface including guarded V97 Issue control, typed manager preflight, token lifecycle, native rsi_control tools, retry policy, and spawn-directive fallback. Use when spawning, transferring a master baton, monitoring, messaging, managing lead-scoped Issues, scheduling wakes, creating attributed issues, invoking rsi-rpc, or debugging agent authority errors.
---

# rsi agent control


An rsi-managed provider session reaches the daemon through the `rsi-rpc` CLI
(`crates/rsi-common/src/bin/rsi-rpc.rs`). Two pieces make this self-describing:

- **Discovery.** `rsi-rpc agent` (alias `rsi-rpc list-agent-verbs`) prints the
  agent control surface and exits 0. It advertises ONLY the closed 30 `Agent*` verbs
  above — `AgentSpawnChild`, `AgentReserveSuccessor`, `AgentGetProgress`, `AgentSendMessage`,
  `AgentGetStatus`, `AgentHalt`, `AgentContinueChild`, `AgentArchiveChild`, `AgentScheduleWake`, `AgentCreateIssue`,
  `AgentListIssues`, `AgentGetIssue`, `AgentUpdateIssue`,
  `AgentUpdateIssueStatus`, `AgentArchiveIssue`, `AgentRestoreIssue`, and
  `AgentListIssueEvents`, `AgentManagerProgress`, `AgentManagerInbox`,
  `AgentManagerSend`, `AgentManagerReply`, `AgentManagerNotify`,
  `AgentManagerInspect`, `AgentManagerUpdate`, `AgentSubmitReviewReceipt`,
  `AgentManagerControl`, `AgentManagerPrepareControl`,
  `AgentManagerCommitPreparedControl`, `AgentManagerGetAction`, and
  `AgentManagerWorkView` —
  each with a one-line description, plus the invocation form
  (`rsi-rpc <Verb> [--params JSON]`) and the token convention. The generic RPC
  passthrough is never advertised to an agent; an agent should invoke no other
  method.
- **Request-schema discovery.** `rsi-rpc <AgentVerb> --schema` prints one
  deterministic, versioned JSON envelope for the exact, case-sensitive verb and
  exits without resolving a socket, reading the authority token, or contacting
  the daemon. It is closed to the same 29 verbs. The schema is the supported
  request shape only: it omits caller and authority identities and does not
  replace daemon-side parsing, authorization, or runtime validation.
- **Identity env.** The daemon stamps each provider process with
  `RSI_SESSION_ID`, `RSI_SOCKET`, and `RSI_SESSION_TOKEN` (Codex CLI
  `build_cmd`, CodexAppServer `launch`; mirrors the existing Claude/Antigravity
  stamping). The authority token rides `$RSI_SESSION_TOKEN` as a transport-only
  credential and must NEVER be typed into `--params`.

On the Codex CLI, a compact agent-discovery nudge (the same closed 30 verbs + token
convention) is prepended to the process stdin on the first turn only — never on
resume, and never into the stored user event.

**Token lifecycle (A6) — every establishment path re-mints.** `RSI_SESSION_TOKEN`
is (re-)minted and registered at every session-establishment site through the
single shared `remint_agent_token` helper (`crates/rsid/src/session/mod.rs`):
fresh launch, `ContinueSession` on a terminal session, handoff-write resume, and
rotation-child spawn. Continued, rotated, and resume-woken sessions therefore
carry a WORKING token, not a stale or missing one. Re-mint is supersession: the
prior token is revoked as part of the same helper call, and a superseded token
fails with `-32602 agent_verb_unknown_session_token`. On rotation the parent's
tokens are revoked only on the SUCCESS leg — a failed child spawn leaves the
parent's authority intact. Lead promotion mints nothing (`SetEpicLead` is authz
bookkeeping): the recipe stays promote-then-rotate, and the rotation supplies
the working token. The registry is in-memory — after a daemon restart only
sessions (re-)established since the restart hold live tokens. The retry path
(`launch_retry`) deliberately carries no token in its config: it re-enters
`launch_session`, which mints fresh.

**Retry policy (A9) — automatic retries are fail-closed for every kind.**
`kind_default_max_retries` in `crates/rsid/src/session/retry_policy.rs` returns
0 for every `SessionKind`, including worker kinds (`TaskRabbit`, `Task`, `Bug`,
`Feature`, `Refactor`, `Research`) — the daemon's live `retry_max_default` is
NOT applied as an automatic per-kind default. A positive retry budget only
comes from an explicit persisted/session retry policy or an explicit launch
`max_retries`; `session_retries_allowed` gates that path and still refuses
Group/Epic containers. `retry_enabled=false` is a live kill-switch for new
arming and restart re-queueing regardless of any session-level budget.
Cancelling a retry via continue, interrupt, `AgentHalt`, or `CancelRetry`
persists retry exhaustion so daemon restart cannot resurrect the same row.

**Agent verb catalog (the ONLY agent-facing verbs — default-deny).** These twenty-nine
`Agent*` RPC verbs are the entire authorized surface; every other method is
denied to an agent. Authority is enforced server-side against the caller session
resolved from `$RSI_SESSION_TOKEN` — never from anything the agent supplies.

- `AgentSpawnChild` — spawn a child agent session under the caller. Caller must
  be a leaf kind AND the lead of its owning Epic (the first Epic ascending its
  parent chain must have `lead_session_id` = the caller); a non-lead or
  Epic-less caller is rejected `NotLead` ("emitter is not the Epic lead") —
  promote via `SetEpicLead`, then retry. The spawning session is bound
  server-side (agent cannot forge the parent). Routes through the same spawn
  coordinator / rate-limit guard as the directive path. A stable
  `idempotency_key` is required. The result immediately contains durable
  `spawn_request_id`, `child_session_id`, normalized optional `agent_role`, and
  the server-reserved positive `epic_spawn_ordinal`; an exact retry returns
  that same identity, while changed content under the same key fails closed.
  `agent_role` is display-only: it is trimmed, whitespace-collapsed,
  case-preserving, control-free, and at most 64 UTF-8 bytes. Caller, Epic,
  parent, lead, predecessor, token, and ordinal inputs remain server-bound and
  absent. Committed ordinal gaps are permanent; the current lead's displayed
  `Demiurge`/`0` is virtual. Display role is separate from SessionKind and the
  SessionKind-derived `$CLAUDE_AGENT_ROLE`. Successful
  durable launch automatically arms the owner's terminal watch. Optional
  `provider` selects any supported child backend; omit it to retain the
  caller's provider/model/effort defaults. For a different provider, use a
  model and effort valid for that target. With no explicit model, the target
  provider's native default is used rather than a source-provider project
  default.
- `AgentReserveSuccessor` — reserve one daemon-authored same-Epic successor for
  authority-preserving master turnover. The caller must be the authenticated
  owning-Epic lead; caller, Epic, predecessor, successor, reservation, launch,
  and token identities are daemon-bound and absent from the request. The
  predecessor remains lead until the successor provider and token are
  established and the final lead-generation CAS commits. Exact retries return
  the original reservation and successor; changed or concurrent requests fail
  closed. Ordinary `Fresh` remains a separate non-authority-preserving mode.
- `AgentGetProgress` — read one UUID-sorted SQLite snapshot across the caller's
  authorized child cohort. It returns status counts plus per-child rotation-tip
  event cursors, freshness, watch state, and message counts. Each cursor's
  `lineage_tip_id`, `event_sequence`, and `custody_generation` are exactly the
  `AgentContinueChild` staleness-fence tuple; `custody_generation` is omitted
  when the tip has no sandbox custody, which is itself the value to expect. A sandboxed
  logical child may also carry `base_commit`, the immutable source commit
  recorded for its custody root; it stays fixed when the cursor moves through
  rotation. Review that child with `git diff <base_commit>..<child-ref>` rather
  than against a newer master tip. An absent field means no durable sandbox
  base is available and must never be inferred from the current master tip.
  Omit
  `session_ids` for the full cohort or supply an authorized subdivision. More
  than 256 rows is rejected with a typed subdivision error; rows are never
  truncated.
- `AgentSendMessage` — enqueue durable owner-to-child mail for the caller's own
  reserved/direct child or a child of an Epic it leads. Sender identity is
  server-bound, a stable `idempotency_key` is required, and delivery never
  interrupts the child's active turn.
- `AgentGetStatus` — read status of the caller or a session it is authorized to
  observe (self, a direct child, a child of an Epic the caller leads, or a
  session in the current manager's live scope).
- `AgentHalt` — interrupt a session under the caller's authority (self, direct
  child, or a child of an Epic the caller leads). Omit the target to halt self.
- `AgentContinueChild` — continue an exact child with a new prompt (a direct
  child, or a child of an Epic the caller leads). Never self: use
  `AgentScheduleWake` with `mode:"resume"` to continue yourself. This is an
  authority wrapper over the daemon's existing continuation engine, so terminal
  gating, single-flight, and custody re-auth are unchanged.
- `AgentArchiveChild` — archive a terminal child using an observed continuation
  cursor. Current Epic lead only. This is RPC-only; supply the target, tip,
  event sequence, and custody generation when present.
- Manager reach: the current appointed manager may also target a leaf in its
  live scope with `AgentSendMessage`, `AgentHalt` and `AgentContinueChild` when
  the operator granted V2 `SessionControl` in Execute mode, with no project or
  Epic pause and no pending question, approval or operator pause on the target
  (`manager_v2_capability_denied`, `manager_v2_paused`,
  `manager_v2_human_or_recovery_owner`, `manager_v2_leaf_required`). Status,
  named-ID progress and terminal watches need only the live scope.

  You MUST supply the observed continuation cursor as an optimistic staleness fence —
  `expected_tip_session_id`, `expected_event_sequence`, and (when the target
  has one) `expected_custody_generation`. Take all three straight from an
  `AgentGetProgress` snapshot; they are the same values it already computes.
  `Session` has no `row_version`, so never send `expected_row_version`.

  This check is not atomic with dispatch and is not an idempotency key.
  Concurrent or rapid sequential requests can both be delivered before either
  query event advances the cursor, and a query-event persistence failure can
  leave the cursor reusable. Treat an accepted continuation as delivered; the
  receipt's `observed` cursor is the pre-continuation witness and must not be
  replayed. Bounds failures use `agent_continue_invalid_request`. A stale refusal carries the observed cursor
  as a witness to inspect before making a new continuation decision. Continuing
  a running child interrupts its active turn through the existing engine. A
  resolved `CodexAppServer` tip is refused `agent_continue_provider_unsupported`
  because it allocates a fresh session id on continue; a logical AppServer root
  already rotated to an ordinary Codex tip is continued at that tip. Reserve a
  successor when the effective tip is still AppServer. A dirty sandbox is never a refusal — dirty work is work. On success
  the caller's terminal watch is re-armed against the logical child, so the
  no-idle invariant survives the restart. RPC-only: there is no
  `rsi_control_continue_child` native tool in slice 1.
- `AgentScheduleWake` — schedule a future wake/callback. `mode` is required;
  omission and unknown values fail closed without creating a row. Use
  `mode:"resume"` for a same-session continuation and `mode:"fresh"` only for
  a deliberate successor after the origin is terminal. `wake_session_id`
  (resume target) is bound to the caller's session server-side and is never in
  the tool's JSON schema, so the agent can neither supply nor spoof it.
  `mode:"on_terminal"` plus `watch_session_id` (A8) arms a daemon-owned
  terminal watch on a watched SUBJECT scoped exactly like `AgentGetStatus`
  targets (self-watch rejected); the wake target remains caller-bound.
  `mode:"program_guard"` is the idempotent master-orchestrate registration
  path: supply only `message` plus that mode. The daemon rejects timing or
  recurrence fields, derives the UUIDv5 row from the transport-bound caller,
  and persists a far-future one-shot same-session Resume sentinel. The returned
  id is program identity, not a continuation wake id.
- `AgentCreateIssue` — create a durable attributed local issue follow-up. Only
  title/body/priority/labels/assignee/idempotency_key are accepted; the caller
  and creator are server-bound. Matching replay returns the original row;
  changed content fails closed. Prefer `--params @file` for multiline bodies.
- `AgentListIssues`, `AgentGetIssue`, `AgentUpdateIssue`,
  `AgentUpdateIssueStatus`, `AgentArchiveIssue`, `AgentRestoreIssue`, and
  `AgentListIssueEvents` — project-wide local Issue control. These require the
  caller to be the current persisted lead of exactly one legal owning Epic,
  or the current appointed manager (committed lineage tip of the appointment)
  holding the V2 `IssueCoordinate` grant for the current scope version. A
  manager mutation is audited with actor `manager` and no owning Epic (V125).
  Scope is derived from that Epic's (or manager's) project; no request accepts project, actor,
  Epic, lead, generation, or token fields. Mutations require a positive
  `expected_row_version` and NUL-free 1–128 byte `idempotency_key`; exact
  replay returns the immutable stored event/result before stale checks.
  `AgentUpdateIssueStatus` is the one status verb: Open→InProgress/Closed/
  Cancelled, InProgress→Open/Closed/Cancelled, and terminal→Open are allowed.
  Archive is terminal-only; restore preserves terminal status. History accepts
  `after_sequence` plus a 1–256 limit. Ordinary workers retain create-only
  follow-up access and are denied every one of these project-wide operations.

- `AgentManagerProgress` — bounded progress for the operator-appointed current
  manager across its current project scope. Request `{}` for the first page;
  follow `next_after_epic_id` with `after_epic_id` until null. Optional `limit` is
  1–64 (default 32). Whole-project and Group scopes include future Epics; explicit
  selection narrows coverage. Appointment changes remain operator-only.
- `AgentManagerInbox` — durable, paginated manager/lead conversation retrieval.
  Request `{"after_sequence":0,"limit":32}`; optional `request_id` selects an
  exchange. Follow `next_after_sequence` until null and retain the last sequence.
- `AgentManagerSend` — manager-to-current-Epic-lead request; accepts only
  `epic_id`, `message`, `idempotency_key`. The receipt's `message_id` identifies
  the request; persistence is not proof that work was accepted or completed.
- `AgentManagerReply` — current managed lead replies to a recorded `request_id`,
  with `message` and `idempotency_key`. A request UUID grants no authority by
  itself. Live scope and current lead checks apply to reads, replies, and wakes.
- `AgentManagerNotify` — current managed lead sends one unsolicited notice to the
  current appointed manager; accepts only `message` and `idempotency_key` (Epic,
  manager and scope are daemon-derived). Exact replay returns the original receipt
  only within the same scope version; after a rescope, or with changed content under
  the same key, it fails `manager_idempotency_conflict`. At most 128 unsettled
  notices per Epic (`manager_pending_notice_limit`). A notice is not a request,
  reply target, approval or acceptance.

- `AgentManagerInspect` — bounded, cursor-paged workers, work, requests, decisions,
  topology, resources, action outcomes, events, archive and health. `{}` selects Overview; optional
  `section`, `epic_id`, `cursor` and `limit` (1–64, default 32) select a page. Follow
  the returned cursor; partial traversal is not a complete feature denominator.
- `AgentManagerUpdate` — one typed coordination ledger change. Required `fence`
  contains observed positive `scope_version`/`policy_version`; also supply
  `idempotency_key` and `change` tagged by `update`: `work`, `stage`, `dependency`,
  `ownership`, `migration`, `accept`, `integration`, `request`, `decision`, or
  `handoff`. Mutations use the current record version; zero is for a new record.
  Only the addressed current lead acknowledges its own request lifecycle.

For `rsi-rolling-land`, claim each changed registered hot file with an active
exclusive ownership record whose `domain` is the exact repository-relative
path and whose `files` contains that same path. The claim's `work_key` must
match the accepted source's live Work. A category domain such as `rpc-catalog`
does not satisfy the landing gate for `crates/rsid/src/rpc.rs`.

- `AgentSubmitReviewReceipt` — the live assigned reviewer submits one immutable
  exact-source review receipt for a DB-native review assignment. Reviewer identity,
  invocation and custody are daemon-bound, never supplied. Read the request shape with
  `rsi-rpc AgentSubmitReviewReceipt --schema`; create no review artifact or evidence
  commit for that assignment.
- `AgentManagerControl` — queue an explicitly granted bounded action. Required
  `fence`, `idempotency_key` and `operation` tagged by `action`: `resume_lead`,
  `pause_lead`, `retry_lead`, `replace_lead`, `create_container`, `update_container`,
  `archive_container`, `delete_container`, `restore_container`, `create_session`,
  `assign_lead`, `archive_session`, `restore_session`, `update_session`, or
  `succeed_manager`. Lead actions carry the exact observed lead/generation/event/
  custody fence; container edits carry their observed update timestamp. Session
  creation never implies lead assignment. Inspect the operation receipt for its
  actual execution result; queued or uncertain is not completed.
  Root `succeed_manager` requires explicit SelfSuccession and a parentless Standard
  current manager. Read the Overview `manager_control` epoch/custody observation,
  supply `expected`, `launch`, and committed `handoff` (source_commit, relative_path,
  blob_oid). Receive the queued receipt, then finish the predecessor turn; never
  launch a replacement yourself. The daemon allocates distinct custody, settles
  the predecessor and publishes authority only after provider establishment.
  Logical mail/work/policy and accounting persist; explicit succession charges
  session creation, not automatic recovery. Preserve all human/recovery owners.
- `AgentManagerPrepareControl` — preferred no-write preflight for
  `resume_lead`, `pause_lead`, `retry_lead`, `replace_lead`, `create_session`,
  or `assign_lead`. Supply semantic intent only: no manager, lead, event, or
  custody fence. The daemon derives live authority and returns a prepared ID,
  target digest, expiry, and bounded ready/blocked result without queueing an effect.
- `AgentManagerCommitPreparedControl` — commit an exact unexpired `prepared_id`
  plus `target_digest` and `idempotency_key`. The daemon atomically rechecks
  scope, policy, lead/event/custody state, runtime blockers, and capacity before
  queueing exactly one legacy action-journal operation. Exact replay returns the
  original receipt; changed content or stale authority fails closed.
- `AgentManagerGetAction` — read one durable action receipt within the
  authenticated current manager's project and stable logical-manager scope.
- `AgentManagerWorkView` — read-only projection for a session the current manager
  created (V2 `create_session`) and still manages: live work, `mine`, active file
  ownership (domain/mode/files), pause, and delivery state (queued/retrieved/delivered,
  `delivery_issue`) of unanswered manager→lead requests for your Epic. Request `{}` or
  `work_key`/`after_work_key`/`limit` (1–32); an exact `work_key` that is no longer
  live fails `manager_work_view_work_not_live`. Granted ownership in this view is
  authoritative; you do not need a relay turn. It grants no write, message or
  continuation authority.

The twelve manager tools have native names `rsi_control_manager_progress`,
`rsi_control_manager_inbox`, `rsi_control_manager_send`, `rsi_control_manager_reply`,
`rsi_control_manager_notify`, `rsi_control_manager_inspect`, `rsi_control_manager_update`, and
`rsi_control_manager_control`, `rsi_control_manager_prepare_control`,
`rsi_control_manager_commit_prepared_control`, `rsi_control_manager_get_action`, and
`rsi_control_manager_work_view` for Harness/AppServer. CLI providers use `rsi-rpc`. Read complete nested shapes
offline with `rsi-rpc <AgentVerb> --schema`. Unknown nested fields, including caller
identity and permission injection, are rejected. Exact replay requires identical
content; stale scope/policy/target errors require a new observation, not guessed
versions or targets.

V2 is a separate operator policy opt-in. Appointment, policy configuration,
operator state inspection and exact decision answers remain operator-only
(`GetHarnessManager`, `ConfigureHarnessManager`, `GetHarnessManagerPolicy`,
`ConfigureHarnessManagerPolicy`, `GetHarnessManagerState`,
`AnswerHarnessManagerDecision`); they have no native tools or agent schemas. A
title/prompt never appoints a manager. Scope changes fence old requests/actions.
Beyond `SessionControl` (scoped halt, continue and mail) and `IssueCoordinate`
(the seven guarded Issue controls), manager grants do not widen other Agent
verbs or expose ProgramRun methods.
Status/monitor/execute intent, operator/Epic pauses, creation quotas, concurrency,
provider/model/effort choices, retry count/delay/deadlines and spend caps are
persisted. Unfinished authorized work remains an obligation after a provider turn
ends. Respect existing recovery owners and operator pauses/approvals.

Queued, retrieved, accepted requests, effected actions, reported stage progress,
source-accepted work and independently verified integration are distinct. Missing
evidence remains unknown; source acceptance requires admitted gates at one exact
work/source revision. Only the operator can answer an exact decision/approval.
Inbox notices preserve busy turns and human gates; direct human instructions take
precedence. Completed AppServer sessions require the operator open/resume flow for
idle inbox notices because continuation replaces their principal. See
`docs/harness-manager.md` for Standard root setup (`:projects`, `:blank`,
`:manager appoint`), `:manager policy`, `:manager board`, `:manager decisions`,
policy bounds, evidence examples and the dogfood loop.

Registration is not authorization. The seven native Issue tools are registered
for every construction-bound Harness/CodexAppServer session so their roster is
stable across fresh launch and rotation. Each invocation still resolves the
persisted topology and permits execution only for the current lead of exactly
  one legal owning Epic or for the current `IssueCoordinate` manager. The tokened CLI likewise advertises all twenty-nine verbs to
every agent while enforcing authority at dispatch.

Concise lead-scoped request/result examples (`<issue>` is a canonical UUID):

| Verb | Example `--params` | Success shape |
|------|--------------------|---------------|
| `AgentListIssues` | `{"archive":"Active","limit":64}` | `{"issues":[...],"next_cursor":null}` |
| `AgentGetIssue` | `{"issue_id":"<issue>"}` | one `Issue` with its current `row_version` |
| `AgentUpdateIssue` | `{"issue_id":"<issue>","expected_row_version":3,"idempotency_key":"edit-v1","title":"Revised"}` | `{"issue":{...},"event":{...},"deduplicated":false}` |
| `AgentUpdateIssueStatus` | `{"issue_id":"<issue>","status":"Closed","expected_row_version":4,"idempotency_key":"close-v1"}` | the same mutation receipt shape |
| `AgentArchiveIssue` | `{"issue_id":"<issue>","expected_row_version":5,"idempotency_key":"archive-v1"}` | the same mutation receipt shape |
| `AgentRestoreIssue` | `{"issue_id":"<issue>","expected_row_version":6,"idempotency_key":"restore-v1"}` | the same mutation receipt shape; status stays terminal |
| `AgentListIssueEvents` | `{"issue_id":"<issue>","after_sequence":0,"limit":64}` | `{"events":[...],"next_after_sequence":null}` |

List/get/history are reads and have no CAS or idempotency key. Every mutation
uses the current positive `row_version`; an exact retry returns the original
immutable receipt with `deduplicated:true` even after later mutations or daemon
restart, while changed semantics under the same key return
`idempotency_conflict`. Safe failures have only
`{"code":"stale_version","expected_row_version":3,"actual_row_version":4,"next_action":"refresh the Issue and retry with its row_version"}`-shaped data;
unknown and cross-project IDs are both `not_found_in_scope`, and internal Store,
SQLite, topology, token, and path details are never exposed.

Malformed requests to these seven controls can additionally carry one optional
`validation` hint: a closed `missing_field`, `unknown_field`, `invalid_field`,
or `invalid_shape` class and, only when safe, one public top-level `field`.
Correct that field and retry. The hint never echoes a rejected value, unknown
key, raw serde diagnostic, path, or schema catalog.

Scoping in one line: an Epic-lead may act on its Epic's children; any leaf may
act on itself and its own direct children; the current manager with
SessionControl may act on Epic leads and descendants in its live scope.

**CLI advertise surface (P1).** `rsi-rpc agent` (alias `rsi-rpc list-agent-verbs`)
prints exactly the twenty-nine verbs above and exits 0. Invoke a verb as
`rsi-rpc <Verb> [--params JSON]`. The generic RPC passthrough is never advertised
to an agent. The authority token rides `$RSI_SESSION_TOKEN` (transport-only) and
must NEVER be typed into `--params`. Inspect a supported request shape offline
with `rsi-rpc <AgentVerb> --schema`; schema mode performs no RPC dispatch and
requires neither a daemon socket nor a token.

**Native in-process `rsi_control` tools (P2).** For the Harness (direct-API) and
CodexAppServer providers — where the shell tool scrubs `RSI_*` from the child
env, so the shell→`rsi-rpc` token bridge cannot reach the tokened verbs — the
daemon exposes native coordination tools (`rsi_control_spawn`,
`rsi_control_reserve_successor`,
`rsi_control_progress`, `rsi_control_send_message`, `rsi_control_status`,
`rsi_control_halt`, `rsi_control_program_guard`, `rsi_control_create_issue`, and
all seven guarded Issue tools, all twelve manager tools and
`rsi_control_submit_review_receipt`) plus, for Harness sessions, a
`schedule_wake` tool (CodexAppServer's dynamic tool set carries no
`schedule_wake`). Registration keeps one stable roster across fresh launch and
rotation; it does not authorize execution. The tools carry
the SAME guarded authority as the RPC verbs (both route through the single
guarded `AgentControlHandle` in `session::agent_verbs`, so scoping lives in one
place). Their advertised request schemas come from the same closed catalog as
the CLI. The generic Harness `schedule_wake` form and argument-free
`rsi_control_program_guard` convenience are native-only and are not additional
catalog verbs. The caller session id is bound at tool construction and kept out
of every tool's JSON schema — it cannot be supplied or spoofed. The `schedule_wake` tool
performs resume-wake (binds `wake_session_id` to the caller, same as
`AgentScheduleWake`) and, when the session carries the control handle, arms
per-master cap as the RPC verb. The argument-free
`rsi_control_program_guard` tool is present for both native providers and calls
the same deterministic registration service as `AgentScheduleWake` and
Harness `schedule_wake`, so all transports converge on one row. This tool set
is identical across a rotated session.

**Directive emit-and-detect FALLBACK.** When native tools and `rsi-rpc` are
unavailable, a lead session may still drive spawn/halt by emitting a
`<docregblock>/spawn_child …</docregblock>` (or `<docregblock>/halt</docregblock>`)
block in assistant text; the daemon detects and enqueues it
(`crates/rsid/src/session/types.rs` regexes, scanned in `session/monitor.rs`,
parsed by `session/spawn_directive.rs`). Prefer the native tools / `Agent*` verbs
when available; the directive path is the documented compatibility fallback and
is pinned by the `session::spawn_directive` detection regression tests.
Its `/spawn_child` header accepts the same optional `provider=<SessionProvider>`
selection as `AgentSpawnChild` and `rsi_control_spawn`, plus optional
`agent_role=<role>`. Use JSON/native transports for roles containing spaces.

**Doc-example conventions.** `GetSession` keys on **`session_id`** (not `id`;
`GetSessionParams`, `rpc.rs`). For multi-line spawn/wake payloads prefer
`--params @file` (`rsi-rpc` reads a `@path` argument as a JSON file) over inline
JSON, and never embed the session token in `--params`.
