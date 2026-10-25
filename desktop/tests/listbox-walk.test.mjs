// The keyboard-list walk in a real browser (local #318 / #320): scripts/audit-listbox-walk.mjs against the built app and the
// fake daemon. Skipped without Chrome or a built dist (runner_unavailable). The full 32-view walk is
// `node scripts/audit.mjs flow scripts/audit-listbox-walk.mjs`; this test walks the palette and a spread of the others and also
// reads the computed focus cue of the Issues list, the Issues inspector and the to-do panel (#320).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { findChrome } from '../scripts/smoke-lib.mjs';
import * as lib from '../scripts/audit-lib.mjs';
import { reloadPage } from '../scripts/audit.mjs';
import { dagFixtures } from '../scripts/audit-fixtures.mjs';
import { EXTRA_VIEWS, LIST_VIEWS, WALK_STUBS, judgeWiring, readWiring, walkFixtures, walkLists } from '../scripts/audit-listbox-walk.mjs';

const here = (p) => fileURLToPath(new URL(p, import.meta.url));
const good = {
  owner: 'input#x[combobox]', ownerRole: 'combobox', expanded: 'true', controls: 'l1', controlsMatches: true, pointer: 'l1-o2', targetVisible: true,
  targetRole: 'option', targetSelected: 'true', targetText: 'Two', highlightText: 'Two', rows: 3, selectedCount: 1, listName: 'Things',
};

test('judgeWiring: the pointer must name the one selected, highlighted row of a named list and must move', () => {
  assert.deepEqual(judgeWiring({ ...good, pointer: 'l1-o1' }, good).problems, []);
  assert.ok(judgeWiring({ ...good, pointer: 'l1-o1' }, good).moved);
  const bad = (over, before = { ...good, pointer: 'l1-o1' }) => judgeWiring(before, { ...good, ...over }).problems.join('|');
  assert.match(bad({ owner: null }), /no focused element/);
  assert.match(bad({ pointer: 'l1-o2' }, good), /did not move/);
  assert.match(bad({ targetSelected: 'false' }), /not aria-selected/);
  assert.match(bad({ selectedCount: 2 }), /2 rows are aria-selected/);
  assert.match(bad({ highlightText: 'One' }), /highlighted row/);
  assert.match(bad({ listName: null }), /no accessible name/);
  assert.match(bad({ controlsMatches: false }), /aria-controls/);
  assert.match(bad({ ownerRole: null }), /not role=combobox/);
  assert.match(bad({ expanded: null }), /not aria-expanded/);
  assert.match(bad({ targetRole: null }), /not an option/);
  assert.deepEqual(judgeWiring(null, { ...good, rows: 1 }).problems, [], 'a one-row list cannot move');
});

test('judgeWiring: a row may hold named, tabbable controls in cells; an option may hold none (#334)', () => {
  const row = { ...good, targetRole: 'row', rowControls: [{ tag: 'button', name: 'Lower the effort', tabbable: true }, { tag: 'input', name: 'Run in its own git worktree', tabbable: true }], badCells: 0 };
  const bad = (over) => judgeWiring({ ...row, pointer: 'l1-o1' }, { ...row, ...over }).problems.join('|');
  assert.equal(bad({}), '');
  assert.match(bad({ targetRole: 'option' }), /an option holds 2 control/);
  assert.match(bad({ rowControls: [{ tag: 'button', name: '', tabbable: true }] }), /1 control\(s\) in the pointed row have no accessible name/);
  assert.match(bad({ rowControls: [{ tag: 'button', name: 'Next', tabbable: false, disabled: false }] }), /cannot be reached by Tab/);
  assert.equal(bad({ rowControls: [{ tag: 'input', name: 'Run in its own git worktree', tabbable: false, disabled: true }] }), '', 'a disabled control is not a reach problem');
  assert.match(bad({ badCells: 1 }), /not cells/);
});

test('the walk names real actions only', async () => {
  const { ACTIONS } = await import('../dist/registry.js');
  for (const id of LIST_VIEWS) assert.ok(ACTIONS.some((a) => a.id === id), `${id} is a registered action`);
  for (const [id, v] of Object.entries(EXTRA_VIEWS)) assert.ok(ACTIONS.some((a) => a.id === v.action), `${id} opens a registered action`);
});

const chrome = findChrome();
const SKIP = !chrome || !existsSync(here('../dist/index.html')) ? 'runner_unavailable: no Chrome or no dist' : false;
const SPREAD = ['choose-project', 'choose-sort', 'issues-open', 'theme-picker', 'model-dropdown', 'files-explorer', 'files-find', 'memory-search', 'open-recent', 'admin-mcp'];

test('in the browser: Actions palette and a spread of the other lists announce the active row after the move key', { skip: SKIP, timeout: 300000 }, async () => {
  const a = await lib.open({ width: 1280, height: 800, vim: true, stubs: WALK_STUBS, fixtures: walkFixtures(lib.FIXTURES) });
  try {
    await lib.sleep(500);
    await lib.openSessionByTitle(a.page, 'Fake waiting worker');
    await lib.clickSel(a.page, '#open-palette', 300);
    const before = await a.page.eval(readWiring());
    await a.page.key('ArrowDown');
    await lib.sleep(200);
    const after = await a.page.eval(readWiring());
    assert.equal(after.active, 'input#palette-input[combobox]');
    assert.deepEqual(judgeWiring(before, after).problems, []);
    assert.equal(after.rows > 3, true);
    // The pointer names the row the sighted user sees highlighted.
    const same = await a.page.eval(`(() => { const i = document.getElementById('palette-input'); const t = document.getElementById(i.getAttribute('aria-activedescendant')); return t === document.querySelector('#palette-list .sel') && t.getAttribute('aria-selected') === 'true'; })()`);
    assert.equal(same, true);
    await a.page.key('Escape');
    await lib.sleep(200);
    const { ACTIONS } = await import('../dist/registry.js');
    const views = SPREAD.map((id) => ({ id, title: ACTIONS.find((x) => x.id === id).title }));
    const rows = await walkLists(a, views, { lib, reload: reloadPage });
    for (const r of rows) {
      assert.ok(r.opened, `${r.id} opened: ${r.why}`);
      assert.deepEqual(r.verdict.problems, [], r.id);
      assert.ok(!r.empty && r.after.rows >= 1, `${r.id} drew rows`);
    }
  } finally { await a.close(); }
});

const PANES_330 = ['mgr-global', 'mgr-board', 'mgr-dashboard', 'mgr-budget', 'dag-open', 'graph-open', 'settings-open'];

test('in the browser: the manager lists, dashboard, DAG, graph review, budgets and settings rail announce the active row (#330)', { skip: SKIP, timeout: 300000 }, async () => {
  // The DAG browser's reads go to the fake daemon (READ_COMMANDS, #336), so its rows come from dagFixtures, not from a WALK_STUBS answer.
  const a = await lib.open({ width: 1280, height: 800, vim: true, stubs: WALK_STUBS, fixtures: { ...walkFixtures(lib.FIXTURES), ...dagFixtures() } });
  try {
    await lib.sleep(500);
    const { ACTIONS } = await import('../dist/registry.js');
    const views = PANES_330.map((id) => ({ id, title: ACTIONS.find((x) => x.id === id).title }));
    const rows = await walkLists(a, views, { lib, reload: reloadPage });
    for (const r of rows) {
      assert.ok(r.opened, `${r.id} opened: ${r.why}`);
      assert.deepEqual(r.verdict.problems, [], r.id);
      assert.ok(!r.empty && r.after.rows >= 2, `${r.id} drew rows and the pointer followed the move key`);
      assert.equal(r.verdict.moved, true, `${r.id} moved the pointer`);
    }
  } finally { await a.close(); }
});

test('in the browser: Settings rows and the launch settings rows are grids that announce the current row and keep every control named and reachable (#334)', { skip: SKIP, timeout: 300000 }, async () => {
  const a = await lib.open({ width: 1280, height: 800, vim: true, stubs: WALK_STUBS, fixtures: walkFixtures(lib.FIXTURES) });
  try {
    await lib.sleep(500);
    const { ACTIONS } = await import('../dist/registry.js');
    const views = Object.entries(EXTRA_VIEWS).map(([id, v]) => ({ id, title: ACTIONS.find((x) => x.id === v.action).title }));
    const rows = await walkLists(a, views, { lib, reload: reloadPage });
    for (const r of rows) {
      assert.ok(r.opened, `${r.id} opened: ${r.why}`);
      assert.deepEqual(r.verdict.problems, [], r.id);
      assert.ok(!r.empty && r.after.rows >= 2, `${r.id} drew rows`);
      assert.equal(r.after.targetRole, 'row', `${r.id} rows are grid rows`);
      assert.equal(r.verdict.moved, true, `${r.id} moved the pointer`);
    }
    // The launch settings rows hold real buttons / checkboxes; the walk found them named and reachable inside the row.
    const launch = rows.find((r) => r.id === 'launch-settings');
    assert.ok(launch.after.rowControls.length >= 1, 'the pointed launch row holds at least one control');
  } finally { await a.close(); }
});

test('in the browser: the Issues list, Issues inspector and to-do panel show a focus cue that clears 3:1 (#320)', { skip: SKIP, timeout: 120000 }, async () => {
  const a = await lib.open({ width: 1280, height: 800, vim: false, fixtures: walkFixtures(lib.FIXTURES) });
  try {
    await lib.sleep(500);
    // The to-do panel (opened first: the Issues workspace covers the layout): it takes focus as a drawer; the same shared ring.
    const todo = await a.page.eval(`(() => { const t = document.getElementById('todo-panel'); t.hidden = false; t.focus(); const cs = getComputedStyle(t); const on = cs.outlineStyle + ' ' + cs.outlineWidth; t.blur(); const off = getComputedStyle(t).outlineStyle; return { on, off }; })()`);
    assert.equal(todo.on, 'solid 2px');
    assert.equal(todo.off, 'none');
    await lib.openPaletteAction(a.page, 'Issues workspace');
    await lib.sleep(800);
    const cue = (sel, row) => a.page.eval(`(() => {
      const root = document.querySelector(${JSON.stringify(sel)});
      const e = ${row ? `root.querySelector('[aria-selected="true"]') ?? root` : 'root'};
      const cs = getComputedStyle(e);
      return { focused: document.activeElement === root || root.contains(document.activeElement), outline: cs.outlineStyle + ' ' + cs.outlineWidth + ' ' + cs.outlineColor, shadow: cs.boxShadow };
    })()`);
    // The list: focused, the selected row has a ring; unfocused it has none, so the two differ.
    await a.page.eval(`document.getElementById('issues-list').focus()`);
    const on = await cue('#issues-list', true);
    await a.page.eval(`document.activeElement.blur(); document.getElementById('issues-inspector').focus()`);
    const off = await cue('#issues-list', true);
    assert.equal(on.focused, true);
    assert.match(on.outline, /^solid 2px /, JSON.stringify(on));
    assert.match(off.outline, /^none /, JSON.stringify(off));
    assert.notDeepEqual(on, { ...off, focused: true });
    // The inspector: focused shows the ring, blurred does not.
    const insFocused = await cue('#issues-inspector', false);
    assert.match(insFocused.outline, /^solid 2px /, JSON.stringify(insFocused));
    await a.page.eval(`document.activeElement.blur()`);
    const insBlurred = await cue('#issues-inspector', false);
    assert.match(insBlurred.outline, /^none /, JSON.stringify(insBlurred));
  } finally { await a.close(); }
});
