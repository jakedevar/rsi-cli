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
- Caller identity comes from the transport token, never from request JSON.
- `SwitchSessionModel` is deprecated; the model is fixed at session creation.
- Agent mail: `queued` is not delivered. The default expiry is 30 minutes from
  first acceptance. Claude sessions also get it at each tool boundary through the per-session PostToolUse hook (`rsi-rpc boundary-mail-hook` -> hook-only `ClaimBoundaryMail`, `session/boundary_mail.rs`, #1049); Codex CLI launches (Codex, Pioneer, Codex-routed OpenRouter/Bedrock) install the same hook with `--mail-only` as a `-c hooks.PostToolUse` override and trust exactly that hook with a `-c hooks.state` key+hash override (never `--dangerously-bypass-hook-trust`; `codex.rs`, #1183); the hook runs only the daemon's installed absolute `rsi-rpc` sibling, never one from `PATH` (no sibling, no hook); hook-claimed agent mail stays `claimed` until the hook calls `ConfirmBoundaryMail` after printing AND the provider transcript named in that call shows the hook context accepted (Codex `hooks.additional_context` item, Claude `hook_additional_context` attachment); anything else settles `uncertain` at `BOUNDARY_MAIL_CONFIRM_DEADLINE`; every Harness-loop session (Harness, Harness-routed OpenRouter/Bedrock) takes it between model calls (`harness/agent_mail.rs`). Local, Antigravity and CodexAppServer get mail only at turn end, and `AgentSendMessage` says so (`delivery_boundary: "turn_end_only"`). The same hook also spills large tool results (PostToolUse `updatedToolOutput` for Bash/Grep) and routes heavy Bash commands (cargo, make, test runners) through `rsi-rpc spill` at PreToolUse (#1097, `rsi-common/src/spill.rs`; a failing command is a `PostToolUseFailure`, which cannot rewrite output, hence the PreToolUse route).
- `AgentRequestDeploy` waits for a quiet point (no landing, local job or
  worker mid-turn). `interrupt_workers: true` (#1461) makes a worker mid-turn
  stop blocking once the `deploy_drain_hold_secs` hold is over, records and
  names the interrupted workers (V158 columns, `deploy_interrupt` friction) and
  resumes them through the restart journal; a landing or a local job still
  blocks. Contract: `docs/agents/rsid-rpc-contracts.md` ("Deploys").
- Host-load admission (#1417, `host_load.rs`): a manager's queued
  new `create_session` stays unclaimed (`claim_manager_action_with_create_admission`) and
  a topology node stays `Reserved` (`NodeEffects::launch_held`; every
  execution's later nodes, only the first launches of an operator execution
  are immediate, #1641 S4b) while `load + recent_admissions >
  host_load_admission_threshold`; only those two paths consult the gate; Issue
  worker continuations, topology retries and launches already in progress
  bypass it. Capacity pressure never fails a node: a `-32029` sandbox refusal
  or an ungranted governor build slot (command nodes) leaves it `Reserved` with
  one `admission_hold{kind}` event per transition. Eligible creates and topology waiters
  share the held order. In unit tests (`cfg(test)`, `test-seam`) the load
  source defaults to unsupported, so a loaded CI host never holds a test; a
  test of the hold installs one with `host_load().set_source(..)`. Contract:
  `docs/agents/rsid-rpc-contracts.md` ("Host-load admission").
- Topology node questions (#1641 S3b/S3c): a node's pending question, a BLOCKED
  handoff with `blocker_question`, and a review that ran out of rounds all park
  the attempt for the on-call manager. A BLOCKED handoff or an exhausted review
  files a `topology:` decision on the PM ledger (`topology/decision.rs`;
  `gate` follows `blocker_class`, so gate classes reach the operator); the
  ruling continues the same session. The executor registers a node waiting on
  an answer (`oncall::note_answer_wait`) and `stall_detector::stall_threshold`
  exempts it from every stall action; the execution deadline still applies (a parked
  decision is withdrawn and the attempt times out at it). The ruling's answer reaches the
  session through `NodeEffects::continue_with_answer` (`ContinuationIntent::TopologyAnswer`):
  the exact execution/attempt/decision binding is rechecked under the spawn guard and
  durably claimed (`delivery.state = started` in the attempt marker) before the provider
  effect; a claim with no sign the session took the answer is shown uncertain, never
  resent (#1715).
- The full `cargo test -p rsid --lib` run takes more than 10 minutes. Scope
  runs to the modules you touch.
