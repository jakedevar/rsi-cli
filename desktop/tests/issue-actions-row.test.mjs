// The Issue detail pane's visible Edit and Change status buttons (local #265): plain labels, one button per action that can
// run on this Issue, the same action ids the keys run, and a Tab stop each so Tab reaches them with Vim keys off (#243).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { unavailable } from '../dist/registry.js';
import { ISSUES_KEYS, issueButtons } from '../dist/issues-model.js';

const here = (p) => fileURLToPath(new URL(p, import.meta.url));

class FakeEl {
  constructor(tag) { this.tag = tag; this.children = []; this.attrs = {}; this.dataset = {}; this.className = ''; this.textContent = ''; this.title = ''; }
  get tabIndex() { return this.attrs.tabindex === undefined ? -1 : Number(this.attrs.tabindex); }
  set tabIndex(v) { this.attrs.tabindex = String(v); }
  set type(v) { this.attrs.type = String(v); }
  get type() { return this.attrs.type ?? ''; }
  append(...kids) { this.children.push(...kids); }
  setAttribute(k, v) { this.attrs[k] = String(v); }
  getAttribute(k) { return this.attrs[k] ?? null; }
}
globalThis.document = { createElement: (tag) => new FakeEl(tag) };
const { issueActionRow } = await import('../dist/issue-actions-row.js');

const issue = (over = {}) => ({
  id: '00000000-0000-4000-8000-000000000001', project_id: '00000000-0000-4000-8000-000000000099', display_number: 12,
  title: 'Fix the thing', body: '', status: 'Open', priority: 2, labels: [], assignee: null,
  created_at: '2026-10-18T09:06:32.216383Z', updated_at: '2026-10-18T10:00:00Z', row_version: 3, archived_at: null, closed_at: null, ...over,
});
const snap = (i) => ({
  open: null, cursorOnRow: false, rowCount: 0, attentionRows: 0, view: 'active', vimKeys: false, hideTools: false,
  showFinished: false, modalOpen: false, event: null,
  issues: { active: true, hasProject: true, issue: i, nextPage: false, previousPage: false, operatorOnly: false },
});

test('an open Issue shows Edit… and Change status… in plain words', () => {
  assert.deepEqual(issueButtons(issue()).map((b) => [b.action, b.label]), [['issues-edit', 'Edit…'], ['issues-status', 'Change status…']]);
  assert.deepEqual(issueButtons(issue({ status: 'InProgress' })).map((b) => b.label), ['Edit…', 'Change status…']);
});

test('a finished Issue offers Change status… (to reopen) and Archive…; an archived one offers Restore only', () => {
  assert.deepEqual(issueButtons(issue({ status: 'Closed' })).map((b) => b.label), ['Change status…', 'Archive…']);
  assert.deepEqual(issueButtons(issue({ status: 'Cancelled' })).map((b) => b.label), ['Change status…', 'Archive…']);
  assert.deepEqual(issueButtons(issue({ status: 'Closed', archived_at: '2026-10-19T00:00:00Z' })).map((b) => b.label), ['Restore']);
});

test('a button is listed exactly when the key and palette would let that action run', () => {
  for (const status of ['Open', 'InProgress', 'Closed', 'Cancelled']) {
    for (const archived_at of [null, '2026-10-19T00:00:00Z']) {
      const i = issue({ status, archived_at });
      const shown = new Set(issueButtons(i).map((b) => b.action));
      for (const action of ['issues-edit', 'issues-status', 'issues-archive', 'issues-restore']) {
        assert.equal(shown.has(action), unavailable(action, snap(i)) === null, `${status}${archived_at ? ' archived' : ''}: ${action}`);
      }
    }
  }
});

test('every button runs an action the keys already run, so the forms, confirmations and RPCs are the same', () => {
  const keyed = new Set(Object.values(ISSUES_KEYS));
  for (const status of ['Open', 'Closed']) for (const b of issueButtons(issue({ status }))) assert.ok(keyed.has(b.action), b.action);
  assert.equal(ISSUES_KEYS.e, 'issues-edit');
  assert.equal(ISSUES_KEYS.S, 'issues-status');
});

test('the row is a named group of real buttons with a Tab stop and a tooltip each', () => {
  const row = issueActionRow(issue());
  assert.equal(row.getAttribute('role'), 'group');
  assert.equal(row.getAttribute('aria-label'), 'Actions for #12');
  assert.deepEqual(row.children.map((b) => [b.tag, b.type, b.tabIndex, b.dataset.action, b.textContent]), [
    ['button', 'button', 0, 'issues-edit', 'Edit…'],
    ['button', 'button', 0, 'issues-status', 'Change status…'],
  ]);
  for (const b of row.children) assert.ok(b.title.length > 10, `${b.textContent} has a tooltip`);
  assert.equal(issueActionRow(issue({ status: 'Closed', archived_at: '2026-10-19T00:00:00Z' })).children.length, 1);
});

test('the labels use no key letters or jargon', () => {
  for (const status of ['Open', 'Closed']) for (const b of [...issueButtons(issue({ status })), ...issueButtons(issue({ status, archived_at: 'x' }))]) {
    assert.doesNotMatch(`${b.label} ${b.title}`, /\([A-Za-z]\)|row_version|RPC|\bepic\b/i);
  }
});

test('the detail pane draws the row between the title and the fields', () => {
  const src = readFileSync(here('../src/issues.ts'), 'utf8');
  assert.match(src, /issueActionRow\(issue\)/);
  assert.match(src, /box\.replaceChildren\(head, \.\.\.\(actions \? \[actions\] : \[\]\), meta, body/);
});
