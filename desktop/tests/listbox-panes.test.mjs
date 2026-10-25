// Local #330 (follow-up to #318): the panes that move a visual cursor with no list role now announce their current row through
// the shared helper (src/listbox-a11y.ts): the manager lists, dashboard, DAG, graph review, budgets, the launch settings panel
// and the settings group rail. The budget list and form run for real against the fake document; the rest are checked in the
// source (each file draws role/aria-selected rows and calls syncListbox on the element that holds focus) and walked in the
// browser by scripts/audit-listbox-walk.mjs (tests/listbox-walk.test.mjs).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { doc } from './fake-doc.mjs';

const policies = [
  { scope_kind: 'global', max_total_tokens: 1000 },
  { scope_kind: 'project', scope_id: 'p1', max_total_tokens: 2000 },
  { scope_kind: 'session', scope_id: 's1', max_total_tokens: 3000 },
];
globalThis.__TAURI__ = { core: { invoke: async (cmd) => (cmd === 'budget_policies' ? { policies } : null) } };
const { openBudgets, openBudgetForm } = await import('../dist/budgets.js');
const { listRows } = await import('../dist/listbox-a11y.js');

const settle = () => new Promise((r) => setTimeout(r, 0));
const dialog = (cls) => doc.body.children.filter((c) => c.tag === 'dialog' && c.className.includes(cls)).at(-1);
/** The owner's pointer names the one selected row, which is the highlighted one. */
function announced(owner, list) {
  const id = owner.getAttribute('aria-activedescendant');
  assert.ok(id, 'the focus owner has an aria-activedescendant');
  const row = doc.getElementById(id);
  assert.ok(row, `${id} names a row`);
  assert.equal(row.getAttribute('aria-selected'), 'true');
  assert.ok(row.className.split(/\s+/).includes('sel'), 'it is the highlighted row');
  assert.deepEqual(listRows(list).filter((r) => r.getAttribute('aria-selected') === 'true'), [row], 'the only selected row');
  return row;
}

test('budget list: the dialog points at the selected policy and j / k move it', async () => {
  openBudgets();
  await settle();
  const dlg = dialog('budgets-dialog');
  const list = dlg.querySelector('ul');
  assert.equal(list.getAttribute('role'), 'listbox');
  assert.equal(list.getAttribute('aria-label'), 'Budget policies');
  assert.equal(announced(dlg, list).dataset.index, '0');
  dlg.fire('keydown', { key: 'j', target: dlg });
  assert.equal(announced(dlg, list).dataset.index, '1');
  dlg.fire('keydown', { key: 'j', target: dlg });
  dlg.fire('keydown', { key: 'k', target: dlg });
  assert.equal(announced(dlg, list).dataset.index, '1');
  dlg.close();
});

test('budget form: the dialog points at the current field and Tab moves it', () => {
  openBudgetForm(null, () => undefined);
  const dlg = dialog('budget-form-dialog');
  const list = dlg.querySelector('ul');
  assert.equal(list.getAttribute('role'), 'listbox');
  assert.equal(list.getAttribute('aria-label'), 'Budget policy fields');
  const first = announced(dlg, list);
  assert.equal(first.dataset.index, '0');
  dlg.fire('keydown', { key: 'Tab', target: dlg });
  const second = announced(dlg, list);
  assert.equal(second.dataset.index, '1');
  assert.notEqual(second.id, first.id);
  dlg.fire('keydown', { key: 'Tab', shiftKey: true, target: dlg });
  assert.equal(announced(dlg, list).dataset.index, '0');
  dlg.close();
});

// ---------- the panes whose DOM needs a browser: the source wires them, the browser walk moves them ----------

const here = (p) => fileURLToPath(new URL(p, import.meta.url));
const src = (f) => readFileSync(here(`../src/${f}`), 'utf8');

/** file -> [pattern, why]: the rows are options / grid rows with aria-selected, and the focus owner is synced. */
const PANES = {
  'managers.ts': [[/li\.setAttribute\('role', 'option'\);\s*li\.setAttribute\('aria-selected'/, 'manager rows are options'], [/syncListbox\(ul, ul\)/, 'the list holds focus'],
    [/syncListbox\(ol, ol\)/, 'the decision options are a listbox'], [/'aria-activedescendant', optionId/, 'h / l announce the option']],
  'dashboard.ts': [[/tr\.setAttribute\('role', 'row'\);\s*tr\.setAttribute\('aria-selected'/, 'rows are grid rows'], [/syncListbox\(table, wrap, \{ role: 'grid' \}\)/, 'the table wrapper holds focus']],
  'dag.ts': [[/r\.setAttribute\('role', 'row'\);\s*r\.setAttribute\('aria-selected'/, 'rows are grid rows'], [/syncListbox\(t, t\.classList\.contains\('active'\) \? root : t, \{ role: 'grid' \}\)/, 'the pane points at the active table']],
  'graphs.ts': [[/syncListbox\(ul, view\(\), \{ label: 'Edges' \}\)/, 'edges'], [/syncListbox\(ul, view\(\), \{ label: title \}\)/, 'the picker']],
  'budgets.ts': [[/syncListbox\(rows, dlg/, 'form fields'], [/syncListbox\(list, dlg/, 'policies']],
  'prompts.ts': [[/syncListbox\(list, side\.panel, \{ label: 'Launch setting rows', role: 'grid' \}\)/, 'launch settings rows are a grid'],
    [/li\.setAttribute\('role', 'row'\)/, 'a launch row is a grid row (it holds buttons and checkboxes, so never an option)'], [/setAttribute\('role', 'rowheader'\)/, 'the label names the row'],
    [/setAttribute\('role', 'gridcell'\)/, 'the value cell holds the controls']],
  'settings.ts': [[/syncListbox\(rail, pane\.nav\.focus === 'rail' \? root : rail/, 'the group rail'],
    [/syncListbox\(list, pane\.nav\.focus === 'items' \? root : list, \{ label: 'Settings rows', role: 'grid' \}\)/, 'the settings rows are a grid']],
};

for (const [file, checks] of Object.entries(PANES)) {
  test(`${file} announces its keyboard cursor`, () => {
    const text = src(file);
    assert.match(text, /from '\.\/listbox-a11y\.js'/);
    for (const [re, why] of checks) assert.match(text, re, why);
  });
}

test('the walk covers each of these views', async () => {
  const { LIST_VIEWS } = await import('../scripts/audit-listbox-walk.mjs');
  for (const id of ['mgr-global', 'mgr-board', 'mgr-dashboard', 'mgr-budget', 'dag-open', 'graph-open', 'settings-open']) assert.ok(LIST_VIEWS.includes(id), id);
});
