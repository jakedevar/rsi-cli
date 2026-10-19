// Graph review (hub #3013, slice 11): the TUI's `<Space>v` / `:graph` overlay
// as pure state (crates/rsi/src/overlay/graph.rs, app/graph.rs and the run
// view in ui/overlay/graph.rs). The DOM side is graphs.ts; the layout is
// graph-layout.ts. No DOM; tested in tests/graphs.test.mjs.
import { type Direction, type GraphLayout, type Point, directionalNeighbor, HORIZONTAL_PAN_STEP, VERTICAL_PAN_STEP } from './graph-layout.js';

// ---------- the workflow document (rsi_graph::format, serde names) ----------

export type NodeType = 'action' | 'topology' | 'subgraph';
export interface PortDef { name: string; description?: string; field_type?: string | null; required?: boolean }
export interface NodeDef {
  id: string;
  name: string;
  type: NodeType;
  description?: string;
  instructions?: string;
  inputs?: PortDef[];
  outputs?: PortDef[];
  tags?: string[];
  pattern?: string | null;
  [extra: string]: unknown;
}
export interface EdgeDef {
  source: string;
  target: string;
  source_port?: string | null;
  target_port?: string | null;
  label?: string | null;
  [extra: string]: unknown;
}
export interface WorkflowDefinition {
  version: string;
  name: string;
  description?: string;
  nodes: NodeDef[];
  edges: EdgeDef[];
  metadata?: Record<string, unknown>;
}

/** A `workflows` row (rsi_common::types::Workflow). */
export interface WorkflowRow {
  id: string;
  title: string;
  stage: string;
  artifact_path?: string | null;
  project_id?: string | null;
  created_at: string;
  updated_at: string;
}
export interface WorkflowDocument { workflow: WorkflowRow; definition: unknown }

export type ExecutionStatus = 'accepted' | 'running' | 'succeeded' | 'failed' | 'interrupted' | 'blocked' | string;
export type NodeState = 'running' | 'succeeded' | 'failed' | 'skipped' | string;
export interface ExecutionUpdate {
  node_id?: string | null;
  sequence: number;
  status: ExecutionStatus;
  node_state?: NodeState | null;
  finished?: boolean;
  error?: string | null;
}
export interface ExecutionSnapshot {
  execution_id: string;
  workflow_id: string;
  workflow_name: string;
  status: ExecutionStatus;
  dry_run?: boolean;
  error?: string | null;
  updates?: ExecutionUpdate[];
  row_version?: number | null;
  blocked_attempt_id?: string | null;
  blocked_reason?: string | null;
  waiting?: { node_id: string; reason: string } | null;
  current_nodes?: string[];
  on_call?: { seat: { kind: string }; live: boolean } | null;
  rulings?: Array<{ node_id: string; question: string; status: string; answered_by?: string | null }>;
  node_attempts?: Array<{ node_id: string; iteration: number; attempt_no: number }>;
}
/** `GetWorkflowExecution` → `{lookup}` (tagged by `status`). */
export type ExecutionLookup =
  | { status: 'found'; execution: ExecutionSnapshot }
  | { status: 'expired'; execution_id: string }
  | { status: 'not_found'; execution_id: string };

/** Validation report (rsi_common WorkflowValidationReport). */
export interface ValidationReport {
  executable: boolean;
  diagnostics: Array<{ code: string; severity: string; subject: { path: string; node_id?: string | null }; message: string }>;
}

// ---------- the open draft and the overlay state ----------

export type Origin = { kind: 'authored' } | { kind: 'recursive'; graphId: string };
export type Persistence = 'clean' | 'dirty' | 'saving' | 'save-failed';
export interface Draft {
  /** Stable identity for the app run (the TUI's draft_id). */
  key: number;
  workflow: WorkflowDefinition;
  /** The saved `workflows` row, null for an unsaved draft or a bridged view. */
  workflowId: string | null;
  projectId: string | null;
  origin: Origin;
  persistence: Persistence;
  executionId: string | null;
}

export type PickerKind = 'topology' | 'saved' | 'recursive';
export type EditField = 'name' | 'instructions' | 'integration' | 'verification';
export type Mode =
  | { kind: 'navigate' }
  | { kind: 'detail' }
  | { kind: 'edit'; field: EditField }
  | { kind: 'picker'; picker: PickerKind; index: number; back: 'navigate' | 'detail' };

export interface Review {
  draft: Draft;
  selected: number;
  selectedEdge: number;
  mode: Mode;
  /** Camera follows the selection; false after a pan (H/J/K/L) until `c` or a move. */
  follow: boolean;
  pan: Point;
  history: WorkflowDefinition[];
  collapsed: string[];
  buffer: string;
}

export function blankWorkflow(name = 'Untitled Workflow'): WorkflowDefinition {
  return { version: '1.0', name, description: '', nodes: [], edges: [], metadata: {} };
}

let draftKeys = 0;

export function newDraft(workflow: WorkflowDefinition, over: Partial<Omit<Draft, 'key'>> = {}): Draft {
  draftKeys += 1;
  return { key: draftKeys, workflow, workflowId: null, projectId: null, origin: { kind: 'authored' }, persistence: 'clean', executionId: null, ...over };
}

export function openReview(draft: Draft): Review {
  return { draft, selected: 0, selectedEdge: 0, mode: { kind: 'navigate' }, follow: true, pan: { x: 0, y: 0 }, history: [], collapsed: [], buffer: '' };
}

/** A durable execution blocked on preserved work stays active (and interruptible). */
export function isExecutionActive(status: ExecutionStatus | null | undefined): boolean {
  return status === 'accepted' || status === 'running' || status === 'blocked';
}

export const isAuthored = (d: Draft): boolean => d.origin.kind === 'authored';

/** Decode a stored definition; a bad one becomes an empty workflow named after the row (the TUI's fallback). */
export function decodeDefinition(definition: unknown, fallbackName: string): { workflow: WorkflowDefinition; error: string | null } {
  const d = definition as Partial<WorkflowDefinition> | null;
  if (!d || typeof d !== 'object' || !Array.isArray(d.nodes) || !Array.isArray(d.edges) || typeof d.name !== 'string') {
    return { workflow: blankWorkflow(fallbackName), error: 'workflow definition decode failed: not a workflow definition' };
  }
  const bad = d.nodes.find((n) => !n || typeof n.id !== 'string' || typeof n.name !== 'string')
    ?? d.edges.find((e) => !e || typeof e.source !== 'string' || typeof e.target !== 'string');
  if (bad) return { workflow: blankWorkflow(fallbackName), error: 'workflow definition decode failed: malformed node or edge' };
  return {
    workflow: {
      version: typeof d.version === 'string' ? d.version : '1.0',
      name: d.name,
      description: typeof d.description === 'string' ? d.description : '',
      nodes: d.nodes.map((n) => ({ ...n, type: (n.type ?? 'action') as NodeType })),
      edges: d.edges.map((e) => ({ ...e })),
      metadata: d.metadata && typeof d.metadata === 'object' ? d.metadata : {},
    },
    error: null,
  };
}

/** The saved-workflow picker: raw prompt titles (`/…`) dropped, newest first, then by title. */
export function savedPickerEntries(rows: readonly WorkflowRow[]): WorkflowRow[] {
  return rows.filter((w) => !w.title.startsWith('/')).sort((a, b) => (b.updated_at < a.updated_at ? -1 : b.updated_at > a.updated_at ? 1 : a.title < b.title ? -1 : a.title > b.title ? 1 : 0));
}

/** Topology fields of a session row (Session has more; the list keeps them). */
export interface TopologySession { id: string; parent_id?: string | null; workflow_id?: string | null; workflow_id_override?: string | null }

/**
 * The workflow the graph review opens for a session (app/layout.rs
 * effective_topology): its override, its own binding, else the nearest
 * ancestor's, at most 16 levels up (a parent cycle gives none).
 */
export function effectiveTopology(sessions: readonly TopologySession[], id: string): string | null {
  const byId = new Map(sessions.map((s) => [s.id, s]));
  const s = byId.get(id);
  if (!s) return null;
  if (s.workflow_id_override) return s.workflow_id_override;
  if (s.workflow_id) return s.workflow_id;
  let cur = s.parent_id ?? null;
  for (let depth = 1; cur; depth++) {
    if (depth >= 16) return null;
    const a = byId.get(cur);
    if (!a) return null;
    if (a.workflow_id) return a.workflow_id;
    cur = a.parent_id ?? null;
  }
  return null;
}

// ---------- node and edge helpers ----------

/** Edges touching the node, in declaration order (the detail panel's edge list). */
export function nodeEdges(workflow: WorkflowDefinition, index: number): Array<{ edge: EdgeDef; at: number }> {
  const node = workflow.nodes[index];
  if (!node) return [];
  return workflow.edges.map((edge, at) => ({ edge, at })).filter(({ edge }) => edge.source === node.id || edge.target === node.id);
}

/** `[`: the source of the first edge into the node. */
export function predecessor(workflow: WorkflowDefinition, index: number): number | null {
  const id = workflow.nodes[index]?.id;
  if (id === undefined) return null;
  const e = workflow.edges.find((x) => x.target === id);
  const at = e ? workflow.nodes.findIndex((n) => n.id === e.source) : -1;
  return at < 0 ? null : at;
}

/** `]`: the target of the first edge out of the node. */
export function successor(workflow: WorkflowDefinition, index: number): number | null {
  const id = workflow.nodes[index]?.id;
  if (id === undefined) return null;
  const e = workflow.edges.find((x) => x.source === id);
  const at = e ? workflow.nodes.findIndex((n) => n.id === e.target) : -1;
  return at < 0 ? null : at;
}

function metaJson(workflow: WorkflowDefinition, key: string): Record<string, unknown> | null {
  const raw = workflow.metadata?.[key];
  if (typeof raw !== 'string') return null;
  try {
    const v = JSON.parse(raw) as unknown;
    return v && typeof v === 'object' ? (v as Record<string, unknown>) : null;
  } catch {
    return null;
  }
}

/** Whether a node's fields may be edited, and why not (bridged recursive graphs carry per-node locks). */
export function nodeEditable(draft: Draft, index: number): { editable: boolean; reason: string | null } {
  if (draft.origin.kind === 'authored') return { editable: true, reason: null };
  const node = draft.workflow.nodes[index];
  if (!node) return { editable: false, reason: 'no selected node' };
  const raw = draft.workflow.metadata?.recursive_node_locks;
  if (typeof raw !== 'string') return { editable: false, reason: 'lock state unknown' };
  const locks = metaJson(draft.workflow, 'recursive_node_locks');
  if (!locks) return { editable: false, reason: 'lock state unparseable' };
  const lock = locks[node.id] as { locked?: unknown; reason?: unknown } | undefined | null;
  if (lock === undefined || lock === null) return { editable: false, reason: 'lock state missing' };
  const locked = typeof lock.locked === 'boolean' ? lock.locked : true;
  if (locked) return { editable: false, reason: typeof lock.reason === 'string' ? lock.reason : 'locked' };
  return { editable: true, reason: null };
}

/** A bridged node's integration / verification strategies (metadata `recursive_node_strategies`). */
export function nodeStrategies(workflow: WorkflowDefinition, nodeId: string): { integration: string | null; verification: string | null } {
  const all = metaJson(workflow, 'recursive_node_strategies');
  const s = all?.[nodeId] as { integration_strategy?: unknown; verification_strategy?: unknown } | undefined;
  return {
    integration: typeof s?.integration_strategy === 'string' ? s.integration_strategy : null,
    verification: typeof s?.verification_strategy === 'string' ? s.verification_strategy : null,
  };
}

export function fieldValue(workflow: WorkflowDefinition, index: number, field: EditField): string | null {
  const node = workflow.nodes[index];
  if (!node) return null;
  switch (field) {
    case 'name': return node.name;
    case 'instructions': return node.instructions ?? '';
    case 'integration': return nodeStrategies(workflow, node.id).integration ?? '';
    case 'verification': return nodeStrategies(workflow, node.id).verification ?? '';
  }
}

const clone = (w: WorkflowDefinition): WorkflowDefinition => JSON.parse(JSON.stringify(w)) as WorkflowDefinition;

function dirty(d: Draft): Draft {
  return d.persistence === 'save-failed' ? d : { ...d, persistence: 'dirty' };
}

// ---------- edits (authored drafts; undoable) ----------

/** `d`: remove the node and its edges; the selection stays in range. */
export function deleteNode(r: Review): Review {
  const w = r.draft.workflow;
  const node = w.nodes[r.selected];
  if (!node) return r;
  const next = clone(w);
  next.nodes.splice(r.selected, 1);
  next.edges = next.edges.filter((e) => e.source !== node.id && e.target !== node.id);
  const left = next.nodes.length;
  return {
    ...r,
    draft: dirty({ ...r.draft, workflow: next, executionId: null }),
    history: [...r.history, w],
    selectedEdge: 0,
    selected: left === 0 ? 0 : Math.min(r.selected, left - 1),
    mode: left === 0 ? { kind: 'navigate' } : r.mode,
  };
}

/** `x` in detail: remove the selected edge of the node. */
export function deleteEdge(r: Review): Review {
  const hit = nodeEdges(r.draft.workflow, r.selected)[r.selectedEdge];
  if (!hit) return r;
  const next = clone(r.draft.workflow);
  next.edges.splice(hit.at, 1);
  return { ...r, draft: dirty({ ...r.draft, workflow: next }), history: [...r.history, r.draft.workflow], selectedEdge: Math.max(0, r.selectedEdge - 1) };
}

/** `u`: restore the previous workflow. */
export function undo(r: Review): Review {
  const prev = r.history[r.history.length - 1];
  if (!prev) return r;
  const n = prev.nodes.length;
  return {
    ...r,
    draft: dirty({ ...r.draft, workflow: prev, executionId: null }),
    history: r.history.slice(0, -1),
    selected: r.selected >= n ? Math.max(0, n - 1) : r.selected,
    selectedEdge: 0,
    mode: n === 0 ? { kind: 'navigate' } : r.mode,
  };
}

/** `z`: fold or unfold a topology or subgraph node (the glyph changes). */
export function toggleCollapse(r: Review): Review {
  const node = r.draft.workflow.nodes[r.selected];
  if (!node || (node.type !== 'topology' && node.type !== 'subgraph')) return r;
  const has = r.collapsed.includes(node.id);
  return { ...r, collapsed: has ? r.collapsed.filter((id) => id !== node.id) : [...r.collapsed, node.id] };
}

/** Enter in an authored edit: write the field into the draft (undoable). */
export function commitAuthoredEdit(r: Review, value: string): Review {
  if (r.mode.kind !== 'edit') return r;
  const field = r.mode.field;
  const node = r.draft.workflow.nodes[r.selected];
  if (!node) return { ...r, mode: { kind: 'detail' }, buffer: '' };
  if (field !== 'name' && field !== 'instructions') return { ...r, mode: { kind: 'detail' }, buffer: '' };
  const next = clone(r.draft.workflow);
  const target = next.nodes[r.selected]!;
  if (field === 'name') target.name = value;
  else target.instructions = value;
  return { ...r, draft: dirty({ ...r.draft, workflow: next }), history: [...r.history, r.draft.workflow], mode: { kind: 'detail' }, buffer: '' };
}

/** A fresh bridged definition after a recursive edit; the selection follows the node id. */
export function replaceBridged(r: Review, workflow: WorkflowDefinition): Review {
  const id = r.draft.workflow.nodes[r.selected]?.id;
  const at = id === undefined ? -1 : workflow.nodes.findIndex((n) => n.id === id);
  const selected = Math.min(at < 0 ? r.selected : at, Math.max(0, workflow.nodes.length - 1));
  return { ...r, draft: { ...r.draft, workflow }, history: [], selected, mode: { kind: 'detail' }, buffer: '' };
}

/** A starter template replaces an empty unsaved draft in place (undoable), else opens as a new draft. */
export function applyTemplate(r: Review, workflow: WorkflowDefinition, currentProject: string | null): { review: Review; replaced: boolean } {
  const d = r.draft;
  const reuse = d.workflowId === null && d.origin.kind === 'authored' && d.workflow.nodes.length === 0 && d.workflow.edges.length === 0;
  if (reuse) {
    return {
      review: { ...openReview(dirty({ ...d, workflow, executionId: null })), history: [...r.history, d.workflow] },
      replaced: true,
    };
  }
  return { review: openReview(dirty(newDraft(workflow, { projectId: d.projectId ?? currentProject }))), replaced: false };
}

// ---------- keys ----------

export interface GraphKey { key: string; ctrl?: boolean; shift?: boolean }

export type GraphEffect =
  | { kind: 'none' }
  | { kind: 'close' }
  | { kind: 'open-picker'; picker: PickerKind }
  | { kind: 'choose'; picker: PickerKind; index: number }
  | { kind: 'run' }
  | { kind: 'interrupt' }
  | { kind: 'begin-edit'; field: EditField }
  | { kind: 'refused'; why: string }
  | { kind: 'focus-dashboard' };

export interface KeyContext {
  layout: GraphLayout;
  /** The draft's latest execution is active (accepted / running / blocked). */
  executing: boolean;
  /** Entries in the open picker. */
  pickerCount: number;
}

const PICKER_PAGE = 10;
const out = (review: Review, effect: GraphEffect = { kind: 'none' }) => ({ review, effect, handled: true });
const ignored = (review: Review) => ({ review, effect: { kind: 'none' } as GraphEffect, handled: false });

function select(r: Review, target: number | null): Review {
  const n = r.draft.workflow.nodes.length;
  if (target === null || n === 0 || target === r.selected) return r;
  return { ...r, selected: Math.max(0, Math.min(target, n - 1)), selectedEdge: 0, follow: true, pan: { x: 0, y: 0 } };
}

function panBy(r: Review, dx: number, dy: number): Review {
  return { ...r, follow: false, pan: { x: r.pan.x + dx, y: r.pan.y + dy } };
}

const PAN: Record<string, [number, number]> = { H: [-HORIZONTAL_PAN_STEP, 0], J: [0, VERTICAL_PAN_STEP], K: [0, -VERTICAL_PAN_STEP], L: [HORIZONTAL_PAN_STEP, 0] };
const MOVE: Record<string, Direction> = { h: 'left', ArrowLeft: 'left', j: 'down', ArrowDown: 'down', k: 'up', ArrowUp: 'up', l: 'right', ArrowRight: 'right' };

function camera(r: Review, k: string): Review | null {
  const p = PAN[k];
  if (p) return panBy(r, p[0], p[1]);
  if (k === 'c') return { ...r, follow: true, pan: { x: 0, y: 0 } };
  return null;
}

function openPicker(r: Review, picker: PickerKind) {
  const back = r.mode.kind === 'detail' ? 'detail' : 'navigate';
  return out({ ...r, mode: { kind: 'picker', picker, index: 0, back }, buffer: '' }, { kind: 'open-picker', picker });
}

/**
 * One key in the graph review (overlay/graph.rs handle_*_key). `handled`
 * false lets the key reach the app keys (`<Space>N`, `<Space>;`, `:`).
 * Edit-field keys are the text box's own; the DOM calls commitAuthoredEdit or
 * the bridged edit on Enter and `{...r, mode: detail}` on Esc.
 */
export function graphKey(r: Review, k: GraphKey, ctx: KeyContext): { review: Review; effect: GraphEffect; handled: boolean } {
  const key = k.key;
  const mode = r.mode;
  if (mode.kind === 'picker') {
    const n = ctx.pickerCount;
    const at = (i: number) => out({ ...r, mode: { ...mode, index: Math.max(0, Math.min(i, Math.max(0, n - 1))) } });
    if (key === 'Escape') return out({ ...r, mode: { kind: mode.back } });
    if (key === 'j' || key === 'ArrowDown') return at(mode.index + 1);
    if (key === 'k' || key === 'ArrowUp') return at(mode.index - 1);
    if (key === 'g') return at(0);
    if (key === 'G') return n > 0 ? at(n - 1) : out(r);
    if (key === 'PageDown' || (k.ctrl && key === 'd')) return at(mode.index + PICKER_PAGE);
    if (key === 'PageUp' || (k.ctrl && key === 'u')) return at(mode.index - PICKER_PAGE);
    if (key === 'Enter') return out(r, { kind: 'choose', picker: mode.picker, index: mode.index });
    return out(r);
  }
  if (mode.kind === 'edit') return ignored(r);
  if (k.ctrl) return ignored(r);

  const authored = isAuthored(r.draft);
  const readOnly = ctx.executing || !authored;
  const count = r.draft.workflow.nodes.length;
  const cam = camera(r, key);
  if (cam) return out(cam);

  if (count === 0) {
    if (key === 't' && !ctx.executing) return openPicker(r, 'topology');
    if (key === 'o' && !ctx.executing) return openPicker(r, 'saved');
    if (key === 'q') return out(r, { kind: 'close' });
    return ignored(r);
  }

  const common = (): { review: Review; effect: GraphEffect; handled: boolean } | null => {
    if (key === '[') return out(select(r, predecessor(r.draft.workflow, r.selected)));
    if (key === ']') return out(select(r, successor(r.draft.workflow, r.selected)));
    if (key === 't') return readOnly ? out(r, { kind: 'refused', why: ctx.executing ? 'A run is active' : 'This graph is read-only' }) : openPicker(r, 'topology');
    if (key === 'o') return readOnly ? out(r, { kind: 'refused', why: ctx.executing ? 'A run is active' : 'This graph is read-only' }) : openPicker(r, 'saved');
    if (key === 'R') return out(r, { kind: 'open-picker', picker: 'recursive' });
    if (key === 'r') {
      if (!authored) return out(r, { kind: 'refused', why: 'A bridged recursive graph is read-only; it cannot run from here' });
      if (ctx.executing) return out(r, { kind: 'refused', why: 'A run is already active (x interrupts it)' });
      return out(r, { kind: 'run' });
    }
    if (key === 'q') return out(r, { kind: 'close' });
    return null;
  };

  if (mode.kind === 'detail') {
    const edges = nodeEdges(r.draft.workflow, r.selected).length;
    if (key === 'Escape') return out({ ...r, mode: { kind: 'navigate' }, selectedEdge: 0 });
    if (key === 'j' || key === 'ArrowDown') return out(edges === 0 ? r : { ...r, selectedEdge: Math.min(r.selectedEdge + 1, edges - 1) });
    if (key === 'k' || key === 'ArrowUp') return out(edges === 0 ? r : { ...r, selectedEdge: Math.max(0, r.selectedEdge - 1) });
    if (key === 'x' && ctx.executing) return out(r, { kind: 'interrupt' });
    if (key === 'x') return readOnly ? out(r, { kind: 'refused', why: 'This graph is read-only' }) : out(deleteEdge(r));
    if (!ctx.executing && (key === 'i' || key === 'I' || key === 's' || key === 'S')) {
      const field: EditField = key === 'i' ? 'name' : key === 'I' ? 'instructions' : key === 's' ? 'integration' : 'verification';
      if (field === 'name' && !authored) return out(r, { kind: 'refused', why: 'A recursive node’s name is not editable here' });
      if ((field === 'integration' || field === 'verification') && authored) return out(r, { kind: 'refused', why: 'Strategies belong to bridged recursive nodes' });
      const ed = nodeEditable(r.draft, r.selected);
      if (!ed.editable) return out(r, { kind: 'refused', why: `Locked: ${ed.reason ?? 'not editable'}` });
      const value = fieldValue(r.draft.workflow, r.selected, field);
      if (value === null) return out(r);
      return out({ ...r, mode: { kind: 'edit', field }, buffer: value }, { kind: 'begin-edit', field });
    }
    return common() ?? ignored(r);
  }

  // navigate
  const dir = MOVE[key];
  if (dir) {
    const next = directionalNeighbor(ctx.layout, r.selected, dir);
    if (next === null && dir === 'right') return out(r, { kind: 'focus-dashboard' });
    return out(select(r, next));
  }
  if (key === 'Tab') return out(r, { kind: 'focus-dashboard' });
  if (key === 'g') return out(select(r, 0));
  if (key === 'G') return out(select(r, count - 1));
  if (key === 'Enter') return out({ ...r, mode: { kind: 'detail' } });
  if (key === 'd') return readOnly ? out(r, { kind: 'refused', why: ctx.executing ? 'A run is active' : 'This graph is read-only' }) : out(deleteNode(r));
  if (key === 'u') return readOnly ? out(r, { kind: 'refused', why: ctx.executing ? 'A run is active' : 'This graph is read-only' }) : out(undo(r));
  if (key === 'z') return readOnly ? out(r) : out(toggleCollapse(r));
  if (key === 'x' && ctx.executing) return out(r, { kind: 'interrupt' });
  return common() ?? ignored(r);
}

// ---------- requests ----------

/** The `workflows` row + definition the TUI upserts before a run (build_workflow_document). */
export function workflowDocument(d: Draft, existing: WorkflowRow | null, now: string, newId: string, currentProject: string | null): WorkflowDocument {
  return {
    workflow: {
      id: existing?.id ?? d.workflowId ?? newId,
      title: d.workflow.name,
      stage: existing?.stage ?? 'Planning',
      artifact_path: existing?.artifact_path ?? null,
      project_id: d.projectId ?? existing?.project_id ?? currentProject,
      created_at: existing?.created_at ?? now,
      updated_at: now,
    },
    definition: d.workflow,
  };
}

/** ExecuteWorkflow params for `r` (the TUI's: no input, not a dry run, no working dir or parent). */
export function executeParams(workflowId: string, workflow: WorkflowDefinition, projectId: string | null): Record<string, unknown> {
  return { workflow_id: workflowId, workflow, input: null, dry_run: false, project_id: projectId, working_dir: null, parent_id: null };
}

/**
 * After ExecuteWorkflow succeeds, the TUI links the selected session to the
 * workflow (overlay/graph.rs run_graph_draft): when the session is known and
 * its own `workflow_id` differs, it sets it locally at once and persists with
 * UpdateSessionWorkflow. Returns the session to bind, else null (no selection,
 * unknown session, or already linked: no daemon call). The override is not
 * compared, as in the TUI.
 */
export function sessionToBind(sessions: readonly TopologySession[], selectedId: string | null, workflowId: string): string | null {
  if (!selectedId) return null;
  const s = sessions.find((x) => x.id === selectedId);
  if (!s || s.workflow_id === workflowId) return null;
  return s.id;
}

/**
 * The TUI persists the link fire-and-forget (`let _ =`): a daemon refusal is
 * dropped and the local link stays. The desktop keeps that behaviour but says so,
 * since a toast is cheaper than a silently unlinked session.
 */
export function bindFailureMessage(error: unknown): string {
  return `workflow ran, but linking the session failed: ${String(error)}`;
}

/** The toast for a validation report that blocks a run (notify_validation_blockers). */
export function blockerMessage(report: ValidationReport): string | null {
  const blocking = report.diagnostics.filter((d) => d.severity === 'error');
  if (blocking.length === 0) return null;
  return `Execution blocked (${blocking.length} issues): ${blocking.slice(0, 3).map((d) => d.message).join('; ')}`;
}

export function blockingNodeIds(report: ValidationReport | null): Set<string> {
  const out = new Set<string>();
  for (const d of report?.diagnostics ?? []) if (d.severity === 'error' && d.subject.node_id) out.add(d.subject.node_id);
  return out;
}

// ---------- the run view and node badges ----------

/** Latest node state per node id from the update stream. */
export function executionStates(ex: ExecutionSnapshot | null): Map<string, NodeState> {
  const m = new Map<string, NodeState>();
  for (const u of ex?.updates ?? []) if (u.node_id && u.node_state) m.set(u.node_id, u.node_state);
  return m;
}

export function nodeBadge(invalid: boolean, state: NodeState | undefined): string {
  if (invalid) return '!';
  switch (state) {
    case undefined: return '·';
    case 'running': return '▶';
    case 'succeeded': return '✓';
    case 'failed': return '✕';
    case 'skipped': return '-';
    default: return '?';
  }
}

export function nodeTypeGlyph(type: NodeType, collapsed: boolean): string {
  if (type === 'topology') return collapsed ? '▶' : '▼';
  if (type === 'subgraph') return collapsed ? '▸' : '▾';
  return '◆';
}

export function truncateLabel(label: string, max: number): string {
  const chars = [...label];
  if (max <= 0) return '';
  if (chars.length <= max) return label;
  if (max === 1) return '…';
  return `${chars.slice(0, max - 1).join('')}…`;
}

/** Durable topology runs get the run view (a row version and not a dry run). */
export const isTopologyRun = (ex: ExecutionSnapshot): boolean => ex.row_version != null && !ex.dry_run;

const STATE_WORD: Record<string, string> = { running: 'running', succeeded: 'succeeded', failed: 'failed', skipped: 'skipped' };
const EXEC_WORD = new Set(['accepted', 'running', 'succeeded', 'failed', 'interrupted', 'blocked']);
export type RunLineKind = 'title' | 'node' | 'current' | 'warn' | 'hint';

/** The run view (ui/overlay/graph.rs run_view_text): node rows, on-call, waiting, rulings. */
export function runViewText(workflow: WorkflowDefinition, ex: ExecutionSnapshot, compact: boolean): Array<[string, RunLineKind]> {
  const title: Array<[string, RunLineKind]> = [[`${ex.workflow_name} ${EXEC_WORD.has(ex.status) ? ex.status : 'unknown'}`, 'title']];
  const lines: Array<[string, RunLineKind]> = [];
  for (const node of workflow.nodes) {
    const state = [...(ex.updates ?? [])].reverse().find((u) => u.node_id === node.id && u.node_state)?.node_state ?? null;
    const at = (ex.node_attempts ?? []).find((a) => a.node_id === node.id);
    const attempt = at?.attempt_no ?? 0;
    const round = at ? at.iteration + 1 : 0;
    const current = (ex.current_nodes ?? []).includes(node.id);
    const word = state === null ? 'pending' : STATE_WORD[state] ?? 'unknown';
    const detail = attempt === 0 && round === 0 ? '' : `  attempt ${Math.max(1, attempt)} round ${round}`;
    lines.push([`${current ? '>' : ' '} ${node.id}  ${word}${detail}`, current ? 'current' : 'node']);
  }
  const onCall: Array<[string, RunLineKind]> = [];
  if (ex.on_call) {
    const seat = ex.on_call.seat.kind === 'project_manager' ? 'project manager' : 'portfolio seat';
    onCall.push([`on-call: ${seat} (${ex.on_call.live ? 'live' : 'not live'})`, ex.on_call.live ? 'node' : 'warn']);
  } else {
    onCall.push(['on-call: none', 'hint']);
  }
  if (ex.waiting) onCall.push([`waiting: ${ex.waiting.node_id} - ${ex.waiting.reason}`, 'warn']);
  const rulings: Array<[string, RunLineKind]> = [[`rulings (${(ex.rulings ?? []).length})`, 'title']];
  if ((ex.rulings ?? []).length === 0) rulings.push(['  none', 'hint']);
  for (const r of ex.rulings ?? []) rulings.push([`  ${r.node_id} [${r.status}${r.answered_by ? ` by ${r.answered_by}` : ''}] ${r.question}`, 'node']);
  return compact ? [...title, ...onCall, ...lines, ...rulings] : [...title, ...lines, ...onCall, ...rulings];
}

/** A short status badge for the title (ui/overlay/graph.rs render_execution_badge). */
export function executionBadge(ex: ExecutionSnapshot | null): string {
  if (!ex) return '';
  return `${ex.dry_run ? 'dry-run ' : ''}${EXEC_WORD.has(ex.status) ? ex.status : 'unknown'}`;
}

/** The ack the TUI records the moment ExecuteWorkflow accepts a run. */
export function ackSnapshot(ack: { execution_id: string; workflow_id: string; dry_run?: boolean }, name: string): ExecutionSnapshot {
  return { execution_id: ack.execution_id, workflow_id: ack.workflow_id, workflow_name: name, status: 'accepted', dry_run: !!ack.dry_run, updates: [] };
}

// ---------- `:topology-resolve` (#634) ----------

export type ResolveAction = 'inspect' | 'accept' | 'retry' | 'discard';
export interface ResolveCommand { executionId: string | null; action: ResolveAction; confirm: string | null }

const UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

/** `:topology-resolve [<execution_id>] inspect|accept|retry|discard [<commit>]` (overlay/graph.rs parse_topology_resolve). */
export function parseTopologyResolve(args: string): ResolveCommand | { error: string } {
  const t = args.trim().split(/\s+/).filter(Boolean);
  let executionId: string | null = null;
  if (t[0] && UUID_RE.test(t[0])) executionId = t.shift()!.toLowerCase();
  const a = t.shift();
  if (a !== 'inspect' && a !== 'accept' && a !== 'retry' && a !== 'discard') {
    return { error: 'usage: :topology-resolve [<execution_id>] inspect|accept|retry|discard [<commit>]' };
  }
  const confirm = t.shift() ?? null;
  if (t.length > 0) return { error: 'too many arguments for :topology-resolve' };
  return { executionId, action: a, confirm };
}

/** ResolveTopologyAttempt params; the key binds the CAS version so a repeat replays. */
export function resolveParams(executionId: string, attemptId: string, action: ResolveAction, rowVersion: number, confirm: string | null): Record<string, unknown> {
  return {
    execution_id: executionId,
    attempt_id: attemptId,
    action,
    expected_row_version: rowVersion,
    idempotency_key: `tui:${executionId}:${attemptId}:${action}:${rowVersion}`,
    ...(confirm ? { confirm_preserved_commit: confirm } : {}),
  };
}
