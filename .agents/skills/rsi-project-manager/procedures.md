# PM procedures

Read only the section needed for the current action. Live verb schemas and
refusals come from AgentGetAuthorityCatalog {verb}.

## Runaway runs (#1337)

On 2026-10-06 a worker ran the full `rsid-store` suite and a broad `rsid` run
for about two hours at 12-14 cores each (host load 60-83 on 32 cores). The
manager saw it at 41 and 83 minutes and let it continue; the operator stopped
it. Do not repeat that:

- **Every wake, read the top CPU consumers**: `uptime`, then
  `ps -eo pid,etimes,pcpu,comm --sort=-pcpu | head` (or `systemd-cgtop`), and
  any `runaway_process` notice in your inbox. The daemon's CPU-time andon
  samples each agent process tree once a minute; past the operator's
  `cpu_andon_cpu_minutes` (default 240) or when one tree dominates a host load
  of `cpu_andon_host_load` (default 40) it records a friction event and
  notifies you with the session id and a suggested halt. It never stops the
  tree itself: that is your call.
- **Intervene on runaway work.** Inspect elapsed time, the admitted budget,
  log progress and CPU pressure. Stop stalled, over-broad or unbounded work,
  or work dominating a host load over 40. A healthy `scoped_test` making
  progress within its admitted budget is not runaway merely because it
  passes 20 minutes; its default budget is 60 minutes.
- **Stop the right owner.** `AgentHalt` stops a provider session; it is not a
  cancellation path for its detached job. For a job, inspect
  `AgentGetAuthorityCatalog {verb:"AgentCancelJob"}`: the owner may cancel its
  own jobs; a current manager may cancel ordinary test/build jobs in live
  scope, but not landing, cloud gate/sweep, release build or automatic
  full-library QA jobs. Use only that documented scope. For provider-session
  work, use `AgentHalt`, then `AgentContinueChild` with a scoped instruction
  and the required observed cursor.
- **Job timeouts**: an ordinary `AgentSubmitJob` `test` job is stopped and fails
  `job_timed_out` after the operator's `job_test_timeout_mins` (default 20),
  while `scoped_test` defaults to 60 minutes (or a higher operator test
  default, capped at 180). Respect the admitted job's timeout and runner
  accounting; a job held behind a deploy drain past the hold cap
  plus 5 minutes fails `job_admission_timed_out` instead (resubmit).
  Raise one job with `params.timeout_minutes` (up to 180) only when a scoped
  run is known to need it; workers cannot raise it.
- **Report every stopped run in your handoff** (session, what ran, how long).

## The integrator model

- **Workers.** At most two or three on the hub at once. Each gets a fresh
  context, one Issue, and `docs/agents/worker-contract.md`. It
  starts at its allocated source, preserves that source's ancestry, commits
  on its assigned sandbox branch, and ends with
  a `RESULT <sha> ...` line. No persistent leads: on 2026-09-29 four leads had
  grown to about 800K context ($70-86 each), and three died when resumed after a
  restart. The cap limits long-lived contexts, not machines: the satellite runs
  workers, QA sweeps and heavy test runs as well.
  Every brief says: merge `origin/rolling` when needed, never rebase a source
  stack; let the queue's lander integrate `rolling`. Custody and a pending
  verification wake depend on the allocated source remaining an ancestor.
- **Plan revisions reach live workers only by message (#2431).** A worker
  pinned to a plan commit never sees a later plan revision on `rolling` unless
  told. When a plan commit, plan review or Issue edit amends a slice whose worker
  is running (`AgentWorkerRoster` or the Issue's execution shows it), send
  that worker the amendment list and the new commit with `AgentSendMessage`
  (or relaunch it from the new pin if it has barely started), before you
  accept or enqueue its work. Do not leave it to discover the change by
  merging `origin/rolling`.
- **Launch a worker by hand** (only when the default route below does not fit,
  with the reason noted on the Issue). Use `AgentManagerLaunchIssueWorker`
  (Issue-bound: the daemon sets the parent Epic, brief and binding, and arms
  your terminal watch). A worker is a leaf; do not assign it as a lead. Read
  its result from git (branch `rsi/<worker-session-id>`) and its final line.
  Close a hand-driven Issue when its landing publishes; a starter closes its own.
- **Default route: the bundled issue-pipeline starters (#2304).** An ordinary
  Issue (one worker, one sandbox, acceptance testable with `scoped_test`) goes
  through a daemon-seeded starter, not a hand-driven worker. Pick by risk:
  `issue-pipeline` for ordinary work; `issue-pipeline-post-review` for an
  authority, custody, queue, lander or deploy change (one review of what
  landed; changes requested become deduped follow-up Issues and never reopen
  the closed Issue); `issue-pipeline-reviewed` for a migration or a credential,
  IAM or network-exposure change (an accepted review of the exact verified head
  precedes every enqueue). When an ordinary or post-review run's change turns
  out to add a migration, its land is refused `land_requires_review` and the
  same execution reviews the exact head (`pre_review`, then `pre_land`, #2688)
  instead of blocking, so triage through `issue-pipeline` needs no relaunch.
  Run `AgentTopologyList` for the starter's `id` and
  `definition_digest`; no plan JSON and no upsert are needed (at startup the
  daemon upgrades a starter row still holding a definition it shipped earlier to
  the current one as a new revision; any other row is kept and
  `AgentTopologyList` shows its `starter_drift`, #2660). Then
  `AgentTopologyExecute {topology_id, expected_digest, epic_id, inputs:
  {"issue": <display_number>}, idempotency_key}`: the daemon snapshots the Issue
  at accept, sets it `InProgress`, runs implement, verify, derived-filter
  landing and close with the published SHA, and settles the Issue itself.
  The execute call arms ONE `topology_execution_settled` resume wake for you
  (hub #2967; a replay arms none; `wake:"none"` opts out, then arm your own
  `AgentScheduleWake mode:"when"` with `message:"<what to do on resume>"` and
  `when.topology_execution_settled`);
  `settle_wake_armed` on the result says so; inspect `settle_wake_refusal`
  if arming failed before arranging a replacement. Do not arm that wake by hand for
  an execution you start without `wake:"none"`: the default covers it, and an
  owner keeps exactly one enabled settle wake per execution (hub #2986: a wake
  you already own makes the default arm nothing; a later manual arm replaces
  the default and names it in `replaced_job_ids`). End the turn. Spend turns only on a
  real gate: an on-call node question, a `blocked` execution (the owner gets the
  notice; `AgentTopologyResolveAttempt` resolves only preserved work, a review blocked `review_unsettled`/`review_rounds_exhausted` and a land blocked `land_admission_lost` (retry); any other cause, e.g. `policy_refused`, is cleared at its source or ended with `AgentTopologyInterrupt`) or a landing
  refusal that names a decision. Do not hand-enqueue the landing or close the
  Issue; do not wake per node. Hand-drive only with an explicit reason noted on
  the Issue: research, design or multi-worker work, a `qa` red fix (until land
  passes `fixes_issue`), an Issue needing a product decision first, or a case a
  starter cannot do (more than one sandbox, no `scoped_test` acceptance).
  Adoption evidence and the measured turns/cost comparison:
  `thoughts/shared/notes/2026-10-15-2304-adoption.md`; the older route
  acceptance is `thoughts/shared/notes/2026-10-14-2238-acceptance.md`.
- **Worker sandbox base (#1195).** A `create_session` (and
  `AgentManagerLaunchIssueWorker`) sandbox branches from the freshly fetched
  `origin/rolling` tip by default, never from the shared checkout's local
  `HEAD`. To build on your own unlanded commits, pass `"sandbox_source":
  {"path": "<your sandbox_root>"}`: the worker branches from that worktree's
  committed `HEAD`, pinned when the action is admitted (uncommitted changes are
  not used; the receipt then reports `source_dirty: true`). Use `{"commit":
  "<40-hex>"}` for an exact commit. A bad source refuses before any effect
  (`manager_sandbox_source_invalid`, `_not_worktree`, `_commit_unknown`).
- **Manager state.** Keep lander copies, gate filter files, chain scripts and
  other seat state in `~/.rsi/mgr/<seat>/`. Reserve `target/` for Cargo outputs;
  detached jobs may outlive a provider turn, and cache reclaim must retain
  targets with a live consumer or unknown top-level content (#1429).
  Submit integration and release builds through `AgentSubmitJob` with the
  integration worktree as `worktree`: integration jobs get a registered target
  under `~/.rsi/build-targets/` (#1864), while manager release builds reuse
  #1218's persistent `~/.rsi/staging/release-target` and its admission lock.
  The job log's `rsi-build-target` line records the selected target. Integration
  source paths stay isolated; the shared release target has a 16 GiB cap.
  Keep the manager's stamped sccache environment. Reviewers use their sandbox
  target. Never create ad-hoc targets under `.cargo`, `.cache`, `review-targets`
  or `staging`; legacy targets require operator adoption in Scratch reclaim.
- **Landing.** One path: submit one accepted source SHA to the daemon merge
  queue (`AgentEnqueueLandingSource`). Only the queue runs `rsi-rolling-land`;
  never invoke it directly. Enqueue with `derive_filters:true` so the queue derives the
  landing filters; never derive them by hand (AGENTS.md "Landing"; one
  accepted change per gate, never batched). Check a worker's verification
  with `AgentCheckLandingCandidate {job_id|source_session_id, source_commit}`
  before you enqueue (never raw SQL on `agent_jobs`): `ok:true` is a finished
  `scoped_test` at that commit with no new failures; `candidate_head_mismatch`
  with `docs_only_delta:true` means only documentation changed since the run,
  `candidate_conflicts_with_rolling` names the paths that no longer merge onto
  `origin/rolling` (the worker merges `origin/rolling`, resolves and reruns one
  `scoped_test`), and baseline reds alone never block. A fix for a known red also
  passes `fixes_issue: <N>` (the red Issue; a qa Issue's `Failing tests:` line
  names the tests) or `fixes_tests: [..]`: the gate then requires those tests
  green and refuses `claimed_fix_still_red` otherwise (#2167). The lander merges, compiles, runs
  the filtered tests and publishes fast-forward only; do not run a separate
  workspace check, hand-pick tests, merge by hand or push `rolling` yourself.
  The queue wakes you once with the outcome (published or refused, with
  `gate_timing`) for every landing you enqueue, even a worker's commit named by
  `source_session_id` (the worker is never woken): arm no `sha_on_rolling` for
  your own enqueue, that is a second manager turn for one landing (#2405). Keep
  `sha_on_rolling` and `issue_closed` for a dependency's landing you did not
  enqueue, and `jobs_terminal` for jobs: one `mode:"when"` wake, never one wake
  per job. A rejected
  fast-forward means the lander retries on the new tip; resubmit if it reports
  not published. Landed means `git merge-base --is-ancestor <SHA>
  origin/rolling` holds. Track your queue entries with `AgentListRollingQueue`
  (newest first; a successor sees its predecessor's) and withdraw an obsolete
  `queued` one with `AgentCancelRollingQueueEntry {entry_id}`; an admitted or
  gating entry stays with the lander. To replace your own still-queued entry
  with a descendant commit, enqueue the new SHA with `supersedes: <entry id>`
  and a new replay key: it keeps the old queue position and single outcome wake.
  `queue_entry_in_flight` means enqueue normally without `supersedes`.
  Never read `rolling_queue_entries` with
  raw SQL for this. Hand long full-suite runs to the satellite.
- **Always run the touched-module tests** (the filters above). On 2026-09-29
  they caught two #1000 bugs that review missed.
- **Migrations.** A new migration is one file
  `crates/rsid-store/src/store/migrations/vNNN.rs` and takes the tip's schema head
  (highest `vNNN.rs`) + 1 at merge time: renumber in landing order with
  `tools/rolling-migration-renumber.py` (file name, `migrate_vNNN`, the
  `if version < N` block, `PRAGMA user_version`, test rewind constants), run
  the guard against `origin/rolling` (the pin file is derived by the lander at publication, #1949: nobody hand-edits it or runs `--refresh` per migration; a merge conflict on an old source's pin is cleared by running `--refresh` over the markers). Released migrations are immutable.
- **Review.** Pre-merge review only for new migrations and credential, IAM or
  network-exposure changes: one plain reviewer pass by a different model family,
  verdict noted in the merge commit. Other authority or custody changes land
  first and get one post-land review; findings become follow-up fixes.
  A bound worker reads its own Issue, its review Issues and the same-project
  Issues that a `#N` or `#A-#B` in its Issue body, handoffs or launch brief names
  (read-only, #218); `related: [N, ...]` copies other Issues' complete texts and
  latest handoffs into the brief, and the brief names its one publication Issue.
  A bound reviewer reads its own review Issue; launch it with
  `review_of: <implementer Issue number>` (#1590): the daemon copies that
  Issue's text, acceptance criteria and complete latest handoff (landing
  filters included) into the reviewer's brief. Name the source SHA in `brief`;
  never paste a handoff yourself (a pasted copy gets truncated). Satellite or
  hand-driven work has no hub handoff to copy: put the implementer's result
  report (root cause, tests run, job receipts) in the review Issue body before
  launch, or the reviewer cannot check it (#2685).
- **Merge pitfalls.** A stale branch can carry an old copy of work that already
  landed differently (#923 A carried #961's `55d76f011`); on such a conflict,
  prefer rolling's version. Conflicts cluster in
  `tools/released-migrations.json`, `rpc.rs` and the lander files.
- **Issues.** A starter execution closes its own Issue with the published SHA;
  close a hand-driven Issue when its fix is on `rolling`
  (`git merge-base --is-ancestor <sha> origin/rolling`). Cancel work the
  current model makes moot.

## Satellite (arch-laptop)

- A brief for a mirrored or older Issue must ask the worker to first check
  current `origin/rolling` with `git log -S<symbol>` or the cited file; if the
  work already landed, publish a handoff naming the covering commit and end
  `already_satisfied` with that SHA and no new commit.
- It runs its own rsid, sessions and cargo. Reach it in two ways:
  - `~/rsi-satellite/INBOX.md` and `OUTBOX.md` over ssh:
    `LAND REQUEST <epic> <sha> branch=<origin branch> filters=<...> requester=<you>`
    lines, answered LANDED or NOT_PUBLISHED.
  - `~/.rsi/mgr4/lap.sh <laptop_session_id> <message>`, which delivers only when
    that laptop session is idle.
- Reports from it: its manager's `AgentReportToHub` (`enqueue`, `deploy_ready`, `result`) takes
  no address: reports route by project to this project's CURRENT manager
  (#2242), so a brief never carries a hub session id and a seat change needs
  no new brief. They arrive as untrusted `satellite_report` notices in
  `AgentManagerInbox` (verify every SHA). A report recorded for a seat that
  was then replaced is re-homed to the new manager within one pull (15 s).
  The laptop manager reads each report's fate in its own `AgentGetDaemonInfo`
  `hub_reports` (`queued`, `fetched`, `held` with the hub's reason,
  `delivered`, `refused`, or the typed failure `undelivered`), and
  `AgentReportToHub` itself refuses `satellite_report_hub_unreachable` while
  the hub is not pulling; at 32 pending it refuses `satellite_report_queue_full` with
  the depth, oldest pending age, per-kind counts and `last_hub_pull_at` (#135), so wait
  for acknowledgements and never re-send a pending ENQUEUE or DEPLOY-READY. OUTBOX.md stays the fallback you read on every wake.
- Satellite work that needs hub verification (#2732): its commit is a fetched
  object the hub has not verified. Start `issue-pipeline-post-review` with
  `base_commit` set to that SHA. The implement worker verifies the head
  unchanged (one `scoped_test`) and hands off `status: complete` with
  `commit` equal to the base and no new commit; because the base is not on
  `origin/rolling` the executor accepts it, then land and review run as usual.
  Never use `already_satisfied` for this (it closes the Issue and lands nothing).
- Use it for every rolling-tip QA sweep (its QA lead), long test runs for
  integration runs, and extra workers whose commits come back to you.
- You own its health. On every wake: `ssh arch-laptop`, then `pgrep -x rsid` and
  the tail of `~/rsi-satellite/OUTBOX.md`. Replace stuck sessions.
- **Deploy route: who does what (#171).** The satellite's rsid is never restarted
  from inside a session that daemon manages (2026-09-28 outage), so the route is
  fixed and every brief to the laptop names it:
  1. The laptop builds and stages: `scripts/install-release.sh --stage-dir
     ~/.rsi/staging/bin-<sha>` (an allowed deploy root there; `--no-restart` alone
     still overwrites the live install, #136). DIR must be new or empty (a non-empty
     DIR is refused) and becomes the `binaries_dir`; its layout is in "Deploys" below
     (#262). It then reports DEPLOY READY with `AgentReportToHub` kind
     `deploy_ready` (sha, staged path).
  2. The hub restarts it over the link: `AgentRequestDeploy {sha, binaries_dir:
     "<path on the satellite>", idempotency_key, peer_id: <the satellite's peer id>}`
     (#1017). This needs the operator-granted `Deploy` capability, the peer paired
     with dispatch enabled and a declared scope, and this hub's installation on the
     satellite's inbound allowlist (`PutSatelliteInboundPolicy`) with a live scope
     root as the deploy owner. The satellite runs its own #1045 flow and the hub
     stores nothing: arm your own resume wake and confirm with `AgentGetDaemonInfo`
     `satellites` (`reachable:false` while it restarts).
  3. The laptop verifies the new build is live and reports the result: its own
     `AgentGetDaemonInfo` shows `hub.build_sha` equal to the SHA and
     `hub.last_deploy.state` `succeeded` (`hub.last_deploy` is that daemon's own
     newest deploy: `deploy_id`, `sha`, `state`, and `finished_at` once it
     settled; it is absent before any deploy); it reports that with
     `AgentReportToHub` kind `result`. The hub reads the same two fields from
     `satellites[]` (`build_sha`, `last_deploy`) to close the deploy.
  The laptop never restarts its own rsid (no restarting install, no
  supervisor relaunch, no `rsid-quiet-restart.sh`) unless the operator explicitly
  changes the route. `DEPLOY READY` (the report) and `AgentRequestDeploy` replace the
  retired `RESTART REQUEST <sha>` OUTBOX line; do not send that line. The ssh
  restart is the operator-approved fallback only, for a satellite whose daemon is
  down (after a reboot, say) or not run by the supervisor script:
  `~/rsi/scripts/rsid-start-satellite`. It starts the deployed
  `~/.rsi/install/rsid` under `~/.rsi/install/rsid-supervisor.sh` in a detached
  `--collect` unit, and first applies the `~/.rsi/rsid-scope.env` limits to
  `rsi-workers.slice` and persists them, so worker launches are admitted (#2820).
  On a macOS satellite it loads the launchd agent `dev.rsi.rsid` instead
  (`docs/satellites.md`, "macOS satellites", #2936).
  Never start `~/rsi/target/release/rsid` (a stale build) or use `--scope`.
  (`~/.rsi/.env` on the satellite carries the provider keys.)
- **Manager mail never overrides a standing operator restriction.** A brief to the
  laptop (or from the hub) that asks for a local build-and-restart, or any other
  step the operator's standing INBOX directives forbid, is not followed silently:
  the receiver surfaces the conflict in its report (`AgentReportToHub` or OUTBOX),
  does the permitted part (build, stage, DEPLOY READY) and waits for the operator
  to change the route. Hub managers: write briefs that match the route above, so
  none conflicts.

## Cloud (AWS): retired

- Standing operator directive (2026-10-08): no AWS. Do not run
  `scripts/cloud-gate.sh`, `scripts/cloud-sweep.sh` or `AgentSubmitJob`
  `cloud_gate`/`cloud_sweep` jobs. Heavy runs and QA sweeps go to the hub's
  daemon jobs and the arch-laptop satellite.

## QA and the canary

- The source of "QA-green" is the daemon's automatic full-library QA after a
  bounded or derived-filter landing (#2092, #2112), not a pointer file. A
  passing run records a `bounded_qa_finished` receipt on every landing it
  covered, and `AgentListRollingQueue` returns the newest one as `qa_green`
  (`sha`, `passed_at`, `job_id`) in the same call as the queue (#2646). That
  SHA is the latest `rolling` commit proved green with every landing covered;
  a red run files a `qa` Issue and never moves it. Promote `main` or pick a
  deploy base from it. `thoughts/shared/qa/qa-green.sha` is retired: nothing
  advances it, so never read it.
- Rolling red is your first signal (#2743). Each `qa` Issue the daemon files sends you one
  `qa_issue:<n>` notice (Issue, failing tests, rolling SHA) that survives a seat succession;
  `AgentManagerOverview` shows `rolling_health` per project (open `qa` Issues, `qa_green` and its
  age) and your successor's launch prompt carries it. Put a worker on an open `qa` Issue before
  other work. An `AgentRequestDeploy` of a SHA `qa_green` does not cover is accepted with
  `qa_warnings: [not_qa_green]` naming those Issues: read it, and say so when you deploy anyway.
- A run passes when its only reds are long-standing seeds that already have
  open Issues and known-failure signatures (a landing fixing one passes
  `fixes_issue`/`fixes_tests`) and flakes that pass their isolated rerun.
- Keep the post-push canary runner stopped until #1025 is fixed: it
  forward-reverted a clean landing on its own setup error.

## Deploys: build, then restart at a quiet point

Preferred since 2026-09-30 (#1045, #1017 slice 2): `AgentRequestDeploy`
restarts the hub through the supervisor's exit-75 path at a quiet point and wakes
you once; `peer_id` deploys a paired satellite over the link. It needs the
operator's Deploy grant (and, for the laptop, the hub on its inbound allowlist
with a scope root). Confirm with `AgentGetDaemonInfo`. A restart interrupts any worker
still mid-turn (provider processes are not re-adopted; the turn is resumed
after the restart), so the deploy waits for workers. It holds new worker starts,
your own creates included, for at most the operator's `deploy_drain_hold_secs`
(default 600 s), then waits for a lull without holding anything until
`max_wait_secs` (#1320, #1311). When a busy fleet never reaches a lull, send
`interrupt_workers: true` (#1461, keep `max_wait_secs` above the hold): past the
hold a worker mid-turn stops blocking, the restart interrupts it, the existing
post-restart path resumes it with a continue prompt, the outcome wake names the
interrupted worker sessions and each is a `deploy_interrupt` friction event; a
landing or a local test/build/landing job still blocks, so use it when the
fixes are worth interrupting workers for. A satellite deploy (`peer_id`) takes `interrupt_workers` and `max_wait_secs` too (#2587; a laptop worker's single long turn otherwise times the deploy out); a satellite build that predates it answers `satellite_deploy_interrupt_unsupported`. Send the same SHA and idempotency key with `cancel: true` (omit
`interrupt_workers`) to withdraw a deploy you no longer want to wait for; a held create shows
`held.reason: deploy_draining` in `AgentManagerGetAction`. Until those grants exist,
use the manual steps below or the hub script `~/.rsi/mgr/deploy-hub-c7b1-v2.sh`
(waits for the build and a quiet point, backs up the DB, restarts with the TUI
environment). The satellite has no manual restart of its own: see "Satellite
(arch-laptop)". Never SIGSTOP an rsid supervisor: a stopped parent cannot reap the
exited rsid, which then shows as a `<defunct>` zombie that `pgrep` still
finds. Stop it with SIGTERM to the supervisor (its trap forwards TERM).

1. Build from the recurring detached integration worktree at the deploy SHA:
   `AgentSubmitJob {"kind":"build","worktree":"<integration-worktree>",
   "params":{"command":"build","workspace":true,"release":true,"sha":"<deploy sha>"},"wake":"none"}`.
   The job compiles exactly that commit (hub #2954): do not move the integration
   worktree while it runs, or it fails `job_source_moved`. Arm one
   `when.jobs_terminal` wake and verify exit 0 on resumption; the receipt's
   `result.source_head` is the commit that was built. Read the
   daemon-selected target from the job log's `rsi-build-target` line. Its release
   output remains warm for the next deploy; never substitute a shared or
   ad-hoc Cargo target. Installed binaries live where the installer links them;
   `AgentRequestDeploy` installs into `~/.local/bin`. Targets backing installed
   links remain live consumers and must be retained.
2. Quiet point: no lander or integration push mid-flight, and no worker you
   care about mid-turn. Tell the operator first; their TUI reconnects.
3. Submit `AgentRequestDeploy` using the verified binaries under the job's
   daemon-selected target and the pinned SHA. Stage them first, from the
   deploy worktree, with `CARGO_TARGET_DIR=<job target>
   scripts/install-release.sh --stage-dir ~/.rsi/staging/bin-<sha9> --link-only
   --allow-foreign-build` (the build job records no installer provenance, so
   `--allow-foreign-build` is required; it reuses the job's binaries, verified
   byte-identical 2026-10-18). It adds every non-cargo member at the top level:
   `scripts/rsid-supervisor.sh`, the full `scripts/cargo-slot`,
   `scripts/target-pool` and `scripts/rsi-spill` (#2958). Never hand-copy a
   member list: a copy script that predates a new member skips it without
   failing (the b7262e75b hub deploy reported
   `Skipped (not in binaries_dir): rsi-spill`). The
   cargo target holds none of these scripts, so a deploy straight from it reports
   `Skipped (not in binaries_dir): rsid-supervisor.sh` (and `cargo-slot`,
   `target-pool`): supervisor fixes (#1592, #2355) never reach the running
   supervisor, and the job runner and worker-sandbox seeding keep stale or no
   copies (#2722). Always stage `cargo-slot` as the full `scripts/cargo-slot`,
   never `scripts/cargo-slot-wrapper` (an old daemon would publish it as the
   implementation). Every member sits at the top level of the staged directory
   (the deploy reads only `<binaries_dir>/<name>`); `scripts/install-release.sh
   --stage-dir <new dir>` writes exactly that layout plus a `MANIFEST.sha256`
   (#262). The deploy publishes `target-pool` into `~/.rsi/bin` and
   cargo-slot's implementation into `~/.rsi/lib/cargo-slot` (not `~/.local/bin`)
   by atomic rename, keeping a running copy's inode. Jobs run `~/.rsi/bin/cargo-slot`,
   a thin wrapper that execs the `lib` copy: a new daemon installs that wrapper
   itself, `lib` first, whenever the bin entry is absent or not the wrapper, so
   a host upgraded from a daemon that published the full script into `bin`
   migrates in the same deploy (a rollback restores the old `bin` file and
   removes a first-time `lib`). `install-release.sh` still bootstraps the
   layout for a fresh install. The laptop copy needs the same three scripts.
   After the restart, check the hub's `installed_tools` in `AgentGetDaemonInfo`
   (`cargo-slot` installed only behind the wrapper, hash equal to
   `sha256sum scripts/cargo-slot`; `target-pool` hash equal to
   `sha256sum scripts/target-pool` at the deployed SHA). Read its result after the quiet
   point and verify `GetHealthStatus`; use the installed restart front door
   when an operator-approved manual deploy is necessary. Do not treat an
   install or health-check refusal as a successful restart.
   Without the Deploy grant, restart through the manual `--link-only` path
   in a transient unit that inherits the TUI environment, preserving
   `OPEN_ROUTER` (#850). Set `<job-target>` to the verified daemon-selected
   target, and `<integration-worktree>` to its source worktree:
   `systemd-run --user --unit=rsi-deploy-env-$(date +%s) --collect --working-directory=<integration-worktree> -E TUIPID=$(pgrep -x rsi|head -1) /bin/bash -c 'while IFS= read -r -d "" kv; do case "$kv" in RSI_SESSION_*) ;; *) export "$kv";; esac; done < /proc/$TUIPID/environ; export CARGO_TARGET_DIR=<job-target>; exec ./scripts/install-release.sh --link-only'`.
   Verify health after it completes; a health-check timeout requires inspection.
4. Re-dispatch any worker the restart cut off. Resuming a session with a very
   large context right after a restart failed three times on 2026-09-29.

## Settle source worktrees under disk pressure

With the operator-granted `storage_control` capability, call
`AgentListSourceWorktreeCohorts`, then `AgentAuditSourceWorktreeCohort` for each
covered repository. When its audit is applyable, send the audit's `plan_digest`
and exact `authorization_phrase` as `authorization` to
`AgentApplySourceWorktreeCohort`, with a stable `idempotency_key`. Inspect the
returned durable receipt; it comes back within seconds while the run keeps settling
(`finished_at` unset), so never wait on the call. Arm one `AgentScheduleWake` with
`when:{"settlement_run_settled":"<run_id>"}` and end the turn; retry the exact request
after a transport interruption or the wake (the finished receipt comes back).
Never ask the operator to perform settlement. A policy or scope refusal means
retain that cohort and report the missing grant at the next milestone; it does
not authorize another deletion path. Eligibility is per root: a PM can settle
covered roots in a mixed repository cohort. Any foreign owner, participant or
receipt owner is `retained_foreign_owner` regardless of status; unknown ownership
is `retained_unknown_owner`. These exclusions never block covered roots.
Dirty, divergent, active and dependent work remains protected.

The local daemon also settles automatically under disk pressure, using the same
proof. Settings > Sandbox storage exposes `source_worktree_auto_settlement_enabled`
(default on) and `disk_pressure_min_free_gb` (default 100; capped at 25% of
total disk size), through operator-only `GetDaemonConfig`/`UpdateDaemonConfig`.
Pool trimming has separate controls.
Run the agent flow separately on the laptop under its own local seat policy;
hub authority does not delete satellite work. The settlement overlay's recent
run history, or operator `ListSourceWorktreeSettlementRuns` and
`GetSourceWorktreeSettlementRun`, shows origin, agent identity and outcome.
After revocation or disabling, recovery resumes only effects already started.
Untouched automatic/agent items recheck their authority and pressure policy
before starting. When those checks stop the run, proved-untouched roots are
retained and their items close as `Unattempted`; recovery never adds roots.
Full manager presets already grant `storage_control`.

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
- Your own enabled `resume` wakes do not block it, and you do not cancel them
  first. The publication that makes the successor the seat, in the same
  transaction, hands your durable obligations to the successor (settled-job and
  deploy outcome wakes, `jobs_terminal` and other predicate waits, terminal
  watches, live jobs and deploys; it confirms them with `AgentListWakes`) and
  cancels your generic backstops (safety nets, check-ins, recurring resumes):
  the rows stay, disabled. The final receipt (`AgentManagerGetAction`) carries
  `cancelled_wakes {reason: manager_succession_displaced_seat, job_ids}`
  naming exactly those wakes; a failed or refused succession names none and
  cancels nothing. The `succession_wakes_settled` manager event repeats both
  lists for audit. Your `health_anomaly` wake is a predicate wait, not a backstop: it
  follows the seat to the successor, so you need not re-arm it; an
  automatic context rotation still carries generic backstops. A wake that comes
  due while the succession is unresolved is held, not lost, and the displaced
  seat never wakes. It is still refused `manager_v2_human_or_recovery_owner`
  for a pending question or approval, an open no-idle-capacity incident, or a
  daemon-owned wake (`keepalive-*` child valve, `background-process-*`); an
  operator pause still holds the provider launch until it is cleared. Resolve
  those instead of cancelling around them.
- The handoff carries the current state, exact SHAs, open Issues, the kaizen filed, fixed and still open, and the next
  actions, and names this skill's path.
- A successor reads its predecessor's transcript with `AgentReadSessionEvents`
  and the predecessor's full session id (read-only; no other verb widens). Every
  earlier holder of your own project's manager slot is readable: a seat that
  rotated into you (`continued_from` lineage) and, after an operator
  reappointment of a fresh session to pick up from the old manager, every seat
  of the displaced appointment (#118, proven from the durable appointment
  retirement record, never from a prompt or title). Another project's seat, a
  non-manager and your workers stay refused `agent_verb_scope_denied`; do not
  fall back to raw SQLite.
- When an appointment hands the seat to another session (a new anchor and scope
  version), the displaced holder's pending work does not vanish (#1553): its
  terminal watches and live deploy (and the deploy's outcome wake) move to the
  new seat, `AgentManagerGetAction` reads its queued actions by id, and a queued
  Issue launch of the old scope version no longer holds the Issue (relaunch it).
  A launch that ends `failed`, `blocked` or `revoked` appends an Issue note.
  Queued actions themselves never run under the new scope: re-issue what you
  still want.

## Control-surface facts

- `AgentManagerCommitPreparedControl` returns `result.receipt.*`;
  `AgentManagerControl` and `AgentManagerGetAction` return the receipt at
  `result.*`. `queued` is not done: read the receipt. Error envelopes carry
  `code`; `.error.message` can be null.
- Replaying a request with the same idempotency key returns the original
  receipt, including a terminal `blocked`. After conditions change, retry with
  a new key; never change the key on an uncertain result.
- A retained notice wakes an idle recipient without interrupting a busy one;
  notices coalesce until `AgentManagerInbox` settles them. An actionable notice
  (satellite report, QA Issue, worker context cap) wakes an idle seat on arrival
  even while older delivered notices are unsettled, and the inbox returns it
  ahead of routine ones (#2871). Still page until `more_notices` is false: a
  single page leaves the rest unsettled.
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
  When the cause is that no launch happened at all (no session row for the
  receipt's `target_session_id`), `settle_uncertain_action` with that receipt's
  `row_version` closes it as `failed` (#1716); relaunch under a new key. A held
  create whose queuing seat was replaced by a new appointment is revoked
  `manager_v2_actor_seat_replaced`, never blocked: relaunch it as the new seat.
- An unsandboxed manager's `succeed_manager` handoff must be the HEAD of its
  working_dir (the shared `~/rsi`): push the handoff, then fast-forward that
  clean checkout to it (`git merge --ff-only`). No commit is made there.
- The operator policy "Operator pause" blocks every `create_session` preflight
  (`operator_pause` → `resume_manager_or_policy`); only the operator clears it.

## The global manager (when one is appointed)

When your authority catalog lists `AgentReportToGlobal`, the operator has
appointed a global manager over this project. Its playbook is
`.claude/skills/rsi-portfolio-manager/SKILL.md` (any tier). It manages project managers and
does no project work.
- **Report up only at milestones.** Call `AgentReportUp` (alias
  `AgentReportToGlobal`) once per
  event: work landed (Issue and SHA), blocked on an operator gate (name the
  gate), or handing off (the handoff path). Do not send status chatter: it
  reads your state through `AgentGlobalOverview`.
- **Its requests arrive as mail.** Take them like operator requests, ahead of
  the rest of the queue. If one conflicts with the operator's own direct
  instruction, the operator wins: say so in your report.
- **It may replace you** (`AgentGlobalAppointManager`). If it asks for a
  handoff, write it, commit it, reply with the path, and end your turn.

## The operator to-do list (#1747)

Every action only the operator can do (a login, a setting, a decision no
manager may make, a real gate) goes into the operator to-do list instead of
being repeated in chat or in reports. The operator sees it in the TUI panel
(`<Space>dt` toggles it, `dy` marks done, `dx` dismisses) and in a status-line
badge.
- **Put it there.** `AgentOperatorTodoUpsert {todo_key, title, how, link?,
  priority?, idempotency_key}`: `how` is the exact command or keys, `link` is
  `issue:#N`, `session:<uuid>` or an http(s) URL, `priority` 1 is most urgent.
  Your manager seat scopes it to your project; the same `todo_key` updates the
  item. When you report a gate up (`AgentReportUp`), name it and rely on the
  item for the `how`; do not paste the command into chat again.
- **Do not poll for it.** While you wait on an operator item, arm ONE
  `AgentScheduleWake` `mode:"when"` with `message:"<what to do on resume>"` and `when:{"todo_closed":"<todo id>"}` (the
  id the upsert returns) and a `timeout_seconds`; the daemon wakes you once when
  the item is done or dismissed, with its status and evidence. Keep the
  `health_anomaly` wake (not an hourly resume) as your only periodic backstop.
- **Close only on observed evidence.** When you observe it was done, call
  `AgentOperatorTodoClose {todo_key, status:"done", evidence, observed:true,
  idempotency_key}` with what you saw. If you did not observe it, leave it open:
  the operator closes it. `status:"dismissed"` with `evidence` is for an item
  that became moot, never for one that is still undone. Closed items never
  reopen.
- Check your open items on each wake and close the ones you can now observe.
