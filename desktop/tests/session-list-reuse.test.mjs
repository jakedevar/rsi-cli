// The session list reuses its row nodes (local #275): at 3,000 sessions a filter keystroke rebuilt every row (about 90 ms);
// now a row whose content did not change keeps its node, so a keystroke touches only the rows that came or went. These
// tests pin the behaviour that must stay identical: the rows shown, their order and marks, selection and cursor, the
// counts, and that an unchanged refresh touches no node.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { buildTree, flattenTree } from '../dist/tree.js';
import { reuseUnchanged } from '../dist/listmodel.js';
import { installTabStops } from '../dist/tab-stops.js';
import { El } from './fake-dom.mjs';

const NOW = Date.parse('2026-10-02T00:00:00Z');
const base = { provider: 'Claude', working_dir: '/w', created_at: '2026-10-01T00:00:00Z', updated_at: '2026-10-01T12:00:00Z' };
const s = (id, extra = {}) => ({ ...base, id, status: 'Running', query: `prompt ${id}`, title: `Task ${id}`, ...extra });
const sessions = () => Array.from({ length: 40 }, (_, i) => s(`s${String(i).padStart(2, '0')}`, { title: i % 4 === 0 ? `Alpha ${i}` : `Beta ${i}` }));
const rowsFor = (list, text = '') => flattenTree(buildTree(list, { projectId: null, text, showFinished: true }), new Set(), { forceExpand: text !== '' });
const ids = (ul) => ul.children.map((li) => li.dataset.id);

globalThis.document = { createElement: (t) => new El(t) };
const { renderSessionTree } = await import('../dist/sessionTree.js');
const ctxOf = (extra = {}) => ({ projectName: () => undefined, onToggle() {}, now: NOW, selectedId: null, cursorId: null, ...extra });

test('a filter keystroke keeps the nodes of the rows that stay, and widening again brings the same nodes back', () => {
  const all = sessions();
  const known = new Set(all.map((x) => x.id));
  const ul = new El('ul');
  renderSessionTree(ul, rowsFor(all), ctxOf({ knownIds: known }));
  const before = new Map(ul.children.map((li) => [li.dataset.id, li]));
  assert.equal(ul.children.length, 40);

  renderSessionTree(ul, rowsFor(all, 'alpha'), ctxOf({ knownIds: known }));
  assert.deepEqual(ids(ul), rowsFor(all, 'alpha').map((r) => r.session.id), 'the rows shown are exactly the matches, in order');
  assert.equal(ul.children.length, 10);
  for (const li of ul.children) assert.equal(li, before.get(li.dataset.id), 'a staying row keeps its node');
  assert.equal(ul.ops.insert, 40, 'only the first render inserted (no node moved while narrowing)');

  ul.ops.insert = 0;
  renderSessionTree(ul, rowsFor(all), ctxOf({ knownIds: known }));
  assert.deepEqual(ids(ul), rowsFor(all).map((r) => r.session.id), 'the unfiltered order is restored');
  for (const li of ul.children) assert.equal(li, before.get(li.dataset.id), 'a returning row is its earlier node, not a rebuild');
  assert.equal(ul.ops.insert, 30, 'only the 30 rows that came back were inserted');
});

test('rendering the same rows again inserts and removes nothing', () => {
  const all = sessions();
  const ul = new El('ul');
  const rows = rowsFor(all);
  renderSessionTree(ul, rows, ctxOf());
  const first = [...ul.children];
  ul.ops.insert = 0;
  ul.ops.remove = 0;
  renderSessionTree(ul, rowsFor(all), ctxOf());
  assert.deepEqual(ul.children, first);
  assert.deepEqual(ul.ops, { insert: 0, remove: 0 });
});

test('moving the selection and the cursor rebuilds only the rows whose marks changed, and the marks are right', () => {
  const all = sessions();
  const ul = new El('ul');
  renderSessionTree(ul, rowsFor(all), ctxOf({ selectedId: 's01', cursorId: 's01' }));
  const before = new Map(ul.children.map((li) => [li.dataset.id, li]));
  renderSessionTree(ul, rowsFor(all), ctxOf({ selectedId: 's01', cursorId: 's05' }));
  const changed = ul.children.filter((li) => li !== before.get(li.dataset.id)).map((li) => li.dataset.id);
  assert.deepEqual(changed.sort(), ['s01', 's05'], 'the old and new cursor rows, nothing else');
  const byId = new Map(ul.children.map((li) => [li.dataset.id, li]));
  assert.match(byId.get('s05').className, /\bcursor\b/);
  assert.doesNotMatch(byId.get('s01').className, /\bcursor\b/);
  assert.match(byId.get('s01').className, /\bselected\b/);
  assert.equal(byId.get('s05').getAttribute('aria-selected'), 'true');
  assert.equal(byId.get('s01').getAttribute('aria-selected'), 'false');
  assert.equal(byId.get('s01').getAttribute('aria-current'), 'true');
  assert.equal(ul.getAttribute('aria-activedescendant'), 'session-row-s05');
  // Selection survives a filter change that keeps the row, and the active descendant is dropped when its row is hidden.
  renderSessionTree(ul, rowsFor(all, 'alpha'), ctxOf({ selectedId: 's04', cursorId: 's04' }));
  assert.equal(ul.getAttribute('aria-activedescendant'), 'session-row-s04');
  renderSessionTree(ul, rowsFor(all, 'alpha'), ctxOf({ selectedId: 's01', cursorId: 's01' }));
  assert.equal(ul.getAttribute('aria-activedescendant'), null);
});

test('a session that changed is rebuilt; one refetched unchanged keeps its node through reuseUnchanged', () => {
  const all = sessions();
  const ul = new El('ul');
  renderSessionTree(ul, rowsFor(all), ctxOf());
  const before = new Map(ul.children.map((li) => [li.dataset.id, li]));
  // The daemon answers again: every session is a new object, one has a new status.
  const refetched = all.map((x) => ({ ...x, ...(x.id === 's03' ? { status: 'Completed', title: 'Renamed' } : {}) }));
  const reused = reuseUnchanged(all, refetched);
  assert.equal(reused[3], refetched[3], 'the changed session keeps its new object');
  assert.equal(reused[2], all[2], 'an unchanged one is the old object');
  renderSessionTree(ul, rowsFor(reused), ctxOf());
  for (const li of ul.children) {
    if (li.dataset.id === 's03') {
      assert.notEqual(li, before.get('s03'));
      assert.match(li.className, /\bst-Completed\b/);
      assert.match(li.children.find((c) => c.className === 'label').textContent, /Renamed/);
    } else assert.equal(li, before.get(li.dataset.id), `${li.dataset.id} was not rebuilt`);
  }
  // Without reuseUnchanged a refetch rebuilds every row (what the list did before), still with correct content.
  const ul2 = new El('ul');
  renderSessionTree(ul2, rowsFor(all), ctxOf());
  const first2 = new Map(ul2.children.map((li) => [li.dataset.id, li]));
  renderSessionTree(ul2, rowsFor(refetched), ctxOf());
  assert.ok(ul2.children.every((li) => li !== first2.get(li.dataset.id)));
  assert.equal(ul2.children.length, 40);
});

test('a row that shows something else (age, project, label) is rebuilt, one that does not is not', () => {
  const all = sessions();
  const ul = new El('ul');
  renderSessionTree(ul, rowsFor(all), ctxOf());
  const before = new Map(ul.children.map((li) => [li.dataset.id, li]));
  renderSessionTree(ul, rowsFor(all), ctxOf({ projectName: (id) => (id === 'p1' ? 'Proj' : undefined) }));
  assert.ok(ul.children.every((li) => li === before.get(li.dataset.id)), 'no row used a project name');
  const withProject = all.map((x) => (x.id === 's07' ? { ...x, project_id: 'p1', short_summary: 'doing it' } : x));
  renderSessionTree(ul, rowsFor(withProject), ctxOf({ projectName: (id) => (id === 'p1' ? 'Proj' : undefined) }));
  const rebuilt = ul.children.filter((li) => li !== before.get(li.dataset.id)).map((li) => li.dataset.id);
  assert.deepEqual(rebuilt, ['s07']);
  renderSessionTree(ul, rowsFor(withProject), ctxOf({ projectName: () => undefined }));
  const sub = (id) => ul.children.find((li) => li.dataset.id === id).children.find((c) => c.className === 'sub').textContent;
  assert.doesNotMatch(sub('s07'), /Proj/, 'the project name follows the project, not the cache');
  // A later render (time passed) shows the new age.
  const ageOf = (id) => ul.children.find((li) => li.dataset.id === id).children.find((c) => c.className === 'age').textContent;
  assert.equal(ageOf('s02'), '12h');
  renderSessionTree(ul, rowsFor(withProject), ctxOf({ now: NOW + 3 * 3600 * 1000 }));
  assert.equal(ageOf('s02'), '15h');
});

test('a row something else decorated in place (the stop badge) is rebuilt, not kept with the stale decoration', () => {
  const all = sessions();
  const ul = new El('ul');
  renderSessionTree(ul, rowsFor(all), ctxOf());
  const li = ul.children[2];
  li.append(new El('span'));
  renderSessionTree(ul, rowsFor(all), ctxOf());
  assert.notEqual(ul.children[2], li);
  assert.equal(ul.children[2].children.length, li.children.length - 1);
});

test('rows of sessions that are gone are dropped from the cache; filtered-out ones are kept when known', () => {
  const all = sessions();
  const ul = new El('ul');
  const known = new Set(all.map((x) => x.id));
  renderSessionTree(ul, rowsFor(all), ctxOf({ knownIds: known }));
  const s04 = ul.children.find((li) => li.dataset.id === 's04');
  renderSessionTree(ul, rowsFor(all, 'alpha'), ctxOf({ knownIds: known }));
  renderSessionTree(ul, rowsFor(all), ctxOf({ knownIds: known }));
  assert.equal(ul.children.find((li) => li.dataset.id === 's04'), s04);
  // s04 is deleted on the daemon: it is not in the next list, so it is not kept, and a session that comes back is built fresh.
  const without = all.filter((x) => x.id !== 's04');
  const k2 = new Set(without.map((x) => x.id));
  renderSessionTree(ul, rowsFor(without), ctxOf({ knownIds: k2 }));
  renderSessionTree(ul, rowsFor(all), ctxOf({ knownIds: known }));
  assert.notEqual(ul.children.find((li) => li.dataset.id === 's04'), s04);
  // Without knownIds only the shown rows are kept.
  const ul2 = new El('ul');
  renderSessionTree(ul2, rowsFor(all), ctxOf());
  const a = ul2.children.find((li) => li.dataset.id === 's00');
  renderSessionTree(ul2, rowsFor(all, 'beta'), ctxOf());
  renderSessionTree(ul2, rowsFor(all), ctxOf());
  assert.notEqual(ul2.children.find((li) => li.dataset.id === 's00'), a);
});

test('the counts stay right: row count, container badge and group headers across filters', () => {
  const lead = s('lead', { session_kind: 'Epic', title: 'Alpha lead' });
  const kids = [s('k1', { parent_id: 'lead', title: 'Alpha one' }), s('k2', { parent_id: 'lead', title: 'Beta two' })];
  const ul = new El('ul');
  const all = [lead, ...kids];
  renderSessionTree(ul, rowsFor(all), ctxOf());
  const badge = () => ul.children[0].children.find((c) => c.className === 'count').textContent;
  assert.equal(ul.children.length, 3);
  assert.equal(badge(), '2');
  renderSessionTree(ul, rowsFor(all, 'alpha one'), ctxOf());
  assert.deepEqual(ids(ul), ['lead', 'k1']);
  assert.equal(badge(), '1');
  renderSessionTree(ul, rowsFor(all, 'nomatch'), ctxOf({ emptyText: 'none' }));
  assert.equal(ul.children.length, 1);
  assert.equal(ul.children[0].className, 'session-empty');
  renderSessionTree(ul, rowsFor(all), ctxOf());
  assert.deepEqual(ids(ul), ['lead', 'k1', 'k2']);
  assert.equal(badge(), '2');
  // Label group headers come and go with the rows.
  const labelled = all.map((x) => ({ ...x, group_id: 'g1' }));
  renderSessionTree(ul, rowsFor(labelled).slice(0, 1), ctxOf({ groupHeaders: true, labelOf: () => ({ id: 'g1', name: 'Ops', color: '#ff0000' }) }));
  assert.deepEqual(ul.children.map((li) => li.className.split(' ')[0]), ['label-head', 'session-item']);
  renderSessionTree(ul, rowsFor(labelled), ctxOf({ groupHeaders: true, labelOf: () => ({ id: 'g1', name: 'Ops', color: '#ff0000' }) }));
  assert.equal(ul.children.filter((li) => li.className === 'label-head').length, 1);
  assert.equal(ul.children.length, 4);
});

test('a reused row\'s twisty calls the latest render callback', () => {
  const lead = s('lead', { session_kind: 'Epic' });
  const all = [lead, s('k1', { parent_id: 'lead' })];
  const ul = new El('ul');
  const calls = [];
  const clicks = [];
  const realAdd = El.prototype.addEventListener;
  El.prototype.addEventListener = function add(type, cb) { if (type === 'click') clicks.push([this, cb]); };
  try {
    renderSessionTree(ul, rowsFor(all), ctxOf({ onToggle: () => calls.push('first') }));
    renderSessionTree(ul, rowsFor(all), ctxOf({ onToggle: () => calls.push('second') }));
  } finally { El.prototype.addEventListener = realAdd; }
  const twisty = ul.children[0].children[0];
  const click = clicks.find(([el]) => el === twisty);
  assert.ok(click, 'the kept row\'s twisty has its click listener');
  click[1]({ stopPropagation() {} });
  assert.deepEqual(calls, ['second']);
});

test('reuseUnchanged: same objects pass through, changed and new ones are fresh, order follows the new list', () => {
  const a = { id: 'a', n: 1 };
  const b = { id: 'b', n: 2 };
  const next = [{ id: 'b', n: 2 }, { id: 'c', n: 3 }, { id: 'a', n: 9 }];
  const out = reuseUnchanged([a, b], next);
  assert.equal(out[0], b);
  assert.equal(out[1], next[1]);
  assert.equal(out[2], next[2]);
  assert.deepEqual(reuseUnchanged([], next), next);
  assert.notEqual(reuseUnchanged([], next), next, 'a copy, never the caller\'s array');
});

test('the tab-stop observer skips session rows (they hold no control to stamp)', () => {
  let cb;
  class Observer { constructor(f) { cb = f; } observe() {} }
  const touched = [];
  const row = { classList: { contains: (c) => c === 'session-item' }, matches() { touched.push('row'); return false; }, querySelectorAll() { touched.push('row'); return []; } };
  const dialog = { classList: { contains: () => false }, matches() { touched.push('dialog'); return false; }, querySelectorAll() { return []; } };
  installTabStops({ matches: () => false, querySelectorAll: () => [] }, Observer);
  cb([{ type: 'childList', target: null, addedNodes: [row, row, dialog] }]);
  assert.deepEqual(touched, ['dialog']);
});
