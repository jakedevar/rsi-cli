import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import {
  ACTIONS, DEDUPE_MS, Deduper, accelerator, appMenuModel, commandQuery, contextMenuModel, displayHint, isPaletteChord, keyHints,
  menuState, movePick, paletteEntries, parseMenuId, rankPalette, tooltipText, unavailable,
} from '../dist/registry.js';
import { COMMAND_BINDINGS, KEY_BINDINGS } from '../dist/keybindings.js';

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
const idle = () => snap({ open: null, cursorOnRow: false, rowCount: 0, attentionRows: 0 });
const ids = (nodes) => nodes.filter((n) => n.kind === 'item').map((n) => n.id);
const menuIds = (menus) => menus.flatMap((m) => ids(m.items));
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
  assert.equal(tooltipText('hard-pause', 'other'), 'Hard pause (X)\nInterrupt and hold until cleared');
  assert.equal(tooltipText('toggle-tools', 'other'), 'Show tool events');
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
  assert.match(archive.why, /Open a session first/);
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
  for (let i = 1; i < se.length; i++) assert.ok(se[i - 1].tier <= se[i].tier, 'tiers never go back up');
  // Word prefix ("pause" in "Hard pause") before a mid-word substring.
  const pause = rankPalette(entries, 'pause');
  assert.ok(pause.every((r) => r.tier === 2), 'no title starts with pause; each has a word that does');
  assert.deepEqual(pause.map((r) => r.entry.id).slice(0, 2), ['hard-pause', 'clear-pause'], 'earlier match first');
  assert.equal(rankPalette(entries, 'ssion')[0].tier, 3);
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
  assert.deepEqual(rankPalette(entries, 'Hard')[0].hits, [0, 1, 2, 3]);
  assert.deepEqual(rankPalette(entries, '').map((r) => r.entry.id), entries.map((e) => e.id));
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
      for (const n of m.items) {
        if (n.kind !== 'item') continue;
        const a = ACTIONS.find((x) => `act:${x.id}` === n.id);
        assert.equal(n.text, a.title);
        assert.equal(n.enabled, unavailable(a.id, s) === null, n.id);
        assert.equal(n.checked, a.checked ? a.checked(s) : undefined, n.id);
        assert.equal(m.id, a.group === 'help' ? 'help' : a.group, `${n.id} sits in its group's menu`);
      }
    }
    const item = (id) => menus.flatMap((m) => m.items).find((n) => n.id === `act:${id}`);
    assert.equal(item('launch').accelerator, 'CmdOrCtrl+N');
    assert.equal(item('palette').accelerator, 'CmdOrCtrl+K');
    assert.equal(item('archive').accelerator, undefined);
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
