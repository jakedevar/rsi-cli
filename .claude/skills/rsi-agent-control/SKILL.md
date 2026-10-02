---
name: rsi-agent-control
description: Strategy for driving the rsi daemon from inside an rsi-managed agent session. Start with AgentGetAuthorityCatalog (it serves your roles, role rules, permitted controls, schemas, examples and refusal codes); this skill keeps only retry and idempotency strategy, debugging authority refusals, token lifecycle and the spawn-directive fallback. Use when a control call is refused, a retry is unclear, or native tools and rsi-rpc are unavailable.
---

# rsi agent control

The daemon serves the mechanics. This skill is only the strategy that is not
derivable from it.

## Start here

Call `AgentGetAuthorityCatalog` (native `rsi_control_authority_catalog`; Claude:
`mcp__rsi-agent__rsi_control_authority_catalog`; CLI `rsi-rpc
AgentGetAuthorityCatalog`).

- With `{}` it returns your current roles, the daemon-shipped operating rules
  for exactly those roles, and the controls you may call now (plus, for a
  manager, the update variants and actions your grant allows).
- With `{"verb": "<name>"}` it returns only the compact envelope
  (`schema_version`, `session_id`, `authority_revision`, `roles`, `pending`) and
  that one control's detail: whether you may call it (`permitted`), its
  parameter `schema`, one minimal valid `example` request, and its stable
  `refusals` (`code` plus `next_action`). No control list or guidance repeats.
- It is read-only, open to every session, and grants nothing. Every call is
  re-checked against current state. Call it at the start of work and again after
  a role change or an authority refusal. `pending: true` means the initial role
  publication has not committed: keep to the worker baseline and call again
  shortly.

Everything else you need about a verb (shape, scope, fences, examples, refusals,
role rules) is in that response. `rsi-rpc agent` lists the verbs offline and
`rsi-rpc <Verb> --schema` prints one request schema without a daemon or a token.
Do not restate verb lists or counts anywhere; they drift and the catalog is the
source of truth.

## Transport and identity

- Prefer the native `rsi_control_*` tools when your provider lists them. The
  fallback is `rsi-rpc <Verb> [--params JSON]`; use `--params @file` for
  multi-line payloads.
- The daemon resolves your identity from `$RSI_SESSION_TOKEN` (set by the daemon,
  used by `rsi-rpc` automatically). The token is transport-only: never type it
  into `--params`, a prompt, a file, a log or a message. Never send caller,
  Epic, lead, parent or permission fields; requests are strict and reject them.
- Use only the controls the catalog lists for you. Any other method is denied to
  an agent. Registration of a native tool is not authorization: authority is
  checked on every call.
- Scope in one line: an Epic lead acts on its Epic's children; any leaf acts on
  itself and its own direct children; the appointed manager acts within its
  granted live scope.

## Token lifecycle

Every session-establishment path (fresh launch, continue on a terminal session,
handoff-write resume, rotation-child spawn) re-mints `RSI_SESSION_TOKEN` and
revokes the prior one. A superseded token fails `-32602
agent_verb_unknown_session_token`. The registry is in memory: after a daemon
restart only sessions established since then hold a live token, so a session
that was not re-established must be continued before it can call verbs. A failed
rotation-child spawn leaves the parent's tokens intact. Lead promotion mints no
token; the recipe is promote, then rotate.

## Retry and idempotency strategy

- **Write verbs take an `idempotency_key`.** Choose a stable key per intended
  effect. An exact replay returns the original receipt (`deduplicated: true`)
  even after later mutations or a daemon restart; changed content under the same
  key fails closed (`*_idempotency_conflict`). For a revised request use a new
  key; for a retry after an uncertain outcome reuse the same key with identical
  content.
- **Uncertain is not failed.** A timeout or transport error on a write may have
  committed. Retry with the same key and identical content, or read the state,
  before issuing a new decision or a new launch.
- **Stale is not retryable as-is.** `stale_*` and fence refusals mean the state
  moved: re-read (`AgentGetProgress`, the Issue, the manager inspect page), adopt
  the observed cursor or version, re-decide whether the action still applies,
  then retry. Never guess a new version, cursor or target.
- **Queued is not delivered, accepted is not done.** A queued receipt proves only
  its recorded state. Inspect the resulting session or action before reporting an
  effect.
- **Do not poll.** Dispatch the work (child, job, review, landing) and end the
  turn; rely on the event wakes (terminal watches, job and lander completion,
  manager mail, `AgentScheduleWake`). A self-wake is always `mode:"resume"`.
- **Automatic session retries are fail-closed.** The daemon applies no default
  retry budget to any session kind: a positive budget comes only from an explicit
  session retry policy or launch `max_retries`, and `retry_enabled=false` is a
  live kill switch. Cancelling a retry (continue, interrupt, `AgentHalt`,
  `CancelRetry`) persists exhaustion so a daemon restart cannot resurrect it.

## Debugging authority refusals

1. Read the refusal `code` and `next_action`; they are stable and safe to act on.
2. Call `AgentGetAuthorityCatalog {"verb": "<the verb>"}`. If `permitted` is
   false you do not hold the role or grant now: refresh the roles and stop
   retrying. If it is true, compare your request with the returned `example` and
   `schema` and check the code against the returned `refusals`.
3. A refusal on a control you were just able to call usually means a role,
   custody, pause or policy change: call the catalog with `{}` again and compare
   `authority_revision` and `roles`.
4. `authority_catalog_unknown_verb` means the name is not a control: pass an
   `Agent*` method, an `rsi_control_*` tool or its `mcp__rsi-agent__` spelling.
5. A refusal that is intentionally uniform (for example a satellite
   `target_not_authorized`) hides which condition failed; check the grants and
   scope the operator declared, do not probe.
6. A code the table does not list is still a real refusal; report it with the
   verb and code rather than retrying blindly. File a durable follow-up with
   `AgentCreateIssue` when it points at a defect.

Human approvals, appointment, scope and policy changes are operator-only; no
agent verb, mail or grant answers or clears them.

## Directive emit-and-detect fallback

When native tools and `rsi-rpc` are both unavailable, a lead session may still
drive spawn and halt by emitting `<docregblock>/spawn_child ...</docregblock>` (or
`<docregblock>/halt</docregblock>`) in assistant text; the daemon detects and
enqueues it (`crates/rsid/src/session/types.rs` regexes, scanned in
`session/monitor.rs`, parsed by `session/spawn_directive.rs`, pinned by the
`session::spawn_directive` detection tests). Its `/spawn_child` header accepts the
same optional `provider=<SessionProvider>` as `AgentSpawnChild`, plus optional
`agent_role=<role>`; use the JSON or native transports for roles containing
spaces. Prefer the native tools and verbs whenever they are available. If no
control surface exists at all, treat worker spawn as unavailable and compile the
next prompt for the user.

## Conventions

`GetSession` keys on `session_id` (not `id`). For multi-line spawn or wake
payloads prefer `--params @file` over inline JSON.
