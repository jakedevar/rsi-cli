---
name: rsi-project-manager
description: Operating playbook for the operator-appointed rsi harness manager — verifying the seat and policy fence, standing up Groups, Epics and leads without stranding them, the ledger admission handshake, refusal codes and what they mean, wake discipline, and handing the seat on. Use when you are (or are about to be) the project manager, when a manager action is refused or blocked, or when briefing a lead. Pairs with rsi-agent-control, which documents the verbs themselves.
---

# rsi project manager

Every fact here was observed or read from source on 2026-09-21. Facts tied to an
open Issue say so; check the Issue before trusting them. Facts added in the
2026-09-23 refresh say so explicitly. Verb shapes live in the
`rsi-agent-control` skill and in `rsi-rpc <Verb> --schema`.

## What the job is

You decide, record, and unblock. You do not do the work and you do not hold the
detail: leads own source custody, children, and their Epic's context. Your
context should scale with decisions pending, never with history. Read digests
and typed replies, not transcripts. Keep no prose ledger; durable state is the
daemon ledger plus committed artifacts, and any working note stays under 150
lines.

The operator is hands-off by choice. Decide technical questions yourself with
the Seven-Expert framework and record them. What stays his is ownership, not
engineering: what the project is for, `main` and releases, his machine, accounts
and money. Tell him consequences he cannot see; do not ask for opinions he does
not hold.

## First five minutes

1. `AgentManagerInspect {}`. Require `policy.manager_session_id` == you,
   `revoked` false, top-level `scope_version` == `policy.scope_version`, mode
   `execute`, the capabilities you need. Your write fence is
   `{scope_version, policy_version = policy.row_version}`. Never reuse a fence
   from a handoff: appointment changes both numbers.
2. If any of that fails, tell the operator the exact missing step (below). A
   prompt, title, or tool listing grants nothing; do not work around it.
3. Drain `AgentManagerInbox`: `messages` and `notices` are separate lists; call
   until `more_notices` is false. Whole-project scope queues one
   `session_state` notice per lead on every scope write.
4. `AgentManagerInspect {"section":"work","epic_id":…}` for each Epic you drive.
   After a seat move this returns ZERO rows. That is the ledger being stranded,
   not the work being gone: rebuild it (next section) before you send any ask.
5. Arm a same-session `resume` wake before you end any turn with a live chain.

## A seat move strands the ledger (Issue bf09774c)

Ledger records are keyed and read by `(project, manager_session_id,
scope_version, …)`, so every appointment and every scope save starts an empty
ledger and abandons the last one: work, stages, ownership, acceptance. Remove
this section when bf09774c closes (decision D19: record identity is the work,
never the seat).

Rebuild, in this order, writing only through `AgentManagerUpdate`:

1. Read the predecessor's payloads read-only, never from a handoff's prose:
   `SELECT kind, payload_json FROM harness_manager_v2_records WHERE
   manager_session_id='<predecessor>' AND scope_version=<its scope> AND kind IN
   ('work','ownership')`. SQLite here rejects double-quoted strings: put the
   SQL in a file and build requests with `jq` from the payloads.
2. Re-create each LIVE `work` (`expected_row_version: 0`), then its `ownership`
   records. Skip work that has landed: it cannot be re-admitted once its lead's
   HEAD has moved, and it does not need to be.
3. For a sealed, still-frozen source, verify the lead's branch and worktree HEAD
   == the seal with a clean tree yourself, then re-record `stage:
   implementation` with the same evidence.
4. Re-read Inspect `work`. `evidence_policy_digest` binds epic, key, revision,
   gates, source commit, source session and head, NOT the seat
   (`store/manager_ledger.rs` `policy_digest_projection`), so it must reproduce
   byte for byte. If it does, nothing issued to a lead or a running reviewer
   changed. If it does not, stop: something moved.

What does not survive: an `accept`. Hold the seat steady between `accept` and
landing, and never ask the operator for a scope change, including a narrowing,
while any handshake is live.

## Operator setup, and its traps

Correct order: `:manager appoint` once, then `:manager policy`, set the preset
row to Execute or Full project control, confirm the capability rows, `s`. Then
never touch appoint again.

- One manager seat per project (`docs/harness-manager.md`, Bounds). Appointing
  you displaces the previous manager and revokes its grants and mail.
- ANY appoint or scope save, even identical, bumps the scope version and revokes
  the saved policy and all in-flight manager mail (Issue eac83cfd,
  `crates/rsid/src/store/harness_manager.rs` `configure_harness_manager`).
- A policy save does not say what it granted. Enter/Space cycles the preset row,
  so a save can land as Observe: mode `monitor`, zero capabilities. Verify with
  step 1 rather than trusting "I saved it".
- `allowed_launches` empty means the daemon restricts nothing. An operator model
  restriction then holds only because agents honor it: carry it in every lead
  contract.

2026-09-23 facts:

- **Authority survives manager rotation** (2026-09-23;
  `manager_config_for_caller` follows `config.current_session_id` and tests
  rotation transfer in `crates/rsid/src/store/harness_manager.rs`): the rotated
  session with `continued_from` equal to the seat passes Inspect and writes;
  `policy.manager_session_id` stays the logical seat ID. Key review-assignment
  queries on that logical ID (`crates/rsid/src/store/manager_reviews.rs`), not
  `$RSI_SESSION_ID`.
- **Record limits are per bookkeeping class** (2026-09-23 #614;
  `harness_manager_v2.rs` `bookkeeping_class`): retrieval, resource
  (`resource_spend`/`resource_launch_origin`), and lifecycle each have their own
  1024-record limit and typed refusal. Do not treat them as one shared total.
- A standing request permits at most **32 replies**; send one tagged reply per
  event.
- A source session archived before its DB-native review verdict is refused as
  review evidence. Rotation is allowed; register the rotation tip that holds
  the same sandbox.
- A commit in a sealed source sandbox after `SEALED`—including a rotation
  handoff commit—voids the seal.
- Integration records fail on stale local `rolling` (#562) and archived source
  sessions (#601); verify the landed remote target and a live source instead.
- An old Epic with uncertain actions or an unknown program owner is untakeable
  until ownership is reconciled (#380/#390).
- Reviewers choose `z-ai/glm-5.3-flashx` before
  `deepseek/deepseek-v4.1-flash` when using OpenRouter, and end with
  `cargo clean` in their sandbox after the receipt.
- A seat move or scope/policy save still starts non-work bookkeeping records
  fresh and invalidates active DB-native review assignments
  (`manager_review_allocation_invalidated`); reviewers can keep running until
  their Epic lead halts them (#614).
- **DB-native acceptance is automatic** (2026-09-23;
  `manager_v2_accepted_source` derives `Acceptance` from an eligible accepted
  receipt, and Inspect projects `source_accepted`): an accepted receipt marks
  the work accepted; a later manual accept then fails
  `manager_v2_record_changed`.
- **Detect a silently dead lead after daemon restart** (#648; 2026-09-23,
  `crates/rsid/src/session/mod.rs`): the watch delivery is abandoned as "was
  never consumed: no provider output", the lead stays `Completed` and assigned,
  and manager mail does not wake it. Replace it with `create_session` →
  `assign_lead` after the candidate's first turn ends → `resume_lead`.
  Since 2026-09-23 (K10d) the abandonment also reaches you as a
  `delivery_abandoned` `session_state` notice and Health `lead_delivery_abandoned`.
  A `Failed` or `Interrupted` lead, or a `Completed` one that `resume_lead`
  refuses `manager_v2_resume_unavailable`, can instead be retried in place with
  `retry_lead` (K13). An authorized lead change readdresses your open requests to
  the new lead (K10c); answer and read the new request IDs.
- A lead that ends "paused" with a handoff but no wake is never resumed; manager
  mail reaches it only because the manager sender's delivery watch fires. Every
  lead contract requires a same-session resume wake of at most 600 s.
- **Idempotency conflict** (2026-09-23; `manager_idempotency_conflict` in
  `crates/rsid/src/store/harness_manager.rs`): reuse with different content
  fails, including keys used by a previous seat.
- **Local-only integration targets**: repositories without `origin` need the
  repository-level `rsi.managerIntegrationTarget` opt-in; see
  [Local-only integration targets](../../docs/harness-manager.md#local-only-integration-targets-rsimanagerintegrationtarget)
  (#644).
- **Machine is disk/I/O bound** (2026-09-23 #647): cap concurrent code reviews;
  tell design reviewers not to build; reclaim non-live `target/` dirs while the
  filesystem has below 80 GB free (reclaim threshold evidence:
  `docs/sandbox-storage.md`, sandbox build-cache pressure).

## Standing up work

Containers: `AgentManagerControl` `create_container` (Group at root, Epic under
a Group). Sessions and leads: prefer `AgentManagerPrepareControl` then
`AgentManagerCommitPreparedControl`; the daemon resolves fences for you.

Creating a lead is two non-atomic steps (Issue 89a39100). The verified sequence:

1. Prepare + commit `create_session` (parent = the Epic).
2. Read `AgentManagerGetAction` until the receipt is `succeeded` /
   `session_established`. Assigning earlier fails
   `manager_v2_candidate_unavailable`.
3. Prepare + commit `assign_lead` immediately; expect `lead_committed`.
4. If the session ended its first turn waiting, prepare + commit `resume_lead`;
   expect `lead_resumed`.

The lead's prompt must open with an authority check and must not enter program
mode, register a guard, or arm a wake until it passes
(`rsi-rpc AgentListIssues --params '{"archive":"Active","limit":1}'` succeeds
only for a committed lead). A session that enters program mode before assignment
becomes unassignable: running it blocks `manager_v2_program_evidence_unknown`,
idle with its own wake it blocks `manager_v2_human_or_recovery_owner`. The cure
is a replacement lead, which can `AgentHalt` the orphan as a child of its Epic;
with `SessionControl` you can halt the orphan leaf yourself (2026-09-23 K1).

### Lead contract checklist

Authority check first · intent by reference (`git show <ref>:<path>`, never a
paraphrase; a new sandbox is cut from `rolling` and cannot see other branches'
working trees) · obligations in order · operator constraints verbatim, including
permitted child launches · one mutating owner for hot shared files (`rpc.rs`,
verb catalogs, `harness_manager_v2.rs`, `store/mod.rs`, `AGENTS.md`) · migration
numbers AND decision numbers are proposed to you, not self-reserved (a lead
once labelled its proposal with the number you had just issued) · Tier-2 needs a reviewer of a
different model family than the author · typed short reports with an exact reply
format · working notes under 150 lines · file an Issue labeled
`human-touch-cause` before needing a human.

## Ledger admission handshake (current ledger)

Source: `crates/rsid/src/session/manager_ledger.rs` (`observe_manager_update`,
`observe_independent_evidence`, `validate_work_bundle`) and
`crates/rsid/src/store/manager_ledger.rs` (`policy_digest`, `scope_label`). This
ceremony is scheduled for replacement; until then, follow it exactly, and read
the consumer contract BEFORE launching a reviewer.

- You must never be a ledger source. Admission requires the source session's
  custody HEAD == the sealed commit with a clean tree, and a manager keeps
  committing. Leads hold custody and FREEZE from seal to admission; any commit,
  even a handoff, voids it.
- `work` of kind `product` must gate implementation + review + verification
  (`manager_v2_product_gates_required`).
- Sequence: lead seals and replies `sealed_source_sha`, `source_session_id` →
  you record `stage: implementation` with evidence source=S, session=lead,
  artifact=S:`<a regular tracked file UNDER thoughts/ present in S>` (anything
  else is refused `manager_v2_invalid_artifact_path`,
  `session/manager_ledger/git.rs` `artifact_path`; if the lead named a source
  file, pick a pre-existing `thoughts/` file yourself rather than unfreeze it; a
  non-closure artifact leaves evidence `unknown` but binds the source) →
  Inspect `work` now projects
  `evidence_policy_digest`, `review_scope`, `required_evidence_linkage` → send
  those to the lead → the lead launches the Closure reviewer at S.
- Reviewer: a Completed child of the work's Epic, not the lead, not you, custody
  allocated at S, one two-file evidence commit (review JSON + manifest v2) whose
  sole parent is S, strict `PIPELINE HANDOFF — REVIEW:` final message, manifest
  covering every `required_evidence_linkage` key, verdict Accepted with zero
  unresolved findings.
- A failing bundle cannot be attached (`manager_v2_evidence_not_passed`): record
  `stage: review, state: failed` with the evidence commit in the note. A fix
  unfreezes the source, so the handshake repeats for the new seal.
- Never soften a verdict to fit admission. Findings go to one mutating owner,
  then one finding-focused delta review.
- Verify the bundle YOURSELF before admitting, including the one gate that is
  not in git: the daemon parses the reviewer's LAST non-empty assistant message
  and its first non-blank line must be the marker
  (`rsi-common/src/agent_contract.rs` `parse_closure_review_handoff_v1`). One
  courtesy sentence before it voided a genuine accepted review
  (`manager_v2_review_handoff_invalid`); the only remedy is a fresh reviewer.
  Put this in every reviewer prompt: "The FIRST characters of your final
  message are PIPELINE HANDOFF — REVIEW: with nothing before them. No code
  fences anywhere in it. Write the exact final message to a scratch file
  outside the evidence worktree, run rsi-contract-validate on it, and emit that
  text unchanged."
- Admit in this order, taking each `expected_row_version` from the previous
  reply: `stage review` → `stage implementation` → `stage verification` (all
  `passed`, all with the evidence commit as artifact) → `accept`. Stop at the
  first refusal. An inadmissible-but-genuine review is recorded `stage: review,
  state: blocked` with NO evidence, never `failed`.
- After `accept` the LEAD builds the landing candidate (merges `origin/rolling`
  into its branch) and runs the guard; a lander only verifies and pushes. An
  ownership `key` is the work key; its record key is derived.
- Before landing a changed registered hot file, the accepted Work needs an
  active exclusive ownership claim with `domain` equal to that exact
  repository-relative path and `files` containing the same path. Claim each
  hot file separately; a category domain such as `rpc-catalog` does not pass
  `rsi-rolling-land` for `crates/rsid/src/rpc.rs`.

## Control-surface facts

- `AgentManagerCommitPreparedControl` returns `result.receipt.*`;
  `AgentManagerControl` and `AgentManagerGetAction` return the receipt at
  `result.*`. `queued` is not done: read the receipt. `AgentManagerGetAction`
  takes `{"operation_id": …}`. Error envelopes carry `code`, not `message`: a
  poll loop that prints `.error.message` hides every failure as `null`.
- Since 2026-09-23 (K10a) a retained notice wakes an idle recipient
  (`Completed`/`Interrupted`, no active or queued turn, no pending question)
  without waiting for the sender's turn: your mail wakes an idle lead, and a
  lead's reply wakes you once you are idle. Busy recipients are never
  interrupted; notices coalesce until `AgentManagerInbox` retrieval settles them.
  Failed, archived, question-gated and running recipients are not woken.
- Lost wake (Issue f94f23ab): a lead's child watch can fire, retire as
  "confirmed consumed", and resume nobody when `rsid::reconciliation` races the
  end of its turn. Symptom: lead `Completed`, child terminal, no event after the
  lead's last message, its only enabled job the year-9999 program sentinel.
  `resume_lead` is then blocked `human_or_recovery_owner`; mail it and end your
  turn. Require every lead to arm a resume wake of at most 600 s IN ADDITION to
  a child watch. Check for this on every wake.
- `scripts/sync-agent-commands.sh --check` exits 1 on a clean `rolling`
  checkout (19 `missing: .gemini/...` lines) until Factory Hygiene obligation 3
  lands. Tell landers the exact expectation, never "expect pass".
- Replaying an identical request with the same idempotency key returns the
  original receipt, including a terminal `blocked`. Retry after conditions
  change with a NEW key; never change the key on an uncertain result.
- `resume_lead` on a program-mode lead is blocked `program_evidence`. Correct:
  that lead owns its continuation and wakes itself. Send mail; it will read it.
- `AgentGetStatus`, named-ID `AgentGetProgress` and terminal watches on a
  scoped lead or worker need only the live scope. `AgentHalt`,
  `AgentContinueChild` and `AgentSendMessage` on a scoped leaf also need
  `SessionControl` in Execute mode, no pause, and no human gate on the target
  (2026-09-23 K1; refusals `manager_v2_capability_denied`, `manager_v2_paused`,
  `manager_v2_human_or_recovery_owner`). Inspect `workers`, `resources` and
  `health` stay the fleet view; read-only SQLite against `~/.rsi/rsi.db` is
  legitimate for diagnostics only.
- `AgentManagerSend` to an Epic without a committed lead fails
  `manager_lead_missing`. A send receipt means queued, never accepted.
- `succeed_manager` has no effect-free preflight and needs the Overview
  `manager_control` observation. That is a ROW, not a top-level field, appended
  after every `intent` and `lead_control` row, so it is on the LAST Overview
  page (page 3 of 3 at whole-project scope): follow `next_cursor` to the end and
  select `type == "manager_control"` for `expected.authority_epoch` and
  `expected.custody_generation`. It exists only for a parentless Standard
  manager (`store/manager_ledger/inspect.rs`). An earlier note here said the
  baton pass "cannot be built" because the field was null; that was a page-1
  read. Succession carries the same logical manager id and scope version
  forward (`store/manager_successions.rs`), so it is the one handover that does
  NOT strand the ledger. What actually blocks it: Issues 355 (replies lost
  after rotation), 396 (successor context without authority), 398 (recovery
  livelock), 413 (self-succession loop), all open P1 on 2026-09-21. Do not use
  it for an unattended handover until they close. Until then hand on manually:
  land everything accepted, commit a handoff, give the operator a short launch
  prompt that reads it by ref, and have the successor rebuild the ledger.
- Project-wide bulk archive/restore (K14, #672) needs `OperatorDelegation` in
  Execute mode, not paused. Both directions loop `operator_call` `ListSessions`
  — a project-bound keyset page over `(updated_at, id)`, ≤64 rows/≤12 KiB,
  follow `next_after` until absent — but the page filter and the per-row skip
  differ, because `archive_blocker` is an `ArchiveSession`-only signal:
  - **Bulk archive**: page with `status_in: ["Completed","Failed","Interrupted"]`,
    skip any row whose page `archive_blocker` is non-null, then `operator_call`
    `ArchiveSession` on the rest with the row's observed `session_updated_at`
    as the `OperatorCallFenceV1`.
  - **Bulk restore**: page with `status_in: ["Archived"]`. Do NOT filter on
    `archive_blocker` here — `delegated_archive_blocker`'s own first check
    refuses every `Archived`/`Deleted` row `manager_v2_session_state_changed`
    (`crates/rsid/src/store/manager_actions.rs:578-580`), so an
    `archive_blocker`-based skip discards every restore candidate and never
    calls `UnarchiveSession`. Skip only rows whose `sandbox_cleanup_state ==
    "Purged"` (a cheap, always-true predictor of
    `manager_v2_historical_restore_refused`; see
    `historical_session_restore_blocked_on`,
    `crates/rsid/src/store/sessions.rs:31-49`), then `operator_call`
    `UnarchiveSession` on the rest with its `OperatorCallFenceV1`, tolerating
    a `manager_v2_historical_restore_refused` refusal for a row whose
    source-worktree settlement history blocks it — that half of the gate is
    not visible on the page.
  For both directions: `ArchiveSession`/`UnarchiveSession` rewrite the target's
  `updated_at`, which can move it past an in-progress cursor, but once its
  status changes it stops matching that direction's `status_in` filter and
  cannot reappear on a later page of the SAME walk. Still treat a duplicate id
  as a no-op: re-`ArchiveSession`-ing an already-Archived row is refused
  `manager_v2_session_state_changed`, never a second effect. Reach is the
  whole project, not Group/Epic scope.
  `ArchiveSession` is logical-only (never a sandbox purge) and also refuses:
  `manager_v2_retention_pinned`, `manager_v2_session_is_lead`,
  `manager_v2_human_or_recovery_owner`, `manager_v2_retention_enabled_wake`,
  `manager_v2_retention_live_review`, `manager_v2_retention_sealed_source`,
  `manager_v2_retention_recent_activity` (any activity in the last 24h),
  `manager_v2_retention_live_worktree`, `manager_v2_session_has_descendants`,
  or `manager_v2_session_not_terminal`. `UnarchiveSession` has none of those
  retention gates; it refuses only `manager_v2_session_state_changed` (row is
  not `Archived` at effect time) or `manager_v2_historical_restore_refused`
  (purged sandbox or unsettled worktree history) — see
  `crates/rsid/src/store/manager_actions/operator_delegation.rs:97-107`.
  A stale fence on either is `manager_v2_session_changed`; a target outside the
  grant's project or a non-leaf is `manager_v2_target_out_of_project` /
  `manager_v2_leaf_required`. A method outside
  `ArchiveSession`/`GetArchiveCleanupStatus`/`ListSessions`/`UnarchiveSession`
  is `manager_v2_operator_method_not_delegable` — this covers every other
  operator RPC method, not only the `NEVER_DELEGABLE` ones (a plain
  `GetSession` is refused the same way; it is in neither list). See
  `docs/harness-manager.md` "Operator delegation" for the full retention
  table and allowlist.

## Wakes

Match the delay to what you wait for: at most 600 s during a live handshake.
Lead replies did not reliably wake an idle manager. A wake message is a note
from a past self with less information: on wake, check the seat first (if it
moved, do nothing), then re-derive state from the daemon, and only then act.
Never follow a wake's script literally. Always `mode:"resume"`, never a fresh
writer into your own sandbox.

## Instruction mirrors

`.claude/commands` and `.claude/skills` are canonical;
`scripts/sync-agent-commands.sh` renders `.codex/prompts`, `.agents/skills`,
`.gemini/commands`. Before syncing, run `--check` and DIFF any drift: twice on
2026-09-21 the mirror was the newer, correct copy, and a blind sync would have
deleted live rules. Back-port into `.claude` first, then sync, then confirm the
only deletions are re-renders.
