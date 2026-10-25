The managed daemon uses `rsid.socket` to retain its front door across
`rsid.service` restarts. Install both units together in the user's systemd unit
directory and reload that user's manager. Enable `rsid.socket` through
`sockets.target`; starting `rsid.service` also requires the socket. The socket
is owned by the user and has mode 0600. Keep the existing 0600 `rsid.env` setup.
For a custom daemon socket, change `ListenStream` and the daemon's configured
socket path together. An inherited listener at a different path or with a
different mode is refused before opening the database.

The supervisor launcher uses `rsi-socket-hold <socket-path> --
<installed-rsid-supervisor.sh> <installed-rsid>`. The holder opens the socket
once, never accepts connections itself, and passes `RSI_LISTEN_FD` to the
supervisor. Daemon exit 75, supervisor refresh and rollback retain that fd;
clients queue in the kernel backlog between daemon lifetimes. Stopping the
holder forwards the signal to the supervisor, waits for it, then removes only
its own socket inode. A competing holder, live socket or unsafe path is refused.
The holder requires the socket's parent directory to exist.

`install-release.sh` builds, installs and links the holder. On Linux its legacy
launcher runs the holder and supervisor in a collected systemd service with the
existing memory and CPU limits. On macOS the same portable holder wraps the
supervisor. The holder builds on Windows and reports execution unsupported.
The managed unit runs the installed `~/.rsi/install/rsid-supervisor.sh` with
`~/.local/bin/rsid`, using the same watchdog restart budget, deploy fallback
and offline database recovery as the holder path. The supervisor converts
its process-addressed systemd listener to `RSI_LISTEN_FD` for every daemon
lifetime, including script refresh and rollback. Each daemon validates the
listener and acquires the database leader lease before opening the store.
This changes the shipped unit only; update and reload an already installed
unit during the next authorized launcher restart. Turn detach remains off
by default.

The first transition from an old launcher to a holder still needs an explicit
launcher restart. A forced installer restart also replaces the launcher; the
stable listener covers daemon swaps within an existing holder and managed
service restarts. Moving a live host to these units and checking a swap remains
a separate operator/PM action.

## Manual holder swap check (#1760 / #1763)

The PM runs this on the hub after the next deploy, in a separate maintenance
window authorized for changing the live launcher. The review-fix worker must
not switch launchers or restart the live daemon. This check exercises the
holder/supervisor path, rather than the direct systemd socket unit.

1. In the same shell used for steps 5-7, retain the daemon configuration
   exports and resolve the socket with the installer's precedence:
   ```bash
   socket="${RSI_DAEMON_SOCKET_PATH:-${RSI_SOCKET:-${MOTHERSHIP_SOCKET:-${FLYWHEEL_SOCKET:-$HOME/.rsi/daemon.sock}}}}"
   export RSI_DAEMON_SOCKET_PATH="$socket" RSI_SOCKET="$socket"
   ```
   Record that socket path and `stat` inode, mode (0600), and owner;
   record exact holder, supervisor, rsid and one active provider PID. On Linux,
   `ps -o pid,ppid,args -p <recorded-pids>` and
   `stat -c '%i %a %U' <socket>` provide the baseline. Use `stat -f '%i %Lp %Su'`
   on macOS. Keep these observations in local deployment evidence.
2. From a separate operator client, issue DaemonInfo requests at 0.2-second
   intervals while the PM requests `AgentRequestDeploy` through its authorized
   control surface. The wire method is `AgentGetDaemonInfo`; use the authorized
   client's transport and an empty request body. Log timestamps, rsid PID,
   successful replies, and transport errors locally. Keep each request open
   across the swap: a backlog-queued response may take longer than 0.2 seconds.
   Continue until the replacement is ready. No ECONNREFUSED or ENOENT is allowed.
3. Verify rsid PID changed, holder PID stayed the same, and socket inode, owner
   and mode stayed identical. Verify the supervisor still has the inherited fd.
   On Linux inspect `/proc/<provider-pid>/fd` and the socket inode from the
   holder's fd symlink to confirm the provider does not hold that descriptor.
   Check only activation environment *names* (RSI_LISTEN_FD, LISTEN_FDS,
   LISTEN_PID, LISTEN_FDNAMES) in the provider; never print its whole environment
   or values. On macOS use `lsof -a -p <provider-pid> -U` for fd ownership and a
   controlled provider-child probe for environment names.
4. Launch a second installed rsid with the same configured socket/database
   from a clean operator shell, without activation variables. It must refuse
   the instance lease, leaving the live socket and daemon healthy.
5. In the authorized maintenance window, stop only the recorded holder (or its
   exact collected systemd unit). Wait for its supervisor and rsid to exit.
   Confirm its socket path is removed and the holder lock is released. Then
   start the holder/supervisor launcher fresh in the SAME step, never leaving
   the host without a daemon (#1862):
   `scripts/install-release.sh --link-only --now` (or `NOW=1 make
   release-install`). With no rsid running it starts one through the
   holder/supervisor path, waits up to 90 s for the socket and a
   `GetHealthStatus` answer from the launched process tree, verifies the
   launcher is still alive, and exits nonzero with `NO DAEMON IS RUNNING` if
   none comes. Treat any nonzero exit as an outage, not as a finished step.
   The restart is scoped to this install (#3476): the installer stops, and
   waits on, only the rsid that serves its socket (`RSI_DAEMON_SOCKET_PATH`,
   `RSI_SOCKET`, else `$HOME/.rsi/daemon.sock`), read from each candidate
   process's own environment (`/proc/PID/environ` on Linux, `ps eww` on macOS)
   or, on macOS, by the Unix socket it holds (`lsof`, #399: macOS 27 hides some
   processes' environment from `ps eww`; the same helper, `rsid_running_pids` in `scripts/rsid-processes.sh`, serves
   the launchd path and `rsid-start-satellite`). An rsid of another RSI home or
   socket (a test's temp home, a dev daemon) and a process that neither source
   places on this install's socket are never signalled and never waited on. The install output
   names the socket it is restarting. Never replace this with a host-wide
   `pgrep` or `pkill`.
6. Verify a live daemon and socket before reporting step 5 done. All three must
   pass, with the installed `rsi-rpc` and the configured socket (default
   `~/.rsi/daemon.sock`):
   - `pgrep -x -u "$(id -u)" rsid` prints a PID (a new one, not the recorded one);
   - `stat -c '%i %a %U' "$socket"` shows mode 600 and your user
     (`stat -f '%i %Lp %Su'` on macOS);
   - the `GetHealthStatus` answer is identity-bound, as the installer's is: a
     reply from any process holding the socket is not enough. From the repo
     root, with `holder_pid` set to the PID of the launcher you just started (the
     new `rsi-socket-hold`: the unit's `MainPID` on Linux, `$!` on macOS):
     ```bash
     source scripts/rsid-health.sh
     rsid_rpc_ready "$HOME/.rsi/install/rsi-rpc" "$socket" "$holder_pid"; echo "exit $?"
     ```
     Exit 0 means a daemon inside that launcher's process tree answered
     `GetHealthStatus` with `persistence_queue_capacity` in its `result`. Exit 1
     is no identified answer (a foreign or missing daemon); 2 means the launcher
     exited; 3 means the process that answered exited right after replying.
     `rsi-rpc --socket "$socket" GetHealthStatus` still prints the reply, but on
     its own it proves only that something answered.
   Then verify DaemonInfo succeeds with a new rsid PID, a new holder PID and
   mode 0600.
7. If step 5's installer exited nonzero or any check in step 6 fails, fall back
   to a direct start immediately; do not wait for a human or a reboot:
   From the repo root in that same configured shell, reuse the installer's
   environment selection (configuration by name, credentials from local config):
   ```bash
   source scripts/rsid-health.sh
   source scripts/rsid-scope.sh
   RSI_HOME_DIR="$HOME/.rsi" RSID_SCOPE_SETTINGS="$HOME/.rsi/rsid-scope.env"
   load_rsid_scope_settings
   rsid_daemon_environment
   unit="rsid-recover-$(date +%s)-$$.service"
   systemd-run --user --collect --service-type=exec \
       --unit="$unit" --slice=user.slice \
       --property="MemoryHigh=${RSID_SCOPE_MEMORY_HIGH_MIB}M" \
       --property="MemoryMax=${RSID_SCOPE_MEMORY_MAX_MIB}M" \
       --property="MemorySwapMax=${RSID_SCOPE_MEMORY_SWAP_MAX_MIB}M" \
       --property="CPUWeight=${RSID_SCOPE_CPU_WEIGHT}" \
       --property=StandardInput=null \
       --property="StandardOutput=append:$HOME/.rsi/daemon.log" \
       --property="StandardError=append:$HOME/.rsi/daemon.log" \
       "${DAEMON_ENV_ARGS[@]}" \
       --setenv="RSI_SOCKET=$socket" --setenv="RSI_DAEMON_SOCKET_PATH=$socket" \
       -- "$HOME/.rsi/install/rsi-socket-hold" "$socket" -- \
       "$HOME/.rsi/install/rsid-supervisor.sh" "$HOME/.rsi/install/rsid"
   launcher_pid="$(systemctl --user show "$unit" --property=MainPID --value)"
   wait_for_rsid_health "$HOME/.rsi/install/rsi-rpc" "$socket" 90 "$unit" "${launcher_pid:-0}"
   ```
   The four limits are the ones `install-release.sh` sets on its launcher unit,
   read from the same `~/.rsi/rsid-scope.env` snapshot; a recovery unit without
   them runs the daemon unbounded.
   (macOS: run the same holder command with `"$socket"` under `nohup ... &`
   in that configured shell, record `$!`, and pass it as the fifth argument to
   `wait_for_rsid_health` with an empty fourth argument). Where the managed
   `rsid.service` is installed, `systemctl --user start rsid.socket rsid.service`
   is the direct start; its socket unit and environment file must carry the same
   configured path and daemon settings (see the opening unit setup). Re-run the
   three step 6 checks (with the recovery launcher's PID as `holder_pid`), and read
   `~/.rsi/daemon.log` if they still fail. A stale socket file with no holder is
   removed by the holder; do not delete a socket that a live holder owns.

Retain the before/after observations and connect log as deploy evidence. A
failed invariant blocks enabling the holder on that host. Do not dump provider
credentials into the evidence. The focused automated portability gate is
`make check-socket-activation-macos` (an installed x86_64-apple-darwin Rust target
is required); it compiles the actual listener and holder sources and unit tests,
without needing the daemon's unrelated native C dependencies on Linux.

## Recovery when rsid is absent (#1862)

`install-release.sh --now` (with or without `--link-only`) never exits 0 with no
daemon: it starts rsid when none is running and fails loudly when no
`GetHealthStatus` answer from its live launcher tree arrives. The 2026-10-08
outage (about 22.6 h) happened
because a stopped holder unit is not restarted by systemd and the old installer
skipped the start.

A satellite whose daemon is down after a reboot starts with
`scripts/rsid-start-satellite` (#2820): it starts the deployed
`~/.rsi/install` daemon in a detached unit and persists the `rsi-workers.slice`
limits from `~/.rsi/rsid-scope.env` first.

Design note, not built: a watchdog outside the holder unit. Nothing currently
restarts rsid when the holder unit is gone, because the holder, supervisor and
rsid all die together. The portable shape is a separate, never-stopped probe
that every minute checks `GetHealthStatus` on the configured socket and, after
three misses with no rsid process, runs the step 7 direct start. On Linux that
is a user `rsid-watchdog.timer` and oneshot service (`Restart` is no help: the
holder unit is stopped on purpose). On macOS a `launchd` `StartInterval` agent
does the same; elsewhere the watchdog is unsupported and the installer's health
wait is the only guard. It must skip while `rsid-install-*.service`,
`rsid-recover-*.service` or `rsid.service` is activating, and while a
maintenance marker file exists, so a planned stop (step 5) is not undone.
Building it needs its own unit files, installer wiring and an operator opt-in,
so it is filed as a follow-up rather than done here.
