# Try RSI Remote (phone view, read-only)

## What you get

A read-only phone view of your RSI sessions: the session list (sessions that
need you are badged and grouped first), one session's detail, its recent
history, and the approvals or questions waiting on your RSI host. While the
page is visible it refreshes itself (the list every 30 s, an open session every
10 s, backing off after errors). There are no actions — no sending, approving,
halting, or creating; answer waiting items on the RSI host. The page is served by `rsi-remote` on the same origin as
the API, so the browser holds the gateway session as an HttpOnly cookie.

## Requirements

- `rsid` running as your normal user (the gateway reads the daemon socket).
- Tailscale on the host and the phone, in the same tailnet.
- Tailscale is pinned to **v1.102.3** for this design. This host's installed
  build is **not yet qualified** — treat that as a known gap, not a guarantee.
- The gateway must run as your normal user, never as root.

## Steps

### 1. Install

```bash
cargo install --path crates/rsi-remote --locked
```

### 2. Find the IDs you need

```bash
tailscale status --json | jq '{owner: .Self.UserID, host: .Self.DNSName}'
tailscale status --json | jq '.Peer[] | {HostName, ID}'
sqlite3 ~/.rsi/rsi.db "select id, name from projects"
```

Use the phone's `ID` from the second command. Project IDs must be the canonical
lowercase UUIDs from the database.

### 3. Write and check the policy

```bash
mkdir -p -m 700 ~/.config/rsi-remote
rsi-remote config init ~/.config/rsi-remote/policy.toml
```

Edit `~/.config/rsi-remote/policy.toml`:

- `enabled = true`
- `canonical_host` — the host `DNSName` without its trailing dot
- `owner_user_id` — the `owner` value above
- `allowed_node_ids = ["<phone ID>"]`
- `project_ids = ["<uuid>", ...]` — at most 32

```bash
rsi-remote config check ~/.config/rsi-remote/policy.toml
```

### 4. Create the socket directory

```bash
install -d -m 700 "$XDG_RUNTIME_DIR/rsi-remote"
```

### 5. Run the gateway (as your normal user)

```bash
rsi-remote run ~/.config/rsi-remote/policy.toml "$XDG_RUNTIME_DIR/rsi-remote/ingress.sock"
```

### 6. Expose it on the tailnet only

```bash
sudo tailscale serve --bg --https=443 "unix:$XDG_RUNTIME_DIR/rsi-remote/ingress.sock"
```

`sudo` may drop `XDG_RUNTIME_DIR`; expand the path yourself, for example
`unix:/run/user/$(id -u)/rsi-remote/ingress.sock`.

Never use Tailscale Funnel. The gateway refuses to serve while Funnel is on.

### 7. Open it on the phone

Go to `https://<canonical_host>/` in the phone browser.

## Stop / disable

```bash
# Ctrl-C the gateway, then:
sudo tailscale serve --https=443 off
rsi-remote config disable ~/.config/rsi-remote/policy.toml
```

Disabling the policy takes effect without a restart; the gateway re-reads it per
request.

## Troubleshooting

| Symptom | Likely cause |
| --- | --- |
| `403` | Phone not in `allowed_node_ids`; the Serve route is not exactly the socket; Funnel is on; `tailscaled` is not Running; the node key is expiring. |
| `401` | The gateway session expired. Reload the page to re-bootstrap. |
| `502` | `rsid` is not running, or the daemon socket path is wrong. |
| Page does not load | Check `tailscale serve` config and the socket file's permissions/ownership. |

## Qualifying the phone path (please report back)

Nobody has run this on a real phone yet. These checks turn "should work" into
"works here". Send the results (pass/fail per line, plus any error text) to the
manager.

Host facts:

- `tailscale version` and `sha256sum "$(command -v tailscaled)"` (the design
  pins v1.102.3; any other build needs a quick source check before we call it
  supported).
- `sudo tailscale serve status --json`: exactly one HTTPS 443 route, `/` to
  `unix:<your ingress.sock>`; and `sudo tailscale funnel status` shows Funnel off.
- As the user that runs the gateway (not root): `tailscale status --json >/dev/null && echo ok`
  (the gateway needs read access to the LocalAPI).
- `timedatectl show -p NTPSynchronized` prints `NTPSynchronized=yes`.

Phone facts: device model, OS version, browser and version; confirm its node
`ID` is the one in `allowed_node_ids`.

Walkthrough on the phone:

1. `https://<canonical_host>/` loads and the session list appears.
2. Open a session: detail and recent history show; text looks right.
3. A session that needs you shows the badge and the "Waiting on your RSI host" list.
4. Leave the page open for a minute: it refreshes on its own; lock the phone
   and unlock it: it refreshes when you come back.
5. After more than 30 minutes idle, reload: it signs in again by itself.
6. On the host run `rsi-remote config disable ~/.config/rsi-remote/policy.toml`:
   the next refresh fails with 403. Re-enable with `enabled = true` and
   `rsi-remote config check`.
7. From a second tailnet device that is NOT in `allowed_node_ids`: 403.

## Known gaps

- Session lists show up to 200 sessions per project (4 pages of 50); older
  history loads on demand with "Load older" (25 events at a time, up to 500
  kept on screen).
- Updates are polled while the page is visible; there is no push stream.
- No actions.
- The installed Tailscale build and a real phone have not been qualified yet
  (#879 / #880); the gateway passed an independent network-exposure review.
- Waiting questions show that a question exists, not its text.
