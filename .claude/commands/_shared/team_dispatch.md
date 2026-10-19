# Team worker dispatch backend

Shared by `team_research`, `team_plan` and `team_implement`. Read once; do not
copy it into the commands.

## Inside an RSI session (the default)

You are in an RSI session when the worker preamble carries the "RSI
orchestration backend policy" or `AgentGetAuthorityCatalog` is available.
Dispatch, observe and steer workers only through RSI controls:

- **Dispatch:** `rsi_control_spawn` / `AgentSpawnChild` (a manager launching an
  Issue worker uses `AgentManagerLaunchIssueWorker`). Pass each worker its
  prompt from the command's template. Send every worker in a tier in one batch
  of calls.
- **Observe:** `AgentGetProgress` / `AgentGetStatus` for state,
  `AgentReadSessionEvents` for the worker's final message, and
  `AgentScheduleWake` with `mode:"on_terminal"` (or `mode:"when"`) to be resumed
  when the batch ends. End the turn after dispatching; never poll or sleep.
- **Steer:** `AgentSendMessage` for a mail note, `AgentContinueChild` only to
  deliberately interrupt and replace a running turn.

Provider-native `Agent`, `Task`, `TaskOutput` and `run_in_background` are not RSI
worker dispatch; do not use them here. A call that does not route through RSI
authority claims no worker ran.

## No child authority (the catalog lists no spawn control)

Do not retry, ask for permission or loop. Finish the bounded work yourself in
this session, one domain or phase at a time, following the command's worker
prompt as your own checklist. If the user asked for a delegation prompt, emit
it instead. If the user explicitly required parallel workers, say in your
report that worker spawn was unavailable and what you did instead. Never state
that workers ran.

## Outside RSI (no RSI controls, no RSI backend policy)

Use the provider's native background-agent mechanism (for example Claude Code's
`Agent` tool with `run_in_background: true`, results via `TaskOutput`).
