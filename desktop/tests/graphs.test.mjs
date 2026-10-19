// Graphs and topology (hub #3013, slice 11): the ported layout (deterministic
// positions, no overlaps on the real starter templates, cycle handling), the
// graph review keys and edits, the topology preview and gR rules, the DAG
// browser's rows, keys and gated controls, the ESP Square, the registry
// entries and the request shapes the webview sends to the Tauri commands.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import {
  directionalNeighbor, layoutWorkflowGraph, nodeOuterWidth, rectsOverlap, stableLayers, viewportOrigin,
} from '../dist/graph-layout.js';
import {
  applyTemplate, bindFailureMessage, blockerMessage, decodeDefinition, deleteNode, effectiveTopology, executeParams, graphKey, newDraft,
  nodeEditable, openReview, parseTopologyResolve, resolveParams, runViewText, savedPickerEntries, sessionToBind, undo, workflowDocument,
} from '../dist/graph-model.js';
import {
  checkRunEpic, chooseTopologyEpic, executeTopologyParams, explainExecuteTopologyError, parseTopologyRun, previewText,
  topologyPreview,
} from '../dist/topology-preview.js';
import {
  artifactPage, cursorChanged, dagKey, heartbeatRows, initialDag, panelRows, parseBudget, parseMaxSteps, parseReason,
  sortGraphs, taskTreeRows,
} from '../dist/dag-model.js';
import {
  ROUNDS, espKey, espPValue, gameOverMessage, interpretation, keyToIndex, moveCursor, newGame, roundDetailsJson, saveParams,
} from '../dist/esp-model.js';
import { ACTIONS, keyHints, unavailable } from '../dist/registry.js';
import { GRAPHS_ACTIONS, GRAPHS_RULES } from '../dist/actions/graphs.js';

const here = (p) => fileURLToPath(new URL(p, import.meta.url));
const STARTERS = JSON.parse(readFileSync(here('./fixtures/graph-starters.json'), 'utf8'));
const G = '0b7c8a52-5f63-4a9e-9d27-3c1a9a5b6e10';
const T = 'e2cae1d4-1833-48eb-a7e8-6f1dd480fdae';

const node = (id, name = id.toUpperCase(), type = 'action') => ({ id, name, type, instructions: '' });
const wf = (nodes, edges, metadata = {}) => ({ version: '1.0', name: 'W', description: '', nodes, edges, metadata });
const edge = (source, target, extra = {}) => ({ source, target, ...extra });

// ---------- layout ----------

test('a chain lays out left to right at the TUI positions', () => {
  const l = layoutWorkflowGraph(wf([node('a'), node('b'), node('c')], [edge('a', 'b'), edge('b', 'c')]));
  assert.deepEqual(l.nodes.map((n) => n.rect), [
    { x: 0, y: 0, width: 16, height: 3 },
    { x: 28, y: 0, width: 16, height: 3 },
    { x: 56, y: 0, width: 16, height: 3 },
  ]);
  // Right anchor (15,1) → lane x 17 → left anchor (28,1); the zero-length jog is dropped.
  assert.deepEqual(l.edges[0].segments, [
    { start: { x: 15, y: 1 }, end: { x: 17, y: 1 } },
    { start: { x: 17, y: 1 }, end: { x: 28, y: 1 } },
  ]);
  assert.deepEqual(l.edges[0].arrow, { position: { x: 28, y: 1 }, glyph: '>' });
  assert.deepEqual(l.bounds, { x: 0, y: 0, width: 72, height: 3 });
  assert.equal(l.hasCycleEdges, false);
});

test('a layer is centred in the tallest layer and box widths follow the title', () => {
  const l = layoutWorkflowGraph(wf([node('a'), node('b'), node('c', 'A much longer node title here'), node('d')],
    [edge('a', 'b'), edge('a', 'c'), edge('b', 'd'), edge('c', 'd')]));
  assert.equal(nodeOuterWidth('A much longer node title here'), 30); // clamped to 28 + border
  assert.equal(nodeOuterWidth(''), 16);
  assert.deepEqual(l.nodes.map((n) => [n.rect.x, n.rect.y]), [[0, 0], [35, 0], [28, 7], [70, 0]]);
});

test('disconnected components stack top to bottom with the component gap', () => {
  const l = layoutWorkflowGraph(wf([node('a'), node('b'), node('c'), node('d')], [edge('a', 'b'), edge('c', 'd')]));
  assert.equal(l.nodes[0].rect.y, 0);
  assert.equal(l.nodes[2].rect.y, 9); // bottom 2 + 1 + gap 6
  assert.ok(l.nodes[3].rect.y > l.nodes[1].rect.y + 2);
});

test('a cycle is broken deterministically and flagged, never thrown', () => {
  const l = layoutWorkflowGraph(wf([node('a'), node('b'), node('c')], [edge('a', 'b'), edge('b', 'c'), edge('c', 'a')]));
  assert.equal(l.hasCycleEdges, true);
  assert.equal(l.nodes.length, 3);
  assert.equal(l.edges.length, 3);
  assert.deepEqual(stableLayers([0, 1, 2], [{ sourceIndex: 0, targetIndex: 1 }, { sourceIndex: 1, targetIndex: 0 }]),
    { layers: [[2], [0], [1]], cycleBroken: true }); // the free node first, then the broken cycle
  // The back edge c → a routes around the top, above the boxes (the component shifts down to make room).
  const back = l.edges[2];
  assert.ok(back.segments.some((s) => s.start.y < l.nodes[0].rect.y));
  assert.equal(l.nodes[0].rect.y, 2);
  // Self loops and dangling edges do not break the layout.
  const odd = layoutWorkflowGraph(wf([node('a')], [edge('a', 'a'), edge('a', 'ghost')]));
  assert.equal(odd.nodes.length, 1);
  assert.equal(odd.edges.length, 1);
});

test('the layout is deterministic and has no overlapping boxes on every starter template', () => {
  const names = Object.keys(STARTERS);
  assert.ok(names.length >= 10, `starter fixture has ${names.length} templates`);
  for (const name of names) {
    const w = STARTERS[name];
    const a = layoutWorkflowGraph(w);
    const b = layoutWorkflowGraph(JSON.parse(JSON.stringify(w)));
    assert.deepEqual(a, b, `${name}: same input, same layout`);
    assert.equal(a.nodes.length, w.nodes.length);
    for (let i = 0; i < a.nodes.length; i++) {
      for (let j = i + 1; j < a.nodes.length; j++) {
        assert.equal(rectsOverlap(a.nodes[i].rect, a.nodes[j].rect), false, `${name}: ${w.nodes[i].id} overlaps ${w.nodes[j].id}`);
      }
    }
    for (const e of a.edges) {
      for (const s of e.segments) assert.ok(s.start.x === s.end.x || s.start.y === s.end.y, `${name}: orthogonal segments`);
    }
  }
  // The TUI's own layout tests, on the same templates.
  const h = layoutWorkflowGraph(STARTERS.horizontal);
  assert.ok(h.nodes[0].rect.x < h.nodes[1].rect.x && h.nodes[1].rect.x < h.nodes[2].rect.x);
  assert.equal(directionalNeighbor(h, 0, 'right'), 1);
  assert.ok(layoutWorkflowGraph(STARTERS.pingpong).edges.some((e) => e.kind === 'visual-loop'));
  const v = layoutWorkflowGraph(STARTERS.vertical);
  const follow = viewportOrigin(v, 32, 12, 0, null);
  const manual = viewportOrigin(v, 32, 12, 0, { x: 8, y: 4 });
  assert.ok(manual.x >= follow.x && manual.y >= follow.y);
});

test('neighbour search and the viewport clamp', () => {
  const l = layoutWorkflowGraph(wf([node('a'), node('b'), node('c'), node('d')], [edge('a', 'b'), edge('a', 'c'), edge('b', 'd'), edge('c', 'd')]));
  assert.equal(directionalNeighbor(l, 0, 'left'), null);
  assert.equal(directionalNeighbor(l, 1, 'down'), 2);
  assert.equal(directionalNeighbor(l, 2, 'up'), 1);
  // A graph smaller than the viewport is centred.
  const small = layoutWorkflowGraph(wf([node('a')], []));
  assert.deepEqual(viewportOrigin(small, 40, 9, 0, null), { x: -12, y: -3 });
});

// ---------- graph review ----------

const ctx = (review, over = {}) => ({ layout: layoutWorkflowGraph(review.draft.workflow), executing: false, pickerCount: 0, ...over });
const press = (review, key, over = {}, mods = {}) => graphKey(review, { key, ...mods }, ctx(review, over));

test('navigate keys move by the layout and edges, Enter opens the detail', () => {
  let r = openReview(newDraft(wf([node('a'), node('b'), node('c')], [edge('a', 'b'), edge('b', 'c')])));
  r = press(r, 'l').review;
  assert.equal(r.selected, 1);
  r = press(r, ']').review;
  assert.equal(r.selected, 2);
  r = press(r, '[').review;
  assert.equal(r.selected, 1);
  r = press(r, 'G').review;
  assert.equal(r.selected, 2);
  assert.equal(press(r, 'l').effect.kind, 'focus-dashboard'); // right edge
  r = press(r, 'g').review;
  assert.equal(r.selected, 0);
  r = press(r, 'L').review;
  assert.equal(r.follow, false);
  assert.deepEqual(r.pan, { x: 12, y: 0 });
  r = press(r, 'c').review;
  assert.equal(r.follow, true);
  r = press(r, 'Enter').review;
  assert.equal(r.mode.kind, 'detail');
  assert.equal(press(r, 'Escape').review.mode.kind, 'navigate');
  assert.equal(press(r, 'q').effect.kind, 'close');
  // Space and `:` are not the pane's: the app keys get them.
  assert.equal(press(r, ' ').handled, false);
});

test('edits are undoable and the selection stays in range', () => {
  let r = openReview(newDraft(wf([node('a'), node('b')], [edge('a', 'b')])));
  r = { ...r, selected: 1 };
  r = press(r, 'd').review;
  assert.deepEqual(r.draft.workflow.nodes.map((n) => n.id), ['a']);
  assert.equal(r.draft.workflow.edges.length, 0);
  assert.equal(r.selected, 0);
  assert.equal(r.draft.persistence, 'dirty');
  r = press(r, 'u').review;
  assert.deepEqual(r.draft.workflow.nodes.map((n) => n.id), ['a', 'b']);
  // Detail: j/k walk the node's edges, x deletes the selected one.
  r = press({ ...r, selected: 0 }, 'Enter').review;
  r = press(r, 'x').review;
  assert.equal(r.draft.workflow.edges.length, 0);
  assert.equal(undo(r).draft.workflow.edges.length, 1);
  // i opens the name editor with the current value.
  const ed = press(r, 'i');
  assert.equal(ed.effect.kind, 'begin-edit');
  assert.equal(ed.review.mode.kind, 'edit');
  assert.equal(ed.review.buffer, 'A');
  assert.equal(deleteNode(openReview(newDraft(wf([], [])))).draft.workflow.nodes.length, 0);
});

test('a running draft and a bridged recursive graph refuse writes', () => {
  const base = openReview(newDraft(wf([node('a')], [])));
  assert.equal(press(base, 'd', { executing: true }).effect.kind, 'refused');
  assert.equal(press(base, 'r', { executing: true }).effect.kind, 'refused');
  assert.equal(press(base, 'x', { executing: true }).effect.kind, 'interrupt');
  assert.equal(press(base, 'r').effect.kind, 'run');
  const locks = JSON.stringify({ a: { locked: true, reason: 'running' } });
  const bridged = openReview(newDraft(wf([node('a')], [], { recursive_node_locks: locks }), { origin: { kind: 'recursive', graphId: G } }));
  assert.equal(press(bridged, 'd').effect.kind, 'refused');
  assert.equal(press(bridged, 'r').effect.kind, 'refused');
  assert.equal(press(bridged, 'R').effect.kind, 'open-picker');
  const detail = press(bridged, 'Enter').review;
  assert.deepEqual(press(detail, 'I').effect, { kind: 'refused', why: 'Locked: running' });
  assert.deepEqual(nodeEditable(bridged.draft, 0), { editable: false, reason: 'running' });
  const open = newDraft(wf([node('a')], [], { recursive_node_locks: JSON.stringify({ a: { locked: false } }) }), { origin: { kind: 'recursive', graphId: G } });
  assert.deepEqual(nodeEditable(open, 0), { editable: true, reason: null });
  assert.equal(nodeEditable(newDraft(wf([node('a')], []), { origin: { kind: 'recursive', graphId: G } }), 0).reason, 'lock state unknown');
});

test('pickers move, page and choose; an empty graph has only t and o', () => {
  const empty = openReview(newDraft(wf([], [])));
  assert.equal(press(empty, 'j').handled, false);
  let r = press(empty, 't').review;
  assert.deepEqual(r.mode, { kind: 'picker', picker: 'topology', index: 0, back: 'navigate' });
  r = press(r, 'j', { pickerCount: 12 }).review;
  r = press(r, 'd', { pickerCount: 12 }, { ctrl: true }).review;
  assert.equal(r.mode.index, 11);
  assert.deepEqual(press(r, 'Enter', { pickerCount: 12 }).effect, { kind: 'choose', picker: 'topology', index: 11 });
  assert.equal(press(r, 'Escape').review.mode.kind, 'navigate');
  // A template fills an empty unsaved draft in place (undoable).
  const t = applyTemplate(empty, STARTERS.horizontal, null);
  assert.equal(t.replaced, true);
  assert.equal(t.review.history.length, 1);
  assert.equal(t.review.draft.persistence, 'dirty');
  assert.equal(t.review.draft.key, empty.draft.key, 'filled in place: same draft');
  const fresh = applyTemplate(t.review, STARTERS.hub, null);
  assert.equal(fresh.replaced, false);
  assert.notEqual(fresh.review.draft.key, t.review.draft.key, 'a new draft beside the first');
});

test('documents, the run request and the run view are the TUI’s', () => {
  const d = newDraft(STARTERS.horizontal, { projectId: T });
  const doc = workflowDocument(d, null, '2026-10-18T00:00:00Z', G, null);
  assert.deepEqual(doc.workflow, { id: G, title: STARTERS.horizontal.name, stage: 'Planning', artifact_path: null, project_id: T, created_at: '2026-10-18T00:00:00Z', updated_at: '2026-10-18T00:00:00Z' });
  assert.deepEqual(executeParams(G, d.workflow, T), { workflow_id: G, workflow: d.workflow, input: null, dry_run: false, project_id: T, working_dir: null, parent_id: null });
  assert.equal(blockerMessage({ executable: true, diagnostics: [] }), null);
  assert.equal(blockerMessage({ executable: false, diagnostics: [{ code: 'x', severity: 'error', subject: { path: 'nodes[0]' }, message: 'no entry' }] }), 'Execution blocked (1 issues): no entry');
  const run = runViewText(wf([node('a'), node('b')], []), {
    execution_id: G, workflow_id: G, workflow_name: 'W', status: 'running', row_version: 3,
    updates: [{ node_id: 'a', sequence: 1, status: 'running', node_state: 'succeeded' }],
    node_attempts: [{ node_id: 'b', iteration: 1, attempt_no: 2 }], current_nodes: ['b'],
    on_call: { seat: { kind: 'project_manager' }, live: false }, waiting: { node_id: 'b', reason: 'review' },
    rulings: [{ node_id: 'b', question: 'ship?', status: 'open', answered_by: null }],
  }, false);
  assert.deepEqual(run.map(([t]) => t), ['W running', '  a  succeeded', '> b  pending  attempt 2 round 2', 'on-call: project manager (not live)', 'waiting: b - review', 'rulings (1)', '  b [open] ship?']);
  assert.equal(run[2][1], 'current');
});

test('stored definitions, the saved picker and the effective topology', () => {
  assert.equal(decodeDefinition({ nodes: 'x' }, 'Row').workflow.name, 'Row');
  assert.ok(decodeDefinition({ nodes: 'x' }, 'Row').error);
  assert.equal(decodeDefinition(STARTERS.hub, 'Row').error, null);
  const rows = [
    { id: '1', title: 'b', stage: 'Planning', created_at: '', updated_at: '2026-01-01' },
    { id: '2', title: '/raw prompt', stage: 'Planning', created_at: '', updated_at: '2026-03-01' },
    { id: '3', title: 'a', stage: 'Planning', created_at: '', updated_at: '2026-02-01' },
    { id: '4', title: 'a0', stage: 'Planning', created_at: '', updated_at: '2026-01-01' },
  ];
  assert.deepEqual(savedPickerEntries(rows).map((r) => r.id), ['3', '4', '1']);
  const sessions = [
    { id: 'e', workflow_id: 'wf-epic' },
    { id: 'c', parent_id: 'e' },
    { id: 'o', parent_id: 'e', workflow_id_override: 'wf-over' },
    { id: 'x', parent_id: 'y' }, { id: 'y', parent_id: 'x' },
  ];
  assert.equal(effectiveTopology(sessions, 'c'), 'wf-epic');
  assert.equal(effectiveTopology(sessions, 'o'), 'wf-over');
  assert.equal(effectiveTopology(sessions, 'x'), null);
  assert.equal(effectiveTopology(sessions, 'nope'), null);
});

test(':topology-resolve parses like the TUI and binds the key to the row version', () => {
  assert.deepEqual(parseTopologyResolve(`${G} retry`), { executionId: G, action: 'retry', confirm: null });
  assert.deepEqual(parseTopologyResolve('discard abc123'), { executionId: null, action: 'discard', confirm: 'abc123' });
  assert.ok('error' in parseTopologyResolve('nuke'));
  assert.ok('error' in parseTopologyResolve('accept a b'));
  assert.deepEqual(resolveParams(G, T, 'accept', 4, null), { execution_id: G, attempt_id: T, action: 'accept', expected_row_version: 4, idempotency_key: `tui:${G}:${T}:accept:4` });
});

// ---------- topology preview and gR ----------

const topo = (nodes, edges = []) => ({ nodes: nodes.map(([id, label, prereqs = []]) => ({ id, kind: 'Task', label, prereqs })), edges: edges.map(([from, to, loop = false]) => ({ from, to, loop_edge: loop })) });

test('the topology preview orders nodes, shows prereqs and keeps the TUI placeholders', () => {
  const lines = topologyPreview(topo([['plan', 'Plan'], ['build', 'Build', ['plan']], ['review', 'Review', ['build']]], [['plan', 'build'], ['build', 'review'], ['review', 'build', true]]));
  assert.deepEqual(lines.map(previewText), ['▶ Plan', '└─ Build (after plan)', '└─ Review (after build)']);
  assert.deepEqual(topologyPreview(topo([])).map(previewText), ['[topology preview unavailable — empty]']);
  assert.deepEqual(topologyPreview(topo([['a', 'A']], [['a', 'ghost']])).map(previewText), ['[topology preview unavailable — dangling edge]']);
  assert.deepEqual(topologyPreview(topo([['a', 'A'], ['b', 'B']], [['a', 'b'], ['b', 'a']])).map(previewText), ['[topology preview unavailable — unresolved cycle]']);
  const many = topologyPreview(topo(Array.from({ length: 12 }, (_, i) => [`n${String(i).padStart(2, '0')}`, `N${i}`])));
  assert.equal(many.length, 8);
  assert.equal(previewText(many[7]), '  …');
  // Roots pop greatest id first, as the TUI's sorted stack does.
  assert.equal(previewText(many[0]), '▶ N11');
});

test('gR refuses like the TUI and runs ExecuteTopology under the Epic', () => {
  assert.deepEqual(checkRunEpic(null), { kind: 'refused', why: 'No session selected', error: false });
  assert.equal(checkRunEpic({ id: G, session_kind: 'Task' }).why, 'Not an Epic — gR only fires on Epic containers');
  assert.equal(checkRunEpic({ id: G, session_kind: 'Epic' }).why, 'Epic has no topology binding — set one via gv overlay first');
  assert.deepEqual(checkRunEpic({ id: G, session_kind: 'Epic', workflow_id: T, project_id: null }), { kind: 'ready', topologyId: T, epicId: G, projectId: null });
  assert.deepEqual(executeTopologyParams(T, null, {}, G), { topology_id: T, project_id: null, inputs: {}, parent_id: G });
  assert.match(explainExecuteTopologyError('operator_owner_no_manager'), /^ExecuteTopology refused: this project has no appointed live project manager/);
  assert.equal(explainExecuteTopologyError('boom'), 'ExecuteTopology failed: boom');
});

test(':topology run parses and picks the Epic the TUI would', () => {
  assert.deepEqual(parseTopologyRun('run issue-implement-review-land'), { name: 'issue-implement-review-land', epic: null });
  assert.ok('error' in parseTopologyRun('go x'));
  const sessions = [
    { id: 'aaaa1111', title: 'Alpha', session_kind: 'Epic', status: 'Running', project_id: 'p' },
    { id: 'bbbb2222', title: 'Beta', session_kind: 'Epic', status: 'Running', project_id: 'p' },
    { id: 'cccc3333', title: 'Old', session_kind: 'Epic', status: 'Archived', project_id: 'p' },
    { id: 'kid', session_kind: 'Task', status: 'Running', project_id: 'p', parent_id: 'bbbb2222' },
  ];
  assert.deepEqual(chooseTopologyEpic(sessions, 'p', 'alpha', null), { epic: 'aaaa1111' });
  assert.deepEqual(chooseTopologyEpic(sessions, 'p', 'aaaa', null), { epic: 'aaaa1111' });
  assert.deepEqual(chooseTopologyEpic(sessions, 'p', null, 'kid'), { epic: 'bbbb2222' });
  assert.match(chooseTopologyEpic(sessions, 'p', null, null).error, /^name an Epic/);
  assert.match(chooseTopologyEpic(sessions, 'q', null, null).error, /no live Epic/);
});

// ---------- DAG browser ----------

const caps = (over = {}) => ({ recursive_dag_inspection: true, recursive_dag_run_inspection: true, recursive_dag_recovery_status: true, ...over });
const detail = (over = {}) => ({
  graph: { graph: { id: G, root_task_id: 'r', status: 'active' }, nodes: [
    { id: 'r', title: 'root', status: 'running', created_at: '1' },
    { id: 'k2', title: 'b', status: 'ready', parent_task_id: 'r', created_at: '3' },
    { id: 'k1', title: 'a', status: 'succeeded', parent_task_id: 'r', created_at: '2' },
    { id: 'o', title: 'orphan', status: 'blocked', created_at: '4' },
  ], edges: [], attempts: [] },
  operational: null, runs: [{ id: 'run1', status: 'running' }], runDetail: null, runEvents: [], cancellations: [], recovery: null,
  heartbeats: [{ live_attempt_id: 'la1', heartbeat_status: 'stale' }], liveRecovery: null,
  live: [{ summary: { task_id: 'k1' }, heartbeat: { live_attempt_id: 'la1', heartbeat_status: 'fresh' } }, { summary: { task_id: 'k2' }, heartbeat: { live_attempt_id: 'la2', heartbeat_status: 'fresh' } }],
  validations: [], artifacts: { status: 'ready', items: [], nextCursor: null, hasMore: false, warnings: [], message: null }, warnings: [], ...over,
});
const ready = (over = {}) => ({ ...initialDag(null), status: 'ready', caps: caps(), graphs: [{ id: G, updated_at: '2026-01-01' }], hydrated: G, detail: detail(), ...over });

test('task tree paths, merged rows and the inventory order', () => {
  assert.deepEqual(taskTreeRows(detail()).map((r) => [r.path, r.task.id]), [['1', 'r'], ['1.1', 'k1'], ['1.2', 'k2'], ['2', 'o']]);
  // Heartbeats merge the stale list and the live attempts without duplicates.
  assert.deepEqual(heartbeatRows(detail()).map((h) => [h.live_attempt_id, h.heartbeat_status]), [['la1', 'stale'], ['la2', 'fresh']]);
  assert.deepEqual(sortGraphs([{ id: 'a', updated_at: '1' }, { id: 'b', updated_at: '3' }, { id: 'c', updated_at: '2' }]).map((g) => g.id), ['b', 'c', 'a']);
  const s = ready({ graphs: [{ id: G }, { id: T }] });
  assert.equal(cursorChanged(s), false);
  assert.equal(cursorChanged({ ...s, cursor: { ...s.cursor, graphs: 1 } }), true);
});

test('DAG keys cycle panels, move rows and load the selected graph', () => {
  let s = ready();
  s = dagKey(s, { key: 'Tab' }).state;
  assert.equal(s.panel, 'tasks');
  s = dagKey(s, { key: 'G' }).state;
  assert.equal(s.cursor.tasks, 3);
  assert.equal(panelRows(s).length, 4);
  s = dagKey(s, { key: 'h' }).state;
  s = dagKey(s, { key: 'h' }).state;
  assert.equal(s.panel, 'artifacts'); // wraps
  assert.deepEqual(dagKey({ ...s, panel: 'graphs' }, { key: 'Enter' }).effect, { kind: 'load', graphId: G });
  assert.deepEqual(dagKey(s, { key: 'd', ctrl: true }).effect, { kind: 'scroll', by: 10 });
  assert.equal(dagKey(s, { key: 'q' }).effect.kind, 'close');
  assert.equal(dagKey(s, { key: ' ' }).handled, false);
});

test('DAG controls stay behind their capability and a typed confirmation', () => {
  const off = ready();
  assert.match(dagKey(off, { key: 'R' }).effect.text, /recursive_dag_scheduler_control capability disabled/);
  assert.match(dagKey(off, { key: 'C' }).effect.text, /cancellation_control capability disabled/);
  assert.match(dagKey(off, { key: 'c' }).effect.text, /recovery_control capability disabled/);
  let s = ready({ caps: caps({ recursive_dag_scheduler_control: true, recursive_dag_cancellation_control: true }) });
  s = dagKey(s, { key: 'R' }).state;
  assert.deepEqual(s.control, { kind: 'steps', live: false, input: '', error: null });
  s = dagKey(s, { key: 'Enter' }).state;
  assert.equal(s.control.error, 'max_steps is required');
  s = { ...s, control: { ...s.control, input: '5' } };
  const sub = dagKey(s, { key: 'Enter' });
  assert.equal(sub.effect.kind, 'submit');
  assert.equal(sub.state.control.kind, 'none');
  // C r needs an active run selected; C t is reserved.
  let c = dagKey(ready({ caps: caps({ recursive_dag_cancellation_control: true }) }), { key: 'C' }).state;
  assert.equal(c.control.kind, 'cancel-prefix');
  assert.equal(dagKey(c, { key: 'r' }).state.control.kind, 'reason');
  assert.match(dagKey(c, { key: 't' }).effect.text, /reserved/);
  c = dagKey(c, { key: 'g' }).state;
  assert.deepEqual(c.control, { kind: 'reason', target: 'graph', input: '', error: null });
  const done = ready({ caps: caps({ recursive_dag_cancellation_control: true }), detail: detail({ graph: { graph: { id: G, status: 'completed' }, nodes: [], edges: [], attempts: [] } }) });
  assert.match(dagKey(dagKey(done, { key: 'C' }).state, { key: 'g' }).effect.text, /not cancellable: Completed/);
  assert.equal(dagKey(c, { key: 'Escape' }).state.control.kind, 'none');
  // Recovery continuation needs visible deferred work.
  const rec = ready({ caps: caps({ recursive_dag_recovery_control: true }) });
  assert.match(dagKey(rec, { key: 'c' }).effect.text, /no deferred recovery work/);
  const withWork = ready({ caps: caps({ recursive_dag_recovery_control: true }), detail: detail({ recovery: { deferred_graph_count: 1, deferred_graphs: [{ graph_id: T }] } }) });
  assert.equal(dagKey(withWork, { key: 'c' }).state.control.kind, 'budget');
});

test('control inputs and artifact pages validate like the TUI', () => {
  assert.equal(parseMaxSteps('12'), 12);
  assert.deepEqual(parseMaxSteps('0'), { error: 'max_steps must be a positive integer' });
  assert.deepEqual(parseMaxSteps('1e3'), { error: 'max_steps must be digits only' });
  assert.equal(parseReason('  stop  '), 'stop');
  assert.ok(typeof parseReason('x'.repeat(241)) === 'object');
  assert.deepEqual(parseBudget('2', ''), { maxGraphs: 2, timeBudgetMs: null });
  assert.deepEqual(parseBudget('2', '0'), { maxGraphs: 2, timeBudgetMs: 0 });
  assert.ok('error' in parseBudget('0', ''));
  const page = artifactPage({ items: [{ id: 1, graph_id: G }, { id: 2, graph_id: T }], next_cursor: null, has_more: true, warnings: [] }, G, true, null);
  assert.deepEqual(page.items.map((i) => i.id), [1]);
  assert.deepEqual(page.warnings, ['dropped 1 artifact summary row(s) from a different graph', 'artifact summary page has more rows but no next cursor']);
  assert.equal(artifactPage(null, G, false, null).status, 'disabled');
  assert.equal(artifactPage(null, G, true, 'boom').status, 'error');
});

// ---------- ESP Square ----------

const fixed = (...xs) => { let i = 0; return () => xs[i++ % xs.length]; };

test('ESP rules: keys, cursor, p-value and the 12th guess saves once', () => {
  assert.equal(keyToIndex('u'), 0);
  assert.equal(keyToIndex('.'), 8);
  assert.equal(keyToIndex('a'), null);
  assert.equal(moveCursor(0, 'up'), 0);
  assert.equal(moveCursor(4, 'down'), 7);
  assert.equal(moveCursor(8, 'right'), 8);
  assert.ok(Math.abs(espPValue(0) - 1) < 1e-10);
  for (let k = 1; k <= 12; k++) assert.ok(espPValue(k) <= espPValue(k - 1));
  assert.equal(interpretation(0.03), 'Significant');
  assert.equal(interpretation(1), 'Normal');
  // Target is always square 0 (`u`); alternate u and i so the debounce never applies.
  const rng = fixed(0);
  let s = newGame(rng, 0);
  let effects = [];
  for (let i = 0; i < ROUNDS; i++) {
    const r = espKey(s, { key: i % 2 === 0 ? 'u' : 'i' }, rng, 1000 * (i + 1));
    s = r.state;
    effects.push(r.effect);
  }
  assert.equal(s.round, 12);
  assert.equal(s.correct, 6);
  assert.deepEqual(effects.filter((e) => e === 'save').length, 1);
  assert.equal(effects[11], 'save');
  assert.equal(s.message, gameOverMessage(6));
  assert.deepEqual(saveParams(s).total_rounds, 12);
  assert.equal(JSON.parse(roundDetailsJson(s.details))[1].hit, false);
  // After the game only r / Enter (new game) and Esc / q act.
  assert.equal(espKey(s, { key: 'u' }, rng, 99999).state, s);
  assert.equal(espKey(s, { key: 'Enter' }, rng, 99999).state.round, 0);
  assert.equal(espKey(s, { key: 'Escape' }, rng, 99999).effect, 'close');
});

test('ESP debounce, pass, cursor guess and score toggle', () => {
  const rng = fixed(0.5); // target 4 (k)
  let s = newGame(rng, 0);
  s = espKey(s, { key: 'u' }, rng, 100).state;
  assert.equal(espKey(s, { key: 'u' }, rng, 300).state, s); // same key within 600 ms
  assert.equal(espKey(s, { key: 'u' }, rng, 800).state.round, 2);
  const pass = espKey(s, { key: ';' }, rng, 900).state;
  assert.equal(pass.round, 1);
  assert.deepEqual(pass.flash, { index: 4, hit: false, until: 1200 });
  let c = espKey(s, { key: 'ArrowDown' }, rng, 900).state;
  assert.equal(c.cursor, 0); // first arrow shows the cursor top-left
  c = espKey(c, { key: 'ArrowDown' }, rng, 900).state;
  c = espKey(c, { key: 'ArrowRight' }, rng, 900).state;
  assert.equal(c.cursor, 4);
  const hit = espKey(c, { key: 'Enter' }, rng, 950).state;
  assert.equal(hit.correct, 1);
  assert.equal(espKey(s, { key: 't' }, rng, 900).state.interactive, false);
});

// ---------- registry and request shapes ----------

const snap = (over = {}) => ({
  open: null, cursorOnRow: false, rowCount: 0, attentionRows: 0, view: 'active', vimKeys: true,
  hideTools: false, showFinished: false, modalOpen: false, event: null, ...over,
});

test('the slice registers the TUI keys and commands with one rule each', () => {
  for (const a of GRAPHS_ACTIONS) {
    assert.ok(ACTIONS.some((x) => x.id === a.id), a.id);
    assert.equal(typeof GRAPHS_RULES[a.id], 'function', a.id);
  }
  assert.deepEqual(keyHints('graph-open').filter((k) => k === '<Space>v' || k === ':graph').sort(), [':graph', '<Space>v']);
  assert.ok(keyHints('esp-open').includes('<Space>gc'));
  assert.ok(keyHints('topology-run-epic').includes('gR'));
  assert.ok(keyHints('dag-open').includes(':dag'));
  assert.ok(keyHints('topology-run-issue').includes(':topology'));
  assert.ok(keyHints('topology-resolve').includes(':topology-resolve'));
  assert.equal(unavailable('graph-open', snap()), null);
  assert.match(unavailable('graph-run', snap()), /Open graph review first/);
  const g = { pane: 'graph', authored: true, executing: false, hasNodes: true, canUndo: false, epicReady: null };
  assert.equal(unavailable('graph-run', snap({ graphs: g })), null);
  assert.match(unavailable('graph-run', snap({ graphs: { ...g, authored: false } })), /read-only/);
  assert.match(unavailable('graph-interrupt', snap({ graphs: g })), /No active/);
  assert.match(unavailable('topology-run-issue', snap()), /Focus an Issue/);
});

test('every webview call names a Tauri command of graphs.rs with its typed args', async () => {
  const calls = [];
  globalThis.__TAURI__ = { core: { invoke: async (cmd, args) => { calls.push([cmd, args]); return cmd === 'graph_capabilities' ? {} : []; } } };
  const { graphsApi } = await import('../dist/graphs-api.js');
  await graphsApi.listRecursive(null, false);
  await graphsApi.executeTopology(T, null, 61, G);
  await graphsApi.dagCancelRun(G, 'why');
  await graphsApi.dagContinueRecovery(2, null);
  await graphsApi.espSave(3, 12, 0.2, '[]');
  await graphsApi.executeWorkflow(G, STARTERS.horizontal, T);
  await graphsApi.bindSessionWorkflow(T, G);
  assert.deepEqual(calls, [
    ['graph_list_recursive', { projectId: null, includeQuarantined: false }],
    ['topology_execute', { topologyId: T, projectId: null, issue: 61, parentId: G }],
    ['dag_cancel_run', { runId: G, reason: 'why' }],
    ['dag_continue_recovery', { maxGraphs: 2, timeBudgetMs: null }],
    ['esp_save_game', { score: 3, roundsPlayed: 12, pValue: 0.2, roundDetails: '[]' }],
    ['graph_execute_workflow', { workflowId: G, workflow: STARTERS.horizontal, projectId: T }],
    ['graph_bind_session_workflow', { sessionId: T, workflowId: G }],
  ]);
  // Every command the adapter can call exists in graphs.rs (and nowhere else in the webview's reach).
  const rust = readFileSync(here('../src-tauri/src/graphs.rs'), 'utf8');
  const adapter = readFileSync(here('../src/graphs-api.ts'), 'utf8');
  const names = [...adapter.matchAll(/invoke<[^>]*>\('([a-z_]+)'/g)].map((m) => m[1]);
  assert.ok(names.length >= 35);
  for (const n of names) assert.match(rust, new RegExp(`#\\[tauri::command\\]\\npub async fn ${n}\\(`), n);
  delete globalThis.__TAURI__;
});

// hub #3013 / local #124: the run links the selected session to the workflow (UpdateSessionWorkflow).
test('a successful run links the selected session only when its own workflow differs', () => {
  const W = '11111111-1111-4111-8111-111111111111';
  const sessions = [
    { id: 'a', workflow_id: null },
    { id: 'b', workflow_id: W },
    { id: 'c', workflow_id: null, workflow_id_override: W },
    { id: 'd', parent_id: 'b' },
  ];
  assert.equal(sessionToBind(sessions, 'a', W), 'a');
  // Already linked: no daemon call (needs_update is false in the TUI).
  assert.equal(sessionToBind(sessions, 'b', W), null);
  // An override is not the session's own workflow_id, so the TUI still links it.
  assert.equal(sessionToBind(sessions, 'c', W), 'c');
  // A child inheriting from its Epic has no workflow_id of its own: it is linked too.
  assert.equal(sessionToBind(sessions, 'd', W), 'd');
  // No selection or an unknown session: nothing to link.
  assert.equal(sessionToBind(sessions, null, W), null);
  assert.equal(sessionToBind(sessions, 'zzz', W), null);
  assert.match(bindFailureMessage(new Error('session not found')), /workflow ran, but linking the session failed: Error: session not found/);
});

test('graphs.ts binds after ExecuteWorkflow succeeds, never before, and the Rust forwarder takes typed ids', () => {
  const ts = readFileSync(here('../src/graphs.ts'), 'utf8');
  const run = ts.slice(ts.indexOf('export async function runDraft'), ts.indexOf('async function bindSelectedSession'));
  const exec = run.indexOf('graphsApi.executeWorkflow');
  const bind = run.indexOf('bindSelectedSession(');
  assert.ok(exec > 0 && bind > exec, 'bind comes after the execute call');
  // The local link is set before the daemon call, and a refusal keeps it (the TUI ignores the error).
  const fn = ts.slice(ts.indexOf('async function bindSelectedSession'));
  assert.ok(fn.indexOf('row.workflow_id = workflowId') < fn.indexOf('bindSessionWorkflow('));
  assert.match(fn, /catch \(e\) \{\s*toast\(bindFailureMessage\(e\)\)/);
  const rust = readFileSync(here('../src-tauri/src/graphs.rs'), 'utf8');
  assert.match(rust, /"UpdateSessionWorkflow",\s*bind_session_workflow_params\(&session_id, &workflow_id\)\?/);
});
