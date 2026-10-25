// The DOM side of the graphs and topology slice (actions/graphs.ts); the
// feature code is graphs.ts (graph review), dag.ts (recursive DAG browser)
// and esp.ts (ESP Square). Collected by actions/runtime.generated.ts.
//
// `gR` runs the Epic's bound topology at once from the key, as the TUI does;
// the menu, palette and right-click ask first and show the topology preview.
// `:topology run` and `:topology-resolve` are typed commands and run at once.
import type { Session } from '../api.js';
import { type Invocation, type SliceRuntime, dispatch } from '../commands.js';
import { closeDag, configureDag, initDag, isDagOpen, openDag, refreshDag, toggleGates } from '../dag.js';
import { el, errorText, toast } from '../dom.js';
import { closeEsp, initEsp, isEspOpen, openEsp } from '../esp.js';
import { parseTopologyResolve } from '../graph-model.js';
import { issuesSnapshot } from '../issues.js';
import { GRAPHS_CSS } from '../graphs-css.js';
import { graphsApi } from '../graphs-api.js';
import {
  closeGraphReview, configureGraphReview, graphSnapshot, initGraphReview, interruptRun, isGraphOpen, knownExecutions,
  openGraphReview, openRunView, rememberExecution, runDraft, runGraphKey,
} from '../graphs.js';
import type { ActionId } from '../registry.js';
import { closeSettings, isSettingsOpen } from '../settings.js';
import { state, uiChanged } from '../state.js';
import {
  type EpicSession, checkRunEpic, chooseTopologyEpic, explainExecuteTopologyError, parseTopologyRun, previewText, topologyPreview,
} from '../topology-preview.js';
import { type GraphsActionId, type GraphsSnapshot, SLICE } from './graphs.js';

type TopoSession = Session & { workflow_id?: string | null; workflow_id_override?: string | null };

/** The session `gR` acts on: the list cursor, else the open session. */
function focusedSession(): TopoSession | null {
  const id = state.cursorId ?? state.selectedId;
  return id ? (state.sessions.find((s) => s.id === id) as TopoSession | undefined) ?? null : null;
}

export function graphsSnapshot(): GraphsSnapshot {
  const pane = isGraphOpen() ? 'graph' : isDagOpen() ? 'dag' : isEspOpen() ? 'esp' : null;
  const s = focusedSession();
  const check = checkRunEpic(s);
  return { pane, ...graphSnapshot(), epicReady: check.kind === 'ready' ? check.epicId : null };
}

/** A full-window pane replaces the session layout: close the others first. */
function opening(except: 'graph' | 'dag'): void {
  if (document.getElementById('issues-view')?.hidden === false) dispatch('issues-close', { source: 'button' });
  if (isSettingsOpen()) closeSettings({ keepStaged: true });
  if (except !== 'graph') closeGraphReview();
  if (except !== 'dag') closeDag();
}

function currentProjectId(): string | null {
  return focusedSession()?.project_id ?? null;
}

// ---------- gR ----------

async function previewDialog(topologyId: string, epic: TopoSession): Promise<boolean> {
  let lines: string[];
  let name = topologyId.slice(0, 8);
  try {
    const t = await graphsApi.getTopology(topologyId);
    name = t.name;
    lines = topologyPreview(t.definition).map(previewText);
  } catch (e) {
    lines = [`[topology preview unavailable — ${errorText(String(e))}]`];
  }
  return new Promise((resolve) => {
    const dlg = el('dialog', 'confirm topo-confirm');
    const form = el('form', 'launch-form');
    form.method = 'dialog';
    const heading = el('h2', '', `Run topology ${name}?`);
    heading.id = 'topo-confirm-title';
    dlg.setAttribute('aria-labelledby', heading.id);
    form.append(heading);
    form.append(el('p', 'meta', `Under Epic ${epic.title?.trim() || epic.id.slice(0, 8)}: its executor sessions spawn as the Epic's children.`));
    const pre = el('pre', 'topo-preview');
    pre.textContent = lines.join('\n');
    form.append(pre);
    const actions = el('div', 'actions');
    const cancel = el('button', 'btn', 'Cancel');
    cancel.type = 'button';
    const ok = el('button', 'btn primary', 'Run topology');
    ok.type = 'submit';
    actions.append(cancel, ok);
    form.append(actions);
    dlg.append(form);
    document.body.append(dlg);
    let result = false;
    cancel.addEventListener('click', () => dlg.close());
    form.addEventListener('submit', () => { result = true; });
    dlg.addEventListener('close', () => { dlg.remove(); resolve(result); });
    dlg.showModal();
  });
}

/** `gR` (action_handler/session.rs run_epic_topology). */
async function runEpicTopology(inv: Invocation): Promise<void> {
  const id = state.cursorId ?? state.selectedId;
  const s = focusedSession();
  // A focused id the list no longer has: silent, as the TUI's UnknownSession.
  if (id && !s) return;
  const check = checkRunEpic(s);
  if (check.kind === 'refused') { toast(check.why); return; }
  if (inv.source !== 'key' && !(await previewDialog(check.topologyId, s!))) return;
  try {
    const res = await graphsApi.executeTopology(check.topologyId, check.projectId, null, check.epicId);
    toast(`Topology execution started under Epic ${check.epicId} (exec ${res.execution_id})`, true);
  } catch (e) {
    toast(`ExecuteTopology failed: ${String(e)}`);
  }
}

// ---------- :topology run ----------

async function runTopologyOnIssue(args: string): Promise<void> {
  const cmd = parseTopologyRun(args);
  if ('error' in cmd) { toast(cmd.error); return; }
  const issue = issuesSnapshot().issue;
  if (!issue) { toast('Focus an Issue in the Issues workspace first'); return; }
  const epic = chooseTopologyEpic(state.sessions as unknown as EpicSession[], issue.project_id, cmd.epic, state.cursorId ?? state.selectedId);
  if ('error' in epic) { toast(epic.error); return; }
  let topology;
  try {
    topology = (await graphsApi.listTopologies(cmd.name)).find((t) => t.name === cmd.name);
  } catch (e) {
    toast(`topology lookup failed: ${String(e)}`);
    return;
  }
  if (!topology) { toast(`topology not found: ${cmd.name}`); return; }
  try {
    const res = await graphsApi.executeTopology(topology.id, issue.project_id, issue.display_number, epic.epic);
    toast(`Started ${topology.name} on #${issue.display_number} (exec ${res.execution_id})`, true);
    if (res.workflow_id) {
      dispatch('issues-close', { source: 'button' });
      await openRunView(res.execution_id, res.workflow_id, issue.project_id);
    }
  } catch (e) {
    toast(explainExecuteTopologyError(String(e)));
  }
}

// ---------- :topology-resolve ----------

async function resolveTopology(args: string): Promise<void> {
  const cmd = parseTopologyResolve(args);
  if ('error' in cmd) { toast(cmd.error); return; }
  let executionId = cmd.executionId;
  if (!executionId) {
    const blocked = knownExecutions().filter((e) => e.status === 'blocked');
    if (blocked.length !== 1) { toast(`${blocked.length} blocked topology executions; name one: :topology-resolve <execution_id> <action>`); return; }
    executionId = blocked[0]!.execution_id;
  }
  let snap;
  try {
    const { lookup } = await graphsApi.getExecution(executionId);
    if (lookup.status !== 'found') { toast(`topology execution not found: ${executionId}`); return; }
    snap = lookup.execution;
  } catch (e) {
    toast(`topology execution lookup failed: ${String(e)}`);
    return;
  }
  if (snap.row_version == null || !snap.blocked_attempt_id) { toast(`topology execution ${executionId} is not blocked on preserved work`); return; }
  rememberExecution(snap);
  try {
    const res = await graphsApi.resolveTopology(executionId, snap.blocked_attempt_id, cmd.action, snap.row_version, cmd.confirm);
    const a = (res.attempt ?? {}) as Record<string, unknown>;
    const report = res.report as { preserved_commit?: string | null; head?: string | null; dirty_paths?: unknown[]; diffstat?: unknown[] } | null | undefined;
    toast(report
      ? `${String(a.node_id)}@${String(a.iteration)} preserved ${report.preserved_commit ?? '-'} head ${report.head ?? '-'} (${report.dirty_paths?.length ?? 0} dirty, ${report.diffstat?.length ?? 0} changed)`
      : `${cmd.action} ${String(a.node_id)}@${String(a.iteration)} attempt ${String(a.attempt_no)}: ${String(a.status)}${res.deduplicated ? ' (replayed)' : ''}`, true);
  } catch (e) {
    toast(`topology resolve failed: ${String(e)}`);
  }
}

// ---------- run ----------

export function runGraphs(id: GraphsActionId, inv: Invocation): void {
  switch (id) {
    case 'graph-open': void openGraphReview(); return;
    case 'graph-close': closeGraphReview(); return;
    case 'graph-pick-topology': runGraphKey('t'); return;
    case 'graph-pick-saved': runGraphKey('o'); return;
    case 'graph-pick-recursive': runGraphKey('R'); return;
    case 'graph-run': void runDraft(); return;
    case 'graph-interrupt': void interruptRun(); return;
    case 'graph-undo': runGraphKey('u'); return;
    case 'graph-follow': runGraphKey('c'); return;
    case 'dag-open': if (isDagOpen()) closeDag(); else openDag(currentProjectId()); return;
    case 'dag-close': closeDag(); return;
    case 'dag-refresh': refreshDag(); return;
    case 'dag-gates': toggleGates(); return;
    case 'esp-open': openEsp(); return;
    case 'topology-run-epic': void runEpicTopology(inv); return;
    case 'topology-run-issue': void runTopologyOnIssue(inv.arg ?? ''); return;
    case 'topology-resolve': void resolveTopology(inv.arg ?? ''); return;
  }
}

function afterAction(id: ActionId): void {
  // Another full-window view replaced the panes.
  if (id === 'issues-open' || id === 'settings-open' || id === 'settings-hooks' || id === 'settings-skills') {
    closeGraphReview();
    closeDag();
  }
  if (id === 'palette' || id === 'help') closeEsp();
}

export function initGraphsSlice(): void {
  const style = document.createElement('style');
  style.id = 'graphs-css';
  style.textContent = GRAPHS_CSS;
  document.head.append(style);
  // The ESP overlay can sit over a pane: its capture listener goes first.
  initEsp();
  initGraphReview();
  initDag();
  configureGraphReview({
    opening: () => opening('graph'),
    // The graph review's info dashboard (Tab / l at the right edge of a bridged
    // recursive graph) opens the DAG browser on that graph.
    focusDashboard: (graphId) => openDag(null, graphId),
  });
  configureDag({ opening: () => opening('dag') });
  uiChanged();
}

export const RUNTIME: SliceRuntime = {
  slice: SLICE,
  run: (id, inv) => runGraphs(id as GraphsActionId, inv),
  snapshot: () => ({ graphs: graphsSnapshot() }),
  afterAction,
  init: initGraphsSlice,
  // After the settings pane (5), before the session list (10) and the Issues
  // view (20): the panes' capture listeners must see their keys first.
  initOrder: 6,
};
