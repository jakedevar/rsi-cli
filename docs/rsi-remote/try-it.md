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
- Tailscale on the host and the phone, in the same tailnet, with MagicDNS and
  HTTPS certificates enabled.
- The `rsi-remote` binary on the host. `make release-install` builds it and
  links it to `~/.local/bin/rsi-remote`.
- Tailscale **1.102.x, patch 3 or later** is the qualified line. The settings
  page shows the installed version and flags anything outside it. The gateway's
  identity checks were verified against live LocalAPI output from 1.102.4
  (#1101): that build reports the deprecated `MachineAuthorized` as `null` for
  an authorized device, so only an explicit `false` denies. Every other check
  (node allowlist, owner, tags, sharer, source address, key expiry, Funnel off)
  is unchanged. A different minor line is flagged until re-qualified.
- The gateway runs as your normal user, never as root (the daemon manages it as
  a systemd *user* unit).

## One-time setup

`tailscale serve` needs privilege. Grant it once, instead of using sudo every
time:

```bash
sudo tailscale set --operator=$USER
```

Optional, so the gateway also starts at boot before you log in:

```bash
loginctl enable-linger $USER
```

## Use it

1. In the TUI open **Settings → Integrations → Remote → Remote access**.
2. Press `Tab` to move between **Devices** and **Projects**; `Space` allows a
   device (your phone) and exposes the projects you want (at most 32). Owner id
   and host name are detected from Tailscale; nothing is typed.
3. Press `e` to enable. rsid validates the policy, starts the gateway as the
   user unit `rsi-remote.service` (restarts on failure, starts at login),
   and adds the `tailscale serve` route. The page shows gateway running, serve
   route present, Funnel off, the Tailscale version and the URL. It refuses,
   changing nothing, if Tailscale's HTTPS port 443 is already used by another
   service or if its serve config cannot be read: Remote never replaces a route
   it does not own.
4. Enabling is all-or-nothing. If any step fails (gateway unit, serve route),
   rsid disables the policy again, removes whatever this attempt created, keeps
   your device and project choices and shows the error. If the serve step needs
   privilege the page shows the exact command; run it once, then press `e` again.
5. Open the URL on the phone: `https://<host>/`.

Press `e` again to disable: the policy is rewritten first (the gateway re-reads
it per request, so access stops with the next request), then the unit is
stopped and the serve route removed. Your device and project choices are kept.

Never use Tailscale Funnel. RSI never enables it; the gateway refuses to serve
while Funnel is on and the page tells you the command to turn it off.

The policy file `~/.config/rsi-remote/policy.toml` is still the source of truth
the gateway reads; the settings page writes it. `rsi-remote config check|show`
still work for inspection. Remote control is operator-only: agents have no
RPC to change the device or project lists.

## Troubleshooting

| Symptom | Likely cause |
| --- | --- |
| `403` (plain text `forbidden`) | The page deliberately does not say which check failed. Read the host log: `journalctl --user -u rsi-remote` shows `rsi-remote: denied: <code>` (for example `node_not_allowed`, `funnel_on`, `not_authorized`, `expired`, `not_ready`), rate-limited to one line per code per 5 s. Likely causes: phone not in `allowed_node_ids`; the Serve route is not exactly the socket; Funnel is on; `tailscaled` is not Running; the node key is expiring. |
| `401` | The gateway session expired. Reload the page to re-bootstrap. |
| `502` | `rsid` is not running, or the daemon socket path is wrong. |
| Page shows "One-time step pending" | Run `sudo tailscale set --operator=$USER`, then press `e` again. |
| Gateway stopped | `systemctl --user status rsi-remote.service`; check the policy with `rsi-remote config check`. |
| Page does not load | Check `tailscale serve` config and the socket file's permissions/ownership. |

## Qualifying the phone path (please report back)

Nobody has run this on a real phone yet. These checks turn "should work" into
"works here". Send the results (pass/fail per line, plus any error text) to the
manager.

Host facts:

- `tailscale version` and `sha256sum "$(command -v tailscaled)"` (the qualified
  line is 1.102.x, patch 3 or later; other lines need a source check first).
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
- Tailscale 1.102.4 whois output is covered by a captured-fixture test (#1101),
  and 1.102.x patch 3+ is the qualified line; the gateway passed an independent
  network-exposure review. A full phone walkthrough is still worth reporting
  back (#879 / #880).
- Waiting questions show that a question exists, not its text.
