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

Both unit templates retry forever with backoff (`StartLimitIntervalSec=0`,
`RestartSec=20s`, `RestartSteps=6`, `RestartMaxDelaySec=5min`; systemd older than
254 ignores the last two and keeps the fixed 20s retry). `install-direct.sh`
refuses to overwrite a differing installed `rsi-satellite-direct@.service` while
another instance (enabled, drop-in or env file) has no `NAME-direct.env`, since
it would fail to start; write those env files, or pass `--replace-template`.

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

### Pair a direct satellite with one command (#2944)

After `install-direct.sh` the tunnel is up but the peer is not paired. Pairing is
operator-only RPCs on two daemons whose inputs depend on each other. The
operator runs one command on the hub (stdlib Python, drives `rsi-rpc --socket`
for the hub socket and the tunnel socket; it refuses to start with
`RSI_SESSION_TOKEN` set, so an agent cannot run it):

```bash
~/rsi/scripts/satellite/pair-direct.py mac-mini --repo /Users/example/rsi \
  --project-name Rsi --appoint-manager --manager-provider Claude \
  --manager-model claude-opus-5-5 --manager-effort high
```

It reads `~/.config/rsi/satellites/NAME-direct.env`, then in order: reads both
installation ids (the satellite's through the tunnel, directly); on the hub
`PutSatellitePeer` (unpaired), `PutSatelliteLink` (`direct_local_forward`, the
trust reference `ssh ALIAS = user@host with KEY; host key TYPE SHA256:... (known_hosts)`
computed from `ssh -G` and `ssh-keygen -F`) and `ProbeSatelliteLink`; **it stops
unless the probed installation id equals the tunnel's**; on the satellite
`CreateProject` (if no project has that path) and `PutSatelliteInboundPolicy`
(adds the hub installation and the project, keeps every existing entry), then with
`--appoint-manager` `LaunchSession` (the brief is the first prompt) and
`ConfigureHarnessManagerWithPolicy`; finally on the hub the paired peer is
read-enabled (`--dispatch` also dispatch-enabled) and `PutSatellitePeerScope`
adds the project. It prints a summary (peer, link, project, manager session,
registry revision) and, when dispatch was left off, the exact `--dispatch`
rerun. Enable dispatch only after the hub runs a build that checks the satellite's
platform before QA placement (#2935).

It is idempotent: rerunning finds the peer by label or installation, the link by
socket path, the project by path, merges inbound entries instead of replacing
them, appoints a manager only when the project has none live, and writes nothing
when everything already holds. `--dry-run` reads and prints every call and changes
nothing. It refuses (exit 2, nothing written) when the peer is already paired with
a different installation, the label belongs to another installation, the host key
fingerprint changed (`--update-trust-reference` accepts it), the repository path is
missing on the satellite (`--no-repo-check` skips the ssh probe), or a project of
that name exists at another path.

The seat policy is the Execute preset made conservative for a small host:
`max_active_sessions` 4 (`--max-active-sessions`), 32 created sessions, no
topology, git-effect, storage, daemon-setting or deploy capability (the hub
restarts satellites), and the launch choices from `--allowed-launch
PROVIDER:MODEL[:EFFORT]` (default: the seat's own). `--manager-policy-file`
supplies a whole `ManagerPolicyV2` JSON instead. The brief defaults to
`scripts/satellite/manager-brief.md` (`--manager-brief FILE` overrides;
`{{SATELLITE_NAME}}`, `{{REPO}}`, `{{PROJECT_NAME}}`, `{{PROJECT_ID}}` and
`{{HUB_INSTALLATION_ID}}` are filled in). A seat that was launched but not
appointed (the second call was refused) is appointed by rerunning with
`--appoint-manager --manager-session <id>`. An appointed seat counts as live only when
the satellite's `GetSatelliteScopeStatus` reports the project `live` (#2952): an
archived or otherwise dead seat exits 1 with the recovery (reseat from the hub with
`AgentReseatSatelliteManager`, or rerun with `--manager-session <live id>`), never
"already appointed". Tests:
`python3 -m unittest scripts.tests.test_pair_direct` (stub `rsi-rpc`).

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

The identity health report carries optional `latest_watchdog_trip` evidence:
restart ID, trip time, last healthy time (RFC3339 UTC with nanoseconds), and the
closed failed-probe codes `rpc_timeout`, `store_probe_timeout`,
`scheduler_tick_stale`, `reconciliation_tick_stale`. It comes from the durable
watchdog record imported at daemon startup, so the hub can read the cause after
recovery. It contains no log text, paths or credentials. Older peers omit it;
no record means no known trip. It describes the latest historical trip, not the
peer's current reachability. The hub keeps the last successful health report
through an outage in its process-local cache; after a hub restart it must poll
successfully to recover this evidence. Staged connect/auth/transport/RPC/store
diagnostics are a separate follow-up to this first protection/reporting slice.

## Full-library QA on the satellite (#2120)

The merge queue's async full `rsid`/`rsid-store` run (`rolling-library-qa`,
#2092/#2112) goes to a satellite instead of the hub when one qualifies: an
enabled, paired, read- and dispatch-enabled peer with a declared scope, whose
latest poll is healthy and under 15 minutes old, and whose live identity
reports `health.platform.os` equal to the hub's `std::env::consts::OS`,
advertises the `qa_run` capability and a QA admission that admits. Thus Linux
hub QA stays on Linux satellites. `GetSatelliteIdentity.health.platform`
carries `os` and `arch` from `std::env::consts`; architecture is displayed but
does not restrict placement. A missing platform (including older builds)
fails closed with `satellite_platform_unknown`; a different OS gives
`satellite_platform_mismatch`. Peer rows in the TUI, `AgentGetDaemonInfo`
satellites and QA fallback reasons show the reported platform (or `unknown`).
Manager satellite inspection rows also include `health.platform`.
The satellite computes that admission from the same gates the
run's job meets: its build governor's disk floor (`min_free_disk_gb`,
default 30 GiB, never below 20 GiB) and its load, memory and slot gates, its
host-load admission, and no other full-library job running; it reports it on
its health and rechecks it when the run starts, so it never accepts a run that
would only time out in build admission. The hub sends the exact published SHA with
`RequestHubQaRun` (operator-only wire method, allowlisted hub installation
only) and polls it about once a minute; the satellite's exit code, failing
tests and job log come back into the hub's queue receipts and `qa` Issue.

On the satellite the run is an ordinary durable recipe job owned by the live
declared scope seat, checked out from that seat's working directory (fetching
`origin/rolling` once when the commit is missing) into
`~/.rsi/jobs/rolling-library-qa/hub-<run>/source`. Every run reuses the one
registered cargo target for that directory, and the previous hub snapshot is
removed when the next run starts. The satellite refuses a run while another
full-library job is queued or running, while its own host-load admission holds,
or below the disk floor; a deploy drain records it `queued` until the restart.
A listed refusal (the satellite raises it before it records a run), or no
qualifying peer, makes the hub run it itself and record `ran_on: hub` with the
reason. When the start was sent but its reply was lost, or the satellite
answered with an error that does not prove it started nothing (for example a
launch error whose job stayed running, #2156), the run may already be going on
the satellite, so it stays bound to that satellite and run id: the hub starts
nothing itself, offers it to no other satellite, and each poll replays the same
idempotent start (without a fresh-placement admission check) until the
satellite reports the run. The hub records the peer, run id and commit before
it sends a start, so a hub that stops after the satellite accepted it binds
that same run on the next pass instead of starting a second copy (#2155). A satellite run that stays unreachable for 240 minutes after
dispatch settles as incomplete coverage.

## Satellite reports to the hub manager (#1103)

The satellite's appointed manager (when it is also the declared seat or its
rotation tip in the inbound policy) can call `AgentReportToHub` with a short
typed line (`enqueue`, `deploy_ready`, `result`). The satellite never dials the
hub: the hub pulls queued reports over its existing hub-initiated link with the
`FetchHubReports` wire method (allowlisted hub installation only), after
re-verifying the pinned installation. Each report becomes one `ledger_change`
manager notice (subject `satellite_report`, version = report id) that wakes the
hub manager once and appears in `AgentManagerInbox` as an untrusted,
informational satellite report. It carries no authority and the wake text never
includes the satellite's words; the hub still verifies every SHA it acts on.

Bounds: 1024 bytes of text, 32 queued reports per satellite, 8 per pull, and 12
recorded per peer per 5 minutes (enforced on the hub; a report over the limit is
held on the satellite and retried, not dropped). The hub needs an enabled,
paired, read- and dispatch-enabled peer and a declared peer scope.

**Routing (#2242).** A report names no hub session. The hub routes it by project
to the project's current manager: the only project with a manager, or (when the
hub runs several) the project of the session that most recently wrote to this
peer among the projects whose manager is live; several live managers with no
dispatcher is `ambiguous_manager_project`, not a guess. The notice is recorded
for the manager seat of that moment. The report is acknowledged once recorded,
but a re-appointment retires the old scope's unretrieved notices (#1310), so
every pull round the hub re-homes the project's retired, never-retrieved reports
onto the current manager (same report id, original timestamp, a `carried_over`
mark naming the old seat) and arms its wake like a new report. Nothing is
re-homed while no live manager exists; it moves when one is appointed. Before
this, a land request recorded for a replaced seat was lost for good (61 of 131
reports in two weeks).

**Delivery state (#2242).** `AgentReportToHub` answers `queued` (acceptance, not
delivery). The reporting seat reads each report's fate in `AgentGetDaemonInfo`
`hub_reports`: `queued` (the hub has not pulled it), `fetched`, `held` (the hub
pulled it and cannot record it yet; `reason` is `no_manager_project_for_peer`,
`manager_unavailable`, `ambiguous_manager_project`, `rate_limited`,
`retention_cap` or `record_failed`), `delivered` (recorded as a notice for the
current manager), `refused` (permanent, with the hub's class) and the typed
failure `undelivered` (nothing happened for 180 s: `hub_not_pulling` or
`hub_not_acknowledging`). The hub sends its `held` and `refused` verdicts on the
next pull only to a satellite whose identity advertises `report_outcomes`; an
older satellite sees acknowledgements only (a refused report then reads as
delivered). While the hub has not pulled for 180 s with a report waiting,
`AgentReportToHub` refuses `satellite_report_hub_unreachable` instead of
promising delivery; keep the report in the manager's own outbox note and retry
after `hub_pulling` is true again. At 32 unacknowledged reports it refuses
`satellite_report_queue_full` and the message says what is stuck (#135): `depth`, the
oldest pending report's `oldest_queued_at` and `oldest_age_secs`, `pending_by_kind`
(result, enqueue, deploy_ready) and `last_hub_pull_at`; `hub_reports.backlog` carries
the same numbers at any time. Wait for the hub to acknowledge, fold further results
into one RESULT line, and never re-send an ENQUEUE or DEPLOY-READY that is pending.

Delivery is at most once: the satellite queue and this state are in memory, so a
satellite restart loses unsent reports and forgets the states. A replayed report
never creates a second notice or wake: the dedup key is the peer plus the report
id, looked up across every notice job, so it holds across manager re-scoping,
re-appointment, re-homing and retrieval. Notices are immutable and never
deleted, so the hub keeps at most 2048 report notices per peer (a re-homed copy
counts); at that cap new reports stay on the satellite (its 32-slot outbox then
refuses more) while already-seen ids still dedupe. A revoked manager scope or
policy ends the right to report, and reports fetched from an installation that
is no longer the peer's pin by the time they are ingested are dropped, not
recorded.

## Deploying a build to a satellite (#171)

The satellite never restarts its own rsid. It builds and stages with
`scripts/install-release.sh --stage-dir ~/.rsi/staging/bin-<sha>` and reports
`deploy_ready`; the hub restarts it with `AgentRequestDeploy {sha, binaries_dir,
peer_id}`; the satellite then verifies `AgentGetDaemonInfo` `hub.build_sha` and
`hub.last_deploy.state` and reports the result. Only the operator can change that
route, and manager mail never overrides a standing operator restriction (a
conflict is surfaced, not followed). The step-by-step is in
`.claude/skills/rsi-project-manager/SKILL.md` ("Satellite (arch-laptop)").

### Starting a satellite daemon that is down (#2820)

After a reboot of a satellite with no enabled `rsid.service`, no daemon runs,
and the runtime-only `rsi-workers.slice` limits are gone (rsid then refuses every
worker launch with `rsi-workers.slice is missing or its aggregate limits differ
from settings`). Over ssh, run the one documented fallback:

```
~/rsi/scripts/rsid-start-satellite
```

It starts the **deployed** `~/.rsi/install/rsid` under
`~/.rsi/install/rsid-supervisor.sh` behind the socket holder, in a detached
collected systemd user unit (it survives the ssh session), after applying the
`~/.rsi/rsid-scope.env` limits to `rsi-workers.slice` and persisting them. It
waits for `GetHealthStatus`, exits nonzero with no daemon started otherwise, and
leaves a running daemon alone. It never builds. When `rsid.service` is installed
it starts that unit instead. Enabling the managed service is the durable fix.
On macOS it loads the launchd agent instead (next section).

## macOS satellites (#2936)

A Mac (the `mac-mini` satellite) runs the same `rsid` as a Linux satellite. Two
things differ: how the hub reaches it and what keeps its daemon running.

**Reaching it: direct mode only.** The dial-home installer and its forward are
systemd user units, so a Mac uses the hub-initiated direct tunnel ("Direct: hub
initiates SSH" above): `install-direct.sh mac-mini mac-mini` on the hub forwards
`~/.rsi/satellites/mac-mini-direct.sock` to the Mac's `~/.rsi/daemon.sock`. The
Mac needs Remote Login (sshd) on and the hub's key authorized. A non-interactive
ssh session there has PATH `/usr/bin:/bin:/usr/sbin:/sbin`, so call tools by
absolute path or through `zsh -lc '...'` (rustup's `cargo` lives in
`/opt/homebrew/opt/rustup/bin`, which a login shell may still lack).

**Keeping it running: the launchd user agent `dev.rsi.rsid`.**
`scripts/install-release.sh` (so `make release-install`) and
`scripts/rsid-start-satellite` render `scripts/launchd/dev.rsi.rsid.plist.in`
into `~/Library/LaunchAgents/dev.rsi.rsid.plist` and run the daemon through it,
never as a detached process:

- The job runs the same chain as Linux: `~/.rsi/install/rsi-socket-hold
  ~/.rsi/daemon.sock -- ~/.rsi/install/rsid-supervisor.sh ~/.rsi/install/rsid`.
  `RunAtLoad` starts it at login; `KeepAlive` restarts the chain whenever the
  holder exits (launchd waits `ThrottleInterval`, 10 s, after a run shorter than
  that). Output goes to `~/.rsi/daemon.log` (created `0600`); `Umask` is `077`;
  `ExitTimeOut` (3700 s) outlasts the supervisor's largest shutdown grace, like
  `TimeoutStopSec` in `rsid.service`; the open-file soft limit is 8192 (launchd's
  default is 256).
- The environment is only `HOME`, `PATH`, `LANG` and the socket path. Provider
  keys stay in `~/.rsi/.env`; nothing secret is written into the plist. launchd
  gives a job only the system PATH, so the installer writes one: `~/.local/bin`,
  `~/.cargo/bin`, the newest `~/.nvm/versions/node/*/bin`, `~/.bun/bin`,
  `/opt/homebrew/opt/rustup/bin`, `/opt/homebrew/bin`, `/opt/homebrew/sbin` and
  `/usr/local/bin` where they exist, then the installing shell's own lasting
  entries (temporary and sandbox directories are dropped), then the system
  directories. `RSI_LAUNCHD_PATH=<path>` replaces it. `GetHealthStatus`
  `provider_clis_missing` shows a CLI the PATH still misses.
- Every live install refreshes the plist. A restart that is not a quiet one
  (`make release-install NOW=1`, a daemon that is down, or one the daemon says is
  unsupervised) boots the loaded job out, stops any rsid started outside launchd
  (an older detached chain), loads the job again (`launchctl bootstrap
  gui/$UID`) and waits for a `GetHealthStatus` answered from inside the job's
  process tree. A quiet restart keeps the running job: the daemon swaps the
  binaries and exits 75, and the supervisor relaunches rsid inside the same job;
  a changed plist then takes effect at the next load. A quiet restart takes a
  `binaries_dir` only under `~/.rsi/staging`, `~/.rsi/sandboxes` or
  `~/.cargo/shared-target`, so from a checkout whose target is `~/rsi/target` it
  is refused `deploy_dir_not_allowed`: use `NOW=1`, or stage
  (`install-release.sh --stage-dir ~/.rsi/staging/<name>`) and deploy that.
- `scripts/rsid-start-satellite` on a Mac refreshes the plist and loads the job,
  or `launchctl kickstart`s a loaded job with no process, then waits for health.
- `RSI_LAUNCHD_AGENT=0` keeps the old detached holder chain (no plist, nothing
  survives a reboot); `rsid-start-satellite` then refuses.

Operate it with:

```bash
launchctl print gui/$(id -u)/dev.rsi.rsid      # state, pid, runs, last exit code
launchctl kickstart -k gui/$(id -u)/dev.rsi.rsid  # restart the chain now
launchctl bootout gui/$(id -u)/dev.rsi.rsid    # stop it (KeepAlive restarts any other exit)
tail -f ~/.rsi/daemon.log
```

What restarts what (verified on the Mac mini, 2026-10-18): rsid crashing or
killed ends the supervisor (a non-75 exit outside a deploy), the holder exits
with it and launchd restarts the chain at once. A killed holder makes launchd
send SIGTERM to the rest of the job's process group; the supervisor forwards it
and rsid shuts down gracefully before the restart. If the holder and the
supervisor are both SIGKILLed, rsid (its own process group) notices its parent
is gone within 2 s and shuts itself down (`rsid-supervisor.sh exited while rsid
runs`), so launchd's next holder finds the socket stale instead of crashlooping
on a live one.

Notes:

- rsid reads its parent's argv with `sysctl` `KERN_PROCARGS2` (macOS has no
  `/proc`), so a launchd satellite reports `supervisor_mode:
  rsid-supervisor.sh` and accepts deploys. A build without it refuses every
  deploy `deploy_needs_supervisor`.
- The supervisor runs under macOS `/bin/bash` 3.2 unless a newer bash comes
  first on the agent's PATH (`brew install bash` puts one in
  `/opt/homebrew/bin`). It is 3.2-safe (no empty-array expansion under
  `set -u`, which ended it at the first daemon exit); without `wait -n` it notices
  a daemon exit up to one poll (1 s) late.
- The hub-driven deploy (#171, "Deploying a build to a satellite" above) works
  unchanged: the Mac stages with `--stage-dir ~/.rsi/staging/bin-<sha>`, the hub
  calls `AgentRequestDeploy {peer_id}`, and rsid comes back inside the same
  launchd job.

**Operator-only steps** (they need an admin password or the console, so agents
cannot do them):

1. Restart after a power loss and never sleep: `sudo pmset -a autorestart 1`
   and `sudo pmset -a sleep 0` (System Settings > Energy: "Start up
   automatically after a power failure", "Prevent automatic sleeping when the
   display is off"). `sudo pmset -a womp 1` keeps wake-on-network.
2. After a reboot the agent starts only once the user is logged in (it is a gui
   user agent). FileVault is on, so automatic login is unavailable: someone
   unlocks the disk at the console, which logs that user in and starts the
   agent. For a planned reboot, `sudo fdesetup authrestart` skips that unlock
   once.
3. Provider logins on the Mac itself: `claude` (`/login`) and `codex login`, as
   the same user; `GetHealthStatus` `provider_logins` shows the result.
4. Remote Login on, with the hub's dedicated key authorized (above).

## Python verification in satellite briefs (#221)

A brief that delegates Python-only work to a satellite names the stdlib runner,
`python3 -m unittest -v <file>`, the one both hosts and the lander have. It never
requires `pytest`: neither the hub nor the laptop installs it. A host that lacks
a planned runner reports the typed `runner_unavailable` (the runner, zero executed
tests, a red job) in the `scoped_test` receipt's `python` entries instead of a
generic exit 1, and no other runner is substituted. Treat it as a missing
prerequisite to report, not a test result. Detail: `docs/agents/worker-contract.md`
("Python verification").

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

On a Mac, unload the launchd agent and remove its plist (the next
`make release-install` would install it again):

```bash
launchctl bootout gui/$(id -u)/dev.rsi.rsid
rm "$HOME/Library/LaunchAgents/dev.rsi.rsid.plist"
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
