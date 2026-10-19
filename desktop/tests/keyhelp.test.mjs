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
  // Every TUI Normal-mode key is in the desktop; the last, <Space>gP, since local #88.
  assert.ok(rows.filter((r) => r.section === 'Normal-mode keys').every((r) => r.bound));
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
  assert.ok(titles(running).includes('Interrupt (soft pause)'));
  assert.ok(!titles(idle).includes('Interrupt (soft pause)'));
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
