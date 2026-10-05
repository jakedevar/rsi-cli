# Managed rsid on Linux

`scripts/systemd/rsid.service` is the opt-in user service for the daemon. It
starts the installed `~/.local/bin/rsid` with a fixed PATH that includes
`~/.bun/bin` for Codex. It requires `~/.config/rsi/rsid.env`; systemd loads
that file for each start, so a TUI shell or release-install caller cannot
silently change the daemon's provider environment. The TUI starts and connects
to the service when it is enabled.

Install after planning a restart of any currently running legacy daemon:

```bash
mkdir -p ~/.config/rsi ~/.config/systemd/user
chmod 700 ~/.config/rsi
install -m 600 scripts/systemd/rsid.env.example ~/.config/rsi/rsid.env
# Edit ~/.config/rsi/rsid.env with only the required KEY=value entries.
install -m 644 scripts/systemd/rsi-workers.slice ~/.config/systemd/user/rsi-workers.slice
install -m 644 scripts/systemd/rsid.service ~/.config/systemd/user/rsid.service
systemctl --user daemon-reload
make release-install
systemctl --user enable rsid.service
```

Stop a legacy `rsid` intentionally before `make release-install`; the daemon
singleton will reject a second process. Do not paste keys into commands, logs, the unit,
or this repository. The unit refuses an environment file outside mode `0600`.
The example contains
names only; the operator supplies values. Use `systemctl --user status
rsid.service` and `rsi-rpc GetHealthStatus` to check readiness.

`make release-install` builds, relinks the release binaries and, by default,
asks the running daemon for a quiet-point restart (`RequestOperatorRestart`,
#1122): the daemon stages and verifies the build, holds new worker starts, waits
until no landing, job, worker or manager is mid-turn (bounded by the printed
`release by` time), swaps the binaries in and restarts under
`rsid-supervisor.sh`, then verifies the running build. The TUI shows
`restart pending: waiting for N turns` in the RSID pane; `:manager restart now`
forces it and `:manager restart cancel` cancels it (the same as
`rsi-rpc ForceOperatorRestart` / `CancelOperatorRestart`). `make release-install
NOW=1` (or `--now`) restarts immediately. A daemon that is positively down
(socket absent or refusing and no rsid process) or answers that it is not under
the supervisor is restarted directly, as below; a timeout, malformed reply or a
socket that is down while an rsid process lives fails the install without
restarting anything and names `NOW=1`. Before building, the installer copies
any installed executable that points into Cargo's output directory to
`~/.rsi/install/`, and on the quiet-point path it does not relink: the daemon's
`.prev` rollback copy is therefore the previous working build, and the new set
is installed by the daemon at the quiet point. Re-running `make release-install`
while a restart is pending replaces the pending build with the newer one.

When the quiet-point path is skipped, `make release-install` relinks the
binaries and, when this unit is
installed, restarts it through systemd. It waits up to 90 seconds for a valid
`GetHealthStatus` response and reports startup progress. An installed but
inactive service starts on deploy; a live legacy daemon causes a refusal until
the operator performs the planned migration. `--no-restart` and `--tui-only`
retain their existing meanings. Daemon scope limits come from
`~/.rsi/rsid-scope.env`; release-install applies those limits to the managed
unit and worker slice before restarting it. The unit files do not hard-code
memory or CPU limits: release-install persists the operator's configured
systemd properties before first start and for subsequent boots. Do not enable
the service until release-install has applied those properties. If an active
worker slice has different limits and contains
workers, release-install leaves it untouched and admission remains closed
until those workers drain.

Before a planned restart, release-install runs the optional executable
`~/.config/rsi/pre-restart-drain` with the daemon socket path as its sole
argument. A failed hook aborts the restart. Issue #929 will provide the
operator drain RPC and can install a hook that calls it. Until then, the hook
is absent and this path does not claim to preserve in-flight turns. An
emergency systemd restart bypasses the hook.

`rsid --version` and `rsid --help` print and exit without starting a daemon.
