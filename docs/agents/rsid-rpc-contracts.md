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
The operator settings `rolling_queue_enabled`, `rolling_queue_batch_size` (1-8,
S1 runs one source per gate) and `rolling_queue_speculation_depth` (0-2) and the
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
rebuilds `agent_jobs` (a provisional migration); each unit has
`MemoryMax`/`CPUQuota` and a capped log (`head -c` in the wrapper: at the cap the
log gets a marker and the recorded status is 153); only an appointed manager may pass `worktree`, and it must be a
worktree of the caller's own repository (same `git rev-parse --git-common-dir`).
The row lives in `agent_jobs` (provisional migration, terminal rows immutable and
undeletable). `submit` inserts the `running` row and starts a
`systemd-run --user --collect` unit `rsi-job-<id>` (`TimeoutStopSec` 60 s, 120 s
for landing and 20 min for `cloud_gate` so its destroy trap finishes;
`RuntimeMaxSec` per kind); the unit's fixed `/bin/sh` wrapper redirects output to
`~/.rsi/jobs/<id>.log` and writes the exit status to `<id>.status` as its last
act. A launch failure returns `job_launch_failed` and never wakes.

Test/build units receive a private, disk-backed `<id>.tmp` directory beside
the log as `TMPDIR`; launch is refused if this resolves to tmpfs/ramfs.
Their `CARGO_TARGET_DIR` preserves an explicit daemon environment override,
otherwise resolves the worktree/ancestor and global Cargo `build.target-dir`,
falling back to the worktree's `target/`. Scratch is removed on launch failure
or terminal settlement, including restart reconciliation. Landing/cloud-gate
units retain their existing environment; session tokens are never forwarded.

The daemon task
`run_agent_jobs_loop` polls `running` rows every 5 s and on startup (the same
poll is the restart reconcile): a status file settles the job `succeeded` or
`failed` (`landing`/`cloud_gate` classify with the queue's `classify_run`,
success means a published tip); an inactive unit with no status after a 60 s launch
grace settles `lost`. `settle_agent_job` CASes `running -> terminal` and inserts
the owner's single `scheduled_jobs` resume wake in the same transaction, so
overlapping polls and restarts never wake twice.
