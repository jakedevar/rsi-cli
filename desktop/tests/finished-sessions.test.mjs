// Finished sessions stay in the list (local #264): Show completed is on by default, as in the TUI, the choice is
// remembered, a finished session is muted not hidden, and the open session never drops out when it finishes.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import {
  SHOW_FINISHED_KEY, emptyListView, parseShowFinished, passesFinished, serializeShowFinished, visibleSessions,
} from '../dist/model.js';
import { buildTree, flattenTree } from '../dist/tree.js';
import { El } from './fake-dom.mjs';

const here = (p) => fileURLToPath(new URL(p, import.meta.url));
const base = { provider: 'Claude', query: 'q', working_dir: '/w', created_at: '2026-10-01T00:00:00Z' };
const s = (id, status, updated_at, extra = {}) => ({ ...base, id, status, updated_at, ...extra });
const run = s('run', 'Running', '2026-10-03T00:00:00Z');
const done = s('done', 'Completed', '2026-10-02T00:00:00Z');
const failed = s('failed', 'Failed', '2026-10-01T00:00:00Z');
const all = [run, done, failed];

test('a fresh profile shows finished sessions; only an explicit off hides them', () => {
  assert.equal(parseShowFinished(null), true);
  assert.equal(parseShowFinished(undefined), true);
  assert.equal(parseShowFinished(''), true);
  assert.equal(parseShowFinished(serializeShowFinished(true)), true);
  assert.equal(parseShowFinished(serializeShowFinished(false)), false);
  assert.match(SHOW_FINISHED_KEY, /^rsi\.desktop\./);
});

test('the choice round-trips through storage values and survives a restart', () => {
  const store = new Map();
  const save = (on) => store.set(SHOW_FINISHED_KEY, serializeShowFinished(on));
  const restore = () => parseShowFinished(store.get(SHOW_FINISHED_KEY));
  assert.equal(restore(), true, 'fresh profile');
  save(false);
  assert.equal(restore(), false, 'hidden stays hidden after a restart');
  save(true);
  assert.equal(restore(), true);
});

test('with Show completed on, a session that goes Running to Completed stays in the list', () => {
  const F = { projectId: null, text: '', showFinished: true };
  assert.deepEqual(visibleSessions([run], F).map((x) => x.id), ['run']);
  const finished = { ...run, status: 'Completed' };
  assert.deepEqual(visibleSessions([finished], F).map((x) => x.id), ['run']);
  assert.deepEqual(flattenTree(buildTree([finished], F), new Set()).map((r) => r.session.id), ['run']);
});

test('hiding finished sessions leaves active, Failed and Interrupted ones', () => {
  const F = { projectId: null, text: '', showFinished: false };
  const interrupted = s('int', 'Interrupted', '2026-10-01T00:00:00Z');
  assert.deepEqual(visibleSessions([...all, interrupted], F).map((x) => x.id).sort(), ['failed', 'int', 'run']);
  assert.deepEqual(flattenTree(buildTree([...all, interrupted], F), new Set()).map((r) => r.session.id).sort(), ['failed', 'int', 'run']);
});

test('the open session stays listed when it finishes while finished sessions are hidden', () => {
  const F = { projectId: null, text: '', showFinished: false, keepId: 'done' };
  assert.ok(visibleSessions(all, F).some((x) => x.id === 'done'));
  assert.ok(flattenTree(buildTree(all, F), new Set()).some((r) => r.session.id === 'done'));
  // Another finished session is still hidden, and so is the open one once it is no longer open.
  const other = s('other', 'Completed', '2026-10-02T00:00:00Z');
  assert.ok(!flattenTree(buildTree([...all, other], F), new Set()).some((r) => r.session.id === 'other'));
  assert.ok(!flattenTree(buildTree(all, { ...F, keepId: null }), new Set()).some((r) => r.session.id === 'done'));
  assert.equal(passesFinished(done, false, 'done'), true);
  assert.equal(passesFinished(done, false, 'run'), false);
});

test('a kept finished worker keeps its lead listed above it', () => {
  const lead = s('lead', 'Running', '2026-10-03T00:00:00Z');
  const w = s('w', 'Completed', '2026-10-03T00:00:00Z', { parent_id: 'lead' });
  const rows = flattenTree(buildTree([lead, w], { projectId: null, text: '', showFinished: false, keepId: 'w' }), new Set());
  assert.deepEqual(rows.map((r) => `${r.depth}:${r.session.id}`), ['0:lead', '1:w']);
});

test('the empty text names Show completed only when finished sessions are hidden', () => {
  const base = { view: 'active', zone: 'main', total: 3, text: 'zzz', projectOn: true, finishedHidden: true, hidden: { finished: 0, archived: 0, otherProjects: 0 } };
  assert.equal(emptyListView(base).text, 'No sessions match \u201czzz\u201d. Clear the filter, choose All projects or tick Show completed.');
  assert.equal(emptyListView({ ...base, finishedHidden: false, projectOn: false }).text, 'No sessions match \u201czzz\u201d. Clear the filter.');
});


test('the page ships Show completed ticked, labelled and muted rows for finished sessions', () => {
  const html = readFileSync(here('../static/index.html'), 'utf8');
  assert.match(html, /<input id="show-finished" type="checkbox" checked> Show completed/);
  const css = readFileSync(here('../static/styles.css'), 'utf8');
  assert.match(css, /\.session-item\.st-Completed \.label \{ color: var\(--muted\); \}/);
});

// ---------- the rendered row (fake DOM, as sessionlist-keys.test.mjs) ----------

test('a Completed row is marked st-Completed, says Completed, and stays selected when it finishes', async () => {
  globalThis.document = { createElement: (t) => new El(t) };
  const { renderSessionTree } = await import('../dist/sessionTree.js');
  const finished = { ...run, status: 'Completed' };
  const rows = flattenTree(buildTree([finished], { projectId: null, text: '', showFinished: false, keepId: 'run' }), new Set());
  const ul = new El('ul');
  renderSessionTree(ul, rows, { projectName: () => undefined, onToggle() {}, selectedId: 'run', cursorId: 'run', now: Date.parse('2026-10-04T00:00:00Z') });
  assert.equal(ul.children.length, 1);
  const li = ul.children[0];
  assert.match(li.className, /\bst-Completed\b/);
  assert.match(li.className, /\bselected\b/);
  assert.equal(li.getAttribute('aria-selected'), 'true');
  assert.match(li.getAttribute('aria-label'), /Completed/);
});
