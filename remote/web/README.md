# RSI Remote web milestone

This is a dependency-free, read-only, mobile-first UI. The gateway serves this
directory and the V1 read routes from the same origin, so the browser sends the
HttpOnly session cookie automatically (`credentials: 'same-origin'`). There are
no actions: the UI only lists sessions and reads one session's recent history.

## Adapter interface

`app.js` calls only these methods in `api.js`, and their return shapes are
unchanged from the earlier prototype:

```js
listSessions() // -> SessionSummary[] { id, title, status, provider, updatedAt }
getSession(id) // -> { session: SessionSummary, events: Event[] }
logout()       // -> ends the gateway session (POST /auth/logout with the CSRF header)
```

`kind` is one of `user`, `assistant`, `tool`, `system`, `error`, or `thinking`.
Statuses are the RSI status names plus `Deleted`. Providers are `Claude`,
`Codex`, `Pioneer`, `OpenRouter`, `Bedrock`, `Local`, `Antigravity`,
`CodexAppServer`, or `Harness`. Text is always rendered as literal text; no HTML
or Markdown interpretation occurs.

## Browser session (ADR D5)

The adapter establishes a gateway session on first use, before the first data
read:

1. `GET /auth/bootstrap` -> `{"nonce"}` (sets the boot cookie).
2. `POST /auth/session` with `{"nonce"}` -> `{"csrf"}` (sets the session cookie).
   The CSRF token is kept in memory only, never `localStorage`,
   `sessionStorage`, or the URL.
3. Data reads go to the V1 routes with the session cookie. A `401` triggers one
   re-bootstrap and one retry; a second `401` is a surfaced `ApiError`.

Data routes:

- `GET /api/v1/projects` -> `{ items: [{ id, name }] }`
- `GET /api/v1/projects/<pid>/sessions` -> `{ items: [SessionSummaryV1] }`
- `GET /api/v1/projects/<pid>/sessions/<sid>` -> `{ item: SessionDetailV1 }`
- `GET /api/v1/projects/<pid>/sessions/<sid>/history` -> `{ items: [HistoryEventV1] }`

`SessionSummaryV1` maps to the internal shape: `id`, `title` <- `own_title`,
`status` and `provider` from the tagged `{state,value|label}` enum, and
`updatedAt` <- `updated_at`. Unrecognized status/provider values display as a
neutral known value (`Archived` / `Local`). History events map `Message`
+role to `user`/`assistant`, `ToolUse`/`ToolResult` to `tool`, `System`/
`Compressed` to `system`, and `Thinking` to `thinking`; unknown kinds become
`system`.

HTTP status handling: `403` means identity or readiness was denied, `502` means
the daemon is unavailable, and each produces a distinct `ApiError` message.

## Bounds

The adapter enforces a 1 MiB response body limit, 32 projects, 500 session rows,
50 events, 64 KiB per event text, and 512 KiB aggregate event text. It rejects
unknown status/provider/event kinds, duplicate session IDs, and history is
windowed to the most recent 50 events. Field shapes come from
`remote/contracts/v1/src/generated.ts`.

## Checks

Run `npm test` from this directory. Tests use Node's built-in test runner and
need no package installation. The operator walkthrough is
`docs/rsi-remote/try-it.md`.
