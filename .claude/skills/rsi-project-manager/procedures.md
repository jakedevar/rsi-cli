# PM procedures

Read only the section needed for the current action. Live verb schemas and
refusals come from AgentGetAuthorityCatalog {verb}.

## Runaway runs (#1337)

On 2026-10-06 a worker ran the full `rsid-store` suite and a broad `rsid` run
for about two hours at 12-14 cores each (host load 60-83 on 32 cores). The
manager saw it at 41 and 83 minutes and let it continue; the operator stopped
it. Do not repeat that:

- **Every wake, read the top CPU consumers**: `uptime`, then
  `ps -A -o pid=,etime=,pcpu=,comm= | LC_ALL=C sort -k3,3nr | head -n 10`
  on Linux or macOS (PID, elapsed time, CPU percentage and command; highest CPU
  first), and any `runaway_process` notice in your inbox. On Linux,
  `systemd-cgtop` also shows usage by service scope. The daemon's CPU-time andon
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
  your terminal watch once the session exists). Save `action.operation_id`,
  `worker_session_id`, action state and watch state together. `queued`/`running`
  with `watch:"pending_launch"` can precede session creation: use
  `AgentManagerGetAction {operation_id}` to inspect `held.reason` (for example,
  `disk_pressure`), address the hold and let the existing launch proceed.
  Do not use `AgentGetStatus` as a launch probe or add another terminal watch.
  If a probe returned `session_not_found`, recover through the saved action,
  not a guessed successor id or a duplicate launch. Once the session exists,
  use `AgentWorkerRoster`/`AgentGetProgress` for its current state; a terminal
  failed/blocked/revoked launch is handled from its action receipt and Issue
  note. A worker is a leaf; do not assign it as a lead. Read
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
- **Lost Epic lead (#3907).** Recover an empty lead slot or an archived/deleted
  lead with one `AgentManagerControl` `replace_lead` action, using the current
  lead fence, query and launch choice. The daemon publishes lead authority
  before the replacement's first provider turn. An empty slot forks published
  `origin/rolling`; a retained predecessor uses its normal handover source.
  Do not use `create_session` followed by `assign_lead` for this recovery: the
  intervening first turn has only worker authority. If the source is unavailable,
  restore the Epic repository/published `origin/rolling` and retry `replace_lead`.
  A failed launch retains the replacement as the recoverable lead; inspect its
  action receipt before deciding the next recovery.
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
  The appointed manager submits integration and release builds through
  `AgentSubmitJob` with the integration worktree as `worktree`; workers cannot
  submit jobs there (`job_directory_not_allowed`). Integration jobs get a
  registered target
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
  green and refuses `claimed_fix_still_red` otherwise (#2167). Expedite an
  incident or red-QA fix that cannot wait behind the queue with `expedite:
  {reason}` (appointed manager only; reason required, 1-256 bytes, recorded and
  shown in `AgentListRollingQueue`): the entry is admitted into the next batch
  ahead of ordinary queued entries, and never preempts a gating batch. At most
  2 live expedited entries per project (`queue_expedite_cap_reached` lists
  them); a `fixes_issue` naming an open `qa` Issue expedites by itself, and past
  the cap stays ordinary with an `expedite_warning`. Never reorder the queue
  with hand-built merge commits or `supersedes` bundles of unrelated changes
  (#3487). The lander merges, compiles, runs
  the filtered tests and publishes fast-forward only; do not run a separate
  workspace check, hand-pick tests, merge by hand or push `rolling` yourself.
  The queue wakes you once with the outcome (published or refused, with
  `gate_timing`) for every landing you enqueue, even a worker's commit named by
  `source_session_id` (the worker is never woken): arm no `sha_on_rolling` for
  your own enqueue, that is a second manager turn for one landing (#2405). Keep
  `sha_on_rolling` and `issue_closed` for a dependency's landing you did not
  enqueue (several SHAs: one `shas_on_rolling` wake with `fire:"each"`, which
  fires once per landing; the seat cap is 16 `when` wakes, `wake_when_cap_reached`), and `jobs_terminal` for jobs: one `mode:"when"` wake, never one wake
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
  `scripts/repo-guard rolling-migration-renumber` (file name, `migrate_vNNN`, the
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

- A hub brief asking the satellite to converge on, build from or review a hub
  source names a fetchable commit: one on `origin/rolling`, or on a pushed
  branch named in the brief. When that is impossible, give a portable content
  fallback (paths plus blob ids the satellite can check). Write hub Issue
  numbers as `hub #N`: bare `#N` means a satellite Issue, and the satellite
  cannot read hub Issues (#292). A satellite manager receiving an unfetchable
  source (`upload-pack: not our ref`) reports it once with `AgentReportToHub`
  kind `result`, naming the SHA, and proceeds on a blob fallback when one exists.
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
  `AgentReportToHub` still accepts a report while the hub is not pulling: the receipt is
  `queued` with reason `hub_not_pulling` (#530), it keeps its place in the daemon's durable
  backlog and goes out in arrival order on the next pull, so do not hold reports in /tmp
  files or re-send them by hand (the `hub_not_pulling` anomaly is the alarm); at 32 pending it refuses `satellite_report_queue_full` with
  the depth, oldest pending age, per-kind counts and `last_hub_pull_at` (#135), so wait
  for acknowledgements and never re-send a pending ENQUEUE or DEPLOY-READY. OUTBOX.md stays the fallback you read on every wake.
- **Satellite ENQUEUE intake is the daemon's (#3345).** A parseable `ENQUEUE <sha>
  [branch=] [issue=#N] [fixes=#N]` report is fetched, probed against rolling and the
  queue, enqueued with `derive_filters` and answered to the satellite by the daemon
  (`docs/satellites.md`, "ENQUEUE intake"); do not fetch, `merge-tree`, enqueue or
  mail "PUBLISHED" by hand. Read the `satellite_intake:*` digest notices; they wake
  you only for a held pre-merge review (migration, credential/IAM/network), a refusal,
  a stalled intake or a source wanting a post-land review. Hold a source with
  `rolling_queue_enabled` off or `AgentCancelRollingQueueEntry`. An ENQUEUE the grammar
  does not parse is still a plain notice you handle by hand.
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
  1. The laptop manager submits both pinned release build jobs from its local
     integration worktree and verifies their completed receipts as described in
     "Deploys" below. Before launching a deploy worker, check
     `AgentGetAuthorityCatalog {verb:"AgentSubmitJob"}`: `worktree` is
     appointed-manager-only (#661), and a brief grants no authority. A worker
     may prepare/check disk and source, then stage and verify using the receipts;
     its brief must not assign either worktree build job. If the receipts are
     not ready, finish preparation and return to the manager for the builds.
     A staging brief supplies the exact SHA, clean source worktree path, both
     successful build results (including `source_head`, `source_dirty` and
     `target_dir`), and a new absolute staging path; do not require the worker
     to read or watch manager-owned jobs. Stage from that worktree with
     `scripts/install-release.sh --stage-dir ~/.rsi/staging/bin-<sha>
     --link-only --build-receipt <receipt.json>` (an allowed deploy root there;
     `--no-restart` alone still overwrites the live install, #136).
     DIR must be new or empty (a non-empty DIR is refused) and becomes the
     `binaries_dir`; its layout is in "Deploys" below
     (#262). The worker returns the verified absolute `binaries_dir` and SHA;
     the laptop manager then reports DEPLOY READY with `AgentReportToHub {kind:
     "deploy_ready", text: "DEPLOY READY <sha>", binaries_dir:
     "<absolute staged path on the satellite>"}`. Expand `~` on the satellite
     before sending; the satellite daemon also expands a leading `~/`, resolves
     symlinks, and requires an existing directory under its staging root. The
     report's typed `binaries_dir` reaches the hub as that absolute path.
     A satellite daemon older than the typed field refuses it (`invalid_input` at
     `params.binaries_dir`): send the same report without the field and put one
     `binaries_dir=<absolute path>` token in the text (no `~`, no `..`, no
     spaces). The hub records it as the report's `binaries_dir` with
     `binaries_dir_source: "legacy_text"` and a `legacy_note`; without a valid
     token it refuses `satellite_report_binaries_dir_required_add_binaries_dir_to_text`
     (#237).
  2. The hub copies `binaries_dir` from the report in its inbox and restarts it
     over the link: `AgentRequestDeploy {sha, binaries_dir:
     "<absolute binaries_dir from the report>", idempotency_key, peer_id: <the satellite's peer id>}`
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

## Cloud (AWS): within the daemon spend caps

- Standing directive 3a2ec445 replaces the 2026-10-08 "no AWS" rule: AWS runs
  are allowed within the daemon's cloud spend settings
  (`cloud_spend_daily_cap_usd`, `cloud_spend_stop_line_usd`; operator-only,
  mirrored to `~/.rsi/cloud/spend-caps.json`). Plan: `thoughts/shared/plans/2026-10-21-3460-aws-throughput.md`.
- Spot only (max $0.75/h, about $0.62/h observed). `scripts/cloud-gate.sh`
  reserves `lifetime x $0.7813/h` in `~/.rsi/cloud/spend.md` before it applies
  and refuses at either cap; the reservation is replaced by the actual cost at
  stop. A host AWS takes back ends the job without a verdict
  (`spot_interrupted`, INCOMPLETE): rerun it; it never means red.
- Use `AgentSubmitJob` `cloud_sweep` for full-library QA (`make
  rolling-library-qa`, 120-minute host, about $1.56 reserved) when the hub or
  the satellite is saturated. The `cloud_gate` kind is not used: it lands
  outside the merge queue.
- Build lane (#3844), Python-free: the same `cloud_sweep` kind with
  `"lane":"build"` builds one rolling-tip SHA on one spot host (60-minute
  lifetime, about $0.78 reserved), then destroys it. Submit it as the manager
  or Epic lead (same authority and admission as the QA sweep):
  `AgentSubmitJob {"kind":"cloud_sweep","wake":"none","params":{"sha":"<40-hex
  current rolling tip>","lane":"build"}}`, then one `AgentScheduleWake
  {"mode":"when","when":{"jobs_terminal":["<job id>"]},"timeout_seconds":5400,
  "message":"cloud build finished: read the receipt"}`. The host runs
  `cargo build --workspace --locked`, then `cargo nextest run -p rsi-common -p
  rsid-core --lib` (`cargo test` if nextest is absent). The job result's
  `sweep.verdict` is `GREEN`, `RED` (`sweep.build.failing_step` is `build` or
  `test`) or `INCOMPLETE` (never red): a spot interruption, a failed `checkout`
  or `toolchain` step, or a typed refusal (`cloud_prereq_missing_<what>` for
  terraform, aws, the aws profile, rsi-cloud, `gate.tfvars` or the ssh
  identity; `cloud_spend_refused`; `cloud_gate_busy`;
  `sweep_sha_not_rolling_tip`). `sweep.build` carries the step exit codes,
  instance type, host seconds, reserved and actual USD from the ledger and the
  leftover-instance check; the tails are in `~/.rsi/cloud/results/build-<sha>/`.
  The first real run follows the deploy of this lane and the operator's IAM
  steps; no `.py` runs on this lane, hub or host.
- Never spend past the settings or raise them: the caps are the operator's.
  Read the ledger and cap lines in the receipt; a `cloud_spend_refused` refusal
  means wait for the next UTC day or report the cap up.

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
after the restart). A peer deploy (`peer_id`) defaults to interrupt-and-resume
once the satellite's `deploy_drain_hold_secs` has passed (default 600 s).
Omit `interrupt_workers` for that default; explicit `interrupt_workers: false`
keeps the wait-for-lull behaviour. Local hub deploys still default to waiting
for a lull; use `interrupt_workers: true` when interruption is warranted.
Keep `max_wait_secs` above the hold. Ordinary test/scoped-test jobs requeue
under the same id and wake after at most the lesser of 300 s and the hold;
landing, release-build and protected jobs still block. The receipt names
expected interrupted sessions and requeued jobs while staged, then actual
interrupted sessions and `requeued_jobs`; the outcome wake names both actual
sets. The restart journal resumes interrupted turns with a continue prompt.
A satellite build without `deploy_interrupt` answers
`satellite_deploy_interrupt_unsupported`; explicit false remains compatible.
Send the same SHA and idempotency key with `cancel: true` (omit
`interrupt_workers`) to withdraw a deploy you no longer want to wait for; a held create shows
`held.reason: deploy_draining` in `AgentManagerGetAction`. Until those grants exist,
use the manual steps below or the hub script `~/.rsi/mgr/deploy-hub-c7b1-v2.sh`
(waits for the build and a quiet point, backs up the DB, restarts with the TUI
environment). The satellite has no manual restart of its own: see "Satellite
(arch-laptop)". Never SIGSTOP an rsid supervisor: a stopped parent cannot reap the
exited rsid, which then shows as a `<defunct>` zombie that `pgrep` still
finds. Stop it with SIGTERM to the supervisor (its trap forwards TERM).

1. The appointed manager on the build host submits ONE release build job from
   the recurring detached integration worktree at the deploy SHA. Check the
   live `AgentGetAuthorityCatalog {verb:"AgentSubmitJob"}` before delegating:
   `worktree` is manager-only, so workers only prepare or stage/verify with
   completed receipts; never assign them this job submission (#661).
   Never use `workspace:true` for a deploy release: Cargo would unify
   `rsi-repo-guards`' `serde_json/preserve_order` into `rsid`, changing Maps
   and starter digests. Never submit per-package release jobs either
   (`package`+`bins`, one job per member): they share one release target, and
   a batch of nine left only the last two members' binaries in `release/`
   (#3993), so the installer refused for a missing `rsid`. The daemon refuses
   a manager's package-scoped or `workspace:true` release `build`
   (`job_invalid_params`, `params.package` or `params.workspace`, #4045) and
   names this route. Submit the aggregate
   `release_members` request (the inventory is the member list):

   ```json
   {"kind":"build","worktree":"<integration-worktree>","wake":"owner",
    "params":{"command":"build","release":true,"sha":"<deploy sha>",
     "release_members":[
      {"package":"rsi","bins":["rsi"]},
      {"package":"rsid","bins":["rsid","rsi-build-rustc","rsi-rolling-land"]},
      {"package":"rsi-common","bins":["rsi-rpc","rsi-agent-mcp","rsi-contract-validate","rsi-known-failure"]},
      {"package":"rsi-remote","bins":["rsi-remote"]},
      {"package":"rsi-turn-shim","bins":["rsi-turn-shim"]},
      {"package":"rsi-socket-hold","bins":["rsi-socket-hold"]},
      {"package":"rsi-cloud","bins":["rsi-cloud"]},
      {"package":"rsi-repo-guards","bins":["rsi-repo-guard"]},
      {"package":"rsi-eval","bins":["rsi-eval"]}]}}
   ```

   Omit `package`, `bins` and `workspace` at the top level. Each member runs in
   its own Cargo invocation under one build slot, one daemon-selected target
   and one clean source fence, preserving the installer's feature isolation
   (rsid never shares a Cargo invocation with `rsi-repo-guards`); the first
   failure stops the job. Start from a clean tree at the pinned SHA and do not
   move or edit the integration worktree while it runs (`job_source_moved`).
   Verify the receipt: exit 0, no refusal, `source_head` equal to the deploy
   SHA, `source_dirty:false`, the `target_dir` matching the log's
   `rsi-build-target` line, and complete `result.release_members` coverage.
   This aggregate receipt is the installer's staging input, which refuses
   missing or incomplete member coverage, including stale binaries in a warm
   target. Before the daemon with `release_members` is live (the catalog does
   not list it), there is no supported multi-member route: do not fall back to
   per-package jobs or a workspace build (the daemon refuses both); wait for
   that daemon and say so in the handoff.
   The release target remains warm for the next deploy; never substitute a
   shared or ad-hoc Cargo target. Installed binaries live where the installer
   links them; `AgentRequestDeploy` installs into `~/.local/bin`. Targets
   backing installed links remain live consumers and must be retained.
2. Quiet point: no lander or integration push mid-flight, and no worker you
   care about mid-turn. Tell the operator first; their TUI reconnects.
3. Submit `AgentRequestDeploy` using the verified binaries under the job's
   daemon-selected target and the pinned SHA. Stage them first, from the
   deploy worktree, with `scripts/install-release.sh --stage-dir
   ~/.rsi/staging/bin-<sha9> --link-only --build-receipt <receipt.json>` (#157;
   the build job leaves no installer record, so pass its result JSON: `exit_code`
   0, `source_head`, `source_dirty` false, and `target_dir`. The script links only
   when `source_head` is the deploy worktree's HEAD, the tree is clean and the
   staged rsid embeds that sha; `--allow-foreign-build` is no longer needed and
   turns the check off). The script looks for the binaries in the receipt's
   `target_dir`, the target the job really built in (#282), so your shell's own
   stamped `CARGO_TARGET_DIR` (a sandbox target) does not matter. That path is
   untrusted input (#3513): it must be absolute and, with symlinks and `..`
   resolved, equal `~/.rsi/staging/release-target` or your checkout's own cargo
   target, or the script refuses before any lock, binary run or copy. A receipt
   without `target_dir` (a job from before #282) is refused unless you pass
   `--target-dir <the job log's rsi-build-target line>` (absolute, canonical, an
   existing directory; #3514). When the binaries are missing the error names the
   expected path, never `--allow-foreign-build`). It adds every non-cargo member at the top level:
   `scripts/rsid-supervisor.sh`, the full `scripts/cargo-slot`,
   `scripts/target-pool` and `scripts/rsi-spill` (#2958).
   `rsi-repo-guard` (the Rust repo-guard executable the lander and queue
   probe run as a bounded sibling and fail closed without, #451) must be built
   in a separate Cargo invocation from `rsid`, because `rsi-repo-guards` enables
   `serde_json/preserve_order` and Cargo unifies features across one build,
   which reorders every `serde_json::Map` in `rsid`. Step 1's aggregate
   `release_members` job enforces that separation; verify its receipt before
   staging. A target without it stages without it
   and the deploy receipt names it in `skipped`. Never hand-copy a
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
   Staging needs no Python (#3742): receipt parsing, the target lock, path resolution and the
   manifest use bash, awk, `flock` and `sha256sum`/`shasum`; do not hand-roll a staging script to avoid it.
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
    `relative_path`, `blob_oid`). A seat with no sandbox (a reseated seat
    in the shared root) never commits there: it writes the commit with
    plumbing in its working directory (`git hash-object -w <file>`,
    `printf '100644 blob <blob>\t<name>\n' | git mktree`,
    `git commit-tree -m handoff <tree>`) and points
    `git update-ref refs/rsi/manager-handoff/<your session id> <commit>` at
    it; the daemon anchors the handoff at that ref (#172).

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
  List pending launches separately from running/parked workers, with each
  operation id, reserved worker id, last observed action/watch state and hold
  reason. Carry the worker's current tip and observed state for follow-up work;
  the successor refreshes them before mail or continuation, rather than treating
  handoff ids as proof that a worker exists or is still live.
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
  (`rg <operation_id>`). Sandbox refusals now have typed outcomes: a free-space
  shortage holds the create as `sandbox_free_space`; the root-count cap ends
  `manager_v2_sandbox_source_root_limit`. A sandbox `git worktree add` timeout
  (#495) cleans its unpublished allocation and holds the same create for 30 s
  as `sandbox_git_timeout` (`held.release_by` names the retry time). The ordinary
  admission loop retries it at most three times; do not relaunch a queued hold.
  Exhaustion ends `blocked` / `sandbox_git_timeout`, and `next_step` says to
  relaunch under a new key. `sandbox_git_timeout_cleanup_required` means residue
  could not be safely removed: inspect and clean it before relaunching.
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
