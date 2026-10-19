# Remote rolling shard gate

`rsi-rolling-land` can run its full rsid shards on a configured SSH host. Both
base and candidate shards run there. Shared base-cache entries are bypassed in
remote mode because a matching toolchain and runner fingerprint does not prove
that tests behave identically on two hosts. Failure comparison and isolated
retries still run in the lander. Other affected-crate commands remain local.

Agents never run the lander directly. Managers enqueue accepted SHAs with
`AgentEnqueueLandingSource {derive_filters:true}`. For `queue_disabled`, keep
the commit pushed and report with `AgentReportUp`, or send the result with
`AgentReportToHub` on a satellite. The operator enables the queue or lands with
`rsi land` or `EnqueueRollingQueueSource`. See
[Landing](agents/landing-and-gates.md#landing).

The queue's internal lander configuration requires all three remote flags
together: `--remote-gate-host`, `--remote-gate-dir`, and `--remote-gate-identity`.

`--remote-gate-run-as USER` defaults to `rsi`. The SSH login must be able to
run `sudo -n` as that user. The host needs Git, Python 3.11, and the project's
pinned Rust toolchain and `cargo-nextest` in the run-as user's cargo home. The
SSH host key must already be trusted in `known_hosts`. Set `CARGO_BUILD_JOBS`
to a value from 1 through 6 if the default of 4 is unsuitable.
When launching from a systemd user unit, put the pinned rustup proxy directory
ahead of the distro Rust binaries on `PATH` (for example,
`/home/USER/.cargo/bin:/usr/local/bin:/usr/bin:/bin`). The fingerprint
includes the actual Rust, Cargo, and Nextest versions used by the lander.

The lander sends an exact commit bundle, verifies the fetched and checked-out
SHA, and checks each shard's fingerprint before accepting the result. It
refuses publication with a typed `remote_*` reason if the host is unreachable,
the result is missing, or the SHA or fingerprint differs. It does not forward
the caller's environment, SSH agent, or credentials to the executor. The
executor attempts to delete its temporary run directory when the gate finishes.
