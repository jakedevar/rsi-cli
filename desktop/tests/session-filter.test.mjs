// The session filter (local #272): it matches the prompt as the TUI does, counts the matches that Show completed
// and the archive hide and offers them, words its empty text per tab, and stays cheap while typing.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import {
  emptyListView, filterDelayMs, filterRenderPlan, matchesFilterText, visibleSessions,
} from '../dist/model.js';
import { buildTree, countHiddenMatches, countZoneSessions, flattenTree } from '../dist/tree.js';

const here = (p) => fileURLToPath(new URL(p, import.meta.url));
const base = { provider: 'Claude', working_dir: '/w', created_at: '2026-10-01T00:00:00Z', updated_at: '2026-10-02T00:00:00Z' };
const s = (id, status, extra = {}) => ({ ...base, id, status, query: `prompt of ${id}`, ...extra });
const none = { finished: 0, archived: 0, otherProjects: 0 };
const F = { projectId: null, text: '', showFinished: false };
const ids = (rows) => rows.map((r) => r.session.id);

// ---------- the matcher ----------

test('the filter matches the prompt, not only the label, id prefix and provider', () => {
  const parser = s('p1', 'Completed', { title: 'Invoice parser cleanup', query: 'export invoices to csv\nthen tidy' });
  assert.equal(matchesFilterText(parser, 'csv'), true, 'a word from the prompt');
  assert.equal(matchesFilterText(parser, 'tidy'), true, 'a later prompt line');
  assert.equal(matchesFilterText(parser, 'parser'), true, 'the title');
  assert.equal(matchesFilterText(parser, 'claude'), true, 'the provider');
  assert.equal(matchesFilterText(parser, 'p1'), true, 'the id prefix');
  assert.equal(matchesFilterText(parser, 'spreadsheet'), false);
  assert.equal(matchesFilterText(parser, ''), true);
});

test('the match is case-insensitive and the prompt is searched from a fresh object each time', () => {
  const a = s('a', 'Running', { query: 'Fix the FLAKY Test' });
  assert.equal(matchesFilterText(a, 'flaky test'), true);
  const b = { ...a, query: 'something else' };
  assert.equal(matchesFilterText(b, 'flaky'), false, 'a refetched session is read again');
});

test('only the start of a very long prompt is searched', () => {
  const huge = s('h', 'Running', { query: `${'x'.repeat(30000)} needle` });
  assert.equal(matchesFilterText(huge, 'needle'), false);
  assert.equal(matchesFilterText(huge, 'xxxx'), true);
});

test('visibleSessions and the tree find a Completed session by a word from its prompt', () => {
  const run = s('run', 'Running', { title: 'Build' });
  const done = s('done', 'Completed', { title: 'Invoice parser cleanup', query: 'export invoices to csv' });
  const f = { projectId: null, text: 'csv', showFinished: true };
  assert.deepEqual(visibleSessions([run, done], f).map((x) => x.id), ['done']);
  assert.deepEqual(ids(flattenTree(buildTree([run, done], f), new Set())), ['done']);
});

test('the Archived tab finds an archived session by its prompt', () => {
  const old = s('old', 'Archived', { title: 'Old invoice export', query: 'export invoices to csv' });
  const other = s('other', 'Archived', { query: 'something unrelated' });
  const rows = flattenTree(buildTree([old, other], { ...F, text: 'csv', mode: 'archived' }), new Set());
  assert.deepEqual(ids(rows), ['old']);
});

// ---------- hidden matches ----------

test('matches hidden by Show completed and by the archive are counted', () => {
  const run = s('run', 'Running', { query: 'invoice work' });
  const d1 = s('d1', 'Completed', { query: 'invoice one' });
  const d2 = s('d2', 'Completed', { query: 'invoice two' });
  const d3 = s('d3', 'Completed', { query: 'unrelated' });
  const arc = s('arc', 'Archived', { query: 'invoice archive' });
  const f = { ...F, text: 'invoice' };
  assert.deepEqual(countHiddenMatches([run, d1, d2, d3], [arc], f), { finished: 2, archived: 1, otherProjects: 0 });
  assert.deepEqual(countHiddenMatches([run, d1, d2, d3], [arc], { ...f, showFinished: true }), { finished: 0, archived: 1, otherProjects: 0 });
});

test('the open session is not counted as hidden, an archived session in both lists counts once', () => {
  const d1 = s('d1', 'Completed', { query: 'invoice' });
  const arc = s('arc', 'Archived', { query: 'invoice' });
  const f = { ...F, text: 'invoice', keepId: 'd1' };
  assert.deepEqual(countHiddenMatches([d1, arc], [arc], f), { finished: 0, archived: 1, otherProjects: 0 });
});

test('with a project chosen, matches in other projects are counted', () => {
  const mine = s('mine', 'Running', { project_id: 'P1', query: 'invoice' });
  const theirs = s('theirs', 'Running', { project_id: 'P2', query: 'invoice' });
  const hiddenDone = s('hd', 'Completed', { project_id: 'P2', query: 'invoice' });
  const f = { ...F, text: 'invoice', projectId: 'P1' };
  assert.deepEqual(countHiddenMatches([mine, theirs, hiddenDone], [], f), { finished: 0, archived: 0, otherProjects: 1 });
});

test('the Jobs zone counts finished matches in its own zone and offers no archived hint', () => {
  const job = s('job', 'Completed', { scheduled_job_id: 'sj1', query: 'nightly report' });
  const plain = s('plain', 'Completed', { query: 'nightly report' });
  const arc = s('arc', 'Archived', { query: 'nightly report' });
  const f = { ...F, text: 'nightly', zone: 'jobs' };
  // The Jobs zone shows every status, so a finished job is not hidden.
  assert.deepEqual(countHiddenMatches([job, plain], [arc], f), { finished: 0, archived: 0, otherProjects: 0 });
  assert.deepEqual(countHiddenMatches([job, plain], [arc], { ...f, zone: 'main' }), { finished: 1, archived: 1, otherProjects: 0 });
});

test('a tab total counts what the tab could list with no filter on', () => {
  const job = s('job', 'Running', { scheduled_job_id: 'sj1' });
  const plain = s('plain', 'Completed');
  const arc = s('arc', 'Archived');
  assert.equal(countZoneSessions([job, plain, arc], { zone: 'jobs', mode: 'active' }), 1);
  assert.equal(countZoneSessions([job, plain, arc], { zone: 'main', mode: 'active' }), 1);
  assert.equal(countZoneSessions([arc, arc], { zone: 'main', mode: 'archived' }), 2);
});

// ---------- the empty text per tab ----------

const E = { view: 'active', zone: 'main', total: 5, text: '', projectOn: false, finishedHidden: false, hidden: none };

test('hidden matches are counted in the text and offered as buttons', () => {
  const v = emptyListView({ ...E, text: 'invoice', finishedHidden: true, hidden: { finished: 1, archived: 1, otherProjects: 0 } });
  assert.equal(v.text, 'No sessions match “invoice”. Also found: 1 completed session hidden, 1 archived session.');
  assert.deepEqual(v.actions.map((a) => a.id), ['show-finished', 'show-archived', 'clear-filter']);
  assert.deepEqual(v.actions.map((a) => a.label), ['Show completed', 'Show archived', 'Clear filter']);
  const many = emptyListView({ ...E, text: 'x', finishedHidden: true, hidden: { finished: 3, archived: 2, otherProjects: 4 }, projectOn: true });
  assert.match(many.text, /3 completed sessions hidden, 2 archived sessions, 4 matches in other projects/);
  assert.deepEqual(many.actions.map((a) => a.id), ['show-finished', 'show-archived', 'all-projects', 'clear-filter']);
});

test('the empty text names only the filters that are on', () => {
  assert.equal(emptyListView({ ...E, text: 'zzz' }).text, 'No sessions match “zzz”. Clear the filter.');
  assert.equal(emptyListView({ ...E, text: 'zzz', projectOn: true }).text, 'No sessions match “zzz”. Clear the filter or choose All projects.');
  assert.equal(emptyListView({ ...E, text: 'zzz', projectOn: true, finishedHidden: true }).text,
    'No sessions match “zzz”. Clear the filter, choose All projects or tick Show completed.');
  assert.equal(emptyListView({ ...E, finishedHidden: true }).text, 'No sessions to show. Tick Show completed.');
  assert.equal(emptyListView({ ...E, projectOn: true }).text, 'No sessions to show. Choose All projects.');
  assert.deepEqual(emptyListView({ ...E, text: 'zzz' }).actions.map((a) => a.id), ['clear-filter']);
  assert.deepEqual(emptyListView({ ...E, projectOn: true }).actions, []);
});

test('the Jobs tab has its own empty text', () => {
  const jobs = { ...E, zone: 'jobs', total: 0 };
  assert.equal(emptyListView(jobs).text, 'No scheduled-job sessions yet. Sessions that scheduled jobs launch appear here.');
  assert.deepEqual(emptyListView(jobs).actions, []);
  // Plain sessions exist but no job session does: still the Jobs wording, not the Main tab's.
  assert.match(emptyListView({ ...jobs, total: 0, text: 'nightly' }).text, /^No scheduled-job sessions yet/);
  const filtered = emptyListView({ ...E, zone: 'jobs', total: 2, text: 'nightly' });
  assert.equal(filtered.text, 'No scheduled-job sessions match “nightly”. Clear the filter.');
});

test('the Archived tab words its own empty text', () => {
  assert.equal(emptyListView({ ...E, view: 'archived', total: 0 }).text, 'No archived sessions.');
  const f = emptyListView({ ...E, view: 'archived', text: 'csv' });
  assert.equal(f.text, 'No archived sessions match “csv”. Clear the filter.');
  assert.deepEqual(f.actions.map((a) => a.id), ['clear-filter']);
});

test('a long filter text is shortened in the message', () => {
  const v = emptyListView({ ...E, text: 'y'.repeat(100) });
  assert.ok(v.text.includes(`${'y'.repeat(39)}…`));
  assert.ok(v.text.length < 140);
});

test('no sessions at all keeps the first-run next step', () => {
  assert.equal(emptyListView({ ...E, total: 0, text: 'abc' }).text, 'No sessions yet. Click New session to start one.');
});

// ---------- typing stays responsive ----------

test('a short list re-renders on every key; a long one renders the first key and coalesces a burst', () => {
  assert.equal(filterDelayMs(100), 0);
  assert.ok(filterDelayMs(3000) > 0 && filterDelayMs(3000) <= 150, 'a pause short enough to feel instant');
  assert.equal(filterRenderPlan(100, 0, false), 'now');
  assert.equal(filterRenderPlan(100, 0, true), 'now');
  assert.equal(filterRenderPlan(3000, 5000, false), 'now', 'first key after a pause: no added latency');
  assert.equal(filterRenderPlan(3000, 10, false), 'later', 'right after a render');
  assert.equal(filterRenderPlan(3000, 5000, true), 'later', 'a wait is already pending');
});

// Deterministic, not wall-clock (#3722): a millisecond threshold flaked under the parallel suite. What keeps a
// key cheap is that each session's lower-cased haystack is built once and reused, so the work is counted:
// the prompt is read once per session on the first pass and never again for the keys that follow.
test('matching 3,000 sessions with long prompts builds each prompt haystack once, then every key reuses it', () => {
  let promptReads = 0;
  const plain = []; // the same text read by hand, so the expectations never touch the counted getter
  const all = Array.from({ length: 3000 }, (_, i) => {
    const query = `task ${i} please refactor the module and write unit tests. `.repeat(10);
    const sess = s(`id${i}`, i % 2 ? 'Completed' : 'Running', { title: `Session ${i} for task ${i * 7}` });
    plain.push({ id: `id${i}`, text: `${sess.title} ${query} claude`.toLowerCase() });
    Object.defineProperty(sess, 'query', { enumerable: true, get() { promptReads += 1; return query; } });
    return sess;
  });
  const f = { ...F, showFinished: true };
  const count = (nodes) => nodes.reduce((n, x) => n + 1 + count(x.children), 0);
  const expected = (text) => plain.filter((x) => x.text.includes(text) || x.id.startsWith(text)).length;

  assert.equal(count(buildTree(all, { ...f, text: 'warm' })), 0);
  assert.equal(promptReads, 3000, 'the first pass reads each prompt once');

  promptReads = 0;
  for (const text of ['t', 'ta', 'tas', 'task', 'task 7', 'task 77', 'task 770']) {
    assert.equal(count(buildTree(all, { ...f, text })), expected(text), `matches for "${text}"`);
  }
  assert.equal(promptReads, 0, 'later keys reuse the cached haystack and never re-read a prompt');
  const byHand = expected('task 770');
  assert.ok(byHand > 0 && count(buildTree(all, { ...f, text: 'task 770' })) === byHand, 'the tree keeps exactly the sessions whose title or prompt has the text');
});

// ---------- the rendered empty row (fake DOM, as finished-sessions.test.mjs) ----------

class El {
  constructor(tag) {
    this.tagName = tag.toUpperCase(); this.attrs = new Map(); this.children = []; this.dataset = {};
    this.style = { setProperty() {} }; this.className = ''; this.textContent = ''; this.title = ''; this.id = ''; this.tabIndex = -1;
    this.listeners = new Map();
  }
  setAttribute(k, v) { this.attrs.set(k, String(v)); }
  getAttribute(k) { return this.attrs.has(k) ? this.attrs.get(k) : null; }
  removeAttribute(k) { this.attrs.delete(k); }
  append(...n) { this.children.push(...n); }
  prepend(...n) { this.children.unshift(...n); }
  replaceChildren(...n) { this.children = n; }
  addEventListener(t, fn) { this.listeners.set(t, fn); }
}

test('the empty row shows the message and one button per offer, and a click runs the action', async () => {
  globalThis.document = { createElement: (t) => new El(t) };
  const { renderSessionTree } = await import('../dist/sessionTree.js');
  const view = emptyListView({ ...E, text: 'invoice', finishedHidden: true, hidden: { finished: 1, archived: 1, otherProjects: 0 } });
  const picked = [];
  const ul = new El('ul');
  renderSessionTree(ul, [], { projectName: () => undefined, onToggle() {}, selectedId: null, emptyText: view.text, emptyActions: view.actions, onEmptyAction: (id) => picked.push(id) });
  const li = ul.children[0];
  assert.equal(li.className, 'session-empty');
  assert.equal(li.children[0].textContent, view.text);
  const buttons = li.children[1].children;
  assert.deepEqual(buttons.map((b) => b.textContent), ['Show completed', 'Show archived', 'Clear filter']);
  assert.deepEqual(buttons.map((b) => b.dataset.empty), ['show-finished', 'show-archived', 'clear-filter']);
  assert.ok(buttons.every((b) => b.tagName === 'BUTTON' && b.type === 'button'));
  buttons[1].listeners.get('click')();
  assert.deepEqual(picked, ['show-archived']);
});

test('an empty row with no offers has no button bar', async () => {
  globalThis.document = { createElement: (t) => new El(t) };
  const { renderSessionTree } = await import('../dist/sessionTree.js');
  const ul = new El('ul');
  renderSessionTree(ul, [], { projectName: () => undefined, onToggle() {}, selectedId: null, emptyText: 'No archived sessions.' });
  assert.equal(ul.children[0].children.length, 1);
  assert.equal(ul.children[0].children[0].textContent, 'No archived sessions.');
});

test('the page wires the filter box to the debounced handler and flushes it on Enter or blur', () => {
  const main = readFileSync(here('../src/main.ts'), 'utf8');
  assert.match(main, /\$\('search'\)\.addEventListener\('input', onFilterInput\)/);
  assert.match(main, /\$\('search'\)\.addEventListener\('change', flushFilter\)/);
  assert.match(main, /\$\('search'\)\.addEventListener\('blur', flushFilter\)/);
});
