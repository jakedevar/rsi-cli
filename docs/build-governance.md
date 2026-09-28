# Agent build resources

Sandboxed agent sessions receive Cargo defaults when their process starts.
The defaults are four jobs per Cargo command and line-tables-only dev
debuginfo. Fresh children and reviewers also receive `CARGO_INCREMENTAL=0`
and use `sccache` when it is installed and enabled. Epic leads retain
incremental compilation. Non-sandboxed sessions keep their own Cargo setup.
Workers share `~/.rsi/agent-sccache` on each machine, separate from the
user's ordinary sccache cache.

`rsi-build-rustc` is installed beside `rsid` by `make release-install`. Cargo
uses it as `RUSTC_WRAPPER` in sandboxed sessions. Each compiler invocation
holds one advisory lock in `~/.rsi/build-slots`; the default machine-wide
limit is 16. Idle sessions and Cargo commands do not hold a slot. This
replaces the need to run agent Cargo commands through `~/.rsi/bin/cargo-slot`
after the new daemon and wrapper are installed. The lock root is local to each
machine, so a laptop satellite or a separate build host has its own pool.

The operator settings page, `GetDaemonConfig`, and `UpdateDaemonConfig` expose:

| Field | Default | Range or effect |
| --- | ---: | --- |
| `agent_build_jobs` | 4 | 1–32 jobs per Cargo command |
| `agent_build_line_tables_only` | true | dev debuginfo override |
| `agent_build_sccache_enabled` | true | worker cache when installed |
| `agent_build_sccache_cache_gib` | 10 | 1–512 GiB local cache cap |
| `agent_build_slots` | 16 | 1–64 compiler processes per machine |

Changes apply to newly started provider processes. A running sccache server
may keep its existing cache-size configuration; after changing the cap, let
active builds finish and run
`SCCACHE_SERVER_UDS="$HOME/.rsi/agent-sccache.sock" sccache --stop-server`
before the next build.
The next worker starts it with the new cap. Keep one sccache server per local
cache directory; concurrent servers sharing one directory can race.
Existing sessions retain their previous slot count until they restart, so
lowering the limit takes full effect after those sessions exit or resume.

The pool is cooperative: a command that bypasses the injected
`RUSTC_WRAPPER` does not take a slot. Agent builds that use Cargo normally
take slots automatically.
