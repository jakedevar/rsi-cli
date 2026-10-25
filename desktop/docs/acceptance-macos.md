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
rsi-desktop: started version=0.1.0 socket=$HOME/.rsi/daemon.sock
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
cd desktop && node scripts/reachability.mjs                 # every registry action, plus overlay form completion (hub #3280): 262 actions, 262 reachable; 1/1 overlay forms finished and cancelled by mouse; 0 problems
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
| Overlay Key Reference, generated (71) | The same View-menu and palette openers: View ▸ Settings…, Choose theme…, Edit theme roles…, Terminal, Graph review (`graph-open`), Recursive DAG browser (`dag-open`), ESP Square (`esp-open`), Manager budget panel (`mgr-budget`); File ▸ New session… (button); Session ▸ Switch model or effort…; the palette (Actions… button) | 0 bound, 61 module-linked. 10 `none`: `theme-role-editor`, `theme-picker` (View menu, above), `command-palette` (Actions… button, View ▸ Command palette…), `model-picker` (Session ▸ Switch model or effort…; the launch default is palette-only, `model-dropdown`), `recursive-dag-browser` (`dag-open`), `budget-policy-form` (`mgr-budget`), `new-session-prompt` and `new-session-settings` (File ▸ New session…), plus two catalog rows (`screens-without-an-overlay-catalog`, `surfaces-documented-by-hand`) that are documentation. Resolved by hand. **The keys inside an overlay are not audited per key here**; they are the overlay's own DOM and are covered by `smoke.mjs` (dialog-vim-gate, settings-button-keys, hooks/providers/bridges/terminal steps, all PASS in section 2) and the dialog tests. **Form completion inside an overlay is audited, not excluded** (hub #3280): `FORM_COMPLETION` in `scripts/reachability-lib.mjs` lists each editor a click opens, and `node scripts/reachability.mjs` fails unless its native Save/Stage and Cancel buttons exist and the smoke steps that click them are present; see "Form completion by mouse" below. |
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
rsi-desktop: started version=0.1.0 socket=$HOME/.rsi/daemon.sock
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

## Form completion by mouse: the Settings text editor (hub #3280, local #158)

Settings ▸ Providers & Sandboxes ▸ Benchmarks ▸ Benchmark docker hosts (and every other text row) opens the shared
editor (`src/settings.ts` `editText`) when its selected value is clicked. The editor now has a native Cancel button and
a submit button labelled **Save** (a Daemon Text Setting, written at once) or **Stage** (a staged row; `w` applies).
Validation, the immediate-versus-staged write, the error line and Enter/Esc are unchanged.

| Check (in `desktop/`) | Result [observed] |
| --- | --- |
| `node scripts/reachability.mjs` | `262 actions, 262 reachable; 1/1 overlay forms finished and cancelled by mouse; 0 problems` |
| `node scripts/smoke.mjs --out /tmp/smoke-158` (fake daemon, read-only bridge) | `PASS settings-text-mouse`: the editor opened by rail, tab and value clicks; Cancel attempted no write; Save attempted `update_daemon_config` at once (refused by the read-only bridge, error shown in the editor); Stage left `1 pending` |
| `node scripts/smoke.mjs --isolated-write --rsid <rsid built with --features e2e-seed> --only settings-text-mouse` | `PASS settings-text-mouse`: by mouse only, Cancel kept `bench.docker_hosts` at `""` in rsid; an invalid host showed rsid's refusal and kept the editor open; Save wrote `ssh://e2e-mouse-host` and rsid read it back; Stage on Stall classifier model left 1 pending with no write; Discard cleared it |

## Tab reaches every control in macOS WebKit with "Keyboard navigation" off (local #242)

With System Settings › Keyboard › "Keyboard navigation" off (the macOS default) and WebKit's default preferences,
WKWebView leaves every `<button>`, checkbox, link and `<summary>` without a `tabindex` out of the Tab order. **The
shipped app does not run those defaults:** wry 0.57 sets `WKPreferences.tabFocusesLinks = YES`, so in the packaged
`rsi-desktop` Tab reaches those controls anyway (#255, #257), and the symptom above cannot happen there. The stamps are
a defense in depth that matters only if wry ever stops setting the preference. The desktop gives each of them `tabindex="0"`: `el()` in `src/dom.ts` stamps buttons and summaries as they are made, and
`installTabStops` (`src/tab-stops.ts`, called once from `src/main.ts`) stamps the static page and everything added
later. A `tabindex` set on purpose (a roving `-1`) is kept, text fields and selects are left alone, and no positive
`tabindex` exists (`tests/tab-stops.test.mjs`). The conversation (`#events`) is a tab stop named "Conversation", so the
arrows and paging keys scroll it.

How to repeat it (local #247): `npm run smoke:webkit` builds, serves the build on the smoke's fake daemon and bridge,
compiles `scripts/webkit-smoke.swift` with `swiftc` and sends real Tab and Shift-Tab keys through a WKWebView with
Keyboard navigation off for that process (`-AppleKeyboardUIMode 0`). Every step runs in two configurations (#257): `wry`,
which sets `tabFocusesLinks` on exactly as the shipped app's wry does (its control asserts a plain button IS reached), and
`default`, WebKit's own defaults with the preference off (its control asserts the plain button is skipped, so the stamps
are still proven). `WRY_PREFERENCE` in `scripts/smoke-webkit.mjs` records the wry version and the line that was read
(`wry-0.57.0/src/wkwebview/mod.rs:517`), and `tests/smoke-webkit.test.mjs` fails when `src-tauri/Cargo.lock` moves to
another wry, with the instruction to re-read wry's WKPreferences setup first. It writes `/tmp/desktop-smoke-webkit/report.json`
with each step's focus sequence. `npm test` runs the same check (`tests/smoke-webkit.test.mjs`) on macOS with `swiftc`,
and skips it with `runner_unavailable` elsewhere. It needs a window server session: run it from a logged-in terminal;
over plain ssh the harness exits 3 (`CGSessionCopyCurrentDictionary` is nil). Its window shows for the run, but the app
never activates, so keyboard focus stays where it was. It writes nothing under the real `~/Library` (#263): the harness
runs with a temp `HOME` and a non-persistent `WKWebsiteDataStore`, and the real-run test compares
`~/Library/WebKit/webkit-smoke*` and `~/Library/Caches/webkit-smoke*` before and after.

| `npm run smoke:webkit` (macOS 27, WebKit 22625.1.29.11.28, local #247) | Result [observed] |
| --- | --- |
| Control, `wry` mode (#257): a button without `tabindex` in a shadow root | Tab 1 stops on the plain button, Tab 2 on its `tabindex="0"` neighbour, as in the packaged app (#255) |
| Control, `default` mode: the same fixture, preference off | Tab skips the plain button and stops on its `tabindex="0"` neighbour; with `-AppleKeyboardUIMode 2` Tab stops on it, and the run reports `runner_unavailable` (exit 3) |
| `page-tab`, `page-shift-tab` | Tab reaches New session, Stop this turn, Archive, the conversation, the message box and Send in page order; Shift-Tab from Send walks back to Stop this turn |
| `confirm-dialog`, `question-dialog`, `launch-dialog` | Tab reaches Cancel/Archive; the answer box, Decline, Hide and Submit; Cancel/Launch; no press focuses a page element outside the dialog (the wrap past the last button briefly leaves the page: `body`) |
| `settings-tab`, `issues-tab` (Vim keys off) | Tab reaches Settings Refresh then Close, and Issues Refresh, New Issue, Close in native order |
| The same run with `stampTabStop` disabled in `dist/tab-stops.js` | `page-tab`, `page-shift-tab`, `launch-dialog` and `issues-tab` fail (Tab reaches only the conversation and the message box), so the check catches #242 |
| Whole run (#257) | two configurations, each boot, a preference read-back and 8 steps, all 20 checks pass, about 20-25 s; the first run adds a few seconds of `swiftc` (later runs reuse the binary) |

The probe that found #242, kept as the harness's history: in the table below "the probe" sent 30 Tabs after a setup that
turned Vim keys off and opened a session or a dialog.

| Check (macOS 27, `fullKeyboardAccess=false`, Vim keys off) | Result [observed] |
| --- | --- |
| Page, a session open, before this change (stamp disabled in `dist`) | Tab visits only Project, Filter, the session list, a Details toggle, the conversation and the message box, then wraps: no button |
| Page, a session open, with this change | Tab visits the Vim keys switch, Issues, Actions…, **New session**, Help, Active/Jobs/Archived, Project…/Sort…/Waiting/Info, Project, Filter, Show finished, the session list, Copy ID, Details, Hide tool calls, **Stop this turn**, **Pause until I resume**, **Archive**, the conversation, the message box and **Send**, then wraps |
| A confirm dialog (Archive) | Tab visits **Cancel** and **Archive** and stays in the dialog |
| The question dialog | Tab visits the answer box, **Decline**, **Hide** and **Submit** (Previous/Next are hidden with one question) |
| `node scripts/smoke.mjs` step `tab-stops` (headless Chrome, reads the attribute) | every visible enabled button, link, checkbox and summary has `tabindex="0"` on the page and in the launch, confirm, question, Trash and Scheduled jobs dialogs; `#events` has `tabindex="0"` and `aria-label="Conversation"` |

The probe drives WKWebView directly; the Tauri-built `rsi-desktop` window runs the same engine and the same `dist`, but
it was not launched for this check [inferred]. **Superseded by #255 below:** the packaged app was launched, and its
WebKit is not configured like this probe's. wry sets `tabFocusesLinks`, so Tab reaches buttons even without the stamp; the
smoke now runs that configuration too (#257).

## Screen-reader basics: toasts, dialog names, the conversation log, a session that needs you (local #245)

What changed: `#toast` is a live region (`role="status"`, polite; `role="alert"`, assertive, for an error) and, while a modal
dialog is open, moves into the topmost one (the page behind a modal dialog is inert, so a toast left there is neither seen nor
spoken) and returns to the page when it closes; every dialog is named from its own heading (`src/a11y.ts`; `aria-labelledby`),
confirms are `role="alertdialog"`; `#events` is a polite `role="log"` with `aria-relevant="additions"` that is busy and not live
while a session's history is rebuilt; a session that newly needs you is announced once through the toast ("<name> is waiting
for you"). Automated: `tests/screen-reader.test.mjs` and the smoke step `screen-reader` (headless Chrome: ten dialogs named,
the toast is the topmost element at its own centre above two of them, `role="alert"` for an error, `#events` is `role="log"`).

Manual VoiceOver check (not run by the agent that wrote this; the ARIA behaviour is [inferred], not [observed] with VoiceOver):

1. Turn VoiceOver on (Cmd-F5), open `rsi-desktop`, open a session that has a long history. VoiceOver must **not** read the
   transcript aloud on open or when you switch sessions; a new message that arrives while you are viewing is read.
2. Do something that toasts ("Archive", a theme change, a Scheduled jobs Enable). The toast text is spoken, including with a
   dialog open (Scheduled jobs, the theme picker); an error toast is spoken at once.
3. Open each dialog (New session, Trash, Notifications, Choose theme, Scheduled jobs, Diagnostics, Memory search, a question,
   Emergency stop): VoiceOver names it ("Trash, dialog") and a confirm is announced as an alert dialog.
4. Have another session turn to Waiting for you: VoiceOver says "<session> is waiting for you" once, not again on the next refresh.

## The packaged app, driven for real: #232, #242/#243, #245, #229 (local #255)

Run on 2026-10-20 at the desktop stack `7d6a64da0`, on the Mac Mini: macOS 27.0.1 (26A434), arm64, WebKit
22625.1.29.11.28, node v24.22.0, rustc 1.94.1. Everything below is [observed] unless tagged otherwise. Things not
touched: the live rsid (never stopped, restarted or kickstarted; its launchd job has run since the day before), the
operator's `~/Applications` and `~/.local/bin/rsi-desktop`, the Vim keys and macOS Keyboard navigation settings
(`AppleKeyboardUIMode` is unset globally, which means off), and the operator's WebKit store
`~/Library/WebKit/dev.rsi.desktop` (its file mtimes match before and after).

### Build and install (temporary prefixes, as in section 1)

```bash
P=$(mktemp -d /tmp/rsi-255-install.XXXXXX); mkdir -p "$P"/{apps,bin,install,xdg}
RSI_DESKTOP_APP_DIR=$P/apps RSI_INSTALL_BIN_DIR=$P/bin RSI_DESKTOP_INSTALL_DIR=$P/install \
  XDG_DATA_HOME=$P/xdg CARGO_BUILD_JOBS=6 make desktop-install       # CARGO_TARGET_DIR = <sandbox>/target
```

The result was `exit=0 seconds=217` (`Finished release profile in 3m 34s`, log `/tmp/rsi-255-install.log`). `plutil -lint` printed OK. The binary is
`Mach-O 64-bit executable arm64`, `flags=0x20002(adhoc,linker-signed)`, with no hardened runtime.

### How the real app was driven (no TCC grant)

This agent has no macOS privacy grants: `AXIsProcessTrusted`, `CGPreflightPostEventAccess` and
`CGPreflightScreenCaptureAccess` are all false, `osascript` to System Events fails with -1743, and `screencapture` fails with "could
not create image from display". Granting them would change operator settings, so it was not done. Instead:

- **A copy with its own bundle id.** `cp -R "$P/apps/RSI Desktop.app" "$P/apps-255/RSI Desktop 255.app"`, then
  `PlistBuddy -c "Set :CFBundleIdentifier dev.rsi.desktop.acceptance255"`. The binary is byte-identical to `$P/install/rsi-desktop`
  (sha256 `689716f6…d4e1d6`). WebKit keys localStorage by bundle id, so the Vim keys answer and switch used here lived
  in `~/Library/WebKit/dev.rsi.desktop.acceptance255`, which was deleted afterwards along with `~/Library/Caches/dev.rsi.desktop.acceptance255`.
- **An in-process driver.** A ~250-line Objective-C dylib (`/tmp/rsi-255-tools/drive.m`; since committed as
  `scripts/app-driver.m` behind `npm run smoke:app`, see the #259 section below) was loaded with `DYLD_INSERT_LIBRARIES` into that copy only. It is allowed because
  there is no hardened runtime. Launch it without `nohup`: nohup is a SIP binary and strips `DYLD_*`. It polls a control
  directory and, on the app's main thread, does these things:
  - sends real key events through `-[NSWindow sendEvent:]` (the path #247's harness uses);
  - runs page JS with `callAsyncJavaScript` and snapshots with `takeSnapshotWithConfiguration`;
  - reads the app's own AX tree.
  Its constructor sets `AppleKeyboardUIMode=0` in the volatile argument domain (this process only, nothing written)
  and unsets `DYLD_INSERT_LIBRARIES`, so the terminal's shells do not inherit it. The app was deactivated at once
  (accessory policy), so the operator's keyboard focus stayed where it was. Each instance was quit by the driver
  (`[NSApp terminate:]`), and `ps` confirmed its PID gone.
- **The interactive checks ran against the smoke's fake rsid** (`scripts/smoke-fake.mjs` `startFakeDaemon`, a Unix-socket
  JSON-RPC server that records every method), so nothing could write to the live daemon. Across the runs it received 811 requests: reads
  plus one `ArchiveSession`, which it refused (Method not found) during the toast check. Only the Start rsid check used the live
  rsid, read-only and with no key or click.

Screenshots and focus walks are in `/tmp/rsi-255-evidence/` on the Mac (not committed): `walk-*.json`, `01-…12-*.png`, `app-*.stderr`.

### Results

| Item | Result | Evidence |
| --- | --- | --- |
| Tab reaches New session, Send and Stop with Keyboard navigation off (#242/#243) | **pass** | Vim keys off, the Fake worker session open, focus started on the Vim keys switch. 24 real Tabs visited Issues, Actions…, **New session** (3rd), Help, Active/Jobs/Archived, Project…/Sort…/Waiting/Info, the project select, Filter, Show completed, the session list, Copy ID, Details, Hide tool calls, **Stop this turn** (19th), Pause until I resume, Archive, Conversation, the message box and **Send** (24th) (`walk-page-tab.json`, `02-tab-reached-send.png`). Shift-Tab from Send walked back through the message box, Conversation, Archive, Pause and Stop this turn. In New session, real Enter opened the dialog with focus inside; Tab stayed in the dialog and ended on Cancel then Launch; Esc closed it and focus returned to New session (`walk-launch-dialog.json`). |
| WebKit configuration seen in the real app | **finding, #257** | wry 0.57 sets `WKPreferences.tabFocusesLinks = YES` (`wry-0.57.0/src/wkwebview/mod.rs:517`). #247's control fixture in the real app: Tab 1 went to the **plain button without tabindex**, Tab 2 to the stamped one. With `tabFocusesLinks` set NO in-process, Tab skipped the plain button, as in `webkit-smoke.swift`. So the shipped app never had #242's symptom; the stamp is harmless and redundant there. #247's harness tested a configuration the app never runs. **Fixed by #257:** the smoke now runs the wry configuration (and the default one as defense in depth). |
| F6 switches panes with Vim keys off (#243) | **pass** | Global dashboard (`openDashboard()`), focus on the projects table, `html[data-vim-keys=off]`. Real F6 moved to the conversation (`.dash-conv-log`, hint "Conversation: … F6 or Esc returns to the projects"); F6 moved back to the table (hint "Up/Down move · Enter open project · F6 conversation · Esc close"). Shift-F6 did the same both ways. Plain Tab left the pane for the next button (native order). Screenshots `04-` and `05-dashboard-*.png`. |
| Esc in the terminal reaches the shell (#229) | **pass** | Vim keys off. `dispatch('toggle-terminal')` opened the overlay: focus in `xterm-helper-textarea`, mode label hidden. Typed by real key events: `vim -u NONE -i NONE -N /tmp/rsi-255-vimtest.txt⏎`. `ps` showed `/bin/zsh` as a child of the app pid and `vim` as the zsh's child. Then `ihello from 255` (`06-terminal-vim-insert.png`: `-- INSERT --`), real **Esc** (the overlay stayed open, focus stayed in the terminal), then `:wq⏎`. Result: the file contains `hello from 255`, no vim process remains, and the terminal shows the shell prompt again. If Esc had not reached vim, `:wq` would have been typed into the buffer and vim would still run. The shell was ended with `exit⏎` before quitting the app. |
| VoiceOver announces a toast and names a dialog (#245) | **pass for the ARIA wiring in the app's WebKit; not covered for speech** | Archive (real Enter on the focused button) opened `dialog.confirm`: `role="alertdialog"`, `:modal`, `aria-labelledby` resolving to its heading ("Archive session "Fake worker"? …"), focus on Cancel (`08-archive-confirm.png`). Confirming (real Enter) sent `ArchiveSession`; the fake refused it and `#toast` became `role="alert"`, `aria-live="assertive"`, `aria-atomic="true"` with "Archive failed: … Details" (`09-error-toast.png`). With New session open (by real Enter), `toast(…, true)` went inside `#launch-dialog`: `role="status"`, `aria-live="polite"`, topmost at its centre; the dialog is named "New session" through `aria-labelledby="launch-title"` (`11-status-toast-in-dialog.png`). After Esc the toast is back on `<body>`, hidden. `#events` is `role="log"`, `aria-live="polite"`, `aria-relevant="additions"`, `aria-label="Conversation"`. **Not covered:** speech, and the web-content AX tree. The window-level AX tree read fine in-process (AXWindow "(1) RSI" plus its buttons). After switching on app accessibility in-process (`_AXSApplicationAccessibilitySetEnabled`) and reloading, the web area appears as one cross-process remote element with no readable attributes. Resolving it needs the system AX server, i.e. an Accessibility grant, and an untrusted AX observer received no live-region notifications. This WebKit has no `Element.computedRole`. Setting `AXEnhancedUserInterface` on our own app element was refused (-25208). The manual VoiceOver steps in the #245 section above remain the operator's check. |
| Start rsid with the daemon running (#232) | **pass, plus finding #258; the button was never pressed** | **Live rsid** (default socket, read-only, no key or click): stderr `boot=ready daemon=connected sessions=16 steps=daemon:ok,projects:ok,labels:ok,sessions:ok,settings:ok,live:ok`, `data-boot=ready`, no "Cannot reach" card, no Start rsid button, and `daemonStart()` = `{service:{kind:"unknown"},run:{kind:"idle"}}` (the service is only queried once rsid is unreachable). **Missing socket** (`RSI_DAEMON_SOCKET=$P/none.sock`): `boot=offline daemon=unreachable`. The card reads "rsid is installed as a login service (dev.rsi.rsid). Start rsid asks macOS to start it; …" with **Start rsid** enabled and focused, plus Retry now, Actions… (Cmd-K) and "For developers" (`12-offline-start-rsid-card.png`); `daemonStart()` = `{service:{kind:"installed",manager:"launchd",name:"dev.rsi.rsid",path:"$HOME/Library/LaunchAgents/dev.rsi.rsid.plist"},run:{kind:"idle"}}`. **What a press would do** [source: `service.rs` `plan`]: `launchctl print gui/<uid>/dev.rsi.rsid` succeeds (read-only here: `state = running`), so it runs `launchctl kickstart gui/<uid>/dev.rsi.rsid`. Without `-k` that is a no-op for a running job [inferred from launchctl(1)]. The card would then poll the app's socket and time out, because the service holds `~/.rsi/daemon.sock`, not `$P/none.sock`. Filed as #258: offer Start only when starting can help. **argv path with the unit fakes:** `cd desktop/src-tauri && cargo test --lib -- --exact service::tests::<7 names>` printed `7 passed; 0 failed` (44 s; `/tmp/rsi-255-service-tests.log`). Those are: macOS kickstart / bootstrap-when-unloaded, nothing to start without the plist, Linux `systemctl --user start rsid.service`, XDG and system unit dirs, unsupported hosts, a loaded agent kickstarted and an unloaded one bootstrapped, and systemd's single step with its status. |

Not covered, and why: VoiceOver speech and the web-content AX tree (needs an operator Accessibility grant or a person
with VoiceOver; see above); the native menu bar (clicking it needs System Events, refused with -1743); and pressing
Start rsid (the live daemon must not be touched; the argv path is covered by the unit fakes).

## Window chrome follows the app theme (local #283)

What changed: the native title bar and frame take the app's effective light or dark side. The page sends `set_window_theme`
(`src-tauri/src/window_theme.rs`, `Window::set_theme`) whenever the painted theme changes: "Match system (light/dark)" sends
`system`, which hands the window back to macOS (so it already matches what the page matched); an explicit theme sends `light` or
`dark` by its canvas (Light is light; every other built-in theme is dark). Automated: `tests/theme-system-dom.test.mjs` (what is
sent and when, a failing or missing command never breaks theming), the Rust parse tests in `window_theme.rs`, and the smoke step
`theme-contrast` (three calls for light, dark, system). The title bar itself is [inferred], not [observed], by the agent that wrote
this (a headless page has none); check it on the Mac:

1. System Settings > Appearance: Dark. Open RSI Desktop with nothing stored (a fresh profile, or `localStorage` cleared): Match
   system is on, the page is dark and the title bar is dark.
2. Switch macOS to Light while the app is open: the page turns Light and the title bar turns light at the same moment.
3. With macOS Light, choose Settings > Appearance > Built-in theme > Desktop default (or `:theme goth`): the page and the title bar
   are both dark. Choose Light with macOS Dark: both are light.
4. Choose Match system again: the title bar goes back to the macOS setting.

## Repeatable packaged-app check: `npm run smoke:app` (local #259)

#255 drove the real app with a hand-built driver kept in `/tmp`. #259 commits it, so the check can be re-run:

```bash
cd desktop && npm run smoke:app                     # builds the app, then runs the checks; report and PNGs in /tmp/desktop-smoke-app
node scripts/smoke-app.mjs --app "PATH/RSI Desktop.app" [--out DIR] [--keep-run]   # an app you already built
```

**What it does** (`scripts/smoke-app.mjs`):

1. **Build or locate.** With no `--app` (or `$RSI_SMOKE_APP`), it runs `scripts/install-desktop.sh --build-only`. That is
   the `make desktop` path: it builds `target/release/bundle/macos/RSI Desktop.app` and installs nothing, so
   `~/Applications` and `~/.local/bin/rsi-desktop` are never touched. The build log is `DIR/build.log`.
2. **Copy.** The bundle is copied to `/tmp/rsi-smoke-app-XXXXXX/RSI Desktop Smoke.app`, and `plutil` sets its
   `CFBundleIdentifier` to `dev.rsi.desktop.smoke-app`. The copy's binary is started directly, not through `open`.
3. **Environment.** It starts from a short allowlist: no `RSI_*` (session token, live socket) and no `DYLD_*` from the
   caller. `HOME` and `CFFIXED_USER_HOME` are set to the run directory, where an empty `.zshrc` waits. In the run below
   WebKit's store and caches went to `<run>/home/Library/{WebKit,Caches}/dev.rsi.desktop.smoke-app`, and nothing went
   under the real `~/Library`. Cleanup still deletes the test bundle id's `~/Library` folders, exact id only, and it
   refuses `dev.rsi.desktop`.
4. **Driver.** `scripts/app-driver.m` is compiled with `xcrun clang` into a dylib, cached by source hash. It is set in
   `DYLD_INSERT_LIBRARIES` on the copy's environment only. Its constructor clears that variable first, so the
   terminal's shells never load it. It acts only when `RSI_APP_DRIVER_CTL` is set and the bundle id starts with
   `dev.rsi.desktop.smoke-app`, so it cannot drive the operator's app. It turns Keyboard navigation off for that process
   only (`AppleKeyboardUIMode=0`, volatile argument domain) and keeps the app an accessory that never activates.
   Commands go through a polled control directory: `info`, `deactivate`, `js`, `key`, `type`, `walk` (#247's step loop
   in-process), `snap` (`takeSnapshotWithConfiguration`), `ax` (the app's own window-level tree) and `quit`.
5. **Checks against the smoke's fake rsid.** The app starts on `<run>/d.sock`, where nothing listens. The script then
   starts `startFakeDaemon()` and symlinks the socket to it. The live rsid is never contacted, and Start rsid is never
   pressed.
6. **Teardown.** It sends `quit` (falling back to SIGTERM, then SIGKILL, for the PID it started), kills any shell the app
   left that it can identify, stops the fake, deletes the run directory and writes `DIR/report.json`.

**Exit codes.** 0 means every check passed. 1 means a check failed. 2 means bad usage. 3 means `runner_unavailable`,
with 0 checks executed: not macOS, no `xcrun clang`, no Aqua session (`launchctl managername`), a failed build, or a
hardened-runtime binary (dyld would ignore the driver). It is never green without running the checks.

| Check | What passes |
| --- | --- |
| `launch` | The copy runs as `dev.rsi.desktop.smoke-app` on a fresh WebKit store (the first-run Vim keys question is `pending`), with `AppleKeyboardUIMode=0` and `DYLD_INSERT_LIBRARIES` cleared. The question is then marked answered and the page reloaded, so Vim keys stay off. |
| `daemon-down` | The card is titled "Cannot reach the rsid daemon" and names the run's socket. Start rsid is shown and disabled, with its status line: the test HOME has no `dev.rsi.rsid.plist`, so there is nothing to start. Retry now is present. Snapshot `01-daemon-down.png`. |
| `reconnect` | After the fake comes up, `data-boot=ready` and sessions are listed, with no key or click. |
| `page-tab`, `page-shift-tab`, `confirm-dialog`, `launch-dialog` | #247's WebKit steps and judge (`smoke-webkit.mjs` `STEPS`, `judge`), run in the real app with real Tab and Shift-Tab. Tab reaches New session, Stop this turn, Archive, the conversation, the message box and Send in page order; Shift-Tab walks back; the dialogs keep Tab inside. Snapshots `02-*.png`. |
| `f6-panes` | With Vim keys off, on the dashboard, F6, F6, Shift-F6, Shift-F6 move focus conversation, table, conversation, table; the hint names F6, and Esc closes the dashboard. Snapshot `03-…`. |
| `terminal-esc` | The terminal overlay opens and zsh is a child of the app. `vim -u NONE -i NONE -N <run>/vim-esc.txt` is typed, and vim is a child of zsh. Then `ihello from smoke`, real Esc (the overlay and its focus stay), and `:wq`: the file holds `hello from smoke`, vim has exited, and then `exit`. Snapshots `04-`, `05-`. |
| `dialog-aria` | Real Enter on Archive opens a modal `alertdialog` named by its heading, with focus on Cancel. Tab reaches its Archive button and Enter confirms; the fake refuses `ArchiveSession`, and the toast is `role=alert` `aria-live=assertive`. Real Enter on New session opens it, and a confirmation toast lands inside `#launch-dialog`, `role=status` polite, topmost, with the dialog named "New session". Esc returns the toast to `<body>` and focus to New session. `#events` is `role=log` polite additions. Snapshots `06-` to `08-`. |
| `ax-window` | The app's own AX tree has an `AXWindow` ("(1) RSI"), dumped to `ax-window.json`. |

Not covered (also listed in the report): VoiceOver speech and the web-content AX tree, because WebKit's remote element
needs an operator Accessibility grant; pressing Start rsid, which would ask launchd to start the live service (the argv
path is covered by `service::tests`); and the native menu bar, which needs System Events.

**Foreground run [observed]** (2026-10-20, Mac Mini, macOS 27.0.1, WebKit 22625.1.29.11.28, desktop stack `a7949aec1`
plus this change): `cd desktop && npm run smoke:app` exited 0 in **121 s**. Of that, 95 s was the build on a warm
target; the release crate recompiles because `npm run build` rewrites `dist/`. A cold build in this sandbox took 208 s,
and #255's took 217 s. The checks took about 25 s. All 11 passed, with app sha256 `abaf71740ea6…`. The fake answered
54 requests. Teardown: the app exited 0, no leftover process, nothing under `~/Library` to remove, and the run directory
was deleted. Three runs with `--app` on the same build took 24.8 to 25.6 s each, all passing.

**Gate decision: on demand, not in the desktop gate.** The checks take only about 25 s, but they need a release build
of the Tauri app: 95 s warm and 208 to 217 s cold, in a profile the gate's debug `cargo test` does not share. That build
alone exceeds a smoke group's 90 s budget (#250). The check also needs a logged-in Aqua session, puts a window on the
operator's screen for about 25 s, and has nothing to run on the Linux gate hosts. So:

- `tests/smoke-app.test.mjs` is in `npm test`. It runs the 8 fast unit tests: arguments, `runner_unavailable` paths,
  the environment allowlist, exact-id cleanup, the driver's bundle guard, and the driver compiling with `-Werror` on
  macOS (under 1 s).
- The real run in that file is opt-in. It runs only when `RSI_SMOKE_APP` names a built bundle, and otherwise skips with
  `runner_unavailable`.
- Run `npm run smoke:app` by hand, or as a job, for desktop changes to keyboard and focus, the terminal, dialogs, toasts
  or the daemon-down card, and before a desktop release. Pass `--app` to reuse a build.

Revisit this if the gate gains a persistent macOS GUI runner with a warm release target.

## Closing the window keeps the app running; the Dock brings it back (local #311)

Needs a built `RSI Desktop.app` on macOS. Linux and Windows are unchanged: closing the last window quits.

1. Open the app, open a session, type an unsent draft in its message box, and open a dialog (Cmd-K, or `:` command mode with text typed).
2. Press Cmd-W. The window disappears, the Dock icon keeps its running dot and the menu bar stays. Repeat with Window > Close Window and with the red button.
3. Click the Dock icon. The window returns with the same session, the draft, the open dialog and any terminal overlay shell as they were.
4. Close again and choose any menu item that has an action (for example View > a pane): the window comes back, and the action ran.
5. Close the window and press Cmd-Q: the app quits. Repeat with File > Quit, and with `ZQ`, `ZZ` and `:quit` on the visible window: all quit. After a quit no terminal shell from the overlay is left running (`ps`), while after a plain close (step 2) the shell was still running.
