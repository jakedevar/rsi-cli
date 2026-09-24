# Local issue tracker

## V77 project ownership and Idea linkage

V77 makes every Issue an immutable, required member of one project. Existing
V76 rows are rebuilt in one `BEGIN IMMEDIATE` transaction using only their
creator Session's persisted project; a missing creator/project or a
cross-project dependency aborts without changing the V76 database. The version
bump is last. After a successful V77 migration, recovery is roll-forward only:
do not run a V76 writer or destructive downmigration.

Operator creation now requires `project_id` and always creates an unlinked
Issue:

```text
rsi-rpc CreateIssue --params '{"project_id":"<project-uuid>","title":"Follow up"}'
```

An operator may link an existing Issue exactly once with `LinkIssueToIdea`.
The project is read from the stored Issue, not the request. A successful call
atomically links the Issue, advances the Idea row version, and appends one
`issue_linked` event. Replay with the identical idempotency key returns the
original event; changed semantics, stale versions, wrong-project Ideas/events,
or an already-linked Issue fail without overwrite. Optional finding provenance
is exactly `finding:v1:sha256:<64 lowercase hex>:<stable-id>`, where the stable
ID is 1–128 ASCII `[A-Za-z0-9._-]` bytes. Its digest is recorded as event
evidence.

```text
rsi-rpc LinkIssueToIdea --params '{"issue_id":"<issue-uuid>","idea_id":"<idea-uuid>","expected_idea_row_version":1,"idempotency_key":"link-follow-up"}'
```

Agent and native create schemas remain unchanged and cannot accept project,
Idea, event, finding, creator, or actor fields. The daemon derives the caller
Session's project and rechecks it in the insert transaction; attributed and C5
issues remain unlinked. `LinkIssueToIdea` is operator-only and is not an agent
verb or native tool.

Dependencies require both endpoint Issues to share a project. Ready queries,
local candidates, by-ID reconciliation, completion, and active-dispatch
restore are project-scoped. Set `RSI_ISSUE_TRACKER_PROJECT_ID` to a valid UUID
when `RSI_ISSUE_TRACKER_KIND=local`; missing or invalid scope disables the
local tracker. Linear configuration remains unchanged.

RSI's local tracker is the V77 SQLite `issues` and `issue_deps` store. Issues
have Open, InProgress, Closed, and Cancelled status; Cancelled is logical
deletion. Ready work is Open work with no unmet same-project dependency,
ordered by unset priority, priority, creation timestamp, then UUID. Select the
local backend through the normal issue-tracker configuration; Linear remains an
independent external backend.

Operators use `CreateIssue`, `GetIssue`, `ListIssues`, `UpdateIssueStatus`,
`AddIssueDep`, `RemoveIssueDep`, `ListReadyIssues`, and `LinkIssueToIdea`
through `rsi-rpc`. For example:
`rsi-rpc CreateIssue --params '{"project_id":"<project-uuid>","title":"Follow up"}'` and
`rsi-rpc UpdateIssueStatus --params '{"issue_id":"<uuid>","status":"Closed"}'`.
These eight verbs remain operator-only.

Agents retain `AgentCreateIssue` (or native `rsi_control_create_issue`) for
ordinary self-attributed follow-ups. Its
strict fields are title, body, priority, labels, assignee, and idempotency_key;
caller and creator identity are server-bound. A matching replay returns the
original issue with `deduplicated:true`; changed content for the same caller/key
is rejected. Prefer `rsi-rpc AgentCreateIssue --params @issue.json` for a
multiline body and never put an authority token or creator in that file.

Eligible settled Failed TaskRabbit, Task, Bug, Feature, Refactor, and Research
lineages automatically create one Open issue per `continued_from` root. It is
labeled `pipeline-failure`, `auto-filed`, and `worker-kind:<kind>`; generated
metadata is bounded and excludes prompts, transcripts, credentials, and paths.
Retries, successors, evals, interactive/container sessions, issue-driven rows,
and user cancellation/continue/archive/delete/purge paths do not create noise.
The activation-gated `daemon_settings` pending journal replays only indexed
pending keys after restore; it never backfills historical Failed rows. Automatic
write failures log and publish an error without changing session state. Failed
issue-dispatch release/annotation remains a separate manager follow-up.

Local candidates normalize to `unstarted`: excluding it from
`RSI_ISSUE_TRACKER_ACTIVE_STATES` silently yields zero candidates. Without
`RSI_ISSUE_TRACKER_COMPLETION_STATE`, completion neither closes a local issue
nor releases its running slot, so dispatch can stall at `max_concurrent`; use
`completed`/`closed` or `UpdateIssueStatus` to recycle the slot. C4/TUI is
deferred; the CLI/RPC is the mutation surface.

## Operating contract

The V77 row fields are immutable id, required project identity, display number,
and creation timestamp; content fields (title, body, optional priority 1–4,
ordered labels, assignee); optional session creator; link-once Idea/event/
finding provenance; status, update timestamp, and `closed_at`. Dependency edges
block ready work until same-project blockers reach Closed or Cancelled. Use the
eight operator verbs for lifecycle, dependency, and one-time linkage changes;
operator `CreateIssue` allocates UUIDv4 with a null creator and null linkage.

`AgentCreateIssue` and `rsi_control_create_issue` have the same strict schema:
only `title`, `body`, `priority`, `labels`, `assignee`, and `idempotency_key`.
They reject caller/session/creator/id/status/display-number/timestamp fields.
The daemon binds caller identity and applies `created_by_session_id`; a stable,
NUL-free 1–128-byte key maps to a durable UUIDv5. Matching replay preserves the
first row, including after restart; a changed create field is an error, never an
update. The native tools are available to Harness and CodexAppServer, while the
tokened RPC is for CLI providers. Neither grants list/read/update/dependency
authority and all eight generic operator verbs remain denied to attributed callers.

## V97 guarded project Issue control and audit

V97 adds `row_version`, `archived_at`, and append-only `issue_events` receipts.
Ordinary workers continue using `AgentCreateIssue`; they do not receive project
read/mutation authority. The current lead of exactly one legal owning Epic, or
the current appointed manager holding the V2 `IssueCoordinate` grant (audited as
actor `manager`), may use the seven guarded agent/native pairs: list/get, content
update, one status update, archive/restore, and event history. Project, actor, Epic, lead, and
token fields are never accepted from the caller.

Harness/CodexAppServer register all seven native tools for every bound session
to keep the roster stable; the owning-Epic lead or `IssueCoordinate` manager
check occurs when a tool runs.
Registration is therefore not execution authority. The tokened CLI follows the
same rule: every agent verb is advertised, then every request is guarded
at dispatch. The compact payload/result examples and shared safe error envelope
are documented in [Agent control](agent-control.md#guarded-issue-control-v95).

For example, an owning lead can update status through the one lifecycle verb:

```text
rsi-rpc AgentUpdateIssueStatus --params '{"issue_id":"<uuid>","status":"Closed","expected_row_version":3,"idempotency_key":"close-v1"}'
```

The exact retry returns the original `issue` and immutable `event` with
`deduplicated:true`; a changed semantic request for that key conflicts. Status
updates allow Open→InProgress/Closed/Cancelled, InProgress→Open/Closed/Cancelled,
and Closed/Cancelled→Open. Archive is terminal-only, restore keeps terminal
status, and archived rows never appear in ready/local dispatch selection.
`AgentListIssues` and `AgentListIssueEvents` are bounded to 1–256 records;
Issue pages use an exclusive `(display_number, issue_id)` cursor, while history
uses `after_sequence`. Operators can inspect bounded audit history via
`ListIssueEvents`; generic Issue verbs remain default-denied to tokened callers.

Automatic content is deliberately bounded. Its title is `Pipeline worker
failed: <Kind> <root-prefix>`; labels are exactly `pipeline-failure`,
`auto-filed`, and `worker-kind:<kebab-kind>`; priority and assignee are absent.
The final newline-terminated body identifies source/root, kind, provider/model,
bounded cause/disposition, and retry values only. It never carries prompts,
transcripts, tool arguments/results, arbitrary stderr, credentials, working
directories, or issue URLs. A root walk is cycle-defended and capped at 64.

Only a post-activation persisted Failed writer may stage a journal record.
Policy runs after retry/rotation disposition: live retry markers defer; policy
decline, exhausted budget, retry-launch failure, crash reconciliation,
store-desync, rotation-monitor panic, and comparable daemon-owned terminal
failure may settle. A successful successor resolves the predecessor marker.
No historical `Failed` scan occurs. The finite restart pass uses the indexed
`c5.autofile.pending.v1/` prefix in batches of at most 64; automatic insert/
select and pending deletion are one transaction. On automatic write error the
pending row remains, one error is logged and one error SystemMessage is
published, while session status, retry, rotation, and original launch errors
remain untouched.
