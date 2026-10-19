// Graph review pane (DOM side of graph-model.ts; slice 11, hub #3013): the
// TUI's `<Space>v` / `:graph` overlay. The graph is drawn as SVG from the
// ported layout (graph-layout.ts); the side panel shows the node detail, the
// open picker or the run view. Daemon text is set with textContent only.
import { dispatch } from './commands.js';
import { $, el, toast } from './dom.js';
import { type GraphLayout, type Point, layoutWorkflowGraph, right, viewportOrigin } from './graph-layout.js';
import {
  type Draft, type EditField, type ExecutionSnapshot, type PickerKind, type Review, type ValidationReport,
  type WorkflowDefinition, type WorkflowRow, ackSnapshot, applyTemplate, blockerMessage, blockingNodeIds,
  commitAuthoredEdit, decodeDefinition, executionBadge, executionStates, fieldValue, graphKey, isAuthored,
  bindFailureMessage, isExecutionActive, isTopologyRun, newDraft, sessionToBind, nodeBadge, nodeEdges, nodeEditable, nodeStrategies, nodeTypeGlyph,
  openReview, replaceBridged, runViewText, savedPickerEntries, truncateLabel, effectiveTopology, workflowDocument,
} from './graph-model.js';
import { type StarterTemplate, graphsApi } from './graphs-api.js';
import { type Row, idOf, str } from './dag-model.js';
import { state, uiChanged } from './state.js';
import { focusOwnsKey, paneKeyAllowed } from './modal-gate.js';
import { isVimKeysOn } from './vimkeys.js';

const CW = 8;
const CH = 18;
const POLL_MS = 1500;
const SVG = 'http://www.w3.org/2000/svg';

interface Pane {
  open: boolean;
  review: Review | null;
  layout: GraphLayout | null;
  /** Drafts kept for the app run, so reopening resumes (the TUI's graph_drafts). */
  drafts: Draft[];
  executions: Map<string, ExecutionSnapshot>;
  workflows: WorkflowRow[];
  starters: StarterTemplate[];
  starterPreview: Map<string, WorkflowDefinition>;
  recursive: Row[];
  validation: ValidationReport | null;
  busy: string | null;
  pollTimer: number | undefined;
  dashboard: boolean;
}

const pane: Pane = {
  open: false, review: null, layout: null, drafts: [], executions: new Map(), workflows: [], starters: [],
  starterPreview: new Map(), recursive: [], validation: null, busy: null, pollTimer: undefined, dashboard: false,
};

let onFocusDashboard: ((graphId: string) => void) | null = null;
let onOpen: (() => void) | null = null;

/** actions/graphs-run.ts wires the DAG browser hand-off and the pane exclusivity. */
export function configureGraphReview(opts: { focusDashboard: (graphId: string) => void; opening: () => void }): void {
  onFocusDashboard = opts.focusDashboard;
  onOpen = opts.opening;
}

export const isGraphOpen = (): boolean => pane.open;

function view(): HTMLElement {
  return $('graph-view');
}

function execution(): ExecutionSnapshot | null {
  const id = pane.review?.draft.executionId;
  return id ? pane.executions.get(id) ?? null : null;
}

export function graphSnapshot(): { authored: boolean; executing: boolean; hasNodes: boolean; canUndo: boolean } {
  const r = pane.review;
  return {
    authored: !!r && isAuthored(r.draft),
    executing: isExecutionActive(execution()?.status),
    hasNodes: (r?.draft.workflow.nodes.length ?? 0) > 0,
    canUndo: (r?.history.length ?? 0) > 0,
  };
}

function setReview(r: Review): void {
  pane.review = r;
  // Keep the app-run draft list in step, by draft key.
  const at = pane.drafts.findIndex((d) => d.key === r.draft.key);
  if (at >= 0) pane.drafts[at] = r.draft;
  else pane.drafts.push(r.draft);
  pane.layout = layoutWorkflowGraph(r.draft.workflow);
}

function currentProject(): string | null {
  const open = state.sessions.find((s) => s.id === (state.cursorId ?? state.selectedId));
  return open?.project_id ?? state.projects[0]?.id ?? null;
}

// ---------- open / close ----------

async function loadSavedDraft(workflowId: string, fallbackProject: string | null): Promise<Draft> {
  try {
    const { document } = await graphsApi.getWorkflow(workflowId);
    const row = document.workflow;
    const { workflow, error } = decodeDefinition(document.definition, row.title);
    if (error) toast(error);
    return newDraft(workflow, { workflowId: row.id, projectId: row.project_id ?? fallbackProject });
  } catch (e) {
    const row = pane.workflows.find((w) => w.id === workflowId);
    toast(`workflow load failed: ${String(e)}`);
    return newDraft(decodeDefinition(null, row?.title ?? 'Untitled Workflow').workflow, { workflowId, projectId: row?.project_id ?? fallbackProject });
  }
}

/** `<Space>v` / `:graph`: resume the active draft, else the focused session's topology, else a blank draft. */
export async function openGraphReview(): Promise<void> {
  if (pane.open) { closeGraphReview(); return; }
  onOpen?.();
  let draft = pane.review?.draft ?? null;
  if (!draft) {
    const sid = state.cursorId ?? state.selectedId;
    const wf = sid ? effectiveTopology(state.sessions, sid) : null;
    const known = wf ? pane.drafts.find((d) => d.workflowId === wf) : undefined;
    draft = known ?? (wf ? await loadSavedDraft(wf, currentProject()) : newDraft(decodeDefinition(null, 'Untitled Workflow').workflow, { projectId: currentProject() }));
  }
  show(openReview(draft));
  void resyncExecution(true);
}

function show(r: Review): void {
  setReview(r);
  pane.open = true;
  pane.dashboard = false;
  document.querySelector<HTMLElement>('.layout')!.hidden = true;
  view().hidden = false;
  draw();
  view().focus({ preventScroll: true });
  schedulePoll();
  void validateDraft();
  uiChanged();
}

export function closeGraphReview(): void {
  if (!pane.open) return;
  pane.open = false;
  window.clearTimeout(pane.pollTimer);
  view().hidden = true;
  document.querySelector<HTMLElement>('.layout')!.hidden = false;
  uiChanged();
}

// ---------- executions ----------

async function resyncExecution(notify: boolean): Promise<void> {
  const r = pane.review;
  const id = r?.draft.executionId;
  if (!r || !id) return;
  try {
    const { lookup } = await graphsApi.getExecution(id);
    if (lookup.status === 'found') {
      pane.executions.set(id, lookup.execution);
    } else {
      pane.executions.delete(id);
      if (pane.review?.draft.executionId === id) setReview({ ...pane.review, draft: { ...pane.review.draft, executionId: null } });
      if (notify) toast(lookup.status === 'expired' ? `workflow execution expired: ${id}` : `workflow execution not found: ${id}`);
    }
  } catch (e) {
    if (notify) toast(`workflow execution resync failed: ${String(e)}`);
  }
  draw();
  uiChanged();
}

function schedulePoll(): void {
  window.clearTimeout(pane.pollTimer);
  if (!pane.open) return;
  pane.pollTimer = window.setTimeout(() => {
    void (async () => {
      if (pane.open && isExecutionActive(execution()?.status)) await resyncExecution(false);
      schedulePoll();
    })();
  }, POLL_MS);
}

async function validateDraft(): Promise<void> {
  const r = pane.review;
  if (!r || r.draft.workflow.nodes.length === 0 || !isAuthored(r.draft)) { pane.validation = null; return; }
  try {
    pane.validation = await graphsApi.validate(r.draft.workflow);
  } catch {
    pane.validation = null;
  }
  draw();
}

/** `r`: save the draft when needed, check it, then ExecuteWorkflow (overlay/graph.rs run_graph_draft). */
export async function runDraft(): Promise<void> {
  const r = pane.review;
  if (!r || !isAuthored(r.draft) || pane.busy) return;
  pane.busy = 'Saving…';
  draw();
  try {
    let draft = r.draft;
    if (!draft.workflowId || draft.persistence !== 'clean') {
      const existing = pane.workflows.find((w) => w.id === draft.workflowId) ?? null;
      const doc = workflowDocument(draft, existing, new Date().toISOString(), crypto.randomUUID(), currentProject());
      try {
        const saved = (await graphsApi.upsertWorkflow(doc)).document;
        const { workflow, error } = decodeDefinition(saved.definition, saved.workflow.title);
        if (error) toast(`saved workflow reload failed: ${error}`);
        draft = { ...draft, workflowId: saved.workflow.id, projectId: saved.workflow.project_id ?? null, persistence: 'clean', workflow: error ? { ...draft.workflow, name: saved.workflow.title } : workflow };
        pane.workflows = [saved.workflow, ...pane.workflows.filter((w) => w.id !== saved.workflow.id)];
        setReview({ ...pane.review!, draft });
      } catch (e) {
        setReview({ ...pane.review!, draft: { ...draft, persistence: 'save-failed' } });
        toast(`workflow save failed: ${String(e)}`);
        return;
      }
    }
    pane.busy = 'Checking…';
    draw();
    const report = await graphsApi.validate(draft.workflow);
    pane.validation = report;
    const blocked = blockerMessage(report);
    if (blocked) { toast(blocked); return; }
    pane.busy = 'Starting…';
    draw();
    const ack = await graphsApi.executeWorkflow(draft.workflowId!, draft.workflow, draft.projectId);
    pane.executions.set(ack.execution_id, ackSnapshot(ack, draft.workflow.name));
    setReview({ ...pane.review!, draft: { ...pane.review!.draft, executionId: ack.execution_id } });
    toast(`Workflow started (exec ${ack.execution_id.slice(0, 8)})`, true);
    await bindSelectedSession(draft.workflowId!);
    void resyncExecution(false);
  } catch (e) {
    toast(`workflow execution failed: ${String(e)}`);
  } finally {
    pane.busy = null;
    draw();
    uiChanged();
  }
}

/**
 * Link the selected session to the workflow it just ran (overlay/graph.rs): set it
 * locally at once, then persist with UpdateSessionWorkflow. A refusal leaves the local
 * link, as the TUI's fire-and-forget does, and is shown as a toast.
 */
async function bindSelectedSession(workflowId: string): Promise<void> {
  const sid = sessionToBind(state.sessions, state.cursorId ?? state.selectedId, workflowId);
  if (!sid) return;
  const row = state.sessions.find((s) => s.id === sid);
  if (row) row.workflow_id = workflowId;
  try {
    await graphsApi.bindSessionWorkflow(sid, workflowId);
  } catch (e) {
    toast(bindFailureMessage(e));
  }
}

export async function interruptRun(): Promise<void> {
  const id = pane.review?.draft.executionId;
  if (!id) { toast('No active workflow execution'); return; }
  if (!isExecutionActive(execution()?.status)) { toast('Workflow execution is not active'); return; }
  try {
    const res = await graphsApi.interruptExecution(id);
    toast(`workflow interrupt requested: ${res.execution_id} (${res.status})`, true);
    void resyncExecution(false);
  } catch (e) {
    toast(`workflow interrupt failed: ${String(e)}`);
  }
}

/** Open the run view of an execution on its workflow (`:topology run`, #1753). */
export async function openRunView(executionId: string, workflowId: string, projectId: string | null): Promise<void> {
  onOpen?.();
  const draft = await loadSavedDraft(workflowId, projectId);
  show(openReview({ ...draft, executionId }));
  await resyncExecution(false);
}

/** Every execution the pane knows about (for `:topology-resolve` without an id). */
export function knownExecutions(): ExecutionSnapshot[] {
  return [...pane.executions.values()];
}

export function rememberExecution(ex: ExecutionSnapshot): void {
  pane.executions.set(ex.execution_id, ex);
}

// ---------- pickers ----------

function pickerCount(kind: PickerKind): number {
  if (kind === 'topology') return pane.starters.length;
  if (kind === 'saved') return savedPickerEntries(pane.workflows).length;
  return pane.recursive.length;
}

async function loadPicker(kind: PickerKind): Promise<boolean> {
  try {
    if (kind === 'topology' && pane.starters.length === 0) pane.starters = await graphsApi.starters();
    if (kind === 'saved') pane.workflows = await graphsApi.listWorkflows(pane.review?.draft.projectId ?? null);
    if (kind === 'recursive') {
      const caps = await graphsApi.capabilities();
      // The source never opens while the daemon capability is off (the key is inert, as in the TUI).
      if (caps.gv_render_recursive_origin !== true) { toast('Recursive graphs need the gv_render_recursive_origin daemon capability'); return false; }
      pane.recursive = await graphsApi.listRecursive(null, false);
    }
    return true;
  } catch (e) {
    toast(kind === 'recursive' ? `failed to list recursive graphs: ${String(e)}` : `picker unavailable: ${String(e)}`);
    return kind !== 'recursive';
  }
}

async function choose(kind: PickerKind, index: number): Promise<void> {
  const r = pane.review;
  if (!r) return;
  if (kind === 'topology') {
    const t = pane.starters[index];
    if (!t) { toast('No topology templates available'); return; }
    let wf: WorkflowDefinition;
    try { wf = await graphsApi.starterWorkflow(t.name); } catch (e) { toast(String(e)); return; }
    const { review } = applyTemplate(r, wf, currentProject());
    show(review);
    return;
  }
  if (kind === 'saved') {
    const w = savedPickerEntries(pane.workflows)[index];
    if (!w) { toast('No saved workflows available'); return; }
    const draft = pane.drafts.find((d) => d.workflowId === w.id) ?? await loadSavedDraft(w.id, w.project_id ?? null);
    show(openReview(draft));
    void resyncExecution(true);
    return;
  }
  const g = pane.recursive[index];
  const gid = idOf(g);
  if (!gid) { toast('No recursive graphs available'); return; }
  try {
    const { definition } = await graphsApi.recursiveAsWorkflow(gid);
    const { workflow, error } = decodeDefinition(definition, str(g, 'title') ?? 'recursive graph');
    if (error) { toast(`recursive bridge decode failed: ${error}`); return; }
    show(openReview(newDraft(workflow, { origin: { kind: 'recursive', graphId: gid } })));
  } catch (e) {
    toast(`failed to bridge recursive graph: ${String(e)}`);
  }
}

async function previewStarter(name: string): Promise<void> {
  if (pane.starterPreview.has(name)) return;
  try {
    pane.starterPreview.set(name, await graphsApi.starterWorkflow(name));
    draw();
  } catch { /* the list still shows the description */ }
}

// ---------- edits ----------

async function commitEdit(value: string): Promise<void> {
  const r = pane.review;
  if (!r || r.mode.kind !== 'edit') return;
  if (isAuthored(r.draft)) {
    setReview(commitAuthoredEdit(r, value));
    draw();
    void validateDraft();
    return;
  }
  if (r.draft.origin.kind !== 'recursive') return;
  const graphId = r.draft.origin.graphId;
  const node = r.draft.workflow.nodes[r.selected];
  if (!node) return;
  const field = r.mode.field;
  try {
    if (field === 'instructions') await graphsApi.editRecursiveInstructions(graphId, node.id, value);
    else if (field === 'integration' || field === 'verification') {
      const cur = nodeStrategies(r.draft.workflow, node.id);
      const integration = field === 'integration' ? value : cur.integration;
      const verification = field === 'verification' ? value : cur.verification;
      await graphsApi.editRecursiveSettings(graphId, node.id, integration, verification);
    } else { toast('Unsupported field edit for bridged recursive node'); return; }
    const { definition } = await graphsApi.recursiveAsWorkflow(graphId);
    const { workflow, error } = decodeDefinition(definition, r.draft.workflow.name);
    if (error) { toast(`Failed to refresh draft: ${error}`); return; }
    setReview(replaceBridged(pane.review!, workflow));
    toast('Saved', true);
  } catch (e) {
    toast(`Edit failed: ${String(e)}`);
  }
  draw();
}

// ---------- keys ----------

function viewportUnits(): { w: number; h: number } {
  const c = view().querySelector<HTMLElement>('.graph-canvas');
  return { w: Math.max(8, Math.floor((c?.clientWidth ?? 800) / CW)), h: Math.max(4, Math.floor((c?.clientHeight ?? 500) / CH)) };
}

function handleKey(key: string, ctrl: boolean, shift: boolean): boolean {
  const r = pane.review;
  if (!r || !pane.layout) return false;
  const executing = isExecutionActive(execution()?.status);
  const kind = r.mode.kind === 'picker' ? r.mode.picker : null;
  const res = graphKey(r, { key, ctrl, shift }, { layout: pane.layout, executing, pickerCount: kind ? pickerCount(kind) : 0 });
  if (!res.handled) return false;
  const editsBefore = r.history.length;
  setReview(res.review);
  const e = res.effect;
  switch (e.kind) {
    case 'close': closeGraphReview(); return true;
    case 'open-picker':
      void loadPicker(e.picker).then((ok) => {
        if (!ok || !pane.review) { if (pane.review?.mode.kind === 'picker') setReview({ ...pane.review, mode: { kind: 'navigate' } }); draw(); return; }
        if (e.picker === 'recursive') setReview({ ...pane.review, mode: { kind: 'picker', picker: 'recursive', index: 0, back: pane.review.mode.kind === 'detail' ? 'detail' : 'navigate' } });
        draw();
      });
      break;
    case 'choose': void choose(e.picker, e.index); break;
    case 'run': void runDraft(); break;
    case 'interrupt': void interruptRun(); break;
    case 'refused': toast(e.why); break;
    case 'focus-dashboard':
      if (r.draft.origin.kind === 'recursive') {
        const gid = r.draft.origin.graphId;
        void graphsApi.capabilities().then((c) => {
          if (c.gv_info_dashboard === true) onFocusDashboard?.(gid);
          else toast('The info dashboard needs the gv_info_dashboard daemon capability');
        }).catch((err) => toast(String(err)));
      }
      break;
    default: break;
  }
  if (pane.review && pane.review.history.length !== editsBefore) void validateDraft();
  draw();
  uiChanged();
  return true;
}

/** Palette / menu runs of the in-pane keys. */
export function runGraphKey(key: string): void {
  handleKey(key, false, false);
}

// ---------- rendering ----------

function svg<K extends keyof SVGElementTagNameMap>(tag: K, attrs: Record<string, string | number>): SVGElementTagNameMap[K] {
  const n = document.createElementNS(SVG, tag);
  for (const [k, v] of Object.entries(attrs)) n.setAttribute(k, String(v));
  return n;
}

const TYPE_COLOR: Record<string, string> = { action: 'var(--blue)', topology: 'var(--mauve)', subgraph: 'var(--peach)' };
const STATE_COLOR: Record<string, string> = { running: 'var(--green)', succeeded: 'var(--green)', failed: 'var(--red)' };

function drawCanvas(host: HTMLElement): void {
  const r = pane.review!;
  const layout = pane.layout!;
  host.replaceChildren();
  if (r.draft.workflow.nodes.length === 0) {
    const empty = el('div', 'graph-empty');
    empty.append(el('h3', '', 'Empty graph'), el('p', '', 't  open a starter topology'), el('p', '', 'o  open a saved workflow'), el('p', '', 'q  close'));
    host.append(empty);
    return;
  }
  const { w, h } = viewportUnits();
  const origin: Point = viewportOrigin(layout, w, h, r.selected, r.follow ? null : r.pan);
  const root = svg('svg', { class: 'graph-svg', viewBox: `${origin.x * CW} ${origin.y * CH} ${w * CW} ${h * CH}`, preserveAspectRatio: 'xMinYMin meet', role: 'img' });
  root.setAttribute('aria-label', `Workflow graph ${r.draft.workflow.name}`);
  const defs = svg('defs', {});
  for (const [id, color] of [['gv-arrow', 'var(--overlay0)'], ['gv-arrow-loop', 'var(--yellow)'], ['gv-arrow-bad', 'var(--red)']] as const) {
    const m = svg('marker', { id, viewBox: '0 0 10 10', refX: 9, refY: 5, markerWidth: 7, markerHeight: 7, orient: 'auto-start-reverse' });
    m.append(svg('path', { d: 'M0,0 L10,5 L0,10 z', fill: color }));
    defs.append(m);
  }
  root.append(defs);
  const invalid = blockingNodeIds(pane.validation);
  const states = executionStates(execution());
  const cx = (x: number) => x * CW + CW / 2;
  const cy = (y: number) => y * CH + CH / 2;
  for (const e of layout.edges) {
    const pts = e.segments.length > 0 ? [e.segments[0]!.start, ...e.segments.map((s) => s.end)] : [];
    if (pts.length < 2) continue;
    const srcId = r.draft.workflow.nodes[e.sourceIndex]?.id ?? '';
    const bad = e.kind === 'executable' && (invalid.has(srcId) || invalid.has(r.draft.workflow.nodes[e.targetIndex]?.id ?? ''));
    const touches = e.sourceIndex === r.selected || e.targetIndex === r.selected;
    const line = svg('polyline', {
      points: pts.map((p) => `${cx(p.x)},${cy(p.y)}`).join(' '),
      class: `gv-edge${e.kind === 'visual-loop' ? ' loop' : ''}${bad ? ' bad' : ''}${touches ? ' near' : ''}`,
      'marker-end': `url(#${e.kind === 'visual-loop' ? 'gv-arrow-loop' : bad ? 'gv-arrow-bad' : 'gv-arrow'})`,
    });
    root.append(line);
    if (e.label) {
      const longest = [...e.segments].sort((a, b) => (Math.abs(b.end.x - b.start.x) + Math.abs(b.end.y - b.start.y)) - (Math.abs(a.end.x - a.start.x) + Math.abs(a.end.y - a.start.y)))[0]!;
      const t = svg('text', { x: cx((longest.start.x + longest.end.x) / 2), y: cy((longest.start.y + longest.end.y) / 2) - 3, class: 'gv-edge-label', 'text-anchor': 'middle' });
      t.textContent = truncateLabel(e.label, 18);
      root.append(t);
    }
  }
  r.draft.workflow.nodes.forEach((node, i) => {
    const n = layout.nodes[i];
    if (!n) return;
    const st = states.get(node.id);
    const bad = invalid.has(node.id);
    const g = svg('g', { class: `gv-node${i === r.selected ? ' sel' : ''}${bad ? ' bad' : ''}`, 'data-index': i });
    const stroke = bad ? 'var(--red)' : st ? STATE_COLOR[st] ?? 'var(--overlay0)' : TYPE_COLOR[node.type] ?? 'var(--blue)';
    g.append(svg('rect', { x: n.rect.x * CW + 1, y: n.rect.y * CH + 2, width: n.rect.width * CW - 2, height: n.rect.height * CH - 4, rx: 7, style: `stroke:${stroke}` }));
    const title = svg('text', { x: n.rect.x * CW + 10, y: (n.rect.y + 1) * CH + CH / 2 + 4, class: 'gv-node-title' });
    title.textContent = `${nodeTypeGlyph(node.type, r.collapsed.includes(node.id))} ${truncateLabel(node.name, Math.max(1, n.innerWidth - 4))}`;
    const badge = svg('text', { x: (right(n.rect) + 1) * CW - 12, y: (n.rect.y + 1) * CH + CH / 2 + 4, class: 'gv-node-badge', 'text-anchor': 'end', style: `fill:${stroke}` });
    badge.textContent = nodeBadge(bad, st);
    const tip = svg('title', {});
    tip.textContent = `${node.name} (${node.type}) ${node.id}`;
    g.append(title, badge, tip);
    g.addEventListener('click', () => {
      const cur = pane.review;
      if (!cur) return;
      setReview({ ...cur, selected: i, selectedEdge: 0, follow: true, pan: { x: 0, y: 0 } });
      draw();
    });
    g.addEventListener('dblclick', () => handleKey('Enter', false, false));
    root.append(g);
  });
  host.append(root);
}

function kv(dl: HTMLElement, k: string, v: string): void {
  dl.append(el('dt', '', k), el('dd', '', v));
}

function drawDetail(side: HTMLElement): void {
  const r = pane.review!;
  const node = r.draft.workflow.nodes[r.selected];
  if (!node) { side.append(el('p', 'meta', 'No node selected.')); return; }
  side.append(el('h3', '', `${nodeTypeGlyph(node.type, r.collapsed.includes(node.id))} ${node.name}`));
  const dl = el('dl', 'gv-facts');
  kv(dl, 'id', node.id);
  kv(dl, 'type', node.type);
  if (node.description) kv(dl, 'description', node.description);
  if (node.pattern) kv(dl, 'pattern', String(node.pattern));
  if (node.tags?.length) kv(dl, 'tags', node.tags.join(', '));
  if (node.inputs?.length) kv(dl, 'inputs', node.inputs.map((p) => p.name).join(', '));
  if (node.outputs?.length) kv(dl, 'outputs', node.outputs.map((p) => p.name).join(', '));
  const ms = node.model_settings as { model?: string | null; provider?: string | null } | null | undefined;
  if (ms?.model || ms?.provider) kv(dl, 'model', [ms.provider, ms.model].filter(Boolean).join(' / '));
  if (r.draft.origin.kind === 'recursive') {
    const s = nodeStrategies(r.draft.workflow, node.id);
    kv(dl, 'integration', s.integration ?? '-');
    kv(dl, 'verification', s.verification ?? '-');
    const ed = nodeEditable(r.draft, r.selected);
    kv(dl, 'editable', ed.editable ? 'yes (I, s, S)' : `no — ${ed.reason ?? 'locked'}`);
  }
  side.append(dl);
  const mode = r.mode;
  if (mode.kind === 'edit') {
    side.append(editBox(mode.field));
  } else {
    side.append(el('h4', '', 'Instructions'));
    side.append(el('pre', 'gv-instructions', node.instructions?.trim() ? node.instructions : '(none)'));
  }
  const edges = nodeEdges(r.draft.workflow, r.selected);
  side.append(el('h4', '', `Edges (${edges.length})`));
  const ul = el('ul', 'gv-edges');
  edges.forEach(({ edge }, i) => {
    const li = el('li', i === r.selectedEdge && mode.kind === 'detail' ? 'sel' : '');
    const port = (p: string | null | undefined) => (p ? `.${p}` : '');
    li.textContent = `${edge.source}${port(edge.source_port)} → ${edge.target}${port(edge.target_port)}${edge.label ? `  [${edge.label}]` : ''}`;
    ul.append(li);
  });
  if (edges.length === 0) ul.append(el('li', 'meta', 'no edges'));
  side.append(ul);
  side.append(el('p', 'meta', isAuthored(r.draft)
    ? 'i name  I instructions  j/k edge  x delete edge  [ ] follow edge  Esc back'
    : 'I instructions  s/S strategies  j/k edge  [ ] follow edge  Esc back'));
}

const FIELD_LABEL: Record<EditField, string> = { name: 'Name', instructions: 'Instructions', integration: 'Integration strategy', verification: 'Verification strategy' };

function editBox(field: EditField): HTMLElement {
  const r = pane.review!;
  const box = el('div', 'gv-edit');
  box.append(el('h4', '', `Edit ${FIELD_LABEL[field]}`));
  const input = field === 'name' ? el('input') : el('textarea');
  if (input instanceof HTMLInputElement) { input.type = 'text'; input.dataset.vim = 'on'; }
  input.value = r.buffer;
  input.spellcheck = false;
  input.className = 'gv-edit-input';
  input.addEventListener('input', () => { if (pane.review) pane.review = { ...pane.review, buffer: input.value }; });
  input.addEventListener('keydown', (ev) => {
    if (focusOwnsKey((ev as KeyboardEvent).key, ev.target as Element | null)) return;
    const kev = ev as KeyboardEvent;
    if (kev.key === 'Escape') {
      kev.preventDefault();
      kev.stopPropagation();
      if (pane.review) setReview({ ...pane.review, mode: { kind: 'detail' }, buffer: '' });
      draw();
      view().focus({ preventScroll: true });
    } else if (kev.key === 'Enter' && !kev.shiftKey) {
      kev.preventDefault();
      kev.stopPropagation();
      void commitEdit(input.value).then(() => view().focus({ preventScroll: true }));
    }
  });
  box.append(input, el('p', 'meta', field === 'name' ? 'Enter saves, Esc cancels' : 'Enter saves, Shift-Enter new line, Esc cancels'));
  window.setTimeout(() => { input.focus(); }, 0);
  return box;
}

function drawPicker(side: HTMLElement, kind: PickerKind, index: number): void {
  const title = kind === 'topology' ? 'Starter topologies' : kind === 'saved' ? 'Saved workflows' : 'Recursive graphs (read-only)';
  side.append(el('h3', '', title));
  const ul = el('ul', 'gv-picker');
  const items: Array<{ label: string; detail: string }> = kind === 'topology'
    ? pane.starters.map((t) => ({ label: t.label, detail: t.description }))
    : kind === 'saved'
      ? savedPickerEntries(pane.workflows).map((w) => ({ label: w.title, detail: `${w.stage} · updated ${w.updated_at.slice(0, 16).replace('T', ' ')}` }))
      : pane.recursive.map((g) => ({ label: str(g, 'title') ?? idOf(g) ?? '?', detail: `${str(g, 'status') ?? '-'} · ${(idOf(g) ?? '').slice(0, 8)}` }));
  items.forEach((it, i) => {
    const li = el('li', i === index ? 'sel' : '');
    li.append(el('span', 'gv-pick-label', it.label), el('span', 'gv-pick-detail', it.detail));
    li.addEventListener('click', () => { if (pane.review?.mode.kind === 'picker') { setReview({ ...pane.review, mode: { ...pane.review.mode, index: i } }); draw(); } });
    li.addEventListener('dblclick', () => void choose(kind, i));
    ul.append(li);
  });
  if (items.length === 0) ul.append(el('li', 'meta', kind === 'saved' ? 'No saved workflows' : kind === 'recursive' ? 'No recursive graphs' : 'Loading…'));
  side.append(ul);
  window.setTimeout(() => ul.children[index]?.scrollIntoView({ block: 'nearest' }), 0);
  if (kind === 'topology') {
    const t = pane.starters[index];
    if (t) {
      if (t.use_cases.length > 0) side.append(el('p', 'meta', `Use for: ${t.use_cases.join('; ')}`));
      const wf = pane.starterPreview.get(t.name);
      if (wf) side.append(miniGraph(wf));
      else void previewStarter(t.name);
    }
  }
  side.append(el('p', 'meta', 'j/k move  g/G ends  PageDown/PageUp page  Enter open  Esc back'));
}

/** A small read-only drawing of a workflow (the starter preview). */
export function miniGraph(wf: WorkflowDefinition): SVGSVGElement {
  const layout = layoutWorkflowGraph(wf);
  const b = layout.bounds;
  const s = 4;
  const root = svg('svg', { class: 'gv-mini', viewBox: `${b.x * s - 4} ${b.y * s * 2 - 4} ${b.width * s + 8} ${b.height * s * 2 + 8}` });
  for (const e of layout.edges) {
    const pts = e.segments.length > 0 ? [e.segments[0]!.start, ...e.segments.map((x) => x.end)] : [];
    root.append(svg('polyline', { points: pts.map((p) => `${p.x * s},${p.y * s * 2}`).join(' '), class: `gv-edge${e.kind === 'visual-loop' ? ' loop' : ''}` }));
  }
  wf.nodes.forEach((node, i) => {
    const n = layout.nodes[i];
    if (!n) return;
    root.append(svg('rect', { x: n.rect.x * s, y: n.rect.y * s * 2, width: n.rect.width * s, height: n.rect.height * s * 2, rx: 3, class: 'gv-mini-node', style: `stroke:${TYPE_COLOR[node.type] ?? 'var(--blue)'}` }));
  });
  return root;
}

function drawRun(side: HTMLElement, ex: ExecutionSnapshot): void {
  const box = el('div', 'gv-run');
  if (isTopologyRun(ex)) {
    for (const [text, kind] of runViewText(pane.review!.draft.workflow, ex, false)) box.append(el('div', `gv-run-${kind}`, text));
  } else {
    box.append(el('div', 'gv-run-title', `${ex.workflow_name} ${executionBadge(ex)}`));
    if (ex.error) box.append(el('div', 'gv-run-warn', ex.error));
  }
  if (ex.blocked_reason) box.append(el('div', 'gv-run-warn', `blocked: ${ex.blocked_reason} (:topology-resolve)`));
  side.append(box);
}

function draw(): void {
  if (!pane.open || !pane.review) return;
  const r = pane.review;
  const root = view();
  const ex = execution();
  const head = root.querySelector<HTMLElement>('.graph-head-title')!;
  head.replaceChildren(el('span', 'gv-name', r.draft.workflow.name));
  if (r.draft.origin.kind === 'recursive') head.append(el('span', 'badge gv-badge-ro', 'recursive (read-only)'));
  if (isAuthored(r.draft)) {
    const p = r.draft.persistence;
    head.append(el('span', `badge gv-badge-${p}`, p === 'clean' ? (r.draft.workflowId ? 'saved' : 'draft') : p === 'save-failed' ? 'save failed' : p));
  }
  if (ex) head.append(el('span', `badge gv-badge-run ${ex.status}`, executionBadge(ex)));
  if (pane.layout?.hasCycleEdges) head.append(el('span', 'badge gv-badge-cycle', 'cycle'));
  if (pane.validation && !pane.validation.executable) head.append(el('span', 'badge gv-badge-bad', `${pane.validation.diagnostics.filter((d) => d.severity === 'error').length} blocking`));
  if (pane.busy) head.append(el('span', 'meta', pane.busy));
  const canvas = root.querySelector<HTMLElement>('.graph-canvas')!;
  drawCanvas(canvas);
  const side = root.querySelector<HTMLElement>('.graph-side')!;
  side.replaceChildren();
  if (r.mode.kind === 'picker') drawPicker(side, r.mode.picker, r.mode.index);
  else if (r.mode.kind === 'detail' || r.mode.kind === 'edit') drawDetail(side);
  else {
    const node = r.draft.workflow.nodes[r.selected];
    if (node) {
      side.append(el('h3', '', node.name));
      side.append(el('p', 'meta', `${node.type} · ${node.id}`));
      const instr = fieldValue(r.draft.workflow, r.selected, 'instructions');
      if (instr) side.append(el('pre', 'gv-instructions', instr));
      side.append(el('p', 'meta', 'Enter opens the node detail'));
    }
    if (pane.validation?.diagnostics.length) {
      side.append(el('h4', '', 'Validation'));
      const ul = el('ul', 'gv-diags');
      for (const d of pane.validation.diagnostics.slice(0, 8)) ul.append(el('li', d.severity === 'error' ? 'bad' : 'warn', `${d.severity}: ${d.message}`));
      side.append(ul);
    }
  }
  if (ex) drawRun(side, ex);
  const status = root.querySelector<HTMLElement>('.graph-status')!;
  const keys = r.draft.workflow.nodes.length === 0
    ? 't starter  o saved  H/J/K/L pan  c follow  q close'
    : 'h/j/k/l move  [ ] edges  g/G ends  Enter detail  d delete  u undo  z fold  t/o/R pickers  r run  x interrupt  H/J/K/L pan  c follow  Tab dashboard  q close';
  status.textContent = `${r.follow ? 'camera: follow' : `camera: manual (${r.pan.x},${r.pan.y})`} · node ${r.draft.workflow.nodes.length === 0 ? 0 : r.selected + 1}/${r.draft.workflow.nodes.length} · ${keys}`;
}

// ---------- wiring ----------

function isTyping(t: HTMLElement | null): boolean {
  return !!t?.closest('input, textarea, select, [contenteditable=""], [contenteditable="true"]');
}

/** The app key engine is mid-chord (`<Space>…`) or has the `:` line open: let it have the key. */
export function engineBusy(): boolean {
  return document.getElementById('keyhint')?.hidden === false || document.getElementById('cmdline')?.hidden === false;
}

function build(): HTMLElement {
  const root = el('section', 'graph-pane');
  root.id = 'graph-view';
  root.hidden = true;
  root.tabIndex = -1;
  root.setAttribute('aria-label', 'Graph review');
  const head = el('header', 'graph-head');
  const btn = (text: string, key: string, title: string) => {
    const b = el('button', 'btn', text);
    b.type = 'button';
    b.dataset.gkey = key;
    b.title = title;
    return b;
  };
  head.append(
    el('h1', '', 'Graph review'), el('span', 'graph-head-title'), el('span', 'spacer'),
    btn('Starter (t)', 't', 'Open a starter topology'), btn('Saved (o)', 'o', 'Open a saved workflow'),
    btn('Recursive (R)', 'R', 'Open a recursive graph read-only'), btn('Run (r)', 'r', 'Save, check and run the workflow'),
    btn('Interrupt (x)', 'x', 'Interrupt the running execution'), btn('Close (q)', 'q', 'Close graph review'),
  );
  const body = el('div', 'graph-body');
  body.append(el('div', 'graph-canvas'), el('aside', 'graph-side'));
  root.append(head, body, el('footer', 'graph-status meta'));
  head.addEventListener('click', (ev) => {
    const b = (ev.target as HTMLElement).closest<HTMLButtonElement>('[data-gkey]');
    if (!b) return;
    window.setTimeout(() => b.blur(), 0);
    const k = b.dataset.gkey!;
    if (k === 'x') { void interruptRun(); return; }
    // From the canvas: leave a picker or the detail first, as the keys do.
    if (pane.review && pane.review.mode.kind !== 'navigate') setReview({ ...pane.review, mode: { kind: 'navigate' } });
    handleKey(k, false, false);
  });
  return root;
}

export function initGraphReview(): void {
  const root = build();
  const layout = document.querySelector('.layout');
  if (layout) layout.after(root);
  else document.body.append(root);
  window.addEventListener('resize', () => { if (pane.open) draw(); });
  window.addEventListener('keydown', (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    if (!pane.open || ev.isComposing) return;
    if (!paneKeyAllowed(isVimKeysOn(), ev)) return;
    if (document.querySelector('dialog[open]')) return;
    const target = document.activeElement as HTMLElement | null;
    if (ev.metaKey || ev.altKey || isTyping(target) || engineBusy()) return;
    if (ev.key === 'Enter' && target?.closest('button')) return;
    if (ev.key === ' ' || ev.key === ':') return;
    if (handleKey(ev.key, ev.ctrlKey, ev.shiftKey)) {
      ev.preventDefault();
      ev.stopImmediatePropagation();
    }
  }, true);
}
