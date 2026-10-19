# Live daemon CPU debugging

Use `python3 -I scripts/rsid-sample-stacks --pid <rsid-pid> --seconds 5`
for a bounded, read-only local CPU investigation. Supply the known daemon process
leader PID; the script requires the same real/effective/saved/filesystem UID and
an executable named `rsid`. `--target rsi` or `--target rsi-agent-mcp` (#332)
samples the TUI or the MCP bridge instead under the same fences: the closed
`TARGETS` table in the script names each approved executable (`/proc/<pid>/exe`
must carry exactly that file name, so a lookalike such as `rsi-rpc` is refused)
and the crate paths reported (`rsi::`, or `rsi_agent_mcp::`/`rsi_common::`).
Adding a target is a reviewed edit of that table, never an argument. It never attaches with ptrace/gdb, reads live SQLite,
restarts the daemon, or changes permissions/sysctls. No remote command surface
is provided. HUB acceptance requires a HUB-measured run; a satellite smoke does
not establish it. The command above is the safe invocation for HUB review.
The default selects three hot threads; omit `--threads` in agent sessions because
the shell hook rejects that flag.

Linux little-endian x86-64 is supported; other platforms/architectures return an
explicit unsupported error. Python 3.9+, GNU `nm` (binutils), `libatomic`, and an
unstripped daemon are required; the `perf` executable is unnecessary. ABI constants
follow `linux/perf_event.h`, `asm/perf_regs.h` and `asm/unistd_64.h`. The target's
own `/proc/<pid>/exe` inode and ELF executable load segments determine symbols
and ASLR relocation, including deleted/replaced installed executables. Changed
executable mappings cause the run to discard its aggregates.

The script ranks at most 4096 threads over a 0.5-second CPU tick interval and
samples up to 3 hot threads for 5 seconds by default, using user-only software
task-clock events at 100 samples per CPU second. New threads after discovery
are not included. Hard limits: 8 threads, 30 seconds sampling, 2 seconds discovery,
8192 stack bytes per sample, 100 Hz, 30000 processed records, 50 symbols per table,
64 MiB symbol input and 15 seconds for `nm`. `--help` lists the bounded options.
The wall duration also includes discovery, symbol setup and bounded draining.

Aggregate **metadata + data** mmap bytes stay within the lesser of the actual
`perf_event_mlock_kb` and a conservative 516 KiB cap, without multiplying by CPU
count or using capabilities/RLIMIT allowances. Rings shrink or the thread count
falls to fit the budget. With 4 KiB pages, default 3-thread/32-data-page rings use
405504 bytes (396 KiB); four such rings would exceed 516 KiB, so four threads get
16-data-page rings. Other perf users may consume shared allowance: a denied
open/mmap is a failure, never a reason to raise the budget. All FDs/maps close
on success, failure, and interrupt. Only the sampler's perf metadata tail is
written; no target memory or persistent runtime data is written. Ring ordering
uses libatomic acquire/release, following the
[kernel perf ring-buffer contract](https://www.kernel.org/doc/html/latest/userspace-api/perf_ring_buffer.html).

Output contains only bounded aggregate leaf and inclusive crate-path symbol
counts per TID (`rsid::`/`rsid_store::` for the default target, plus the other
linked workspace crates `rsi_codegraph::`, `rsi_common::`, `rsid_core::`,
`rsid_remote_read::` and the bundled SQLite `rusqlite::`/`libsqlite3_sys::`/`sqlite3*`
symbols, #2943), a top **unmatched leaf** table naming the leaf symbols outside
those crates (`<outside target executable ...>` is libc, vdso or any address
outside the executable's own mapping), sample/lost/malformed/throttle/unmatched
counters and allocation limits. Raw stack, registers, addresses, memory, environment, and
credentials are never printed or saved. Inclusive counts scan aligned stack
words for possible return addresses and count each symbol at most once per
sample; this is **approximate**, not a DWARF unwind. Stale stack words (return
addresses left by an earlier call on the same thread, such as a digest that ran
before the real hot call) are counted as inclusive frames, so a frame can show
351/351 inclusive and still be stale. Read the leaf and unmatched-leaf tables
first, and confirm a suspect frame with the thread's
`/proc/<pid>/task/<tid>/io` (is it reading or writing?), the open fds and the
logs before trusting it. Data pointers may look like
return addresses; inlining, omitted frames, limited stack depth, unresolved
symbols and perf skid affect coverage. Leaf counts use the sampled IP; external
leaves are counted as unmatched while their rsid callers can still contribute.
Lost/throttled records indicate incomplete observations. Exit 0 requires samples
and at least one resolved rsid frame; denial, unsupported, no samples/frames or
record-cap exhaustion exit 1 (cap exhaustion prints partial aggregates), invalid
arguments exit 2, and interrupt exits 130. Zero counts do not prove a loop absent.

Deterministic decoder, relocation, budget, unsupported and cleanup tests run with
`python3 -m unittest -v scripts/tests/test_rsid_sample_stacks.py`; the scoped-test
and merge-queue Python selectors include them for changes to this script.

## Bus and draw rates (#332)

Sampling shows where CPU goes; these counters show how often work is requested.
Both are bounded aggregates: a closed key set, no payloads, no per-session
identity, no per-frame log.

- **Event bus.** `AgentGetDaemonInfo` (appointed manager or Epic lead) carries
  `bus`: `emitted_total`, `delivered_total`, `lagged_total`, `lag_events_total`,
  `subscribers`, `window_secs` and `by_kind` (`kind`, `emitted`, `delivered`,
  sorted, one row per `DaemonEvent` variant seen). `emitted` counts every
  `publish`, with or without a subscriber; `delivered` sums the receivers that
  held a slot for each event (an emit with no subscriber adds 0). `lagged` is the
  events receivers report skipping (`EventBus::record_lagged`, wired into the push
  stream and the watch, issue-tracker, chain-driver and until-evaluator listeners;
  other listeners that do not report are not counted); the channel does not name
  the skipped kinds, so `lagged` has no per-kind split. Read it twice, `T`
  seconds apart, and divide the deltas for exact events/s.
- **TUI draws.** The Diagnostics overlay (`:diagnostics`) lists frames and, per
  dirty reason (`key`, `mouse`, `paste`, `resize`, `push`, `poll`, `bootstrap`,
  `expiry`, `defensive`, `other`), `draws / marks`; marks that arrive before the
  next render opportunity coalesce into one draw, so `marks >= draws`.
  `animation_frames` are the pacer's follow-ups with no mark and
  `unattributed_frames` dirty draws without a recorded mark. For a headless run
  set `RSI_DRAW_STATS_FILE=<absolute path>`: the 1 s repaint tick rewrites a
  small JSON snapshot there (unset writes nothing).
