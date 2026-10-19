// The keybindings help's overlay tables and Symbols view (gap sweep G4, hub #3000).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { isFresh, parseOverlayTables } from '../scripts/overlay-help.mjs';
import { HELP_VIEWS, INITIAL_HELP, SYMBOLS, allOverlayRows, helpKey, overlayRows, symbolRows } from '../dist/keyhelp-model.js';

test('src/overlay-help.generated.ts matches docs/keybindings.md (run `node scripts/overlay-help.mjs` if not)', () => {
  assert.equal(isFresh(), true);
});

test('the parser reads each overlay heading and its Keys / Action / What it does columns', () => {
  const md = '## Overlay Key Reference (generated)\n\n### Foo\n\n| Keys | Action |\n| --- | --- |\n| `j / k` | Move |\n\n### Bar\n\n| Keys | Action | What it does |\n| --- | --- | --- |\n| `Esc` | Close | Closes it. |\n\n## Next\n| `x` | y |';
  assert.deepEqual(parseOverlayTables(md), [
    { name: 'Foo', rows: [{ keys: 'j / k', action: 'Move', detail: '' }] },
    { name: 'Bar', rows: [{ keys: 'Esc', action: 'Close', detail: 'Closes it.' }] },
  ]);
});

test('help lists an overlay’s own keys under its name, and every table in All Commands', () => {
  const rows = overlayRows(['Model Picker']);
  assert.ok(rows.length >= 10);
  assert.equal(rows[0].section, 'Model Picker (overlay keys)');
  assert.ok(rows.some((r) => r.keys[0] === 'Tab / Shift-Tab'));
  assert.deepEqual(overlayRows(['No Such Overlay']), []);
  assert.ok(allOverlayRows().some((r) => r.section === 'Overlay: File Viewer'));
});

test('s opens the Symbols view and Tab cycles Contextual → All Commands → Symbols', () => {
  assert.deepEqual(HELP_VIEWS, ['contextual', 'all', 'symbols']);
  assert.equal(helpKey(INITIAL_HELP, { key: 's' }, 5).st.view, 'symbols');
  const sym = { ...INITIAL_HELP, view: 'symbols' };
  assert.equal(helpKey(sym, { key: 'Tab' }, 5).st.view, 'contextual');
  assert.equal(helpKey(INITIAL_HELP, { key: 'Tab', shift: true }, 5).st.view, 'symbols');
  assert.equal(helpKey({ ...INITIAL_HELP, searching: true }, { key: 's' }, 5).st.query, 's', 's types while searching');
  assert.equal(symbolRows().length, SYMBOLS.length);
  assert.ok(symbolRows().some((r) => r.keys[0] === '◆' && r.section === 'Session list'));
});
