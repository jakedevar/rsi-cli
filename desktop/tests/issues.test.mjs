import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import {
  ACTIONS, appMenuModel, contextMenuModel, keyHints, menuState, paletteEntries, rankPalette, unavailable,
} from '../dist/registry.js';
import { COMMAND_BINDINGS, KEY_BINDINGS } from '../dist/keybindings.js';
import { ISSUES_ACTIONS, ISSUES_RULES, isIssuesAction } from '../dist/actions/issues.js';
import {
  ISSUES_KEYS, applySavedView, canArchive, canEdit, canRestore, checkDraft, classifyFailure, copyText, createBody,
  defaultFilters, dependencyEdge, describeEvent, draftFromIssue, filterChips, findByNumber, idleKeys, listRequest, moveSelection,
  newWrite, nextSection, nextSort, numberJumpFilters, operatorOnly, parseIssueNumber, parsePriorities, parseProvenance,
  parseStatuses, patchFrom, priorityLabel, readinessLabel, rebase, reconcileSelection, resolveIssuesKey, rowSummary,
  statusTargets, toggleOperatorOnly, writeBlock, writeBody, ISSUES_CHORD_MS,
} from '../dist/issues-model.js';

const here = (p) => fileURLToPath(new URL(p, import.meta.url));
const ID = (n) => `00000000-0000-4000-8000-${String(n).padStart(12, '0')}`;

const issue = (over = {}) => ({
  id: ID(1), project_id: ID(99), display_number: 12, title: 'Fix the thing', body: 'Details', status: 'Open',
  priority: 2, labels: ['bug', 'ui'], assignee: 'ana', created_at: '2026-10-18T09:06:32.216383Z',
  updated_at: '2026-10-18T10:00:00Z', row_version: 3, archived_at: null, closed_at: null, ...over,
});
const row = (over = {}, extra = {}) => ({ issue: issue(over), readiness: 'ready', open_blocker_count: 0, dependent_count: 0, ...extra });
const page = (rows, next = null, prev = null) => ({ rows, next_cursor: next, previous_cursor: prev });

const snap = (issues, over = {}) => ({
  open: null, cursorOnRow: false, rowCount: 0, attentionRows: 0, view: 'active', vimKeys: true, hideTools: false,
  showFinished: false, modalOpen: false, event: null,
  issues: { active: true, hasProject: true, issue: issue(), nextPage: false, previousPage: false, operatorOnly: false, ...issues },
  ...over,
});

// ---------- registry integration ----------

test('every Issues action is registered, listed in the palette and the Issues menu', () => {
  const s = snap({});
  const entries = paletteEntries(s, 'other');
  for (const a of ISSUES_ACTIONS) {
    assert.ok(ACTIONS.some((x) => x.id === a.id), a.id);
    assert.ok(entries.some((e) => e.id === a.id), `${a.id} in the palette`);
    assert.equal(a.group, 'issues');
    assert.ok(isIssuesAction(a.id));
    assert.ok(typeof ISSUES_RULES[a.id] === 'function', `${a.id} has a rule`);
  }
  const menus = appMenuModel('other', s);
  const m = menus.find((x) => x.id === 'issues');
  assert.equal(m.text, 'Issues');
  const inMenu = m.items.filter((n) => n.kind === 'item').map((n) => n.id);
  for (const a of ISSUES_ACTIONS) assert.ok(inMenu.includes(`act:${a.id}`), `${a.id} in the menu`);
  assert.ok(menus.length <= 12 && m.items.length <= 64, 'within the Rust menu bounds');
  for (const a of ISSUES_ACTIONS) assert.ok(a.title.length <= 96 && !a.title.includes('&'));
});

test('the TUI entry points are bound: <Space>i, :issues and :issue', () => {
  assert.equal(KEY_BINDINGS['<Space>i'].action, 'issues-open');
  assert.equal(COMMAND_BINDINGS.issues.action, 'issues-open');
  assert.equal(COMMAND_BINDINGS.issue.action, 'issues-go-number');
  assert.deepEqual(keyHints('issues-open'), ['<Space>i', ':issues']);
  assert.deepEqual(keyHints('issues-go-number'), [':issue']);
  assert.deepEqual(keyHints('issues-cancel'), ['dd']);
  assert.deepEqual(keyHints('issues-copy-ref'), ['y#']);
});

test('every workspace key resolves to a registered action', () => {
  for (const [k, id] of Object.entries(ISSUES_KEYS)) assert.ok(isIssuesAction(id), `${k} -> ${id}`);
  for (const id of Object.values(ISSUES_KEYS)) {
    assert.ok(ISSUES_ACTIONS.find((a) => a.id === id)?.extraKeys?.length, `${id} shows its key`);
  }
});

test('outside the workspace only opening and :issue N are available', () => {
  const off = snap({ active: false, issue: null });
  assert.equal(unavailable('issues-open', off), null);
  assert.equal(unavailable('issues-go-number', off), null);
  assert.match(unavailable('issues-new', off), /Issues workspace first/);
  assert.match(unavailable('issues-edit', off), /Issues workspace first/);
  const none = { open: null, cursorOnRow: false, rowCount: 0, attentionRows: 0, view: 'active', vimKeys: true, hideTools: false, showFinished: false, modalOpen: false };
  assert.match(unavailable('issues-refresh', none), /Issues workspace first/);
  assert.equal(unavailable('issues-go-number', none), 'No project to look in yet');
});

test('write actions follow the Issue lifecycle and say why they cannot run', () => {
  const open = snap({ issue: issue() });
  for (const id of ['issues-edit', 'issues-priority', 'issues-assign', 'issues-labels', 'issues-status', 'issues-cancel']) {
    assert.equal(unavailable(id, open), null, id);
  }
  assert.match(unavailable('issues-archive', open), /Close or cancel/);
  assert.match(unavailable('issues-restore', open), /not archived/);
  const closed = snap({ issue: issue({ status: 'Closed' }) });
  assert.match(unavailable('issues-edit', closed), /reopen it first/);
  assert.equal(unavailable('issues-status', closed), null, 'reopen is a status change');
  assert.equal(unavailable('issues-archive', closed), null);
  const archived = snap({ issue: issue({ status: 'Cancelled', archived_at: '2026-10-18T11:00:00Z' }) });
  assert.match(unavailable('issues-edit', archived), /restore it first/);
  assert.match(unavailable('issues-status', archived), /Restore/);
  assert.equal(unavailable('issues-restore', archived), null);
  assert.equal(unavailable('issues-deps', archived), null, 'dependencies edit even for archived Issues');
  const empty = snap({ issue: null });
  assert.match(unavailable('issues-edit', empty), /Select an Issue/);
  assert.match(unavailable('issues-copy-uuid', empty), /Select an Issue/);
  assert.equal(unavailable('issues-new', empty), null);
  assert.match(unavailable('issues-new', snap({ hasProject: false })), /project/);
  assert.match(unavailable('issues-page-next', open), /last page/);
  assert.equal(unavailable('issues-page-next', snap({ nextPage: true })), null);
  assert.match(unavailable('issues-page-prev', open), /first page/);
  assert.match(unavailable('issues-edit', snap({ issue: issue() }, { modalOpen: true })), /dialog/);
});

test('menu check mark and the Issue row context menu come from the registry', () => {
  const on = menuState(appMenuModel('other', snap({ operatorOnly: true })));
  assert.equal(on['act:issues-operator'].checked, true);
  assert.equal(menuState(appMenuModel('other', snap({ operatorOnly: false })))['act:issues-operator'].checked, false);
  const ctx = contextMenuModel('issue-row', snap({}));
  const ids = ctx.filter((n) => n.kind === 'item').map((n) => n.id);
  for (const id of ['edit', 'status', 'priority', 'assign', 'labels', 'deps', 'cancel', 'archive', 'restore', 'copy-uuid', 'copy-ref']) {
    assert.ok(ids.includes(`ctx:issues-${id}`), id);
  }
  assert.ok(ids.every((i) => /^ctx:[a-z-]+$/.test(i)));
  const archived = contextMenuModel('issue-row', snap({ issue: issue({ status: 'Closed', archived_at: 'x' }) }));
  const state = Object.fromEntries(archived.filter((n) => n.kind === 'item').map((n) => [n.id, n.enabled]));
  assert.equal(state['ctx:issues-restore'], true);
  assert.equal(state['ctx:issues-edit'], false);
});

test('the palette finds Issues actions by title, key and :command', () => {
  const entries = paletteEntries(snap({}), 'other');
  const top = (q) => rankPalette(entries, q)[0]?.entry.id;
  assert.equal(top('issues work'), 'issues-open');
  assert.equal(top(':issues'), 'issues-open');
  assert.equal(top('dd'), 'issues-cancel');
  assert.equal(top('new issue'), 'issues-new');
  assert.equal(top('saved view ready'), 'issues-view-ready');
});

// ---------- filters and requests ----------

test('saved views start from clean filters; Mine needs a name', () => {
  assert.equal(applySavedView('ready', null).readiness, 'ready');
  assert.equal(applySavedView('blocked', null).readiness, 'blocked');
  assert.deepEqual(applySavedView('mine', null), { needs: 'assignee' });
  assert.equal(applySavedView('mine', 'ana').assignee, 'ana');
  assert.equal(applySavedView('recent', null).sort, 'updated_desc');
  assert.deepEqual(applySavedView('recent', null), defaultFilters());
});

test('the operator filter toggles exactly the two operator labels', () => {
  const f = defaultFilters();
  const on = toggleOperatorOnly(f);
  assert.deepEqual(on.labels_any, ['operator-request', 'operator-decision']);
  assert.ok(operatorOnly(on));
  assert.deepEqual(toggleOperatorOnly(on).labels_any, []);
  assert.deepEqual(filterChips(defaultFilters()), []);
  assert.ok(filterChips(on).includes('operator'));
});

test('filter chips show what is narrowing the list', () => {
  const f = { ...defaultFilters(), query: 'cache', statuses: ['Open', 'InProgress'], priorities: [1], readiness: 'blocked', assignee: 'ana', archive: 'All', sort: 'priority_asc' };
  const chips = filterChips(f);
  for (const c of ['“cache”', 'status Open|InProgress', 'priority P1', 'blocked', '@ana', 'incl. archived', 'by priority']) assert.ok(chips.includes(c), c);
});

test('page requests match the daemon contract', () => {
  const f = { ...defaultFilters(), assignee: 'ana', query: '  hello  ', labels_any: ['a'] };
  const start = listRequest(ID(99), f, { kind: 'start' });
  assert.equal(start.project_id, ID(99));
  assert.equal(start.direction, 'forward');
  assert.equal(start.limit, 64);
  assert.equal(start.assignee, 'ana');
  assert.equal(start.query, 'hello');
  assert.deepEqual(start.labels_any, ['a']);
  assert.ok(!('cursor' in start));
  const cursor = { kind: 'updated_desc', display_number: 5, issue_id: ID(5), updated_at: 'x' };
  const next = listRequest(ID(99), f, { kind: 'next', cursor });
  assert.equal(next.direction, 'forward');
  assert.deepEqual(next.cursor, cursor);
  const prev = listRequest(ID(99), f, { kind: 'previous', cursor });
  assert.equal(prev.direction, 'backward');
  assert.equal(listRequest(ID(99), f, { kind: 'end' }).direction, 'backward');
  const un = listRequest(ID(99), { ...f, unassigned: true }, { kind: 'start' });
  assert.ok(!('assignee' in un) && un.unassigned === true, 'assignee and unassigned never go together');
  const plain = listRequest(ID(99), defaultFilters(), { kind: 'start' });
  assert.ok(!('query' in plain) && !('labels_any' in plain));
});

test('go to #N searches every Issue, archived included', () => {
  const f = numberJumpFilters(2377);
  assert.equal(f.query, '#2377');
  assert.equal(f.archive, 'All');
  assert.equal(parseIssueNumber('#2377'), 2377);
  assert.equal(parseIssueNumber(' 12 '), 12);
  assert.equal(parseIssueNumber('issue 7'), 7);
  assert.equal(parseIssueNumber('0'), null);
  assert.equal(parseIssueNumber('abc'), null);
  const p = page([row({ display_number: 123, id: ID(2) }), row({ display_number: 12, id: ID(3) })]);
  assert.equal(findByNumber(p, 12).issue.id, ID(3), 'a #12 text search can also match #123');
  assert.equal(findByNumber(p, 99), undefined);
});

test('form text parsers forgive case and name what they could not read', () => {
  assert.deepEqual(parseStatuses('open, inprogress|CLOSED'), { value: ['Open', 'InProgress', 'Closed'], bad: [] });
  assert.deepEqual(parseStatuses('open nope'), { value: ['Open'], bad: ['nope'] });
  assert.deepEqual(parsePriorities('P1, 2 5'), { value: [1, 2], bad: ['5'] });
  assert.deepEqual(parseProvenance('Operator idea'), { value: ['operator', 'idea'], bad: [] });
});

// ---------- selection and paging ----------

test('j / k move by Issue id and cross a page edge only when the server has another page', () => {
  const p = page([row({ id: ID(1) }), row({ id: ID(2) })], { c: 'n' }, null);
  assert.deepEqual(moveSelection(p, ID(1), 1), { kind: 'select', id: ID(2) });
  assert.deepEqual(moveSelection(p, ID(2), 1), { kind: 'fetch', anchor: 'next' });
  assert.deepEqual(moveSelection(p, ID(1), -1), { kind: 'none' }, 'no previous page');
  assert.deepEqual(moveSelection(page(p.rows, null, { c: 'p' }), ID(1), -1), { kind: 'fetch', anchor: 'previous' });
  assert.deepEqual(moveSelection(p, null, 1), { kind: 'select', id: ID(1) });
  assert.deepEqual(moveSelection(p, null, -1), { kind: 'select', id: ID(2) });
  assert.deepEqual(moveSelection(p, ID(77), 1), { kind: 'select', id: ID(1) }, 'a vanished selection restarts at the edge');
  assert.deepEqual(moveSelection(page([]), null, 1), { kind: 'none' });
  assert.deepEqual(moveSelection(null, null, 1), { kind: 'none' });
});

test('after a page loads the selection stays on the same Issue when it is still there', () => {
  const p = page([row({ id: ID(1) }), row({ id: ID(2) }), row({ id: ID(3) })]);
  assert.equal(reconcileSelection(p, ID(2), 'first'), ID(2));
  assert.equal(reconcileSelection(p, ID(9), 'first'), ID(1));
  assert.equal(reconcileSelection(p, null, 'last'), ID(3));
  assert.equal(reconcileSelection(page([]), ID(1), 'first'), null);
});

// ---------- lifecycle ----------

test('status targets follow the shared lifecycle matrix', () => {
  assert.deepEqual(statusTargets(issue({ status: 'Open' })), ['InProgress', 'Closed', 'Cancelled']);
  assert.deepEqual(statusTargets(issue({ status: 'InProgress' })), ['Open', 'Closed', 'Cancelled']);
  assert.deepEqual(statusTargets(issue({ status: 'Closed' })), ['Open']);
  assert.deepEqual(statusTargets(issue({ status: 'Cancelled' })), ['Open']);
  assert.deepEqual(statusTargets(issue({ status: 'Closed', archived_at: 'x' })), []);
  assert.ok(canEdit(issue()) && !canEdit(issue({ status: 'Closed' })));
  assert.ok(canArchive(issue({ status: 'Closed' })) && !canArchive(issue()));
  assert.ok(canRestore(issue({ archived_at: 'x' })));
  assert.equal(writeBlock(null, 'edit'), 'Select an Issue first (j/k)');
  assert.equal(writeBlock(issue(), 'cancel'), null);
});

// ---------- drafts and patches ----------

test('a draft is validated and only the changed fields are patched', () => {
  const base = issue();
  const same = checkDraft(draftFromIssue(base));
  assert.ok(same.ok);
  assert.deepEqual(patchFrom(base, same.value), {}, 'no change, no patch');
  const edited = checkDraft({ ...draftFromIssue(base), title: ' New title ', priority: '', labels: 'ui, bug,ui', assignee: '' });
  assert.ok(edited.ok);
  assert.deepEqual(edited.value.labels, ['ui', 'bug']);
  assert.deepEqual(patchFrom(base, edited.value), { title: 'New title', labels: ['ui', 'bug'], priority: null, assignee: null });
  assert.deepEqual(patchFrom(base, { ...same.value, labels: ['bug', 'ui'] }), {}, 'same labels in the same order');
  for (const bad of [{ title: '  ' }, { priority: '5' }, { priority: 'x' }, { labels: Array.from({ length: 65 }, (_, i) => `l${i}`).join(',') }, { title: 'x'.repeat(1025) }]) {
    assert.equal(checkDraft({ ...draftFromIssue(base), ...bad }).ok, false, JSON.stringify(bad).slice(0, 30));
  }
  const c = checkDraft({ title: 'T', body: '', priority: '3', labels: '', assignee: '' });
  assert.deepEqual(createBody(c.value), { title: 'T', body: '', labels: [], priority: 3 });
  assert.equal(priorityLabel(null), '—');
  assert.equal(priorityLabel(1), 'P1');
});

// ---------- writes ----------

test('a retry sends the identical request; a rebase mints a new key against the latest version', () => {
  let n = 0;
  const mint = () => `key-${++n}`;
  const w = newWrite('update', issue({ row_version: 3 }), { patch: { title: 'X' } }, 'updated', mint);
  assert.deepEqual(writeBody(w), { patch: { title: 'X' }, expected_row_version: 3, idempotency_key: 'key-1' });
  assert.deepEqual(writeBody(w), writeBody(w), 'retrying reuses the exact request');
  const r = rebase(w, issue({ row_version: 5 }), mint);
  assert.deepEqual(writeBody(r), { patch: { title: 'X' }, expected_row_version: 5, idempotency_key: 'key-2' });
  assert.deepEqual(writeBody(w), { patch: { title: 'X' }, expected_row_version: 3, idempotency_key: 'key-1' }, 'the original is untouched');
  const c = newWrite('create', null, { title: 'N' }, 'created', mint);
  assert.deepEqual(writeBody(c), { title: 'N', idempotency_key: 'key-3' });
});

test('errors are sorted into stale, definitive and uncertain', () => {
  assert.equal(classifyFailure('daemon error -32000: row changed [stale_version]').kind, 'stale');
  const cycle = classifyFailure('daemon error -32000: cycle [dependency_cycle]');
  assert.deepEqual([cycle.kind, cycle.code], ['definitive', 'dependency_cycle']);
  assert.match(cycle.message, /cycle/);
  assert.equal(classifyFailure('cannot reach rsid: no socket').kind, 'uncertain');
  assert.equal(classifyFailure('daemon did not answer in time').kind, 'uncertain');
  assert.equal(classifyFailure('daemon i/o error: broken pipe').kind, 'uncertain');
  const rejected = classifyFailure('title is empty');
  assert.deepEqual([rejected.kind, rejected.message], ['definitive', 'title is empty']);
  assert.equal(classifyFailure('daemon error -32602: bad params').message, 'bad params');
});

test('dependency edges: blocked-by points this at the other, blocks points the other at this', () => {
  assert.deepEqual(dependencyEdge('blocked_by', ID(1), ID(2)), { issueId: ID(1), dependsOnId: ID(2) });
  assert.deepEqual(dependencyEdge('blocks', ID(1), ID(2)), { issueId: ID(2), dependsOnId: ID(1) });
  assert.equal(dependencyEdge('blocks', ID(1), ID(1)), null);
});

// ---------- keys ----------

test('workspace keys: single keys, gg / dd / yy / y# sequences, and expiry', () => {
  const run = (st, key, now = 0) => resolveIssuesKey(st, key, now);
  assert.deepEqual(run(idleKeys(), 'j').result, { kind: 'run', action: 'issues-down' });
  assert.deepEqual(run(idleKeys(), 'n').result, { kind: 'run', action: 'issues-new' });
  assert.deepEqual(run(idleKeys(), 'G').result, { kind: 'run', action: 'issues-last' });
  assert.deepEqual(run(idleKeys(), 'Escape').result, { kind: 'run', action: 'issues-close' });
  assert.deepEqual(run(idleKeys(), 'Tab').result, { kind: 'run', action: 'issues-section' });
  const d = run(idleKeys(), 'd');
  assert.deepEqual(d.result, { kind: 'pending', pending: 'd' });
  assert.deepEqual(run(d.next, 'd', 100).result, { kind: 'run', action: 'issues-cancel' });
  const y = run(idleKeys(), 'y', 0);
  assert.deepEqual(run(y.next, 'y', 10).result, { kind: 'run', action: 'issues-copy-uuid' });
  assert.deepEqual(run(y.next, '#', 10).result, { kind: 'run', action: 'issues-copy-ref' });
  const g = run(idleKeys(), 'g', 0);
  assert.deepEqual(run(g.next, 'g', 10).result, { kind: 'run', action: 'issues-first' });
  assert.deepEqual(run(g.next, 'j', 10).result, { kind: 'run', action: 'issues-down' }, 'a broken sequence falls to the new key');
  assert.deepEqual(run(g.next, 'g', ISSUES_CHORD_MS + 1).result, { kind: 'pending', pending: 'g' }, 'an old prefix does not pair up');
  assert.deepEqual(run(idleKeys(), 'z').result, { kind: 'none' }, 'unknown keys go on to the global engine');
  assert.deepEqual(run(idleKeys(), '?').result, { kind: 'none' });
  assert.deepEqual(run(idleKeys(), ':').result, { kind: 'none' });
  assert.deepEqual(run(idleKeys(), ']').result, { kind: 'run', action: 'issues-tab-next' }, 'G2: ] and [ switch the Local / Dispatched / Sync tabs');
});

// ---------- display ----------

test('rows and events read as one line', () => {
  assert.equal(rowSummary(row()), '#12 P2 Open  Fix the thing');
  assert.equal(rowSummary(row({ status: 'InProgress', archived_at: 'x', priority: null })), '#12 — In progress (archived)  Fix the thing');
  assert.equal(readinessLabel(row({}, { readiness: 'blocked', open_blocker_count: 2 })), 'blocked by 2');
  assert.equal(readinessLabel(row({}, { readiness: 'not_eligible' })), '');
  assert.equal(
    describeEvent({ sequence: 4, operation: 'status_updated', actor_label: 'operator', occurred_at: '2026-10-18T09:06:32Z', resulting_row_version: 5, issue: issue() }),
    '4. status updated by operator → v5 · 2026-10-18 09:06',
  );
  assert.equal(nextSort('updated_desc'), 'display_number_asc');
  assert.equal(nextSort('priority_asc'), 'updated_desc');
  assert.equal(nextSection('blocked_by'), 'blocks');
  assert.equal(nextSection('handoffs'), 'blocked_by');
  assert.deepEqual(copyText(issue(), 'ref'), { text: '#12', toast: 'issue #12 copied' });
  assert.equal(copyText(issue(), 'uuid').text, ID(1));
});

// ---------- Rust side ----------

test('the Rust backend registers every command the adapter calls', () => {
  const api = readFileSync(here('../src/issues-api.ts'), 'utf8');
  const lib = readFileSync(here('../src-tauri/src/handlers.generated.rs'), 'utf8');
  const called = [...api.matchAll(/invoke<[^>]*(?:<[^>]*>)?[^>]*>\('([a-z_]+)'/g)].map((m) => m[1]);
  assert.ok(called.length >= 12, `found ${called.length} commands`);
  for (const c of called) assert.ok(lib.includes(`issues::${c},`), `${c} is registered`);
});
