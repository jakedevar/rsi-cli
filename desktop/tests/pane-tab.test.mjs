// Local #243: with Vim keys off no pane or dialog claims Tab or Shift-Tab, so the browser's focus order reaches every
// button; F6 is the pane-cycle key. With Vim keys on Tab still switches sections as the TUI does. One shared rule
// (modal-gate paneKeyAllowed / tabIsNative / paneKeyName) is checked over the eight views the Issue lists.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { paneKeyAllowed, paneKeyName, tabIsNative, dialogKeyBlocked, STANDARD_KEYS } from '../dist/modal-gate.js';
import { INITIAL_NAV, paneKey } from '../dist/settings-model.js';
import { dagKey, initialDag } from '../dist/dag-model.js';
import { graphKey, newDraft, openReview } from '../dist/graph-model.js';
import { idleKeys, resolveIssuesKey } from '../dist/issues-model.js';
import { VIEW_KEYS } from '../dist/managers-model.js';
import { DASHBOARD_KEYS } from '../dist/dashboard-model.js';
import { helpKey, INITIAL_HELP } from '../dist/keyhelp-model.js';
import { dropdownKey } from '../dist/model-dropdown-model.js';
import { formatKeyHint } from '../dist/key-hints.js';

const src = (f) => readFileSync(fileURLToPath(new URL(`../src/${f}`, import.meta.url)), 'utf8');
const keydown = (key, shiftKey = false) => ({ key, shiftKey });

const wf = { version: '1.0', name: 'W', description: '', nodes: [{ id: 'a', name: 'a', type: 'action', instructions: '' }], edges: [], metadata: {} };
const review = openReview(newDraft(wf));
const ctx = { layout: { boxes: [], edges: [], cols: 0, rows: 0 }, executing: false, pickerCount: 0 };
const DROPDOWN = { query: '', typing: false, sel: 0 };

/** Each view: how its key logic reacts to the key name a pane handler passes it (null when it does nothing). */
const VIEWS = {
  settings: (name) => { const fx = paneKey(INITIAL_NAV, name, false, 3); return fx.kind === 'none' ? null : fx.kind; },
  dag: (name) => { const r = dagKey(initialDag(null), { key: name }); return r.handled ? r.effect.kind + r.state.panel : null; },
  graph: (name) => { const r = graphKey(review, { key: name }, ctx); return r.handled ? r.effect.kind : null; },
  issues: (name) => { const r = resolveIssuesKey(idleKeys(), name, 0).result; return r.kind === 'run' ? r.action : null; },
  managers: (name) => VIEW_KEYS.board[name] ?? null,
  dashboard: (name) => DASHBOARD_KEYS[name] ?? null,
  help: (name) => { const r = helpKey(INITIAL_HELP, { key: name }, 5); return r.kind === 'none' ? null : r.st?.view ?? r.kind; },
  picker: (name) => { const r = dropdownKey(DROPDOWN, { key: name }, 3); return r.effect.kind === 'provider' ? 'provider' : null; },
};

test('the shared gate: Tab is not a pane key with Vim keys off, F6 is; both keys act with Vim keys on', () => {
  assert.equal(STANDARD_KEYS.has('Tab'), false);
  assert.equal(paneKeyAllowed(false, keydown('Tab')), false, 'off: the pane returns before preventDefault');
  assert.equal(paneKeyAllowed(false, keydown('Tab', true)), false);
  assert.equal(paneKeyAllowed(true, keydown('Tab')), true, 'on: the pane takes Tab as the TUI does');
  for (const vim of [false, true]) assert.equal(paneKeyAllowed(vim, keydown('F6')), true, `F6, Vim keys ${vim}`);
  assert.equal(paneKeyName(keydown('F6')), 'Tab');
  assert.equal(paneKeyName(keydown('Tab')), 'Tab');
  assert.equal(paneKeyName(keydown('Enter')), 'Enter');
  assert.equal(tabIsNative(false, keydown('Tab')), true);
  assert.equal(tabIsNative(true, keydown('Tab')), false);
  assert.equal(tabIsNative(false, keydown('F6')), false);
  assert.equal(tabIsNative(false, keydown('j')), false);
});

test('every view: Vim keys off leaves Tab to the browser, Vim keys on keeps the Tab action, F6 does the same action in both', () => {
  for (const [view, act] of Object.entries(VIEWS)) {
    const handled = (vim, key) => (paneKeyAllowed(vim, keydown(key)) && !tabIsNative(vim, keydown(key)) ? act(paneKeyName(keydown(key))) : null);
    const tabAction = act('Tab');
    assert.ok(tabAction, `${view}: Tab does something with Vim keys on`);
    assert.equal(handled(true, 'Tab'), tabAction, `${view}: Vim keys on, Tab`);
    assert.equal(handled(false, 'Tab'), null, `${view}: Vim keys off, Tab is not claimed`);
    assert.equal(handled(false, 'F6'), tabAction, `${view}: Vim keys off, F6 is the pane-cycle key`);
    assert.equal(handled(true, 'F6'), tabAction, `${view}: Vim keys on, F6 too`);
  }
});

test('Shift-Tab and Shift-F6 reach the previous section where the view has one', () => {
  assert.equal(VIEW_KEYS.board['S-Tab'], 'mgr-section-prev');
  assert.notEqual(dagKey(initialDag(null), { key: 'Tab', shift: true }).state.panel, dagKey(initialDag(null), { key: 'Tab' }).state.panel);
  const next = helpKey(INITIAL_HELP, { key: 'Tab' }, 5).st.view;
  const prev = helpKey(INITIAL_HELP, { key: 'Tab', shift: true }, 5).st.view;
  assert.notEqual(next, prev);
});

test('every pane handler asks paneKeyAllowed first and reads its key through paneKeyName', () => {
  for (const file of ['issues.ts', 'settings.ts', 'dag.ts', 'managers.ts', 'dashboard.ts', 'graphs.ts']) {
    const text = src(file);
    assert.match(text, /paneKeyAllowed\(isVimKeysOn\(\), ev\)/, `${file}: gated`);
    assert.match(text, /paneKeyName\(ev\)/, `${file}: F6 reads as Tab`);
  }
  // The two dialogs are outside the pane gate and ask tabIsNative before they act on the key.
  for (const file of ['keyhelp.ts', 'model-dropdown.ts']) {
    const text = src(file);
    assert.match(text, /tabIsNative\(/, `${file}: Tab is native with Vim keys off`);
    assert.match(text, /paneKeyName\(ev\)/, `${file}: F6 reads as Tab`);
  }
  assert.doesNotMatch(src('dashboard.ts'), /dashboardClaimsTab/, 'the dashboard special case is gone');
  assert.doesNotMatch(src('dashboard-model.ts'), /dashboardClaimsTab/);
});

test('the dialog gate leaves Tab to the dialog (it is native with Vim keys off) and F6 passes', () => {
  const dialogEl = { tagName: 'DIALOG', closest: (s) => (s === 'dialog' ? {} : null) };
  for (const key of ['Tab', 'F6']) {
    assert.equal(dialogKeyBlocked(false, { key, target: dialogEl }), false, key);
    assert.equal(dialogKeyBlocked(true, { key, target: dialogEl }), false, key);
  }
});

test('hint lines name F6, not Tab, for the pane switch with Vim keys off', () => {
  assert.equal(formatKeyHint('1-5 · Tab/Shift-Tab sections · j/k fields · Esc close'), 'F6/Shift-F6 sections · Esc close');
  assert.equal(formatKeyHint('j/k · Tab conversation · Esc'), 'F6 conversation · Esc');
  assert.equal(formatKeyHint('j/k move · g/G ends · / filter · Tab/Shift-Tab provider · 1-9 or Enter pick · Esc or q close'),
    'F6/Shift-F6 provider · Enter pick · Esc close');
  // The admin dialogs that switch tabs follow the same rule (local #248).
  assert.equal(formatKeyHint('Tab cycles peers, links and cached sessions; j/k select; Esc close.'), 'F6 cycles peers, links and cached sessions; Esc close.');
  assert.equal(formatKeyHint('Tab switches Devices and Projects; j/k select; Esc close.'), 'F6 switches Devices and Projects; Esc close.');
  // A text field that still uses Tab (path completion, field cycling) keeps its wording.
  assert.equal(formatKeyHint('Tab moves, Space toggles a checkbox, Enter saves, Esc cancels.'), 'Tab moves, Enter saves, Esc cancels.');
});
