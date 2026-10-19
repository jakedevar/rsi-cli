// Gap sweep G3 (hub #3000): the manager `:` commands — :manager global,
// portfolio, escalations, node and restart — their parsers, resolvers and
// summaries, the keymap routes, and the wiring to src-tauri/src/manager_ops.rs.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { desktopKeymap } from '../dist/keybindings.js';
import { KeyEngine } from '../dist/keyengine.js';
import { unavailable } from '../dist/registry.js';
import {
  CAP_REDUCTION, ESCALATIONS_USAGE, GLOBAL_USAGE, NODE_USAGE, PORTFOLIO_USAGE, SET_USAGE, capPreview, escalationRows,
  freeProjects, grantSummary, hopSummary, needsCapConfirm, nodeSummary, noticeSummary, parseEscalationsCommand,
  parseGlobalCommand, parseNodeCommand, parsePortfolioCommand, parseRestart, portfolioSummary, resolveId, resolveNode,
  resolveProjects, restartDescribe, undeliveredSummary,
} from '../dist/manager-cmds-model.js';

const here = (p) => fileURLToPath(new URL(p, import.meta.url));
const cmd = (line) => new KeyEngine(desktopKeymap()).runCommand(line);

const A = 'aaaaaaaa-1111-4111-8111-111111111111';
const B = 'bbbbbbbb-2222-4222-8222-222222222222';
const C = 'cccccccc-3333-4333-8333-333333333333';
const PROJECTS = [{ id: A, name: 'rsi' }, { id: B, name: 'Mac Mini' }, { id: C, name: 'docs' }];

test('the manager commands route to their actions with the argument', () => {
  const want = {
    'manager global': 'mgr-global-grant', 'manager global set active 12': 'mgr-global-grant',
    'manager global add-project docs': 'mgr-global-grant', 'manager global appoint rsi, docs': 'mgr-global-appoint',
    'manager global revoke': 'mgr-global-revoke', 'manager portfolio show global': 'mgr-portfolio',
    'manager escalations rule 1a2b fix it': 'mgr-escalations', 'manager node': 'mgr-node',
    'manager node get x': 'mgr-node', 'manager restart': 'mgr-restart', 'manager restart now': 'mgr-restart',
  };
  for (const [line, action] of Object.entries(want)) assert.equal(cmd(line).action, action, `:${line}`);
  assert.equal(cmd('manager global set active 12').arg, 'set active 12');
  assert.equal(cmd('manager global appoint rsi, docs').arg, 'rsi, docs');
  assert.equal(cmd('manager restart now').arg, 'now');
});

test('appointing a global manager needs a focused session', () => {
  const snap = (seat) => ({
    open: null, cursorOnRow: false, rowCount: 0, attentionRows: 0, view: 'active', vimKeys: true, hideTools: false,
    showFinished: false, modalOpen: false, event: null, managers: { view: null, hasProject: true, seatCandidate: seat },
  });
  assert.match(unavailable('mgr-global-appoint', snap(false)), /Select the session/);
  assert.equal(unavailable('mgr-global-appoint', snap(true)), null);
  assert.equal(unavailable('mgr-node', snap(false)), null);
  assert.equal(unavailable('mgr-restart', snap(false)), null);
});

test('project names resolve like the TUI', () => {
  assert.deepEqual(resolveProjects(PROJECTS, ''), [A, B, C]);
  assert.deepEqual(resolveProjects([], ''), { error: 'No projects to grant.' });
  assert.deepEqual(resolveProjects(PROJECTS, 'mac mini'), [B]);
  assert.deepEqual(resolveProjects(PROJECTS, 'rsi docs'), [A, C]);
  assert.deepEqual(resolveProjects(PROJECTS, 'Mac Mini, rsi'), [B, A]);
  assert.deepEqual(resolveProjects(PROJECTS, 'rsi nope'), { error: 'Unknown project: nope' });
});

test(':manager global parses every sub-command', () => {
  assert.deepEqual(parseGlobalCommand(undefined), { verb: 'show' });
  assert.deepEqual(parseGlobalCommand('show'), { verb: 'show' });
  assert.deepEqual(parseGlobalCommand('revoke'), { verb: 'revoke' });
  assert.deepEqual(parseGlobalCommand('configure {"a":1}'), { verb: 'configure', json: '{"a":1}' });
  assert.deepEqual(parseGlobalCommand('add-project rsi docs'), { verb: 'add-project', names: 'rsi docs' });
  assert.match(parseGlobalCommand('add-project').error, /add-project <project names/);
  assert.deepEqual(parseGlobalCommand('set spend 40 confirm'), { verb: 'set', field: 'spend', value: '40', confirm: true });
  assert.deepEqual(parseGlobalCommand('set active 3'), { verb: 'set', field: 'active', value: '3', confirm: false });
  assert.deepEqual(parseGlobalCommand('set nope 3'), { error: SET_USAGE });
  assert.deepEqual(parseGlobalCommand('set active'), { error: SET_USAGE });
  assert.deepEqual(parseGlobalCommand('appoint --defaults rsi'), { verb: 'appoint', names: 'rsi', useDefaults: true });
  assert.deepEqual(parseGlobalCommand('appoint'), { verb: 'appoint', names: '', useDefaults: false });
  assert.deepEqual(parseGlobalCommand('prefill docs'), { verb: 'prefill', names: 'docs', useDefaults: false });
  assert.deepEqual(parseGlobalCommand('frobnicate'), { error: GLOBAL_USAGE });
});

test('the grant summary names projects, launches and the PM mode', () => {
  const g = {
    grant_version: 4, seat_session_id: A, state: 'active', project_ids: [A, 'dddddddd-0000-4000-8000-000000000000'],
    allowed_launches: [{ provider: 'Claude', model: 'claude-opus-5-5', effort: 'high' }, { provider: 'Codex', model: 'gpt-6-astra', effort: null }],
    project_policy: { mode: 'execute' },
  };
  assert.equal(grantSummary(g, PROJECTS), 'Global manager v4 seat aaaaaaaa (active): projects [rsi, dddddddd]; launches [Claude/claude-opus-5-5/high, Codex/gpt-6-astra/any effort]; PM policy execute');
});

test(':manager portfolio parses list, show, appoint with --adopt, launch, configure, revoke', () => {
  assert.deepEqual(parsePortfolioCommand(''), { verb: 'list' });
  assert.deepEqual(parsePortfolioCommand('list'), { verb: 'list' });
  assert.deepEqual(parsePortfolioCommand('show global'), { verb: 'show', node: 'global' });
  assert.deepEqual(parsePortfolioCommand('revoke 1a2b'), { verb: 'revoke', node: '1a2b' });
  assert.deepEqual(parsePortfolioCommand('configure {}'), { verb: 'configure', json: '{}' });
  assert.deepEqual(parsePortfolioCommand('appoint pinnacle rsi --adopt n1,n2 docs'), { verb: 'appoint', label: 'pinnacle', projects: 'rsi docs', adopt: ['n1', 'n2'] });
  assert.deepEqual(parsePortfolioCommand('appoint swarm --adopt=n3'), { verb: 'appoint', label: 'swarm', projects: '', adopt: ['n3'] });
  assert.deepEqual(parsePortfolioCommand('appoint swarm --adopt'), { error: '--adopt needs <node,...>' });
  assert.deepEqual(parsePortfolioCommand('launch add global Codex/gpt-6-astra/high Claude/claude-opus-5-5'), { verb: 'launch', add: true, node: 'global', launches: ['Codex/gpt-6-astra/high', 'Claude/claude-opus-5-5'] });
  assert.deepEqual(parsePortfolioCommand('launch remove global Codex/x').add, false);
  assert.match(parsePortfolioCommand('launch drop global x').error, /launch add\|remove/);
  assert.deepEqual(parsePortfolioCommand('show'), { error: PORTFOLIO_USAGE });
  assert.deepEqual(parsePortfolioCommand('list extra'), { error: PORTFOLIO_USAGE });
});

const node = (id, label, state, projects) => ({ node_id: id, tier_label: label, state, authority_epoch: 2, grant: { grant_version: 3, seat_session_id: B, project_ids: projects, project_policy: { mode: 'execute' } } });

test('portfolio nodes resolve by id, prefix or a unique active label', () => {
  const nodes = [node(A, 'global', 'active', [A]), node(B, 'pinnacle', 'revoked', [B]), node(C, 'pinnacle', 'active', [C])];
  assert.equal(resolveNode(nodes, A.toUpperCase()), A);
  assert.equal(resolveNode(nodes, 'bbbb'), B);
  assert.equal(resolveNode(nodes, 'global'), A);
  assert.equal(resolveNode(nodes, 'pinnacle'), C);
  assert.match(resolveNode(nodes, 'swarm').error, /No portfolio node matches swarm/);
  assert.match(resolveNode([...nodes, node('dddddddd-0000-4000-8000-000000000000', 'global', 'active', [])], 'global').error, /names 2 portfolio nodes/);
  assert.deepEqual(freeProjects(PROJECTS, nodes), [B]);
  assert.equal(portfolioSummary(nodes[0], PROJECTS), 'global aaaaaaaa (active) v3 epoch 2 seat bbbbbbbb: projects [rsi]; PM policy execute');
});

test(':manager escalations parses list, all, rule, ack and undelivered', () => {
  assert.deepEqual(parseEscalationsCommand(''), { verb: 'list', includeClosed: false });
  assert.deepEqual(parseEscalationsCommand('all'), { verb: 'list', includeClosed: true });
  assert.deepEqual(parseEscalationsCommand('rule 1a2b3c4d Split the branch.'), { verb: 'rule', hop: '1a2b3c4d', ruling: 'Split the branch.' });
  assert.deepEqual(parseEscalationsCommand('rule 1a2b'), { error: ESCALATIONS_USAGE });
  assert.deepEqual(parseEscalationsCommand('ack 9f8e'), { verb: 'ack', notice: '9f8e' });
  assert.deepEqual(parseEscalationsCommand('ack a b'), { error: ESCALATIONS_USAGE });
  assert.deepEqual(parseEscalationsCommand('undelivered'), { verb: 'undelivered', after: null });
  assert.deepEqual(parseEscalationsCommand('undelivered c1'), { verb: 'undelivered', after: 'c1' });
  assert.deepEqual(parseEscalationsCommand('bogus'), { error: ESCALATIONS_USAGE });
});

test('ids resolve by a unique 4+ character prefix', () => {
  assert.equal(resolveId([A, B], 'aaaa', 'escalation'), A);
  assert.equal(resolveId([A, B], C, 'escalation'), C);
  assert.match(resolveId([A, B], 'aa', 'report').error, /4\+ character prefix/);
  assert.match(resolveId([A, B], 'ffff', 'report').error, /No open report matches ffff/);
  assert.match(resolveId([A, 'aaaaaaaa-9999-4999-8999-999999999999'], 'aaaa', 'escalation').error, /names 2 escalations/);
});

test('the escalation queue reads like the TUI lines', () => {
  const hop = { hop_id: A, state: 'open', hop: 2, source_ref: 'project:x', project_id: B, reason: 'need a call\nmore', ruling: null };
  const notice = { message_id: B, state: 'queued', source_ref: 'portfolio:y', body: 'weekly digest' };
  const mail = { message_id: C, kind: 'report', state: 'failed', source_ref: 's', target_ref: 't', settle_reason: 'gone', body: 'b' };
  assert.equal(hopSummary(hop), 'escalation aaaaaaaa (open, hop 2) from project:x in project bbbbbbbb: need a call');
  assert.equal(hopSummary({ ...hop, ruling: 'do X' }).endsWith(' -> ruled: do X'), true);
  assert.equal(noticeSummary(notice), 'report bbbbbbbb (unread) from portfolio:y: weekly digest');
  assert.equal(undeliveredSummary(mail), 'report cccccccc failed (s -> t): gone [b]');
  const rows = escalationRows({ escalations: [hop], notices: [notice], undelivered: [mail], next_undelivered_after: 'cur' });
  assert.deepEqual(rows.map((r) => r.queue), ['escalation', 'notice', 'undelivered', 'more']);
  assert.equal(rows[2].kind, 'report');
  assert.equal(rows[3].title, 'More undelivered mail: :manager escalations undelivered cur');
});

test(':manager node and :manager restart parse their sub-commands', () => {
  assert.deepEqual(parseNodeCommand(''), { verb: 'list' });
  assert.deepEqual(parseNodeCommand('tree'), { verb: 'tree' });
  assert.deepEqual(parseNodeCommand(`get ${A.toUpperCase()}`), { verb: 'get', id: A });
  assert.match(parseNodeCommand('get 123').error, /get <node UUID>/);
  assert.deepEqual(parseNodeCommand('configure {"x":1}'), { verb: 'configure', json: '{"x":1}' });
  assert.deepEqual(parseNodeCommand(`revoke ${A} 3`), { verb: 'revoke', id: A, epoch: 3 });
  assert.match(parseNodeCommand(`revoke ${A}`).error, /revoke <node UUID> <epoch>/);
  assert.deepEqual(parseNodeCommand('frob'), { error: NODE_USAGE });
  assert.equal(nodeSummary({ node_id: A, seat_root_session_id: B, state: 'active', grant_state: 'granted', selector: { mode: 'selected', group_ids: [1], epic_ids: [1, 2] }, grant_version: 4, authority_epoch: 1, direct_reports: 0 }),
    `${A} seat=${B} active/granted scope=1 Groups, 2 Epics grant=4 epoch=1 reports=0`);
  assert.match(nodeSummary({ selector: null }), /scope=revoked/);
  assert.equal(parseRestart(undefined), 'status');
  assert.equal(parseRestart('now'), 'now');
  assert.equal(parseRestart('cancel'), 'cancel');
  assert.match(parseRestart('later').error, /status\|now\|cancel/);
});

test('the restart status reads like OperatorRestartStatusV1::summary', () => {
  const base = { pending: true, state: 'staged', forced: false, blockers: ['worker_mid_turn', 'landing_in_progress'], turns_in_flight: 3, release_by: '2026-10-03T01:00:00Z', reason: null };
  assert.equal(restartDescribe(base), 'restart pending: waiting for a landing + 3 turns, release by 2026-10-03T01:00:00Z');
  assert.equal(restartDescribe({ ...base, forced: true }), 'restart pending: restarting now');
  assert.equal(restartDescribe({ ...base, state: 'restarting' }), 'restart in progress');
  assert.equal(restartDescribe({ ...base, blockers: [], turns_in_flight: 0, release_by: null }), 'restart pending: quiet, restarting');
  assert.equal(restartDescribe({ pending: false, state: 'failed', reason: 'cancelled_by_operator' }), 'Last restart failed: cancelled_by_operator');
  assert.equal(restartDescribe({ pending: false, state: null }), 'No restart pending.');
});

test('a cap-reduction refusal is recognised and previewed', () => {
  const err = `${CAP_REDUCTION}: active 8 -> 4 in rsi. Confirm with confirm_cap_reductions`;
  assert.equal(needsCapConfirm(err), true);
  assert.equal(needsCapConfirm('manager_v2_invalid_policy'), false);
  assert.equal(capPreview(err), `${CAP_REDUCTION}: active 8 -> 4 in rsi`);
});

test('the commands call Tauri commands the Rust module defines, with matching argument names', () => {
  const api = readFileSync(here('../src/managers-api.ts'), 'utf8');
  const rust = readFileSync(here('../src-tauri/src/manager_ops.rs'), 'utf8');
  const cmds = [...api.matchAll(/invoke<[^>]*>\('([a-z_]+)'/g)].map((m) => m[1]);
  const g3 = cmds.filter((c) => c.startsWith('mgr_') || ['global_manager_configure', 'portfolio_node_configure_request', 'portfolio_node_launches', 'manager_nodes_list',
    'manager_node_configure', 'operator_restart_status', 'operator_restart_force', 'operator_restart_cancel', 'operator_escalation_rule', 'operator_notice_ack',
    'operator_undelivered', 'manager_node_workspace', 'harness_manager_with_policy'].includes(c));
  assert.ok(g3.length >= 28, `${g3.length} G3 commands`);
  for (const c of g3) assert.match(rust, new RegExp(`pub (async )?fn ${c}\\(`), c);
  // Tauri maps camelCase invoke args onto the snake_case parameters.
  assert.match(api, /'mgr_global_appoint_plan', \{ current, nodes, sessionId, projectIds, useDefaults \}/);
  assert.match(rust, /current: Value,\s*nodes: Value,\s*session_id: Option<String>,\s*project_ids: Vec<String>,\s*use_defaults: bool,/);
  assert.match(api, /'operator_escalation_rule', \{ hopId, ruling, idempotencyKey \}/);
  assert.match(rust, /hop_id: String,\s*ruling: String,\s*idempotency_key: String,/);
  for (const m of ['ConfigureGlobalManager', 'ConfigurePortfolioNode', 'ConfigurePortfolioNodeLaunches', 'ListManagerNodes', 'ConfigureManagerNode', 'GetOperatorRestart',
    'ForceOperatorRestart', 'CancelOperatorRestart', 'RuleOperatorEscalation', 'AcknowledgeOperatorNotice', 'ListOperatorEscalations', 'GetManagerNodeWorkspace',
    'ConfigureHarnessManagerWithPolicy']) {
    assert.match(rust, new RegExp(`"${m}"`), m);
  }
});
