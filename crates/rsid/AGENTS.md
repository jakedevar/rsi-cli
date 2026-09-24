# Daemon crate (`rsid`)

## RPC Surface

The full method list is the match arms in `crates/rsid/src/rpc.rs`. Only the
non-obvious contracts are recorded here.

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
`AgentScheduleWake`,
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
an enabled wake, a live review (author or reviewer), a sealed but
unintegrated source, activity inside the last 24h, or a live `GitWorktree`
sandbox; it never runs archive cleanup. `UnarchiveSession` is the same
logical restore as the housekeeping/operator path (Archived to Completed, no
worktree recreated). Every call is journaled with the manager as actor and
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
boundary. This finite deadline is intentional. Claude and Codex CLI sessions
have no authenticated mid-turn mail delivery and acknowledgment path here;
mail can only reach a supported idle boundary before expiry. Use
`AgentContinueChild` only when deliberate interruption and replacement of a
running child is intended; it is not a mail delivery acknowledgment.
