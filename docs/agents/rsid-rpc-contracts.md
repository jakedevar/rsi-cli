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
work, while restore preserves terminal status. `AgentUpdateIssueStatus` takes an
optional `note` (at most 2048 bytes; why it closed: covered-by SHA, duplicate,
moot, or a reopen reason, #2329). It lives in the status event's request JSON
(no migration), is part of the request fingerprint (same key with a different
note is `idempotency_conflict`), and is returned as `status_note` by
`AgentGetIssue` (the newest status change's note) and as `request.note` by
`AgentListIssueEvents`. `ListIssueEvents` is bounded
operator audit access and remains default-denied to tokened callers.

A live Issue-bound worker may also use `AgentListIssues` to dedupe before filing:
`title_contains` is required and `project_id` must be omitted. Its live launch
binding determines project scope within the list transaction. It receives only
`display_number`, `id`, `title`, `labels`, `status`, `priority`, `created_at`
and `updated_at` in each row, with the same page envelope and cursor. All list
filters, including `labels` (all exact matches), work on both projections. RPC
and native tools serialize the same typed result. Leads and managers choose
with `fields: "summary" | "full"` (#2052): omitted, a `title_contains` search
returns the summary rows (no bodies; read one with `AgentGetIssue`) and every
other list keeps full records; `fields:"full"` restores bodies on a search. A
bound worker always gets summary rows.

**Bound-worker reads of referenced Issues (#218; hub #2505, #2391, #2408).** A worker
launched by `AgentManagerLaunchIssueWorker` reads, with `AgentGetIssue` and read-only,
(1) its bound Issue, (2) the Issues launched with `review_of` = it (#1845), and (3) the
same-project Issues it references. Referenced means a `#N`, `#A-#B`, `#A-B`, `#A..#B`
(or en-dash) selector in: the bound Issue's body (read now), its newest 64 handoff records
(read now, worker-authored ones included: a worker can extend its own reads inside its
project but gains no write and never crosses projects), the manager's launch brief (the
selectors are snapshotted into the launch binding as `brief_refs`, at most 64 distinct;
more refuses the launch `manager_issue_worker_references_overflow`), or the launch's
`related` list (at most 16 numbers, the same-project Issues whose COMPLETE text and latest
handoff are also copied into the brief; unknown number: `manager_issue_worker_related_unavailable`).
`related` and `brief_refs` ride in the action's journalled `issue_binding` (no migration);
a `continue_from` successor inherits both as reads, a capacity relaunch keeps them, and
a replay under one key with a different `related` is `manager_v2_idempotency_conflict`.
A plain relaunch of the same Issue (no `continue_from`, no `review_of`) also carries over the `related`
Issues of its earlier launches (#275): a union with the new `related`, deduplicated, within the 16 bound;
they ride as `carried_related` in the binding and result, join `brief_refs` (the read grant), and are named
in the brief. An earlier Issue that no longer exists, is archived or does not fit is `skipped_related`,
named, never a refusal.
Reference text is a data selector: only digits are read, a `#` glued to a word, `&`, `_`
or `/` is not a selector, and a range selects `A..=B` only when it spans at most 32
numbers (a reversed or wider range selects just its two endpoints, so no middle number
is granted silently). References never chain: the text of a read or attached Issue
selects nothing. The attachment is a snapshot at launch (whole or named with its size
and left to be read, never cut); the read is live at call time. A foreign-project or
unknown UUID, a missing number, an unreferenced Issue and a binding whose launch is no
longer `running`/`succeeded` (revoked, failed) keep `agent_issue_authority_denied`.
Writes, status changes and handoffs stay on the bound Issue: the compiled brief ends
with a section naming it as the only publication Issue whatever the freeform brief says.

`AgentManagerOverview {fleet?}` (#2052) takes `fleet: "none" | "summary" |
"full"`. The default `summary` replaces the fleet rollup with `{view:"summary",
as_of, active, totals:{1h,24h}, models:[{model, active, 1h, 24h}], ...}` where
each window is `{invocations, errors, cost}`; `none` returns `fleet: null`;
`full` is the whole rollup (every project, provider and model group over
5m/1h/24h). The operator `GetManagerNodeWorkspace` always returns the full
shape. `AgentGlobalOverview` carries no fleet.

ProgramRun kernel (operator-only; V78, custody-hardened by V79; divergent legacy
V78 baselines are normalized forward by `repair_legacy_d05_v78_baseline` before
the V79 block runs — see `docs/program-run-kernel.md`): the
`*ProgramRun*` methods are absent from `AGENT_VERBS`, `READ_VERBS`, native
provider tools, and the agent CLI catalog.

Source-worktree cohort settlement (V94 journal with V97 hardening; see
`docs/cohort-settlement.md`): a run also keeps the roots it retained (#2635, V179
`source_worktree_settlement_retained_roots`): the apply that commits the intent
snapshots every non-eligible audit root (session, custody, sandbox root, source
ref, `disposition` class, diagnostic) in the same transaction, and
`GetSourceWorktreeSettlementRun` and the apply receipt return them as
`retained_roots`, exactly `counts.retained` rows in audit order, so "what is still
held and why" is one read. The rows are immutable (a BEFORE INSERT guard refuses `REPLACE` and UPSERT
collisions, #2689); a run recorded before V179 has none. `AgentAuditSourceWorktreeCohort` returns
with `fields:"full"` the latest run's snapshot as `last_run_retained_roots` beside the fresh `items` (#2690): it is what
that run recorded, not today's classification, and lists only the roots whose owner the caller
covers now. A retained item explains itself in its `diagnostic` (#2731): `custody_unverified` and
`custody_generation_drift` carry `check=<field> stored=<..> observed=<..>` for the first stored custody,
session-identity or runtime-completed-entry check that failed, and `nonancestor` carries
`compared_ref=<ref>@<oid12> ref_updated_secs_ago=.. tip_committed_secs_ago=.. fetched_this_pass=..`, plus
`stale_ref` when a remote-tracking ref this pass did not fetch has not changed for 30 minutes. The build-cache
reclaim report's `recovery_sweep.intent_counts.stuck` lists residual Prepared/Staged/Deleting reclaim intents
unchanged for over an hour, with a reason. The operator methods the operator methods `ListSourceWorktreeCohorts`,
`AuditSourceWorktreeCohort`, `ApplySourceWorktreeCohort`, and
`GetSourceWorktreeSettlementRun` and `ListSourceWorktreeSettlementRuns` remain default-denied to tokened callers and
absent from agent/read/native/CLI catalogs. #1879 adds separate
`AgentListSourceWorktreeCohorts`, `AgentAuditSourceWorktreeCohort`, and
`AgentApplySourceWorktreeCohort` verbs and native tools. Only a current,
unpaused Execute-mode PM or portfolio seat with the operator-granted
`storage_control` capability may call them. Coverage is derived from current
seat grants, never from JSON: the repository must have a covered owner, participant or retained receipt owner.
Each root is checked separately: every foreign owner, participant or receipt
owner, regardless of status, and unknown ownership are retained with
`retained_foreign_owner` or `retained_unknown_owner`. The plan digest binds only
the eligible set and its safety evidence; retained metadata and unrelated ref
activity do not invalidate an unchanged plan. Eligibility changes still refuse apply.
`AgentAuditSourceWorktreeCohort {repository_identity, fields?}` accepts
`fields: "summary" | "full"`, following the `AgentManagerInbox` naming. Omitted
`fields` selects summary when `applyable` is false, full otherwise. Full preserves
the existing response exactly. Summary adds `view:"summary"`,
`counts_by_disposition` (fresh item counts keyed by the snake_case disposition),
and `last_run_retained_root_count` (the latest run's roots within caller coverage);
it omits `last_run_retained_roots` and keeps only eligible roots in `items`.
The existing aggregate `counts` still describe the complete fresh audit, so
`counts.observed` may exceed `items.length` in summary. All other metadata,
including `applyable`, `plan_digest`, `authorization_phrase`, refusal and run id,
is preserved. Apply accepts the summary's exact digest and phrase: projection
happens after the complete audit and never changes the safety proof. Request
`fields:"full"` to inspect retained-root evidence. The operator audit remains full.

A covered terminal owner (including Deleted) may be settled by the repository
manager only through the full custody, runtime and dependency proof. Operator
and automatic settlement may reclaim foreign terminal orphaned roots through
that same proof. Deleted Sessions stay Deleted after settlement. An unrelated repository refuses with `storage_control_not_in_scope`. These verbs call the same local, ancestor-only audited
apply as the operator, with its exact digest/phrase, idempotency key and durable
run receipt. `AgentApplySourceWorktreeCohort` is
asynchronous (#2789, #2866, #2869): it answers with the receipt once the run's durable intent
exists, while the run is still applying (`finished_at` unset). The audit that binds `plan_digest`
(bounded on the repository mutex: past 10 s it is `source_settlement_busy`; a stale digest or
non-applyable audit is refused) and the intent insert run in one detached daemon task, and the
caller waits at most 5 s for it. Past that it gets the typed `source_settlement_apply_pending`
refusal (`disposition: pending`, `run_recorded: false`, `retry_after_secs`) and the task keeps
going: retry the exact request with the same `idempotency_key` to join it. The retry receives the
run once recorded, or the stale or refused outcome the audit ended with (a different request on
the same key is refused); no root is touched without a fresh audit that matches `plan_digest`.
The pending state is process-local (no effect has happened yet, so a daemon restart simply makes
the retry audit afresh). The same task then takes the per-root spawn guards, checks that no
owner became active and the orphan scan is readable (a failure closes the recorded roots as
`unattempted` with the observation), recomputes nothing, and settles the roots; each root
re-proves its own evidence digest before any effect, so guard contention never holds the
caller. An exact retry while that task runs returns the
stored receipt and starts no second driver (`settlement_runs_in_flight`); after a restart
startup recovery resumes the run as before. Wait with `AgentScheduleWake`
`when:{"settlement_run_settled":"<run_id>"}` (below) instead of polling. The operator
`ApplySourceWorktreeCohort` and the automatic tick still wait for the whole run. Authority is rechecked before durable intent and under the Store
lock spanning each removal effect. Dirty or non-ancestor work and live or
retained dependencies remain protected. Retained-intent startup recovery keeps
its existing proof and rechecks the initiating agent's current authority before
an untouched item starts an effect. Revocation closes remaining untouched items as unattempted immediately in apply or recovery;
effect-started items retain exact recovery. Recovery never adds roots to that
intent or enumerates new agent work. The Full manager preset already grants `storage_control`, so
current Full-preset PM/portfolio seats receive these verbs within their coverage.
This does not widen
portfolio delegated operator calls or D00/69A target-cache reclaim.
Automatic settlement uses that same apply under the operator-only live settings
`source_worktree_auto_settlement_enabled` (default on) and
`disk_pressure_min_free_gb` (default 100; 1..=4096), capped at 25% of the
filesystem using the launch-admission helper. Pool trimming has separate
controls. The local daemon checks its
sandbox filesystem every five minutes, rechecks before intent and each removal,
and retains work on a failed sample or proof. V167 records immutable run origin
and authenticated agent identity atomically with intent. Recent run summaries
are operator-only (`ListSourceWorktreeSettlementRuns`, limit default 20,
1..=50), visible in the settlement overlay, and full receipts retain their
original attribution on replay. Historical receipts have no attribution.
An automatic pass has no waiting caller (#2452), so an outcome that needs an owner files
one `auto-filed` project Issue (`store/settlement_notice.rs`) in the project that owns the
retained work, with run id, repository, sessions, refusal codes and observations. Only a
persistent outcome files (#2630): a root retained by the same refusal or recovery fence in 2
consecutive automatic passes, or the same audit/apply/resume error class twice in a row; a
one-pass custody drift, stale-digest race or a fence the next pass cleared stays in the run
history. The streak is process memory (`SettlementPersistence`, one `begin_pass` per pass; a
restart delays a filing, never advances it). The Issue id is a UUIDv5 of project, repository
and outcome signature (#2650: the phase class and refusal code of the persistent roots, or
failed stage and error class), so a later pass with other roots or counts rewrites the open
Issue in place (counts, latest run) instead of filing another, while a different refusal code
or repository gets its own Issue; closing it acknowledges, and a closed Issue is never
rewritten. A root the normal archive already purged is left out before filing (#2650).
A manual run returns its receipt to its caller and files no run notice. Custody checks are
unchanged.


Agent Sessions (P0 lead/child dispatch) — the entire agent-facing surface:
`AgentSpawnChild`, `AgentReserveSuccessor`, `AgentGetProgress`,
`AgentSendMessage`, `AgentGetStatus`, `AgentHalt`, `AgentContinueChild`,
`AgentArchiveChild` (current Epic lead only),
`AgentScheduleWake`, `AgentCancelWake` (own jobs only), `AgentListWakes` (own jobs; `session_id` reads a roster-scoped live worker, #2418; native `rsi_control_list_wakes`, #2389: predicate, deadline and any pending delivered message per wake),
`AgentWorkerRoster` (own launches; a project's manager or covering portfolio seat: the project),
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
Each Progress row carries `last_model_turn_at` (#9): when the current lead last started a real
model turn, read from the model-invocation ledger (newest `started_at` of an admitted invocation of
the lead that is neither a background helper nor a compaction); `null` when the lead has none. Use
it, not `updated_at`, to tell a dormant lead from a busy one: `updated_at` (and `lead_last_activity_at`
in the Inspect intent rows, which is the newer of `updated_at` and the last conversation event) also
move on title refinements, ledger notices, wake arming and metadata writes, while
`last_model_turn_at` moves only on a model turn. `AgentManagerInbox` notices stay frozen audit
snapshots and carry no such field; compare `last_model_turn_at` from Progress. No migration.
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
`operator_answer`, `ledger_change`). A kind other than `message` suppresses
the message page, including on audit reads. Routine manager reads return only
incoming replies and lead notices, excluding the seat's own sends across
rotation. `include_sent:true`, `request_id`, or `fields:"full"` also returns
outbound sends for audits; lead inbox delivery is unchanged. Satellite reports are `ledger_change`
notices whose `subject_id` is `satellite_report`. Returned notices are marked
retrieved and settled atomically. `fields:"summary"` (#285, default `"compact"`) trims the returned notices to a `state_summary` string in place of `state` (projection only; the same notices settle). `settle_notice_ids` explicitly settles up to
32 exact IDs owned by the caller's current seat lineage, within current scope;
unknown, foreign, or retired IDs refuse the whole transaction with
`manager_notice_not_authorized`. Retries preserve the original timestamps.
Settlement acknowledges notification delivery only, never decisions or approvals.
`read_notice_ids` (#3068) re-reads up to 32 exact retained notices of the caller's seat, settled or not, in the requested order and `fields` form: a summary drain settles a notice but keeps its full `state`, so a long satellite report is expanded with `read_notice_ids:[<notice_id>], fields:"full"`. It is read-only (settles nothing, drains no other notice, moves no cursor, returns no cursor) and refuses an unknown, retired or foreign ID with `manager_notice_not_authorized`.

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
source outside a clean live worktree, activity inside the last 24h (the refusal reads `manager_v2_retention_recent_activity: eligible_at=<RFC3339>`, the end of the window, and `ListSessions` rows carry the same `eligible_at`, so a bulk pass skips those rows with a reason instead of issuing a refused call each, #1794), or a live
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
`GetHarnessManagerState`; `ConfigureHarnessManagerWithPolicy` (#2237, operator-only,
never delegable) saves the appointment scope and the policy in one transaction, so
a refused grant leaves no appointment behind (the new-session modal and the global
workspace's create flow use it); `AnswerHarnessManagerDecision` answers an exact
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

`AgentManagerInspect {section:"overview"}` bounds each intent row's
`work_versions` to the newest 16 unfinished items (updated time descending,
key ascending for ties), preserving each entry's key/version/acceptance fields.
`work_versions_format:"work_summary_v1"` versions this projection;
`work_versions_count` counts the retained Work window, `work_counts` has disjoint
pending/partial/accepted/integrated/retired counts, and
`work_versions_unfinished_count`, `work_versions_limit` and
`work_versions_truncated` describe the preview. These are current Work facts,
including topology-owned work, rather than the coordinator's stored snapshot.
Retired rows are settled history and excluded from program/product progress
counts. `work_query` is a ready-to-use request for the full paginated `work`
section (start with its own cursor); the retained history window is unchanged.
The TUI and desktop render these generic row fields without assuming that the
preview array is the complete ledger. Stored intent and event payloads are
unchanged. Topology settlement already integrates admitted accepted sources
that reached `rolling` and retires unlanded rows on success/failure/cancellation
(#2607); recovery backfills historical executions through the same path.

`AgentManagerInspect {section:"workers"}` defaults to actionable workers,
newest created first: live sessions, pending ledger work, queued/running jobs,
pending rolling landings, unanswered questions, and recent (seven days)
failures, interruptions or completed sandbox/artifact results without delivery
evidence. Explicit pending obligations remain visible beyond that window.
`include_history:true` returns all retained workers in the existing creation
order. Both views keep insertion horizons and recheck scope and status on every
page; their cursors cannot be exchanged. The recent cutoff is fixed for the
cursor. Native tools and the agent CLI use the same catalog request schema.

DB-native `request_review` keeps the Work's original author and exact source
commit after sandbox reclamation (#1431). A durably purged custody root permits
the review to fork from its recorded canonical repository when the commit is
locally reachable from a ref and authored after the recorded custody base.
Admission, launch and infrastructure retry authenticate the recorded repository
identity; generic creates and continuations still require live custody. Missing
or quarantined roots do not use this fallback. A failed repository proof names
the reclaimed sandbox in `manager_review_reclaimed_source_unavailable`.

*Stage evidence from a job the caller ran (#169).* A stage's `evidence` names a
worker session (`source_session_id`) and binds the Work to that session's custody,
so a manager or lead that ran `scoped_test` itself has no session to cite (naming
its own id is refused `manager_v2_session_out_of_scope`). Use
`{"update":"stage",...,"job_evidence":{"job_id","source_commit","artifact_path","artifact_commit"}}`
instead; it excludes `evidence` (`manager_v2_job_evidence_conflict`) and names no
session. The daemon reads everything else itself and refuses with a stable code
otherwise: the job must be the authenticated caller's own and recorded under the
ledger's target project (a foreign, missing, other-project or legacy no-project id
is `manager_v2_job_evidence_job_unavailable`, #2710), a terminal `scoped_test`
(`..._job_running`, `..._kind_unsupported`: a `qa_lane` or package job proves no
tested bytes), with a recorded receipt (`..._receipt_missing`) whose `head` equals the
supplied `source_commit` (`..._head_mismatch`). `artifact_commit` must be in the
caller's own committed history (`..._artifact_unreachable`) and hold `artifact_path`
under `thoughts/` as a regular file; the tested commit and the report may differ.
The recorded `job_evidence` (stage record, inspectable) carries the daemon-observed
owner, tested commit, artifact digest, exit code, plan and a verdict: `green`
(exit 0), `baseline_red` (every failure is named by an open known-failure record,
listed with its Issue), `red` (a failure none names) or `incomplete` (a build or
guard failure, timeout or partial run). `passed` needs green or baseline_red;
`failed` needs red or incomplete (`manager_v2_job_evidence_state_mismatch`
otherwise); `partial` and `blocked` take any verdict; `pending`, `running` and
`unknown` take none. A Work that already records a source commit refuses a job that
tested another (`manager_v2_source_changed`); job evidence never sets the Work's
source session or commit. It is reported evidence like any stage note: it is never an
`admission`, so it satisfies no acceptance gate and `evidence_state` stays `unknown`;
acceptance still needs the independent review path.

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
| `blocks` | `{launches: bool, epic_ids: [Epic], execution_ids: [execution], work_key, request_id, reason}`; `launches` is true while the record is `pending` or `answer_queued` and not archived. Daemon-filed `topology:` decisions name only the asking execution in `execution_ids`: that executor pauses new launches until the ruling, while other executions and manager launches in the Epic remain available. Other records name `epic_ids` (create_session under those Epics refuses `manager_v2_pending_operator_decision`). |
| `history` | bounded audit trail `[{event: asked \| reasked \| answered \| declined \| ruled \| withdrawn \| archived, at, actor, note?}]` |
| `created_at`, `age_seconds` | age of the question |
| `stale`, `stale_reason`, `stale_after_days` | pending and (`manager_gone` or older than 7 days) |
| `archived` | the record is archived history |
| `owner_manager_session_id`, `scope_version` | the owning ledger, for the archive request |

A topology handoff's `resource` blocker defaults to `spend`, answerable only
by the operator. It becomes manager-answerable only when the blocker, evidence
or question explicitly names tooling and none names money or another real gate.
Tooling vocabulary (whole words/phrases, case-insensitive, punctuation normalized):
`tool copy`, `stale binary`, `stale tool`, `redeploy rsid`, `restart rsid`,
`cargo-slot`, `target-pool`, `build cache`, `target dir`, `disk`, `sandbox`,
`worktree`, `installed tools`, `deploy`.
The resource-only money scan recognizes `USD`, `EUR`, `GBP`, `JPY`,
`dollar(s)`, `euro(s)`, `pound(s)` (before or after amounts, or alone),
`cost`, `costs`, `costing`, `price`, `priced`, `paid`, `charge`, `charged`,
`credit`, `credits`, `billed`, plus the existing gate scan (including currency
symbols and buy/pay/rent wording). Unknown resource phrasing stays operator-owned.
The wider scan does not change ordinary non-resource manager questions.
Actual spending, credentials, main or release and data deletion remain
operator-only. Starter implement briefs classify broken tooling as
`technical_impasse` and require amounts and currency for real spending requests.

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
boundary. This finite deadline is intentional. A session parked on its own wake
or daemon job (#2144: `Completed`, no final `PIPELINE HANDOFF`, an armed
`resume`/`when`/`on_terminal` wake or a non-terminal job it owns; the #2212
liveness predicate, `Store::parked_session_pending`) accepts mail instead of
refusing `agent_message_target_terminal`: the mail stays `queued`, the
dispatcher holds it (`AwaitingResume`) and the reconciler does not settle it,
and it is delivered through the normal claim once the wake resumes the session.
An omitted deadline is 6 hours when the recipient owns an armed result wake
or non-terminal daemon job, including while its provider turn is still live
after arming the wait (#2411). This bounded fallback also covers wakes without
a deadline and multiple pending wakes/jobs; it never renews an accepted
deadline and does not promise delivery beyond six hours. Unparked recipients
retain the 30-minute default; explicit caller deadlines always win. A session with a final handoff, or nothing left to resume it, is
terminal: acceptance is refused, and mail queued while it was parked settles
`failed` (terminal before delivery). A running session also takes
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

Coordinator budget visibility (#3079): a whole catalog for a coordinating
seat carries optional `coordinator_budget` and the same explanation in its
guidance. Manager overview/workspace seat projections carry the identical
serde-defaulted field. Operator-only `GetDaemonConfig` adds
`coordinator_context_budgets` (session ID plus budget for each current seat);
the TUI Context Rotation coordinator-cap details consume these values verbatim
and refresh on model/window changes. The budget names provider/model, the
active window and evidence, the measured window when observed, the raw cap,
unit and exact model/provider/global source, succession percentage and source,
effective tokens/percentage and binding setting. It explains existing policy;
it changes no setting, default, measurement or rotation admission. Missing
Codex usage remains unknown; overview reads use the same live-tip session
projection for parent and self views.

Transport admission (#1711 Phase 2b): the RPC dispatcher resolves a token once
into a registry-issued `BoundAgentCaller` (session, launch witness, fresh-launch
classification). A task-local scope carries that non-secret copy through
Agent/read/hook handlers; it carries no registry guard and is not inherited by
spawned tasks. Catalog snapshots, scoped finite reads, wake list/cancel,
`AgentSendMessage` enqueue, and boundary-mail claim/confirmation recheck the
original witness at their snapshot, synchronous commit or exact claim. Store
(or the pending-confirmation ledger) comes before A6; guards end before
serialization, socket writes, subsequent locks, awaits and effect dispatch.
A read/claim committed before revoke may finish; a stale copy cannot start the
next admission. Background settlement retains its existing claim fences.

Adopted transport establishment remains unavailable in production. Both the
legacy UUID resolver and RPC dispatch refuse adopted bindings with
`agent_verb_unknown_session_token` for Agent verbs, scoped reads and hooks.
Unscoped reads (`GetHealthStatus` and `GetDaemonCapabilities`) carry no caller
authority, so dispatch does not resolve their bearer or recheck it after the
handler; they remain available even when the supplied token is stale, foreign
or adopted. The seven guarded Issue controls render transport admission
refusals as their existing typed `authority_denied` envelope, before decoding
params or adding schema diagnostics; this also applies to unknown and revoked
bearers. The six scoped topology controls likewise retain their existing
redacted `authority_denied` envelope at admission. Other fresh-launch effect
families keep their existing paths;
this gate does not declare those paths fenced for adoption. A later activation
gate must explicitly admit only converted routes after owning-lock liveness,
custody and semantic-grant checks. No durable enrollment, persistence, setting
or default changes are introduced here.

## Rolling merge queue (#1007)

`AgentEnqueueLandingSource` admits a source (in `AGENT_VERBS`, the closed
catalog and `rsi-rpc`; no native tool); `AgentListRollingQueue` and
`AgentCancelRollingQueueEntry` (#1964, below) read and withdraw entries. The daemon
binds the caller from the token; only the current appointed manager and the
current lead of the caller's Epic may enqueue (`queue_not_authorized`
otherwise). Request `{source_commit, test_filters? | derive_filters?, fixes_issue?, fixes_tests?, idempotency_key, supersedes?}` (claimed fixes: below); `derive_filters` runs `scripts/check-touched-shards --filters-only --json` on the source (`crates/rsid/src/rolling_queue.rs` `derive_test_filters`), normalizes (dedupe focused rsid/rsid-store filters across shards, split filters over 256 bytes, no count cap) and a topology `land` node with no `test_filters` derives the same way (#1807, #1781, #1786); explicit filters stay capped at 32 of 256 bytes. Refusals:
`queue_disabled`, `queue_source_invalid`, `queue_source_session_out_of_scope`,
`queue_filter_invalid`, `queue_tip_read_timeout`, `queue_probe_timeout`,
`queue_duplicate_source` (a live entry for the commit exists),
`queue_idempotency_conflict`. Conflict warnings (#2118): the receipt of `AgentEnqueueLandingSource` may carry `conflict_warnings`, a typed list from an in-memory `git merge-tree --write-tree` of the new source against the rolling tip (`queue_conflicts_with_rolling`) and against each live (`queued`/`admitted`/`gating`) entry of the same project ahead of it (`queue_conflicts_with_entry`, naming `entry_id`, `entry_sequence` and up to 8 conflicting `files`). It is a warning, never a refusal: the entry is queued either way, since an entry ahead may still be withdrawn or refused; the enqueuer can have the worker merge `origin/rolling` first or cancel the entry, instead of finding the conflict after a gate (`queue_batch_merge_conflict`). A probe git cannot run (object missing, old git) yields no warning (`rolling_queue::queue_conflict_warnings`). Enqueue latency bound (#2907): `AgentEnqueueLandingSource` is an admission step and returns in seconds under load. The filter derivation and the conflict scan run side by side, so the call takes the longer of the two, and each has a hard wall bound. The conflict scan stops at `ENQUEUE_CONFLICT_SCAN_BUDGET` (4 s for the whole scan, each probe under the time left); a scan cut short ends its warnings with one `queue_conflict_scan_incomplete` warning naming how many probes it skipped, so a missing warning never reads as "no conflict". `derive_filters:true` waits at most `ENQUEUE_DERIVE_WAIT` (4 s) and one call is enough (#2993): a derivation still running then does not refuse the call. The source is probed without filters, the entry is recorded `queued` with `filters_state: deriving` (the reserved stored item `filters_state=deriving` in `test_filters_json`, no schema change; `test_filters` is empty until it lands) and returned, and a background task (`rolling_queue::finish_filter_derivation_in_background`, at most one per entry, re-attached to every pending entry by the queue loop's per-tick sweep so a restart loses none) finishes it. The lander (`claim_rolling_queue_batch_lanes`, `queued_rolling_queue_repo_paths`, `rolling_queue_lane_candidates`) never admits an entry carrying that state, and a pending entry does not hold up later ones. When the derivation ends the filters replace the marker (`finish_rolling_queue_filter_derivation`, a CAS on the entry still being pending, so a cancelled entry is left alone); a derivation that fails (the checker did not start, timed out after 180 s or printed no usable receipt: `queue_filter_derivation_failed`; a derived filter that provably selects no test: `filter_matches_no_tests`; a rolling-tip read that never completes: `queue_tip_read_timeout`) settles the entry `refused` with that code (`refuse_rolling_queue_filter_derivation`, lander friction like any refusal) and writes the enqueuer's single outcome wake in the same transaction; a second settle writes nothing. A replay under the same `idempotency_key` returns the same entry (never a second one) whether its filters are pending, final or refused, without asking the derivation again. A derivation that fails while the call is still waiting (inside the 4 s) keeps the unfiltered-gate fallback it always had. The derivation is keyed by repository, source and rolling tip, so a retry joins it or reads its finished result for 10 minutes; it never falls back to an unfiltered gate once an entry waits on it. Callers never sleep, poll or call again for the filters. Every git read of the call runs under a deadline (#2912): the rolling-tip read behind the derivation wait shares the 4 s `ENQUEUE_DERIVE_WAIT` (a read that does not finish is refused `queue_tip_read_timeout`, retryable, nothing recorded, and stays andon friction, #2992: it is git trouble, not a derivation in progress), the source probe (existence, merge base, diffs, filter greps) shares `ENQUEUE_WALL_BOUND` (5 s from the start of the call, so it gets what the derivation left) and the conflict scan's rolling-ref resolve shares the scan budget. A stalled git is killed with its process group at the deadline; a probe it cut short is refused `queue_probe_timeout` (retryable, nothing recorded, same `idempotency_key`), never `queue_source_invalid`, and a cut-short ref resolve counts as a skipped probe in `queue_conflict_scan_incomplete`. The call therefore returns within `ENQUEUE_WALL_BOUND` plus a small margin whatever git does (the derivation itself runs in the background and is not killed). The operator path (`EnqueueRollingQueueSource`) still waits for the derivation and its source probe. Each call logs `enqueue phase:` lines (`derive_filters`, `derive_and_probe`, `conflict_scan`, `total`) with per-phase `*_ms`. Entries live in `rolling_queue_entries`
(provisional migration) in FIFO `sequence` order; a run is one `gating` entry plus a
`rolling_queue_batches` row. The runner (`rsid::rolling_queue`) claims the head
plus up to `rolling_queue_batch_size - 1` later entries of the same project and
repository while `rolling_queue_enabled` is on (#1945). The repository is the git
common directory of the entry's `repo_path`, not the path: every Issue worker lands
from its own sandbox worktree, and all of them share one object database. The daemon
resolves the queued checkouts outside the store transaction
(`Store::claim_rolling_queue_batch_by` only looks the keys up); a checkout git cannot
resolve keeps its own path and batches only with that path. The runner runs
`rsi-rolling-land` once with every member's `--accepted` source and the union of
their `--test-filter`s, and settles each entry once: the settling transaction CASes
`gating -> terminal` and inserts the owner's single `scheduled_jobs` resume wake
(`wake_job_id` is UNIQUE). Single-flight is one batch per lane (#1224): while a batch gates, the runner
resolves every gating and the first 32 queued sources to a package set
(`rolling_queue/lanes.rs`: the Cargo packages the diff against the rolling tip touches
plus their workspace dependents, read from the rolling tip's manifests) and
`Store::claim_rolling_queue_batch_lanes` claims a second batch (at most
`ROLLING_QUEUE_MAX_LANES` = 2, a `lane_claimed` event, `parallel_lane` on the claim)
only when every involved source resolved, the sets are disjoint, the project and
repository match, no entry carries a migration and the candidate shares a package with
no earlier queued entry it would overtake (FIFO among overlapping entries). A manifest,
`Cargo.lock`, script, tool or any file outside a package is the whole workspace and
shares the gate with nothing; `docs/` and `thoughts/` share it with everything. Both lanes
gate on the same base tip in their own private lander clones; the lander's stale-advance
reuse (`stale_delta`, #1940) decides at publication whether the other lane's advance
needs a re-gate, so the package sets only decide scheduling, never safety. A multi-member batch records a `batch_formed` event
(members, distinct checkouts, queue checkout), and the lander's `gate_timing=` line in
each outcome detail ends with `batch_size=N`, the sources that gate gated together
(a bisect step reports its own size).
`gate_timing` carries `thread_inputs=quota:Q,slice_use:U,host_load:L,host_threads:T,basis:slice|host|default` (#3225), what the gate's test threads were chosen from: with the lander's cgroup bound readable (the tightest `cpu.max` up its cgroup path, `rsi-build.slice`) and sampled for a second (`cpu.stat` `usage_usec`), threads are `clamp(floor(quota - slice_use), 4, min(16, quota))`, so an idle 8-CPU slice runs eight whatever the whole host's load; without it the #2086 host rule (`clamp((host threads - load1) / 2, 4, 16)`) applies. `gate_timing` carries `check_steps=name@secs,...` (#2424), the wall seconds of each step of
`checks_secs` in order (`static-guards`, `python-tests`, `check:<packages>`,
`release-check:<packages>`, `prebuild-beside`), and the lander log prints each as a
`gate_step name=... secs=...` line. The `python-tests` step follows the same "no new failures relative to rolling" rule as the Rust shards (#2576): a failing suite is triaged per test; a test that passes when rerun alone is a flake (`candidate_flake=<id>`, in the outcome's flakes), one that still fails is run on the base and, if the base fails it too, is a baseline red (`python_base_red=<id>`), and only a failure the base does not share refuses `new test failures relative to rolling`. A failure that names no single test (import or `setUpClass` error), a hung or unavailable runner and a claimed `fixes_tests` Python test that is red (#2167; claim = dotted id, `Class.test` or `test`) still refuse. The dev-profile `cargo check -p P --all-targets` of
every selected or dependent package runs as one invocation, and so do the release-build
`cargo check -p P` checks (they stay apart: different feature sets, #1970); a failed
merged release check reruns the release check per package (failure path only,
`release-check-rerun:<package>` steps) and refuses `release_build_failed:<crate>` for
the first package that fails standalone (#2445); the crate cargo names is the
fallback when none fails alone. The merged dev-profile check unifies features, so a
crate that compiles only because another selected package enables a dependency
feature passes the gate (#2444); the standalone guarantee lives in the automatic
full-library QA recipe instead (`make rolling-library-qa` runs
`scripts/check-standalone-packages.sh` after the tests: `cargo check -p P
--all-targets` for every workspace package, each failure printed as
`standalone_check_failed:<package>` and a nonzero exit, so it files like any QA red).
Warm gate targets (#2467, `crates/rsid/src/lander_warm_target.rs`): before #2467 each gate
built into a fresh pair of cargo targets (`test`, `check`) that died with its workspace, so
every third-party and workspace crate compiled again (no `RUSTC_WRAPPER` or sccache runs
there: the daemon environment sets neither). The warm path is ON by default: it shipped
opt-in (#2467) until the stale-artifact defects #2490, #2491 and #2508 from reviews #2489 and
#2507 were fixed. `RSI_LANDER_WARM_TARGET=0` (also `false`, `off`, `no`, `disable`,
`disabled`, any case) opts out and every gate builds cold as before #2467. When enabled, a queue
lander leases one of `RSI_LANDER_WARM_SLOTS` (default 2, 0 disables) slots under `~/.rsi/queue/target/warm/` with an exclusive `flock` on `slot-N.lock`
for the whole run; no free slot (or a lander whose target is inside the repository, or the
detached canary runner) builds cold into private targets as before. The candidate worktree
builds into `slot-N/test-target` and `slot-N/check-target`; the base side and every other
tree keep private targets. The lander prints `warm_slot=N` and, per gate,
`warm_target slot=N state=cold|warm|toolchain_changed|over_size bytes=.. manifest=..
restored_files=a/b restored_dirs=c/d checkout_changed=bool`, then `warm_target=committed files=N`
after the build phase. Reuse of unchanged *workspace* crates works through mtimes (Cargo's freshness):
`manifest.json` holds the git blob id and mtime of every tracked file and directory of the
tree the last successful build phase (compile checks and prebuild both green) saw; the next
gate gives files whose blob id is unchanged their recorded mtime back. Cargo fingerprints the
bytes it reads through a path and follows symlinks, so a blob id alone does not fix them
(#2508): the manifest also records every tracked symlink (path to the blob id of its link
text), every tracked `.gitattributes` (path to blob id), a digest of any tracked path that is
not UTF-8, and a digest of the git settings outside the tree that convert files at checkout
(`core.autocrlf`, `core.eol`, `core.attributesfile`, `core.symlinks`, `filter.*`, and the
`info/attributes`, global and system attribute files). When any of them differs from the
candidate's, no mtime is given back (`checkout_changed=true`, `restored_files=0/N`) and the
whole workspace rebuilds, exactly as a cold gate would. `bind` also checks that the cloned
HEAD equals the candidate worktree's HEAD; on a mismatch it fails and the gate builds cold
(`warm_target=unavailable reason=..`). The lander runs every gate cargo command with
`RSI_BUILD_SHA=gate-build` (unless the spec names one): gate binaries are never deployed, and
`crates/rsid/build.rs` otherwise read the clone's `git rev-parse HEAD` and tracked
`.git/HEAD`, which a re-clone rewrites at every bind, so its script reran and `rsid` plus every
crate above it recompiled whatever the candidate changed. A pinned `RSI_BUILD_SHA` makes the
script skip git and its rerun triggers; release and deploy builds keep the real SHA.
Measured floor (#2467, `cargo --timings`, 4 jobs, one `rsid`-only edit in a warm pair of targets): only `rsid` recompiles. The test target is a serial chain, `rsid` lib built with `test-seam` for the self dev-dependency (44 s, 37 s of it single-threaded frontend) then `rsid` lib test (109 s): 153 s with or without the checks beside it, so CPU contention is not the cost. The check target is the same shape (28 s then 66 s, 95 s total), and `check -p rsid` (default features) recompiles `rsi-common`, `rsid-store` and `rsid` under a different feature set (32 s of the sequential release check). Cold, the same pair took 172/241 s. The remainder is `rsid` itself on a stable toolchain (no parallel frontend), so the < 130 s prebuild target is bounded by the crate decomposition #2308. The prebuild also builds each non-rsid package a plain `cargo test -p PKG` guard runs (`rsi-common --lib` pulls `criterion`/`insta` features and recompiled 36 crates, 34 s, after the checks) beside the checks.
The manifest is taken
off disk when a gate binds and written back only after a green build phase, so a failed or
killed gate leaves none and the next gate rebuilds every workspace crate (third-party crates
stay warm). A source that reads `env!("CARGO_MANIFEST_DIR")` outside a comment keeps its
checkout mtime and recompiles. A toolchain change (`rustc -vV` or `rust-toolchain.toml`)
empties the slot; a lockfile change does not (Cargo keys third-party artifacts on the exact
resolved package). A slot over 16 GiB is emptied at its next lease, and the daemon's
build-cache reclaim pass (`session/launch.rs`, next to the warm target pool GC) removes
unleased slots idle for 72 hours (10 minutes under disk pressure). Expected effect on a
typical rsid-only gate (observed before, 12 gates 2026-10-14: `prebuild-beside` 196-238 s,
`check:rsid` 150-184 s, `release-check:rsid` 52-63 s, candidate tests 107-130 s, wall
316-370 s): the first gate in a slot or after a toolchain change costs about the same; the
next ones skip third-party crates (about 50-60 s of each target, measured from fingerprint
mtimes of a live gate) and the unchanged workspace crates (`rsid-core`, `rsi-common`,
`rsid-store` when untouched, about 80 s of the test target), so `prebuild-beside` and
`check:` should fall to roughly 90-130 s and wall to roughly 230-260 s; a gate that changes
`rsid-store` keeps that crate's rebuild. The manager measures after deploy from the
`warm_target` lines and `check_steps`.
For an unfiltered library gate, `gate_timing` also records
`selection=closure selected=N reached=M`, `selection=bounded selected=N reached=M`,
or `selection=everything reason=...` (#2092). Above 400 reached tests, the
selector keeps the nearest tests by call-graph distance, then full name; every
direct observer and the complete minimum-distance tier survive even when they
exceed that soft budget. Reached bin and integration test targets are ranked
the same way by their nearest affected item (#2094): targets that own, mount or
read the changed code or call it directly (distance 0 or 1) always run, the
nearest others fill a budget of 5, and an omitted target appends
`targets=bounded selected=N reached=M`. A gate that excluded the slow tier (#2270) while
its diff touched `rsid` or `rsid-store` appends ` slow_tier=excluded` whatever its selection
(explicit filters included), and the queue treats it as a full-QA obligation like the bounded
forms (#2286). Long exact filtersets use a temporary nextest config,
so Windows command-line length does not cap coverage.

Base reds reach the manager (#2698): a gate that publishes over a base red prints `base_red=<test>`, its `known_red=` annotation and `base_red_command=<test><TAB><shard command>`. For each red the snapshot does not own (`:new` or no annotation; `#N` and `known?` are owned, and a landing's own `fixes_tests` is excluded) the queue files one `qa`+`bug` Issue (`Store::report_gate_base_reds`, `rolling_queue/gate_base_red.rs`) naming the test, the shard command, the base SHA and the gate log (#2865: the unowned reds one shard command observed are ONE Issue, whose `Failing tests:` line names every test so `fixes_issue` claims each, with a shard-wide note from three tests up and at most 64 tests per Issue; a red with no recorded command stands alone), unless an open `qa` Issue or signature Issue of the project already names the test (the Issue is then reported as `already open`). The published outcome detail carries `base_red_issue=#N test=<test> (filed|already open)`, so the owner's wake names it, and a `gate_base_red` batch event records it. Filing is best effort after the push: a store failure logs and names nothing. Coverage: the async full-library QA (`make rolling-library-qa`) runs only `rsid`, `rsid-store` and `rsid-remote-read`; `rsi-common` integration targets (e.g. `research_corpus`) are covered by the hourly sweep's `rsi-common` lane (`scripts/run-rolling-qa.py`, `cargo test -p rsi-common`), not by the post-landing run, so a base red there is first seen by whichever gate selects it.

Each lander run keeps its bounded output at `<queue workspace>/logs/<batch>-<ms>-<seq>.log`
(newest 40 runs retained) and, beside it, a `.timings.tsv` sidecar
(`run, binary, test, seconds, status`, one row per completed nextest test of every
candidate, base, retry and remote gate command; the file stops at 4 MiB and each
command adds at most 20 000 rows, #2085). The daemon creates both before the lander starts and passes the sidecar
as `RSI_LANDER_TEST_TIMINGS`; a gate stopped by `queue_gate_timeout` keeps the rows
completed so far, and each pair is rotated together. No table or migration holds it.

A topology land node's entry (replay key `topology.node:<execution>:...`, its
execution still live) settles without the queue's owner wake (#2386): the land
node reads the settled entry, the execution records the outcome (a refusal
fails or blocks it) and its one `topology_execution_settled` wake carries it.
Once the execution is final, or for any hand-enqueued entry, the owner wake is
written as before.

A separate daemon task discovers published bounded gates from their immutable
outcomes and coalesces pending ancestors into one pinned rolling snapshot per
repository; a gate with `targets=bounded` owes the same run. Every queue
landing reaches the lander with explicit `--test-filter`s, so for a filter set
the queue derived (`derive_filters:true`, or a topology land node without
filters) the lander records `selection=closure reason=explicit_filters`; the
queue therefore writes a `bounded_qa_obliged` event (`reason:
derived_filters`) for each such member just before it settles published
(#2112). Filters the operator or an agent named explicitly stay exempt; after a
daemon restart an entry still queued is obliged like a derived one (the
explicit mark is in memory), and an unfiltered gate is judged by the lander's
own evidence. A project's next start waits `rolling_queue_full_qa_interval_mins`
(0-240, default 60) after its previous dispatch, while a deploy drains, and
while host-load admission holds; landings published meanwhile coalesce into
it. The job unit runs at `CPUWeight=20` and nice 10 (systemd's default weight
for other job units and the daemon's gates is 100) inside the same CPU
ceiling, and each `bounded_qa_requested` receipt lists every landing the run
covers. The
`rolling-library-qa` durable recipe runs both `rsid` and `rsid-store`
libraries and every bin and integration test target (#2094) without shard
features or fail-fast (180-minute execution
cap, half the host threads clamped to 2..12 CPU cores, #2087; nextest threads
stay within the same ceiling). Queue events `bounded_qa_requested` and `bounded_qa_finished`
record the job, exact SHA and result; a failed/incomplete job atomically files
one `qa` Issue naming every included landing and the job log. Test reds carry
the `bug` label; timeout/admission/interruption outcomes carry
`coverage-incomplete` and an incomplete-coverage title instead. Publication
recovered without a gate receipt is marked `selection=unknown` and conservatively
gets the same full QA. An eight-hour overdue Issue reports an admission/outage
gap while keeping the obligation pending. #2149: nobody waits on this run, so
its build admission (governor slot, artifact and sandbox lock waits) lasts up to
its own job timeout instead of the ordinary 10-minute cap
(`agent_jobs::admission_allowance_secs`: cargo-slot gets
`RSI_CARGO_ADMISSION_MAX_SEC`, the daemon's wait accounting and the unit's
runtime cap carry the same allowance; execution keeps its own timeout and the
unit stays in `rsi-build.slice` at its low weight). An attempt that still ends
`job_admission_timed_out` or `job_start_timed_out` (capacity; no test ran) files
no Issue and no `bounded_qa_finished` receipt: each covered landing gets one
`bounded_qa_capacity_deferred` event (job, SHA, refusal, `ran_on`, covered
landings) and is pending and unbound again, so the next start (after the
interval) coalesces it with later landings under a new job key and satellite run
id (one per prior deferral). Test reds and other incomplete outcomes are
unchanged. The task continues while the queue
is paused; it respects deploy drains and reconciles durable jobs after restart.
It reuses one registered build target and removes completed source snapshots.
#2646: a passing receipt (the run tested every landing it covered) is the record of the
latest full-library-green `rolling` SHA: `AgentListRollingQueue` returns the newest as
`qa_green` (`sha`, `passed_at`, `job_id`), scoped like the entries it lists (and to
`project_id` when given), `null` before any pass. The old `qa-green.sha` pointer file is
retired; nothing advances it.
#2743: rolling red must reach the seat. (1) When the daemon files a `qa` Issue (a red or
incomplete full-library run in `finish_bounded_qa`, or a base red a gate reports) the current
project manager gets one project-wide `ledger_change` notice (`record_kind: qa_issue`, subject
`qa_issue:<Issue number>`, fixed version, so one per Issue and a replayed settle adds none):
`issue_number`, `title`, `outcome` (`test_red`/`coverage_incomplete`), `rolling_sha`, up to 8
`failing_tests` and `failing_tests_total`. It is carried to a successor seat like the other
project-wide notices. (2) `AgentManagerOverview`/`AgentGlobalOverview` give each project
`rolling_health` (`RollingHealthV1`): `open_qa_issues` (capped at 8, `open_qa_issues_total`
the full count), `qa_green` and `qa_green_age_secs`; a project manager's context-cap successor
prompt carries the same as a `## Rolling health` block. (3) `AgentRequestDeploy` of a SHA that
`qa_green` does not cover (the SHA or an ancestor of it, ancestry read in the project's repo)
is accepted and its receipt carries `qa_warnings` with a `not_qa_green` entry naming the open
`qa` Issues; it is recomputed on a replay and never refuses a deploy.
#2120: each start is first offered to a qualifying project satellite (enabled,
paired, read- and dispatch-enabled, scoped, healthy observation under 15
minutes, live identity with `health.platform.os` equal to the hub's
`std::env::consts::OS`, `qa_run` and a `qa_admission` that admits: the
satellite's build governor floor `min_free_disk_gb`, never below
`SATELLITE_QA_MIN_DISK_FREE_BYTES`, its other governor gates, host-load
admission and no other full-library job, rechecked at start, #179) with the operator-only `RequestHubQaRun`
wire method and a run id derived from the anchor landing (and its prior capacity deferrals, #2149), so a retry or a hub
restart replays the same satellite job. `bounded_qa_requested` then records
`ran_on: satellite` (`peer_id`, `peer_label`, `remote_job_id`,
`requested_at`); a satellite run in flight holds its project like a hub job,
the interval and coalescing are unchanged, and the hub polls it at most once a
minute (`start: false`; `satellite_qa_not_found` or no answer 240 minutes
after dispatch settles incomplete). A start whose request was sent but whose
reply was lost or malformed, or which the satellite answered with an error that
does not prove a non-effect, is uncertain (#180, #2156): it binds that peer
and run with `remote_job_id: null`, nothing else is tried, and each poll
replays the idempotent `start: true` for the same run id (no fresh-placement
admission check, so a satellite made busy by its own original run still
replays it) until the satellite reports it. Only the refusals in
`SATELLITE_QA_PASS_THROUGH` (including `satellite_qa_launch_failed`: nothing
is recorded or the launch settled failed with its unit stopped) are definite
non-effects; a launch error that left the satellite's job running answers with
that job's receipt instead. The hub records a `bounded_qa_dispatching` intent
(peer, run id, commit) per covered landing before it sends a start and a
`bounded_qa_dispatch_withdrawn` record only after a definite non-effect; an
intent that has no `bounded_qa_requested` binding (the hub stopped after the
satellite accepted the start, #2155) is bound to that exact peer with
`remote_job_id: null` on the next pass and reconciled the same way, so one
obligation never runs on two hosts.
With no qualifying satellite, or every candidate refusing (a definite
non-effect), the hub runs it and records `ran_on: hub` with the reason
(`no_satellite_peer`, `<label>: satellite_stale`, `<label>:
satellite_qa_disk_low`, `<label>: satellite_qa_busy`, ...). #2935: missing
platform fails closed with `<label>: satellite_platform_unknown`; another OS
gives `<label>: satellite_platform_mismatch`. `GetSatelliteIdentity` carries
the additive, optional `health.platform: {os, arch}` from `std::env::consts`;
older identities still decode. Architecture does not restrict QA placement.
The TUI peer rows and `ListSatellitePeers.observation.platform` show the last
polled platform (process-local, unknown after a hub restart until polled),
`AgentGetDaemonInfo.satellites[].platform` shows the live platform, and each
peer rejection reason appends `(platform: <os>/<arch>)` or `(platform: unknown)`.
Manager satellite inspection rows also carry `health.platform`.
Existing run polling and uncertain-start reconciliation skip fresh platform
admission, preserving the binding to the original run. Every
`bounded_qa_finished` receipt carries the placement, `exit_code`,
`failing_tests` (at most 50, plus `failing_tests_total`), `log_path` and the
covered `landings`; the Issue evidence names where it ran. Satellite receipt
text is sanitized before it reaches the Issue (test-path and path alphabets,
code-shaped refusals).
#2176: the satellite run is a guest that must not block deploys or starve
the satellite's workers. Deploy wins: a satellite whose live identity reports a
deploy `staged` or `restarting` (`health.last_deploy`, read even from an older
build) or whose admission says `satellite_qa_deploy_pending` (it rechecks its
own live deploy row at start; listed in `SATELLITE_QA_PASS_THROUGH`) is not sent
the run; it is not asked again for a minute, and when no other satellite takes
the run it waits for that deploy (nothing bound, no hub run) instead of
falling back. A staged deploy on the satellite stops a running hub
run on the next agent-jobs tick, without `interrupt_workers` or the job grace
(`agent_jobs::cancel_test_jobs_for_deploy`, refusal `interrupted_by_deploy`),
so the deploy reaches its quiet point. The hub settles any run (satellite or
hub) a deploy stopped as the #2149 deferral with refusal `deploy_preempted`:
no Issue, no finished receipt, obligation re-armed under a new run id. Disk is
returned: once a hub run settled (success or not) and no full-library job is
queued or running, the satellite's agent-jobs loop
(`satellite_qa::reclaim_settled_runs`) removes every directory in the shared
QA target (`release_target::reclaim_build_output`, under its admission lock,
keeping the allocation's record, `CACHEDIR.TAG`, `.rustc_info.json` and lock
files) and the run's snapshot. Before removal it measures the target and, for a
run that built everything and ran its tests, records the size in
`<jobs>/rolling-library-qa/.hub-run-target-bytes`. The admission's
`target_bytes` reports that size, or `SATELLITE_QA_DEFAULT_TARGET_BYTES`
(25 GiB) before the first measurement. A satellite qualifies only with free
space of at least its governor floor (`min_free_disk_gb`, what its workers
need) plus that size; the satellite and the hub (`admits()`, assuming the
default for an older satellite that reports none) both check it. Otherwise the
hub runs it inside `rsi-build.slice` with `ran_on: hub` and the reason
`<label>: satellite_qa_disk_low`. A satellite's operator sets its worker floor
with `governor_min_free_disk_gb`.
 A batch from several checkouts first checks
that every member commit exists in the queue checkout; a member whose commit is
gone (its sandbox was reclaimed and the commit pruned: same common directory means
same object database, so there is nothing to fetch) settles `refused` with
`queue_source_unreachable` and a `source_unreachable` event, and the rest gate. Restart reconcile settles a `gating` entry whose
source is an ancestor of `origin/rolling` and re-queues the rest. An out-of-band
push (a direct `rsi-rolling-land`, an operator merge) moves rolling under a gate
without stopping it (#1940): the lander (`RSI_LANDER_EXACT_GATE=1`) classifies the
advance (`rsi-rolling-land/stale_delta.rs`), runs again only the test targets, compile
checks and Python suites the advance selects, and reuses the candidate's evidence for
the rest. An advance reuses the whole gate when `scripts/check-touched-shards
--filters-only` on it selects nothing the candidate selected, it changes no file of the
candidate's diff and no crate the candidate's tests depend on. A shared file, a
manifest or toolchain change, an unplaceable path or an unavailable selector run
everything again. The re-runs are bounded by the wall time the batch has left
(`RSI_LANDER_REGATE_BUDGET_SECS`, the batch deadline), not a count: a re-gate that
cannot finish inside it is refused `queue_out_of_band_regate_exhausted` before it
starts, and a batch that runs out is refused `queue_gate_timeout`. The lander's
`gate_timing=` line and its `stale_reason_N=` / `stale_retry_N=` lines name the reason
and how many commands were reused or re-run. There are no
hot-file waits, claims or seals; migration numbers are renumbered at landing.
A source already on `origin/rolling` (landed outside the queue) is never gated
(#1208): ancestry is re-checked right before a batch is gated, before each bisect
step and after a lander "already integrated" refusal, and such a member settles
`published` with the containing tip (an `already_landed` batch event). One
batch's gating is bounded by `rolling_queue_gate_timeout_mins` (30-1440, default
60, #1940); past it the lander's process group is stopped and unsettled members are
refused `queue_gate_timeout`.
The operator settings `rolling_queue_enabled`, `rolling_queue_batch_size` (1-8), `rolling_queue_speculation_depth` (0-2) and
`rolling_queue_gate_timeout_mins`, `rolling_queue_full_qa_interval_mins` (0-240, #2112)
and the unfiltered read `GetRollingQueue` are
operator-only: absent from `AGENT_VERBS`, `READ_VERBS`, native tools and the
agent CLI catalog.

### Operator landing (#2055)

`EnqueueRollingQueueSource` is the operator's way onto the queue; it is the only
alternative to a manual `git merge` and push, which skips every gate. It is
operator-only: absent from `AGENT_VERBS`, `READ_VERBS`, native tools and the agent
CLI catalog, and refused to a session-attributed caller.

Params `{session_id | branch, commit?, derive_filters (default true),
test_filters?, idempotency_key, dry_run?}`. Exactly one of `session_id` and
`branch` names the source; a branch resolves to the newest session whose sandbox
is on it (the entry needs a session, `queue_source_invalid` otherwise). `commit`
defaults to the sandbox `HEAD`. `dry_run` needs no key: it returns the resolved
`source` (`session_id`, `branch`, `commit`, `ahead` of the local `origin/rolling`,
`on_rolling`, `dirty`) and enqueues nothing. A real call runs the agent path's own
filter derivation, probe and insert (`derive_and_probe`, `insert_source`) and
returns `{source, entry, replayed}`; the same key replays the same entry. A source
already an ancestor of `origin/rolling` is refused `queue_source_already_on_rolling`,
a disabled queue `queue_disabled`. The settlement wake goes to the project's
current manager seat when there is one, otherwise to the owning session.

Clients: `rsi land <session-id|branch> [--commit OID] [--yes] [--wait]`, the TUI
`<Space>gl` / `:land` (first press shows the commit and arms, the second confirms
and enqueues exactly the shown commit; the entry's outcome is announced when it
settles), or `rsi-rpc EnqueueRollingQueueSource --params '{"branch":"rsi/<id>","idempotency_key":"k1"}'`.

### Claimed fixes of known reds (#2167)

"No new failures relative to rolling" lets a landing whose purpose is to make a
known red green publish while that red stays red (the base is red too). A fix
therefore states its claim: `AgentEnqueueLandingSource` takes optional
`fixes_tests: [NAME, ..]` (test names as the gate and the QA run report them,
`[A-Za-z0-9_:.-]`, at most 64) and/or `fixes_issue: <display number>`; the verb
reads that Issue (in the source's project) and adds every name its body lists on
`Failing tests:` lines (the qa Issue format, `a, b (and N more)`). Refusals:
`queue_fixes_invalid`, `queue_fixes_issue_not_found`,
`queue_fixes_issue_lists_no_failing_tests` (pass `fixes_tests` instead). Claims
combine with `derive_filters` or `test_filters`. No migration: the entry keeps
them in its `test_filters_json` as reserved `fixes_test=NAME` items, which the
store splits back out into `RollingQueueEntryV1.fixes_tests` (`test_filters` is
unchanged, and the claims are part of the replay identity); a lander that
predates claims refuses such an item as an invalid filter. The runner passes the
union of a batch's claims as `rsi-rolling-land --fixes-test NAME`; a bisect step
claims only its own members'. The lander requires every claimed test to show a
passing line (`test NAME ... ok` or nextest `PASS`) in a candidate run of its own
gate: a claimed test the candidate fails refuses at once, before any base run
(a base red excuses nothing), and one no candidate run ran green (the selection
never selected it) refuses at the end of the gate. Either refusal is
`claimed_fix_still_red: NAME, ..; <why>` with one `failing_test=NAME` line per
test, settles the entry `refused` with that code and the tests, and is bisected
like a red gate. A green claim prints `claimed_fix_green=NAMES`. Landings
without claims behave as before.

### Replacing a queued source in place (#1944)

`AgentEnqueueLandingSource` accepts optional `supersedes: <entry UUID>` from
`AgentListRollingQueue` or an enqueue receipt. Use a new `idempotency_key` and a
`source_commit` that descends from that entry's commit. Only the original
enqueuer (`source_session_id`, including when landing another session's sandbox)
may replace it, and both entries must have the same project. Unknown or
foreign-enqueuer/project entries are refused `queue_entry_not_found`; missing
ancestry in the new source checkout is `queue_supersedes_not_descendant`.

An immediate transaction rechecks ownership, project, the probed source and
`queued` state, then settles the old entry `superseded` with outcome refusal
`queue_entry_superseded` and inserts the replacement at its FIFO `sequence`.
The old row moves to a fresh sequence before its terminal state freezes it;
its id, source and replay key remain available. No migration or deletion is
needed. Listing is ordered by current sequence, so start a new page traversal
after replacing an entry. A failed insert rolls the entire replacement back.
The replacement inherits the old settlement-wake target (including a target
transferred on manager succession); superseding produces no wake. Only the
replacement's eventual settlement wakes the enqueuer. The original key replays
the superseded row, and the replacement key replays the replacement even after
the old entry has settled.

An `admitted` or `gating` entry is refused `queue_entry_in_flight` with the next
step: enqueue normally without `supersedes`. Its lander finishes first, and the
later entry gates its delta against the resulting rolling tip. A terminal
entry is refused `queue_entry_settled`; enqueue normally without `supersedes`.
The claim and replacement share the write transaction boundary, so a claim
that wins during the Git probe causes the same in-flight refusal, with no new
entry or wake.

### Predecessor seat tail and handoff reports (#2531)

`AgentReadSessionEvents` also admits the live holder of a project manager slot
reading an earlier seat of the same slot (`Store::manager_seat_reads_predecessor`:
the caller is the non-revoked appointment's `current_session_id`, the target an
earlier hop of its succession lineage, or (#118) a hop of the lineage of an
earlier appointment of the same project that an explicit reappointment
displaced, as recorded in the forward-only, never-deleted
`harness_manager_notice_scope_retirements` rows the appointment transaction
writes, newest 32 anchors; never a later seat, itself, another
project's seat, or any other verb). It is read-only and uses the one
`agent_verb_scope_denied` refusal otherwise. Pass the full id: a short prefix
resolves only inside the `authorize_agent_target` scope. The context-cap daemon
handoff (`rsi.daemon_handoff.v1`) gains `recent_reports_up` (the newest five
`AgentReportUp` rows sent by the seat or an earlier seat of its `continued_from`
chain: time, idempotency key, mailbox state, first line) and
`predecessor_last_message` (sequence, time, one-line excerpt), both redacted and
rendered as untrusted quoted data. Both default to empty, so a stored handoff
without them still parses.

The handoff also carries `worker_parent_epics` (#406): live Epic ids and titles
from Issue-worker launches by the seat or its earlier `continued_from` holders
(the same bounded 16-seat history), most recently admitted launch first,
deduplicated. Only running or succeeded Issue-bound `create_session` actions
count; the parent comes from the launch journal. Action Items tell the successor
to pass `parent_epic_id` after confirming the Epic is still in scope. This list
is a recovery hint, not authority, and defaults to empty for older documents.

### Short ids on read verbs (#2312)

`AgentGetJob` (`job_id`), `AgentGetStatus` and `AgentReadSessionEvents`
(`session_id`) and `AgentGetProgress` (`session_ids[]`) take a full UUID or a
unique prefix of at least 8 hex characters (hyphens optional): the form notices
and ledgers print. The daemon resolves the prefix inside the verb's own scope
(`session/agent_id_prefix.rs`; a job is the caller's own, a session is one
`authorize_agent_target` admits) before it decodes the typed request, in the
RPC handlers and the harness-native tools alike. `id_prefix_too_short` under 8
digits; `id_prefix_ambiguous: <prefix> matches N in your scope: <full ids>`
names only in-scope candidates; an unknown id and one outside the scope are
refused identically (`id_prefix_not_found`, `job_not_found` for jobs).
Mutating verbs (`AgentHalt`, `AgentCancelJob`, `AgentSendMessage`) stay
UUID-only.

### Checking a landing candidate (#2310)

`AgentCheckLandingCandidate {job_id?, source_session_id?, source_commit}`
(`crates/rsid/src/session/agent_landing_candidate_verb.rs`, verdict in
`rsi_common::agent_landing_candidate`) replaces the manager's raw read of
`agent_jobs` before `AgentEnqueueLandingSource`. It is read-only, has the native
tool `rsi_control_check_landing_candidate` and is in the closed catalog.

- **Verb gate.** Manager, Epic lead or a Status-mode portfolio seat
  (`portfolio_reader`, the list rule); anyone else, a worker included, is refused
  `candidate_not_authorized`. The request is validated first
  (`candidate_invalid_request`: `source_commit` is 40 lowercase hex, `job_id` is a
  full id or at least 8 hex characters, and a job or a session is named).
- **Scope** is the job owner's reach under `authorize_agent_target` (the same as
  `AgentGetStatus`). An unknown job, an unknown session and one outside the
  caller's reach are all `candidate_not_found`; a job-id prefix counts only the
  jobs inside that reach, so `candidate_job_id_ambiguous` never reveals a foreign
  job. A session's jobs are the ones it owns: a rotated worker's finished jobs stay
  with the predecessor, so pass `job_id` for those.
- **Verdict.** `ok` only for a finished `scoped_test` job whose receipt head is
  `source_commit` and whose `new_failures` is empty or `null` with a green run.
  Otherwise `ok:false` with one `refusal`, most blocking first:
  `candidate_job_not_terminal`, `candidate_admission_timeout` (the job's own
  `job_admission_timed_out`/`job_start_timed_out`), `candidate_receipt_missing`,
  `candidate_job_not_green` (the run failed before the baseline comparison named
  its reds, `new_failures: null`), `candidate_new_failures`,
  `candidate_head_mismatch`, `candidate_conflicts_with_rolling` (#348: the
  receipt's `merges_cleanly_onto_rolling` is `false` at the verified head, or a
  fresh read-only, fetch-free `git merge-tree` of `source_commit` onto the local
  `origin/rolling` conflicts; the fresh result wins, it is reported as
  `merges_cleanly_onto_rolling_now`/`conflict_paths_now`, `detail` names the
  paths, and a clean fresh probe lifts a stale receipt's conflict). A session with no `scoped_test` job is
  `candidate_no_verification_job`. A run that exits non-zero only on baseline
  reds is `ok`: the gate is no new failures relative to `rolling`.
- **Docs-only delta.** On a head mismatch the daemon reads
  `git diff --name-only <receipt head> <source_commit>` in the owner's checkout (then
  the caller's) and reports `docs_only_delta`, `delta_files` and up to 10
  `delta_code_paths`. A path is documentation only when it is outside `crates/`
  and `scripts/` and under `docs/` or `thoughts/` or a Markdown file; a build
  file or tool config is code. `docs_only_delta` is advice for the manager, never
  a change to `ok`.
- `AgentEnqueueLandingSource` is unchanged: it does not take the job id or refuse
  on a mismatch (a follow-up if managers still enqueue unchecked candidates).

### Listing and cancelling your entries (#1964)

`AgentListRollingQueue {state?, project_id?, limit?, before_sequence?}` and
`AgentCancelRollingQueueEntry {entry_id, reason?}` (`crates/rsid/src/session/rolling_queue_verb.rs`,
`Store::list_rolling_queue_for_agent` / `cancel_rolling_queue_entry_for_agent`) have native tools
(`rsi_control_list_rolling_queue`, `rsi_control_cancel_rolling_queue_entry`) and are
in the closed catalog. No schema migration: `superseded` was already a valid
terminal state and nothing wrote it before.

- **Verb gate.** The advertised projection: list is the enqueue rule plus a
  Status-mode portfolio seat (`portfolio_reader`); cancel is the enqueue rule
  (manager, Epic lead, Execute-mode portfolio seat). Anyone else, a worker
  included, is refused `queue_not_authorized`. The request is validated first
  (`queue_invalid_request`, `queue_reason_invalid`).
- **Scope** is resolved inside the transaction that reads or acts, so it follows the
  seat. The project's current appointed manager (the live lineage tip, so a
  retired or displaced seat holds nothing) reaches every entry of its project;
  a portfolio seat reaches the projects of its grant (cancel: while its policy
  executes); every caller also reaches the entries whose outcome it would
  receive, `COALESCE(wake_session_id, source_session_id)`. `source_session_id`
  stays the immutable enqueuer identity (replay key); only the delivery target
  `wake_session_id` moves on succession (#1604). A list `project_id` outside the
  caller's scope (and not its own project) is `manager_project_not_in_scope`; a
  cancel of an unknown or out-of-scope entry is `queue_entry_not_found` (one code, so
  the verb never confirms an entry the caller cannot reach).
- **List** is newest-first (`sequence` descending), bounded to 100 rows (default 50),
  paged by `before_sequence` (`next_before_sequence`). Rows are the bounded
  `AgentRollingQueueEntryV1` (no hot-file or filter lists; `test_filter_count`
  and a truncated `outcome` summary instead).
- **Cancel** is one CAS `queued -> superseded` under an immediate transaction, the
  same one a claim takes, so an entry is either claimed or cancelled. The entry
  settles with outcome refusal `queue_entry_cancelled` and detail
  `cancelled by <session>[: <reason>]`; the terminal-immutability trigger then
  freezes it. `admitted` and `gating` entries are the lander's: refused
  `queue_entry_in_flight` (the lander remains the single publisher); a settled
  one is `queue_entry_settled`; an entry already cancelled replays as success
  (`replayed: true`, no second wake). No batch exists for a queued entry, so no
  `rolling_queue_events` row is written. When the outcome target is not the caller
  the same transaction inserts one `merge-queue-cancel-<entry id>` resume wake to
  it (recorded as the entry's `wake_job_id`); a topology `land` node mirrors the
  cancelled entry as refused (`superseded`). Cancel works with the queue
  disabled. Re-enqueue with a new `idempotency_key`: the live-source index
  ignores `superseded`, but the old key replays the cancelled entry.

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

## Unscoped health identity (#1872)

`GetHealthStatus` needs no session token or manager/lead authority. Its
`daemon_pid` is the process sending the reply, even when systemd or a socket
holder created the listener. `daemon_started_at` is that process's fixed startup
timestamp, RFC3339 with nanoseconds in UTC, also used by `AgentGetDaemonInfo`.
Both fields are optional when deserializing older replies. The installer checks
the reported process against its launched process tree; Linux also checks the
reply's kernel sender credentials. Older daemons retain the existing kernel
identity fallback. Readiness still requires a valid health payload and a live
launcher after the reply.

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

## Operator configuration: hooks, custom providers, bridges (hub #3039)

The TUI and the desktop edit these three through the daemon, never as local
files. Wire types: `rsi_common::operator_config`; daemon: `operator_config/`.
All twelve methods are operator-only (`OPERATOR_METHODS`): absent from
`AGENT_VERBS`, `READ_VERBS`, delegation, native tools, the agent CLI catalog
and rsi-remote, and a tokened caller is refused `operator_config_operator_only`
before its params are parsed. Fence test:
`rpc::tests::operator_config_methods_are_operator_only_and_keys_never_leave_the_daemon`.
The desktop consumes them through `desktop/src-tauri/src/operator_config.rs` (one
Tauri command per RPC, no arguments beyond the wire fields) and the `:hooks`,
Custom providers and Message bridges dialogs, so it lists the same providers as
the TUI; `desktop/tests/operatorconfig.test.mjs` pins the command-to-method map.

- `ListClaudeHooks {}` -> `{path, exists, revision, hooks}`;
  `PutClaudeHooks {expected_revision, hooks}` -> the same view. The `hooks`
  block of the daemon host's `~/.claude/settings.json`; every other top-level
  key is kept. Atomic write (temp, fsync, rename; an existing file's
  permissions kept). `revision` covers the file bytes and mtime (`"absent"`
  for no file); a stale one is refused with the typed `claude_hooks_conflict`
  and `data.current_revision`. Overwrite = put again with that revision.
  `DeleteClaudeHooks {expected_revision}` removes the `hooks` key (other keys
  kept, same conflict check, an absent file stays absent) and returns the view.
- `ListCustomProviders {}` -> `{providers: [summary]}`;
  `GetCustomProvider {id}`, `PutCustomProvider {id?, name, base_url,
  default_model?, api_key?}` -> summary; `DeleteCustomProvider {id}` ->
  `{id, deleted, cleared_routes}`. A summary is `{id, name, base_url,
  default_model, api_key: {state: set|absent, fingerprint?, set_at?}}`.
  `api_key` on put is `{action: keep}` (default), `{action: set, secret}` or
  `{action: clear}`; the key goes to `~/.rsi/vault/custom-provider-keys.json`
  (0600, its own file so `credentials.json` keeps the shape older daemons
  read) and is never returned, logged or published. Rows are nonsecret
  `daemon_settings` keys `custom_provider.<uuid>`; delete tombstones the row
  (`deleted_at`). A base URL is `http(s)://` with no credentials in it.
- `SetCustomProviderRoute {role, id?}` -> `{role, id, api_key_set}`: `role` is
  `title_model`, `memory_model_fallback`, `dream_model` or
  `prompt_compile_model`. The daemon writes `<role>_base_url` (persisted) and
  the in-memory `<role>_api_key` from the row and the vault, stores the
  assignment (`custom_provider_route.<role>`) and reapplies it on every change
  to that provider and at daemon start. `id: null` detaches the role.
- `ListCustomProviderRoutes {}` -> `{routes: [{role, id, name, api_key}]}`: the
  stored assignment of every role in the order above, read through the same
  accessors `SetCustomProviderRoute` writes with. A detached role (or one whose
  provider no longer exists) has `id`, `name` and `api_key` null; `api_key` is
  the provider's key metadata (state, fingerprint), never the key. A frontend
  shows the current route from this, not by matching a role's base URL.
- `LaunchSession`, `GenerateText` and `CompilePrompt` take
  `custom_provider_id`; the daemon fills an absent base URL and key from it.
- `ListMessageBridges {}`, `GetMessageBridge {kind}`, `PutMessageBridge {kind,
  config}`: `kind` is `signal` or `imessage`, `config` is `{enabled, account,
  allow_from, working_dir}` (`null` while the file is absent). The file is
  `<data dir>/<kind>.toml` in the TUI's former format, written atomically.
  `DeleteMessageBridge {kind}` removes that file (idempotent) and returns the
  bridge with `config: null`.
- One-time import (`operator_config::legacy_import`, at daemon start): the
  TUI's `~/.rsi/state.json` `settings.custom_providers` (ids, order and
  plaintext keys) are copied into the rows and the vault. The file is only
  read, never written; the marker `custom_provider_import.v1` stops later
  runs, and a provider whose row exists (live or tombstoned) or a key already
  in the vault is skipped, so a crash midway converges. The TUI keeps that
  legacy list as loaded and writes it back unchanged for older TUIs.

## Model withdrawal (`model_unavailable`, #2735)

A provider refusal that names the selected model as unsupported or unavailable
(the Codex 400 "The '<model>' model is not supported when using Codex with a
ChatGPT account", an OpenAI `model_not_found`, Claude's `unrecognized_model`)
is the typed `model_unavailable` class, not a generic provider error or an
infrastructure retry.

- Classification: `terminal_provider_stop_reason` (`session/monitor.rs`) stores
  `provider_error:model_unavailable:<model>` (`model_unavailable_in_text`,
  `store_support::provider_defaults`); Claude keeps `provider_model_unrecognized`.
  An empty model name means the session's own `model`.
- Health is derived from those session rows, with no schema:
  `Store::models_unavailable` lists the models refused inside
  `MODEL_UNAVAILABLE_TTL_SECS` (six hours) with no later launch of the same
  provider and model that got past startup (running at least 120 s, or
  Completed/Archived). `Codex` and `CodexAppServer` share one catalog.
  `AgentGetProviderStatus` reports them under the provider's `models_unavailable`
  (`model`, `code`, `first_seen`, `last_seen`, evidence session and stop reason);
  the provider's `launch_admission` stays `open` for its other models.
- Admission: `freeze_launch_identity`, the one launch chokepoint, refuses a launch
  of such a model with `PolicyDenied("model_unavailable: provider <P> refused model
  <M> ...")` before any effect, so sessions, `AgentSpawnChild`, topology nodes,
  review assignments, retries and recovery relaunches never spawn it. A manager
  action receipt keeps the code `model_unavailable`. After the probe window, or
  once a launch of the model gets past startup, it is admitted again.
- Reviewer fallback: `review_launch_substitute` (`manager_reviews.rs`) replaces an
  unavailable reviewer, at the first reservation and at an infra-retry
  successor, with the manager policy's next allowed launch that is available
  (same family first, then the next allowed family; the effort stays when the grant
  admits it). The assignment's request records `model_substitution` (`from`,
  `reason: model_unavailable`, `first_seen`) and the `review_assignment` event
  carries it. With no usable alternative the reviewer launch is refused with the
  typed code.
- Notice: the manager coordinator records one deduped `ledger_change` notice per
  provider, model and episode (`model_unavailable:<provider>:<model>`, version
  the episode's first refusal).

## Provider CLI login state (`provider_login_required`, #2753)

Whether `claude` and `codex` are logged in on a host is a typed fact the operator
and every manager read before a session is spent, and a definitely logged-out CLI
is refused at launch.

- Probe (`provider_login.rs`): `claude auth status --json` and `codex login
  status`, run by the daemon (never an auth file, 10 s timeout, output bounded).
  The probe gets the launch environment: `vault::scrub_credential_env` removes every
  inherited provider key (so an `ANTHROPIC_API_KEY` the daemon holds cannot read
  as `logged_in` for a launch that will not see it) while `HOME`,
  `CLAUDE_CONFIG_DIR` and `CODEX_HOME` stay inherited (#2810). It leads its own
  process group, and the whole group is killed at the deadline, when a caller drops
  the probe, and once the status command exits, so no helper it forked outlives it
  (#2812).
  The result is reduced to `state` (`logged_in`, `logged_out`, `unknown`), `method`
  (the closed Claude `authMethod` set `none|claude.ai|oauth_token|api_key|
  api_key_helper|third_party`, `chatgpt` for Codex, else `unknown`) and
  `checked_at`; the CLI's other output (e-mail, organisation, directory) is dropped
  before anything is stored, served or logged. Claude: `loggedIn` decides;
  Codex: exit 0 is logged in, exit 1 with empty output or "Not logged in" is
  logged out; a missing CLI, a timeout or anything unreadable is `unknown`.
- Cache and triggers: 60 s TTL, one probe in flight per provider (concurrent callers
  share its result). Probes run at daemon start, on each manager-coordinator pass
  when stale, after a session stops on `provider_auth_invalid` or
  `provider_login_required` (`note_provider_auth_stop`), on a satellite identity
  poll when stale, and on operator refresh.
- Fail open: `unknown` never refuses a launch; the launch proceeds and its outcome
  is classified. Only `logged_out` refuses.
- Admission: `enforce_launch_provider_logged_in`, beside the allowlist and
  `model_unavailable`, refuses a launch that runs on the CLI login (provider
  `Claude` unless the model is a Bedrock Claude model; `Codex`; `CodexAppServer`,
  which shares the Codex login) with `PolicyDenied("provider_login_required: the
  <provider> CLI is not logged in on this host; run `claude auth login` / `codex
  login` ...")` before any effect. A `logged_out` record older than the TTL is
  probed again first, so a login the operator just did lifts the refusal at once.
  A manager action receipt keeps the code. It is distinct from
  `provider_auth_invalid`, a credential the provider rejected.
- Classification: `provider_defaults::classify_provider_auth_text` types `Not
  logged in`, `Please run /login`, `Login expired` and the Codex `Missing bearer or
  basic authentication` (which rides a 401 and wins over it) as
  `provider_error:provider_login_required`, and a 401 or invalidated token as
  `provider_error:provider_auth_invalid`; Claude `system/api_retry` (the wire
  `type: "system"`, `subtype: "api_retry"`, `monitor::api_retry_auth_class`, #2811)
  `error`
  `authentication_failed` is the login class, `oauth_org_not_allowed` and
  `account_on_hold` the credential class, `billing_error` the typed
  `provider_error:provider_billing_error` (a credit-class stop reason). A login
  failure sets `last_auth_failure_at` but opens no auth-failure episode.
- Auth-failure episode end (hub #2957, #1610): a rejected credential
  (`provider_auth_invalid`) opens an episode that reads `launch_admission: refused`,
  `refusal_detail: auth_invalid`, and `AgentGetProviderStatus` names it in
  `auth_episode {started_at, last_failure_at, state, next_probe_at}`. It ends without a
  launch: a `logged_in` login reading with `checked_at` after `last_failure_at` (and at
  least `PROVIDER_AUTH_LOGIN_RECHECK_MIN_SECS`, 5 min, after it) reads `cleared_by_login`
  and `open`; with no such reading, `PROVIDER_AUTH_PROBE_INTERVAL_SECS` (1 h) after the
  last failure the state reads `probe_admitted` and `open` (a failed probe is a new last
  failure and refuses for another hour). `logged_out` reads `relogin_needed`: refused, no
  `next_probe_at`; `awaiting_probe` carries the `next_probe_at` that re-opens it. A
  later launch that gets past startup still closes the episode.
- Agents: `AgentGetProviderStatus` entries `claude`, `codex` and `codex_app_server`
  carry `login {state, method, checked_at, hosts[]}`; `logged_out` reads
  `launch_admission: refused`, `refusal_detail: provider_login_required`. `hosts`
  are the paired satellites' own probes (their health snapshot carries
  `provider_logins`, state/method/time only).
- Operator: `GetProviderLoginState` (the cache, probing a stale result) and
  `RefreshProviderLoginState` (probe now), both `{provider?}`, operator-only and out
  of `AGENT_VERBS`, `READ_VERBS`, the native tools and the agent CLI catalog. The
  TUI Settings > Provider Keys list ends with a `claude login` and a `codex login`
  row (state, method, age, the host-side fix when logged out; `c` probes now).
- Notice: the manager coordinator records one deduped `ledger_change` notice per
  provider and login loss (`provider_login_required:<provider>`, version the
  `logged_in` -> `logged_out` transition time) naming the host-side fix.

## Provider CLI contract canary (`provider_contract`, #2755)

RSI re-implements pieces of the Claude Code and Codex CLI contracts by hand (the
Codex hook trust hash that carries agent mail and the sandbox custody guard, the
Claude launch flags, the non-`--bare` Claude behaviour). A CLI that drifts breaks
them silently: an untrusted Codex hook is simply not run. A bounded, non-billing
canary per CLI makes the drift visible.

- Triggers: daemon start, and a CLI whose version changed. Each manager-coordinator
  pass stats the two binaries (path, size, mtime); only a changed binary whose
  `--version` differs from the last result runs the canary again (a rewritten
  binary with the same version keeps its verdict). No polling of the CLI itself.
  A canary that could not run to a verdict (`unchecked`, `canary_failed`) is
  retried after 5 minutes at most. `RefreshProviderContractState` runs it now.
- Codex (`codex -c hooks.PostToolUse=... -c hooks.PreToolUse=... -c hooks.state=...
  app-server`, the overrides `codex_session_hook_overrides` also gives a launch):
  the `CodexAppServer` handshake (`initialize` with `experimentalApi`, then
  `initialized`) and `hooks/list`. Both session-flag hooks must be listed
  (`codex_hook_missing`), `trusted` or `managed` (`untrusted` is
  `codex_hook_untrusted`, `modified` is a mismatch) and have a `currentHash` equal
  to RSI's (`codex_hook_hash_mismatch`). A refused or malformed handshake is
  `codex_handshake_failed`. No model turn starts.
- Claude: each flag of `claude::CLAUDE_LAUNCH_FLAGS` must be in `claude --help` or
  accepted by `claude -p <flag>` (with no prompt the CLI parses its options, then
  refuses for want of input; an `unknown option '<flag>'` answer is
  `claude_flag_unknown:<flag>`, so a hidden flag such as `--max-turns` passes);
  the stream-json `initialize` control request must answer with
  `claude_code_version` and `capabilities` (`claude_initialize_failed`,
  `claude_contract_shape`); and when the Claude login is an OAuth login
  (`claude.ai`, the login probe of #2753) the account a launch reads must carry
  an e-mail, organisation or subscription, or it reads as if `--bare` were the
  default for `-p` (`claude_ambient_auth_dropped`). RSI never passes `--bare`.
- Result per provider (`ProviderContractV1`): `contract_state` `ok`/`degraded`/
  `unchecked`, `reason` (a degraded reason above, or for `unchecked`:
  `cli_unavailable`, `canary_failed`, `canary_disabled`), `cli_version` (the first
  line of `--version`), `checked_at`. Nothing else the CLIs print has a field.
- Fail open except custody: `unchecked` and every non-custody degradation never
  refuse a launch. A Codex CLI launch that installs the custody hook (a session
  launch) is refused with `PolicyDenied("codex_hook_untrusted: ...")` (or
  `codex_hook_hash_mismatch`, `codex_hook_missing`) when the setting
  `codex_refuse_untrusted_custody_hook` (default on) is on and the canary found
  the hook untrusted on the binary now installed. The chokepoint is
  `spawn_provider_process` (fresh launches, continuations, rotations). A manager
  action receipt keeps the reason as its code.
- Settings (rule 10): `provider_contract_canary` (default on; off reports
  `unchecked`/`canary_disabled` and runs nothing) and
  `codex_refuse_untrusted_custody_hook` (default on; off launches Codex without
  custody and only reports `degraded`), both live, TUI Settings > Provider
  Isolation, validated as booleans by `UpdateDaemonConfig`.
- Operator: `GetProviderContractState` (the cache, running a due canary) and
  `RefreshProviderContractState` (run now), both `{provider?}`, answering
  `{generated_at, canary_enabled, codex_refuse_untrusted_custody_hook,
  providers[{provider, contract}]}`; operator-only and out of `AGENT_VERBS`,
  `READ_VERBS`, the native tools and the agent CLI catalog. The TUI Settings >
  Provider Keys list ends with a `claude contract` and a `codex contract` row
  (state, reason, CLI version, age, the launch consequence; `c` checks now).
- Agents: `AgentGetProviderStatus` entries `claude`, `codex` and
  `codex_app_server` carry `contract`; a custody failure with the refusal setting
  on reads `launch_admission: refused`, `refusal_detail: <reason>` on `codex`.
- Notice: the manager coordinator records one deduped `ledger_change` notice per
  provider and degradation (`provider_contract_degraded:<provider>`, version the
  time the provider entered that reason).

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
  `enabled_tools`. It is validated at launch (`tool_policy_invalid`). Every
  provider either enforces each field the policy sets or the launch is refused
  `tool_policy_unsupported_for_provider:<what>` (`<what>` is the provider label,
  plus the field for a CLI: `claude:web_access`, `local`, `harness:cli_access`);
  nothing is silently widened. Harness, OpenRouter and Bedrock enforce the
  Harness fields above; Claude, Codex and Pioneer enforce only `cli_access`.
- **CLI access (`cli_access: "read_only"`, #2758).** The one field a CLI enforces,
  for read-only reviewers and untrusted-repo workers. Codex runs `--sandbox
  read-only` on a fresh turn and `-c sandbox_mode="read-only"` on a resumed one
  (it narrows the daemon-wide `codex_sandbox_mode`, never widens it, and drops the
  writable roots). Claude runs `--permission-mode dontAsk` (instead of
  `bypassPermissions`) with `--tools=Read,Grep,Glob` (no shell, no edit tool; MCP
  tools are not part of `--tools`) and `--allowedTools=Read,Grep,Glob,mcp__rsi-agent`
  (`dontAsk` denies an MCP tool without an allow rule). The `rsi-agent` MCP server,
  the wake/cron denial and the `--settings` hooks (custody `PreToolUse`, mail
  `PostToolUse`) are unchanged for both CLIs. `ToolPolicyGuard`
  (`session/provider_spawn.rs`) hands the CLI launch the session's resolved
  policy, so a continue, retry or rotation maps it like a fresh launch. A launch
  with no policy keeps today's argv exactly. The monitor writes one visible
  `System` row per run naming the effective mode (`provider_health.kind =
  access_policy`). Operator surface: the `LaunchSession` `tool_policy` field and
  the TUI create-entity form (`R`, shown as "Read-only" in the Sandbox row).
- **Persistence.** One immutable `session_tool_policies` row per session
  (provisional migration), written at launch. Continue, retry and rotation
  resolve it through the `continued_from` chain, so restrictions survive
  rotation. An unreadable row launches the session fail-closed (no tools, no web).
- **Spawned children.** A child inherits its emitter's policy unchanged, so a
  spawn can only keep or narrow it, never widen it. A child whose provider
  cannot enforce it is refused (`tool_policy_unsupported_for_provider:<what>`),
  and a Harness -> Codex CLI route fallback is refused for a policy session. A
  read-only Claude or Codex emitter's children stay on a CLI that enforces it.
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
- **Native web search (#2251).** On Harness routes without a provider-hosted
  search the loop offers a native `web_search` function, only when the
  `harness_web_search_backend` setting (`auto` | `serper` | `exa` | `searxng` |
  `disabled`; default `auto`, read at each Harness launch) names a configured
  backend: `Serper` (`SERPER_API_KEY`) or `Exa` (`EXA_API_KEY`, #2252), each a
  Provider Keys vault slot that must resolve, or the keyless `SearXNG`
  (#2315), which needs only the `harness_searxng_url` setting (default unset;
  an http(s) base URL with no credentials, query or fragment, validated by
  `normalize_searxng_url`; the TUI row is Settings > Harness Tools > Harness
  SearXNG URL; Enter opens the text editor, `overlay/daemon_text_editor.rs`
  #2393, which shows the daemon's refusal and keeps the draft). `auto`
  takes the first configured backend: Serper, then Exa, then SearXNG when the
  URL is set. Nothing configured, or the named backend lacking its key or
  URL: the tool is absent and the session emits one `system` event, subtype
  `web_search_unavailable`, with the reason (`no_backend_credential`,
  `no_backend_url` or `backend_unavailable`). The vendor host is a constant
  (SearXNG: the operator's URL, never model-controlled) reached through a
  dedicated no-redirect client, not `EgressFetcher`; `egress=offline` still
  refuses before any request. A key is read per call from the slot alone (no
  generic `API_KEY` fallback) and never appears in arguments, outputs, events
  or errors; SearXNG has no key, and its errors name the fix (JSON format
  disabled, instance unreachable, redirect) without echoing the URL. The
  SearXNG request is `GET {base}/search?q=...&format=json`; a local
  loopback-only instance is one command, `scripts/searxng-local.sh up` (public
  instances usually block automated JSON queries, so the local instance is the
  supported path). The call is charged the conservative
  `HOSTED_SEARCH_COST_USD_MICROS` estimate and counts against
  `max_search_calls`; `web_access` applies as for the hosted tool.
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
job is recorded `queued` (`held: "held_for_deploy"`, no `started_at`, no unit),
is visible in `AgentGetJob`/`AgentListJobs`, and the job loop launches it (state
`running`, `started_at` stamped, one owner wake at the end as usual) once the
drain ends or the restarted daemon is past the hold. `AgentCancelJob` settles a
queued job `failed` `job_cancelled` without a unit. A queued job never blocks
the deploy's quiet point (only `running` jobs do), and wall-clock limits count
from `started_at`. `landing`, `cloud_gate` and `cloud_sweep` submits keep the
typed retryable `deploy_draining` refusal. Schema: V159 (`queued` state,
`started_at`).

Held for a staged deploy (#2190): a staged `interrupt_workers` deploy stops
running `test` jobs after their grace (below), so a `test`/`build` job started
meanwhile would only be killed. For the whole life of such a deploy (to its own
deadline, past the `deploy_drain_hold_secs` worker-start cap, and for its caller
and parentless sessions too) every new `test`/`build` submit is held exactly as
above: `held: "held_for_deploy"`, no unit, no timeout clock, and the receipt's
`release_by` names the latest release (the deploy deadline). The jobs start in
submission order through normal governor admission when the deploy settles
(success, rollback or timeout), is abandoned (`cancel`) or reaches its deadline,
and each owner's one wake fires at the job's real terminal state. Jobs already
running when the deploy is staged keep the grace below. A satellite runs its own
deploy flow on its own daemon, so the same rule holds jobs on that satellite.

`AgentSubmitJob`, `AgentGetJob`, `AgentListJobs` and `AgentCancelJob` (#1106; the owner's stop, the job settles `failed` with refusal `job_cancelled`; #2213: the current appointed manager, or a portfolio seat inside its grant, may also stop an ordinary worker `test` or `build` job owned by a session in its live scope under the `manager_session_control_scope` mutation checks, the result's `cancelled_by_manager` and the job detail name the manager and the owner; #2232: a daemon-owned job is never cancelled by a manager (`landing`, `cloud_gate`, `cloud_sweep`, a `release` build, the `rolling-library-qa` recipe and any job a `bounded_qa_requested` binding names: `agent_jobs::manager_may_cancel`), which is `job_not_found` to every non-owner; #2233: every non-owner refusal is `job_not_found` (including `manager_target_owned_by_ancestor`, a container owner and a custody change), and only the owner's current in-scope manager sees the typed `manager_v2_capability_denied`, `manager_v2_paused` and `manager_v2_human_or_recovery_owner`) are agent verbs (in #2470: `AgentGetJob`/`AgentListJobs` also serve the live manager seat (project, area or portfolio) the jobs, settled or not, that its published rotation predecessors owned (project jobs must name the seat's project), and `AgentGetJob` a job owned by a session in its manager scope (a worker's receipt); owner and cancel authority are unchanged, a stale predecessor or foreign manager gets `job_not_found`.
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

Lost job id (#269): the `AgentSubmitJob` receipt repeats the id as a top-level `job_id`
(beside `job.id`; replies serialize with sorted keys, so it lands after the large `job`
object and survives a `tail`). `AgentListJobs {limit?}` (1-100, default 20) returns the
caller's own jobs newest first for every session, worker included; only a live manager seat
also sees its published rotation predecessors' jobs (#2470), and another session's job is
never listed or readable (`job_not_found`).

Package test jobs accept `params.exact: true` (#1600) to pass libtest `--exact`
after Cargo's `--` separator, matching whole names in `filters`. Omitted or
`false` keeps substring matching; empty filters still run every test. `exact`
is a boolean and `true` is refused for shard, recipe and candidate-receipt
runs. Flags in `filters` remain invalid. For example:
`{"kind":"test","params":{"package":"rsi","lib_only":true,"filters":["module::tests::one"],"exact":true}}`.

For one integration-test harness, set `test_target:"<name>"` instead of
`lib_only:true` (#371). It passes Cargo `--test <name>`, requires `exact:true`
and nonempty filters, and is exclusive with all other test job forms. The
target is a plain ASCII letter/digit/underscore/hyphen name (no leading hyphen,
path or shell syntax; 200 bytes max); invalid names and combinations are
`job_invalid_params`. Existing requests without the selector are unchanged.

A package test job runs plain `cargo test` under the job's private, deliberately
disk-backed TMPDIR (the job refuses a tmpfs/ramfs TMPDIR and caps its scratch);
it does not use the tmpfs scratch or serial libtest execution that
`scripts/scoped-test` applies (#1966). Migration-heavy tests (the `rsid-store`
migration and schema suites) can be far slower there: one such exact job
compiled in 69 s but timed out at 20 minutes with 30/32 tests done. Do not use
a package job to preflight them; the `scoped-test` recipe that earlier text
named is retired (`job_recipe_use_scoped_test`) and `scoped_test` takes no
filters, so the final committed `scoped_test` verifies them.

Optional early feedback (#192). Before the one `scoped_test`, a worker may
submit ONE exact package job per crate for a new or changed runtime or schema
assertion it is unsure about (for example an `rsi-common` fixture test, or an
`rsid-store` assertion that is not a migration suite), with `lib_only` and the
full module path in `filters`. It runs the working tree at submission, not
committed bytes, and carries no head or source stamp, so it is never acceptance
evidence; the committed `scoped_test` is, and it still derives and runs every
test. Mistyped names no longer read green: when `filters` is non-empty, the
job exits 0, and the whole job log (streamed, not only the bounded `detail`
tail) proves libtest ran no test, the job settles `failed` with refusal
`job_filter_matched_no_tests` and the observed `exit_code` 0. The proof needs
at least one `test result:` summary, a summary for every test binary cargo
started, and `passed` plus `failed` totalling zero across all of them;
`ignored`, `measured` and `filtered out` are not executions. A binary that
prints no summary (a custom harness), an unreadable summary line or a log
that cannot be read to its end (a read error at any point, even after a zero
summary) keeps the ordinary result (`succeeded` on exit 0). Empty
`filters`, a non-zero exit, and scoped, recipe, shard and candidate runs are
unchanged. A real preflight still pays its governor-slot wait and one
test-profile build.

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
declares nothing for it, and `make check-touched-shards` refuses it (exit 2).
`scoped-test` and `scoped-test-tmpfs` recipes are retired (#1954), including
aliases whose target is either gate, even in stale manifests. They return
`job_recipe_use_scoped_test` before recording or launching a job; submit
`{"kind":"test","params":{"scoped_test":{"base":"origin/rolling"}}}` instead,
without `recipe` or `filters`. Local Makefile targets remain available.
The typed `test {scoped_test:{base, head?}}` form (#1638, any
leaf; `base`/`head` are bare branch, `origin/<branch>` or 40-hex refs, no other
field) runs `scripts/scoped-test --base B [--head H] --runtime-max-sec T-120
--total-max-sec T-60 --cpu-quota 200` in the caller's sandbox under the ordinary
test unit and `timeout_minutes` rules (T = the timeout in seconds), needing no
manifest entry. Its default timeout is 60 minutes, or `job_test_timeout_mins` when
larger, capped at 180 (#1979): the tool budgets each package separately, so the
20-minute test default killed a plan of two 25-minute packages; the other test
forms keep `job_test_timeout_mins`. Only a larger explicit `timeout_minutes`
needs the manager/Epic-lead authority. The stamped `params.timeout_minutes`
(`AgentGetJob`) and the receipt's `job_timeout_minutes` show the effective timeout. It uses the current `origin/rolling` tip's tool, materialized
read-only, unless the candidate changes that tool or a helper; then it runs
the candidate's copy (#1938). Resolution failure falls back to the worktree.
The receipt's `tool` object names its `path`, `blob`, `source` and `rev`.
The script's last line is `SCOPED_TEST_RECEIPT <json>`; the job
settles `succeeded` only on exit 0 with `ok: true` and carries it as
`result.receipt` next to `result.exit_code` and the job `log_path`
(`scoped_test_receipt_missing` when none was printed). Submit with `wake: "none"`
and arm one `AgentScheduleWake mode:"when"`. Names and targets use
ASCII letters, digits, `_`, `-`, `.` and cannot start with `-` (200 bytes max).
The manifest is version 1, at most 64 KiB and 64 recipes, with no unknown
fields; its resolved path must stay inside the worktree. Missing manifests or
undeclared recipes return `job_recipe_not_allowed`; malformed manifests return
`job_recipe_invalid`, before inserting or launching a job.

Declared timeout caps are 5-180 minutes and CPU caps are 1-1600 percent.
The request's test timeout must fit the declaration (`job_invalid_params`
otherwise); for a declaration below the operator default, supply a matching
`params.timeout_minutes`. A declaration never raises the operator default or
grants manager authority to raise a timeout. The one daemon-owned exception is
the `test-timings` recipe (#2089, one nextest JUnit run over the `rsid` and
`rsid-store` libraries): a submit that names no timeout gets the larger of 90
minutes and `job_test_timeout_mins` (capped at 180), so any worker can request
it; the declaration can only lower that, and a larger explicit timeout still
needs the manager/Epic-lead authority. Linux applies the declared CPU
cap and the existing 24 GiB memory cap; macOS uses the existing launchd backend
and cannot enforce aggregate CPU/memory caps. Both use the build-slot prefix,
private scratch, bounded log and status wrapper, cancellation, timeout,
restart reconciliation and typed result of test jobs. `wake:"none"` supports
the existing `when.jobs_terminal` batch wake. These are repository gates with
the same authority as Cargo tests; declarations grant no cloud, publishing or
credential authority. Package/shard/candidate-receipt fields are exclusive
with `recipe`.

Per-job CPU budget (#161): a package, shard, `scoped_test` or `build` job may carry
`params.cpu_quota_percent` (100-1600, percent of one CPU; out of range or on a
`recipe`/`candidate_receipt` is `job_invalid_params`). `agent_jobs::job_command` takes
the smaller of it and the budget it derived (host ceiling, `rsi-build.slice`/
`hub_build_cpu_percent`, kind default) for the outer unit's `CPUQuota`, passes the same
value as the `scoped_test` `--cpu-quota` (the script sizes nextest threads and its nested
`rsi-scoped-test-*` units from it, itself capped again by the live slice) plus
`RSI_JOB_BUILD_JOBS=min(4, quota/100)` after the `env -u` prefix (the script's `cargo-slot`
holder, not the stamped `CARGO_BUILD_JOBS`, picks the build jobs, #154) and, for a plain
cargo job, sets `RSI_JOB_BUILD_JOBS`, `RUST_TEST_THREADS` and `NEXTEST_TEST_THREADS` to
`min(4, quota/100)` before `cargo-slot` starts. A request never raises a budget or
overrides an operator setting; omitted keeps today's argv and quota. The `RSI_JOB_BUILD_JOBS`
cap needs the installed `cargo-slot` from the same deploy (an older copy keeps four build
jobs; the quota and test threads still apply). macOS jobs run `env` as the slot wrapper and
keep launchd's two build jobs and four test threads (#2937); the daemon itself holds a macOS cargo job
`held_for_capacity` while the platform build slots (2) are full or less than 4 GiB of memory is available
(`host_memory.rs`); Windows has no job runtime.

Test-job timeout (#1337): a `test` job carries `params.timeout_minutes`
(5-180). When omitted the daemon stamps the operator's `job_test_timeout_mins`
(default 20, read at submit) into the stored params; a `candidate_receipt` run
gets no default (its 3 h unit cap applies). A timeout above the default is
refused `job_timeout_not_authorized` unless the caller is the current
appointed manager or an Epic lead, or uses the eligible QA shard form below.
The unit's `RuntimeMaxSec` is the execution timeout plus the build
admission allowance (`agent_jobs::admission_allowance_secs`) and 5 minutes of grace; the job poll stops a running unit
past its timeout and settles
the job `failed` with refusal `job_timed_out` (also when the unit cap stopped it
while rsid was down), waking its owner once and recording the friction event
`runaway_process:job_test:timeout`. The timeout budget starts at the wrapper's
durable `job-start` marker, excluding governor and sandbox/artifact lock waits
(#1706): a job held `queued` for a deploy
spends none of it. The pre-launch wait has its own cap, the drain's
`deploy_drain_hold_secs` (default 600 s when uncapped) plus 5 minutes, counted
from the later of the submission and the deploy hold's last engagement (#2190:
a long hold, and a daemon restart through it, never count); a job
still unlaunched past it settles `failed` with the distinct refusal
`job_admission_timed_out` (wakes the owner once; resubmit it) and never reports
`job_timed_out` (#1611). A unit that never writes `job-start` (its command never
ran: a systemd start delay, not a test) gets one `start-wait` line with the
unit's state in its log after 2 minutes and, past its independent ten-minute
start admission limit, settles `failed` with `job_start_timed_out`, never `job_timed_out` (#1832). The job log
is written unbuffered (`stdbuf -o0 head -c`), so `job-start`, slot/lock waits
and test output are visible while the job runs (#1830).

Same-owner test/build peers in one sandbox queue before unit launch, oldest
first, with `held: "sandbox_job_queued"`. Their admission and execution clocks
stay suspended while the running holder is active and progressing. Progress
means log output (including phases) within ten minutes; start, build admission
and execution budgets still bound the holder. A waiter behind an inactive
holder without an exit status (after the launch grace) fails with
`job_sandbox_holder_dead`; an exhausted budget or ten minutes without log
progress yields `job_sandbox_holder_stalled`. Both results name the holder job,
unit and log, and leave its service alone. Completed holders are reconciled
normally before the next peer launches. Deploy-drain holds take precedence
and keep their own admission cap.

Linux test/build jobs sharing a Cargo target serialize under an exclusive
`sandbox-lock` before governor admission, so Cargo's silent build-lock wait
does not consume execution time or a governor slot. macOS retains Cargo's own
serialization. Build admission is separate from execution. #2211: an ordinary test job waits
for its governor slot up to its own timeout (at least ten minutes) and a `build`
job (a manager's release build) up to one hour (`BUILD_ADMISSION_WAIT_SECS`), so
neither loses a fixed ten-minute race against the jobs holding the one slot
(cargo-slot gets `RSI_CARGO_ADMISSION_MAX_SEC` from the unit's environment,
`JobCommand::admission_max_secs`); exceeding it settles `job_admission_timed_out` with the last phase and full wait
duration. Scoped-test jobs (#2025) count slot/lock waits in their own wall budget
from `job-start`; admission may use the remaining budget minus a 120-second
runtime reserve. A granted slot leaves only the remaining wall budget to run.
The script derives the diff before acquiring a slot: a selection with no Rust
package runs its Python suites without a governor slot. Scoped-test units carry
only the job timeout plus 5 minutes of grace, without a separate slot allowance.
The Linux launcher uses `--no-block` so service start cannot stall submission
of the rest of a batch; a unit that ends before `job-start` reports
`job_launch_failed` with its unit state and launch phase.

QA shard lanes (#1520): `test` params may carry `qa_lane:{sha:<40 lowercase
hex>}` together with a known `shard` (store-01..04, session-01..05,
memory-01..02, other-01..05), optional existing `filterset` (one or more `test(NAME)`, `test(=NAME)` or `test(/REGEX/)` atoms joined by ` | `, at most 32 atoms and 1024 bytes, #1605; a refusal names this form) and
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
(`rsi-workers.slice/rsi-worker-<invocation>-<n>.scope`, `rsi-job-<id>.service`
in `rsi-build.slice` or `app.slice`, and the nested
`rsi-scoped-test-<job id>-<hex>.service` of `scripts/scoped-test`, which the
daemon's `RSI_JOB_ID` unit setting names after its job, #2127; Linux only, explicitly `cpu_andon_unsupported` elsewhere). A tree past
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

Toolchain custody (#2823): a Linux or Mac `test`/`build`/`landing`/`cloud_gate`
submit whose daemon `PATH` resolves a `rustc` other than the exact version the
job directory's `rust-toolchain.toml` pins (probed with `rustc --version` in that
directory, 20 s bound, so a rustup proxy applies the repository's override) is
refused `toolchain_mismatch` before a row exists, naming the expected and found
compiler and the PATH searched; no `rustc` on the PATH is the same refusal. A
channel name (`stable`) or an inconclusive probe is not refused. The fix is on
the daemon side: `scripts/rsid-health.sh` (`rsid_daemon_path`, used by
`install-release.sh` and `rsid-start-satellite`) puts `~/.cargo/bin` and
`~/.local/bin` in front of the caller's PATH on the daemon unit, and the
`rsid.service` template already carries them.

Test/build units receive a private, disk-backed `<id>.tmp` directory beside
the log as `TMPDIR`; launch is refused if this resolves to tmpfs/ramfs.
Their `CARGO_TARGET_DIR` is the sandbox's `target/`; Cargo `build.target-dir`
settings and the daemon's inherited target environment cannot redirect agent
builds outside reclaim (#1864). A manager job in another same-repository
worktree receives a registered `~/.rsi/build-targets/rsi-build-<path-hash>`
allocation instead for integration builds; different worktrees receive
different targets. Manager release builds use #1218's persistent
`staging/release-target`, whose lock inodes the generic scratch sweep preserves
by leaving that singleton to its existing bounded reclaim path. The job log records the selected
path in its `rsi-build-target` line. Job units inherit the owner's recorded build
settings, including sccache for manager seats when the existing setting enables
it. These targets use the existing scratch age, holder, Git and manifest proofs;
legacy targets require operator adoption in Settings > Scratch reclaim.
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

## Topology names and bundled starters (hub #3157)

Topology names are unique within `(owner_kind, project_id, epic_id)`, with
NULL scope values treated as equal. Operator names are global, manager names
belong to the project (and survive a manager-seat rotation), and Epic names
belong to that Epic. Archived rows continue to reserve their scoped name.
Agent upsert creates or revises only the requested ownership scope; a private
row cannot reserve an operator starter's name.

`AgentTopologyGet {name}` resolves only visible, unarchived rows, in this order:

- A manager's project-owned manager row, then its in-scope Epic rows, then a
  shared operator row.
- An Epic lead's own Epic row, then a shared manager row in its project, then a
  shared operator row.

Multiple in-scope Epic rows for a manager resolve by ascending Epic UUID,
then topology UUID. `AgentTopologyList` returns the same winning row per name;
its scope/Epic filters apply before this ordering, and its name cursor remains
complete across duplicate stored names. `Get {topology_id}` reads that exact
visible row, including one shadowed by name. Existing visibility and execute
policy checks still apply; name ordering grants no additional authority.

The daemon seeds bundled starters as shared operator rows at their fixed IDs.
Private manager/Epic rows are never adopted or refreshed by seeding, even when
their text is an older bundled definition. Managers and Epic leads use the
same fixed starter ID and its returned digest to execute the same starter
revision. A manager's private same-name row remains its default name lookup;
use the fixed ID to select the shared starter. Operator edits and archives
retain the existing seed-preservation behavior.

## Wake predicate `topology_execution_settled` (#2261)

`AgentScheduleWake mode:"when"` accepts `when:{"topology_execution_settled":"<execution id>"}`
(exactly one predicate per wake). The daemon fires one owner wake when the
execution's status is `succeeded`, `failed`, `cancelled` or `blocked`, so a
launcher that executed a topology waits once instead of polling or learning of a
failed or blocked run at its timeout. The wake message carries `execution_id`,
`status`, `error`, the `blocked_reason` summary (`kind`, `node_id`, `attempt_id`)
and `finished_at`. A vanished execution settles the wake `unsatisfiable`
(`execution_missing`). `blocked` counts as settled: a blocked execution that is
later resolved needs a new wake.

Arm-time authorization is the topology verbs' own: the caller resolves as a
`TopologyCaller` (manager with `Automation`, or the Epic's lead) and the
execution's Epic must be readable at `Reach::Read`; an unknown or out-of-scope
id (including another project's) is refused `wake_when_execution_not_found`.

## Wake predicate `settlement_run_settled` (#2789)

`AgentScheduleWake mode:"when"` (with the required `message`) accepts `when:{"settlement_run_settled":"<run id>"}`: the
daemon resumes the owner once when that source-worktree settlement run is finished (the run
row carries `finished_at`: every root `settled`, `refused`, `recovery_required` or
`unattempted`; a `partial` run still applying is pending), when it can never become true
(`settlement_run_missing`) or at `timeout_seconds`. Arming needs the cohort's authority
over the run's repository; an unknown run and one outside that authority are the same
refusal, `wake_when_settlement_run_not_found`. The wake carries the run state, the per-phase
counts, any terminal error and the finish time (`wake_when::settlement_run_line`); a retry of
the apply with the same key then returns the finished receipt. Polled every 5 s with no model
turn.

## Wake predicate `health_anomaly` (#2747)

`AgentScheduleWake mode:"when"` (with the required `message`) accepts `when:{"health_anomaly":{"ignore":[...]}}`: the
replacement for a manager's hourly safety-net resume. The scheduler's fast lane
evaluates it every `HEALTH_POLL_SECS` (60 s) with no model call and resumes the
owner ONCE when any condition holds, with one `condition=<kind> subject=<id> ignore_key=<kind>:<subject> detail="..."`
line per condition (at most 24) after the wake's own text. With nothing abnormal
the owner takes no turn at all. Only a project manager (`is_manager`, scope = its
project) or a portfolio seat (`is_global_manager`, scope = the projects its grant
covers, which for a child node is a strict subset, #2788) may arm it
(`wake_when_health_anomaly_manager_only`); the daemon pins `project`/`projects` into
the stored spec at arm time and overwrites any client-supplied value. The stored
scope is only an upper bound: every evaluation re-derives the observable projects
(`Scope::live`) as the stored set intersected with the owner's CURRENT portfolio grant
(a PM keeps its project only while it is still its session's project), so a narrowed or
revoked grant stops reporting the removed project at once, and every project-bound
fact (seats, blocked executions, overrun attempts, red QA, PM context) is limited to
that set in the query itself. Host readings are not project-bound. It is a
predicate wait, so it follows the seat on a succession or rotation
(`manager_obligations.rs`). `timeout_seconds` is honoured (a `timed_out` check-in
at most that rarely) but is not needed.

Conditions (`rsi_common::health_anomaly`, evaluated by the pure
`rsid::health_anomaly::evaluate` over `StoreFacts` (`collect_store_facts`),
`HostReading` and `LiveHealthInputs` (`SessionLauncher::health_live_inputs`)):

| `kind` | `subject` | Holds when |
| --- | --- | --- |
| `pm_seat_succession_blocked` | project id | the PM seat is vacant, `Failed`, `Completed`, `Archived` or `Deleted` and its newest root succession is finally refused (`manager_succession_refusal`, #2667) |
| `pm_context_over_threshold` | PM session id | live `context_fill_pct` >= the seat's rotation threshold with no unresolved succession for 10 minutes |
| `topology_execution_blocked` | execution id | `blocked` for `health_stuck_minutes` |
| `node_attempt_past_wall_limit` | attempt id | `running` more than `health_stuck_minutes` past its real wall limit (#2786): the earlier of its execution's `deadline_at` and its node wall time (`SESSION_WALL_TIME`, 4 h from the attempt's start plus the time it waited on an answer), as the executor enforces it; an execution with no deadline still bounds it |
| `hub_disk_high` | `/` | root filesystem use >= `health_disk_used_pct` |
| `satellite_disk_low` | peer id | the satellite's reported free build disk is below its reported floor |
| `cpu_hot` | `hub` | the hottest CPU sensor >= `health_thermal_c` for 3 minutes |
| `provider_model_unavailable` | `<hub or peer id>:<provider>:<model>` | a launch was refused `model_unavailable` (same data as `AgentGetProviderStatus`; a satellite reports its own on `SatelliteHealthV1.provider_faults`) |
| `provider_auth_failure` | `<hub or peer id>:<provider>` | an open 401-class auth episode |
| `rolling_qa_red_untracked` | tested SHA | the newest full-library QA receipt is red, older than `health_stuck_minutes`, and its Issue is closed, cancelled or was never filed |
| `reclaim_unsupported` | `hub` | this build cannot reclaim sandbox build caches (the fail-closed `target_reclaim_unsupported.rs` shim: not Linux, not macOS); persistent, and `AgentGetDaemonInfo` `hub.build_cache_reclaim` reads `unsupported` (hub #3055) |

The thresholds are daemon settings with an operator surface (RPC
`GetDaemonConfig`/`UpdateDaemonConfig` and TUI Settings > Merge queue): `health_disk_used_pct`
(default 90, 0 off, else 50-99), `health_thermal_c` (80, 0 off, 60-110) and
`health_stuck_minutes` (30, 0 off, 5-720), mirrored to
`rsid_core::health_thresholds`. "Held for N seconds" clocks live per armed wake in
memory (`health_anomaly::sustain`) and restart with the daemon or after a fire.
A persisting condition fires again after a re-arm; the owner accepts it by adding the
printed `ignore_key=` value, verbatim, to `ignore`. Satellite readings are fetched over the link at most
every five minutes and only for a wake that watches everything.

## One Issue, one live run (#2427)

An execution whose `inputs.issue` names an Issue holds it from accept until it
settles (`blocked` and `cancelling` still hold it). Under the accepting lock,
`AgentTopologyExecute` refuses `topology_issue_already_live` (naming the live
execution, or the #2212 Issue worker) unless `supersedes_execution` names the
holding execution, which is interrupted at accept. `supersedes_execution` naming
anything that does not hold the Issue is `topology_supersedes_not_live`; it never
replaces a worker. `AgentManagerLaunchIssueWorker` refuses
`manager_issue_worker_already_live` naming the execution the same way. One query
defines the hold (`Store::live_topology_executions_for_issue`). Accept sets the
Issue `InProgress` with a note naming the execution (as the caller; best
effort); an execution that ends `failed` or `cancelled` returns it to `Open`
when no other execution or worker holds it (`topology/issue_hold.rs`).

## Worker roster (#2311)

`AgentWorkerRoster {project_id?, states?, limit?}` is a read-only agent verb with a
native tool (`rsi_control_worker_roster`) and no database schema change. A session's
`Completed` status is shown both for a worker parked on its own running job and
for one that wrote its final handoff; the roster derives the state a manager acts
on from the predicates the daemon already uses, in `store/agent_worker_roster.rs`,
so it cannot disagree with the stranded notice (#1555) or the one-worker-per-Issue
check (#2212):

| `state` | Rule |
| --- | --- |
| `running` | `Starting`, `Running` or `WaitingApproval` |
| `failed` | `Failed` or `Interrupted` |
| `final_handoff` | `Completed` and `latest_turn_final_result` (#3090: a final `RESULT`/`PIPELINE HANDOFF`/`REVIEW` line in the latest turn AND no `when.jobs_terminal` result wake still owed: a marker written while that predicate is pending, or before the last named job finished, is provisional (#2054) and the worker stays parked, because the daemon retires the wake only for a handoff written at or after the predicate settled, `wake_superseded_by_handoff`); or, with nothing parked, the worker published an Issue handoff (`issue_handoffs.author_session_id`, #2362): a reviewer whose verdict is on the review Issue but whose turn lacks the marker is done |
| `parked_on_job` | `Completed`, no final result (a provisional marker counts as none), and `owner_running_job_has_completion_wake` (an enabled `when.jobs_terminal` wake of the worker's names a `queued`/`running` job it owns) |
| `parked_on_wake` | `Completed`, no final result, and `owner_armed_result_wake` (an enabled `resume`, `when` or `on_terminal` wake the worker owns; the one liveness predicate shared with `parked_session_pending`) |
| `stranded` | `Completed`, no report, no published Issue handoff and none of the above; a job it still owns (`open_jobs`) cannot resume it |

Scope is resolved from the transport token. Without `project_id`, a project's
current appointed manager (the committed lineage tip) lists its own project, and
every other caller lists the workers it launched itself: `agent_spawn_requests`
`launched` rows it owns plus the sessions its own `AgentManagerLaunchIssueWorker`
actions created (`create_session` operations in `running`/`succeeded` whose
`actor_session_id` is the caller, #2362), so a portfolio seat sees its launches
without naming a project. With `project_id`, the project's appointed manager, or a
portfolio seat whose grant covers it (`global_project_principal`, no capability
beyond the seat), lists the project: sessions a manager created
(`create_session` operations in `running`/`succeeded`, with or without an Issue
binding) and children its Epic leads spawned. Any other caller is refused
`worker_roster_not_authorized`; a seat outside its grant is
`manager_project_not_in_scope`. Archived and deleted sessions are not listed.
In the default view (no `states`), a `stranded` worker idle for over
`WORKER_ROSTER_STALE_STRANDED_DAYS` (7) days is omitted and counted in
`stale_stranded_hidden` (#2362); `states: ["stranded"]` lists every one.
Workers come newest `updated_at` first; at most 512 are examined (deriving a
completed worker reads its latest turn) and `truncated` says when more matched or
were left unexamined.

`latest_job` is the shared minimal job summary `AgentJobSummaryV1` (`job_id`,
`kind`, `state`, `exit_code`, `refusal`, `receipt_head`); `receipt_head` is
`result.receipt.head` copied as stored. The roster never checks a receipt against
a candidate; a reader that must (the landing-candidate receipt check) compares
`receipt_head` itself.

`queued_launches` (#407) lists due new `create_session` actions in that same
authorized scope, oldest first, including targets whose session row does not
exist yet. Each row has `operation_id`, `target_session_id`, `not_before` and
the live host-admission `held_reason` (`disk_pressure`, `sandbox_free_space`,
`thermal` or `host_load`; null when host admission permits it). This list has
its own request `limit`; `truncated` also covers queued launches left out of
the page. `states`, `count` and `counts` describe existing workers only.
Disk and sandbox free-space holds request a non-dry-run reclaim pass through
the usual serialized worker, with trigger `launch_hold`, at most once every
three minutes per daemon. A reclaim submission failure observes the same
debounce and is logged; launch admission remains queued and retryable.

## Gone-target wakes (#2451)

A `Resume` wake (an agent job result, a merge-queue outcome, any other one-shot)
whose target has no session row at fire time is refused `SessionNotFound`. The
scheduler no longer retires it with only a log line: `Store::escalate_gone_wake_target`
(`store/gone_wake_escalation.rs`) first gives the outcome one durable owner and
never resumes the gone session or widens authority. The project is derived from
the producer row (`agent_jobs.wake_job_id`, `rolling_queue_entries.wake_job_id`),
the wake row, then the target's remaining row. A current manager seat gets one
ordinary `Resume` wake `gone-wake-escalation:<wake id>` (id derived from the
original wake id, so a repeated fire or a scheduler restart inserts nothing twice)
naming the wake id, original target, producer job/entry id, the refusal and the
original message. With no resolvable manager, one operator-scope to-do
`gone-wake:<wake id>` retains the same facts. A transient store failure leaves
the wake due, so the next tick retries.

## Own wakes (#2389)

`AgentListWakes {include_disabled?, limit?, session_id?}` (native tool `rsi_control_list_wakes`)
lists the caller's own wakes by default, `wake_session_id == caller`, so a
wake transferred to a successor seat (#1553, #2021) lists under the successor
and no other session's wake is disclosed. Each row carries `job_id`, `name`,
`mode` (`fresh`, `resume`, `on_terminal`, or `when` for a row with a
`$.wake_when` record), `watch_session_id`, the `when` predicate and its
`deadline`, `next_fire_at`, `enabled`, `created_at`, `protected` and
`pending_message`: set when a `manager_tier_messages` row with the wake's id is
still `queued` or `claimed` (a manager report, escalation, ruling or message
waiting inside a `resume` wake), with `kind`, `from`, `state` and a 400-character
`preview`. The store read is `Store::list_owned_wake_views`
(`scheduled_jobs.rs`); the RPC handler and both native tools share
`AgentControlHandle::agent_list_wakes`.

`session_id` (#2418) reads another session's wakes, read-only. Omitted or the
caller's own id is the self read, unchanged. Any other id is served only when
`Store::roster_authorizes_session` accepts it: the target is a live leaf worker
(not archived, deleted or the caller) in the candidate set `AgentWorkerRoster`
derives for the caller's current scope (the same two candidate queries, narrowed
to the target), i.e. the workers the caller launched itself, the caller's project
when it is that project's appointed manager at the committed lineage tip, or a
project inside a portfolio seat's grant. The check runs on every call against
live state, so a replaced manager, a narrowed grant or an archived worker is
refused at once. A refusal is `list_wakes_target_not_authorized` and does not
tell an unknown id from a foreign one. The read adds no write, cancel or create
authority; `AgentCancelWake` stays own-jobs-only. `pending_message` previews of the
target's wakes are included, as for a self read.

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
- `ConfigurePortfolioNodeLaunches {node_id, add, remove, expected_node_grant_version,
  idempotency_key, confirm_cap_reductions?}` (#2103, operator-only, outside
  `AGENT_VERBS`): adds or removes launches on a node's current grant without
  re-typing it. The daemon rebuilds the full configure request from the node
  (seat, projects, policy, child policy and direct-report limit unchanged) inside
  one transaction, CAS on the grant version (`manager_node_stale` when it moved;
  the authority epoch is read in the same transaction). `add` extends the grant's
  `allowed_launches` and the policy's list when that is non-empty; `remove`
  drops exact entries from the grant, policy and child policy, and refuses
  (`portfolio_invalid_request`) a removal that would empty a list. Added
  launches pass the same model validation as `ConfigurePortfolioNode`. TUI:
  `:manager portfolio launch add|remove <node> <Provider/model[/effort]>...`.
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
resource-policy rows. An entry with no effort permits any effort on that
provider/model; intersections retain the narrower entry. A child no-effort
entry requires a parent no-effort entry. Launch lists accept up to 64 entries.
Inspect and manager overview responses include `launches_display`, rendering
omitted effort as "any effort" while retaining the structured launch choices.
A launch refusal retains `manager_v2_launch_not_granted`
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
  same commit). `node_id` equal to the caller's own node is a self-handoff
  (#2021): the seat passes to the session this call launches, whoever granted
  the node (operator-granted nodes included), with `launch` checked against
  the node's own `allowed_launches`, `expected_grant_version` fencing the
  grant (`manager_node_stale`) and the optional request field `handoff` (a
  repo path or commit, at most 512 bytes) recorded on the appointment row
  (V170 `handoff`). Nothing is re-granted or widened. The node's terminal
  watches, queue outcomes, daemon jobs, deploys and non-protected resume wakes
  move to the successor with the seat. An operator re-seat in flight wins:
  it changes the node's epoch and seat, so the old seat's completion is
  refused `global_manager_not_seat` (the launched successor holds nothing). The old seat is refused
  from the commit on, so an exact retry after completion is refused too;
  the node's `last_seat_handoff` in `AgentManagerOverview` and
  `GetManagerNodeWorkspace` (old seat, new seat, handoff, time) shows it
  happened. #2749: once the baton passed, the retired seat's only channel to
  its successor is editing the handoff file, so the daemon stats a `handoff`
  that names an absolute or `~/` path when it assembles the workspace and sets
  `last_seat_handoff.handoff_changed_after_appointment` (with
  `handoff_modified_at`) when the file was modified after the appointment
  completed: the successor re-reads the file. A commit, a repository-relative
  path or an unreadable file is never flagged. Context-cap rotation of a portfolio seat is separate and already
  exists (`context_cap.rs`). No target has a parent or adopt field; `deny_unknown_fields`
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
  project, or the child's epoch and grant version). Portfolio appointments
  re-run their checks and publish the grant after durable session establishment,
  before provider context or the first provider turn (#1835); a publication
  refusal settles the launch without dispatching a provider. Project
  appointments finish after launch. If provider startup fails after portfolio
  publication, the failed session retains the grant. A same-key retry returns
  deduplicated success for that seat; the manager must re-seat the child with
  a new appointment key to recover. Completion is fenced on the grantor's
  authority epoch:
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

Local release detection (#363): startup and a 30-second background scan read
`<harness repo>/target/release/rsid` and the daemon-owned persistent release
target (`<data dir>/staging/release-target/release/rsid`, with `.exe` on Windows).
The scan caches `--build-info` by artifact mtime and size, with a two-second
deadline and 1 KiB output limit per probe. Missing, unknown or unreadable builds
are optional, never startup or RPC errors. No build is installed by detection.
`GetDaemonInfo` (operator-only) returns `DaemonInfoV1`; `AgentGetDaemonInfo.hub`
uses the same cache. Their additive optional `local_build` carries `sha`, `path`,
`built_at` (artifact mtime) and `differs` (different SHA, strictly newer than the
running executable captured at startup). Checkout HEAD does not decide recency.
One warning per differing SHA names both builds and `make release-install`.
The TUI's background health poll reads `GetDaemonInfo` every 15 seconds and
keeps a wrapped banner below the top bar while `differs` holds. A successful
matching/absent read clears it; a transient failed read preserves it. An older
daemon without `GetDaemonInfo` simply has no banner. Rebuilding alone does not
change which binary the supervisor starts: use `make release-install` to stage
the verified quiet-point install and restart.

- **Why a deploy waits (#2056).** While a deploy is `staged`, the
  `AgentRequestDeploy` receipt (a replay with the same key is the way to ask
  again) and the satellite `last_deploy` in `AgentGetDaemonInfo` (which also reads its own daemon's as `hub.last_deploy`, with `finished_at` once settled, hub #2980) carry
  `blockers` (kind plus up to 8 subjects: gating queue `queue_sequence` and
  `batch_id`, running `job_id`/`job_kind`/`owner_session_id`, mid-turn worker
  `session_id`; `subjects_total` is the full count) and `waiting_since` (the
  request time). They are read live from the same query set as the gate's
  blocker names (`deploy_quiet_blocker_details_with`); no migration.
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
- **Tool scripts (#2722).** `scripts/cargo-slot`, `scripts/target-pool` and
  `scripts/rsi-spill` (hub #2958: `~/.local/bin/rsi-spill` links this stable
  `~/.rsi/bin` copy, never the installing checkout) are
  optional manifest artifacts like the supervisor: put them in `binaries_dir`
  (`make release-install` packages them) and the deploy publishes them into
  `~/.rsi/bin`, where the daemon runs them (job `cargo-slot`, sandbox
  seeding by `target-pool`), not `~/.local/bin`. `cargo-slot` is the exception
  (#2777): the staged full script becomes the implementation `~/.rsi/lib/cargo-slot`
  and the daemon itself stages the embedded `scripts/cargo-slot-wrapper` as
  manifest entry `cargo-slot-wrapper` at `~/.rsi/bin/cargo-slot` whenever that
  file is absent or not the wrapper (an old daemon's full script, a bare host),
  after the `lib` entry so `lib` is published first; a rollback restores the old
  `bin` file and removes a first-time `lib`. `installed_tools` reports
  `cargo-slot` installed only behind the wrapper. They are hash-verified,
  `cargo-slot` is checked with `bash -n` and `target-pool` with a Python parse
  before staging succeeds, and the swap is the manifest's atomic rename, so a
  running copy keeps its inode (`<name>.prev` until the deploy verifies). A
  `binaries_dir` without them is not refused: the receipt's `skipped` list
  names each (never a silent skip). `AgentGetDaemonInfo` `hub.installed_tools`
  lists both with `installed` and the file's `sha256`, so a missing or stale tool
  is visible (the hub-side `target-pool` absence of #2722 made worker sandboxes
  unseeded with no report). A satellite deploy (`peer_id`) runs the same code on
  the satellite, whose own `AgentGetDaemonInfo` reports its tools.
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
- **Cancel.** Send the deploy's SHA and idempotency key with `cancel: true`.
  With `peer_id`, the hub forwards the cancellation over the paired deploy link;
  the satellite applies its own owner and idempotency checks against its stored
  deploy. A waiting deploy settles as `deploy_cancelled`; after restart begins
  or the deploy has settled, the satellite refuses with
  `deploy_cancel_too_late`. The hub keeps no deploy row for peer requests.
- **Queue admission (#1806, #2190).** The in-flight batch finishes, but the
  queue admits no NEW batch while a deploy is staged, from its first admission
  opportunity to the deploy's own deadline (not bounded by
  `deploy_drain_hold_secs`), so a stream of landings cannot starve the drain of
  its quiet point. Disabling the deploy drain, cancellation, settlement or the
  deadline releases the queue.
  `AgentGetDaemonInfo.deploy_drain.held` reports `queue_batch` while sources
  are deferred; `draining` and `release_by` include that hold.
- **Restart order (#2201, #2202).** The hold is in-memory, so a restarted daemon
  rebuilds it from the durable live deploy row (or, for a timed-out `interrupt_workers`
  deploy's retry window, its last row, #2552) before it starts any admission:
  `deploy::start_admission_after_restore` applies the drain settings, engages
  exactly what a deploy poll would (`restore_drain_at_startup`), and only then
  spawns the merge-queue, deploy and job loops. The first job-loop pass and the
  first queue admission after a restart therefore see a staged deploy's hold;
  a restore failure fails startup instead of admitting past an unread hold.
- **Restart budget (#1777).** The operator setting `deploy_restarts_per_hour`
  (1-3, default 2; live; manager-adjustable through `ProposeDaemonSetting`
  within operator bounds) caps deploy restarts in a rolling hour; 3 is the
  supervisor's own restart cap. Over it the request, or the quiet-point
  restart, is refused `deploy_restart_budget retry_after=<RFC3339 seconds,
  Z>`: the time the oldest restart that must leave the window does. Satellite
  pass-through keeps the bare code.
- **Shutdown backstop (#2561).** A daemon shutdown that never finishes is
  ended by `rsid-supervisor.sh`. The daemon writes `pid=<its pid> reason=<code>`
  to the file named in `RSID_SHUTDOWN_MARKER` when a shutdown begins (SIGTERM,
  Ctrl-C, or an operator DRAIN restart, `supervisor_shutdown::mark_shutdown_begun`);
  the supervisor being told to stop counts too. The daemon reads that variable
  once at the top of `main` and removes it from its environment
  (`capture_marker_path`), so no worker, job unit, lander or test inherits a path
  to the real marker (#2594); the supervisor starts its grace clock only for a
  marker naming its own child's pid and removes one naming any other pid.
  `rsid.service` sets `TimeoutStopSec=3700` so a stop outlasts the largest grace
  plus the SIGTERM wait (#2590). The operator setting
  `supervisor_shutdown_grace_secs` (60-3600, default 120; live; TUI Settings >
  Merge queue "Shutdown grace (s)"; line 2 of the `deploy-restart-budget`
  snapshot, re-read when a shutdown begins) is the allowance. Past it the
  supervisor sends SIGTERM to the daemon's process group (started under job
  control, so its group id is its pid), waits a few seconds, sends SIGKILL,
  and appends one line to `~/.rsi/forced-shutdown-incident`. It then takes the
  exit-75 path: restart, or, for a just-restarted binary whose deploy marker is
  still present and that wedged inside the fast-failure window, the rollback
  to `.prev`. After a supervisor stop it exits instead of restarting. The next
  daemon start (after readiness) reads and consumes the file
  (`supervisor_shutdown::surface_incidents`): one `forced_shutdown:<trigger>`
  friction event (trigger `daemon_shutdown` or `supervisor_stop`) and one
  `session_state` manager notice carrying `daemon_forced_shutdown`, so the
  forced end is never silent. The process-group kill is Unix-only; on
  Windows (no supervisor) the script says the backstop is unsupported.
- **`interrupt_workers: true`** (optional, default false; V158 columns
  `agent_deploys.interrupt_workers` and `interrupted_sessions`). Once the hold
  is over (`created_at + deploy_drain_hold_secs`; with the drain setting off
  there is no hold, so it is over at once) `worker_mid_turn` stops blocking
  (`Store::deploy_quiet_blockers_with(caller, false)`). `landing_in_progress`
  and `job_running` still block, so a landing or a local build/landing job
  is never cut off; running `test`/`scoped_test` jobs are bounded by the grace
  below. Inside the hold, and for a deploy that did not ask, nothing
  changes. `max_wait_secs` still bounds the wait: a wait shorter than the hold
  ends in `timed_out` as before, so keep it above the hold. Not available with
  `cancel`. With `peer_id` (#2587) the hub forwards it (and `max_wait_secs`) in the
  satellite deploy request; the satellite runs its own flow, so its hold and its
  workers are the ones that count, and the worker resumes on the new satellite
  build. Compatibility: the satellite advertises `capabilities.deploy_interrupt`
  in its identity; the field is sent only when asked (`true`), and an older
  satellite (no capability, it would refuse the unknown field) is refused
  `satellite_deploy_interrupt_unsupported` before anything is sent, never
  silently run without it. The flag is part of the replay fingerprint (a request without it
  keeps the old fingerprint), so a replay cannot flip it.
- **Job grace (#2048; supersedes the AWS-era #1177 idea of leaving jobs alone).**
  A fixed policy constant, not an operator setting (`DEPLOY_JOB_GRACE_SECS` =
  300, counted from the request; it only bounds re-runnable verification and
  the receipt reports it). Past it, each agent-jobs tick
  (`agent_jobs::cancel_test_jobs_for_deploy`) stops every running `test` job of
  a staged `interrupt_workers` deploy: the unit is stopped and (#2278) the job
  goes back to `queued` under the same id, owner, params and wake policy
  (`Store::requeue_agent_job_for_deploy`; `held_for_deploy`, the deploy recorded
  as `requeued_by_deploy` in the params JSON, no schema change). It reruns on
  the new daemon once the hold ends, and the owner's wake (per-job wake or a
  `when.jobs_terminal` wait on that id) fires once, when the rerun ends, so
  nothing is resubmitted by hand. `build`,
  `landing` and cloud jobs and the queue's lander are never stopped and keep
  blocking. #2176: a hub's full-library QA run on a satellite is stopped the
  same way by any staged deploy at once (no `interrupt_workers`, no grace, and
  even while a landing blocks the deploy: QA is preemptible, the #2190 order below
  spares only ordinary test jobs); it is not requeued: it settles `failed` with refusal
  `interrupted_by_deploy` and `deploy_id` in its result, and the hub defers it as
  `deploy_preempted`. The receipt carries `job_grace_secs` and `cancelled_jobs`
  (`Store::deploy_cancelled_jobs`: requeued jobs through `requeued_by_deploy`, settled ones
  through the result's `deploy_id`), and the settle wake lists both kinds. Order (#2190):
  stopping test jobs gains nothing while the deploy is blocked by something it
  cannot preempt (`Store::deploy_unpreemptible_blockers`: an admitted or gating
  queue landing, a running `build` or `landing` job), so none is stopped
  meanwhile (ordinary test jobs only) and the grace runs from the latest tick that saw such a blocker
  (the request when none was seen). New `test`/`build` jobs are held for the
  deploy throughout (see "Held for a staged deploy" under "Durable agent
  jobs"), so only a job already running when the deploy was staged is stopped.
  **Retry window (#2278, #2551-#2553).** The job hold ends at the deploy's deadline, so a
  deploy that times out (a landing or build it cannot preempt held it to its deadline) would
  start every held job at once and meet its retry with a fresh crop of running jobs. A staged
  `interrupt_workers` deploy therefore installs its retry window together with its job hold
  (`hold_jobs_until_settled`): new and requeued `test`/`build` jobs stay held from the deadline to
  `deadline_at + DEPLOY_JOB_RETRY_HOLD_SECS` (300 s, `retry_hold_until`). The deadline is the one
  clock boundary: the job hold is continuous across it, so the jobs loop or a submission cannot
  admit a job between the deadline and the runner's timeout settlement. The window is owned by
  its deploy id: only that deploy's cancel releases it (`release(id)`; a replayed cancel of an
  older deploy leaves a newer deploy's window alone, #2553), a retry staged inside the window
  takes it over (`hold_jobs_until_settled`), and a deploy that settles without timing out drops
  it (`reconcile_retry_hold` on each poll). It is derived from the durable row, not kept only
  in memory (#2552): at startup `restore_drain_at_startup` reinstalls it when the newest deploy
  is `timed_out` with `interrupt_workers`, before any job is admitted, so an unplanned restart
  neither releases the held jobs early nor extends the window past its original end.
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
  marks the turn Interrupted, `claim_restart_intents_at_startup` claims the
  intent before readiness (the row stays `claimed`, so the topology executor
  still treats the session as journal-owned) and a task spawned after
  `request_ready` (`continue_claimed_restart_intents`, journal order, #2375)
  dispatches the continuation with the daemon's continue prompt. Pinned by
  `deploy_interrupt_resume::a_worker_cut_off_by_a_deploy_restart_is_continued_by_the_next_process`.
- **DRAIN wait early end (#2353).** The 30 s wait (`OPERATOR_DRAIN_LIMIT`) ends as
  soon as every remaining session is `waiting_on_daemon` (the state shown by
  `GetDrainRestartStatus`): its only open work is a call the daemon is serving, a
  turn that cannot end while the daemon drains. The daemon proves it, never from
  idle time: `RpcServer` registers every admitted tokened request in
  `SessionManager.daemon_wait` for as long as its handler runs; a call counts after
  1 s in flight (`DAEMON_WAIT_SETTLE`); and the Claude CLI stream feeds a
  per-turn tool ledger (`TrackedSession.turn_tools`). The turn qualifies only when
  every open tool is a typed `rsi-agent` control tool, so a manager parked on a pure
  control call such as `AgentEnqueueLandingSource` still ends early. A shell (`Bash`)
  tool never qualifies (#2388, #2387): a shell can run builtins with no child program,
  and a process listing cannot tell it from a shell blocked on its client nor always
  read a working sibling, so any open shell tool keeps the full wait. The
  classification is one synchronous read (#2387): `drain_entries` classifies every
  session under a single read of the active map (the monitor updates the ledger under
  its write lock) and the registry, with no await between, and the decision acts on
  that read; a tool that opens or a call that completes before it is a full wait
  (pinned by `a_tool_that_opens_or_a_call_that_completes_during_the_decision_keeps_the_full_wait`).
  Any other tool (edit, read, search, subagent, a shell), a turn with no open tool
  (the model is sampling), a turn
  adopted mid-way, a pending question and every non-Claude provider keep the full
  wait. `shutdown_drain` waits on the same
  classification. Cut turns take the Resume path above, pinned by
  `deploy_interrupt_resume::a_worker_blocked_on_a_daemon_call_ends_the_drain_early_and_is_continued_by_the_next_process`.
  At the connection drain the daemon also closes connections idle between requests
  and ends push streams (`RpcServer::begin_connection_drain`) instead of waiting
  out `CONNECTION_DRAIN_LIMIT`, including a push stream parked in a write to a
  reader that stopped reading (#2383); a connection with a request begun keeps the
  deadline. The phase log lines are unchanged; one added line,
  `Operator DRAIN wait ends early`, names the daemon-waiting count.

## Satellite manager policy push (#1748)

`AgentPushSatelliteManagerPolicy` is RPC-only and uses Deploy authority:
current PM or portfolio seat, Execute, unpaused, project in its grant. The hub
checks a paired, enabled/read/dispatch peer and an exact operator-declared
remote manager address: a session id, or a project id (#1928, "Satellite scope
follows the seat" below). It compares every provider/model/effort triple with the
live effective launch list and compares created/active/provider caps with the
live grant, including parent ceilings. A missing provider ceiling in the push
is widening when the hub has one. Widening returns
`satellite_policy_bounds_exceeded`, never clips. Empty launch lists are refused
because a delegated empty list can mean inheritance.

The custody-checked deploy socket and paired identity handshake carry
`RequestHubManagerPolicy`; it stays operator-only, never agent/read/native or
delegable. The hub snapshots authority, the wire request and the paired link
under its Store mutex, then releases it before the exchange, as for deploy.
Admission uses that snapshot; later hub edits apply to subsequent calls. The satellite
requires an allowlisted hub, the declared address in its own inbound scope
resolved to a live seat (a session root's rotation tip, or a project's current
appointed manager) and a current, unrevoked PM appointment. Every authorization
denial is `target_not_authorized`. Installation/seat fields are supplied by the hub;
agent callers cannot supply them.

The satellite derives the project from that appointment and checks
`expected_scope_version` and `expected_policy_version` (0 means no saved
policy). Scope/policy mismatch returns `satellite_policy_stale`. It changes
only `allowed_launches`, `max_created_sessions`, `max_active_sessions` and
`provider_limits`, preserving mode, pause, capabilities and every other
operator field. The existing policy-save transaction bumps row_version and
records a `satellite_policy` event and operation receipt with hub installation
and sender seat. No migration. A key is namespaced by hub and sender and the
whole wire request is fingerprinted: changed payload returns
`satellite_policy_key_conflict`; exact retries return the original receipt
with `deduplicated:true`, including after an operator's later save or scope
version change, without reapplying. Current inbound and manager authority
still gate replays, and the stored receipt retains its `deduplicated` field.
Unknown remote refusal text collapses to
`satellite_policy_refused`; link/protocol failure is
`satellite_policy_unreachable`, safe to retry identically.

## Satellite scope follows the seat (#1928)

A satellite dispatch scope entry names a **fixed session id** or a **project
id**. A session id keeps its #1112 meaning (the lineage tip of that session;
its `parent_id` descendants). A project id means "the project's currently
appointed manager" and is resolved live by the host that owns the sessions, so
the declaration survives self-succession and operator re-appointment with no
edit on either host. Both hosts declare it, both operator-only (never in
`AGENT_VERBS`, `READ_VERBS`, native tools or the agent CLI catalog):

- Hub: `PutSatellitePeerScope` takes `remote_session_ids` and
  `remote_project_ids` (replaced together under the registry revision fence);
  `ListSatellitePeers` returns `dispatch_scope` and `dispatch_project_scope`.
  Tables `satellite_peer_scope` and `satellite_peer_project_scope`.
- Satellite: `PutSatelliteInboundPolicy` takes `scope_roots` and
  `scope_projects` (table `satellite_inbound_project_scope`).
  `GetSatelliteScopeStatus` returns every declared inbound entry with its live
  state (`live`, `unappointed`, `revoked`, `no_live_seat`) and resolved seat.
- TUI satellites view: a scope field accepts a bare uuid (session) or
  `project:<uuid>`; the view shows each peer's declared scope and "Inbound
  scope (this host)" with the live state and seat.

**One resolver.** `store/satellite_scope.rs` is the only place a declared
entry becomes a seat, and message delivery, manager-policy push, hub deploy
owner selection and satellite reports (`queue_hub_report`) all call it, so the
four paths cannot disagree. Resolution fails closed. A project resolves to
`get_harness_manager(project).current_session_id` and is `unappointed` with no
appointment, `revoked` when the appointment or its manager policy is revoked
(an explicit empty scope), and `no_live_seat` when the seat is missing or
archived. Every refusal toward the hub is the uniform `target_not_authorized`.

**What each host verifies.** The hub cannot see the satellite's sessions or
appointments, so it verifies only the *declared* address. The satellite alone
resolves the seat and decides whether a target is in scope. An address (a
declared session id or the project id) is the target. For a session the hub
has never seen, such as a descendant of the seat, the message carries the
project that authorizes it **separately from the session target**:
`AgentSendSatelliteMessage.scope_project_id` (optional), stored beside the
queued message (`satellite_message_scope_project`, append-only) and re-checked
on every delivery attempt, then sent as `DeliverHubMessage.scope_project_id`.
With a `scope_project_id` the hub requires the *peer* to declare that project
(the target is then unrestricted at the hub); without one the target itself must
be a declared address. The satellite, given a `scope_project_id`, requires the
project in its own inbound scope, resolves its live seat, and accepts the
target only when it is that seat, a rotation predecessor of it, or a `parent_id`
descendant of either, in the project (walk bounded at 32); it delivers to the
target's own rotation tip. A re-appointment therefore takes the previous seat's
tree out of scope on the next message with no scope edit, and a stray project
the satellite did not declare authorizes nothing. A recorded delivery is still
replayed from its receipt after the seat moved (phase one is static, phase two
live).

**Delivery to a busy target (#2566).** The hub does not hold a message for a
running target. The satellite resolves the target's rotation tip and, when it is
`Completed`, continues it (exactly once, write-ahead attempt row); when it is
`Starting`, `Running` or `WaitingApproval`, it writes one operator-queue row
(`operator_messages`, keyed by the hub message id, so a replay queues nothing
new) and answers `delivered`. The per-session `PostToolUse` hook then shows it at
the next tool boundary (#1049) and the turn-end dispatcher delivers it otherwise;
`sent` on the hub means the satellite durably accepted it, not that the model has
read it. A target that is `Interrupted`, failed or archived is still refused
`busy` and the row waits. A failed queue write is a definite non-effect (`busy`,
retried). The hub receipt (and a replay of the same idempotent request, the typed
status read) carries `attempt_count` and, while queued or after expiry,
`waiting_for`: `remote_busy` (interrupted target, or a satellite that refused
`busy`) or `peer_unreachable`. The reason lives in `satellite_messages.safe_error_class`
while the row is `queued` and is overwritten when it settles. A message still
`queued` `SATELLITE_MESSAGE_STALL_NOTICE_SECS` (10 minutes) after acceptance
notifies the sender once (`<id>:stalled` notice, `stalled: true`, `waiting_for`,
`attempt_count`); a later failure or expiry still files its own #2456 notice.

**Policy push after re-seating.** `AgentPushSatelliteManagerPolicy` addresses
the declared project id (or fixed session id) in `remote_session_id` and
reaches whoever is appointed now. Re-appointing a manager revokes the old
policy grant (#1145), so the resolver reports the entry `revoked` and the push
is refused `target_not_authorized` until the operator saves the new manager's
policy on the satellite; after that, pushes follow the seat with no scope edit.

**Reseat after a provider outage (#2750).** A session's provider is fixed at
creation and `succeed_manager` must be called by the seat itself, so a manager
seat whose provider is down (`Failed` after a lost login or quota) cannot hand
over. `ReseatHarnessManager {project_id, provider, model?, effort?, reason,
provider_unavailable?}` (operator-only, never delegable; TUI
`:manager-reseat <provider>[/<model>] <reason>`, run twice to confirm) replaces
it in one step: `SessionManager::reseat_harness_manager` launches a root successor
on the new provider with `continued_from` the failed seat (same working dir,
tags, role, sandbox kind), then `Store::commit_manager_reseat` re-appoints it
through the ordinary scope save (so the displaced seat's wakes, queued tier mail
and open requests follow, #1553/#2468/#2469) and, in the same transaction,
re-grants the project's saved policy to the new tuple instead of leaving a
revoked draft (a policy that was already revoked stays a draft: a reseat never
confirms authority nobody confirmed). The reason is written as a
`seat_reseat` `harness_manager_v2_events` record and as a System row on the
replaced seat; the successor's first prompt carries it. The seat must be the
project's appointed, parentless Standard manager; `Failed` always qualifies,
`Completed`/`Interrupted` only with `provider_unavailable: true`, and a running,
starting or approval-waiting seat is refused `manager_seat_live`
(`manager_seat_not_failed`, `manager_reseat_not_configured`,
`manager_reseat_revoked`, `manager_reseat_no_seat`,
`manager_reseat_root_required` are the other refusals). A reseat whose
re-appointment is refused interrupts the successor it launched. Session-pinned
scopes need no edit on either host: the hub's peer scope and the satellite's
inbound roots name the failed seat's id, and `satellite_delivery_tip` resolves
that id to the successor through `continued_from` (a project-scoped entry
resolves through the appointment). `AgentReseatSatelliteManager {peer_id,
remote_session_id, provider, model?, effort?, reason, provider_unavailable?}`
(RPC-only, Deploy authority, same paired link and declared-address check as the
policy push, wire method `RequestHubManagerReseat`, also operator-only and never
delegable) asks a paired satellite to do the same for the manager its declared
address resolves to; the provider/model/effort must be inside the hub's own
effective launch grant (`satellite_reseat_bounds_exceeded`), and the satellite
answers with the same result. A retry after success finds the healthy successor
and is refused `manager_seat_live`.

**Deploy and reports.** The satellite deploy owner is the live seat of the
first declared entry that resolves (replay identity stays the declared id);
`AgentReportToHub` is accepted only from a live seat. Schema V166 adds three
tables and no column to a released one.

**Report routing and state (#2242).** A satellite report carries no hub session
id. The hub records it as a `satellite_report` notice for the CURRENT manager of
the project it routes to (`record_satellite_report`), and every pull round
re-homes the project's retired, never-retrieved report notices onto that manager
(`adopt_orphaned_satellite_reports_for_peer`), because a re-appointment retires
the old scope's unretrieved notices (#1310) after the satellite was already
acknowledged. The satellite keeps a process-local ledger per report
(`HubReportOutbox`): `AgentGetDaemonInfo.hub_reports` returns it to the
reporting seat (`queued`, `fetched`, `held`, `delivered`, `refused`,
`undelivered`) plus a `backlog` summary (`depth`, `max`, `oldest_queued_at`,
`oldest_age_secs`, `by_kind` counts of `result`/`enqueue`/`deploy_ready`, #135),
`AgentReportToHub` refuses `satellite_report_hub_unreachable`
while the hub has not pulled for 180 s, refuses `satellite_report_queue_full` at 32
unacknowledged reports with the same summary in the message (`depth=N/32`,
`oldest_queued_at`, `oldest_age_secs`, `pending_by_kind=result:N,enqueue:N,deploy_ready:N`,
`last_hub_pull_at`, `hub_pulling`, and a `next:` step; nothing pending is dropped and
reports are never coalesced), and the hub's `held`/`refused` verdicts
ride the `outcomes` field of the next `FetchHubReports` request, sent only to a
satellite whose identity has `capabilities.report_outcomes` (an older build
refuses unknown request fields).

**Pending mail across a seat or scope change (#2469).** A replacement bumps the
`(manager_session_id, scope_version)` tuple every manager message and notice is
bound to, which hides the old tuple's mail from the successor. The old rows stay
immutable; the replacement writes live copies under the new tuple, only while the
Epic is inside the live scope with a valid lead (a narrowed scope carries nothing
foreign) and the project still has a live, non-revoked seat. Open requests are
re-issued to their lead through the lead-replacement `request_readdress` link
(newest `MAX_READDRESSED_REQUESTS` per Epic). Never-retrieved lead-to-manager
mail notices re-issue their reply or lead notice (key `adopt:<source id>`, source
text, sender and timestamp copied). Never-retrieved `ledger_change` notices the
manager may retrieve are re-inserted with a `carried_over` marker. The page that
is retired carries first (`adopt_manager_scope_retirement_page`), next to the
#2455 escalation. Not carried: action results (#2455), satellite reports
(#2242), `session_state` (re-derived), operator answers (the scope bump revokes
the policy grant, so the successor inherits no authority). A typed refusal skips
that one item (savepoint); a storage error fails the change.

## Satellite message `issue_refs` (#2359)

A satellite cannot read hub Issues: its `AgentGetIssue` reads its own store, and the
hub keeps no map from a peer to a hub project (a peer declares only satellite
projects and sessions). `AgentSendSatelliteMessage` therefore carries `issue_refs`
(at most 8 distinct display numbers) and the hub expands them, whole and read-only,
into the delivered message (title, status, priority, labels and the complete body,
which holds the acceptance). Nothing is added to the satellite side: no new wire method, no
satellite-to-hub read, no Issue write path, no operator setting.

- **Scope is the sender's, never the request's.** Each ref goes through the same
  `agent_get_issue_scope_tx` as the sender's own `AgentGetIssue`, so a ref discloses
  nothing that read would refuse: no project is named in the request, a manager reads
  only its own project, and a missing Issue, another project's number and a sender
  without Issue authority all return the one `satellite_issue_ref_unavailable`.
- **Authorize first, read second.** The peer and target checks (`target_not_authorized`)
  run before any Issue is read, so an unauthorized peer learns nothing about Issues.
- **Never truncated.** The message plus every Issue must fit
  `SATELLITE_MESSAGE_MAX_BYTES` (16 KiB, the stored payload's CHECK and the wire bound);
  otherwise `satellite_issue_refs_too_large` and nothing is queued. Send fewer Issues
  per message. Malformed refs are `satellite_issue_ref_invalid`.
- **Framed as data.** Each Issue sits between fences that carry the body's byte length
  and the first 16 hex of its SHA-256; a body cannot embed its own hash, so it cannot forge
  the closing fence, and the receiver can check the body arrived whole. The header states
  it is a snapshot of hub data, not the sender's own words.
- **One snapshot.** The expansion is taken once, at acceptance, and stored as the
  payload the dispatcher delivers; the request fingerprint covers the refs, not the
  Issue text, so a replay returns the first receipt after an Issue edit and a replay
  with other refs is `satellite_message_key_conflict`. A request without refs keeps its
  old fingerprint and payload.

## Worker CPU quota and scope teardown (#1992)

- **Setting.** Operator setting `worker_cpu_quota_percent` (0..=102400, default
  400 = four CPUs; 0 disables; live for scopes started afterwards; TUI row
  "Worker CPU quota", `GetDaemonConfig`/`UpdateDaemonConfig`). The store mirrors
  it into `rsid_core::worker_cpu_quota`; `WorkerScopeLimits::from_launcher_snapshot`
  reads it per launch and `ScopedWorkerCommand` adds `--property=CPUQuota=N%`.
  Linux only (other platforms launch without a scope).
- **Halt kills the scope.** `signal_process_group_because` sends the
  group `SIGKILL` and, when the leader lives in an `rsi-worker-*.scope`, also
  `systemctl --user kill --kill-whom=all --signal=SIGKILL <scope>`, so a
  descendant that left the process group dies with the worker.

## Fast compile-feedback lane (#2430)

A worker may not run cargo in the foreground, so its compile errors come back
through an `AgentSubmitJob` `build` job (`{"kind":"build","params":{"command":
"check","package":"<crate>"}}`). Those `cargo check` runs used to share the
build pool with full builds and test runs and queued behind them.

- **Class.** `scripts/cargo-slot` classifies a run whose command line has
  `cargo check` as the governor class `check` (`AdmissionClass::Check`):
  its own pool and FIFO, sized by operator setting `governor_check_slots` (1..=16,
  default 2; live; TUI Settings > Orchestration "Check slots",
  `GetDaemonConfig`/`UpdateDaemonConfig`), so full builds and test runs never
  fill it. `cargo build`, `cargo test` and `scripts/scoped-test` stay `build`.
- **Every crate's errors in one pass (#2516).** Every `check` build job passes
  `--keep-going`, so cargo keeps compiling crates that do not depend on a failed
  one and the log shows each crate's errors, not only the first failing crate's.
  A worker iterating on a change that spans crates submits one
  `{"command":"check","workspace":true,"all_targets":true}` job; it does not count
  against the one `scoped_test` job. `cargo build` jobs are unchanged.
- **Gates.** The disk and memory floors apply to the check class. The load gate
  does not: the lane exists to give feedback while full builds saturate the host,
  and the `rsi-build.slice` CPU cap (below) still bounds it. Build and lander
  classes keep every gate.
- **Without the daemon** `cargo-slot` uses `RSI_CHECK_SLOTS` (default 2) local
  `check-slot-N.lock` files and skips the local load gate.
- **Unchanged.** Jobs of one sandbox still run one at a time (they share one
  Cargo target), so a check submitted while the sandbox's own test job runs
  waits for it. `GetResourceGovernor` reports `check_in_use` and the queue/lease
  views carry `class: "check"`.

## Shard-feature test builds (#2692)

A shard build is `cargo test -p PACKAGE --lib --no-run --no-default-features --features
test-shard-NAME`. It is the evidence that a test-module edit compiles in every shard, and the
only place the shard configurations' compiler warnings show. A worker never self-launches it
(a self-started `systemd-run` unit ran outside `rsi-build.slice` with no CPU cap, and each
shard's rsid harness left its own copy in the target: 96 GB); it submits a `build` job.

- **Request.** `{"kind":"build","params":{"command":"test_no_run","package":"rsid",
  "shards":["store-01","other-02"]}}` builds the shards one after another, each with
  `--no-default-features`. `features:["test-shard-NAME"]` (with optional
  `no_default_features`) is the one-build form; `features` and `shards` are exclusive, at most
  32 each, every feature must be a `test-shard-*` name and `package` is required (no
  `workspace`, `all_targets` or `release`). `check` and `build` take none of these fields.
- **Execution.** The unit runs `scripts/shard-feature-sweep` under `cargo-slot` in
  `rsi-build.slice`, so the hub CPU budget, `cpu_quota_percent` and the memory and disk floors
  apply like any build. The script cleans the package (`cargo clean -p PACKAGE --profile test`)
  after every shard, so the target holds one shard's harness at a time; a failed shard does not
  stop the sweep. The unit's cap is 2 h plus half an hour per shard beyond the first, at most 8 h.
- **Receipt.** `result.receipt` is the last `SHARD_SWEEP_RECEIPT` line: `{ok, partial, package,
  shards:[{shard, features, no_default_features, exit_code, warnings:{package:count}, degraded,
  clean_exit_code, errors?}], warnings}`. `warnings` is the #1897 baseline gate over the whole
  sweep (`docs/agents/landing-and-gates.md`, "Compiler-warning gate"). The job settles
  `succeeded` only with exit 0 and `ok:true` (every shard built, every clean worked, the gate is
  not `new_warnings`/`baseline_raised`/`unavailable`); a run that printed no receipt settles
  `failed` with refusal `shard_sweep_receipt_missing`.

## Hub build CPU budget (#2119)

Per-unit `CPUQuota` caps do not bound concurrency: N job units each at the
host-derived ceiling take N x that many cores and compete with the desktop. One
hub-wide budget now covers every cargo build and test unit RSI starts.

- **Setting.** Operator setting `hub_build_cpu_percent` (10..=100, default 50;
  live; TUI Settings > Orchestration "Hub build CPU budget",
  `GetDaemonConfig`/`UpdateDaemonConfig`). The store mirrors it into
  `rsid_core::build_slice`.
- **The slice.** `rsi-build.slice` (an implicit user slice, `rsi.slice/rsi-build.slice`)
  carries `CPUQuota = hub_build_cpu_percent x logical CPUs` (50% of 32 threads is
  `1600%`, `systemctl --user show rsi-build.slice -p CPUQuotaPerSecUSec` reads
  `16s`) and `CPUWeight=20`. `rsid_core::build_slice::apply_with` compares the
  slice with `systemctl --user show` and writes only a difference
  (`systemctl --user set-property --runtime`), so it is idempotent and applies
  live to units already running in the slice. The daemon applies it at start
  (`rsid::build_budget::apply("start")`), on every accepted
  `UpdateDaemonConfig` of the field (the response then carries `build_slice`:
  `state` `applied`/`unchanged`/`unavailable`, `cpu_quota_percent`,
  `budget_cores`, `detail`) and before each unit it launches in the slice. The
  setting is saved even when the slice cannot be written; `unavailable` names why
  and units run with their own limits. Linux (systemd user manager) only: other
  platforms report `unavailable` ("needs a Linux systemd user manager"). The
  host size is the online CPU count (`sysconf`), never the calling process's
  cgroup-limited parallelism, and `test-seam` builds (every daemon test and
  fixture) never write the real slice: it belongs to the user's systemd manager,
  shared with the running daemon.
- **What runs in it.** `AgentSubmitJob` units with a build environment (`test`,
  `build`, `landing`, `cloud_gate`; `--slice=rsi-build.slice` on the
  `systemd-run` line, the job log gets an `rsi-build-slice` line); the nested
  `rsi-scoped-test-*.service` units of `scripts/scoped-test`
  (`RSI_BUILD_SLICE` overrides the name, empty opts out) and of
  `scripts/systemd-test-fallback`; the merge-queue lander, which the daemon starts
  as `systemd-run --user --scope --slice=rsi-build.slice --property=PartOf=rsid.service`
  (`rsi-lander-<uuid>.scope`, exec'd in place so its pid, process group and pipes
  are unchanged; a timed-out or failed run also gets `systemctl kill` on its
  scope; an unconfigurable slice runs the lander unscoped); the
  `rolling-library-qa` recipe is an ordinary job unit. Provider worker scopes
  (`rsi-workers.slice`) are not builds and are untouched.
- **Per-job threads.** A job's CPU ceiling is `min(its own ceiling, budget cores)`
  (`rsid_core::build_slice::per_job_ceiling_percent`): the `scoped_test` ceiling
  (`clamp(host threads / 2, 2, 12)` cores, #2087), the default 1600% of other
  cargo jobs and a recipe's declared quota. `scripts/scoped-test` derives its test
  threads inside `--cpu-quota`, and the lander's `available_parallelism` already
  honours the slice's quota, so concurrent jobs share the budget.
- **Andon.** `cpu_andon` samples `rsi-job-<id>.service` and the job's nested
  `rsi-scoped-test-<job id>-<hex>.service` (a separate cgroup, so the job unit
  holds only the driver; #2127) under `rsi.slice/rsi-build.slice` as well as
  `app.slice` (units that predate the slice). A scoped-test unit without a job
  id (an operator run) is no agent tree.
- **Parent slice (#2126).** `rsi.slice` (the daemon, `rsi-workers.slice` and the
  build slice) gets `CPUWeight=30` (`PARENT_CPU_WEIGHT`, the `background.slice`
  class) against the desktop's `app.slice` at 100, so the desktop wins CPU
  contention, and the SAME `CPUQuota` as the build slice (`hub_build_cpu_percent`
  of the host's logical CPUs), so a cargo run in a worker's own scope, which
  never enters `rsi-build.slice`, is bounded too (agent CLIs are light; the
  budget is effectively for builds). The relative weights inside it (workers 200,
  builds 20) are unchanged. `apply_weighted_with` writes both idempotently
  right after the build slice, at daemon start, on every accepted
  `hub_build_cpu_percent` change and before each unit launch; Linux only
  (elsewhere, and in `test-seam` builds, nothing is written). The response's
  `build_slice.parent_state` says what happened. Read it back with
  `systemctl --user show rsi.slice -p CPUWeight -p CPUQuotaPerSecUSec`.
- **Who writes the slices (#2282).** Both slices belong to the user's systemd
  manager, shared by every process on the host, so only the user's own hub
  daemon writes them, and only with a value the store gave it. At start the
  daemon publishes the loaded setting (the default when never set) and whether
  its data directory is the passwd home's `.rsi` (`rsid::build_budget::apply_at_start`,
  `rsid_core::build_slice::owns_user_slices`); `write_refusal` refuses every
  write otherwise. The gate sits at the one place that runs `systemctl --user`,
  `rsid_core::build_slice::user_manager_runner`, which refuses a `set-property`
  of either slice unless this process may write them (and always in a
  `test-seam` build); the start apply, the setting hook, the lander and the job
  launch (`SystemdJobRuntime::ensure_build_slice`) all take their runner from
  it. A throwaway daemon with a fresh store (a TUI end-to-end
  fixture with a `$HOME`-relative data dir) would otherwise "apply" the default
  50% over the operator's value: the 25% hub read `16s` on both slices after a
  deploy, while the hub daemon's own start and job launches had applied 25%
  throughout. Verify on a deploy: `systemctl --user show rsi.slice rsi-build.slice -p CPUQuotaPerSecUSec`
  reads `hub_build_cpu_percent` x logical CPUs / 100 seconds (25% of 32 threads
  is `8s`) right after the restart and again after the next build job; a
  daemon that is not the hub's logs `not the user's hub daemon`. To tell
  whether anything wrote the slices, compare the mtimes of
  `$XDG_RUNTIME_DIR/systemd/user.control/{rsi,rsi-build}.slice.d/50-CPU*.conf`
  before and after: `set-property --runtime` logs nothing to the journal, and
  the journal's `Reload requested` comes from `systemctl revert` or
  `daemon-reload` (the probe tests used to `revert` their throwaway slices and
  looked like slice writes; they now remove the drop-ins directly).

## Provider capacity recovery (#1930, #2109)

A provider "model at capacity" / HTTP 503 failure is transient: the daemon
recovers the worker; no manager turn is spent. The stop reason
(`provider_error:<provider>:<detail>`, `is_provider_capacity_stop_reason`) is the
signal; the failure text itself lands in `Assistant` messages marked
`provider_diagnostic`, which never count as model output. Auth, invalid-model,
quota and usage-limit stop reasons keep failing fast.

1. **Same session first.** The finalizer's transient-failure heal (#1015) takes
   reason `provider_capacity`: it resumes the SAME session on the same model
   3 times (`CAPACITY_RESUME_MAX_ATTEMPTS`) with #123's backoff (60 s, 120 s,
   240 s), at launch or hours into the work. The resume prompt starts
   `capacity: resumed`. #123's incident tables (`codex_usage_limit`) do not fit
   a per-model, per-session failure, so only its backoff table is reused.
2. **Then relaunch.** An Issue-bound worker whose resumes are spent (or never
   armed) is relaunched by the daemon (`reconcile_issue_worker_capacity_retries`)
   up to 4 times on the next allowed launch of the launching manager's policy:
   same capability class and other provider family first, then the same
   provider's other models, never a model already tried. A worker that ran
   tools is continued (`continue_from` = the failed worker: its committed HEAD
   plus its final message and the Issue's latest handoff); one that failed
   before its first tool call restarts from the original brief.
3. **The manager hears once.** While a resume or relaunch is owed the manager's
   terminal watch on the failed worker is held (`capacity_recovery_holds_watch`)
   and it is retired when the relaunch is admitted. The Issue note says
   `capacity: relaunched as <id> on <Provider/model>`; the relaunched worker's
   own watch line carries `capacity: relaunched as <id> on <model> (from <id>,
   attempt n/4)`; a worker the heal resumed carries `capacity: resumed n/3`; a
   failure whose recovery is spent fires with `capacity: recovery exhausted`.
   A manager that relaunches the Issue by hand retires the armed resume
   (`retire_superseded_capacity_resumes`), so two sessions never run one Issue.

## Host-load admission (#1417)

- **Setting.** Operator setting `host_load_admission_threshold` (0..=1024,
  default 40; 0 disables; live; TUI settings row "Host load limit for new
  launches", `GetDaemonConfig`/`UpdateDaemonConfig`). Read at every admission
  decision (`host_load.rs`).
- **Governor and QA knobs (#2123).** `governor_build_slots` (default bounds 1-4),
  `governor_lander_slots` (1-5), `rolling_queue_full_qa_interval_mins`
  (30-240) and `hub_build_cpu_percent` (#2119, 10-50) are lower-only by default
  (max is the registry default; an explicit operator bound may widen them) and manager-adjustable through `ProposeDaemonSetting` with the same
  built-in default bounds as the threshold (operator bounds replace or narrow
  them; `governor_max_load` stays operator-only), so a manager can cool the host
  without an operator round-trip.
- **Disk floor (#186).** `governor_min_free_disk_gb` (daemon range 0..=4096,
  default 30) is manager-adjustable through `ProposeDaemonSetting` with default
  bounds 30-100: raise-only, because a lower floor risks filling the disk, so a
  manager cannot go below the 30 GiB default unless the operator's explicit
  bound allows it (the bound may lower it down to 0, which disables the gate).
- **Hold, never refuse.** While `load + recent_admissions > threshold` the
  daemon holds a manager's new worker: a queued `create_session`
  (`AgentManagerControl`, `AgentManagerLaunchIssueWorker`, review allocations)
  stays `queued` because `Store::claim_manager_action_with_create_admission` leaves
  it unclaimed (the deploy-drain mechanism, #1073); a topology node of a
  topology node stays `Reserved` (`NodeEffects::launch_held`, retried on the
  executor tick) and writes nothing. The hold covers every execution's
  later node launches, manager-, Epic-lead- or operator-requested alike; only
  the first launches of an operator-requested execution (before any of its
  attempts has settled) start when asked (#1641 S4b).
- **Resource pressure holds topology nodes, never fails them (#1641 S4b).** A
  node launch refused by the sandbox capacity gate (JSON-RPC `-32029`,
  `launch.rs::admit_sandbox_allocation`: `free_space_limit` or
  `source_root_limit`) returns the attempt to `Reserved`; the executor writes
  one `admission_hold{kind: disk_floor|source_root_limit}` event per transition
  (not per tick) and the 30 s tick launches it once the pressure clears. A
  command node takes a governor build slot (`governor.rs`, gates: slots, load,
  memory, `min_free_disk_gb`) before its sandbox is allocated; while the slot
  is not granted it stays `Reserved` with `admission_hold{kind: disk_floor|build_slot}`,
  and the slot is released when the node settles. A sandbox-capacity refusal
  of a command node holds it the same way.
  Both start on their own when the load drops. `load` is the 1-minute average
  (`/proc/loadavg` on Linux; any other platform, or an unreadable file, is
  `supported: false` and admits everything).
- **Host pressure holds the same work (#2562).** One predicate
  (`host_pressure::hold_now`) answers "too hot or too full": the hottest CPU
  sensor at or above the operator's `job_thermal_ceiling_c` (#2524), or free
  disk on `/` below the governor's `min_free_disk_gb` floor. Build/test jobs
  (`agent_jobs`, `held_for_thermal`), the governor's disk gate, a manager's new
  `create_session` and a topology node's first launch all use it, so no manager
  polls `sensors` or `df`. The create or node is held, never refused, and shares
  the host-load held queue, so it starts oldest-first once the condition clears.
  `AgentManagerGetAction.held` is `{reason: thermal, temperature_c, ceiling_c}`
  or `{reason: disk_pressure, free_disk_gb, min_free_disk_gb}`; a topology
  node stays `Reserved` with one `admission_hold{kind: thermal|disk_pressure}`
  event per transition; `AgentGetDaemonInfo.host_load.held[].reason` carries the
  same names. Pressure holds an operator-requested execution's first launch too
  (only the load queue is skipped for it). The same exemptions as host load
  apply (retries, continuations, lead recovery). Tests install
  `HostLoadAdmission::set_pressure_source`; a test build never reads the host.
- **Sandbox free space (#2920).** A launch the sandbox allocator refuses for
  free space (`sandbox_capacity_refused`, `code: free_space_limit`: the sandbox
  floor `sandbox_min_free_gib`, scaled down on a small disk) latches
  `PressureHold::SandboxFreeSpace` for `SANDBOX_FREE_SPACE_RETRY` (30 s). It
  holds every new manager create and topology node launch like the other
  pressure holds (`held {reason: sandbox_free_space, free_disk_gb,
  min_free_disk_gb}` with the sandbox floor rounded up; topology also keeps its
  `admission_hold{kind: disk_floor}`). The refused create is requeued
  (`requeue_held_manager_action`), never `blocked`, and the next attempt after
  the delay runs the allocator's one reclaim page again. A refusal at
  `sandbox_max_source_roots` (`code: source_root_limit`) is not held: it ends
  `blocked` with `manager_v2_sandbox_source_root_limit` (a free-space refusal
  after the effect began ends `manager_v2_sandbox_free_space_limit`).
- **Oldest first.** The claim query orders queued creates by `(not_before, id)`
  across all projects; waiting topology nodes queue by execution age
  (`HostLoadAdmission::admit_waiter`, stale entries dropped after 90 s). The two
  lanes share one held queue: only eligible manager creates enter it, so
  younger creates cannot consume capacity ahead of older topology work (or
  vice versa). Cancelled or no-longer-eligible entries age out after 90 s.
- **No thundering herd.** The 1-minute average lags, so each launch admitted in
  the last 60 s counts one toward the comparison (`recent_admissions`).
- **Async full-library QA (#2112).** The merge queue's `rolling-library-qa`
  start reads the same hold (`hold_now`) and simply waits; it records no
  admission and joins no held queue (an eight-hour overdue Issue reports a
  long stall). Its job then waits in its own cargo-slot governor admission.
  A satellite asked to take that run (`RequestHubQaRun`, #2120) reads its own
  `hold_now` and refuses `satellite_qa_busy` while it holds; the hub then runs
  it itself (`ran_on: hub`). A pending deploy there (#2176) instead makes the
  run wait for that deploy.
- **Never held.** Operator launches, a worker's `AgentSpawnChild` children,
  retries, rotation successors, Issue worker `continue_from` launches,
  recovery of topology attempts already `Launching`, lead recovery (`replace_lead`, `retry_lead`,
  `resume_lead`), every non-create manager action, and the first launches of a
  topology execution the operator requested.
- **Visibility.** `AgentManagerGetAction` carries `held {reason: host_load, load,
  threshold, recent_admissions}` on a queued create (a deploy hold, when both
  apply, is reported first). `AgentGetDaemonInfo.host_load` reports `threshold`,
  `supported`, `load`, `recent_admissions`, `holding` and `held` (oldest first,
  at most 64: `manager_create_session` and `topology_node`, `reason: host_load`).
  `host_load.build_disk {min_free_gb, free_gb, open}` (#163) is the build
  admission floor, apart from any launch policy: the live
  `governor_min_free_disk_gb` and free GiB on `/` (where `scripts/cargo-slot`
  measures). A manager reads it before promising verification: below it a build
  or test unit still launches, but waits in the governor and spends its ten
  minute admission (a scoped test's wall budget), so a lower launch override
  does not make a build runnable. Same-owner jobs on one sandbox target already
  queue before launch (above), nested worktrees included.

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

## Agent-created projects (#1626)

- **Agent verbs.** `AgentCreateProject {name, path, description?, color?}` and
  `AgentUpdateProject {project_id, name?, path?, description?, color?}`
  (native `rsi_control_create_project`, `rsi_control_update_project`). Types
  and refusal codes: `rsi_common::agent_projects`; authority and writes:
  `Store::agent_project_scope` / `agent_register_project` /
  `agent_edit_project` (`rsid-store/src/store/agent_projects.rs`); path
  containment and cache refresh: `session/agent_projects.rs`.
- **Authority.** A project manager in Execute mode, not paused, acts on its
  own project; a portfolio seat in Execute mode, not paused, acts on its
  grant's coverage. A session holding both seats evaluates each seat
  independently: any permitting seat suffices and coverage is the union. The catalog (`VerbRights::project_admin`) and the store
  use the same predicate. Caller identity comes from the token. An area seat
  or a worker is refused `project_not_authorized`.
- **Create.** The path must be absolute, exist, be a directory and sit inside
  a configured workspace root (`project_path_invalid`); with no workspace roots
  configured the path must be a strict descendant of the daemon user's home
  directory, never the home directory, anything under `~/.rsi`, or `/`. A path
  equal to, containing or inside the harness root, or any path change on the
  harness project, is `project_harness_protected` (operator-only); authority is checked
  before the filesystem is touched. Names are unique (`project_name_taken`),
  a directory registers once (`project_path_taken`); the same name and
  directory again returns the project with `deduplicated:true` only inside the
  caller's coverage (otherwise `project_name_taken`, no project fields). The created
  project has no manager and is not added to the creator's grant. The daemon
  rebuilds its launch-time project index and the project's RSI.md cache, as
  the operator's `CreateProject` does. A handle built without a manager (a
  rotation-built Harness handle) refuses `project_admin_unavailable` rather
  than leave those caches stale.
- **Update.** Limited to the caller's coverage; an uncovered or unknown id is
  `project_not_in_scope`. A path change is refused while the project has a
  Starting, Running or WaitingApproval session (`project_has_live_sessions`).
  An omitted field is unchanged (a description cannot be cleared by an agent).
- **Delete and archive are not agent verbs.** `projects` has no archive state
  (adding one is a migration, not part of this slice) and a hard delete is the
  operator's `DeleteProject`. Revisit archive with the migration if the
  operator asks.
- **`project_created` event (slice 2).** A new agent create (not a replay, not
  an operator `CreateProject`, which publishes nothing) publishes
  `DaemonEvent::ProjectCreated {project_id, name, source:"agent",
  created_by_session_id}` as bus event `project_created`; it is daemon-global.
  The payload type is `rsi_common::agent_projects::ProjectCreatedEventV1`.
- **Operator surfaces.** The operator already has `CreateProject`,
  `UpdateProject` and `DeleteProject`. The TUI handles `project_created`
  (`app/agent_projects.rs`): it records the event, refreshes the project list,
  switches to the new project's tab when the persisted daemon setting
  `follow_agent_created_projects` is on (default on; Settings ▸ Orchestration,
  `UpdateDaemonConfig`), and, when the creator is the active global seat and the
  project is not in its grant, offers the grant addition as a `y` confirm in
  `:manager tree` (or `:manager global add-project <name>` when an overlay is
  open). The offer is an operator `ConfigureGlobalManager` call fenced on the
  grant version; the agent never makes it.

## Operator to-do list (#1747, V163)

- **What.** One durable list of the things only the operator can do (a login,
  a key to press, a decision), written by managers and by the operator, shown
  wherever the operator is (the TUI panel and status badge are slice 2). Table
  `operator_todos` (+ `operator_todo_receipts` for idempotency) in
  `rsid-store/src/store/migrations/v163.rs`; types and refusal codes in
  `rsi_common::operator_todos`; storage and rules in
  `Store::operator_todo_*` (`rsid-store/src/store/operator_todos.rs`); agent
  handlers in `session/operator_todos.rs`; RPC glue in `rpc/operator_todos.rs`.
- **Item.** `id`, `scope` (`operator`, `portfolio:<id>`, `project:<id>`,
  `area:<id>`), optional `project_id`, `todo_key` (a stable per-scope slug,
  unique with `scope`), `title`, `how`, optional `link`, `priority` 1..4,
  `added_by` (`operator` or `session:<uuid>`), `status` (`open`, `done`,
  `dismissed`), `evidence`, `evidence_observed`, `closed_by`, and RFC3339-nanos
  `created_at`, `updated_at`, `closed_at`.
- **Agent verbs.** `AgentOperatorTodoUpsert` and `AgentOperatorTodoClose`
  (native `rsi_control_operator_todo_upsert` / `_close`) are open to any
  manager node seat (`VerbRights::tier_seat`, the `AgentReportUp` predicate).
  The scope comes from the seat (`Store::tier_caller_node`); the requests carry
  no scope, project or author and reject unknown fields. A project or area
  seat touches only its own scope; a portfolio seat may close any item by id.
  A session without a seat is `operator_todo_not_authorized`.
- **Evidence rule.** An agent close always records `evidence`; `done` also
  needs `observed:true`. The table CHECKs repeat the rule, so no writer can
  store an agent-closed row without evidence or an unobserved `done`. Closed
  rows are terminal (trigger): nothing reopens and the identity columns never
  change.
- **Idempotency.** `(scope, idempotency_key)` identifies a call; the receipt
  holds a request fingerprint, so a replay returns the same item
  (`deduplicated:true`) and a changed request under the key is
  `operator_todo_idempotency_conflict`.
- **Operator surface (rule 10).** `OperatorTodoList {closed_since?}` (open
  items most urgent then newest first, plus items closed at or after
  `closed_since`: the catch-up view), `OperatorTodoAdd {title, how, link?,
  priority?, project_id?, idempotency_key}`, `OperatorTodoClose {todo_id,
  status, note?}` and `OperatorTodoReopen {todo_id}`. They are not in
  `AGENT_VERBS`, `READ_VERBS`, native tools or the agent CLI catalog
  (`rsi_common::operator_todos::OPERATOR_TODO_OPERATOR_METHODS`).
- **Undo (#2950).** `OperatorTodoReopen` undoes the operator's own close within
  `OPERATOR_TODO_UNDO_WINDOW_SECS` (60 s, plus `OPERATOR_TODO_REOPEN_GRACE_SECS`
  for transport). A closed row is immutable (the `operator_todos_closed_terminal`
  trigger), so no migration was added: the reopen inserts a fresh open row in the
  same scope with the closed row's title, how, link, priority, project, author
  and creation time and a new key (`<key>~r<8 hex of the new id>`); the closed row
  is untouched and a manager that waited on its close was already woken. The
  history record is the `operator_todo_receipts` row `(operator,
  reopen:<closed id>) -> new id`, so a repeat answers with the same copy
  (`deduplicated:true`). Refusals: `operator_todo_not_found`,
  `operator_todo_not_closed`, `operator_todo_reopen_not_operator_closed` (a
  manager's close stays final), `operator_todo_reopen_window_elapsed`,
  `operator_todo_limit`.

### Standing operator directives (#2649)

An operator instruction that must outlive a manager seat ("keep the hub busy
overnight", "send ONE morning summary") is daemon state on the manager scope
(`project:<id>`, `area:<id>` or `portfolio:<id>`), not handoff prose and not
session state. Storage `standing_directives` (V178; rows are never deleted:
`cleared_at` and `expires_at` end delivery and the row stays as audit); store
`store/standing_directives.rs`; wire types `rsi_common::standing_directives`.

- **Delivery.** An active directive (not cleared, not past `expires_at`) is
  delivered verbatim, with its id, author, provenance and timestamps, to the
  seat that holds the scope (`Store::standing_directives_for_seat`, keyed on the
  seat's node, never on the session): in the `AgentGetAuthorityCatalog`
  guidance (`session/authority_catalog_verb.rs`), in the first prompt of a
  context-cap successor (`context_cap::successor_prompt`, read from state at
  launch, outside the handoff document), and in `AgentManagerOverview`
  (`standing_directives` of the node) and `AgentGlobalOverview` /
  `GetManagerNodeWorkspace` (`standing_directives` on each project row). A
  scope's directives are not inherited by the scopes below it.
- **Write authority.** The operator writes any scope: `StandingDirectiveSet
  {scope, text, expires_in_secs?, idempotency_key}`, `StandingDirectiveClear
  {directive_id}`, `StandingDirectiveList {scope?, include_inactive?}`
  (operator-only: not in `AGENT_VERBS`, `READ_VERBS`, native tools or the CLI
  catalog, `STANDING_DIRECTIVE_OPERATOR_METHODS`). A manager seat relays an
  operator instruction with `AgentStandingDirective {action:"set", scope,
  text, provenance, expires_in_secs?, idempotency_key}` (native
  `rsi_control_standing_directive`, `VerbRights::tier_seat`) only to a scope
  strictly below its own (`Store::tier_ancestors` contains the seat's node;
  never its own scope) and must name provenance; `action:"clear",
  directive_id` clears a directive a manager relayed, never one the operator
  wrote. The author (`operator` or `session:<uuid>`) is derived from the seat,
  never read from the request (`deny_unknown_fields`).
- **Bounds.** Text at most 2048 bytes, provenance 512, `expires_in_secs`
  1..=7,776,000 (none: until cleared), at most 8 active per scope
  (`standing_directive_limit`). `(author, idempotency_key)` identifies a set; a
  replay returns the first row (`deduplicated:true`), a changed request under
  the key is `standing_directive_idempotency_conflict`.
- **Refusals.** `standing_directive_not_authorized`,
  `standing_directive_invalid_request`, `standing_directive_not_found` (also
  for a directive of a scope the caller does not sit above),
  `standing_directive_limit`, `standing_directive_idempotency_conflict`,
  `manager_tier_target_unknown`.

Manager release builds (#1218): a `build` job with `release=true` owned by a
manager builds in one persistent daemon-owned target, `~/.rsi/staging/release-target`
(`rsid::release_target::RELEASE_TARGET_RELATIVE`; staging is a deploy source root,
so `release-target/release/<bin>` deploys as built), so each deploy recompiles
only what changed. Worker release builds, dev builds and tests keep their
per-sandbox targets. Manager integration jobs in another worktree use #1864's
registered per-worktree scratch instead. The target's `.rsi-job-admission.lock` serializes
builds; `release_target::is_busy` is the "a build holds it" probe for the
stray-target sweep, which must keep a busy target and empty it with
`release_target::reclaim_contents` (lock files survive). Before each such build
the daemon empties the target if it exceeds `RELEASE_TARGET_MAX_BYTES` (16 GiB; a warm target is about 2 GiB).

Build source custody (hub #2954, `agent_jobs/source.rs`): a `build` job compiles
exactly one commit. At submit the daemon records the job worktree's HEAD and
whether it has uncommitted changes (the bounded Git runner of sandbox custody,
`observe_clean_head_bounded`; a sidecar `<jobs>/<id>.source` beside the log, no
schema change; a job directory that is not a worktree root is not enforced).
Optional `params.sha` (40 lowercase hex) pins the expected HEAD and is refused up
front as `job_source_sha_mismatch` (nothing recorded or launched) when the
worktree is on another commit. A record that cannot be written (jobs directory
missing, read-only or full) refuses the build `job_source_record_failed` before
the job row exists or a unit launches (hub #3025): a build is never accepted and
run unenforced, and a build with no submit-time record is never stamped with a
finish-time `source_head`. The daemon compares the worktree with the record
when a held job is about to start, on every jobs-loop poll while the unit runs
(slot wait, slot grant and the compile alike) and at finish. A moved HEAD, or a
tree that was clean and is not, stops the unit and settles the job `failed` with
refusal `job_source_moved` and a detail naming both SHAs (at finish the build's own
exit code and log tail stay on the result); a tree that was already dirty at
submit is built as is and only a HEAD move fails it. `result.source_head` and
`result.source_dirty` carry what the daemon observed when the job ended, so a deploy
can compare `source_head` with the SHA it is asked to install. A manager building
for a deploy passes `params.sha` and leaves the integration worktree alone until
the job is terminal.

## Cold-storage archive (#2384, V176, slices S1-S5)

Plan: `thoughts/shared/plans/2026-10-14-cold-storage-archive.md`. The daemon copies
the bulky, non-`Message` rows of old `Archived`/`Deleted` sessions
(`conversation_events` and their `conversation_event_provenance`) into append-only
`SQLite` archive files of per-session zstd chunks, reads them back to verify, and (slice
S4, `move` mode) then deletes the verified rows from the live database. S1 stages and
verifies; S2 adds cold reads and restore; S3 adds the remaining read surfaces and the
operator's TUI; S4 adds the move, reclaim, compaction and notices (below). Slice S5 flips the
default: `cold_archive_enabled` is **on** for a daemon that never had the setting written, and
an operator's explicit off is a persisted `false` that survives a restart (below, "Default on"
and the acceptance protocol).

- **The move (S4, P6, `store/cold_archive/mover.rs`).** `Store::cold_move_batch_admitted`
  (`cold_move_batch` is its no-check form for the test seam) is the
  only production code that deletes archived rows. A `move` pass (`run_move_pass`) hands
  P6 only sessions whose chunks it read back and checked against their digests **in the
  same pass** (`VerifiedChunks`; a session an earlier pass staged is read back again
  first, so a pass resumed after a crash re-verifies before it deletes). The read-back
  also compares each chunk's recorded `columns_json` with the live tables' ordered
  `(name, type)` lists (`cold_archive_verify_columns`): the digest covers only the encoded
  values, so a renamed or reordered column list would otherwise pass it and read back into
  other fields. A session that fails stays live with the typed error on its row, and the
  next pass re-stages it under a new generation. Before **every** delete
  transaction (each batch, each isolated retry, the trailing batch) the pass admits it
  twice. Outside the locks: the host-load gate must be open (waiting for it never holds
  the store) and the wall budget and cancel flag clear (`wait_for_gate`; a proof that is
  not admitted stays `staged`, counted in `moved.sessions_deferred`), and it requires a
  **fresh backup** (`store/cold_archive/backup.rs`, `admit_backup`): a readable
  `SQLite` file in `<data_dir>/backups/` younger than 24 h minus a 5-minute reserve with
  the live `application_id` and a `user_version` no newer than the live one (a pre-migration
  backup qualifies). The proof is the last backup's file opened again, never a flag
  carried from the first batch, so one that aged out, was rotated, replaced or removed
  during a long pass is replaced. Then the pass waits for the store mutex and for
  `SQLite`'s write lock, which can take long, so the same questions are asked **again at
  the transaction boundary**: `Store::cold_move_batch_admitted` runs the pass's check once
  its `IMMEDIATE` transaction holds both locks and before its first read or delete, and
  that check is current and nonblocking: the backup file is opened again first (still
  there, still a readable backup of this database, the reserve left:
  `FreshBackup::requalify`, with a zero busy timeout, so a backup another connection holds
  a lock on is not admitted instead of waited for with the store held), and the cancel flag,
  the wall budget and the host-load gate are read last, after it, so nothing that changed
  while the backup was read can start a delete. A refusal
  rolls the transaction back, releases the locks and sends the attempt round again
  outside them (a closed gate is waited out there, a backup is found or taken there,
  at most three rounds), so nothing is deleted on a stale answer and `VACUUM INTO` never
  runs under the store lock. Otherwise the pass takes its own `VACUUM INTO
  backups/rsi-cold-pass-<ts>.db` on its read connection (written under a `.tmp` name,
  fsynced, renamed; only the newest pass-owned copy is kept, an older one is removed
  after the new one opened read-only), and if none qualifies after that it fails
  `cold_archive_backup_stale` and deletes nothing more: a pass that had not moved
  anything returns the error, one that had keeps its committed batches (run `partial`
  with `error_code = cold_archive_backup_stale`, `StageReport.move_refused`). A session
  re-read for a move counts in the batch throttle like freshly staged work, so the batch
  sleep and the gate apply to a pass that only re-verifies. Batches are `<= 32` sessions or
  `<= 64 MiB`, one `IMMEDIATE` transaction each; each session's spawn guard is
  try-acquired before the store mutex and never awaited (`guard_busy` skips it).
  Inside the transaction each session must still be `staged` and verified with the
  proof's chunk ids and digests, `Archived`/`Deleted`, free of every hold, provably
  older than the cutoff, and its live movable rows must hash to the archived digest
  (else `changed`, and it must be verified again); then `deleting`, set-based deletes of
  the provenance and the movable rows, both counts checked, `cold`. An error rolls the
  batch back and the pass retries it one session at a time (the culprit records
  `last_error_code`). The test seam `cold_move_session_for_test` now delegates to it.
- **Reclaim (S4, `store/cold_archive/reclaim.rs`).** After a move pass,
  `PRAGMA incremental_vacuum(4096)` steps, one store hold each, until the freelist is
  `<= 1%` or 30 s pass (`pages_reclaimed` on the run row); a file not in incremental mode
  needs the one-time compaction first. A **new** database is created incremental
  (`Store::open`). The compaction (`auto_vacuum=INCREMENTAL; VACUUM;
  wal_checkpoint(TRUNCATE)`) never runs in the live daemon (it would need SQLite's
  process-global temp directory, review #2398): the deploy loop decides it at a supervised
  restart's quiet point (`cold_compact::decide_at_quiet_point`, before
  `mark_agent_deploy_restarting`; an operator's request, or a due one while the archive is
  on), and after the graceful shutdown `run_to_bounded_exit` spawns the executable captured
  at startup as `rsid cold-archive-compact --db --tmp-dir --result --run-id` (a hidden mode
  handled before `prepare_daemon`) with `SQLITE_TMPDIR`/`TMPDIR`/`TMP`/`TEMP` set on that
  `Command` only, waits at most 180 s, kills and reaps it past that, and exits 75 whatever
  happened. The helper opens a raw connection (no migration), refuses a newer schema and a
  disk with less than 2.5x the used bytes free, and writes its result file. The next
  daemon's cold-archive loop (after `request_ready`) settles the `mode='compact'` run row
  from it (`compact_interrupted` without one). A pass's stale-run sweep leaves compaction
  rows alone. **The helper starts only on a quiet database** (#205,
  `cold_archive_lifecycle.rs`): a pass runs on its own OS thread that the runtime teardown
  does not stop. The quiet-point decision (and the restart firing) closes cold-archive
  admission (`start_cold_archive_run` refuses `cold_archive_shutting_down`) and raises the
  cancel flag every pass reads at its safe boundaries (between sessions, between move
  batches, inside the gate wait, between reclaim steps); the pass returns `partial`
  (`cancelled`, no error, no notice) with its SQLite handles closed, and
  `cold_compact::quiesce_then_run` waits at most 20 s for the pass thread's ticket before it
  spawns the helper. If the thread outlives the wait the helper is **not** started, a result
  file `outcome: "deferred"` is written, the exit is still 75, and the next daemon's
  reconcile puts the request back (an operator's request waits for the next restart; no
  failure notice). An abandoned restart reopens admission.
- **Notices (S4, `store/cold_archive/notices.rs`).** A failed or partial run, a failed
  verification, an archive unavailable on read (queued by the reader, raised by the
  loop), a failed or interrupted compaction, a compaction due for more than 24 h,
  `cold_archive_backup_stale` and `cold_restore_conflict` each log a WARN line, record the
  friction event `cold_archive:<code>`, appear in the status `errors`, and open one
  operator to-do keyed `system:cold-archive:<code>:<stamp>` (one open item per code). A
  completed pass closes the pass notices, a completed compaction the compaction notices;
  read and restore notices stay for the operator.
- **Schedule (S4).** The scheduled pass (at most every 6 h while the setting is on) is a
  `move`; a `verify` pass runs when some archived chunk was last read back more than a
  week ago.

- **Default on (S5).** `RuntimeConfig` starts `cold_archive_enabled` at
  `COLD_ARCHIVE_ENABLED_DEFAULT` (`true`, `store/cold_archive/mod.rs`); the persisted-settings
  overlay (`apply_persisted_runtime_config`) leaves it alone when no row exists and applies a
  stored `false` on top, so a store that never saw the switch archives and one whose operator
  turned it off stays off across restarts. Nothing else changes: the setting is still read on
  every loop tick, the loop still starts only after `request_ready` and waits 15 minutes, the
  first pass is the scheduled `move` (so rows are deleted only for chunks read back in the same
  pass, only with a qualifying backup re-derived before every delete transaction, and only when
  the host-load gate, wall budget and cancel flag admit each batch), `auto_compaction` follows
  the setting at the next supervised restart, and a due compaction is claimed there. No new
  setting, RPC, TUI action or migration. The hub's acceptance measurement (size, per-table
  conservation, archive verification, restore, pre-migration backup time, startup, first
  automatic pass) is protocol only until the hub fills it:
  `thoughts/shared/verification/2026-10-14-cold-archive-s5-acceptance.md`; every measured
  value there is `PENDING`.
- **Cold reads (S2).** A session whose index row is `cold` keeps a live tail; every
  transcript reader returns the transcript it returned before the move. The merge is
  one store function, `Store::merge_cold_events` (`store/cold_archive/reader.rs`),
  called by `Store::load_events_since` (so `GetConversation*`, hydration, memory sync,
  the dreamer, graph executions and the launch paths all see the whole transcript),
  `read_session_events_page` (`AgentReadSessionEvents`, whose result gains
  `storage: live|cold`) and the program-gate window (`check_manager_program_gate`,
  through `cold_assistant_window`: its assistant-role window includes `ToolUse` rows).
  The count and content length of `event_watermark`, the lead-retirement witness writer
  and `manager_lead_program_outcome_superseded` are *logical aggregates* (live rows plus
  the rows archived for a `cold` session, `LOGICAL_EVENT_COUNT_SQL`), so a move or a
  restore does not change them (#2396). A decoded transcript is cached by chunk digest
  (64 MiB). **A missing, unreadable or checksum-failing chunk is the typed error
  `cold_archive_unavailable{file, reason, detail}` (a `StructuredRpc`, `-32603`), never
  the tail alone.** `store/cold_archive/read_classes.rs` classifies every production
  reader of `conversation_events` (merge, logical aggregate, tail-exact, status-disjoint,
  hold-disjoint, writer, cross-session); `cold_archive_reader_classification` fails on
  an unclassified site or a rule whose SQL changed, and `cold_archive_reader_invariance`
  runs one probe per entry before the move, after it and after the restore. A new reader
  of `conversation_events` is added to `COLD_READER_CLASSES` with its proof.
- **Remote V1 and the portable bundle (S3).** Remote V1 (`rsid-remote-read`) serves only
  sessions that are not `Archived` or `Deleted` (`require_session_project`; the phone never
  lists or opens an archived session), and only an `Archived`/`Deleted` session is ever cold
  (the candidate query and Guard 3). So `history_page`, `observed_history_page` and
  `session_detail_source` are *status-disjoint*: a cold session is refused `NotFound` before
  any event is read, exactly as the same session was before it moved, and they carry no
  archive code (no decompression inside the 50 ms Remote store budget).
  `rpc::tests::cold_s3` moves a session for real and compares what every reader returns
  before and after, with an unmoved `Running` session as the positive control.
  `tool_pair_ambiguous` stays *cross-session*: only same-session rows decide ambiguity, the
  session is visible to Remote so none of its rows is cold, and another session's moved rows
  only stop filling the 65-row cap (a capped `SourceUnavailable` becomes the exact answer; the
  same test pins it). If Remote V1 ever serves an archived session, its readers must gain the
  merge first (the classification entries say why they do not need it today). The portable
  bundle's manifest count for `conversation_events` is a *logical aggregate* (live rows plus
  `LOGICAL_ARCHIVED_TOTAL_COUNT_SQL`, probe `bundle_event_count`).
- **Restore (S2).** `RestoreColdSession {session_id}` (operator-only; works whether or
  not the archive is enabled) returns `{outcome: restored|already_hot, generation, events,
  provenance}`; both outcomes are success. `GetColdSessionStorage {session_id}` returns
  `{tier: live|cold|restored, generation, archived_at, restored_at, file, archived_events,
  raw_bytes, compressed_bytes}`. One store protocol serves the RPC and the internal
  `SessionManager::ensure_session_hot(&SpawnGuard, id)`: plan (`cold_restore_plan`, under
  the store), decode (`decode_cold_session`, no store: outside the store mutex), commit
  (`cold_restore_commit`, one `IMMEDIATE` transaction). The commit fences with a
  compare-and-set on `(state='cold', generation, file, chunk ids, digests)`: a fence that
  matches nothing is `AlreadyHot` (another caller restored it: nothing is inserted) or
  `Stale` (re-plan, three attempts, then `cold_restore_contended`). The inserts are plain
  `INSERT`s with the original ids, never `OR IGNORE`; a primary-key collision rolls back,
  the row stays `cold` and the error is `cold_restore_conflict`. The spawn guard is the
  caller's proof and is never acquired inside (it is not re-entrant): `RestoreColdSession`
  takes it, `unarchive_session` and `undelete_session` take it, restore, flip the status
  and release it before hydration, and `continue_session` restores right after it
  acquires its own guard (before the wake-owner check, the fence check and hydration).
  Rotation guards need no restore: a running session, a fresh child and the predecessor of
  an open rotation intent (hold H19) are never cold. The V176 guards turn a missed path
  into a typed refusal: Guard 1 refuses an insert into a cold session, Guard 3 refuses the
  status flip, `purge_session` refuses with `session_cold_archived`. The move
  try-acquires the same guard, so a move and a restore of one session never interleave.

- **TUI (S3, `crates/rsi/src/cold_storage.rs`).** A session detail view asks
  `GetColdSessionStorage` once when it becomes active (only for an `Archived`/`Deleted`
  session, off the render path); a `cold` tier draws the `[cold storage]` badge in the
  conversation header, with `:cold-restore puts the rows back`. `:cold-restore` calls
  `RestoreColdSession` for the focused session (both outcomes are success and name what
  happened) and re-reads the tier. `:cold-archive [status|run|move|verify|compact]` reads
  `GetColdArchiveStatus` or starts a pass with `RunColdArchiveNow` (`run` is `stage`;
  `compact` reports that it is scheduled for the next supervised restart).
  Settings > Cold Storage holds the three switches and three rows backed by the status read:
  the status line (live database, archive files, staged/verified/cold sessions, running), the
  last pass (mode, outcome, sessions staged, bytes, any problem) and **Run cold-storage pass**
  (Enter = `stage`). The tab refreshes when it opens and on `R`. The receiving shapes are
  `rsi_common::cold_storage`; `rpc::tests::cold_s3::the_tui_view_types_read_the_daemons_cold_replies`
  parses the daemon's real replies into them.
- **Settings** (live; `GetDaemonConfig`/`UpdateDaemonConfig`; TUI Settings >
  Cold Storage): `cold_archive_enabled` (default **on** from slice S5; turn it off to stop all
  archive passes, archived sessions stay readable and restorable),
  `cold_archive_after_days` (3..=3650, default 14), `cold_archive_dir` (absolute;
  `""` = `<database directory>/archive`; TUI: the Settings row opens a text editor
  overlay, `overlay/daemon_text_editor.rs`, #2438, which shows the daemon's refusal and keeps
  the draft). **Containment (#2435):** the directory must not resolve into a sandbox
  root (the daemon's `RSI_SANDBOX_BASE` and `~/.rsi/sandboxes`) or a `.git` directory.
  `store/cold_archive/containment.rs` canonicalizes the longest existing prefix (symlinks
  followed, `..` applied after them), refuses `.`/`..` segments in the setting, a dangling
  symlink and a relative path, and compares the result with each forbidden root (itself
  resolved). The check runs when the setting is written and again at the start of every pass
  (`cold_archive_dir_forbidden`, before any file is created; the pass then uses the resolved
  path); the writer also refuses an archive file that is a symlink, and the verify pass
  refuses a recorded file whose directory now resolves into a forbidden root. A directory
  change affects new archive files only; the index records each file's absolute path.
- **RPCs** (operator-only by default-deny; absent from `AGENT_VERBS`, `READ_VERBS`,
  native tools and the agent CLI catalog): `GetColdArchiveStatus {}` returns
  `settings`, `live_db` (file/WAL bytes, page and freelist counts, `auto_vacuum`),
  `archives` (path, month, bytes, chunk count, last verification), `totals`
  (staged/verified/cold sessions, archived events, raw vs compressed bytes), `last_run`
  (holds by reason, sessions/events/provenance moved, pages reclaimed), `running`,
  `compaction` (`available`, `due`, `pending`, `last_at`, `last_outcome`, `last_ms`,
  `bytes_before`, `bytes_after`) and `errors`.
  `RunColdArchiveNow {mode?}` (`stage` default, `move` or `verify`) opens a run row and
  returns `{run_id, mode}`; `compact` records a request and returns `{run_id, mode,
  scheduled: true}` (idempotent while one waits). Refusals `cold_archive_disabled`,
  `cold_archive_running`, `cold_archive_no_database`.
- **The loop** (`session/cold_archive.rs`) is spawned after `request_ready`
  (`main.rs`; a source test pins the order), waits 15 minutes, then looks at the
  setting every 10 minutes and starts a pass at most once per 6 hours: the last stage start is
  read from `cold_archive_runs` (`Store::cold_last_stage_started_at`, any trigger, a failed run
  counts), so a daemon restart does not reset the interval (#2436). A pass runs on its own
  thread at nice 19 / idle I/O priority (Linux), one at a time, holds a batch while
  `host_load.hold_now()` is set, and stops after 10 wall-clock minutes (the next pass
  resumes). The verify pass obeys the same gate and batch throttle as the stage pass (it asks
  the gate before it starts and after every full batch, one `BatchThrottle`, #2437). It reads the live database through a separate read-only connection and
  takes the store mutex only for the short index writes.
- **Cold set** (`store/cold_archive/candidates.rs`; plan §2.2-§2.4 as revised by review #2392):
  `Archived`/`Deleted` sessions whose newest *event* is at least `cold_archive_after_days`
  old (not `updated_at`, which sandbox maintenance bumps in bulk). The newest-by-sequence row
  is a cheap prefilter; the age proof then reads every `created_at` of the session, requires
  each to parse as strict RFC 3339 and the newest to be at or before the cutoff, else the
  session is skipped as `age_unproven`. Holds keep a session live and are counted under
  their reason: wake, wake lineage (H1b, a descendant through `continued_from` of an enabled
  job's target; past depth 64 it holds), agent job, queue entry, topology, manager seat,
  succession, review, watch, manager operation, agent mail, spawn/restart/watch/relaunch,
  custody/authority, settlement/closure, rotation successor (H14), tracker dispatch, container
  lead (H18, any session naming it as `lead_session_id`), open rotation intent (H19),
  pending question, usage-limit window (H20, the Codex hold's 8-day look-back), recently
  restored. A state-bearing hold is a *settled-state exclusion*: a referencing row holds the
  session unless its state is in the hold's settled list, bound as JSON from
  `HOLD_STATE_CLASSES` (`json_each(?N)`), so an unknown or new state holds.
  `check_hold_classification` proves every classified column is `NOT NULL` and that live plus
  settled is exactly the admitted `CHECK ... IN (...)` set (`sessions.status`, which has no
  `CHECK`, by the `SessionStatus` names plus every status stored in `sessions`: the enum is
  `#[non_exhaustive]`, so no `match` can force a new variant); a pass runs it first and refuses with `cold_archive_hold_schema_unclassified`.
  Only `ToolUse`/`ToolResult` rows move. The live tail keeps every other row type, the newest
  row (T2), the newest provider row (T3), the row that attains the TEXT `MAX(created_at)` (T5)
  and the one over non-`System` rows (T6) (ties to the newest `(sequence, id)`), and every row
  another table points at (T4, `EVENT_FK_REFERENCES`, compared with `pragma_foreign_key_list`
  by a test).
- **Protocol** (per session): build (read + encode + sha256) -> P2 stage (index row
  `staged`, generation, counts, digests) -> P4 write chunks + snapshot in one archive
  transaction (`synchronous=FULL`, directory fsync for a new file) -> P5 read back
  through a separate read-only connection (identity, sha256, row count, content
  length, id/sequence range) -> record. A chunk is never updated or deleted
  (triggers); a changed session writes a new `generation`. A crash at any point is
  picked up by the next pass (`failpoint.rs` injects one per step in tests); a second
  pass over unchanged data writes no chunk.
- **Schema** (V176): `cold_archive_files`, `cold_archive_sessions` (the S1 column
  `verified_at` marks a read-back-verified chunk), `cold_archive_runs`; Guard 1
  (`conversation_events_cold_insert_guard`: an insert into a `cold`/`deleting`
  session aborts `cold_session_restore_required`) and Guard 2 (provenance
  `no_delete` relaxes only while the owning session's index row is `deleting`, inside
  the archiver's transaction) and Guard 3 (`sessions_cold_status_guard`: a `cold`/`deleting`
  session cannot change status out of `Archived`/`Deleted` until it is restored; aborts
  `cold_session_restore_required`).
- **Failure codes** (`cold_archive_*`, recorded on the session row's
  `last_error_code` and the run row): `sqlite`, `io`, `dir_unusable`, `bad_format`,
  `schema_drift`, `invalid_utf8`, `state_conflict`, `verify_sha_mismatch`,
  `verify_count`, `verify_missing`, `verify_decompress`, `file_missing`, `chunk_missing`,
  `verify_identity`. Restore: `cold_restore_conflict`, `cold_restore_contended`,
  `cold_restore_count_mismatch`, `cold_restore_failed`, `cold_restore_guard_mismatch`.
  Notices for these (friction signature, operator to-do) arrive with slice S4; S2 logs a
  warning and returns the typed error.

## Project files (hub #3012, slice 10A of #3000)

Four read-only, operator-only methods serve the desktop file explorer, fuzzy
picker and viewer from the local socket (`rpc/project_files.rs`, logic in
`project_files.rs`, wire types `rsi_common::project_files`):
`ListProjectDirectory`, `ListProjectFiles`, `ReadProjectFile`,
`GetProjectFileGitGutter`. They are in no attributed audience (a tokened caller
is refused before dispatch), and the rsi-remote gateway never calls them (it
speaks only the `Remote*V1/V2` methods); a gateway exposure is a reviewed
network change.

- Root: exactly one of `project_id` (the project's `path`) or `session_id`
  (the session's `working_dir`), canonicalized. An unknown id, a pathless
  project or a missing directory is `project_file_unknown_root`; both or
  neither is `project_file_invalid_request`.
- Path scope: `path` is relative to the root. A `..` component, an absolute
  path (Unix root, Windows drive/UNC prefix) or a canonical target outside the
  root (a symlink escape) is `project_file_outside_root`; a missing path is
  `project_file_not_found`. All refusals are `INVALID_PARAMS` with the code as
  the message prefix.
- Bounds: directory entries default 2000, max 10000 (scan stops at 50000);
  picker paths default 20000, max 100000; read bytes default 1 MiB, max 4 MiB;
  gutter files up to 4 MiB (`too_large` beyond). Each answer carries
  `truncated` where a cap applies.
- Git is read-only: `GIT_OPTIONAL_LOCKS=0`, `core.fsmonitor=false`,
  `--no-ext-diff --no-textconv`, a 10 s deadline and a 64 MiB output cap. A
  root inside a directory its enclosing repository ignores is not a work tree
  (picker falls back to a `.gitignore`-aware walk; gutter says `not_a_repo`).

### Project-file writes (hub #3103, #3143-#3151)

Six more operator-only methods on the same root resolution change files for the
desktop explorer and editor (`project_files/write.rs`, primitives in
`write/fsx.rs`, operations in `write/ops.rs`): `WriteProjectFile`,
`CreateProjectEntry`, `MoveProjectEntry`, `CopyProjectEntry`,
`TrashProjectEntry`, `UndoProjectFileOperation`. They are in
`PROJECT_FILE_WRITE_METHODS` and on no exposure surface: not `AGENT_VERBS`,
`READ_VERBS`, `HOOK_VERBS` or any attributed audience, no native tool mapping
(by RPC method), not the CLI catalog, not manager-delegable, and not a daemon
call of the rsi-remote gateway (`rsi_remote::reads::GATEWAY_DAEMON_METHODS`).
`project_file_write_rpcs_are_operator_only_on_every_surface` enumerates the
surfaces and `project_file_write_fence_catches_a_write_method_exposed_on_any_surface`
proves each fence fails on an exposed method. Off unix (native Windows) every
write answers `project_file_unsupported_platform`.

- Handles (hub #3143): the root is opened as a directory handle and a path is
  resolved beneath it with `cap-std` (an in-root symlinked directory is
  followed; an escape, an absolute symlink included, is
  `project_file_outside_root`). Every mutation is one call on a parent handle
  and one plain name, never following it, so a parent swapped for a symlink
  after the check leaves the operation on the object it checked. `..`,
  absolute and empty (the root) paths are refused lexically first.
- Version: `ReadProjectFile` also answers `version`
  (`v1:<size>:<mtime unix nanos>:<sha256>`), absent above 64 MiB or when the
  file changed while it was read (the token always describes the bytes
  answered). Reload is `ReadProjectFile` again.
- Save (hub #3146): `WriteProjectFile {path, content (<= 8 MiB),
  expected_version?, force?}`. A FIFO, socket or device is
  `project_file_not_a_file` from `lstat`, before any open (hub #3150); every
  open is `O_NONBLOCK`. A version other than the current one is
  `project_file_stale` unless `force`; a missing file is `project_file_stale`
  unless `force` (then created, its directory must exist). The temp file
  beside the target is fsynced, and immediately before it is renamed over the
  target the target must still be the checked object, untouched (inode, size,
  mtime, ctime), else `project_file_stale` (`force` replaces a changed file).
  A created file is published without replacing (a file that appeared is
  `project_file_stale`). The answered `version` is computed from the
  published inode's bytes and metadata, never a reopen. Residual window: an
  in-place write by another process between that re-check and the rename (two
  system calls) is replaced. A save follows the file's own symlink only to a
  regular file inside the root; that resolved target is trash-protected (hub
  #3149). Answers `{root, path, size, version, forced, created}`.
- Explorer (hub #3144, #3145): create (`kind` `file`|`dir`, missing parents
  created one handle at a time), move and copy (`from`, `to`) never replace:
  each publishes with a no-replace rename (`renameat2 RENAME_NOREPLACE` on
  Linux, `renameatx_np RENAME_EXCL` on macOS; a filesystem without it is
  `project_file_unsupported_platform`), so a destination created concurrently
  is `project_file_exists` and keeps its bytes. A copy is built under a
  private name beside the destination (files, directories, symlinks as links;
  a FIFO or device fails it) and a failure removes only that private copy. A
  move into itself is `project_file_invalid_request`. Trash moves the entry to
  `~/.rsi/project-file-trash/<batch>/<name>` with a `<batch>.origin` sidecar.
  Each answers `{root, op, path, to?, created_top?, undo_depth}`.
- Across filesystems (hub #3148): a move copies under a private name, checks
  the source is still exactly what was copied (every entry's inode, size,
  mtime, ctime), publishes without replacing, then moves the source into the
  trash. A trash to another filesystem copies into the batch and removes only
  the source entries still exactly as copied. A changed or added entry stays
  at the source and the call is `project_file_conflict` (nothing is lost:
  every byte is at the source or in the trash).
- Undo (hub #3147): per canonical root, in memory, last 64 operations (a
  restart forgets the log; the trash keeps the data). It re-validates what it
  acts on and refuses `project_file_undo_conflict` on a mismatch, keeping the
  operation: a moved or trashed entry must be the same object (a file also
  the same size and mtime); an add's every entry must be the object it made,
  untouched, each directory holding only the next entry it made (then all are
  removed); a copy's whole tree must match its seal (then it is trashed, never
  destroyed). Trash restores and move-backs never replace (an occupied
  original is `project_file_undo_conflict`). The daemon trash is refused as
  any endpoint, an undo's included (hub #3149). An empty log is
  `project_file_nothing_to_undo`.
- Trash retention (local #120): the TUI explorer's 7-day rule through the one
  shared routine `rsi_common::trash_prune` (the TUI prunes its
  `explorer-trash` with it when the explorer first opens). A batch is named
  `<UTC %Y%m%dT%H%M%S>-<8 hex>`; it and its sidecar are removed once that
  recorded time is more than 7 days old (not the mtime; a future time is
  kept). The pass opens the trash root `O_NOFOLLOW` (a symlinked root is not
  pruned), keeps every symlink, special file and foreign name at the top level,
  and removes a batch one level at a time through `O_NOFOLLOW` handles checked
  against their `lstat` identity, so a link inside a batch is unlinked as the
  link and its target is never touched. Bounded per pass (10,000 top-level
  names, 200,000 removed entries, depth 256). It runs once at daemon start
  (a blocking task, log line `Project-file trash prune pass finished`,
  `trigger=daemon_start`) and after a trash write when the previous pass is at
  least an hour old (`trigger=trash_write`), always under the write lock. Undo
  of a trashed entry whose batch is gone (pruned or removed by hand) is
  `project_file_no_longer_in_trash` and drops the entry, so the next undo
  reaches the operation before it.

## Input-bar assets (hub #3044)

Operator-only methods move the desktop input bar's host file access behind
`rsid`, so the desktop works off-host (`rpc/input_assets.rs`, wire types and
the shared discovery in `rsi_common::input_assets`). Like the project-file
methods they are in no attributed audience and the rsi-remote gateway never
calls them; `input_asset_rpcs_are_operator_only_and_off_the_remote_gateway`
pins all of them (`INPUT_ASSET_METHODS`).

- `UploadPasteImage {mime, data_base64}` -> `{path, name, mime, bytes, sha256,
  reused}`. `mime` is one of `image/png`, `image/jpeg`, `image/gif`,
  `image/webp` (case-insensitive, else `paste_image_unsupported_type`) and the
  decoded bytes' magic must declare that same type (`paste_image_type_mismatch`).
  At most 32 MiB decoded: a longer base64 text is refused before decoding and a
  larger decode after it (`paste_image_too_large`); empty is
  `paste_image_empty`, bad base64 or an unknown field
  `paste_image_invalid_request`. The image is stored as `<sha256>.<ext>` in
  `~/.rsi/paste/rsid/` (`data_path("paste")`; one level below the top-level
  `*.png` files the TUI removes when it exits), directory `0700`, file `0600`,
  written to a `create_new` temporary file and renamed over the name, so a link
  planted at the name is replaced, never followed. An identical regular file
  already there is `reused: true` and nothing is written. The request chooses
  no part of the path. `path` is what a message references (`@<path>`).
- `ListSlashCommands {project_id | session_id, max_entries?}` -> `{root,
  commands: [{name, description, source: command|skill}], truncated,
  skipped_outside_root}`. The root resolves as for the project-file methods
  (same `project_file_unknown_root` / `project_file_invalid_request` codes; an
  unknown field is `slash_commands_invalid_request`). Entries are
  `.claude/commands/<name>.md` files and `.claude/skills/<name>/SKILL.md`
  directories; every candidate (`.claude`, both directories, each entry, each
  `SKILL.md`) must canonicalize inside the root, else it is skipped and counted
  in `skipped_outside_root`. `description` is the frontmatter `description`
  (first 16 KiB read, one line, at most 300 chars), else the TUI's generic
  text. Sorted by name; `max_entries` default 500, max 2000. The TUI's own
  suggestions (`rsi::suggestions::discover_commands`) call the same
  discovery.
- `ListSkills {project_id | session_id | user: true, max_entries?}` ->
  `{skills_dir, skills: [{name, enabled, description, has_skill_md}],
  truncated, skipped_outside_root}` and `GetSkill {project_id | session_id |
  user: true, name}` -> `{name, enabled, path, content, bytes, truncated}`
  (hub #3062, read-only). `user: true` reads the daemon user's
  `~/.claude/skills` (the TUI's Settings > Skills; its containment root is that
  canonical directory, so a linked `~/.claude` works); a project or session
  root reads `<root>/.claude/skills`, which must canonicalize inside the root.
  A `user` mixed with an id, a missing `name` or an unknown field is
  `skill_invalid_request`; the id forms keep the project-file codes. Each skill
  directory and its `SKILL.md` must canonicalize inside the containment root:
  `GetSkill` refuses a link out of it (`skill_outside_root`) and `ListSkills`
  skips and counts it. `name` is one path component (no `/`, `\`, NUL, `.`,
  `..`; at most 255 bytes, else `skill_invalid_name`); `<name>.disabled/` (the
  TUI's disabled form) is read when `<name>/` has no `SKILL.md`, else
  `skill_not_found`. `content` is at most 256 KiB, cut at a character boundary
  with `truncated: true`; `bytes` is the whole file. `enabled` is false for a
  `.disabled` directory. The TUI's Settings > Skills list and skill preview
  (`rsi::claude_config`) call the same `list_skills` / `read_skill`.
- `SetSkillEnabled {name, enabled}` -> `{name, enabled, changed, path}` and
  `DeleteSkill {name}` -> `{name, enabled, path}` (local #97): the TUI's
  Settings > Skills `e` and `d` on the daemon user's `~/.claude/skills` only
  (no scope field; any other field is `skill_invalid_request`). Disable renames
  `<name>/` to `<name>.disabled/` and enable renames it back; an existing
  `<name>/` is the current state when both exist, already being in the
  requested state is `changed: false`, and a rename that would land on the
  other existing directory is `skill_conflict`. Delete removes `<name>/`, else
  `<name>.disabled/`, with everything below it, permanently: the TUI's `d` has
  always removed the tree (no trash, no undo); the confirmation lives in the
  client (the desktop asks first, the TUI does not). `name` follows
  `GetSkill`'s rules. The entry must be a real directory directly in the
  skills directory: a symlink (in or out of the root), file, FIFO or other
  entry is `skill_not_directory` and untouched. All I/O runs through handles
  pinned at the canonical skills directory with no-follow opens
  (`AssetDir::rename`, `AssetDir::remove_tree`): a link inside a deleted skill
  is unlinked, never its target. Missing is `skill_not_found`; a failed rename
  or removal is `skill_write_failed` (a removal can stop part-way, as
  `remove_dir_all` does). The TUI's `e`/`d` (`rsi::claude_config`) call the
  same `toggle_skill` / `delete_skill`. Unix only (`AssetDir`); elsewhere the
  call fails `skill_read_failed` with an unsupported error.

## Prompt files (hub #3040, local #74)

Four operator-only methods serve the prompt creator's store
`<data dir>/prompts/*.md` (`rsi_common::identity::data_dir()`, normally
`~/.rsi`) from the local socket (`rpc/prompt_files.rs`, logic in
`prompt_files.rs`, wire types `rsi_common::prompt_files`). Like the project
files they are in no attributed audience, no native tool or agent CLI verb, and
off the rsi-remote gateway (fence test
`prompt_file_rpcs_are_operator_only_and_off_the_remote_gateway`).

- `ListPromptFiles {}` -> `{dir, files: [{name, first_line, size, mtime?}],
  truncated}`, newest first then by name; a missing directory is an empty list.
  At most 2000 files (scan stops at 20000 entries); `first_line` comes from the
  first 4 KiB, without leading `#`, at most 200 characters. Invalid names,
  symlinks and non-files are skipped.
- `GetPromptFile {name}` -> `{name, content, size, mtime?}`.
- `PutPromptFile {name, content, create_only?: false}` -> `{name, size, mtime?,
  created}`: a temp file in the prompts directory, fsynced, renamed over the
  name (`create_only` uses a no-clobber rename and refuses
  `prompt_file_exists`). A symlink at the name is refused, never written through.
- `DeletePromptFile {name}` -> `{name, trash_path}`: the file moves to
  `<data dir>/trash/prompts/<YYYYMMDDTHHMMSS.nnnnnnnnnZ>-<name>` (hard link and
  unlink, or a no-clobber copy and unlink across file systems; `-<n>-` is
  inserted on a name collision). Nothing is unlinked without a trash copy;
  moving the file back restores it.
- Names: one flat file name ending in `.md`, at most 200 bytes, with no `/`,
  `\`, `..`, `:`, control character, leading `.` or surrounding whitespace
  (`prompt_file_invalid_name`). The prompts directory must resolve directly
  inside the canonical data directory and the trash inside it
  (`prompt_file_outside_root`); a symlinked prompt is `prompt_file_symlink`
  (Unix reads open `O_NOFOLLOW`). Content is at most 1 MiB
  (`prompt_file_too_large`) and UTF-8 (`prompt_file_not_utf8`); a missing
  prompt is `prompt_file_not_found`; bad params are
  `prompt_file_invalid_request`. All refusals are `INVALID_PARAMS` with the code
  as the message prefix.
