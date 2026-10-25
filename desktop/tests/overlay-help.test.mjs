// The keybindings help's overlay tables and Symbols view (gap sweep G4, hub #3000).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { isFresh, parseOverlayTables } from '../scripts/overlay-help.mjs';
import { HELP_VIEWS, INITIAL_HELP, SYMBOLS, allOverlayRows, helpKey, overlayRows, symbolRows } from '../dist/keyhelp-model.js';
import { OVERLAY_TABLES } from '../dist/overlay-help.generated.js';

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

test('s opens the Symbols view and Tab cycles Contextual → All Commands → Symbols → Getting started', () => {
  assert.deepEqual(HELP_VIEWS, ['contextual', 'all', 'symbols', 'start']);
  assert.equal(helpKey(INITIAL_HELP, { key: 's' }, 5).st.view, 'symbols');
  const sym = { ...INITIAL_HELP, view: 'symbols' };
  assert.equal(helpKey(sym, { key: 'Tab' }, 5).st.view, 'start');
  assert.equal(helpKey(INITIAL_HELP, { key: 'Tab', shift: true }, 5).st.view, 'start');
  assert.equal(helpKey({ ...INITIAL_HELP, searching: true }, { key: 's' }, 5).st.query, 's', 's types while searching');
  assert.equal(symbolRows().length, SYMBOLS.length);
  assert.ok(symbolRows().some((r) => r.keys[0] === '◆' && r.section === 'Session list'));
});

// #340: the large editor's help says what Enter does on the desktop. The generated table is the TUI's
// (`Enter / Ctrl-Enter` sends there); the desktop swaps in its own rows and the generated file is untouched.
test('Input Modal help: Enter is a new line and only Ctrl/Cmd-Enter sends (desktop override of the generated row)', async () => {
  const rows = overlayRows(['Input Modal']);
  const row = (k) => rows.find((r) => r.keys[0] === k);
  assert.equal(row('Enter / Ctrl-Enter'), undefined, 'the TUI row that says Enter sends is replaced');
  assert.equal(row('Ctrl-Enter / Cmd-Enter')?.title, 'Send message');
  assert.match(row('Enter')?.title ?? '', /^New line \(insert mode\)/);
  assert.ok(row('Ctrl-Q'), 'the other generated rows stay');
  assert.ok(allOverlayRows().some((r) => r.section === 'Overlay: Input Modal' && r.keys[0] === 'Ctrl-Enter / Cmd-Enter'), 'All Commands lists the same rows');
  assert.ok(OVERLAY_TABLES.find((t) => t.name === 'Input Modal').rows.some((r) => r.keys === 'Enter / Ctrl-Enter'), 'the generated table keeps the TUI row');
});

test('Input Modal behaviour behind that help: Enter never sends, in insert or normal mode; Ctrl/Cmd-Enter sends', async () => {
  const { submitKey } = await import('../dist/input-model.js');
  const { vimKey, enterNormal } = await import('../dist/vim-field.js');
  // The editor's send test (actions/input.ts openInputModal: submitKey(ev, false)).
  assert.equal(submitKey({ key: 'Enter' }, false), false, 'plain Enter does not send');
  assert.equal(submitKey({ key: 'Enter', shiftKey: true }, false), false);
  assert.equal(submitKey({ key: 'Enter', ctrlKey: true }, false), true, 'Ctrl-Enter sends');
  assert.equal(submitKey({ key: 'Enter', metaKey: true }, false), true, 'Cmd-Enter sends');
  // Insert mode: the Vim editor leaves Enter to the textarea, which types a newline.
  const field = { tagName: 'TEXTAREA', value: 'ab', selectionStart: 0, selectionEnd: 0, dataset: {}, setSelectionRange() {}, dispatchEvent() {} };
  const enter = { key: 'Enter', ctrlKey: false, metaKey: false, altKey: false, shiftKey: false, isComposing: false };
  assert.equal(vimKey(field, enter, 'linux').outcome, 'pass', 'insert mode: Enter is the textarea\'s own (a newline)');
  // Normal mode: the editor sees Enter and the dialog does nothing with it (it is not a send).
  enterNormal(field);
  const r = vimKey(field, enter, 'linux');
  assert.equal(r.token, 'Enter');
  assert.notEqual(r.outcome, 'pass', 'normal mode: the editor owns the key, so it is not a newline either');
  assert.equal(field.value, 'ab', 'and the text did not change');
});
