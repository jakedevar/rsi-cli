# Running a desktop UX audit (local #277)

Audits #205, #221, #239 and #262 each rewrote the same driver: a bridge that stubs writes, per-size launches, theme and
system-appearance switches, a contrast walker and a big-install fixture. It is now committed, tested tooling in
`desktop/scripts/`. Reuse it; add to it when an audit needs something new.

Nothing here touches a real rsid, `~/Applications` or operator settings. Reads come from the fake daemon
(`smoke-fake.mjs`), writes are recorded and answered with a stub and are never forwarded (`smoke-stub-bridge.mjs`).
It needs Chrome (`$RSI_SMOKE_CHROME` or the usual places; with none it prints `runner_unavailable`, exit 3).

## One command per measurement

```bash
cd desktop && npm ci && npm run build                       # once
npm run audit -- sizes  --out /tmp/audit                      # 760x480, 1024x640, 1280x800, 2560x1440
npm run audit -- themes --out /tmp/audit                      # default dark, system light and all 14 built-in themes: contrast in EVERY main view (--themes light,ruby for a subset)
npm run audit -- keys   --out /tmp/audit                      # press every key each key table lists (63 swept, 9 skipped with a reason); FAIL when one changes nothing (--overlay "Trash,Sort Picker" for a subset)
npm run audit -- views  --size 760x480 --theme light          # every main view once at one size and one theme (defaults 1024x640, dark)
npm run audit -- views  --slow 3000                           # every view with each daemon read delayed 3 s (see "Slow and refused reads")
npm run audit -- views  --refuse                              # every view with each daemon read refused
npm run audit -- scale  --sessions 3000 --events 20000        # defaults: 800 sessions, 5000 events
npm run audit -- all                                          # all three, report under /tmp/rsi-audit-<time>
npm run audit -- flow scripts/audit-flow-example.mjs          # your own flow (below)
node scripts/audit.mjs flow scripts/audit-listbox-walk.mjs    # screen-reader walk: does every keyboard list announce its active row (VIEWS=a,b for a subset)
```

(`npm run audit` rebuilds `dist/` first; `node scripts/audit.mjs …` skips the build.) Screenshots and `report.json` go under
`--out`; a summary prints. Audit 4's numbers reproduce: `sizes` shows the 760x480 cut-off controls and clean 1024x640,
1280x800 and 2560x1440; `themes` showed the Light badge 4.05:1, hard-pause button 4.32:1 and High Contrast 4.41:1 misses
(fixed in #274) and now reports every theme clean; `scale` prints first paint (about 115 ms at 800 sessions), DOM size, long tasks, scroll and
the cost of opening the long conversation. Its filter row is `filterTyping6`: the keystroke time, the row count before, the row
count once the list's filter debounce has applied (`settled: true`, with `settleMs`), never the count from before the debounce.

### Every main view (`views` and `themes`)

`scripts/audit-views.mjs` lists the palette actions that open a screen, panel or dialog (`AUDIT_VIEWS`, about 65: Settings,
Issues, the manager views, Diagnostics, the admin dialogs, MCP servers, Satellite registry, Skill settings, Open manual, …).
`views` and `themes` open each one with real mouse events (Actions…, type the title, click the row), after a reload so one
view never leaks into the next, then run `contrastAudit`, `cutOffControls`, `overflowingText` and `docScroll`. One report row per
view: `{ view, title, opened, checked, failed, failures, cutOff, overflow, doc, toast, stubbedOnOpen }`; `stubbedOnOpen` lists any command the bridge stubbed as a write while the view opened (opening a view writes nothing, so it is a read missing from `READ_COMMANDS`, answered `null`, and the view is showing the wrong thing); a view that is unavailable with a
session open, or whose row is gone, is a row with `opened: false` and the reason. `themes` runs the whole list in each of the 16
theme runs (the default dark, the default under a light system, and every `THEMES` key of the built app; about 2 s a view, so
about 35 minutes in all: pass `--themes light,ruby` to split it so each call fits a 10-minute limit). Each theme's result is appended
to `themes.jsonl` when it ends, so a run that is cut off keeps the themes it finished. The registry does not mark which actions are views, so `tests/audit-views.test.mjs` fails when an
action that looks like it opens something (an ellipsis title, an `-open`, `open-`, `admin-` or `mgr-` id) is in neither `AUDIT_VIEWS`
nor `AUDIT_SKIPPED` (each skip carries its reason): add a new view to the list when you add its action.

### Slow and refused reads (`views --slow MS`, `views --refuse`; local #313)

Both modes walk the same view list, but switch the knob on just before the view is chosen (the session list is already up) and off again
after, and report one row per view: `{ view, opened, drawnWithin300, firstDrawMs, toast, errorLine, reads }`. `drawnWithin300` is true
when anything visible changed within 300 ms of the click (a dialog, a "Loading…" line, a toast, a pane); `firstDrawMs` is null when
nothing came before `3 x MS + 1500` ms (`--slow`) or 2 s (`--refuse`); `toast` and `errorLine` are the error text the page showed
once settled; `reads` are the daemon methods the view made. With screenshots on, `view-…-300ms.png` is the page at 300 ms. `--refuse`
answers every read with the backend's own format, `daemon error -32000: audit: this read is refused [audit_refused]`.

Each run ends with a summary line and `views.summary` in the report: `{ probed, blank, silentRefusals }`. `blank` lists the views that
drew nothing within 300 ms (the target is none, at any `--slow` value); `silentRefusals` (`--refuse` only) lists views that made a read
of their own, had it refused and showed no error text. The row also has `retry`: a visible Retry button in the open dialog. After
local #310 both lists are empty and every view that must read first goes through one frame, `src/view-load.ts` (below).

### One frame for a view that reads first (`loadView`, `src/view-load.ts`; local #310)

A view whose content needs rsid's answer calls `loadView({ key, title, read, open, failed })` instead of awaiting its reads and building
its dialog afterwards. The frame is up in the same turn as the choice with "Loading…" (`aria-busy`) in the place the content will fill;
`read` returns what the view needs (independent reads run together with `Promise.all`; it must not change shared state); `open(data)`
builds the real view once the frame has closed. A failed read replaces "Loading…" in the same box with `failed: <the daemon's message>`
(plain words plus Details, `role=alert`) and a focused Retry (`r`); Close is `q` or Esc. A late answer after the frame was closed, or after
another view was chosen, draws nothing; choosing the same `key` again while it loads joins that load. A new view that reads before it
draws must use `loadView`; `tests/audit-views.test.mjs` walks the former blank views slow and refused at 760x480, and
`tests/view-load.test.mjs` and the `view-load` smoke step cover the pattern itself.

In your own flow the same knobs are on `open()` and live on `a.reads`, so a flow can slow or refuse one read for a moment:

```js
const a = await lib.open({ reads: { delayMs: { ListMcpServers: 3000 } } });  // or a number for every read
a.reads.delayMs = 3000;                                    // a number, or { ListMcpServers: 3000, '*': 0 }
a.reads.refuse.ListMcpServers = 'mcp is busy';             // -> "daemon error -32000: mcp is busy"
a.reads.refuse.GetColdArchiveStatus = { message: 'row changed', code: 'stale_version', nextAction: 'reload' };
a.reads.refuse = { '*': 'rsid is draining' };              // every read; `a.reads.refuse = {}` ends it
a.reads.calls;                                             // [{ method, at, delayedMs, refused }]: what the page asked, and when
await lib.probeViewOpen(a, { title: 'MCP servers…' });     // from audit.mjs: { drawnWithin300, firstDrawMs, toast, errorLine, reads }
```

A delayed read is forwarded after the delay; a refused one is never forwarded (and is delayed too). Writes keep their own knob,
`a.bridge.failWrites`; `setDown` takes rsid away entirely. Do not wrap `open()`'s bridge yourself: a copy of it that got the
refusal match wrong produced false findings in audit 3.

### Fixtures that wait or fail

A fake-daemon fixture (`{ RpcMethod: (params) => result }`) may return a Promise (the answer comes late), may return
`{ error: { code, message, data? } }` to answer a JSON-RPC error (`data.code` shows as `[code]` in the page, as the real backend does),
and may throw: the fake answers `-32603` with the message and keeps serving. The shim's `render_markdown` renders paragraphs, lists,
fenced code, `code` and **bold** (escaped first), so a screenshot of a transcript shows rendered text; it is not the real
`markdown.rs`, which a table or a link would still tell apart.

## Writing a flow (mouse-first walks)

A flow is a module with a default `async ({ lib, out })`. `scripts/audit-flow-example.mjs` is the template: it sends a
message with the write stubbed, then again while rsid is "down".

```js
const a = await lib.open({
  width: 1024, height: 640,              // window size (CSS px); scale: 2 for a retina display
  scheme: 'light',                       // the emulated system prefers-color-scheme: 'light' | 'dark' | null
  theme: 'light',                        // the app's saved theme key (or { theme, overrides }); null = desktop default
  vim: false,                            // Vim keys (default off, the new-install default)
  fixtures: lib.scaleFixtures({ sessions: 800, events: 5000 }).fixtures,   // or your own { RpcMethod: (params) => result }
  stubs: { send_message: { queued: true }, archive_session: (args) => ({}) },  // answers for write commands; unlisted: null
});
await lib.clickText(a.page, 'New session');          // real mouse events, by visible text or aria-label
await lib.clickSel(a.page, '#message');
a.writes();                                          // [{ cmd, args, outcome: 'stubbed' | 'failed' }]
a.bridge.failWrites('send_message', 'rsid said no'); // make a write fail on demand ('name', ['a','b'] or true); clearFailures()
a.bridge.setDown(true);                              // rsid unreachable, as the real backend reports it: daemon_info answers { connected: false, error }
                                                     // (the daemon-down card: "rsid is not running", Start rsid, Retry now); every other command fails
a.bridge.failBackend();                              // the desktop backend itself not answering: daemon_info throws too (the backend-error card)
await a.resize(760, 480);                            // resize without a reload
await lib.contrastAudit(a.page);                     // { checked, failed, failures: [{ text, selector, fg, bg, ratio, need }] }
await a.page.eval(lib.cutOffControls);               // controls outside the viewport or clipped by an overflow ancestor
await a.page.shot(`${out}/x.png`);
await a.close();                                     // always
```

Other helpers in `audit-lib.mjs`: `overflowingText`, `docScroll`, `visibleButtons`, `observeLongTasks` and `drainLongTasks`,
`openSessionByTitle` (waits for the row: the list paints in steps, local #314), `openPaletteAction(page, title)` (open a palette action the way a person does), `waitForRowCount` (wait for
a debounced list to apply and settle), `mouse`. A Tauri command that the read-only bridge does not know is a write and is stubbed
(answered `null`); a read the fake daemon has no fixture for answers `Method not found`, so give it one in `fixtures`. A
`list_*` or `get_*` command the app calls on a view's open must be in `READ_COMMANDS` (or `LOCAL_COMMANDS`) with a realistic
fixture in `smoke-fake.mjs`, or the view fails under audit with "Cannot read properties of null" and hides the real view:
`tests/audit-tooling.test.mjs` scans `desktop/src` and fails on one that is neither (`GET_LIST_WRITES` in
`smoke-stub-bridge.mjs` names a `list_*`/`get_*` command that really writes, none today).

### Overlay key sweep (`keys`; local #327, #336)

`scripts/audit-keysweep.mjs` checks keymap parity in the browser. For each table in `SWEEP_OVERLAYS` it opens the view that table
documents (fake daemon, Vim keys on, writes stubbed), presses Ctrl-Alt-G and records whether the app's own help lists that
overlay's keys (`helpLists`), then presses **every key the table lists**, each from a fresh open, and compares a snapshot of the page
(open dialogs, visible text with ages and clocks masked, focus and `aria-activedescendant`, selected / pressed / checked rows, input
values, widths of the panels in view, scroll, toast, row count, stubbed writes) before and after. A key that changes nothing is a
`FAIL`. The key tables are `OVERLAY_TABLES` of the built app (`dist/overlay-help.generated.js`, the table the `?` help prints):
nothing outside `desktop/` is read.

```bash
npm run audit -- keys                                         # every swept table (about 70 minutes for 63; run it as a job)
npm run audit -- keys --overlay "Trash,Notifications"         # a subset, so one call fits a 10-minute limit
```

**Every table of the app is swept or skipped with a reason.** `SWEEP_OVERLAYS` (63 tables: the dialogs, the panes that are not
dialogs, the forms, the typed-filter overlays, and the modes of a view such as `Graph Review / Node Detail`) and `SKIPPED_OVERLAYS`
(9, each with why) together name all 72 tables; `coverage()` and `tests/audit-keysweep.test.mjs` fail when a table is in neither or
both, so a table the TUI adds later has to be classified.

Rows are appended to `keys.jsonl` as each key finishes (`{ overlay, spec, label, action, status, changed }`; `status` is `changed`,
`no-change`, `expected-no-op`, `typed` or `error`), plus one `summary` row per overlay; `report.json` holds the whole run. The exit
code is 1 when any key failed. Cells that are not one key press (typed text, descriptions, the global `Space N` leader) are skipped,
with the reason in the result.

How one overlay is configured (`SWEEP_OVERLAYS['Table name']`, all optional but the way to open it):

| Field | Meaning |
| --- | --- |
| `action` / `open` | the palette action id that opens the view, or `open(a, { lib, sleepMs })` for one that needs more |
| `pane`, `sel` | the view is not a dialog: `sel` is the CSS selector of its root, read for the snapshot and for "still on screen" |
| `enter` | keys (`{ key, ctrl?, shift? }` or `{ text }`) pressed after the view opens, to reach a mode of it (`o` + Enter opens a saved workflow) |
| `type`, `seed`, `noSession` | text typed once open, a function run in the page before the view opens, no session opened first |
| `modes` | `[{ specs, text?, enter? }]`: the listed keys (a cell as the table writes it, or one key's name) are pressed from an open that first typed `text` and pressed `enter`; for a key that only acts in a mode (Backspace with a filter typed, `h` after `l`, `[` in the Inspect section) |
| `reloads` | keys whose daemon read counts as their effect (`r`); views poll on their own, so a read alone hides a dead key |
| `remap` | a key the desktop binds differently from the table, pressed as the desktop's key (`Ctrl-A` is `Ctrl-Alt-A`; plain Ctrl-A stays select-all) |
| `noop` | a key that cannot change the fake page, with the reason (`expected-no-op`); a listed key that does change something is just a change |

A key that moves back or to the top (`g`, `Home`, `Up`, `k`, `Ctrl-K`, `Ctrl-P`, `Ctrl-U`, `PageUp`) is pressed after a step down off
the first row. A plain character pressed while a text box has the focus that changes the box's value is typed text: that is reported `typed` (not verified as
that key's action), not `changed`. The caret counts as an effect only for a plain character (a Vim motion): a named key or a chord moves it natively. The Vim editors (the large editor, the launch and continue prompts, the provider form) open in insert mode, so
their normal-mode keys are pressed after Esc, `0` and `l` (`NORMAL_MODE`). `known: { key: '#N' }` names a key whose gap is filed as Issue N:
it is reported `known-gap`, not a FAIL, and the sweep says `FIXED` once the key works so the entry can go. Tests: `tests/audit-keysweep.test.mjs` checks each config against the built app (table, action,
selector, and that every `noop`, `remap` and `modes` key is one the table lists) and runs Trash, Notifications and a pane in the browser.

The keyboard-list walk (`audit-listbox-walk.mjs`) and the sweep share `scripts/audit-fixtures.mjs`: `keyFixtures` gives Trash, Scheduled
jobs, labels, Manager scope and decisions, Settlement cohorts and runs, Satellite and Remote lists and Recent completions two or three
rows each, three saved workflows (one document: a plan that branches into two nodes, one a topology node, so a fold key has something to
fold), a 160-line file for the viewer, and `dagFixtures()`, the **DAG browser fixture**: every `recursive_dag_*` capability, two graphs (the
newer one hydrated: five tasks, two scheduler runs, a cancellation request, a deferred graph, a stale heartbeat, two live attempts with
validations, three artifacts), so each of its nine panels has rows and each control is enabled. Its reads are in `READ_COMMANDS`
(`smoke-bridge.mjs`: `graph_list_recursive` and the twelve `dag_*` reads; the controls stay writes). `KEY_LOCAL` gives the model
switcher an effort ladder, and `seedNotices` records Notifications in the page (they are not a daemon read). Add a fixture there, not to
`smoke-fake.mjs`, when a list needs rows only for key testing.

## What is in the gate and what is on demand

| Check | Where | Why |
| --- | --- | --- |
| Text contrast of the **default theme** (session list and New session dialog, WCAG AA) | smoke step `text-contrast`, group `core` (about 0.5 s; the group was 12 s of its 90 s budget) | One page evaluation, deterministic, and the default theme passes today, so a regression is a real one. |
| The tooling's own unit tests (stub bridge: no write reaches a socket; the walker's maths and its DOM run; the fixtures; the open() knobs; one headless boot and the example flow) | `tests/audit-tooling.test.mjs`, `npm test` | The toolkit must stay trustworthy for the next audit. |
| Window sizes, cut-off controls, overflow | on demand: `npm run audit -- sizes` | Needs one browser per size (about 45 s for four) and 760x480 already fails (#273): a gate would be red or hide it. |
| Light, High Contrast, Transparent and Truly Transparent themes (session list and New session dialog), the native window theme call, the New session summary glyphs | smoke step `theme-contrast`, group `settings` (about 5 s); `tests/contrast.test.mjs` checks every token pair of every built-in theme | #274 and #283 fixed the known misses, so a regression is a real one. The system appearance stays on demand: `npm run audit -- themes`. |
| Every theme, in every main view | on demand: `npm run audit -- themes` (about 2 minutes a theme, 16 runs; split with `--themes`) | No known misses since #274 and #283. |
| Every key table's listed keys, in a browser | on demand: `npm run audit -- keys` (about 70 minutes; use `--overlay` for a subset); `tests/audit-keysweep.test.mjs` runs Trash, Notifications and a pane in `npm test` | Finds a documented key the desktop ignores, in the real page with its fake daemon. |
| Every main view at one size and theme | on demand: `npm run audit -- views --size WxH --theme T` | About two minutes; the answer a person reads, not a pass or fail. |
| Scale: 800 to 3000 sessions, 5000 to 20000 events | on demand: `npm run audit -- scale` | Timings on a shared CI host are noise; the report is for a person. |
| Mouse-first task flows | on demand: `npm run audit -- flow FILE` | They are exploratory; promote one into a smoke step when it becomes an acceptance check. |

Promote a check into the gate only when it is cheap (a few hundred milliseconds in a browser that is already open), passes
today and cannot flake on a loaded host.

## Limits

- The contrast walker judges text against the colour under it, not image or gradient pixels, and skips disabled controls,
  `inert`, hidden and `aria-hidden` subtrees (WCAG 1.4.3 exempts inactive components). It reads computed styles in Chrome;
  WebKit may differ slightly in how it serialises `color-mix()`, which `parseColor` handles for `color(srgb …)` only.
- Headless Chrome is not macOS WebKit. Tab-order and native-control findings still need `npm run smoke:webkit`.
- The fake daemon serves canned reads. A flow that needs another read adds a fixture; it never goes to a live rsid.
