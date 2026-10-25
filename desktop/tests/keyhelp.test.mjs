import { test } from 'node:test';
import assert from 'node:assert/strict';
import { INITIAL_HELP, allRows, contextualRows, filterRows, helpKey } from '../dist/keyhelp-model.js';
import { COMMANDS, NORMAL_KEYS } from '../dist/keymap.generated.js';
import { COMMAND_BINDINGS } from '../dist/keybindings.js';

const snap = (over = {}) => ({
  open: { status: 'Running', readOnly: false, pauseLevel: 'none', hasQuestion: false },
  cursorOnRow: true, rowCount: 3, attentionRows: 0, view: 'active', vimKeys: true, hideTools: false,
  showFinished: false, modalOpen: false, event: null, ...over,
});

test('All Commands lists every TUI key and command with the desktop answer', () => {
  const rows = allRows();
  assert.equal(rows.length, NORMAL_KEYS.length + COMMANDS.length);
  const t = rows.find((r) => r.keys[0] === 'T');
  assert.equal(t.bound, true);
  assert.match(t.detail, /Desktop: Choose theme…\./);
  // Every TUI Normal-mode key is in the desktop except the declared parity gap <Space>hu (#3568, owner #3636,
  // parity-status.d/lifecycle.json); binding it must drop it from this list.
  const unboundKeys = rows.filter((r) => r.section === 'Normal-mode keys' && !r.bound).map((r) => r.keys[0]);
  assert.deepEqual(unboundKeys, ['<Space>hu']);
  const gp = rows.find((r) => r.keys[0] === '<Space>gP');
  assert.equal(gp.bound, true);
  assert.match(gp.detail, /Desktop: Prompt creator\.$/);
  const help = rows.find((r) => r.keys[0] === ':help');
  assert.deepEqual(help.keys, [':help', ':keys']);
  assert.equal(help.bound, !!COMMAND_BINDINGS.help);
});

test('Contextual lists only what can run now, by menu, with keys', () => {
  const idle = contextualRows(snap({ open: null, cursorOnRow: false, rowCount: 0 }), 'other');
  const running = contextualRows(snap(), 'other');
  const titles = (rows) => rows.map((r) => r.title);
  assert.ok(titles(running).includes('Stop this turn'));
  assert.ok(!titles(idle).includes('Stop this turn'));
  const settings = running.find((r) => r.title === 'Settings…');
  assert.equal(settings.section, 'View');
  assert.ok(settings.keys.includes('<Space>,'));
  assert.ok(settings.keys.includes(':set'));
  const mac = contextualRows(snap(), 'mac').find((r) => r.title === 'Command palette…');
  assert.ok(mac.keys.includes('Cmd-K'));
});

test('search needs every term, case-insensitive, over names, details and keys', () => {
  const rows = allRows();
  const hits = filterRows(rows, 'THEME built-in');
  assert.ok(hits.length > 0);
  for (const r of hits) assert.match(`${r.title} ${r.detail} ${r.keys.join(' ')}`.toLowerCase(), /theme/);
  assert.equal(filterRows(rows, '').length, rows.length);
  assert.deepEqual(filterRows(rows, 'zzzz-no-such-thing'), []);
});

test('help keys: scroll, views, search, Esc clears then closes', () => {
  let st = INITIAL_HELP;
  const step = (k, total = 100) => {
    const fx = helpKey(st, k, total);
    if (fx.kind === 'state') st = fx.st;
    return fx;
  };
  step({ key: 'j' });
  assert.equal(st.scroll, 1);
  step({ key: 'd', ctrl: true });
  assert.equal(st.scroll, 16);
  step({ key: 'G' });
  assert.equal(st.scroll, 99);
  step({ key: 'g' });
  assert.equal(st.scroll, 0);
  step({ key: 'k' });
  assert.equal(st.scroll, 0);
  step({ key: 'Tab' });
  assert.equal(st.view, 'all');
  step({ key: 'Tab', shift: true });
  assert.equal(st.view, 'contextual');
  step({ key: '/' });
  for (const c of 'th?s') step({ key: c });
  assert.equal(st.query, 'th?s', '? is filter text while searching');
  step({ key: 'Backspace' });
  step({ key: 'Enter' });
  assert.equal(st.searching, false);
  assert.equal(st.query, 'th?');
  assert.equal(step({ key: 'Escape' }).kind, 'state');
  assert.equal(st.query, '');
  assert.equal(step({ key: 'Escape' }).kind, 'close');
  assert.equal(helpKey(INITIAL_HELP, { key: 'q' }, 5).kind, 'close');
  assert.equal(helpKey(INITIAL_HELP, { key: '?' }, 5).kind, 'close');
});

// ---------- Vim keys off (local #227) ----------

test('plainRows drops Vim-only rows and keys with the switch off and changes nothing with it on', async () => {
  const { plainRows, helpFooter, initialHelp, gettingStartedRows } = await import('../dist/keyhelp-model.js');
  const all = allRows();
  assert.deepEqual(plainRows(all, true), all);
  const off = plainRows(all, false);
  assert.ok(off.length < all.length);
  assert.ok(!off.some((r) => r.section === 'Normal-mode keys' && r.keys.some((k) => /^[a-z]$/.test(k))), 'no single-letter key row');
  for (const r of off) for (const k of r.keys) assert.ok(/^(?:Ctrl|Alt|Shift|Cmd|Option)-|^F\d|^(?:Enter|Esc|Tab|Up|Down|Left|Right|PageUp|PageDown|Home|End)$/.test(k), `${r.title}: ${k}`);
  assert.ok(off.some((r) => r.keys.includes('Ctrl-K')) || off.some((r) => r.keys.some((k) => k.startsWith('Ctrl-'))), 'chords stay');
  // Contextual keeps its actions (they are in Actions… too), minus the keys that do nothing.
  const ctx = contextualRows(snap({ vimKeys: false }), 'other');
  const kept = plainRows(ctx, false, true);
  assert.equal(kept.length, ctx.length);
  assert.deepEqual(kept.find((r) => r.title === 'Next session').keys, []);
  assert.deepEqual(plainRows(gettingStartedRows('other'), false, true).map((r) => r.title), gettingStartedRows('other').map((r) => r.title));
  // The footer follows the switch and the platform.
  assert.match(helpFooter(true, 'other'), /j\/k scroll/);
  assert.doesNotMatch(helpFooter(false, 'other'), /j\/k|g\/G|q closes|\/ search|s symbols/);
  assert.match(helpFooter(false, 'mac'), /Cmd-K opens Actions…/);
  assert.match(helpFooter(false, 'other'), /Ctrl-K opens Actions…/);
  // Getting started opens first with Vim keys off or the first time.
  assert.equal(initialHelp(false, true).view, 'start');
  assert.equal(initialHelp(true, false).view, 'start');
  assert.equal(initialHelp(true, true).view, 'contextual');
});

test('Contextual rows carry the palette words, so "help" finds Keyboard shortcuts', () => {
  const rows = contextualRows(snap({ vimKeys: false }), 'other');
  assert.ok(filterRows(rows, 'help').some((r) => r.title === 'Keyboard shortcuts'));
  assert.ok(!contextualRows(snap({ vimKeys: false }), 'other').some((r) => r.keys.includes('j')));
});
