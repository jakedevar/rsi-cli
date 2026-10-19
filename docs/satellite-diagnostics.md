# Hub satellite reachability diagnostics

`AgentGetDaemonInfo.satellites[].diagnostics` and manager satellite health rows
carry the latest hub attempt, its link UUID, check time and elapsed milliseconds.
These process-local observations are unknown after hub restart until a probe
finishes. At most 128 peer observations are retained; each peer keeps one attempt.
With multiple routes, the link UUID identifies which attempt the diagnostic
describes; it is not an aggregate verdict on all routes. Concurrent older attempts
cannot overwrite a newer check.

The fixed stages carry only closed states and optional integer timings:

| Stage | What the hub actually observed |
| --- | --- |
| `connect` | Custody checks and local Unix-socket connection |
| `transport` | Write to that forwarded socket |
| `daemon_rpc` | Response deadline, envelope and identity validation; `rpc` identifies identity or session-page exchange |
| `store_probe` | Successful satellite-local health-count read, only when that peer reports its elapsed time; excludes store-lock wait |
| `auth` | Always `unobserved`: the hub does not perform the tunnel's SSH authentication |

A silent daemon yields a successful connect/write and `daemon_rpc: timed_out`.
A refused connection leaves the later stages `not_attempted`. Write backpressure
instead yields `transport: timed_out`. A response timeout alone cannot separate
SSH forwarding delay, remote daemon scheduling, store-lock wait and SQLite read
delay: store and authentication remain `unobserved`. Old peers omit the new
store measurement and remain compatible. No stage carries error text, paths,
SSH targets or credentials. Timings saturate at 300,000 ms.

On Linux, direct links using the installer's `<name>-direct.sock` convention
also report the local `rsi-satellite-direct@<name>.service` state using a
read-only `systemctl --user show` query (250 ms, one attempt, 256 output bytes).
An active service is not evidence of SSH authentication. Custom socket names
are `unmapped`; reverse links are `remote_service` because their tunnel service
runs on the satellite. Other platforms report `unsupported` for a mapped local
service. Query errors and unknown output collapse to `unavailable`; no journal
or error text is exposed. No unit is started, stopped, reloaded or restarted.

Existing RPC deadlines remain 3 seconds for each write and response, four links
per peer, twenty session pages, four in-flight scheduled peer reads, and bounded
15-second to 300-second poll backoff plus jitter. Service observation adds at
most 250 ms per attempted link. There are no new retries or operator settings.

Daemon info also retains `latest_watchdog_trip` from the most recent verified
health report when a subsequent connection/RPC fails. Manager health already
retains that historical report; current diagnostics appear beside it. Historical
trips are not a statement that the satellite is currently unhealthy. These
diagnostics do not measure compile-pressure resilience (tracked separately).
