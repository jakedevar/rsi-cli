# RSI Remote API v2 contract (#2962)

API v2 is what the native iOS client (#2961) talks to, through the same
`rsi-remote` gateway, the same Tailscale Serve route and the same ingress as the
browser view. **v1 is unchanged and keeps working.** v2 adds three routes:

1. the exposed projects, each with its project-manager seat and the covering
   global (portfolio) manager seat;
2. one manager seat's conversation, newest first, with a backward cursor;
3. one write: a message to a manager seat, delivered exactly like the TUI's
   operator message.

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

Newest first, 25 items per page, only message events:

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
- `text` is cut at 4096 bytes (`truncated: true`). Provider diagnostics, tool
  calls and thinking are not included.
- Walk back by passing `next_before` as `?before=` unchanged. `next_before` is
  `null` and `has_more` false on the oldest page. The cursor is an anchor (the
  event's `sequence` and `id`), not an authority: every page re-proves the seat.
- `operator_messages` (first page only, up to 10 newest first) is the delivery
  view for the client's own sends (below). It never repeats message text.

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
| `400` | `{"error":"invalid_message"}` / `{"error":"invalid_request"}` | Body or text invalid; nothing sent. |
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
`message_outcome_unknown_504`, `message_busy_503`.

They are not hand-written. `cargo test -p rsi-remote --lib -- --exact
managers::tests::contract_fixtures_are_produced_by_the_code` runs every case
through the real gateway pipeline and fails when a file differs from the
output, when a file is missing, or when a file no case produces; after an
intended contract change, regenerate with `RSI_REGEN_CONTRACT_FIXTURES=1` on the
same test and review the diff. `success_fixtures_round_trip_through_the_wire_types`
checks every success body is the canonical serialization of its Rust type, so
the type and the file cannot disagree. The iOS app's decoding tests read these
files and the field tables above.
