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
  first acceptance. CLI providers receive mail only at idle boundaries.
- The full `cargo test -p rsid --lib` run takes more than 10 minutes. Scope
  runs to the modules you touch.
