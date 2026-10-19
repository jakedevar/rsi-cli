# RSI Desktop: macOS acceptance evidence (hub #3016, #3000 criterion 6)

Issue #142. This is the macOS half; the hub verifies the Linux `make desktop-install`.
Everything marked `[observed]` was run for this report on one Mac; `[source]` cites a file;
`[inferred]` is reasoning, not a result.

| | |
| --- | --- |
| Host | macOS 27.0.1 (26A434), arm64, 12 cores; node v24.22.0, npm 11.19.1, rustc 1.94.1 (`rust-toolchain.toml`) |
| Tree | the `origin/rolling` tip `d8d2cae1f` plus this change's desktop-only commits (the install ran on the `rolling` sources; this change touches only `desktop/scripts`, `desktop/tests`, `desktop/docs` and the README, none of which is in the binary) |
| Not signed or notarized | the operator's step; see README "Platforms" |
| Not touched | the operator's `~/Applications` (still only `Claude Code URL Handler.app`), `~/.local/bin/rsi-desktop` (still its Oct 17 symlink to `~/.rsi/install/rsi-desktop`), `rsid` (never restarted; the live-daemon launch only sends read requests) |

## 1. Fresh unsigned install into temporary prefixes

```bash
P=$(mktemp -d /tmp/rsi-142-install.XXXXXX); mkdir -p "$P"/{apps,bin,install,xdg}
RSI_DESKTOP_APP_DIR=$P/apps RSI_INSTALL_BIN_DIR=$P/bin RSI_DESKTOP_INSTALL_DIR=$P/install \
  XDG_DATA_HOME=$P/xdg CARGO_BUILD_JOBS=6 make desktop-install
```

[observed] `exit=0 seconds=195` (this sandbox's `target/` was cold: a full release build, `Finished release profile in 3m 13s`; #117 measured 90 s on a warm target). Steps the script ran (`scripts/install-desktop.sh --required`):
`npm ci`, `npm run build` (`slices: generated files are current`, then `tsc`), `cargo build --release`, then the install. Its last lines:

```text
Desktop UI installed:
  /tmp/rsi-142-install.AltQlh/bin/rsi-desktop -> /tmp/rsi-142-install.AltQlh/install/rsi-desktop
  /tmp/rsi-142-install.AltQlh/apps/RSI Desktop.app
```

The log also has `warning: sources in <sandbox> changed while the release build ran; --link-only will not link this build (#938)`: I edited reachability scripts while it built. It only disables `--link-only`; the install above completed.

What is on disk [observed]:

- `plutil -lint "$P/apps/RSI Desktop.app/Contents/Info.plist"` -> `OK`
- `file …/Contents/MacOS/rsi-desktop` -> `Mach-O 64-bit executable arm64`; the bundle is 13 MB
- `codesign -dv` -> `flags=0x20002(adhoc,linker-signed)`, `Signature=adhoc` (the linker's ad-hoc signature, no identity: unsigned)
- `$P/bin/rsi-desktop` is a symlink to `$P/install/rsi-desktop`; `XDG_DATA_HOME` received nothing on macOS (the `.desktop` entry and icons are Linux-only)

## 2. Launches of the installed binary: boot completed, not just a window

Both launches ran `"$P/apps/RSI Desktop.app/Contents/MacOS/rsi-desktop"` directly so stderr is captured, waited, and
stopped it by its own PID (`kill <pid>`; `ps` confirmed it gone). The log is the process's stderr
(the `sandbox_extension_issue_file_to_process failed …` line is the agent sandbox refusing a file-handle grant, not the app).

**(a) No daemon: a temporary socket that does not exist** [observed, 12 s alive before stop]
`RSI_DAEMON_SOCKET=$P/none.sock`

```text
rsi-desktop: started version=0.1.0 socket=/tmp/rsi-142-install.AltQlh/none.sock
rsi-desktop: boot=offline daemon=unreachable sessions=0 steps=daemon:failed,projects:skipped,labels:skipped,sessions:skipped,settings:skipped,live:ok error="daemon: cannot reach rsid: /tmp/rsi-142-install.AltQlh/none.sock: No such file or directory (os error 2)"
```

`offline` is a good boot, not a failed one: the page shows the "Cannot reach the rsid daemon" guidance (README "What you see on first run"); dependent steps are skipped, not failed.

**(b) The live daemon, read-only** [observed, 15 s alive before stop; default socket, `RSI_DAEMON_SOCKET` unset]

```text
rsi-desktop: started version=0.1.0 socket=/Users/operator/.rsi/daemon.sock
rsi-desktop: boot=ready daemon=connected sessions=11 steps=daemon:ok,projects:ok,labels:ok,sessions:ok,settings:ok,live:ok
```

Every boot step is `ok`, including `settings`, the step whose failure was invisible in the earlier candidate. The boot only reads (health, projects, labels, sessions, `GetDaemonConfig`); no write RPC is sent.

**The page's own `html data-boot`, from the headless harness** (no macOS GUI permission needed; Google Chrome 155 headless against the same `dist/` the binary embeds) [observed]:

| Command | Result | `data-boot` evidence |
| --- | --- | --- |
| `node scripts/smoke.mjs --out /tmp/142-proof/smoke-fake` (fake rsid, 48 s) | exit 0, 16 PASS | `PASS boot booted, session list present, data-boot=ready` |
| `node scripts/smoke.mjs --live --out /tmp/142-proof/smoke-live` (live rsid, read-only allowlist, 48 s) | exit 0, 16 PASS (session list 7 rows, 64 Issues, every dialog opened) | `PASS boot booted, session list present, data-boot=ready` |
| `node scripts/smoke.mjs --first-run --out /tmp/142-proof/firstrun` (3.7 s) | exit 0, 5 scenarios PASS, 35 checks `ok` | daemon missing: `data-boot is offline (got offline)`; up: `ready`; Settings read fails: `data-boot is failed (got failed)` with the visible banner; rsid starts later: `data-boot becomes ready after Retry now (got ready)` |

Screenshots: `/tmp/142-proof/firstrun/*.png`, `/tmp/142-proof/smoke-live/*.png` (not committed; the commands regenerate them).

## 3. Mouse and menu reachability, one row per PARITY.md section

Two commands, both [observed] exit 0 on this tree (`npm run build` first):

```bash
cd desktop && node scripts/reachability.mjs                 # every registry action: 262 actions, 262 reachable, 0 problems
cd desktop && node scripts/reachability.mjs --by-section   # new: one row per PARITY.md `##` section: 10 PARITY sections, 514 rows, 262 actions (262 reachable), 0 problems
```

The per-action output is committed as `desktop/docs/acceptance-macos-reachability.tsv` (id, menu group, surfaces).
By group: 4 File, 38 Go, 2 Help, 41 Issues, 51 Session, 126 View. By surface: 135 menu+palette, 64 palette only (menu-less by design, reached through the Actions… button), 24 menu+session-row context menu+palette, 15 menu+issue-row context menu+palette, 15 menu+toolbar button+palette, 5 menu+context+button+palette, 2 context-only message actions (`copy-event`, `toggle-event`), and 2 more that are in the message context menu as well as the menus.

`--by-section` (`scripts/reachability-lib.mjs` `bySection`; tested in `tests/reachability.test.mjs`) links each PARITY row to registry actions three ways, strongest first:
**binding** (the row's TUI key or `:` command, or a key or command in an overlay row's title, is bound to the action in `KEY_BINDINGS` / `COMMAND_BINDINGS`);
**module** (the row's desktop location names a slice module, `src/actions/<x>.ts`, `<x>-run.ts` or `<x>-spec.ts`, so the slice's actions reach it; weaker, a neighbour rather than the exact action);
**none** (no action link). It fails if any linked action lacks a mouse/menu surface, and requires one table row per `##` heading. Surfaces it counts: the native menu bar (mac and other layouts), a right-click menu with a wired `contextmenu` listener, a toolbar `data-action` button, the command palette (opened by the Actions… button, View ▸ Command palette… and a clickable list).

Script output, verbatim:

```text
| PARITY section | rows | via binding | via module | no action | mouse/menu entry points (binding-linked actions) |
| Global Keybindings | 55 | 9 | 42 | 4 | 3 actions (menus go/help, palette); e.g. Go ▸ Open session under cursor; Help ▸ Keyboard shortcuts; Go ▸ Back or up a level; +54 via modules (menus go/session/view, button, context:session-row, palette) |
| Normal Mode Keybindings | 185 | 130 | 31 | 24 | 102 actions (menus file/go/help/issues/session/view, button, context:event, context:session-row, palette); e.g. Help ▸ Keyboard shortcuts; Go ▸ Next session; Go ▸ Previous session; +39 via modules (menus go/issues/session/view, button, context:issue-row, context:session-row, palette) |
| Command Mode | 77 | 73 | 3 | 1 | 55 actions (menus file/go/help/issues/session/view, button, context:issue-row, context:session-row, palette); e.g. Help ▸ Keyboard shortcuts; View ▸ Command palette…; Help ▸ Open manual; +23 via modules (menus go/session/view, context:session-row, palette) |
| Overlay Keybindings | 29 | 12 | 16 | 1 | 11 actions (menus view, button, palette); e.g. View ▸ Manager board; View ▸ Edit project card…; View ▸ Sort sessions…; +124 via modules (menus go/session/view, button, context:session-row, palette) |
| Overlay Key Reference (generated) | 71 | 0 | 61 | 10 | none bound directly; +158 via modules (menus go/issues/session/view, button, context:issue-row, context:session-row, palette) |
| Input Bar Keybindings (Session Detail View) | 5 | 0 | 5 | 0 | none bound directly; +28 via modules (menus go/session/view, context:session-row, palette) |
| Vim Text Editing (Input Bar & Overlay) | 8 | 0 | 7 | 1 | none bound directly; +15 via modules (menus go/session/view, palette) |
| Session Detail and Recent Files | 2 | 0 | 2 | 0 | none bound directly; +55 via modules (menus go/session/view, button, context:session-row, palette) |
| Notes | 6 | 0 | 4 | 2 | none bound directly; +28 via modules (menus go/session/view, context:session-row, palette) |
| TUI view modules | 76 | 0 | 57 | 19 | none bound directly; +198 via modules (menus go/issues/session/view, button, context:issue-row, context:session-row, palette) |
```

The report table, one row per section, with the entry points named. "Resolved by hand" means the script left the row at `none` and I
named its entry point from the registry (each action id is in the committed TSV with its surfaces):

| PARITY section (rows) | Mouse / menu entry points | Evidence and the rows not linked mechanically |
| --- | --- | --- |
| Global Keybindings (55) | Help ▸ Keyboard shortcuts (`help`); View ▸ Command palette… and the top bar's **Actions…** button (`palette`); Go ▸ Open session under cursor (`open`), Go ▸ Filter sessions (`focus-search`), Go ▸ Back or up a level; View ▸ Command line (`command-line`); File ▸ New session… (`launch`, button) | 9 bound, 42 module-linked. 4 `none`: `global:ctrl-c/any-context`, `global:esc/command-line`, `global:char/command-line`, `global:char/search-line` are keystrokes typed inside the command line or search field, which a mouse opens first (View ▸ Command line, Go ▸ Filter sessions). They are not actions. |
| Normal Mode Keybindings (185) | The whole menu bar: File, Go, Session, Issues, View, Help; the session-row and message right-click menus; toolbar buttons; the palette. e.g. Go ▸ Next session / Previous session, Session ▸ Archive session…, Issues ▸ Issues workspace | 130 bound (of 151 key rows, 129 resolve by key), 31 module-linked. 24 `none`: 18 `key:` rows are retired TUI chords kept inert (`gt g? gX gq gn gc gv gp gK gG gE gS gT gB`, `<Space>gr o R S`; every one has the PARITY.md note "the engine answers with the TUI's reason and never falls through"); 3 are prefixes (`D` of DD, `y` of yy, `<Space>` the leader with its which-key hint), so none has a mouse entry by design; `section:theme-selection` -> View ▸ Choose theme… (`theme-picker`); `section:quit` -> File ▸ Quit RSI Desktop (`quit`); `section:all-normal-mode-keys-generated` is a heading whose note says every row is tracked as its own `key:` row. Resolved by hand. |
| Command Mode (77) | View ▸ Command line, then any `:` command; and every command's own menu item, button or context item: 73 commands are bound to actions listed in all six menus | 73 bound. 1 `none`: `cmd:pause-clear` (`:pause clear`) -> Session ▸ Clear pause (`clear-pause`: menu, session-row context menu, button, palette). Resolved by hand. |
| Overlay Keybindings (29) | View ▸ Manager board (`mgr-board`), Edit project card…, Sort sessions…, Settings…, Terminal, Graph review, Recursive DAG browser, ESP Square, Manager budget panel; Session ▸ Switch model or effort… | 12 bound, 16 module-linked, 1 `none`: `overlay:theme-picker-overlay` -> View ▸ Choose theme… (`theme-picker`, menu + palette). Resolved by hand. |
| Overlay Key Reference, generated (71) | The same View-menu and palette openers: View ▸ Settings…, Choose theme…, Edit theme roles…, Terminal, Graph review (`graph-open`), Recursive DAG browser (`dag-open`), ESP Square (`esp-open`), Manager budget panel (`mgr-budget`); File ▸ New session… (button); Session ▸ Switch model or effort…; the palette (Actions… button) | 0 bound, 61 module-linked. 10 `none`: `theme-role-editor`, `theme-picker` (View menu, above), `command-palette` (Actions… button, View ▸ Command palette…), `model-picker` (Session ▸ Switch model or effort…; the launch default is palette-only, `model-dropdown`), `recursive-dag-browser` (`dag-open`), `budget-policy-form` (`mgr-budget`), `new-session-prompt` and `new-session-settings` (File ▸ New session…), plus two catalog rows (`screens-without-an-overlay-catalog`, `surfaces-documented-by-hand`) that are documentation. Resolved by hand. **The keys inside an overlay are not audited per key here**; they are the overlay's own DOM and are covered by `smoke.mjs` (dialog-vim-gate, settings-button-keys, hooks/providers/bridges/terminal steps, all PASS in section 2) and the dialog tests. |
| Input Bar Keybindings (5) | Click the message box; Session ▸ Message this session (`focus-composer`, also in the session-row and message context menus); Session ▸ Send "continue"; Session ▸ Continue with a message | 5 module-linked (`src/actions/input.ts`), 0 none. Sending is the Send button as well as Ctrl+Enter ([source] README "Session detail"). |
| Vim Text Editing, Input Bar and Overlay (8) | Modal editing is typed inside the message box or a dialog field, reached by mouse; View ▸ Vim keys (`toggle-vim-keys`) turns the modal keys off for a plain mouse-and-shortcut app | 7 module-linked. 1 `none`: `section:count-modifiers`, an editing grammar (counts before motions), not an action. Vim editing behaviour is tested in `tests/vim-parity.test.mjs` and the headless `tests/vim-fields.test.mjs` (real key events in Chrome). |
| Session Detail and Recent Files (2) | Click a session row, Go ▸ Open session under cursor; View ▸ File explorer (`files-explorer`), Find file… and the other `files-*` items | 2 module-linked, 0 none. |
| Notes (6) | none needed: prose rows (operator messages, key-sequence timing, restart) | 4 module-linked. 2 `none`: `section:operator-messages-and-restart` (the operator-message dialogs, reached from Session ▸ Message this session) and `section:key-sequence-timing` (a keyboard-engine rule, not an action). |
| TUI view modules (76) | Render modules (transcript, glyphs, layout, highlight) have no action; the overlay modules are opened by the View menu items above | 57 module-linked. 19 `none`: render and layout modules (`ui/content.rs`, `glyphs.rs`, `height.rs`, `highlight.rs`, `navigator_layout.rs`, `graph_layout.rs`) and overlay modules whose opener is named in the overlay rows above (`budget_policy_form.rs`, `command_palette.rs`, `esp_square.rs`, `keybindings_help.rs` -> Help ▸ Keyboard shortcuts, `recursive_dag.rs`, `theme_picker.rs`, `text_area_bg_editor.rs`, …). |

Limits, stated plainly:

- Reachable means a mouse or menu surface lists the action and its DOM wiring exists; it does not say the item is enabled in every state (a disabled item says why; those rules are in `tests/registry.test.mjs`).
- No native menu item was clicked: macOS refuses GUI automation to this agent (#108). The menu model is the one the Rust side builds (`src-tauri/src/menu.rs`) [source]. The unit and smoke runs exercise the same actions through the DOM.
- The "via module" column ties a row to its slice's actions, not to an exact action; only "via binding" and the hand-resolved rows name an exact action.
- 62 of the 514 rows are `none` (4+24+1+1+10+0+1+0+2+19). Each is named or classed in the table: text-entry keystrokes, retired inert chords, prefixes, headings, render modules, or an overlay whose opener is named. None is an action without a mouse entry. The `none` rows I resolved by hand are resolved from the registry and PARITY.md notes, not by the script.

## 4. Newcomer steps checked against `desktop/README.md`

Each README step was matched to the source or to a run above.

| README step | Check | Result |
| --- | --- | --- |
| Install prerequisites (node, npm, pinned Rust, Xcode command line tools on macOS) | the install ran with exactly these | matches |
| `make desktop-install` -> `~/.rsi/install/rsi-desktop`, `~/.local/bin/rsi-desktop`, `~/Applications/RSI Desktop.app` | section 1: the temp prefixes received the binary, symlink and `.app`; the operator's paths untouched | matches (all three env overrides honoured) |
| Throwaway-install snippet, then `RSI_DAEMON_SOCKET=$P/none.sock "$P/apps/RSI Desktop.app/Contents/MacOS/rsi-desktop"` | section 2(a) used this command | matches; prints `boot=offline` |
| "Run and connect to rsid": default socket `~/.rsi/daemon.sock`, `RSI_DAEMON_SOCKET`, top bar link state | section 2(b) | matches |
| "What you see on first run" table (daemon missing / no sessions / sessions / backend error) | `smoke.mjs --first-run`: 5 scenarios, 35 checks PASS; the strings are in `src/onboarding-model.ts` (`Cannot reach the rsid daemon`, `Retry now`, `Connected to rsid. No sessions yet.`) | matches |
| "Did it boot?": `data-boot`, the stderr `started` and `boot=` lines | section 2 | matches |
| Run it from a terminal: `"/Applications/RSI Desktop.app/Contents/MacOS/rsi-desktop"` | the installer writes `~/Applications` (README line 58), not `/Applications` | **defect, fixed**: now `"$HOME/Applications/RSI Desktop.app/Contents/MacOS/rsi-desktop"` |
| "the menu bar (File, Session, Go, View, Help; on macOS also the app, Edit and Window menus)" | `appMenuModel`: mac `app file edit session go issues view window help`, other `file session go issues view help` | **defect, fixed**: the Issues menu (41 actions) was missing from the list |
| Palette: Ctrl-K (Cmd-K on macOS) and the Actions… button | `keyHints('palette')` = `Ctrl-K`, `displayHint(..., 'mac')` = `Cmd-K`; `reachability.mjs` requires the button, the View item and the click handler | matches |
| New session: Cmd-N / Ctrl-N | `keyHints('launch')` = `Ctrl-N`, shown `Cmd-N` on macOS | matches |
| Right-click on a session row or a message; toolbar buttons with tooltips | `contextMenuModel` kinds `session-row`, `event`, `issue-row`, each with a wired listener (reachability check) | matches (the Issues row right-click menu is also there) |
| `?` or Ctrl-Alt-G lists the Vim keys | `keyHints('help')` = `? Ctrl-Alt-G :help` | matches |
| Settings: View › Settings…, `<Space>,`, `:set`; the Vim keys switch at View › Vim keys | the View menu has `Settings…` and `Vim keys` items; smoke "settings" step PASS | matches |
| `node scripts/smoke.mjs`, `--live`, `--first-run`, `reachability.mjs` as documented | all run above | matches; `--by-section` documented in "First-run and reachability checks" |
| `cd desktop && npm ci && npm test` | [observed] exit 0, 834 tests, 833 pass, 0 fail, 1 skipped (the opt-in real-rsid e2e: `runner_unavailable` without `RSI_E2E_RSID`), 56 s; log `/tmp/142-npmtest.log` | matches, with one caution: see "Defects" |

## 5. Defects found

Fixed in this change: the README's menu list and its macOS terminal-launch path (section 4); `scripts/reachability.mjs` gained `--by-section` (section 3) with two tests.

Filed:

- #147 (kaizen): in `npm test` the two headless-Chrome tests (`tests/smoke.test.mjs` fake smoke, `tests/vim-fields.test.mjs`) hung to their timeouts on two of three full runs on this busy Mac (831 other tests passed; the third run passed everything in 56 s; the same tests pass alone). The cause is not found; it is likely Chrome start-up or CDP waits under contention [inferred].

Not defects, recorded so nobody re-files them: 64 actions are palette-only (no menu entry) by design (`noMenu`); the `sandbox_extension_issue_file_to_process` stderr line is the agent sandbox, not the app.

## 6. Repeat it

```bash
cd desktop && npm ci && npm run build
node scripts/reachability.mjs && node scripts/reachability.mjs --by-section
node scripts/smoke.mjs --first-run --out /tmp/desktop-firstrun
node scripts/smoke.mjs --out /tmp/desktop-smoke        # fake rsid; add --live for the running rsid, read-only
npm test
```

## Re-run at the exact acceptance head c520af8230cde803cdd7deea59a3107a0bb0516a (2026-10-19, hub #3253 gap B)

Hub #3253 asked for the same evidence pack at exactly `c520af8230cde803cdd7deea59a3107a0bb0516a` (it adds Settings and
operator-config runtime changes after `d8d2cae1f`). Run by the Mac PM seat 241d3b87 in a detached worktree at that
SHA, with no local changes, on the Mac Mini (arm64). The operator's `~/Applications` (still only `Claude Code URL
Handler.app`) and `~/.local/bin/rsi-desktop` (still the symlink to `~/.rsi/install/rsi-desktop`) were not touched,
and rsid was not restarted. [observed]

### Install (fresh, unsigned, temporary prefixes)

```sh
P=$(mktemp -d /tmp/rsi-3253-install.XXXXXX); mkdir -p "$P"/{apps,bin,install,xdg}
RSI_DESKTOP_APP_DIR=$P/apps RSI_INSTALL_BIN_DIR=$P/bin RSI_DESKTOP_INSTALL_DIR=$P/install \
  XDG_DATA_HOME=$P/xdg CARGO_BUILD_JOBS=6 make desktop-install
```

`exit=0 seconds=188` (`Finished release profile in 3m 04s`). It installed
`/tmp/rsi-3253-install.iM9WqG/bin/rsi-desktop -> /tmp/rsi-3253-install.iM9WqG/install/rsi-desktop` and `/tmp/rsi-3253-install.iM9WqG/apps/RSI Desktop.app`. The binary is
`Mach-O 64-bit executable arm64`, `Signature=adhoc` (unsigned for distribution; signing stays the operator's step).

### Launches: boot completed

`RSI_DAEMON_SOCKET=$P/none.sock "$P/apps/RSI Desktop.app/Contents/MacOS/rsi-desktop"` (12 s, stopped by its PID):

```text
rsi-desktop: started version=0.1.0 socket=/tmp/rsi-3253-install.iM9WqG/none.sock
rsi-desktop: boot=offline daemon=unreachable sessions=0 steps=daemon:failed,projects:skipped,labels:skipped,sessions:skipped,settings:skipped,live:ok error="daemon: cannot reach rsid: /tmp/rsi-3253-install.iM9WqG/none.sock: No such file or directory (os error 2)"
```

`"$P/apps/RSI Desktop.app/Contents/MacOS/rsi-desktop"` against the live daemon, read-only (15 s, stopped by its PID):

```text
rsi-desktop: started version=0.1.0 socket=/Users/operator/.rsi/daemon.sock
rsi-desktop: boot=ready daemon=connected sessions=11 steps=daemon:ok,projects:ok,labels:ok,sessions:ok,settings:ok,live:ok
```

### Headless smokes (`html data-boot`)

| Command (in `desktop/`, after the install's `npm ci` and build) | Result |
| --- | --- |
| `node scripts/smoke.mjs --out /tmp/3253-proof/smoke-fake` | exit 0 in 48 s, 16 PASS, 0 FAIL; `PASS boot booted, session list present, data-boot=ready` |
| `node scripts/smoke.mjs --live --out /tmp/3253-proof/smoke-live` (read-only allowlist) | exit 0 in 47 s, 16 PASS, 0 FAIL; `PASS boot booted, session list present, data-boot=ready` |
| `node scripts/smoke.mjs --first-run --out /tmp/3253-proof/firstrun` | exit 0 in 3 s; 5 scenarios PASS (daemon-missing, connected-empty, connected-sessions, settings-fails, daemon-recovers), 35 checks `ok`; daemon-missing reports `data-boot is offline (got offline)` |

### Reachability at this head

`node scripts/reachability.mjs`: `262 actions, 262 reachable, 0 problems`.
`node scripts/reachability.mjs --by-section`: `10 PARITY sections, 514 rows, 262 actions (262 reachable), 0 problems`:

| PARITY section | rows | via binding | via module | no action | mouse/menu entry points (binding-linked actions) |
| --- | --- | --- | --- | --- | --- |
| Global Keybindings | 55 | 9 | 42 | 4 | 3 actions (menus go/help, palette); e.g. Go ▸ Open session under cursor; Help ▸ Keyboard shortcuts; Go ▸ Back or up a level; +54 via modules (menus go/session/view, button, context:session-row, palette) |
| Normal Mode Keybindings | 185 | 130 | 31 | 24 | 102 actions (menus file/go/help/issues/session/view, button, context:event, context:session-row, palette); e.g. Help ▸ Keyboard shortcuts; Go ▸ Next session; Go ▸ Previous session; +39 via modules (menus go/issues/session/view, button, context:issue-row, context:session-row, palette) |
| Command Mode | 77 | 73 | 3 | 1 | 55 actions (menus file/go/help/issues/session/view, button, context:issue-row, context:session-row, palette); e.g. Help ▸ Keyboard shortcuts; View ▸ Command palette…; Help ▸ Open manual; +23 via modules (menus go/session/view, context:session-row, palette) |
| Overlay Keybindings | 29 | 12 | 16 | 1 | 11 actions (menus view, button, palette); e.g. View ▸ Manager board; View ▸ Edit project card…; View ▸ Sort sessions…; +124 via modules (menus go/session/view, button, context:session-row, palette) |
| Overlay Key Reference (generated) | 71 | 0 | 61 | 10 | none bound directly; +158 via modules (menus go/issues/session/view, button, context:issue-row, context:session-row, palette) |
| Input Bar Keybindings (Session Detail View) | 5 | 0 | 5 | 0 | none bound directly; +28 via modules (menus go/session/view, context:session-row, palette) |
| Vim Text Editing (Input Bar & Overlay) | 8 | 0 | 7 | 1 | none bound directly; +15 via modules (menus go/session/view, palette) |
| Session Detail and Recent Files | 2 | 0 | 2 | 0 | none bound directly; +55 via modules (menus go/session/view, button, context:session-row, palette) |
| Notes | 6 | 0 | 4 | 2 | none bound directly; +28 via modules (menus go/session/view, context:session-row, palette) |
| TUI view modules | 76 | 0 | 57 | 19 | none bound directly; +198 via modules (menus go/issues/session/view, button, context:issue-row, context:session-row, palette) |

All results match the earlier pack (sections 1-3), and no defect was found at this head.
