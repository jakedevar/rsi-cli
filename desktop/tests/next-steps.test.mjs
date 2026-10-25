// Local #208: the first-run "Use Vim-style keys?" question, the one next-step formatter, the
// Budgets buttons and the empty states, each with Vim keys on and off.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import {
  VIM_KEYS_ASKED_KEY, VIM_KEYS_STORAGE_KEY, answerVimKeysFirstRun, readVimKeys, vimKeysFirstRun,
} from '../dist/keyengine.js';
import { formatKeyHint, formatKeyTip, nextStep, stripKeyLabel } from '../dist/key-hints.js';
import { EMPTY_STATES, MORE_ACTIONS_HINT, emptyLine, emptyText } from '../dist/empty-states.js';
import { BUDGET_BUTTONS, budgetButtonLabel, budgetListKey } from '../dist/budgets-model.js';
import { sectionRows } from '../dist/settings-model.js';

const src = (f) => readFileSync(fileURLToPath(new URL(`../src/${f}`, import.meta.url)), 'utf8');

/** A localStorage stand-in that can list its keys. */
function memStore(init = {}) {
  const mem = new Map(Object.entries(init));
  return {
    mem,
    get length() { return mem.size; },
    key: (i) => [...mem.keys()][i] ?? null,
    getItem: (k) => mem.get(k) ?? null,
    setItem: (k, v) => { mem.set(k, String(v)); },
  };
}

// ---------- first run ----------

test('a new install is asked once, with Vim keys off (default No) until it answers', () => {
  const store = memStore();
  assert.deepEqual(vimKeysFirstRun(store), { ask: true, on: false });
  // The default is applied and stored at once: a window closed before answering stays off.
  assert.equal(store.mem.get(VIM_KEYS_STORAGE_KEY), 'off');
  assert.equal(store.mem.get(VIM_KEYS_ASKED_KEY), 'pending');
  assert.deepEqual(vimKeysFirstRun(store), { ask: true, on: false }, 'unanswered: asked again on the next launch');
  answerVimKeysFirstRun(store, false);
  assert.deepEqual(vimKeysFirstRun(store), { ask: false, on: false }, 'answered No: never asked again, stays off');
  assert.equal(store.mem.get(VIM_KEYS_ASKED_KEY), 'answered');
});

test('answering Yes turns Vim keys on and persists it', () => {
  const store = memStore();
  vimKeysFirstRun(store);
  answerVimKeysFirstRun(store, true);
  assert.equal(store.mem.get(VIM_KEYS_STORAGE_KEY), 'on');
  assert.deepEqual(vimKeysFirstRun(store), { ask: false, on: true });
  assert.equal(readVimKeys(store), true);
});

test('an existing install keeps its current setting and is never asked', () => {
  const never = memStore({ 'rsi.desktop.theme.v1': '{"name":"mocha"}' });
  assert.deepEqual(vimKeysFirstRun(never), { ask: false, on: true }, 'never touched the switch: still on');
  assert.equal(never.mem.get(VIM_KEYS_ASKED_KEY), 'answered');
  assert.equal(never.mem.has(VIM_KEYS_STORAGE_KEY), false, 'its setting is not rewritten');
  const off = memStore({ [VIM_KEYS_STORAGE_KEY]: 'off' });
  assert.deepEqual(vimKeysFirstRun(off), { ask: false, on: false });
  const todo = memStore({ 'rsi-desktop.todo': '{}' });
  assert.deepEqual(vimKeysFirstRun(todo), { ask: false, on: true });
  // Unreadable storage asks nothing and keeps the old default.
  const broken = { length: 0, key: () => null, getItem: () => { throw new Error('denied'); }, setItem: () => { throw new Error('denied'); } };
  assert.deepEqual(vimKeysFirstRun(broken), { ask: false, on: true });
  assert.deepEqual(vimKeysFirstRun(undefined), { ask: false, on: true });
});

test('Settings shows the Vim keys toggle, and changing it answers an open first-run question', () => {
  const rows = sectionRows('editing_mode', {});
  assert.deepEqual(rows.find((r) => r.id === 'desktop_vim_keys')?.widget, { type: 'local', local: 'vim-keys' });
  const vim = src('vimkeys.ts');
  assert.match(vim, /const first = vimKeysFirstRun\(storage\(\)\);\s*toggle\.checked = first\.on;/);
  assert.match(vim, /if \(first\.ask\) askVimKeys\(\);/);
  assert.match(vim, /if \(firstRunCard\) \{ answerVimKeysFirstRun\(storage\(\), on\); closeFirstRun\(\); \}/);
  assert.match(vim, /'Use Vim-style keys\?'/);
  // No is the first, focused, primary button; Esc answers No.
  assert.match(vim, /const no = el\('button', 'btn primary', 'No'\);/);
  assert.match(vim, /no\.focus\(\);/);
  assert.match(vim, /if \(ev\.key !== 'Escape'\) return;[\s\S]*?answer\(false\);/);
  const html = readFileSync(fileURLToPath(new URL('../static/index.html', import.meta.url)), 'utf8');
  assert.match(html, /<input id="vim-keys" type="checkbox">/);
});

// ---------- the formatter ----------

test('nextStep: the button first, the key in secondary text only with Vim keys on', () => {
  assert.equal(nextStep({ click: 'Add peer', key: 'a' }, true), 'Click Add peer (or press a)');
  assert.equal(nextStep({ click: 'Add peer', key: 'a' }, false), 'Click Add peer');
  assert.equal(nextStep({ click: 'an Issue in the list', key: 'j / k' }, false), 'Click an Issue in the list');
  assert.equal(nextStep({ menu: 'Actions… › New session', key: 'n' }, true), 'Choose Actions… › New session (or press n)');
  assert.equal(nextStep({ menu: 'Actions… › New session', key: 'n' }, false), 'Choose Actions… › New session');
  // A key that works with Vim keys off stays.
  assert.equal(nextStep({ click: 'New', key: 'Ctrl+N' }, false), 'Click New (or press Ctrl+N)');
  assert.equal(nextStep({ click: 'Edit', key: 'Enter' }, false), 'Click Edit (or press Enter)');
  assert.equal(nextStep({ click: 'Close' }, true), 'Click Close');
});

test('the key-hints pass turns the on wording into the off wording (no re-render on a switch flip)', () => {
  for (const step of [{ click: 'Add', key: 'a' }, { click: 'Delete', key: 'dd' }, { menu: 'Settings', key: '<Space>s' }, { click: 'one', key: 'j / k' }]) {
    const on = `Nothing here. ${nextStep(step, true)}.`;
    const off = `Nothing here. ${nextStep(step, false)}.`;
    assert.equal(formatKeyTip(on), off, `tip ${on}`);
    assert.equal(formatKeyHint(on), off, `hint ${on}`);
  }
});

// ---------- empty states ----------

test('every empty state names a visible next step, with Vim keys on and off', () => {
  for (const [id, spec] of Object.entries(EMPTY_STATES)) {
    const on = emptyLine(spec, true);
    const off = emptyLine(spec, false);
    assert.equal(emptyText(id), on);
    assert.match(off, /\b(Click|Choose) \S/, `${id} off names a button or menu: ${off}`);
    assert.match(on, /\b(Click|Choose) \S/, `${id} on names a button or menu: ${on}`);
    assert.equal(formatKeyTip(on), off, `${id}: the tip pass gives the off wording`);
    assert.equal(formatKeyHint(on), off, `${id}: the hint pass gives the off wording`);
  }
});

test('a sample of empty states in both modes', () => {
  assert.equal(emptyText('admin-peers'), 'No peers. Click Add (or press a) to add one.');
  assert.equal(emptyText('admin-peers', false), 'No peers. Click Add to add one.');
  assert.equal(emptyText('admin-schedules'), 'No scheduled jobs. Click New (or press n) to create one.');
  assert.equal(emptyText('admin-schedules', false), 'No scheduled jobs. Click New to create one.');
  assert.equal(emptyText('admin-mcp', false), 'No MCP servers. Click Add to add one.');
  assert.equal(emptyText('assist-facts', false), 'No facts yet. Click Add to add one.');
  assert.equal(emptyText('budgets', false), 'No explicit budget policies: the daemon’s built-in per-scope defaults apply. Click Add policy to add one.');
  assert.equal(emptyText('issues-inspector'), 'No Issue selected. Click an Issue in the list (or press j / k) to read it here.');
  assert.equal(emptyText('issues-inspector', false), 'No Issue selected. Click an Issue in the list to read it here.');
});

test('each empty state names a button its panel shows (label as the key-hints pass leaves it)', () => {
  const panels = [
    ['admin-ops.ts', 'admin-peers', "button('Add (a)')"], ['admin-ops.ts', 'admin-sessions', "button('Refresh (r)')"],
    ['admin-schedules.ts', 'admin-schedules', "button('New (n)')"], ['admin-keys.ts', 'admin-mcp', "button('Add (a)')"],
    ['assist.ts', 'assist-facts', "button('Add (a)')"], ['prompt-creator.ts', 'prompts', "button('New (Ctrl+N)')"],
  ];
  for (const [file, id, btn] of panels) {
    const text = src(file);
    assert.ok(text.includes(btn), `${file} shows ${btn}`);
    assert.ok(text.includes(`'${id}'`), `${file} renders the ${id} empty state`);
    const label = /button\('([^']+)'\)/.exec(btn)[1];
    assert.ok(emptyText(id, false).includes(`Click ${stripKeyLabel(label).replace(/ \(Ctrl\+N\)$/, '')}`), `${id} names ${label}`);
  }
  for (const file of ['issues.ts', 'budgets.ts', 'graphs.ts']) assert.match(src(file), /emptyText\('/);
});

test('the managers footer ends on the Actions… menu, which survives Vim keys off', () => {
  assert.equal(MORE_ACTIONS_HINT, 'Click Actions… (or press Ctrl+K) for every action');
  assert.equal(formatKeyHint(`j/k · a appoint · e edit · Enter open · Esc close · ${MORE_ACTIONS_HINT}`),
    `Enter open · Esc close · ${MORE_ACTIONS_HINT}`);
  assert.match(src('managers.ts'), /textContent = `\$\{hints\(\)\} · \$\{moreActionsHint\(currentPlatform\(\)\)\}`/);
});

// ---------- budgets ----------

test('the Budgets buttons run the same actions as their keys', () => {
  assert.deepEqual(BUDGET_BUTTONS.map(budgetButtonLabel), ['Add policy (a)', 'Edit', 'Delete (d)', 'Refresh (R)', 'Close (q)']);
  for (const b of BUDGET_BUTTONS) assert.equal(budgetListKey(b.key), b.action, `${b.label} and ${b.key}`);
  assert.equal(budgetListKey(' '), 'edit');
  assert.equal(budgetListKey('j'), 'down');
  assert.equal(budgetListKey('ArrowUp'), 'up');
  assert.equal(budgetListKey('x'), null);
  // With Vim keys off the labels lose only their key suffix.
  assert.deepEqual(BUDGET_BUTTONS.map((b) => stripKeyLabel(budgetButtonLabel(b))), ['Add policy', 'Edit', 'Delete', 'Refresh', 'Close']);
  const dom = src('budgets.ts');
  // Buttons and keys reach one dispatcher.
  assert.match(dom, /btn\.addEventListener\('click', \(\) => run\(b\.action\)\);/);
  assert.match(dom, /const a = budgetListKey\(ev\.key\);\s*if \(a === null\) return;\s*ev\.preventDefault\(\);\s*ev\.stopPropagation\(\);\s*run\(a\);/);
  assert.match(dom, /emptyState\('li', 'pick-empty', emptyText\('budgets'\), \{ label: 'Add policy', key: 'a', run: \(\) => run\('add'\) \}\)/);
  // The form has Save and Cancel buttons too.
  assert.match(dom, /el\('button', 'btn primary', 'Save'\)/);
  assert.match(dom, /el\('button', 'btn', 'Cancel'\)/);
});
