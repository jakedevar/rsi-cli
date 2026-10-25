// Manager surfaces (hub #3011, slice 9): the pure models (tree, board and
// decisions, scope and policy forms, budget, fleet, workspace) and the request
// shapes, plus the wiring from managers-api.ts to src-tauri/src/managers.rs.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import {
  VIEW_KEYS, answerRequest, appointRequest, applyCap, budgetEditBlock, budgetRows, clampIndex, cycle, decisionBlock,
  decisionOptions, decisionStatusLabel, defaultPolicy, descendants, filterScope, fleetAgents, fleetGroups, foldOrParent,
  normalizeCap, optionAnswer, parentKeys, policyProblems, policySaveRequest, POLICY_FIELDS, portfolioRaiseRequest,
  projectRaiseRequest, raiseTo, recordDetail, retryKey, rowSession, rowTitle, scopeClearRequest, scopeDraftForSeat, scopeDraftFrom, scopeMark,
  scopeSaveRequest, sectionQuery, seatHealth, setPolicyScalar, stepPolicy, toggleCapability, toggleScope, treeActionBlock,
  treeLoad, unavailableSelections, usageLine, visibleTreeRows, workspaceRows,
} from '../dist/managers-model.js';

const here = (p) => fileURLToPath(new URL(p, import.meta.url));
const P = 'e2cae1d4-1833-48eb-a7e8-6f1dd480fdae';
const S = '0b7c8a52-5f63-4a9e-9d27-3c1a9a5b6e10';
const S2 = '1c8d9b63-6a74-4b5f-8e38-4d2b0b6c7f21';
const G = '7d0f4c2e-9a1b-4c3d-8e5f-6a7b8c9d0e1f';
const E1 = '2a3b4c5d-6e7f-4a8b-9c0d-1e2f3a4b5c6d';
const E2 = '3b4c5d6e-7f8a-4b9c-8d1e-2f3a4b5c6d7e';

const load = { running_workers: 2, direct_reports: null, pending_escalations: 0, pending_decisions: 1 };
const row = (key, parent, depth, kind, over = {}) => ({
  key, parent_key: parent, depth, kind, label: key, project_id: null, node_id: null, epic_id: null, scope: null,
  seat: null, focus_session_id: null, grant: null, load, complete: true, ...over,
});
const TREE = [
  row('portfolio:g', null, 0, 'portfolio', { tier_label: 'global', node_id: S2 }),
  row(`project:${P}`, 'portfolio:g', 1, 'project', { project_id: P, focus_session_id: S }),
  row('area:a', `project:${P}`, 2, 'area', { project_id: P, node_id: G }),
  row('epic:e', 'area:a', 3, 'epic', { epic_id: E1, focus_session_id: E2 }),
  row('project:other', 'portfolio:g', 1, 'project', { project_id: G }),
];

// ---------- tree ----------

test('the tree folds: a folded row hides its whole subtree', () => {
  assert.deepEqual([...parentKeys(TREE)].sort(), ['area:a', 'portfolio:g', `project:${P}`].sort());
  assert.deepEqual(visibleTreeRows(TREE, new Set()).map((r) => r.key), TREE.map((r) => r.key));
  assert.deepEqual(visibleTreeRows(TREE, new Set([`project:${P}`])).map((r) => r.key), ['portfolio:g', `project:${P}`, 'project:other']);
  assert.deepEqual(visibleTreeRows(TREE, new Set(['portfolio:g'])).map((r) => r.key), ['portfolio:g']);
  assert.deepEqual(descendants(TREE, `project:${P}`).map((r) => r.key), ['area:a', 'epic:e']);
});

test('h folds an open parent, else moves to the parent', () => {
  const a = foldOrParent(TREE, new Set(), `project:${P}`);
  assert.ok(a.folded.has(`project:${P}`));
  assert.equal(a.select, `project:${P}`);
  const b = foldOrParent(TREE, a.folded, `project:${P}`);
  assert.equal(b.select, 'portfolio:g');
  assert.equal(foldOrParent(TREE, new Set(), 'epic:e').select, 'area:a');
});

test('an uncounted load is ?, never 0, and the seat line keeps its facts', () => {
  assert.equal(treeLoad(TREE[0]), 'w 2 · rep ? · esc 0 · dec 1');
  assert.equal(seatHealth(null), 'no seat');
  assert.equal(seatHealth({ session_id: S, status: 'Running', model: 'opus', context_fill_pct: 41.6, updated_at: '' }), 'Running · ctx 42% · opus');
  assert.equal(seatHealth({ session_id: S, status: 'Running', model: null, context_fill_pct: null, updated_at: '' }), 'Running · ctx ?');
});

test('in-tree actions follow the TUI availability table', () => {
  const [portfolio, project, area, epic, empty] = TREE;
  const cand = { id: S2, project_id: P, archived: false, leaf: true };
  assert.equal(treeActionBlock(project, 'appoint', cand), null);
  assert.match(treeActionBlock(project, 'appoint', null), /Select the session/);
  assert.match(treeActionBlock(project, 'appoint', { ...cand, archived: true }), /archived/);
  assert.match(treeActionBlock(project, 'appoint', { ...cand, leaf: false }), /Group or Epic/);
  assert.match(treeActionBlock(project, 'appoint', { ...cand, project_id: G }), /another project/);
  assert.match(treeActionBlock(project, 'appoint', { ...cand, id: S }), /already holds/);
  assert.match(treeActionBlock(area, 'appoint', cand), /No RPC moves an area seat/);
  assert.equal(treeActionBlock(project, 'edit', null), null);
  assert.match(treeActionBlock(empty, 'edit', null), /Appoint a manager first/);
  assert.equal(treeActionBlock(portfolio, 'revoke', null), null);
  assert.equal(treeActionBlock(area, 'revoke', null), null);
  assert.equal(treeActionBlock(project, 'revoke', null), null);
  assert.match(treeActionBlock(empty, 'revoke', null), /no manager to revoke/);
  for (const a of ['appoint', 'edit', 'revoke']) assert.match(treeActionBlock(epic, a, cand), /Epic rows/);
  assert.equal(treeActionBlock(epic, 'open', null), null);
  assert.match(treeActionBlock(null, 'open', null), /Select a row/);
});

// ---------- board and decisions ----------

const policy = { project_id: P, manager_session_id: S, scope_version: 4, row_version: 9, policy: {}, updated_at: '', revoked: false };
const page = (rows, over = {}) => ({ observed_at: 't', scope_version: 4, policy, section: 'decisions', rows, next_cursor: null, complete: true, ...over });
const decision = (over = {}) => ({
  key: 'd:1', status: 'pending', row_version: 3, target_digest: 'abc', question: 'Ship it?\nA) Yes\nB) No (recommended)', ...over,
});

test('full sections map to the daemon sections; Board composes four first pages', () => {
  assert.equal(sectionQuery('decisions', 'workers'), 'decisions');
  assert.equal(sectionQuery('inbox', 'workers'), 'requests');
  assert.equal(sectionQuery('inspect', 'topology'), 'topology');
  assert.equal(sectionQuery('board', 'workers'), null);
  assert.equal(cycle(['a', 'b', 'c'], 'c', 1), 'a');
  assert.equal(cycle(['a', 'b', 'c'], 'a', -1), 'c');
});

test('decision options come from structured options first, then the question lines', () => {
  const structured = decisionOptions({ question: 'Pick', options: [{ label: 'A', detail: 'Read the DB', recommended: false }, { label: 'B', detail: 'Call the API', recommended: true }] });
  assert.deepEqual(structured, { options: [{ label: 'A', text: 'Read the DB' }, { label: 'B', text: 'Call the API' }], recommended: 1 });
  const nested = decisionOptions({ payload: { options: [{ label: 'Yes', recommended: true }, { label: 'No' }] } });
  assert.deepEqual(nested.options.map(optionAnswer), ['Yes', 'No']);
  assert.equal(nested.recommended, 0);
  const parsed = decisionOptions(decision());
  assert.deepEqual(parsed.options, [{ label: 'A', text: 'Yes' }, { label: 'B', text: 'No' }]);
  assert.equal(parsed.recommended, 1);
  const named = decisionOptions({ question: 'Which?\n- A: left\n- B: right\nRecommend: A' });
  assert.equal(named.recommended, 0);
  assert.deepEqual(decisionOptions({ question: 'Free text only' }), { options: [], recommended: null });
  assert.equal(optionAnswer({ label: 'B', text: 'Call the API' }), 'B: Call the API');
});

test('only a pending record with its identity and a current policy can be answered', () => {
  const p = page([decision()]);
  assert.equal(decisionBlock(decision(), p), null);
  assert.match(decisionBlock(decision({ status: 'answered' }), p), /not pending/);
  assert.match(decisionBlock(decision({ status: undefined }), p), /state is unknown/);
  assert.match(decisionBlock(decision({ target_digest: '' }), p), /digest unavailable/);
  assert.match(decisionBlock(decision({ row_version: 0 }), p), /version unavailable/);
  assert.match(decisionBlock(decision({ route_state: 'unavailable', next_action: 'Open the gate session' }), p), /Open the gate session/);
  assert.match(decisionBlock(decision(), page([], { policy: { ...policy, revoked: true } })), /current operator policy/);
  assert.match(decisionBlock(decision(), page([], { scope_version: 5 })), /current operator policy/);
  assert.match(decisionBlock(null, p), /Select a pending decision/);
  assert.equal(decisionStatusLabel(decision()), 'needs answer');
  assert.equal(decisionStatusLabel({ type: 'operator_gate', status: 'pending' }), 'approval gate');
});

test('AnswerHarnessManagerDecision carries the exact digest, version and both fences', () => {
  assert.deepEqual(answerRequest(P, decision(), page([]), 'B: No', 'k1'), {
    project_id: P, fence: { scope_version: 4, policy_version: 9 }, decision_key: 'd:1', expected_row_version: 3,
    target_digest: 'abc', answer: 'B: No', idempotency_key: 'k1',
  });
  assert.equal(answerRequest(P, { decision_key: 'd:2', payload: { row_version: 7, target_digest: 'x' } }, page([]), 'ok', 'k').expected_row_version, 7);
});

test('records name themselves, find their session and never show a credential', () => {
  assert.equal(rowTitle({ question: 'First line\nsecond' }), 'First line');
  assert.equal(rowTitle({ type: 'action' }), 'action');
  assert.equal(rowSession({ lead: { session_id: S } }), S);
  assert.equal(rowSession({ session_id: 'not-a-uuid' }), null);
  const detail = Object.fromEntries(recordDetail({ key: 'k', session_token: 'abc', api_key: 'x', input_tokens: 12, payload: { secret: 's', ok: 1 }, nested: { auth_token: 'q', n: 1 } }));
  assert.equal(detail.session_token, '(hidden)');
  assert.equal(detail.api_key, '(hidden)');
  assert.equal(detail.secret, '(hidden)');
  assert.equal(detail.input_tokens, '12');
  assert.equal(detail.ok, '1');
  assert.ok(detail.nested.includes('(hidden)') && detail.nested.includes('"n":1'));
});

// ---------- scope form ----------

const cfg = (over = {}) => ({ project_id: P, manager_session_id: S, epic_ids: [E1], scope_mode: 'selected', selected_epic_ids: [E1], group_ids: [], row_version: 5, updated_at: '', ...over });
const group = { id: G, title: 'Backend', kind: 'Group', group_id: null, group_title: null };
const epicIn = { id: E1, title: 'API', kind: 'Epic', group_id: G, group_title: 'Backend' };
const epicOut = { id: E2, title: 'Docs', kind: 'Epic', group_id: null, group_title: null };

test('a new or revoked appointment opens on the whole project; a selection reopens as saved', () => {
  assert.deepEqual(scopeDraftFrom(null), { whole: true, groups: [], epics: [] });
  assert.deepEqual(scopeDraftFrom(cfg({ scope_mode: 'project' })), { whole: true, groups: [], epics: [] });
  assert.deepEqual(scopeDraftFrom(cfg({ epic_ids: [], selected_epic_ids: [] })), { whole: true, groups: [], epics: [] });
  assert.deepEqual(scopeDraftFrom(cfg({ group_ids: [G] })), { whole: false, groups: [G], epics: [E1] });
});

test(':manager appoint opens a new seat on the whole project and the same seat as saved', () => {
  assert.deepEqual(scopeDraftForSeat(cfg(), S2), { whole: true, groups: [], epics: [] });
  assert.deepEqual(scopeDraftForSeat(cfg(), S), { whole: false, groups: [], epics: [E1] });
  assert.deepEqual(scopeDraftForSeat(cfg(), null), { whole: false, groups: [], epics: [E1] });
  assert.deepEqual(scopeDraftForSeat(null, S2), { whole: true, groups: [], epics: [] });
});

test('Space narrows, Groups own their Epics and the 32 limits hold', () => {
  const whole = scopeDraftFrom(null);
  assert.equal(scopeMark(whole, epicOut), '[+]');
  const g = toggleScope(whole, group).draft;
  assert.deepEqual(g, { whole: false, groups: [G], epics: [] });
  assert.equal(scopeMark(g, group), '[x]');
  assert.equal(scopeMark(g, epicIn), '[+]');
  assert.equal(scopeMark(g, epicOut), '[ ]');
  assert.match(toggleScope(g, epicIn).error, /Deselect the Group/);
  const e = toggleScope(g, epicOut).draft;
  assert.equal(scopeMark(e, epicOut), '[x]');
  assert.deepEqual(toggleScope(e, epicOut).draft.epics, []);
  const full = { whole: false, groups: [], epics: Array.from({ length: 32 }, (_, i) => `00000000-0000-4000-8000-${String(i).padStart(12, '0')}`) };
  assert.match(toggleScope(full, epicOut).error, /32/);
});

test('the scope filter matches name, Group name or ID and keeps unavailable selections visible', () => {
  const cands = [group, epicIn, epicOut];
  assert.deepEqual(filterScope(cands, 'backend').map((c) => c.id), [G, E1]);
  assert.deepEqual(filterScope(cands, E2.slice(0, 6)).map((c) => c.id), [E2]);
  assert.deepEqual(filterScope(cands, ' ').length, 3);
  assert.deepEqual(unavailableSelections({ whole: false, groups: [S], epics: [E1] }, cands), [S]);
});

test('ConfigureHarnessManager: whole project omits both lists, an empty selection revokes', () => {
  assert.deepEqual(scopeSaveRequest(P, S, { whole: true, groups: [], epics: [] }, 5), { project_id: P, session_id: S, expected_row_version: 5 });
  assert.deepEqual(scopeSaveRequest(P, S, { whole: false, groups: [G], epics: [] }, 5), { project_id: P, session_id: S, expected_row_version: 5, epic_ids: [], group_ids: [G] });
  assert.deepEqual(scopeClearRequest(cfg()), { project_id: P, session_id: S, expected_row_version: 5, epic_ids: [] });
  assert.deepEqual(appointRequest(P, S2, cfg()), { project_id: P, session_id: S2, expected_row_version: 5, epic_ids: [E1] });
  assert.deepEqual(appointRequest(P, S2, null), { project_id: P, session_id: S2, expected_row_version: 0 });
  assert.deepEqual(appointRequest(P, S2, cfg({ epic_ids: [], selected_epic_ids: [] })), { project_id: P, session_id: S2, expected_row_version: 5 });
});

// ---------- policy form ----------

test('the policy form steps choices, types scalars and refuses what the daemon would', () => {
  const p0 = defaultPolicy();
  const mode = POLICY_FIELDS.find((f) => f.key === 'mode');
  assert.equal(stepPolicy(p0, mode).mode, 'monitor');
  assert.equal(stepPolicy(stepPolicy(stepPolicy(p0, mode), mode), mode).mode, 'status');
  const paused = POLICY_FIELDS.find((f) => f.key === 'paused');
  assert.equal(stepPolicy(p0, paused).paused, true);
  const active = POLICY_FIELDS.find((f) => f.key === 'max_active_sessions');
  assert.equal(setPolicyScalar(p0, active, '12').policy.max_active_sessions, 12);
  assert.match(setPolicyScalar(p0, active, '0').error, /1\.\.100/);
  assert.match(setPolicyScalar(p0, active, '2.5').error, /whole number/);
  const spend = POLICY_FIELDS.find((f) => f.key === 'max_spend_usd');
  assert.equal(setPolicyScalar(p0, spend, '25.5').policy.max_spend_usd, 25.5);
  assert.equal(setPolicyScalar({ ...p0, max_spend_usd: 3 }, spend, '').policy.max_spend_usd, null);
  assert.match(setPolicyScalar(p0, spend, '0').error, /positive/);
  assert.deepEqual(policyProblems(p0), []);
  const groups = { ...p0, allow_create_groups: true };
  assert.match(policyProblems(groups)[0], /topology/);
  assert.deepEqual(policyProblems(toggleCapability(groups, 'topology')), []);
  assert.deepEqual(toggleCapability(toggleCapability(p0, 'deploy'), 'deploy').capabilities, []);
});

test('ConfigureHarnessManagerPolicy is fenced on scope and policy versions; a retry keeps its key', () => {
  const draft = { ...defaultPolicy(), mode: 'execute' };
  assert.deepEqual(policySaveRequest(cfg(), null, draft, 'k'), { project_id: P, expected_scope_version: 5, expected_policy_version: 0, idempotency_key: 'k', policy: draft });
  assert.equal(policySaveRequest(cfg(), policy, draft, 'k').expected_policy_version, 9);
  let n = 0;
  const mint = () => `k${++n}`;
  const a = retryKey(null, 'x', mint);
  assert.equal(retryKey(a, 'x', mint).key, 'k1');
  assert.equal(retryKey(a, 'y', mint).key, 'k2');
});

// ---------- budget ----------

const budget = {
  node_id: S2, node_label: 'global', resets: 'no timed reset',
  projects: [{ project_id: P, project_name: 'rsi', chain: [
    { kind: 'portfolio', node_id: S2, label: 'global', version: 4, epoch: 2, counted_since: null, caps: [{ field: 'created_sessions', used: 30, limit: 32, binding: true }] },
    { kind: 'project', node_id: P, label: 'rsi PM', version: 9, epoch: 4, counted_since: null, caps: [{ field: 'spend_usd', used: 1, limit: null }] },
    { kind: 'delegate', node_id: G, label: 'area', version: 1, epoch: 1, counted_since: null, caps: [{ field: 'active_sessions', used: 1, limit: 2 }] },
  ] }],
};

test('budget caps normalise like BudgetCapFieldV1 and + raises one step', () => {
  assert.equal(normalizeCap('active_sessions', 0), 1);
  assert.equal(normalizeCap('created_sessions', 2000), 1024);
  assert.equal(normalizeCap('created_containers', 3.6), 4);
  assert.equal(normalizeCap('spend_usd', 0), null);
  const rows = budgetRows(budget);
  assert.equal(rows.length, 3);
  assert.deepEqual(raiseTo(rows[0].cap, null), { to: 64 });
  assert.deepEqual(raiseTo(rows[1].cap, null), { to: 11 });
  assert.deepEqual(raiseTo(rows[1].cap, '0'), { to: null });
  assert.match(raiseTo(rows[0].cap, 'lots').error, /number/);
  assert.equal(budgetEditBlock(rows[0]), null);
  assert.match(budgetEditBlock(rows[2]), /:manager tree/);
  assert.deepEqual(applyCap({ max_active_sessions: 4 }, 'active_sessions', null), { max_active_sessions: 1 });
});

test('a budget raise re-sends the current grant or policy behind its fences', () => {
  const [portfolioRow, projectRow] = budgetRows(budget);
  const node = {
    node_id: S2, tier_label: 'global', state: 'active', authority_epoch: 2, parent_node_id: null, max_direct_reports: 8, child_policy: null,
    grant: { grant_version: 4, seat_session_id: S, project_ids: [P], allowed_launches: [], project_policy: { max_created_sessions: 32, mode: 'execute' } },
  };
  assert.deepEqual(portfolioRaiseRequest(portfolioRow.node, node, 'created_sessions', 64, 'k').request, {
    node_id: S2, parent_node_id: null, tier_label: 'global', seat_session_id: S, project_ids: [P], allowed_launches: [],
    policy: { max_created_sessions: 64, mode: 'execute' }, child_policy: null, max_direct_reports: 8,
    expected_node_grant_version: 4, expected_authority_epoch: 2, idempotency_key: 'k',
  });
  assert.match(portfolioRaiseRequest(portfolioRow.node, { ...node, authority_epoch: 3 }, 'created_sessions', 64, 'k').error, /grant changed/);
  const current = { ...policy, policy: { max_spend_usd: null, mode: 'monitor' } };
  assert.deepEqual(projectRaiseRequest(projectRow.node, current, 'spend_usd', 50, 'k').request, {
    project_id: P, expected_scope_version: 4, expected_policy_version: 9, idempotency_key: 'k', policy: { max_spend_usd: 50, mode: 'monitor' },
  });
  assert.match(projectRaiseRequest(projectRow.node, { ...current, row_version: 10 }, 'spend_usd', 50, 'k').error, /PM policy changed/);
});

// ---------- fleet and workspace ----------

const usage = (cost, inv = 10, err = 1) => ({ invocations: inv, errors: err, input: 600, output: 0, cache_read: 0, cache_write: 0, cost, unknown_usage: 0 });
const agent = (id, project, provider, ctx) => ({ session: { id, provider, model: 'm', status: 'Running', title: `t ${project}`, context_fill_pct: ctx }, project, role: 'worker', turn_started_at: null });
const fleet = {
  as_of: 't', agents: [agent(S, 'beta', 'Codex', 10), agent(S2, 'alpha', 'Claude', 80)],
  groups: [
    { dimension: 'project', key: P, label: 'alpha', active: 1, windows: [usage(1), usage(2), usage(3)] },
    { dimension: 'project', key: G, label: 'beta', active: 3, windows: [usage(5), usage(6), usage(7)] },
    { dimension: 'provider', key: 'Claude', label: 'Claude', active: 1, windows: [usage(1), usage(1), usage(1)] },
  ],
  totals: [usage(6), usage(8), usage(10)], agents_truncated: false, usage_truncated: false,
};

test('the fleet filters, sorts and groups like the TUI', () => {
  assert.deepEqual(fleetAgents(fleet, '', 0, false).map((a) => a.project), ['alpha', 'beta']);
  assert.deepEqual(fleetAgents(fleet, '', 0, true).map((a) => a.project), ['beta', 'alpha']);
  assert.deepEqual(fleetAgents(fleet, '', 4, false).map((a) => a.session.context_fill_pct), [10, 80]);
  assert.deepEqual(fleetAgents(fleet, 'codex', 0, false).map((a) => a.project), ['beta']);
  assert.deepEqual(fleetGroups(fleet, 0, 0, '', 1, false).map((g) => g.label), ['beta', 'alpha']);
  assert.deepEqual(fleetGroups(fleet, 0, 0, '', 3, false).map((g) => g.label), ['beta', 'alpha']);
  assert.deepEqual(fleetGroups(fleet, 1, 0, '', 0, false).map((g) => g.label), ['Claude']);
  assert.equal(usageLine(usage(1), 0), '120 tok/min · $12.00/h · 10 calls · 10.0% err');
  assert.deepEqual(fleetAgents(null, '', 0, false), []);
});

test('the global workspace lists the seat, then each project with its signals', () => {
  const ws = {
    grant: { grant_id: G, grant_version: 3, seat_session_id: S, state: 'active', project_ids: [P], project_policy: {}, operator_origin: 'tui', updated_at: '' },
    seat: { session_id: S, project_id: P, status: 'Running', provider: 'Claude', model: 'opus', context_fill_pct: 50, cost_usd: 1.5, updated_at: '', pending_question: true },
    projects: [{ scope_revoked: false, overview: {
      project_id: P, name: 'rsi', path: null, manager: null, policy: { policy_version: 2, mode: 'execute', revoked: false, paused: true, capabilities: [] },
      issues: { open: 4, in_progress: 1, open_operator_requests: 2 }, running_sessions: 3, waiting_approval_sessions: 0, pending_questions: 0, pending_approvals: 0,
    } }],
    missing_project_ids: [],
  };
  const rows = workspaceRows(ws);
  assert.equal(rows.length, 2);
  assert.equal(rows[0].health, 'Running · Claude/opus · ctx 50% · $1.50 · question waiting');
  assert.ok(rows[0].attention);
  assert.equal(rows[1].title, 'rsi');
  assert.match(rows[1].detail, /^execute \(paused\) v2 · Issues 4 open/);
  assert.ok(rows[1].attention);
  assert.deepEqual(workspaceRows(null), []);
});

test('cursor clamping and the overlay key tables', () => {
  assert.equal(clampIndex(5, 3), 2);
  assert.equal(clampIndex(-1, 3), 0);
  assert.equal(clampIndex(0, 0), -1);
  for (const [view, keys] of Object.entries(VIEW_KEYS)) {
    assert.equal(keys.Escape, 'mgr-close', `${view} closes on Esc`);
    if (view !== 'scope') assert.equal(keys.r, 'mgr-refresh', `${view} refreshes on r`);
  }
  assert.equal(VIEW_KEYS.board['2'], 'mgr-section-2');
  assert.equal(VIEW_KEYS.tree.x, 'mgr-revoke');
});

// ---------- wiring to src-tauri/src/managers.rs ----------

// G3 (hub #3000) extends the slice with src-tauri/src/manager_ops.rs: forwarders
// name their method through a METHOD_* constant, and its planners (mgr_*) send nothing.
const RUST = readFileSync(here('../src-tauri/src/managers.rs'), 'utf8') + readFileSync(here('../src-tauri/src/manager_ops.rs'), 'utf8');
const API = readFileSync(here('../src/managers-api.ts'), 'utf8');
const CONSTS = new Map([...RUST.matchAll(/pub const (METHOD_\w+): &str = "(\w+)";/g)].map((m) => [m[1], m[2]]));

/** `#[tauri::command] pub async fn name(...)` → the daemon method it forwards. */
function commands() {
  const out = new Map();
  const re = /#\[tauri::command\]\s*pub async fn (\w+)\([\s\S]*?forward\(\s*&state,\s*(?:"(\w+)"|(METHOD_\w+))/g;
  for (const m of RUST.matchAll(re)) out.set(m[1], m[2] ?? CONSTS.get(m[3]));
  return out;
}

/** The no-daemon planners: `#[tauri::command] pub fn mgr_*`. */
const PLANNERS = new Set([...RUST.matchAll(/#\[tauri::command\]\s*pub fn (mgr_\w+)\(/g)].map((m) => m[1]));

test('every manager RPC the desktop calls is an operator method the TUI client uses', () => {
  const methods = [...new Set(commands().values())].sort();
  assert.deepEqual(methods, [
    'AnswerHarnessManagerDecision', 'ArchiveStaleManagerDecisions', 'ConfigureHarnessManager', 'ConfigureHarnessManagerPolicy',
    'ConfigurePortfolioNode', 'GetFleetOverview', 'GetGlobalDashboard', 'GetGlobalManager', 'GetGlobalManagerBudget', 'GetGlobalManagerWorkspace',
    'GetHarnessManager', 'GetHarnessManagerPolicy', 'GetHarnessManagerState', 'GetManagerNode', 'GetManagerTree',
    'GetPortfolioNode', 'ListFrictionRollup', 'ListHarnessManagerScope', 'ListOperatorEscalations', 'ListPortfolioNodes',
    'ListStaleManagerDecisions', 'RevokeGlobalManager', 'RevokeManagerNode', 'RevokePortfolioNode',
  ].concat(['AcknowledgeOperatorNotice', 'CancelOperatorRestart', 'ConfigureGlobalManager', 'ConfigureHarnessManagerWithPolicy',
    'ConfigureManagerNode', 'ConfigurePortfolioNodeLaunches', 'ForceOperatorRestart', 'GetManagerNodeWorkspace', 'GetOperatorRestart',
    'ListManagerNodes', 'RuleOperatorEscalation']).sort());
  const client = readFileSync(here('../../crates/rsi/src/client.rs'), 'utf8');
  for (const m of methods) assert.ok(client.includes(`"${m}"`) || (m === 'GetGlobalManagerBudget' && client.includes('GET_GLOBAL_MANAGER_BUDGET')), `${m} is a TUI client method`);
});

test('managers-api.ts calls only commands managers.rs defines, and uses them all', () => {
  const defined = commands();
  const used = new Set([...API.matchAll(/invoke<[\s\S]*?>\(\s*'(\w+)'/g)].map((m) => m[1]));
  for (const name of used) assert.ok(defined.has(name) || PLANNERS.has(name), `${name} is a Tauri command`);
  assert.deepEqual([...defined.keys(), ...PLANNERS].filter((n) => !used.has(n)), []);
});

test('intent overview details distinguish the unfinished preview from full work counts', () => {
  const detail = Object.fromEntries(recordDetail({
    type: 'intent', work_versions_format: 'work_summary_v1',
    work_versions: [{ key: 'topology:live:review', version: 3, integrated: false }],
    work_versions_count: 203, work_versions_unfinished_count: 1,
    work_counts: { pending: 0, partial: 0, accepted: 1, integrated: 200, retired: 2 },
    work_query: { section: 'work', epic_id: E1, limit: 32 },
  }));
  assert.equal(detail.work_versions_format, 'work_summary_v1');
  assert.equal(detail.work_versions, '1 item(s)');
  assert.equal(detail.work_versions_count, '203');
  assert.equal(detail.work_versions_unfinished_count, '1');
  assert.equal(JSON.parse(detail.work_counts).integrated, 200);
  assert.equal(JSON.parse(detail.work_query).section, 'work');
});
