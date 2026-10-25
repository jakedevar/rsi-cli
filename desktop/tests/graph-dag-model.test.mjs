// Local #231, the pure parts: Esc on the Graph review canvas with the Vim keys switch off, the
// empty-canvas text, the status line and tooltips, and the DAG browser's operator buttons.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { GRAPH_BUTTON_TITLES, graphEmptyLines, graphKey, graphStatusLine, newDraft, openReview } from '../dist/graph-model.js';
import { DAG_ACTION_KEYS, dagActions, dagKey, initialDag } from '../dist/dag-model.js';

const wf = (nodes) => ({ version: '1.0', name: 'W', description: '', nodes, edges: [], metadata: {} });
const node = (id) => ({ id, name: id, type: 'action', instructions: '' });
const ctx = (over = {}) => ({ layout: { boxes: [], edges: [], cols: 0, rows: 0 }, executing: false, pickerCount: 0, ...over });
const press = (r, key, over = {}) => graphKey(r, { key, ctrl: false, shift: false }, ctx(over));

test('Esc on the canvas closes Graph review only with the switch off (escCloses)', () => {
  for (const nodes of [[], [node('a'), node('b')]]) {
    const r = openReview(newDraft(wf(nodes)));
    assert.deepEqual(press(r, 'Escape', { escCloses: true }).effect, { kind: 'close' });
    assert.equal(press(r, 'Escape').handled, false, 'Vim keys on: Esc is the TUI’s, nothing to do');
    assert.equal(press(r, 'Escape', { escCloses: false }).handled, false);
  }
});

test('with the switch off Esc first leaves a picker or the node detail, then closes', () => {
  const r = openReview(newDraft(wf([node('a'), node('b')])));
  const picker = { ...r, mode: { kind: 'picker', picker: 'saved', index: 0, back: 'navigate' } };
  const back = press(picker, 'Escape', { escCloses: true });
  assert.equal(back.review.mode.kind, 'navigate');
  assert.deepEqual(back.effect, { kind: 'none' });
  const detail = { ...r, mode: { kind: 'detail' } };
  assert.equal(press(detail, 'Escape', { escCloses: true }).review.mode.kind, 'navigate');
  const edit = { ...r, mode: { kind: 'edit', field: 'name' } };
  assert.equal(press(edit, 'Escape', { escCloses: true }).handled, false, 'the text box owns Esc while editing');
});

test('the empty canvas: key lines with Vim keys on, the buttons by name with them off', () => {
  assert.deepEqual(graphEmptyLines(true), { title: 'Empty graph', lines: ['t  open a starter topology', 'o  open a saved workflow', 'q  close'] });
  const off = graphEmptyLines(false);
  assert.deepEqual(off.lines, ['Click Starter to begin from a template or Saved to open a saved workflow.']);
  assert.doesNotMatch(off.lines.join(' '), /\b[toq]\s{2}|press/);
});

test('status line says what the view does, not "camera"; Recursive and Interrupt have tooltips', () => {
  assert.equal(graphStatusLine({ follow: true, pan: { x: 0, y: 0 }, selected: 1 }, 3), 'node 2/3 · view follows the running node');
  assert.equal(graphStatusLine({ follow: false, pan: { x: 4, y: -2 }, selected: 0 }, 0), 'node 0/0 · view panned (4,-2)');
  assert.equal(GRAPH_BUTTON_TITLES.R, 'Browse recursive graphs (read-only)');
  assert.equal(GRAPH_BUTTON_TITLES.x, 'Stop the running workflow');
});

const CAPS = Object.fromEntries(['recursive_dag_cancellation_control', 'recursive_dag_recovery_control', 'recursive_dag_recovery_status', 'recursive_dag_scheduler_control', 'recursive_dag_live_scheduler_control'].map((k) => [k, true]));
const graph = (extra = {}) => ({ graph: { id: 'g1', status: 'active', ...extra }, nodes: [], attempts: [], edges: [] });
const detail = (over = {}) => ({
  graph: graph(), operational: null, runs: [], runDetail: null, runEvents: [], cancellations: [], recovery: null, heartbeats: [], liveRecovery: null,
  live: [], validations: [], artifacts: { status: 'ready', items: [], nextCursor: null, hasMore: false, warnings: [], message: null }, warnings: [], ...over,
});
const ready = (over = {}) => ({ ...initialDag(null), status: 'ready', caps: CAPS, hydrated: 'g1', detail: detail(), ...over });
const byId = (s) => Object.fromEntries(dagActions(s).map((a) => [a.id, a]));

test('DAG action buttons are enabled from the selection and the capabilities, as the keys are gated', () => {
  const a = byId(ready());
  assert.equal(a['cancel-graph'].enabled, true);
  assert.equal(a['cancel-run'].enabled, false);
  assert.match(a['cancel-run'].title, /^Unavailable: no scheduler run is selected/);
  assert.equal(a.recover.enabled, false);
  assert.equal(a.inspect.enabled, false);
  assert.equal(a['run-fake'].enabled, true);
  const withRun = byId(ready({ detail: detail({ runs: [{ id: 'r1', status: 'running' }] }), panel: 'runs' }));
  assert.equal(withRun['cancel-run'].enabled, true);
  const art = { id: 'a1', kind: 'diff' };
  const inspecting = byId(ready({ panel: 'artifacts', detail: detail({ artifacts: { status: 'ready', items: [art], nextCursor: null, hasMore: false, warnings: [], message: null } }) }));
  assert.equal(inspecting.inspect.enabled, true);
  assert.equal(inspecting['artifact-next'].enabled, true);
  const noCap = byId(ready({ caps: {} }));
  assert.equal(noCap['cancel-graph'].enabled, false);
  assert.match(noCap['cancel-graph'].title, /recursive_dag_cancellation_control capability disabled/);
  const busy = byId(ready({ control: { kind: 'steps', live: false, input: '', error: null } }));
  assert.equal(busy['cancel-graph'].enabled, false, 'one control at a time');
  assert.equal(byId(initialDag(null))['cancel-graph'].enabled, false, 'not loaded');
});

test('each action presses the key sequence the TUI uses, so click and key reach the same control', () => {
  let s = ready();
  for (const key of DAG_ACTION_KEYS['cancel-graph']) s = dagKey(s, { key }).state;
  assert.deepEqual(s.control, { kind: 'reason', target: 'graph', input: '', error: null });
  s = ready({ detail: detail({ runs: [{ id: 'r1', status: 'running' }] }) });
  for (const key of DAG_ACTION_KEYS['cancel-run']) s = dagKey(s, { key }).state;
  assert.equal(s.control.kind, 'reason');
  assert.equal(s.control.target, 'run');
  s = ready();
  s = dagKey(s, { key: DAG_ACTION_KEYS['run-live'][0] }).state;
  assert.deepEqual(s.control, { kind: 'steps', live: true, input: '', error: null });
});
