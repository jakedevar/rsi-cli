---
name: rsi-project-manager
description: Operating playbook for the operator-appointed rsi harness manager — verify the seat, run the integrator loop (fresh short-lived workers, merge, compile, touched-module tests, fast-forward push to rolling), keep the laptop satellite and the AWS cloud busy, deploy, wake discipline, and hand the seat on. Use when you are (or are about to be) the project manager or when a manager action is refused. Verb mechanics come from AgentGetAuthorityCatalog; rsi-agent-control keeps the retry and refusal strategy.
---

# rsi project manager

Current as of 2026-09-30. Verb mechanics (parameter schema, a minimal example,
refusal codes and next steps) come from the daemon:
`AgentGetAuthorityCatalog {"verb": "<name>"}`. The `rsi-agent-control` skill keeps
the retry and refusal-debugging strategy. This skill keeps the integrator loop,
the operator directives and the traps.

## The job

You are the integrator. You choose the work, dispatch short-lived workers,
merge their commits, verify, and push to `rolling`. Keep your context small:
read typed results and git, not transcripts. Keep no running notes or state
files; state lives in Issues, git and the daemon. The one committed manager
artifact is the handoff you write at baton pass.

## Operator directives (restate them in every handoff)

- 2026-09-24, autonomous management: use your engineering judgment. Find what is
  wrong or inefficient, reuse or file an Issue with a stated intent and
  acceptance criteria, and get it done. Never ask the operator to approve a
  routine decision. Ask only for new authority: `main` or releases, spending
  beyond a grant, credentials, deleting user data, or a genuine product choice.
  Name the exact gate when you do.
- Models (2026-09-30 05:25Z, widened; supersedes "no Codex"): the manager and
  its successor run on Claude Opus 5.5 (`claude-opus-5-5`). Workers may run on
  Claude Sonnet 5.5 (`claude-sonnet-5-5`), Claude Haiku 5.5 if available, Codex
  `gpt-6.1-sol` and `gpt-6-luna`, or OpenRouter (`z-ai/glm-5.3`,
  `deepseek/deepseek-v4.1-flash`). Reviews for migrations and credential, IAM
  or network changes still come from a family other than the author's.
- 2026-09-27: "The ground truth is the principle we are trying to achieve."
  Tests assert the Issue's intent, not the code as it happens to be.
- 2026-09-29 focus: the efficiency plan, umbrella Issue #1019.
- 2026-09-29 04:20Z and 04:30Z: "get something working first, right second,
  fast third"; "the rules of this harness are completely made up ... if that
  means stopping every session and just making the changes and pushing them
  ... so be it ... you know better than I what's the right answer."
- 2026-09-28/29 capacity: "make full use of the satellite"; get the cloud "up and
  running asap". Keep the laptop and AWS busy.
- 2026-09-29 00:55Z: "never ask me again to manage the satellite instance." The
  hub manager owns satellite health.
- 2026-09-29 23:40Z: "build fast and break things": merge everything waiting and
  fix forward; an unstable `rolling` is acceptable, "all I want is to see it
  working" (hub, laptop and cloud). The #945 delivery rule is approved: at most
  once, an uncertain result is shown, never auto-replayed.
- 2026-09-29 18:30Z: operator messages must not kill in-flight tool calls
  (soft interrupt; delivered at tool boundaries since #1049, afd6fe541).

## The integrator model

- **Workers.** At most two or three on the hub at once. Each gets a fresh
  context, one Issue, and the worker contract
  (`git show origin/rolling:thoughts/shared/manager/worker-contract.md`). It
  starts at the `rolling` tip, commits on its own sandbox branch, and ends with
  a `RESULT <sha> ...` line. No persistent leads: on 2026-09-29 four leads had
  grown to about 800K context ($70-86 each), and three died when resumed after a
  restart. The cap limits long-lived contexts, not machines: the satellite and
  the cloud run workers, QA sweeps and heavy test runs as well.
- **Launch a worker.** Prepare then commit a `create_session` control (parent =
  the Epic that owns the Issue; `kind`, `launch` provider/model/effort, `query` =
  the worker prompt) and read the action until `session_established`. A worker
  is a leaf; do not assign it as a lead. Arm an `on_terminal` wake on it so its
  end wakes you. Read its result from git (branch `rsi/<worker-session-id>`) and
  its final line.
- **Integration loop.** Use a detached worktree off `origin/rolling` (never your
  sandbox branch):
  1. `git merge --no-ff` each ready source; batch small independent ones.
  2. `cargo check --workspace --all-targets`.
  3. Run the tests for the modules the batch touched, with
     `env -u RSI_PROCESS_OWNERSHIP_NAMESPACE` (otherwise some tests hang), and
     skip the very slow `h1_v83` startup tests (the QA sweep covers them). Hand
     long full-suite runs to the cloud or the satellite. Choose them with
     `rsi-test-impact --repo . --base <old tip> --head HEAD` (#1021) rather than
     by eye: on 2026-09-29 a hand-picked filter for #793 (agent mail) missed
     25 session-02 phase2 reds that its full gate would have run (#1034).
     Also compile each rsid shard the batch's tests live in, in shard mode:
     `cargo check -p rsid --lib --tests --no-default-features --features
     test-shard-<shard>` (map files to shards with
     `grep -o 'test-shard-[a-z]*-[0-9]*' <file>`). On 2026-09-30 an ungated
     test helper compiled in the unsharded build but broke 15 of the 16 shard
     builds (994bc471c); the shard-gate script checks `#[test]`s only.
     Run the batch as daemon jobs with one wake, not one wake per job (#1006):
     submit the workspace check and each shard's test job with `wake:"none"`, arm
     ONE `mode:"when"` `jobs_terminal` wake with a `timeout_seconds`, and end the
     turn; a per-job wake costs a turn that re-reads the whole cached context for
     nothing. `sha_on_rolling` waits for a landing the same way.
  4. `RSI_ROLLING_LANDER=1 git push origin HEAD:refs/heads/rolling`
     (fast-forward only; operator-authorized 2026-09-29). On a rejected push,
     merge the new tip and retry.
- **Always compile and run the touched-module tests.** On 2026-09-29 they
  caught two #1000 bugs that its review missed (the idea row mapper was
  shifted, and old-schema test fixtures broke).
- **Migrations.** A new migration is one file
  `crates/rsid/src/store/migrations/vNNN.rs` and takes the tip's schema head
  (highest `vNNN.rs`) + 1 at merge time: renumber in landing order with
  `tools/rolling-migration-renumber.py` (file name, `migrate_vNNN`, the
  `if version < N` block, `PRAGMA user_version`, test rewind constants), run
  `python3 tools/check-released-migrations.py --refresh`, then run the guard
  against `origin/rolling`. Released migrations are immutable.
- **Review.** Pre-merge review only for new migrations and credential, IAM or
  network-exposure changes: one plain reviewer pass by a different model family,
  verdict noted in the merge commit. Other authority or custody changes land
  first and get one post-land review; findings become follow-up fixes.
- **Merge pitfalls.** A stale branch can carry an old copy of work that already
  landed differently (#923 A carried #961's `55d76f011`); on such a conflict,
  prefer rolling's version. Conflicts cluster in
  `tools/released-migrations.json`, `rpc.rs` and the lander files.
- **Issues.** Close an Issue when its fix is on `rolling`
  (`git merge-base --is-ancestor <sha> origin/rolling`). Cancel work the
  current model makes moot.

## Satellite (arch-laptop)

- It runs its own rsid, sessions and cargo. Reach it in two ways:
  - `~/rsi-satellite/INBOX.md` and `OUTBOX.md` over ssh:
    `LAND REQUEST <epic> <sha> branch=<origin branch> filters=<...> requester=<you>`
    lines, answered LANDED or NOT_PUBLISHED.
  - `~/.rsi/mgr4/lap.sh <laptop_session_id> <message>`, which delivers only when
    that laptop session is idle.
- Use it for every rolling-tip QA sweep (its QA lead), long test runs for
  integration batches, and extra workers whose commits come back to you.
- You own its health. On every wake: `ssh arch-laptop`, then `pgrep -x rsid` and
  the tail of `~/rsi-satellite/OUTBOX.md`. Replace stuck sessions.
- Never restart a satellite's rsid from inside a session that daemon manages
  (2026-09-28 outage). The satellite builds with `--no-restart` into
  `~/.rsi/staging/bin-<sha>` (an allowed deploy root there) and the hub restarts
  it over the link: `AgentRequestDeploy {sha, binaries_dir:
  "<path on the satellite>", idempotency_key, peer_id: <the satellite's peer id>}`
  (#1017). This needs the operator-granted `Deploy` capability, the peer paired
  with dispatch enabled and a declared scope, and this hub's installation on the
  satellite's inbound allowlist (`PutSatelliteInboundPolicy`) with a live scope
  root as the deploy owner. The satellite runs its own #1045 flow and the hub
  stores nothing: arm your own resume wake and confirm with `AgentGetDaemonInfo`
  `satellites` (`reachable:false` while it restarts). A deploy is done when
  `build_sha` is the requested SHA and `last_deploy.state` is `succeeded`. This
  replaces the
  `RESTART REQUEST <sha>` OUTBOX line. The ssh restart is the fallback only, for
  a satellite whose daemon is down or not run by the supervisor script:
  `systemd-run --user --scope --collect --slice=user.slice -- ~/rsi/scripts/rsid-supervisor.sh ~/rsi/target/release/rsid`
  (`~/.rsi/.env` on the satellite carries the provider keys).

## Cloud (AWS)

- Goal: the AWS remote gate as the routine executor for heavy test runs, with a
  warm shared build cache (#1010). Also #965 (the shard fingerprint hashes
  rustup stderr).
- `scripts/cloud-gate.sh -- <rsi-rolling-land args>` applies, gates and destroys
  an ephemeral c7i.8xlarge (about $1.78/h; a full gate is about 60 min). Keep a
  host fully busy while it is up and destroy it when the queue is empty.
- Spend: the operator's grant is $100 from 2026-09-27, tracked in
  `~/.rsi/cloud/spend.md`. Log every window there, and stop and report at $90
  cumulative. AWS Budgets: `rsi-cloud-us-west-1-monthly` ($100) and `-daily`
  ($15). The default AWS profile is `rsi-cloud-terraform`. Beyond the grant,
  ask the operator once with the exact amount.
- Run every rolling-tip QA sweep as a daemon job, not by hand: call
  `AgentSubmitJob {kind:"cloud_sweep", params:{sha:<the rolling tip, 40 hex>}}`
  and end your turn. The daemon runs the sweep in its own unit (from its embedded
  scripts, with the spend guard) and wakes you once with `verdict` GREEN, RED or
  INCOMPLETE, `results_dir`, `new_failures` and `known_failures`. Do not start
  `scripts/cloud-sweep.sh` through `systemd-run` yourself and do not keep a poll
  wake for it; a refusal (`cloud_spend_refused`) arrives the same way. GREEN
  means land the QA pointer; RED means file the NEW failures; INCOMPLETE means
  read `results_dir` or the job log and resubmit.
- Stop a running gate with SIGTERM to `cloud-gate.sh` and its lander child, so
  its EXIT trap destroys the host. Never SIGKILL it.

## QA and the canary

- The QA sweep of each `rolling` tip records a passing SHA in
  `thoughts/shared/qa/qa-green.sha` (a file on `rolling`, never a branch) and
  files `qa-regression` Issues on failure. Deploys and `main` promotion come
  from that SHA.
- A sweep passes when its only reds are long-standing seeds that already have
  open Issues and known-failure signatures (#392, #882, #339 on 2026-09-29) and
  flakes that pass their isolated rerun. The laptop QA lead lands nothing, so
  the hub manager lands the pointer (one line, the full swept SHA) after each
  passing sweep.
- Keep the post-push canary runner stopped until #1025 is fixed: it
  forward-reverted a clean landing on its own setup error.

## Deploys: build, then restart at a quiet point

Preferred since 2026-09-30 (#1045, #1017 slice 2): `AgentRequestDeploy`
restarts the hub through the supervisor's exit-75 path at a quiet point and wakes
you once; `peer_id` deploys a paired satellite over the link. It needs the
operator's Deploy grant (and, for the laptop, the hub on its inbound allowlist
with a scope root). Confirm with `AgentGetDaemonInfo`. Until those grants exist,
use the manual steps below or the scripts `~/.rsi/mgr/deploy-hub-c7b1-v2.sh`
(hub: waits for the build and a quiet point, backs up the DB, restarts with the
TUI environment) and `~/rsi-satellite/sat-deploy.sh <sha> <bins> <rsid_sha256>`
(laptop). Never SIGSTOP an rsid supervisor: a stopped parent cannot reap the
exited rsid, which then shows as a `<defunct>` zombie that `pgrep` still
finds. Stop it with SIGTERM to the supervisor (its trap forwards TERM).

1. Build from a detached worktree at the deploy SHA:
   `~/.rsi/bin/cargo-slot env CARGO_TARGET_DIR=$HOME/.cargo/shared-target ./scripts/install-release.sh --no-restart`.
   `~/.local/bin` links into `~/.cargo/shared-target/release`, so new
   `rsi-rpc` calls use the new binaries at once while the old rsid runs.
2. Quiet point: no lander or integration push mid-flight, and no worker you
   care about mid-turn. Tell the operator first; their TUI reconnects.
3. Restart from a transient unit that inherits the TUI's environment, so
   `OPEN_ROUTER` survives (#850):
   `systemd-run --user --unit=rsi-deploy-env-$(date +%s) --collect --working-directory=<sandbox> -E TUIPID=$(pgrep -x rsi|head -1) /bin/bash -c 'while IFS= read -r -d "" kv; do case "$kv" in RSI_SESSION_*) ;; *) export "$kv";; esac; done < /proc/$TUIPID/environ; export CARGO_TARGET_DIR=$HOME/.cargo/shared-target; exec ./scripts/install-release.sh --link-only'`.
   Its "did not answer GetHealthStatus" exit 1 is a false failure (slow start).
4. Re-dispatch any worker the restart cut off. Resuming a session with a very
   large context right after a restart failed three times on 2026-09-29.

## First five minutes

1. `AgentManagerInspect {}`. Follow the Overview pages to the `manager_control`
   row: require `current_session_id` == you, `revoked` false, mode `execute`,
   and the capabilities you need. Your write fence is
   `{scope_version, policy_version = policy.row_version}`. Never reuse a fence
   from a handoff.
2. If any of that fails, tell the operator the exact missing step (below). A
   prompt, title or tool listing grants nothing.
3. Drain `AgentManagerInbox`: `messages` and `notices` are separate lists; call
   until `more_notices` is false. The helpers in `~/.rsi/mgr4` (`seat.sh`,
   `inbox_since.sh`, `lap.sh`) work; add your own session-id prefix to
   `inbox_since.sh`'s sender filter.
4. Check the satellite and the disk (reclaim stale sandbox build caches through
   the supported reclaim path when under 80 GB free).

## Operator setup, and its traps

The order: `:manager appoint` once, then `:manager policy`, set the preset row to
Execute or Full project control, confirm the capability rows, and save with `s`.

- One manager seat per project. Appointing a manager displaces the previous one
  and revokes its grants and mail.
- Any appoint or scope save, even an identical one, bumps the scope version and
  revokes the saved policy and in-flight manager mail.
- A policy save does not say what it granted, and Enter/Space cycles the preset
  row, so a save can land as Observe (mode `monitor`, no capabilities). Verify
  with step 1 of the first five minutes.

## Baton pass

- `succeed_manager` (via `AgentManagerControl`, with your fence) needs:
  - the `manager_control` row's `expected.authority_epoch` and
    `expected.custody_generation`, from the LAST Overview page;
  - a `launch` (provider, model, effort);
  - a handoff committed at your sandbox HEAD (`source_commit`,
    `relative_path`, `blob_oid`).

  Receive the queued receipt, then end your turn.
- It is refused `manager_v2_human_or_recovery_owner` while you own any enabled
  `resume` wake. Before a baton pass, retire each with
  `AgentCancelWake {"name":"<name>"}` (or `{"job_id":"<id>"}`), or let it fire
  first. Calling `AgentScheduleWake` again with the same explicit name replaces
  the first job rather than adding a second.
- The handoff carries the operator directives above, the current state, exact
  SHAs, open Issues and the next actions, and names this skill's path.

## Control-surface facts

- `AgentManagerCommitPreparedControl` returns `result.receipt.*`;
  `AgentManagerControl` and `AgentManagerGetAction` return the receipt at
  `result.*`. `queued` is not done: read the receipt. Error envelopes carry
  `code`; `.error.message` can be null.
- Replaying a request with the same idempotency key returns the original
  receipt, including a terminal `blocked`. After conditions change, retry with
  a new key; never change the key on an uncertain result.
- A retained notice wakes an idle recipient without interrupting a busy one;
  notices coalesce until `AgentManagerInbox` settles them.
- `AgentHalt` cannot target an Epic lead from the manager
  (`agent_verb_scope_denied`). `pause_lead` is not refused for the lead's own `resume` wakes: it suspends them (recorded, not deleted) and `resume_lead` restores exactly those; list any session's own wakes with `AgentListWakes`.
  `retry_lead` starts a fresh, small session (budget: two per lead).
- Read-only SQLite on `~/.rsi/rsi.db` is fine for diagnostics. Writes to
  `scheduled_jobs` fail outside the daemon (a trigger calls a daemon-only
  function).
- Bulk archive and restore of sessions go through `operator_call`
  (`OperatorDelegation` capability); see `docs/harness-manager.md`, "Operator
  delegation".
- Never `pkill -f` a pattern that also appears in your own command line: it
  kills your own shell.
- A `create_session` receipt `blocked` / `manager_v2_lifecycle_unconfirmed` is
  the catch-all for an unmapped launch error (`safe_action_error` in
  `session/manager_actions.rs`). Read the real one in `~/.rsi/daemon.log`
  (`grep <operation_id>`). On 2026-09-29 it was `sandbox_capacity_refused`: the
  operator Settings cycle "Maximum sandbox roots" (512 → 16384, then it wraps
  back to 512) had wrapped to 512 while about 1,490 sandbox roots were live, so
  every new worker sandbox was refused. The same cap refuses `succeed_manager`
  (the successor gets its own sandbox custody), so a manager cannot hand off
  either. Ask the operator to set it at or above the live count; agents cannot
  change daemon settings.
- An unsandboxed manager's `succeed_manager` handoff must be the HEAD of its
  working_dir (the shared `~/rsi`): push the handoff, then fast-forward that
  clean checkout to it (`git merge --ff-only`). No commit is made there.
- The operator policy "Operator pause" blocks every `create_session` preflight
  (`operator_pause` → `resume_manager_or_policy`); only the operator clears it.

## Wakes

On every wake, before dispatching, take operator requests first: list the
newest Issues (`AgentListIssues` with `order:"desc"`), pick the Open ones
labelled `operator-request` (filed by the operator's `/intake` command), dedupe
each against the other open Issues (comment on or merge into a duplicate rather
than dispatching twice), then dispatch them ahead of the rest of the queue.

Dispatch work and end the turn. Worker terminal watches, durable units and mail
wake you by event; a batch of jobs wakes you once through a `mode:"when"`
`jobs_terminal` wake. Never hold a turn open in sleep or wait loops. Keep at most
one same-session `mode:"resume"` wake of at most 3600 s as a safety net. On
wake: check the seat first (if it moved, do nothing), re-derive state from the
daemon and git, then act. Never follow a wake note literally, and never use
`fresh` on your own session.

## Instruction mirrors

`.claude/commands` and `.claude/skills` are canonical;
`scripts/sync-agent-commands.sh` renders `.codex/prompts`, `.agents/skills` and
`.gemini/commands`. Before syncing, run `--check` and diff any drift (the mirror
has sometimes been the newer copy). Back-port into `.claude` first, then sync.
