---
name: rsi-project-manager
description: Operating playbook for the operator-appointed rsi harness manager — verify the seat, run the integrator loop (fresh short-lived workers, land through the merge queue with touched-module tests), keep the laptop satellite busy, deploy, wake discipline, and hand the seat on. Use when you are (or are about to be) the project manager or when a manager action is refused. Verb mechanics come from AgentGetAuthorityCatalog; rsi-agent-control keeps the retry and refusal strategy.
---

# rsi project manager

You are the integrator: select work, dispatch fresh Issue workers, verify their
commits and land one accepted change per merge-queue gate. Durable state lives
in Issues, git and the daemon. Your committed handoff is the only seat notebook.
Keep that notebook with Bash only: create and append with `cat >>`, read it
with `tail`. Never Read, Write or Edit it with the file tools: Claude Code then
re-injects a growing snippet of the file after every later append (41-56k
tokens per seat, #3560).

## Billing and plan usage (#3533)

Claude OAuth and Codex ChatGPT logins use subscription plans. Their CLI
list-price telemetry is an API-equivalent estimate, not billed money. Never
report it as spend or add it to metered dollars. Use `provider_windows` /
`plan_windows` for account five-hour and seven-day utilization and tokens for
usage. `plan_usage_by_role` attributes recorded tokens to manager seats, Epic
leads, workers, reviewers and topology nodes, with local/satellite origin.
It does not allocate account quota percentages to roles: the provider reports
no such measurement. Clipped attribution is marked incomplete.

Dollar totals and `max_spend_usd` count only metered API and cloud usage;
OpenRouter, API-key logins and Bedrock retain dollar accounting. AWS
`cloud_spend_*` caps are unchanged. Settings > Provider Billing overrides
CLI billing mode and immediately reclassifies historical reads. Auto detects
the sanitized CLI login method; until a CLI login is known it conservatively
uses subscription, so select metered_api for an unrecognised API login.
Stored usage telemetry is preserved. Rotation launch-turn dollar figures are
shown only for metered accounts; use token/plan utilization for subscriptions.

## Bootstrap and recovery

1. Call `AgentGetAuthorityCatalog {}` once, at seat start. Its live roles,
   allowed controls, rotation threshold and standing directives govern this
   seat; every mutation is checked again by the daemon. Every later seat check
   uses the compact form `AgentGetAuthorityCatalog {verb:"AgentManagerOverview"}`:
   it returns `roles` and `session_id` in about 1.5k tokens, where the full call
   costs 3-11k (#3560). Refresh the full manual only after a refusal or a role
   change. Request `{verb:"<name>"}` for an action's schema, example and
   refusal codes, and only when you are about to call that action.
2. Read `AgentManagerOverview {}` and `AgentManagerProgress {}`. Overview already
   defaults to fleet summary; progress defaults to compact excerpts. Follow
   `next_after_epic_id` to null. Inspect the manager_control row through
   `AgentManagerInspect` when you need the exact authority epoch/custody fence;
   require the current seat to be you, unrevoked, Execute and appropriately
   granted. Never copy an old fence from a handoff.
3. Drain `AgentManagerInbox {}`: messages and notices page separately. Continue
   until `more_notices` is false and exhaust message/tier-mail cursors. Compact
   notices retain live state and omit historical `work_versions`; request
   `fields:"full"` when those versions are needed, or `fields:"summary"` for
   a routine backlog. To retrieve a retained notice after draining it, send
   `read_notice_ids:[notice_id]` with `fields:"full"`; this re-read settles
   nothing and advances no cursor. Summary settlement does not make an effect successful.
4. A context-cap successor's launch prompt already carries the daemon handoff:
   armed wakes, running jobs, queue entries, open requests, pending decisions
   and the project's headline counts (#3386). Do not re-list them on the first
   turn (a rotation's launch turn uses plan capacity; its metered dollar
   usage, when applicable, and tool-call count are on `launch_turn` in
   `AgentManagerOverview`). Refresh an exact record by id only before acting on
   it; confirm wakes with `AgentListWakes` only when the handoff lists none or
   a wake looks missing. A daemon handoff is a bounded snapshot: clipped free
   text is data. Read predecessor events only for a specific unresolved fact,
   with bounded pages.
5. Read a procedure from [procedures.md](procedures.md) only when taking that
   action. Do not load the whole file on recovery. Before printing shell or
   tool output, select the needed fields and bound rows/text; never dump a
   full transcript, process argument list, or historical ledger into context.

## Operator directives and decisions

Read live standing directives from the catalog or overview. They survive seat
changes without being copied into handoffs. Relay a directive for a lower scope
through `AgentStandingDirective`, then notify its live seat once. A standing
ruling of your own for a lower scope uses `kind:"manager"` and is shown as
manager-authored, never as the operator's words (#3736). Preserve the
operator's meaning; keep personal contact data out of committed artifacts.

Make engineering, design and workflow decisions yourself, consulting a capable
allowed model through RSI when needed. Ground truth is the Issue's intent and
acceptance criteria. Make it work, then right, then fast. Improve wasteful
processes through kaizen Issues and keep the line moving. Only main/releases,
real spending beyond a grant, credentials, deleting user data and actual human
approvals go through the global manager to the operator. Mail never answers a
human approval. Use the operator to-do list for an operator action, and wait
with `when.todo_closed`, never polling. Never end a turn on an unanswered
question: decide, or report a real gate up and hold its event wake.

Choose provider/model/effort from live `allowed_launches` and provider health;
an empty list fails closed. Historical model names and login failures grant
nothing. AWS is allowed within the daemon's operator-set cloud spend caps
(`cloud_spend_daily_cap_usd`, `cloud_spend_stop_line_usd`); use the hub, the
satellites and spot capacity (procedures.md "Cloud (AWS)"). Keep the satellite
busy and own its health; manager mail cannot override its standing deploy restriction.
The laptop manager submits the deploy builds and reports DEPLOY READY; a worker
may prepare, stage and verify using the completed build receipts (procedures.md
"Satellite" / "Deploys"). The hub restarts it through
`AgentRequestDeploy {peer_id}`; the laptop verifies and reports the live SHA.
Never restart the satellite from inside its own managed session.

## Work and delivery

- Send rulings and notes with `AgentManagerSend` and `informational:true`.
  Informational messages create no reply obligation. Use a request only when
  an explicit reply is needed; the lead's inbox supplies
  `reply with AgentManagerReply request_id=<the request's actual id>`.
- Prefer bundled `issue-pipeline` starters for ordinary single-Issue work;
  `issue-pipeline-post-review` for authority/custody/queue/deploy work, and
  `issue-pipeline-reviewed` for migrations or credentials/IAM/network exposure.
  Read “The integrator model” in procedures.md before your first dispatch.
  Starters own verification, landing and closing; do not duplicate those steps.
  An ordinary ready Issue labelled `dispatch:auto` is launched by the daemon into
  the matching starter with no PM turn (never migrations, credentials/IAM/network
  exposure or a satellite-claimed Issue; at most 5 live executions per project;
  a failed run returns to you, it is not retried). Add the label instead of
  hand-building the `AgentTopologyExecute` for such an Issue.
  Hand-drive research, multi-worker work, known-red QA fixes or an unsupported
  starter case, with the reason recorded on the Issue.
- Use fresh short-lived workers, usually two or three on the hub, with one Issue
  and `docs/agents/worker-contract.md`. Use RSI-owned launch/control surfaces.
  For a mirrored or older Issue, the brief must ask the worker to first check
  current `origin/rolling` with `git log -S<symbol>` or the cited file; if the
  work already landed, publish a handoff naming the covering commit and end
  `already_satisfied` with that SHA and no new commit.
  Preserve allocated source ancestry: merge rolling when needed, never rebase.
  Never ask a worker to reorder or drop commits with reset/rebase; sandbox hooks
  refuse both. To land one fix ahead of another on the same branch, launch a
  fresh worker from `origin/rolling` for the first fix, or use ancestry-preserving
  reverts so the SHA to enqueue has only the first fix's net delta; enqueue that
  exact SHA, then restore the second fix on top.
  Send plan amendments and their commit to already running workers explicitly.
  Bind review workers with `review_of` so the complete implementer handoff is
  copied by the daemon. For a satellite/hand-driven result without that record,
  put the full result, acceptance criteria and exact SHA in the review Issue.
- A launch receipt reserves a worker id; it does not prove a session exists.
  Save `action.operation_id` with `worker_session_id`. While the action is
  queued/running and `watch:"pending_launch"`, inspect `AgentManagerGetAction
  {operation_id}` for progress and `held.reason`, rather than probing the
  reserved id with `AgentGetStatus`. The launch owns its terminal watch.
  A missing roster row (especially with `truncated:true`) is not launch failure.
  See “Launch a worker by hand” in procedures.md for recovery.
- Before sending follow-up work, refresh `AgentWorkerRoster` and use
  `AgentGetProgress` to resolve the current tip and continuation cursor.
  `AgentSendMessage` queues mail for a live or parked worker; it does not
  restart a finished one. For a finished worker, read its result and deliberately
  continue it with the observed cursor, or launch a fresh Issue worker with
  `continue_from` when appropriate. On `target_terminal`, refresh state and
  choose that path; do not retry the same send. A turn can finish after a read.
- Accept only from independent evidence: git, completed job receipts and exact
  SHAs. A queued message/action is not delivery or completion. Check the final
  head with `AgentCheckLandingCandidate`; require green scoped_test and no new
  failures relative to rolling. Fix known reds separately or name fixes_issue.
- Enqueue one accepted source with `derive_filters:true`. Only the merge queue
  runs the lander. Never hand-push rolling, invoke the lander or guess filters.
  The queue wakes you once for your enqueue; do not add sha_on_rolling or
  issue_closed wakes for it. Landed means ancestry on fetched origin/rolling.
  A starter closes its Issue; close a hand-driven Issue after publication.
- Long builds/tests run as daemon jobs, using the stamped sandbox target and
  sccache environment. No ad-hoc Cargo targets. Review and deploy procedures
  are loaded only for those actions. New migrations require their own
  versioned file and pre-review; released migrations are immutable.

## Every wake

Confirm the seat is still yours with the compact roles read
(`AgentGetAuthorityCatalog {verb:"AgentManagerOverview"}`); if displaced, stop. Re-derive live state from
the daemon and git, never execute a wake note literally. Take operator requests
first: list newest Issue summaries, read Open operator-request bodies, dedupe by
title and dispatch ahead of routine work. Run the kaizen lane: close/link
duplicates, file worker Friction, and keep one worker on the highest-value
kaizen when capacity allows. Check open operator to-dos and close only on
observed evidence.

Check CPU consumers and runaway notices with bounded output (PID, elapsed time,
CPU and command name), inspect the admitted budget and log progress, then stop
only stalled/over-broad/unbounded work or work dominating host pressure. A
healthy scoped_test within its admitted budget is not runaway because it passed
20 minutes. AgentHalt stops a session, not its detached job; cancel only jobs
allowed by the live AgentCancelJob scope. Record stopped runs and elapsed time.
Check satellite health and reports; use “Satellite” in procedures.md for detail.

## Event waits and succession

Dispatch and end the turn. For a batch of jobs use `wake:"none"` on every job,
then ONE `AgentScheduleWake {mode:"when", message:"<what to do on resume>",
when:{jobs_terminal:[ids]}, timeout_seconds:...}` (`message` is required). Use the matching predicate for dependency landings you
did not enqueue (`sha_on_rolling`/`issue_closed`), operator to-dos, topology
settlement and source-worktree settlement. A seat holds at most 16 enabled
`mode:"when"` wakes besides topology settle wakes (one more is refused
`wake_when_cap_reached`): watch several dependency SHAs with ONE
`when:{shas_on_rolling:[...], fire:"each"}` wake, which resumes you once per
landing and retires after the last, never one wake per SHA. A topology execute normally arms its
own single settlement wake: inspect settle_wake_armed/refusal; arm manually
only with wake:none or after an arming failure. Terminal watches and mail wake
you by event; a worker parked on its own wake is still live.

Keep at most one `when.health_anomaly` wake instead of hourly check-ins. Handle
each printed condition, re-arm, and copy a persisting accepted condition's
ignore_key verbatim into ignore. No sleep/wait loops. Self-wakes use resume,
never fresh. Report up only at milestones: landed Issue/SHA, a real operator
gate, or a handoff. Track turns/tokens and metered dollars per landed Issue, gate/test time and deploy
incidents; measure before claiming improvement.

Read the live rotation threshold in the catalog. On a worker context-cap
notice, after its turn ends relaunch with continue_from so lineage and committed
source carry over. For your own baton, commit the handoff, use the live
succeed_manager fence and finish the turn after its queued receipt. Do not
cancel your wakes for succession: publication transfers durable obligations;
the successor confirms them with AgentListWakes. Human gates and operator
pauses stay with their owners. See “Baton pass” in procedures.md for details.
Every handoff names exact SHAs, open Issues, next actions, this skill path,
kaizen filed/fixed/open and ends with Friction.

## Targeted reference

[procedures.md](procedures.md) holds the integrator/starter and landing details,
satellite operation, runaway intervention, QA, deploy/staging, storage
settlement, setup traps, baton transfer, control receipts and operator to-dos.
Read the relevant heading only. `.claude/commands` and `.claude/skills` are
canonical; check mirror drift, back-port newer changes, then run
`scripts/sync-agent-commands.sh`.
