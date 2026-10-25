import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import {
  ACTIONS, DEDUPE_MS, collapsedCount, Deduper, accelerator, actionSpec, appMenuModel, commandQuery, contextMenuModel, displayHint, isPaletteChord, isSettingsChord, keyHints,
  menuState, movePick, paletteEntries, parseMenuId, rankPalette, tooltipText, unavailable,
  MENU_LAYOUT, MENU_LIMITS, MENU_SCAN_LIMIT, isNewSessionChord, layoutProblems, menuInstallProblem, menuItems,
} from '../dist/registry.js';
import { COMMAND_BINDINGS, KEY_BINDINGS } from '../dist/keybindings.js';
import { ACTION_KEYWORDS } from '../dist/action-keywords.js';
import { isVimOnlyHint } from '../dist/key-hints.js';

const here = (p) => fileURLToPath(new URL(p, import.meta.url));

/** A running, writable open session with rows in the list. */
const snap = (over = {}) => ({
  open: { status: 'Running', readOnly: false, pauseLevel: 'none', hasQuestion: false },
  cursorOnRow: true,
  rowCount: 3,
  attentionRows: 1,
  view: 'active',
  vimKeys: true,
  hideTools: false,
  showFinished: false,
  modalOpen: false,
  event: null,
  ...over,
});
const idle = (over = {}) => snap({ open: null, cursorOnRow: false, rowCount: 0, attentionRows: 0, ...over });
const ids = (nodes) => nodes.filter((n) => n.kind === 'item').map((n) => n.id);
const menuIds = (menus) => menus.flatMap((m) => menuItems(m.items).map((n) => n.id));
const listed = ACTIONS.filter((a) => !a.contextOnly);
const inMenuBar = ACTIONS.filter((a) => !a.contextOnly && !a.noMenu);

// ---------- registry ----------

test('every key and : command the engine binds runs a registered action', () => {
  const known = new Set(ACTIONS.map((a) => a.id));
  for (const [k, b] of Object.entries(KEY_BINDINGS)) assert.ok(known.has(b.action), `${k} -> ${b.action}`);
  for (const [c, b] of Object.entries(COMMAND_BINDINGS)) assert.ok(known.has(b.action), `:${c} -> ${b.action}`);
});

test('registry ids are unique and titles are menu-safe', () => {
  assert.equal(new Set(ACTIONS.map((a) => a.id)).size, ACTIONS.length);
  for (const a of ACTIONS) {
    assert.match(a.id, /^[a-z-]+$/);
    assert.ok(a.title.trim().length > 0 && a.title.length <= 96, a.id);
    assert.ok(!a.title.includes('&'), `${a.id}: & is a mnemonic marker in native menus`);
    assert.ok(['file', 'session', 'go', 'issues', 'view', 'help'].includes(a.group), a.id);
  }
});

test('key hints come from the TUI keymap plus desktop-only keys', () => {
  assert.deepEqual(keyHints('archive'), ['<Space>a', ':archive']);
  assert.deepEqual(keyHints('cursor-first'), ['gg']);
  assert.deepEqual(keyHints('palette'), ['Ctrl-K', '<Space>;', ':commands']);
  assert.deepEqual(keyHints('help'), ['?', 'Ctrl-Alt-G', ':help']);
  assert.equal(actionSpec('help').detail, 'Lists every action and its keys; type to search');
  assert.deepEqual(keyHints('quit'), ['ZQ', 'ZZ', ':quit']);
  assert.deepEqual(keyHints('launch'), ['Ctrl-N', '<Space>N', ':blank']);
  assert.deepEqual(keyHints('toggle-tools'), []);
  assert.equal(displayHint('Ctrl-N', 'mac'), 'Cmd-N');
  assert.equal(displayHint('Ctrl-N', 'other'), 'Ctrl-N');
  assert.equal(accelerator('launch'), 'CmdOrCtrl+N');
  assert.equal(accelerator('palette'), 'CmdOrCtrl+K');
  assert.equal(accelerator('help'), undefined, 'Ctrl-Alt chords stay off native accelerators');
});

test('tooltips show the action title and its keys', () => {
  assert.equal(tooltipText('archive', 'other'), 'Archive session… (<Space>a, :archive)');
  assert.equal(tooltipText('launch', 'mac').split('\n')[0], 'New session… (Cmd-N, <Space>N, :blank)');
  assert.equal(tooltipText('hard-pause', 'other').split('\n')[0], 'Pause until I resume (X)');
  assert.equal(tooltipText('toggle-tools', 'other'), 'Show tool calls');
});

test('every toolbar button with data-action names a registered action', () => {
  const html = readFileSync(here('../static/index.html'), 'utf8');
  const used = [...html.matchAll(/data-action="([^"]+)"/g)].map((m) => m[1]);
  assert.ok(used.length >= 9);
  const known = new Set(ACTIONS.map((a) => a.id));
  for (const id of used) assert.ok(known.has(id), id);
  for (const id of ['launch', 'palette', 'soft-interrupt', 'hard-pause', 'clear-pause', 'archive']) assert.ok(used.includes(id), id);
});

// ---------- availability ----------

test('session actions need an open session and say why', () => {
  for (const id of ['soft-interrupt', 'hard-pause', 'clear-pause', 'archive', 'quick-continue', 'focus-composer', 'answer-question', 'copy-session-id']) {
    assert.match(unavailable(id, idle()), /Open a session first/, id);
  }
  const s = snap();
  assert.equal(unavailable('soft-interrupt', s), null);
  assert.equal(unavailable('hard-pause', s), null);
  assert.match(unavailable('clear-pause', s), /not paused/);
  assert.equal(unavailable('clear-pause', snap({ open: { ...s.open, pauseLevel: 'hard' } })), null);
  assert.match(unavailable('answer-question', s), /no question/);
  assert.equal(unavailable('answer-question', snap({ open: { ...s.open, hasQuestion: true } })), null);
  const done = snap({ open: { ...s.open, status: 'Completed' } });
  assert.match(unavailable('soft-interrupt', done), /not running/);
  assert.match(unavailable('hard-pause', done), /not running/);
  const ro = snap({ view: 'archived', open: { ...s.open, status: 'Archived', readOnly: true } });
  assert.match(unavailable('focus-composer', ro), /read-only/);
  assert.match(unavailable('archive', ro), /already archived/);
  assert.match(unavailable('toggle-finished', ro), /Archived view/);
});

test('list motions, the : line and dialogs gate the rest', () => {
  assert.match(unavailable('cursor-down', idle()), /No sessions/);
  assert.match(unavailable('open', idle()), /No session under the cursor/);
  assert.match(unavailable('next-attention', snap({ attentionRows: 0 })), /waiting for you/);
  assert.match(unavailable('command-line', snap({ vimKeys: false })), /Vim keys/);
  const modal = snap({ modalOpen: true });
  assert.match(unavailable('launch', modal), /dialog/);
  assert.equal(unavailable('quit', modal), null);
  assert.equal(unavailable('palette', idle()), null);
  assert.match(unavailable('copy-event', snap()), /no text/);
  assert.equal(unavailable('copy-event', snap({ event: { hasText: true, foldable: false } })), null);
  assert.match(unavailable('toggle-event', snap({ event: { hasText: true, foldable: false } })), /nothing to fold/);
});

// ---------- palette ----------

test('every registered action appears in the palette with its binding and disabled state', () => {
  const entries = paletteEntries(idle(), 'other');
  assert.deepEqual(entries.map((e) => e.id), listed.map((a) => a.id));
  for (const e of entries) assert.deepEqual(e.keys, keyHints(e.id), e.id);
  for (const k of Object.keys(KEY_BINDINGS)) {
    // The Issues workspace's own keys (j, a, ...) only apply while it is open.
    const global = entries.filter((e) => e.keys.includes(k) && (ACTIONS.find((a) => a.id === e.id).group !== 'issues' || KEY_BINDINGS[k].action === e.id));
    assert.equal(global.length, 1, `key ${k} shows on one palette row`);
  }
  for (const c of Object.keys(COMMAND_BINDINGS)) {
    assert.equal(entries.filter((e) => e.keys.includes(`:${c}`)).length, 1, `:${c} shows on one palette row`);
  }
  const archive = entries.find((e) => e.id === 'archive');
  assert.equal(archive.enabled, false);
  assert.match(archive.why, /^Open a session first: click one in the list/);
  assert.equal(entries.find((e) => e.id === 'launch').enabled, true);
  assert.deepEqual(paletteEntries(idle(), 'mac').find((e) => e.id === 'palette').keys, ['Cmd-K', '<Space>;', ':commands']);
  assert.ok(!entries.some((e) => e.id === 'copy-event'), 'right-click-only actions stay out of the palette');
});

test('fuzzy search ranks prefix matches first', () => {
  const entries = paletteEntries(snap(), 'other');
  const top = (q, n = 3) => rankPalette(entries, q).slice(0, n).map((r) => r.entry.id);
  // Title prefixes beat word prefixes and substrings.
  assert.deepEqual(top('arch', 2), ['archive', 'view-archived']);
  const se = rankPalette(entries, 'se');
  assert.equal(se[0].tier, 0);
  assert.ok(se.filter((r) => r.tier === 0).some((r) => r.entry.id === 'quick-continue'), '"Send …" starts with se');
  assert.ok(se.findIndex((r) => r.entry.id === 'cursor-down') > 0, '"Next session" only has a word starting with se');
  // Enabled actions come first (local #227); inside each group the tiers never go back up.
  for (const on of [true, false]) {
    const g = se.filter((r) => r.entry.enabled === on);
    for (let i = 1; i < g.length; i++) assert.ok(g[i - 1].tier <= g[i].tier, 'tiers never go back up');
  }
  // A title prefix ("Pause until I resume") ranks first for the plain word.
  const pause = rankPalette(entries, 'pause');
  assert.equal(pause[0].entry.id, 'hard-pause');
  assert.equal(pause[0].tier, 0);
  assert.equal(rankPalette(entries, 'ssion')[0].tier, 5);
  // An exact key, then a key prefix.
  assert.equal(top('gg', 1)[0], 'cursor-first');
  assert.equal(top('x', 1)[0], 'soft-interrupt');
  assert.equal(top('X', 1)[0], 'hard-pause', 'keys match case-sensitively');
  assert.deepEqual(top(':arc', 2).sort(), ['archive', 'view-archived']);
  // Letters in order.
  assert.equal(top('nxs', 1)[0], 'cursor-down');
  // Several words each prefixing a word.
  assert.equal(top('sess id', 1)[0], 'copy-session-id');
  // Highlights mark the matched title letters.
  assert.deepEqual(rankPalette(entries, 'Pause')[0].hits, [0, 1, 2, 3, 4]);
  // An empty query lists what can run now in registry order, then the rest (big context-only families are collapsed).
  const empty = rankPalette(entries, '').map((r) => r.entry.id);
  assert.deepEqual(empty.slice(0, entries.filter((e) => e.enabled).length), entries.filter((e) => e.enabled).map((e) => e.id));
  assert.deepEqual(rankPalette(entries, 'zzzzqqq'), []);
});

test('a palette query starting with : is a TUI command line', () => {
  assert.equal(commandQuery(':archive'), 'archive');
  assert.equal(commandQuery('  : continue fix it '), 'continue fix it');
  assert.equal(commandQuery(':'), null);
  assert.equal(commandQuery('archive'), null);
});

test('the palette highlight wraps', () => {
  assert.equal(movePick(5, 4, 1), 0);
  assert.equal(movePick(5, 0, -1), 4);
  assert.equal(movePick(5, -1, 1), 0);
  assert.equal(movePick(5, -1, -1), 4);
  assert.equal(movePick(0, 0, 1), -1);
});

test('Ctrl-K opens the palette; Cmd-K on macOS, even from a text field', () => {
  assert.ok(isPaletteChord({ key: 'k', ctrlKey: true }, 'other', false));
  assert.ok(isPaletteChord({ key: 'k', ctrlKey: true }, 'other', true));
  assert.ok(!isPaletteChord({ key: 'k', metaKey: true }, 'other', false), 'Super-K is the desktop\'s');
  assert.ok(isPaletteChord({ key: 'k', metaKey: true }, 'mac', true));
  assert.ok(isPaletteChord({ key: 'k', ctrlKey: true }, 'mac', false));
  assert.ok(!isPaletteChord({ key: 'k', ctrlKey: true }, 'mac', true), 'Ctrl-K is kill-line in macOS text fields');
  assert.ok(!isPaletteChord({ key: 'K', ctrlKey: true, shiftKey: true }, 'other', false));
  assert.ok(!isPaletteChord({ key: 'k' }, 'other', false));
  assert.ok(isPaletteChord({ key: '˚', code: 'KeyK', metaKey: true }, 'mac', false) === true);
});

test('the Settings chord is Cmd-, on macOS and Ctrl-, elsewhere, and nothing shifted or with Alt (local #269)', () => {
  assert.ok(isSettingsChord({ key: ',', metaKey: true }, 'mac'));
  assert.ok(isSettingsChord({ key: ',', ctrlKey: true }, 'mac'), 'the physical Ctrl key works on a Mac too, as in the key engine');
  assert.ok(!isSettingsChord({ key: ',', ctrlKey: true, metaKey: true }, 'mac'));
  assert.ok(isSettingsChord({ key: ',', ctrlKey: true }, 'other'));
  assert.ok(!isSettingsChord({ key: ',', metaKey: true }, 'other'), 'the Windows key is never ours');
  assert.ok(isSettingsChord({ key: '≤', code: 'Comma', metaKey: true, altKey: false }, 'mac') === true, 'a layout that types another character still has the comma key');
  assert.ok(!isSettingsChord({ key: ',', ctrlKey: true, altKey: true }, 'other'));
  assert.ok(!isSettingsChord({ key: ',', ctrlKey: true, shiftKey: true }, 'other'));
  assert.ok(!isSettingsChord({ key: ',', isComposing: true, ctrlKey: true }, 'other'));
  assert.ok(!isSettingsChord({ key: ',' }, 'other'), 'a plain comma is typed text');
  assert.equal(accelerator('settings-open'), 'CmdOrCtrl+,');
  assert.equal(displayHint('Ctrl-,', 'mac'), 'Cmd-,');
  assert.match(tooltipText('settings-open', 'mac'), /Settings… \(Cmd-,/);
  assert.match(tooltipText('settings-open', 'other'), /Settings… \(Ctrl-,/);
});

// ---------- menus ----------

for (const platform of ['mac', 'other']) {
  test(`the ${platform} menu bar is built from the registry`, () => {
    const s = snap();
    const menus = appMenuModel(platform, s);
    const all = menuIds(menus);
    assert.equal(new Set(all).size, all.length, 'item ids are unique');
    const expected = inMenuBar.filter((a) => !(platform === 'mac' && a.id === 'quit')).map((a) => `act:${a.id}`);
    assert.deepEqual([...all].sort(), [...expected].sort(), 'every menu-bar action exactly once');
    const titles = menus.map((m) => m.text);
    for (const t of ['File', 'Session', 'View', 'Help']) assert.ok(titles.includes(t), t);
    for (const m of menus) {
      assert.ok(m.items.length > 0, `${m.id} is not empty`);
      assert.notEqual(m.items[0].kind, 'separator', `${m.id} starts with an item`);
      assert.notEqual(m.items.at(-1).kind, 'separator', `${m.id} ends with an item`);
      m.items.forEach((n, i) => assert.ok(!(n.kind === 'separator' && m.items[i + 1]?.kind === 'separator'), `${m.id}: double separator`));
      for (const n of menuItems(m.items)) {
        const a = ACTIONS.find((x) => `act:${x.id}` === n.id);
        assert.equal(n.text, a.title);
        assert.equal(n.enabled, unavailable(a.id, s) === null, n.id);
        assert.equal(n.checked, a.checked ? a.checked(s) : undefined, n.id);
        // Settings… sits in the macOS app menu (Cmd-,), not in View (local #269).
        const home = a.id === 'settings-open' && platform === 'mac' ? 'app' : a.group;
        assert.equal(m.id, home, `${n.id} sits in its group's menu`);
      }
    }
    const item = (id) => menus.flatMap((m) => menuItems(m.items)).find((n) => n.id === `act:${id}`);
    assert.equal(item('launch').accelerator, 'CmdOrCtrl+N');
    assert.equal(item('palette').accelerator, 'CmdOrCtrl+K');
    assert.equal(item('archive').accelerator, undefined);
    // Settings… is Cmd-, / Ctrl-, (local #269): in the app menu on macOS, in View elsewhere, once either way.
    assert.equal(item('settings-open').accelerator, 'CmdOrCtrl+,');
    assert.equal(menus.filter((m) => menuItems(m.items).some((n) => n.id === 'act:settings-open')).map((m) => m.id).join(), platform === 'mac' ? 'app' : 'view');
    assert.equal(item('view-active').checked, true);
    assert.equal(item('view-archived').checked, false);
    assert.equal(item('clear-pause').enabled, false);
    if (platform === 'mac') {
      assert.equal(menus[0].id, 'app');
      assert.ok(menus[0].items.some((n) => n.kind === 'predefined' && n.item === 'quit'));
      const edit = menus.find((m) => m.id === 'edit');
      assert.deepEqual(edit.items.filter((n) => n.kind === 'predefined').map((n) => n.item), ['undo', 'redo', 'cut', 'copy', 'paste', 'select_all']);
      assert.ok(menus.some((m) => m.id === 'window'));
    } else {
      assert.equal(menus[0].id, 'file');
      assert.ok(menus.every((m) => m.items.every((n) => n.kind !== 'predefined')), 'no OS items outside macOS');
      assert.equal(ids(menus[0].items).at(-1), 'act:quit');
    }
  });
}

test('menu state follows the snapshot, keyed by item id', () => {
  const st = menuState(appMenuModel('other', idle()));
  assert.deepEqual(st['act:archive'], { enabled: false });
  assert.deepEqual(st['act:launch'], { enabled: true });
  assert.deepEqual(st['act:toggle-vim-keys'], { enabled: true, checked: true });
  const open = menuState(appMenuModel('other', snap({ hideTools: true })));
  assert.deepEqual(open['act:archive'], { enabled: true });
  assert.deepEqual(open['act:toggle-tools'], { enabled: true, checked: false });
});

test('context menus are built from the registry for session rows and events', () => {
  const s = snap();
  const row = contextMenuModel('session-row', s);
  const rowIds = ACTIONS.filter((a) => a.contexts?.includes('session-row')).map((a) => `ctx:${a.id}`);
  assert.deepEqual(ids(row), rowIds);
  assert.ok(ids(row).includes('ctx:archive') && ids(row).includes('ctx:copy-session-id'));
  assert.equal(row.find((n) => n.id === 'ctx:clear-pause').enabled, false);
  assert.equal(row.find((n) => n.id === 'ctx:archive').enabled, true);
  assert.ok(row.every((n) => n.kind !== 'item' || n.accelerator === undefined));
  const ev = contextMenuModel('event', snap({ event: { hasText: true, foldable: true } }));
  assert.deepEqual(ids(ev), ['ctx:copy-event', 'ctx:toggle-event', 'ctx:focus-composer', 'ctx:toggle-tools']);
  assert.equal(ev[0].enabled, true);
  assert.equal(ev.find((n) => n.id === 'ctx:toggle-tools').checked, true);
  for (const menu of [row, ev]) {
    assert.notEqual(menu[0].kind, 'separator');
    assert.notEqual(menu.at(-1).kind, 'separator');
  }
});

test('menu events map back to registry actions only', () => {
  assert.deepEqual(parseMenuId('act:archive'), { action: 'archive', source: 'menu' });
  assert.deepEqual(parseMenuId('ctx:copy-event'), { action: 'copy-event', source: 'context' });
  assert.equal(parseMenuId('act:format-disk'), null);
  assert.equal(parseMenuId('archive'), null);
  assert.equal(parseMenuId('xyz:archive'), null);
});

test('one keystroke reported by both the menu accelerator and keydown runs once', () => {
  const d = new Deduper();
  assert.equal(d.admit('launch', 'key', 1000), true);
  assert.equal(d.admit('launch', 'menu', 1000 + DEDUPE_MS - 1), false);
  assert.equal(d.admit('launch', 'menu', 5000), true);
  assert.equal(d.admit('launch', 'key', 5100), false, 'either order');
  assert.equal(d.admit('launch', 'key', 5200), true, 'a real second press runs');
  assert.equal(d.admit('cursor-down', 'key', 6000), true);
  assert.equal(d.admit('cursor-down', 'key', 6010), true, 'key repeat is not a duplicate');
  assert.equal(d.admit('palette', 'palette', 7000), true);
  assert.equal(d.admit('palette', 'menu', 7010), true, 'only key/menu pairs collapse');
});

test('the Rust menu builder knows every OS item the registry uses', () => {
  const rust = readFileSync(here('../src-tauri/src/menu.rs'), 'utf8');
  const body = rust.match(/pub enum Predefined \{([^}]*)\}/)[1];
  const names = new Set(body.split(',').map((v) => v.trim()).filter(Boolean)
    .map((v) => v.replace(/[A-Z]/g, (c, i) => (i ? '_' : '') + c.toLowerCase())));
  for (const p of ['mac', 'other']) {
    for (const m of appMenuModel(p, snap())) {
      for (const n of m.items) if (n.kind === 'predefined') assert.ok(names.has(n.item), n.item);
    }
  }
});

// ---------- palette search for everyday words, Vim keys off (local #227) ----------

const firstIds = (q, s, n = 3) => rankPalette(paletteEntries(s, 'other'), q).slice(0, n).map((r) => r.entry);
const off = (over = {}) => snap({ vimKeys: false, ...over });

test('everyday words find an enabled opener first: help, files, todo, dag, stop', () => {
  const files = { hasRoot: true, explorer: false, viewer: false, picker: false, showHidden: false, followPreview: false, target: false, editing: false };
  for (const s of [idle({ files }), off({ files }), snap({ files })]) {
    const entries = paletteEntries(s, 'other');
    const ranked = (q) => rankPalette(entries, q).map((r) => r.entry);
    assert.ok(['help', 'cm-manual'].includes(ranked('help')[0].id), '"help" finds the shortcuts and the manual');
    assert.ok(ranked('help').slice(0, 2).some((e) => e.id === 'help'));
    assert.equal(ranked('files')[0].id, 'files-explorer', '"files" opens the file explorer, not "Show hidden files"');
    assert.ok(ranked('files').slice(0, 3).some((e) => e.id === 'files-find'));
    assert.equal(ranked('todo')[0].id, 'todo-toggle', '"todo" opens the panel, not "To-do: dismiss…"');
    assert.equal(ranked('dag')[0].id, 'dag-open', '"dag" opens the browser, not "DAG: reload graph"');
    assert.equal(ranked('dag')[0].enabled, true);
    for (const q of ['help', 'files', 'todo', 'dag']) assert.equal(ranked(q)[0].enabled, true, `${q}: the preselected row can run`);
  }
  const stop = firstIds('stop', snap());
  assert.ok(stop.some((e) => e.id === 'soft-interrupt') && stop.some((e) => e.id === 'emergency-stop'));
});

test('an enabled action ranks above a disabled one that matches better, except an exact key', () => {
  const entries = paletteEntries(idle(), 'other');
  const r = rankPalette(entries, 'dag');
  const firstOff = r.findIndex((x) => !x.entry.enabled);
  assert.ok(firstOff < 0 || r.slice(firstOff).every((x) => !x.entry.enabled), 'no enabled row after a disabled one');
  assert.equal(rankPalette(paletteEntries(snap(), 'other'), 'gg')[0].entry.id, 'cursor-first', 'Vim-on exact keys are unchanged');
  assert.equal(rankPalette(paletteEntries(snap(), 'other'), ':archive')[0].entry.id, 'archive');
});

test('keywords: a plural finds the singular word; every keyword names a real action', () => {
  const entries = paletteEntries(idle(), 'other');
  assert.equal(rankPalette(entries, 'todos')[0].entry.id, 'todo-toggle');
  assert.equal(rankPalette(entries, 'shortcuts')[0].entry.id, 'help');
  const known = new Set(ACTIONS.map((a) => a.id));
  for (const id of Object.keys(ACTION_KEYWORDS)) assert.ok(known.has(id), `${id} is a registered action`);
});

test('an empty query collapses the context-only families behind their opener', () => {
  const entries = paletteEntries(idle(), 'other');
  const all = rankPalette(entries, '');
  const hidden = collapsedCount(entries);
  assert.ok(hidden >= 30, `the Issues family is hidden (${hidden})`);
  assert.equal(all.length, entries.length - hidden);
  assert.ok(all.some((r) => r.entry.id === 'issues-open'), 'the opener stays');
  assert.ok(!all.some((r) => r.entry.id === 'issues-down'), 'a family member waits behind it');
  assert.ok(rankPalette(entries, 'next issue').some((r) => r.entry.id === 'issues-down'), 'typing finds it again');
  assert.equal(all[0].entry.enabled, true);
});

test('Vim keys off: the palette shows only keys that still work and no Vim key in a reason', () => {
  const entries = paletteEntries(off(), 'other');
  for (const e of entries) {
    for (const k of e.keys) assert.ok(!isVimOnlyHint(k), `${e.id} still shows ${k}`);
    for (const text of [e.why, e.detail].filter(Boolean)) {
      assert.ok(!/<Space>/.test(text), `${e.id}: ${text}`);
      assert.ok(!/\((?:[A-Za-z]|gg|DD|yy|\/)\)|\(:/.test(text), `${e.id}: ${text}`);
    }
  }
  const byId = (id) => entries.find((e) => e.id === id);
  assert.deepEqual(byId('palette').keys, ['Ctrl-K']);
  assert.deepEqual(byId('cursor-down').keys, []);
  assert.deepEqual(byId('launch').keys, ['Ctrl-N']);
  assert.match(byId('issues-down').why, /^Open the Issues workspace first/);
  // The same entry with the switch on keeps every key.
  assert.ok(paletteEntries(snap(), 'other').find((e) => e.id === 'cursor-down').keys.includes('j'));
  assert.ok(!/<Space>/.test(paletteEntries(off(), 'other').map((e) => e.why ?? '').join('\n')));
});

test('"Appoint a portfolio manager…" is in the palette and found by portfolio and manager', () => {
  const spec = actionSpec('mgr-portfolio-appoint');
  assert.equal(spec.title, 'Appoint a portfolio manager…');
  for (const q of ['portfolio', 'manager', 'portfolio manager', 'appoint a']) {
    const hit = rankPalette(paletteEntries(snap(), 'other'), q).slice(0, q === 'manager' ? 40 : 5).map((r) => r.entry.id);
    assert.ok(hit.includes('mgr-portfolio-appoint'), `"${q}" finds it (got ${hit})`);
  }
  assert.match(unavailable('mgr-portfolio-appoint', snap({ managers: { view: null, hasProject: true, seatCandidate: false } })), /Select the session/);
  assert.equal(unavailable('mgr-portfolio-appoint', snap({ managers: { view: null, hasProject: true, seatCandidate: true } })), null);
});

// ---------- the menu bar always installs (#305) ----------

const rustMenu = readFileSync(fileURLToPath(new URL('../src-tauri/src/menu.rs', import.meta.url)), 'utf8');
const rustConst = (name) => Number(rustMenu.match(new RegExp(`const ${name}: usize = (\\d+);`))[1]);

test('MENU_LIMITS is the cap menu.rs enforces', () => {
  assert.deepEqual(MENU_LIMITS, {
    submenus: rustConst('MAX_SUBMENUS'), items: rustConst('MAX_ITEMS'), text: rustConst('MAX_TEXT'),
    actionId: rustConst('MAX_ACTION_ID'), depth: rustConst('MAX_DEPTH'),
  });
  assert.ok(MENU_SCAN_LIMIT < MENU_LIMITS.items, 'the scan limit is under the hard cap');
});

/** Every problem validate_app_menu (menu.rs) would find in this bar, plus levels a person cannot scan. */
function barProblems(menus) {
  const problems = [];
  const seen = new Set();
  const text = (where, t) => {
    if (t.trim() === '' || [...t].length > MENU_LIMITS.text || /[\p{Cc}]/u.test(t) || t.includes('&')) problems.push(`${where}: bad text ${JSON.stringify(t)}`);
  };
  const level = (where, nodes, depth) => {
    if (nodes.length === 0) problems.push(`${where}: empty`);
    if (nodes.length > MENU_LIMITS.items) problems.push(`${where}: ${nodes.length} items, over the Rust cap ${MENU_LIMITS.items}`);
    if (nodes.length > MENU_SCAN_LIMIT) problems.push(`${where}: ${nodes.length} entries, over the scan limit ${MENU_SCAN_LIMIT}`);
    if (nodes[0]?.kind === 'separator' || nodes.at(-1)?.kind === 'separator') problems.push(`${where}: starts or ends with a separator`);
    nodes.forEach((n, i) => { if (n.kind === 'separator' && nodes[i + 1]?.kind === 'separator') problems.push(`${where}: double separator`); });
    for (const n of nodes) {
      if (n.kind === 'item') {
        const m = /^act:([a-z0-9-]+)$/.exec(n.id);
        if (!m || m[1].length > MENU_LIMITS.actionId) problems.push(`${where}: bad id ${n.id}`);
        if (seen.has(n.id)) problems.push(`${n.id} appears twice`);
        seen.add(n.id);
        text(n.id, n.text);
        if (n.accelerator !== undefined && !/^CmdOrCtrl\+(Shift\+)?[A-Z0-9,]$/.test(n.accelerator)) problems.push(`${n.id}: accelerator ${n.accelerator}`);
      } else if (n.kind === 'submenu') {
        if (depth + 1 > MENU_LIMITS.depth) problems.push(`${where} > ${n.text}: nested deeper than ${MENU_LIMITS.depth}`);
        text(`${where} > ${n.text}`, n.text);
        level(`${where} > ${n.text}`, n.items, depth + 1);
      }
    }
  };
  if (menus.length === 0 || menus.length > MENU_LIMITS.submenus) problems.push(`${menus.length} menus in the bar`);
  const subIds = new Set();
  for (const m of menus) {
    if (!/^[a-z]{1,16}$/.test(m.id) || subIds.has(m.id)) problems.push(`menu id ${m.id}`);
    subIds.add(m.id);
    text(m.id, m.text);
    level(m.text, m.items, 1);
  }
  return problems;
}

test('every generated menu bar fits every cap menu.rs enforces and the scan limit, on macOS and elsewhere', () => {
  const states = [
    idle(), snap(), snap({ vimKeys: false }), snap({ modalOpen: true }), snap({ view: 'archived', hideTools: true, showFinished: true }),
  ];
  for (const p of ['mac', 'other']) {
    for (const s of states) assert.deepEqual(barProblems(appMenuModel(p, s)), [], `${p} ${JSON.stringify(s.open)}`);
  }
  // The check itself catches the #305 bar: one flat View menu of every View action.
  const flat = appMenuModel('other', snap()).map((m) => ({ ...m, items: menuItems(m.items) }));
  assert.ok(barProblems(flat).some((x) => /View: \d+ items, over the Rust cap/.test(x)), 'a flat View breaks the cap');
});

test('the menu layout names real menu actions of its own group, each once, and every one of them', () => {
  assert.deepEqual(layoutProblems(), []);
  for (const [group, entries] of Object.entries(MENU_LAYOUT)) {
    const named = new Set(entries.flatMap((e) => (typeof e === 'object' ? e.items : [e])));
    const missing = inMenuBar.filter((a) => a.group === group && !named.has(a.id)).map((a) => a.id);
    assert.deepEqual(missing, [], `${group}: actions the layout does not place (they would fall to the end of the menu)`);
  }
  assert.deepEqual(layoutProblems({ view: ['launch', 'no-such-action', 'cm-cold-archive', 'palette', { sub: 'X', items: ['palette'] }] }), [
    'view menu names launch, which is in the file group',
    'view menu names no-such-action, which is no action',
    'view menu names cm-cold-archive, which is not a menu action',
    'palette is placed twice in the menu layout',
  ]);
});

test('the macOS bar keeps the standard menus: app (About, Settings Cmd-, Hide, Quit), Edit for text fields, Window', () => {
  const menus = appMenuModel('mac', snap());
  assert.deepEqual(menus.map((m) => m.text), ['RSI Desktop', 'File', 'Edit', 'Session', 'Go', 'Issues', 'View', 'Window', 'Help']);
  const pre = (id) => menus.find((m) => m.id === id).items.filter((n) => n.kind === 'predefined').map((n) => n.item);
  assert.deepEqual(pre('app'), ['about', 'services', 'hide', 'hide_others', 'show_all', 'quit']);
  assert.equal(menuItems(menus[0].items)[0].accelerator, 'CmdOrCtrl+,');
  assert.deepEqual(pre('edit'), ['undo', 'redo', 'cut', 'copy', 'paste', 'select_all']);
  assert.deepEqual(pre('window'), ['minimize', 'maximize', 'close_window']);
  assert.deepEqual(appMenuModel('other', snap()).map((m) => m.text), ['File', 'Session', 'Go', 'Issues', 'View', 'Help']);
});

test('nested submenus read as the TUI help groups them, and an accelerator inside one still shows', () => {
  const view = appMenuModel('mac', snap()).find((m) => m.id === 'view');
  const sub = (name) => view.items.find((n) => n.kind === 'submenu' && n.text === name);
  assert.ok(menuItems(sub('Files').items).some((n) => n.id === 'act:files-save' && n.accelerator === 'CmdOrCtrl+S'));
  assert.ok(menuItems(sub('Folds').items).every((n) => n.id.startsWith('act:fold-')));
  assert.ok(menuItems(sub('Managers').items).every((n) => n.id.startsWith('act:mgr-')));
  const session = appMenuModel('other', snap()).find((m) => m.id === 'session');
  const land = session.items.find((n) => n.kind === 'submenu' && n.text === 'Branch and context');
  assert.ok(menuItems(land.items).some((n) => n.id === 'act:land-session' && n.text === 'Land session branch…'));
  // State reaches nested items too.
  assert.equal(menuState(appMenuModel('other', snap({ hideTools: true })))['act:toggle-tools'].checked, false);
  assert.ok('act:files-save' in menuState([view]));
});

test('off macOS a menu never takes a text field\'s own Ctrl chord (Ctrl-Y redo); macOS keeps Cmd-Y', () => {
  const accel = (p, id) => menuItems(appMenuModel(p, snap()).flatMap((m) => m.items)).find((n) => n.id === `act:${id}`).accelerator;
  assert.equal(accel('mac', 'compile-prompt'), 'CmdOrCtrl+Y');
  assert.equal(accel('other', 'compile-prompt'), undefined);
  assert.equal(accel('other', 'launch'), 'CmdOrCtrl+N');
});

test('Cmd-N (Ctrl-N off macOS) is the New session chord, with or without a text field', () => {
  assert.ok(isNewSessionChord({ key: 'n', metaKey: true }, 'mac'));
  assert.ok(isNewSessionChord({ key: 'N', code: 'KeyN', metaKey: true }, 'mac'), 'Caps Lock');
  assert.ok(!isNewSessionChord({ key: 'n', ctrlKey: true }, 'mac'), 'physical Ctrl-N stays the text field\'s next line');
  assert.ok(!isNewSessionChord({ key: 'n', metaKey: true, shiftKey: true }, 'mac'));
  assert.ok(!isNewSessionChord({ key: 'n', metaKey: true, altKey: true }, 'mac'));
  assert.ok(!isNewSessionChord({ key: 'n', metaKey: true, isComposing: true }, 'mac'));
  assert.ok(isNewSessionChord({ key: 'n', ctrlKey: true }, 'other'));
  assert.ok(!isNewSessionChord({ key: 'n', metaKey: true }, 'other'), 'Super-N is the desktop\'s');
  assert.ok(!isNewSessionChord({ key: 'm', ctrlKey: true }, 'other'));
  // The chord the menu shows is the one the webview takes.
  assert.equal(accelerator('launch'), 'CmdOrCtrl+N');
  assert.equal(displayHint('Ctrl-N', 'mac'), 'Cmd-N');
});

test('a menu bar that did not fully install is said in plain words, naming the menu and the palette', () => {
  const menus = appMenuModel('mac', snap());
  assert.equal(menuInstallProblem(menus, { installed: ['app', 'file'], skipped: [] }, 'mac'), null);
  assert.equal(
    menuInstallProblem(menus, { installed: ['app'], skipped: [{ id: 'view', reason: 'menu has more than 64 items' }] }, 'mac'),
    'Menu bar installed without View (menu has more than 64 items). Those actions are still in the command palette (Cmd-K).',
  );
  assert.equal(
    menuInstallProblem(menus, null, 'other', new Error('no menu can be installed')),
    'Menu bar not installed: no menu can be installed. Every action is still in the command palette (Ctrl-K).',
  );
});
