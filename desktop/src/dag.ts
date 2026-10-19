// Recursive DAG browser pane (DOM side of dag-model.ts; slice 11, hub #3013):
// the TUI's `:dag` overlay. Readbacks are loaded the way load_selected_graph
// does (each optional read degrades to a warning); controls open only behind
// their daemon capability and a typed confirmation (max_steps, a reason, a
// recovery budget). Daemon text is set with textContent only.
import { $, el, toast } from './dom.js';
import {
  type Control, type DagState, type Detail, type Panel, type Row, PANELS, PANEL_LABEL, TASK_LIMIT, arr, artifactPage,
  artifactStep, cancellationRows, cap, capsLine, cursorChanged, dagKey, deferredRows, focusRunId, footerText,
  gateLines, heartbeatRows, idOf, initialDag, interruptRows, obj, panelRows, parseBudget, parseMaxSteps, parseReason,
  short, sortGraphs, str, taskCounts, taskTreeRows, ts, word,
} from './dag-model.js';
import { graphsApi } from './graphs-api.js';
import { invoke } from './api.js';
import { artifactRef, bucketLines, liveAttemptOf, previewBody, previewHead, validationOf } from './dag-readbacks.js';
import { engineBusy } from './graphs.js';
import { uiChanged } from './state.js';
import { focusOwnsKey, paneKeyAllowed } from './modal-gate.js';
import { isVimKeysOn } from './vimkeys.js';

let s: DagState = initialDag(null);

// ---------- gap sweep G4: inspector readbacks ----------

type Readback = { status: 'loading' } | { status: 'ok'; value: Row } | { status: 'error'; error: string };
const readbacks = new Map<string, Readback>();

/** The cached readback for `key`, starting `fetch` the first time. */
function readback(key: string, fetch: () => Promise<unknown>): Readback {
  const had = readbacks.get(key);
  if (had) return had;
  readbacks.set(key, { status: 'loading' });
  fetch().then((v) => {
    readbacks.set(key, { status: 'ok', value: (v && typeof v === 'object' ? v : { value: v }) as Row });
    draw();
  }, (e: unknown) => {
    readbacks.set(key, { status: 'error', error: e instanceof Error ? e.message : String(e) });
    draw();
  });
  return { status: 'loading' };
}

/** Enter in the Live panel loads the attempt's artifact buckets (GetRecursiveLiveAttemptArtifacts). */
let liveOpen: string | null = null;
let open = false;
let epoch = 0;
let pendingBracket: string | null = null;
let onOpen: (() => void) | null = null;

export function configureDag(opts: { opening: () => void }): void {
  onOpen = opts.opening;
}

export const isDagOpen = (): boolean => open;

function view(): HTMLElement {
  return $('dag-view');
}

// ---------- loading ----------

async function soft<T>(warnings: string[], what: string, p: Promise<T>): Promise<T | null> {
  try {
    return await p;
  } catch (e) {
    warnings.push(`${what} readback failed: ${String(e)}`);
    return null;
  }
}

async function loadDetail(graphId: string): Promise<Detail> {
  const caps = s.caps;
  const warnings: string[] = [];
  const graph = await graphsApi.dagGraph(graphId);
  const runOn = cap(caps, 'recursive_dag_run_inspection');
  const liveOn = cap(caps, 'recursive_dag_live_status_inspection');
  const [operational, runs, cancellations, recovery, heartbeats, liveRecovery, live, validations, artifacts] = await Promise.all([
    soft(warnings, 'operational status', graphsApi.dagOperational(graphId)),
    runOn ? soft(warnings, 'scheduler run list', graphsApi.dagRuns(graphId)) : Promise.resolve([] as Row[]),
    soft(warnings, 'cancellation request', graphsApi.dagCancellations(graphId)),
    cap(caps, 'recursive_dag_recovery_status') ? soft(warnings, 'recovery status', graphsApi.dagRecovery(graphId)) : Promise.resolve(null),
    liveOn ? soft(warnings, 'stale heartbeat', graphsApi.dagStaleHeartbeats(graphId)) : Promise.resolve([] as Row[]),
    liveOn ? soft(warnings, 'live recovery', graphsApi.dagLiveRecovery(graphId)) : Promise.resolve(null),
    liveOn ? soft(warnings, 'live attempt', graphsApi.dagLiveAttempts(graphId)) : Promise.resolve([] as Row[]),
    cap(caps, 'recursive_dag_live_validation_inspection') ? soft(warnings, 'live validation', graphsApi.dagValidations(graphId)) : Promise.resolve([] as Row[]),
    cap(caps, 'recursive_dag_artifact_list_pagination')
      ? graphsApi.dagArtifacts(graphId, null).then((p) => ({ page: p, error: null as string | null }), (e) => ({ page: null, error: String(e) }))
      : Promise.resolve({ page: null, error: null as string | null }),
  ]);
  const runList = runs ?? [];
  const runId = runOn ? focusRunId(operational, runList) : null;
  const [runDetail, runEvents] = runId
    ? await Promise.all([soft(warnings, 'selected scheduler run', graphsApi.dagRun(runId)), soft(warnings, 'scheduler run event', graphsApi.dagRunEvents(runId))])
    : [null, []];
  for (const w of arr(liveRecovery, 'warnings').slice(0, 5)) warnings.push(`live recovery readback warning ${str(w, 'code') ?? ''}: ${str(w, 'message') ?? ''}`);
  const page = artifactPage(artifacts.page, graphId, cap(caps, 'recursive_dag_artifact_list_pagination'), artifacts.error);
  if (page.status === 'error' && page.message) warnings.push(page.message);
  return {
    graph, operational, runs: runList, runDetail, runEvents: runEvents ?? [], cancellations: cancellations ?? [], recovery,
    heartbeats: heartbeats ?? [], liveRecovery, live: live ?? [], validations: validations ?? [], artifacts: page, warnings,
  };
}

/** load_recursive_dag_browser: caps, the inventory (newest first), then the chosen graph's readbacks. */
async function load(graphId: string | null, message: string | null = null): Promise<void> {
  const my = ++epoch;
  s = { ...s, status: 'loading', message: message ?? 'loading recursive DAG readbacks', busy: true };
  draw();
  try {
    const caps = await graphsApi.capabilities();
    if (my !== epoch) return;
    if (!cap(caps, 'recursive_dag_inspection')) {
      s = { ...s, caps, status: 'disabled', message: 'daemon does not advertise recursive_dag_inspection', busy: false };
      return;
    }
    s = { ...s, caps };
    const graphs = sortGraphs(await graphsApi.listRecursive(s.projectId, true));
    if (my !== epoch) return;
    const want = graphId && graphs.some((g) => idOf(g) === graphId) ? graphId : idOf(graphs[0]);
    const detail = want ? await loadDetail(want) : null;
    if (my !== epoch) return;
    const at = Math.max(0, graphs.findIndex((g) => idOf(g) === want));
    s = {
      ...s, status: 'ready', graphs, hydrated: want, detail, message, busy: false, loadedAt: new Date().toISOString().slice(11, 19),
      cursor: { ...s.cursor, graphs: at, tasks: Math.min(s.cursor.tasks, TASK_LIMIT - 1) },
    };
    // Keep each cursor inside its (new) list.
    for (const p of PANELS) {
      const n = panelRows(s, p).length;
      if (s.cursor[p] >= n) s = { ...s, cursor: { ...s.cursor, [p]: Math.max(0, n - 1) } };
    }
  } catch (e) {
    if (my !== epoch) return;
    s = { ...s, status: 'error', message: String(e), busy: false };
  } finally {
    if (my === epoch) { draw(); uiChanged(); }
  }
}

async function loadMoreArtifacts(): Promise<void> {
  const d = s.detail;
  const gid = s.hydrated;
  if (!d || !gid || !d.artifacts.nextCursor) return;
  try {
    const page = artifactPage(await graphsApi.dagArtifacts(gid, d.artifacts.nextCursor), gid, true, null);
    const seen = new Set(d.artifacts.items.map((i) => str(i, 'id')));
    const items = [...d.artifacts.items, ...page.items.filter((i) => !seen.has(str(i, 'id')))];
    s = { ...s, detail: { ...d, artifacts: { ...page, items, warnings: [...d.artifacts.warnings, ...page.warnings] } } };
  } catch (e) {
    toast(`artifact summary page failed: ${String(e)}`);
  }
  draw();
}

// ---------- controls ----------

async function submit(c: Exclude<Control, { kind: 'none' } | { kind: 'cancel-prefix' }>): Promise<void> {
  const gid = s.hydrated;
  try {
    if (c.kind === 'steps') {
      const n = parseMaxSteps(c.input);
      if (typeof n !== 'number' || !gid) return;
      const res = c.live ? await graphsApi.dagLiveRun(gid, n) : await graphsApi.dagFakeRun(gid, n);
      const run = obj(res, 'run') ?? res;
      await load(gid, `${c.live ? 'LIVE' : 'FAKE'} run ${short(idOf(run))} ${word(run.status)} (${str(run, 'step_count') ?? '?'}/${n} steps)`);
      return;
    }
    if (c.kind === 'reason') {
      const why = parseReason(c.input);
      if (typeof why !== 'string') return;
      let res: Row;
      if (c.target === 'graph') {
        if (!gid) return;
        res = await graphsApi.dagCancelGraph(gid, why);
      } else {
        const runId = idOf(s.detail?.runs[s.cursor.runs]);
        if (!runId) return;
        res = await graphsApi.dagCancelRun(runId, why);
      }
      // Applied / Rejected come back as warnings: the target may have finished first.
      await load(gid, `cancellation ${short(idOf(res))} ${word(res.status)}${str(res, 'rejection_reason') ? `: ${str(res, 'rejection_reason')}` : ''}`);
      return;
    }
    const b = parseBudget(c.maxGraphs, c.timeBudget);
    if ('error' in b) return;
    const pass = await graphsApi.dagContinueRecovery(b.maxGraphs, b.timeBudgetMs);
    await load(gid, `recovery pass ${word(pass.status)}: checked=${str(pass, 'checked') ?? 0} recovered=${str(pass, 'recovered') ?? 0} deferred=${str(pass, 'deferred') ?? 0} errors=${str(pass, 'errors') ?? 0}`);
  } catch (e) {
    s = { ...s, message: `control failed: ${String(e)}` };
    toast(`Control failed: ${String(e)}`);
    draw();
    await load(gid, `control failed: ${String(e)}`);
  }
}

// ---------- open / close ----------

/** `:dag` (and the graph review's dashboard, opened on its recursive graph). */
export function openDag(projectId: string | null, graphId: string | null = null): void {
  if (open && graphId === null) { closeDag(); return; }
  onOpen?.();
  open = true;
  s = { ...initialDag(projectId), panel: graphId ? 'runs' : 'graphs' };
  document.querySelector<HTMLElement>('.layout')!.hidden = true;
  view().hidden = false;
  view().focus({ preventScroll: true });
  void load(graphId);
  uiChanged();
}

export function closeDag(): void {
  if (!open) return;
  open = false;
  epoch++;
  view().hidden = true;
  document.querySelector<HTMLElement>('.layout')!.hidden = false;
  uiChanged();
}

export function refreshDag(): void {
  readbacks.clear();
  void load(s.hydrated ?? idOf(s.graphs[s.cursor.graphs]));
}

export function toggleGates(): void {
  s = { ...s, gates: !s.gates };
  draw();
}

function handleKey(key: string, ctrl: boolean, shift: boolean): boolean {
  if (s.control.kind === 'none' && !ctrl && (key === ']' || key === '[')) { pendingBracket = key; return true; }
  if (pendingBracket && key === 'a') {
    s = artifactStep(s, pendingBracket === ']' ? 1 : -1);
    pendingBracket = null;
    draw();
    return true;
  }
  pendingBracket = null;
  if (key === 'Enter' && !ctrl && s.panel === 'live' && s.control.kind === 'none' && s.detail) {
    const live = liveAttemptOf((panelRows(s)[s.cursor.live] as Row | undefined) ?? null);
    if (!live) { toast('This live row has no attempt id to load'); return true; }
    if (!cap(s.caps, 'recursive_dag_live_status_inspection')) { toast('live attempt artifacts unavailable: daemon capability disabled'); return true; }
    liveOpen = liveOpen === live ? null : live;
    draw();
    return true;
  }
  const res = dagKey(s, { key, ctrl, shift });
  if (!res.handled) return false;
  const before = s.control.kind;
  s = res.state;
  const e = res.effect;
  switch (e.kind) {
    case 'close': closeDag(); return true;
    case 'load': readbacks.clear(); liveOpen = null; void load(e.graphId); break;
    case 'more-artifacts': void loadMoreArtifacts(); break;
    case 'submit': void submit(e.control); break;
    case 'toast': toast(e.text); break;
    case 'scroll': view().querySelector('.dag-body')?.scrollBy({ top: e.by * 18 }); break;
    default: break;
  }
  draw();
  const now: Control['kind'] = s.control.kind;
  if (before === 'none' && now !== 'none' && now !== 'cancel-prefix') {
    window.setTimeout(() => view().querySelector<HTMLInputElement>('.dag-control input')?.focus(), 0);
  }
  return true;
}

// ---------- rendering ----------

function table(head: string[], rows: string[][], selected: number | null, active: boolean, onPick?: (i: number) => void): HTMLElement {
  const t = el('table', `dag-table${active ? ' active' : ''}`);
  const tr = el('tr');
  for (const h of ['', ...head]) tr.append(el('th', '', h));
  t.append(tr);
  rows.forEach((cells, i) => {
    const r = el('tr', i === selected && active ? 'sel' : '');
    r.append(el('td', 'dag-mark', i === selected && active ? '>' : ''));
    for (const c of cells) r.append(el('td', '', c));
    if (onPick) r.addEventListener('click', () => onPick(i));
    t.append(r);
  });
  return t;
}

function section(panel: Panel, title: string): HTMLElement {
  const sec = el('section', `dag-section${s.panel === panel ? ' active' : ''}`);
  const h = el('h3', '', title);
  h.addEventListener('click', () => { s = { ...s, panel }; draw(); });
  sec.append(h);
  return sec;
}

const dim = (text: string) => el('p', 'meta', text);
function kvLine(label: string, value: string): HTMLElement {
  const p = el('p', 'dag-kv');
  p.append(el('span', 'dag-k', label), el('span', '', value));
  return p;
}

function pick(panel: Panel) {
  return (i: number) => { s = { ...s, panel, cursor: { ...s.cursor, [panel]: i } }; draw(); };
}

function drawDetail(body: HTMLElement, d: Detail): void {
  const g = obj(d.graph, 'graph') ?? {};
  const counts = taskCounts(d);
  const sum = section('graphs', 'SELECTED GRAPH');
  sum.append(
    kvLine('graph', `${str(g, 'title') ?? '-'}  id=${short(idOf(g))} root=${short(str(g, 'root_task_id'))} status=${word(g.status)} mode=${word(g.execution_mode)}`),
    kvLine('bounds', `depth=${str(g, 'max_depth') ?? '-'} fanout=${str(g, 'max_fanout') ?? '-'} descendants=${str(g, 'max_descendants') ?? '-'} step_limit=${str(g, 'step_limit') ?? '-'} tasks=${arr(d.graph, 'nodes').length} ready=${counts.ready} running=${counts.running} blocked=${counts.blocked} failed=${counts.failed} cancelled=${counts.cancelled} succeeded=${counts.succeeded}`),
    kvLine('provenance', `project=${short(str(g, 'project_id'))} workflow=${short(str(g, 'workflow_id'))} topology=${short(str(g, 'topology_id'))} parent_session=${short(str(g, 'parent_session_id'))} source_execution=${str(g, 'source_execution_id') ?? '-'}`),
  );
  if (str(g, 'quarantined_at') || str(g, 'malformed_reason')) {
    sum.append(kvLine('quarantine', `at=${ts(str(g, 'quarantined_at'))} reason=${str(g, 'quarantine_reason') ?? '-'} malformed=${str(g, 'malformed_reason') ?? '-'}`));
  }
  const op = d.operational;
  sum.append(op
    ? kvLine('operation', `active_run=${short(idOf(obj(op, 'active_run')))} latest_run=${short(idOf(obj(op, 'latest_run')))} running_attempts=${arr(op, 'running_attempts').length} open_cancellations=${arr(op, 'open_cancellation_requests').length} live_active=${arr(op, 'active_live_attempts').length} live_recovery_pending=${arr(op, 'live_recovery_pending').length} validations=${arr(op, 'latest_live_validations').length}`)
    : dim('operational status readback unavailable or capability-disabled'));
  if (d.warnings.length > 0) {
    sum.append(kvLine('warnings', `${d.warnings.length} non-fatal readback issue(s)`));
    for (const w of d.warnings.slice(0, 5)) sum.append(dim(`- ${w}`));
  }
  body.append(sum);

  const tasks = section('tasks', `TASK TREE (showing up to ${TASK_LIMIT})`);
  const tree = taskTreeRows(d);
  const attempts = arr(d.graph, 'attempts');
  const edges = arr(d.graph, 'edges');
  const nodes = arr(d.graph, 'nodes');
  tasks.append(table(['path', 'status', 'try', 'deps', 'kids', 'id', 'title'], tree.map(({ path, task }) => {
    const id = idOf(task);
    return [path, word(task.status), String(attempts.filter((a) => str(a, 'task_id') === id).length), String(edges.filter((e) => str(e, 'to_task_id') === id).length), String(nodes.filter((n) => str(n, 'parent_task_id') === id).length), short(id), str(task, 'title') ?? ''];
  }), s.cursor.tasks, s.panel === 'tasks', pick('tasks')));
  const task = tree[s.cursor.tasks]?.task;
  if (s.panel === 'tasks' && task) {
    tasks.append(kvLine('objective', str(task, 'objective') ?? '-'));
    if (str(task, 'blocked_reason')) tasks.append(kvLine('blocked', str(task, 'blocked_reason')!));
    if (str(task, 'verification_strategy')) tasks.append(kvLine('verify', str(task, 'verification_strategy')!));
  }
  body.append(tasks);

  const runs = section('runs', 'SCHEDULER RUNS (limit 12)');
  if (!cap(s.caps, 'recursive_dag_run_inspection')) runs.append(dim('run inspection capability disabled'));
  else if (d.runs.length === 0) runs.append(dim('no scheduler runs'));
  else {
    runs.append(table(['status', 'source', 'steps', 'heartbeat', 'id', 'reason'], d.runs.map((r) => [
      word(r.status), word(r.source), `${str(r, 'step_count') ?? 0}/${str(r, 'max_steps') ?? '?'}`,
      `${ts(str(r, 'lease_heartbeat_at'))} / ${ts(str(r, 'lease_expires_at'))}`, short(idOf(r)), r.stop_reason ? word(r.stop_reason) : str(r, 'failure_reason') ?? '-',
    ]), s.cursor.runs, s.panel === 'runs', pick('runs')));
  }
  if (d.runDetail) {
    const rd = d.runDetail;
    runs.append(kvLine('selected run', `active_attempts=${arr(rd, 'active_attempts').length} cancellations=${arr(rd, 'cancellation_requests').length} live_attempts=${arr(rd, 'live_attempts').length} live_interrupts=${arr(rd, 'live_interrupts').length} live_validations=${arr(rd, 'latest_live_validations').length} report_artifact=${str(obj(rd, 'report_artifact'), 'id') ?? '-'}`));
  }
  if (d.runEvents.length === 0) runs.append(dim('events: no run events loaded'));
  else for (const ev of d.runEvents.slice(0, 6)) runs.append(kvLine(`#${str(ev, 'id') ?? '?'}`, `${ts(str(ev, 'created_at'))} ${str(ev, 'event_type') ?? ''} ${str(ev, 'message') ?? ''}`));
  body.append(runs);

  const rec = section('recovery', 'RECOVERY / DEFERRED / QUARANTINE');
  rec.append(dim(`control gate: recoverctl=${cap(s.caps, 'recursive_dag_recovery_control') ? 'yes' : 'no'}; manual only, no background recursive recovery loop`));
  if (!cap(s.caps, 'recursive_dag_recovery_status')) rec.append(dim('recovery status capability disabled; recovery controls remain unavailable'));
  else if (!d.recovery) rec.append(dim('recovery status unavailable'));
  const gs = obj(d.recovery, 'graph_status');
  if (gs) rec.append(kvLine('graph recovery', `state=${word(gs.state)} pass=${short(str(gs, 'pass_id'))} reason=${str(gs, 'reason') ?? '-'} last_error=${str(gs, 'last_error') ?? '-'}`));
  const lp = obj(d.recovery, 'latest_pass');
  if (lp) rec.append(kvLine('latest pass', `status=${word(lp.status)} source=${word(lp.source)} checked=${str(lp, 'checked') ?? 0} recovered=${str(lp, 'recovered') ?? 0} quarantined=${str(lp, 'quarantined') ?? 0} deferred=${str(lp, 'deferred') ?? 0} skipped=${str(lp, 'skipped') ?? 0} errors=${str(lp, 'errors') ?? 0}`));
  if (d.liveRecovery) rec.append(kvLine('live recovery', `attempts=${arr(d.liveRecovery, 'live_attempts').length} heartbeats=${arr(d.liveRecovery, 'heartbeat_states').length} operator_review=${d.liveRecovery.operator_review_required === true ? 'yes' : 'no'} deferred=${obj(d.liveRecovery, 'deferred_graph') ? 'yes' : 'no'} warnings=${arr(d.liveRecovery, 'warnings').length}`));
  const deferred = deferredRows(d);
  rec.append(kvLine('deferred', `count=${str(d.recovery, 'deferred_graph_count') ?? 'unknown'} rows=${deferred.length}`));
  if (deferred.length > 0) {
    rec.append(table(['graph', 'state', 'reason', 'next after', 'last error'], deferred.map((r) => [short(str(r, 'graph_id') ?? str(r, 'raw_graph_id')), word(r.state), str(r, 'reason') ?? '-', ts(str(r, 'next_after')), str(r, 'last_error') ?? '-']), s.cursor.recovery, s.panel === 'recovery', pick('recovery')));
  } else rec.append(dim('no deferred recovery graphs'));
  body.append(rec);

  const can = section('cancellations', 'CANCELLATION REQUESTS (limit 24)');
  can.append(dim(`control gate: cancelctl=${cap(s.caps, 'recursive_dag_cancellation_control') ? 'yes' : 'no'}; C g graph, C r run, C t task reserved`));
  const cr = cancellationRows(d);
  if (cr.length === 0) can.append(dim('no cancellation requests'));
  else can.append(table(['status', 'scope', 'source', 'requested', 'id', 'reason'], cr.map((r) => [word(r.status), word(r.scope), word(r.source), ts(str(r, 'requested_at')), short(idOf(r)), str(r, 'reason') ?? '']), s.cursor.cancellations, s.panel === 'cancellations', pick('cancellations')));
  body.append(can);

  const hb = section('heartbeats', 'HEARTBEATS / LEASES');
  if (!cap(s.caps, 'recursive_dag_live_status_inspection')) hb.append(dim('live heartbeat readback unavailable: daemon capability disabled'));
  else {
    const rows = heartbeatRows(d);
    if (rows.length === 0) hb.append(dim('no live heartbeat readbacks'));
    else hb.append(table(['status', 'attempt', 'live', 'session', 'last/expires', 'reason'], rows.map((r) => [word(r.heartbeat_status), short(str(r, 'live_attempt_id')), word(r.live_attempt_status), short(str(r, 'session_id')), `${ts(str(r, 'heartbeat_at'))} / ${ts(str(r, 'heartbeat_expires_at'))}`, str(r, 'reason') ?? str(r, 'stale_reason') ?? '-']), s.cursor.heartbeats, s.panel === 'heartbeats', pick('heartbeats')));
  }
  body.append(hb);

  const ir = section('interrupts', 'LIVE INTERRUPTS (limit 24)');
  if (!cap(s.caps, 'recursive_dag_live_status_inspection')) ir.append(dim('live interrupt readback unavailable: daemon capability disabled'));
  else {
    const rows = interruptRows(d);
    if (rows.length === 0) ir.append(dim('no live interrupt readbacks'));
    else ir.append(table(['status', 'requested', 'completed', 'task', 'session', 'reason'], rows.map((r) => [word(r.status), ts(str(r, 'requested_at')), ts(str(r, 'completed_at')), short(str(r, 'task_id')), short(str(r, 'session_id')), str(r, 'reason') ?? '']), s.cursor.interrupts, s.panel === 'interrupts', pick('interrupts')));
  }
  body.append(ir);

  const lv = section('live', 'LIVE ATTEMPTS / VALIDATION (limit 24)');
  if (!cap(s.caps, 'recursive_dag_live_status_inspection')) lv.append(dim('live readback unavailable: daemon capability disabled'));
  else if (d.live.length === 0) lv.append(dim('no live attempts'));
  else {
    lv.append(table(['status', 'recovery', 'heartbeat', 'interrupt', 'validation', 'task', 'session/model'], d.live.map((a) => {
      const sm = obj(a, 'summary') ?? a;
      return [word(sm.status), word(sm.recovery_status), obj(a, 'heartbeat') ? word(obj(a, 'heartbeat')!.heartbeat_status) : 'missing', obj(a, 'latest_interrupt') ? word(obj(a, 'latest_interrupt')!.status) : '-', obj(a, 'latest_validation') ? word(obj(a, 'latest_validation')!.status) : '-', short(str(sm, 'task_id')), `${short(str(sm, 'session_id'))} ${str(sm, 'model') ?? '-'}`];
    }), s.cursor.live, s.panel === 'live', pick('live')));
  }
  if (d.validations.length > 0) lv.append(kvLine('validation', `${d.validations.length} result rows loaded`));
  if (liveOpen) {
    const live = liveOpen;
    const r = readback(`live:${live}`, () => invoke<unknown>('dag_live_artifacts', { liveAttemptId: live }));
    const box = el('div', 'dag-inspector');
    box.append(el('h4', '', `Live attempt ${short(live)} · artifact buckets (Enter closes)`));
    if (r.status === 'loading') box.append(dim('loading the attempt’s artifact buckets…'));
    else if (r.status === 'error') box.append(dim(`live attempt artifacts failed: ${r.error}`));
    else {
      for (const line of bucketLines(r.value)) box.append(el('div', 'dag-kv', line));
      const pre = el('pre', 'dag-pre');
      pre.textContent = JSON.stringify(r.value, null, 2).slice(0, 12000);
      box.append(pre);
    }
    lv.append(box);
  }
  body.append(lv);

  const art = section('artifacts', 'ARTIFACTS / INSPECTORS');
  const page = d.artifacts;
  if (page.status === 'disabled') art.append(dim(page.message ?? 'artifact summaries unavailable'));
  else if (page.status === 'error') art.append(dim(page.message ?? 'artifact summary readback failed'));
  for (const w of page.warnings) art.append(dim(`warning: ${w}`));
  if (page.items.length === 0) art.append(dim('no artifact, validation, test, diff, or report rows loaded'));
  else {
    art.append(table(['kind', 'role', 'id', 'task', 'label'], page.items.map((a) => [word(a.kind), word(a.role), str(a, 'id') ?? '-', short(str(a, 'task_id')), str(a, 'label') ?? str(a, 'uri') ?? str(a, 'title') ?? '']), s.cursor.artifacts, s.panel === 'artifacts', pick('artifacts')));
    if (page.hasMore) art.append(dim('more artifact summaries: select past the last row and press Enter'));
  }
  if (s.inspector.open) art.append(inspector(page.items[s.cursor.artifacts] ?? null));
  body.append(art);
}

function inspector(row: Row | null): HTMLElement {
  const box = el('div', 'dag-inspector');
  box.append(el('h4', '', `Inspector · ${s.inspector.view}`));
  if (!row) { box.append(dim('no artifact row selected')); return box; }
  const ref = artifactRef(row, s.hydrated);
  const lookup = ref && cap(s.caps, 'recursive_dag_artifact_lookup')
    ? readback(`artifact:${ref.graphId}:${ref.artifactId}`, () => invoke<unknown>('dag_artifact_lookup', { graphId: ref.graphId, artifactId: ref.artifactId }))
    : null;
  const validation = validationOf(row);
  switch (s.inspector.view) {
    case 'metadata': {
      const pre = el('pre', 'dag-pre');
      const full = lookup?.status === 'ok' ? lookup.value : row;
      pre.textContent = JSON.stringify(full, null, 2).slice(0, 8000);
      if (lookup?.status === 'loading') box.append(dim('loading the artifact (GetRecursiveExecutionArtifact)…'));
      if (lookup?.status === 'error') box.append(dim(`artifact lookup failed: ${lookup.error}; showing the summary row`));
      if (!lookup) box.append(dim(cap(s.caps, 'recursive_dag_artifact_lookup') ? 'no artifact id on this row' : 'artifact lookup unavailable: capability disabled; showing the summary row'));
      box.append(pre);
      if (validation && cap(s.caps, 'recursive_dag_live_validation_inspection')) {
        const iss = readback(`issues:${validation}`, () => invoke<unknown>('dag_validation_issues', { validationId: validation, liveAttemptId: null }));
        const issues = iss.status === 'ok' ? (Array.isArray(iss.value) ? iss.value : arr(iss.value, 'issues')) : [];
        box.append(el('h4', '', 'Validation issues'));
        if (iss.status === 'loading') box.append(dim('loading validation issues…'));
        else if (iss.status === 'error') box.append(dim(`validation issues failed: ${iss.error}`));
        else if (issues.length === 0) box.append(dim('no validation issues'));
        else box.append(table(['severity', 'class', 'code', 'message'], issues.map((i) => [word((i as Row).severity), word((i as Row).class), word((i as Row).code), str(i, 'message') ?? '']), null, false));
      }
      break;
    }
    case 'links': {
      const links = lookup?.status === 'ok' ? arr(lookup.value, 'links') : [];
      box.append(kvLine('graph', short(str(row, 'graph_id'))), kvLine('task', short(str(row, 'task_id'))), kvLine('attempt', str(row, 'attempt_id') ?? '-'), kvLine('live attempt', short(str(row, 'live_attempt_id'))), kvLine('run', short(str(row, 'scheduler_run_id'))), kvLine('validation', str(row, 'validation_id') ?? '-'));
      if (lookup?.status === 'loading') box.append(dim('loading links…'));
      for (const l of links) box.append(el('div', 'dag-kv', JSON.stringify(l).slice(0, 300)));
      break;
    }
    case 'preview': {
      if (!cap(s.caps, 'recursive_dag_artifact_preview_inspection')) { box.append(dim('bounded artifact preview unavailable: capability disabled')); break; }
      if (!ref) { box.append(dim('no artifact id on this row to preview')); break; }
      const p = readback(`preview:${ref.graphId}:${ref.artifactId}`, () => invoke<unknown>('dag_artifact_preview', { graphId: ref.graphId, artifactId: ref.artifactId }));
      if (p.status === 'loading') { box.append(dim('loading the bounded preview (PreviewRecursiveExecutionArtifact)…')); break; }
      if (p.status === 'error') { box.append(dim(`artifact preview failed: ${p.error}`)); break; }
      box.append(el('div', 'dag-kv', previewHead(p.value)));
      for (const w of arr(p.value, 'warnings')) box.append(el('p', 'dag-warn', JSON.stringify(w).slice(0, 200)));
      const pre = el('pre', 'dag-pre');
      pre.textContent = previewBody(p.value);
      box.append(pre);
      break;
    }
    case 'test': box.append(dim('typed test detail unavailable (no daemon readback parses raw artifact content)')); break;
    case 'diff': box.append(dim('typed diff detail unavailable (no daemon readback parses raw diff text)')); break;
  }
  return box;
}

function controlBox(c: Control): HTMLElement | null {
  if (c.kind === 'none') return null;
  const box = el('div', 'dag-control');
  if (c.kind === 'cancel-prefix') { box.append(el('strong', '', 'CANCEL: g graph · r run · t task (reserved) · Esc')); return box; }
  const input = (value: string, placeholder: string, onInput: (v: string) => void): HTMLInputElement => {
    const i = el('input');
    i.type = 'text';
    i.value = value;
    i.placeholder = placeholder;
    i.spellcheck = false;
    i.addEventListener('input', () => onInput(i.value));
    i.addEventListener('keydown', (ev) => {
      if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
      if (ev.key !== 'Enter' && ev.key !== 'Escape' && ev.key !== 'Tab') return;
      ev.preventDefault();
      ev.stopPropagation();
      handleKey(ev.key, false, ev.shiftKey);
      if (s.control.kind === 'none') view().focus({ preventScroll: true });
    });
    return i;
  };
  if (c.kind === 'steps') {
    box.append(el('strong', '', `${c.live ? 'LIVE' : 'FAKE'} run on ${short(s.hydrated)}: max_steps`));
    if (c.live) box.append(el('span', 'dag-warn', ' a LIVE run launches provider sessions'));
    box.append(input(c.input, 'positive digits', (v) => { if (s.control.kind === 'steps') s = { ...s, control: { ...s.control, input: v, error: null } }; }));
  } else if (c.kind === 'reason') {
    const target = c.target === 'graph' ? `graph ${short(s.hydrated)}` : `run ${short(idOf(s.detail?.runs[s.cursor.runs]))}`;
    box.append(el('strong', '', `Cancel ${target}: reason (≤ 240 characters)`));
    box.append(input(c.input, 'why', (v) => { if (s.control.kind === 'reason') s = { ...s, control: { ...s.control, input: v, error: null } }; }));
  } else {
    box.append(el('strong', '', 'Continue deferred recovery: max_graphs (required), time_budget_ms (optional)'));
    const a = input(c.maxGraphs, 'max_graphs', (v) => { if (s.control.kind === 'budget') s = { ...s, control: { ...s.control, maxGraphs: v, error: null } }; });
    const b = input(c.timeBudget, 'time_budget_ms', (v) => { if (s.control.kind === 'budget') s = { ...s, control: { ...s.control, timeBudget: v, error: null } }; });
    box.append(a, b);
    window.setTimeout(() => (c.field === 'max_graphs' ? a : b).focus(), 0);
  }
  if (c.error) box.append(el('div', 'form-error', c.error));
  return box;
}

function draw(): void {
  if (!open) return;
  const root = view();
  root.querySelector<HTMLElement>('.dag-caps')!.textContent = capsLine(s.caps);
  root.querySelector<HTMLElement>('.dag-loaded')!.textContent = `${s.projectId ? 'project scope' : 'all projects'} · loaded ${s.loadedAt ?? '-'} · panel ${PANEL_LABEL[s.panel]}${s.busy ? ' · loading…' : ''}`;
  const tabs = root.querySelector<HTMLElement>('.dag-tabs')!;
  tabs.replaceChildren(...PANELS.map((p) => {
    const b = el('button', `seg${p === s.panel ? ' active' : ''}`, PANEL_LABEL[p]);
    b.type = 'button';
    b.addEventListener('click', () => { s = { ...s, panel: p }; draw(); window.setTimeout(() => b.blur(), 0); });
    return b;
  }));
  const ctl = root.querySelector<HTMLElement>('.dag-control-slot')!;
  const box = controlBox(s.control);
  ctl.replaceChildren(...(box ? [box] : []));
  const gates = root.querySelector<HTMLElement>('.dag-gates')!;
  gates.hidden = !s.gates;
  gates.replaceChildren(el('h4', '', 'Control gates (!)'), ...gateLines(s).map((l) => el('div', '', l)));
  const body = root.querySelector<HTMLElement>('.dag-body')!;
  body.replaceChildren();
  if (s.message) body.append(el('p', 'dag-message', s.message));
  if (s.status === 'ready' || (s.status === 'loading' && s.graphs.length > 0)) {
    const inv = section('graphs', `GRAPHS (showing up to 100)`);
    if (s.graphs.length === 0) inv.append(dim('no recursive DAG graphs found for this scope'));
    else {
      inv.append(table(['status', 'mode', 'updated', 'id', 'title'], s.graphs.map((g) => [word(g.status), word(g.execution_mode), ts(str(g, 'updated_at')), short(idOf(g)), str(g, 'title') ?? '']), s.cursor.graphs, s.panel === 'graphs', (i) => { s = { ...s, panel: 'graphs', cursor: { ...s.cursor, graphs: i } }; draw(); }));
      const g = s.graphs[s.cursor.graphs];
      const why = str(g, 'quarantine_reason') ?? str(g, 'malformed_reason') ?? str(g, 'last_stop_reason');
      if (why) inv.append(kvLine('reason', why));
      if (cursorChanged(s)) inv.append(el('p', 'dag-warn', 'selected graph is not hydrated; press Enter to load it'));
    }
    body.append(inv);
    if (s.detail) drawDetail(body, s.detail);
  }
  root.querySelector<HTMLElement>('.dag-footer')!.textContent = footerText(s);
  body.querySelector('tr.sel')?.scrollIntoView({ block: 'nearest' });
}

// ---------- wiring ----------

function isTyping(t: HTMLElement | null): boolean {
  return !!t?.closest('input, textarea, select, [contenteditable=""], [contenteditable="true"]');
}

export function initDag(): void {
  const root = el('section', 'dag-pane');
  root.id = 'dag-view';
  root.hidden = true;
  root.tabIndex = -1;
  root.setAttribute('aria-label', 'Recursive DAG browser');
  const head = el('header', 'dag-head');
  const close = el('button', 'btn', 'Close (q)');
  close.type = 'button';
  close.addEventListener('click', closeDag);
  const reload = el('button', 'btn', 'Reload (r)');
  reload.type = 'button';
  reload.addEventListener('click', () => { refreshDag(); window.setTimeout(() => reload.blur(), 0); });
  head.append(el('h1', '', 'Recursive DAG'), el('span', 'meta dag-loaded'), el('span', 'spacer'), reload, close);
  root.append(head, el('div', 'meta dag-caps'), el('div', 'dag-tabs view-toggle'), el('div', 'dag-control-slot'), el('div', 'dag-gates'), el('div', 'dag-body'), el('footer', 'dag-footer meta'));
  const layout = document.querySelector('.layout');
  if (layout) layout.after(root);
  else document.body.append(root);
  window.addEventListener('keydown', (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    if (!open || ev.isComposing) return;
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
