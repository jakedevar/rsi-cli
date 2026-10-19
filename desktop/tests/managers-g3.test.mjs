// Gap sweep G3 (hub #3000): the manager overlay rows the sweep finishes — the
// Board's KPI line, Shift-Tab and detail paging, the policy form's presets,
// suggestions, Advanced, list editors and launch picker.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  KPI_OVERVIEW_PAGES, POLICY_FIELDS, PRESETS, VIEW_KEYS, hasAllKpis, kpiLines, kpiRows, launchCatalog, launchText,
  parseLaunch, parseListField, pickLaunches, policyValue,
} from '../dist/managers-model.js';

test('the Board KPI line reads program and product rows like board.rs', () => {
  const page = { rows: [
    { type: 'overview', kind: 'program', accepted: 3, integrated: 2, denominator: 5, ready: 1, partial: 0, unknown: 1, missing_work_scope: false },
    { type: 'lead', kind: 'program' },
    { payload: { type: 'overview', kind: 'product', accepted: 1, integrated: null, denominator: 4, ready: 0, partial: 2, unknown: 0, missing_work_scope: true } },
  ] };
  const kpis = kpiRows(page);
  assert.equal(kpis.length, 2);
  assert.equal(hasAllKpis(kpis), true);
  assert.equal(hasAllKpis(kpis.slice(0, 1)), false);
  assert.deepEqual(kpiLines(kpis, false), [
    'program accepted 3 · integrated 2 / 5 · ready 1 · partial 0 · unknown 1',
    'product accepted 1 · integrated unknown / 4 · ready 0 · partial 2 · unknown 0 · missing work scope',
  ]);
  assert.match(kpiLines([{ kind: 'program' }], false)[0], / · work scope unknown$/);
  assert.deepEqual(kpiLines([], false), ['KPIs unknown: the daemon returned no overview KPI rows.']);
  assert.equal(kpiLines([], true)[0], `KPIs beyond ${KPI_OVERVIEW_PAGES} Overview pages not loaded · Enter on a SIGNALS row, then n pages Overview`);
  assert.deepEqual(kpiRows(null), []);
});

test('the board takes Shift-Tab and Page Up / Page Down', () => {
  assert.equal(VIEW_KEYS.board['S-Tab'], 'mgr-section-prev');
  assert.equal(VIEW_KEYS.board.Tab, 'mgr-section-next');
  assert.equal(VIEW_KEYS.board.PageDown, 'mgr-detail-down');
  assert.equal(VIEW_KEYS.board.PageUp, 'mgr-detail-up');
});

test('the policy form has the TUI action rows and edits every list field', () => {
  const kinds = Object.fromEntries(POLICY_FIELDS.map((f) => [f.key, f.kind]));
  assert.equal(POLICY_FIELDS[0].kind, 'preset');
  assert.equal(kinds.suggest, 'suggest');
  assert.equal(kinds.advanced, 'advanced');
  for (const k of ['paused_epic_ids', 'group_ids', 'provider_limits', 'daemon_setting_bounds']) assert.equal(kinds[k], 'json', k);
  assert.equal(kinds.allowed_launches, 'launches');
  assert.ok(!POLICY_FIELDS.some((f) => f.kind === 'kept'));
  assert.deepEqual(PRESETS.map((p) => p.id), ['observe', 'execute', 'full']);
  const launches = POLICY_FIELDS.find((f) => f.key === 'allowed_launches');
  assert.equal(policyValue({ allowed_launches: [] }, launches), 'any valid choice');
  assert.equal(policyValue({ allowed_launches: [{ provider: 'Codex', model: 'm', effort: null }] }, launches), 'Codex/m');
  assert.equal(policyValue({}, POLICY_FIELDS[0]), '');
});

test('launch spellings parse like portfolio_command.rs', () => {
  assert.deepEqual(parseLaunch('codex/gpt-6-astra/xhigh'), { provider: 'Codex', model: 'gpt-6-astra', effort: 'xhigh' });
  assert.deepEqual(parseLaunch('OpenRouter/openai/gpt-x'), { provider: 'OpenRouter', model: 'openai/gpt-x', effort: null });
  assert.equal(parseLaunch('nope/x'), null);
  assert.equal(parseLaunch('Claude'), null);
  assert.equal(parseLaunch('Claude/'), null);
  assert.equal(launchText({ provider: 'Claude', model: 'm', effort: null }), 'Claude/m/any effort');
});

test('the launch picker merges the catalog, the checks and typed spellings', () => {
  const a = { provider: 'Claude', model: 'claude-opus-5-5', effort: 'high' };
  const b = { provider: 'Codex', model: 'gpt-6-astra', effort: 'high' };
  const catalog = launchCatalog([a], [a, b], [{ ...b, effort: undefined }]);
  assert.equal(catalog.length, 3);
  assert.deepEqual(pickLaunches(catalog, [launchText(b)], ''), { launches: [b] });
  assert.deepEqual(pickLaunches(catalog, [launchText(a)], 'codex/gpt-6-astra/high, Local/llama'), {
    launches: [a, b, { provider: 'Local', model: 'llama', effort: null }],
  });
  assert.deepEqual(pickLaunches(catalog, [], 'bad'), { error: 'Invalid launch bad; use Provider/model[/effort].' });
  assert.deepEqual(pickLaunches(catalog, [], ''), { launches: [] });
});

test('list fields are typed as JSON lists', () => {
  assert.deepEqual(parseListField('["a","b"]', 'Paused Epics'), { value: ['a', 'b'] });
  assert.deepEqual(parseListField('  ', 'Paused Epics'), { value: [] });
  assert.deepEqual(parseListField('{"a":1}', 'Group grants'), { error: 'Group grants must be a JSON list' });
  assert.match(parseListField('[', 'Group grants').error, /^Group grants: /);
});

// ---------- manager tree: a / e / A / m on global, portfolio and area rows ----------

import {
  NODE_FIELDS, capsFromForm, capsOf, editorProjects, grantEditReview, nodeEditValues, treeActionBlock,
} from '../dist/managers-model.js';

const P1 = 'aaaaaaaa-1111-4111-8111-111111111111';
const P2 = 'bbbbbbbb-2222-4222-8222-222222222222';
const SEAT = 'cccccccc-3333-4333-8333-333333333333';
const CAND = { id: 'dddddddd-4444-4444-8444-444444444444', project_id: P1, archived: false, leaf: true };
const grant = { grant_version: 3, capabilities: [], max_active_sessions: 4, max_created_sessions: 8, max_created_containers: 0, max_direct_reports: 2, max_spend_usd: null, reserved: [] };
const load = { running_workers: 0, direct_reports: 0, pending_escalations: 0, pending_decisions: 0 };
const trow = (kind, over = {}) => ({ key: `${kind}:x`, parent_key: null, depth: 0, kind, label: kind, project_id: P1, node_id: P2, epic_id: null, scope: null, seat: null, focus_session_id: SEAT, grant, load, complete: true, ...over });

test('global and portfolio rows take a, e and (portfolio) A and m', () => {
  for (const kind of ['global', 'portfolio']) {
    assert.equal(treeActionBlock(trow(kind), 'appoint', CAND), null, kind);
    assert.match(treeActionBlock(trow(kind), 'appoint', null), /Select the session/);
    assert.match(treeActionBlock(trow(kind), 'appoint', { ...CAND, id: SEAT }), /already holds/);
    // A portfolio seat may sit in any project: no project check.
    assert.equal(treeActionBlock(trow(kind), 'appoint', { ...CAND, project_id: P2 }), null);
    assert.equal(treeActionBlock(trow(kind), 'edit', null), null);
    assert.match(treeActionBlock(trow(kind, { grant: null }), 'edit', null), /no active grant/);
  }
  assert.equal(treeActionBlock(trow('portfolio'), 'above', CAND), null);
  assert.match(treeActionBlock(trow('portfolio'), 'above', { ...CAND, archived: true }), /archived/);
  assert.equal(treeActionBlock(trow('portfolio'), 'move', null), null);
  for (const kind of ['global', 'project', 'area']) assert.match(treeActionBlock(trow(kind), 'move', null), /Only a portfolio row/);
  assert.match(treeActionBlock(trow('portfolio', { grant: null }), 'above', CAND), /no active grant/);
});

test('an area row edits a childless granted node and never moves its seat', () => {
  assert.equal(treeActionBlock(trow('area'), 'edit', null), null);
  assert.match(treeActionBlock(trow('area', { grant: null }), 'edit', null), /revoked/);
  assert.match(treeActionBlock(trow('area', { load: { ...load, direct_reports: 2 } }), 'edit', null), /2 direct report/);
  assert.match(treeActionBlock(trow('area'), 'appoint', CAND), /No RPC moves an area seat/);
  assert.match(treeActionBlock(trow('epic'), 'above', CAND), /Epic rows/);
});

test('the grant editor reads caps within the policy bounds', () => {
  const caps = capsOf({ max_active_sessions: 4, max_created_sessions: 64, max_created_containers: 2, max_spend_usd: 10, allow_create_groups: true });
  assert.deepEqual(caps, { max_active_sessions: 4, max_created_sessions: 64, max_created_containers: 2, max_spend_usd: 10, allow_create_groups: true });
  const form = { max_active_sessions: '6', max_created_sessions: '0', max_created_containers: '64', max_spend_usd: '$12.5', allow_create_groups: false };
  assert.deepEqual(capsFromForm(form), { max_active_sessions: 6, max_created_sessions: 0, max_created_containers: 64, max_spend_usd: 12.5, allow_create_groups: false });
  assert.equal(capsFromForm({ ...form, max_spend_usd: '' }).max_spend_usd, null);
  assert.equal(capsFromForm({ ...form, max_spend_usd: 'none' }).max_spend_usd, null);
  assert.match(capsFromForm({ ...form, max_active_sessions: '0' }).error, /max active sessions must be a whole number 1-100/);
  assert.match(capsFromForm({ ...form, max_created_containers: '65' }).error, /0-64/);
  assert.match(capsFromForm({ ...form, max_spend_usd: '-3' }).error, /positive USD/);
});

test('the grant editor lists known projects and unknown granted ids, by name', () => {
  const list = editorProjects([{ id: P2, name: 'zeta' }, { id: P1, name: 'Alpha' }], [P1, SEAT]);
  assert.deepEqual(list.map((p) => [p.name, p.granted]), [['Alpha', true], ['cccccccc', true], ['zeta', false]]);
});

test('a grant edit review names the change and flags narrowing', () => {
  const projects = [{ id: P1, name: 'rsi' }, { id: P2, name: 'docs' }];
  const caps = { max_active_sessions: 4, max_created_sessions: 8, max_created_containers: 0, max_spend_usd: null, allow_create_groups: false };
  const raise = grantEditReview(projects, { ids: [P1], caps }, { ids: [P1, P2], caps: { ...caps, max_active_sessions: 6 } });
  assert.deepEqual(raise, { lines: ['max active sessions: 4 → 6', '+ grant: docs'], destructive: false, scope: 'projects and caps' });
  const cut = grantEditReview(projects, { ids: [P1, P2], caps }, { ids: [P1], caps: { ...caps, max_spend_usd: 5 } });
  assert.equal(cut.destructive, true);
  assert.deepEqual(cut.lines, ['max spend (USD): none → 5', '- remove: docs (their seats leave this manager’s supervision)']);
  assert.equal(grantEditReview(projects, { ids: [P1], caps }, { ids: [P1], caps: { ...caps, max_created_sessions: 2 } }).destructive, true);
  assert.deepEqual(grantEditReview(projects, { ids: [P1], caps }, { ids: [P1], caps }), { error: 'No change to save.' });
  assert.match(grantEditReview(projects, { ids: [P1], caps }, { ids: [], caps }).error, /at least one project/);
});

test('an area edit reads three bounded allowances', () => {
  assert.deepEqual(NODE_FIELDS.map((f) => [f.min, f.max]), [[1, 100], [0, 1024], [1, 64]]);
  const v = nodeEditValues(['3', '8', '2'], [4, 8, 2]);
  assert.deepEqual(v.values, [3, 8, 2]);
  assert.equal(v.destructive, true);
  assert.equal(v.lines[0], 'max active sessions: 4 → 3');
  assert.equal(nodeEditValues(['5', '8', '2'], [4, 8, 2]).destructive, false);
  assert.deepEqual(nodeEditValues(['4', '8', '2'], [4, 8, 2]), { error: 'No change to save.' });
  assert.match(nodeEditValues(['0', '8', '2'], [4, 8, 2]).error, /1-100/);
});

test('the tree binds A and m', () => {
  assert.equal(VIEW_KEYS.tree.A, 'mgr-above');
  assert.equal(VIEW_KEYS.tree.m, 'mgr-move');
});

// ---------- workspace console, node consoles, launch and appoint ----------

import {
  HALTABLE, MAX_SCOPE_PROJECTS, defaultLaunchPrompt, launchHost, launchScope, launchedSessionId, nodeHeader, nodeRefId,
  nodeWorkspaceRows, seatActionBlock, validateLaunch, workspaceRows,
} from '../dist/managers-model.js';

const seat = (id, over = {}) => ({ session_id: id, project_id: P1, status: 'Running', provider: 'Claude', model: 'claude-opus-5-5', context_fill_pct: 40, cost_usd: 1.5, updated_at: 'T', pending_question: false, ...over });
const pred = (id) => ({ session_id: id, project_id: P1, status: 'Completed', provider: 'Codex', model: null, context_fill_pct: null, cost_usd: null, updated_at: 'T0' });
const project = (id, name, manager) => ({ overview: { project_id: id, name, path: null, manager, policy: null, issues: { open: 1, in_progress: 0, open_operator_requests: 0 }, running_sessions: 0, waiting_approval_sessions: 0, pending_questions: 0, pending_approvals: 0 }, scope_revoked: false });

test('the global console lists earlier seat sessions and missing projects', () => {
  const w = {
    grant: { grant_id: 'g', grant_version: 2, seat_session_id: SEAT, state: 'active', project_ids: [P1], project_policy: {}, operator_origin: 'operator', updated_at: 'T' },
    seat: seat(SEAT, { predecessors: [pred(P2)], predecessors_truncated: true }),
    projects: [project(P1, 'rsi', { ...seat(CAND.id), scope_version: 1, predecessors: [pred(P2)] })],
    missing_project_ids: [P2],
  };
  const rows = workspaceRows(w);
  assert.deepEqual(rows.map((r) => r.kind), ['seat', 'predecessor', 'predecessor', 'project', 'predecessor', 'missing']);
  assert.equal(rows[0].level, 'console');
  assert.equal(rows[1].title, 'earlier seat bbbbbbbb');
  assert.equal(rows[1].sessionId, P2);
  assert.equal(rows[2].title, 'more earlier seats not listed');
  assert.equal(rows[3].level, 'project');
  assert.equal(rows[5].health, 'MISSING');
});

const nodeWs = {
  node: { kind: 'portfolio', node_id: P2 }, label: 'global', state: 'active', parent: { kind: 'portfolio', node_id: SEAT },
  grant: { grant_version: 5, project_ids: [P1, P2], seat_session_id: SEAT, state: 'active' }, seat: seat(SEAT),
  children: [{ node: { kind: 'portfolio', node_id: CAND.id }, label: 'east', state: 'active', grant_version: 1, seat: null, project_ids: [P1],
    counts: { issues: { open: 0, in_progress: 0, open_operator_requests: 0 }, running_sessions: 2, waiting_approval_sessions: 0, pending_questions: 1, pending_approvals: 0 }, pending_escalations: 0 }],
  children_truncated: false, projects: [project(P1, 'rsi', null)], missing_project_ids: [],
  escalations: [{}, {}], escalations_truncated: false,
  fleet: { as_of: 'T', active: 3, totals: [{ invocations: 1, errors: 0, input: 300, output: 0, cache_read: 0, cache_write: 0, cost: 0.5, unknown_usage: 0 }, { invocations: 2, errors: 0, input: 3600, output: 0, cache_read: 0, cache_write: 0, cost: 1, unknown_usage: 0 }, { invocations: 9, errors: 0, input: 0, output: 0, cache_read: 0, cache_write: 0, cost: 0, unknown_usage: 0 }], agents_truncated: false, usage_truncated: false },
};

test('a node console shows its seat, child digests and projects, with its place and fleet', () => {
  const rows = nodeWorkspaceRows(nodeWs);
  assert.deepEqual(rows.map((r) => r.kind), ['seat', 'child', 'project']);
  assert.equal(rows[0].title, 'global seat');
  assert.deepEqual(rows[1].node, { kind: 'portfolio', node_id: CAND.id });
  assert.equal(rows[1].health, 'ACTIVE');
  assert.match(rows[1].detail, /^grant v1 · 1 project\(s\) · 2 running · 1 questions · 0 approvals · 0 escalation\(s\)$/);
  assert.equal(rows[1].attention, true);
  assert.equal(nodeHeader(nodeWs), 'global (active) reports to portfolio:cccccccc · grant v5 over 2 project(s) · fleet 3 active · 5 min 60 tok/min · $6.00/h · 1 h 60 tok/min · $1.00/h · 24 h 9 calls · 2 escalation(s) waiting');
  assert.equal(nodeRefId({ kind: 'project', project_id: P1 }), P1);
  assert.equal(nodeRefId({ kind: 'area', node_id: P2 }), P2);
  assert.deepEqual(nodeWorkspaceRows(null), []);
});

test('seat keys refuse the rows they cannot act on', () => {
  const rows = workspaceRows({ grant: null, seat: seat(SEAT, { predecessors: [pred(P2)] }), projects: [project(P1, 'rsi', null)], missing_project_ids: [] });
  const [current, earlier, noPm] = rows;
  assert.equal(seatActionBlock(current, 'type'), null);
  assert.match(seatActionBlock(earlier, 'type'), /read-only history/);
  assert.equal(seatActionBlock(earlier, 'archive'), null);
  assert.match(seatActionBlock(current, 'archive'), /current seat/);
  assert.match(seatActionBlock(noPm, 'halt'), /no session/);
  assert.match(seatActionBlock(null, 'status'), /Select a seat row to show/);
  assert.match(seatActionBlock(nodeWorkspaceRows(nodeWs)[1], 'type'), /opens its console/);
  assert.deepEqual(HALTABLE, ['Starting', 'Running', 'WaitingApproval']);
});

test('the console binds the TUI keys', () => {
  const k = VIEW_KEYS.workspace;
  for (const [key, action] of Object.entries({ i: 'mgr-type', t: 'mgr-talk', s: 'mgr-status', x: 'mgr-halt', X: 'mgr-halt-now', a: 'mgr-archive-seat', n: 'mgr-launch', Tab: 'mgr-conversation', Backspace: 'mgr-parent', l: 'mgr-enter' })) {
    assert.equal(k[key], action, key);
  }
  assert.equal(k[':'], undefined, ': reaches the command line');
});

test('the launch form follows global_manager_workspace_launch.rs', () => {
  const projects = [{ id: P1, name: 'rsi' }, { id: P2, name: 'docs' }];
  assert.match(defaultLaunchPrompt({ kind: 'global' }, ['rsi', 'docs']), /global manager for: rsi, docs\. Load the rsi-portfolio-manager skill/);
  assert.match(defaultLaunchPrompt({ kind: 'project', projectId: P1, name: 'rsi' }, []), /project manager for rsi\. Load the rsi-project-manager skill/);
  assert.deepEqual(launchScope(projects, null), [P1, P2]);
  assert.deepEqual(launchScope(projects, [P2]), [P2]);
  assert.equal(launchHost([P1, P2], P2, P1), P2);
  assert.equal(launchHost([P1], P2, P1), P1);
  assert.equal(launchHost([P2], null, P1), P2);
  assert.equal(launchHost([], null, P1), null);
  const l = { provider: 'Claude', model: 'claude-opus-5-5', effort: 'high' };
  const ok = validateLaunch({ kind: 'global' }, l, [l], ' go ', [P1, P2], P2, projects);
  assert.deepEqual(ok, { role: { kind: 'global' }, launch: l, prompt: 'go', scope: [P1, P2], home: P2 });
  assert.match(validateLaunch({ kind: 'global' }, null, [l], 'go', [P1], null, projects).error, /allowed catalog/);
  assert.match(validateLaunch({ kind: 'global' }, l, [l], '  ', [P1], null, projects).error, /first instruction/);
  assert.match(validateLaunch({ kind: 'global' }, l, [l], 'go', [], null, projects).error, /at least one project/);
  assert.match(validateLaunch({ kind: 'global' }, l, [l], 'go', [SEAT], null, projects).error, /no longer exists/);
  const many = Array.from({ length: MAX_SCOPE_PROJECTS + 1 }, (_, i) => `id${i}`);
  assert.match(validateLaunch({ kind: 'global' }, l, [l], 'go', many, null, many.map((id) => ({ id }))).error, /at most 64 projects; uncheck 1/);
  assert.equal(validateLaunch({ kind: 'project', projectId: P1, name: 'rsi' }, l, [l], 'go', [], null, projects).home, P1);
  assert.equal(launchedSessionId({ session_id: SEAT }), SEAT);
  assert.equal(launchedSessionId({}), null);
});
