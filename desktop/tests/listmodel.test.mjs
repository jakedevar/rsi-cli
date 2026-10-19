import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  DEFAULT_SORT, LABEL_COLORS, PROJECT_COLORS, SORT_ORDERS, activityFirst, applyFold, ascendTarget, attentionQueue,
  attentionSlot, cycleColor, filterByName, findProject, labelBoundary, labelCounts, matchIndices, parentChoices,
  parseProjectArg, parseSortOrder, scopeHeader, sessionComparator, sessionInfoRows, stepMatch, validateLabel,
  validateProject,
} from '../dist/listmodel.js';
import { buildTree, flattenTree } from '../dist/tree.js';

const base = { provider: 'Claude', query: 'q', working_dir: '/w' };
const s = (id, status, updated_at, extra = {}) => ({ ...base, id, status, updated_at, created_at: '2026-10-01T00:00:00Z', ...extra });
const F = { projectId: null, text: '', showFinished: true };
const order = (id, labels = {}) => ({ cmp: sessionComparator(id, (g) => labels[g]), activityFirst: activityFirst(id) });
const rowIds = (sessions, id, labels) => flattenTree(buildTree(sessions, { ...F, order: order(id, labels) }), new Set()).map((r) => r.session.id);

// ---------- sort ----------

test('sort orders carry the TUI names in the TUI order', () => {
  assert.deepEqual(SORT_ORDERS.map((o) => o.label), ['Longest idle', 'Recently active', 'Oldest first', 'Newest first', 'Grouped by label']);
  assert.equal(DEFAULT_SORT, 'stalest');
  assert.equal(parseSortOrder('freshest'), 'freshest');
  assert.equal(parseSortOrder('nonsense'), 'stalest');
  assert.equal(parseSortOrder(null), 'stalest');
  assert.equal(scopeHeader('stalest', 20), '⇅ Longest idle · 20 sessions');
  assert.equal(scopeHeader('by-label', 1), '⇅ Grouped by label · 1 session');
});

test('each order sorts by the right timestamp', () => {
  const a = s('a', 'Completed', '2026-10-03T00:00:00Z', { created_at: '2026-09-01T00:00:00Z' });
  const b = s('b', 'Completed', '2026-10-01T00:00:00Z', { created_at: '2026-09-03T00:00:00Z' });
  const c = s('c', 'Completed', '2026-10-02T00:00:00Z', { created_at: '2026-09-02T00:00:00Z' });
  assert.deepEqual(rowIds([a, b, c], 'stalest'), ['b', 'c', 'a'], 'idle the longest at the top');
  assert.deepEqual(rowIds([a, b, c], 'freshest'), ['a', 'c', 'b']);
  assert.deepEqual(rowIds([a, b, c], 'oldest-created'), ['a', 'c', 'b']);
  assert.deepEqual(rowIds([a, b, c], 'newest-created'), ['b', 'c', 'a']);
});

test('needs-you and in-flight sections stay first except when grouped by label', () => {
  const quiet = s('quiet', 'Completed', '2026-10-01T00:00:00Z', { group_id: 'L1' });
  const live = s('live', 'Running', '2026-10-05T00:00:00Z');
  const need = s('need', 'WaitingApproval', '2026-10-06T00:00:00Z');
  assert.deepEqual(rowIds([quiet, live, need], 'stalest'), ['need', 'live', 'quiet']);
  assert.deepEqual(rowIds([quiet, live, need], 'by-label', { L1: 'Alpha' }), ['quiet', 'live', 'need'], 'labeled first, then ungrouped by idle time');
});

test('pins lead the newest-first orders and trail the oldest-first ones', () => {
  const pinned = s('p', 'Completed', '2026-10-02T00:00:00Z', { pinned_at: '2026-10-09T00:00:00Z' });
  const x = s('x', 'Completed', '2026-10-01T00:00:00Z');
  const y = s('y', 'Completed', '2026-10-03T00:00:00Z');
  assert.deepEqual(rowIds([x, pinned, y], 'freshest'), ['p', 'y', 'x']);
  assert.deepEqual(rowIds([x, pinned, y], 'stalest'), ['x', 'y', 'p']);
  const later = s('q', 'Completed', '2026-10-02T00:00:00Z', { pinned_at: '2026-10-10T00:00:00Z' });
  assert.deepEqual(rowIds([pinned, later], 'freshest'), ['q', 'p'], 'the latest pin leads');
});

test('grouping by label orders groups by label name, unlabeled last, stalest first inside', () => {
  const ss = [
    s('z1', 'Completed', '2026-10-02T00:00:00Z', { group_id: 'Lz' }),
    s('a2', 'Completed', '2026-10-03T00:00:00Z', { group_id: 'La' }),
    s('a1', 'Completed', '2026-10-01T00:00:00Z', { group_id: 'La' }),
    s('n1', 'Completed', '2026-10-01T00:00:00Z'),
  ];
  assert.deepEqual(rowIds(ss, 'by-label', { La: 'alpha', Lz: 'Zed' }), ['a1', 'a2', 'z1', 'n1']);
});

// ---------- label groups ----------

const rows = (sessions) => flattenTree(buildTree(sessions, { ...F, order: order('by-label', { A: 'a', B: 'b' }) }), new Set());
const grouped = rows([
  s('a1', 'Completed', '2026-10-01T00:00:00Z', { group_id: 'A' }),
  s('a2', 'Completed', '2026-10-02T00:00:00Z', { group_id: 'A' }),
  s('b1', 'Completed', '2026-10-01T00:00:00Z', { group_id: 'B' }),
  s('n1', 'Completed', '2026-10-01T00:00:00Z'),
]);

test(']g and [g land on the first row of the next or previous label group', () => {
  assert.equal(labelBoundary(grouped, 'a1', 1), 'b1');
  assert.equal(labelBoundary(grouped, 'a2', 1), 'b1');
  assert.equal(labelBoundary(grouped, 'b1', 1), 'n1');
  assert.equal(labelBoundary(grouped, 'n1', 1), null);
  assert.equal(labelBoundary(grouped, 'n1', -1), 'b1');
  assert.equal(labelBoundary(grouped, 'b1', -1), 'a1', 'start of the previous group, not its last row');
  assert.equal(labelBoundary(grouped, 'a1', -1), null);
  assert.equal(labelBoundary(grouped, null, 1), 'a1');
  assert.equal(labelBoundary([], 'x', 1), null);
});

test('label counts include an unlabeled bucket', () => {
  const c = labelCounts(grouped.map((r) => r.session));
  assert.equal(c.get('A'), 2);
  assert.equal(c.get(''), 1);
});

// ---------- folds and hierarchy ----------

const g = s('g', 'Completed', '2026-10-01T00:00:00Z', { session_kind: 'Group' });
const e = s('e', 'Completed', '2026-10-01T00:00:00Z', { session_kind: 'Epic', parent_id: 'g' });
const t1 = s('t1', 'Running', '2026-10-02T00:00:00Z', { parent_id: 'e' });
const solo = s('solo', 'Completed', '2026-10-01T00:00:00Z');
const containers = ['g', 'e'];

test('zo / zc / za fold the cursor row; zM and zR act on every container', () => {
  const none = new Set();
  assert.deepEqual([...applyFold('close', none, 'g', containers).collapsed], ['g']);
  assert.deepEqual([...applyFold('open', new Set(['g']), 'g', containers).collapsed], []);
  assert.deepEqual([...applyFold('toggle', none, 'e', containers).collapsed], ['e']);
  assert.deepEqual([...applyFold('toggle', new Set(['e']), 'e', containers).collapsed], []);
  const leaf = applyFold('close', none, 'solo', containers);
  assert.equal(leaf.applied, false, 'a leaf has nothing to fold');
  assert.equal(applyFold('close', none, null, containers).applied, false);
  assert.deepEqual([...applyFold('close-all', none, null, containers).collapsed].sort(), ['e', 'g'], 'zM closes the nested epic too');
  assert.deepEqual([...applyFold('open-all', new Set(['g', 'e']), null, containers).collapsed], []);
  assert.equal(applyFold('close-all', none, null, []).applied, false);
});

test('folding hides the subtree in the flattened rows', () => {
  const forest = buildTree([g, e, t1, solo], F);
  assert.deepEqual(flattenTree(forest, new Set()).map((r) => r.session.id), ['g', 'e', 't1', 'solo']);
  assert.deepEqual(flattenTree(forest, applyFold('close', new Set(), 'g', containers).collapsed).map((r) => r.session.id), ['g', 'solo']);
  assert.deepEqual(flattenTree(forest, applyFold('close-all', new Set(), null, containers).collapsed).map((r) => r.session.id), ['g', 'solo']);
});

test('ascend finds the parent row; parent choices exclude the session and its descendants', () => {
  const list = flattenTree(buildTree([g, e, t1, solo], F), new Set());
  assert.equal(ascendTarget(list, 't1'), 'e');
  assert.equal(ascendTarget(list, 'e'), 'g');
  assert.equal(ascendTarget(list, 'g'), null);
  assert.equal(ascendTarget(list, null), null);
  const all = [g, e, t1, solo];
  assert.deepEqual(parentChoices(all, 't1').map((c) => c.id), [null, 'g', 'e']);
  assert.equal(parentChoices(all, 't1').find((c) => c.id === 'e').current, true);
  assert.deepEqual(parentChoices(all, 'g').map((c) => c.id), [null], 'a Group cannot move under its own Epic');
  assert.deepEqual(parentChoices(all, 'e').map((c) => c.id), [null, 'g']);
  assert.equal(parentChoices(all, 'solo')[0].current, true);
});

// ---------- attention queue ----------

test('attention slots number the sessions that need you, in list order', () => {
  const ask = s('ask', 'Running', '2026-10-01T00:00:00Z', { pending_question: { questions: [{ question: 'ok?', header: 'h' }] } });
  const wait = s('wait', 'WaitingApproval', '2026-10-02T00:00:00Z');
  const list = flattenTree(buildTree([solo, wait, ask], { ...F, order: order('stalest') }), new Set());
  const q = attentionQueue(list);
  assert.deepEqual(q.map((x) => x.id), ['ask', 'wait']);
  assert.equal(attentionSlot(q, 1).id, 'ask');
  assert.equal(attentionSlot(q, 2).id, 'wait');
  assert.equal(attentionSlot(q, 3), null);
  assert.equal(attentionSlot(q, 0), null);
  assert.equal(attentionSlot(q, 1.5), null);
});

// ---------- search ----------

test('n / N step through matching events and wrap', () => {
  const m = matchIndices(['Fix the BUG', 'ok', 'a bug here', 'none', 'debug'], ' bug ');
  assert.deepEqual(m, [0, 2, 4]);
  assert.deepEqual(matchIndices(['x'], '  '), []);
  assert.equal(stepMatch(m, null, 1), 0);
  assert.equal(stepMatch(m, null, -1), 4);
  assert.equal(stepMatch(m, 0, 1), 2);
  assert.equal(stepMatch(m, 4, 1), 0, 'wraps forward');
  assert.equal(stepMatch(m, 2, -1), 0);
  assert.equal(stepMatch(m, 0, -1), 4, 'wraps backward');
  assert.equal(stepMatch([], 0, 1), null);
});

// ---------- projects and labels ----------

test('project and label forms validate before sending', () => {
  const ok = { name: 'Web', path: '/srv', color: '#89b4fa' };
  assert.equal(validateProject(ok), null);
  assert.match(validateProject({ ...ok, name: '  ' }), /Name is required/);
  assert.match(validateProject({ ...ok, color: 'blue' }), /color/);
  assert.match(validateProject({ ...ok, path: '/a\nb' }), /control/);
  assert.equal(validateLabel({ name: 'Urgent', description: '', color: '#cba6f7' }), null);
  assert.match(validateLabel({ name: '', description: '', color: '#cba6f7' }), /Name is required/);
});

test('palettes match the TUI forms and cycle with wrap', () => {
  assert.equal(PROJECT_COLORS.length, 8);
  assert.equal(PROJECT_COLORS[0].hex, '#89b4fa', 'projects default to blue');
  assert.equal(LABEL_COLORS[0].hex, '#cba6f7', 'labels default to mauve');
  assert.equal(cycleColor(PROJECT_COLORS, '#89b4fa', 1), '#a6e3a1');
  assert.equal(cycleColor(PROJECT_COLORS, '#89b4fa', -1), '#f5c2e7');
  assert.equal(cycleColor(PROJECT_COLORS, '#F5C2E7', 1), '#89b4fa');
  assert.equal(cycleColor(PROJECT_COLORS, '#123456', 1), '#89b4fa', 'an unknown color starts from the first');
});

test(':project-new splits name from path; :project finds by name', () => {
  assert.deepEqual(parseProjectArg(' web  /srv/web app '), { name: 'web', path: '/srv/web app' });
  assert.deepEqual(parseProjectArg('web'), { name: 'web', path: '' });
  assert.deepEqual(parseProjectArg(undefined), { name: '', path: '' });
  const ps = [{ id: '1', name: 'Web' }, { id: '2', name: 'Webhooks' }, { id: '3', name: 'API' }, { id: '4', name: 'Billing API' }];
  assert.equal(findProject(ps, 'web').project.id, '1', 'an exact name beats prefixes');
  assert.equal(findProject(ps, 'bill').project.id, '4');
  assert.equal(findProject(ps, 'ap').project.id, '3', 'a unique prefix wins over substrings');
  assert.match(findProject(ps, 'i').error, /matches 2 projects: API, Billing API/);
  assert.equal(findProject(ps, 'i').project, undefined);
  assert.match(findProject(ps, 'zzz').error, /No project/);
  assert.match(findProject(ps, ' ').error, /Name a project/);
});

test('picker filter is a case-insensitive name substring that keeps order', () => {
  const items = [{ n: 'Alpha' }, { n: 'beta' }, { n: 'Gamma ray' }];
  assert.deepEqual(filterByName(items, 'A', (i) => i.n).map((i) => i.n), ['Alpha', 'beta', 'Gamma ray']);
  assert.deepEqual(filterByName(items, ' RAY ', (i) => i.n).map((i) => i.n), ['Gamma ray']);
  assert.equal(filterByName(items, '', (i) => i.n).length, 3);
});

test('session info lists the TUI rows in order', () => {
  const sess = s('00000000-aaaa', 'Running', '2026-10-02T00:00:00Z', {
    title: 'T', model: 'm', project_id: 'p1', group_id: 'l1', parent_id: 'abcdef012345', rating: 7, tags: ['x', 'y'], description: 'D',
  });
  const rowsOut = sessionInfoRows(sess, [{ id: 'p1', name: 'Web' }], [{ id: 'l1', name: 'Urgent' }]);
  assert.deepEqual(rowsOut.map((r) => r[0]), ['title', 'id', 'kind', 'status', 'provider', 'model', 'dir', 'project', 'created', 'context', 'parent', 'rating', 'label', 'tags', 'description']);
  const get = (k) => rowsOut.find((r) => r[0] === k)[1];
  assert.equal(get('project'), 'Web');
  assert.equal(get('label'), 'Urgent');
  assert.equal(get('parent'), 'abcdef01');
  assert.equal(get('rating'), '7/10');
  assert.equal(get('tags'), 'x, y');
  const bare = sessionInfoRows(s('i', 'Completed', '2026-10-02T00:00:00Z'), [], []);
  assert.equal(bare.find((r) => r[0] === 'title')[1], '(untitled)');
  assert.equal(bare.find((r) => r[0] === 'rating')[1], 'unrated');
  assert.equal(bare.find((r) => r[0] === 'label')[1], 'none');
  assert.equal(bare.find((r) => r[0] === 'tags')[1], 'none');
  const grp = sessionInfoRows(s('k', 'Completed', '2026-10-02T00:00:00Z', { session_kind: 'Epic', lead_session_id: 'deadbeef1234' }), [], []);
  assert.equal(grp.find((r) => r[0] === 'lead')[1], 'deadbeef');
});
