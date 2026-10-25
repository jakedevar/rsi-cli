// The overlay key sweep (local #327, #336). Run: `npm run audit -- keys [--overlay NAME,NAME] [--out DIR]`.
//
// For each table in SWEEP_OVERLAYS it opens the view that table documents in the built app (fake daemon with several rows per list, Vim
// keys on, writes stubbed), asks the app's own `?` help whether it lists that overlay's keys, then presses EVERY key the table lists
// (OVERLAY_TABLES of the built app: src/overlay-help.generated.ts, the table the help overlay prints), each from a fresh open, and diffs a
// snapshot of the page before and after. A key that changes nothing is a FAIL unless it is in the overlay's `noop` list with a reason.
// Dialogs, panes that are not dialogs (`pane` + `sel`), forms, typed-filter overlays (`modes`) and modes of a view (`enter`) are all swept;
// the tables that are not are named in SKIPPED_OVERLAYS with the reason, and `coverage` says which table is in neither.
// Results are appended to keys.jsonl as they are produced (a run that dies keeps what it had); `--overlay` picks a subset so one run
// fits a 10-minute call. No path leaves desktop/: the tables are read from dist/, never from ../../docs.
//
// Pure parts (parseKeySpec, judgePress, plan, coverage, wasTyped) are exported for tests/audit-keysweep.test.mjs; sweepOverlay needs a page.
import { appendFileSync } from 'node:fs';
import { join } from 'node:path';
import { KEY_LOCAL, keyFixtures, seedNotices } from './audit-fixtures.mjs';
import { WALK_STUBS } from './audit-listbox-walk.mjs';

/** Put a draft in the composer: the AI commands and the large editor are unavailable until the message box has text. */
const draftMessage = (page) => page.eval(`(() => { const m = document.getElementById('message'); m.focus(); m.value = 'audit draft'; m.dispatchEvent(new Event('input', { bubbles: true })); })()`);
const K321 = { 'Ctrl-J': '#321', 'Ctrl-K': '#321', Home: '#321', End: '#321' };
const SCROLL_FITS = 'scrolls the detail pane; the fake detail fits the pane, so there is nothing to scroll';
const SCROLL_FITS_PREVIEW = 'scrolls the preview; the fake session\'s prompt fits the pane, so there is nothing to scroll';
const SCROLL_FITS_TODO = 'scrolls the item text; the fake to-do fits the pop-up, so there is nothing to scroll';
const NORMAL_MODE = [{ key: 'Escape' }, { key: '0' }, { key: 'l' }];
const ONE_LINE = 'moves between lines; the draft is one line';
const NO_PREVIEW = 'accepts or discards a compiled preview; none is showing';
const NO_CLIPBOARD = 'pastes from the system clipboard, which the headless page does not have';
/** The desktop keeps plain Ctrl-A as select-all (assist.ts): the AI command chord is Ctrl-Alt-A. */
const AI_CHORD = { 'Ctrl-A': 'Ctrl-Alt-A' };
/**
 * The overlays swept, by the name of their key table (`docs/ux-audit.md` has the field table). `action` is the palette action id that opens
 * the view (or `open(a, ctx)`); `pane` + `sel` say it is not a dialog; `enter` keys reach a mode of it; `type` is typed once it is open;
 * `seed` runs in the page before the view opens; `modes` press listed keys from an open that first typed text / pressed keys; `reloads` are
 * keys whose daemon read is their effect; `remap` presses the desktop's own key for a table key; `noop` maps a key spec (as the table writes
 * it) to why pressing it may change nothing in the fake-daemon page. Add an overlay here and it is swept.
 */
export const SWEEP_OVERLAYS = {
  // `known`: a key whose gap is filed as an Issue; it is reported as a known gap, not a FAIL, and the sweep says when it starts to work
  'Sort Picker': { action: 'choose-sort', known: { g: '#321', G: '#321' } },
  'Theme Picker': { action: 'theme-picker' },
  // `modes`: keys that only act in a mode of the view (filter text typed, a section chosen) are pressed from a fresh open that gets there first
  'Project Picker': { action: 'choose-project', known: K321, modes: [{ text: 'p', specs: ['Type, Backspace'] }] },
  'Label Picker': { action: 'choose-label', known: K321, modes: [{ text: 'l', specs: ['Type, Backspace'] }] },
  'Parent Picker': { action: 'move-to-parent', known: K321, modes: [{ text: 'e', specs: ['Type, Backspace'] }] },
  'Model Picker': {
    action: 'model-dropdown',
    modes: [{ text: '/o', specs: ['Up / Down / Ctrl-N / Ctrl-P', 'Backspace / Ctrl-W / Ctrl-U', 'Home / End'] }],
  },
  // ---- panes (not dialogs): `sel` is the pane's root, `pane: true` lets the sweep read it instead of a dialog ----
  'Global Dashboard': { reloads: ['r'], action: 'mgr-dashboard', pane: true, sel: '#dashboard-view' },
  'Manager Board': {
    reloads: ['r'], action: 'mgr-board', pane: true, sel: '#managers-view', modes: [{ specs: ['[ / ]'], enter: [{ key: '4' }] }],
    noop: { 1: 'already the Board section', n: 'the fake daemon has one page of rows', PageDown: SCROLL_FITS, PageUp: SCROLL_FITS },
  },
  'Manager Decisions': {
    reloads: ['r'], action: 'mgr-decisions', pane: true, sel: '#managers-view',
    modes: [{ specs: ['[ / ]'], enter: [{ key: '4' }] }, { specs: ['h', 'Left'], enter: [{ key: 'l' }] }],
    noop: { 2: 'already the Decisions section', n: 'the fake daemon has one page of rows', PageDown: SCROLL_FITS, PageUp: SCROLL_FITS },
  },
  'Manager Policy': {
    reloads: ['r'], action: 'mgr-policy', pane: true, sel: '#managers-view', modes: [{ specs: ['[ / ]'], enter: [{ key: '4' }] }],
    noop: { 5: 'already the Policy section' },
  },
  // Graph review: an empty canvas; `o` + Enter opens the saved workflow (three nodes), a second Enter its node detail; `o` alone shows the picker
  'Graph Review / Empty': { action: 'graph-open', pane: true, sel: '#graph-view', modes: [{ specs: ['c'], enter: [{ key: 'H', shift: true }] }] },
  'Graph Review': { action: 'graph-open', pane: true, sel: '#graph-view', enter: [{ key: 'o' }, { key: 'Enter' }], modes: [{ specs: ['h', 'j', '[', 'z'], enter: [{ key: 'l' }] }, { specs: ['k'], enter: [{ key: 'l' }, { key: 'j' }] }, { specs: ['c'], enter: [{ key: 'H', shift: true }] }, { specs: ['u'], enter: [{ key: 'd' }] }], noop: { Tab: 'focuses the dashboard of a bridged graph; this is a saved workflow' } },
  'Graph Review / Node Detail': { action: 'graph-open', pane: true, sel: '#graph-view', enter: [{ key: 'o' }, { key: 'Enter' }, { key: 'Enter' }], modes: [{ specs: ['['], enter: [{ key: ']' }] }, { specs: ['c'], enter: [{ key: 'H', shift: true }] }] },
  'Graph Review / Picker': { action: 'graph-open', pane: true, sel: '#graph-view', enter: [{ key: 'o' }], modes: [{ specs: ['Ctrl-U'], enter: [{ key: 'ArrowDown' }] }] },
  'Graph Review / Edit Field': {
    action: 'graph-open', pane: true, sel: '#graph-view', enter: [{ key: 'o' }, { key: 'Enter' }, { key: 'Enter' }, { key: 'i' }],
  },
  'Recursive DAG Browser': {
    action: 'dag-open', pane: true, sel: '#dag-view', reloads: ['r', 'Enter'],
    modes: [{ specs: [']a'], enter: [{ key: 'Tab', shift: true }] }, { specs: ['[a'], enter: [{ key: 'Tab', shift: true }, { key: ']' }, { key: 'a' }] }, { specs: ['Ctrl-D', 'p / m / t / d'], enter: [{ key: 'Enter' }] }, { specs: ['Ctrl-U'], enter: [{ key: 'Tab', shift: true }, { key: 'Enter' }, { key: 'd', ctrl: true }] }],
  },
  'File Explorer': {
    action: 'files-explorer', pane: true, sel: '.files-overlay',
    // h / Left / z need an expanded folder (l opens src/); the width keys need a drawer that is not at its default width (two > widen it; the width is kept in localStorage across the sweep's reloads, so one step could land back on the default)
    modes: [{ specs: ['h', 'Left', 'z'], enter: [{ key: 'l' }] }, { specs: ['Ctrl-0', '='], enter: [{ key: '>' }, { key: '>' }] }],
  },
  // ---- dialogs the palette opens ----
  'Manager Scope': { action: 'mgr-scope', pane: true, sel: '#managers-view' },
  'Source Worktree Settlement': { action: 'admin-settlement', reloads: ['r'], modes: [{ specs: ['K'], enter: [{ key: 'J', shift: true }] }] },
  'Theme Role Editor': { action: 'theme-roles' },
  'Rename Session': { action: 'rename-session' },
  'Rate Session': { action: 'rate-session', modes: [{ specs: ['Enter'], enter: [{ key: '5' }] }] },
  'Session Info': { action: 'session-info' },
  'Prompt Preview': { action: 'prompt-preview', noop: { 'Ctrl-D': SCROLL_FITS_PREVIEW, 'Ctrl-U': SCROLL_FITS_PREVIEW } },
  Diagnostics: { action: 'admin-diagnostics' },
  'Card Editor': { action: 'card', modes: [{ specs: ['K'], enter: [{ key: 'j' }] }] },
  Dialectic: { action: 'dialectic', modes: [{ text: 'ab', specs: ['Backspace', 'Enter'] }], noop: { 'Up / Down': 'scrolls the conversation; the fake conversation is empty' } },
  'AI Command': { action: 'ai-command', seed: draftMessage, modes: [{ text: 'ab', specs: ['Backspace', 'Enter'] }] },
  'AI Chat': { action: 'ai-chat', seed: draftMessage, modes: [{ text: 'ab', specs: ['Backspace', 'Enter'] }], noop: { 'Up / Down': 'scrolls the conversation; the fake conversation is empty' } },
  // The Vim editors open in insert mode, where a normal-mode key is typed text: Esc, then 0 and l put the caret in the second column of a
  // two-word draft, so h l w b have somewhere to go (j k have no second line)
  'Input Modal': {
    action: 'input-modal', seed: draftMessage, remap: { ...AI_CHORD, 'w b': 'w' }, enter: NORMAL_MODE,
    noop: { 'Ctrl-V': NO_CLIPBOARD, j: ONE_LINE, k: ONE_LINE }, modes: [{ specs: ['Esc', 'Enter'], enter: [{ key: 'i' }] }],
  },
  'New Session Prompt': {
    action: 'launch', remap: { ...AI_CHORD, 'w b': 'w' }, enter: [{ text: 'audit one two' }, ...NORMAL_MODE],
    noop: { 'Ctrl-V': NO_CLIPBOARD, j: ONE_LINE, k: ONE_LINE, 'a / d': NO_PREVIEW }, modes: [{ specs: ['Esc'], enter: [{ key: 'i' }] }],
  },
  'Continue Prompt': {
    action: 'continue', remap: { ...AI_CHORD, 'w b': 'w' }, enter: [{ text: 'audit one two' }, ...NORMAL_MODE],
    noop: { 'Ctrl-V': NO_CLIPBOARD, j: ONE_LINE, k: ONE_LINE, 'a / d': NO_PREVIEW }, modes: [{ specs: ['Esc'], enter: [{ key: 'i' }] }],
  },
  'Text Area Background': {
    action: 'text-area-background', modes: [{ text: '#12', specs: ['Backspace'] }],
    noop: { Delete: 'clears a saved override; the fresh page has none', Enter: 'the seventh character of a hex colour already applies it, so Enter after a full colour changes nothing' },
  },
  // the form opens in a text box (insert mode): two Esc reach its normal-mode keys
  'Create Entity / Normal': { action: 'create-task', enter: [{ key: 'Escape' }, { key: 'Escape' }], modes: [{ specs: ['h', 'l'], enter: [{ key: 'k' }] }, { specs: ['Ctrl-Enter'], enter: [{ key: 'n' }, { key: 'i' }, { text: 'audit task' }, { key: 'Tab' }, { text: 'audit,' }] }] },
  'Project Form': { action: 'project-new', modes: [{ text: 'ab', specs: ['Backspace'] }, { specs: ['Left', 'Right'], enter: [{ key: 'Tab' }, { key: 'Tab' }] }] },
  'Provider Key Form': { action: 'admin-provider-keys', enter: [{ key: 'Enter' }], modes: [{ text: 'ab', specs: ['Backspace'] }] },
  'Provider Form': {
    action: 'open-providers', enter: [{ key: 'Enter' }, ...NORMAL_MODE], remap: { 'w b': 'w' }, noop: { j: ONE_LINE, k: ONE_LINE }, modes: [{ specs: ['Esc'], enter: [{ key: 'i' }] }],
  },
  'Message Bridge Form': { action: 'open-bridges', enter: [{ key: 'Enter' }], modes: [{ specs: ['Backspace'], enter: [{ key: 'Tab' }, { text: 'ab' }] }, { specs: ['Shift-Tab'], enter: [{ key: 'Tab' }] }] },
  'Hook Form': { action: 'open-hooks', enter: [{ key: 'Enter' }], modes: [{ text: 'ab', specs: ['Backspace'] }] },
  'To-do Pop-up': {
    action: 'todo-view', modes: [{ specs: ['p'], enter: [{ key: 'n' }] }, { specs: ['Enter'], enter: [{ key: 'd' }] }],
    noop: { j: SCROLL_FITS_TODO, k: SCROLL_FITS_TODO, Down: SCROLL_FITS_TODO, Up: SCROLL_FITS_TODO, 'Ctrl-D': SCROLL_FITS_TODO, 'Ctrl-U': SCROLL_FITS_TODO, g: SCROLL_FITS_TODO, Home: SCROLL_FITS_TODO },
  },
  Fleet: { action: 'mgr-fleet', pane: true, sel: '#managers-view', reloads: ['r'] },
  'Fleet Filter': { action: 'mgr-fleet', pane: true, sel: '#managers-view', enter: [{ key: '/' }], modes: [{ text: 'ab', specs: ['Backspace'] }] },
  'Fleet Sort': { action: 'mgr-fleet', pane: true, sel: '#managers-view', enter: [{ key: 's' }] },
  // the fake daemon has one project group but two provider groups: `b` regroups before the move keys run
  'Fleet Usage': { action: 'mgr-fleet', pane: true, sel: '#managers-view', enter: [{ key: 'Tab' }, { key: 'b' }], reloads: ['r'] },
  'Global Manager Workspace': { action: 'mgr-global', pane: true, sel: '#managers-view', reloads: ['r'] },
  'Question / Normal': { action: 'answer-question', noop: { 'Enter / Backspace': 'moves between questions; the fake waiting session asks one' } },
  'Question / Insert': { action: 'answer-question', enter: [{ key: 'i' }], modes: [{ text: 'ab', specs: ['Ctrl-Enter'] }] },
  'Label Form': { action: 'choose-label', enter: [{ key: 'n', ctrl: true }], modes: [{ text: 'ab', specs: ['Backspace'] }, { specs: ['Left', 'Right'], enter: [{ key: 'Tab' }, { key: 'Tab' }] }] },
  'Schedule Form': { action: 'admin-schedules', enter: [{ key: 'n' }], modes: [{ text: 'ab', specs: ['Backspace'] }, { specs: ['Left', 'Right'], enter: [{ key: 'Tab' }, { key: 'Tab' }] }] },
  'Create Entity / Insert': { action: 'create-task', enter: [{ key: 'i' }], modes: [{ specs: ['Ctrl-Enter'], enter: [{ text: 'audit task' }, { key: 'Tab' }, { text: 'audit,' }] }] },
  'Create Entity / Body': {
    action: 'create-task', enter: [{ key: 'b' }, { text: 'audit one two' }, ...NORMAL_MODE], remap: { 'w b': 'w' }, noop: { j: ONE_LINE, k: ONE_LINE },
    modes: [{ specs: ['Esc'], enter: [{ key: 'i' }] }, { specs: ['Ctrl-Enter'], enter: [{ key: 'n' }, { key: 'i' }, { text: 'audit task' }, { key: 'Tab' }, { text: 'audit,' }] }],
  },
  'Create Entity / Topology': { action: 'create-task', enter: [{ key: 'T' }], modes: [{ specs: ['Ctrl-Enter'], enter: [{ key: 'Escape' }, { key: 'n' }, { key: 'i' }, ...[{ text: 'audit task' }, { key: 'Tab' }, { text: 'audit,' }]] }], noop: { 'j / k, Down / Up, G': 'the fake daemon lists too few topologies to move between (the typed j / k filter instead)' } },
  'New Session Settings': { action: 'launch', enter: [{ key: 'Escape' }, { key: 'o', ctrl: true }] },
  // the files explorer: its drawer, the fuzzy finder inside it, the viewer, and the keys when the viewer has the focus
  'File Explorer / Finder': { action: 'files-explorer', pane: true, sel: '.files-overlay', enter: [{ key: '/' }], modes: [{ text: 'ab', specs: ['Backspace', 'Ctrl-U', 'Ctrl-W'] }] },
  'File Explorer / Viewer Focus': { action: 'files-explorer', pane: true, sel: '.files-overlay', enter: [{ key: 'j' }, { key: 'Enter' }, { key: 'l', ctrl: true }] },
  'File Viewer': {
    action: 'files-explorer', pane: true, sel: '.files-overlay', enter: [{ key: 'j' }, { key: 'Enter' }],
    // h needs a column to move left from, Esc needs insert mode to leave; `w b` is a motion and its inverse, so only w is pressed
    modes: [{ specs: ['h'], enter: [{ key: 'l' }] }, { specs: ['Esc'], enter: [{ key: 'i' }] }],
    remap: { 'w b': 'w' },
  },
  Telescope: { action: 'files-find', pane: true, sel: '.files-picker', modes: [{ text: 'a', specs: ['Type, Backspace', 'Ctrl-U'] }] },
  'Switch Model / Effort': { action: 'switch-model', modes: [{ specs: ['l', 'Right'], enter: [{ key: 'h' }] }] },
  'Memory Search': { action: 'memory-search', type: 'ab', known: { 'Ctrl-J': '#321', 'Ctrl-K': '#321' } },
  Notifications: {
    action: 'open-notifications',
    seed: (page) => seedNotices(page, 3),
    noop: { Enter: 'opens the notice\'s linked session (the seeded notices link none)', 'Ctrl+0': 'resets the dialog geometry (it is already at its default)' },
  },
  Trash: { action: 'open-trash' },
  'Recent Completions': { action: 'open-recent' },
  'Scheduled Jobs Browser': { action: 'admin-schedules', known: { '?': '#321' }, noop: { r: 'refreshes the list; the fake daemon answers the same rows' } },
};

/**
 * The overlay key tables the sweep does NOT press, each with the reason. Every table of the built app is in SWEEP_OVERLAYS or here
 * (`coverage` and tests/audit-keysweep.test.mjs fail otherwise), so a table added later has to be classified.
 */
export const SKIPPED_OVERLAYS = {
  'Command Palette': 'the sweep opens every view through the palette and its snapshot leaves the palette out; tests/palette-*.test.mjs press its keys',
  Terminal: 'the keys go to a shell in an xterm; a fake-daemon page has no shell to answer them',
  'Budget Policy Form': 'opens from the Budget policies row of the settings screen, which the fake daemon\'s settings catalog does not list',
  'Hook Save Conflict': 'appears only when the daemon reports that settings.json changed since it was read; the fake daemon never does',
  'Daemon Text Setting': 'a text box inside a settings row; the sweep does not walk the settings screens (audit views opens them)',
  'Skill Preview': 'opens from a skill row of the skills settings, which the fake daemon lists empty',
  'Manager / Text Entry': 'a sub-mode reached through a policy field that takes a number; the fake policy\'s rows open a pick list or the preset prompt first',
  'Screens Without an Overlay Catalog': 'a list of screens that have no key table, not a key table',
  'Surfaces Documented by Hand': 'a list of surfaces whose keys are not a table, not a key table',
};

/** Which tables of the app are swept, skipped, neither (`missing`) or both, and which names are not tables (`stale`). Pure. */
export function coverage(tables) {
  const names = tables.map((t) => t.name);
  const swept = Object.keys(SWEEP_OVERLAYS);
  const skipped = Object.keys(SKIPPED_OVERLAYS);
  return {
    swept: names.filter((n) => swept.includes(n)),
    skipped: names.filter((n) => skipped.includes(n)),
    missing: names.filter((n) => !swept.includes(n) && !skipped.includes(n)),
    both: names.filter((n) => swept.includes(n) && skipped.includes(n)),
    stale: [...swept, ...skipped].filter((n) => !names.includes(n)),
  };
}

// ---------- key specs ----------

const NAMED = {
  Esc: 'Escape', Escape: 'Escape', Enter: 'Enter', Tab: 'Tab', Space: ' ', Backspace: 'Backspace', Delete: 'Delete', Down: 'ArrowDown', Up: 'ArrowUp',
  Left: 'ArrowLeft', Right: 'ArrowRight', Home: 'Home', End: 'End', PageUp: 'PageUp', PageDown: 'PageDown',
};
const MODS = { ctrl: 'ctrl', control: 'ctrl', shift: 'shift', alt: 'alt', meta: 'meta' };

/** One token such as `j`, `Down`, `Ctrl-D`, `Shift-Tab` -> `{ key, ctrl?, shift?, alt? }`, or null when it is not a key name. */
function tokenStep(token) {
  const mods = {};
  let rest = token;
  for (let m = /^(ctrl|control|shift|alt|meta)[-+](.+)$/i.exec(rest); m; m = /^(ctrl|control|shift|alt|meta)[-+](.+)$/i.exec(rest)) {
    mods[MODS[m[1].toLowerCase()]] = true;
    rest = m[2];
  }
  const named = NAMED[rest];
  if (named === undefined && rest.length !== 1) return null;
  // `Ctrl-D` is the key d with Ctrl held; a bare capital (`G`) is shifted.
  const letter = named === undefined && /[A-Za-z]/.test(rest);
  const modified = mods.ctrl || mods.alt || mods.meta;
  const key = named ?? (letter && modified ? rest.toLowerCase() : rest);
  return { key, ...mods, ...(letter && !modified && rest !== rest.toLowerCase() ? { shift: true } : {}) };
}

const TEXT_ENTRY = /^(type\b|other keys|no overlay|paste)/i;

/**
 * A table's `keys` cell -> what to press. `presses` are `{ label, steps }` (steps are `{ key, ctrl?, shift?, alt? }` pressed in turn);
 * `skipped` are `{ label, why }` for the parts that are not one key press (typed text, descriptions, the global leader).
 * The cell is a list of alternatives (`j / k, Down / Up`); `dd` or `Space e` is a chord; `h j k l` is four keys; `1-9` presses `1`.
 */
export function parseKeySpec(spec) {
  const presses = [];
  const skipped = [];
  const skip = (label, why) => skipped.push({ label, why });
  // a cell that describes a mode ("Grant page: j / k, h / l, Space, digits") is one description, whatever it lists
  if (/^[A-Za-z][A-Za-z ]*: \S/.test(spec) && spec.length > 2) { skip(spec, 'a description, not a key name'); return { presses, skipped }; }
  for (const raw of spec.split(/\s*,\s+|\s+\/\s+/)) {
    const alt = raw.replace(/\s*\([^)]*\)\s*$/, '').trim();
    if (!alt) continue;
    if (TEXT_ENTRY.test(alt)) { skip(alt, 'typed text or a description, not one key'); continue; }
    if (/arrows$|^grant page|: |:$/i.test(alt) && alt !== ':') { skip(alt, 'a description, not a key name'); continue; }
    const range = /^(\d)-(\d)$/.exec(alt);
    if (range) { presses.push({ label: alt, steps: [{ key: range[1] }] }); continue; }
    const tokens = alt.split(/\s+/);
    if (tokens[0] === 'Space' && tokens.length === 2 && (tokens[1] === 'N' || tokens[1] === ';')) { skip(alt, 'the global leader, not this overlay\'s key'); continue; }
    const single = tokenStep(alt);
    if (single && tokens.length === 1) { presses.push({ label: alt, steps: [single] }); continue; }
    if (tokens.length === 1) {
      // `dd`, `za`, `]a`: a chord typed as letters
      const steps = [...alt].map(tokenStep);
      if (alt.length > 1 && alt.length <= 3 && steps.every(Boolean)) { presses.push({ label: alt, steps }); continue; }
      skip(alt, 'not a key name'); continue;
    }
    const steps = tokens.map(tokenStep);
    if (steps.some((s) => !s)) { skip(alt, 'not a key name'); continue; }
    // `Space e`, `C g`, `Esc Esc` are chords; `h j k l` lists four keys.
    if (tokens.length === 2 || tokens[0] === 'Space') presses.push({ label: alt, steps });
    else steps.forEach((s, i) => presses.push({ label: tokens[i], steps: [s] }));
  }
  return { presses, skipped };
}

/** A key that closes the overlay ends the run of an overlay, so it is pressed last. */
const closes = (p) => p.steps.length === 1 && ['Escape', 'q'].includes(p.steps[0].key);
/** Keys that move backwards (or to the top) do nothing on the first row: the sweep steps off it first so a key that works is seen working. */
const needsRowAbove = (p) => p.steps.length === 1 && !p.steps[0].alt && (['g', 'Home', 'Up', 'ArrowUp', 'PageUp', 'k'].includes(p.steps[0].key) || (p.steps[0].ctrl && ['k', 'p', 'u'].includes(p.steps[0].key)));
/** "Jump to first" starts at the first row, so it is pressed after the keys that move away from it (the table lists `g` before `G`). */
const toFirst = (p) => p.steps.length === 1 && ['g', 'Home'].includes(p.steps[0].key) && !p.steps[0].ctrl;

/** The index of the `modes` entry (see SWEEP_OVERLAYS) whose `specs` name this row's cell or this key, else -1. */
const modeOf = (modes, spec, label) => modes.findIndex((m) => m.specs.includes(spec) || m.specs.includes(label));

/**
 * The plan for one overlay: every key its table lists, as `{ spec, action, label, steps, noop? }`, closing keys last, plus what it skipped.
 * `tables` is OVERLAY_TABLES of the built app; `noop` maps a spec to a reason.
 */
export function plan(name, tables, noop = {}, modes = [], remap = {}, known = {}) {
  const table = tables.find((t) => t.name === name);
  if (!table) throw new Error(`the app has no overlay key table named ${name}`);
  const presses = [];
  const skipped = [];
  for (const row of table.rows) {
    const p = parseKeySpec(row.keys);
    for (const k0 of p.presses) {
      // `remap`: a key the desktop binds differently from the TUI table (documented in the source), pressed as the desktop's key
      const to = remap[k0.label];
      const k = to ? { label: k0.label, steps: parseKeySpec(to).presses[0].steps } : k0;
      const mode = modeOf(modes, row.keys, k.label);
      const why = noop[row.keys] ?? noop[k.label];
      presses.push({
        spec: row.keys, action: row.action, label: k.label, steps: k.steps, ...(to ? { remappedTo: to } : {}),
        ...(toFirst(k) ? { toFirst: true } : {}), ...(needsRowAbove(k) ? { offFirst: true } : {}),
        ...(mode >= 0 ? { mode } : why ? { noop: why } : {}),
        ...(known[row.keys] ?? known[k.label] ? { known: known[row.keys] ?? known[k.label] } : {}),
      });
    }
    for (const s of p.skipped) skipped.push({ spec: row.keys, action: row.action, ...s });
  }
  const middle = presses.filter((p) => !closes(p));
  return { name, presses: [...middle.filter((p) => !toFirst(p)), ...middle.filter(toFirst), ...presses.filter(closes)], skipped };
}

// ---------- snapshot and verdict ----------

/**
 * In the page: everything a key press could change. The palette is left out (it closes when an action is chosen). `sel` is the CSS selector of
 * a pane that is not a dialog (the file explorer, the DAG browser); the snapshot reads the topmost open dialog, else that pane, else the body,
 * and `present` says whether the pane is still on screen (a key that closes it is a change, and the next key starts from a fresh open).
 */
export const snapshotFor = (sel = null) => `(() => {
  const open = [...document.querySelectorAll('dialog[open]')].filter((d) => d.id !== 'palette');
  const top = open[open.length - 1] ?? null;
  const pane = ${JSON.stringify(sel)} ? document.querySelector(${JSON.stringify(sel)}) : null;
  const root = top ?? pane ?? document.body;
  // ages and clocks tick on their own ("new · 3s", "12:03"): they are not a key's effect
  const calm = (t) => t.replace(/\\s+/g, ' ').replace(/\\b\\d+(?:ms|s|m|h|d)\\b|\\b\\d{1,2}:\\d{2}(?::\\d{2})?\\b/g, '#').trim();
  const a = document.activeElement;
  const toast = document.getElementById('toast')?.checkVisibility() ? document.getElementById('toast').innerText.trim() : '';
  let scroll = 0;
  for (const e of root.querySelectorAll('*')) scroll += e.scrollTop;
  return {
    dialogs: open.map((d) => d.id || d.className || 'dialog').join('>'),
    present: ${JSON.stringify(sel)} ? !!pane?.checkVisibility() : true,
    // widths of the panels in view: a drawer that was resized or reset changes no text
    box: [root, ...root.querySelectorAll('aside, section, [class*=drawer]')].map((e) => Math.round(e.getBoundingClientRect().width)).join(','),
    text: calm(root.innerText ?? root.textContent ?? '').slice(0, 6000),
    typing: !!a && (a.tagName === 'TEXTAREA' || (a.tagName === 'INPUT' && !['checkbox', 'radio', 'button', 'submit'].includes(a.type))),
    caret: a && typeof a.selectionStart === 'number' ? a.selectionStart + ':' + a.selectionEnd : '',
    active: a ? a.tagName + '#' + a.id + (a.getAttribute('aria-activedescendant') ? '@' + a.getAttribute('aria-activedescendant') : '') : '',
    selected: [...root.querySelectorAll('[aria-selected="true"], [aria-current="true"], [aria-pressed="true"], [aria-checked="true"], .sel, .selected, .active')].map((e) => (e.id || '') + (e.getAttribute('class') ?? '').replace(/\\s+/g, '.') + calm(e.innerText ?? e.textContent ?? '').slice(0, 40)).join('|'),
    values: [...root.querySelectorAll('input, textarea, select')].map((e) => e.type === 'checkbox' || e.type === 'radio' ? String(e.checked) : e.value).join('|'),
    rows: root.querySelectorAll('[role=option], [role=treeitem], li, tr').length,
    scroll, toast, page: top ? '' : document.body.innerText.length + ':' + document.title,
  };
})()`;
export const snapshotSource = snapshotFor(null);

/** The fields that differ between two snapshots (plus `writes`, the count of stubbed writes). Pure. */
export function changedFields(before, after) {
  return Object.keys(after).filter((k) => before[k] !== after[k]);
}

/** `changed`, `no-change` (a FAIL) or `expected-no-op` (a listed key that may change nothing here) for one key press. Pure. */
export function judgePress(before, after, noop = null, known = null) {
  const changed = changedFields(before, after);
  if (changed.length) return { status: 'changed', changed, ...(known ? { fixed: known } : {}) };
  if (known) return { status: 'known-gap', changed, why: known };
  return noop ? { status: 'expected-no-op', changed, why: noop } : { status: 'no-change', changed };
}

/**
 * A plain character pressed while a text box has the focus is typed into it: the field changing is not that key's action, so the sweep
 * reports it as `typed` (not verified) instead of `changed`. `before` is the snapshot taken before the press. Pure.
 */
export function wasTyped(before, press, changed) {
  const only = press.steps.length === 1 ? press.steps[0] : null;
  // the field's value must have changed: a Vim box that only switched mode (the indicator text) or moved the caret did what the key does
  return !!before.typing && !!only && only.key.length === 1 && !only.ctrl && !only.alt && !only.meta
    && changed.includes('values') && changed.every((f) => ['values', 'text', 'typing', 'active', 'page', 'caret'].includes(f));
}

// ---------- the sweep ----------

const sleepMs = (ms) => new Promise((r) => setTimeout(r, ms));

/** Press one step (`{ key, ctrl?, shift?, alt?, meta? }`) with the settle time the sweep uses. */
const pressStep = async (page, st, settle = 60) => {
  if (st.text !== undefined) { await page.keys(st.text); await sleepMs(settle); return; } // `{ text }`: type text
  await page.key(st.key, { shift: !!st.shift, ctrl: !!st.ctrl, meta: !!st.meta, alt: !!st.alt });
  await sleepMs(settle);
};

/**
 * Open the overlay's view fresh: reload, open the session (unless `cfg.noSession`), seed, then `cfg.open(a, ctx)` or the palette action,
 * then type `typeText`, then press `cfg.enter` (keys that move into a mode of the view). Resolves to the id of the topmost open dialog (''
 * for a pane that is not a dialog), or throws with the reason.
 */
async function openOverlay(a, title, cfg, { lib, reload, sessionTitle }, mode = null) {
  const typeText = mode ? mode.text ?? null : cfg.type ?? null;
  await reload(a.page);
  await sleepMs(400);
  if (!cfg.noSession) await lib.openSessionByTitle(a.page, sessionTitle);
  await cfg.seed?.(a.page);
  if (cfg.open) await cfg.open(a, { lib, sleepMs });
  else {
    const r = await lib.openPaletteAction(a.page, title, { settleMs: 700 });
    if (!r.opened) throw new Error(r.why);
  }
  for (const st of cfg.enter ?? []) await pressStep(a.page, st, 250);
  if (typeText) { await a.page.keys(typeText); await sleepMs(500); }
  for (const st of mode?.enter ?? []) await pressStep(a.page, st, 250);
  const snap = await a.page.eval(snapshotFor(cfg.sel ?? null));
  if (!cfg.pane && !snap.dialogs) throw new Error('no dialog is open after the view was chosen (this overlay is not a dialog: give it `pane: true` and a `sel`)');
  if (cfg.pane && cfg.sel && !snap.present) throw new Error(`nothing matches ${cfg.sel} after the view was chosen`);
  return snap.dialogs;
}

/** Does the app's `?` help list this overlay's keys? Pressed in the open overlay; closes the help again. */
export async function helpListsOverlay(a, name) {
  await a.page.key('g', { ctrl: true, alt: true }); // Ctrl-Alt-G: contextual help from any dialog
  await sleepMs(400);
  // The help opens on Getting started the first time: switch to Contextual, where the overlay's keys are listed.
  const text = await a.page.eval(`(() => { const h = document.getElementById('keys-help'); if (!h?.open) return null; [...h.querySelectorAll('button.seg')].find((b) => b.textContent.trim() === 'Contextual')?.click(); return h.innerText; })()`);
  const contextual = text === null ? null : (await sleepMs(200), await a.page.eval(`document.getElementById('keys-help').innerText`));
  if (text !== null) { await a.page.key('Escape'); await sleepMs(250); }
  return contextual !== null && contextual.toLowerCase().includes(`${name} (overlay keys)`.toLowerCase());
}

/**
 * Sweep one overlay. Resolves to `{ overlay, opened, why?, helpLists, skipped, rows }`; each row is `{ spec, label, action, status, changed }`.
 * `onRow(row)` is called as each key finishes (the CLI appends it to keys.jsonl).
 */
export async function sweepOverlay(a, name, { lib, reload, tables, onRow = () => {}, sessionTitle = 'Fake waiting worker' }) {
  const cfg = SWEEP_OVERLAYS[name];
  if (!cfg) throw new Error(`${name} is not in SWEEP_OVERLAYS (known: ${Object.keys(SWEEP_OVERLAYS).join(', ')})`);
  let title = null;
  if (cfg.action) {
    const { ACTIONS } = await import(new URL('registry.js', new URL('../dist/', import.meta.url)));
    const act = ACTIONS.find((x) => x.id === cfg.action);
    if (!act) throw new Error(`SWEEP_OVERLAYS names ${cfg.action}, which the app no longer registers`);
    title = act.title;
  }
  const snapSrc = snapshotFor(cfg.sel ?? null);
  const modes = cfg.modes ?? [];
  const p = plan(name, tables, cfg.noop ?? {}, modes, cfg.remap ?? {}, cfg.known ?? {});
  const result = { overlay: name, action: cfg.action ?? null, opened: false, helpLists: null, skipped: p.skipped, rows: [] };
  const ctx = { lib, reload, sessionTitle };
  let base;
  try { base = await openOverlay(a, title, cfg, ctx); } catch (e) { result.why = String(e.message ?? e).split('\n')[0]; return result; }
  result.opened = true;
  result.helpLists = await helpListsOverlay(a, name);
  let fresh = true; // the help dialog took the focus: the first key starts from a fresh open
  let openMode = null; // the mode the current open was put into (an index of `modes`), or null
  for (const k of p.presses) {
    const row = { overlay: name, spec: k.spec, label: k.label, action: k.action, ...(k.remappedTo ? { pressed: k.remappedTo } : {}) };
    try {
      const need = k.mode ?? null;
      let now = await a.page.eval(snapSrc);
      if (fresh || now.dialogs !== base || !now.present || need !== openMode) { await openOverlay(a, title, cfg, ctx, need === null ? null : modes[need]); openMode = need; now = await a.page.eval(snapSrc); }
      // see needsRowAbove: step off the first row (ArrowDown), then press the key.
      if (k.offFirst) { await a.page.key('ArrowDown'); await sleepMs(200); now = await a.page.eval(snapSrc); }
      const w0 = a.writes().length;
      const r0 = a.reads.calls.length;
      for (const s of k.steps) await pressStep(a.page, s);
      await sleepMs(300);
      const after = await a.page.eval(snapSrc);
      const writes = a.writes().length - w0;
      // daemon reads count as an effect only for a key the config lists in `reloads`: views poll on their own, so a read alone would hide a dead key
      const reads = (cfg.reloads ?? []).includes(k.label) ? a.reads.calls.length - r0 : 0;
      // the caret counts as a key's effect only for a plain character (a Vim motion): a named key or a chord moves it natively (Up, Home)
      const caretKey = k.steps.every((st) => st.key.length === 1 && !st.ctrl && !st.alt && !st.meta);
      const [was, is] = [now, after].map((snap) => (caretKey ? snap : { ...snap, caret: '' }));
      Object.assign(row, judgePress({ ...was, writes: 0, reads: 0 }, { ...is, writes, reads }, k.noop ?? null, k.known ?? null));
      if (row.status === 'changed' && wasTyped(now, k, row.changed)) row.status = 'typed';
      fresh = true;
    } catch (e) { row.status = 'error'; row.why = String(e.message ?? e).split('\n')[0]; fresh = true; }
    result.rows.push(row);
    onRow(row);
  }
  return result;
}

/** Sweep the named overlays (default all) against the built app; results go to `out/keys.jsonl` as they come. */
export async function runKeySweep({ out, overlays = null, quiet = false }) {
  const lib = await import('./audit-lib.mjs');
  const { reloadPage } = await import('./audit.mjs');
  const { OVERLAY_TABLES } = await import(new URL('overlay-help.generated.js', new URL('../dist/', import.meta.url)));
  const cov = coverage(OVERLAY_TABLES);
  if (cov.missing.length || cov.both.length || cov.stale.length) throw new Error(`SWEEP_OVERLAYS and SKIPPED_OVERLAYS do not match the app's key tables: unclassified ${cov.missing.join(', ') || '-'}; in both ${cov.both.join(', ') || '-'}; not a table ${cov.stale.join(', ') || '-'}`);
  const names = overlays ?? Object.keys(SWEEP_OVERLAYS);
  const unknown = names.filter((n) => !SWEEP_OVERLAYS[n]);
  if (unknown.length) throw new Error(`--overlay names ${unknown.join(', ')}; known: ${Object.keys(SWEEP_OVERLAYS).join(', ')}`);
  const sink = (row) => appendFileSync(join(out, 'keys.jsonl'), `${JSON.stringify(row)}\n`);
  const a = await lib.open({ width: 1280, height: 800, vim: true, stubs: WALK_STUBS, local: KEY_LOCAL, fixtures: keyFixtures(lib.FIXTURES) });
  const results = [];
  try {
    await sleepMs(500);
    for (const name of names) {
      const r = await sweepOverlay(a, name, { lib, reload: reloadPage, tables: OVERLAY_TABLES, onRow: sink });
      sink({ overlay: name, summary: { opened: r.opened, why: r.why ?? null, helpLists: r.helpLists, skipped: r.skipped.length, keys: r.rows.length } });
      results.push(r);
      if (!quiet) console.log(summaryLine(r));
    }
  } finally { await a.close(); }
  return results;
}

/** One line per overlay, then one per key that failed. */
export function summaryLine(r) {
  if (!r.opened) return `${r.overlay}: not opened (${r.why})`;
  const count = (s) => r.rows.filter((x) => x.status === s).length;
  const fails = r.rows.filter((x) => x.status === 'no-change' || x.status === 'error');
  const known = count('known-gap') ? `, ${count('known-gap')} known gap${count('known-gap') === 1 ? '' : 's'} (${[...new Set(r.rows.filter((x) => x.status === 'known-gap').map((x) => x.why))].join(', ')})` : '';
  const fixed = r.rows.filter((x) => x.fixed);
  const typed = count('typed') ? `, ${count('typed')} only typed into a text box (not verified)` : '';
  return [`${r.overlay}: ${r.rows.length} keys, ${count('changed')} changed, ${count('expected-no-op')} expected no-op${typed}${known}, ${fails.length} FAIL${r.helpLists ? '' : ' (the ? help does not list this overlay)'}`,
    ...fixed.map((x) => `    FIXED ${x.label}: it changes something now, so drop it from \`known\` (${x.fixed})`),
    ...fails.map((x) => `    FAIL ${x.label} (${x.spec}: ${x.action})${x.status === 'error' ? ` ${x.why}` : ' changed nothing'}`)].join('\n');
}

/** Failing keys across results: a key that changed nothing and is not listed as an expected no-op, or one that errored, or an overlay that would not open. */
export function failures(results) {
  return results.flatMap((r) => (r.opened ? r.rows.filter((x) => x.status === 'no-change' || x.status === 'error').map((x) => `${r.overlay}: ${x.label}`) : [`${r.overlay}: not opened`]));
}
