# Daemon crate (`rsid`)

The RPC method list is the match arms in `src/rpc.rs`. Per-family contracts
(Issue controls, ProgramRun, cohort settlement, harness manager v1/v2, operator
delegation, successor reservation, agent mail) are in
`docs/agents/rsid-rpc-contracts.md`. Read the section for a family before you
change it.

Facts that bite:

- The agent surface is exactly the `Agent*` verbs in `AGENT_VERBS`; every other
  method is default-denied to tokened callers. Operator-only families stay out
  of `AGENT_VERBS`, `READ_VERBS`, native tools and the agent CLI catalog, and
  catalog tests pin this.
- Write verbs (`AgentSendMessage`, `AgentManagerUpdate`, `AgentManagerControl`)
  are never in `READ_VERBS`.
- Caller identity comes from the transport token, not request JSON.
- `SwitchSessionModel` is deprecated; the model is fixed at session creation.
- A new production subprocess spawn (`Command::new`) or HTTP POST needs a model-control registry row ("Model-control registry" in `crates/rsid-store/AGENTS.md`); only the `rsi-model-control-validate` bin test fails without it.
- Claude env and manager tool preload (§ Claude launch environment and manager tool preload).
- Full `cargo test -p rsid --lib` takes over 10 minutes; scope to your modules.
- Size ceiling 8 KiB (`scripts/repo-guard agents-md-size`); detail goes in
  `docs/agents/rsid-daemon-facts.md`, at most one line here.

One line per area fact. The full text, with identifiers and issue history, is in
`docs/agents/rsid-daemon-facts.md`, section named in parentheses.

Agent mail and hooks (§ Agent mail and hooks):
- Mail `queued` is not delivered; expiry is 30 min (6 h if the recipient owns a wake or job); delivery boundary differs per provider.
- The boundary hook runs only the installed absolute `rsi-rpc` sibling, never `PATH`; unconfirmed mail settles `uncertain`.
- Headless Claude cannot answer `AskUserQuestion`; the PreToolUse hook denies it with a typed notice.

Deploys and shutdown (§ Deploys and shutdown):
- `AgentRequestDeploy` waits for a quiet point; `interrupt_workers` interrupts mid-turn workers and resumes them from the restart journal; landings and local builds still block.
- New `test`/`build` jobs are `held_for_deploy` while a deploy is staged; running tests keep running across the restart (#3554).
- A shutdown past `supervisor_shutdown_grace_secs` is ended by `scripts/rsid-supervisor.sh`; the next start files a `forced_shutdown` incident.

Topology executor (§ Topology executor):
- A settled execution carries `execution.outcome`; its wake appends a digest of at most 1.5 KB (`topology/outcome.rs`).
- A clean success settles quietly (`$.quiet_settle`, held up to 30 min); every other outcome wakes the owner at once.
- A reviewer that cannot run a check files an `infra_*` finding, which settles `failed(review_infrastructure)`, uncharged.
- The active reviewer reads only its own execution (`AgentTopologyGetExecution`, `receipts`); every other topology verb stays denied.
- A reviewer `blocked` by an `interrupt_workers` deploy is infrastructure, not a verdict.
- A malformed handoff (`handoff_invalid`) is continued once as the same session; a second fails the node.
- `structured_handoff` has the daemon-opened handoff turn schema-validated; a new handoff field goes into `FIELDS` in `rsi-common/src/structured_handoff.rs`.
- `topology:<execution>:<node>` Work rows belong to the execution, not a lead (`settle_review_work`).
- With a supplied off-rolling `base_commit`, the first implement node may hand off `complete` with no new commit.
- `already_satisfied` needs an ancestor `evidence_commit` on `origin/rolling`, a clean tree and no new commit.
- A dirty `complete` handoff is continued once, then blocks `preserved_work`; a parked session is `running`, not terminal.
- A `land_admission_lost` land attempt recovers with `ResolveTopologyAttempt retry`.
- Node questions park the attempt for the on-call manager; the ruling continues the same session once.
- A node that crosses the worker context cap ends with `PIPELINE HANDOFF — BATON <sha>`; the executor continues the node (max 3, uncharged), not `continue_from` (§ Context-cap baton).
- `dispatch:auto` Issues are launched by the daemon on the 15 s tick, once per Issue; kill switch `topology_executor_enabled`.

Host load and job resources (§ Host load and job resources):
- Host-load admission holds a manager's `create_session` and topology launches (`Reserved`); capacity never fails a node; tests default the load source to unsupported.
- A `build` job compiles one commit; a moved HEAD or newly dirty tree fails it `job_source_moved`.
- Job units with a private `TMPDIR` also get `GIT_CEILING_DIRECTORIES`, so fixtures never find the home repository.
- A cargo unit is bounded by `min(job ceiling, rsi-build.slice)`; `cpu_quota_percent` only lowers it; `job_thermal_ceiling_c` holds new jobs.
- macOS bounds cargo units itself (`host_memory.rs`, `held_for_capacity`).

Satellite QA (§ Satellite QA and QA target):
- The async full-library QA run goes to a healthy satellite; an uncertain start stays bound to its peer and run id and never runs twice.
- The one reused QA target is trimmed after each run; backstop `qa_target_max_gib`.

Health and maintenance (§ Health and maintenance):
- `when.health_anomaly` is evaluated every minute with no model call; a new condition needs a `KIND_*`, an `evaluate` arm and an injection test.
- Each maintenance lane logs one bounded `pass_cost` line per pass, never one per candidate (`journalctl --user -u rsid`).
- The 300 s settlement pass skips the audit of an unchanged cohort (`settlement_audit_fence.rs`); apply and holder proofs still run.

Sandbox and harness custody (§ Sandbox and harness custody):
- One lexical git scan (`rsi_common::custody_scan`) guards every provider family; `session/branch_custody.rs` restores a clean drifted branch at turn end.
- A Harness session may bind to one Terminal-Bench container (`exec_target`); only daemon code builds the target; tests use the fake docker shim.
- The bench adapter socket serves only `Bench*` methods, which are not in `AGENT_VERBS`; per-task USD cap `BenchTaskGuardedControl`.

Providers (§ Providers):
- OpenRouter spend cap `openrouter_spend_cap_usd` is enforced at launch, spawn and every model call; a stale probe fails closed; tests use a mock `OpenRouterApi`.
- A provider plan-window outage holds scheduled resumes until reset and returns launches to `Reserved`.
- The CLI contract canary pins launch flags: a new Claude flag goes into `CLAUDE_LAUNCH_FLAGS`, a new Codex hook into `codex_session_hook_overrides` and `_expectations`.
- `provider_skill_delivery = explicit` delivers only the loaded skill set (Claude `--plugin-dir`, Codex `skills.config`).
- `provider_lifecycle_hooks` and the SessionStart preamble share the per-session hook path; `none` keeps argv byte-identical.
- Only a fresh `live` Codex catalog may refuse a model (`model_unavailable`); a failed probe fails open.
- A Codex catalog is `Trusted` only if every consumed field is typed (`CODEX_CATALOG_MODEL_FIELDS`).
- Codex plan windows are polled into `provider_rate_limit_windows`; a failed read never errors a launch.
- Claude launches set `CLAUDE_CODE_TOTAL_TOKENS_REMINDER=off` (#3565).
- `RSI_OFFLINE=1` means no outbound connection; a new background probe, poll or fetch takes the same gate.
- Claude stream events raise typed health rows; a new project-wide notice subject goes in both prefix lists in `manager_notices.rs`.
- Subagent caps (`claude_subagent_model`, `*_max_concurrent_subagents`) are read at each spawn.

Session reads and handover (§ Session reads and handover):
- `AgentReadSessionEvents` alone admits a seat reading its slot's predecessor and an Issue-bound worker reading its reporter.
- `replace_lead`/`retry_lead` fork from the old lead's worktree, or from published `rolling` when its custody is gone.

Tests and tools (§ Tests and tools):
- A test file's helpers follow its shard gate; `scripts/repo-guard rsid-test-shards` refuses an ungated item in a gated module.
- `src/bin/rsi-e2e-seed.rs` is test-only (`e2e-seed` feature); never add a daemon-side import path.
