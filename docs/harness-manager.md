# Harness manager

The harness manager is an ordinary session appointed by the operator to coordinate
feature Epics in one project. V1 supplies scoped progress and durable request/reply
mail. V2 adds a separate, explicit policy for persistent operating intent, bounded
control, work evidence and exact operator decisions. An existing appointment gains
no mutation capabilities automatically. Feature leads retain their own identities,
work and normal worker controls.

## Create and appoint a manager

> **Order matters, and saves are not idempotent (Issue eac83cfd).** Appoint
> once, then save the policy, then leave appointment alone. Every
> `:manager appoint` / `:manager scope` save, even an identical one, bumps the
> scope version and revokes the saved policy and all in-flight manager mail, so
> a re-appoint after a policy save silently undoes the grant. There is one
> manager seat per project: appointing a new session displaces the current
> manager. A policy save does not state what it granted, and Enter/Space cycles
> the preset row, so a save can land as Observe (mode `monitor`, no
> capabilities). Confirm the result by asking the manager to run
> `AgentManagerInspect {}`: the policy must name that session, read
> `revoked: false`, match the live scope version, and list the capabilities.
> Agents operating the seat: see the `rsi-project-manager` skill.

1. Run `:projects` and select the project. Create a Standard root session with
   `:blank` (or `:blank Coordinate this project's feature work`). Blank is the
   general-purpose Standard session flow, with no workflow or parent. Select an
   available provider/model in the normal launch popup. A Group or Epic is a
   container and cannot be the manager conversation.
2. Focus that session and run `:manager appoint`. New appointments default to
   **whole current project**, including future Epics and Groups. Enter saves.
   Space on a Group or Epic narrows scope to explicit selections; `a` restores
   whole-project coverage. A selected Group includes its current and future legal
   Epics, including when the Group is empty today. Combine Groups and individual
   Epics in one scope. `[x]` marks explicit selection; `[+]` marks inherited coverage.
   Deselect a Group before choosing only some of its Epics.
   The picker fetches legal Groups/Epics from the daemon in pages, including
   Groups never opened in navigation. Each row retains its own name and short ID;
   Epic rows also show their parent Group. Discovery failure reports an error.
3. `:manager` opens the current manager conversation, following its rotation
   lineage. It does not start or continue a stopped process. Use the normal
   continuation flow when needed. `:manager scope` edits selection;
   `:manager clear` saves an empty scope and revokes supervision.
4. For v1, ask the manager to read progress, send one readiness request per Epic,
   retrieve replies, and report evidence and unresolved blockers.
5. For v2, open `:manager policy`, select a permission preset and review its
   limits, then press `s` to save the complete draft. Every policy field is a
   top-level row.
   Appointment and policy are separate operator actions; a prompt or tool name
   grants no authority.

The manager must belong to the selected project. Setup needs no SQL. An unavailable
rotation tip is displayed through the persisted manager identity; appoint a valid
session to repair lineage. Reserved/Fresh candidates do not inherit appointment.
Existing explicit scopes survive upgrades; reopening the same appointment preserves
its selection. Scope edits revoke old mail and fence the old policy/actions. Reopening policy
retains a revoked grant's exact saved values and labels it revoked. Opening grants
nothing; explicitly saving regrants the displayed draft under the current scope.

## Policy (`:manager policy`)

Use `j`/`k`, arrows or Tab/Shift-Tab to move. Enter/Space toggles or cycles a choice,
or edits one scalar field. In an edit, Ctrl-U clears, Backspace deletes, Enter
applies, and Esc cancels that edit. Outside an edit, `s` saves the whole policy,
`r` reloads, and Esc/`q` closes. Save errors retain the draft and the observed
versions. After a stale error, reload and review the current policy before saving.
Unchanged retries reuse the same idempotency key.

The editor shows **Saved** and **Draft**, their effective permission profiles,
whether the draft has unsaved changes, and a **Usage** line: sessions created
against the created-session quota (the same count admission charges; DB-native
reviews are not counted), remaining allowance, and active sessions against the
saved active limit. Usage loads in the background from Inspect Resources when
Policy opens; `5` refreshes it (`refreshing` shows while a fetch is in flight).
After a save the header shows the new quota against the current count at once;
a reload into a changed appointment scope discards the previous scope's counts
and shows `scope changed · 5 refreshes`. Presets edit the draft; they do not save or
change appointment scope.

Rows are grouped under titled sections: **Authority · presets**, **Budgets**
(created session/container quotas, active limit, spend cap, provider limits),
**Launches** (catalog choices; *Exact launch tuples* expands raw
provider/model/effort rows), **Authority · mode and grants** (mode, operator
pause, each capability), **Scope** (Epic pauses, Group grants, root Group
creation), **Recovery** (attempts, retry delay, request timeout) and a dimmed
**Effective preview · read-only** summary. Labels are aligned; `*` marks a row
whose draft differs from the saved policy and shows `(saved X)`. The line under
the list says how Enter acts on the selected row, its valid range and what it
means. Numeric entry outside the range is refused with the range and the edit
stays open; a draft that daemon validation would refuse shows `✗ reason` on the
row, and `s` selects the first such row instead of sending the save. A
successful save states the effective preset, mode and grants the daemon
recorded (for example `Policy saved as Observe · mode Monitor · no write
grants`).

| Preset | Permissions |
| --- | --- |
| Observe | Monitor mode with no write grants. |
| Execute | Work/evidence coordination, lead control, session creation, lead assignment, integration evidence, explicit manager self-succession, scoped session control and project Issue coordination. |
| Full project control | Execute permissions plus topology management and Operator delegation (`operator_call` against the closed allowlist; see Operator delegation below). Whole-project scope can newly enable root Group creation; selected scope retains its existing explicit grants. |
| Custom | The exact effective policy, including legacy settings and individual overrides. |

Grants are operator-owned and never widened automatically. A policy saved as
Execute or Full project control before `SessionControl` and `IssueCoordinate`
existed keeps its exact grants and is shown as Custom; re-apply the preset (or
toggle the two grants) and save to give the manager those permissions. The
operator must re-apply the saved Full preset to make live `SessionControl` use
effective (2026-09-23 K1; `docs/harness-manager.md` policy history).

For a new policy, Execute/Full start with four active sessions, eight created
containers, 32 created sessions and three automatic recoveries. Empty launch
restrictions cover any valid choice. Saved and manually edited limits always
survive preset selection, including zero, provider ceilings, spend, pauses and
model restrictions. A zero that disables a profile operation is shown explicitly;
the policy remains valid Custom. **Use suggested allowances** changes only the
listed conflicting zero fields in the draft. For example, a saved recovery limit
of zero stays zero until explicitly edited; it does not prevent separately granted
manual succession when its creation and runtime gates pass. Existing additional
Group and root-creation grants remain visible as explicit overrides.
Routine capacity edits (created sessions, active limit) sit at the top of the
Budgets section.

| Form field | Meaning and bounds |
| --- | --- |
| Operating mode | `status`: answer on demand; `monitor`: persist observation and outstanding obligations; `execute`: permit due work under explicit grants and runtime gates. Provider turn completion does not clear operating intent. |
| Operator pause / Pause Epic | Persist a project-manager pause or selected scoped Epic pauses (at most 32). A derived Epic can be added by its Topology ID. Managers cannot clear operator-owned pauses. |
| Grant WorkPlan | Work definitions, stages, dependencies, ownership, migration reservations and coordination records within scope. |
| Grant LeadControl | Fenced resume/pause/retry/replacement of scoped leads through existing lifecycle and custody checks. |
| Grant Topology | Bounded Group/Epic creation and metadata/lifecycle operations under granted parents. |
| Grant SessionCreate / LeadAssign | Session creation and lead assignment are separate permissions and separate actions, including a vacant Epic's first child. |
| Grant SelfSuccession | Permit the current parentless Standard manager to reserve its own successor with a committed handoff and a permitted launch choice. Legacy policies do not gain this permission automatically. |
| Grant Integration | Record independently checked integration evidence at an exact target commit. This is not a shell or Git merge API. |
| Grant GitEffect | Advance an integration target ref (fast-forward or two-parent merge) through the guarded rolling merge engine. Distinct from `Integration` which records evidence only; a policy with `Integration` but not `GitEffect` cannot move a ref. Not included in Execute or Full presets; the agent-managed preset (Slice 4) grants it. |
| Grant SessionControl | Permits `AgentHalt`, `AgentContinueChild` and `AgentSendMessage` against Epic leads and their descendants inside the manager's live scope (Execute mode, not paused, no pending operator question, approval or pause on the target). Read-only `AgentGetStatus`/`AgentGetProgress`/terminal-watch reach over scoped sessions needs appointment scope only. Also permits the `archive_session`, `restore_session` and `update_session` housekeeping actions on one scoped leaf (see Scoped session housekeeping). Leads and workers never inherit it. |
| Grant IssueCoordinate | Permit the current appointed manager to use the guarded Issue controls project-wide, alongside the current owning-Epic lead path: the reads (`AgentListIssues`, `AgentGetIssue`, `AgentListIssueEvents`) and the four CAS mutations (`AgentUpdateIssue`, `AgentUpdateIssueStatus`, `AgentArchiveIssue`, `AgentRestoreIssue`). Each manager mutation is audited in `issue_events` with actor `manager` (its own session ID and request key, no owning Epic); the grant and live scope are rechecked inside the mutation transaction. This does not expose operator-only generic Issue RPC or ProgramRun, and revocation takes effect within the next Issue transaction. |
| Grant OperatorDelegation | Permit `AgentManagerControl` `operator_call` (Execute mode, not paused) to invoke one method from the closed, versioned `DELEGABLE_OPERATOR_METHODS` allowlist against a leaf in the manager's own project. See Operator delegation below. |
| Grant Group / Grant Group by ID | Select existing project Groups (at most 32). Enter an ID if it is not cached in the session list. |
| Allow root Group creation | Separate root-creation opt-in; also requires Topology. |
| Created container / session quota | Persistent creation ceilings: 0–64 containers and 0–1024 sessions. Defaults are zero. Policy edits do not reset scope usage. Manager workers, `retry_lead`, `replace_lead` and root successions consume the session quota. DB-native reviewer launches do not; they stay bounded by per-work review rounds and the active session limit. A blocked or revoked operation that never created its session is not charged (queued, running, failed and uncertain operations are). |
| Active session limit | 1–100, default 4; applies alongside existing provider/Model Control admission. |
| Provider active limits | Optional 1–64 ceiling per provider. Blank removes the override and uses the overall policy. |
| Allowed launches | Empty means any provider/model/effort is permitted by this policy. Adding entries restricts launches to those exact choices (up to 32). Use the provider/model catalog picker and that model's supported effort choices, or retain exact values under Exact launch tuples. Default effort means no explicit effort; it is not an effort wildcard. Enter on Remove deletes that draft choice; removing the last entry restores unrestricted choice. Use currently configured valid model/effort values. |
| Recovery attempt limit | 0–32 persisted attempts; default zero disables automatic recovery allowance. Existing retry owners and kill-switches still apply. |
| Retry delay | 1–86400 seconds, default 60. Waiting for a due time remains an outstanding obligation. |
| Request timeout | 30–604800 seconds, default 900. Expiration records a blocker/recovery obligation, never invented completion. |
| Spend cap (USD) | Optional positive finite amount; blank removes the cap. Unknown usage remains unknown; a finite cap cannot treat an unproved balance as zero. |

The provider rows are Claude, Codex, Pioneer, Local, Antigravity, CodexAppServer and
Harness. Catalog errors, cancelled selection and unavailable legacy choices
retain the complete draft. The picker keeps canonical model IDs, displays retained
identities, and fences stale discovery results. TUI-only custom endpoints cannot
be encoded in the existing provider/model tuple; their names are shown with that
limitation. The daemon-configured Local catalog remains available. No endpoint
credentials are stored in a permission restriction. Model selection and launch still pass the production provider validation,
stop controls, capacity, approval and sandbox custody gates. Replacement cannot
abandon a dirty predecessor or manufacture authority by copying a branch/path.
A successor interrupted after admission but before custody publication may be
nonrestorable. Recovery retains its allocation and settles that exact failed
attempt only after proving its process cohort gone; it does not infer permission
to reuse a worktree from its pathname. The receipt identifies the retained
allocation for the established custody-recovery workflow.

With an empty allowed-launch list, automatic recovery retains the current lead's
provider/model/effort. The manager can explicitly select another valid launch
choice. If the current lead has no known model, automatic recovery reports that
no choice is available rather than guessing one. Execute mode, capability grants,
creation/recovery limits and operator pauses apply independently of this list.

An action receipt may be queued, running, succeeded, failed, blocked, uncertain or
revoked. Inspect it before making another action; uncertain publication is not a
license to retry a launch with a different key.

### Recovering stale ownership (LeadControl)

The daemon settles an uncertain `pause_lead` (or `assign_lead`, whose only
possible uncertain effect is interrupting the predecessor) once that exact
predecessor is durably terminal, absent from the active map and its process
cohort is reaped to a fixed point. A queued exact resume or lead repair is then
no longer fenced. Settlement appends `action_settled` evidence; it never
rewrites the original request or re-executes an effect.

`settle_uncertain_action {operation_id, expected_row_version}` asks the daemon
to re-observe one uncertain action in the manager's project and scope. It needs
`LeadControl` and the original action's capability. A proven effect becomes
`succeeded`; a provably absent one becomes `failed` with
`manager_v2_recovered_effect_absent`; otherwise the request is blocked with
`manager_v2_effect_unobservable` and the original stays uncertain. Root
succession and integration keep their own reconcilers.

`retire_lead_continuations {epic_id, expected}` targets a terminal or idle lead
(`manager_v2_lead_active` otherwise). It disables, never deletes, the lead's
enabled `resume` wakes and program sentinel, and records a
`lead_continuation_retirement` witness of the lead's exact current evidence.
Its agent-declared `human_gate` or unknown program evidence then stops blocking
`assign_lead`/`replace_lead`/`resume_lead`; any later lead output or sentinel
re-registration is evaluated normally again. It is refused with
`manager_v2_human_or_recovery_owner`, changing nothing, while a pending
question, approval (or open AppServer approval), operator pause, daemon retry or
C5 owner, or open capacity incident exists. It is not yet part of the prepared
subset; use `AgentManagerControl` with the observed lead fence.

## Change the root manager model

Enable SelfSuccession deliberately through Execute/Full or its Grant row in the policy editor.
The current parentless Standard manager then:

1. Commits its handoff in its own assigned branch. It records the full source HEAD,
   the repository-relative handoff path and that committed blob's full object ID.
   The daemon reads the committed regular text blob; dirty working copies remain
   in the predecessor's retained sandbox.
2. Reads `AgentManagerInspect` Overview and uses the typed `manager_control`
   outcome. `eligibility: eligible` carries the exact current/logical manager
   identities, authority epoch and custody generation, plus the inspection's
   scope/policy fence. `eligibility: ineligible` carries no guessed fence: its
   bounded denial names either `repair_manager_lineage` or
   `appoint_parentless_standard_manager` as the required action. Appointed
   nested or non-Standard managers must be replaced with a parentless Standard
   manager by the operator before self-succession is available.
3. Sends `AgentManagerControl` with action `succeed_manager`, an identical-retry key,
   `expected`, a permitted `launch`, and `handoff` containing `source_commit`,
   `relative_path` and `blob_oid`. The daemon derives all session/project/custody
   identities. `rsi-rpc AgentManagerControl --schema` gives the exact nested shape.
4. Receives the queued operation ID, records it in its final handoff message and
   finishes its turn. The daemon waits for actual predecessor settlement before
   establishing the successor in a distinct assigned sandbox and branch. A queued
   receipt does not mean authority has moved.

The logical appointment, scope/policy revisions, mail, work and original actors
survive publication. The candidate becomes manager only after confirmed provider
establishment and the atomic publication commit; the predecessor then loses that
authority. `:manager` follows the current manager. The Overview row retains both
logical and current identities; `o` opens the current manager session. Inspect the
durable operation before another attempt, including after restart or an uncertain result.

Explicit succession consumes one session-creation allowance and current capacity;
it does not consume or reset automatic-recovery attempts. Retained and unknown
spend remains accounted for. Global rotation disable, policy/operator pauses,
actual recovery owners and human gates still apply. Per-session automatic rotation
disable is inherited unchanged; an explicit succession grant authorizes this manual
operation. An uncertain provider effect retains its cleanup owner until exact
process settlement is proved. The predecessor's dirty source is preserved.

The preflight is read-only and does not grant succession. An eligible fence is
only an observation: `AgentManagerControl` still requires the explicit
SelfSuccession capability and rechecks the parentless Standard/current-manager
binding, scope and policy, authority epoch, custody, committed handoff, provider
establishment and atomic publication.

Unfiltered manager and operator Overview reads always include exactly one
`manager_control` outcome. Feature-lead-scoped Overview reads do not expose
manager succession authority.

With the `Topology` capability in Execute mode, a manager can archive a Group
or Epic together with its non-empty descendant tree when every row is terminal
and no non-archived leaf has a human or recovery owner. The cascade is bounded
to 512 rows, audited as one operation, and stores the exact rows it newly
archived; restore returns only those rows that are still Archived. Individually
archived descendants stay archived, while non-empty delete remains refused.

Guarded Issue reads expose the dependency graph read-only (2026-09-23 K8;
`crates/rsid/src/store/issues.rs`). `AgentListIssues.ready` defaults to false;
true selects Open, active Issues with no Open/InProgress blocker while retaining
the usual archive filter, status filter, cursor order, and page limit.
`AgentGetIssue` returns the Issue plus sorted `blocked_by` and `blocks`
references, at most 256 per relation, with positive `*_truncated` flags when
more exist. Dependency mutations remain operator-only.
`AgentArchiveIssue` and `AgentRestoreIssue` change only the Issue archive marker
for the current owning-Epic lead or the `IssueCoordinate` manager; both require
the expected CAS version and replay key (2026-09-23 K11;
`AgentRestoreIssueRequestV1`).

## Board and decisions

`:manager board` opens the operator's scoped view even when the manager process
is unavailable. Tab/Shift-Tab visits Overview, Workers, Work, Requests, Decisions,
Topology, Resources, Actions and Events; `[`/`]` in Inspect also reaches Archive and
Health (see [Health section](#health-section)). Use `j`/`k` to select a row, Page Up/Down
to scroll its details, `n`/`p` for cursor pages, `o` to open the selected row's session,
and `r` to refresh from page one.
The manager-wide Resources row reports `active_sessions`, pending operations,
spend and `created_sessions` `{used, limit, remaining}` against the saved
`max_created_sessions` (the count creation admission charges).
Workers and Topology page across every legal direct Epic child with bounded indexed
reads. Their cursors retain an insertion horizon while each page checks current
scope and parentage. Refresh to include later insertions or rows moved behind the
current position. Ordinary progress updates do not restart this traversal; policy
or container-scope changes invalidate it. Unsupported nested hierarchy remains an
explicit unknown. A filtered empty page can still have a next page. Cursor errors
retain the previous page with a visible error. The header shows observation time
and partial traversal;
“traversal complete” concerns paging, not feature acceptance.

Rows retain manager, Epic, worker and work identities. Details expose returned
freshness, stop reasons, custody/source references, reported stage states, blockers,
request state and receipts; null evidence is shown as unknown. Overview separates
program and product denominators and reported partial progress from source-accepted
and integrated counts/weights. Work details contain dependency readiness,
shared/exclusive domain ownership, advisory file overlap, migration claims and
integration evidence. A process status of Completed proves only that a turn ended.

Actions rows name the action kind, semantic target type and durable receipt state.
Queued rows say `admission only` because their reserved target UUID does not prove
that a container was committed or that a provider was launched. A succeeded Epic
container creation reports `Container committed` and `Lead unassigned`; it is not
shown as an agent/provider launch. Provider-backed creation reports `Provider
established` only after establishment. The same distinctions appear in the selected
row's details and in the typed action receipt.

`:manager decisions` opens the same board at Decisions. Select a pending target
and press `a` or Enter. Its question, record version and target key accompany the
answer field. Enter submits only the displayed decision's captured digest/version
and scope/policy fence; Esc leaves it pending. A stale/failed answer keeps the draft
and error visible. Esc then `r` refreshes before choosing a new target. A receipt
reports submission; refresh to observe delivery and gate state. This is an
operator-only flow: manager mail cannot answer a tool approval, newer question or
unrelated work acceptance gate. Structured Claude questions use a producer-bound
publication identity; legacy or incompletely persisted questions remain visible
unresolved gates. Existing tool approval records without a verified provider reply
channel are shown with their exact session and approval ID. They stay pending;
`o` opens that session for inspection. The manager does not invent a reply channel
or convert an ordinary message into approval.

Decisions use bounded live keyset pages. Retained native approval history stays
visible and does not consume the separate 1,024-record coordination-work budget
or block current answer delivery. New or changed rows behind a cursor appear on
refresh; scope or policy changes invalidate the cursor. A completed traversal is
an observation of those pages, not a frozen historical snapshot. An empty page
can still have a next page; use `n` to continue the traversal.

Live AppServer command and file-change approvals show their exact request and
available answers. Enter `approve` or `deny` only when that choice is offered.
Concurrent requests have independent targets. A receipt saying `enqueued` means
the reply reached the local writer queue; it does not prove provider consumption.
A provider request closure is recorded separately as `resolved`. Lost writers or
ambiguous reused IDs stay visible for inspection and are never automatically
answered again. The live set is bounded at 64 requests per writer; overflow
retains the unresolved identities and stops that provider through checked cleanup.

Operator answers to manager-created work/request questions are returned in the
Decisions inspection. Reading them as the current manager or lead records that
reader's identity and target digest. An operator's board refresh is not agent
retrieval. A changed work or request version refuses an old drafted answer.

Manager notifications are exact retained subjects, not opaque changes to one
fingerprint. Each row binds the live scope and, when the subject has one, its
affected Epic to a kind, subject identity, subject version, source session when
applicable, recipient, and durable state snapshot. Project-wide action results
have no fabricated Epic. Session notices include status, model invocation,
event sequence and session update time. Message and action notices name their
immutable message or operation receipt; operator-answer notices name the
decision version and target digest without treating the notice as approval.

The scheduler uses one bounded transport watch per Epic/direction plus one
project-wide action-result transport for the appointed manager. Insertion
records both the changed subject and its queued transport obligation. The
action-result queue is indexed by live scope and oldest queued receipt version,
so restart reconciliation examines at most one bounded page without sorting the
operation journal. Watch creation, exact notice insertion, prompt refresh, and
queue reconciliation commit atomically. A retry can adopt a matching
deterministic scheduled-job row left by an older partial write, while rejecting
an identity collision. A later terminal version of the same operation (including
checked succession settlement after an uncertain outcome) is a distinct retained
obligation and triggers the same exact-job event path. Checked succession always
finishes critical candidate cleanup, completed-state insertion, and authority-token
revocation before attempting that immediate transport. A transient notice or
transport error is logged and deferred to the retained versioned queue rather
than reopening or abandoning already-committed cleanup. A pending-archive lead's
terminal state is captured while the Epic still names that lead; archive may then
clear the pointer, while the retained exact route remains authorized and
recoverable after restart. Startup restoration repeats the same pre-cleanup
capture for a persisted pending archive. A live-at-crash row first persists the
truthful `Failed` state without staging a C5 autofile marker, then capture reloads
that exact row before any pointer clear or archive. If status persistence or
capture fails, archive/pointer cleanup is deferred so the durable topology
remains retryable without exposing cancellation as autofile work.
Pending subjects coalesce
into bounded exact tranches without losing their separate identities. Ordered
partial indexes serve current-scope manager, lead, and undelivered-transport
pages. Oversized pending questions use an 8 KiB-bounded truthful projection with
`manager_question_requires_session_view`, question count, and serialized byte
lower bound; the full question remains on the session and is never fully
materialized merely to compute the projection. Project reconciliation commits
independently, so one invalid project cannot roll back another project's work.
Project/Group Epic discovery advances through a retained 32-Epic keyset cursor;
v2 ledger notices use separate per-Epic/per-kind 16-record cursors. These pages
bound every materialization transaction without dropping later scope members.
Deferred transport recovery advances an indexed `(first_sequence, job_id)` cursor
through at most 256 one-row-per-job candidates per page. Already-enabled jobs are
skipped while still advancing that cursor, and the derived candidate row is
deleted once no live undelivered notice remains. Repeated notices for one enabled
job collapse to one candidate, more than 256 dead candidates advance across
pages, and more than 256 distinct enabled live candidates advance across two
passes without retiring valid prefix owners. That larger set remains reachable
across distinct recipients and can also exist in retained legacy/raw state.
Obsolete-route retirement uses V118's ordered live job/route index: it seeks
the exact job, project, manager, scope version, and direction before selecting
at most 16 notices in ascending sequence order. Unrelated retained notices do
not enlarge that scan. Retirement preserves null retrieval/settlement evidence,
and later passes continue through the remaining live rows after restart. V118
adds this index without changing the released V117 catalog or lifecycle rules.
Within one recipient, every operator `UpdateScheduledJob` or
`ToggleScheduledJob` disabled-to-enabled transition is serialized under the
Store guard and rejects the 65th enabled terminal watch with
`terminal_watch_cap_reached`. Re-enabling an already-enabled row is idempotent,
disabling stays available, deterministic program guards keep their independent
mutation protection, and concurrent attempts cannot both consume the final
slot. Reachable enabled work and retained history therefore cannot hide a later
missing transport.
Request-filtered inbox reads use exact message-subject and request-expression
indexes; one request remains bounded by its original message and 32-reply limit.
Only subjects named in an accepted
continuation receive `delivered_at`, and overflow remains undelivered on the
same re-armed watch. Later provider output is not an acknowledgement.
`AgentManagerInbox` returns a bounded `notices` page and writes
`retrieved_at`/`settled_at` for those exact rows in the same transaction. Read it
again while `more_notices` is true. Background scope/lineage reconciliation
marks unreachable rows `retired_at`; it never fabricates retrieval or settlement
evidence. Every scope-version change also records a retained retirement owner
for the obsolete generation. The configuration transaction retires one page for
that exact owner immediately, independent of older queued owners; scheduler
reconciliation drains at most eight owners after restart. Each owner pass retires
at most 256 notice rows and 256 still-unmaterialized action receipts. A terminal
receipt created after its scope became obsolete, including V117 backfill, is born
as an immutable retired queue record rather than a live transport obligation.
V117 trigger and historical-backfill producers admit only
`kind='lifecycle_action'`; routine policy, ledger, and decision receipts never
enter the action-notice queue. If an older unreleased V117 fixture already
contains an unmaterializable mixed-kind row, reconciliation retains it with a
truthful `retired_at` witness and advances one bounded page per pass, so the
next exact lifecycle-action receipt remains recoverable without fabricating a
notice.
The owner completes only after both retained sources are drained, and regranting
scope cannot revive those rows. Inbox
settlement does not reply to mail, acknowledge v2 work, answer a
decision, clear a pending question, remove a pause, or grant approval.
Reconciliation deduplicates the same subject version while a later
invocation/event/action/message version remains distinct.

Manager watches created before V117 have no durable subject rows. They retain
their legacy provider-output confirmation path after upgrade, so an existing
pending-mail watch is not mistaken for an already retrieved V117 notice.

## Agent transports and strict schemas

All eleven manager/reviewer verbs bind caller identity from transport. The
operator-owned SessionControl grant extends `AgentHalt`, `AgentContinueChild`
and `AgentSendMessage` to scoped Epic leads and descendants; spawn permissions
remain unchanged. Guarded Issue reads require either owning-Epic leadership or
the server-checked `IssueCoordinate` grant, and so do guarded Issue mutations
(a manager mutation is audited with actor `manager` and no owning Epic). Both are
project-scoped, preserve CAS/audit semantics, and do not expose operator-only
Issue RPC or ProgramRun. ProgramRun methods remain
operator-only. Targets and observed staleness witnesses are permitted request
fields; caller identity, project grants and injected permissions are rejected,
including inside nested operations. Daemon checks remain authoritative.

| RPC | Native Harness / AppServer tool | Request |
| --- | --- | --- |
| `AgentManagerProgress` | `rsi_control_manager_progress` | `{}` or `after_epic_id`, `limit` (1–64, default 32); follow `next_after_epic_id` until null |
| `AgentManagerInbox` | `rsi_control_manager_inbox` | `after_sequence` (default 0), `limit` (1–32, default 32), optional `request_id`; response also returns exact settled `notices` and `more_notices` |
| `AgentManagerSend` | `rsi_control_manager_send` | `epic_id`, `message`, `idempotency_key` |
| `AgentManagerReply` | `rsi_control_manager_reply` | `request_id`, `message`, `idempotency_key` |
| `AgentManagerInspect` | `rsi_control_manager_inspect` | `section`, optional `epic_id`/`cursor`, `limit` (1–64, default 32); `{}` selects Overview |
| `AgentManagerUpdate` | `rsi_control_manager_update` | `fence`, `idempotency_key`, tagged `change` |
| `AgentSubmitReviewReceipt` | `rsi_control_submit_review_receipt` | `assignment_id`, `verdict`, immutable `findings`, `idempotency_key`; reviewer identity, invocation, source and custody are transport-bound |
| `AgentManagerControl` | `rsi_control_manager_control` | `fence`, `idempotency_key`, tagged `operation` |
| `AgentManagerPrepareControl` | `rsi_control_manager_prepare_control` | semantic `operation` (the six lead-lifecycle actions only: `resume_lead`, `pause_lead`, `retry_lead`, `replace_lead`, `create_session`, `assign_lead`); daemon derives live fences and returns a prepared ID/digest without queueing an effect |
| `AgentManagerCommitPreparedControl` | `rsi_control_manager_commit_prepared_control` | exact `prepared_id`, `target_digest`, `idempotency_key`; daemon atomically rechecks and queues the prepared action |
| `AgentManagerGetAction` | `rsi_control_manager_get_action` | `operation_id`; reads one durable action receipt in the current manager scope |
| `AgentManagerWorkView` | `rsi_control_manager_work_view` | `{}` or `work_key`/`after_work_key`, `limit` (1–32, default 32); read-only projection for a session the current manager created: live work, active ownership, pause, unanswered-request delivery state (no bodies); follow `next_after_work_key` until null |

The `archive` inspect section pages archived and deleted Groups and Epics within the appointed manager's scope. Each row carries its current `expected_updated_at` restore fence, `restorable` boolean, and nullable `restore_blocker` admission code. Manager-created containers remain in scope only for the appointment version that created them. Overview also includes the manager's project metadata.

### Health section

`AgentManagerInspect` with `{"section":"health"}` (operator: `GetHarnessManagerState`)
returns one `type:"health"` row per scoped Epic, keyed and paged by Epic id with the
usual `limit`/`cursor`/`complete` rules; an `epic_id` filter returns that Epic only,
and a feature lead sees only its own Epic. It replaces raw SQLite polling of
sessions, scheduled jobs, reviews and successor reservations. Every aggregate is one
indexed per-Epic read bounded at 32 items (`HEALTH_BOUND`). When a bound is reached
the row carries `complete:false` and names the field in `truncated`
(`children.live`, `wakes.manager_watches`, `notices.to_manager`, `notices.to_lead`,
`reports.open_requests`, `reviews`, `successors.uncertain`), and the page
`complete` is false. Inspect performs no filesystem work.

| Field | Contents |
| --- | --- |
| `lead` | current lead `session_id`, `status`, `status_updated_at`, `age_seconds`, `lineage_tip_id`/`lineage_state`, `provider`, `model`; `lead_state` is `current` or `unavailable` |
| `children` | `live` (Starting/Running/WaitingApproval direct Epic children other than the lead), `oldest_live_session_id`, `oldest_live_age_seconds` |
| `wakes` | enabled/disabled manager-notice watches per direction; `lead_child_watches` (enabled lead terminal watches on those live children) and `lead_child_watch` |
| `notices` | per direction: `pending`, `undelivered`, `delivered_unretrieved`, `oldest_pending_at`, `oldest_pending_age_seconds` |
| `reports` | `latest` lead→manager message (`reply` or `lead_notice`: id, `created_at`, age), `unanswered_requests` (active manager requests addressed to the current lead, bounded), `active_requests` (every unanswered request, including a `failed → accepted` reopen) and `pending_requests` (the capacity slots the send cap and Progress count; a reopen never reclaims one, so `active_requests > pending_requests` shows any overshoot above the cap) |
| `reviews` | `active` (reserved/allocating/active) and `failed` current assignments, with `failure_codes` |
| `facts` | latest K10d `delivery_abandoned` fact on the lead (`job_id`, `watched_session_id`, `at`, `event_sequence`), `requests_lead_changed` (open requests whose recipient is not the current lead and were not readdressed), `successor_uncertain` reservation ids, `rotation_refused` (`rotation_id`, `code`, `at`), `lead_retry_owner` (open capacity-recovery incident id of a Failed lead) |
| `stuck` | `[{code, evidence:[ids]}]`; an empty list means no pattern was detected |

Stuck codes are observations with evidence ids, never gates or verdicts:

- `manager_watch_missing` — lead Completed or Interrupted, at least one live child,
  and no enabled terminal watch by the lead on any live child. Evidence: lead, children.
- `manager_notice_transport_stalled` — a pending notice whose watch job is disabled
  or missing. Evidence: job ids.
- `manager_notice_unread` — delivered but unretrieved for at least 30 minutes
  (`HEALTH_NOTICE_UNREAD_AFTER_SECS`). Evidence: notice ids.
- `agent_successor_uncertain` — a successor reservation for the Epic in state
  `uncertain`. Evidence: reservation ids.
- `manager_review_*` — each current failed review assignment under its existing
  failure code (for example `manager_review_custody_unavailable`,
  `manager_review_receipt_missing`). Evidence: assignment ids.
- `lead_delivery_abandoned` — the lead's latest `delivery_abandoned` health fact has
  no later provider output (assistant or thinking event). The fact is searched in
  the lead's newest 2,048 transcript events (`HEALTH_FACT_EVENT_WINDOW`). Evidence: job id.
- `manager_request_lead_changed` — an open manager request whose recipient lineage
  tip is not the current lead and that was not readdressed. Evidence: request ids.
- `lead_unavailable` — the Epic's committed lead is Failed, Interrupted,
  Archived or Deleted. Same vocabulary as the Requests `delivery_issue`.
  Evidence: lead id, status, status time and, for a Failed lead with an open
  capacity-recovery incident, that incident id. The incident does not suppress
  the code (its wake may be disabled or it may be stale); it is also reported as
  `facts.lead_retry_owner`.
- `lead_rotation_refused` — the lead's newest rotation outcome (`completed`,
  `suppressed_final_handoff` or `refused:<code>` in its newest 256
  `rotation_events`, `HEALTH_ROTATION_EVENT_WINDOW`) is a refusal, so no later
  rotation completed. Evidence: rotation id, code, time.

Uncertain successors and the latest report are read through the Epic's own
children (`sessions` by `parent_id`, then the per-session reservation or sender
index), never by scanning other Epics' rows.

The `:manager` board fetches the first Health page with its other first pages and
shows a HEALTH band: one line per Epic with its stuck codes, lead status and age,
live children and pending notices. `ok` appears only when the stuck list is empty
and the current lead is not Failed, Interrupted, Archived or Deleted; an empty
list with such a lead (for example a Failed lead whose recovery is pending) reads
`unverified`. Details use the generic row renderer;
Enter opens Inspect Health.

External CLI providers use `rsi-rpc`; Harness and AppServer use native tools. Idle
inbox notice resumption supports providers that retain the session ID (for example
Claude and Codex CLI). Completed AppServer sessions require the operator's normal
open/resume flow for those notices because their continuation replaces the
principal; active sessions have native tools and their inbox remains durable.
The session token travels outside `params`. Discover schemas offline:

```sh
rsi-rpc agent
rsi-rpc AgentManagerInspect --schema
rsi-rpc AgentManagerUpdate --schema
rsi-rpc AgentSubmitReviewReceipt --schema
rsi-rpc AgentManagerControl --schema
rsi-rpc AgentManagerPrepareControl --schema
rsi-rpc AgentManagerCommitPreparedControl --schema
rsi-rpc AgentManagerGetAction --schema
rsi-rpc AgentManagerInspect --params '{"section":"work","limit":32}'
```

`fence` contains the observed positive `scope_version` and `policy_version`.
Ledger edits carry the relevant record's `expected_row_version` (zero only for a
new record). Lead operations also carry `expected` with `lead_session_id`,
`lead_generation`, `event_sequence` and optional `custody_generation`, exactly as
observed. A target ID never grants permission. Refresh after a stale refusal.

`change.update` supports `work`, `stage`, `dependency`, `ownership`, `migration`,
`migration_transfer`, `migration_release`, `accept`, `integration`, `request`,
`decision`, and `handoff`.
For a migration transfer, `key` names the new work holder; for a release it
names the current holder. Both changes require `version` and the reservation's
current `expected_row_version`. Release keeps an inactive historical record; a
new `migration` claim can reuse it with that record's current row version.
Versions at or below the released schema head cannot be reserved, transferred,
or released. Inspect Work marks those historical reservations as `consumed`.

Request lifecycle acknowledgements require retrieval by the current addressed feature lead. A proven
rotation successor inherits the exchange through the same receipt checks as v1;
inspection preserves the original recipient and shows the effective current reader.
Raw lead links do not inherit old requests. An authorized manager lead action or
operator SetEpicLead reissues each open request to the replacement lead in the
same transaction. Inspect shows the original as `readdressed`, links its new
request ID, and shows `readdressed_from` on the new request. The new lead replies
to the new ID; a reply to the old ID returns `manager_request_readdressed` with
the next ID. A manager's assertion cannot manufacture lead acceptance. Work stages are planning,
implementation, review, verification and integration; reported states are unknown,
pending, running, partial, passed, failed and blocked. Reported Passed is metadata
until evidence admission proves the declared gate.

`operation.action` supports `resume_lead`, `pause_lead`, `retry_lead`,
`replace_lead`, `create_container`, `update_container`, `archive_container`,
`delete_container`, `restore_container`, `create_session`, `assign_lead`,
`archive_session`, `restore_session`, and `update_session`.
Creation consumes persistent quotas and does not imply lead assignment. Manager
delete requires an empty container; archive also admits the all-terminal cascade
described above, and other nonempty targets return a blocker.
Restoration preserves retained identity and does not silently restore lead authority.

`retry_lead` admits a `Failed` or `Interrupted` lead, and a `Completed` lead whose
provider session `resume_lead` cannot continue (no captured provider session id,
or an AppServer lead). Admission and every execution gate apply the same test. A
resumable `Completed` lead is refused with `manager_v2_retry_lead_resumable`
(`next_action: resume_lead`); a `Starting`, `Running` or `WaitingApproval` lead
with `manager_v2_retry_requires_terminal`. Human gates, pauses, fences, the
`retry_delay_seconds` delay and the `max_recovery_attempts` budget are unchanged:
the successor is a fresh session forked from the lead's frozen source that takes
the lead role by the lead-generation CAS. `resume_lead` admission and the
continuation gate share that resumability test: a settled lead that is an
AppServer lead or has no captured provider session id returns
`manager_v2_resume_unavailable` (`next_action: retry_lead`) at admission, and
such a lead is always retry-admissible. If the id is lost after admission, the
receipt outcome carries the same code. Other unconfirmed resume failures remain
`manager_v2_lifecycle_unconfirmed`.

The additive action receipt fields are `action_kind`, `target_type`, durable `state`
and optional typed `result`, alongside the existing operation ID, target session ID,
legacy outcome and deduplication fields. `result` is absent for queued, running and
unsuccessful receipts. A successful `create_container` result is
`container_committed`; an Epic also carries `lead_state: unassigned`. A successful
`create_session`, `retry_lead` or `replace_lead` result is `provider_established`.
Thus queued is durable admission, never completion, even when a target UUID has
already been reserved.

## Requests and evidence

Use `--params @file` for multiline messages and structured mutations. Reuse a write
key only for identical content. A request's original `message_id` identifies it;
a reply's `request_id` correlates to that original, never to a session ID. Follow
`next_after_sequence` on inbox pages, retain the greatest observed sequence, and
use the inspection cursor only for its own section/scope.

| Observation | What it establishes |
| --- | --- |
| Queued | Mail or an action was durably admitted. |
| Retrieved | An authorized caller retrieved mail; this is not a provider injection receipt. |
| Accepted request | The current addressed lead explicitly acknowledged an obligation. |
| Replied | A correlated reply exists; inspect its evidence and blockers. |
| Effected action | The action journal reports its actual execution result. Queue admission is insufficient. |
| Reported partial/passed work | A reported stage state; readiness still depends on admitted evidence. |
| Source-accepted work | Required gates were admitted for the same declared work/source revision. |
| Integrated delivery | The exact accepted source is an ancestor of the recorded integration target; legacy work also carries independent combined verification evidence. |

A v1 `pending_reply` remains unanswered until an explicit reply exists.
`scope_revoked`/`lead_changed` fences an old exchange. `readdressed` identifies an
open exchange transferred by an authorized lead change. Notices coalesce while a lead
is busy and do not interrupt its active turn. Delivery updates check the captured
watch generation so newer mail survives an older dispatch. A delivered exact
notice stays armed but quiet until inbox retrieval settles it; a new subject
re-arms the same bounded transport. A retained notice wakes an idle recipient
even while its route source is still working: a lead's reply wakes a
`Completed`/`Interrupted` manager with no active turn, queued turn, or pending
question, and manager mail wakes such a lead without waiting for the manager's
turn to end. That wake reads `N durable manager notices pending; read
AgentManagerInbox`, carries every ready transport for the same recipient, and
fires only while no already-delivered subject on that transport awaits
retrieval, so a second report before retrieval coalesces. A recipient going idle
re-evaluates its pending transports at once. Failed, archived, deleted,
question-gated, starting and running recipients are not woken by this rule.
Review assignments reaching `submitted` or `failed` record a `to_manager`
`ledger_change` notice with subject `review:<assignment_id>` and the terminal
state as its version, in the same transaction as the transition; when no live
manager, in-scope Epic, or current lead resolves, the transition commits without
a notice. Direct operator instructions take precedence;
surface conflicts and reply with evidence or a blocker. Human-gated sessions
remain parked.

Abandoned deliveries to a lead notify the manager. When an ordinary terminal
watch gives up because its wake tip never consumed the delivery (no provider
output within the 20-minute re-delivery window), the scheduler still disables
the watch and logs the same warning, and additionally records one `System`
health fact on the tip's transcript (`[rsid-health] delivery_abandoned: ...`,
metadata `health_fact = "delivery_abandoned"` with `job_id`,
`watched_session_id`, `minutes_unconsumed` and `abandoned_at`). If that tip is
the current lead of an Epic in a live manager scope, it also queues one
`to_manager` `session_state` notice: subject is the lead id, version
`delivery_abandoned:<job_id>`, state `{lead_session_id, epic_id,
watched_session_id, job_id, minutes_unconsumed, abandoned_at}`; an idle manager
is woken for it as for any other notice. Each (lead, job) pair notifies at most
once. The lead's `SessionStatus` is unchanged: the health fact and the notice
are how "this lead cannot be resumed" becomes visible, so the manager can
resume, replace or escalate it instead of discovering the stranded Epic by
polling.

For DB-native review, the current manager submits a `request_review` update for
an existing work row and its exact 40-character `source_commit`. The daemon first
reserves the assignment, then allocates a distinct Research session from that
exact source using the bounded `launch` choice. Allocation progresses durably
through `reserved`, `allocating`, and `active`; terminal states are `submitted`,
`superseded`, `cancelled`, or `failed`. Re-review creates a new assignment and
keeps prior assignments and receipts as history.

The assigned reviewer submits exactly one receipt during its bound invocation.
The receipt contains only a verdict and bounded findings; it does not use an
evidence commit, manifest, policy digest, gate taxonomy, handoff footer, or source
freeze. Manager Work inspection exposes the current and historical assignments,
receipt verdict, exact source, and admission eligibility. Once an exact
work/spec/source is enrolled, this DB state is authoritative: pending, blocking,
failed, cancelled, superseded, or absent receipts cannot be overridden by legacy
evidence. Work never enrolled continues through the legacy path below.

For legacy work, after obtaining current versions, a stage update file has this shape
(replace every placeholder with the actual committed source and admitted artifact):

```json
{
  "fence": {"scope_version": 3, "policy_version": 2},
  "idempotency_key": "feature-a-review-source-1",
  "change": {
    "update": "stage",
    "key": "feature-a",
    "expected_row_version": 4,
    "stage": "review",
    "state": "passed",
    "note": "Independent review at the recorded source",
    "evidence": {
      "source_session_id": "<source-session-uuid>",
      "source_commit": "<full-source-sha>",
      "artifact_path": "<committed-review-artifact.json>",
      "artifact_commit": "<full-review-artifact-commit-sha>",
      "closure_evidence_id": null
    }
  }
}
```

Submit with `rsi-rpc AgentManagerUpdate --params @stage.json`. This records a claim
for admission; inspect the work row for accepted evidence or an explicit blocker.
Ordinary independent evidence uses the strict `ClosureReviewArtifactV1` artifact
and the V2 verification manifest at the same stem with `.manifest.md`. The reviewer
must commit the evidence against the exact source, supply the canonical review
handoff, and satisfy reviewer independence, authorized Epic, invocation and custody
checks. Work key, specification revision, declared gates and source must agree with
the review policy/scope. An arbitrary Markdown note, a worker's own review, or a
fabricated digest is insufficient. See [Closure evidence](closure-kernel.md).

After required gate admission, `change.update:"accept"` requests source acceptance
at the observed work version. `change.update:"integration"` additionally records
`source_commit`, the exact `target_commit`, and `verification`. For DB-enrolled
work, `verification` must be `null`: the immutable accepted receipt is the sole
review authority. Legacy work supplies verification evidence in the shape above.
The existing authorized Git workflow performs the merge; the daemon checks exact
accepted-source ancestry and, for legacy work, combined verification. Record and
display its receipt without converting “ready to integrate” into “integrated”.

For remote integration admission, the daemon observes `origin`'s `rolling`
branch when present. If that branch does not exist, it observes the remote's
symbolic `HEAD` branch (for example, `dev`). It fetches the selected branch into
a temporary private ref, checks the declared landing commit and accepted source
against that exact remote history, then rechecks the branch and tip before
recording integration. A missing or ambiguous remote default branch is refused.

Repositories without an `origin` remote require an explicit repository-level
Git opt-in; see [Local-only integration targets](#local-only-integration-targets-rsimanagerintegrationtarget).

V2 ledger limits are per bookkeeping class, not one shared total (2026-09-23
#614; `harness_manager_v2.rs` `bookkeeping_class`): retrieval, resource
(`resource_spend` plus `resource_launch_origin`), and lifecycle records each
have their own 1024-record bound and typed refusal. Work identity remains
seat-independent, while non-work records remain scoped by logical seat and
scope version.

Automatic failed-session Issue settlement is idempotent (2026-09-23 #578;
`settle_c5_autofile_pending`): repeated settlement for the same lineage reuses
the deterministic Issue ID, reports `deduplicated: true`, allocates no second
display number, and emits no duplicate event.

## Local-only integration targets (`rsi.managerIntegrationTarget`)

V2 integration admission requires a verified remote observation of `origin`
(`manager_ledger/git.rs:163`). Repositories with no configured `origin` are
refused `manager_v2_remote_missing` (`git.rs:183`, `manager_ledger.rs:723`) --
lacking a remote is not an opt-in.

The single explicit opt-out is repository-level Git config:

```sh
git config --local rsi.managerIntegrationTarget local-only   # permit local-only
git config --local --unset rsi.managerIntegrationTarget      # revoke
```

Semantics (`git.rs:191`): read with `--local --get-all`; exactly one value must
equal the literal `local-only`, else `manager_v2_local_only_policy_invalid`
(any other or repeated value) or `manager_v2_local_only_policy_unknown`
(unreadable). With the policy set and no `origin`, admission instead requires
`refs/heads/rolling` to equal the declared target and the accepted source to be
its ancestor (`manager_ledger.rs:715-733`). The policy and absence of `origin`
are rechecked immediately before the ledger commit.

**Appropriate only for offline/test repositories without `origin`** (hermetic
fixtures, disconnected clones). It is **not a routine operator knob** and must
not be used to bypass a missing, offline, or stale remote on a shared repository.

| Refusal | Meaning |
| --- | --- |
| `manager_v2_remote_missing` | No `origin` and no `local-only` policy. |
| `manager_v2_local_only_policy_changed` | `origin` appeared, or the policy was removed/altered, between admission and commit (`manager_ledger.rs:738`). |
| `manager_v2_target_changed` | Local-only: `refs/heads/rolling` moved off the declared target (`manager_ledger.rs:741`). |
| `manager_v2_target_or_ancestry_mismatch` | Local-only: local `rolling` is not the declared target, or source is not its ancestor. |
| `manager_v2_remote_target_mismatch` / `manager_v2_remote_ancestry_mismatch` | Remote: observed tip differs from the declared target / source is not its ancestor. |
| `target_rewound_or_diverged` | Inspection freshness: remote delivery moved to a non-descendant of the recorded target (`target_advanced` if merely ahead). |
| `manager_v2_remote_ambiguous` / `manager_v2_remote_unknown` | `origin` has zero/multiple URLs, an invalid default branch, or a non-exact ref reply / remote could not be queried (freshness stays `unknown`). |

## Manager seat recovery (#669)

The appointed manager's lineage tip is the seat. When that tip is `Failed`
and no live process owns it, the seat is down. The daemon records this in a
durable `manager_seat` record (Inspect Overview field `manager_seat`, next to
`manager_available`) with `state` `down`, `recovering`, `exhausted` or `live`,
the tip, `since`, `attempts`/`max_attempts`, `not_before`, a typed `reason`,
`next_action`, and the tip's last invocation, error class and
`terminal_reason`.

Bounded in-place recovery runs on the manager coordinator pass (10 s backstop
and session status hints) and needs all of: policy `Execute`, not paused,
`max_recovery_attempts > 0`, runtime retries enabled, no spend hold, no human
gate on the tip (pending question, approval, operator pause, armed resume
wake, program human gate), a tip the shared manager resumability predicate
accepts (never `CodexAppServer`, whose continuation would allocate a new row,
and never a tip with no captured provider session), and resource admission.
Such a tip is reported `down` with `manager_seat_recovery_unavailable`, and
the operator or an authorized manager must retry or replace it. The seat reuses the
policy's `max_recovery_attempts` and `retry_delay_seconds` but keeps its own
count: attempts are `seat_recovery` operations for the current tip, so lead
recovery and seat recovery never spend each other's attempts, and a new tip
(rotation, succession or appointment) starts a fresh budget. Attempt `n`
waits `retry_delay_seconds * 2^(n-1)` (capped at one day) after the failure
was observed. The default `max_recovery_attempts = 0` never resumes the
manager, but the seat is still reported down. A V1 appointment without the
separate V2 policy opt-in is observed too. It is reported `down` with
`manager_seat_policy_absent`, raises the same signals and has no automatic
recovery. These seats are scanned 32 per coordinator pass behind a wrapping
cursor, so each is observed within `ceil(n / 32)` passes.

Each attempt resumes the same session row and sandbox in place with a
daemon-attributed prompt telling the manager to read `AgentManagerInbox`. It
never launches Fresh or AgentFresh, never succeeds the seat, and never
creates a session. The idempotency key `seat:<tip>:<n>` and a
queued-to-running claim admit an attempt once. At the continuation boundary,
under the tip's spawn guard, the daemon re-checks four things. It checks them
again as the last step before the provider launch, after context preparation,
model admission and orphan reaping, holding the store lock across the
synchronous spawn: the claim is still this boot's, the claimed tip is
still the exact current lineage tip and still `Failed`, every bound above
still holds, and the tip is not busy. A pause, revocation, new question,
rotation or busy tip settles the attempt as a typed `blocked` outcome (for
example `manager_v2_policy_paused` or `manager_seat_tip_changed`). It never
launches a successor. Every claimed attempt is charged, so the next claim
takes a fresh key and the budget always exhausts. A claim left running across a daemon restart becomes
`uncertain`. The seat then shows `down` with
`manager_seat_recovery_unconfirmed` and is never relaunched blindly. A
Claude resume that aborts before streaming (`terminal_reason=aborted_streaming`,
no provider output) counts as a transient failure and gets retried. When the
budget is spent, the seat is `exhausted`
(`manager_seat_recovery_budget_exhausted`) until the operator resumes the
manager, appoints another, or succeeds the seat. The record returns to `live`
only on positive evidence: the tip is not `Failed` and has emitted provider
output after the last down observation or attempt.

Signals: every seat state change publishes a `SystemMessage` prefixed
`[manager-seat]`. The level is `error` while the seat is down and `info` when
it recovers. The TUI shows system messages as notifications (`error` High,
`warn` Medium, other Low). The `:manager` board status line starts with the
seat state, for example `seat EXHAUSTED 2/2 (...)`. Leads see the same record
as `manager_seat` on `AgentManagerInbox` results and on
`AgentManagerSend`/`AgentManagerReply` receipts. Mail and notices stay durably
queued while the seat is down. The manager-notice resume gate defers a
non-`Completed` recipient, so pending subjects remain undelivered, not
dropped, and are delivered after the recovered turn.

## Operator RPC reference

These methods are for the TUI or an operator terminal outside an agent session.
They have no agent/native tools and are excluded from agent `--schema` discovery.

| Method | Parameters |
| --- | --- |
| `GetHarnessManager` | `project_id`; returns configuration or null |
| `ListHarnessManagerEpics` | `project_id`, optional `after_id`, `limit` (1–64); returns legal active Epic identities with parent Group names and `next_after_id` |
| `ConfigureHarnessManager` | `project_id`, `session_id`, `epic_ids`, `expected_row_version` |
| `GetHarnessManagerPolicy` | `project_id`; returns policy configuration or null |
| `ConfigureHarnessManagerPolicy` | `project_id`, `expected_scope_version`, `expected_policy_version`, `idempotency_key`, `policy` |
| `GetHarnessManagerState` | `project_id`, `query` (the typed inspection request) |
| `AnswerHarnessManagerDecision` | `project_id`, `fence`, `decision_key`, `expected_row_version`, `target_digest`, `answer`, `idempotency_key` |

Use zero for the first appointment/policy version, then the returned row version.
The policy's scope version must match the appointment. Configuration retains the
persisted manager anchor and resolves `current_session_id`; a null tip needs
operator repair. Scope clear explicitly names the project and saves `epic_ids:[]`.
Omit `epic_ids` and `group_ids` to appoint whole-project scope. Supply `group_ids`
and optionally `epic_ids` to select their union. Explicit `epic_ids:[]` with no
Groups still revokes, preserving older clients' clear operation. `ListHarnessManagerScope`
pages legal Groups (including empty ones) and Epics; the older
`ListHarnessManagerEpics` endpoint remains available. Both are operator-only.
`HarnessManagerConfigV1` returns `scope_mode`, explicit `selected_epic_ids` and
`group_ids`, plus the current expanded `epic_ids`. V110 preserves all legacy scopes
as `selected`; no existing manager gains project-wide authority through migration.

Routine settings are all available in the policy form; SQL is not an operator API.

## Scoped agent session control

The current appointed manager may read status and progress for Epic leads and
descendants in its live scope, and may watch them for terminal status. These
reads need no V2 grant. `AgentHalt`, `AgentContinueChild`, and `AgentSendMessage`
also require the live `SessionControl` grant in Execute mode. The daemon refuses
these writes for paused policy or Epics, containers, pending questions or
approvals, and operator-paused sessions. Halt and continuation record a
`requested` manager V2 audit event before the effect, including when that effect
fails; mail acceptance records its event in the acceptance transaction. Events
name the verb, target, and Epic without message bodies. Lead and worker
permissions remain as before.

## Scoped session housekeeping

With `SessionControl` in Execute mode, the manager can do the operator's routine
per-session housekeeping through `AgentManagerControl` on one leaf session in its
live scope (nearest Epic covered). `archive_session`, `restore_session` and
`update_session` are `AgentManagerControl`-only, like the container actions:
`AgentManagerPrepareControl` covers only the six lead-lifecycle actions, so the
manager supplies the observed `expected_updated_at` itself. Each action carries the target's
`expected_updated_at`; every refusal changes nothing, and a same-key replay
returns the original receipt. Policy/Epic pauses, the manager's own pause of the
Epic's lead, pending operator decisions and decision holds on the leaf's nearest
Epic apply as for other actions.

- `archive_session` archives one terminal (Completed/Failed/Interrupted) leaf
  that is not active, has no in-memory retry owner and no human or recovery
  owner. It refuses a leaf with non-archived descendants
  (`manager_v2_session_has_descendants`; use the container cascade) and a current
  lead (`manager_v2_session_is_lead`; replace or unassign first). It never runs
  archive cleanup or a sandbox purge.
- `restore_session` returns an Archived leaf to Completed and rehydrates it. A
  purged sandbox or source-worktree settlement history refuses it
  (`manager_v2_historical_restore_refused`).
- `update_session` applies a typed `patch` with at least one of `title`,
  `description`, `rating` (1..=10), `active_task`, `label` (an existing label of
  the project or a cross-project label; label definitions stay operator-only) and
  `tags` (a full replacement set, normalized like `UpdateSessionTags`). Optional
  fields take `{"set": value}` or `"clear"`. Archived and deleted targets are
  refused; a running leaf may be edited.

`AgentSendMessage` agent mail accepted before a scope or policy revocation can
still be delivered afterward. Claim-time delivery does not recheck manager
authority; proper revocation of accepted agent mail is tracked by Issue #631.

## Operator delegation (K14, #672)

With `OperatorDelegation` in Execute mode, the manager can call one operator
RPC method through `AgentManagerControl` `operator_call`, never through the
tokened RPC dispatcher. This is a different reach than Scoped session
housekeeping above: reach is the manager's whole **project** (any leaf whose
`project_id` matches the grant), not the manager's Group/Epic scope, and the
executable surface is a closed, versioned allowlist rather than three fixed
actions.

The current allowlist is `DELEGABLE_OPERATOR_METHODS` v2:

- `ArchiveSession` — logical-only, same retention table as below.
- `GetArchiveCleanupStatus` — read-only.
- `ListSessions` — project-bound, byte-bounded page.
- `UnarchiveSession` — logical restore (added in v2).

A method outside the allowlist is refused `manager_v2_operator_method_not_delegable`
before it reaches any handler — this is true of every non-allowlisted `rpc.rs`
arm, not only the `NEVER_DELEGABLE` ones: a plain `GetSession` call, which is
in neither list, is refused the same way. `NEVER_DELEGABLE` is a separate,
explicitly enumerated set that stays operator-only regardless of the grant:
manager appointment/scope/policy/grant changes and human-gate answers
(self-escalation and operator-owned decisions);
`SetSessionParent`/`UpdateSessionProject`/`CreateContainer`/`SetEpicLead`
(scope self-escalation); daemon configuration and spend policy; launches,
workflows, schedulers and generation calls (spend outside manager quotas);
`ContinueSession`/`InterruptSession`/`MarkPendingArchive` (K14_PAUSE — an
operator continue clears manager pause; halt already exists under
`SessionControl`); deletion and undeletion of any kind; daemon-global custody
(`GetSandboxStorageStatus`, `RunSandboxBuildCacheReclaim`, the source-worktree
cohort family); `ProgramRun`/Closure kernels; the generic Issue verbs (the
guarded `IssueCoordinate` path covers Issues instead); and streaming, memory
writes and cache clears. A test asserts: every allowlisted and every
`NEVER_DELEGABLE` name is a real `rpc.rs` arm; the two sets are disjoint; and
every catalog arm that is NOT on the allowlist — whether or not it is in
`NEVER_DELEGABLE` — is refused `manager_v2_operator_method_not_delegable` and
journals nothing.

**Admission and fences.** The caller must be the current appointed manager
with `OperatorDelegation` granted, mode `Execute`, not paused, and (for
`ArchiveSession`/`UnarchiveSession`) the target's Epic not in
`paused_epic_ids`. The same checks re-run at effect time; losing the grant
between queueing and effect settles the action `Revoked`. `ArchiveSession` and
`UnarchiveSession` additionally require an `OperatorCallFenceV1` with the
target's observed `session_updated_at`; a mismatch refuses
`manager_v2_session_changed`. A target outside the grant's project, or a
non-leaf target, refuses `manager_v2_target_out_of_project` /
`manager_v2_leaf_required` rather than revealing whether the row exists.

| Refusal code | When |
| --- | --- |
| `manager_v2_operator_method_not_delegable` | Method is not one of the four allowlisted methods |
| `manager_v2_operator_params_invalid` | Params fail decode or shape validation for the method |
| `manager_v2_capability_denied` | Caller lacks the `OperatorDelegation` grant |
| `manager_v2_execute_required` | Policy mode is not `Execute` |
| `manager_v2_policy_paused` | Policy paused, or (Archive/Unarchive) target Epic is in `paused_epic_ids` |
| `manager_v2_operator_fence_required` | Archive/Unarchive call omitted `OperatorCallFenceV1` |
| `manager_v2_session_changed` | Fence `session_updated_at` does not match the observed row |
| `manager_v2_target_out_of_project` | Target is not in the grant's project, or does not exist |
| `manager_v2_leaf_required` | Target is not a leaf kind |
| `manager_v2_session_state_changed` | `ArchiveSession` row is not Completed/Failed/Interrupted at effect time (including already Archived/Deleted); `UnarchiveSession` row is not Archived |
| `manager_v2_historical_restore_refused` | `UnarchiveSession` target has a purged sandbox or source-worktree settlement history |
| `manager_v2_action_queue_full` | 64 pending manager actions already queued (`MAX_PENDING`) |
| `manager_v2_operator_result_too_large` / `manager_v2_operator_result_invalid` | A result (a `ListSessions` page or otherwise) cannot fit the byte/row bounds |

**`ArchiveSession` retention.** Logical-only: never a sandbox purge or archive
cleanup. `delegated_archive_blocker` checks in this order and the first match
is both the refusal and the per-row `archive_blocker` value on a `ListSessions`
page; a `null` means archivable now:

| Case | `archive_blocker` / refusal code |
| --- | --- |
| Not a leaf kind | `manager_v2_leaf_required` |
| Already `Archived`/`Deleted` | `manager_v2_session_state_changed` |
| Not yet terminal (`Starting`/`Running`/`WaitingApproval`) | `manager_v2_session_not_terminal` |
| Has a descendant that is not `Archived`/`Deleted` | `manager_v2_session_has_descendants` |
| Is the current lead of another session | `manager_v2_session_is_lead` |
| Pending question/approval, an operator pause marker, an enabled `resume` self-wake, or an open no-idle-capacity incident | `manager_v2_human_or_recovery_owner` |
| `pinned_at` is set | `manager_v2_retention_pinned` |
| An enabled wake targets it (`wake_session_id` or `on_terminal:<id>`) | `manager_v2_retention_enabled_wake` |
| A live review assignment (author or reviewer, state `reserved`/`allocating`/`active`) | `manager_v2_retention_live_review` |
| A sealed source commit not yet integrated | `manager_v2_retention_sealed_source` |
| Activity (row `updated_at` or any conversation event) inside the last 24h, or an unparseable event time (fails closed) | `manager_v2_retention_recent_activity` |
| `GitWorktree` sandbox still `Live` | `manager_v2_retention_live_worktree` |
| None of the above | `null` — archivable now |

A positive archive flips `Completed`/`Failed`/`Interrupted` straight to
`Archived` and leaves `sandbox_root`/`sandbox_branch` and the worktree
untouched. Re-`ArchiveSession`-ing an already-`Archived` row is refused
`manager_v2_session_state_changed` (the guarded `UPDATE ... WHERE status IN
(...)` matches zero rows); it is never a second effect.

**`UnarchiveSession`** shares the fence, project and leaf checks above, then
requires only `status == Archived` and no blocked restore history. It is the
same logical restore as the operator's own unarchive: `Archived` → `Completed`,
no prior status stored, no worktree recreated. A purged sandbox or a
source-worktree settlement history refuses it
(`manager_v2_historical_restore_refused`), same as Scoped session
housekeeping's `restore_session`.

**`ListSessions` page and result shapes.** A keyset page over `(updated_at,
id)`, at most `DELEGATED_PAGE_MAX_ROWS` (64) rows and `DELEGATED_PAGE_MAX_BYTES`
(12 KiB) of serialized envelope, bounded to the grant's project. `status_in`
(empty means every status except `Deleted`) and `terminal_before` narrow the
query. Each row (`DelegatedSessionRowV1`) carries `id`, `parent_id`, `kind`,
`status`, `updated_at`, `last_activity_at`, `pinned`, `sandbox_cleanup_state`,
and `archive_blocker` (the table above). The receipt result
(`OperatorCallResultV1`) is one of two shapes: `Scalar { method, result }` for
`ArchiveSession`, `UnarchiveSession` and `GetArchiveCleanupStatus`, or
`Page { method, rows, next_after, row_count }` for `ListSessions`, where
`row_count == rows.len()` and `next_after` (a `DelegatedSessionCursorV1
{ updated_at, id }`) is present exactly when more rows exist beyond the page.

`next_after` visits every row of a **stable** project snapshot exactly once;
that guarantee does not extend to a bulk walk that mutates rows as it pages.
`ArchiveSession`/`UnarchiveSession` rewrite the target's `updated_at`, which can
move it past an in-progress cursor and surface it again on a later page of the
same walk. Filtering `status_in` to only the pre-mutation status (for example
`["Completed","Failed","Interrupted"]` for a bulk archive, or `["Archived"]`
for a bulk restore) avoids the reappearance instead of merely tolerating it:
once the row's status changes it stops matching that filter on every
subsequent page. A bulk walk should still treat a repeated id as a no-op —
see the retention table above for what a duplicate `ArchiveSession` returns.

**Journal.** Every `operator_call` is journaled like other manager actions,
with the manager itself as `actor_session_id`, and is read back through
`AgentManagerGetAction`. It shares the existing `MAX_PENDING=64` queue and
`BATCH=4` processing with other manager actions
(`manager_v2_action_queue_full` past the limit) and the same
`UNIQUE(project, manager, scope_version, idempotency_key)` replay contract.

## Bounds and verification

One manager covers one project. Explicit selection allows up to 32 Groups plus
32 individual Epics; inherited membership has no 32-Epic cap. The daemon allows
64 project appointments. Progress details are paged; `config.epic_ids` lists the
current effective IDs. Refresh traversal after topology changes. It bounds unanswered requests at 128 per scope,
replies at 32 per request, and enabled terminal watches at 64 per recipient.
The project-wide action-result transport consumes one of those slots while it
is enabled. Overflow notices to a manager wait for the next reconciliation with capacity;
their signatures stay unacknowledged. Durable replies remain retrievable while
notice capacity is full. This watch limit does not narrow scope.
Messages are nonblank, NUL-free, and at most 8192 UTF-8 bytes; idempotency keys are
nonempty, NUL-free, and at most 128 bytes. V2 pages are bounded at 64, pending actions
at 128 and declared work at 256; policy limits are listed above. All references and
versions are validated by the daemon.

The live acceptance loop includes two feature Epics, a busy lead, a stopped lead,
explicit request acknowledgement, work evidence, partial versus accepted counts,
a scope/policy change, restart/rotation, resource exhaustion, and an exact operator
decision. Verify all three transports and the TUI against the integrated daemon.
Source/unit tests do not establish that this live pilot ran. Install and restart
only through the normal reviewed operator workflow.

See [agent control](agent-control.md), [keybindings](keybindings.md), and
[session watches](session-watches.md).
