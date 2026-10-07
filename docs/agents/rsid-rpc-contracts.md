# rsid RPC family contracts (on-demand)

Moved from `crates/rsid/AGENTS.md` on 2026-09-26. Read the section for the RPC
family you are changing. The method list itself is the match arms in
`crates/rsid/src/rpc.rs`.

The full method list is the match arms in `crates/rsid/src/rpc.rs`, which holds
only the router; the handlers live in per-family modules under
`crates/rsid/src/rpc/` (sessions, issues, manager, agent_verbs, scheduled_jobs,
settings, storage, satellites, program_runs, topology, recursive_*, ...). Source-scanning
tests read them through `rpc_production_source()`. Only the non-obvious
contracts are recorded here.

Session lifecycle & management: `SwitchSessionModel` is deprecated (model is
locked at session creation).

Local issue tracker (V97 project-owned `issues`/`issue_deps` plus immutable
`issue_events`): `LinkIssueToIdea` and generic Issue verbs remain operator-only
and are absent from `AGENT_VERBS`/`READ_VERBS`. Ordinary project workers retain
`AgentCreateIssue` for self-attributed follow-ups. Only the authenticated
current lead of one legal owning Epic, or the current appointed manager holding
the V2 `IssueCoordinate` grant (audited as actor `manager` since V125), may use
the bounded project controls
`AgentListIssues`, `AgentGetIssue`, `AgentUpdateIssue`,
`AgentUpdateIssueStatus`, `AgentArchiveIssue`, `AgentRestoreIssue`, and
`AgentListIssueEvents`. Agent mutations use idempotency keys and positive
`expected_row_version`; archive is terminal-only and excludes rows from ready
work, while restore preserves terminal status. `ListIssueEvents` is bounded
operator audit access and remains default-denied to tokened callers.

ProgramRun kernel (operator-only; V78, custody-hardened by V79; divergent legacy
V78 baselines are normalized forward by `repair_legacy_d05_v78_baseline` before
the V79 block runs — see `docs/program-run-kernel.md`): the
`*ProgramRun*` methods are absent from `AGENT_VERBS`, `READ_VERBS`, native
provider tools, and the agent CLI catalog.

Source-worktree cohort settlement (operator-only; V94 journal with V97
hardening; see `docs/cohort-settlement.md`): `ListSourceWorktreeCohorts`,
`AuditSourceWorktreeCohort`, `ApplySourceWorktreeCohort`, and
`GetSourceWorktreeSettlementRun` are absent from `AGENT_VERBS`, `READ_VERBS`,
native provider tools, preambles, skills, and the agent CLI catalog. A
session-attributed caller is default-denied for all four methods. This path is
ancestor-only, local-only, digest/phrase-bound destructive maintenance; it does
not weaken D00 or the independent 69A target-cache reclaim contract.

Agent Sessions (P0 lead/child dispatch) — the entire agent-facing surface:
`AgentSpawnChild`, `AgentReserveSuccessor`, `AgentGetProgress`,
`AgentSendMessage`, `AgentGetStatus`, `AgentHalt`, `AgentContinueChild`,
`AgentArchiveChild` (current Epic lead only),
`AgentScheduleWake`, `AgentCancelWake` (own jobs only), `AgentListWakes` (own jobs only),
`AgentCreateIssue`, `AgentListIssues`, `AgentGetIssue`, `AgentUpdateIssue`,
`AgentUpdateIssueStatus`, `AgentArchiveIssue`, `AgentRestoreIssue`,
`AgentListIssueEvents`, `AgentManagerProgress`, `AgentManagerInbox`,
`AgentManagerSend`, `AgentManagerReply`, `AgentManagerNotify`,
`AgentManagerInspect`, `AgentManagerUpdate`, `AgentSubmitReviewReceipt`,
`AgentManagerControl`, `AgentManagerPrepareControl`,
`AgentManagerCommitPreparedControl`, `AgentManagerGetAction`,
`AgentManagerWorkView`

Harness manager v1 (V102/V103; see `docs/harness-manager.md`): operator-only
`GetHarnessManager` / `ConfigureHarnessManager` appoint one ordinary session
per project. New appointments default to the whole current project; explicit
Groups/Epics narrow coverage. Selected Groups include current and future Epics
(V110); legacy explicit scopes and empty revocations retain their meaning. Neither
appointment method is agent-exposed. Operator-only `ListHarnessManagerScope`
pages legal Groups/Epics, including empty Groups; `ListHarnessManagerEpics` remains
compatible. Progress details page via `after_epic_id`/`next_after_epic_id` (1–64).
The four manager verbs bind caller identity, resolve current manager/lead
ownership, and grant only progress/inbox access and correlated request/reply
mail. The separate SessionControl grant widens halt, continue and agent mail
to scoped Epic leads and descendants, and `IssueCoordinate` admits the manager
to the guarded Issue controls; spawn permissions stay as before.
Scope revisions revoke old exchanges and notices. Tool retrieval rechecks
current scope; terminal watches carry only a notice to read the inbox.
`AgentManagerInbox` preserves `after_sequence` / `next_after_sequence` message
paging (including the legacy continuation while notices remain). Notices also
page independently via `after_notice_sequence` / `next_after_notice_sequence`,
with optional `notice_kind` (`session_state`, `message`, `action_result`,
`operator_answer`, `ledger_change`). Satellite reports are `ledger_change`
notices whose `subject_id` is `satellite_report`. Returned notices are marked
retrieved and settled atomically. `settle_notice_ids` explicitly settles up to
32 exact IDs owned by the caller's current seat lineage, within current scope;
unknown, foreign, or retired IDs refuse the whole transaction with
`manager_notice_not_authorized`. Retries preserve the original timestamps.
Settlement acknowledges notification delivery only, never decisions or approvals.

K14 (#672) adds a separate `OperatorDelegation` grant executed only through
`AgentManagerControl` `operator_call`, never through the tokened RPC
dispatcher. `session/delegated_operator.rs` is the sole executor and runs the
same runtime gate (grant, Execute mode, not paused) every lifecycle action
passes before its effect. Project reach is
`manager_action_project_target` (`store/manager_actions/operator_delegation.rs`):
the target must share the grant's `project_id` and be a leaf kind, or the call
refuses `manager_v2_target_out_of_project`/`manager_v2_leaf_required`, never a
cross-project existence oracle. The logical `ArchiveSession` retention
predicate `delegated_archive_blocker` refuses a pinned row, the current lead,
an enabled wake, a live review (author or reviewer), a sealed but unintegrated
source outside a clean live worktree, activity inside the last 24h, or a live
`GitWorktree` sandbox with dirty tracked files, an unfinished Git operation, or
an unreadable status. Untracked files do not block logical archive; archive
never runs cleanup or moves the worktree or branch. `UnarchiveSession` is the same
logical restore as the housekeeping/operator path (Archived to Completed, no
worktree recreated). The `archive_container` cascade uses the same live
worktree check for each leaf and can archive a fully terminal manager-paused
Epic without resuming its lead. Every call is journaled with the manager as actor and
read back through `AgentManagerGetAction`. A catalog test
(`session/tests/manager_operator_delegation.rs`) parses every `rpc.rs` arm and
asserts each is either on the closed `DELEGABLE_OPERATOR_METHODS` allowlist or
refused; `NEVER_DELEGABLE_OPERATOR_METHODS_V1` is asserted disjoint from the
allowlist, and `AGENT_VERBS`/`READ_VERBS` stay pinned unchanged.

Harness manager v2 (V104) adds a separate operator opt-in through
`GetHarnessManagerPolicy` / `ConfigureHarnessManagerPolicy`, inspected with
`GetHarnessManagerState`; `AnswerHarnessManagerDecision` answers an exact
version/digest-bound pending target. All four are operator-only and absent from
agent/read/native catalogs. TUI commands are `:manager policy`, `:manager board`
and `:manager decisions`; every routine policy field is editable without SQL.
The three new agent verbs use typed nested contracts, observed scope/policy and
target fences, and durable idempotency. `AgentManagerUpdate` and
`AgentManagerControl` are writes, never read-allowlisted. Capability grants do
not widen generic lifecycle/Issue verbs beyond SessionControl's scoped halt,
continue and mail path, or expose ProgramRun. Reported progress,
queued/retrieved/accepted requests, effected actions, accepted source and
integrated delivery are distinct. Operator pauses and exact approvals remain
operator-owned; missing evidence stays unknown.

DB-native `request_review` keeps the Work's original author and exact source
commit after sandbox reclamation (#1431). A durably purged custody root permits
the review to fork from its recorded canonical repository when the commit is
locally reachable from a ref and authored after the recorded custody base.
Admission, launch and infrastructure retry authenticate the recorded repository
identity; generic creates and continuations still require live custody. Missing
or quarantined roots do not use this fallback. A failed repository proof names
the reclaimed sandbox in `manager_review_reclaimed_source_unavailable`.

#### Decision records: gate or non-gate (#1415)

A `decision` record is either a **real gate** (operator-only) or a **non-gate**
that a delegated manager may settle, so the operator answers only real gates.
Records are JSON payloads in `harness_manager_v2_records` (no schema change).

*Classification* (`manager_decision_rulings.rs::classify_decision`):
`gate` is the class when the record is a gate, else `null`.
`gate_source` says why: `reserved_key` (daemon-created: an `accept:`,
`question:` or `approval:` key, or a record with a `decision_target`; always
`human_approval`), `declared` (the asker's `gate` on the `decision` update:
`main_or_release`, `spend`, `credentials`, `data_deletion`, `human_approval`),
or `daemon_scan` (an undeclared question that names one of those gates). A
declared `gate` is authoritative; omitting it means non-gate unless the scan
hits. `AnswerHarnessManagerDecision` is unchanged: it answers any record,
operator-only.

*Manager ruling.* `AgentManagerUpdate` gains two variants, listed in the
catalog's `manager_update_variants` for a manager with `WorkPlan` (a portfolio
seat only in `Execute` mode, unpaused):

- `{"update":"decision_ruling","key","expected_row_version","target_digest",
  "answer","owner_manager_session_id"?}`: the owning project manager, or any
  portfolio seat covering the project and strictly above the owning ledger,
  settles a pending non-gate record. `owner_manager_session_id` names the ledger
  that owns the record (the `rulings` row carries it); omitted means the caller's own
  ledger. The record becomes `status:"answered"` with `answered_by` and a
  `delivery` of `available_in_scoped_inbox` (`actor:"manager"`), so the lead
  retrieves it exactly like an operator answer, and the
  `manager_v2_pending_operator_decision` launch block clears. A gate refuses with
  `manager_v2_decision_operator_gate`; a moved version, digest, work or request
  refuses `manager_v2_decision_changed` / `manager_v2_decision_target_changed`;
  a ledger the caller does not outrank refuses `manager_v2_decision_owner_denied`
  (`manager_v2_decision_owner_unknown` when it names no ledger of the project).
  An area manager cannot rule, including on its own ledger, and is refused
  `manager_v2_decision_owner_denied`.
- `{"update":"decision_withdraw","key","expected_row_version","reason"}`: the
  owning manager withdraws its own pending, non-reserved record (`status:
  "withdrawn"`; the block clears). A lead or another manager is refused.

`AgentManagerInspect {section:"rulings"}` (managers only, one page, no cursor)
lists the pending non-gate records the caller may rule on: its own ledger's and,
for a portfolio seat, every ledger below it (the project manager's). Each row is
a decision row plus `owner_manager_session_id`, `scope_version` and
`owner_role` (`self`, `project_manager`, `portfolio_manager`).

*Read fields.* `GetHarnessManagerState {section:"decisions"}` (the operator
board's read) and the agent `decisions` section return, per `decision` row, the
stored fields `question`, `options` (`[{label, detail?, recommended}]`, the
asker's recommendation), `status` (`pending`, `answer_queued`, `answered`,
`declined`, `withdrawn`, `archived`), `answer`, `asked_by`, `answered_by` and
`history`, plus these computed ones:

| field | meaning |
| --- | --- |
| `answerable_by` | `"operator"` (a gate) or `"manager"` (a non-gate), `null` once not pending |
| `gate`, `gate_source` | the class and why (above), `null` for a non-gate |
| `answered_by` | `{kind: operator \| project_manager \| portfolio_manager, session_id, node_label, at}`; show "ruled by <node_label or kind>" when `kind != "operator"` |
| `blocks` | `{launches: bool, epic_ids: [Epic], work_key, request_id, reason}`; `launches` is true while the record is `pending` or `answer_queued` and not archived (create_session under those Epics refuses `manager_v2_pending_operator_decision`) |
| `history` | bounded audit trail `[{event: asked \| reasked \| answered \| declined \| ruled \| withdrawn \| archived, at, actor, note?}]` |
| `created_at`, `age_seconds` | age of the question |
| `stale`, `stale_reason`, `stale_after_days` | pending and (`manager_gone` or older than 7 days) |
| `archived` | the record is archived history |
| `owner_manager_session_id`, `scope_version` | the owning ledger, for the archive request |

Human-gate rows the daemon projects without a record (legacy approvals,
`unresolved:*` questions) carry `gate:"human_approval"` and
`answerable_by:"operator"`.

*Stale listing and bulk archive* (operator-only: absent from `AGENT_VERBS`,
`READ_VERBS`, native tools, the agent CLI catalog and the delegable operator
methods; the TUI decisions board wires both: `X` lists, confirms the count, then archives, #1428):

- `ListStaleManagerDecisions {project_id?, older_than_days?, limit?}` returns
  `{older_than_days, rows:[{decision:{project_id, owner_manager_session_id,
  scope_version, key, expected_row_version}, epic_id, question, status,
  answerable_by, gate, created_at, age_seconds, stale_reason}], complete}`.
  Stale is a pending, unarchived record older than `older_than_days` (default
  7, max 3650) or owned by a manager that is gone (re-appointed project,
  revoked grant, no live seat, ended node): `stale_reason` is `older_than_days`
  or `manager_gone`.
- `ArchiveStaleManagerDecisions {items:[ManagerDecisionRef], older_than_days?}`
  archives (never deletes) each item that is still at its `expected_row_version`,
  still pending and still stale at archive time. The record keeps its payload,
  sets `status:"archived"`, `archived=1` and an `archived` history entry by the
  operator. Returns `{archived:[ref], skipped:[{decision:ref, reason}]}` with
  reason `missing`, `changed`, `already_archived`, `not_stale` or `not_open`;
  a retry is safe.

Manager preflight adds a separate server-owned path for six semantic actions:
`AgentManagerPrepareControl` derives live authority and target fences without
queueing an effect; `AgentManagerCommitPreparedControl` atomically rechecks and
queues through the existing action journal; `AgentManagerGetAction` reads the
scoped durable receipt. The exact-fence `AgentManagerControl` contract remains
unchanged for compatibility and for operations outside the prepared subset.
`AgentManagerWorkView` is a read-only projection for a session the current
manager created; it is in `AGENT_VERBS` only, writes no row, and grants no
write, mail or continuation authority.

`AgentReserveSuccessor` is the strong master-turnover primitive. The daemon
binds the authenticated predecessor and derives its owning Epic, stable
candidate, and authority fence; none is accepted from request JSON. An exact
predecessor/key replay returns the same durable receipt, while changed content
conflicts. The candidate is a direct child of the same Epic and records
`continued_from` lineage. The predecessor remains authoritative until provider
establishment and the expected-predecessor plus lead-generation CAS commit.
Generic Fresh/AgentFresh work transfers no hierarchy or lead authority.

Malformed requests to the seven guarded Issue verbs may include one optional,
allowlisted validation class/field hint. It is never a schema surface and never
includes raw serde diagnostics, caller keys or values, paths, tokens, or topology.

`AgentSendMessage` (Issue 21 Phase 2, P2-03) is an attributed WRITE: it is in
`AGENT_VERBS` but deliberately NOT in `READ_VERBS`. Sender identity is resolved
from the caller's token and is never a request field.
An omitted or null `expires_at` gets a deadline 30 minutes after first
acceptance; an explicit RFC3339 deadline is preserved. Exact retries return
the original deadline without extending it. `queued` means accepted for
delivery, not delivered to a provider. `failed` and `expired` are final mailbox
settlements, not proof of provider effect. The deadline runs from first
acceptance even while the delivery Session is missing, recovery is pending, or
dispatch is denied; held queued mail can expire before recovery or an idle
boundary. This finite deadline is intentional. A running session also takes
mail at its next tool boundary, at most once (one claim per message; a claim
lost to a crash settles `uncertain`, never redelivered): Claude through the
per-launch `PostToolUse` hook (#1049); the Codex CLI family (Codex, Pioneer, and
the Codex routes of OpenRouter and Bedrock) through the same hook installed as
a session-flag `-c hooks.PostToolUse=...` override (`rsi-rpc
boundary-mail-hook --mail-only`, #1183), trusted by a `-c hooks.state` override
naming its exact key and content hash so every other hook keeps Codex's review;
Harness and the Harness routes of OpenRouter and Bedrock between tool-loop
model calls. A hook runs only the daemon's installed absolute `rsi-rpc`
sibling; without one no hook is installed. Hook-claimed agent mail stays
`claimed` until the hook, after writing its output, calls the hook-only
`ConfirmBoundaryMail` with the provider's transcript locator from its hook
input, and that transcript (verified to be the session's own provider
conversation) records the provider accepting the hook context (Codex: a
developer `hooks.additional_context` item; Claude: a `hook_additional_context`
attachment). Only then is it `injected` (acknowledgeable by a later assistant
event). A reply that cannot be written, a hook the provider timed out or
rejected, an unverifiable locator (an error to the hook), or a delivery record
that cannot be persisted leaves it pending, and
`BOUNDARY_MAIL_CONFIRM_DEADLINE` settles it `uncertain`. The provider hook
timeout is 10 s, well above the hook's 3 s + 1 s daemon I/O. Local, Antigravity and CodexAppServer have no mid-turn
boundary: their send receipt carries `delivery_boundary: "turn_end_only"`
(`AgentMessageDeliveryBoundaryV1`), so the sender knows at send time. Use
`AgentContinueChild` only when deliberate interruption and replacement of a
running child is intended; it is not a mail delivery acknowledgment.

## Agent authority catalog

`AgentGetAuthorityCatalog` (native `rsi_control_authority_catalog`) is open to
every tokened caller and self-scoped: the daemon resolves the token, reads one
`agent_authority_projection`, and renders `AgentAuthorityCatalogV1`
(`rsi_common::agent_authority_catalog`) via
`session::preamble::render_authority_catalog`. Request `{verb?}` with
`deny_unknown_fields`; null params mean `{}`. Refusals:
`authority_catalog_invalid_request`, `authority_catalog_unknown_verb`, and the
projection's own caller refusal. With `verb` the response is the compact
envelope plus one `control` detail (`permitted`, `parameters`, `example`,
`refusals`); `controls` and `guidance` are omitted. Examples and refusals come
from `rsi_common::agent_control_examples`, exhaustive over the verb enum, so a
new verb cannot ship without an example the tests validate. It is an
advertisement, never a grant.

## Rolling merge queue (#1007)

`AgentEnqueueLandingSource` is the only agent-facing queue verb (in
`AGENT_VERBS`, the closed catalog and `rsi-rpc`; no native tool). The daemon
binds the caller from the token; only the current appointed manager and the
current lead of the caller's Epic may enqueue (`queue_not_authorized`
otherwise). Request `{source_commit, test_filters?, idempotency_key}`. Refusals:
`queue_disabled`, `queue_source_invalid`, `queue_filter_invalid`,
`queue_duplicate_source` (a live entry for the commit exists),
`queue_idempotency_conflict`. Entries live in `rolling_queue_entries`
(provisional migration) in FIFO `sequence` order; a run is one `gating` entry plus a
`rolling_queue_batches` row. The runner (`rsid::rolling_queue`) claims the head
while `rolling_queue_enabled` is on, runs `rsi-rolling-land` with the entry's
`--test-filter` set and settles the entry once: the settling transaction CASes
`gating -> terminal` and inserts the owner's single `scheduled_jobs` resume wake
(`wake_job_id` is UNIQUE). Restart reconcile settles a `gating` entry whose
source is an ancestor of `origin/rolling` and re-queues the rest. An out-of-band
push costs at most one regate (`RSI_LANDER_MAX_REGATED_STALE_RETRIES=1`); a
second is refused `queue_out_of_band_regate_exhausted` to the owner. There are no
hot-file waits, claims or seals; migration numbers are renumbered at landing.
A source already on `origin/rolling` (landed outside the queue) is never gated
(#1208): ancestry is re-checked right before a batch is gated, before each bisect
step and after a lander "already integrated" refusal, and such a member settles
`published` with the containing tip (an `already_landed` batch event). One
batch's gating is bounded by `rolling_queue_gate_timeout_mins` (30-1440, default
360); past it the lander's process group is stopped and unsettled members are
refused `queue_gate_timeout`.
The operator settings `rolling_queue_enabled`, `rolling_queue_batch_size` (1-8,
S1 runs one source per gate), `rolling_queue_speculation_depth` (0-2) and
`rolling_queue_gate_timeout_mins` and the
read `GetRollingQueue` are operator-only: absent from `AGENT_VERBS`,
`READ_VERBS`, native tools and the agent CLI catalog.

## Child-aware continuations: hold and keep-alive valve (#794 S3)

Operator-only; no agent verb reads or sets any of it, and there is no schema.

- **Children a parent waits on** are its enabled automatic `agent-child-*`
  terminal watches whose logical child's published rotation tip is live.
- **Hold** (`program_hold_while_children_run`, default on). A program-mode
  master's ordinary one-shot same-session Resume wake, when due, is *held* while
  its `Completed` tip has a running child: the row stays enabled and untouched
  (so `exact_master_continuation_guard_present` and the no-idle invariant still
  hold), and nothing is delivered. It is released when the last child settles
  (the child watch resumes the master as before) or, once, when the window
  elapses. The window starts at the later of the wake's due time and the
  parent's last provider output. The sentinel, valve rows, recurring wakes,
  non-program wakes and a manual "trigger now" are never held.
- **Valve** (`child_keepalive_enabled`, default off;
  `child_keepalive_window_secs`, 300-21600, default 1500). Each scheduler tick,
  an idle `Completed` parent with a running child, no pending question,
  approval, operator pause or capacity incident, and no other enabled resume wake
  gets at most one daemon-owned one-shot Resume row per window (primary key
  derived from `(parent, window_start)`, name `keepalive-{parent}`). Delivery is
  the ordinary Resume path (fence, spawn guard, retry backoff); a busy tip is
  refused `continuation_target_busy` and retried. If every child settled before
  delivery, the row is disabled undelivered. It never launches a Fresh session,
  so no second writer can enter a sandbox. While the row exists it is an enabled
  resume wake under the #1028 owner rule.
- **Read side.** `ListScheduledJobHolds` (operator-only) returns the held wakes;
  the TUI Scheduled Jobs overlay shows `HELD xN until HH:MM`.
- **Settings.** The three keys are persisted runtime-config fields through
  `GetDaemonConfig`/`UpdateDaemonConfig` and TUI Orchestration rows. The
  scheduler reads the durable `daemon_settings` rows each tick, with the same
  defaults `RuntimeConfig` publishes; an unreadable setting turns hold and valve
  off (the pre-#794 behaviour).

## Resource governor (#1014)

`crates/rsid/src/governor.rs` admits builds and landers against slots, 1-minute
load, disk free on `/`, `MemAvailable` and the workers slice's anon+shmem memory
(never `memory.current`, which counts reclaimable page cache). Operator-only
verbs `AcquireAdmission` (`{class: build|lander, pid, label?, ticket_id?}` ->
`granted{lease_id}` or `queued{ticket_id, position, reason, message}`),
`ReleaseAdmission` (`{lease_id}`; also cancels a queued ticket) and
`GetResourceGovernor` (policy, gate margins, leases, queue; the same object
rides on `GetHealthStatus` as `resource_governor`). They are absent from
`AGENT_VERBS`/`READ_VERBS`: the client `scripts/cargo-slot` sends no session
token, and a lease grants a slot only. A lease and a queued ticket are tied to
the client pid + `/proc` start time and are reaped on the next governor call
once the client is gone; a queued ticket not polled for 120 s is dropped. Policy
settings `governor_build_slots` (4), `governor_lander_slots` (5),
`governor_max_load` (0 = 1.25 x cores), `governor_min_free_disk_gb` (30),
`governor_min_avail_mem_gb` (16), `governor_max_workers_slice_gb` (30) are live
daemon settings with Settings > Orchestration rows. If rsid is unreachable the
script falls back to its local flock gates.
## Provider profile and AWS setup (#1407)

- `provider_profile` (`all` | `aws_only`) is an ordinary persisted daemon
  setting (`UpdateDaemonConfig`, Settings > Provider isolation row). `all` is
  today's behaviour. `aws_only` admits only Claude Code on a Bedrock Claude
  model id (`[<geo>.]anthropic.claude-*`); a `Bedrock` launch of a Claude
  model passes only while `api_route.bedrock` resolves it to Claude Code.
- One predicate, `RuntimeConfig::launch_model_refusal`, checks the profile
  before the launch-model allowlist. Every path already calls it: the launch
  chokepoint (`freeze_launch_identity`: interactive, manager, Issue-worker,
  topology and appointment launches), the provider-spawn backstop, the
  continuation and rotation preflights and the `AgentSpawnChild` pre-check
  (`agent_spawn_rejected:ProviderProfileRefused`). The refusal starts with
  `provider_profile_refused`; rotations record that code.
- Under `aws_only` a Claude launch that names no model runs
  `rsi_common::provider_profile::AWS_ONLY_DEFAULT_MODEL`.
- `AgentGetProviderStatus` reports `provider_profile` and marks every provider
  but `claude` and `bedrock` `refused` / `provider_profile_refused`.
- First-run setup: `bedrock_region` (daemon setting, written by the TUI
  `:aws-setup <region>`; it wins over `AWS_REGION`), the Bedrock key through
  `SetProviderCredential` (vault only), then the operator-only
  `VerifyBedrockSetup {model?}`: one `InvokeModel` call (one output token)
  returning a secret-free `BedrockSetupCheck` (stage, status, detail code,
  fixed text; never the body or the key).

## Harness tool policy (#792)

A per-session Harness tool policy is operator-only. It has no agent verb: it is
absent from `AGENT_VERBS`, `READ_VERBS`, native tools and the agent CLI catalog,
and `AgentSpawnChild` (`deny_unknown_fields`) has no field for it.

- **Launch parameter.** `LaunchSessionParams.tool_policy`:
  `{enabled_tools?, denied_tools[], web_access?, egress?, context_editing?,
  budgets{max_search_calls?, max_fetch_calls?, max_result_bytes?,
  max_web_cost_usd_micros?}}`. `context_editing` (#1097; default on, `false`
  turns it off) asks the Anthropic API to clear old tool results server-side
  (`clear_tool_uses_20250919`, beta `context-management-2025-06-27`, trigger
  100k input tokens, keep 8 recent tool uses, clear at least 40k tokens). It is
  sent only on the direct Anthropic Harness transport for Claude models
  (never Bedrock, OpenAI-style or other providers) and never edits a message
  client-side. Harness tool results over 8 KB / 200 lines spill into the
  rsi-common spill store and return a stub; the `read_output` tool reads them
  back.
  `web_access` is `enabled | hosted_only | disabled`; `denied_tools` wins over
  `enabled_tools`. It is validated at launch (`tool_policy_invalid`) and refused
  for providers that do not run the Harness loop
  (`tool_policy_unsupported_provider`; Harness, OpenRouter and Bedrock only).
- **Persistence.** One immutable `session_tool_policies` row per session
  (provisional migration), written at launch. Continue, retry and rotation
  resolve it through the `continued_from` chain, so restrictions survive
  rotation. An unreadable row launches the session fail-closed (no tools, no web).
- **Spawned children.** A child inherits its emitter's policy unchanged, so a
  spawn can only keep or narrow it, never widen it. A child whose provider
  cannot enforce it is refused (`tool_policy_unsupported_provider`), and a
  Harness -> Codex CLI route fallback is refused for a policy session.
- **Daemon defaults.** `harness_web_access`, `harness_max_search_calls`,
  `harness_max_fetch_calls`, `harness_max_result_bytes` and
  `harness_max_web_cost_usd_micros` (0 = unlimited) through
  `GetDaemonConfig`/`UpdateDaemonConfig` and the TUI Orchestration settings.
  `harness_context_editing` (bool, default on; #1111) is the daemon default for
  the session `context_editing` field: an explicit `tool_policy.context_editing`
  wins, else the daemon default decides. A running session keeps the value it
  launched with; a continue or rotation re-resolves its stored policy, so a
  stored policy that left `context_editing` unset follows the default then.
  A session policy overrides a default it sets; unset fields inherit. Read at
  each Harness launch.
- **Network egress (#774).** `egress` (`deny_private` | `offline`; unset =
  `deny_private`) on the session `tool_policy`, with the daemon default
  `harness_egress_mode` (Orchestration settings, read at each Harness launch).
  There is no allow-all mode and no host allow-list. The policy reaches tools as
  `ToolContext.policy.egress`; a network-capable tool fetches only through
  `session::harness::egress::EgressFetcher`, which refuses non-http(s) and
  credential URLs, IP literals and every DNS answer that is loopback,
  link-local, cloud metadata (`169.254.169.254`, `fd00:ec2::254`), private
  (RFC 1918, CGNAT, ULA), unspecified, multicast or reserved (IPv4-mapped, NAT64
  and 6to4 forms classify by the embedded IPv4), connects to the vetted
  address only (no proxy, no re-resolution), re-vets every redirect hop by hand
  (`max_redirects` 5), and caps the body (5 MiB) and time (30 s), each with a
  visible `egress_denied` / `egress_limit_exceeded` tool error. `offline` refuses
  every fetch and runs the `shell`/`exec_command`/completion-gate commands in an
  empty user+network namespace (fails closed when `unshare` or unprivileged user
  namespaces are unavailable). In `deny_private` the shell's network is left
  alone (git and builds need it), so the shell is not covered by the private-range
  guard; systemd `IPAddressDeny` is not enforced for user scopes. No Harness
  network tool exists yet (#748): `EgressFetcher` has no production caller until
  one lands. The classifier is deny-by-default (IANA special-purpose tables; IPv6
  only inside `2000::/3`), and DNS resolution shares the fetch's total deadline.
- **Enforcement.** The catalog build omits refused native tools and never sends
  a refused or exhausted hosted web spec. Execution refuses again: a forced
  call to a refused tool, or a hosted call the policy does not admit, settles as
  a visible tool-error row `Error: tool_policy_denied: ...` with no tool run and
  no hosted result admitted. Budget exhaustion settles as `Error:
  tool_budget_exhausted: ...`; the session continues. Cost is an estimate
  (hosted searches at 0.01 USD, fetches token-only).
## Durable agent jobs (#1002)

Deploy drain (#1566): while a deploy drain holds worker starts, a `test` or
`build` `AgentSubmitJob` from a worker or manager is accepted, not refused. The
job is recorded `queued` (`held: "deploy_draining"`, no `started_at`, no unit),
is visible in `AgentGetJob`/`AgentListJobs`, and the job loop launches it (state
`running`, `started_at` stamped, one owner wake at the end as usual) once the
drain ends or the restarted daemon is past the hold. `AgentCancelJob` settles a
queued job `failed` `job_cancelled` without a unit. A queued job never blocks
the deploy's quiet point (only `running` jobs do), and wall-clock limits count
from `started_at`. `landing`, `cloud_gate` and `cloud_sweep` submits keep the
typed retryable `deploy_draining` refusal. Schema: V159 (`queued` state,
`started_at`).

`AgentSubmitJob`, `AgentGetJob`, `AgentListJobs` and `AgentCancelJob` (#1106; owner-only stop, the job settles `failed` with refusal `job_cancelled`) are agent verbs (in
`AGENT_VERBS`, the closed catalog and `rsi-rpc`; no native tool, none in
`READ_VERBS`). The daemon binds the caller from the token; the job is owned by
that session. `params` are typed per `kind` (`test`, `build`, `landing`,
`cloud_gate`, `cloud_sweep`; `rsi_common::agent_jobs`) and become a fixed argv in
`rsid::agent_jobs::job_command`: an agent never supplies a command line, and every
free-text field is a validated bare token. The working directory is the caller's
own `sandbox_root`; `landing`, `cloud_gate` and `cloud_sweep` are refused `job_kind_not_authorized`
unless the caller is the current appointed manager or current Epic lead; the cloud
gate and the sweep run the scripts and Terraform embedded in the daemon binary
(written to `~/.rsi/jobs/trusted-gate`), never the caller's tree (the sweep gets
the caller's repository only as `--repo`: git never runs there or with its
configuration; the `origin` URL is read with `git config --file` and `rolling` is
fetched into the daemon-owned bare mirror `~/.rsi/jobs/sweep-mirror.git` under
sanitized git configuration, then bundled from it); `test {candidate_receipt: ref}` (#1099; manager/Epic lead only, `job_kind_not_authorized` otherwise; the ref is a bare branch or sha, no new job kind or migration) runs `scripts/candidate-receipt.sh <ref>` in the cwd, which checks the candidate out in a temporary detached worktree with its own cargo target dir and runs `scripts/check-touched-shards`; the job settles with the typed `result.receipt` (the script's final `RECEIPT_JSON` line), `succeeded` only when its `ok` is true, refusal `candidate_receipt_missing` when no receipt was printed; `cloud_sweep {sha}` runs
`cloud-sweep.sh cloud <sha> --repo <cwd> --mirror <jobs_dir>/sweep-mirror.git` (unit `RuntimeMaxSec` 8 h, `TimeoutStopSec` 20 min) and settles with a typed
`result.sweep` (`verdict` GREEN|RED|INCOMPLETE from the last log line, which
must be exactly `VERDICT <GREEN|RED|INCOMPLETE> <sha> new=<n>` (GREEN only with
`new=0`, RED only with `n>=1`; any other token is INCOMPLETE), plus the NEW/KNOWN
names in `~/.rsi/cloud/results/<sha>/QA.md`; GREEN also needs a readable,
within-2-MiB report for that SHA whose verdict section's NEW count and final
line agree, else INCOMPLETE with a `sweep_report_*` refusal) and a typed `refusal` when the spend
guard or another gate stops it; the migration that widened the `kind` CHECK
rebuilds `agent_jobs` (a provisional migration); each Linux unit has
`MemoryMax`/`CPUQuota` and a capped log (`head -c` in the wrapper: at the cap the
log gets a marker and the recorded status is 153); only an appointed manager may pass `worktree`, and it must be a
worktree of the caller's own repository (same `git rev-parse --git-common-dir`).

Package test jobs accept `params.exact: true` (#1600) to pass libtest `--exact`
after Cargo's `--` separator, matching whole names in `filters`. Omitted or
`false` keeps substring matching; empty filters still run every test. `exact`
is a boolean and `true` is refused for shard, recipe and candidate-receipt
runs. Flags in `filters` remain invalid. For example:
`{"kind":"test","params":{"package":"rsi","lib_only":true,"filters":["module::tests::one"],"exact":true}}`.

Project gates (#1477) use `AgentSubmitJob` with
`{"kind":"test","params":{"recipe":"check-cpu"}}`. This is a test-job
workflow, so no new database kind or migration is needed. Declare the gate in
the submitting worktree's `.rsi/jobs.toml` and review it with the project code:

```toml
version = 1

[recipes.check-cpu]
runner = "just"
target = "check-cpu"
timeout_minutes = 20
cpu_quota_percent = 200

[recipes.clippy]
runner = "make"
target = "clippy"
timeout_minutes = 20
cpu_quota_percent = 200
```

`runner` selects only `just` (local `justfile`) or `make` (local `Makefile`).
Targets are literal argv tokens after `--`, without extra arguments, variable
assignments, executable paths or environment overrides. The one exception is
focused test selection (#1584): a recipe run takes `params.filters`, at most 16 `PACKAGE=FILTER` lander filters of at most 256 bytes
with no control characters or leading `-`, and the daemon hands them to the
target as one `RSI_SCOPED_TEST_FILTERS` argv entry (newline-separated), which
`scripts/scoped-test` uses instead of the diff-derived selection; the manifest
declares nothing for it, and `make check-touched-shards` refuses it (exit 2). The
`scoped-test` targets also pass `--total-max-sec` (1140, under the 20-minute
cap): each sequential package gets the lesser of its own budget and the time left,
and a package with none left reports `scoped_test_timeout` instead of being cut
off by the job cap. Names and targets use
ASCII letters, digits, `_`, `-`, `.` and cannot start with `-` (200 bytes max).
The manifest is version 1, at most 64 KiB and 64 recipes, with no unknown
fields; its resolved path must stay inside the worktree. Missing manifests or
undeclared recipes return `job_recipe_not_allowed`; malformed manifests return
`job_recipe_invalid`, before inserting or launching a job.

Declared timeout caps are 5-180 minutes and CPU caps are 1-1600 percent.
The request's test timeout must fit the declaration (`job_invalid_params`
otherwise); for a declaration below the operator default, supply a matching
`params.timeout_minutes`. A declaration never raises the operator default or
grants manager authority to raise a timeout. Linux applies the declared CPU
cap and the existing 24 GiB memory cap; macOS uses the existing launchd backend
and cannot enforce aggregate CPU/memory caps. Both use the build-slot prefix,
private scratch, bounded log and status wrapper, cancellation, timeout,
restart reconciliation and typed result of test jobs. `wake:"none"` supports
the existing `when.jobs_terminal` batch wake. These are repository gates with
the same authority as Cargo tests; declarations grant no cloud, publishing or
credential authority. Package/shard/candidate-receipt fields are exclusive
with `recipe`.

Test-job timeout (#1337): a `test` job carries `params.timeout_minutes`
(5-180). When omitted the daemon stamps the operator's `job_test_timeout_mins`
(default 20, read at submit) into the stored params; a `candidate_receipt` run
gets no default (its 3 h unit cap applies). A timeout above the default is
refused `job_timeout_not_authorized` unless the caller is the current
appointed manager or an Epic lead, or uses the eligible QA shard form below.
The unit's `RuntimeMaxSec` is the timeout
plus 5 minutes; the job poll stops a running unit past its timeout and settles
the job `failed` with refusal `job_timed_out` (also when the unit cap stopped it
while rsid was down), waking its owner once and recording the friction event
`runaway_process:job_test:timeout`. The timeout budget starts when the unit
launches (`started_at`), not at submit: a job held `queued` behind a deploy
drain spends none of it. The pre-launch wait has its own cap, the drain's
`deploy_drain_hold_secs` (default 600 s when uncapped) plus 5 minutes; a job
still unlaunched past it settles `failed` with the distinct refusal
`job_admission_timed_out` (wakes the owner once; resubmit it) and never reports
`job_timed_out` (#1611).

QA shard lanes (#1520): `test` params may carry `qa_lane:{sha:<40 lowercase
hex>}` together with a known `shard` (store-01..04, session-01..05,
memory-01..02, other-01..05), optional existing `filterset:test(NAME)` and
`timeout_minutes` (5-90). The SHA is a label of the submission-time HEAD
only, retained in job params for inspection. A dedicated read-only probe runs
only `git rev-parse HEAD` in the canonical owner sandbox (`-C`), outside the
Store mutex, through the existing bounded capture runner (30 seconds, 64 KiB
per stdout/stderr stream, process-group cleanup). Its environment is cleared,
retaining only PATH/HOME, with system/global config disabled, lazy fetch and
interactive prompts disabled, askpass/SSH forced to fail, and fsmonitor/hooks
and protocols disabled by command options. Git environment redirects and
config injection are excluded. Any probe failure, deadline, malformed output
or SHA mismatch refuses `job_qa_lane_sha_mismatch` before any row or unit.
**The working tree is not checked for dirtiness.** The sandbox can change
before or after the job starts; QA lane results prove nothing about exact
tested bytes. For an exact pin, the manager must use its pinned detached
worktree with the existing `worktree` parameter on an ordinary test job.
QA lanes do not allocate or freeze a checkout and do not change shared
fork/terminal cleanliness observations.
This form is permitted
for the current appointed manager or Epic lead, or a live worker whose latest
`AgentManagerLaunchIssueWorker` binding is still live and unsuperseded **and
carries explicit manager delegation**. The manager sets `qa_lane:true` in that
launch request; it is a typed boolean stored in the existing daemon-owned
`harness_manager_v2_operations.payload_json.issue_binding`. Omitted defaults to
false for both launch and continuation. Continuation requires an explicit new
grant; the predecessor's grant is not inherited. Changing the grant under the
same launch idempotency key is a replay conflict. Ordinary Issue workers and
reviewers remain refused: titles, briefs, labels and caller-supplied test
`qa_lane` params cannot grant this capability.
Plain-created, terminal, superseded and rotation-only workers are refused
`job_timeout_not_authorized`, even for a QA timeout below the default. Authority,
exact binding identity and the delegation flag are checked again
immediately before insertion/launch under the shared daemon Store mutex, held
through both effects. The authority predicate is outside the insertion's
IMMEDIATE transaction; this fence covers daemon writers, not independent SQL
writers. Capacity is checked inside that transaction. QA lanes alone let an
eligible worker exceed the operator default; ordinary test jobs retain the
existing refusal. The fixed 90-minute QA ceiling applies to every caller; an
omitted timeout uses the operator default capped at 90. No recipe, package, candidate
receipt, companion lane, command line, other worktree or cross-session sandbox
is enabled by this form. At most **two running QA lanes per owner** are admitted
(transactionally, `job_qa_lane_limit` on the third); exact idempotent replay uses
no additional capacity, and settlement releases capacity. No new kind, verb,
operator setting or migration is added. These are existing `test` jobs, so the
same timeout poll, service timeout backstop, CPU andon and owner wake apply.

CPU-time andon (#1337, `rsid::cpu_andon`): once a minute the daemon reads
cgroup v2 `cpu.stat` for each agent process tree under the user manager
(`rsi-workers.slice/rsi-worker-<invocation>-<n>.scope`, `rsi-job-<id>.service`;
Linux only, explicitly `cpu_andon_unsupported` elsewhere). A tree past
`cpu_andon_cpu_minutes` CPU-minutes (default 240), or using at least 4 cores and
15% of a 1-minute load at or above `cpu_andon_host_load` (default 40), trips
once: a `runaway_process:<session|job_kind>:<cpu_minutes|host_load>` friction
event, one `ledger_change` manager notice (`record_kind: runaway_process`,
subject `runaway_process:<session>`, version the tree id; the Epic lead gets
agent mail when the project has no live manager) carrying the session id and a
suggested halt, and an operator warning. It never stops a tree. Either
threshold set to 0 is off; both are live operator settings.
The row lives in `agent_jobs` (provisional migration, terminal rows immutable and
undeletable). On Linux, `submit` inserts the `running` row and starts a
`systemd-run --user --collect` unit `rsi-job-<id>` (`TimeoutStopSec` 60 s, 120 s
for landing and 20 min for `cloud_gate` so its destroy trap finishes;
`RuntimeMaxSec` per kind); the unit's fixed `/bin/sh` wrapper redirects output to
`~/.rsi/jobs/<id>.log` and writes the exit status to `<id>.status` as its last
act. A confirmed launch failure returns `job_launch_failed` and never wakes.
If launch control fails after dispatch and stop cannot be confirmed, the row and
scratch remain running for reconciliation; the refusal names the job ID instead
of claiming that potentially live work stopped.

On macOS, package `test` and `build` jobs use a transient per-user launchd
service `gui/<uid>/rsi-job-<id>` (or `user/<uid>` without a GUI domain). The full
service target is stored in the existing `unit_name` field for restart recovery.
The private plist is written atomically beside
the log; literal argv, cwd, HOME/PATH and the build environment are XML-escaped,
and the command strips session-token and ownership-namespace variables. It runs
once (explicit `kickstart`, `RunAtLoad=false`, no `KeepAlive`), outside the session/daemon process tree.
The same bounded log/status wrapper records completion; a service-label watchdog
enforces the wall-time limit even while the daemon is down. Completion, timeout
and cancellation boot out that exact service, with `AbandonProcessGroup=false`
and the per-kind `ExitTimeOut`, so launchd reaps the remaining job group. No PID
file or PID-only kill is used. Controller errors conservatively keep jobs live;
cancellation must confirm service absence before settlement or scratch cleanup.
Mac jobs run Cargo directly (two workers and debug info disabled); launchd does
not provide Linux's aggregate `MemoryMax`/`CPUQuota`. The Linux slot/shard,
candidate-receipt, landing and cloud scripts are not Mac workflows: requests for
those jobs on Mac, and all jobs on other unsupported operating systems, return
`job_platform_unsupported` before inserting a job. Linux keeps its existing
systemd backend and resource limits.

Test/build units receive a private, disk-backed `<id>.tmp` directory beside
the log as `TMPDIR`; launch is refused if this resolves to tmpfs/ramfs.
Their `CARGO_TARGET_DIR` resolves a worktree/ancestor Cargo `build.target-dir`,
falling back to the worktree's `target/`; the daemon's target environment and
global Cargo home config are ignored to avoid sharing artifacts across worktrees.
Scratch is removed on confirmed launch failure
or terminal settlement, including restart reconciliation. Landing/cloud-gate
units retain their existing environment; session tokens are never forwarded.

The daemon task
`run_agent_jobs_loop` polls `running` rows every 5 s and on startup (the same
poll is the restart reconcile): a status file settles the job `succeeded` or
`failed` (`landing`/`cloud_gate` classify with the queue's `classify_run`,
success means a published tip); a status is settled only after the service stops,
so scratch stays available during group teardown. An inactive unit with no status after a 60 s launch
grace settles `lost`. `settle_agent_job` CASes `running -> terminal` and inserts
the owner's single `scheduled_jobs` resume wake in the same transaction, so
overlapping polls and restarts never wake twice.

## Fleet workspace (#1232)

`GetFleetOverview {}` is an operator-only read, excluded from the attributed
agent/read catalogs. It returns active leaf sessions across every project,
project/provider/model groups, and 5-minute, 1-hour and 24-hour usage windows.
The daemon stamps context fill using its shared live/persisted projection.
Turn age uses the latest running invocation's start; absent evidence stays unknown.

Usage is attributed to invocation creation time (inclusive window endpoints),
with input/output/cache-read/cache-write counts and estimated cost. Tokens/min
includes all four token counters; dollars/hour scales the selected window.
Error percentage is failed invocations divided by all invocations in that window.
Missing usage fields contribute only their known values and increment
`unknown_usage`; the UI marks partial totals. These are estimates, not billing.

Indexed reads cap active sessions at 2,048 and the most recent 24-hour invocations
at 50,000, fetching one extra row to detect truncation. Truncation flags are
visible in the workspace. Active sessions have no lower age cutoff, so a
long-running agent remains visible. Project names do not act as aggregation keys.
The workspace refreshes every five seconds while open and retains the last
snapshot, explicitly marked stale, when a refresh fails.

## Portfolio nodes (#1236, hierarchy S2)

A manager above project level is a portfolio node: a `manager_portfolio_nodes`
row (`tier_label` is display only; authority code never reads it), its grant
history in `global_manager_grants` (one active grant per node and per seat,
keyed by `node_id`) and its coverage in `manager_portfolio_coverage`
(`PRIMARY KEY(project_id, depth)`, so roots and siblings are disjoint by
construction). Nothing is deleted; revoked is final.

Operator-only, excluded from `AGENT_VERBS`, `READ_VERBS`, native tools and the
agent CLI catalog (`rsi_common::portfolio_nodes::OPERATOR_METHODS`):

- `ListPortfolioNodes {include_revoked?}` and `GetPortfolioNode {node_id}`.
- `ConfigurePortfolioNode`: `node_id: null` creates a node; a `node_id` writes a
  new grant version (seat, projects, launches, policy). CAS on
  `expected_node_grant_version` and `expected_authority_epoch` (both 0 to
  create) plus an `idempotency_key`; every edit bumps the epoch to the new grant
  version. #1237 (S3): `parent_node_id` names an active node to nest under
  (optional CAS `expected_parent_grant_version`); the node's coverage sits one
  level deeper. `adopt_node_ids` names current children of that parent (roots,
  for a root) that move under this node in the same IMMEDIATE transaction:
  each adopted node and its descendants get a new grant version that keeps
  their grantor, seat and authority epoch (so their V2 ledgers and workers stay
  theirs) and their coverage shifts one level down. The node's projects must
  include every adopted node's coverage. An existing node's parent is fixed
  (`portfolio_parent_immutable`): it moves only by being adopted, so "move
  under" is the new parent's edit with the moved sibling in `adopt_node_ids`.
  Every touched edge must satisfy `grant_narrows` (the child within the parent:
  coverage, capabilities with equality allowed, launches, each finite allowance
  strictly lower, direct reports and spend no higher), including every current
  child when a parent is edited, so a narrowing that would leave a descendant
  over-granted is refused before any write. #1302 (plan §2.2): a parent's
  active children together leave it at least one unit of each finite
  allowance and provider ceiling (spend may reach its cap), checked among the
  siblings under the parent and among the children (current plus adopted)
  under the node; a parent already over (see revoke) may be edited but no
  write may add to its overflow. Only the operator creates a root,
  adopts or re-parents (`manager_node_root_operator_only`). Refusals:
  `manager_scope_overlap`, `manager_node_stale`, `manager_ancestor_revoked`,
  `portfolio_adopt_not_root`, `portfolio_coverage_not_superset`,
  `manager_scope_widened`, `manager_capability_widened`,
  `manager_allowance_exceeded` (also a parent or node over its
  `max_direct_reports`), `global_manager_seat_unavailable` (missing, archived or
  container seat, or a seat holding another node), `portfolio_tier_label_immutable`,
  `portfolio_idempotency_conflict` (#1303: the replay identity includes the
  adoption set, so a same-key request adopting other nodes conflicts). A
  refusal writes nothing. #1398: both `ConfigurePortfolioNode` and the
  `ConfigureGlobalManager` shim accept optional `confirm_cap_reductions`
  (default `false`). Creating or editing an operator grant that lowers a live
  project's effective active-session, lifetime-creation, provider or spend
  ceiling refuses with `portfolio_cap_reduction_confirmation_required`, listing
  each affected project and its old/new ceilings. Review that preview and resend
  the same request with `confirm_cap_reductions:true` to authorize the reduction.
  Replay and topology/CAS checks precede this guard; the preview writes nothing.
  The manager tree turns this refusal into a second confirmation requiring `y`;
  command users can send the flag in `:manager portfolio configure <JSON>` or
  `:manager global configure <JSON>`. Overviews expose `policy.effective_caps`
  as the minimum across the live PM policy and active portfolio chain, or null
  without a live policy. The console shows these configured ceilings on the
  selected project's detail pane; they are not remaining capacity.
- `RevokePortfolioNode {node_id, expected_grant_version, expected_authority_epoch,
  idempotency_key}` is grantor-scoped (operator decision 2026-10-05; authority
  dies with its grantor, never a whole-subtree cascade): the node's grant goes,
  its coverage is released and its queued mail retired; a descendant whose
  grantor is a revoked node (`node:<id>`) is revoked with its whole subtree;
  every other descendant survives one level up (the revoked node's direct
  children move under its parent, or become roots) under new grant versions
  that keep their grantor, seat and epoch. #1305: the re-parenting always
  happens, even past the new parent's `max_direct_reports` or allowance sum;
  that parent is then over capacity (named in the store's revoke outcome) and
  appoints nothing new, by any grantor, until it is back under. Queued actions of a revoked node are
  refused at effect time because every effect re-resolves its principal. A
  replay at the same versions returns the revoked node.

The resolver is depth-agnostic (#1237): a node's seat holds the PM verb set in
every project its coverage contains, at any depth, and authority code never
reads `tier_label`. Rule (c) (mutations flow down) refuses a target created by
the ledger of any node above the acting principal in the project's chain
(lead actions check the Epic's current lead and an AssignLead candidate, at
admission and at effect, #1276), and
every other node's seat stays out of the Epic-scoped read, watch, mail and
control reach (#1239 adds the descendant-seat reach below). Rule (d) charges an admission against the acting
node and every ancestor in `portfolio_chain_for_project(p)`, counting
lifecycle creations and manager-requested topology session attempts
(`manager_ancestor_creation_allowance`, #1275); a descendant's cap never
limits an ancestor. The resource gate (`manager_v2_resource_gate_with`,
#1274) enforces the acting principal's own policy (`manager_policy_for_config`)
plus every ancestor's project policy over a project-wide cohort, and the
model-call gates fall back to the chain when no PM cohort covers the session.

Manager launch admission and `AgentManagerInspect` resolve the same effective
launch list: an explicit local list intersected with every live ancestor's
effective grant. An appointed PM's empty stored list inherits those grants
live; an empty resolved list refuses every launch, including a PM without an
ancestor. Inspect reports the resolved list in `policy.allowed_launches` and
resource-policy rows. A launch refusal retains `manager_v2_launch_not_granted`
as its code prefix and appends `allowed_launches=<JSON provider/model/effort
triples>` from that same intersection. This does not edit stored policies or
add launch choices.

The authority epoch is the grant version of the operator edit that opened it;
a context-cap seat transfer (`transfer_portfolio_seat_in_tx`) writes a new
grant version but keeps the node id and epoch, so the successor keeps the V2
ledger principal `(project, seat root, epoch)` and the predecessor's token is
refused `manager_node_custody_changed`. The `AgentGlobal*` verbs resolve the
caller's own node. `ConfigureGlobalManager`, `GetGlobalManager`,
`RevokeGlobalManager`, `GetGlobalManagerWorkspace` and `:manager global` are
shims over the single active root labelled `global` and refuse
`global_manager_ambiguous` when there are several. `GetManagerTree` renders each
node as a `portfolio` row with its `tier_label`; TUI: `:manager portfolio
list|show <node>|appoint <label> [--adopt <node,...>] [projects...]|configure <JSON>|revoke <node>`,
and the `:manager tree` portfolio-row actions `A` (appoint the focused session
as a manager above the node) and `m` (mark, then `m` on a sibling to move the
node under it), each confirming after a descendant-impact preview.

### Delegation at every level (#1239, hierarchy S5, M3 V156)

Agent verbs, granted to any active portfolio seat (catalog role
`global_manager`), RPC-only:

- `AgentManagerAppointChild {target, launch, query, idempotency_key,
  sandbox?}` launches a Standard root session and seats it in one idempotent
  operation. `target` is `{kind:"project", project_id}` (appoint or replace
  that covered project's PM, whole-project scope, then save the node's
  `child_policy`, or its own policy when unset), `{kind:"portfolio",
  node_id:null, tier_label, project_ids, policy, child_policy?,
  allowed_launches?, max_direct_reports?, launch_project_id?}` (a child node
  with grantor `node:<caller node>` under the caller's node) or
  `{kind:"portfolio", node_id, launch_project_id?, expected_grant_version?}`
  (a new seat for a child the caller's node granted; the child keeps its node
  id, grant and epoch, so its ledger principal and workers stay and the
  predecessor seat's token is refused `manager_node_custody_changed` in the
  same commit). No target has a parent or adopt field; `deny_unknown_fields`
  refuses a forged one. Refusals, all before any session or appointment row
  exists: `global_manager_not_seat`, `global_launch_not_allowed` (not in the
  node's `allowed_launches`), `manager_project_not_in_scope` (a project
  outside coverage), `manager_scope_not_narrowed` (a child equal to its
  parent), `manager_scope_overlap` (a sibling covers a project),
  `manager_direct_report_cap` (the node's active child nodes plus the live
  PMs of the projects it covers deepest already reach `max_direct_reports`;
  replacing a live PM adds none), the `grant_narrows` refusals
  (`manager_capability_widened`, `manager_allowance_exceeded`),
  `manager_child_operator_granted` and `manager_node_not_in_scope` (a seat
  replacement outside the caller's own direct children), `manager_node_stale`,
  `portfolio_idempotency_conflict`. The `manager_portfolio_appointments` row
  (unique per grantor node and key) records the reserved session id before
  the launch, so a replay never launches twice; it moves `launched` to
  `appointed` with the versions (`scope_version`/`policy_version` of the
  project, or the child's epoch and grant version). The appointment re-runs
  its checks after the launch and is fenced on the grantor's authority epoch:
  an operator re-grant of the grantor in between refuses it
  (`global_manager_not_seat`), a context-cap successor finishes it.
  `AgentGlobalAppointManager` is its project-target alias (new rows go to the
  V156 table; `global_manager_appointments` is kept for audit).
- `AgentManagerRevokeChild {node_id, expected_grant_version,
  idempotency_key}` revokes a child node the caller's node granted, through
  the grantor-scoped revoke above: nodes it granted die with it, operator
  granted descendants re-parent. An operator-granted child is refused
  `manager_child_operator_granted`; a sibling, an ancestor or a node another
  node granted is `manager_node_not_in_scope`. A replay against the revoked
  child returns it with `deduplicated`.

Session control over child seats: a portfolio seat reads
(`AgentGetStatus`, `AgentReadSessionEvents`), watches (`on_terminal`) and
mails (`AgentSendMessage`) the seat of any descendant node and the PM seat of
any covered project, and halts or continues (`AgentHalt`,
`AgentContinueChild`) only a direct child seat: a child node's seat
(including its retired seats) or the PM of a project it covers deepest. A
halt or continue also needs `SessionControl` in the node's own policy in
Execute mode, unpaused, and no pending human gate on the target. It never
reaches a sibling's or an ancestor's seat. The operator keeps
`ConfigurePortfolioNode` and `RevokePortfolioNode` over every node;
`GetManagerTree` rows carry `grantor` (`operator` or `node:<id>`, also for a
PM a node appointed) and the TUI tree shows it on each row (`by <tier>
<short id>`) and in the detail (`granted by ...`).

## N-level routing (#1238, hierarchy S4)

`parent_of(node)` is the one routing primitive: an area goes to its parent
area, or to `Project(p)` under the project root; `Project(p)` goes to the
deepest active portfolio node covering `p`, else the operator; a portfolio node
goes to its active grant's live parent node, else the operator. Reports and
escalations climb one parent at a time as mail and never carry authority.

- **Agent verbs.** `AgentReportUp {message, idempotency_key}` sends durable
  mail from the caller's node (portfolio seat, live PM or area seat lineage
  tip) to `parent_of` it; from a root it is an operator notice row, never an
  authority row. `AgentSendDown {target: ManagerNodeRefV1, message,
  idempotency_key}` reaches a strict descendant's live seat; the caller itself
  or an ancestor is refused `manager_target_not_descendant`, anything outside
  its coverage (siblings, other trees) `manager_project_not_in_scope`.
  `AgentReportToGlobal` and `AgentGlobalSend` stay as aliases for one release
  with the v0 request, receipt, delivery text and refusal codes; they write
  tier rows too (`global_manager_messages` keeps pre-#1238 rows for audit and
  replay of their keys).
- **Storage (M2, V155).** `manager_tier_messages` (direction, kind
  `message|report|escalation|ruling`, `source_ref`/`target_ref`
  `portfolio:<id>|project:<id>|area:<id>|operator`, both seats and grant
  versions, body and digest, `UNIQUE(source_session_id, idempotency_key)`,
  state `queued -> claimed|delivered|retired|failed|uncertain`, `claimed ->
  delivered|failed|uncertain`, then final; `failed` and `uncertain` carry a
  `settle_reason`). `manager_tier_escalations`
  holds one row per hop above a project root (linked to its
  `manager_node_escalations` row; one open hop per escalation; `target_ref`
  `operator` is the operator queue) and `manager_tier_escalation_events` one
  immutable row per hop change (`opened`, `forwarded`, `ruled`, `returned`,
  `retired`). No row is deleted; identity columns never change.
- **Delivery.** A message to a seat is a durable one-shot resume wake on the
  global-message path (`is_global_message`, the scheduler fence and the
  continuation effect claim): it is delivered at the recipient's next idle
  boundary and wakes an idle recipient, never mid-turn (it is not #1183 agent
  mail). A claim that succeeds marks its messages `claimed`; a refused batch
  leaves its current messages queued. The continuation then settles them
  (#1266, the #945 rule: at most once, an uncertain result is shown and never
  auto-replayed): `delivered` when it launched, `failed` with the error when
  it failed before any provider effect (admission, custody, a refusal),
  `uncertain` when it failed after one; each disables the wake. A settlement
  write that fails keeps the outcome in memory and the 15-second recovery
  pass writes it (#1294); a claim older than two minutes with no continuation
  in flight and no recorded outcome settles `uncertain`; a restart settles
  every still-`claimed` row `uncertain`. None of these delivers again. Failed
  and uncertain mail is listed with its reason in the sender's and
  recipient's `AgentManagerInbox.undelivered_tier_mail` (newest 32, filtered
  to the seat's lineage in SQL, `more_undelivered_tier_mail` when older rows
  exist; a portfolio seat with no project inbox gets just that list) and in
  `ListOperatorEscalations.undelivered`, paged 256 at a time by
  `undelivered_after`/`next_undelivered_after` (#1295); nothing reopens it. The fence re-checks the
  target (portfolio: same grant version and exact seat; project: the live PM
  tip; area: same grant version and seat tip) and, for agent mail, the source
  grant. Grant replacement, revocation and context-cap transfer of a portfolio
  node retire queued mail at either end and its open hops (the escalation
  returns to the project root, which may forward it again); PM displacement
  retires down-mail to the seat and its own reports.
- **Escalations.** `AgentManagerResolveEscalation` with no ruling at a project
  root opens hop 1 to `parent_of(Project p)`; the in-project row stays open at
  the root, which is refused `manager_node_escalation_forwarded_above` while a
  hop is open. A portfolio seat lists its hops with
  `AgentManagerListEscalations` (`version` is the escalation version plus the
  hop number) and rules or forwards with `AgentManagerResolveEscalation`. A
  ruling at any tier rules the hop, records `returned` on every earlier hop,
  rules the in-project row at its root and wakes the source seat with the
  ruling. A ruling never answers a human approval. The source's live grant,
  epoch and grant version are fenced above the root as below it: a stale
  source retires the open hop and refuses the resolve, and revoking a source
  area retires its open hops. A portfolio seat's identical retry (same key)
  replays its stored result before any fence.
- **Lead mail.** `AgentManagerNotify` from the current lead of a live Epic in
  a project with no live PM routes to the deepest covering portfolio node as a
  tier message (`sequence` 0 in the receipt).
- **Operator surface (rule 10).** Operator-only `ListOperatorEscalations
  {include_closed}`, `RuleOperatorEscalation {hop_id, ruling,
  idempotency_key}` and `AcknowledgeOperatorNotice {message_id}`; TUI
  `:manager escalations [list|all|rule <hop> <text>|ack <notice>|undelivered [<cursor>]]`. Tree rows
  count the open hops addressed to a portfolio node; a project row no longer
  counts an escalation held above it.

## Node workspace and overview (#1240, hierarchy S6)

One snapshot shape, `ManagerNodeWorkspaceV1`, describes any manager node
(`ManagerNodeRefV1`: portfolio of any tier, project, or area). There is no
per-tier query: the snapshot reuses the portfolio grant rows, the area node
rows, `parent_of`, #1213's per-project rows and #1232's fleet aggregation.

- **Contents.** `node`, `label` (tier label, project name or `area`), `state`
  (`active`, `revoked`, or `vacant` for a project with no live PM), `parent`
  (`parent_of(node)`, `null` for the operator), the portfolio `grant` (active,
  else the last revoked one), `grantor` and `max_direct_reports`, an area's
  grant (`area`), the live `seat`, `children`, `projects`
  (`GlobalWorkspaceProjectV1`: PM seat and policy, Issue counts,
  Running/WaitingApproval counts, pending questions and approvals),
  `missing_project_ids`, `escalations` and a `fleet` rollup. Children are the
  child portfolio nodes, then the covered projects no deeper node covers (the
  projects this node manages directly), then the child areas. Each child is a
  bounded digest (`ManagerNodeChildV1`): its seat (status, model, context fill),
  state, grantor, grant version, coverage, counts summed over its coverage
  (`null` for an area) and pending escalations; never its own children,
  projects or inbox. At most 128 children and 64 escalations (reasons cut to
  512 bytes) are listed, with `*_truncated` flags.
- **Escalations.** A portfolio node lists the open hops addressed to it; a
  project or area node lists the open in-project escalations addressed to it
  that are not held above the project root (the manager tree's count).
- **Fleet.** `Store::fleet_overview_scoped` is #1232's aggregation with a
  row filter: a portfolio or project node keeps its coverage's rows, an area
  keeps the sessions under its selected Epics (each Epic, its lead and every
  descendant; a project-wide selector keeps the project). The row bounds
  apply before the filter, so a node's counts equal #1232's totals filtered to
  its coverage and inherit its truncation flags. The rollup carries `active`,
  the three usage windows and the groups, without per-agent rows.
- **Operator RPC (rule 10).** `GetManagerNodeWorkspace {node}` lists the
  node's whole coverage. It is not in `AGENT_VERBS`, `READ_VERBS`, native
  tools or the agent CLI catalog. Unknown nodes are refused
  `manager_tier_target_unknown`; a project's root area is answered as its
  project. `GetGlobalManagerWorkspace` is its shim: the operator workspace of
  the single active root labelled `global`, projected to the #1213 shape
  (`grant`, `seat`, `projects`, `missing_project_ids`); with no active root it
  keeps showing the latest revoked grant.
- **Agent verb.** `AgentManagerOverview {}` is the caller's own node
  (portfolio seat, live PM or area seat tip; `manager_tier_not_node_seat`
  otherwise), bounded for agents: `projects` lists only the projects the node
  manages directly, so a pinnacle sees its globals as digests and never their
  projects' rows. Native `rsi_control_manager_overview`. `AgentGlobalOverview`
  stays as its v0-shaped alias (the same project rows over the whole grant,
  `global_manager_not_seat`).
- **TUI.** The manager console (#1231) renders any node: `gm` opens the
  global through the shim, and Enter on a Portfolio, Project or Area row of
  the manager tree (#1214) opens that node's console.

## Portable install bundle (#1406)

Operator-only (not in the attributed verb registry; the handlers refuse a
tokened caller again with `portable_bundle_operator_only`). Logic:
`crates/rsid-store/src/store/portable_bundle.rs`; handlers:
`crates/rsid/src/rpc/portable.rs`; CLI: `crates/rsi/src/portable_cli.rs`.

- **`ExportPortableBundle {path, overwrite?}`** writes one JSON bundle
  (`format: "rsi.portable_bundle"`, `format_version`, `schema_version`) to an
  absolute `path` through a temp file and rename. It carries `projects`,
  `daemon_settings` (minus runtime state, entity-scoped keys holding a UUID,
  and secret-named keys, listed by name in `withheld_settings`),
  global/project `permission_rules`, non-session `model_budget_policies`, and
  every Issue with its `issue_events` and `issue_deps`. Manager and portfolio
  policies travel as `manager_templates` with every session id, epic/group id
  and idempotency key removed; they are never inserted into live manager
  tables. Sessions, events, sandboxes, jobs, ledgers, wakes, queues and ideas
  stay behind (`left_behind` counts). The vault is never read into the bundle,
  and the export is refused (`portable_export_secret_found`, naming table, row
  and column, never the value) if any carried value contains a vault entry or
  a set credential env var value.
- **`ImportPortableBundle {path, merge?, path_remaps?, dry_run?}`** imports
  into the daemon's database (already built by the normal migrations) in one
  `BEGIN IMMEDIATE` transaction. Refusals: `portable_bundle_schema_too_new`,
  `portable_bundle_format_too_new`, `portable_import_target_not_empty`
  (projects, issues, sessions or ideas exist and `merge` is false), an unknown
  table or column. `merge` keeps every existing row and skips the bundle's
  copy (and its children). `path_remaps: [{from, to}]` rewrite project paths
  whose whole path or `/`- or `\`-separated prefix is `from`. Issue-to-Idea
  links are cleared (ideas stay behind). Manager templates are written to
  `<db dir>/portable/manager-templates-<exported_at>.json`. Credentials are
  never imported; settings apply after an rsid restart.
- **CLI.** `rsi export --clean <bundle>` and `rsi init --from <bundle>
  [--remap OLD=NEW]... [--merge] [--yes]`; `init` starts rsid if needed, runs a
  dry run, prompts for every project path missing on this machine, then
  imports.

## Deploys: the quiet point and `interrupt_workers` (#1045, #1320, #1461, V158)

- **Supervisor refresh (#1592).** Put `scripts/rsid-supervisor.sh` from the
  candidate checkout alongside `rsid` in `binaries_dir`. It is an optional
  manifest artifact (older binary-only callers remain supported), verified by
  hash and file identity like the binaries, and checked with `bash -n` before
  staging succeeds. `make release-install` packages it automatically. Between
  daemon lifetimes the supervisor checks the installed script, validates a
  private snapshot and re-execs it with restart history and fallback state
  preserved. Invalid syntax leaves the running supervisor in charge; its
  script is retained as `rsid-supervisor.sh.last-good` before a refresh.
  The live hub's pre-#1592 supervisor needs one manual
  `make release-install NOW=1` to start this loop (a quiet daemon-only restart
  cannot refresh that older supervisor).
- **Quiet point.** `AgentRequestDeploy` stages and verifies the binaries, then
  the deploy runner (`deploy.rs`, one poll per 5 s over the single live row)
  swaps and restarts after two consecutive polls with no blocker
  (`Store::deploy_quiet_blockers`): `landing_in_progress` (a merge-queue entry
  admitted or gating), `job_running` (a running local `test`, `build` or
  `landing` job; `cloud_sweep` and `cloud_gate` jobs survive a restart and do not
  block) and `worker_mid_turn` (a Starting or Running scoped, non-container
  session other than the caller). An operator restart (#1122) also counts
  `manager_mid_turn` (parentless sessions) and is unchanged. While it waits the
  deploy holds new worker starts for at most the operator's
  `deploy_drain_hold_secs` after it was requested (#1320); from its first quiet
  poll it holds again through the swap.
- **`interrupt_workers: true`** (optional, default false; V158 columns
  `agent_deploys.interrupt_workers` and `interrupted_sessions`). Once the hold
  is over (`created_at + deploy_drain_hold_secs`; with the drain setting off
  there is no hold, so it is over at once) `worker_mid_turn` stops blocking
  (`Store::deploy_quiet_blockers_with(caller, false)`). `landing_in_progress`
  and `job_running` still block, so a landing or a local test/build/landing job
  is never cut off. Inside the hold, and for a deploy that did not ask, nothing
  changes. `max_wait_secs` still bounds the wait: a wait shorter than the hold
  ends in `timed_out` as before, so keep it above the hold. Not available with
  `peer_id` (`deploy_invalid_request`: the satellite runs its own flow) or
  `cancel`. The flag is part of the replay fingerprint (a request without it
  keeps the old fingerprint), so a replay cannot flip it.
- **Outcome.** At the confirming poll, after the restart fired, the runner
  reads the workers mid-turn (`Store::deploy_mid_turn_workers`), records them
  once on the row (`record_agent_deploy_interrupted`, only while `restarting`)
  and files one `deploy_interrupt:worker_mid_turn` friction event per worker
  (session = the worker, evidence `deploy:<id>`). The next process settles the
  row and its single wake names those session ids; the receipt
  (`interrupt_workers`, `interrupted_workers`) carries them on a replay. The list
  is the workers mid-turn at the swap: the graceful drain gives each up to 30 s to
  finish before interrupting, so one whose turn ends inside that window is
  listed but resumes nothing.
- **Resume.** No new path. The drain journals each still-running turn
  (`daemon_restart_intents`, `InterruptSource::DeployDrain`); the next process
  marks the turn Interrupted and `reconcile_restart_intents_at_startup`
  dispatches the continuation with the daemon's continue prompt. Pinned by
  `deploy_interrupt_resume::a_worker_cut_off_by_a_deploy_restart_is_continued_by_the_next_process`.

## Host-load admission (#1417)

- **Setting.** Operator setting `host_load_admission_threshold` (0..=1024,
  default 40; 0 disables; live; TUI settings row "Host load limit for new
  launches", `GetDaemonConfig`/`UpdateDaemonConfig`). Read at every admission
  decision (`host_load.rs`).
- **Hold, never refuse.** While `load + recent_admissions > threshold` the
  daemon holds a manager's new worker: a queued `create_session`
  (`AgentManagerControl`, `AgentManagerLaunchIssueWorker`, review allocations)
  stays `queued` because `Store::claim_manager_action_with_create_admission` leaves
  it unclaimed (the deploy-drain mechanism, #1073); a topology node of a
  manager- or Epic-lead-requested execution stays `Reserved`
  (`NodeEffects::launch_held`, retried on the executor tick) and writes nothing.
  Both start on their own when the load drops. `load` is the 1-minute average
  (`/proc/loadavg` on Linux; any other platform, or an unreadable file, is
  `supported: false` and admits everything).
- **Oldest first.** The claim query orders queued creates by `(not_before, id)`
  across all projects; waiting topology nodes queue by execution age
  (`HostLoadAdmission::admit_waiter`, stale entries dropped after 90 s). The two
  lanes share one held queue: only eligible manager creates enter it, so
  younger creates cannot consume capacity ahead of older topology work (or
  vice versa). Cancelled or no-longer-eligible entries age out after 90 s.
- **No thundering herd.** The 1-minute average lags, so each launch admitted in
  the last 60 s counts one toward the comparison (`recent_admissions`).
- **Never held.** Operator launches, a worker's `AgentSpawnChild` children,
  retries, rotation successors, Issue worker `continue_from` launches,
  recovery of topology attempts already `Launching`, lead recovery (`replace_lead`, `retry_lead`,
  `resume_lead`), every non-create manager action, and topology executions the
  operator requested.
- **Visibility.** `AgentManagerGetAction` carries `held {reason: host_load, load,
  threshold, recent_admissions}` on a queued create (a deploy hold, when both
  apply, is reported first). `AgentGetDaemonInfo.host_load` reports `threshold`,
  `supported`, `load`, `recent_admissions`, `holding` and `held` (oldest first,
  at most 64: `manager_create_session` and `topology_node`, `reason: host_load`).

## Friction telemetry, the andon (#1333, V157)

- **Recording.** The daemon appends one `friction_events` row (signature,
  session, project, evidence ref, `recorded_at`) at six points: a tokened
  `Agent*` verb that returns an error (`agent_refusal:<verb>:<code>`, the code
  from `data.code`, a code-shaped message or a fixed class, never prose), a
  deploy that times out (`deploy_timeout:<blocker>`, e.g. `worker_mid_turn`), a
  deploy restart that interrupts a worker mid-turn (`deploy_interrupt:worker_mid_turn`,
  one per worker, #1461), a
  session finalized Failed with `terminal_handoff_superseded_by_tool`
  (`terminal:<reason>`), a refused or failed merge-queue entry
  (`lander:<state>:<refusal|tests_failed|unclassified>`) and a lost job or
  failed landing job (`agent_job:<kind>:<state>`). Signatures and evidence refs
  are code tokens and ids only; the V157 CHECKs refuse anything else. The
  project comes from the session. Rows are never updated or deleted.
  Recording is best effort and never fails the observed operation.
- **Andon.** Every ten minutes (and at boot) the daemon files one Issue,
  labelled `kaizen` and `andon`, for each `(project, signature)` with at least
  3 occurrences across at least 2 sessions in the last 24 h. The Issue id is a
  UUIDv5 of the pair and `andon_filings` keys on it, so a signature is filed
  once, ever (a recurrence after closure shows in the rollup against the filed
  Issue). At most 5 filings in any rolling 24 h, rechecked in each filing's
  transaction. Project-less events (an operator deploy) are rolled up but
  never filed.
- **Operator RPC (rule 10).** `ListFrictionRollup {project_id?, window_hours?
  (1..=720, default 24), limit? (1..=200, default 50)}` returns rows by
  occurrences (`signature`, `kind`, `occurrences`, `sessions`, `first_at`,
  `last_at`, `evidence_refs`, `filed_issue_id`, `filed_display_number`,
  `due`), `truncated`, `filings_last_24h` and `daily_filing_cap`. It is not in
  `AGENT_VERBS`, `READ_VERBS`, native tools or the agent CLI catalog
  (`rsi_common::friction::OPERATOR_METHODS`).
- **Agent read.** `AgentManagerInspect {section:"friction"}` (managers only;
  a worker is refused `manager_v2_scope_denied`) pages the project's rollup
  keyed by signature.
- **TUI.** `:manager friction [<hours>]` summarizes the rollup; the manager
  board's Inspect · Friction section lists one project's rows.
