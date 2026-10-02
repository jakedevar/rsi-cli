# rsi-remote gateway

A read-only gateway that serves the bundled phone UI and the six bounded
Remote V1 daemon reads over a protected Unix socket, for exposure on the
tailnet with `tailscale serve`. It has no TCP listener, no Funnel path and no
mutation. Operator steps: `docs/rsi-remote/try-it.md`.

The policy is a local TOML file. `config init <file>` creates a 0600 disabled
policy in an existing owner-controlled directory. Set `enabled = true` only
after specifying a canonical FQDN, nonzero owner user ID, at least one allowed
node ID, and at least one canonical project UUID. `config check <file>` checks
the policy, `config show <file>` prints its settings with node and project
counts, and `config disable <file>` atomically disables it. `run <file>
<ingress.sock>` requires a non-root gateway UID and an existing 0700 gateway
owned socket directory. It creates a 0600 Unix socket and accepts only root
peers by SO_PEERCRED before reading HTTP. Live policy disable is checked for
each request.

Every request passes these layers in order (ADR D4/D5):

1. Root peer credentials, then a strict HTTP parser with a closed list of GET
   and POST routes, the Serve forwarding envelope and a tailnet source address.
2. `LocalAPI` readiness (status, serve-config route, self WhoIs) refreshed every
   4 s and valid for 5 s, plus a fresh WhoIs of the requesting device checked
   against the owner and node allowlist.
3. A browser session: `GET /auth/bootstrap` then `POST /auth/session` with
   nonce and exact Origin issue a `__Host-` HttpOnly cookie and an in-memory
   CSRF token; data reads need the cookie and a same-origin request.
4. The closed first-page read adapter: project scope is checked before any
   request is built; requests go tokenless to the daemon socket owned by the
   same user, bounded in time and size.

Known gaps: first page only (no cursors or older history), no live updates,
the installed Tailscale build is not yet qualified (ADR G3), and real-device
qualification is pending.
