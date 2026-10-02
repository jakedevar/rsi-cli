# RSI satellites

An RSI satellite is an RSI peer whose daemon socket is reachable from another
peer through SSH Unix socket forwarding. The peers are symmetric: either can
host the daemon and either can initiate SSH. Choose the initiator based on
which direction the network permits. Phase 0 provides the dial-home systemd
unit as the default for laptops that cannot accept inbound connections.

## Global polling switch

`Satellite polling` (the daemon setting `satellite_polling_enabled`, also a row
under Settings → Satellites) is the operator's global off switch for hub
polling. With it off, the hub poller stops scheduling registry-driven probes and
link inspections on its next tick: no peer socket is dialled. Paired-peer
registry rows and cached observations are retained, so disabling polling is not
a delete. Polling resumes on the next tick once the switch is turned back on,
with no daemon restart. It defaults to on.

## Dial-home: satellite initiates SSH

The laptop opens SSH to the hub and reverse-forwards its local `rsid` socket to
`~/.rsi/satellites/<name>.sock` on the hub. This works when the hub can be
reached from the laptop, even if the laptop is behind NAT or a restrictive
firewall.

From the updated RSI checkout on the Arch laptop, run the one-command installer.
The `~/rsi` examples assume that checkout is current. Before running helper
commands from a hub checkout, update it with `git -C ~/rsi pull --ff-only`, or
use a known installed path for the helper script.
For this hub and laptop name:

```bash
~/rsi/scripts/satellite/install.sh arch-laptop you@laptop.example.net --port 22
```

The general form is:

```bash
~/rsi/scripts/satellite/install.sh NAME SSH_TARGET [LOCAL_RSID_SOCKET] [--port PORT] [--jump TARGET] [--ssh-config PATH]
```

Port defaults to the SSH config or OpenSSH default. `--jump` supplies a
ProxyJump target. To use an operator-managed SSH config that defines a
`ProxyCommand`, identity, host alias, or other transport settings, pass
`--ssh-config /absolute/path/to/config`; omitting it uses the normal OpenSSH
configuration. The installer passes host, port, jump and config as separate
arguments and does not accept a raw ProxyCommand string. Keep any ProxyCommand
in a trusted, operator-owned SSH config. Config and socket paths may not
contain spaces.

The installer prepares `~/.rsi/satellites` on the hub, installs a systemd user
unit and launcher under `~/.config/systemd/user` and
`~/.local/libexec/rsi-satellite`, and stores only connection metadata in
`~/.config/rsi/satellites/<name>.env` with mode `0600`. It does not create SSH
keys or modify SSH configuration. The laptop needs an active systemd user
manager and a local daemon socket accessible to that Unix user. The local
socket defaults to `~/.rsi/daemon.sock`; supply the optional socket argument
for another path. Before enabling the unit, the installer checks that SSH works
with `BatchMode=yes`; load the operator-managed key into an available SSH agent
before rerunning if this check fails. After restart, it waits up to 30 seconds
for the hub socket and prints the unit's recent journal if the socket does not
appear. Before enabling the unit, it also asks the hub's `sshd -T` for the
effective settings of the current SSH user and client address. It requires
`AllowStreamLocalForwarding remote` (or `yes`), `StreamLocalBindUnlink yes`,
and `DisableForwarding no`. If the hub denies forwarding, the installer exits
without enabling the unit. The hub operator can add a narrow `Match User` and
`Address` exception with those settings after reviewing local SSH policy.
This config check cannot override restrictions on an authorized key; the socket
readiness check still catches a refused forward. The installer never edits
hub `sshd` configuration.

The service uses `ExitOnForwardFailure`, `BatchMode=yes`, a 10 second connect
timeout, and SSH keepalives every 30 seconds with a three-miss limit. Before
connecting it removes a hub socket only after checking that it refuses
connections; after binding it applies mode `0600`. The stale-socket check needs
Python 3 and command execution on the hub. A failed tunnel is retried after 20
seconds, up to three starts within ten minutes. Once the limit is reached,
inspect the journal and fix the cause before restarting it. Inspect the service with:

```bash
systemctl --user status rsi-satellite@arch-laptop.service
journalctl --user -u rsi-satellite@arch-laptop.service
```

## Direct: hub initiates SSH

When the laptop has a routeable address and accepts SSH from the hub, install
the hub-initiated direct tunnel on the hub. Use the laptop's SSH host or config
alias as the target:

```bash
~/rsi/scripts/satellite/install-direct.sh arch-laptop laptop-ssh --port 22
```

The general form is:

```bash
~/rsi/scripts/satellite/install-direct.sh NAME SSH_TARGET [--port PORT] [--jump TARGET] [--ssh-config PATH]
```

Port defaults to the SSH config or OpenSSH default. `--jump` supplies a
ProxyJump target. An operator-managed SSH config may define a ProxyCommand,
identity, host alias, or other transport settings; pass it with `--ssh-config`
and omit `--jump` when the config defines ProxyCommand. Config paths contain no
spaces. The installer checks noninteractive SSH with `BatchMode=yes`, resolves
the laptop home, installs a systemd user unit and launcher under
`~/.config/systemd/user` and `~/.local/libexec/rsi-satellite`, and stores only
connection metadata in `~/.config/rsi/satellites/NAME-direct.env` with mode
`0600`. It creates no keys and does not modify SSH configuration.

The hub service forwards `~/.rsi/satellites/NAME-direct.sock` to the laptop's
`~/.rsi/daemon.sock`. Its socket directory is owned by the hub user with mode
`0700`, and the forwarded socket is checked for hub-user ownership and set to
mode `0600`. A stale socket is removed only if it is a non-listening Unix
socket owned by that user. SSH uses BatchMode, ExitOnForwardFailure, a 10
second connect timeout, and keepalives every 30 seconds with a three-miss
limit. The service retries failed tunnels after 20 seconds, up to three starts
within ten minutes. The installer waits up to 30 seconds for its socket, then
requires a bounded `GetHealthStatus` response from the laptop daemon. It prints
recent journal output when readiness or health fails.

For direct access, the laptop must run `sshd`, and its firewall and network
must allow the hub to reach the SSH port. The operator configures those
services and rules; the installer does not. Direct and dial-home forwards use
separate socket names. The hub list helper includes both `NAME.sock` and
`NAME-direct.sock` entries and checks ownership, mode, and health.
Verify a direct instance with:

```bash
systemctl --user status rsi-satellite-direct@arch-laptop.service
journalctl --user -u rsi-satellite-direct@arch-laptop.service
~/rsi/scripts/satellite/hub-list.sh
```

## Network reachability choices

- **LAN:** use a private address when both peers share a trusted local network.
- **Tailscale:** use the peer's tailnet name or address when both devices are
  enrolled and policy permits SSH traffic.
- **AWS Client VPN:** use the private address and route provided by the
  operator-managed VPN when both peers have access to the relevant network.
- **AWS Systems Manager Session Manager SSH:** use an operator-managed SSH
  config with the appropriate `ProxyCommand` when SSM is the chosen transport.

These are transport choices, not installer-managed setup. The operator owns
VPN enrollment and routes, Tailscale policy, AWS access, SSH identities, host
keys, laptop `sshd`, and firewall rules. Use an operator-owned SSH identity
dedicated to the hub connection, with the matching public identity authorized
on the hub. Keep the private identity on the laptop or in its agent/hardware
key; the installer neither creates nor copies keys. For direct mode, authorize
the hub's dedicated identity on the laptop as appropriate. If an SSH config
uses `ProxyCommand`, omit `--jump` and select that config with `--ssh-config`.
Each peer also needs its own provider credentials. This laptop currently
reports `provider_openrouter_available=false`; the operator must configure its
OpenRouter credential in that laptop's RSI vault before scheduling OpenRouter
sessions there. Do not copy the hub's provider credential into a tunnel config.

## Hub registry and cached session view

In the hub TUI, open **Settings → Integrations → Satellites → Satellite
registry**. The browser uses the hub daemon socket throughout. Its remote
session rows are cached observations labeled by both peer ID and remote
session ID; selecting one cannot run a local session action. Offline peers
retain their last observation with a stale badge and age. The browser never
starts a daemon or connects the TUI directly to a peer.

After installing a tunnel, add a disabled peer, then add its local socket link
under `~/.rsi/satellites/` on the hub. A dial-home link uses the reverse
forwarded socket; a direct link uses the local forwarded socket and its SSH
target. Record the operator-owned SSH trust reference for each link. Enable
the peer and link, select the link, then press `p` to probe its installation
and daemon incarnation IDs. Compare the installation ID with the intended
peer before entering it as the peer's expected installation ID and enabling
session reads. The background poller only reads enabled, paired peers. It
checks every enabled link for that peer before accepting a snapshot, so an
identity conflict quarantines the peer even if another link answers.

Use `R` on a peer only after repairing its SSH trust or reconciling a
replacement installation. This explicit acknowledgement clears quarantine
to a degraded state; a fresh identity check is still required before the peer
is healthy. Editing a label or link does not clear quarantine. `r` refreshes
the browser; `n` and `b` page through up to 30 cached sessions at a time.
The hub registry stores no provider credentials and grants no per-method
restriction to a forwarded daemon socket. Restrict the hub Unix user, socket
permissions, SSH account, key and host policy accordingly.

## Health check and TUI attach

Always probe the selected socket before starting the TUI. `rsi` can start a
local daemon when its socket connection fails, which could attach the TUI to a
new local daemon instead of the satellite.

The hub helper lists the configured satellites and performs a bounded
`GetHealthStatus` RPC:

```bash
~/rsi/scripts/satellite/hub-list.sh
```

`healthy` means an owner-owned `0600` socket answered the health RPC within
five seconds. `insecure` means its owner or mode is unexpected; `unreachable`
includes a stale socket inode or stopped laptop daemon. Set
`RSI_SATELLITE_DIR` to select another directory or `RSI_RPC_BIN` to select an
`rsi-rpc` executable.

For a single satellite, probe and attach with its socket path. Start `rsi` only
if the health command succeeds:

```bash
RSI_DAEMON_SOCKET_PATH="$HOME/.rsi/satellites/arch-laptop.sock" rsi-rpc GetHealthStatus
RSI_DAEMON_SOCKET_PATH="$HOME/.rsi/satellites/arch-laptop.sock" rsi
```

The hub client must use the same Unix user that owns the hub-side socket. A
forwarded RSI daemon socket grants full operator control of that daemon,
including its sessions and data.

## Remove

On the hub, stop and disable a direct unit, then remove its instance settings
and socket while the tunnel is stopped:

```bash
systemctl --user disable --now rsi-satellite-direct@arch-laptop.service
rm "$HOME/.config/rsi/satellites/arch-laptop-direct.env"
rm "$HOME/.rsi/satellites/arch-laptop-direct.sock"
```

Remove the shared direct unit and launcher only when no other direct
instances use them:

```bash
rm "$HOME/.config/systemd/user/rsi-satellite-direct@.service"
rm "$HOME/.local/libexec/rsi-satellite/direct.sh"
systemctl --user daemon-reload
```

On the laptop, stop and disable the unit, then remove its settings:

```bash
systemctl --user disable --now rsi-satellite@arch-laptop.service
rm "$HOME/.config/rsi/satellites/arch-laptop.env"
```

Remove the shared unit template and launcher only when no other satellite
instances use them:

```bash
rm "$HOME/.config/systemd/user/rsi-satellite@.service"
rm "$HOME/.local/libexec/rsi-satellite/dial-home.sh"
systemctl --user daemon-reload
```

If a stale hub socket remains after removing an instance, remove that
instance's socket while its tunnel is stopped:

```bash
ssh -p 22 you@laptop.example.net 'rm -f "$HOME/.rsi/satellites/arch-laptop.sock"'
```

## Path and security assumptions

- The installer resolves the hub home over SSH. It expects an absolute hub
  home path made from letters, numbers, `_`, `.`, `/` or `-` (the usual
  `/home/user` layout). Socket and custom SSH config paths are absolute and
  contain no spaces.
- The hub `~/.rsi/satellites` directory is mode `0700`; the forwarded socket is
  set to mode `0600`. Access is limited to the hub Unix account. Treat access
  to that account as access to every attached satellite.
- The local laptop socket must be accessible to the Unix user running the
  systemd unit. No credentials or tokens are written by the installer.
