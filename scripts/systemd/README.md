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
The managed unit still starts rsid directly; supervisor rollback parity there
belongs to #1647 slice 9.

The first transition from an old launcher to a holder still needs an explicit
launcher restart. A forced installer restart also replaces the launcher; the
stable listener covers daemon swaps within an existing holder and managed
service restarts. Moving a live host to these units and checking a swap remains
a separate operator/PM action.
