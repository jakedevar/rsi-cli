# RSI Remote API v2 contract (#2962)

API v2 is what the native iOS client (#2961) talks to, through the same
`rsi-remote` gateway, the same Tailscale Serve route and the same ingress as the
browser view. **v1 is unchanged and keeps working.** v2 adds five routes:

1. the exposed projects, each with its project-manager seat and the covering
   global (portfolio) manager seat;
2. one manager seat's conversation, newest first, with a backward cursor;
3. one write: a message to a manager seat, delivered exactly like the TUI's
   operator message;
4. the dashboard (hub #3287): the satellites the hub polls and the running
   agents per provider and effort, labels and counts only.
5. the providers (hub #3796): each provider's plan windows, overage posture and
   1h/24h spend, labels and numbers only.

The waiting-decision reads (`/api/v1/.../decisions`, `.../decision-targets`) and
the answer writes (`.../decisions/answer`, `.../decisions/answer-pending`) are
v1 routes and are what the app keeps using for decisions.

There is no new listener, port, Funnel or credential. Every request still passes,
in order: root peer credentials on the ingress socket, the strict request parser,
fresh `LocalAPI` readiness plus a per-request client `WhoIs` (a device not in
`allowed_node_ids` gets the same fixed `403 forbidden` as ever), then the
session checks below, then the policy of *this* request (the policy file is
re-read per request). Every denial is tested to dispatch nothing to the daemon.

The wire types are `rsi_common::remote_manager_v2`; the fixtures under
[`remote/contracts/v2/fixtures`](../../remote/contracts/v2/fixtures) are produced
and checked by the gateway tests (see "Fixtures").

## Native client binding

A browser holds the gateway session as `HttpOnly` cookies and proves origin with
`Origin` or Fetch Metadata. A native client has no Fetch Metadata and no page
origin, so it binds the same protections explicitly. **Nothing about the browser
path is weakened**; a native client is just a client that sends what a browser
sends by itself.

1. **Bootstrap.** `GET /auth/bootstrap` (no cookie needed) answers
   `200 {"nonce":"<64 hex>"}` and `Set-Cookie: __Host-rsi_boot=<64 hex>; Max-Age=60; …`.
   The boot cookie and nonce live 60 s and work once.
2. **Session.** `POST /auth/session` with `Content-Type: application/json`,
   `Origin: https://<canonical_host>`, `Cookie: __Host-rsi_boot=<boot>` and body
   `{"nonce":"<nonce>"}` answers `200 {"csrf":"<64 hex>"}` and
   `Set-Cookie: __Host-rsi_remote=<64 hex>; Secure; HttpOnly; SameSite=Strict` (and
   clears the boot cookie). **The `Origin` header is mandatory**: without it, or
   with another value, the answer is `403` and the boot nonce is spent (bootstrap
   again). `<canonical_host>` is the host in the URL the operator opens (the
   settings page shows it); it is the exact `Host`-without-port of the Serve URL.
3. **Every later request** carries `Cookie: __Host-rsi_remote=<session>` and
   `Origin: https://<canonical_host>`. A request with neither a matching `Origin`
   nor `Sec-Fetch-Site: same-origin` is `401`; the app sends `Origin` on every
   request (reads included) and never relies on Fetch Metadata.
4. **Writes** (`POST …/messages`, and the v1 answer routes) additionally carry
   `x-rsi-csrf: <csrf>` (the token from step 2) and **must** carry the exact
   `Origin` header (Fetch Metadata alone is not accepted for a write). A missing
   or wrong CSRF token or Origin is `403`; a missing or unknown session is `401`.
5. **Lifetime.** A session is bound to the device (`WhoIs` node), the owner and
   the server node. It expires after 30 minutes idle or 8 hours absolute, at most
   64 sessions exist, and the gateway forgets all of them when Remote is disabled
   or the policy becomes unreadable. On any `401` the client re-bootstraps (steps
   1–2). `POST /auth/logout` (cookie, CSRF and Origin) ends a session.

Cookie handling: the cookies are `Secure` with the `__Host-` prefix and no
`Domain`. `URLSession`'s cookie storage accepts them over HTTPS; a client that
manages cookies itself stores the two `Set-Cookie` name/value pairs and replays
them in `Cookie`. The CSRF token is never a cookie. Keep both secrets in memory
only (do not write them to disk or logs).

## Routes

| Method | Path | Needs | Daemon call |
| --- | --- | --- | --- |
| `GET` | `/api/v2/projects` | session, `Origin` | `RemoteListManagerSeatsV2` |
| `GET` | `/api/v2/projects/{project}/managers/{seat}/conversation[?before=<sequence>:<id>]` | session, `Origin` | `RemoteGetManagerConversationV2` |
| `POST` | `/api/v2/projects/{project}/managers/{seat}/messages` | session, exact `Origin`, CSRF, `allow_messages` | `RemoteListManagerSeatsV2` (seat check), then `RemoteSendManagerMessageV2` |
| `GET` | `/api/v2/dashboard` | session, `Origin` | `RemoteGetDashboardV2` |
| `GET` | `/api/v2/providers` | session, `Origin` | `RemoteGetProvidersV2` |

`{project}` and `{seat}` are lowercase hyphenated UUIDs. The route table is
closed: no request-supplied method name is forwarded, and `cursor` and
`pending_after` are not accepted on v2.

### `GET /api/v2/projects`

Lists exactly the projects in the policy's `project_ids` that exist. The `403`
for a project outside the policy applies to the per-project routes.

```json
{"projects":[{
  "project_id":"<uuid>","name":"Alpha",
  "project_manager":{"session_id":"<uuid>","role":"project_manager",
    "status":"Running","provider":"Claude","model":"claude-opus-5-5",
    "last_activity_at":"2026-10-17T09:30:00.000000000Z",
    "pending_decisions":2,"pending_decisions_truncated":false},
  "global_manager":null
}],
"messages_enabled":false}
```

- `messages_enabled` (top-level boolean, always present) is the policy's
  `allow_messages` as the gateway read it for this request. It lets a client
  show messaging as off (for example, a disabled compose box) before it sends.
  It is informational and read-only: the gateway still checks the switch on
  every send, and a send while messaging is off is still the bare
  `403 forbidden` in the error table below, with no new body or field. The
  switch is read per request, so a client that gets a `403` on a send can
  re-list to learn whether messaging was turned off.

- `project_manager` is the project's appointed (current) manager seat, `null`
  when the project has none or its grant is revoked.
- `global_manager` is the active portfolio node covering the project (the
  root-most one whose seat session is live), `null` when none is appointed. Its
  `pending_decisions` is `null`: it has no per-project decision ledger.
- `status` is the session status (`Starting`, `Running`, `WaitingApproval`,
  `Completed`, `Failed`, `Interrupted`); `provider` and `model` are the session's.
  `last_activity_at` is the session row's last update.
- `pending_decisions` counts the waiting question/approval decisions in the
  project's manager ledger (capped scan; `pending_decisions_truncated` when the
  cap stopped it). Read and answer them with the v1 decision routes.
- A seat is identified by its **current session id**. A seat session can change
  (context-cap rotation, succession); a message or conversation read for a
  stale id fails (`404`/`403`) and the client re-lists.

### `GET …/conversation`

Newest first, 25 items per page: message events and, since #3797, tool-call
events (`kind`: `message` (default, omitted), `tool_use`, `tool_result`):

```json
{"seat":{…},
 "items":[{"id":"904","sequence":4,"speaker":"manager",
   "text":"…","truncated":false,
   "created_at":"2026-10-17T09:24:00.000000000Z","operator_message_id":null}],
 "next_before":"3:903","has_more":true,
 "operator_messages":[{"message_id":"<uuid>","state":"delivered",
   "created_at":"…","updated_at":"…","delivered_at":"…","text_len":32}]}
```

- `speaker`: `operator` (a message the operator queued and the daemon delivered:
  from the TUI or from this API), `manager` (the seat's own assistant message),
  `system` (a daemon-written user-role message such as a wake prompt or agent
  mail). The distinction is the event metadata, not the text.
- `text` is cut at 4096 bytes (`truncated: true`). Provider diagnostics and
  thinking are not included.
- A tool item (`kind` `tool_use` or `tool_result`, speaker `manager`) carries
  `tool_name` and `tool_use_id`, the provider call id: a `tool_use` and its
  `tool_result` share one. The id is exact or absent (an id over 256 bytes is
  omitted, never cut). `text` is the stored, clipped content; the tool input
  column is never returned. Tool items count against the 25-item page and use
  the same `next_before` cursor. A message item has no new field except
  `idempotency_key` on an operator item sent from this API.
- `operator_message_id` on an operator item, and `message_id` plus
  `idempotency_key` (the key the phone sent) on `operator_messages`, let the
  outbox mark an entry delivered; `idempotency_key` is absent for a message
  queued elsewhere.
- Walk back by passing `next_before` as `?before=` unchanged. `next_before` is
  `null` and `has_more` false on the oldest page. The cursor is an anchor (the
  event's `sequence` and `id`), not an authority: every page re-proves the seat.
- `operator_messages` (first page only, up to 10 newest first) is the delivery
  view for the client's own sends (below). It never repeats message text.

### `GET /api/v2/dashboard`

The satellites the hub polls and the hub's running agents, read in one bounded
daemon call (`RemoteGetDashboardV2`, params `{}`). It takes no query (any query
key is `400 invalid_request`) and names no project, so it is not limited by
`project_ids`: the bound session is its whole scope (see "Exposure note").

```json
{"observed_at":"2026-10-19T19:30:00.000000000Z",
 "satellites":[{"label":"mac-mini","reachable":true,
   "build_sha":"a077ca3b02ae7d9de512c24d56a5803d88f703e5",
   "last_seen_at":"2026-10-19T19:29:40.000000000Z","running_agents":3}],
 "satellites_truncated":false,
 "agents":[{"provider":"Claude","effort":"high","running":4},
           {"provider":"Claude","effort":null,"running":1}],
 "agents_total":5,"agents_truncated":false,"agents_incomplete":false}
```

- `satellites` (label order, at most 32) are the peers `AgentGetDaemonInfo`
  lists: enabled, read-enabled and paired. The read takes at most 128 peer
  rows (the registry's own peer limit) before it orders them, and
  `satellites_truncated` is true when more than 32 exist. Rows visited: at most
  every stored peer row, which the registry caps at 128 (enabled or not), plus
  one primary-key lookup of the hub's observation per peer read. `label` is the operator's peer label
  (at most 128 bytes). `reachable` is true when the hub's latest poll found the
  satellite healthy. `last_seen_at` is the hub's last good poll (`null` if
  never). `build_sha` (hex only, else `null`) and `running_agents` (its
  `Running` plus `WaitingApproval` sessions) are what the satellite last
  reported to this hub; both are `null` until it has reported since the hub
  started. The read never contacts a satellite: `AgentGetDaemonInfo` dials each
  one live, the dashboard reads the poller's latest observation (refreshed every
  poll and every live read), so the two agree up to one poll interval.
- `agents` counts the hub's running leaf sessions (`Running` or
  `WaitingApproval`; Groups and Epics never run) per `provider` and `effort`,
  largest group first, at most 32 groups. `effort` is `null` when the session
  has none set. The read stops after 1024 running agents
  (`DASHBOARD_MAX_RUNNING_AGENTS`, one more row is read to tell), and both the
  groups and `agents_total` come from that one bounded set. Rows visited: the
  read walks `idx_fleet_active_sessions` (migration v153, a partial index that
  holds only `Starting`, `Running` and `WaitingApproval` sessions; `INDEXED BY`
  refuses any other plan), so it visits at most the hub's active sessions,
  never its finished history, and stops after 1025 matching rows. `agents_total`
  counts every agent read, including any group the group cap cut
  (`agents_truncated`). `agents_incomplete` is true when more than 1024 are
  running: `agents_total` is then exactly 1024 and every count is a lower
  bound, so a client shows "more than 1024", never the number as a total.
- Every field is listed above; the wire type refuses any other, and the
  gateway forwards a daemon reply only when it is exactly that type. There is
  no session id, title, working directory, host address, link, socket path,
  SSH target, installation id, error text or credential in it.

### `GET /api/v2/providers`

Each provider's plan windows, overage posture and spend, for the app's usage
dashboard (hub #3796, one daemon call `RemoteGetProvidersV2`, params `{}`). It
takes no query (any query key is `400 invalid_request`) and names no project.
Labels and numbers only. Every field below is optional for a client, unknown
fields are ignored by a tolerant client; the gateway itself accepts the daemon
reply only as exactly this type.

```json
{"observed_at":"2026-10-22T12:00:00.000000000Z",
 "providers":[{"provider":"Claude","status":"allowed","binding_window":"five_hour",
   "overage_status":"rejected","is_using_overage":false,
   "observed_at":"2026-10-22T12:00:00.000000000Z","stale_reason":null,
   "windows":[{"key":"five_hour","utilization":0.42,
     "resets_at":"2026-10-22T15:00:00.000000000Z","resets_at_epoch":1792681200}],
   "windows_reason":null,"windows_truncated":false,
   "spend":{"cost_1h_usd":1.25,"cost_24h_usd":9.5},"spend_reason":null,
   "models":[{"model":"claude-opus","cost_1h_usd":1.0,"cost_24h_usd":8.0}],
   "models_truncated":false}],
 "providers_truncated":false,"windows_incomplete":false}
```

- `providers`: one row for every provider the daemon knows, in canonical order
  (`Claude`, `Codex`, `Pioneer`, `OpenRouter`, `Bedrock`, `Local`,
  `Antigravity`, `CodexAppServer`, `Harness`), at most 16, including a provider
  with no data. Absent data is explicit, never a guess: a block that is not
  known is `null` with a reason code beside it.
- `status`, `binding_window` (the window the provider itself calls binding),
  `overage_status`, `is_using_overage` and `observed_at` come from the hub's
  latest plan-window observation (`provider_rate_limit_windows`, the same rows
  `GetProviderRateLimits` serves, honouring an operator's account-change
  invalidation); all `null`/false when the hub never observed the provider.
- `windows` (key order, at most 8): `key` is the provider's own window name,
  `utilization` the fraction consumed (finite, clamped to `0..1`), `resets_at`
  RFC3339 and `resets_at_epoch` the same instant in unix seconds (both `null`
  when the provider reported none). `windows_reason` is `no_observation` when
  there are none. `stale_reason` (a stable code such as `probe_timeout`) is set
  when the latest refresh failed, so the windows are the last known values.
- `spend` is the provider's cost over the last 1h and 24h in USD (four
  decimals), summed from each invocation's own cost (a session-cumulative row
  counts its increment, as in the fleet rollup); an unpriced invocation counts
  zero. `models` lists the largest 24h cost first, at most 8. Spend is a
  daemon-cached figure (refreshed in the background, at most once per 30 s, and
  never inside the Remote read's SQL budget): `spend` is `null` with
  `spend_reason` `warming` (no figure yet after a start), `unavailable` (the
  refresh failed), `stale` (older than 5 minutes) or `truncated` (the usage read
  hit its 50 000-row cap, so the sums would be lower bounds).
- `windows_incomplete` is true when the rate-limit read hit its 256-row cap.
- Not served: credit balances (OpenRouter and other credit providers). The
  daemon has no passive, offline-safe source for them (they need a live call
  to the provider), so there is no `credits` block; a client shows spend only.
- Strings are at most 64 bytes of ASCII letters, digits and `_ - . : /`; a
  value that is not is dropped to `null` (a model becomes `other`). There is no
  account, e-mail, organisation, key, token, URL, session id, prompt, host path
  or raw provider error text in it.

### `POST …/messages`

Request, `Content-Type: application/json`, a body of at most 32 KiB, an object
with exactly these fields (anything else is `400 invalid_message`):

```json
{"text":"Please prioritize the migration.","idempotency_key":"<uuid, lowercase>"}
```

`text` is 1–4096 bytes of UTF-8, not blank, without NUL. Reply `200`:

```json
{"message_id":"<uuid>","seat_session_id":"<uuid>","state":"queued",
 "created_at":"2026-10-17T09:31:00.000000000Z","deduplicated":false}
```

**Delivery** is `QueueOperatorMessage`'s: the message is durably `queued`, then
delivered to the seat at its next tool boundary (or when the session is next
idle), **at most once**. `state` is one of:

| `state` | Meaning |
| --- | --- |
| `queued` | Durable, not yet handed to the manager. |
| `dispatching`, `effect_possible` | Being handed to the manager now. |
| `delivered` | The manager has it; the conversation has the `operator` item. |
| `uncertain` | The daemon restarted mid-delivery. It is **not** re-sent; check the conversation. |
| `withdrawn` | The operator withdrew it in the TUI before delivery. |

The client shows `queued` from the send receipt, then follows the state in the
conversation's `operator_messages` (poll the first page).

**Idempotency.** `idempotency_key` is a client-chosen UUID. A retry with the same
key and the same text replays the stored row (`deduplicated: true`, same
`message_id`) and never queues a second message; the same key with different
text is `409 idempotency_conflict`. After a timeout or a `502`/`504`
`outcome_unknown` the client **must** retry with the same key (the send may have
been queued) and show the result of the retry. A new message gets a new key.

### Errors

| Status | Body | When |
| --- | --- | --- |
| `400` | `{"error":"invalid_message"}` / `{"error":"invalid_request"}` | Body or text invalid, or a query on the dashboard; nothing sent. |
| `401` | `unauthorized` (text) | No/unknown/expired session, or a read with no matching `Origin`. Re-bootstrap. |
| `403` | `forbidden` (text, no detail) | Device not allowed; bad `Origin`/CSRF; `allow_messages` off; project not exposed; the target is not a manager seat of the project. The reason goes to the host log, never the body. |
| `404` | `{"error":"not_found"}` | A read of a session that is not a manager seat of the project. |
| `409` | `{"error":"idempotency_conflict"}` / `{"error":"seat_not_live"}` | Key reused for other text; the seat session is gone. |
| `429` | `{"error":"rate_limited"}` + `Retry-After` | More than 1 message per second or 32 per hour per session. Nothing sent. |
| `502`, `504` | `{"error":"outcome_unknown"}` | The send may have been queued; retry with the same key. |
| `503` | `{"error":"busy"}` or `audit_unavailable`, with `Retry-After` | Daemon busy / audit trail unwritable; nothing was sent. |

Bare statuses carry a short `text/plain` body naming only the status.

## What the gateway enforces for a write

`MessageGate::serve` is a closed pipeline; every step before the final daemon
call is a denial that dispatches nothing to the message method:

1. exact `Origin`, bound session cookie and `x-rsi-csrf`;
2. `allow_messages` (default **off**) and project scope, from this request's policy;
3. per-session rate limit (separate from answers);
4. a typed body that denies unknown fields;
5. a fresh daemon read of the project's seats naming the target (the daemon proves
   it again from its own state: the project's current manager seat, or the
   portfolio seat covering it; a worker, another project's manager or a rotated
   predecessor is refused);
6. a durable `attempt` line in `$XDG_STATE_HOME/rsi-remote/audit.jsonl` (else
   `503`, nothing sent), then one `RemoteSendManagerMessageV2` call, then an
   `outcome` line. Lines hold the device node, a session digest, project, seat,
   key, the SHA-256 and byte length of the text — **never the text, the cookie or
   the CSRF token**. An uncertain transport result is recorded `unknown`, never
   as success.

## Operator controls

`allow_messages` is an operator setting. Turn it on in the TUI under
**Settings → Integrations → Remote → Remote access** with `m` (next to the
answers switch, `n`), or with `rsi-remote config messages <policy.toml> on|off`.
Agents have no RPC to change it; the daemon's `RemoteSetConfig` is operator-only.
The gateway re-reads the policy per request, so the switch takes effect on the
next request. `RemoteGetStatus` reports `allow_answers` and `allow_messages`.

## Exposure note

Conversations are the managers' own words. The covering global seat spans every
project it manages, so its conversation can mention projects the policy does not
expose. Exposing a project exposes its covering global seat's conversation and
makes that seat messageable from the phone. The operator opts into messaging
with `allow_messages`; reading seats and conversations needs only an exposed
project, as the v1 reads do.

The dashboard is fleet-wide: any bound session (an allowed device that passed
the binding above) reads the satellite labels and the running-agent counts per
provider and effort of every project, including projects the policy does not
expose. It carries labels and counts only (the field list above is closed), is
read-only and dispatches one bounded daemon read under the shared Remote
admission, Store budget and response size cap.

The providers read is likewise fleet-wide and read-only: any bound session reads
the plan-window labels and spend numbers of every provider (the field list above
is closed and secret-free), under the same admission, Store budget and size cap.

## Fixtures

[`remote/contracts/v2/fixtures/*.json`](../../remote/contracts/v2/fixtures) hold
one exchange each: `request` (method, path, symbolic headers, body) and
`response` (`status`, `retry_after`, `body`; `null` for a bare status):

`projects_with_global_200` (`messages_enabled` false),
`projects_without_global_200` (`messages_enabled` true), `projects_empty_200`
(`messages_enabled` false),
`conversation_first_page_200`, `conversation_older_page_200`,
`conversation_empty_200`, `conversation_unknown_seat_404`, `unauthenticated_401`,
`message_accepted_200`, `message_replayed_200`, `message_switch_off_403`,
`message_bad_csrf_403`, `message_not_a_seat_403`, `message_invalid_400`,
`message_key_conflict_409`, `message_rate_limited_429`,
`message_outcome_unknown_504`, `message_busy_503`,
`dashboard_200`, `dashboard_empty_200`, `dashboard_incomplete_200`, `dashboard_busy_503`.
`providers_200`, `providers_empty_200`, `providers_busy_503`.

They are not hand-written. `cargo test -p rsi-remote --lib -- --exact
managers::tests::contract_fixtures_are_produced_by_the_code` runs every case
through the real gateway pipeline and fails when a file differs from the
output, when a file is missing, or when a file no case produces; after an
intended contract change, regenerate with `RSI_REGEN_CONTRACT_FIXTURES=1` on the
same test and review the diff. `success_fixtures_round_trip_through_the_wire_types`
checks every success body is the canonical serialization of its Rust type, so
the type and the file cannot disagree. The iOS app's decoding tests read these
files and the field tables above.
