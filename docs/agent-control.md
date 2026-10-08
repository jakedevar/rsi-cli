# Agent Control via RPC-CLI

RSI lets an AI provider session (Claude Code, Codex/Pioneer, Antigravity/Gemini, the
direct-API Harness, or CodexAppServer) **drive the daemon it runs under** — spawn
child agent sessions, check their status, halt them, and schedule its own
resume-wake — without handing the agent the full operator control surface. This
page describes that capability, its security model, and how each provider reaches
it.

The daemon RPC surface is the single source of truth. Agents are exposed to it
only through a **restricted, tokened set of `Agent*` verbs** — never the generic
passthrough that the TUI uses.

---

## Why it is shaped this way (threat model)

RSI is a single-user local tool. The daemon socket (`~/.rsi/daemon.sock`) is
`0o600` — trust-by-ownership. Any process running as the user can open it and
call any verb, so **method-name allowlisting at the socket is not a security
boundary** (the TUI legitimately needs raw `LaunchSession`).

The enforceable lever is **session attribution**, not cryptography:

- A call carrying a valid **per-session token** is an *agent* call → it is
  default-denied except for the `Agent*` verbs and an enumerated set of read
verbs, and those verbs apply their own self/lead scoping.

## Codex and Claude typed tools

Standard Codex sessions and tokened Claude CLI sessions receive the
session-ephemeral `rsi-agent-mcp` stdio server. Its tool list is derived from
the same closed native-agent catalog used by Harness and CodexAppServer; it
intentionally excludes RPC-only `AgentContinueChild`. The daemon supplies its
installed sibling path (never a PATH lookup): Codex gets per-process
`-c mcp_servers.rsi-agent.*` overrides, Claude gets one `--mcp-config` JSON
argument (tools appear as `mcp__rsi-agent__rsi_control_*`). The token is
inherited through the environment and never appears in argv. RSI never runs
`codex mcp add`/`claude mcp add` or edits a user configuration file. A Claude
launch without the installed gateway falls back to `rsi-rpc`.

Tool input is locally validated before a socket call, then daemon authority and
runtime gates still decide the request. `rsi-rpc` remains the compatible,
validated fallback. Neither transport accepts a token, caller identity, or
permission in input.
- A call with **no token** is an *operator* call (TUI / script) → it keeps the
  full, unchanged surface.

The token is a **correctness guard against a *confused* agent** (one that follows
its instructions but reaches for a verb it shouldn't), **not** a defense against a
malicious local process. We do not attempt the latter — it is the wrong threat
model for a single-user tool.

---

## The token

At launch the daemon mints a random `RSI_SESSION_TOKEN`, maps `token →
session_id`, and stamps it into the provider process environment alongside
`RSI_SESSION_ID` and `RSI_SOCKET`. The token:

- rides `RpcRequest.session_token` on the wire — a **sibling of `params`**, sourced
  from `$RSI_SESSION_TOKEN` by `rsi-rpc`;
- must **never** be typed into `--params` (shell commands and tool inputs are
  persisted; the token must not land in a stored transcript);
- rotates on `ContinueSession` / `RotateSession` and is invalidated on terminal
  status.

Authority is always resolved **server-side** from the token. Nothing the agent
supplies in `params` can change *which* session it is acting as.

---

## The agent verb catalog

**Entry point: `AgentGetAuthorityCatalog`.** Every tokened session may call it
(native `rsi_control_authority_catalog`). With `{}` it renders the caller's
current `agent_authority_projection` (`crates/rsid/src/session/agent_authority.rs`)
as an operator's manual: `roles`, role `guidance` (the binary-shipped
`crates/rsid/src/session/guidance/*_v1.md` sections for exactly those roles),
the permitted `controls` in catalog order, and, for managers, the permitted
update variants, control/prepared actions and delegated operator methods.
`{"verb": "<method | native tool | mcp__rsi-agent__ tool>"}` returns only the
compact envelope (`schema_version`, `session_id`, `authority_revision`, `roles`,
`pending`) and one `control` detail: `permitted`, the parameter `parameters`
schema, one minimal valid `example` request (the schema fixture the tests
validate with `validate_params`; `crates/rsi-common/src/agent_control_examples.rs`)
and the stable `refusals` (`code`, `next_action`) the control returns. No
`controls` list, `guidance` or manager lists repeat in a `verb` response. A
pending initial role
publication returns the worker baseline with `pending: true`. It is read-only
and grants nothing; every other verb keeps its own guard. Refusals:
`authority_catalog_unknown_verb`, `authority_catalog_invalid_request`. The
startup prompt is a short role-independent frame that points here, so role
rules and control lists are served from current authority instead of a static
prompt.

These `Agent*` RPC verbs (53, pinned by the schema catalog test) form the closed agent control surface. The
separate, narrow read allowlist is unchanged; generic lifecycle and
configuration methods remain denied to token-attributed callers.

**Read scope (#241).** A token-attributed caller of `GetSession`,
`GetSessionSummary`, `ListSessionChildren`, `GetConversation`,
`GetConversationsSince` or `GetTurnMetrics` may read only sessions inside its
read scope. That scope covers itself and its `continued_from` rotation lineage in
both directions. It covers sessions below it on the parent chain and every
session of an Epic it leads. For the current appointed manager it also covers
sessions reached by `manager_session_scope`. For the three metadata verbs only,
it also covers the caller's owning Epic and that Epic's Group, but never an
intermediate parent session. A `parent_id` or `continued_from` chain that
repeats a session or runs past its bound (64 parent links, 256 lineage links)
on either the caller's or the target's side is refused, not truncated.
Anything else, including a session that does not exist, is refused with the
constant typed error `agent_read_scope_denied`, which names no target.
`ListSessionChildren` refuses an unreadable named parent and otherwise returns
only readable rows, so a root listing shows only readable roots.
`GetConversationsSince` is all-or-nothing: one unreadable cursor refuses the
batch. `GetHealthStatus` and `GetDaemonCapabilities` stay unscoped, and
unattributed (operator/TUI) reads are unchanged.

| Verb | What it does | Who may call |
|------|--------------|--------------|
| `AgentGetAuthorityCatalog` | Return the caller's roles, role guidance and currently permitted controls; `verb` adds one control's schema. Read-only. | Any tokened session (self only) |
| `AgentSpawnChild` | Reserve and enqueue a child session. An optional `provider` selects any supported child backend; omit it to preserve same-provider inheritance. Optional `agent_role` is normalized display metadata. For a different provider, supply a target-valid `model`/`effort` when needed; no model uses that provider's native default. A required stable `idempotency_key` makes exact retries return the same durable `spawn_request_id`, `child_session_id`, role, and server-reserved Epic ordinal; changed content conflicts. Durable launch automatically arms the owner's terminal watch. The parent is bound server-side. | A leaf-kind caller that is the **lead** of its owning Epic — the first Epic ascending its parent chain must have `lead_session_id` = the caller. A non-lead leaf, or a leaf with no Epic ancestor, is rejected `NotLead` ("emitter is not the Epic lead"); remedy: promote the caller via `SetEpicLead` (or spawn from the lead), then retry |
| `AgentReserveSuccessor` | Reserve one stable, same-Epic successor for authority-preserving master turnover after the caller settles. Exact replay returns the same receipt; changed content under the same key conflicts. | A live spawnable leaf that is the current lead of exactly one owning Epic |
| `AgentGetProgress` | Read one UUID-sorted durable snapshot for the caller's authorized child cohort: status counts, rotation-tip cursor/freshness, automatic-watch state, and message counts. The cursor's `lineage_tip_id`, `event_sequence`, and `custody_generation` are exactly the `AgentContinueChild` staleness-fence tuple, read from the same snapshot, so they may be used directly; `custody_generation` is omitted when the tip has no sandbox custody. Omit `session_ids` for the full cohort or pass at most 256 raw entries (duplicates still count) in an authorized subdivision. | Direct children and children of an Epic the caller leads; the current manager may also name sessions in its live scope |
| `AgentSendMessage` | Enqueue durable owner-to-child mail. Sender identity is server-bound and a stable `idempotency_key` gives exact replay/conflict behavior. Delivery does not interrupt the child's active turn. | A direct reserved/started child, a child of an Epic the caller leads, or a manager-scoped leaf under `SessionControl` |
| `AgentGetStatus` | Read status of the caller or a session it is authorized to observe. | Self, a direct child, a child of an Epic the caller leads, or a session in the current manager's live scope |
| `AgentReadSessionEvents` | Read a bounded, byte-capped page of a session's conversation events (`session_id`; `after_sequence` for a forward page, else the newest `limit`; optional `event_types`, `max_bytes`) plus `final_message` (last assistant message of the rotation tip), `status`, `terminal_reason` and paging cursors. Read-only; content, tool input and metadata are clipped per event. An unknown target is refused identically to an out-of-scope one. | Same as `AgentGetStatus`: self, a direct child, a child of an Epic the caller leads, or a session in the current manager's live scope |
| `AgentHalt` | Interrupt a session under the caller's authority. Omit the target to halt self. | Self, a direct child, a child of an Epic the caller leads, or a manager-scoped leaf under `SessionControl` |
| `AgentContinueChild` | Continue an exact child session with a new prompt. Wraps the daemon's existing continuation engine; it does not reimplement one. Requires the observed continuation cursor `(expected_tip_session_id, expected_event_sequence, expected_custody_generation)` as an optimistic staleness fence — `Session` carries no `row_version`, so these three are what move when a child advances. The check is not atomic with dispatch and is not an idempotency key: concurrent or rapid sequential requests can both be delivered before either query event advances the cursor, and a query-event persistence failure can leave the cursor reusable. Treat an accepted continuation as delivered; never replay it from the receipt's pre-continuation `observed` cursor. A later stale refusal carries the observed witness to inspect before making a new decision. Bounds failures use `agent_continue_invalid_request`; self-targeting, a resolved `CodexAppServer` tip (it allocates a fresh id on continue), and a stale cursor are also typed refusals. A logical AppServer root that already rotated to an ordinary Codex tip is checked and continued at that effective tip. Continuing a running child interrupts its active turn through the existing continuation engine. A dirty sandbox is NOT a refusal. The caller's terminal watch is re-armed against the logical child, best-effort. RPC-only — no native `rsi_control` tool in slice 1. | A direct child, a child of an Epic the caller leads, or a manager-scoped leaf under `SessionControl`; never self |
| `AgentArchiveChild` | Archive a terminal child of the Epic the caller currently leads, using the observed continuation cursor as a staleness fence. | Current lead of the child's Epic |
| `AgentScheduleWake` | Schedule a future wake/callback. The resume target `wake_session_id` is bound to the caller server-side and is not in the JSON schema — it cannot be supplied or spoofed. `mode:"on_terminal"` + `watch_session_id` (A8) arms a daemon-owned [session watch](session-watches.md) on a watched subject scoped like `AgentGetStatus` targets; the wake target remains caller-bound. `mode:"when"` (#1006) arms a daemon-evaluated predicate wake: `when` is `{"jobs_terminal":[job ids you own]}` or `{"sha_on_rolling":"<40-hex>"}`, with optional `timeout_seconds`; the scheduler evaluates it (no model turn), it survives a restart, and it resumes the caller once with a bounded per-job report (`timed_out: true` on timeout). | Self (resume); watched subject: direct child, child of a led Epic, or a session in the current manager's live scope |
| `AgentCancelWake` | Disable your own scheduled wake(s) by exactly one of `job_id` or `name`; the owner is the authenticated caller, so another session's job is reported as not found. Rows are disabled (`enabled=0`), never deleted; the daemon-owned program guard and manager watches are refused. RPC-only. Already-disabled own jobs are an idempotent no-op. | Self |
| `AgentListWakes` | List your own scheduled wakes (id, name, mode, watch target, next_fire_at, enabled, created_at), bounded by `limit` (default 64, max 256); `include_disabled` adds cancelled or fired history. Caller-bound: no other session's jobs are visible. RPC-only. | Self |
| `AgentCreateIssue` | Create a durable local issue follow-up. Strict content and idempotency fields only; creator identity is server-bound. | Self |
| `AgentListIssues` / `AgentGetIssue` | Read bounded project Issue pages or one Issue by ID. | Current lead of one legal owning Epic, or the current appointed manager with the V2 `IssueCoordinate` grant |
| `AgentUpdateIssue` | CAS-update active Issue content. A worker launched by `AgentManagerLaunchIssueWorker` may, while its binding is live (the worker is live and no later launch of the Issue superseded it), append to its own Issue's body only: `body` alone, keeping the current body as its prefix (#1284). A bound worker's refusals are distinct (#1545): `bound_issue_binding_not_live` (the worker ended or a later launch superseded it), `bound_issue_wrong_issue` (another Issue or project), `bound_issue_field_not_allowed` (`validation.field` names the first field beyond `body`), `bound_issue_not_append_only` (the new body does not start with the current body byte for byte: re-read with `AgentGetIssue` and resend it verbatim plus your text). A caller that was never bound keeps `authority_denied`. | Current owning-Epic lead, or `IssueCoordinate` manager (recorded as actor `manager`), or a live Issue-bound worker on its own Issue (recorded as actor `session` with its owning Epic) |
| `AgentUpdateIssueStatus` | CAS-update one Issue lifecycle status. | Current owning-Epic lead, or `IssueCoordinate` manager (recorded as actor `manager`) |
| `AgentArchiveIssue` / `AgentRestoreIssue` | Archive a terminal Issue or restore its archive marker without reopening it. | Current owning-Epic lead, or `IssueCoordinate` manager (recorded as actor `manager`) |
| `AgentListIssueEvents` | Read immutable audit history in ascending bounded pages. | Current owning-Epic lead or `IssueCoordinate` manager |
| `AgentManagerProgress` | Read bounded selected-Epic progress, evidence, questions, and recent request states. | Operator-appointed project manager |
| `AgentManagerInbox` | Retrieve durable manager requests or replies with `after_sequence`. Page notices independently with `after_notice_sequence` / `next_after_notice_sequence`, optionally filtered by `notice_kind`; returned notices settle automatically. Settle up to 32 of your own notices with `settle_notice_ids`. | Appointed manager or current lead of the addressed scoped Epic |
| `AgentManagerSend` | Queue an attributed request to a scoped Epic's current lead without interrupting its turn. Manager-mail fencing uses the durable request ID: a moved seat or changed scope invalidates old IDs and reissues a standing request. Answer each current direct request on its own recorded ID; never reuse an older ID after a seat move. | Appointed manager |
| `AgentManagerReply` | Record an explicit reply correlated with a request. The daemon binds the reply to the named current standing request; a fenced old ID is refused rather than silently rerouted. | Current lead of the addressed scoped Epic |
| `AgentManagerNotify` | Queue an unsolicited lead notice to the current manager without interrupting its turn. | Current lead of a scoped Epic |
| `AgentManagerInspect` | Cursor-page scoped workers, work, requests, decisions, topology, resources, actions, events, archive and health with freshness and explicit unknowns. `section:"friction"` (managers only, #1333) is the project's 24 h friction rollup: one row per signature with counts, sessions, evidence refs and the andon's filed kaizen Issue. `section:"rulings"` (managers only, #1415) lists the pending non-gate decision records the caller may settle with `decision_ruling`: its own ledger's and, for a portfolio seat, the project manager's. | Live manager/lead scope checked by the shared inspection service |
| `AgentManagerUpdate` | Apply one typed work/stage/dependency/ownership/migration/acceptance/integration/request/decision/handoff change with scope/policy and record fences. `request_review` carries `expected_row_version`, the work row's current `row_version`: read it from `AgentManagerInspect` `section:"work"` (each work row carries `row_version` and its `review` state; no SQLite read needed). A stale value refuses `manager_review_work_version_changed: current_row_version=N`, so the retry uses N. `decision_ruling` settles a non-gate decision record (the owning manager, or a covering portfolio seat above it), `decision_withdraw` withdraws the owner's own (#1415); real gates stay operator-only. | Explicitly granted manager; own request lifecycle acknowledgement remains current-lead-bound |
| `AgentSubmitReviewReceipt` | Submit one immutable exact-source receipt for a DB-native review assignment. Reviewer identity, invocation and custody are daemon-bound. | Live assigned reviewer of the assignment |
| `AgentManagerControl` | Queue a bounded lead lifecycle, container, session creation, lead assignment or scoped session housekeeping action with exact target fences. A receipt establishes journal admission, not execution. | Current manager with the explicit capability and live operator policy |
| `AgentManagerPrepareControl` | Preflight a supported semantic lead/session action against daemon-resolved live authority. It returns bounded readiness and creates no lifecycle action. | Current manager with the explicit capability and live operator policy |
| `AgentManagerCommitPreparedControl` | Commit an exact prepared ID/digest with an idempotency key. Authority, target state and runtime gates are atomically rechecked before one legacy action-journal operation is queued. | The same current manager that prepared the action |
| `AgentManagerGetAction` | Read one durable action receipt without widening inspection scope. | Authenticated current manager in the same project and stable logical-manager lineage; an active project manager may also read a proven retired appointment's action by id. Global/node epochs are separate principals (#1553, #1571) |
| `AgentManagerLaunchIssueWorker` | `{issue, brief, launch, parent_epic_id?, idempotency_key}` (#1100): launch one worker bound to one Issue in one idempotent call. It runs the `create_session` manager action (same authority, allowlist, capacity and Epic scope, refused before any effect), appends an Issue note naming the worker, sets the Issue InProgress, and arms the caller's `on_terminal` watch once the worker session exists (the result's `watch` is `armed` or `pending_launch`; a replay re-arms). The action receipt, note and status commit in one transaction. The bound worker may call `AgentGetIssue` on its own Issue only (any other Issue is `agent_issue_authority_denied`), so briefs need not paste the Issue. `review_of: <implementer Issue number>` (#1590) makes the daemon copy that Issue's text and complete latest handoff (an oversized handoff keeps its head and tail, so the landing filters survive) into the reviewer's brief; do not paste a handoff yourself. An unknown `review_of` Issue is refused `manager_issue_worker_reviewed_unavailable`; it excludes `continue_from`. | Current manager holding both `SessionCreate` and `IssueCoordinate` |
| `AgentManagerWorkView` | Read-only (#548): the caller's Epic live work (`mine` when its recorded source is the caller's lineage), active granted file ownership (domain/mode/files), pause, and delivery state (queued/retrieved/delivered, `delivery_issue`) of at most 8 unanswered manager requests, without message bodies. `{}` or `work_key`/`after_work_key`/`limit` (1–32). Writes nothing and grants no write, mail or continuation authority. | Live lineage tip of a session the current logical manager created (`create_session`/`retry_lead`/`replace_lead`) at the current scope version, in a live scoped Epic; otherwise `manager_work_view_not_managed`, `…_stale_session` or `…_unsupported_topology`; an exact `work_key` that is no longer live fails `manager_work_view_work_not_live` |
| `AgentTopologyUpsert` | Create, revise (`expected_revision` CAS) or `validate_only` a scoped deterministic topology; returns `definition_digest` and `diagnostics[]`. Every session node's explicit provider/model/effort must equal an operator `allowed_launches` entry (empty fails closed); a reviewer's vendor family must differ from its author's; wide same-kind layers must be OpenRouter (`topology_bulk_fanout_min_openrouter`). | Current manager with `Automation` (in-scope Epics or manager scope), or an Epic lead for its own Epic (`scope:"epic"` only) |
| `AgentTopologyList` | Page visible topologies (and, with `include_executions`, the scope's live executions). Operator and manager topologies are visible to leads only when the operator marked them `shared`. | Same |
| `AgentTopologyExecute` | Start one daemon-run execution fenced by `expected_digest` on an in-scope Epic; policy is rechecked, an identical key replays (`deduplicated`), manager launches charge `max_created_sessions`, and at most 3 session nodes run at once. | Same; a manager also needs Execute mode and no pause |
| `AgentTopologyGetExecution` | Read one execution's status, `row_version`, node attempts and a page of audit events (no payloads). | Same, for executions under an Epic in scope |
| `AgentTopologyInterrupt` | Request interruption under a `row_version` CAS; idempotent on the key. Allowed while the manager is paused. | Same |
| `AgentTopologyResolveAttempt` | Resolve preserved work: `inspect`, `accept`, `retry`, or `discard` with `confirm_preserved_commit` (full 40-hex, required iff discard; mismatch ⇒ `preserved_commit_mismatch`). | Same; `discard` is manager-only (`discard_requires_manager` for a lead) |
| `AgentEnqueueLandingSource` | Enqueue one accepted source commit (`source_commit`, optional `test_filters` as `PACKAGE=FILTER`, `idempotency_key`) on the daemon-owned rolling merge queue (#1007). The daemon gates it with the lander, publishes one fast-forward and wakes the enqueuer exactly once with the landed SHA, refusal code or failing tests; refused `queue_disabled` while the operator has the queue off. No native tool. | Current appointed manager or current Epic lead; else `queue_not_authorized` |
| `AgentSubmitJob` / `AgentGetJob` / `AgentListJobs` / `AgentCancelJob` | Run a typed `test`, `build`, `landing`, `cloud_gate` or `cloud_sweep` (manager/lead only; typed GREEN/RED/INCOMPLETE verdict) operation as a daemon-owned durable job (#1002) in your own sandbox (`worktree` lets an appointed manager name another worktree of its repository); `{kind, params, name?, idempotency_key?, wake?}`. Linux jobs run in a `systemd-run --user` unit; macOS package test/build and recipe jobs run in a per-user launchd service (two Cargo workers; Linux-only shard, candidate-receipt, landing and cloud workflows, and other operating systems, return `job_platform_unsupported`). Both run outside every session scope and one resume wake carries the typed result; `wake:"none"` suppresses that per-job wake (the result stays readable) so a batch can share one `AgentScheduleWake` `mode:"when"` `jobs_terminal` wake. `Get`/`List` read your own jobs. A `test` job with `{recipe: "check-cpu"}` runs a project-declared just/make gate from `.rsi/jobs.toml` (version 1, `[recipes.check-cpu]` with `runner`, `target`, `timeout_minutes` cap and `cpu_quota_percent` cap); see [the recipe contract](agents/rsid-rpc-contracts.md). It shares test timeout, result, cancellation and batch-wake behavior. A package `test` run needs no `filters` (empty runs every test of the package) and always passes `--no-fail-fast`, so one red binary never hides the others (#1106). A `test` job with `{candidate_receipt: <branch or sha>}` (manager/Epic lead only, #1099) verifies that candidate against `origin/rolling` in a temporary detached worktree with its own target dir (`scripts/candidate-receipt.sh` -> `scripts/check-touched-shards`) and returns the typed receipt (base, head, merge clean, shards compiled, audit verdicts, new migrations, `suggested_filters`) in `result.receipt` (the wake carries it too); it settles `succeeded` only when the receipt's `ok` is true. `AgentCancelJob` `{job_id}` (owner only; `job_not_found` otherwise) stops a running job's platform service by label and settles it `failed` with refusal `job_cancelled`, without a wake; cancelling a settled job changes nothing. | `test`/`build`: any live leaf session; `landing`/`cloud_gate`/`cloud_sweep` and `test` with `candidate_receipt`: current appointed manager or current Epic lead, else `job_kind_not_authorized` |
| `AgentSendSatelliteMessage` | Queue one message (`peer_id`, `remote_session_id`, `message`, `idempotency_key`, optional `expires_at`) for an idle session on a paired satellite host. Current appointed manager only; the operator must enable per-satellite dispatch and declare the target in scope. Delivery waits until the remote session is idle and never interrupts; `queued` is acceptance, not delivery. Every refusal is the same `target_not_authorized`. |
| `AgentReportToHub` | Queue one short typed report (`kind` `enqueue`, `deploy_ready` or `result`, `text` at most 1024 bytes) for the hub manager (#1103). Satellite-side only: the appointed manager that is also the operator-declared seat or its rotation tip, with an allowlisted hub. The hub pulls it over the existing link into its `AgentManagerInbox` as an untrusted, informational satellite report that carries no authority; `queued` is acceptance, not delivery (at most once; a restart loses unsent reports). RPC-only; every authorization failure is the same `satellite_report_not_authorized`. |
| `AgentGetDaemonInfo` | Read-only `{}`: the hub daemon's `build_sha`, `binary_sha256`, `started_at` (RFC3339 nanos), `uptime_secs`, `schema_version`, `disk_free` (data dir and sandbox base), `load` (1/5/15) and `supervisor_mode` (#1045), plus for the appointed manager `satellites`: each enabled paired peer read over the link now (`build_sha`, `binary_sha256`, `started_at`, `schema_version`, `supervisor_mode`, `last_deploy`, or `reachable:false`; #1017). Secret-free. Current appointed manager or Epic lead only (`daemon_info_not_authorized` otherwise). | Same |
| `AgentRequestDeploy` | `{sha, binaries_dir, idempotency_key, max_wait_secs?, interrupt_workers?, peer_id?}` (#1045; `interrupt_workers: true` (#1461) stops a worker mid-turn blocking the quiet point once the operator's drain hold is over: the restart interrupts it, the post-restart path resumes it with a continue prompt, the outcome names the interrupted worker sessions and each is a `deploy_interrupt` friction event; a landing or local job still blocks; not with `peer_id`; with `peer_id` the request runs the same deploy flow on that paired satellite over the hub link, #1017: `binaries_dir` is a path on the satellite, the hub stores nothing, and the outcome is read from `AgentGetDaemonInfo` `satellites`): stage and verify built binaries (`sha` must equal the staged `rsid --build-info`; the directory must be under the sandbox base, `~/.rsi/staging` or `~/.cargo/shared-target`), wait for a quiet point, swap them in and restart under `rsid-supervisor.sh`; the outcome wakes the caller once (`succeeded`, `failed`, `timed_out`). `build:true` is refused `deploy_build_not_supported`. Current appointed manager holding the operator-granted `Deploy` capability, Execute mode, not paused. | Same |
| `AgentQueryFailureSignatures` | Read-only `{test_id?, digest?}` (at least one; `digest` is the 64-hex `rsi-known-failure block` digest): the known-failure signature records (#1016) that open Issues of your project carry, as `{records:[{record, issue_id, issue_status}], malformed_issues}`. `record.issue` is the owner Issue and `record.class` is `regression`, `flake`, `env` or `seed`. Records of a Closed, Cancelled or archived owner Issue are never returned, so expiry is live, and no Issue body is returned. An unknown query is `records: []`. Native `rsi_control_query_failure_signatures`. | Every live leaf session, scoped to its own project |
| `AgentGlobalOverview` | Read-only `{}` (#872 Slice B): for each project in the global grant, its current project manager (status, provider, model, effort, `context_fill_pct`, cost, `updated_at`, scope version), that manager's policy (mode, revoked, paused, capabilities), Issue counts (Open, InProgress, Open `operator-request`), Running and WaitingApproval session counts, pending questions and approvals. Native `rsi_control_global_overview`. | The active global seat (`global_manager_not_seat` otherwise) |
| `AgentManagerOverview` | Read-only `{}` (#1240): your own manager node as one bounded snapshot: its grant and seat, `parent` (`parent_of` it), each child (child portfolio nodes, the project managers you manage directly, child areas) as a digest (seat status, model, `context_fill_pct`, grant version, coverage, summed Issue and session counts, pending escalations; never the child's own projects or inbox), the projects you manage directly (PM seat and policy, Issue counts, Running and WaitingApproval counts, pending questions and approvals), the escalations waiting on you and a `fleet` rollup of your coverage (active agents and 5m/1h/24h usage by project, provider and model). `AgentGlobalOverview` is its v0-shaped alias. Native `rsi_control_manager_overview`. | Any manager seat: a portfolio seat of any tier, a project's live PM or an area seat (`manager_tier_not_node_seat` otherwise) |
| `AgentCreateProject` | `{name, path, description?, color?}` (#1626): register a project for a directory. `path` is absolute, must exist and sit inside a configured workspace root; with no workspace roots configured, it must be a strict descendant of the daemon user's home directory (never the home directory itself, anything under `~/.rsi`, or `/`) (`project_path_invalid` otherwise); a path equal to, containing or inside the harness root is `project_harness_protected`; `name` is unique (`project_name_taken`) and a directory is registered once (`project_path_taken`). A repeat of the same name and path returns the existing project with `deduplicated:true` only when it is in the caller's coverage; otherwise `project_name_taken` with no project fields. The daemon refreshes its launch-time project index and RSI.md cache. The new project has no manager and is not added to the creator's coverage. Native `rsi_control_create_project`. | An appointed project manager or a portfolio seat, in Execute mode and not paused (`project_not_authorized` otherwise). A session holding both seats may use either; coverage is the union of the permitting seats' |
| `AgentUpdateProject` | `{project_id, name?, path?, description?, color?}` (#1626): rename a project or change its path, description or color. Limited to the caller's coverage: a project manager's own project, a portfolio seat's grant (`project_not_in_scope` otherwise, also for an unknown id). A path change is refused while the project has a Starting, Running or WaitingApproval session (`project_has_live_sessions`). Any path change on the harness project is `project_harness_protected` (operator-only). There is no agent delete or archive: the `projects` table has no archive state and a hard delete stays operator-only. Native `rsi_control_update_project`. | Same as `AgentCreateProject` |
| `AgentGlobalSend` | `{project_id, message, idempotency_key}`: durable mail to that granted project's current project manager, queued as a one-shot resume wake: delivered at its next idle boundary, and it wakes an idle manager. A replay returns the same `message_id`. Refusals: `global_project_not_in_grant`, `global_project_has_no_manager`. Native `rsi_control_global_send`. | The active global seat |
| `AgentGlobalAppointManager` | `{project_id, launch:{provider, model, effort?}, query, idempotency_key, sandbox?}`: one idempotent call that launches a Standard root session in the project (the `:blank` launch path; `launch` must be in the grant's `allowed_launches`, checked before any effect), appoints it with whole-project scope (displacing the current manager like the operator's appoint; the area-delegates refusal still applies) and saves the grant's project policy under the new scope version. Returns `{session_id, scope_version, policy_version, deduplicated}`; a replay returns the same session. RPC-only. | The active global seat |
| `AgentManagerAppointChild` | `{target, launch:{provider, model, effort?}, query, idempotency_key, sandbox?}` (#1239): one idempotent call that launches a Standard root session and seats it as a child of your node. `target` is `{kind:"project", project_id}` (appoint or replace a covered project's PM, saved with your `child_policy`), `{kind:"portfolio", tier_label, project_ids, policy, child_policy?, allowed_launches?, max_direct_reports?, launch_project_id?}` (a child node over a strict subset of your coverage, disjoint from your other children, narrowing your grant in every dimension) or `{kind:"portfolio", node_id}` (a new seat for a child you granted; its grant, ledger and workers stay). Every refusal comes before any session is created (`manager_direct_report_cap`, `manager_scope_overlap`, `manager_scope_not_narrowed`, `manager_capability_widened`, `manager_allowance_exceeded`, `global_launch_not_allowed`, `manager_project_not_in_scope`, `manager_child_operator_granted`). Returns `{appointment_id, session_id, target_ref, scope_version, policy_version, deduplicated}`. RPC-only. | An active portfolio seat at any tier |
| `AgentManagerRevokeChild` | `{node_id, expected_grant_version, idempotency_key}` (#1239): revoke a child node your node granted; nodes it granted go with it, operator-granted descendants move up one level. An operator-granted child is refused `manager_child_operator_granted`. RPC-only. | An active portfolio seat at any tier |
| `AgentReportToGlobal` | `{message, idempotency_key}`: durable mail from a project manager to the global seat (landed work, a blocking gate, a handoff path), queued as a one-shot resume wake that wakes an idle global seat. Native `rsi_control_report_to_global`. | The current project manager of a project in the active global grant (`global_report_not_authorized` otherwise) |
| `AgentReportUp` | `{message, idempotency_key}` (#1238): durable mail to the manager one level above your node (`parent_of`: parent area, the project manager, the deepest portfolio node covering your project, or your portfolio parent), queued as a one-shot resume wake; from a root it is an operator notice. Reports carry no authority. A replay returns the same `message_id`. `AgentReportToGlobal` is its alias for one release. Native `rsi_control_report_up`. | Any manager node seat: area seat, project manager or portfolio seat (`manager_tier_not_node_seat` otherwise) |
| `AgentSendDown` | `{target: {kind: portfolio\|area, node_id} or {kind: project, project_id}, message, idempotency_key}` (#1238): durable mail to a descendant node's live seat inside your coverage. Refusals: `manager_target_not_descendant` (yourself or an ancestor), `manager_project_not_in_scope` (siblings, other trees), `manager_tier_target_vacant`. `AgentGlobalSend` is its alias for one release. Native `rsi_control_send_down`. | Any manager node seat |

Existing child-control scoping: **an Epic-lead may act on its Epic's children; any leaf may
act on itself and its own direct children.** The current appointed manager may
also read status, progress and watches for sessions in its live scope; halt,
continue and mail to a scoped leaf additionally need the V2 `SessionControl`
grant in Execute mode, no project or Epic pause, and no pending question,
approval or operator pause on the target. The active global seat (#872) may read status and
session events and arm `on_terminal` watches on the project-manager seat of
each granted project, and nothing else on it. Spawning is stricter than acting:
`AgentSpawnChild` is lead-only (non-lead callers are rejected `NotLead`);
successor reservation additionally freezes the caller's current Epic-lead
authority for a later fenced transfer. Status/halt/watch scoping is unchanged
by those requirements.

### Spawn display identity

`agent_role` is an optional display-ready pipeline function such as
`Researcher`, `Planner`, or `Reviewer`. The daemon trims it, collapses internal
whitespace, preserves case, rejects control characters and empty values, and
limits the normalized UTF-8 representation to 64 bytes. Normalize-equivalent
retries deduplicate; a semantic role change under the same idempotency key
conflicts.

The response includes the normalized `agent_role` when present and the
server-reserved `epic_spawn_ordinal`. Ordinals start at 1 independently within
each Epic, are consumed when the durable reservation commits, and are never
reused after later admission, custody, provider, or cancellation failure.
Rotation, retry, continuation, provider replacement, restart, and strong
successor turnover preserve the lineage identity. The current Epic lead is
displayed as `Demiurge` with virtual ordinal `0`; this never changes its stored
role or positive ordinal. Raw/manual `Session.title` remains recoverable and is
not overwritten by this display identity.

Role and ordinal do not grant authority. Caller, owner, Epic, parent, lead,
child, predecessor, token, and ordinal inputs remain server-bound and absent
from request JSON. Display role is also distinct from `SessionKind` and from
`$CLAUDE_AGENT_ROLE`, which remains the SessionKind-derived Git-hook policy
value.

Manager authority starts with a separate operator appointment for at most 32
existing Epics in one project. V1 grants progress and durable request/reply access.
V2 requires a separate operator policy before any expanded capability. The original
sixteen verbs retain their permissions; manager grants never widen generic spawn
authority or expose ProgramRun methods. The session exception is the V2
`SessionControl` grant described above. The one Issue exception is
the V2 `IssueCoordinate` grant: the current appointed manager (the committed lineage
tip of the appointment) may then use the guarded Issue controls project-wide,
alongside the owning-Epic lead path: the reads (`AgentListIssues`, `AgentGetIssue`,
`AgentListIssueEvents`) and the four CAS mutations (`AgentUpdateIssue`,
`AgentUpdateIssueStatus`, `AgentArchiveIssue`, `AgentRestoreIssue`). A manager
mutation's immutable audit event records `actor_kind` `manager` with the manager's
session ID and request key and no owning Epic (a lead's event records `session` and
its real owning Epic). The grant and live appointment scope are rechecked inside
the mutation transaction; idempotent replay, `stale_version` and the redacted error
envelopes match the lead path. Operator-only generic Issue RPC stays closed.

Appointment/scope (`GetHarnessManager`, `ConfigureHarnessManager`), policy
(`GetHarnessManagerPolicy`, `ConfigureHarnessManagerPolicy`), operator inspection
(`GetHarnessManagerState`) and exact decision answers
(`AnswerHarnessManagerDecision`) are operator-only and absent from agent schemas,
native tools and read allowlists. The TUI provides `:manager policy`,
`:manager board` and `:manager decisions`. See
[Harness manager setup, policy bounds and evidence examples](harness-manager.md).

The six v2 schemas close every nested object. `AgentManagerInspect` defaults to
Overview and a 32-row page, with section/optional Epic/cursor and a 1–64 limit.
`AgentManagerUpdate` requires `fence`, `idempotency_key` and a `change` tagged by
`update`; `AgentManagerControl` requires the same fence/key and an `operation`
tagged by `action`. Fences contain observed scope/policy versions; lead targets
also include observed lead/generation/event/custody witnesses. Those are staleness
checks, not injected caller identity or permission. Both mutations are excluded
from `READ_VERBS`. An identical key/content retries the same durable operation;
changed content or stale authority must be re-observed before a new request.
For the six supported prepared actions, prefer `AgentManagerPrepareControl`,
commit only the returned `prepared_id` and `target_digest` with
`AgentManagerCommitPreparedControl`, then inspect the durable result with
`AgentManagerGetAction`. The existing exact-fence `AgentManagerControl` remains
source-compatible for legacy callers and for action variants outside the prepared subset.

A queued action, retrieved message, explicit request acceptance, effected action,
source-accepted work and verified integrated delivery are distinct facts. Reported
stage completion alone is insufficient. Persistent mode, pause, quota, concurrency,
retry and spend policies retain unfinished obligations across provider turns and
restart. Exact operator questions/approvals remain operator-owned: a decision
record is a real gate (main or a release, spend, credentials, deleting user
data, a human approval) answered only by the operator, or a non-gate that a
delegated manager settles with an audited `decision_ruling` (#1415).

The attribution gate lives in the daemon's single dispatch choke-point
(`handle_request_inner`), **before** the streaming `Subscribe` branch, so a
token-carrying `Subscribe` cannot slip past it. The allowlist is a **closed static
set** — never a `Get*`/`List*` prefix match (a future `Get`-named mutation would
otherwise slip through).

### Guarded Issue control (V97)

`AgentCreateIssue` remains deliberately usable by ordinary project workers for
durable follow-ups. The seven project-wide Issue controls derive caller,
project, owning Epic, current lead, and lead generation from persisted rows;
none can be supplied in JSON. Mutations require `issue_id`, positive
`expected_row_version`, and a stable NUL-free `idempotency_key`. Exact retries
return the original immutable event/result receipt; changed requests under the
same key are rejected.

The status matrix permits Open→InProgress/Closed/Cancelled,
InProgress→Open/Closed/Cancelled, and terminal→Open. Same-state and
terminal-to-terminal transitions are rejected. Archive is allowed only for
Closed/Cancelled rows; archived rows are excluded from readiness and may be
restored without changing terminal status. Use `AgentListIssueEvents` with
`after_sequence` and a 1–256 `limit` for audit pagination. Unknown and
cross-project IDs both return the same safe `not_found_in_scope` envelope.

`AgentListIssues` accepts `ready:true` to page only open, active Issues without
open or in-progress blockers. `AgentGetIssue` keeps its flat Issue fields and
adds `blocked_by` and `blocks` arrays (up to 256 related Issues each), with each
entry containing its ID, display number, and status. `blocked_by_truncated` and
`blocks_truncated` indicate when either array has additional entries.

The native roster is construction-bound, not lead-filtered: every established
Harness/CodexAppServer session gets the same seven Issue tools, and execution
performs the persisted Issue authority check. Likewise, the CLI advertises the
same thirty-six verbs to every tokened session; seeing a verb or tool is never
proof of authority.

| Verb | Example payload | Success |
|------|-----------------|---------|
| `AgentListIssues` | `{"archive":"Active","limit":64}` | `issues` plus an optional exclusive `next_cursor` |
| `AgentGetIssue` | `{"issue_id":"<uuid>"}` | the flat Issue fields plus `blocked_by`, `blocks`, and their truncation flags |
| `AgentUpdateIssue` | `{"issue_id":"<uuid>","expected_row_version":3,"idempotency_key":"edit-v1","title":"Revised"}` | `issue`, immutable `event`, `deduplicated:false` |
| `AgentUpdateIssueStatus` | `{"issue_id":"<uuid>","status":"Closed","expected_row_version":4,"idempotency_key":"close-v1"}` | the same mutation receipt shape |
| `AgentArchiveIssue` | `{"issue_id":"<uuid>","expected_row_version":5,"idempotency_key":"archive-v1"}` | the same receipt with `archived_at` set |
| `AgentRestoreIssue` | `{"issue_id":"<uuid>","expected_row_version":6,"idempotency_key":"restore-v1"}` | the same receipt with terminal status preserved |
| `AgentListIssueEvents` | `{"issue_id":"<uuid>","after_sequence":0,"limit":64}` | ascending `events` plus optional `next_after_sequence` |

Only the four mutation verbs use CAS and idempotency. Exact replay returns the
original event/result with `deduplicated:true`, even after later changes or a
daemon restart; changed semantics with the same key produce
`idempotency_conflict`. A stale CAS produces only a safe envelope such as
`{"code":"stale_version","expected_row_version":3,"actual_row_version":4,"next_action":"refresh the Issue and retry with its row_version"}`. Parser,
authority, not-found, conflict, transition/archive, and storage failures use the
same redacted envelope over RPC, Harness, and CodexAppServer.

For a malformed request to one of these seven controls, `invalid_request` may
add one optional `validation` object. Its `class` is only `missing_field`,
`unknown_field`, `invalid_field`, or `invalid_shape`; `field`, when present, is
one allowlisted public top-level field. For example, omitting `issue_id` can
return `{"validation":{"class":"missing_field","field":"issue_id"}}`;
an unsupported key returns `{"validation":{"class":"unknown_field"}}`.
Unknown keys, rejected values, serde text, paths, and complete field lists are
never echoed. This is a repair hint, not schema introspection.

---

## How each provider reaches the verbs

RSI is **hybrid** — not "shell everywhere":

### External CLI providers — Claude Code, Codex/Pioneer, Antigravity/Gemini

They shell out to the `rsi-rpc` CLI, which reads `$RSI_SESSION_TOKEN` from the
environment and attaches it out-of-band.

- **Discovery.** `rsi-rpc agent` (alias `rsi-rpc list-agent-verbs`) prints the
  control surface and exits 0. It advertises **only** the thirty-six `Agent*` verbs —
  each with a one-line description — plus the invocation form and the token
  convention. The generic RPC passthrough is never shown to an agent.
- **Request schemas.** `rsi-rpc <AgentVerb> --schema` prints one deterministic,
  versioned JSON envelope for that verb and exits without resolving a socket,
  reading the authority token, or contacting the daemon. Lookup is exact and
  case-sensitive and is closed to those same thirty-six verbs. The schema describes
  the supported request shape only: it omits caller and authority identities and
  does not replace daemon-side parsing, authorization, or runtime validation.
- **Invocation.** `rsi-rpc <Verb> [--params JSON]`. For multi-line spawn/wake
  payloads, prefer `--params @file` (a `@path` argument is read as a JSON file)
  over inline JSON — and never put the token in `--params`.
- **Codex CLI nudge.** Because Codex CLI takes its prompt via piped stdin and gets
  no launch system-prompt, a compact agent-discovery nudge (the same thirty-six verbs +
  token convention) is prepended to the process stdin on the **first turn only** —
  never on a resume turn, and never mutated into the stored user event.

### In-process providers — Harness (direct API) and CodexAppServer

The Harness `shell` tool scrubs `RSI_*` from the child environment, so the
shell → `rsi-rpc` token bridge cannot carry the token there. These providers get
**native in-process tools** instead:

- Both receive `rsi_control_spawn`, `rsi_control_reserve_successor`,
  `rsi_control_progress`, `rsi_control_send_message`, `rsi_control_status`,
  `rsi_control_halt`, `rsi_control_program_guard`, `rsi_control_create_issue`,
  the seven guarded Issue tools, and `rsi_control_manager_progress`,
  `rsi_control_manager_inbox`, `rsi_control_manager_send`,
  `rsi_control_manager_reply`, `rsi_control_manager_notify`, `rsi_control_manager_inspect`,
  `rsi_control_manager_update`, `rsi_control_manager_control`,
  `rsi_control_manager_prepare_control`,
  `rsi_control_manager_commit_prepared_control`,
  `rsi_control_manager_get_action`, and `rsi_control_manager_work_view`.
- Harness additionally receives `schedule_wake`, which can arm resume wakes and
  `mode:"on_terminal"` terminal watches under the same subject scoping, dedup,
  and per-master cap as `AgentScheduleWake`. CodexAppServer's dynamic roster has
  no generic `schedule_wake` tool.
- Native tools use the same request schemas as their corresponding catalog
  verbs. The unbound Harness `schedule_wake` form and the argument-free
  `rsi_control_program_guard` convenience are not additional catalog verbs;
  their distinct schemas remain native-only.
- Registration is uniform; execution carries the **same guarded authority** as
  the RPC verbs. Both route through one shared `AgentControlHandle`, so scoping
  logic lives in a single place.
- The caller session id is **bound at tool construction** and kept out of every
  tool's JSON schema — the agent can neither supply nor spoof it. Two enforcement
  layers: construction-binding *and* the shared guard.
- The tool set is **identical across a rotated session** (fresh and rotation paths
  build tools from one registration function).

---

## Authority-preserving master turnover

`AgentReserveSuccessor` and `rsi_control_reserve_successor` are the strong
turnover path. Call one while the current master still has a live token and is
the lead of its owning Epic. The strict request contains:

- `kind`, optional `model` and `effort`, and the successor `query`;
- optional `topology_node`, `iteration`, and `tags` metadata; and
- a stable `idempotency_key`.

It deliberately contains no caller, predecessor, Epic, parent, lead,
lead-generation, candidate, reservation, invocation, or token identity. The
daemon resolves the caller from the transport, derives the owning Epic, and
allocates a stable reservation and candidate. The receipt returns
`reservation_id`, `predecessor_session_id`, `epic_id`,
`candidate_session_id`, `kind`, `state`, `state_version`, `deduplicated`, and
an optional safe error class. States move forward through `reserved`,
`launching`, `committed`, `failed`, and `uncertain`.

The replay identity is the server-bound predecessor plus idempotency key. An
exact replay returns the same reservation/candidate and current receipt with
`deduplicated=true`; changed content under that key fails as an idempotency
conflict. Restart reconciliation uses those same stable identities and never
allocates a replacement candidate merely because the launch boundary is
uncertain.

The candidate is a direct child of the predecessor's owning Epic;
`continued_from` records predecessor lineage but never supplies hierarchy or
authority. The daemon waits for the predecessor to settle, persists launch
intent before provider effects, and confirms provider establishment before
attempting transfer. The final transaction requires both the expected
predecessor lead value and the frozen lead generation, then commits the new
Epic lead and `committed` receipt together. The predecessor remains the sole
authority through every pre-commit failure. A reservation, candidate row, or
provider process is not proof of turnover; only a committed receipt is.

After a committed program successor starts, its first RSI control action must
be `rsi_control_program_guard {}` or the exact `AgentScheduleWake`
`mode:"program_guard"` fallback described below, before it emits or continues
program output. Launching the successor never implicitly rearms program
identity.

---

## Resume-wake

`AgentScheduleWake` (and the native `schedule_wake` tool) schedule a future
callback that **resumes** the caller's session — keeping its context — rather than
starting fresh. This is the headline value of agent self-wake: a fresh session
loses the working context that made the wake worth scheduling. The resume target
is bound from the token server-side; the persistence columns already exist, so no
schema migration is involved.

### Fresh successors

`AgentScheduleWake` and the native `schedule_wake` tool may instead use
explicit `mode:"fresh"` for a one-shot root launch. The public payload is
unchanged: the daemon, after token resolution or native bound construction,
persists its private `agent_fresh` provenance marker and the required origin
link. Supplying an origin UUID in JSON cannot select that marker. Generic
operator-created Fresh jobs, legacy `fresh` rows with an origin, and unbound
native Fresh jobs remain generic scheduled Fresh work.

In Normal model-control mode, only the daemon-attributed Fresh successor is
allowed to use the narrow paid-background exception. It remains Background work:
`PauseBackground`, `DenyPaid`, `LocalOnly`, and `StopAll` still deny a remote
paid successor. E2 uses this only for a one-shot scheduled recovery; a job row
alone is not proof that a successor ran successfully.

Fresh successors enter the normal launch path as explicit `Standard` sessions,
so a project-level worker-kind default cannot enable worker retry behavior or
alter the successor contract. Their prospective authority binding is minted
only after model admission and before provider start. Confirmed pre-active
failures revoke that binding; when provider termination is unconfirmed, the
binding and model-admission reservation remain until recovery can establish a
terminal outcome.

Fresh/AgentFresh remains compatible best-effort scheduling, but it creates a
root session and transfers no hierarchy or Epic-lead authority. Use
`AgentReserveSuccessor` for master turnover. A consumed Fresh job or successful
process launch must not be reported as an authority handoff.

**Terminal watches (A8).** `mode:"on_terminal"` turns the same persisted-wake
machinery into a daemon-owned child-completion notify: "resume me when session
Y goes terminal (or parks on a question)". Delivery is DB-truth-based with bus
acceleration, coalesces multiple finished children into one wake, requeues
until a busy master goes idle, and chases rotation lineage. Full semantics,
security model, and the downgrade note: [docs/session-watches.md](session-watches.md).

### Program guard closure and rearm

`mode:"program_guard"` is program identity registration, not a continuation
wake. The RPC/native schedule payload accepts only `message` plus that mode;
the argument-free `rsi_control_program_guard` native tool is preferred when
available. The daemon binds the caller and derives one deterministic,
far-future, one-shot same-session Resume sentinel. Exact registration replay
returns that same job id.

Queue exhaustion and typed human gates close program identity by disabling the
valid sentinel. A later ordinary non-program turn is intentionally silent: no
wake, issue, bus event, tracing warning, or implicit rearm. Program output while
the guard remains closed fails closed once and tells the agent to explicitly
register again. Explicit program-guard registration is the sole rearm; it
restores the same deterministic row. A Scheduled Jobs TUI toggle or generic
scheduled-job update is not registration and cannot rearm a closed sentinel.

---

## Fallback: emit-and-detect directives

When native tools and `rsi-rpc` are both unavailable, a lead session can still
drive spawn/halt by **emitting a directive block** in its assistant text —
`<docregblock>/spawn_child …</docregblock>` or `<docregblock>/halt</docregblock>`
— which the daemon detects and enqueues. Prefer the native tools / `Agent*` verbs
when available; the directive path is the documented compatibility fallback and is
pinned by the daemon's directive-detection regression tests.
The `/spawn_child` header accepts optional `agent_role=<role>` and routes it
through the same normalizer. Because the fallback header is whitespace-tokenized,
use JSON-RPC or a native tool for roles containing spaces.

---

## Quick reference

```bash
# Discover the agent control surface (only the thirty-six Agent* verbs)
rsi-rpc agent

# Inspect one supported request shape offline (no socket or token required)
rsi-rpc AgentSpawnChild --schema

# Spawn a child (token comes from $RSI_SESSION_TOKEN, never --params)
rsi-rpc AgentSpawnChild --params @spawn.json     # spawn.json holds the payload + idempotency_key

# Reserve a same-Epic successor while this master still holds lead authority
rsi-rpc AgentReserveSuccessor --params @successor.json

# Example: a lead on any provider requests a Claude/Sonnet child.
# {"kind":"Task","provider":"Claude","model":"claude-sonnet-5","effort":"high","agent_role":"Implementer","query":"Implement the reviewed change.","idempotency_key":"impl-sonnet-1"}

# Read the full authorized cohort in one durable snapshot
rsi-rpc AgentGetProgress

# Enqueue durable mail to an authorized child
rsi-rpc AgentSendMessage --params @message.json

# Check status / halt (omit target to act on self)
rsi-rpc AgentGetStatus
rsi-rpc AgentHalt

# Schedule a resume-wake (wake target bound server-side from the token)
rsi-rpc AgentScheduleWake --params @wake.json

# Arm a terminal watch on a child (see docs/session-watches.md)
rsi-rpc AgentScheduleWake --params @watch.json   # {"mode":"on_terminal","watch_session_id":"<child-uuid>"}

# Register or explicitly rearm this session's deterministic program identity
rsi-rpc AgentScheduleWake --params @guard.json   # {"message":"master-orchestrate program guard","mode":"program_guard"}
```

Conventions: `GetSession` keys on **`session_id`** (not `id`); prefer
`--params @file` for multi-line payloads; the session token is transport-only via
`$RSI_SESSION_TOKEN`. Omit `provider` to inherit the caller's provider; set it
to launch a child on another backend. A cross-provider spawn with no `model`
uses the target provider's native default, not the caller's project model.

### Sandbox build custody

`CARGO_TARGET_DIR` for a sandboxed session names that session's authenticated
build scratch. Agents must never set, override, or unset it: `env -u
CARGO_TARGET_DIR cargo clean` can fall back to the global Cargo configuration
and delete a shared target outside the session sandbox (2026-09-23 #667). A
lead must never clean a child's build directory. A session cleaning up after
its own final commit runs plain `cargo clean` in its sandbox. For a leftover
child target, use exactly `cargo clean --target-dir <child sandbox_root>/target`.

---

## Adding an agent verb: checklist (#1116)

Two cloud sweeps went red on 2026-10-03 because verb additions landed with
focused gates that skipped these tests. Rosters and counts are now derived from
`agent_control_catalog_v1()`, so a new verb touches only the places below.
Include the listed tests in the focused gate:

1. **Descriptor**: add the `AgentControlVerbV1` variant and its descriptor
   (name, schema, `native_tool`) in `crates/rsi-common/src/agent_control_schema.rs`,
   plus the request-validation arm in that file's test helper. If the verb has
   no native tool it must join the RPC-only pin in
   `catalog_is_closed_ordered_unique_and_valid` and the Harness test
   `agent_bound_native_tool_schemas_match_the_common_catalog` (a deliberate
   review point: RPC-only verbs are listed by hand on purpose).
2. **Gate**: `AGENT_VERBS`, the `rsi-rpc agent` listing and `READ_VERBS` derive
   from the descriptor table through `rpc_verb_registry`; add nothing by hand.
   Add the RPC dispatch arm in `crates/rsid/src/rpc.rs`.
3. **Native tool** (when `native_tool` is set): register it in
   `crates/rsid/src/session/harness/tools/` and `crates/rsid/src/tool_registry.rs`.
   The Harness roster test
   (`rotation_and_fresh_share_identical_tool_set`) and the Codex registry count
   (`manager_codex_tools_bind_caller_and_redact_malformed_args`) derive their
   expectations from the catalog, so they fail if the registration is missing.
4. **Focused gate**: run at least
   `cargo test -p rsi-common agent_control_schema`,
   `cargo test -p rsid --lib -- agent_schema_catalog_matches_the_independent_authorization_allowlist rotation_and_fresh_share_identical_tool_set agent_bound_native_tool_schemas_match_the_common_catalog manager_codex_tools_bind_caller`.
5. **Docs**: list the verb in the catalog section above.

## Implementation map

| Concern | Code |
|---------|------|
| Token + identity constants | `crates/rsi-common/src/identity.rs`; `RpcRequest.session_token` in `crates/rsi-common/src/rpc.rs` |
| CLI transport + advertise | `crates/rsi-common/src/bin/rsi-rpc.rs` (`agent` / `list-agent-verbs` / `--schema`) |
| Attribution gate + `Agent*` handlers | `crates/rsid/src/rpc.rs` (closed `AGENT_VERBS` allowlist, gate before `Subscribe`) |
| Shared guarded authority | `crates/rsid/src/session/agent_verbs.rs` (`AgentControlHandle`) |
| Successor reservation/reconciliation | `crates/rsid/src/session/agent_verbs.rs`, `crates/rsid/src/session/launch.rs`, `crates/rsid/src/store/successor_reservations.rs` |
| Native in-process tools | `crates/rsid/src/session/harness/tools/rsi_control.rs`, `.../schedule_wake.rs`, `crates/rsid/src/tool_registry.rs` |
| Provider identity env + nudge | `crates/rsid/src/{codex,codex_app_server}.rs`, `crates/rsid/src/session/preamble.rs` |
| Directive fallback | `crates/rsid/src/session/{types.rs,monitor.rs,spawn_directive.rs}` |

See also: [`agent-sandbox.md`](agent-sandbox.md), [`agent-team-pipeline.md`](agent-team-pipeline.md).

---

## Idea controller semantic fence (D03)

The A6 token attributes a process; it does not by itself authorize Idea
mutation. D03 composes that transport attribution with a durable semantic
fence. A controller action succeeds only when the server-resolved Session ID
also matches the Idea's current controller Session, controller epoch, and
expected row version. The daemon binds those values internally. They are not
accepted from an agent payload.

D03 adds no agent verb, native tool, or generic RPC writer. Its controller
mutation allowlist contains only projection changes, stage transitions, and
exact self-release. All other Idea operations remain denied. At attributed
boundaries, missing Ideas, wrong projects, stale versions/epochs, reservation
state, and controller mismatch collapse to the same non-oracular denial before
any Store effect.

Tokens remain transport-only: raw values are never stored in Idea rows or
events, serialized into controller payloads, included in bus messages, or
logged/debug-formatted. A prospective launch witness holds a token only long
enough to prove that its candidate binding is still current at assignment.

Controller establishment follows these rules:

- A new UUID gains control only through a durable reservation and confirmed
  assignment. `continued_from`, rotation depth, hierarchy, or copied session
  metadata are not authority.
- Ordinary continuation and handoff-write resume use the same reconstruction
  sequence. The active map must first contain an installed live provider; the
  tracked incarnation must not have `interrupt_requested` set; the
  durable Session row must match its UUID, project, provider, and a live
  `Starting`/`Running`/`WaitingApproval` status; the newly reminted A6 binding
  must still be current; and the Idea must still assign that UUID at the same
  epoch. Only then does the Store-owned process-local grant registry receive
  the unchanged semantic grant. A failed same-ID establishment removes the
  grant and revokes the prospective A6 binding even if the provider has not yet
  reported its final exit.
- A same-ID provider change reconstructs no grant. Provider replacement uses a
  new UUID and transfer. In particular, a failed `CodexAppServer` launch that
  falls back to CLI `Codex` is an effective-provider replacement, not a
  same-ID resume: the replacement gets a new UUID and the next controller
  epoch.
- Rotation and controller retry successors reserve, confirm, assign, publish
  the committed Idea event ID, and only then revoke/archive the former
  controller. Any pre-commit failure releases the reservation and preserves the
  former controller and its token.
- New-ID assignment revalidates the currently installed provider and
  `interrupt_requested` while holding the active-session write guard across the
  Store -> A6 assignment commit. Synchronous providers require a live generic
  installed-handle witness; a fresh `CodexAppServer` requires both its completed
  initialize/thread-start handshake and the current live app-server process
  variant. A synchronous CLI `Codex` process cannot confirm an app-server row.
  This is the cancellation witness shared by
  `InterruptSession` and `AgentHalt`: if cancellation takes the guard first,
  the exact reservation is terminally released as `Cancelled`; if assignment
  takes it first, the durable transfer commits before the interrupt can report
  success. A captured confirmation alone is never sufficient.
- The semantic registry transfer happens only after durable assignment. Under
  the Store and A6 guards, it installs the successor grant and removes the
  former grant as one process-local operation. A failed assignment or cancelled
  race therefore cannot strand the former durable owner without its grant.
- Generic Fresh/child launches never inherit control.
- Terminal status alone never releases durable controller ownership.

After daemon restart, the Store-owned controller-grant registry begins empty.
Startup reconciliation also clears it before scanning durable controller tails,
releases pre-boot reservations with zero grace but retains committed
assignments. Such an assignment cannot act until same-ID re-establishment or a
new confirmed transfer restores both the A6 and semantic fences.

# ProgramRun authority composition

ProgramRun power is not part of the agent control surface. None of the
`Agent*` verbs or native `rsi_control` tools advertises
a ProgramRun read, mutation, cursor, controller epoch, lease generation, claim
generation, or caller identity field. A token-attributed call to any of the
eight operator ProgramRun RPCs is default-denied before Store access.

For daemon-internal controller work, authority composes four independent
witnesses: a live provider incarnation, the current D03 semantic grant, a live
A6 token binding for that same Session, and the durable ProgramRun controller
Session/epoch. Lock heartbeats add the manager-owned daemon boot UUID and lease
generation; outbox publication adds that boot UUID, claim generation, claimed
run version, and claimed lease generation. Every mutation revalidates the live
witnesses rather than trusting bind-time booleans. The global acquisition order is active
incarnation, Store, then A6 read. No ProgramRun method takes the A6 write lock
while holding Store.

Same-ID continue and handoff resume remint A6 and reestablish the existing D03
grant without changing durable ProgramRun generations. New-ID rotation, retry,
and effective-provider replacement use the existing confirmed controller
candidate path; the combined D03/ProgramRun assignment commits before old-token
revocation. Failure leaves the prior owner, token, leases, and action binding
intact.
