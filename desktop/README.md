# RSI Desktop

A desktop UI for rsi: a Tauri v2 window with a TypeScript frontend and a Rust
backend that talks to `rsid` over its JSON-RPC socket (`~/.rsi/daemon.sock`,
override with `RSI_DAEMON_SOCKET`). It runs alongside the TUI against the same
daemon.

```bash
./scripts/desktop.sh          # build frontend + run (debug)
./scripts/desktop.sh release  # optimized binary
cd desktop && npm test        # frontend model tests
cd desktop/src-tauri && cargo test --locked
```

`scripts/scoped-test` and the merge-queue gate run both test sets whenever a change touches
`desktop/`, `docs/keybindings.md`, `crates/rsi/src` or a crate that `src-tauri/Cargo.toml` names as a
`path` dependency (`scripts/desktop-test-gate.py`, on a private copy of this tree); a host without node,
npm, cargo or (Linux) `webkit2gtk-4.1` reports `runner_unavailable`, never green.
The private copy is the whole candidate tree minus build output, `node_modules` and `.git` (#3129), so a
compile-time `include_str!`/`include_bytes!` of a file outside `desktop/` (`docs/manual/rsi-manual.md`), a
`build.rs` read, or a `path` dependency inheriting `workspace.package`, `workspace.dependencies` or
`workspace.lints` from the root manifest builds on the hub exactly like it does in a checkout. Cargo still
builds only `src-tauri`'s own dependency closure.
Keep the desktop's separate lock in sync when changing rsi-common, rsi-graph or
their dependencies. A stale lock fails the shared lane as `desktop_lock_stale`:
refresh `desktop/src-tauri/Cargo.lock` with
`cargo update --workspace --manifest-path desktop/src-tauri/Cargo.toml` from the
repository root, and commit it with the dependency change. Use `--locked` for
desktop Cargo tests on every platform so local tests also catch this mistake.
To reproduce it by hand: `python3 scripts/desktop-test-gate.py --run` (set `DESKTOP_LANE_CARGO_TARGET`
to keep the Tauri build between runs).
The lane copies this tree first, so it never rewrites the tracked `node_modules` or `dist`.

Needs `webkit2gtk-4.1` and node. `cargo tauri build` (in `desktop/src-tauri`)
produces `.deb`/AppImage bundles.

## Getting started

### Install

You need node and npm, the pinned Rust toolchain, and the platform webview's
build files: `webkit2gtk-4.1` (with `pkg-config`) on Linux, or the Xcode
command line tools on macOS (`xcode-select --install`; Tauri uses the system
WebKit). From the repository root:

```bash
make desktop-install   # npm ci, npm run build, cargo build --release, install
make desktop           # build only; prints the binary (and, on macOS, the .app)
```

`make desktop-install` runs `scripts/install-desktop.sh --required`, which
fails loudly on a missing toolchain. It installs:

- every platform: `~/.rsi/install/rsi-desktop` (an atomic copy) and the
  `~/.local/bin/rsi-desktop` symlink;
- Linux: the app-menu entry `~/.local/share/applications/rsi-desktop.desktop`
  and icons in `~/.local/share/icons/hicolor/{32x32,128x128,512x512}/apps/`;
- macOS: `~/Applications/RSI Desktop.app`, an unsigned bundle around the same
  binary (open it from Finder, Launchpad or Spotlight).

An agent session proves the build and install through the daemon, not a hand-run cargo:
`AgentSubmitJob {"kind":"test","params":{"recipe":"desktop-install"}}` runs `make desktop-install-job`
in `rsi-build.slice` against a job-owned throwaway HOME (removed afterwards; the receipt carries the
exit code and the log path; `docs/agents/landing-and-gates.md`, "Desktop lane").

The real-rsid graph-run E2E (hub #3279) is a job too: `AgentSubmitJob {"kind":"test","params":{"recipe":"desktop-e2e-graph"}}`
builds `rsid` and `rsi-e2e-seed` together, runs `node scripts/smoke.mjs --isolated-write --only graph-run` and
judges the report (`scripts/graph-run-receipt.mjs`). It exits 0 only when `graph-seeded-daemon`,
`graph-seeded-render` and `graph-seeded-readback` all executed and passed; `graph-run-receipt.json`
(source SHA, daemon sha256, command, exit code) and the raw `e2e-report.json` are printed into the job log (the
output directory under the job's TMPDIR is removed when the job ends). A skipped
`RSI_E2E_RSID` test in `npm test` is not that evidence.

`make release-install` also runs the script, in its soft mode: a missing
toolchain or a failed desktop build only warns, and the rsid install goes on.
`RSI_INSTALL_BIN_DIR`, `RSI_DESKTOP_INSTALL_DIR`, `RSI_DESKTOP_APP_DIR` and
`XDG_DATA_HOME` move each target, so a temp directory gives a throwaway
install, and the first-run states below can be tried without touching your
own install or your rsid:

```bash
P=$(mktemp -d); mkdir -p "$P"/{apps,bin,install,xdg}
RSI_DESKTOP_APP_DIR=$P/apps RSI_INSTALL_BIN_DIR=$P/bin RSI_DESKTOP_INSTALL_DIR=$P/install \
  XDG_DATA_HOME=$P/xdg make desktop-install
# rsid missing: the window shows the "Cannot reach" guidance; the terminal shows boot=offline
RSI_DAEMON_SOCKET=$P/none.sock "$P/apps/RSI Desktop.app/Contents/MacOS/rsi-desktop"
```

### Run and connect to rsid

Start rsid first (`make release-install` restarts it; `./scripts/dev-daemon.sh`
runs a dev daemon), then start `rsi-desktop` or open the app. The desktop
connects to `~/.rsi/daemon.sock`; set `RSI_DAEMON_SOCKET` to use another
daemon. The top bar shows the link state, and the desktop reconnects by itself
when rsid restarts. It never sends a session token: it acts as the operator,
like the TUI.

For development, the commands at the top of this file build and run a debug
copy and run both test sets.

### What you see on first run

The empty right-hand pane tells you where you stand; nothing needs a terminal
to understand it.

| State | The pane says | Do |
| --- | --- | --- |
| rsid is not running (or the socket is wrong) | **Cannot reach the rsid daemon**: what the app is, the socket it looked at and the last error, how to start rsid (`rsid`, `./scripts/dev-daemon.sh`, `make release-install`), and `RSI_DAEMON_SOCKET` for another daemon | Start rsid; the top bar flips to "daemon connected" within about five seconds on its own, or press **Retry now** |
| rsid stops while a session (or the Issues list, or the dashboard) is open | A strip above the window: **rsid is not running. What this window shows is the last known state and may be out of date.** with the same Start rsid / Restart rsid / Retry now as the card (never a Start that could not work, local #258) and More. The open session stays, its status is tagged *last known*, Stop, Pause and Archive are off, Send stays on and a failed send keeps the draft. The top-bar pill becomes a button that jumps to the strip (local #266) | Press **Start rsid** (or **Restart rsid** / **Retry now**); when rsid answers the strip goes away and the same session carries on |
| rsid is up, no sessions yet | **Connected to rsid. No sessions yet.** with New session (Cmd-N, Ctrl-N elsewhere), the Actions… palette (Cmd-K) and the Vim keys switch | Press **New session** |
| rsid is up, sessions exist | **Select a session.** | Click one |
| the desktop backend itself did not answer | **The desktop backend did not answer** with the error | Reopen the app; read its terminal log |

### Did it boot? (not just "a window opened")

A window that opens proves nothing: an earlier build opened normally while its
Settings read had failed, and nobody saw it. Boot now reports every step
(`daemon`, `projects`, `labels`, `sessions`, `settings`, `live`) as ok, failed or
skipped, and the result is visible in three places:

- **the page**: `<html data-boot="ready|offline|failed">` and `data-boot-report`
  (the JSON the CDP harness reads); `ready` means every step passed and no
  uncaught page error was seen, `offline` means rsid is not reachable (the
  guidance above is showing; dependent steps are skipped), `failed` shows a red
  banner under the top bar naming the step and its error;
- **the process log** (stderr): `rsi-desktop: started version=… socket=…` when the
  window is created, then one line from the page,
  `rsi-desktop: boot=ready daemon=connected sessions=N steps=daemon:ok,…,live:ok`
  (`error="settings: …"` for each failed step). A start line with no `boot=` line
  means the page never booted;
- **after the fact**: when rsid comes up after an offline start the data steps
  re-run and the report turns `ready`.

To see it, run the binary from a terminal (`~/.local/bin/rsi-desktop`, or
`"$HOME/Applications/RSI Desktop.app/Contents/MacOS/rsi-desktop"`): the log goes to
that terminal.

### Mouse, menus and the palette, or Vim keys

You do not need the Vim keys. Every feature is an action you can reach with
the mouse:

- the **menu bar** (File, Session, Go, Issues, View, Help; on macOS also the app,
  Edit and Window menus);
- the **command palette**: Ctrl-K (Cmd-K on macOS) or the top bar's
  **Actions…** button. Type a few letters of a feature's name, then press Enter.
  It lists each action's keys too, so you can learn them as you go;
- **right-click** on a session row or a message;
- the toolbar buttons, whose tooltips name the action and its keys.

If you know the TUI, its modal keys work here too (`j`/`k`, `gg`, `:` commands,
the `<Space>` leader with a which-key hint) whenever focus is not in a text
field. `?` or Ctrl-Alt-G lists them. "Vim keys" and "Actions, palette and
menus" below cover the details.

### Settings

Open Settings with the **Settings** button in the top bar, Cmd-, (Ctrl-, elsewhere;
the app menu has Settings… on macOS), View › Settings…, `<Space>,` or `:set` (local #269).
One click on a row's value starts the edit: a switch flips, a choice steps (its left arrow
goes back), a text value opens its editor. Values read as words ("Provider: …", "On",
"Off"); the raw field names sit under the info card's **Details**. Closing with changes
that were not applied asks **Apply**, **Discard** or **Keep editing** (Esc means Keep
editing); another full-window view opening over Settings keeps the changes for when you
return. The **Vim keys**
switch (Settings › Workspace › Editing Mode, or View › Vim keys) turns the
modal keys off when you want a plain mouse-and-shortcut app; it is kept per
machine. Rows that belong to rsid (providers, hooks, bridges, budgets) are
saved through the daemon and apply to the TUI as well. See "Settings, themes
and help" below.

## Smoke test without a GUI (hub #3000 acceptance)

`scripts/smoke.mjs` boots the built `dist/` in headless Chrome and walks the acceptance checklist with
screenshots. It needs no Screen Recording, Accessibility or other macOS permission, and runs on Linux too.

```bash
cd desktop && npm run build
node scripts/smoke.mjs --out /tmp/desktop-smoke-fake        # a fake rsid: the primary mode, safe anywhere
node scripts/smoke.mjs --live --out /tmp/desktop-smoke      # the running rsid, READ-ONLY
node scripts/smoke.mjs --steps keyboard --out /tmp/smk-kb   # boot plus one step group (local #250)
```

With no flag every step runs, in order. `--steps core|settings|manager|keyboard|a11y[,…]` runs `boot` and the named groups
(`scripts/smoke-groups.mjs`); `tests/smoke.test.mjs` runs one group per test with its own 90 s budget, and a test fails
when a step declares no group, two, or an unknown one. `report.json` records each step's `durationMs`. A step names its
own `group` (and `refuses`, the writes it tries that the read-only bridge refuses); `smoke-groups.mjs` holds only the group
names and budgets. A new step: add a file under `scripts/smoke-steps/` (see the README there), so parallel slices never
edit the same lines.

- **How it works.** A local HTTP server serves `dist/` with a shim for the only Tauri surface the frontend
  uses (`__TAURI__.core.invoke`, `__TAURI__.event.listen`; `tests/smoke.test.mjs` fails when another file
  starts using `__TAURI__`). Each `invoke` is POSTed to the harness, which maps the Tauri command to the same
  rsid JSON-RPC method the Rust handler calls (`scripts/smoke-bridge.mjs` `READ_COMMANDS`). Keys go in through
  CDP `Input.dispatchKeyEvent`, screenshots come from `Page.captureScreenshot`.
- **Steps** (each pass, fail, `blocked-by-daemon` or `substituted`, with a screenshot): boot with no uncaught
  page error; session list and j/k; Enter opens a conversation; the Issues workspace; `:` command mode; `?`
  help; Settings; the hooks, providers and bridges dialogs; the terminal overlay; the default theme's text contrast (`text-contrast`). The embedded shell is
  Tauri-only, so the shim stubs the PTY (no process runs): the overlay renders, no shell. A step is
  `blocked-by-daemon` in `--live` when the running rsid answers `Method not found` for the read it needs.
- **Read-only.** The bridge forwards an exact allowlist of read methods and refuses (and logs) any other
  command, any argument that looks like a credential, and any malformed id. It never sends
  `$RSI_SESSION_TOKEN` or any credential, and redacts `GetDaemonConfig` before the page sees it. It is a test
  tool, not an rsid proxy: it listens on `127.0.0.1` on an ephemeral port only while the run lasts, and
  `POST /__invoke` needs a per-run random secret that only the injected shim knows plus the harness's own
  `Origin`. It lives in `scripts/` and is never part of the app or `dist/`.
- **Fake mode** (`scripts/smoke-fake.mjs`) is a Unix-socket server speaking the `daemon.rs` framing with canned
  reads; it records every method it receives, and the test asserts all of them are allowlisted reads.
- **Chrome.** `$RSI_SMOKE_CHROME`, `--chrome PATH`, the macOS Google Chrome or Chromium app, else
  `google-chrome`/`chromium` on `PATH`; with none the run prints `runner_unavailable` and exits 3 (the
  Chrome-driven test in `npm test` is skipped with that reason). Exit 0 is all green, 1 a failed step.
- `npm test` also runs `tests/smoke.test.mjs`: the shim, the allowlist (a mutation is refused and never reaches
  the daemon), the secret and Origin gate, redaction, and one full headless run against the fake daemon.

### UX audit toolkit (`npm run audit`, local #277)

An audit of window sizes, light/dark, many-session scale or mouse-first flows does not need a new driver:
`scripts/audit-lib.mjs` opens the built app against the fake daemon at a chosen size, theme and system colour scheme with a
bridge that records and stubs every write (never forwarded; it can fail on demand or play "rsid is down"), and carries a WCAG
text-contrast walker, a cut-off-controls check and a many-session / long-conversation fixture generator
(`scaleFixtures` in `smoke-fake.mjs`). `npm run audit -- sizes|themes|scale|all` reproduces audit 4's numbers in one command,
and the smoke step `text-contrast` gates the default theme's contrast. How to run an audit, write a flow, and which checks are
in the gate and which are on demand: [`docs/ux-audit.md`](docs/ux-audit.md).

### macOS WebKit keyboard smoke (`npm run smoke:webkit`, local #247)

Chrome is not the engine the macOS app ships: Tauri runs WKWebView, which with "Keyboard navigation" off (the macOS
default) and WebKit's default preferences skips every button without a `tabindex` (#242). The shipped app's wry sets
`tabFocusesLinks`, which lifts that skip (#257), so the smoke runs every step twice: `wry` (the shipped configuration,
`tabFocusesLinks` on) and `default` (the preference off, so the `tabindex` stamps stay proven). `WRY_PREFERENCE` in the
script pins the wry version that was read, and a test fails when `src-tauri/Cargo.lock` moves to another wry. `scripts/smoke-webkit.mjs` serves the built `dist/` on the
same fake rsid and bridge, compiles `scripts/webkit-smoke.swift` with `swiftc` (no Xcode project; the binary is cached
by source hash) and runs it with Keyboard navigation off for that process (`-AppleKeyboardUIMode 0`). The harness sends
real Tab and Shift-Tab key events through an `NSWindow`; the script checks that Tab reaches New session, Stop, Archive,
the conversation, the message box and Send in page order, Shift-Tab walks back, the confirm, question and New session
dialogs keep Tab inside and reach their buttons, and with Vim keys off Tab follows native order in Settings and Issues.
A control step per configuration proves it (`wry` reaches a button without `tabindex`, `default` skips it). Report:
`DIR/report.json` (default `/tmp/desktop-smoke-webkit`), about 20-25 s. The app never activates, so the operator's keyboard focus stays put.
It leaves nothing behind (#263): each harness run gets a temp `HOME` (removed afterwards) and the page a non-persistent
WebKit data store, so nothing is written under the real `~/Library/WebKit` or `~/Library/Caches` (a bundle-less binary
otherwise gets `webkit-smoke` folders there) and every run starts clean; `tests/smoke-webkit.test.mjs` compares those
folders before and after a real run.
Exit 3, `runner_unavailable`, off macOS, without a working `swiftc`, without a window server session (ssh), or when the
`default` control shows Keyboard navigation stayed on (and `wry` passed); `tests/smoke-webkit.test.mjs` then skips the real run with that reason.
Manual detail: `docs/acceptance-macos.md`.

### Packaged macOS app acceptance (`npm run smoke:app`, local #259)

The WebKit smoke is a bare WKWebView; the shipped app is Tauri's (wry configures WebKit differently, #257). This
on-demand check builds the real app (`scripts/install-desktop.sh --build-only`, nothing installed; or `--app PATH`),
runs a copy under the test bundle id `dev.rsi.desktop.smoke-app` with a temp HOME on the smoke's fake rsid, and drives
it from inside with `scripts/app-driver.m` (a dylib loaded through `DYLD_INSERT_LIBRARIES` into that copy only; no
macOS privacy grant). It checks the daemon-down card and the reconnect, Tab to New session, Stop and Send with Keyboard
navigation off, F6 between dashboard panes, Esc reaching vim in the terminal, toast and dialog ARIA and the window's AX
tree. Report and snapshots: `/tmp/desktop-smoke-app`. About 25 s plus the build (about 3.5 min cold). Not part of
`npm test`; detail and the gate decision: `docs/acceptance-macos.md`.

### Real-rsid end-to-end smoke (`--isolated-write`, hub #3105)

```bash
cargo build -p rsid --features e2e-seed --bin rsid --bin rsi-e2e-seed   # debug only; the seeder sits next to rsid
cd desktop && npm run build
node scripts/smoke.mjs --isolated-write --out /tmp/desktop-e2e                  # finds target/{debug,release}/rsid
node scripts/smoke.mjs --isolated-write --rsid /path/to/rsid --only normal,notes
RSI_E2E_RSID=/path/to/rsid npm test       # the opt-in test that runs the whole thing (skipped otherwise)
```

- **Isolated by construction.** `scripts/smoke-isolated.mjs` starts the given rsid binary itself with a fresh
  `HOME`, database and socket under `/tmp/rsi-e2e-*` and a scrubbed environment (no `RSI_*` variable except the
  socket path, no session token, no provider key), seeds a synthetic git project, two Issues, a Group and one
  finished session (never a provider launch), and removes everything afterwards. Only the processes it started are stopped. The write bridge
  (`createBridge({ isolated })` in `smoke-bridge.mjs`) throws unless the socket is one this run started and is
  still running, and never the operator's `~/.rsi/daemon.sock`; `--live` stays read-only and cannot be combined
  with `--isolated-write`.
- **Seeding the finished session (local #122).** `rsi-e2e-seed` (`crates/rsid/src/bin/rsi-e2e-seed.rs`) writes
  the session and its conversation through the store's own API (`insert_session`, `insert_event`) into the
  run's database while its daemon is stopped, then the daemon restarts and loads them; no SQL function is
  stubbed and no row is written by hand. It cannot reach production: it is a separate binary behind rsid's
  `e2e-seed` cargo feature (`required-features`), which `crates/rsid/build.rs` refuses in the release profile
  (no override, hub #3244)
  and `scripts/check-release-seam.sh` refuses in the release dependency graph; the daemon has no RPC, verb,
  flag or setting for it (`crates/rsid/tests/e2e_seed.rs` scans `src/`); and it writes only into a directory
  under the system temp directory that holds the harness's `.rsi-e2e-isolated-state` marker (never `~/.rsi`
  itself), refusing an `rsi.db` or `-wal`/`-shm`/`-journal` sibling there that is a symlink or not a plain
  file (hub #3245). The harness finds it next to the rsid binary (or `$RSI_E2E_SEEDER`) and reports `runner_unavailable`
  when it was not built.
- **Graph run (local #128).** The extra `graph-run` saves a one-node workflow (a session node pinned to the
  unconfigured `local` provider) straight into the isolated rsid, then drives `<Space>v`, `o`, `Enter`, `r` as CDP
  key events on the finished session. It asserts, by direct reads of rsid, that `ExecuteWorkflow` was accepted,
  that `UpdateSessionWorkflow` set the session's `workflow_id` to the workflow, and that the execution rsid recorded
  reached a terminal state (it ends `failed` at rsid's admission, before any model call: no real provider, key or
  network, and the egress guard still reports zero attempts). The bridge forwards `graph_upsert_workflow`,
  `graph_execute_workflow` and `graph_bind_session_workflow` in isolated-write mode only (never by default or with
  `--live`), mirrors `graphs.rs` params, and answers `graph_validate` with a stand-in for the Tauri-side
  `rsi_graph` preflight (structural errors only; rsid's own validation stays the authority). ExecuteWorkflow takes its
  repository from the daemon's working directory and its base from `origin/rolling`, so the harness starts the
  isolated rsid inside the synthetic project, whose `origin` is a bare repository beside it on the local disk. No
  Rust changed: the route needs no test-only code in any daemon build.
  Step 2 of the same extra (local #150) covers a SUCCEEDED run: the seed also writes, through `rsi-e2e-seed`
  (`graph_run` in its spec, `SEEDED_GRAPH_RUN` in `smoke-isolated.mjs`), a saved two-node workflow and its finished
  durable execution, each node settled `succeeded` with its result. The desktop remembers each workflow's last run
  (`localStorage` key `rsi.graph.lastExecution`, as the TUI keeps a draft's `last_execution_id`); the harness stands
  in for the earlier app session that showed it, opens the workflow with `o` and `Enter`, and asserts the run view
  (`E2E seeded run succeeded`, each node `succeeded  attempt 1 round 1`, a success badge on every node) against
  rsid's `GetWorkflowExecution` (status, per-node state, attempt and recorded result).
- **No outbound network (local #123, #130).** The isolated rsid runs in rsid's offline mode, `RSI_OFFLINE=1`
  (`crates/rsid-core/src/offline.rs`; tests and isolated harness runs only, never set by an install script):
  from its first boot no provider probe, poll, catalog fetch or credential check opens a connection, and each
  reports a typed offline state instead (a provider-key check reads `unknown` with detail `offline_unchecked`).
  As the witness, every proxy variable (`HTTP(S)_PROXY`, `ALL_PROXY`, both cases) also points at a local
  refusing listener (`scripts/smoke-egress.mjs`) that records each request line and answers 403. The run's
  `egress-guard` result FAILS on ANY attempt that reaches the listener, from the first boot on (there is no
  boot window and no allowed host), on a direct non-loopback connection of the rsid process (`lsof` sample
  after each section; skipped explicitly when `lsof` is missing), and when a provider key was set but rsid did
  not record its check as `offline_unchecked`. The report lists the attempts and the key check under `egress`.
- **What may be written.** The read allowlist plus a short table (`WRITE_COMMANDS`): Issue create, edit, status,
  archive and restore, session pin, provider key set/rotate/clear (a value must match `synthetic-e2e-…`; the
  answer is allowlisted metadata only), `update_daemon_config`, and the project-file changes of #113
  (`write_project_file`, `create_project_entry`, `move_project_entry`, `copy_project_entry`,
  `trash_project_entry`, `undo_project_file_operation`; closed key sets, one root, rsid judges the paths; the
  isolated HOME also holds rsid's project-file trash). The builders mirror the Rust handlers.
  Everything else, notably `send_message` (it would start a provider turn), is refused and logged.
- **What it covers.** One interaction per H2 section of the generated `PARITY.md` (`E2E_SECTIONS` in
  `scripts/smoke-e2e-sections.mjs`; `tests/smoke-e2e.test.mjs` fails when a section has none or names a row
  that is not in it), the Issue lifecycle (create, edit, cancel, archive, restore, each confirmed in rsid with a
  direct read and the confirmation steps exercised), the Vim keys switch turned off and on by real key events
  (Settings row Space, palette action) with the Issues pane honouring it (hub #3101), and a Settings write
  (`w` applies Retry on failure through `UpdateDaemonConfig`), and the file editor and explorer changes
  (`--only files-edit`, #113: a save, a stale-save refusal, a force-save, `:e!`, Ctrl-S and `:wq`, a draft kept
  across files, add / rename / copy / refused names / trash / undo by key, and the toolbar and right-click
  menu, each read back from the isolated rsid). Keys are CDP key events; end states are read back from rsid,
  not from the page.
- **Report.** `e2e-report.json` and `e2e-report.md` in `--out`: per section the PARITY row, source files,
  command, result, runner SHA (this checkout) and daemon SHA (the git HEAD of the checkout holding the binary,
  plus the binary's SHA-256), with explicit skips (the message box's send, which would start a provider) and
  observations. Exit 0 all green, 1 a failed step or an uncovered section, 3 `runner_unavailable` (no Chrome
  or no rsid binary).

### First-run and reachability checks (hub #3016, no GUI permission)

```bash
cd desktop && npm run build
node scripts/smoke.mjs --first-run --out /tmp/desktop-firstrun   # five boots in headless Chrome
node scripts/reachability.mjs                                      # table + exit 1 on an unreachable action
node scripts/reachability.mjs --by-section                         # one row per PARITY.md section (macOS evidence: docs/acceptance-macos.md)
```

- `--first-run` boots `dist/` against a bridge that behaves like each situation and reads back what the page
  shows and the boot evidence: rsid missing (guidance, socket, Retry now, `data-boot=offline`), rsid up with no
  sessions (New session opens the launch dialog), rsid up with sessions, a Settings read that fails
  (`data-boot=failed`, visible banner), and rsid starting later (Retry now turns the report `ready`). It runs in
  `npm test` when Chrome and `dist/` exist and is `runner_unavailable` otherwise.
- `reachability.mjs` is also a test (`tests/reachability.test.mjs`). For every action in the registry it checks a
  mouse or menu surface LISTS it: the native menu bar (macOS and other layouts), a right-click menu whose
  `contextmenu` listener exists, a toolbar `data-action` button, or the command palette, which the Actions…
  button, the View menu and a clickable list open without a key. A right-click-only action must be in a wired
  context menu. Whether an item is enabled is state-dependent (a disabled item says why); that is the registry's
  rules, tested in `tests/registry.test.mjs`. The check is negative-tested: removing the palette button, its
  click handler, a context listener, or an action's palette entry makes it fail.

## Platforms

| Platform | Builds | Runs against rsid | Install |
| --- | --- | --- | --- |
| Linux (the hub) | yes | yes | `make desktop-install` (binary, menu entry, icons) |
| macOS | yes (Apple Silicon, verified 2026-10-18) | yes | `make desktop-install` (binary and `RSI Desktop.app`) |
| Windows | Rust type check only | no: rsid is Unix-only | none yet |

**Linux.** Needs `webkit2gtk-4.1`. `cargo tauri build` in `src-tauri/` (with
`tauri-cli` installed) also makes the `.deb` and AppImage bundles that
`tauri.conf.json` names.

**macOS.** `make desktop-install` assembles the `.app` by hand (the layout
`cargo tauri build --bundles app` writes: `Contents/MacOS/rsi-desktop`,
`Info.plist` from `tauri.conf.json`, `Resources/icon.icns`), so no `tauri-cli`
is needed. The binary carries only the linker's ad-hoc signature. A bundle
you build yourself has no quarantine flag, so it opens without a Gatekeeper
prompt. To ship it to another Mac, the operator signs and notarizes it with
their own Apple Developer account. These steps use the operator's own
credentials and are never committed:

```bash
codesign --force --deep --options runtime --timestamp \
  --sign "Developer ID Application: <name> (<team id>)" "RSI Desktop.app"
ditto -c -k --keepParent "RSI Desktop.app" rsi-desktop.zip
xcrun notarytool submit rsi-desktop.zip --keychain-profile <profile> --wait
xcrun stapler staple "RSI Desktop.app"
```

**Windows.** The backend builds for `x86_64-pc-windows-msvc`. The terminal
overlay uses ConPTY and `%COMSPEC%`, and the home directory falls back to
`USERPROFILE`. rsid itself listens only on a Unix socket and does not run on
Windows yet (hub #1229), so on Windows every daemon call fails with a clear
"rsid's socket transport is Unix-only" error, and the tests that drive a fake
daemon over a Unix socket compile out. From macOS or Linux,
`desktop/scripts/windows-check.sh` type-checks every target for Windows. It
needs only `rustup target add x86_64-pc-windows-msvc`, and a stub stands in for
`llvm-rc`. A Windows CI lane (hub #1229 / #3001) runs this on a Windows runner
with MSVC and WebView2:

```bash
cd desktop && npm ci && npm test
cd src-tauri && cargo test && cargo build --release
# optional installers: cargo install tauri-cli --version "^2"; cargo tauri build --bundles nsis,msi
```

`icons/icon.icns` (macOS) and `icons/icon.ico` (Windows) wrap the committed
PNG sizes; `node scripts/icons.mjs` rebuilds them after a PNG changes.

## Parity status

`PARITY.md` checks every TUI key, command, overlay and view module against the
desktop (see "Updating PARITY.md"). Completion means preserved TUI behaviour, not
a row count: a read-only replacement for editing, or any unsupported feature, is
`partial` with the Issue that owns it, never `substitute`. At the time of writing
it lists 520 rows: none missing, none partial, 65 substitute, 455 done (the
global dashboard, `gd` / `:dashboard`, moved to done with #3366, and the New
session settings, which keep the saved manager grant, with #204).

Every `substitute` row is justified in `parity-audit.json` (see "Substitute audit
and source guard"). Interaction evidence is model and unit tests in this tree;
the real-rsid DOM harness (#115) is not part of it yet, and each audit group says so.

Frontend assets are embedded at compile time. `build.rs` watches `dist/`, so
after `npm run build` a plain `cargo build` picks up frontend-only changes.


## Layout

- `src-tauri/`: the Rust backend. It is a standalone Cargo workspace, so the
  root workspace build never pulls in Tauri/webkit.
  - `daemon.rs`: line-framed JSON-RPC client. Each call is bounded by a 20 s
    deadline and a 64 MiB response limit, and never sends a session token.
  - `lib.rs`: the closed command set (with the slice modules; the invoke
    handler is the generated `handlers.generated.rs`, see "How a slice
    registers"). Each command validates its input and forwards exactly one
    daemon method: `GetHealthStatus`, `ListProjects`,
    `ListSessions`, `GetSession`, `GetConversation`, `ContinueSession` (the
    daemon queues it when a turn is active), `InterruptSession` (soft pause)
    and `LaunchSession`. The webview has no generic passthrough.
- `src/`: the TypeScript frontend (`api.ts` typed command adapter, `model.ts`
  pure view logic, `main.ts` DOM). It has no framework or bundler and uses
  the global `__TAURI__` bridge. Its only runtime npm dependencies are
  xterm.js and its fit addon for the terminal overlay, vendored into
  `dist/vendor/` by the build (below). All daemon text is rendered as text,
  never as HTML.
- `static/`: the HTML shell and CSS (Catppuccin Mocha, matching the TUI).

## Vim keys

The desktop takes the TUI's modal keys whenever focus is not in a text field
(text input, textarea, select, contenteditable) and no dialog is open. The
**Vim keys** switch in Settings › Workspace › Editing Mode (or View › Vim keys)
turns them off (kept in `localStorage`). With it off, the panes with their own
key handlers (Settings, Issues, managers, files, graphs, DAG, ESP) keep only
the standard keys (arrows, Enter, Escape, Tab, paging) and ignore their modal
letters (`src/modal-gate.ts`; `tests/modal-gate.test.mjs` fails when a pane
handler skips the check).
Enter and Space on a focused control (button, link, summary, button-like input,
checkbox, radio, file input, `[role=button]`) belong to that control: one window
capture-phase guard (`src/activation-guard.ts`, first import of `main.ts`) stops
every later listener; native controls keep their browser activation, a
non-native `[role=button]` (including an `<a>` without `href`) gets one emulated
click (`modal-gate.ts` `activationGuard` states the contract).

- `src/keymap.generated.ts` is generated from the TUI's tables in
  `docs/keybindings.md` (the generated Normal-mode keys, reserved sequences and
  command-mode table) by `node scripts/keymap.mjs`; `npm test` fails while it
  is stale, so rerun it after a TUI keybinding change.
- `src/keyengine.ts` is the pure engine: counts (`5j`, `12G`), chords
  (`g`, `z`, `]`, `[`, `<Space>` leader tree) with a which-key hint, the `:`
  command line (Tab / Shift-Tab or Ctrl-N / Ctrl-P cycle completions, Enter
  runs, Esc cancels) and press-twice keys (`X`, `<Space>hc`).
- `src/keybindings.ts` lists the keys and commands the desktop can run. Every
  other TUI key or command answers "not in the desktop yet"; retired chords
  say why; an unknown key says it is not bound.
- `src/vimkeys.ts` is the DOM adapter. `j`/`k`/`gg`/`G` move a list cursor and
  `Enter`/`l`/`L` open it; session actions (`x`, `X`, `<Space>a`, `<Space>c`,
  `<Space>hc`, `i`/`a`/`o`/`O`, `<Space>gq`, `yy`) act on the open session. A
  bound key dispatches its action from the registry (below).
- Modifiers: on macOS Cmd acts as Ctrl (physical Ctrl works too) except the
  system chords Cmd-C/V/X/A/Z/Q/W/H/M; on Linux and Windows Ctrl-C/V/X/A/Z/Y
  stay with the webview and the Super/Windows key is never taken.

### Vim text fields (hub #3106)

With **Vim keys** on, a text field is a vim field (`src/vim-field.ts` over the
pure `src/vim-editor.ts`, a port of the TUI's `vim_textarea`): insert types
natively, Esc enters normal, `v` / `V` select. The grammar is the TUI's
"Vim Text Editing (Input Bar & Overlay)": counts (`3w`, `2dd`, `3x`, `3~`,
`3rX`, `5G`, `3J`), motions, `f t F T ; ,`, `d c y` with motions and the text
objects `iw aw i" a" i' a' i( a( ib i{ a{ iB i[ a[ is as ip ap`, `x s r D C J ~
p u C-r .`. A field in a dialog shows the TUI's mode row as a tag (`INSERT`,
`NORMAL`, `VISUAL`, `V-LINE`) after it while focused.

| Field | TUI surface | Desktop |
| --- | --- | --- |
| Message box (input bar) | Session input bar: vim on a non-empty draft | vim field; an empty draft hands every key to the session; mode badge before the text |
| Launch / continue prompt, Ctrl+G editor, prompt creator, Issue body, manager and entity forms, approval answers (every `<textarea>`) | Prompt overlay, input modal, forms | vim fields; the Issue description is multi-line and scrolls |
| Every text field of the Issue editor and the project form (Issue title, description, labels, assignee, the dependency number, the filter and search inputs; project name and folder; hub #3421, desktop #317) | "Every field is a vim textarea": opens in INSERT at the end of its text, Esc in INSERT goes to NORMAL only, Esc in NORMAL on a dirty Issue form shows UNSAVED CHANGES and a further Esc discards, Tab / Shift-Tab move between fields, the mode in the field border | wrapping vim fields (`src/form-field.ts`, rules in `src/form-text.ts`): a one-line field is a `<textarea data-line="single">` that wraps and grows with its text (then scrolls) and never takes a newline: Enter submits, a paste has its line breaks turned to spaces; the Issue title is cut at 200 characters in the editor only (live counter `n/200` from 150, a `TITLE LIMIT` hint when typing is refused or a paste cut; an older, longer title can still be shortened); the mode shows in the field border and tag; a click still places the caret; Vim keys off: the same fields edit plainly |
| One-line text fields of forms and Settings (manager and entity forms, manager and entity forms, `hierarchy-run` name / tags / model, Settings value editor, graph name, admin plain fields) | "Each field is a shared vim text surface" (Settings, forms) | vim fields (`data-vim="on"`), one-line: `o` / `O` act as `A` / `I`, `J` is a no-op, `p` flattens newlines; Enter still submits from normal mode |
| Provider-key, token and every `type=password` / `data-secret` field | Masked, write-only | never a vim field: masked, Esc closes the dialog, nothing is echoed |
| Search / filter boxes, the palette, the `:` line, the file viewer's `:` and `/` prompts, the destructive-confirmation phrase, the terminal's hidden textarea | single-line prompts, not vim textareas | plain fields (`data-vim="off"` or unmarked) |
| File viewer buffer | shared vim surface | view mode: navigation and `/` search; `i a A I o O` (or any editing key) start the editor, a text box driven by the vim editor above (Esc and Vim keys off: a plain text box; Esc leaves a clean buffer); its `:` line: `:q` (refused while unsaved), `:q!` and `:quit!` close, `:<n>` goes to a line, `:w` / `:w!` / `:wq` / `:x` save through rsid (`:w` on an unedited file rewrites it, as the TUI does), `:e!` reloads from disk; Ctrl-S saves; `:set [no]autopair` toggles bracket auto-pairing (on by default); while editing the viewer's colours, search (`/ ? n N * #`), folds (`z a o c M R`), bracket match and git signs all work; Ctrl-V pastes the clipboard (the TUI has no visual-block mode) |

With Vim keys off every field is an ordinary field: typing, arrows, selection
by keyboard or mouse, the Actions menu and the mouse all work and Esc closes a
dialog. Source-backed exceptions: the TUI has no count before a text object
(`2di(` ignores the 2), no Replace mode (`R`), `gU` / `gu`, blockwise visual or
ex commands in a text field outside the file viewer.

Evidence is real keyboard events through the mounted controls:
`node scripts/smoke-vim.mjs [--out DIR]` (fifteen scenarios in headless Chrome
against the fake daemon: modes, counts, text objects, dot / undo / registers,
the one-line and multi-line fields, every Issue editor field, the Esc contract,
the 200-character title cap, the project form, the filter / search / dependency
inputs, the viewer's `:` line, Vim off, the secret
field; `runner_unavailable` and exit 3 without Chrome). `tests/vim-parity.test.mjs`
holds the model's cases; `tests/vim-fields.test.mjs` runs the suite under
`npm test` when Chrome and `dist/` exist. The secret field's real submission
and the Issue writes are the isolated-rsid run's (`--isolated-write`).

## Actions, palette and menus

One action list, `src/registry.ts` (pure, tested in `tests/registry.test.mjs`),
feeds every way to run a feature: keys, the command palette, the menu bar,
right-click menus and toolbar buttons. Each action has an id (the same string
the key bindings name), a title, a menu group, its keys (read from
`src/keybindings.ts`, so the TUI notation stays the only source), an
availability rule that says why it cannot run now, and a `run` in
`src/commands.ts`.

- **Command palette**: Ctrl-K (Cmd-K on macOS, also from a text field), the
  top-bar **Actions…** button, View > Command palette…, `<Space>;` or
  `:commands` (`?`, Ctrl-Alt-G and `:help` open the keybindings help, below).
  Type to filter: title prefixes rank first, then an exact key
  (`gg`, `X`), word prefixes, substrings, key prefixes (`:arc`) and letters in
  order. Disabled actions stay listed with the reason. Arrows or Ctrl-N /
  Ctrl-P move, Enter runs, Esc closes. A query starting with `:` also offers
  to run it as a TUI command line (works with Vim keys off).
- **Session list from the keyboard** (local #226, `src/sessionlist-keys.ts`):
  the list is one tab stop, an ARIA tree whose active row is the cursor. Tab
  enters it on the open session (else the first row); Up/Down, Home/End and
  PageUp/PageDown move, Left/Right fold or unfold a Group/Epic row (or step to
  its parent / first child), Enter opens. ArrowDown in the Filter field moves
  into the list. These work with Vim keys on or off; `j`/`k`/`gg`/`G` keep
  their Vim meaning. A row reads its name and plain status.
- **Menu bar** (Tauri v2 menus, `src/menus.ts` → `src-tauri/src/menu.rs`):
  File, Session, Go, Issues, View, Help built from the registry, with enabled
  state and check marks kept in sync. macOS adds the app menu (About,
  Settings… Cmd-,, Services, Hide, Quit), an Edit menu (needed for Cmd-C/V in
  text fields) and a Window menu. The big menus nest submenus laid out in
  `MENU_LAYOUT` (`src/registry.ts`), grouped as the TUI's keybinding help
  groups them (View › Files, View › Managers, Session › Organize, ...), so no
  level has more than `MENU_SCAN_LIMIT` (30) entries; an action the layout does
  not name still shows at the end of its menu, and a test fails until it is
  placed. menu.rs caps each level at 64 items and installs the bar menu by
  menu: a menu over a cap is logged (`rsi-desktop: menu "view" not installed:
  ...`) and left out, the rest install, and the app shows a notice naming it
  (#305). Plain Ctrl-letter keys become native accelerators (Cmd/Ctrl-N new
  session, Cmd/Ctrl-K palette; off macOS none takes a text field's Ctrl-C/V/X/A/Z/Y);
  Cmd-N (Ctrl-N elsewhere) also works in the webview with the Vim keys off,
  from the message box too, and a keystroke that reaches both the menu and the
  webview runs once.
- **Right-click**: a session row opens it and shows its session actions; a
  message shows Copy message text, Expand or collapse, Message this session and
  Show tool events. A text selection keeps the webview's own menu.
- **Tooltips**: every toolbar button with `data-action` shows the action title
  and its keys.
- To add a core feature: add the id to `CoreActionId`, a spec to
  `CORE_ACTIONS`, a rule to `CORE_RULES` and a case to `run` in
  `src/commands.ts`; bind a TUI key in `CORE_KEY_BINDINGS`
  (`src/keybindings.ts`) if it has one. A slice adds its features from its own
  files instead (next section). The tests check that every bound key is in the
  palette and every menu is built from the registry.

## How a slice registers

Parallel slices (hub #3000) register without editing any shared file. A
slice adds files only:

1. **A pure spec module** `src/actions/<slice>.ts` (no DOM) that exports
   `SLICE: SliceSpec<MyActionId>` (`src/registry.ts`): its `name`, `order`
   (its place in menus, the palette and the snapshot), `actions`, one `rules`
   entry per action, and optional `keys` / `commands` in the TUI notation. It
   adds its own `UiSnapshot` field with a module augmentation:
   `declare module '../registry.js' { interface UiSnapshot { mine?: MySnapshot } }`.
2. **A DOM module** in `src/actions/` that exports `RUNTIME: SliceRuntime`
   (`src/commands.ts`): the `slice` it runs, `run(id, inv)`, and optional
   `snapshot()` (its UiSnapshot field), `afterAction(id)`, `init()`,
   `initOrder` (the order the inits, and so their DOM listeners, run in) and
   `claim(id, inv)`: take a core action before `commands.ts` runs it where the
   TUI gives its key a pane meaning, returning the action id that ran in its
   place (the navigation slice claims `gg` / `G` in the transcript and `a` /
   `o` / `O` for the message-box caret). Feature code may live elsewhere (`src/issues.ts` behind
   `src/actions/issues-run.ts`).
3. **Its parity rows** in `parity-status.d/<slice>.json` (see "Updating
   PARITY.md").
4. **Tauri commands** as `#[tauri::command]` fns in
   `src-tauri/src/<slice>.rs`. No shared line: the module declaration is
   generated too (`modules.generated.rs`, below); a test-only file named
   `*_tests.rs` is not a module (its owner pulls it in with `#[path]`).

`npm run build` runs `node scripts/slices.mjs`, which writes
`src/actions/index.generated.ts` (every `SLICE`; `ActionId` is
`CoreActionId | SliceActionId`), `src/actions/runtime.generated.ts` (every
`RUNTIME`) and `src-tauri/src/handlers.generated.rs` (the
`tauri::generate_handler!` list of every `#[tauri::command]` in
`src-tauri/src/*.rs`, which `lib.rs` includes), and
`src-tauri/src/modules.generated.rs` (the `pub mod <file>;` list of every module
file in `src-tauri/src/`, which `lib.rs` also includes, #181); `src-tauri/build.rs` runs the
same generator, so a plain `cargo build` or `cargo test` is current too). These
files, and `PARITY.md`, are build products listed in `desktop/.gitignore`: never
commit them (#60). A slice therefore adds only its own files, two slices built
from the same base merge with no conflict, and the merged tree needs no hand
regeneration: the next `npm run build` / `npm test` / cargo build rewrites them
from the merged sources (`tests/generated-merge.test.mjs` merges two scratch
slices to prove it). The generator refuses a Tauri command name defined in two
modules (the webview calls commands by bare name); `node scripts/slices.mjs
--check` writes nothing and also fails when an output is missing or stale. `tests/slices.test.mjs` checks with
`sliceProblems()` that no action id is registered twice, every action has
exactly one rule (a slice may take over a core action's rule, as slice 6 does
for `answer-question`), no rule is orphaned, no slice key or `:` command
rebinds a core one or another slice's to a different action, and every
`SLICE` has exactly one `RUNTIME`. Two slices may bind the same key to the
same action: the fold keys `zo zc za zM zR` live in both the detail and the
session-list key tables, and `src/actions/folds.ts` owns the one spec per id
and sends each run to the pane that has the keys.

**Keys a slice routes outside the registry (#81).** A slice that handles keys in
its own `window` / `document` keydown listener (G1's `routeGlobal` in
`input-model.ts`, G2's `routeNavKey` in `nav-model.ts`, the detail arrows, the
terminal toggle) declares the key tokens it takes in its `SliceSpec`, in
keybindings.md notation: `rawKeys: ['C-h', 'C-l', 'C-S-Left']`. `sliceProblems()`
then fails when two slices declare the same raw token, or a slice declares a
token another slice binds in `keys`. The check cannot see a listener on its own,
so `tests/slices.test.mjs` also scans `src/` for global keydown/keyup listeners
(`window.`, `document.` or `win.addEventListener`, or `onkeydown =`): a listener
in `src/actions/` fails unless its slice declares `rawKeys`, and one elsewhere
fails unless it is in the test's `RAW_LISTENER_ALLOWLIST` with the reason it
cannot collide (it only acts while its own pane or dialog is open, or it routes
no key). Listeners on a dialog or field are not global and are not scanned.
Declare a key as soon as you route it, or two parallel sweeps will collide at
runtime instead of in the test.

Core actions that open a slice's dialog (bare `:continue`, `<Space>gq`,
refresh reloading labels) still call that slice from `src/commands.ts`, and
`src/main.ts` still loads labels at startup; they are wiring, not registration.

## Session detail (hub #3007)

The transcript and the session dialogs, all in the registry above (the entries
live in `src/actions/detail-spec.ts`, the DOM side in `src/actions/detail.ts`,
the pure rules in `src/detail-model.ts`, tested in `tests/detail.test.mjs`):

- **Event cursor**: `Shift-Down` / `Shift-Up` select the next / previous shown
  event, `]u` / `[u` the next / previous user message (wrapping). `yy` copies the
  selected event once the keys are in the transcript, else the session ID.
- **Folds**: `zo` / `zc` / `za` on the selected event; a tool call and its
  result are paired by the provider's `tool_use_id` (never by adjacency) and
  move together. `zM` / `zR` collapse / expand all. `zs` / `zt` (and the View
  menu) hide system / thinking events, next to Show tool events.
- **Input bar priority**: a focused message box owns every key. `Esc` hands the
  keyboard back to the app keys (the draft stays), `Up` / `Down` scroll the
  transcript from a one-line draft, and `Up` / `Down` scroll it from anywhere
  else when nothing else has the keys.
- **Dialogs** (new `src-tauri/src/detail.rs` commands, each forwarding one
  existing daemon method): rename (`F2`, `UpdateSessionTitle`), rate (`:rate`,
  `UpdateSessionRating`), switch model / effort (`<Space>mm`,
  `GetSessionModelSwitchOptions` + `DiscoverModels` + `QueueSessionModelUpdate`),
  prompt preview (`p`) and the continue prompt (`:continue`). `Space N` and
  `Space ;` leave a dialog for a new session or the palette.
- The slice registers through `SLICE` in `src/actions/detail-spec.ts` and
  `RUNTIME` in `src/actions/detail.ts` (see "How a slice registers").

## Session lifecycle, questions and notifications (hub #3008)

Entries in `src/actions/lifecycle-spec.ts`, DOM in `src/actions/lifecycle.ts`,
pure rules in `src/lifecycle-model.ts` (tests: `tests/lifecycle.test.mjs`).
The Rust commands are in `src-tauri/src/lifecycle.rs`; each validates its input
and forwards one existing daemon method (`DeleteSession`, `UnarchiveSession`,
`ListDeletedSessions`, `UndeleteSession`, `PurgeSession`).

- **Delete to trash** (`DD`, `:delete`): acts on the session under the list
  cursor, else the open one. The TUI's confirmation is the double-tap `DD`, so
  the keys run at once; the Session menu, palette, right-click and buttons ask
  first. Running, starting and archived sessions are refused with the TUI's
  reasons.
- **Unarchive** (`U`), **downgrade a HARD pause** (`<Space>hs`, `:pause soft`).
- **Trash browser** (`<Space>gX`): sessions grouped by time period, `j`/`k`/`G`,
  `U` restores, `D` purges permanently. The TUI purges on one `D`; the desktop
  asks for a second `D` on the same session within 5 s.
- **Question modal** (`<Space>gq`): the open session's pending question, else
  the first session with one. `j`/`k`, `Space`, `1`-`9`, `Enter`/`Backspace`
  between questions, `i` for a free-text answer, `Ctrl/Cmd-Enter` submits, `d`
  sends "no preference; use your best judgment" (the button says **Let the agent decide**; the TUI has no
  separate "no" answer, so to say no pick an option or type it), `Esc`/`q` hides it (the question stays pending). Free text is kept
  per question (the TUI keeps one box). The banner above the transcript stays.
- **Notifications** (`<Space>n`, `:alerts`): every toast plus session
  transitions (completed, failed, interrupted, waiting for you), active first,
  then history; `x` dismisses one, `N` all, `Enter` opens the linked session
  (switching to the Archived view when needed). The list lives for the app run.
- **Recent completions** (`gr`): the six latest finished sessions.
- Not in the desktop: pin, rotate and the other lifecycle toggles, the Jobs zone.

## Emergency stop (hub #3008 follow-up)

Entry in `src/actions/emergency-spec.ts`, DOM in `src/actions/emergency.ts`,
pure rules in `src/emergency-model.ts` (tests: `tests/emergency.test.mjs`). The
Rust command `emergency_stop_all` in `src-tauri/src/emergency.rs` takes no
arguments and forwards the TUI's request: `UpdateModelControlPolicy`
`{mode: "stop_all", interrupt_active: true}` (`Client::update_model_control_policy`,
called by `emergency_stop_all` in `crates/rsi/src/action_handler/daemon_config.rs`).
It halts running sessions; it does not spend anything.

- **Keys:** `<Space>X`, `:stopall` and `:stop-all`; also the Session menu and the
  palette. The TUI has **no confirmation** on any path (`LcAction::EmergencyStopAll`
  goes straight to `emergency_stop_all`, `action_handler/mod.rs:354`; the Settings
  row `model_control_stop_all` is the same call). So the key path (the chord and
  the `:` line) runs at once, and the menu, palette, right-click and buttons ask
  first, as the delete action does.
- **Result:** the toast is the TUI's `Emergency stop applied: N cancelled, M interrupted`.
  Each interrupted session gets a red `HARD` badge in the list until its pause is
  cleared or downgraded or it leaves the list; the open session re-reads its pause
  from the daemon. The desktop has no Settings panel yet, so the Settings action row
  has no counterpart.

## Terminal overlay (hub #3014)

`SLICE` in `src/actions/terminal-spec.ts`, `RUNTIME` in
`src/actions/terminal.ts`, pure rules in `src/terminal-model.ts` (tests:
`tests/terminal.test.mjs`), parity rows in `parity-status.d/terminal.json`
(see "How a slice registers"). The PTY is `src-tauri/src/pty.rs`
(portable-pty, the crate the TUI uses; its `pty_*` commands reach the handler
through `handlers.generated.rs`): no daemon method is involved. Shared lines
it keeps, because no per-slice hook exists for them:
`.manage(pty::Terminals::default())` and `.on_window_event(pty::on_window_event)`
in `lib.rs`, and one guard in `src/palette.ts`'s capture listener so a focused
shell keeps `Ctrl-K` (that listener runs ahead of xterm's, so the slice cannot
stop it from its own listener without also stopping the key reaching the shell).

- **Toggle** with `Ctrl-\` (also `Cmd-\` on macOS) from anywhere, including a
  text field, or `:term` / `:terminal` / `:shell`, View > Terminal, or the
  palette. Hiding keeps the shell running; **End terminal shell** (View menu,
  palette) kills it.
- The shell is `$SHELL` (else `/bin/sh`; `%COMSPEC%`, else `cmd.exe`, on
  Windows through ConPTY) in the open session's `working_dir`, or the home
  directory when no session is open or the directory is gone. Each session
  gets its own terminal; all of them are killed when the window closes.
- **Insert mode** (on opening): every key goes to the shell, including
  `Ctrl-C` (its interrupt) and, on Linux and Windows, `Ctrl-K` (the palette's
  chord elsewhere). The vim keys stand aside while the terminal has focus.
  `Esc` enters **normal mode** for the scrollback: `j`/`k` and the arrows a
  line, `Ctrl-d`/`Ctrl-u` half a page, `g` top, `G` bottom, `i`/`a` back to
  insert. On macOS Cmd-C / Cmd-V copy and paste.
- Output arrives as `pty-output` Tauri events (UTF-8 decoded across reads),
  the exit as one `pty-exit`; the pane sends `pty_write` (pastes split under
  64 KiB), `pty_resize` (only when the fitted grid changes) and `pty_kill`.
- xterm.js (`@xterm/xterm`, `@xterm/addon-fit`, exact pins in
  `package-lock.json`) is copied into `dist/vendor/` with its licenses by
  `scripts/vendor-terminal.mjs` during `npm run build` and imported the first
  time the overlay opens; nothing is loaded from the network. xterm styles its
  cells with inline styles, so the CSP's `style-src` allows `'unsafe-inline'`;
  `script-src` stays `'self'`.

## Settings, themes and help (hub #3009)

Slice 7 registers through `SLICE` in `src/actions/settings.ts` and `RUNTIME`
in `src/actions/settings-run.ts`; the DOM is `src/settings.ts`,
`src/themes.ts` and `src/keyhelp.ts`, the pure rules `src/settings-model.ts`,
`src/theme-model.ts` and `src/keyhelp-model.ts` (tests: `tests/settings.test.mjs`,
`tests/theme.test.mjs`, `tests/keyhelp.test.mjs`).

- **Generated from the TUI.** `node scripts/settings.mjs` writes
  `src/settings.generated.ts` from `crates/rsi/src/settings_registry.rs` (groups,
  sections, every row's label, summary, owner and apply class), `settings.rs`
  (`DaemonFeatureEntry::defaults`: each daemon row's switch or choice list),
  `overlay/daemon_text_editor.rs`, `ui/theme.rs` (palettes, themes, role
  baselines) and `ui/theme_roles.rs`. `tests/settings-gen.test.mjs` fails while it
  is stale, and the parser throws on a shape it does not know.
- **Settings pane** (`<Space>,`, `:set`, View › Settings…; `:hooks` / `:skills`
  open it at that section): the category rail, section tabs, rows and an info card
  (what Enter does, when a change applies, where it is stored). Keys: `j`/`k`,
  `h`/`l`, `Tab`/`Shift-Tab`/`]`/`[`, `Enter`/`Space`, `+`/`-`, `w` apply, `R`
  refresh, `/` `n` `N` search, `Esc`/`q` close and discard. It reads
  `GetDaemonConfig` and stages edits; `w` sends ONE `UpdateDaemonConfig
  {"updates": [...]}` with every changed field (the daemon applies all or none)
  and re-reads the config. The TUI's text editors (cold-storage dir, SearXNG URL,
  launch overrides, subagent and fallback models, spend cap) save on Enter, as in
  the TUI. The Tauri commands are in `src-tauri/src/settings.rs`.
- **No placeholders for rsid-backed rows** (local #125). Every Settings row is one of:
  a control over its rsid operation, an overlay the row opens, or a documented
  TUI-only preference (`tests/settings-state.test.mjs` fails on any other row).
  *Model control mode* is a staged choice (normal, pause-background, deny-paid,
  local-only, stop-all); `w` sends the config batch first, then the mode through
  `UpdateModelControlPolicy {mode, interrupt_active: false}` on its own, as the
  TUI does, and a refused mode stays pending alone. The row values are read through
  `GetModelControlStatus` (`src-tauri/src/settings_state.rs`). The daemon-state
  rows are served by `src/settings-state.ts` (pure wording in
  `src/settings-state-model.ts`): *Emergency stop* (asks, then `stop_all` with
  active work interrupted); *Usage* and *Stop-all and cancel* (`GetUsageStats`
  and `GetModelControlStatus`: lifetime spend, spend by model, warnings, blocked
  requests and live work; Enter or `c` cancels the picked running invocation
  with `CancelModelInvocation`, `X` stops all, both ask first, the TUI does not);
  *Cold-storage status / last run* (`GetColdArchiveStatus`, Enter refreshes) and
  *Run pass* (`RunColdArchiveNow stage`); *Sandbox storage*
  (`GetSandboxStorageStatus`), *Preview reclaim* (`RunSandboxBuildCacheReclaim`
  dry run) and *Reclaim now* (same method, asks first; the TUI does not); *Warm
  build pools* (`GetTargetPoolStatus`). Status rows load when the pane opens and on
  `R`. *Source-worktree settlement*, *Legacy scratch adoption*, *Satellite
  registry*, *Remote access* and *MCP servers* open the admin overlays, with
  their own confirmations. No new RPC was needed.
- **TUI-only rows.** 14 presentation preferences the TUI keeps in its own
  `state.json` (formulation animation and speed, activity indicator, detail
  column, navigator preset and columns, card fields, the transcript defaults,
  submit-on-Enter, auto-open question panel, prompt compiler, default model) have
  no rsid state; they show "TUI only" with the reason and are documented in
  `parity-audit.json` (`tuiOnlyRows`). The Claude skills row opens the skills list,
  whose `e` and `d` run over `SetSkillEnabled` / `DeleteSkill` (#97); the model-role rows' other fields stay
  read-only here. Credential values are never shown, and `get_daemon_config`
  redacts any credential-like field and URL user-info before the webview sees it.
- **Rows another slice serves.** `registerRowOverlay(rowId, …)` (`src/settings.ts`)
  lets a slice own a row's Enter and its TUI list keys (`a`, `d`, …) from its own
  init: the operator-configuration slice (its own section below) serves Claude
  Hooks, API Providers, the two bridge rows and the model-role routes this way,
  and the prompt-creator slice the Claude Skills row.
- **Themes** (`T`, `:theme [name]`, Settings › Appearance): the TUI's 14 built-in
  themes plus the desktop's own default (the shipped Catppuccin Mocha). A theme
  sets the desktop CSS variables through the semantic roles (`DESKTOP_VARS` in
  `src/theme-model.ts`); the picker previews live and `Esc` rolls back. The
  **role editor** (Settings › Theme roles) overrides one of the 17 roles with a
  WCAG contrast check; a low-contrast colour needs a second Enter, `Delete`
  resets the role, `Esc` restores the opening overrides. Choice and overrides
  persist in `localStorage`. Terminal-default and transparent backgrounds use the
  palette's colour (a window has no terminal behind it).
  **Match system** (the first picker row, `:theme system`; the default on a new
  install, local #274) follows the system light/dark setting live: Light when the
  system is light, the desktop default otherwise. Any theme picked by name
  overrides it, and a profile that already stored a choice keeps it. The page's
  `color-scheme` follows the painted canvas, so checkboxes, selects and scrollbars
  match the theme. Text on a filled accent (badges, primary and danger buttons)
  uses `--on-<accent>`, which a theme sets to the colour that reaches 4.5:1
  (`onColor`); `tests/contrast.test.mjs` enforces the text, accent and fill pairs
  in Light, High Contrast and the default, and the smoke step `theme-system`
  emulates `prefers-color-scheme` (every other smoke page starts as a dark system).
- **Keybindings help** (`?`, Ctrl-Alt-G, `:help`, Help › Keyboard shortcuts):
  the core `help` action, which the slice's `afterAction` turns into the overlay.
  *Contextual* lists the actions that can run where you were, with their keys;
  *All Commands* lists every TUI key and `:` command with the desktop action it
  runs or "not in the desktop yet". `Tab`, `/` (all terms must match), `j`/`k`,
  `Ctrl-d`/`u`/`f`/`b`, `g`/`G`, `Esc` clears then closes, `q`/`?` close.

## Project files (hub #3012)

The file explorer (`<Space>e`), the fuzzy file picker (`<Space><Space>`) and
a file viewer and editor with a git gutter, registered as one slice
(`src/actions/files.ts`, RUNTIME in `src/actions/files-run.ts`, DOM in
`src/files.ts` and `src/files-ops.ts`, pure rules in `src/files-model.ts`,
`src/files-edit-model.ts` and `src/files-ops-model.ts`, Tauri commands in
`src-tauri/src/files.rs`, rows in `parity-status.d/files.json`).

- Every byte comes from, and every change goes to, rsid's project-root-scoped
  methods: the reads `ListProjectDirectory`, `ListProjectFiles`,
  `ReadProjectFile` and `GetProjectFileGitGutter`, and the operator-only
  changes `WriteProjectFile`, `CreateProjectEntry`, `MoveProjectEntry`,
  `CopyProjectEntry`, `TrashProjectEntry` and `UndoProjectFileOperation`
  (contract: `docs/agents/rsid-rpc-contracts.md`, "Project files" and
  "Project-file writes"); the desktop never touches the disk (the source guard
  in `npm test` enforces it). Each Tauri command checks the shape (exactly one
  of `project_id` / `session_id`, a relative `path` or `from` / `to`, caps in
  range, `content` at most 8 MiB, no unknown field) and forwards one method.
  rsid owns the scope, the stale-version fence, the never-overwrite rule and
  the undo log: a `..`, absolute or symlink escape comes back as
  `project_file_outside_root`, and a stale save, a `project_file_conflict` or
  an `project_file_undo_conflict` show in the TUI's words.
- The root is the open session's project folder, else its working directory,
  else the project the list is filtered to.
- The panels cover the detail pane and leave the session list usable. While
  the tree or the viewer has focus every plain key goes to it (the TUI's
  overlay keys); Ctrl/Cmd chords they do not use, such as the palette and the
  terminal, pass through. Clicking a panel focuses it, clicking elsewhere
  hands the keys back to the app.
- The picker scores with a port of fzy (smart case, best 100 results, ties in
  rsid's order); `tests/files.test.mjs` pins the ordering. The TUI uses skim's
  matcher; both rank exact, word-start and consecutive matches first.
- The viewer reads 1 MiB (a button reads up to rsid's 4 MiB ceiling), renders
  only the visible lines, and maps rsid's gutter markers onto the TUI's signs:
  `+` added, `~` modified, `-` on the line before a deletion.
- **Reading tools** (gap sweep G4, `src/viewer-model.ts`, tests in
  `tests/viewer.test.mjs`): a character cursor with the vim column and word
  motions, `/` `?` `n` `N` `*` `#` search with every hit marked, folds
  (`za zo zc zM zR`; regions from multi-line brackets and block comments,
  indentation for Python and YAML, fences and headings for Markdown, in place
  of the TUI's tree-sitter nodes), syntax colours for the TUI's tree-sitter
  languages from a small line lexer (no new dependency; the same lexer colours
  fenced `lang` blocks in transcript Markdown, `src/codeblocks.ts`), the
  matching-bracket highlight, and `<Space>m` for a rendered Markdown preview.
- **Editing** (#113; `src/files-edit-model.ts`, DOM in `src/files.ts`; tests
  `tests/files-edit.test.mjs`, the real-rsid run `--only files-edit`): in the
  viewer `i a A I o O` (or an editing key such as `x`, which also runs), the
  Edit button, View > Edit file or the palette open a text box on the file; with
  Vim keys on it is driven by the shared vim editor (`:` opens the viewer's
  command line, Esc leaves a clean buffer for the viewer), with them off it is a
  plain box. `Ctrl-S` and `:w` save; `:wq` / `:x` save and close; `:w!`
  force-saves; `:e!` reloads from disk and discards the buffer; `:q` is refused
  while the buffer is unsaved (`No write since last change (use :q! to
  override)`) and `:q!` discards it. A save sends the version `ReadProjectFile`
  answered; rsid refuses a file that changed on disk (`project_file_stale`) and
  the viewer re-reads it as the TUI does: a buffer equal to the disk says
  `Already up to date`, a deleted file is written again, anything else shows
  `Save refused — file changed on disk: <path> (use :w! to overwrite or :e! to
  reload)`. A file is edited only when it was read whole (1 MiB, or 4 MiB after
  the button), is not binary and carries a version; anything else is refused
  with the reason. CRLF files keep CRLF, and a file that mixes line endings
  (or has a lone CR) keeps each unchanged line's own break on save: a new line
  takes the break of the line above it, and `:w` on an unedited file rewrites
  it as it was read, as the TUI does.
  Opening another file keeps an unsaved buffer as a draft (a `●` in the tree;
  it returns when the file is opened again, flagged when the disk moved on),
  and closing the window asks while a draft is unsaved.
- **The viewer while editing** (#134; pure rules in `src/files-editor-model.ts`,
  tests `tests/files-editor.test.mjs`, the real-rsid steps `files-editor-view`,
  `files-autopair`, `files-mixed-eol` and `files-paste` of `--only files-edit`):
  the editor's text box is invisible and only takes the input; the lines are
  the viewer's own, drawn from the buffer, so everything the viewer shows
  carries over. Syntax colours are re-lexed as you type (after a 150 ms pause
  in files over 2000 lines); `/ ? n N * #` search in normal mode with every hit
  lit; `za zo zc zM zR` fold the regions of the current text, a caret that
  types inside a fold opens it and `j` / `k` step over a closed one; the
  bracket under the caret marks its pair; the git gutter signs follow the
  lines through unsaved edits (the lines changed since the save show none) and
  are refreshed after a save. Bracket auto-pairing is the TUI's
  (`handle_auto_pair`): while typing `{ ( [` types the pair with the caret
  between, a closer types over the same closer, and Backspace between an empty
  pair removes both; `:set noautopair` / `:set autopair` toggle it (on by
  default; brackets only, as the TUI). Ctrl-V pastes the system clipboard at
  the caret from any mode (leaving normal mode first), which is the TUI's
  Ctrl-V: its viewer has no visual-block mode, so none was added (`v` / `V`
  select as in the TUI; Cmd-V pastes natively). Limits: a mouse press puts the
  caret but a drag does not select text, and text being composed by an input
  method shows once it is committed.
- **Explorer changes** (#113; `src/files-ops.ts`, `src/files-ops-model.ts`):
  `a` new file (a trailing `/` makes a folder; missing parents are created), `A`
  new folder, `r` rename, `m` move, `c` copy (a folder as the destination
  receives the entry), `d` delete to rsid's trash after a yes/no (Enter, `y`,
  `d` confirm; Esc, `n`, `q` cancel), `u` undo, with the TUI's prompt labels,
  Tab path completion and Ctrl-A/E/B/F/H/U/W editing. A bad name (empty, above
  the root, an existing entry) keeps the prompt open with the TUI's message.
  The same operations are the toolbar under the drawer title, a right-click menu
  on a row, and View menu and palette actions. rsid keeps the undo log (64
  operations per project folder, in memory) and the trash
  (`~/.rsi/project-file-trash`, not pruned); undo never overwrites.
- The styles are a `<style>` element the slice adds at startup (the CSP's
  `style-src` allows inline styles), so the slice adds no static file.

## Graphs and topology (hub #3013)

Slice 11 registers through `SLICE` in `src/actions/graphs.ts` and `RUNTIME` in
`src/actions/graphs-run.ts`; the DOM is `src/graphs.ts`, `src/dag.ts` and
`src/esp.ts`, the pure rules `src/graph-layout.ts`, `src/graph-model.ts`,
`src/dag-model.ts`, `src/topology-preview.ts` and `src/esp-model.ts` (tests:
`tests/graphs.test.mjs`), the Tauri commands `src-tauri/src/graphs.rs`, the
parity rows `parity-status.d/graphs.json`.

- **Layout.** `src/graph-layout.ts` is a line-for-line port of the TUI's
  `ui/overlay/graph_layout.rs` in its cell units (components stacked, Kahn
  layers left to right, a cycle broken at the lowest node and flagged,
  orthogonal edge routes, the neighbour search and viewport clamp); the pane
  scales it into SVG. The tests lay out every starter template from
  `tests/fixtures/graph-starters.json`, which a `cargo test` keeps equal to
  `rsi_graph` (`RSI_DESKTOP_WRITE_FIXTURES=1 cargo test` rewrites it).
- **Graph review** (`<Space>v`, `:graph`, View menu): the TUI's keys
  (`h/j/k/l`, `[ ]`, `g/G`, `Enter`, `d`, `u`, `z`, `i/I/s/S`, `t/o/R`, `r`,
  `x`, `H/J/K/L`, `c`, `Tab`, `q`). Starter templates and the run preflight
  are the TUI's own client-side code: `src-tauri` links `crates/rsi-graph`
  (`graph_starters`, `graph_starter_workflow`, `graph_validate`; no daemon
  call). `r` saves the draft (`UpsertWorkflowDefinition`), checks it and sends
  `ExecuteWorkflow`; the run is polled with `GetWorkflowExecution` every 1.5 s
  while active and `x` sends `InterruptWorkflowExecution`. Once `ExecuteWorkflow`
  succeeds, the list's selected session (cursor, else the open one) is linked
  to the workflow as the TUI does: its `workflow_id` is set at once and
  `UpdateSessionWorkflow` persists it (`graph_bind_session_workflow`), but only
  when the session's own `workflow_id` differs. The TUI drops a refusal of that
  call; the desktop keeps the local link and shows it as a toast. The TUI draws
  no graph in the session pane (its comment saying so is stale), so none is
  drawn here; the next `<Space>v` on that session resumes the graph through its
  effective topology. `R` lists recursive
  graphs (`ListRecursiveTaskGraphs`, gated on `gv_render_recursive_origin`)
  and bridges one read-only (`GetRecursiveGraphAsWorkflow`); its unlocked
  nodes edit through `EditRecursiveNodeInstructions` / `EditRecursiveNodeSettings`.
  The TUI's info dashboard (`Tab`, or `l` at the right edge, gated on
  `gv_info_dashboard`) opens the DAG browser on that graph instead.
  The isolated-write smoke drives the run-and-link path (extra `graph-run`, local #128): see the
  "Graph run" bullet of the real-rsid section.
- **Recursive DAG browser** (`:dag`): the TUI's readbacks (the graph, its
  operational status, scheduler runs, the focused run and its events,
  cancellation requests, recovery and live recovery status, stale heartbeats,
  live attempts, validation results, artifact summaries with load-more), each
  optional one degrading to a warning as in the TUI. `R` / `L` (fake / live
  run, `max_steps` required), `C g` / `C r` (cancel with a typed reason,
  `requested_by = "rsi-desktop"`) and `c` (continue recovery with a budget) open
  only behind their daemon capability. Not yet: the bounded artifact preview,
  artifact and validation detail lookups, live-attempt artifact buckets.
- **Topology.** `gR` runs the cursor Epic's bound topology (`ExecuteTopology`
  with `parent_id` = the Epic) at once from the key, as the TUI does; the
  Session menu, palette and right-click first show the topology preview
  (`GetTopology`, the TUI's `render_dag_ascii` lines). `:topology run <name>
  [<epic>]` starts a stored topology (`ListTopologies`) on the Issues
  workspace's selected Issue and opens the run view; `:topology-resolve`
  sends `ResolveTopologyAttempt` with the TUI's idempotency key.
- **ESP Square** (`<Space>gc`): the 12-round game with the TUI's colours,
  flashes, debounce and p-value; a finished game is saved with `SaveEspGame`.

## Assist overlays and the operator to-do panel (hub #3015, slice 13B)

Two slices: `src/actions/assist.ts` (RUNTIME `src/actions/assist-run.ts`, DOM
`src/assist.ts`, pure rules `src/assist-model.ts`, Tauri commands
`src-tauri/src/assist.rs`, rows `parity-status.d/assist.json`) and
`src/actions/todo.ts` (RUNTIME `src/actions/todo-run.ts`, DOM `src/todo.ts`,
rules `src/todo-model.ts`, commands `src-tauri/src/todo.rs`, rows
`parity-status.d/todo.json`). Tests: `tests/assist.test.mjs`,
`tests/todo.test.mjs` and the Rust request-shape tests in both modules.

- **Memory search** (`<Space>M`): `MemorySearch` on every edit, ten global
  results as in the TUI. Up/Down or Ctrl-N/Ctrl-P move, Esc closes.
- **AI command** (Ctrl-Alt-A in the message box or the launch prompt; plain
  Ctrl-A stays select-all, as in the TUI's standard editing) and **AI chat**
  (Ctrl-Shift-A); both also in the Session menu and the palette. They call
  `GenerateText` with the TUI's own system prompts and layout. The model runs
  inside rsid: the desktop never calls a model API and sends no model,
  provider, endpoint or key, so rsid uses its configured prompt-processor
  model. The AI command's result is a preview: `a` accepts it into the draft,
  `d` / Esc discard it.
- **Dialectic** (`:ask`, `:ask <question>` fills the input): `QueryMemory`
  with the conversation so far, scoped to the current project (the open
  session's, else the list's project filter). `s` on an empty input shows the
  sources.
- **Card editor** (`:card`, `:card user`, `:card [user] add <fact>`):
  `GetEntityCard` / `SetEntityCard`; every change saves the whole card, as in
  the TUI. The TUI's keys (`j`/`k`, `g`/`G`, `a`, `e`/`i`/Enter, `dd`, `J`/`K`,
  Ctrl-S, Esc/`q`).
- **Operator to-do** (`<Space>dt`, `:todo`; `<Space>dj`/`dk`/`do`/`dd`/`dx`/
  `dy`/`dc`/`dv`/`du`/`da`): a right-hand panel and a top-bar badge over
  `OperatorTodoList` (read every 5 s), the pop-up with the TUI's keys, closes
  through `OperatorTodoClose` only after the pop-up's y/n, and the 60 s undo
  through `OperatorTodoReopen`. The open choice and the last visit (the
  catch-up baseline) stay in `localStorage`. A URL link opens in the system
  browser (`open_url`, `src-tauri/src/nav.rs`), else it is copied.
- The prompt creator (`<Space>gP`) is in the prompt-creator slice below: it reads
  and writes `~/.rsi/prompts/*.md` through the hub #3040 prompt-file RPCs.

## Normal-mode gap sweep G2 (hub #3000)

Every row of PARITY.md's Normal Mode Keybindings section is done or a
documented substitute (`<Space>gP` is the prompt-creator slice). Three slices
plus extensions of their neighbours:

- **Session operations** (`actions/sessionops*.ts`, `sessionops-model.ts`,
  `src-tauri/src/sessionops.rs`): `P` pin, `<Space>t` testing-needed,
  `<Space>r` auto-rotation, `<Space>k` cancel retry, `R` / `:rotate`,
  `<Space>x` commit and push (`/ci_commit`), `<Space>gl` / `:land` (a dry run
  arms the shown commit for 30 s; the second press queues exactly it, and the
  entry is watched through `GetRollingQueue {entry_ids}` until it settles).
  Rows show the pin, testing, rotation-off and retry markers.
- **Hierarchy** (`actions/hierarchy*.ts`, `hierarchy-model.ts`,
  `src-tauri/src/hierarchy.rs`): `<Space>G` / `<Space>E` / `<Space>gS` /
  `<Space>gT` / `<Space>B` open one create-entity form (CreateContainer for a
  Group or Epic, LaunchSession for a leaf kind) after the `legal_children`
  precheck against the container the cursor is in; `gL` / `:lead` sets the
  Epic lead.
- **Navigation** (`actions/nav*.ts`, `nav-model.ts`, `src-tauri/src/nav.rs`):
  `gj` Jobs zone (Active becomes the TUI's Main zone), `Backspace`,
  `<Space>q` / `:close`, `e`, `Ctrl-M` / `:model` (the New session dialog's
  default provider and model), `gf1`..`gf9`, `<Space>gg` lazygit and
  `<Space>bb` btop in the embedded terminal, `>` / `<` (project substitute),
  `<Space>T`; the modalkit pane keys (Left/Right, Ctrl-Tab, Ctrl-d/u/f/b,
  Ctrl-h/l, Ctrl-Left/Right, Ctrl-w h/j/k/l, Ctrl-Shift-Left/Right resizes the
  list); right-clicked transcript links open in the browser and inline file
  paths copy or open in the viewer.
- **Extensions**: F3 context-budget rows and `R` (slice 4), the compiled
  per-model effort ladder for `<Space>mm` (slice 5), `n` / `N` over matching
  sessions with no conversation open (slice 4), and the Issues workspace's
  Dispatched and Sync tabs (`[` / `]`, `P` poll), `T`, PageUp, associated
  sessions, the dependency candidate search and Ctrl-L / Ctrl-R in a stale form
  (`issues-tracker-model.ts`, `src-tauri/src/issues_tracker.rs`).

## Overlay gap sweep G4 (hub #3000)

The remaining overlay and view-module rows of PARITY.md. The slice is
`actions/views.ts` (RUNTIME `actions/views-run.ts`); most of the work is
shared widgets the other slices call:

- **Model dropdown** (`model-dropdown-model.ts` pure, `model-dropdown.ts`
  DOM): one picker for `Ctrl-M` / `:model`, the New session prompt and the
  create-entity form (`m`), with the TUI's tentative browsing, filter rules,
  provider cycle and keys. Catalogs come from `DiscoverModels`.
- **Prompts** (`prompt-model.ts`, `prompts.ts`, `src-tauri/src/prompts.rs`):
  `Ctrl-Q`, `Ctrl-Y` / `Ctrl-Shift-G` previews with `a` / `d`, and `?` in the
  launch and continue prompts and the input modal; the New session prompt's
  header row and settings side (`Ctrl-O`; model, effort ladder, sandbox,
  manager, scope, policy). A sandboxed launch goes through `launch_prompt`
  (`LaunchSession` with a git-worktree `sandbox`); a planned manager is
  appointed after the launch through one `ConfigureHarnessManagerWithPolicy`,
  the policy built by `rsi_common`'s own preset code (hence the `rsi-common`
  path dependency, already in the lockfile through `rsi-graph`). There is one
  New session dialog: stacking, tabs and splits are documented substitutes.
- **Create-entity keys** (`entity-form-keys.ts`): the form's normal, insert,
  body and topology keys, the model dropdown and the parent picker (`gp`).
- **Picker leader** (`overlay-keys.ts`): `Space N` / `Space ;` in the
  generic pickers and the telescope while the filter is empty.
- **Text-area background** (`textbg.ts`): the two Settings › Appearance ›
  Screen rows and the TUI's hex editor, as a desktop preference.
- **Budgets** (`budgets-model.ts`, `budgets.ts`, `src-tauri/src/budgets.rs`):
  Settings › Safety & Spend › Budgets lists `GetModelControlStatus`'s
  policies; the form follows `budget_policy_form.rs`, and a save or delete
  re-reads the list, refuses when the row changed, and sends one
  `UpdateModelControlPolicy` with `replace_policies` (mode unchanged, no
  interrupt). The settings pane also gains `>` / `<` / `=` panel widths and
  `a` / `d` / `s` / `r` / `c` / `i` on its list rows.
- **Help** (`keyhelp*.ts`): a Symbols view (`s`) and every overlay's own keys,
  generated by `node scripts/overlay-help.mjs` from the TUI's "Overlay Key
  Reference" into `src/overlay-help.generated.ts` (committed;
  `tests/overlay-help.test.mjs` fails when it is stale). Contextual help lists
  the open overlay's table first.
- **Recursive DAG inspector** (`dag-readbacks.ts`, `src-tauri/src/dag_inspect.rs`):
  the artifact lookup with links, the bounded preview (`p`), validation issues
  and a live attempt's artifact buckets (Enter in Live), each gated on its
  capability.
- **Tool pairing** (`tool-projection.ts`): calls and results pair by
  `tool_use_id` as `ui/tool_projection.rs` does; the transcript keeps one row
  per event and annotates both sides.
- **Command palette**: `Tab` puts the action's `:` command on the line to edit
  its argument; `Esc` leaves that first.

## v1 features

- Session list: polled every 3 s, active sessions first; filters for project,
  text and show completed. Finished sessions are listed by default (as in the
  TUI) with a muted label; unticking Show completed hides them, is remembered
  across launches (`rsi.desktop.showFinished` in localStorage), and never hides
  the session that is open.
  The filter box matches a session's label, id prefix, provider and prompt text
  (the first 20,000 characters). When it leaves the list empty, the empty row
  counts the matches that Show completed and the archive hide, or that sit in
  other projects, and offers Show completed, Show archived, All projects and
  Clear filter; it names only the filters that are on, and the Jobs tab has its
  own text. On a list over 400 sessions the first key after a pause filters at
  once and a burst of keys costs one more render, not one per key.
- Conversation view: incremental `since_sequence` polling every 1.5 s. Tool
  calls, results and thinking are collapsed; base64 blobs are elided and long
  messages folded. Older events load in pages.
- Send a message, interrupt, and launch a new session (choose provider, model,
  effort, project and working dir).
- Daemon connection indicator.

## Updating PARITY.md

`PARITY.md` is the checklist for "the desktop mirrors the TUI" (hub #3000). It is
generated and not committed (#60, `desktop/.gitignore`): **`npm run parity`**
(in `desktop/`) regenerates the registration files and `PARITY.md` and prints the
table to stdout, one command for a human who wants to read it (`npm test` also
writes it). The committed source of truth is `parity-status.d/`.

- `node scripts/parity.mjs` parses `docs/keybindings.md` and lists the
  `crates/rsi/src/ui/*.rs` and `ui/overlay/*.rs` modules, merges the hand-kept
  status files `parity-status.d/*.json` and writes `PARITY.md` (part of
  `npm test`). It exits 1, writing nothing, when a status file names an unknown
  row, two status files name the same row, a generated row has no status entry,
  or `parity-audit.json` does not justify a substitute.
- `node scripts/parity.mjs --print` writes the same file and prints it (what
  `npm run parity` runs, after `node scripts/slices.mjs`).
- `node scripts/parity.mjs --check` runs the same checks without writing and also
  exits 1 when `PARITY.md` is missing or stale. `--root DIR` reads `docs/` and
  `crates/` from another tree, to try an edited scratch copy without touching the repo.
- Mid-stack, while a commit moves a row from one slice's file to another's, both
  files briefly name it. `node scripts/parity.mjs --allow-duplicates` still writes
  `PARITY.md` (the first file's entry wins, the second is still validated), warns
  on stderr and puts a banner naming each pair atop `PARITY.md`; a row in three
  files still fails. `--check` refuses the flag and `npm test` never passes it, so
  a duplicate cannot land. Nothing needs regenerating during a rebase: no
  generated file is tracked.
- To record progress, edit your slice's file in `parity-status.d/` (a new
  slice adds `parity-status.d/<slice>.json`; `core.json` holds slices 1-3) and
  regenerate. Each file is
  `{"version":1,"rows":{"<id>":{"status":"done","location":"src/x.ts","note":"..."}}}`,
  and a row id in two files fails the check, so parallel slices never edit the
  same row: move a row into your file when your slice takes it over.
  `status` is `missing`, `partial`, `done` or
  `substitute` (a note is required, and `parity-audit.json` must justify the row).
  A `partial` or `missing` row needs `"owner": ["#N", ...]`, the Issue that will
  close the gap, shown in `PARITY.md` as `(owner #N)`. An id the generator
  does not produce fails the check, so a renamed or removed keybindings row is
  noticed. When keybindings.md changes, regenerate and fix the sidecar ids it
  names.

Row ids are `<kind>:<key>` and are derived only from the keybindings text, never
from line numbers or row order, so they survive unrelated edits:

| kind | id | source |
| --- | --- | --- |
| `key` | `key:<Keys cell>`, e.g. `key:j`, `key:<Space>gg`, `key:Ctrl-Alt-G` | the generated "All Normal-Mode Keys" table |
| `leader` | `leader:<After cell>` | `<Space>` sequences of the leader-migration table that the normal-key table lacks |
| `cmd` | `cmd:<command words, slugged>`, e.g. `cmd:help`, `cmd:manager-board` | the "rsi-Specific Commands" table; arguments are dropped |
| `global` | `global:<chord slug>/<context slug>` | the Global Keybindings tables |
| `overlay` | `overlay:<heading slug>` | each `###` heading under "Overlay Keybindings" |
| `overlay-ref` | `overlay-ref:<heading slug>` | each `###` heading under "Overlay Key Reference (generated)" |
| `section` | `section:<heading slug>`; a `####` is `section:<parent>-<heading>` | `###`/`####` headings elsewhere |
| `module` | `module:ui/<file>.rs` or `module:ui/overlay/<file>.rs` | the TUI view modules |

The slug is the heading lowercased with every run of non-alphanumerics turned
into `-`. Key rows from the generated tables are the ones to bind in the desktop
key engine: a keymap can look a binding up as `key:<Keys cell>`. An overlay row's
key count is the number of rows in its tables. Overlay, section, global and key
rows get a TUI module from the hints in `scripts/parity-lib.mjs`; a hint that
names a file crates/rsi no longer has fails the generator.

Live updates and Markdown rendering have no keybindings row, so they are not
tracked here.

### Substitute audit and source guard (hub #3104)

- **Inventory guard (#3003).** `node scripts/parity.mjs` (and `--check`) fails when
  the TUI gains a keybinding, command, overlay heading or view module
  (`crates/rsi/src/ui`) that no `parity-status.d` file names: every row needs an
  entry (`missing` with an owner when the desktop lacks it), and the error lists
  the new row ids. `tests/parity-audit.test.mjs` demonstrates each case against a scratch copy
  of the TUI sources, and the hub's desktop lane runs `npm test` for any diff that
  touches `docs/keybindings.md` or `crates/rsi/src`.
- **Substitute audit.** `parity-audit.json` maps every `substitute` row to a group
  naming what replaces the TUI behaviour, the rsid operation (or why none), the
  confirmation, the refusal and the interaction evidence. `parity.mjs` verifies the
  references: the TUI files exist, each rsid method is quoted in `src-tauri/src` and
  in `crates/rsi/src` (or the group says what the TUI does instead in
  `tuiEquivalent`), and each evidence anchor is in its file. A row that stops being a
  substitute must leave the audit; a new substitute without an entry fails.
- **Source guard.** `node scripts/source-guard.mjs --check` (in `npm test`) rejects
  direct file-system, store and rsi-state access in the shipped client: `src`,
  `static`, `src-tauri/src` (production code), the Cargo and npm dependencies and
  the Tauri capabilities. The only exceptions are the facilities documented in
  `source-guard.json` (the rsid socket path, the URL opener, the terminal PTY), each
  with a reason and a use count; an exception nothing uses fails as stale.
  `scripts/` and `tests/` are developer-host tools and are out of scope.

## Issues workspace

`<Space>i`, `:issues`, View > Issues or the top-bar **Issues** button open the
TUI's Issues workspace for one project (hub #3010). `:issue 12` jumps to
Issue #12, archived ones included. It is the Local tab of the TUI pane, over
the same operator RPCs (`ListIssuesPage`, `GetIssueInProject`, `CreateIssueV2`,
`UpdateIssue`, `UpdateIssueStatusV2`, `ArchiveIssue`, `RestoreIssue`,
`ListIssueDependencies`, `Add/RemoveIssueDependency`, `ListIssueEventsV2`,
`ListIssueHandoffs`). The Tauri commands are in `src-tauri/src/issues.rs`
(validated again by the daemon); the Dispatched and Sync tabs are not here yet.

- **Keys** (while the workspace is open and focus is not in a field):
  `j`/`k` (page edges load the adjacent page), `gg`/`G`, `Enter`, `r`, `/`,
  `f` filters, `s` sort, `O` operator requests, `n` new, `e` edit, `S`
  status/reopen, `p`, `a`, `l`, `b` dependencies, `dd` cancel, `A` archive,
  `U` restore, `yy` copy UUID, `y#` copy `#N`, `Tab` next inspector section,
  `PageDown` more history, `Esc`/`q` close. Every one is also a registry
  action (palette, **Issues** menu, right-click on a row).
- **Writes** follow the TUI's rules: a write carries the row version and a
  retry key; an uncertain failure keeps the identical request for a retry
  (submit again, or Ctrl-Enter for a form-less write such as restore); a stale
  version offers *Reload latest* or *Rebase my edit*; a dirty form needs Esc
  twice. Terminal Issues reopen with `S`; archived ones are read-only until
  restored.
- `src/issues-model.ts` (filters, requests, lifecycle, drafts, retry identity,
  keys) and `src/actions/issues.ts` (registry specs and rules) are pure and
  tested in `tests/issues.test.mjs`; `src/issues.ts` is the DOM.

## Managers (hub #3011)

The TUI's manager surfaces, over the same operator RPCs its client uses
(`src-tauri/src/managers.rs`, each command one daemon method, validated
again by the daemon; hub ruling #3015). Nothing is added to the agent verb
lists. One full-screen view (Esc/q closes) shows:

- **Manager tree** (`:manager tree`, `GetManagerTree`): folds (`h`/`l`,
  Space, `H`/`L`), `p` preview, `o` project, Enter opens the board, the
  global workspace or the seat session; `a` on a project row appoints the
  session selected in the list (keeping the live scope), `e` opens its scope
  picker, `x` revokes a project, portfolio, area or global row after a
  destructive confirm (`ConfigureHarnessManager`, `RevokePortfolioNode`,
  `RevokeManagerNode`, `RevokeGlobalManager`), each fenced on the version read
  just before.
- **Harness manager** (`:manager`, `<Space>gb` / `gd` / `gp`, `:manager
  board|decisions|inbox|inspect|policy`): `1`-`5` Board, Decisions, Inbox,
  Inspect (`[`/`]` subsections), Policy over `GetHarnessManagerState`.
  Decisions are answered with the record's exact key, version, digest and the
  scope/policy fences (`AnswerHarnessManagerDecision`; `h`/`l` option, Enter,
  `y` recommended, `a` free text; an unchanged retry keeps its key). `X`
  archives stale decisions after a count confirm. Policy edits the common
  fields and saves the whole policy (`ConfigureHarnessManagerPolicy`); presets,
  launches, provider ceilings and Group grants are kept as saved.
- **Scope picker** (`:manager scope`, `:manager appoint`, `:manager clear`):
  the TUI's `[x]`/`[+]` rules, 32 Groups plus 32 Epics, `a` whole project,
  `/` filter, Enter saves; an empty explicit selection revokes.
- **Global manager workspace** (`gm`, `:global-manager`) and its **Budget
  panel** (`b`): `+` or a typed limit is reviewed, saved through
  `ConfigurePortfolioNode` or `ConfigureHarnessManagerPolicy` and read back.
- **Fleet** (`:fleet`, `GetFleetOverview`), and the escalation, friction and
  portfolio lists (`:manager escalations|friction|portfolio`).
- **Global Dashboard** (`gd`, `:dashboard`, `:gd`; #3366): one view above every
  project over `GetGlobalDashboard {window}`. A row per project (the home
  project first, then by the `s` sort) and a pinned TOTAL row: live agents by
  role and provider, tokens in and out, cost, Issues open, in progress,
  created and closed in the window, operator requests, landings and QA. Column
  headers carry plain-words titles; zero counts are dim. A yellow
  **Incomplete** badge shows whenever the daemon capped a read (the numbers
  are then a lower bound). The side pane (`t`) shows the top-level manager's
  conversation (`Tab` focuses it, `i` types to it); with no live seat `n`
  opens the gm launch-and-appoint dialog, hosted in the home project, whose
  button is the confirm. `j`/`k` move, `Enter`/`l` open the project, `w`
  cycles 1h/24h/7d, `gm` opens the workspace, `r` refreshes (also every 10 s),
  `Esc`/`q` close. Rules: `src/dashboard-model.ts` (tests:
  `tests/dashboard.test.mjs`); DOM: `src/dashboard.ts`.

Credential-like record fields are never shown. `src/managers-model.ts`
(pure) and `src/actions/managers.ts` are tested in `tests/managers*.test.mjs`;
`src/managers.ts` is the DOM. What is still partial is listed per row in
`parity-status.d/managers.json`.

## Admin overlays (hub #3015, slice 13a)

The operator configuration overlays the TUI keeps in Settings, all under
**View** and in the palette. Entries in `src/actions/admin-spec.ts`, DOM in
`src/actions/admin.ts`, `src/admin-schedules.ts`, `src/admin-keys.ts` and
`src/admin-ops.ts`, pure rules in `src/admin-model.ts` (tests:
`tests/admin.test.mjs`). The Rust commands are in `src-tauri/src/admin.rs`
and `src-tauri/src/admin_ops.rs`; each forwards one operator method the TUI
client calls.

- **Scheduled jobs** (`<Space>K`), **Provider keys**, **MCP servers**,
  **Satellite registry**, **Remote access**, **Legacy scratch adoption**,
  **Source-worktree settlement** and **Diagnostics** (`:diagnostics`, which
  shows desktop measurements in place of the TUI's `RSI_PROFILE` metrics).
  Each overlay takes the TUI's keys.
- **Credentials are write-only.** A provider key or MCP secret is typed into a
  password field, sent once to its Tauri command, and the field is cleared on
  submit and on cancel. The Tauri side allowlists every response that can
  describe a credential, so only its state, the SHA-256 fingerprint prefix and
  the check metadata reach the webview. The tests feed it a daemon that echoes
  the secret back.
- The settlement dialog reads the latest run's receipt the way the TUI does
  (`GetSourceWorktreeSettlementRun`): who started the run, its outcome, each
  root as settled, refused or needing recovery with its reason in plain words,
  and the programs it left running because it could not prove them its own
  (what that means, and to stop them by hand).
- Settlement applies only after a fresh applyable audit and the exact typed
  `APPLY` phrase. The Tauri side checks the phrase again. Scratch adoption
  lists every path and sends nothing until `y`.
- Hooks, custom providers and message bridges are the operator-configuration
  slice below (they go through the hub #3039 daemon RPCs).

## Operator configuration and the prompt creator (hub #3039 / #3040)

Claude hooks, custom API providers and their model-role routes, and the
Signal / iMessage bridges are **one implementation**: the operator-configuration
slice from hub #3039 (`src/actions/operatorconfig-spec.ts` entries,
`src/actions/operatorconfig.ts` DOM, `src/operator-config-model.ts` rules,
`src/operator-config-api.ts`, `src-tauri/src/operator_config.rs`). It goes through the
daemon's operator RPCs, the same ones the TUI uses, so the desktop never reads or
writes `~/.claude/settings.json`, `tui-state.json` or the bridge TOML itself
(tests: `tests/operatorconfig.test.mjs`).

- **Claude hooks** (View › Claude hooks…, `:hooks`, Settings › Claude Hooks):
  the list, Ctrl-N adds, Enter edits in place (an event or matcher change moves
  the hook), Ctrl-D deletes. A stale revision (`claude_hooks_conflict`) asks to
  overwrite.
- **Custom providers** (View › Custom providers…, Settings › API Providers):
  add, edit, delete, and Ctrl-R routes a model role to a provider. The API key
  is write-only (a password field; the daemon returns only its state and
  fingerprint).
- **Model roles**: Enter on Settings' Title / Prompt compiler / Memory / Dream
  model rows opens that role's route picker (`SetCustomProviderRoute`); the picker
  marks the role's current route, read with `ListCustomProviderRoutes`.
- **Message bridges** (View › Message bridges…, Settings' Signal / iMessage rows).

The Settings rows are served through `registerRowOverlay`, registered from the
operator-configuration slice's `init`.

The **prompt creator** and the **Claude skills** list are the prompt-creator slice
(local #88, hub #3040 / #3062): entries in `src/actions/promptcreator.ts`, DOM in
`src/actions/promptcreator-run.ts`, `src/prompt-creator.ts` and `src/skills.ts`,
pure rules in `src/prompt-creator-model.ts` and `src/skills-model.ts` (tests:
`tests/promptcreator.test.mjs`). The Rust commands are in
`src-tauri/src/prompt_files.rs`; each forwards one method.

- **Prompt creator** (`<Space>gP`, View › Prompt creator): `ListPromptFiles`
  cards (name, first line, age, size); `j`/`k`, `Enter` opens the editor
  (`GetPromptFile`), `Ctrl+N` (or `n`) creates `prompt-YYYYMMDD-HHMMSS.md`
  (`PutPromptFile` with `create_only`), `d` moves the prompt to the daemon's
  trash (`DeletePromptFile`, which names the trash path), `m` the model badge
  picker, `q`/`Esc` close. In the editor `Ctrl+S` saves (an atomic put), Enter
  inserts a line break, and `Esc` or `q` (Vim normal mode) returns to the list;
  a dirty editor asks for a second close before it discards. Names are checked
  with the daemon's rules on both sides.
- **Claude skills** (`:skills`, Enter on Settings › Claude Skills): the list and
  Skill Preview over `ListSkills` / `GetSkill`; `e` enables or disables the
  selected skill (`SetSkillEnabled`) and `d` twice deletes it (`DeleteSkill`,
  #97), both run by rsid with the TUI's own rename and removal. The TUI's `d`
  asks for the same second press (2 s window, #131), so both frontends confirm.
