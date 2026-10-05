import { test } from 'node:test';
import assert from 'node:assert/strict';
import { buildTree, flattenTree, toggleCollapsed } from '../dist/tree.js';

const base = {
  provider: 'Claude', query: 'q', working_dir: '/w', created_at: '2026-10-01T00:00:00Z',
};
const s = (id, status, updated_at, extra = {}) => ({ ...base, id, status, updated_at, ...extra });
const F = { projectId: null, text: '', showFinished: true };
const ids = (rows) => rows.map((r) => `${r.depth}:${r.session.id}`);

const group = s('g', 'Completed', '2026-10-01T00:00:00Z', { session_kind: 'Group', title: 'Grp' });
const epic = s('e', 'Completed', '2026-10-01T00:00:00Z', { session_kind: 'Epic', parent_id: 'g', title: 'Epic' });
const w1 = s('w1', 'Running', '2026-10-02T00:00:00Z', { parent_id: 'e', title: 'worker one' });
const w2 = s('w2', 'Completed', '2026-10-03T00:00:00Z', { parent_id: 'e', title: 'worker two' });

test('builds nested hierarchy with counts and aggregate status', () => {
  const rows = flattenTree(buildTree([w2, w1, epic, group], F), new Set());
  assert.deepEqual(ids(rows), ['0:g', '1:e', '2:w1', '2:w2']);
  const g = rows[0];
  assert.equal(g.childCount, 1);
  assert.equal(g.descendantCount, 3);
  assert.equal(g.aggStatus, 'Running');
  assert.equal(g.anyActive, true);
  assert.equal(rows[1].childCount, 2);
});

test('aggregate status is own status when nothing runs', () => {
  const rows = flattenTree(buildTree([group, epic, w2], F), new Set());
  assert.equal(rows[0].aggStatus, 'Completed');
  assert.equal(rows[0].anyActive, false);
});

test('collapsed containers hide their subtree; filter text forces expand', () => {
  const forest = buildTree([group, epic, w1, w2], F);
  assert.deepEqual(ids(flattenTree(forest, new Set(['e']))), ['0:g', '1:e']);
  const collapsedRow = flattenTree(forest, new Set(['e']))[1];
  assert.equal(collapsedRow.expanded, false);
  assert.equal(collapsedRow.childCount, 2);
  assert.deepEqual(ids(flattenTree(forest, new Set(['g']))), ['0:g']);
  assert.equal(flattenTree(forest, new Set(['g']), { forceExpand: true }).length, 4);
});

test('toggleCollapsed is pure', () => {
  const a = new Set();
  const b = toggleCollapsed(a, 'x');
  assert.equal(a.size, 0);
  assert.ok(b.has('x'));
  assert.ok(!toggleCollapsed(b, 'x').has('x'));
});

test('orphans render at top level', () => {
  const orphan = s('o', 'Running', '2026-10-05T00:00:00Z', { parent_id: 'missing' });
  assert.deepEqual(ids(flattenTree(buildTree([orphan, group], F), new Set())), ['0:o', '0:g']);
});

test('filters apply to leaves and keep ancestors of matches', () => {
  const all = [group, epic, w1, w2];
  const byText = flattenTree(buildTree(all, { ...F, text: 'worker two' }), new Set());
  assert.deepEqual(ids(byText), ['0:g', '1:e', '2:w2']);
  const noFinished = flattenTree(buildTree(all, { ...F, showFinished: false }), new Set());
  assert.deepEqual(ids(noFinished), ['0:g', '1:e', '2:w1']);
  const none = buildTree(all, { ...F, text: 'nothing-matches' });
  assert.deepEqual(none, []);
});

test('project filter keeps only matching leaves and their ancestors', () => {
  const a = s('a', 'Running', '2026-10-02T00:00:00Z', { parent_id: 'e', project_id: 'p' });
  const b = s('b', 'Running', '2026-10-02T00:00:00Z', { parent_id: 'e', project_id: 'q' });
  const rows = flattenTree(buildTree([group, epic, a, b], { ...F, projectId: 'p' }), new Set());
  assert.deepEqual(ids(rows), ['0:g', '1:e', '2:a']);
});

test('container with no visible leaves is hidden, empty container passes the filter', () => {
  const done = s('d', 'Completed', '2026-10-02T00:00:00Z', { parent_id: 'e' });
  assert.deepEqual(buildTree([group, epic, done], { ...F, showFinished: false }).length, 0);
  const lone = s('l', 'Running', '2026-10-02T00:00:00Z', { session_kind: 'Epic' });
  assert.equal(buildTree([lone], F).length, 1);
});

test('archived mode keeps Archived sessions and ignores showFinished', () => {
  const a = s('a1', 'Archived', '2026-10-02T00:00:00Z', { title: 'old' });
  assert.equal(buildTree([a], { ...F, showFinished: false }).length, 0);
  assert.equal(buildTree([a], { ...F, showFinished: false, mode: 'archived' }).length, 1);
});

test('parent cycles do not hide or duplicate sessions', () => {
  const a = s('a', 'Running', '2026-10-02T00:00:00Z', { parent_id: 'b' });
  const b = s('b', 'Running', '2026-10-03T00:00:00Z', { parent_id: 'a' });
  const rows = flattenTree(buildTree([a, b], F), new Set());
  assert.equal(rows.length, 2);
  assert.equal(new Set(rows.map((r) => r.session.id)).size, 2);
});

test('siblings: running subtrees first, then newest', () => {
  const x = s('x', 'Completed', '2026-10-09T00:00:00Z');
  const y = s('y', 'Running', '2026-10-01T00:00:00Z');
  assert.deepEqual(ids(flattenTree(buildTree([x, y], F), new Set())), ['0:y', '0:x']);
});
