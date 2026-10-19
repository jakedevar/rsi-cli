// Gap sweep G2 (hub #3000): the Issues workspace's Dispatched and Sync tabs,
// the manual poll, T, PageUp, associated sessions and the dependency candidate
// search.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { unavailable } from '../dist/registry.js';
import { ISSUES_ACTIONS, ISSUES_RULES } from '../dist/actions/issues.js';
import { ISSUES_KEYS, idleKeys, resolveIssuesKey } from '../dist/issues-model.js';
import {
  CANDIDATE_LIMIT, DEFAULT_ISSUE_TOPOLOGY, associatedSessions, candidateQuery, candidateRequest, dispatchFields, dispatchKey,
  dispatchLine, moveDispatch, parseDispatched, pollSummary, reconcileAssociated, sortDispatched, stepTab, syncRows,
} from '../dist/issues-tracker-model.js';

const here = (p) => fileURLToPath(new URL(p, import.meta.url));
const issue = { id: 'i1', display_number: 7, title: 'T', status: 'Open', priority: 2, labels: [], body: '', row_version: 1, created_at: '', updated_at: '' };
const snap = (issues) => ({
  open: null, cursorOnRow: false, rowCount: 0, attentionRows: 0, view: 'active', vimKeys: true, hideTools: false,
  showFinished: false, modalOpen: false,
  issues: { active: true, hasProject: true, issue, nextPage: false, previousPage: false, operatorOnly: false, tab: 'local', session: false, ...issues },
});

test('the new workspace keys run their actions', () => {
  const key = (k) => resolveIssuesKey(idleKeys(), k, 0).result;
  assert.deepEqual(key('['), { kind: 'run', action: 'issues-tab-prev' });
  assert.deepEqual(key(']'), { kind: 'run', action: 'issues-tab-next' });
  assert.deepEqual(key('P'), { kind: 'run', action: 'issues-poll' });
  assert.deepEqual(key('T'), { kind: 'run', action: 'issues-topology' });
  assert.deepEqual(key('PageUp'), { kind: 'run', action: 'issues-less' });
  for (const id of ['issues-tab-prev', 'issues-tab-next', 'issues-poll', 'issues-topology', 'issues-less', 'issues-open-session']) {
    assert.ok(ISSUES_ACTIONS.some((a) => a.id === id), id);
    assert.equal(typeof ISSUES_RULES[id], 'function', id);
  }
  assert.equal(ISSUES_KEYS.P, 'issues-poll');
});

test('tabs wrap Local, Dispatched, Sync', () => {
  assert.equal(stepTab('local', 1), 'dispatched');
  assert.equal(stepTab('sync', 1), 'local');
  assert.equal(stepTab('local', -1), 'sync');
});

test('the rules follow the tab', () => {
  assert.equal(unavailable('issues-tab-next', snap({ tab: 'sync' })), null);
  assert.match(unavailable('issues-poll', snap({})), /Sync tab/);
  assert.equal(unavailable('issues-poll', snap({ tab: 'sync' })), null);
  assert.match(unavailable('issues-new', snap({ tab: 'dispatched' })), /Local tab/);
  assert.match(unavailable('issues-edit', snap({ tab: 'sync' })), /Local tab/);
  assert.equal(unavailable('issues-new', snap({})), null, 'the Local tab keeps its keys');
  assert.equal(unavailable('issues-topology', snap({})), null);
  assert.match(unavailable('issues-topology', snap({ issue: null })), /Select an Issue/);
  assert.equal(unavailable('issues-inspect', snap({ tab: 'dispatched', session: true })), null);
  assert.match(unavailable('issues-inspect', snap({ tab: 'dispatched' })), /dispatched Issue/);
  assert.match(unavailable('issues-inspect', snap({ tab: 'sync' })), /nothing to inspect/);
  assert.match(unavailable('issues-open-session', snap({})), /No session was launched/);
  assert.equal(unavailable('issues-open-session', snap({ session: true })), null);
  assert.equal(unavailable('issues-less', snap({})), null);
  assert.equal(DEFAULT_ISSUE_TOPOLOGY, 'issue-implement-review-land');
});

const rec = (session, issueId, at, over = {}) => ({
  issue_id: issueId, issue_identifier: `ENG-${issueId}`, tracker: 'linear', session_id: session, dispatched_at: at,
  last_reconciled_at: null, terminal_state: null, ...over,
});

test('dispatched records sort newest first and select by session plus tracker id', () => {
  const rows = sortDispatched(parseDispatched([rec('s1', '1', '2026-10-01T00:00:00Z'), rec('s2', '2', '2026-10-03T00:00:00Z'), { bogus: true }]));
  assert.deepEqual(rows.map((r) => r.session_id), ['s2', 's1']);
  assert.equal(dispatchKey(rows[0]), 's2|2');
  assert.equal(moveDispatch(rows, null, 1), 's2|2');
  assert.equal(moveDispatch(rows, 's2|2', 1), 's1|1');
  assert.equal(moveDispatch(rows, 's1|1', 5), 's1|1');
  assert.equal(moveDispatch([], null, 1), null);
  assert.equal(dispatchLine(rec('abcdef0123', '9', 'x', { terminal_state: 'Done' })), 'ENG-9 · linear · Done · session abcdef01');
  assert.deepEqual(dispatchFields(rows[0]).map((f) => f[0]), ['Issue', 'Tracker', 'Tracker id', 'Session', 'Dispatched', 'Reconciled', 'Terminal state']);
  assert.deepEqual(parseDispatched(null), []);
});

test('the Sync status and the poll result read as the TUI shows them', () => {
  const rows = syncRows({ enabled: true, tracker: 'linear', last_poll_at: null, next_poll_at: '2026-10-18T00:01:00Z', dispatched_count: 2, max_concurrent: 4, poll_interval_ms: 60000, active_states: ['Todo', 'In Progress'] });
  const get = (k) => rows.find((r) => r[0] === k)[1];
  assert.equal(get('Polling'), 'enabled');
  assert.equal(get('Interval'), '1 min');
  assert.equal(get('Last poll'), 'never');
  assert.equal(get('Dispatched'), '2 of 4 concurrent');
  assert.equal(get('Active states'), 'Todo, In Progress');
  assert.equal(pollSummary({ issues_found: 3, dispatched: 1, skipped_claimed: 1, skipped_blocked: 0, errors: [] }), 'Poll: 3 found, 1 dispatched, 1 already claimed, 0 blocked');
  assert.match(pollSummary({ issues_found: 0, dispatched: 0, skipped_claimed: 0, skipped_blocked: 0, errors: ['a', 'b'] }), /2 errors$/);
});

test('associated sessions are the ones launched for the Issue, by UUID', () => {
  const sessions = [{ id: 'b', issue_tracker_id: 'i1' }, { id: 'a', issue_tracker_id: 'i1' }, { id: 'c', issue_tracker_id: 'other' }, { id: 'd' }];
  assert.deepEqual(associatedSessions(sessions, 'i1').map((s) => s.id), ['a', 'b']);
  assert.equal(reconcileAssociated(['a', 'b'], 'b'), 'b');
  assert.equal(reconcileAssociated(['a', 'b'], 'gone'), 'a');
  assert.equal(reconcileAssociated([], 'x'), null);
});

test('the dependency field takes a number or words to search a bounded candidate page', () => {
  assert.deepEqual(candidateQuery('#12'), { number: 12 });
  assert.deepEqual(candidateQuery(' login bug '), { search: 'login bug' });
  assert.ok('error' in candidateQuery('  '));
  const req = candidateRequest('p1', 'login');
  assert.equal(req.project_id, 'p1');
  assert.equal(req.limit, CANDIDATE_LIMIT);
  assert.equal(JSON.stringify(req).includes('login'), true);
});

test('the Rust tracker commands exist and the run side calls them', () => {
  const rust = readFileSync(here('../src-tauri/src/issues_tracker.rs'), 'utf8');
  const dom = readFileSync(here('../src/issues.ts'), 'utf8');
  for (const [cmd, method] of [['dispatched_issues', 'ListDispatchedIssues'], ['issue_tracker_status', 'GetIssueTrackerStatus'], ['trigger_issue_poll', 'TriggerIssueTrackerPoll']]) {
    assert.match(rust, new RegExp(`pub async fn ${cmd}\\(`), cmd);
    assert.match(rust, new RegExp(`"${method}"`), method);
    assert.match(dom, new RegExp(`'${cmd}'`), cmd);
  }
  assert.match(dom, /dispatch\('topology-run-issue', \{ source: 'key', arg: `run \$\{DEFAULT_ISSUE_TOPOLOGY\}` \}\)/);
});
